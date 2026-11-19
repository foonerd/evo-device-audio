// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Protocol-settings applier.
//!
//! Keeps MPD playing with the operator's ReplayGain mode,
//! crossfade and single mode for as long as the plugin is
//! loaded, whether or not a playback custody is live.
//!
//! ## Why
//!
//! MPD forgets its ReplayGain mode every time it restarts: its
//! state file brings back crossfade and single mode, but not the
//! ReplayGain mode, which starts again at `off`. MPD restarts at
//! every steward start (the fragment writer recycles it as it
//! renders the output) and on every route change, and no custody
//! need be live when it does. A custody's supervisor applies the
//! settings once, as the custody starts; this task applies them
//! to every MPD session and on every change.
//!
//! ## How
//!
//! - One connection held in `idle` marks an MPD session. When it
//!   drops, MPD's session has ended; the task reconnects, with
//!   backoff, and each new session gets the settings again.
//! - The settings are sent on a short-lived connection: at each
//!   new session once the operator's settings have arrived, and
//!   on every change. A transport failure reconnects and sends
//!   them again, within a bounded budget. A send the budget does
//!   not cover is made by the next session or the next change.
//! - Until the operator's settings have arrived nothing is sent,
//!   and MPD keeps what it restored.
//! - A change made to these options by anything else (the
//!   playback shelf's `set_single`, the test tone) wakes the idle
//!   connection and is left as it is.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::futures::OwnedNotified;
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;

use crate::mpd::{
    ConnectTimeouts, IdleSubsystem, MpdConnection, MpdEndpoint, MpdError,
};
use crate::{AudioProtocolSettings, PLUGIN_NAME};

/// How long one `idle` call may wait. MPD answers `idle` only when
/// something changes, so the wait is long: the session normally
/// ends by MPD dropping the connection, not by this budget.
const IDLE_BUDGET: Duration = Duration::from_secs(86_400);

/// First pause before connecting again or sending again.
const INITIAL_BACKOFF: Duration = Duration::from_millis(200);

/// Longest pause before connecting again or sending again.
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// Sends tried per session or change before giving up on it:
/// about 16 s of pauses, longer than MPD takes to come back from
/// a restart.
const SEND_ATTEMPTS: u32 = 8;

/// Handle the plugin keeps for the load's lifetime.
pub(crate) struct ProtocolSettingsApplierHandle {
    task: JoinHandle<()>,
    shutdown: Arc<Notify>,
}

impl ProtocolSettingsApplierHandle {
    /// Stop the task and wait for it to end.
    pub(crate) async fn stop(self) {
        self.shutdown.notify_waiters();
        let _ = self.task.await;
    }
}

/// Start the applier.
///
/// `settings` carries `None` until the operator's settings have
/// arrived. A [`ProtocolSettingsApplierHandle::stop`] at any time
/// after this returns, even before the task first runs, ends it.
pub(crate) fn spawn(
    endpoint: MpdEndpoint,
    timeouts: ConnectTimeouts,
    settings: watch::Receiver<Option<AudioProtocolSettings>>,
) -> ProtocolSettingsApplierHandle {
    let shutdown = Arc::new(Notify::new());
    // Made here, before the task exists: a waiter sees every
    // `notify_waiters` call made after it is created.
    let mut stop = Box::pin(Arc::clone(&shutdown).notified_owned());
    stop.as_mut().enable();
    let task = tokio::spawn(run(endpoint, timeouts, settings, stop));
    ProtocolSettingsApplierHandle { task, shutdown }
}

