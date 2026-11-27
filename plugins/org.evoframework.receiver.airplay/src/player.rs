// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! The only code in the receiver that touches the player.
//!
//! What a sender streams plays through the local player as one queue
//! item that the receiver adds and removes itself. The receiver does that
//! with `audio.playback` verbs, sent to the `playback.mpd` plugin by peer
//! dispatch. This module never speaks to MPD itself, and the receiver
//! holds no copy of what is playing.
//!
//! The verbs name a capture from the player's closed table (`airplay`);
//! the receiver never supplies a device or a URI.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use evo_plugin_sdk::contract::shelf_dispatch::{
    ShelfDispatchError, ShelfRequestDispatcher,
};
use serde_json::{json, Value};

/// The shelf the verbs live on.
pub const PLAYBACK_SHELF: &str = "audio.playback";

/// The capture the player knows this receiver by.
pub const CAPTURE: &str = "airplay";

/// Longest title, artist or album the player takes, in characters.
pub const TAG_CHARS: usize = 256;

/// How long one player call may take before the player is called
/// unavailable.
pub const CALL_BUDGET: Duration = Duration::from_secs(5);

/// What a track is called, as the sender last said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tags {
    /// Track title.
    pub title: Option<String>,
    /// Track artist.
    pub artist: Option<String>,
    /// Track album.
    pub album: Option<String>,
}

impl Tags {
    /// Tags from what a sender sent: control characters dropped, the
    /// ends trimmed, anything longer than the player takes cut, and an
    /// empty string no tag at all.
    pub fn from_metadata(
        title: Option<&str>,
        artist: Option<&str>,
        album: Option<&str>,
    ) -> Tags {
        Tags {
            title: tidy(title),
            artist: tidy(artist),
            album: tidy(album),
        }
    }

    fn payload(&self) -> Value {
        let mut body = json!({ "v": 1, "capture": CAPTURE });
        for (key, value) in [
            ("title", &self.title),
            ("artist", &self.artist),
            ("album", &self.album),
        ] {
            if let Some(value) = value {
                body[key] = Value::String(value.clone());
            }
        }
        body
    }
}

fn tidy(text: Option<&str>) -> Option<String> {
    let cleaned: String = text?.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return None;
    }
    Some(cleaned.chars().take(TAG_CHARS).collect())
}

/// Why a player call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlayerError {
    /// Nothing is writing to the capture, so the player will not play it.
    #[error("nothing is writing to the capture")]
    Idle,
    /// The capture is not in the player's queue: the operator ended it.
    #[error("the capture is not in the player")]
    Absent,
    /// The player cannot be reached right now.
    #[error("player unavailable: {0}")]
    Unavailable(String),
    /// The player refused the request.
    #[error("player refused: {0}")]
    Refused(String),
}

/// What the receiver needs from the player.
#[async_trait]
pub trait Player: Send + Sync {
    /// Put the capture at the end of the queue, tag it and play it.
    async fn play_capture(&self, tags: &Tags) -> Result<(), PlayerError>;
    /// Replace the capture's tags. Refused with [`PlayerError::Absent`]
    /// when the capture is no longer in the queue.
    async fn set_capture_tags(&self, tags: &Tags) -> Result<(), PlayerError>;
    /// Take the capture out of the queue. Succeeds when it is not there.
    async fn end_capture(&self) -> Result<(), PlayerError>;
    /// Set the device volume, 0 to 100.
    async fn set_volume(&self, volume: u8) -> Result<(), PlayerError>;
    /// Mute or unmute the device.
    async fn set_mute(&self, muted: bool) -> Result<(), PlayerError>;
}

/// The player when none is wired: every call is `Unavailable`, so a stream
/// is refused rather than accepted and silenced.
pub struct NoPlayer;

fn none() -> PlayerError {
    PlayerError::Unavailable("no player is wired to this receiver".into())
}

#[async_trait]
impl Player for NoPlayer {
    async fn play_capture(&self, _: &Tags) -> Result<(), PlayerError> {
        Err(none())
    }
    async fn set_capture_tags(&self, _: &Tags) -> Result<(), PlayerError> {
        Err(none())
    }
    async fn end_capture(&self) -> Result<(), PlayerError> {
        Err(none())
    }
    async fn set_volume(&self, _: u8) -> Result<(), PlayerError> {
        Err(none())
    }
    async fn set_mute(&self, _: bool) -> Result<(), PlayerError> {
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

    async fn call(&self, verb: &str, body: Value) -> Result<(), PlayerError> {
        let payload = serde_json::to_vec(&body)
            .map_err(|e| PlayerError::Refused(format!("encode {verb}: {e}")))?;
        let sent =
            self.dispatcher
                .dispatch(PLAYBACK_SHELF, verb, payload, None);
        match tokio::time::timeout(CALL_BUDGET, sent).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(map_dispatch_error(verb, e)),
            Err(_) => Err(PlayerError::Unavailable(format!(
                "{verb}: no answer within {} s",
                CALL_BUDGET.as_secs()
            ))),
        }
    }
}

