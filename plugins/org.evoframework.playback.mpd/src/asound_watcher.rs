// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! `/etc/asound.d/` watcher.
//!
//! The plugin's `snd_pcm_t` handle for `pcm.evo` is opened once
//! by MPD at output-enable time and resolved against the
//! composition of `/etc/asound.conf` + `/etc/asound.d/*.conf` AS
//! IT EXISTED AT OPEN TIME. Drop-ins that arrive afterwards
//! (multi-room source-mode bridge, operator-options rewrite,
//! future pipeline stages) do not retroactively change the
//! handle; MPD must close and reopen for the new composition to
//! take effect.
//!
//! This watcher closes that loop. A running MPD keeps the ALSA
//! configuration it loaded when it started: closing and reopening
//! its output (`disableoutput` + `enableoutput`) reopens the same
//! handle against the same stale configuration, which was seen on
//! the reference device (a paused output kept the DAC open through
//! a cycle, through a touched `asound.conf`, and only a process
//! restart moved it). So on every observed change under
//! `/etc/asound.d/` the watcher restarts MPD through the plugin's
//! own restarter (the `mpd_systemctl_restart` intent and its
//! narrow NOPASSWD drop-in, the same path a fragment rewrite
//! uses), and when the player was playing it resumes playback
//! afterwards, because MPD restores a paused queue. The output
//! cycle stays as the fallback when no restarter is available.
//!
//! Detection mechanism: mtime polling at a fixed interval. The
//! directory contents change only on operator gestures
//! (multi-room engage / disengage, options change), so a poll
//! that issues one `read_dir` + per-entry `metadata` syscall
//! per interval is cheap; an `inotify` dependency for this
//! frequency of event is unwarranted. The first poll seeds the
//! snapshot without dispatching (the supervisor's initial open
//! already matched the on-disk state at admit time).
//!
//! Failure modes:
//! - `/etc/asound.d/` missing → the watcher logs at debug once
//!   and continues polling; the directory is created by the
//!   distribution bootstrap, so its absence indicates an
//!   unbootstrapped install. The watcher self-heals when the
//!   directory appears.
//! - `read_dir` or `metadata` error → suppress (transient FS
//!   states, e.g. mid-rename inside the directory by the
//!   multi-room plugin's atomic write); the next poll observes
//!   the steady state.
//! - Supervisor disconnected / shutdown → dispatch fails with
//!   `PlaybackError::Shutdown`; the watcher logs at debug and
//!   continues. A subsequent custody admit installs a fresh
//!   sender via `active_command_sender`, picking up where the
//!   prior left off.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use tokio::time::interval;

use crate::mpd::{ConnectTimeouts, MpdConnection, MpdEndpoint, PlayState};
use crate::mpd_restart::MpdRestarter;
use crate::playback_supervisor::SupervisorCommandSender;

/// Directory the watcher observes. The bootstrap installs this
/// path with mode 0775 owned by the steward's service user; the
/// multi-room plugin writes its drop-in here; the delivery.alsa
/// plugin writes the operator-options drop-in here.
pub(crate) const ASOUND_DROP_IN_DIR: &str = "/etc/asound.d";

/// Polling cadence. ALSA composition changes only on operator
/// gestures (multi-room engage / disengage, options rewrite);
/// 3 seconds is fast enough that an operator does not perceive
/// the watcher as the latency bottleneck while being far
/// slower than the change rate, keeping syscall overhead
/// negligible on the smallest target tier.
const POLL_INTERVAL: Duration = Duration::from_secs(3);

/// Handle for the spawned watcher. Hold `Some(_)` after a
/// successful spawn; `take()` + `stop()` from the plugin's
/// `unload` drains the task on shutdown.
pub(crate) struct AsoundWatcherHandle {
    shutdown: Arc<Notify>,
    task: JoinHandle<()>,
}

impl AsoundWatcherHandle {
    /// Signal shutdown and await task exit. `notify_one()` (not
    /// `notify_waiters()`) so the shutdown permit is stored if
    /// the task hasn't yet parked at `.notified().await` — a
    /// `stop()` fired immediately after `spawn()` is race-safe.
    pub(crate) async fn stop(self) {
        self.shutdown.notify_one();
        let _ = self.task.await;
    }
}