async fn run(
    endpoint: MpdEndpoint,
    timeouts: ConnectTimeouts,
    mut settings: watch::Receiver<Option<AudioProtocolSettings>>,
    mut stop: Pin<Box<OwnedNotified>>,
) {
    let mut settings_follow = true;
    loop {
        let Some(mut session) =
            connect(&endpoint, timeouts, stop.as_mut()).await
        else {
            return;
        };
        let current = *settings.borrow_and_update();
        if let Some(wanted) = current {
            if !send(&endpoint, timeouts, wanted, "session", stop.as_mut())
                .await
            {
                return;
            }
        }

        // Hold the session until MPD ends it.
        loop {
            let ended = {
                let idle = session.idle(&[IdleSubsystem::Options], IDLE_BUDGET);
                tokio::pin!(idle);
                loop {
                    tokio::select! {
                        biased;
                        _ = stop.as_mut() => return,
                        changed = settings.changed(), if settings_follow => {
                            if changed.is_err() {
                                // The plugin dropped the sender; it
                                // is unloading and stops this task
                                // next.
                                settings_follow = false;
                                continue;
                            }
                            let current = *settings.borrow_and_update();
                            if let Some(wanted) = current {
                                if !send(
                                    &endpoint,
                                    timeouts,
                                    wanted,
                                    "change",
                                    stop.as_mut(),
                                )
                                .await
                                {
                                    return;
                                }
                            }
                        }
                        result = &mut idle => break result,
                    }
                }
            };
            match ended {
                // Something changed these options; it stands.
                Ok(_) => continue,
                Err(e) => {
                    tracing::debug!(
                        plugin = PLUGIN_NAME,
                        error = %e,
                        "protocol-settings applier: MPD session ended; \
                         reconnecting"
                    );
                    break;
                }
            }
        }
    }
}

