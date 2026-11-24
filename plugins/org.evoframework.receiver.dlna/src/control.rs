// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! The control actions of the three services.
//!
//! A control point sets a URI and presses play. The renderer keeps only
//! what the protocol makes it keep: the URI and metadata the controller
//! set, and whether it has asked the player to load that URI. Everything
//! else is read back from the player when asked. A track the operator
//! started locally in the meantime is not this renderer's, and a
//! controller is told the transport is stopped, not shown someone else's
//! playback as its own.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::bridge::{BridgeError, Modes, NowPlaying, Player, Transport};
use crate::description::service;
use crate::soap::{response_xml, Action, Fault};

/// Largest URI accepted.
pub const MAX_URI: usize = 2048;
/// Largest metadata document accepted.
pub const MAX_META: usize = 64 * 1024;

/// The sink protocols advertised: what a controller may send.
pub const SINK_PROTOCOLS: &str = "http-get:*:audio/mpeg:*,\
http-get:*:audio/mp3:*,\
http-get:*:audio/mp4:*,\
http-get:*:audio/x-m4a:*,\
http-get:*:audio/aac:*,\
http-get:*:audio/flac:*,\
http-get:*:audio/x-flac:*,\
http-get:*:audio/ogg:*,\
http-get:*:audio/x-ogg:*,\
http-get:*:audio/opus:*,\
http-get:*:audio/wav:*,\
http-get:*:audio/x-wav:*,\
http-get:*:audio/L16:*,\
http-get:*:audio/x-aiff:*,\
http-get:*:audio/x-ms-wma:*,\
http-get:*:audio/*:*";

/// What the controller set.
#[derive(Debug, Default, Clone)]
pub struct Session {
    /// Current URI (empty when none).
    pub uri: String,
    /// Its metadata, echoed back to the controller.
    pub meta: String,
    /// Next URI.
    pub next_uri: String,
    /// Its metadata.
    pub next_meta: String,
    /// Whether the renderer asked the player to load `uri`.
    pub loaded: bool,
    /// The operator's play-order settings, kept while a session runs in plain
    /// order and put back when it ends.
    pub saved_modes: Option<Modes>,
}

/// A view of the AVTransport state for events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvSnapshot {
    /// `STOPPED`, `PLAYING`, `PAUSED_PLAYBACK` or `NO_MEDIA_PRESENT`.
    pub transport_state: &'static str,
    /// Current URI.
    pub uri: String,
    /// Current metadata.
    pub meta: String,
    /// Next URI.
    pub next_uri: String,
    /// Track duration as `H:MM:SS`.
    pub duration: String,
    /// Comma-separated actions currently possible.
    pub actions: &'static str,
}

/// The renderer's control state.
pub struct Control {
    player: Arc<dyn Player>,
    session: Mutex<Session>,
}

/// The last player failure logged and when, so a controller polling a dead
/// player produces one warning a minute, not one per request.
static LAST_FAILURE: std::sync::Mutex<Option<(String, std::time::Instant)>> =
    std::sync::Mutex::new(None);

fn failed(e: BridgeError) -> Fault {
    let msg = e.to_string();
    let mut last = LAST_FAILURE.lock().expect("failure log");
    let fresh = match last.as_ref() {
        Some((m, at)) => {
            *m != msg || at.elapsed() > std::time::Duration::from_secs(60)
        }
        None => true,
    };
    if fresh {
        tracing::warn!(
            error = %msg,
            "a renderer control action failed at the player; the controller \
             is told Action Failed"
        );
        *last = Some((msg, std::time::Instant::now()));
    }
    Fault::ACTION_FAILED
}

/// `H:MM:SS` for a duration.
pub fn fmt_hms(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

/// Parse `H:MM:SS` or `H:MM:SS.FFF` into milliseconds.
pub fn parse_hms(s: &str) -> Option<u64> {
    let s = s.trim().trim_start_matches('+');
    let (whole, frac) = match s.split_once('.') {
        Some((w, f)) => (w, Some(f)),
        None => (s, None),
    };
    let parts: Vec<&str> = whole.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: u64 = parts[0].parse().ok()?;
    let m: u64 = parts[1].parse().ok()?;
    let sec: u64 = parts[2].parse().ok()?;
    if m > 59 || sec > 59 {
        return None;
    }
    let mut ms = ((h * 60 + m) * 60 + sec) * 1000;
    if let Some(f) = frac {
        // Fractions may be decimal ("5") or "F0/F1"; only decimal digits count.
        let f = f.split('/').next().unwrap_or("");
        if !f.is_empty() {
            let digits: String = f.chars().take(3).collect();
            if !digits.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            let scale = 10u64.pow(3 - digits.len() as u32);
            ms += digits.parse::<u64>().ok()? * scale;
        }
    }
    Some(ms)
}

fn valid_uri(uri: &str) -> bool {
    uri.len() <= MAX_URI
        && (uri.starts_with("http://") || uri.starts_with("https://"))
        && uri.len() > "http://".len()
        && !uri.chars().any(|c| c.is_control())
}

fn arg<'a>(a: &'a Action, name: &str) -> Result<&'a str, Fault> {
    a.args
        .get(name)
        .map(String::as_str)
        .ok_or(Fault::INVALID_ARGS)
}

