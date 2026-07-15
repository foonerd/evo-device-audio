// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Ambient now-playing state observer.
//!
//! Runs unconditionally for the plugin's load lifetime — does
//! NOT require a custody to be held. Owns its own MPD command +
//! idle connections, listens to MPD's IDLE protocol, and
//! publishes the `audio_playback_now_playing` subject's state
//! on every observed transition.
//!
//! ## Why
//!
//! The supervisor task is custody-gated: it only spawns when an
//! operator's gesture takes custody on the warden. Without
//! custody, the subject's state stays `None` (the subject is
//! announced at load but never updated). Downstream consumers
//! that gate on `transport_state == "playing"` (the audio.terminus
//! visualiser's leader-of-active-group gate, in particular) stay
//! closed → no spectrum events even when MPD is actively
//! playing. The visualiser appears dark on every fresh boot
//! until someone takes custody.
//!
//! The ambient observer closes that gap. State publication is
//! tied to the PLUGIN's lifecycle (load → unload), not to
//! custody lifecycle. MPD's state reaches the subject regardless
//! of how playback was started (operator gesture through evo,
//! direct mpc invocation, MPD's autostart of a saved playlist,
//! anything else that drives the MPD daemon).
//!
//! ## Coexistence with the supervisor
//!
//! When custody is held the supervisor also publishes
//! `now_playing` on every state change via its own MPD idle
//! channel. Both publishers see the same MPD state at the same
//! IDLE-event boundary; both render identical
//! `PlaybackStateReport`s; both `update_state` calls carry
//! byte-identical payloads. The framework's subject substrate
//! is idempotent on identical content — a duplicate update_state
//! with the same value is a cheap no-op. No coordination flag
//! needed.
//!
//! ## Failure semantics
//!
//! Open failure: WARN log + 1-second reconnect retry until
//! shutdown. The ambient observer is best-effort; the
//! supervisor's own connections are independent.
//!
//! IDLE error: WARN log + reconnect via the same retry path the
//! supervisor uses.
//!
//! Subject publish error: DEBUG log; the next state change
//! re-publishes.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::mpd::{
    ConnectTimeouts, IdleSubsystem, MpdConnection, MpdEndpoint, MpdSong,
};
use crate::playback_supervisor::report::PlaybackStateReport;
use crate::playback_supervisor::subject_emitter::SubjectEmitter;

const PLUGIN_NAME: &str = "org.evoframework.playback.mpd";

/// Backoff between reconnect attempts when the ambient observer
/// loses its MPD connection. Short enough that a brief MPD
/// restart (the framework's fragment-rewrite path bounces MPD
/// every ~second on multi-room engagement transitions) recovers
/// within one cycle.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(1);

/// Handle to the ambient observer task. Dropping the handle (or
/// notifying via the cloned shutdown) terminates the task at the
/// next reconnect-or-shutdown branch.
pub(crate) struct AmbientObserverHandle {
    task: JoinHandle<()>,
    shutdown: Arc<Notify>,
}

impl AmbientObserverHandle {
    /// Notify the task to shut down and await its exit. Idempotent
    /// — calling twice is safe; the second call's await returns
    /// immediately on the already-completed task.
    pub(crate) async fn stop(self) {
        self.shutdown.notify_waiters();
        let _ = self.task.await;
    }
}

/// Spawn the ambient observer. Returns the handle the plugin
/// retains for the load's lifetime.
///
/// `endpoint` + `timeouts` configure the MPD connection the
/// observer opens (one command-side, one idle-side); the
/// observer reuses the same MPD endpoint the supervisor uses,
/// but on its own connection pair so the two are independent.
///
/// `subject_emitter` is the canonical announcer wrapper; the
/// observer calls `update_now_playing` on every observed
/// transition.
pub(crate) fn spawn(
    endpoint: MpdEndpoint,
    timeouts: ConnectTimeouts,
    subject_emitter: SubjectEmitter,
    music_directory: Option<std::path::PathBuf>,
) -> AmbientObserverHandle {
    let shutdown = Arc::new(Notify::new());
    let task_shutdown = Arc::clone(&shutdown);

    let task = tokio::spawn(async move {
        run(
            endpoint,
            timeouts,
            subject_emitter,
            music_directory,
            task_shutdown,
        )
        .await;
    });

    AmbientObserverHandle { task, shutdown }
}

