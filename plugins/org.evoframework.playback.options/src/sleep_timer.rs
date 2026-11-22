// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! A sleep timer fades to silence and pauses.
//!
//! One timer. `options.set_sleep_timer { v, seconds, fade_ms }`
//! arms it: after `seconds` the global volume on `audio.playback`
//! falls to silence over `fade_ms`, playback pauses, and the
//! volume returns to the level it had when the fade began, so the
//! next play is not silent. `seconds` 0 cancels. A cancel during
//! the fade stops it and restores that level without pausing. A
//! second set replaces the armed timer.
//!
//! The timer lives in memory only. Nothing is written to the
//! options file, and a restart cancels it: unload cancels, and a
//! fresh load starts with none. It is not crossfade, it is not an
//! alarm, and it does not program the RTC.

use std::sync::Arc;
use std::time::Duration;

use evo_plugin_sdk::contract::ShelfRequestDispatcher;
use serde_json::{json, Value};
use tokio::sync::oneshot;
use tokio::sync::oneshot::error::TryRecvError;
use tokio::task::JoinHandle;

use crate::PLUGIN_NAME;

/// The shelf whose transport and global volume the timer drives.
const PLAYBACK_SHELF: &str = "audio.playback";

/// Shortest step of the fade. Each step is one `set_volume`
/// round trip through the steward to the playback warden.
const FADE_STEP_MIN: Duration = Duration::from_millis(100);

/// The one timer.
#[derive(Default)]
pub(crate) struct SleepTimer {
    armed: tokio::sync::Mutex<Option<Armed>>,
}

/// A timer that is waiting or fading.
struct Armed {
    cancel: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl Armed {
    /// Stop the timer and wait for it. A timer that was fading
    /// has put the volume back when this returns.
    async fn disarm(self) {
        let _ = self.cancel.send(());
        if let Err(e) = self.task.await {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "sleep timer task ended abnormally"
            );
        }
    }
}

impl SleepTimer {
    /// Arm the timer: wait `wait`, then fade over `fade`, pause,
    /// and restore. An armed timer is cancelled first.
    pub(crate) async fn arm(
        &self,
        dispatcher: Arc<dyn ShelfRequestDispatcher>,
        wait: Duration,
        fade: Duration,
    ) {
        let mut slot = self.armed.lock().await;
        if let Some(old) = slot.take() {
            old.disarm().await;
        }
        let (cancel, cancelled) = oneshot::channel();
        let task = tokio::spawn(run(dispatcher, wait, fade, cancelled));
        *slot = Some(Armed { cancel, task });
        tracing::info!(
            plugin = PLUGIN_NAME,
            wait_s = wait.as_secs(),
            fade_ms = fade.as_millis() as u64,
            "sleep timer armed"
        );
    }

    /// Cancel the timer. Nothing armed is not an error.
    pub(crate) async fn cancel(&self) {
        if let Some(old) = self.armed.lock().await.take() {
            old.disarm().await;
            tracing::info!(plugin = PLUGIN_NAME, "sleep timer cancelled");
        }
    }

    /// Whether a timer is waiting or fading.
    #[cfg(test)]
    pub(crate) async fn is_armed(&self) -> bool {
        self.armed
            .lock()
            .await
            .as_ref()
            .is_some_and(|a| !a.task.is_finished())
    }
}

/// The timer's whole life: wait, fade, pause, restore.
async fn run(
    dispatcher: Arc<dyn ShelfRequestDispatcher>,
    wait: Duration,
    fade: Duration,
    mut cancelled: oneshot::Receiver<()>,
) {
    tokio::select! {
        _ = tokio::time::sleep(wait) => {}
        _ = &mut cancelled => return,
    }
    let d = &*dispatcher;
    // The level the fade starts from is the level it returns to.
    // Without it there is nothing to restore, so there is no fade:
    // the timer pauses and leaves the volume alone.
    let level = match volume(d).await {
        Ok(level) => Some(level),
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "sleep timer could not read the volume; pausing without a fade"
            );
            None
        }
    };
    if let Some(level) = level {
        let steps = fade_steps(level, fade);
        let interval = fade / steps;
        for step in 1..=steps {
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = &mut cancelled => {
                    restore(d, level).await;
                    return;
                }
            }
            if let Err(e) = set_volume(d, faded(level, step, steps)).await {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "sleep timer fade step failed; pausing now"
                );
                break;
            }
        }
        if !matches!(cancelled.try_recv(), Err(TryRecvError::Empty)) {
            restore(d, level).await;
            return;
        }
    }
    match send(d, "pause", json!({ "v": 1 })).await {
        Ok(_) => {
            tracing::info!(plugin = PLUGIN_NAME, "sleep timer paused playback")
        }
        Err(e) => tracing::warn!(
            plugin = PLUGIN_NAME,
            error = %e,
            "sleep timer could not pause playback"
        ),
    }
    if let Some(level) = level {
        restore(d, level).await;
    }
}

/// How many volume steps a fade takes: one per point of volume,
/// at most one per [`FADE_STEP_MIN`], and at least one.
fn fade_steps(level: u8, fade: Duration) -> u32 {
    let by_time = u32::try_from(fade.as_millis() / FADE_STEP_MIN.as_millis())
        .unwrap_or(u32::MAX);
    by_time.min(u32::from(level)).max(1)
}

/// The volume after `step` of `steps`; the last step is silence.
fn faded(level: u8, step: u32, steps: u32) -> u8 {
    u8::try_from(u32::from(level) * (steps - step) / steps).unwrap_or(0)
}

/// Put the volume back where the fade found it.
async fn restore(d: &dyn ShelfRequestDispatcher, level: u8) {
    if let Err(e) = set_volume(d, level).await {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            level,
            error = %e,
            "sleep timer could not restore the volume"
        );
    }
}

/// The global volume `audio.playback` reports now.
async fn volume(d: &dyn ShelfRequestDispatcher) -> Result<u8, String> {
    let body = send(d, "get_now_playing", json!({ "v": 1 })).await?;
    let state: Value =
        serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    state
        .get("volume")
        .and_then(Value::as_u64)
        .and_then(|v| u8::try_from(v).ok())
        .ok_or_else(|| "get_now_playing answered no volume".to_string())
}

async fn set_volume(
    d: &dyn ShelfRequestDispatcher,
    level: u8,
) -> Result<(), String> {
    send(d, "set_volume", json!({ "v": 1, "volume": level }))
        .await
        .map(|_| ())
}

async fn send(
    d: &dyn ShelfRequestDispatcher,
    verb: &str,
    payload: Value,
) -> Result<Vec<u8>, String> {
    let bytes = serde_json::to_vec(&payload).map_err(|e| e.to_string())?;
    d.dispatch(PLAYBACK_SHELF, verb, bytes, None)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fade steps once per point of volume or once per 100 ms,
    /// whichever is fewer, and at least once; its last step is
    /// silence.
    #[test]
    fn a_fade_ends_in_silence() {
        assert_eq!(fade_steps(40, Duration::from_millis(3000)), 30);
        assert_eq!(fade_steps(10, Duration::from_millis(3000)), 10);
        assert_eq!(fade_steps(40, Duration::ZERO), 1);
        assert_eq!(fade_steps(0, Duration::from_millis(3000)), 1);
        for (level, steps) in [(40, 30), (10, 10), (40, 1), (0, 1)] {
            assert_eq!(faded(level, steps, steps), 0);
            assert!(faded(level, 1, steps) <= level);
        }
    }
}