fn instance_zero(a: &Action) -> Result<(), Fault> {
    match arg(a, "InstanceID")?.trim() {
        "0" => Ok(()),
        _ => Err(Fault::INVALID_INSTANCE),
    }
}

impl Control {
    /// A control state over `player`.
    pub fn new(player: Arc<dyn Player>) -> Arc<Self> {
        Arc::new(Self {
            player,
            session: Mutex::new(Session::default()),
        })
    }

    /// Whether the player is playing what the controller set.
    fn owned(s: &Session, now: &NowPlaying) -> bool {
        s.loaded
            && !s.uri.is_empty()
            && now.track_uri.as_deref() == Some(s.uri.as_str())
    }

    fn transport_word(s: &Session, now: Option<&NowPlaying>) -> &'static str {
        if s.uri.is_empty() {
            return "NO_MEDIA_PRESENT";
        }
        match now {
            Some(n) if Self::owned(s, n) => match n.transport {
                Transport::Playing => "PLAYING",
                Transport::Paused => "PAUSED_PLAYBACK",
                Transport::Stopped => "STOPPED",
            },
            _ => "STOPPED",
        }
    }

    fn actions_for(word: &str, has_next: bool) -> &'static str {
        match (word, has_next) {
            ("PLAYING", true) => "Pause,Stop,Seek,Next",
            ("PLAYING", false) => "Pause,Stop,Seek",
            ("PAUSED_PLAYBACK", _) => "Play,Stop,Seek",
            ("STOPPED", _) => "Play",
            _ => "",
        }
    }

    /// Load `uri` into the player in plain play order. The operator's
    /// settings are kept the first time and put back by [`Control::end`].
    /// The mode change is best effort: a player that refuses it still plays
    /// the track.
    async fn load(&self, uri: &str) -> Result<(), BridgeError> {
        let keep = self.session.lock().await.saved_modes.is_none();
        if keep {
            if let Ok(now) = self.player.now_playing().await {
                self.session.lock().await.saved_modes = Some(now.modes);
            }
        }
        if let Err(e) = self.player.apply_modes(&Modes::plain()).await {
            tracing::debug!(error = %e, "could not set plain play order");
        }
        self.player.play_uri(uri).await
    }

    /// The session is over: put the operator's play-order settings back.
    pub async fn end(&self) {
        let saved = self.session.lock().await.saved_modes.take();
        if let Some(m) = saved {
            if let Err(e) = self.player.apply_modes(&m).await {
                tracing::debug!(error = %e, "could not restore the play order");
            }
        }
    }

    /// The AVTransport state, for events and for the info actions.
    pub async fn av_snapshot(&self) -> AvSnapshot {
        let s = self.session.lock().await.clone();
        let now = self.player.now_playing().await.ok();
        let word = Self::transport_word(&s, now.as_ref());
        let owned = now.as_ref().is_some_and(|n| Self::owned(&s, n));
        let duration = now
            .as_ref()
            .filter(|_| owned)
            .and_then(|n| n.duration_ms)
            .map(fmt_hms)
            .unwrap_or_else(|| "0:00:00".to_string());
        AvSnapshot {
            transport_state: word,
            actions: Self::actions_for(word, !s.next_uri.is_empty()),
            uri: s.uri,
            meta: s.meta,
            next_uri: s.next_uri,
            duration,
        }
    }

    /// Volume and mute, for events. `None` when the player cannot be read.
    pub async fn rc_snapshot(&self) -> Option<(u8, bool)> {
        self.player
            .now_playing()
            .await
            .ok()
            .map(|n| (n.volume, n.muted))
    }

    /// Advance the session when the player has finished the track: play the
    /// next URI if the controller queued one, otherwise settle to stopped.
    /// Called on a short cadence while a session is loaded.
    pub async fn tick(&self) {
        let (uri, loaded) = {
            let s = self.session.lock().await;
            (s.uri.clone(), s.loaded)
        };
        if !loaded {
            return;
        }
        let Ok(now) = self.player.now_playing().await else {
            return;
        };
        let still_ours = now.track_uri.as_deref() == Some(uri.as_str());
        if still_ours && now.transport != Transport::Stopped {
            return;
        }
        let mut s = self.session.lock().await;
        if !s.loaded || s.uri != uri {
            return;
        }
        if !s.next_uri.is_empty() {
            let next = std::mem::take(&mut s.next_uri);
            let meta = std::mem::take(&mut s.next_meta);
            s.uri = next.clone();
            s.meta = meta;
            drop(s);
            if self.load(&next).await.is_err() {
                self.session.lock().await.loaded = false;
                self.end().await;
            }
        } else {
            s.loaded = false;
            drop(s);
            self.end().await;
        }
    }

    /// Run one action and return the response envelope.
    pub async fn handle(
        &self,
        service_name: &str,
        a: &Action,
    ) -> Result<String, Fault> {
        let def = service(service_name).ok_or(Fault::INVALID_ACTION)?;
        let out = match service_name {
            "ConnectionManager" => self.connection_manager(a)?,
            "RenderingControl" => self.rendering_control(a).await?,
            "AVTransport" => self.av_transport(a).await?,
            _ => return Err(Fault::INVALID_ACTION),
        };
        Ok(response_xml(def.service_type, &a.name, &out))
    }

    fn connection_manager(
        &self,
        a: &Action,
    ) -> Result<Vec<(&'static str, String)>, Fault> {
        match a.name.as_str() {
            "GetProtocolInfo" => Ok(vec![
                ("Source", String::new()),
                ("Sink", SINK_PROTOCOLS.to_string()),
            ]),
            "GetCurrentConnectionIDs" => {
                Ok(vec![("ConnectionIDs", "0".into())])
            }
            "GetCurrentConnectionInfo" => {
                if arg(a, "ConnectionID")?.trim() != "0" {
                    return Err(Fault {
                        code: 706,
                        description: "Invalid connection reference",
                    });
                }
                Ok(vec![
                    ("RcsID", "0".into()),
                    ("AVTransportID", "0".into()),
                    ("ProtocolInfo", String::new()),
                    ("PeerConnectionManager", String::new()),
                    ("PeerConnectionID", "-1".into()),
                    ("Direction", "Input".into()),
                    ("Status", "OK".into()),
                ])
            }
            _ => Err(Fault::INVALID_ACTION),
        }
    }

    async fn rendering_control(
        &self,
        a: &Action,
    ) -> Result<Vec<(&'static str, String)>, Fault> {
        match a.name.as_str() {
            "ListPresets" => {
                instance_zero(a)?;
                Ok(vec![("CurrentPresetNameList", "FactoryDefaults".into())])
            }
            "SelectPreset" => {
                instance_zero(a)?;
                match arg(a, "PresetName")?.trim() {
                    "FactoryDefaults" => Ok(vec![]),
                    _ => Err(Fault {
                        code: 701,
                        description: "Invalid Name",
                    }),
                }
            }
            "GetVolume" | "GetMute" | "SetVolume" | "SetMute" => {
                instance_zero(a)?;
                if arg(a, "Channel")?.trim() != "Master" {
                    return Err(Fault::INVALID_ARGS);
                }
                match a.name.as_str() {
                    "GetVolume" => {
                        let n =
                            self.player.now_playing().await.map_err(failed)?;
                        Ok(vec![("CurrentVolume", n.volume.to_string())])
                    }
                    "GetMute" => {
                        let n =
                            self.player.now_playing().await.map_err(failed)?;
                        Ok(vec![(
                            "CurrentMute",
                            if n.muted { "1" } else { "0" }.into(),
                        )])
                    }
                    "SetVolume" => {
                        let v: u32 = arg(a, "DesiredVolume")?
                            .trim()
                            .parse()
                            .map_err(|_| Fault::INVALID_ARGS)?;
                        if v > 100 {
                            return Err(Fault::INVALID_ARGS);
                        }
                        self.player
                            .set_volume(v as u8)
                            .await
                            .map_err(failed)?;
                        Ok(vec![])
                    }
                    _ => {
                        let m = match arg(a, "DesiredMute")?.trim() {
                            "1" | "true" | "True" => true,
                            "0" | "false" | "False" => false,
                            _ => return Err(Fault::INVALID_ARGS),
                        };
                        self.player.set_mute(m).await.map_err(failed)?;
                        Ok(vec![])
                    }
                }
            }
            _ => Err(Fault::INVALID_ACTION),
        }
    }

    async fn av_transport(
        &self,
        a: &Action,
    ) -> Result<Vec<(&'static str, String)>, Fault> {
        instance_zero(a)?;
        match a.name.as_str() {
            "SetAVTransportURI" => {
                let uri = arg(a, "CurrentURI")?.trim().to_string();
                let meta = a
                    .args
                    .get("CurrentURIMetaData")
                    .cloned()
                    .unwrap_or_default();
                if meta.len() > MAX_META
                    || (!uri.is_empty() && !valid_uri(&uri))
                {
                    return Err(Fault::INVALID_ARGS);
                }
                let was_ours = {
                    let s = self.session.lock().await;
                    s.loaded
                };
                if was_ours {
                    // A new URI replaces what this renderer was playing.
                    let _ = self.player.stop().await;
                    self.end().await;
                }
                let mut s = self.session.lock().await;
                s.uri = uri;
                s.meta = meta;
                s.loaded = false;
                s.next_uri.clear();
                s.next_meta.clear();
                Ok(vec![])
            }
            "SetNextAVTransportURI" => {
                let uri = arg(a, "NextURI")?.trim().to_string();
                let meta =
                    a.args.get("NextURIMetaData").cloned().unwrap_or_default();
                if meta.len() > MAX_META
                    || (!uri.is_empty() && !valid_uri(&uri))
                {
                    return Err(Fault::INVALID_ARGS);
                }
                let mut s = self.session.lock().await;
                s.next_uri = uri;
                s.next_meta = meta;
                Ok(vec![])
            }
            "Play" => {
                match arg(a, "Speed")?.trim() {
                    "1" | "" => {}
                    _ => return Err(Fault::INVALID_ARGS),
                }
                let (uri, loaded) = {
                    let s = self.session.lock().await;
                    (s.uri.clone(), s.loaded)
                };
                if uri.is_empty() {
                    return Err(Fault::NO_CONTENTS);
                }
                let now = self.player.now_playing().await.map_err(failed)?;
                let ours =
                    loaded && now.track_uri.as_deref() == Some(uri.as_str());
                match (ours, now.transport) {
                    (true, Transport::Playing) => {}
                    (true, Transport::Paused) => {
                        self.player.resume().await.map_err(failed)?;
                    }
                    _ => {
                        self.load(&uri).await.map_err(failed)?;
                        self.session.lock().await.loaded = true;
                    }
                }
                Ok(vec![])
            }
            "Pause" => {
                let s = self.session.lock().await.clone();
                let now = self.player.now_playing().await.map_err(failed)?;
                if !Self::owned(&s, &now) {
                    return Err(Fault::TRANSITION_NOT_AVAILABLE);
                }
                match now.transport {
                    Transport::Playing => {
                        self.player.pause().await.map_err(failed)?
                    }
                    Transport::Paused => {}
                    Transport::Stopped => {
                        return Err(Fault::TRANSITION_NOT_AVAILABLE)
                    }
                }
                Ok(vec![])
            }
            "Stop" => {
                let s = self.session.lock().await.clone();
                let now = self.player.now_playing().await.map_err(failed)?;
                if Self::owned(&s, &now) && now.transport != Transport::Stopped
                {
                    self.player.stop().await.map_err(failed)?;
                }
                self.session.lock().await.loaded = false;
                self.end().await;
                Ok(vec![])
            }
            "Seek" => {
                let unit = arg(a, "Unit")?.trim().to_string();
                let target = arg(a, "Target")?.trim().to_string();
                let ms = match unit.as_str() {
                    "REL_TIME" | "ABS_TIME" => {
                        parse_hms(&target).ok_or(Fault::SEEK_TARGET)?
                    }
                    "TRACK_NR" => match target.as_str() {
                        "1" => 0,
                        _ => return Err(Fault::SEEK_TARGET),
                    },
                    _ => return Err(Fault::SEEK_MODE),
                };
                let s = self.session.lock().await.clone();
                let now = self.player.now_playing().await.map_err(failed)?;
                if !Self::owned(&s, &now) || now.transport == Transport::Stopped
                {
                    return Err(Fault::TRANSITION_NOT_AVAILABLE);
                }
                if now.duration_ms.is_some_and(|d| ms > d) {
                    return Err(Fault::SEEK_TARGET);
                }
                self.player.seek(ms).await.map_err(failed)?;
                Ok(vec![])
            }
            "Next" => {
                let (next, meta) = {
                    let s = self.session.lock().await;
                    (s.next_uri.clone(), s.next_meta.clone())
                };
                if next.is_empty() {
                    return Err(Fault::TRANSITION_NOT_AVAILABLE);
                }
                self.load(&next).await.map_err(failed)?;
                let mut s = self.session.lock().await;
                s.uri = next;
                s.meta = meta;
                s.next_uri.clear();
                s.next_meta.clear();
                s.loaded = true;
                Ok(vec![])
            }
            "Previous" => {
                let s = self.session.lock().await.clone();
                let now = self.player.now_playing().await.map_err(failed)?;
                if !Self::owned(&s, &now) || now.transport == Transport::Stopped
                {
                    return Err(Fault::TRANSITION_NOT_AVAILABLE);
                }
                self.player.seek(0).await.map_err(failed)?;
                Ok(vec![])
            }
            "SetPlayMode" => match arg(a, "NewPlayMode")?.trim() {
                "NORMAL" => Ok(vec![]),
                _ => Err(Fault {
                    code: 712,
                    description: "Play mode not supported",
                }),
            },
            "GetTransportInfo" => {
                let snap = self.av_snapshot().await;
                Ok(vec![
                    ("CurrentTransportState", snap.transport_state.into()),
                    ("CurrentTransportStatus", "OK".into()),
                    ("CurrentSpeed", "1".into()),
                ])
            }
            "GetMediaInfo" => {
                let snap = self.av_snapshot().await;
                let s = self.session.lock().await.clone();
                Ok(vec![
                    (
                        "NrTracks",
                        if s.uri.is_empty() { "0" } else { "1" }.into(),
                    ),
                    ("MediaDuration", snap.duration),
                    ("CurrentURI", s.uri),
                    ("CurrentURIMetaData", s.meta),
                    ("NextURI", s.next_uri),
                    ("NextURIMetaData", s.next_meta),
                    ("PlayMedium", "NETWORK".into()),
                    ("RecordMedium", "NOT_IMPLEMENTED".into()),
                    ("WriteStatus", "NOT_IMPLEMENTED".into()),
                ])
            }
            "GetPositionInfo" => {
                let s = self.session.lock().await.clone();
                let now = self.player.now_playing().await.ok();
                let ours = now.as_ref().filter(|n| Self::owned(&s, n));
                let rel = ours
                    .and_then(|n| n.elapsed_ms)
                    .map(fmt_hms)
                    .unwrap_or_else(|| "0:00:00".into());
                let dur = ours
                    .and_then(|n| n.duration_ms)
                    .map(fmt_hms)
                    .unwrap_or_else(|| "0:00:00".into());
                Ok(vec![
                    ("Track", if s.uri.is_empty() { "0" } else { "1" }.into()),
                    ("TrackDuration", dur),
                    ("TrackMetaData", s.meta),
                    ("TrackURI", s.uri),
                    ("RelTime", rel.clone()),
                    ("AbsTime", rel),
                    ("RelCount", "2147483647".into()),
                    ("AbsCount", "2147483647".into()),
                ])
            }
            "GetDeviceCapabilities" => Ok(vec![
                ("PlayMedia", "NETWORK".into()),
                ("RecMedia", "NOT_IMPLEMENTED".into()),
                ("RecQualityModes", "NOT_IMPLEMENTED".into()),
            ]),
            "GetTransportSettings" => Ok(vec![
                ("PlayMode", "NORMAL".into()),
                ("RecQualityMode", "NOT_IMPLEMENTED".into()),
            ]),
            "GetCurrentTransportActions" => {
                let snap = self.av_snapshot().await;
                Ok(vec![("Actions", snap.actions.into())])
            }
            _ => Err(Fault::INVALID_ACTION),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::fake::FakePlayer;
    use std::collections::HashMap;

    fn act(name: &str, args: &[(&str, &str)]) -> Action {
        Action {
            name: name.into(),
            namespace: String::new(),
            args: args
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<HashMap<_, _>>(),
        }
    }

    const URL: &str = "http://192.168.1.5:9000/a.flac";

    async fn av(
        c: &Control,
        name: &str,
        args: &[(&str, &str)],
    ) -> Result<String, Fault> {
        let mut v = vec![("InstanceID", "0")];
        v.extend_from_slice(args);
        c.handle("AVTransport", &act(name, &v)).await
    }

    fn field(xml: &str, name: &str) -> String {
        let doc = roxmltree::Document::parse(xml).unwrap();
        doc.descendants()
            .find(|n| n.has_tag_name(name))
            .and_then(|n| n.text())
            .unwrap_or("")
            .to_string()
    }

    #[test]
    fn times_parse_and_format() {
        assert_eq!(fmt_hms(3_723_000), "1:02:03");
        assert_eq!(parse_hms("0:01:30"), Some(90_000));
        assert_eq!(parse_hms("00:01:30.500"), Some(90_500));
        assert_eq!(parse_hms("0:00:01.5"), Some(1_500));
        assert_eq!(parse_hms("1:60:00"), None);
        assert_eq!(parse_hms("90"), None);
        assert_eq!(parse_hms("a:b:c"), None);
    }

    #[test]
    fn only_http_uris_are_accepted() {
        assert!(valid_uri("http://h/x"));
        assert!(valid_uri("https://h/x"));
        for bad in [
            "file:///etc/passwd",
            "ftp://h/x",
            "mpd-path:x",
            "http://",
            "javascript:1",
            "http://h/\nx",
        ] {
            assert!(!valid_uri(bad), "{bad}");
        }
        assert!(!valid_uri(&format!("http://h/{}", "a".repeat(MAX_URI))));
    }

    #[tokio::test]
    async fn set_uri_then_play_loads_the_player_once() {
        let p = FakePlayer::new();
        let c = Control::new(p.clone());
        av(
            &c,
            "SetAVTransportURI",
            &[("CurrentURI", URL), ("CurrentURIMetaData", "<DIDL-Lite/>")],
        )
        .await
        .unwrap();
        assert!(
            p.calls().is_empty(),
            "setting a URI does not start playback"
        );
        av(&c, "Play", &[("Speed", "1")]).await.unwrap();
        assert_eq!(
            p.calls(),
            vec![
                "modes repeat=false shuffle=off single=false".to_string(),
                format!("play_uri {URL}")
            ],
            "plain play order is set before the track is loaded"
        );
        // A second Play while playing is a no-op.
        av(&c, "Play", &[("Speed", "1")]).await.unwrap();
        assert_eq!(p.calls().len(), 2);
        let t = av(&c, "GetTransportInfo", &[]).await.unwrap();
        assert_eq!(field(&t, "CurrentTransportState"), "PLAYING");
    }

    #[tokio::test]
    async fn pause_resume_stop_follow_the_state() {
        let p = FakePlayer::new();
        let c = Control::new(p.clone());
        av(
            &c,
            "SetAVTransportURI",
            &[("CurrentURI", URL), ("CurrentURIMetaData", "")],
        )
        .await
        .unwrap();
        av(&c, "Play", &[("Speed", "1")]).await.unwrap();
        av(&c, "Pause", &[]).await.unwrap();
        let t = av(&c, "GetTransportInfo", &[]).await.unwrap();
        assert_eq!(field(&t, "CurrentTransportState"), "PAUSED_PLAYBACK");
        av(&c, "Play", &[("Speed", "1")]).await.unwrap();
        assert_eq!(p.calls().last().unwrap(), "resume");
        av(&c, "Stop", &[]).await.unwrap();
        let t = av(&c, "GetTransportInfo", &[]).await.unwrap();
        assert_eq!(field(&t, "CurrentTransportState"), "STOPPED");
        // Play after Stop loads again.
        av(&c, "Play", &[("Speed", "1")]).await.unwrap();
        assert_eq!(p.calls().last().unwrap(), &format!("play_uri {URL}"));
    }

    #[tokio::test]
    async fn play_with_nothing_set_and_pause_when_stopped_are_refused() {
        let p = FakePlayer::new();
        let c = Control::new(p.clone());
        assert_eq!(
            av(&c, "Play", &[("Speed", "1")]).await.unwrap_err(),
            Fault::NO_CONTENTS
        );
        av(
            &c,
            "SetAVTransportURI",
            &[("CurrentURI", URL), ("CurrentURIMetaData", "")],
        )
        .await
        .unwrap();
        assert_eq!(
            av(&c, "Pause", &[]).await.unwrap_err(),
            Fault::TRANSITION_NOT_AVAILABLE
        );
    }

    #[tokio::test]
    async fn a_track_the_operator_started_is_not_shown_as_the_controllers() {
        let p = FakePlayer::new();
        let c = Control::new(p.clone());
        av(
            &c,
            "SetAVTransportURI",
            &[("CurrentURI", URL), ("CurrentURIMetaData", "")],
        )
        .await
        .unwrap();
        av(&c, "Play", &[("Speed", "1")]).await.unwrap();
        // Someone plays something else locally.
        p.state.lock().unwrap().track_uri = Some("INTERNAL/x.flac".into());
        let t = av(&c, "GetTransportInfo", &[]).await.unwrap();
        assert_eq!(field(&t, "CurrentTransportState"), "STOPPED");
        assert_eq!(
            av(&c, "Pause", &[]).await.unwrap_err(),
            Fault::TRANSITION_NOT_AVAILABLE
        );
        // The controller's Play then loads its own URI again.
        av(&c, "Play", &[("Speed", "1")]).await.unwrap();
        assert_eq!(p.calls().last().unwrap(), &format!("play_uri {URL}"));
    }

    #[tokio::test]
    async fn bad_uris_and_instances_are_refused_and_nothing_reaches_the_player()
    {
        let p = FakePlayer::new();
        let c = Control::new(p.clone());
        for bad in ["file:///etc/passwd", "ftp://h/x", "http://h/\u{7}"] {
            let r = av(
                &c,
                "SetAVTransportURI",
                &[("CurrentURI", bad), ("CurrentURIMetaData", "")],
            )
            .await;
            assert_eq!(r.unwrap_err(), Fault::INVALID_ARGS, "{bad}");
        }
        let r = c
            .handle(
                "AVTransport",
                &act("Play", &[("InstanceID", "1"), ("Speed", "1")]),
            )
            .await;
        assert_eq!(r.unwrap_err(), Fault::INVALID_INSTANCE);
        assert!(p.calls().is_empty());
    }

    #[tokio::test]
    async fn seek_validates_unit_target_and_range() {
        let p = FakePlayer::new();
        let c = Control::new(p.clone());
        av(
            &c,
            "SetAVTransportURI",
            &[("CurrentURI", URL), ("CurrentURIMetaData", "")],
        )
        .await
        .unwrap();
        assert_eq!(
            av(&c, "Seek", &[("Unit", "REL_TIME"), ("Target", "0:00:10")])
                .await
                .unwrap_err(),
            Fault::TRANSITION_NOT_AVAILABLE
        );
        av(&c, "Play", &[("Speed", "1")]).await.unwrap();
        av(&c, "Seek", &[("Unit", "REL_TIME"), ("Target", "0:00:10")])
            .await
            .unwrap();
        assert_eq!(p.calls().last().unwrap(), "seek 10000");
        assert_eq!(
            av(&c, "Seek", &[("Unit", "FRAME"), ("Target", "1")])
                .await
                .unwrap_err(),
            Fault::SEEK_MODE
        );
        assert_eq!(
            av(&c, "Seek", &[("Unit", "REL_TIME"), ("Target", "9:00:00")])
                .await
                .unwrap_err(),
            Fault::SEEK_TARGET
        );
        assert_eq!(
            av(&c, "Seek", &[("Unit", "REL_TIME"), ("Target", "nope")])
                .await
                .unwrap_err(),
            Fault::SEEK_TARGET
        );
    }

    #[tokio::test]
    async fn position_info_reports_the_player_for_the_controllers_track_only() {
        let p = FakePlayer::new();
        let c = Control::new(p.clone());
        av(
            &c,
            "SetAVTransportURI",
            &[("CurrentURI", URL), ("CurrentURIMetaData", "<m/>")],
        )
        .await
        .unwrap();
        av(&c, "Play", &[("Speed", "1")]).await.unwrap();
        p.state.lock().unwrap().elapsed_ms = Some(65_000);
        let x = av(&c, "GetPositionInfo", &[]).await.unwrap();
        assert_eq!(field(&x, "RelTime"), "0:01:05");
        assert_eq!(field(&x, "TrackDuration"), "0:03:00");
        assert_eq!(field(&x, "TrackURI"), URL);
        assert_eq!(field(&x, "TrackMetaData"), "<m/>");
    }

    #[tokio::test]
    async fn the_next_uri_plays_when_the_track_ends() {
        let p = FakePlayer::new();
        let c = Control::new(p.clone());
        av(
            &c,
            "SetAVTransportURI",
            &[("CurrentURI", URL), ("CurrentURIMetaData", "")],
        )
        .await
        .unwrap();
        av(
            &c,
            "SetNextAVTransportURI",
            &[("NextURI", "http://h/b.flac"), ("NextURIMetaData", "")],
        )
        .await
        .unwrap();
        av(&c, "Play", &[("Speed", "1")]).await.unwrap();
        c.tick().await; // still playing: nothing happens
        assert_eq!(p.calls().len(), 2);
        p.set_transport(Transport::Stopped); // the track ended
        c.tick().await;
        assert_eq!(p.calls().last().unwrap(), "play_uri http://h/b.flac");
        let x = av(&c, "GetMediaInfo", &[]).await.unwrap();
        assert_eq!(field(&x, "CurrentURI"), "http://h/b.flac");
        assert_eq!(field(&x, "NextURI"), "");
    }

    #[tokio::test]
    async fn a_finished_track_with_no_next_settles_to_stopped() {
        let p = FakePlayer::new();
        let c = Control::new(p.clone());
        av(
            &c,
            "SetAVTransportURI",
            &[("CurrentURI", URL), ("CurrentURIMetaData", "")],
        )
        .await
        .unwrap();
        av(&c, "Play", &[("Speed", "1")]).await.unwrap();
        p.set_transport(Transport::Stopped);
        c.tick().await;
        let s = c.av_snapshot().await;
        assert_eq!(s.transport_state, "STOPPED");
        assert_eq!(s.actions, "Play");
    }

    #[tokio::test]
    async fn rendering_control_reads_and_sets_volume_and_mute() {
        let p = FakePlayer::new();
        let c = Control::new(p.clone());
        let rc = |n: &str, a: &[(&str, &str)]| {
            let mut v = vec![("InstanceID", "0"), ("Channel", "Master")];
            v.extend_from_slice(a);
            act(n, &v)
        };
        let g = c
            .handle("RenderingControl", &rc("GetVolume", &[]))
            .await
            .unwrap();
        assert_eq!(field(&g, "CurrentVolume"), "30");
        c.handle(
            "RenderingControl",
            &rc("SetVolume", &[("DesiredVolume", "55")]),
        )
        .await
        .unwrap();
        assert_eq!(p.calls().last().unwrap(), "set_volume 55");
        c.handle("RenderingControl", &rc("SetMute", &[("DesiredMute", "1")]))
            .await
            .unwrap();
        let m = c
            .handle("RenderingControl", &rc("GetMute", &[]))
            .await
            .unwrap();
        assert_eq!(field(&m, "CurrentMute"), "1");
        let bad = c
            .handle(
                "RenderingControl",
                &rc("SetVolume", &[("DesiredVolume", "101")]),
            )
            .await;
        assert_eq!(bad.unwrap_err(), Fault::INVALID_ARGS);
        let ch = c
            .handle(
                "RenderingControl",
                &act("GetVolume", &[("InstanceID", "0"), ("Channel", "LF")]),
            )
            .await;
        assert_eq!(ch.unwrap_err(), Fault::INVALID_ARGS);
    }

    #[tokio::test]
    async fn connection_manager_advertises_a_sink_and_no_source() {
        let c = Control::new(FakePlayer::new());
        let x = c
            .handle("ConnectionManager", &act("GetProtocolInfo", &[]))
            .await
            .unwrap();
        assert!(field(&x, "Sink").contains("http-get:*:audio/flac:*"));
        assert_eq!(field(&x, "Source"), "");
        let id = c
            .handle("ConnectionManager", &act("GetCurrentConnectionIDs", &[]))
            .await
            .unwrap();
        assert_eq!(field(&id, "ConnectionIDs"), "0");
    }

    #[tokio::test]
    async fn unknown_actions_and_player_failures_fault_cleanly() {
        let p = FakePlayer::new();
        let c = Control::new(p.clone());
        assert_eq!(
            av(&c, "Teleport", &[]).await.unwrap_err(),
            Fault::INVALID_ACTION
        );
        assert_eq!(
            c.handle("ContentDirectory", &act("Browse", &[]))
                .await
                .unwrap_err(),
            Fault::INVALID_ACTION
        );
        *p.fail.lock().unwrap() = Some(BridgeError::Unavailable("down".into()));
        av(
            &c,
            "SetAVTransportURI",
            &[("CurrentURI", URL), ("CurrentURIMetaData", "")],
        )
        .await
        .unwrap();
        assert_eq!(
            av(&c, "Play", &[("Speed", "1")]).await.unwrap_err(),
            Fault::ACTION_FAILED
        );
    }

    #[tokio::test]
    async fn every_declared_action_has_a_handler() {
        // The SCPD a controller reads and the handlers must not drift apart:
        // no declared action may answer "Invalid Action".
        let p = FakePlayer::new();
        let c = Control::new(p.clone());
        av(
            &c,
            "SetAVTransportURI",
            &[("CurrentURI", URL), ("CurrentURIMetaData", "")],
        )
        .await
        .unwrap();
        for svc in crate::description::SERVICES {
            for name in crate::description::action_names(svc.name) {
                let mut args: Vec<(&str, &str)> = vec![
                    ("InstanceID", "0"),
                    ("Channel", "Master"),
                    ("ConnectionID", "0"),
                    ("Speed", "1"),
                    ("CurrentURI", URL),
                    ("CurrentURIMetaData", ""),
                    ("NextURI", ""),
                    ("NextURIMetaData", ""),
                    ("Unit", "REL_TIME"),
                    ("Target", "0:00:00"),
                    ("NewPlayMode", "NORMAL"),
                    ("PresetName", "FactoryDefaults"),
                    ("DesiredVolume", "10"),
                    ("DesiredMute", "0"),
                ];
                args.sort();
                let r = c.handle(svc.name, &act(name, &args)).await;
                assert!(
                    r.as_ref().err() != Some(&Fault::INVALID_ACTION),
                    "{}.{name} is declared in the SCPD but has no handler",
                    svc.name
                );
            }
        }
    }

    #[tokio::test]
    async fn the_operators_play_order_comes_back_when_the_session_ends() {
        let operator = "modes repeat=true shuffle=tracks single=false";
        let plain = "modes repeat=false shuffle=off single=false";
        // Stop puts it back.
        let p = FakePlayer::new();
        let c = Control::new(p.clone());
        av(
            &c,
            "SetAVTransportURI",
            &[("CurrentURI", URL), ("CurrentURIMetaData", "")],
        )
        .await
        .unwrap();
        av(&c, "Play", &[("Speed", "1")]).await.unwrap();
        assert!(
            !p.state.lock().unwrap().modes.repeat,
            "plain during the session"
        );
        av(&c, "Stop", &[]).await.unwrap();
        let calls = p.calls();
        assert_eq!(
            calls
                .iter()
                .filter(|x| x.starts_with("modes"))
                .cloned()
                .collect::<Vec<_>>(),
            vec![plain.to_string(), operator.to_string()]
        );
        assert!(
            p.state.lock().unwrap().modes.repeat,
            "the operator's repeat is back"
        );

        // A track that ends with nothing queued puts it back; a queued next
        // track keeps the session, and the order, in plain mode.
        let p = FakePlayer::new();
        let c = Control::new(p.clone());
        av(
            &c,
            "SetAVTransportURI",
            &[("CurrentURI", URL), ("CurrentURIMetaData", "")],
        )
        .await
        .unwrap();
        av(
            &c,
            "SetNextAVTransportURI",
            &[("NextURI", "http://h/b.flac"), ("NextURIMetaData", "")],
        )
        .await
        .unwrap();
        av(&c, "Play", &[("Speed", "1")]).await.unwrap();
        p.set_transport(Transport::Stopped);
        c.tick().await; // promotes the next track
        assert!(
            !p.state.lock().unwrap().modes.repeat,
            "still plain on the next track"
        );
        p.set_transport(Transport::Stopped);
        c.tick().await; // nothing left: the session ends
        assert!(
            p.state.lock().unwrap().modes.repeat,
            "the operator's order is back at the end"
        );
        assert_eq!(
            p.calls().iter().filter(|x| x.as_str() == operator).count(),
            1
        );
    }

    #[tokio::test]
    async fn disabling_ends_the_session_and_restores_the_order() {
        let p = FakePlayer::new();
        let c = Control::new(p.clone());
        av(
            &c,
            "SetAVTransportURI",
            &[("CurrentURI", URL), ("CurrentURIMetaData", "")],
        )
        .await
        .unwrap();
        av(&c, "Play", &[("Speed", "1")]).await.unwrap();
        c.end().await;
        assert!(p.state.lock().unwrap().modes.repeat);
        // A second end is a no-op.
        let n = p.calls().len();
        c.end().await;
        assert_eq!(p.calls().len(), n);
    }
}
