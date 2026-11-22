// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! A track that finishes is remembered, and counted.
//!
//! A play is a track reaching its end: the player moved on, or
//! stopped, because the song ended. A skip is not a play — next,
//! previous, choosing another row, clear, an operator stop. A
//! pause is not a play. A stream with no end never finishes. The
//! same track finishing again is another play. One window is
//! accepted as the limit of reading `status`: a stop, or a seek
//! back to the start, inside the last two seconds plus the
//! crossfade counts as a finish, because from outside it is one.
//!
//! MPD does not say why the player moved on, so the tracker
//! decides from what it saw: at each wake it notes where the
//! track was and when; at the next, if the player has moved on
//! and the track must have reached its end by then — its last
//! known position plus the time since, within a tolerance that
//! covers MPD's own crossfade — that was a finish. The tracker is
//! pure: observations in, finishes out.
//!
//! Two lists come from the finishes. The last hundred plays,
//! newest first, in which the same track can appear more than
//! once; and every track that has finished at least once, with
//! its count, highest first, a tie going to the more recently
//! finished. A track that never finished is in neither: a count
//! of zero is not a stand-in for the rest of the library.
//!
//! Both live in the plugin's own state and survive a restart. The
//! steward's queue history is a different record of a different
//! thing and is not consulted.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Mutex;

use crate::library::{VerbError, LIBRARY_PAYLOAD_VERSION};
use crate::mpd::{MpdSong, MpdStatus, PlayState};

const PLUGIN_NAME: &str = "org.evoframework.playback.mpd";

/// File-format version of the kept history.
pub(crate) const PLAY_HISTORY_VERSION: u32 = 1;

/// How many recent plays are kept. The 101st drops the oldest.
pub(crate) const RECENT_CAP: usize = 100;

/// How close to its end a track must have been, when the player
/// moved on, to count as finished. MPD's crossfade is added: with
/// crossfade on, the player moves on that many seconds early.
pub(crate) const END_TOLERANCE: Duration = Duration::from_secs(2);

// ----- the tracker -----

/// What one wake saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Observation {
    /// When it was seen.
    pub(crate) at: Instant,
    /// The current song's URI, the identity the queue uses.
    pub(crate) uri: Option<String>,
    /// Its queue position — a second copy of the same URI in the
    /// queue is a different row.
    pub(crate) position: Option<u32>,
    pub(crate) state: PlayState,
    pub(crate) elapsed: Option<Duration>,
    pub(crate) duration: Option<Duration>,
    pub(crate) crossfade: Duration,
}

impl Observation {
    pub(crate) fn from_mpd(
        at: Instant,
        status: &MpdStatus,
        song: Option<&MpdSong>,
    ) -> Self {
        Self {
            at,
            uri: song.map(|s| s.file_path.clone()),
            position: status.song_position,
            state: status.state,
            elapsed: status.elapsed,
            // `status` carries the duration when MPD knows it; a
            // stream with no end carries none.
            duration: status.duration.or_else(|| song.and_then(|s| s.duration)),
            crossfade: Duration::from_secs(u64::from(status.crossfade_seconds)),
        }
    }
}

/// A track that reached its end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Finished {
    pub(crate) uri: String,
}

/// Decides, from one observation to the next, whether the track
/// that was playing finished.
#[derive(Debug, Default)]
pub(crate) struct PlayTracker {
    last: Option<Observation>,
}

impl PlayTracker {
    /// Note what the wake saw; say whether the previous track
    /// finished in between.
    ///
    /// The previous track finished when the player has moved on
    /// — a different song or row, a stop, or the same song back
    /// at its start — and the track must have reached its end by
    /// then: where it was last seen, plus the time since if it
    /// was playing (a paused track does not advance), comes
    /// within the tolerance of its length. A track with no known
    /// length never finishes. Nothing before means nothing.
    pub(crate) fn observe(&mut self, obs: Observation) -> Option<Finished> {
        let prev = std::mem::replace(&mut self.last, Some(obs.clone()))?;
        if prev.state == PlayState::Stopped {
            return None;
        }
        let uri = prev.uri.clone()?;
        let duration = prev.duration?;
        let elapsed = prev.elapsed?;
        let reached = if prev.state == PlayState::Playing {
            elapsed + obs.at.saturating_duration_since(prev.at)
        } else {
            elapsed
        };
        if reached + END_TOLERANCE + prev.crossfade < duration {
            return None;
        }
        let same_row = obs.uri == prev.uri && obs.position == prev.position;
        let back_at_start = same_row
            && obs.state != PlayState::Stopped
            && obs.elapsed.is_some_and(|e| e <= Duration::from_secs(1));
        let moved_on =
            obs.state == PlayState::Stopped || !same_row || back_at_start;
        moved_on.then_some(Finished { uri })
    }
}