async fn run(
    endpoint: MpdEndpoint,
    timeouts: ConnectTimeouts,
    subject_emitter: SubjectEmitter,
    music_directory: Option<std::path::PathBuf>,
    shutdown: Arc<Notify>,
) {
    tracing::info!(
        plugin = PLUGIN_NAME,
        endpoint = %endpoint,
        "ambient now-playing observer task started"
    );

    // Source-format probe cache: last MPD file_path we probed.
    // Survives across reconnects so a reconnect with the same
    // current song does NOT re-probe. Reset to None on song-gone.
    let mut last_probed_file: Option<String> = None;

    // Outer reconnect loop: connect, run the inner loop until it
    // errors out, sleep + retry.
    loop {
        if shutdown_requested(&shutdown).await {
            tracing::info!(
                plugin = PLUGIN_NAME,
                "ambient observer: shutdown received; exiting"
            );
            return;
        }

        let (cmd_conn, idle_conn) =
            match open_pair(endpoint.clone(), timeouts).await {
                Ok(pair) => pair,
                Err(()) => {
                    sleep_or_shutdown(&shutdown, RECONNECT_BACKOFF).await;
                    continue;
                }
            };

        // Publish an initial snapshot so subscribers connecting
        // before the first IDLE wake see the current state. If
        // MPD has been playing for a while when the plugin
        // loads, this is what fills the `current_state` slot
        // immediately rather than waiting for the next
        // transition.
        let mut cmd_conn = cmd_conn;
        emit_now_playing(
            &mut cmd_conn,
            &subject_emitter,
            music_directory.as_deref(),
            &mut last_probed_file,
        )
        .await;

        // Inner loop: each iteration blocks on MPD's IDLE; on
        // wake, re-query status + currentsong and republish.
        // Any transport error returns from the inner loop and
        // the outer loop reconnects.
        run_inner(
            idle_conn,
            &mut cmd_conn,
            &subject_emitter,
            music_directory.as_deref(),
            &mut last_probed_file,
            &shutdown,
        )
        .await;
    }
}

async fn open_pair(
    endpoint: MpdEndpoint,
    timeouts: ConnectTimeouts,
) -> Result<(MpdConnection, MpdConnection), ()> {
    let cmd_conn =
        match MpdConnection::connect_with_timeouts(endpoint.clone(), timeouts)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                // Transient connect error during the bootstrap
                // window (MPD's TCP listener not ready, socket
                // reset, restart in flight). The caller retries on
                // the next supervisor tick, so this is DEBUG per
                // LOGGING.md §2 — the operator has no action while
                // the recovery loop is healing.
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "ambient observer: command-conn open error; retrying"
                );
                return Err(());
            }
        };
    let idle_conn =
        match MpdConnection::connect_with_timeouts(endpoint.clone(), timeouts)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                // Same bootstrap-race contract as the command-conn
                // path above; DEBUG with retry-on-next-tick.
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "ambient observer: idle-conn open error; retrying"
                );
                return Err(());
            }
        };
    Ok((cmd_conn, idle_conn))
}

async fn run_inner(
    mut idle_conn: MpdConnection,
    cmd_conn: &mut MpdConnection,
    subject_emitter: &SubjectEmitter,
    music_directory: Option<&std::path::Path>,
    last_probed_file: &mut Option<String>,
    shutdown: &Arc<Notify>,
) {
    // The supervisor uses the same idle subsystems + a 24-hour
    // budget; mirror the choice so both observers wake on the
    // same MPD state changes. (Constants live in actor.rs; we
    // duplicate them here rather than couple the module graph
    // around two unrelated concepts.)
    const IDLE_SUBSYSTEMS: &[IdleSubsystem] = &[
        IdleSubsystem::Player,
        IdleSubsystem::Mixer,
        IdleSubsystem::Options,
        IdleSubsystem::Playlist,
    ];
    const IDLE_BUDGET: Duration = Duration::from_secs(86_400);

    loop {
        tokio::select! {
            _ = shutdown.notified() => {
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    "ambient observer: shutdown received from idle loop; exiting"
                );
                return;
            }
            wake = idle_conn.idle(IDLE_SUBSYSTEMS, IDLE_BUDGET) => {
                match wake {
                    Ok(changed) if changed.is_empty() => {
                        // No-change wake (idle exhausted budget
                        // or noidle fired elsewhere). Re-enter
                        // idle on the next iteration.
                        continue;
                    }
                    Ok(_changed) => {
                        emit_now_playing(
                            cmd_conn,
                            subject_emitter,
                            music_directory,
                            last_probed_file,
                        )
                        .await;
                    }
                    Err(e) => {
                        // Idle-conn errors are the expected signal
                        // that MPD has restarted (transport closed
                        // by the peer). The outer loop reconnects
                        // deterministically; a warn-class emit on
                        // every mpd-restart cycle is journal noise
                        // that fires on every install/reset/deploy
                        // primitive, not a fault. Debug-class is
                        // the correct level: observable when the
                        // operator enables debug logging, silent
                        // in normal operation.
                        tracing::debug!(
                            plugin = PLUGIN_NAME,
                            error = %e,
                            "ambient observer: idle errored; reconnecting"
                        );
                        return;
                    }
                }
            }
        }
    }
}