/// Sort a dispatch failure into what the receiver can do about it. The
/// player says why in words; the two the receiver acts on are matched.
pub fn map_dispatch_error(verb: &str, e: ShelfDispatchError) -> PlayerError {
    match e {
        ShelfDispatchError::Transient { detail } => {
            if detail.contains("capture_idle") {
                PlayerError::Idle
            } else if detail.contains("capture_absent") {
                PlayerError::Absent
            } else {
                PlayerError::Unavailable(format!("{verb}: {detail}"))
            }
        }
        ShelfDispatchError::SubstrateFailure { detail } => {
            PlayerError::Unavailable(format!("{verb}: {detail}"))
        }
        ShelfDispatchError::DeadlineExceeded { budget_ms } => {
            PlayerError::Unavailable(format!(
                "{verb}: over the {budget_ms} ms budget"
            ))
        }
        ShelfDispatchError::NoPluginOnShelf { shelf } => {
            PlayerError::Unavailable(format!(
                "{verb}: no plugin on shelf {shelf}"
            ))
        }
        other => PlayerError::Refused(format!("{verb}: {other}")),
    }
}

#[async_trait]
impl Player for PeerPlayer {
    async fn play_capture(&self, tags: &Tags) -> Result<(), PlayerError> {
        self.call("play_capture", tags.payload()).await
    }
    async fn set_capture_tags(&self, tags: &Tags) -> Result<(), PlayerError> {
        self.call("set_capture_tags", tags.payload()).await
    }
    async fn end_capture(&self) -> Result<(), PlayerError> {
        self.call("end_capture", json!({ "v": 1, "capture": CAPTURE }))
            .await
    }
    async fn set_volume(&self, volume: u8) -> Result<(), PlayerError> {
        self.call("set_volume", json!({ "v": 1, "volume": volume.min(100) }))
            .await
    }
    async fn set_mute(&self, muted: bool) -> Result<(), PlayerError> {
        self.call("set_mute", json!({ "v": 1, "enabled": muted }))
            .await
    }
}