/// Spawn the watcher. Returns the handle the plugin's `load()`
/// stashes for shutdown coordination.
///
/// `active_command_sender` is the same `Arc<Mutex<Option<...>>>`
/// cell the envelope subscriber consumes; populated by the
/// warden on `take_custody`, cleared on `release`. The watcher
/// looks at the cell on every detected change and skips the
/// dispatch when no custody is active (no MPD output to cycle).
pub(crate) fn spawn(
    active_command_sender: Arc<Mutex<Option<SupervisorCommandSender>>>,
    restarter: Arc<dyn MpdRestarter>,
    endpoint: MpdEndpoint,
    timeouts: ConnectTimeouts,
) -> AsoundWatcherHandle {
    let shutdown = Arc::new(Notify::new());
    let task = tokio::spawn(run(
        Arc::clone(&shutdown),
        active_command_sender,
        restarter,
        endpoint,
        timeouts,
    ));
    AsoundWatcherHandle { shutdown, task }
}

async fn run(
    shutdown: Arc<Notify>,
    active_command_sender: Arc<Mutex<Option<SupervisorCommandSender>>>,
    restarter: Arc<dyn MpdRestarter>,
    endpoint: MpdEndpoint,
    timeouts: ConnectTimeouts,
) {
    let mut last: HashMap<PathBuf, SystemTime> = HashMap::new();
    let mut seeded = false;
    let mut tick = interval(POLL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = shutdown.notified() => {
                tracing::debug!(
                    target = "asound_watcher",
                    "shutdown signal received; exiting"
                );
                return;
            }
            _ = tick.tick() => {
                let current = snapshot_mtimes();
                if seeded && current != last {
                    tracing::info!(
                        target = "asound_watcher",
                        dir = ASOUND_DROP_IN_DIR,
                        prior_entries = last.len(),
                        current_entries = current.len(),
                        "asound composition change detected; restarting MPD"
                    );
                    let watched = Watched {
                        mpd: DirectMpd {
                            endpoint: endpoint.clone(),
                            timeouts,
                        },
                        sender: active_command_sender.lock().await.clone(),
                    };
                    reload_mpd(&watched, restarter.as_ref()).await;
                }
                last = current;
                seeded = true;
            }
        }
    }
}

/// Snapshot the directory's entry-name → mtime map. Suppresses
/// read errors (mid-rename inside the directory; bootstrap
/// hasn't run yet; transient I/O). The empty map IS a valid
/// snapshot — it just means the directory is currently empty
/// or unreadable.
fn snapshot_mtimes() -> HashMap<PathBuf, SystemTime> {
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir(ASOUND_DROP_IN_DIR) else {
        return out;
    };
    for entry in entries.flatten() {
        if let Ok(meta) = entry.metadata() {
            if let Ok(mtime) = meta.modified() {
                out.insert(entry.path(), mtime);
            }
        }
    }
    out
}

/// What the reload needs: whether the player is playing, a way to
/// play again, and, as the fallback, a way to cycle the output.
trait Player {
    /// Whether the player is playing right now.
    async fn is_playing(&self) -> bool;
    /// Start playback again.
    async fn resume(&self) -> Result<(), String>;
    /// Close and reopen the MPD output (the fallback).
    async fn cycle_output(&self) -> Result<(), String>;
}

/// MPD, spoken to directly on its own socket.
///
/// The player's state is MPD's, not the custody's: a steward that
/// has just started has taken no custody yet, and asking the
/// custody then read "not playing" for a player that was, so the
/// restart left it paused. Asking MPD does not depend on when the
/// steward woke.
struct DirectMpd {
    endpoint: MpdEndpoint,
    timeouts: ConnectTimeouts,
}

/// How long to wait for a restarted MPD to take connections.
const RESUME_CONNECT_TRIES: u32 = 40;
const RESUME_CONNECT_GAP: Duration = Duration::from_millis(500);

impl DirectMpd {
    async fn connect(&self) -> Result<MpdConnection, String> {
        MpdConnection::connect_with_timeouts(
            self.endpoint.clone(),
            self.timeouts,
        )
        .await
        .map_err(|e| e.to_string())
    }

    async fn is_playing(&self) -> bool {
        let Ok(mut conn) = self.connect().await else {
            return false;
        };
        matches!(conn.status().await.map(|s| s.state), Ok(PlayState::Playing))
    }