/// Query MPD's current state and publish on the
/// `audio_playback_now_playing` subject. The render path mirrors
/// the supervisor's `emit_best_effort_report` minus the
/// custody-side reporter step (no custody handle is required
/// here; the observer is custody-agnostic).
///
/// Transport errors on the query path do NOT fail the loop —
/// they log at DEBUG (the reconnect path in `run` recovers via
/// outer-loop reset on the next idle error) and the publish is
/// simply skipped for this cycle.
async fn emit_now_playing(
    cmd_conn: &mut MpdConnection,
    subject_emitter: &SubjectEmitter,
    music_directory: Option<&std::path::Path>,
    last_probed_file: &mut Option<String>,
) {
    let status = match cmd_conn.status().await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                error = %e,
                "ambient observer: status query failed; skipping this cycle"
            );
            return;
        }
    };
    let song: Option<MpdSong> = match cmd_conn.current_song().await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                error = %e,
                "ambient observer: currentsong query failed; skipping this cycle"
            );
            return;
        }
    };
    // Publish the source codec name BEFORE now_playing. The
    // source_codec is derived from the file extension on the
    // currentsong file: path; `None` covers both "no current
    // song" and "current song has an unknown extension". The
    // stream_format subject's `source_codec` field is therefore
    // always coherent with the currentsong state subscribers
    // observe on `audio_playback_now_playing`.
    let source_codec = song.as_ref().and_then(|s| s.codec_name.as_deref());
    subject_emitter.update_source_codec(source_codec).await;

    // File-side source-format probe: when the current file
    // differs from the last one we probed, read its head and
    // publish the parsed AudioFormat. The probe is bounded
    // I/O (a few KiB) and cached by file_path, so subsequent
    // status cycles on the same song are zero-cost. A song
    // transition that crosses a music_directory boundary, a
    // remote-mounted library that stalls, or an unknown codec
    // all surface as `None` — the wire envelope's `source`
    // field clears honestly rather than carrying stale data.
    maybe_probe_source_format(
        song.as_ref(),
        music_directory,
        last_probed_file,
        subject_emitter,
    )
    .await;

    // The ambient observer cannot know the operator's mute
    // intent (mute state is supervisor-task-local). Report the
    // raw MPD volume; consumers that distinguish muted-vs-zero
    // get that signal from the custody-held supervisor's
    // reports. The visualiser gate only cares about
    // `transport_state`, which is unaffected by mute.
    let muted_unknown_to_observer = false;
    let report =
        PlaybackStateReport::from_mpd(status, song, muted_unknown_to_observer);
    subject_emitter.update_now_playing(&report).await;
}

