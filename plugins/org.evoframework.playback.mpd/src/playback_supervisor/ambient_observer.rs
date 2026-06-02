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
) -> AmbientObserverHandle {
    let shutdown = Arc::new(Notify::new());
    let task_shutdown = Arc::clone(&shutdown);

    let task = tokio::spawn(async move {
        run(endpoint, timeouts, subject_emitter, task_shutdown).await;
    });

    AmbientObserverHandle { task, shutdown }
}

async fn run(
    endpoint: MpdEndpoint,
    timeouts: ConnectTimeouts,
    subject_emitter: SubjectEmitter,
    shutdown: Arc<Notify>,
) {
    tracing::info!(
        plugin = PLUGIN_NAME,
        endpoint = %endpoint,
        "ambient now-playing observer task started"
    );

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
        emit_now_playing(&mut cmd_conn, &subject_emitter).await;

        // Inner loop: each iteration blocks on MPD's IDLE; on
        // wake, re-query status + currentsong and republish.
        // Any transport error returns from the inner loop and
        // the outer loop reconnects.
        run_inner(idle_conn, &mut cmd_conn, &subject_emitter, &shutdown).await;
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
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "ambient observer: command-conn open failed; retrying"
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
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "ambient observer: idle-conn open failed; retrying"
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
                        emit_now_playing(cmd_conn, subject_emitter).await;
                    }
                    Err(e) => {
                        tracing::warn!(
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