// ----- the record -----

/// One play.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PlayRecord {
    pub(crate) uri: String,
    /// Seconds since the Unix epoch when the track finished.
    pub(crate) finished_at: u64,
}

/// One track's count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PlayCount {
    pub(crate) uri: String,
    pub(crate) count: u64,
    pub(crate) last_finished_at: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct Persisted {
    v: u32,
    recent: Vec<PlayRecord>,
    counts: Vec<PlayCount>,
}

struct Inner {
    tracker: PlayTracker,
    /// Newest first.
    recent: VecDeque<PlayRecord>,
    counts: HashMap<String, PlayCount>,
    path: Option<PathBuf>,
}

/// The kept history: the tracker, the two lists, and where they
/// are written. Cloneable; the observer records into it and the
/// library verbs read from it.
#[derive(Clone)]
pub(crate) struct PlayHistory {
    inner: Arc<Mutex<Inner>>,
}

/// Now, in seconds since the Unix epoch.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl PlayHistory {
    fn with(path: Option<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                tracker: PlayTracker::default(),
                recent: VecDeque::new(),
                counts: HashMap::new(),
                path,
            })),
        }
    }

    /// A history that keeps nothing on disk. Tests that are not
    /// about this row use it.
    #[cfg(test)]
    pub(crate) fn disabled() -> Self {
        Self::with(None)
    }

    /// The history at this path, read back if it is there.
    pub(crate) async fn open(path: PathBuf) -> Self {
        let history = Self::with(Some(path.clone()));
        match tokio::fs::read_to_string(&path).await {
            Ok(body) => match toml::from_str::<Persisted>(&body) {
                Ok(p) if p.v == PLAY_HISTORY_VERSION => {
                    let mut inner = history.inner.lock().await;
                    inner.recent =
                        p.recent.into_iter().take(RECENT_CAP).collect();
                    inner.counts = p
                        .counts
                        .into_iter()
                        .map(|c| (c.uri.clone(), c))
                        .collect();
                    tracing::info!(
                        plugin = PLUGIN_NAME,
                        recent = inner.recent.len(),
                        tracks = inner.counts.len(),
                        "play history read back"
                    );
                }
                Ok(p) => tracing::warn!(
                    plugin = PLUGIN_NAME,
                    version = p.v,
                    "play history is from another format version; starting \
                     empty"
                ),
                Err(e) => tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "play history does not parse; starting empty"
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "play history unreadable; starting empty"
            ),
        }
        history
    }

    /// Feed the tracker one wake. A finish is recorded and kept.
    pub(crate) async fn observe(
        &self,
        at: Instant,
        status: &MpdStatus,
        song: Option<&MpdSong>,
    ) -> Option<Finished> {
        let obs = Observation::from_mpd(at, status, song);
        let finished = {
            let mut inner = self.inner.lock().await;
            inner.tracker.observe(obs)
        }?;
        self.record_finish(&finished.uri, now_unix()).await;
        Some(finished)
    }

    /// Keep one finish: newest first, the cap enforced, the count
    /// raised, and the file rewritten.
    pub(crate) async fn record_finish(&self, uri: &str, finished_at: u64) {
        let mut inner = self.inner.lock().await;
        inner.recent.push_front(PlayRecord {
            uri: uri.to_string(),
            finished_at,
        });
        inner.recent.truncate(RECENT_CAP);
        let entry = inner.counts.entry(uri.to_string()).or_insert(PlayCount {
            uri: uri.to_string(),
            count: 0,
            last_finished_at: finished_at,
        });
        entry.count += 1;
        entry.last_finished_at = entry.last_finished_at.max(finished_at);
        tracing::info!(
            plugin = PLUGIN_NAME,
            uri,
            count = entry.count,
            "track finished"
        );
        if let Err(e) = Self::persist(&inner).await {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "play history could not be written; it is kept in memory \
                 until the next finish"
            );
        }
    }

    /// Newest first, at most `limit`.
    pub(crate) async fn recently_played(
        &self,
        limit: usize,
    ) -> Vec<PlayRecord> {
        let inner = self.inner.lock().await;
        inner.recent.iter().take(limit).cloned().collect()
    }

    /// Highest count first; an equal count keeps the more
    /// recently finished track first; at most `limit`.
    pub(crate) async fn most_played(&self, limit: usize) -> Vec<PlayCount> {
        let inner = self.inner.lock().await;
        let mut rows: Vec<PlayCount> = inner.counts.values().cloned().collect();
        rows.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then(b.last_finished_at.cmp(&a.last_finished_at))
                .then(a.uri.cmp(&b.uri))
        });
        rows.truncate(limit);
        rows
    }

    async fn persist(inner: &Inner) -> Result<(), String> {
        let Some(path) = inner.path.as_ref() else {
            return Ok(());
        };
        let mut counts: Vec<PlayCount> =
            inner.counts.values().cloned().collect();
        counts.sort_by(|a, b| a.uri.cmp(&b.uri));
        let body = toml::to_string_pretty(&Persisted {
            v: PLAY_HISTORY_VERSION,
            recent: inner.recent.iter().cloned().collect(),
            counts,
        })
        .map_err(|e| format!("serialise: {e}"))?;
        let parent = path
            .parent()
            .ok_or_else(|| format!("{path:?} has no parent"))?;
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("mkdir {parent:?}: {e}"))?;
        let staging = parent.join(format!(
            ".{}.tmp",
            path.file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| "play-history.toml".to_string())
        ));
        tokio::fs::write(&staging, body)
            .await
            .map_err(|e| format!("write {staging:?}: {e}"))?;
        tokio::fs::rename(&staging, path)
            .await
            .map_err(|e| format!("rename {staging:?} -> {path:?}: {e}"))?;
        Ok(())
    }
}