/// Run the file-side source-format probe iff the current song's
/// file path differs from the last one we probed. Publishes the
/// parsed AudioFormat (or `None` on probe failure / unknown
/// codec) through [`SubjectEmitter::update_source_format`].
///
/// The probe is cached by file_path: a status cycle on the same
/// song is zero I/O. The cache is reset to `None` when the song
/// goes away, so re-entering the same song afterwards re-probes.
///
/// **Shared with the actor (custody supervisor).** Both the
/// ambient observer and the custody supervisor must drive this
/// probe on every track transition so the `audio_playback_stream_format`
/// subject's `source` field stays coherent with the currently-
/// playing track. A `Some(prior-track-format)` MUST be replaced
/// by either `Some(new-track-format)` (if the parser succeeds)
/// or `None` (if it doesn't) — never silently left in place
/// across a track change.
pub(super) async fn maybe_probe_source_format(
    song: Option<&MpdSong>,
    music_directory: Option<&std::path::Path>,
    last_probed_file: &mut Option<String>,
    subject_emitter: &SubjectEmitter,
) {
    match song {
        None => {
            if last_probed_file.is_some() {
                *last_probed_file = None;
                subject_emitter.update_source_format(None).await;
            }
        }
        Some(s) => {
            if last_probed_file.as_deref() == Some(s.file_path.as_str()) {
                return;
            }
            *last_probed_file = Some(s.file_path.clone());
            let abs_path = match music_directory {
                Some(base) => base.join(&s.file_path),
                None => {
                    // No music_directory resolved at load time;
                    // publish None so the wire stays coherent.
                    subject_emitter.update_source_format(None).await;
                    return;
                }
            };
            let codec_hint = match s.codec_name.as_deref() {
                Some(c) => c,
                None => {
                    subject_emitter.update_source_format(None).await;
                    return;
                }
            };
            let probed = tokio::task::spawn_blocking({
                let abs_path = abs_path.clone();
                let codec_hint = codec_hint.to_string();
                move || {
                    crate::source_probe::probe_source_format(
                        &abs_path,
                        &codec_hint,
                    )
                }
            })
            .await
            .unwrap_or(None);
            if probed.is_none() {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    file = %s.file_path,
                    codec = %codec_hint,
                    "source-format probe returned None \
                     (file unreadable / unknown shape)"
                );
            }
            subject_emitter.update_source_format(probed.as_ref()).await;
        }
    }
}

async fn shutdown_requested(shutdown: &Arc<Notify>) -> bool {
    // Non-blocking check via tokio's select with a fired notify.
    // Used at the top of the outer loop to avoid a sleep+reconnect
    // cycle when shutdown fires before connect.
    tokio::select! {
        _ = shutdown.notified() => true,
        _ = std::future::ready(()) => false,
    }
}

async fn sleep_or_shutdown(shutdown: &Arc<Notify>, dur: Duration) {
    tokio::select! {
        _ = shutdown.notified() => {}
        _ = tokio::time::sleep(dur) => {}
    }
}

#[cfg(test)]
mod tests {
    //! End-to-end acceptance tests for the source-format probe's
    //! track-transition semantics.
    //!
    //! Driving these in isolation (without standing the full
    //! ambient observer + MPD mock) means writing synthetic
    //! file heads directly into a tempdir and calling
    //! [`maybe_probe_source_format`] with the matching
    //! [`MpdSong`] shape. The capturing emitter records every
    //! published envelope so the test asserts the EXACT
    //! `source` field shape after each transition.
    //!
    //! Acceptance criteria match the UI team's bug report:
    //!
    //! 1. A track transition from a parser-recognised source
    //!    (DSF / DSD64) to a different track replaces the
    //!    prior `source` field's shape. The new field is
    //!    either the new parser's result OR `None`, never the
    //!    stale prior shape.
    //! 2. The `source_codec` field stays current alongside
    //!    (the actor's update_source_codec runs in parallel;
    //!    here we focus on source_format and assume the
    //!    sibling field's invariant from
    //!    `subject_emitter::tests`).

    use super::*;
    use crate::mpd::MpdSong;
    use crate::playback_supervisor::test_mock::capturing_emitter;
    use std::time::Duration;

    /// Build a synthetic DSF file head the parser recognises as
    /// DSD64 stereo. Mirror of `source_probe::tests::make_dsf_head`
    /// (private to source_probe). Inlining the few bytes here
    /// avoids cross-module visibility changes on a 24-byte
    /// fixture.
    fn dsf_head_dsd64_stereo() -> Vec<u8> {
        let mut v = vec![0u8; 0x50];
        v[0..4].copy_from_slice(b"DSD ");
        v[4..12].copy_from_slice(&28u64.to_le_bytes());
        v[0x1C..0x20].copy_from_slice(b"fmt ");
        v[0x20..0x28].copy_from_slice(&52u64.to_le_bytes());
        v[0x34..0x38].copy_from_slice(&2u32.to_le_bytes());
        v[0x38..0x3C].copy_from_slice(&2_822_400u32.to_le_bytes());
        v[0x3C..0x40].copy_from_slice(&1u32.to_le_bytes());
        v
    }

