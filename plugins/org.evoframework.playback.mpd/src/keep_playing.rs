// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Restart MPD without leaving a playing player paused.
//!
//! MPD restores its queue paused (`restore_paused`), so every
//! `systemctl restart mpd` stops a player that was playing. Two
//! things in this plugin restart MPD on their own: the fragment
//! worker (a new `audio_output`) and the `/etc/asound.d/` watcher
//! (a new ALSA composition). At a steward start both can fire within
//! a second of each other, and whichever came last left the player
//! paused, with nobody to play it again.
//!
//! [`PlayKeepingRestarter`] wraps the plugin's restarter so that
//! every restart, from either, asks MPD on its own socket whether
//! it is playing, restarts, and plays again when it was. The asking
//! and the playing go to MPD itself, not to the custody, which a
//! steward that has just started has not taken yet. One gate runs
//! the whole sequence, so a second restart waits for the first to
//! finish playing again and then sees a playing player to keep.
//! A paused player stays paused.

use std::sync::Arc;
use std::time::Duration;

use crate::mpd::{ConnectTimeouts, MpdConnection, MpdEndpoint, PlayState};
use crate::mpd_restart::{MpdRestarter, RestartFuture};

/// MPD, spoken to directly on its own socket.
#[derive(Debug, Clone)]
pub(crate) struct DirectMpd {
    endpoint: MpdEndpoint,
    timeouts: ConnectTimeouts,
}

/// How long to wait for a restarted MPD to take connections.
const RESUME_CONNECT_TRIES: u32 = 40;
const RESUME_CONNECT_GAP: Duration = Duration::from_millis(500);

impl DirectMpd {
    pub(crate) fn new(
        endpoint: MpdEndpoint,
        timeouts: ConnectTimeouts,
    ) -> Self {
        Self { endpoint, timeouts }
    }

    async fn connect(&self) -> Result<MpdConnection, String> {
        MpdConnection::connect_with_timeouts(
            self.endpoint.clone(),
            self.timeouts,
        )
        .await
        .map_err(|e| e.to_string())
    }

    /// Whether MPD says it is playing. A socket that does not answer
    /// is not a playing player.
    pub(crate) async fn is_playing(&self) -> bool {
        let Ok(mut conn) = self.connect().await else {
            return false;
        };
        matches!(conn.status().await.map(|s| s.state), Ok(PlayState::Playing))
    }

    /// Play, once MPD takes connections again. A restored paused
    /// queue carries on from where it was.
    pub(crate) async fn resume(&self) -> Result<(), String> {
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

/// A restarter that keeps a playing player playing.
pub(crate) struct PlayKeepingRestarter {
    inner: Arc<dyn MpdRestarter>,
    mpd: DirectMpd,
    gate: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for PlayKeepingRestarter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlayKeepingRestarter")
            .field("inner", &self.inner)
            .finish()
    }
}

impl PlayKeepingRestarter {
    pub(crate) fn new(inner: Arc<dyn MpdRestarter>, mpd: DirectMpd) -> Self {
        Self {
            inner,
            mpd,
            gate: tokio::sync::Mutex::new(()),
        }
    }
}

impl MpdRestarter for PlayKeepingRestarter {
    fn restart(&self) -> RestartFuture<'_> {
        Box::pin(async move {
            let _one_at_a_time = self.gate.lock().await;
            let was_playing = self.mpd.is_playing().await;
            self.inner.restart().await?;
            if was_playing {
                match self.mpd.resume().await {
                    Ok(()) => tracing::info!(
                        plugin = crate::PLUGIN_NAME,
                        "playback resumed after the MPD restart"
                    ),
                    Err(e) => tracing::warn!(
                        plugin = crate::PLUGIN_NAME,
                        error = %e,
                        "playback did not resume after the MPD restart"
                    ),
                }
            }
            Ok(())
        })
    }

    fn strategy_name(&self) -> &'static str {
        self.inner.strategy_name()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;

    use super::*;
    use crate::mpd_restart::{FailingRestarter, NoOpRestarter};

    /// An MPD that holds a state. A `restart` puts it paused, as
    /// `restore_paused` does; `play` puts it playing.
    #[derive(Debug)]
    struct StubMpd {
        state: StdMutex<&'static str>,
        plays: AtomicUsize,
    }

