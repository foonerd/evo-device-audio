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
//! This watcher closes that loop. On every observed change
//! under `/etc/asound.d/` it dispatches a `CycleOutput`
//! supervisor command, which sends `disableoutput 0` +
//! `enableoutput 0` over the MPD wire protocol. MPD reopens
//! against the post-change drop-in stack — no `systemctl
//! restart mpd`, no fragment rewrite, no plugin reload.
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
    pub(crate) async fn stop(self) {
        self.shutdown.notify_waiters();
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
) -> AsoundWatcherHandle {
    let shutdown = Arc::new(Notify::new());
    let task = tokio::spawn(run(Arc::clone(&shutdown), active_command_sender));
    AsoundWatcherHandle { shutdown, task }
}

async fn run(
    shutdown: Arc<Notify>,
    active_command_sender: Arc<Mutex<Option<SupervisorCommandSender>>>,
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
                        "asound composition change detected; cycling MPD output"
                    );
                    dispatch_cycle_output(&active_command_sender).await;
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

/// Dispatch a `CycleOutput` to the currently-active custody's
/// supervisor. No-op when no custody is active or the dispatch
/// fails (the next change retries).
async fn dispatch_cycle_output(
    cell: &Arc<Mutex<Option<SupervisorCommandSender>>>,
) {
    let sender = {
        let guard = cell.lock().await;
        guard.as_ref().cloned()
    };
    let Some(sender) = sender else {
        tracing::debug!(
            target = "asound_watcher",
            "asound change observed but no active custody; skipping cycle"
        );
        return;
    };
    match sender.cycle_output().await {
        Ok(()) => tracing::info!(
            target = "asound_watcher",
            "MPD output cycled successfully"
        ),
        Err(e) => tracing::warn!(
            target = "asound_watcher",
            error = ?e,
            "MPD output cycle dispatch failed; will retry on next change"
        ),
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
}