    /// Play, once MPD takes connections again. MPD restores its
    /// queue paused (`restore_paused`), so `play` carries on from
    /// where it was.
    async fn resume(&self) -> Result<(), String> {
        let mut last = String::from("no attempt");
        for _ in 0..RESUME_CONNECT_TRIES {
            match self.connect().await {
                Ok(mut conn) => {
                    return conn.play().await.map_err(|e| e.to_string());
                }
                Err(e) => last = e,
            }
            tokio::time::sleep(RESUME_CONNECT_GAP).await;
        }
        Err(last)
    }
}

/// The player as the watcher sees it: MPD itself for state and for
/// resuming, and the active custody (when there is one) for the
/// output-cycle fallback.
struct Watched {
    mpd: DirectMpd,
    sender: Option<SupervisorCommandSender>,
}

impl Player for Watched {
    async fn is_playing(&self) -> bool {
        self.mpd.is_playing().await
    }

    async fn resume(&self) -> Result<(), String> {
        self.mpd.resume().await
    }

    async fn cycle_output(&self) -> Result<(), String> {
        match &self.sender {
            Some(sender) => {
                sender.cycle_output().await.map_err(|e| format!("{e:?}"))
            }
            None => Err("no active custody".to_string()),
        }
    }
}

/// Make a running MPD take the changed composition.
///
/// MPD is restarted through the plugin's restarter. A restarted
/// MPD comes back paused, so when the player was playing it is
/// told to play again: the operator hears the gap of a restart,
/// not a stopped player. When the restart is refused or
/// unavailable the output is cycled instead, which is the best a
/// running MPD can do, and the warning says that the change
/// applies at the next MPD start.
async fn reload_mpd<P: Player>(player: &P, restarter: &dyn MpdRestarter) {
    let was_playing = player.is_playing().await;
    match restarter.restart().await {
        Ok(()) => {
            tracing::info!(
                target = "asound_watcher",
                strategy = restarter.strategy_name(),
                was_playing,
                "MPD restarted to take the asound composition"
            );
            if was_playing {
                match player.resume().await {
                    Ok(()) => tracing::info!(
                        target = "asound_watcher",
                        "playback resumed after the MPD restart"
                    ),
                    Err(e) => tracing::warn!(
                        target = "asound_watcher",
                        error = %e,
                        "playback did not resume after the MPD restart"
                    ),
                }
            }
        }
        Err(reason) => {
            tracing::warn!(
                target = "asound_watcher",
                reason = %reason,
                "MPD restart unavailable; cycling the output instead, so the \
                 change applies at the next MPD start"
            );
            if let Err(e) = player.cycle_output().await {
                tracing::warn!(
                    target = "asound_watcher",
                    error = %e,
                    "MPD output cycle failed"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_handles_missing_directory_without_panicking() {
        // We do not control whether /etc/asound.d/ exists in
        // the test environment, so just call the snapshot and
        // assert it returns a HashMap (possibly empty).
        let s = snapshot_mtimes();
        let _: HashMap<PathBuf, SystemTime> = s;
    }

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex as StdMutex;

    use crate::mpd_restart::{FailingRestarter, NoOpRestarter};

    /// Records what the reload asked of the custody, in order.
    struct Recorder {
        playing: bool,
        calls: StdMutex<Vec<&'static str>>,
        resume_fails: AtomicBool,
    }

    impl Recorder {
        fn new(playing: bool) -> Self {
            Self {
                playing,
                calls: StdMutex::new(Vec::new()),
                resume_fails: AtomicBool::new(false),
            }
        }
        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Player for Recorder {
        async fn is_playing(&self) -> bool {
            self.calls.lock().unwrap().push("is_playing");
            self.playing
        }
        async fn resume(&self) -> Result<(), String> {
            self.calls.lock().unwrap().push("resume");
            if self.resume_fails.load(Ordering::SeqCst) {
                Err("gone".to_string())
            } else {
                Ok(())
            }
        }
        async fn cycle_output(&self) -> Result<(), String> {
            self.calls.lock().unwrap().push("cycle_output");
            Ok(())
        }
    }

    /// A playing player is restarted and then told to play: the
    /// output cycle, which reopens a stale configuration, is not
    /// used.
    #[tokio::test]
    async fn a_playing_player_is_restarted_and_resumed_not_cycled() {
        let restarter = NoOpRestarter::new();
        let c = Recorder::new(true);
        reload_mpd(&c, &restarter).await;
        assert_eq!(restarter.call_count(), 1);
        assert_eq!(c.calls(), vec!["is_playing", "resume"]);
    }

    /// A paused or stopped player is restarted and left alone.
    #[tokio::test]
    async fn a_paused_player_is_restarted_and_not_resumed() {
        let restarter = NoOpRestarter::new();
        let c = Recorder::new(false);
        reload_mpd(&c, &restarter).await;
        assert_eq!(restarter.call_count(), 1);
        assert_eq!(c.calls(), vec!["is_playing"]);
    }

    /// When the restart is refused the output is cycled instead.
    #[tokio::test]
    async fn a_refused_restart_falls_back_to_the_output_cycle() {
        let restarter = FailingRestarter::new("sudo refused");
        let c = Recorder::new(true);
        reload_mpd(&c, &restarter).await;
        assert_eq!(restarter.call_count(), 1);
        assert_eq!(c.calls(), vec!["is_playing", "cycle_output"]);
    }

    /// A resume that fails is logged and does not panic or retry.
    #[tokio::test]
    async fn a_failed_resume_is_survivable() {
        let restarter = NoOpRestarter::new();
        let c = Recorder::new(true);
        c.resume_fails.store(true, Ordering::SeqCst);
        reload_mpd(&c, &restarter).await;
        assert_eq!(c.calls(), vec!["is_playing", "resume"]);
    }

    /// A one-connection-at-a-time MPD that reports `state` and counts
    /// the `play` commands it is sent.
    async fn stub_mpd(
        state: &'static str,
    ) -> (MpdEndpoint, Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let listener =
            tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let plays = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = Arc::clone(&plays);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let seen = Arc::clone(&seen);
                tokio::spawn(async move {
                    let (r, mut w) = stream.into_split();
                    let _ = w.write_all(b"OK MPD 0.23.5\n").await;
                    let mut lines = BufReader::new(r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        if line.starts_with("status") {
                            let body = format!("state: {state}\nOK\n");
                            let _ = w.write_all(body.as_bytes()).await;
                        } else {
                            if line.starts_with("play") {
                                seen.fetch_add(1, Ordering::SeqCst);
                            }
                            let _ = w.write_all(b"OK\n").await;
                        }
                    }
                });
            }
        });
        (
            MpdEndpoint::tcp(addr.ip().to_string(), addr.port()).unwrap(),
            plays,
        )
    }

    /// The steward has just started and has taken no custody: the
    /// player is still found playing, from MPD itself, and played
    /// again after the restart. Asking the custody read "not
    /// playing" here and left a playing player paused.
    #[tokio::test]
    async fn a_player_is_resumed_when_the_steward_has_no_custody_yet() {
        let (endpoint, plays) = stub_mpd("play").await;
        let watched = Watched {
            mpd: DirectMpd {
                endpoint,
                timeouts: ConnectTimeouts::default(),
            },
            sender: None,
        };
        let restarter = NoOpRestarter::new();
        reload_mpd(&watched, &restarter).await;
        assert_eq!(restarter.call_count(), 1);
        assert_eq!(plays.load(Ordering::SeqCst), 1, "played again");
    }

    /// A paused player is restarted and left paused.
    #[tokio::test]
    async fn a_paused_player_is_not_played_by_the_reload() {
        let (endpoint, plays) = stub_mpd("pause").await;
        let watched = Watched {
            mpd: DirectMpd {
                endpoint,
                timeouts: ConnectTimeouts::default(),
            },
            sender: None,
        };
        let restarter = NoOpRestarter::new();
        reload_mpd(&watched, &restarter).await;
        assert_eq!(restarter.call_count(), 1);
        assert_eq!(plays.load(Ordering::SeqCst), 0);
    }

    /// With no custody and no restart, the fallback has nothing to
    /// cycle and the reload still ends.
    #[tokio::test]
    async fn a_refused_restart_with_no_custody_ends_quietly() {
        let (endpoint, plays) = stub_mpd("play").await;
        let watched = Watched {
            mpd: DirectMpd {
                endpoint,
                timeouts: ConnectTimeouts::default(),
            },
            sender: None,
        };
        let restarter = FailingRestarter::new("sudo refused");
        reload_mpd(&watched, &restarter).await;
        assert_eq!(restarter.call_count(), 1);
        assert_eq!(plays.load(Ordering::SeqCst), 0);
    }
}