// ----- the two library reads -----

fn default_version() -> u32 {
    LIBRARY_PAYLOAD_VERSION
}

/// `{ v, limit }` — `limit` is optional and capped.
#[derive(Debug, Deserialize)]
pub(crate) struct HistoryReadPayload {
    #[serde(default = "default_version")]
    pub(crate) v: u32,
    #[serde(default)]
    pub(crate) limit: Option<u32>,
}

fn check_version(v: u32, verb: &str) -> Result<(), VerbError> {
    if v != LIBRARY_PAYLOAD_VERSION {
        return Err(VerbError::PayloadVersion {
            verb: verb.to_string(),
            got: v,
            expected: LIBRARY_PAYLOAD_VERSION,
        });
    }
    Ok(())
}

/// `library.recently_played` — the last plays, newest first,
/// without the tracks the operator has forgotten.
pub(crate) async fn handle_recently_played(
    history: &PlayHistory,
    forgotten: &crate::forgotten_tracks::ForgottenTracks,
    payload: HistoryReadPayload,
) -> Result<serde_json::Value, VerbError> {
    check_version(payload.v, "library.recently_played")?;
    let limit = payload
        .limit
        .map(|l| l as usize)
        .unwrap_or(RECENT_CAP)
        .min(RECENT_CAP);
    let forgotten = forgotten.snapshot().await;
    let plays: Vec<serde_json::Value> = history
        .recently_played(RECENT_CAP)
        .await
        .into_iter()
        .filter(|p| !forgotten.contains(&p.uri))
        .take(limit)
        .map(|p| json!({ "uri": p.uri, "finished_at": p.finished_at }))
        .collect();
    Ok(json!({
        "v": LIBRARY_PAYLOAD_VERSION,
        "plays": plays,
        "cap": RECENT_CAP,
    }))
}

/// The most tracks `most_played` will return in one answer.
pub(crate) const MOST_PLAYED_CAP: usize = 1000;

