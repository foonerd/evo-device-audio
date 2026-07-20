// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Startup-volume applier.
//!
//! Runs once per plugin load. Waits for the operator's audio-
//! options settings state to arrive (via the framework's
//! subject substrate) and for MPD to accept its first
//! command, then sends a single `setvol` carrying the operator-
//! configured startup volume clamped to the operator-configured
//! ceiling.
//!
//! ## Why
//!
//! MPD restores its own `mixer.volume` from its persistent state
//! file on daemon start. The framework's own configured
//! `startup_volume_percent` is the operator's declared "always
//! boot at this level" — an ear- and speaker-safety choice. If
//! MPD's restored value is not overridden after MPD comes up,
//! every reboot inherits whatever level the last shutdown left
//! behind, and the operator's declared startup value never fires.
//!
//! The applier closes that gap. It is the framework-side control
//! loop that guarantees "post-boot volume == configured startup"
//! regardless of what MPD's statefile carried in.
//!
//! ## Boundaries
//!
//! - **One-shot per load**: the task exits after the successful
//!   `setvol`. Subsequent operator gestures against the volume
//!   flow through the normal `set_volume` wire path; this
//!   applier does not run again until the next plugin load.
//! - **Clamp only**: no curve translation happens here. The
//!   caller supplies the effective value already clamped to
//!   `max_volume_percent`; the applier just sends the byte.
//!   Curve application (`VolumeCurve::apply`) is a separate
//!   defect — currently no code path applies curves for any
//!   volume gesture, so startup matches operator gestures
//!   (both effectively Linear) until the curve model is wired
//!   end-to-end.
//! - **Bounded retry**: MPD may not accept commands the instant
//!   the framework's supervisor comes up (post-restart of the
//!   `mpd` unit, cold-boot race with `mpd.service`). The
//!   applier retries with exponential backoff up to
//!   [`APPLY_DEADLINE`] before giving up and logging a WARN.
//!   Giving up is not a load failure — the framework has no
//!   way to force MPD past whatever is blocking it; the
//!   operator sees the stale volume and can rectify with any
//!   volume gesture.
//! - **Idempotent stop**: the handle's `stop` may be called
//!   twice (unload paths sometimes double-tap); the second call
//!   returns immediately.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;

use crate::mpd::{ConnectTimeouts, MpdConnection};
use crate::MpdEndpoint;

/// Plugin-log tag. Kept in sync with `PLUGIN_NAME` in `lib.rs`.
const PLUGIN_NAME: &str = "org.evoframework.playback.mpd";

/// Total wall-clock the applier is willing to spend waiting for
/// MPD before giving up. 60 s covers the worst-observed cold-
/// boot race on the reference target; MPD's own systemd unit
/// typically comes up within 3-5 s post-`multi-user.target`.
const APPLY_DEADLINE: Duration = Duration::from_secs(60);

/// Initial retry backoff between MPD-connect attempts.
const INITIAL_BACKOFF: Duration = Duration::from_millis(200);

/// Maximum retry backoff. Doubles from [`INITIAL_BACKOFF`] up
/// to this ceiling.
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// Operator-configured startup-volume pair. Extracted from the
/// audio-options settings subject state by the parser in
/// `lib.rs`. The applier reads this via a watch channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StartupVolume {
    /// The operator's declared startup volume (0..=100).
    pub startup_percent: u8,
    /// The operator's declared ceiling (0..=100).
    pub max_percent: u8,
}

impl StartupVolume {
    /// Effective startup value = min(startup, max). The operator
    /// may set `startup_volume_percent` higher than
    /// `max_volume_percent` at different times; the ceiling
    /// always wins.
    pub(crate) fn effective(&self) -> u8 {
        self.startup_percent.min(self.max_percent)
    }
}

/// Handle the plugin retains for the load's lifetime.
pub(crate) struct StartupVolumeApplierHandle {
    task: JoinHandle<()>,
    shutdown: Arc<Notify>,
}

impl StartupVolumeApplierHandle {
    /// Notify the task to shut down and await its exit.
    /// Idempotent — calling twice is safe.
    pub(crate) async fn stop(self) {
        self.shutdown.notify_waiters();
        let _ = self.task.await;
    }
}