    fn song_for(rel_path: &str) -> MpdSong {
        let codec_name = crate::mpd::derive_source_codec_name(rel_path);
        MpdSong {
            file_path: rel_path.to_string(),
            title: None,
            artist: None,
            album: None,
            duration: Some(Duration::from_secs(180)),
            codec_name,
            classical: Default::default(),
        }
    }

    #[tokio::test]
    async fn track_transition_dsf_to_mp3_clears_prior_dsd_source() {
        let tmp = tempfile::tempdir().unwrap();

        // First track: synthetic DSF (DSD64 / stereo).
        std::fs::write(tmp.path().join("track-a.dsf"), dsf_head_dsd64_stereo())
            .unwrap();

        // Second track: synthetic MP3 file with a head that
        // does NOT contain a valid MPEG frame header. The MP3
        // parser returns None on this — exactly the case the
        // bug surfaced (a parser that can't yield a shape MUST
        // clear, not leave the prior DSD shape in place).
        std::fs::write(tmp.path().join("track-b.mp3"), b"not a valid mp3 head")
            .unwrap();

        let (subjects, _relations, emitter) = capturing_emitter();
        let music_dir: std::path::PathBuf = tmp.path().to_path_buf();
        let mut last_probed_file: Option<String> = None;

        // First track: probe + publish; expect source.kind = "dsd".
        maybe_probe_source_format(
            Some(&song_for("track-a.dsf")),
            Some(music_dir.as_path()),
            &mut last_probed_file,
            &emitter,
        )
        .await;
        let publish_count_after_dsf = subjects.state_update_count();
        assert!(
            publish_count_after_dsf >= 1,
            "DSF probe must publish the envelope"
        );
        let (_, state_dsf) = subjects
            .state_update_at(publish_count_after_dsf - 1)
            .expect("post-DSF envelope present");
        assert_eq!(
            state_dsf["source"]["kind"], "dsd",
            "DSF parser must yield Dsd shape on the source field; \
             got {}",
            state_dsf["source"]
        );

        // Second track: probe + publish; expect source field
        // either cleared (null) OR re-parsed as PCM — but
        // NEVER the stale DSD shape from track A.
        maybe_probe_source_format(
            Some(&song_for("track-b.mp3")),
            Some(music_dir.as_path()),
            &mut last_probed_file,
            &emitter,
        )
        .await;
        let publish_count_after_mp3 = subjects.state_update_count();
        assert!(
            publish_count_after_mp3 > publish_count_after_dsf,
            "track transition must publish a new envelope"
        );
        let (_, state_mp3) = subjects
            .state_update_at(publish_count_after_mp3 - 1)
            .expect("post-MP3 envelope present");
        // The exact assertion the UI team named: source.kind
        // MUST NOT be "dsd" after the transition. The cleanest
        // outcome is null (parser returned None); a PCM shape
        // would also be acceptable but the head we wrote is
        // not a valid MP3 frame, so we expect null.
        let new_source = &state_mp3["source"];
        assert!(
            new_source.is_null() || new_source["kind"] != "dsd",
            "post-transition source must NOT carry stale DSD shape; \
             got {new_source}"
        );
    }

    #[tokio::test]
    async fn same_track_repeat_does_not_republish() {
        // Cache hit: same file_path twice in a row → second call
        // is zero-publish (the cached probe stays valid).
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("track.dsf"), dsf_head_dsd64_stereo())
            .unwrap();
        let (subjects, _relations, emitter) = capturing_emitter();
        let music_dir = tmp.path().to_path_buf();
        let mut last_probed_file: Option<String> = None;
        maybe_probe_source_format(
            Some(&song_for("track.dsf")),
            Some(music_dir.as_path()),
            &mut last_probed_file,
            &emitter,
        )
        .await;
        let after_first = subjects.state_update_count();
        // Same path, second call. Cache hit → no publish.
        maybe_probe_source_format(
            Some(&song_for("track.dsf")),
            Some(music_dir.as_path()),
            &mut last_probed_file,
            &emitter,
        )
        .await;
        assert_eq!(
            subjects.state_update_count(),
            after_first,
            "repeated probe on same file_path must not republish"
        );
    }
}
