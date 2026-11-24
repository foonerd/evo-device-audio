// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! The only code in the renderer that touches the player.
//!
//! A controller's commands become `audio.playback` verbs, sent to the
//! `playback.mpd` plugin by peer dispatch. This module never speaks to
//! MPD itself, and the renderer holds no copy of what is playing: it
//! reads the state back from the player every time it needs it.

use std::sync::Arc;

use async_trait::async_trait;
use evo_plugin_sdk::contract::shelf_dispatch::{
    ShelfDispatchError, ShelfRequestDispatcher,
};
use serde_json::{json, Value};

/// The shelf the transport verbs live on.
pub const PLAYBACK_SHELF: &str = "audio.playback";

/// Where the player's transport stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Audio is playing.
    Playing,
    /// Paused.
    Paused,
    /// Stopped, or nothing loaded.
    Stopped,
}

/// The player's play-order settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Modes {
    /// Repeat the queue.
    pub repeat: bool,
    /// Shuffle: `off`, `tracks`, `albums` or `similar`.
    pub shuffle: String,
    /// Play a single track and stop.
    pub single: bool,
}

impl Modes {
    /// Plain play order: a track plays once and the transport then stops.
    pub fn plain() -> Self {
        Self {
            repeat: false,
            shuffle: "off".into(),
            single: false,
        }
    }
}

/// What the player reports it is doing.
#[derive(Debug, Clone, PartialEq)]
pub struct NowPlaying {
    /// Transport state.
    pub transport: Transport,
    /// Track title, when the player knows one.
    pub title: Option<String>,
    /// Track artist.
    pub artist: Option<String>,
    /// Track album.
    pub album: Option<String>,
    /// The player's own identifier for the track (a path or a stream URL).
    pub track_uri: Option<String>,
    /// Position in the track, in milliseconds.
    pub elapsed_ms: Option<u64>,
    /// Length of the track, in milliseconds.
    pub duration_ms: Option<u64>,
    /// Volume, 0 to 100.
    pub volume: u8,
    /// Whether the output is muted.
    pub muted: bool,
    /// The play-order settings.
    pub modes: Modes,
}

/// Why a player call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BridgeError {
    /// The player cannot be reached right now (no dispatcher, no plugin,
    /// a transient failure). Retrying later may work.
    #[error("player unavailable: {0}")]
    Unavailable(String),
    /// The player refused the command.
    #[error("player refused: {0}")]
    Refused(String),
}

/// What a renderer needs from the player.
#[async_trait]
pub trait Player: Send + Sync {
    /// Load `uri` and start playing it. `uri` is an `http(s)` URL.
    async fn play_uri(&self, uri: &str) -> Result<(), BridgeError>;
    /// Resume after a pause.
    async fn resume(&self) -> Result<(), BridgeError>;
    /// Pause.
    async fn pause(&self) -> Result<(), BridgeError>;
    /// Stop.
    async fn stop(&self) -> Result<(), BridgeError>;
    /// Next queue entry.
    async fn next(&self) -> Result<(), BridgeError>;
    /// Previous queue entry.
    async fn previous(&self) -> Result<(), BridgeError>;
    /// Seek to an absolute position.
    async fn seek(&self, position_ms: u64) -> Result<(), BridgeError>;
    /// Set the volume, 0 to 100.
    async fn set_volume(&self, volume: u8) -> Result<(), BridgeError>;
    /// Mute or unmute.
    async fn set_mute(&self, muted: bool) -> Result<(), BridgeError>;
    /// Read the current state.
    async fn now_playing(&self) -> Result<NowPlaying, BridgeError>;
    /// Set the play-order settings. A renderer session uses
    /// [`Modes::plain`], so a track plays once and the transport stops
    /// (UPnP's `NORMAL` play mode, and how a controller learns a track
    /// ended), and puts the operator's settings back when it ends.
    async fn apply_modes(&self, modes: &Modes) -> Result<(), BridgeError>;
}

/// The player when none is wired: every call is `Unavailable`.
pub struct NoPlayer;

fn none() -> BridgeError {
    BridgeError::Unavailable("no player is wired to this renderer".into())
}