/// Spawn the applier. Returns the handle the plugin retains.
///
/// `settings_state_rx` yields `Some(StartupVolume)` when the
/// options-settings subscriber has parsed a state payload. The
/// applier waits for the first `Some`, applies once, then
/// exits.
pub(crate) fn spawn(
    endpoint: MpdEndpoint,
    timeouts: ConnectTimeouts,
    settings_state_rx: watch::Receiver<Option<StartupVolume>>,
) -> StartupVolumeApplierHandle {
    let shutdown = Arc::new(Notify::new());
    let task_shutdown = Arc::clone(&shutdown);
    let task = tokio::spawn(async move {
        run(endpoint, timeouts, settings_state_rx, task_shutdown).await;
    });
    StartupVolumeApplierHandle { task, shutdown }
}

async fn run(
    endpoint: MpdEndpoint,
    timeouts: ConnectTimeouts,
    mut settings_state_rx: watch::Receiver<Option<StartupVolume>>,
    shutdown: Arc<Notify>,
) {
    tracing::info!(
        plugin = PLUGIN_NAME,
        endpoint = %endpoint,
        "startup-volume applier task started; awaiting options settings"
    );

    // Phase 1 — wait for the first Some(StartupVolume) from the
    // options-settings subscriber. This handles the plugin
    // admission order: playback.mpd admits before playback.options
    // on the reference distribution's discovery walk. The
    // subscriber inside lib.rs opens the subject subscription
    // with backoff; by the time it publishes a state we know
    // the options plugin is fully up.
    let startup = loop {
        tokio::select! {
            _ = shutdown.notified() => {
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    "startup-volume applier: shutdown received before \
                     options settings arrived; exiting"
                );
                return;
            }
            res = settings_state_rx.changed() => {
                if res.is_err() {
                    tracing::warn!(
                        plugin = PLUGIN_NAME,
                        "startup-volume applier: settings watch channel \
                         closed before publishing; exiting"
                    );
                    return;
                }
                if let Some(sv) = *settings_state_rx.borrow_and_update() {
                    break sv;
                }
            }
        }
    };

    let effective = startup.effective();
    tracing::info!(
        plugin = PLUGIN_NAME,
        startup_percent = startup.startup_percent,
        max_percent = startup.max_percent,
        effective_percent = effective,
        "startup-volume applier: options settings resolved; attempting apply"
    );

    // Phase 2 — apply + verify loop. The fragment-writer worker
    // recycles MPD on every observed audio_output change post-
    // boot; a bare `setvol` fired between the initial connect and
    // the fragment-writer's restart is wiped when MPD reloads
    // its state file. The applier compensates by:
    //   1. setvol
    //   2. read status back
    //   3. if status.volume == effective for [`STABILITY_WINDOW`]
    //      → done
    //   4. otherwise re-apply and try again
    //
    // Bounded by [`APPLY_DEADLINE`]; on timeout the applier logs
    // a WARN and gives up (operator can fix with any gesture).
    let deadline = Instant::now() + APPLY_DEADLINE;
    let mut backoff = INITIAL_BACKOFF;
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;

        match apply_and_verify_stable(&endpoint, timeouts, effective).await {
            Ok(()) => {
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    attempt,
                    effective_percent = effective,
                    stability_ms = STABILITY_WINDOW.as_millis() as u64,
                    "startup-volume applier: applied + verified stable; \
                     task exiting"
                );
                return;
            }
            Err(err) => {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    attempt,
                    error = %err,
                    effective_percent = effective,
                    "startup-volume applier: apply+verify cycle failed; \
                     will retry"
                );
            }
        }

        if Instant::now() >= deadline {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                attempts = attempt,
                deadline_secs = APPLY_DEADLINE.as_secs(),
                effective_percent = effective,
                "startup-volume applier: deadline elapsed before MPD's \
                 mixer stabilised at the effective value; giving up. \
                 Operator's next volume gesture takes effect normally; \
                 the missed startup apply is not retried until the next \
                 plugin load."
            );
            return;
        }

        tokio::select! {
            _ = shutdown.notified() => {
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    "startup-volume applier: shutdown received during \
                     retry loop; exiting"
                );
                return;
            }
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Duration the mixer must hold the effective value before the
/// applier declares success. Chosen larger than the observed
/// fragment-writer + MPD-restart window at boot: the
/// fragment-writer rewrites `mpd.conf` and restarts MPD twice
/// during initial plugin bring-up (worst-observed: 500 ms after
/// first setvol), so a 2 s window guarantees the applier
/// re-fires after any restart that wiped it and lands in a
/// steady state before returning.
const STABILITY_WINDOW: Duration = Duration::from_secs(2);