/// A recording player for the receiver's tests.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::sync::Mutex;

    /// One call the receiver made.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Call {
        Play(Tags),
        Tags(Tags),
        End,
        Volume(u8),
        Mute(bool),
    }

    /// Records every call; each verb can be told to fail.
    #[derive(Default)]
    pub struct FakePlayer {
        pub calls: Mutex<Vec<Call>>,
        pub play_error: Mutex<Option<PlayerError>>,
        pub tags_error: Mutex<Option<PlayerError>>,
        /// Make ending the capture fail.
        pub end_error: Mutex<Option<PlayerError>>,
        /// How long ending the capture takes, so a test can tell the
        /// receiver waiting for it from something else ending it.
        pub end_delay: Mutex<Duration>,
    }

    impl FakePlayer {
        pub fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        pub fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        pub fn count(&self, wanted: impl Fn(&Call) -> bool) -> usize {
            self.calls().iter().filter(|c| wanted(c)).count()
        }

        fn record(&self, call: Call) {
            self.calls.lock().unwrap().push(call);
        }
    }

    #[async_trait]
    impl Player for FakePlayer {
        async fn play_capture(&self, tags: &Tags) -> Result<(), PlayerError> {
            self.record(Call::Play(tags.clone()));
            match self.play_error.lock().unwrap().clone() {
                Some(e) => Err(e),
                None => Ok(()),
            }
        }
        async fn set_capture_tags(
            &self,
            tags: &Tags,
        ) -> Result<(), PlayerError> {
            self.record(Call::Tags(tags.clone()));
            match self.tags_error.lock().unwrap().clone() {
                Some(e) => Err(e),
                None => Ok(()),
            }
        }
        async fn end_capture(&self) -> Result<(), PlayerError> {
            let delay = *self.end_delay.lock().unwrap();
            tokio::time::sleep(delay).await;
            self.record(Call::End);
            match self.end_error.lock().unwrap().clone() {
                Some(e) => Err(e),
                None => Ok(()),
            }
        }
        async fn set_volume(&self, volume: u8) -> Result<(), PlayerError> {
            self.record(Call::Volume(volume));
            Ok(())
        }
        async fn set_mute(&self, muted: bool) -> Result<(), PlayerError> {
            self.record(Call::Mute(muted));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records what was dispatched and answers as told.
    struct Recorder {
        sent: Mutex<Vec<(String, String, Value)>>,
        answer: Mutex<Option<ShelfDispatchError>>,
    }

    impl Recorder {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                sent: Mutex::new(Vec::new()),
                answer: Mutex::new(None),
            })
        }
    }

    impl ShelfRequestDispatcher for Recorder {
        fn dispatch<'a>(
            &'a self,
            shelf: &'a str,
            request_type: &'a str,
            payload: Vec<u8>,
            _instance_id: Option<&'a str>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<Vec<u8>, ShelfDispatchError>,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                self.sent.lock().unwrap().push((
                    shelf.to_string(),
                    request_type.to_string(),
                    serde_json::from_slice(&payload).unwrap(),
                ));
                match self.answer.lock().unwrap().take() {
                    Some(e) => Err(e),
                    None => Ok(b"{}".to_vec()),
                }
            })
        }
    }

    #[test]
    fn tags_are_tidied_trimmed_and_bounded() {
        let long = "é".repeat(TAG_CHARS + 50);
        let t = Tags::from_metadata(
            Some("  Song\u{0}\n "),
            Some(""),
            Some(long.as_str()),
        );
        assert_eq!(t.title.as_deref(), Some("Song"));
        assert_eq!(t.artist, None, "empty is no tag");
        assert_eq!(
            t.album.as_deref().map(|a| a.chars().count()),
            Some(TAG_CHARS)
        );
    }

    #[tokio::test]
    async fn the_verbs_name_the_capture_and_never_a_device() {
        let d = Recorder::new();
        let player = PeerPlayer::new(d.clone());
        player
            .play_capture(&Tags::from_metadata(Some("T"), Some("A"), None))
            .await
            .unwrap();
        player.set_capture_tags(&Tags::default()).await.unwrap();
        player.end_capture().await.unwrap();
        player.set_volume(250).await.unwrap();
        player.set_mute(true).await.unwrap();
        let sent = d.sent.lock().unwrap().clone();
        let verbs: Vec<&str> = sent.iter().map(|s| s.1.as_str()).collect();
        assert_eq!(
            verbs,
            [
                "play_capture",
                "set_capture_tags",
                "end_capture",
                "set_volume",
                "set_mute"
            ]
        );
        for (shelf, verb, body) in &sent {
            assert_eq!(shelf, PLAYBACK_SHELF);
            assert_eq!(body["v"], 1, "{verb}");
            if verb.contains("capture") {
                assert_eq!(body["capture"], CAPTURE, "{verb}");
                let keys: Vec<&String> =
                    body.as_object().unwrap().keys().collect();
                for k in keys {
                    assert!(
                        ["v", "capture", "title", "artist", "album"]
                            .contains(&k.as_str()),
                        "{verb} carries {k}, which is not a display tag"
                    );
                }
            }
        }
        assert_eq!(sent[0].2["title"], "T");
        assert_eq!(sent[0].2["artist"], "A");
        assert!(sent[0].2.get("album").is_none());
        assert_eq!(sent[3].2["volume"], 100, "volume is capped at 100");
        assert_eq!(sent[4].2["enabled"], true);
    }

    #[test]
    fn what_the_player_says_is_sorted_into_what_the_receiver_can_do() {
        let t = |d: &str| ShelfDispatchError::Transient { detail: d.into() };
        assert_eq!(
            map_dispatch_error("v", t("capture_idle")),
            PlayerError::Idle
        );
        assert_eq!(
            map_dispatch_error("v", t("transient error: capture_absent")),
            PlayerError::Absent
        );
        assert!(matches!(
            map_dispatch_error("v", t("mpd away")),
            PlayerError::Unavailable(_)
        ));
        assert!(matches!(
            map_dispatch_error(
                "v",
                ShelfDispatchError::NoPluginOnShelf { shelf: "s".into() }
            ),
            PlayerError::Unavailable(_)
        ));
        assert!(matches!(
            map_dispatch_error(
                "v",
                ShelfDispatchError::Permanent {
                    detail: "no".into()
                }
            ),
            PlayerError::Refused(_)
        ));
    }

    #[tokio::test]
    async fn no_player_refuses_everything_so_a_stream_is_never_silenced() {
        let p = NoPlayer;
        assert!(matches!(
            p.play_capture(&Tags::default()).await,
            Err(PlayerError::Unavailable(_))
        ));
        assert!(p.end_capture().await.is_err());
    }
}