#[async_trait]
impl Player for NoPlayer {
    async fn play_uri(&self, _: &str) -> Result<(), BridgeError> {
        Err(none())
    }
    async fn resume(&self) -> Result<(), BridgeError> {
        Err(none())
    }
    async fn pause(&self) -> Result<(), BridgeError> {
        Err(none())
    }
    async fn stop(&self) -> Result<(), BridgeError> {
        Err(none())
    }
    async fn next(&self) -> Result<(), BridgeError> {
        Err(none())
    }
    async fn previous(&self) -> Result<(), BridgeError> {
        Err(none())
    }
    async fn seek(&self, _: u64) -> Result<(), BridgeError> {
        Err(none())
    }
    async fn set_volume(&self, _: u8) -> Result<(), BridgeError> {
        Err(none())
    }
    async fn set_mute(&self, _: bool) -> Result<(), BridgeError> {
        Err(none())
    }
    async fn now_playing(&self) -> Result<NowPlaying, BridgeError> {
        Err(none())
    }
    async fn apply_modes(&self, _: &Modes) -> Result<(), BridgeError> {
        Err(none())
    }
}

/// The real player: `audio.playback` verbs by peer dispatch.
pub struct PeerPlayer {
    dispatcher: Arc<dyn ShelfRequestDispatcher>,
}

impl PeerPlayer {
    /// Wrap the load context's dispatcher.
    pub fn new(dispatcher: Arc<dyn ShelfRequestDispatcher>) -> Self {
        Self { dispatcher }
    }

    async fn call(
        &self,
        verb: &str,
        body: Value,
    ) -> Result<Vec<u8>, BridgeError> {
        let payload = serde_json::to_vec(&body)
            .map_err(|e| BridgeError::Refused(format!("encode {verb}: {e}")))?;
        self.dispatcher
            .dispatch(PLAYBACK_SHELF, verb, payload, None)
            .await
            .map_err(|e| map_dispatch_error(verb, e))
    }
}

fn map_dispatch_error(verb: &str, e: ShelfDispatchError) -> BridgeError {
    match e {
        ShelfDispatchError::Transient { detail }
        | ShelfDispatchError::SubstrateFailure { detail } => {
            BridgeError::Unavailable(format!("{verb}: {detail}"))
        }
        ShelfDispatchError::DeadlineExceeded { budget_ms } => {
            BridgeError::Unavailable(format!(
                "{verb}: over the {budget_ms} ms budget"
            ))
        }
        ShelfDispatchError::NoPluginOnShelf { shelf } => {
            BridgeError::Unavailable(format!(
                "{verb}: no plugin on shelf {shelf}"
            ))
        }
        other => BridgeError::Refused(format!("{verb}: {other}")),
    }
}

#[async_trait]
impl Player for PeerPlayer {
    async fn play_uri(&self, uri: &str) -> Result<(), BridgeError> {
        self.call("play_now", json!({ "v": 1, "uri": uri }))
            .await
            .map(|_| ())
    }
    async fn resume(&self) -> Result<(), BridgeError> {
        self.call("resume", json!({ "v": 1 })).await.map(|_| ())
    }
    async fn pause(&self) -> Result<(), BridgeError> {
        self.call("pause", json!({ "v": 1 })).await.map(|_| ())
    }
    async fn stop(&self) -> Result<(), BridgeError> {
        self.call("stop", json!({ "v": 1 })).await.map(|_| ())
    }
    async fn next(&self) -> Result<(), BridgeError> {
        self.call("next", json!({ "v": 1 })).await.map(|_| ())
    }
    async fn previous(&self) -> Result<(), BridgeError> {
        self.call("previous", json!({ "v": 1 })).await.map(|_| ())
    }
    async fn seek(&self, position_ms: u64) -> Result<(), BridgeError> {
        self.call("seek", json!({ "v": 1, "position_ms": position_ms }))
            .await
            .map(|_| ())
    }
    async fn set_volume(&self, volume: u8) -> Result<(), BridgeError> {
        self.call("set_volume", json!({ "v": 1, "volume": volume.min(100) }))
            .await
            .map(|_| ())
    }
    async fn set_mute(&self, muted: bool) -> Result<(), BridgeError> {
        self.call("set_mute", json!({ "v": 1, "enabled": muted }))
            .await
            .map(|_| ())
    }
    async fn now_playing(&self) -> Result<NowPlaying, BridgeError> {
        let bytes = self.call("get_now_playing", json!({ "v": 1 })).await?;
        parse_now_playing(&bytes)
    }
    async fn apply_modes(&self, m: &Modes) -> Result<(), BridgeError> {
        self.call("set_repeat", json!({ "v": 1, "enabled": m.repeat }))
            .await?;
        self.call("set_shuffle", json!({ "v": 1, "mode": m.shuffle }))
            .await?;
        self.call("set_single", json!({ "v": 1, "enabled": m.single }))
            .await?;
        Ok(())
    }
}