/// Connect for a new MPD session, retrying with backoff until MPD
/// answers. `None` when stopped first.
async fn connect(
    endpoint: &MpdEndpoint,
    timeouts: ConnectTimeouts,
    mut stop: Pin<&mut OwnedNotified>,
) -> Option<MpdConnection> {
    let mut backoff = INITIAL_BACKOFF;
    loop {
        tokio::select! {
            biased;
            _ = stop.as_mut() => return None,
            result = MpdConnection::connect_with_timeouts(
                endpoint.clone(),
                timeouts,
            ) => match result {
                Ok(conn) => return Some(conn),
                Err(e) => tracing::debug!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "protocol-settings applier: MPD not answering; \
                     trying again"
                ),
            },
        }
        tokio::select! {
            biased;
            _ = stop.as_mut() => return None,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Send `wanted` to MPD, connecting again and resending on a
/// transport failure. `false` when stopped first.
async fn send(
    endpoint: &MpdEndpoint,
    timeouts: ConnectTimeouts,
    wanted: AudioProtocolSettings,
    reason: &'static str,
    mut stop: Pin<&mut OwnedNotified>,
) -> bool {
    let mut backoff = INITIAL_BACKOFF;
    for attempt in 1..=SEND_ATTEMPTS {
        let outcome = tokio::select! {
            biased;
            _ = stop.as_mut() => return false,
            outcome = send_once(endpoint, timeouts, wanted) => outcome,
        };
        match outcome {
            Ok(()) => {
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    reason,
                    attempt,
                    replay_gain_mode = wanted.replay_gain_mode,
                    crossfade_seconds = wanted.crossfade_seconds,
                    gapless = wanted.gapless,
                    "protocol-settings applier: MPD has the operator's \
                     ReplayGain, crossfade and single mode"
                );
                return true;
            }
            Err(e) if attempt < SEND_ATTEMPTS && is_transport(&e) => {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    reason,
                    attempt,
                    error = %e,
                    "protocol-settings applier: send failed; sending again"
                );
                tokio::select! {
                    biased;
                    _ = stop.as_mut() => return false,
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            Err(e) => {
                // LOGGING.md §2: warn. Operator-visible: until the
                // next MPD session or the next change, MPD plays
                // with a ReplayGain mode, crossfade or single mode
                // other than the one the operator chose.
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    reason,
                    attempts = attempt,
                    replay_gain_mode = wanted.replay_gain_mode,
                    error = %e,
                    "protocol-settings applier: MPD did not take the \
                     operator's settings; the next MPD session or change \
                     sends them again"
                );
                return true;
            }
        }
    }
    true
}

/// One connection, all three settings. A transport failure ends
/// the attempt so it can be made again on a fresh connection; MPD
/// refusing one setting is logged and the others are still sent.
async fn send_once(
    endpoint: &MpdEndpoint,
    timeouts: ConnectTimeouts,
    wanted: AudioProtocolSettings,
) -> Result<(), MpdError> {
    let mut conn =
        MpdConnection::connect_with_timeouts(endpoint.clone(), timeouts)
            .await?;
    refused_or(
        conn.set_crossfade(wanted.crossfade_seconds).await,
        "crossfade",
    )?;
    // `gapless` on is MPD `single 0`: play on through the queue.
    refused_or(conn.set_single(!wanted.gapless).await, "single")?;
    refused_or(
        conn.set_replay_gain_mode(wanted.replay_gain_mode).await,
        "replay_gain_mode",
    )?;
    Ok(())
}

/// Pass a transport failure up; log MPD refusing the setting and
/// carry on.
fn refused_or(
    result: Result<(), MpdError>,
    setting: &'static str,
) -> Result<(), MpdError> {
    match result {
        Err(e) if is_transport(&e) => Err(e),
        Err(e) => {
            // LOGGING.md §2: warn. Operator-visible: MPD refused
            // this one setting, so it does not take effect.
            tracing::warn!(
                plugin = PLUGIN_NAME,
                setting,
                error = %e,
                "protocol-settings applier: MPD refused the setting"
            );
            Ok(())
        }
        Ok(()) => Ok(()),
    }
}

fn is_transport(e: &MpdError) -> bool {
    matches!(e, MpdError::Transport(_) | MpdError::Timeout { .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playback_supervisor::test_mock::short_timeouts;
    use crate::{REPLAY_GAIN_ALBUM, REPLAY_GAIN_OFF, REPLAY_GAIN_TRACK};
    use std::sync::Mutex;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::broadcast;

    /// An MPD that records every command it is sent, holds `idle`,
    /// and can be restarted: a restart closes every open
    /// connection, and new ones are served as before.
    struct MockMpd {
        endpoint: MpdEndpoint,
        commands: Arc<Mutex<Vec<String>>>,
        restart: broadcast::Sender<()>,
        close_on: Arc<Mutex<Option<String>>>,
    }

    impl MockMpd {
        async fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let endpoint =
                MpdEndpoint::tcp(addr.ip().to_string(), addr.port()).unwrap();
            let commands = Arc::new(Mutex::new(Vec::new()));
            let (restart, _) = broadcast::channel(4);
            let close_on = Arc::new(Mutex::new(None));
            let (c, r, o) = (
                Arc::clone(&commands),
                restart.clone(),
                Arc::clone(&close_on),
            );
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    tokio::spawn(serve(
                        stream,
                        Arc::clone(&c),
                        r.subscribe(),
                        Arc::clone(&o),
                    ));
                }
            });
            Self {
                endpoint,
                commands,
                restart,
                close_on,
            }
        }

        /// MPD restarts: every open connection is closed.
        fn restart(&self) {
            let _ = self.restart.send(());
        }

        /// The next time `command` arrives, the connection closes
        /// without an answer, as when MPD goes down mid-command.
        fn close_on_next(&self, command: &str) {
            *self.close_on.lock().unwrap() = Some(command.to_string());
        }

        fn sent(&self) -> Vec<String> {
            self.commands.lock().unwrap().clone()
        }

        fn count(&self, command: &str) -> usize {
            self.sent().iter().filter(|c| *c == command).count()
        }

        /// Wait until `command` has been sent `times` times.
        async fn sent_times(&self, command: &str, times: usize) -> bool {
            for _ in 0..250 {
                if self.count(command) >= times {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            false
        }
    }

    async fn serve(
        stream: TcpStream,
        commands: Arc<Mutex<Vec<String>>>,
        mut restart: broadcast::Receiver<()>,
        close_on: Arc<Mutex<Option<String>>>,
    ) {
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        if write.write_all(b"OK MPD 0.24.0\n").await.is_err() {
            return;
        }
        loop {
            let line = tokio::select! {
                _ = restart.recv() => return,
                line = lines.next_line() => match line {
                    Ok(Some(line)) => line,
                    _ => return,
                },
            };
            commands.lock().unwrap().push(line.clone());
            {
                let mut close = close_on.lock().unwrap();
                if close.as_deref() == Some(line.as_str()) {
                    *close = None;
                    return;
                }
            }
            if line.starts_with("idle") {
                // Held until MPD restarts.
                let _ = restart.recv().await;
                return;
            }
            let reply = match line.as_str() {
                "status" => {
                    "volume: 30\nrepeat: 0\nrandom: 0\nsingle: 0\n\
                             consume: 0\nplaylist: 1\nplaylistlength: 0\n\
                             state: stop\nOK\n"
                }
                _ => "OK\n",
            };
            if write.write_all(reply.as_bytes()).await.is_err() {
                return;
            }
        }
    }

    fn settings(replay_gain_mode: &'static str) -> AudioProtocolSettings {
        AudioProtocolSettings {
            replay_gain_mode,
            ..AudioProtocolSettings::audiophile_default()
        }
    }

    fn mode(m: &str) -> String {
        format!("replay_gain_mode \"{m}\"")
    }

    /// What owns the protocol settings while a pin runs: the
    /// applier, as the plugin starts it at load.
    async fn start_owner(
        mpd: &MockMpd,
        settings: watch::Receiver<Option<AudioProtocolSettings>>,
    ) -> ProtocolSettingsApplierHandle {
        spawn(mpd.endpoint.clone(), short_timeouts(), settings)
    }

    /// With no custody live, the stored mode reaches MPD. After a
    /// reboot nothing is playing under a custody yet, and MPD
    /// starts with ReplayGain `off`.
    #[tokio::test]
    async fn the_stored_mode_reaches_mpd_with_no_custody() {
        let mpd = MockMpd::start().await;
        let (tx, rx) = watch::channel(Some(settings(REPLAY_GAIN_ALBUM)));
        let owner = start_owner(&mpd, rx).await;
        assert!(
            mpd.sent_times(&mode("album"), 1).await,
            "MPD must be given the stored mode: {:?}",
            mpd.sent()
        );
        owner.stop().await;
        drop(tx);
    }

    /// Nothing is sent until the operator's settings have arrived;
    /// then they are.
    #[tokio::test]
    async fn nothing_is_sent_before_the_operator_settings_arrive() {
        let mpd = MockMpd::start().await;
        let (tx, rx) = watch::channel(None);
        let owner = start_owner(&mpd, rx).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        let early = mpd.sent();
        assert!(
            early.iter().all(|c| !c.starts_with("replay_gain_mode")
                && !c.starts_with("crossfade")
                && !c.starts_with("single")),
            "nothing may be sent before the settings arrive: {early:?}"
        );
        tx.send_replace(Some(settings(REPLAY_GAIN_TRACK)));
        assert!(
            mpd.sent_times(&mode("track"), 1).await,
            "the settings must be sent once they arrive: {:?}",
            mpd.sent()
        );
        owner.stop().await;
    }

    /// MPD restarts and forgets the mode; the new session gets it
    /// again.
    #[tokio::test]
    async fn an_mpd_restart_gets_the_mode_again() {
        let mpd = MockMpd::start().await;
        let (tx, rx) = watch::channel(Some(settings(REPLAY_GAIN_ALBUM)));
        let owner = start_owner(&mpd, rx).await;
        assert!(mpd.sent_times(&mode("album"), 1).await, "{:?}", mpd.sent());
        mpd.restart();
        assert!(
            mpd.sent_times(&mode("album"), 2).await,
            "the mode must be sent again after MPD restarts: {:?}",
            mpd.sent()
        );
        owner.stop().await;
        drop(tx);
    }

    /// A change that meets a connection MPD has closed is sent
    /// again, not lost.
    #[tokio::test]
    async fn a_change_that_meets_a_closed_connection_is_sent_again() {
        let mpd = MockMpd::start().await;
        let (tx, rx) = watch::channel(Some(settings(REPLAY_GAIN_OFF)));
        let owner = start_owner(&mpd, rx).await;
        assert!(mpd.sent_times(&mode("off"), 1).await, "{:?}", mpd.sent());
        mpd.close_on_next(&mode("track"));
        tx.send_replace(Some(settings(REPLAY_GAIN_TRACK)));
        assert!(
            mpd.sent_times(&mode("track"), 2).await,
            "a change cut off by a closed connection must be sent again: \
             {:?}",
            mpd.sent()
        );
        owner.stop().await;
    }

    /// A stop right after the applier starts ends it.
    #[tokio::test]
    async fn a_stop_right_after_start_ends_the_applier() {
        let mpd = MockMpd::start().await;
        let (_tx, rx) = watch::channel(Some(settings(REPLAY_GAIN_OFF)));
        let owner = spawn(mpd.endpoint.clone(), short_timeouts(), rx);
        tokio::time::timeout(Duration::from_secs(5), owner.stop())
            .await
            .expect("the applier ended on the stop");
    }
}