/// One poll interval inside the stability window.
const STABILITY_POLL_INTERVAL: Duration = Duration::from_millis(250);

async fn apply_and_verify_stable(
    endpoint: &MpdEndpoint,
    timeouts: ConnectTimeouts,
    effective: u8,
) -> Result<(), String> {
    // Apply.
    {
        let mut conn =
            MpdConnection::connect_with_timeouts(endpoint.clone(), timeouts)
                .await
                .map_err(|e| format!("connect: {e}"))?;
        conn.set_volume(effective)
            .await
            .map_err(|e| format!("setvol {effective}: {e}"))?;
    }

    // Verify — poll status every STABILITY_POLL_INTERVAL over
    // STABILITY_WINDOW. Every poll must report the effective
    // value. A single miss triggers a fresh apply cycle.
    let end = Instant::now() + STABILITY_WINDOW;
    while Instant::now() < end {
        tokio::time::sleep(STABILITY_POLL_INTERVAL).await;
        let mut conn =
            MpdConnection::connect_with_timeouts(endpoint.clone(), timeouts)
                .await
                .map_err(|e| format!("verify connect: {e}"))?;
        let status = conn
            .status()
            .await
            .map_err(|e| format!("verify status: {e}"))?;
        match status.volume {
            Some(v) if v == effective => continue,
            Some(other) => {
                return Err(format!(
                    "verify mismatch: expected {effective}, MPD reports \
                     {other} (mixer likely wiped by MPD restart / \
                     fragment-worker cycle)"
                ));
            }
            None => {
                return Err("verify mismatch: MPD reports volume=-1 (mixer \
                     disabled / unknown); retrying"
                    .into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_clamps_startup_at_max() {
        // Startup above max: max wins.
        let sv = StartupVolume {
            startup_percent: 80,
            max_percent: 50,
        };
        assert_eq!(sv.effective(), 50);
    }

    #[test]
    fn effective_returns_startup_when_below_max() {
        let sv = StartupVolume {
            startup_percent: 30,
            max_percent: 100,
        };
        assert_eq!(sv.effective(), 30);
    }

    #[test]
    fn effective_returns_zero_when_startup_is_zero() {
        let sv = StartupVolume {
            startup_percent: 0,
            max_percent: 100,
        };
        assert_eq!(sv.effective(), 0);
    }

    #[test]
    fn effective_saturates_at_max_when_both_are_zero() {
        let sv = StartupVolume {
            startup_percent: 0,
            max_percent: 0,
        };
        assert_eq!(sv.effective(), 0);
    }

    #[tokio::test]
    async fn applier_exits_on_shutdown_before_settings_arrive() {
        let (_settings_tx, settings_rx) = watch::channel(None);
        let handle = spawn(
            MpdEndpoint::Tcp {
                host: "127.0.0.1".to_string(),
                port: 6600,
            },
            ConnectTimeouts::default(),
            settings_rx,
        );
        // Immediately stop — the task never received a settings
        // value and never opened a socket. This proves the
        // shutdown branch fires before Phase 2.
        handle.stop().await;
    }

    #[tokio::test]
    async fn applier_exits_on_shutdown_during_retry_loop() {
        let (settings_tx, settings_rx) = watch::channel(None);
        let handle = spawn(
            // Deliberately unreachable endpoint: connect will
            // keep failing until the shutdown notification.
            MpdEndpoint::Tcp {
                host: "127.0.0.1".to_string(),
                port: 1, // reserved-not-listening
            },
            ConnectTimeouts::default(),
            settings_rx,
        );
        // Publish a startup value so the applier enters Phase 2.
        settings_tx
            .send(Some(StartupVolume {
                startup_percent: 30,
                max_percent: 100,
            }))
            .unwrap();
        // Give the task one tick to start attempting connect.
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.stop().await;
    }
}