/// Read the player's `get_now_playing` envelope.
pub fn parse_now_playing(bytes: &[u8]) -> Result<NowPlaying, BridgeError> {
    let v: Value = serde_json::from_slice(bytes).map_err(|e| {
        BridgeError::Refused(format!("now_playing is not JSON: {e}"))
    })?;
    let transport = match v.get("transport_state").and_then(Value::as_str) {
        Some("playing") => Transport::Playing,
        Some("paused") => Transport::Paused,
        _ => Transport::Stopped,
    };
    let text = |k: &str| {
        v.get("track")
            .and_then(|t| t.get(k))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let ms = |k: &str| v.get(k).and_then(Value::as_u64);
    Ok(NowPlaying {
        transport,
        title: text("title"),
        artist: text("artist"),
        album: text("album"),
        track_uri: text("mpd_path"),
        elapsed_ms: ms("elapsed_ms"),
        duration_ms: ms("duration_ms"),
        volume: v
            .get("volume")
            .and_then(Value::as_u64)
            .map(|n| n.min(100) as u8)
            .unwrap_or(0),
        muted: v.get("muted").and_then(Value::as_bool).unwrap_or(false),
        modes: Modes {
            repeat: v.get("repeat").and_then(Value::as_bool).unwrap_or(false),
            shuffle: v
                .get("shuffle_mode")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    if v.get("shuffle")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        "tracks".into()
                    } else {
                        "off".into()
                    }
                }),
            single: v.get("single").and_then(Value::as_bool).unwrap_or(false),
        },
    })
}

#[cfg(test)]
pub(crate) mod fake {
    //! A recording player for the control and event tests.
    use super::*;
    use std::sync::Mutex;

    /// Records every call and answers `now_playing` from a settable state.
    pub struct FakePlayer {
        pub calls: Mutex<Vec<String>>,
        pub state: Mutex<NowPlaying>,
        pub fail: Mutex<Option<BridgeError>>,
    }