    async fn stub_mpd(state: &'static str) -> (MpdEndpoint, Arc<StubMpd>) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let listener =
            tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stub = Arc::new(StubMpd {
            state: StdMutex::new(state),
            plays: AtomicUsize::new(0),
        });
        let shared = Arc::clone(&stub);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let stub = Arc::clone(&shared);
                tokio::spawn(async move {
                    let (r, mut w) = stream.into_split();
                    let _ = w.write_all(b"OK MPD 0.23.5\n").await;
                    let mut lines = BufReader::new(r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        if line.starts_with("status") {
                            let state = *stub.state.lock().unwrap();
                            let body = format!("state: {state}\nOK\n");
                            let _ = w.write_all(body.as_bytes()).await;
                        } else {
                            if line.starts_with("play") {
                                tokio::time::sleep(Duration::from_millis(40))
                                    .await;
                                stub.plays.fetch_add(1, Ordering::SeqCst);
                                *stub.state.lock().unwrap() = "play";
                            }
                            let _ = w.write_all(b"OK\n").await;
                        }
                    }
                });
            }
        });
        (
            MpdEndpoint::tcp(addr.ip().to_string(), addr.port()).unwrap(),
            stub,
        )
    }

    /// A restarter that does what a real restart does to MPD: the
    /// player comes back paused.
    #[derive(Debug)]
    struct PausingRestarter {
        stub: Arc<StubMpd>,
        calls: AtomicUsize,
    }

    impl MpdRestarter for PausingRestarter {
        fn restart(&self) -> RestartFuture<'_> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(50)).await;
                *self.stub.state.lock().unwrap() = "pause";
                Ok(())
            })
        }
        fn strategy_name(&self) -> &'static str {
            "pausing_test"
        }
    }

    fn keeping(
        endpoint: MpdEndpoint,
        inner: Arc<dyn MpdRestarter>,
    ) -> PlayKeepingRestarter {
        PlayKeepingRestarter::new(
            inner,
            DirectMpd::new(endpoint, ConnectTimeouts::default()),
        )
    }

    /// A playing player is playing again after the restart.
    #[tokio::test]
    async fn a_playing_player_is_played_again_after_the_restart() {
        let (endpoint, stub) = stub_mpd("play").await;
        let inner = Arc::new(PausingRestarter {
            stub: Arc::clone(&stub),
            calls: AtomicUsize::new(0),
        });
        let r = keeping(endpoint, inner.clone());
        r.restart().await.unwrap();
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
        assert_eq!(*stub.state.lock().unwrap(), "play");
        assert_eq!(stub.plays.load(Ordering::SeqCst), 1);
    }

    /// A paused player stays paused.
    #[tokio::test]
    async fn a_paused_player_stays_paused() {
        let (endpoint, stub) = stub_mpd("pause").await;
        let inner = Arc::new(PausingRestarter {
            stub: Arc::clone(&stub),
            calls: AtomicUsize::new(0),
        });
        let r = keeping(endpoint, inner.clone());
        r.restart().await.unwrap();
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
        assert_eq!(*stub.state.lock().unwrap(), "pause");
        assert_eq!(stub.plays.load(Ordering::SeqCst), 0);
    }

    /// The fragment worker and the asound watcher restart MPD a
    /// second apart at a steward start. The gate runs them one at a
    /// time, so the second sees a playing player and keeps it.
    #[tokio::test]
    async fn two_restarts_at_once_still_end_with_the_player_playing() {
        let (endpoint, stub) = stub_mpd("play").await;
        let inner = Arc::new(PausingRestarter {
            stub: Arc::clone(&stub),
            calls: AtomicUsize::new(0),
        });
        let r = Arc::new(keeping(endpoint, inner.clone()));
        let (a, b) = (Arc::clone(&r), Arc::clone(&r));
        // The second arrives while the first is between its restart
        // and its play: without the gate it reads a paused player,
        // restarts after the first has played, and nobody plays it.
        let (ra, rb) = tokio::join!(
            tokio::spawn(async move { a.restart().await }),
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(60)).await;
                b.restart().await
            }),
        );
        ra.unwrap().unwrap();
        rb.unwrap().unwrap();
        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
        assert_eq!(*stub.state.lock().unwrap(), "play");
        assert_eq!(stub.plays.load(Ordering::SeqCst), 2);
    }

    /// A refused restart is the inner restarter's error, and plays
    /// nothing.
    #[tokio::test]
    async fn a_refused_restart_is_passed_on_and_plays_nothing() {
        let (endpoint, stub) = stub_mpd("play").await;
        let r = keeping(endpoint, Arc::new(FailingRestarter::new("refused")));
        assert_eq!(r.restart().await, Err("refused".to_string()));
        assert_eq!(stub.plays.load(Ordering::SeqCst), 0);
    }

    /// No MPD to ask is no player to keep: the restart still goes.
    #[tokio::test]
    async fn with_no_mpd_to_ask_the_restart_still_goes() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        let endpoint =
            MpdEndpoint::tcp(addr.ip().to_string(), addr.port()).unwrap();
        let inner = Arc::new(NoOpRestarter::new());
        let r = keeping(endpoint, inner.clone());
        r.restart().await.unwrap();
        assert_eq!(inner.call_count(), 1);
    }
}