/// `library.most_played` — every track that has finished at
/// least once, highest count first, without the tracks the
/// operator has forgotten.
pub(crate) async fn handle_most_played(
    history: &PlayHistory,
    forgotten: &crate::forgotten_tracks::ForgottenTracks,
    payload: HistoryReadPayload,
) -> Result<serde_json::Value, VerbError> {
    check_version(payload.v, "library.most_played")?;
    let limit = payload
        .limit
        .map(|l| l as usize)
        .unwrap_or(RECENT_CAP)
        .min(MOST_PLAYED_CAP);
    let forgotten = forgotten.snapshot().await;
    let tracks: Vec<serde_json::Value> = history
        .most_played(MOST_PLAYED_CAP)
        .await
        .into_iter()
        .filter(|c| !forgotten.contains(&c.uri))
        .take(limit)
        .map(|c| {
            json!({
                "uri": c.uri,
                "count": c.count,
                "last_finished_at": c.last_finished_at,
            })
        })
        .collect();
    Ok(json!({
        "v": LIBRARY_PAYLOAD_VERSION,
        "tracks": tracks,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: f64) -> Duration {
        Duration::from_secs_f64(s)
    }

    fn obs(
        at: Instant,
        uri: Option<&str>,
        position: Option<u32>,
        state: PlayState,
        elapsed: Option<f64>,
        duration: Option<f64>,
    ) -> Observation {
        Observation {
            at,
            uri: uri.map(str::to_string),
            position,
            state,
            elapsed: elapsed.map(secs),
            duration: duration.map(secs),
            crossfade: Duration::ZERO,
        }
    }

    fn playing(at: Instant, uri: &str, pos: u32, elapsed: f64) -> Observation {
        obs(
            at,
            Some(uri),
            Some(pos),
            PlayState::Playing,
            Some(elapsed),
            Some(180.0),
        )
    }

    // ===== the tracker =====

    /// A track seen near its end, then the player on the next
    /// song: that was a finish, and it is the track that was
    /// playing, not the one that is.
    #[test]
    fn a_track_that_reaches_its_end_is_a_play() {
        let t0 = Instant::now();
        let mut t = PlayTracker::default();
        assert_eq!(
            t.observe(playing(t0, "a.flac", 0, 170.0)),
            None,
            "nothing before"
        );
        let got = t.observe(playing(t0 + secs(10.5), "b.flac", 1, 0.3));
        assert_eq!(
            got,
            Some(Finished {
                uri: "a.flac".into()
            })
        );
    }

    /// Next, previous, another row, clear, an operator stop: the
    /// player moved on and the track had not reached its end.
    /// Nothing is recorded.
    #[test]
    fn a_skip_is_not_a_play() {
        let t0 = Instant::now();
        // Next / another row.
        let mut t = PlayTracker::default();
        t.observe(playing(t0, "a.flac", 0, 30.0));
        assert_eq!(t.observe(playing(t0 + secs(5.0), "b.flac", 1, 0.2)), None);
        // Previous: a different row, even with the same URI.
        let mut t = PlayTracker::default();
        t.observe(playing(t0, "a.flac", 3, 30.0));
        assert_eq!(t.observe(playing(t0 + secs(5.0), "a.flac", 2, 0.2)), None);
        // An operator stop, or a clear.
        let mut t = PlayTracker::default();
        t.observe(playing(t0, "a.flac", 0, 30.0));
        assert_eq!(
            t.observe(obs(
                t0 + secs(5.0),
                None,
                None,
                PlayState::Stopped,
                None,
                None
            )),
            None
        );
        // A skip from paused.
        let mut t = PlayTracker::default();
        t.observe(obs(
            t0,
            Some("a.flac"),
            Some(0),
            PlayState::Paused,
            Some(30.0),
            Some(180.0),
        ));
        assert_eq!(
            t.observe(playing(t0 + secs(500.0), "b.flac", 1, 0.2)),
            None,
            "paused does not advance"
        );
    }

    /// A pause is not a play, and a paused track does not
    /// advance: only the time it was actually playing counts
    /// toward its end. Resumed and played out, it finishes.
    #[test]
    fn a_pause_is_not_a_play_and_does_not_advance() {
        let t0 = Instant::now();
        let mut t = PlayTracker::default();
        t.observe(playing(t0, "a.flac", 0, 170.0));
        let paused = obs(
            t0 + secs(5.0),
            Some("a.flac"),
            Some(0),
            PlayState::Paused,
            Some(175.0),
            Some(180.0),
        );
        assert_eq!(t.observe(paused), None, "a pause records nothing");
        // Long pause, then resumed at the same place.
        assert_eq!(
            t.observe(playing(t0 + secs(900.0), "a.flac", 0, 175.0)),
            None,
            "resuming records nothing"
        );
        // Played out from there.
        let got = t.observe(playing(t0 + secs(905.5), "b.flac", 1, 0.3));
        assert_eq!(
            got,
            Some(Finished {
                uri: "a.flac".into()
            })
        );

        // A pause in the last seconds is still a pause: the
        // player has not moved on.
        let mut t = PlayTracker::default();
        t.observe(playing(t0, "a.flac", 0, 178.0));
        let late_pause = obs(
            t0 + secs(1.0),
            Some("a.flac"),
            Some(0),
            PlayState::Paused,
            Some(179.0),
            Some(180.0),
        );
        assert_eq!(
            t.observe(late_pause),
            None,
            "paused at the end is not finished"
        );

        // And a track paused early does not creep toward its end
        // while paused: skipped after a long pause, it is a skip.
        let mut t = PlayTracker::default();
        t.observe(obs(
            t0,
            Some("a.flac"),
            Some(0),
            PlayState::Paused,
            Some(30.0),
            Some(180.0),
        ));
        assert_eq!(
            t.observe(playing(t0 + secs(500.0), "b.flac", 1, 0.2)),
            None,
            "paused for 500 s at 30 s is still at 30 s"
        );
    }

    /// A stream with no end never finishes, however long it ran
    /// and whatever came after it.
    #[test]
    fn an_endless_stream_is_not_a_play() {
        let t0 = Instant::now();
        let mut t = PlayTracker::default();
        t.observe(obs(
            t0,
            Some("http://s.example/live"),
            Some(0),
            PlayState::Playing,
            Some(3600.0),
            None,
        ));
        assert_eq!(
            t.observe(playing(t0 + secs(100.0), "a.flac", 1, 0.2)),
            None
        );
        assert_eq!(
            t.observe(obs(
                t0 + secs(200.0),
                None,
                None,
                PlayState::Stopped,
                None,
                None
            )),
            None
        );
    }

    /// The player stopped because the song ended — end of the
    /// queue, or single mode. That is a finish.
    #[test]
    fn a_stop_because_the_song_ended_is_a_play() {
        let t0 = Instant::now();
        let mut t = PlayTracker::default();
        t.observe(playing(t0, "a.flac", 0, 179.0));
        let got = t.observe(obs(
            t0 + secs(1.5),
            None,
            None,
            PlayState::Stopped,
            None,
            None,
        ));
        assert_eq!(
            got,
            Some(Finished {
                uri: "a.flac".into()
            })
        );
    }

    /// The same track finishing again — repeat, single — is
    /// another play, each time.
    #[test]
    fn the_same_track_finishing_again_is_another_play() {
        let t0 = Instant::now();
        let mut t = PlayTracker::default();
        t.observe(playing(t0, "a.flac", 0, 179.0));
        let first = t.observe(playing(t0 + secs(1.5), "a.flac", 0, 0.2));
        assert_eq!(
            first,
            Some(Finished {
                uri: "a.flac".into()
            })
        );
        // Around again.
        t.observe(playing(t0 + secs(170.0), "a.flac", 0, 168.5));
        let second = t.observe(playing(t0 + secs(182.0), "a.flac", 0, 0.1));
        assert_eq!(
            second,
            Some(Finished {
                uri: "a.flac".into()
            })
        );
        // But a seek back to the start from the middle is not.
        t.observe(playing(t0 + secs(200.0), "a.flac", 0, 18.0));
        assert_eq!(
            t.observe(playing(t0 + secs(201.0), "a.flac", 0, 0.0)),
            None
        );
    }

    /// With crossfade on, MPD moves on that many seconds early;
    /// the tolerance widens by exactly that.
    #[test]
    fn crossfade_widens_the_end() {
        let t0 = Instant::now();
        let mut t = PlayTracker::default();
        let mut early = playing(t0, "a.flac", 0, 172.0);
        early.crossfade = secs(5.0);
        t.observe(early);
        let got = t.observe(playing(t0 + secs(1.0), "b.flac", 1, 0.2));
        assert_eq!(
            got,
            Some(Finished {
                uri: "a.flac".into()
            }),
            "172 + 1 + 2 + 5 reaches 180"
        );

        let mut t = PlayTracker::default();
        t.observe(playing(t0, "a.flac", 0, 172.0));
        assert_eq!(
            t.observe(playing(t0 + secs(1.0), "b.flac", 1, 0.2)),
            None,
            "without crossfade it did not"
        );
    }

    /// An observation is built from what MPD says: the song's
    /// URI and position, the state, the clock, and crossfade.
    #[test]
    fn an_observation_is_read_from_mpd() {
        let status = MpdStatus {
            state: PlayState::Playing,
            song_position: Some(4),
            elapsed: Some(secs(12.5)),
            duration: Some(secs(200.0)),
            volume: Some(50),
            repeat: false,
            random: false,
            single: false,
            consume: false,
            crossfade_seconds: 3,
            updating_db: None,
        };
        let song = MpdSong {
            file_path: "INTERNAL/x.flac".into(),
            title: None,
            artist: None,
            album: None,
            duration: Some(secs(200.0)),
            codec_name: None,
            classical: Default::default(),
        };
        let o = Observation::from_mpd(Instant::now(), &status, Some(&song));
        assert_eq!(o.uri.as_deref(), Some("INTERNAL/x.flac"));
        assert_eq!(o.position, Some(4));
        assert_eq!(o.elapsed, Some(secs(12.5)));
        assert_eq!(o.duration, Some(secs(200.0)));
        assert_eq!(o.crossfade, secs(3.0));
        let none = Observation::from_mpd(Instant::now(), &status, None);
        assert_eq!(none.uri, None);
    }

    // ===== the record =====

    /// The 101st play drops the oldest; the list is newest first
    /// and the same track can appear more than once.
    #[tokio::test]
    async fn the_hundred_and_first_play_drops_the_oldest() {
        let h = PlayHistory::disabled();
        for i in 0..101u64 {
            h.record_finish(&format!("t{}.flac", i % 7), 1_000 + i)
                .await;
        }
        let recent = h.recently_played(RECENT_CAP + 50).await;
        assert_eq!(recent.len(), RECENT_CAP);
        assert_eq!(recent[0].finished_at, 1_100, "newest first");
        assert_eq!(recent[99].finished_at, 1_001, "the oldest, 1_000, is gone");
        assert!(recent.iter().filter(|r| r.uri == "t0.flac").count() > 1);
    }

    /// Every finish counts, beyond the hundred kept; a track that
    /// never finished is absent; highest first; an equal count
    /// keeps the more recently finished first.
    #[tokio::test]
    async fn most_played_counts_every_finish_and_omits_the_rest() {
        let h = PlayHistory::disabled();
        for i in 0..150u64 {
            h.record_finish("a.flac", 10_000 + i).await;
        }
        h.record_finish("b.flac", 500).await;
        h.record_finish("c.flac", 900).await;
        let rows = h.most_played(10).await;
        assert_eq!(rows.len(), 3, "never-finished tracks are absent");
        assert_eq!(
            (rows[0].uri.as_str(), rows[0].count),
            ("a.flac", 150),
            "beyond the hundred kept"
        );
        assert_eq!(
            rows[1].uri, "c.flac",
            "equal count: more recently finished first"
        );
        assert_eq!(rows[2].uri, "b.flac");
        assert!(rows.iter().all(|r| r.count > 0));
        assert_eq!(h.most_played(1).await.len(), 1, "limited");
    }

    /// The history survives a restart: what was kept is read back.
    #[tokio::test]
    async fn the_history_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("play-history.toml");
        {
            let h = PlayHistory::open(path.clone()).await;
            h.record_finish("a.flac", 1).await;
            h.record_finish("b.flac", 2).await;
            h.record_finish("a.flac", 3).await;
        }
        let h = PlayHistory::open(path.clone()).await;
        let recent = h.recently_played(10).await;
        assert_eq!(
            recent.iter().map(|r| r.uri.as_str()).collect::<Vec<_>>(),
            vec!["a.flac", "b.flac", "a.flac"]
        );
        let rows = h.most_played(10).await;
        assert_eq!(
            (
                rows[0].uri.as_str(),
                rows[0].count,
                rows[0].last_finished_at
            ),
            ("a.flac", 2, 3)
        );
        assert!(!path.with_file_name(".play-history.toml.tmp").exists());
        // A bad file is an empty history, not a panic.
        std::fs::write(&path, "nonsense = [").unwrap();
        assert!(PlayHistory::open(path)
            .await
            .recently_played(10)
            .await
            .is_empty());
    }

    /// Through the handle: a skip adds no row and no count; a
    /// finish adds both.
    #[tokio::test]
    async fn a_skip_adds_nothing_and_a_finish_adds_both() {
        let h = PlayHistory::disabled();
        let status = |state: PlayState, pos: u32, elapsed: f64| MpdStatus {
            state,
            song_position: Some(pos),
            elapsed: Some(secs(elapsed)),
            duration: Some(secs(180.0)),
            volume: Some(50),
            repeat: false,
            random: false,
            single: false,
            consume: false,
            crossfade_seconds: 0,
            updating_db: None,
        };
        let song = |uri: &str| MpdSong {
            file_path: uri.to_string(),
            title: None,
            artist: None,
            album: None,
            duration: Some(secs(180.0)),
            codec_name: None,
            classical: Default::default(),
        };
        let t0 = Instant::now();
        // Playing a, skipped to b after 20 s.
        h.observe(
            t0,
            &status(PlayState::Playing, 0, 10.0),
            Some(&song("a.flac")),
        )
        .await;
        let skip = h
            .observe(
                t0 + secs(20.0),
                &status(PlayState::Playing, 1, 0.3),
                Some(&song("b.flac")),
            )
            .await;
        assert_eq!(skip, None);
        assert!(h.recently_played(10).await.is_empty(), "a skip adds no row");
        assert!(h.most_played(10).await.is_empty(), "a skip adds no count");
        // Paused b, nothing.
        let pause = h
            .observe(
                t0 + secs(30.0),
                &status(PlayState::Paused, 1, 10.3),
                Some(&song("b.flac")),
            )
            .await;
        assert_eq!(pause, None);
        assert!(
            h.recently_played(10).await.is_empty(),
            "a pause adds no row"
        );
        // Resumed, played to the end, player moved on.
        h.observe(
            t0 + secs(40.0),
            &status(PlayState::Playing, 1, 10.3),
            Some(&song("b.flac")),
        )
        .await;
        let fin = h
            .observe(
                t0 + secs(210.0),
                &status(PlayState::Playing, 2, 0.2),
                Some(&song("c.flac")),
            )
            .await;
        assert_eq!(
            fin,
            Some(Finished {
                uri: "b.flac".into()
            })
        );
        assert_eq!(h.recently_played(10).await[0].uri, "b.flac");
        assert_eq!(h.most_played(10).await[0].count, 1);
    }

    // ===== the two reads =====

    #[tokio::test]
    async fn the_reads_answer_newest_first_and_without_zero_rows() {
        let h = PlayHistory::disabled();
        h.record_finish("a.flac", 1).await;
        h.record_finish("b.flac", 2).await;
        h.record_finish("a.flac", 3).await;
        let recent = handle_recently_played(
            &h,
            &crate::forgotten_tracks::ForgottenTracks::default(),
            HistoryReadPayload {
                v: 1,
                limit: Some(2),
            },
        )
        .await
        .unwrap();
        assert_eq!(recent["cap"], 100);
        assert_eq!(recent["plays"].as_array().unwrap().len(), 2, "limited");
        assert_eq!(recent["plays"][0]["uri"], "a.flac");
        assert_eq!(recent["plays"][0]["finished_at"], 3);
        assert_eq!(recent["plays"][1]["uri"], "b.flac");
        let most = handle_most_played(
            &h,
            &crate::forgotten_tracks::ForgottenTracks::default(),
            HistoryReadPayload { v: 1, limit: None },
        )
        .await
        .unwrap();
        let tracks = most["tracks"].as_array().unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0]["uri"], "a.flac");
        assert_eq!(tracks[0]["count"], 2);
        assert_eq!(tracks[1]["count"], 1);
        assert!(tracks.iter().all(|t| t["count"].as_u64().unwrap() > 0));
        // A limit over the cap is the cap.
        let over = handle_recently_played(
            &h,
            &crate::forgotten_tracks::ForgottenTracks::default(),
            HistoryReadPayload {
                v: 1,
                limit: Some(9_999),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            over["plays"].as_array().unwrap().len(),
            3,
            "a limit over the cap is the cap, and the cap holds all three"
        );
        // The wrong version is refused.
        let bad = handle_most_played(
            &h,
            &crate::forgotten_tracks::ForgottenTracks::default(),
            HistoryReadPayload { v: 2, limit: None },
        )
        .await;
        assert!(matches!(bad, Err(VerbError::PayloadVersion { .. })));
    }
}