    impl FakePlayer {
        pub fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                state: Mutex::new(NowPlaying {
                    transport: Transport::Stopped,
                    title: None,
                    artist: None,
                    album: None,
                    track_uri: None,
                    elapsed_ms: None,
                    duration_ms: None,
                    volume: 30,
                    muted: false,
                    modes: Modes {
                        repeat: true,
                        shuffle: "tracks".into(),
                        single: false,
                    },
                }),
                fail: Mutex::new(None),
            })
        }
        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        pub fn set_transport(&self, t: Transport) {
            self.state.lock().unwrap().transport = t;
        }
        fn rec(&self, c: String) -> Result<(), BridgeError> {
            if let Some(e) = self.fail.lock().unwrap().clone() {
                return Err(e);
            }
            self.calls.lock().unwrap().push(c);
            Ok(())
        }
    }

    #[async_trait]
    impl Player for FakePlayer {
        async fn play_uri(&self, uri: &str) -> Result<(), BridgeError> {
            self.rec(format!("play_uri {uri}"))?;
            let mut s = self.state.lock().unwrap();
            s.transport = Transport::Playing;
            s.track_uri = Some(uri.to_string());
            s.elapsed_ms = Some(0);
            s.duration_ms = Some(180_000);
            Ok(())
        }
        async fn resume(&self) -> Result<(), BridgeError> {
            self.rec("resume".into())?;
            self.state.lock().unwrap().transport = Transport::Playing;
            Ok(())
        }
        async fn pause(&self) -> Result<(), BridgeError> {
            self.rec("pause".into())?;
            self.state.lock().unwrap().transport = Transport::Paused;
            Ok(())
        }
        async fn stop(&self) -> Result<(), BridgeError> {
            self.rec("stop".into())?;
            self.state.lock().unwrap().transport = Transport::Stopped;
            Ok(())
        }
        async fn next(&self) -> Result<(), BridgeError> {
            self.rec("next".into())
        }
        async fn previous(&self) -> Result<(), BridgeError> {
            self.rec("previous".into())
        }
        async fn seek(&self, ms: u64) -> Result<(), BridgeError> {
            self.rec(format!("seek {ms}"))?;
            self.state.lock().unwrap().elapsed_ms = Some(ms);
            Ok(())
        }
        async fn set_volume(&self, v: u8) -> Result<(), BridgeError> {
            self.rec(format!("set_volume {v}"))?;
            self.state.lock().unwrap().volume = v;
            Ok(())
        }
        async fn set_mute(&self, m: bool) -> Result<(), BridgeError> {
            self.rec(format!("set_mute {m}"))?;
            self.state.lock().unwrap().muted = m;
            Ok(())
        }
        async fn now_playing(&self) -> Result<NowPlaying, BridgeError> {
            if let Some(e) = self.fail.lock().unwrap().clone() {
                return Err(e);
            }
            Ok(self.state.lock().unwrap().clone())
        }
        async fn apply_modes(&self, m: &Modes) -> Result<(), BridgeError> {
            self.rec(format!(
                "modes repeat={} shuffle={} single={}",
                m.repeat, m.shuffle, m.single
            ))?;
            self.state.lock().unwrap().modes = m.clone();
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_players_now_playing_envelope_is_read() {
        let body = br#"{"v":1,"transport_state":"playing","track":{"title":"T","artist":"A","album":"B","mpd_path":"http://x/y.flac"},"elapsed_ms":1500,"duration_ms":240000,"volume":42,"muted":true,"repeat":true,"shuffle":true,"shuffle_mode":"albums","single":false}"#;
        let n = parse_now_playing(body).unwrap();
        assert_eq!(n.transport, Transport::Playing);
        assert_eq!(n.title.as_deref(), Some("T"));
        assert_eq!(n.track_uri.as_deref(), Some("http://x/y.flac"));
        assert_eq!(n.elapsed_ms, Some(1500));
        assert_eq!(n.duration_ms, Some(240_000));
        assert_eq!(n.volume, 42);
        assert!(n.muted);
        assert_eq!(
            n.modes,
            Modes {
                repeat: true,
                shuffle: "albums".into(),
                single: false
            }
        );
    }

    #[test]
    fn a_stopped_player_with_no_track_reads_as_stopped() {
        let body = br#"{"v":1,"transport_state":"stopped","track":null,"elapsed_ms":null,"duration_ms":null,"volume":0,"muted":false}"#;
        let n = parse_now_playing(body).unwrap();
        assert_eq!(n.transport, Transport::Stopped);
        assert!(n.title.is_none() && n.track_uri.is_none());
        assert_eq!(n.elapsed_ms, None);
    }

    #[test]
    fn an_unknown_state_or_a_garbage_body_is_safe() {
        let n =
            parse_now_playing(br#"{"transport_state":"buffering"}"#).unwrap();
        assert_eq!(n.transport, Transport::Stopped);
        assert!(parse_now_playing(b"not json").is_err());
    }

    #[test]
    fn dispatch_failures_split_into_unavailable_and_refused() {
        let t = map_dispatch_error(
            "x",
            ShelfDispatchError::Transient { detail: "d".into() },
        );
        assert!(matches!(t, BridgeError::Unavailable(_)));
        let n = map_dispatch_error(
            "x",
            ShelfDispatchError::NoPluginOnShelf {
                shelf: "audio.playback".into(),
            },
        );
        assert!(matches!(n, BridgeError::Unavailable(_)));
        let p = map_dispatch_error(
            "x",
            ShelfDispatchError::Permanent {
                detail: "no".into(),
            },
        );
        assert!(matches!(p, BridgeError::Refused(_)));
    }

    #[tokio::test]
    async fn no_player_is_unavailable_everywhere() {
        let p = NoPlayer;
        assert!(matches!(p.pause().await, Err(BridgeError::Unavailable(_))));
        assert!(matches!(
            p.now_playing().await,
            Err(BridgeError::Unavailable(_))
        ));
    }
}
