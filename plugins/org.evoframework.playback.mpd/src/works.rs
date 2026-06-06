//! Works browse aggregation + library state counter computation.
//!
//! Single source of truth for the `library.list_works` and
//! `library.get_work_recordings` verbs AND for the three
//! library-state classical counters (total_tracks_with_composer,
//! distinct_works, works_with_multiple_recordings). One walk of
//! MPD's database via `listallinfo` computes everything; the
//! results feed both the verb response paths and the
//! `audio_library_state` envelope.
//!
//! # Identifier stability
//!
//! Both `work_id` and `recording_id` are 16-hex-character
//! truncations of SHA-256 over a normalised input. The
//! normalisation lowercases input components and uses ASCII
//! Unit Separator (`\x1F`, the canonical record-separator
//! character) between components so a value like `"Bach\x1FCello"`
//! never collides with `"Bach Cello"`. Truncation to 16 hex
//! chars (64 bits of entropy) keeps the identifiers short
//! enough to embed in operator-UI URLs while remaining
//! collision-resistant within any personal library.
//!
//! `work_id` = SHA-256(normalised(composer) + "\x1F" +
//! normalised(work))[..16].
//!
//! `recording_id` = SHA-256(normalised(album) + "\x1F" +
//! normalised(conductor) + "\x1F" + normalised(original_date))[..16].
//!
//! `None` components in the recording identifier collapse to
//! empty strings so chamber-music recordings without conductors
//! still produce stable IDs.
//!
//! # Grouping discipline
//!
//! A track contributes to a Work entry only when MPD's `Work`
//! tag is populated. The conservative path: NO synthesis from
//! Composer+Title when Work is absent. The catalogue acceptance
//! row `library-works-grouped-only-by-explicit-mpd-work-tag`
//! pins this so the UI ships matching rule.
//!
//! Distinct recordings within a work are identified by the
//! tuple `(album, conductor, original_date)`. Tracks with the
//! same `(work_id, album, conductor, original_date)` belong to
//! the same recording; the per-recording `track_uris` is the
//! ordered list of those tracks' file paths.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

use crate::mpd::{ClassicalTags, MpdLibraryEntry};

/// One Work entry for the `library.list_works` response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkSummary {
    /// Stable 16-hex-character identifier derived from
    /// (composer, work). See module docs for the derivation.
    pub(crate) work_id: String,
    /// Canonical work name (e.g. "Symphony No. 5 in C minor,
    /// Op. 67"). MPD's `Work` tag verbatim.
    pub(crate) name: String,
    /// Composer of the work, when MPD's `Composer` tag is set
    /// on the contributing tracks. `None` when the contributing
    /// tracks have no Composer tag (rare; an operator may
    /// tag a Work without a Composer).
    pub(crate) composer: Option<String>,
    /// Number of distinct recordings of the work in the
    /// library — distinct `(album, conductor, original_date)`
    /// tuples among contributing tracks.
    pub(crate) recording_count: u32,
    /// Sorted distinct source ids the work appears under. The
    /// playback warden's source-resolver determines source per
    /// track; the aggregation walks the resolved set per work.
    pub(crate) sources: Vec<String>,
}

/// One recording within a work for the
/// `library.get_work_recordings` response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkRecording {
    /// Stable 16-hex-character identifier derived from
    /// (album, conductor, original_date).
    pub(crate) recording_id: String,
    /// Conductor of the recording, when MPD's `Conductor`
    /// tag is set on the contributing tracks. `None` for
    /// chamber music and solo recordings.
    pub(crate) conductor: Option<String>,
    /// Ensemble (orchestra / chamber group / quartet). MPD's
    /// `Ensemble` tag.
    pub(crate) ensemble: Option<String>,
    /// Featured performer (soloist). MPD's `Performer` tag.
    pub(crate) performer: Option<String>,
    /// Year the recording was made (MPD `OriginalDate`).
    pub(crate) original_date: Option<String>,
    /// Year of release / issue (MPD `Date`).
    pub(crate) recording_date: Option<String>,
    /// Record label (MPD `Label`).
    pub(crate) label: Option<String>,
    /// Distribution medium (MPD `Media`).
    pub(crate) medium: Option<String>,
    /// MPD-relative album path the contributing tracks live
    /// under. Used by the UI to navigate to the album.
    pub(crate) album_uri: String,
    /// Ordered list of MPD-relative track URIs comprising the
    /// recording (movements). Order follows MPD's
    /// `listallinfo` traversal.
    pub(crate) track_uris: Vec<String>,
    /// Sum of track durations in milliseconds. `None` when
    /// none of the contributing tracks reported a duration.
    pub(crate) total_duration_ms: Option<u64>,
}

/// Counters projected onto the `audio_library_state` envelope.
///
/// All `Option<u32>` per the wire-shape-defaults-must-be-truth-
/// or-null invariant: `None` means the scan has not yet
/// computed the value; never default to `0` (which would
/// conflate "library has zero classical tracks" with "scan
/// hasn't run yet").
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ClassicalCounters {
    /// Count of tracks whose MPD `Composer` tag is populated.
    pub(crate) total_tracks_with_composer: Option<u32>,
    /// Count of distinct (composer, work) pairs in the
    /// library — i.e. distinct Work entries.
    pub(crate) distinct_works: Option<u32>,
    /// Count of distinct Works that have more than one
    /// recording. The "more than one recording" predicate is
    /// the operator-meaningful classical-library indicator
    /// that drives the UI's Auto/On/Off resolution for the
    /// Works shelf.
    pub(crate) works_with_multiple_recordings: Option<u32>,
}

/// Aggregation output from one pass over MPD's database.
///
/// Computed by [`aggregate`] from the `listallinfo` response;
/// fed into the verb response paths (list_works / get_work_recordings)
/// and the library_state counter projection. Cached on the
/// LibraryContext and refreshed on `Database` / `Update` idle
/// events so steady-state operator queries hit the cache.
#[derive(Debug, Clone, Default)]
pub(crate) struct WorksAggregate {
    /// Sorted by composer-then-name. Per work: name, composer,
    /// recording_count, sources.
    pub(crate) works: Vec<WorkSummary>,
    /// Per-work recordings index keyed by `work_id`. Each
    /// vector is the recording set for that work, ordered by
    /// (original_date, label, conductor) ascending.
    pub(crate) recordings_by_work: BTreeMap<String, Vec<WorkRecording>>,
    /// Library state counter snapshot.
    pub(crate) counters: ClassicalCounters,
}

/// Compute the 16-hex-character stable identifier for a Work.
pub(crate) fn work_id_for(composer: Option<&str>, work: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(normalise(composer.unwrap_or("")).as_bytes());
    hasher.update([0x1F]);
    hasher.update(normalise(work).as_bytes());
    hex::encode(&hasher.finalize()[..8])
}

/// Compute the 16-hex-character stable identifier for a
/// Recording. None components collapse to the empty string.
pub(crate) fn recording_id_for(
    album: Option<&str>,
    conductor: Option<&str>,
    original_date: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(normalise(album.unwrap_or("")).as_bytes());
    hasher.update([0x1F]);
    hasher.update(normalise(conductor.unwrap_or("")).as_bytes());
    hasher.update([0x1F]);
    hasher.update(normalise(original_date.unwrap_or("")).as_bytes());
    hex::encode(&hasher.finalize()[..8])
}

/// Normalise a hash input: trim, lowercase, collapse internal
/// whitespace runs to single spaces. Keeps identifiers stable
/// against minor MPD tag editing (extra trailing space, case
/// shift) while leaving the underlying data unchanged.
fn normalise(s: &str) -> String {
    let trimmed = s.trim();
    let lower = trimmed.to_lowercase();
    // Collapse runs of whitespace to a single space.
    let mut out = String::with_capacity(lower.len());
    let mut prev_space = false;
    for c in lower.chars() {
        if c.is_whitespace() {
            if !prev_space && !out.is_empty() {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out.trim_end().to_string()
}

/// Walk a `listallinfo` response and produce the complete
/// aggregation: works list, per-work recordings, classical
/// counters. The walk is single-pass over the entries; the
/// final sort order is deterministic so subject_states diffs
/// remain stable across reboots.
pub(crate) fn aggregate(
    entries: &[MpdLibraryEntry],
    source_id_for_path: impl Fn(&str) -> Option<String>,
) -> WorksAggregate {
    // Per-(work_id) staging: collect tracks contributing to
    // the work. Per-(work_id, recording_id) sub-staging:
    // collect tracks comprising the recording.
    let mut work_tracks: BTreeMap<String, WorkStaging> = BTreeMap::new();

    let mut total_tracks_with_composer: u32 = 0;
    let mut tracks_with_work: u32 = 0;

    for entry in entries {
        let (path, classical) = match entry {
            MpdLibraryEntry::File {
                path, classical, ..
            } => (path, classical),
            _ => continue,
        };
        if classical.composer.is_some() {
            total_tracks_with_composer =
                total_tracks_with_composer.saturating_add(1);
        }
        let work = match classical.work.as_deref() {
            Some(w) => w,
            None => continue,
        };
        tracks_with_work = tracks_with_work.saturating_add(1);
        let work_id = work_id_for(classical.composer.as_deref(), work);
        let recording_id = recording_id_for(
            classical_album_from(entry),
            classical.conductor.as_deref(),
            classical.original_date.as_deref(),
        );
        let album_uri = album_uri_from_path(path);

        let staging = work_tracks.entry(work_id.clone()).or_insert_with(|| {
            WorkStaging::new(work.to_string(), classical.composer.clone())
        });
        staging.note_track(
            path.clone(),
            recording_id,
            classical,
            album_uri,
            duration_from(entry),
            source_id_for_path(path),
        );
    }

    let mut works: Vec<WorkSummary> = Vec::with_capacity(work_tracks.len());
    let mut recordings_by_work: BTreeMap<String, Vec<WorkRecording>> =
        BTreeMap::new();
    let mut works_with_multiple_recordings: u32 = 0;

    for (work_id, staging) in work_tracks {
        let (summary, recordings) = staging.finalise(work_id.clone());
        if recordings.len() > 1 {
            works_with_multiple_recordings =
                works_with_multiple_recordings.saturating_add(1);
        }
        recordings_by_work.insert(work_id, recordings);
        works.push(summary);
    }
    // Final stable ordering: composer ascending, then name
    // ascending, breaking ties on work_id for determinism.
    works.sort_by(|a, b| {
        a.composer
            .as_deref()
            .unwrap_or("")
            .cmp(b.composer.as_deref().unwrap_or(""))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.work_id.cmp(&b.work_id))
    });

    let distinct_works = u32::try_from(works.len()).unwrap_or(u32::MAX);
    let counters = ClassicalCounters {
        total_tracks_with_composer: Some(total_tracks_with_composer),
        distinct_works: Some(distinct_works),
        works_with_multiple_recordings: Some(works_with_multiple_recordings),
    };
    let _ = tracks_with_work; // counter not surfaced today
    WorksAggregate {
        works,
        recordings_by_work,
        counters,
    }
}

/// Album path heuristic — the parent directory of the track
/// file. Used as the `album_uri` on recordings so the UI can
/// navigate to the album view. MPD does not expose an album
/// path explicitly; the directory-as-album convention is the
/// near-universal organisation pattern.
fn album_uri_from_path(path: &str) -> String {
    match path.rfind('/') {
        Some(i) => path[..i].to_string(),
        None => path.to_string(),
    }
}

fn classical_album_from(entry: &MpdLibraryEntry) -> Option<&str> {
    if let MpdLibraryEntry::File { album, .. } = entry {
        album.as_deref()
    } else {
        None
    }
}

fn duration_from(entry: &MpdLibraryEntry) -> Option<u64> {
    if let MpdLibraryEntry::File {
        duration: Some(d), ..
    } = entry
    {
        Some(d.as_millis() as u64)
    } else {
        None
    }
}

#[derive(Debug)]
struct WorkStaging {
    name: String,
    composer: Option<String>,
    /// (recording_id → recording staging) per the work.
    recordings: BTreeMap<String, RecordingStaging>,
    sources: std::collections::BTreeSet<String>,
}

impl WorkStaging {
    fn new(name: String, composer: Option<String>) -> Self {
        Self {
            name,
            composer,
            recordings: BTreeMap::new(),
            sources: Default::default(),
        }
    }

    fn note_track(
        &mut self,
        path: String,
        recording_id: String,
        classical: &ClassicalTags,
        album_uri: String,
        duration_ms: Option<u64>,
        source_id: Option<String>,
    ) {
        if let Some(s) = source_id {
            self.sources.insert(s);
        }
        let staging = self
            .recordings
            .entry(recording_id.clone())
            .or_insert_with(|| {
                RecordingStaging::new(recording_id, classical, album_uri)
            });
        staging.note_track(path, duration_ms);
    }

    fn finalise(self, work_id: String) -> (WorkSummary, Vec<WorkRecording>) {
        let recording_count =
            u32::try_from(self.recordings.len()).unwrap_or(u32::MAX);
        let mut recordings: Vec<WorkRecording> = self
            .recordings
            .into_values()
            .map(RecordingStaging::finalise)
            .collect();
        // Per-work recordings sorted by original_date then
        // label then conductor for stable presentation.
        recordings.sort_by(|a, b| {
            a.original_date
                .as_deref()
                .unwrap_or("")
                .cmp(b.original_date.as_deref().unwrap_or(""))
                .then_with(|| {
                    a.label
                        .as_deref()
                        .unwrap_or("")
                        .cmp(b.label.as_deref().unwrap_or(""))
                })
                .then_with(|| {
                    a.conductor
                        .as_deref()
                        .unwrap_or("")
                        .cmp(b.conductor.as_deref().unwrap_or(""))
                })
        });
        let summary = WorkSummary {
            work_id,
            name: self.name,
            composer: self.composer,
            recording_count,
            sources: self.sources.into_iter().collect(),
        };
        (summary, recordings)
    }
}

#[derive(Debug)]
struct RecordingStaging {
    recording_id: String,
    conductor: Option<String>,
    ensemble: Option<String>,
    performer: Option<String>,
    original_date: Option<String>,
    recording_date: Option<String>,
    label: Option<String>,
    medium: Option<String>,
    album_uri: String,
    track_uris: Vec<String>,
    total_duration_ms: Option<u64>,
}

impl RecordingStaging {
    fn new(
        recording_id: String,
        classical: &ClassicalTags,
        album_uri: String,
    ) -> Self {
        Self {
            recording_id,
            conductor: classical.conductor.clone(),
            ensemble: classical.ensemble.clone(),
            performer: classical.performer.clone(),
            original_date: classical.original_date.clone(),
            recording_date: classical.recording_date.clone(),
            label: classical.label.clone(),
            medium: classical.medium.clone(),
            album_uri,
            track_uris: Vec::new(),
            total_duration_ms: None,
        }
    }

    fn note_track(&mut self, path: String, duration_ms: Option<u64>) {
        self.track_uris.push(path);
        if let Some(d) = duration_ms {
            self.total_duration_ms =
                Some(self.total_duration_ms.unwrap_or(0).saturating_add(d));
        }
    }

    fn finalise(self) -> WorkRecording {
        WorkRecording {
            recording_id: self.recording_id,
            conductor: self.conductor,
            ensemble: self.ensemble,
            performer: self.performer,
            original_date: self.original_date,
            recording_date: self.recording_date,
            label: self.label,
            medium: self.medium,
            album_uri: self.album_uri,
            track_uris: self.track_uris,
            total_duration_ms: self.total_duration_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn file(
        path: &str,
        composer: Option<&str>,
        work: Option<&str>,
        conductor: Option<&str>,
        original_date: Option<&str>,
        album: Option<&str>,
        duration_ms: Option<u64>,
    ) -> MpdLibraryEntry {
        let classical = ClassicalTags {
            composer: composer.map(String::from),
            work: work.map(String::from),
            conductor: conductor.map(String::from),
            original_date: original_date.map(String::from),
            ..Default::default()
        };
        MpdLibraryEntry::File {
            path: path.to_string(),
            title: None,
            artist: None,
            album: album.map(String::from),
            duration: duration_ms.map(Duration::from_millis),
            classical,
        }
    }

    #[test]
    fn work_id_is_16_hex_chars() {
        let id = work_id_for(Some("Beethoven"), "Symphony No. 5");
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn work_id_stable_across_minor_input_drift() {
        // Trailing whitespace, case shifts MUST not change
        // the identifier — the operator may edit MPD tags
        // and we still want the same work_id to surface.
        let a = work_id_for(Some("Beethoven"), "Symphony No. 5");
        let b = work_id_for(Some(" beethoven "), "Symphony No. 5");
        let c = work_id_for(Some("BEETHOVEN"), "  Symphony No. 5  ");
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn work_id_distinguishes_distinct_works_under_same_composer() {
        let a = work_id_for(Some("Beethoven"), "Symphony No. 5");
        let b = work_id_for(Some("Beethoven"), "Symphony No. 9");
        assert_ne!(a, b);
    }

    #[test]
    fn work_id_distinguishes_collision_via_unit_separator() {
        // "Bach" + "Cello" vs "BachCello" — the unit
        // separator (0x1F) between components prevents the
        // collision that would otherwise hash identically.
        let a = work_id_for(Some("Bach"), "Cello");
        let b = work_id_for(Some(""), "BachCello");
        assert_ne!(a, b);
    }

    #[test]
    fn recording_id_handles_null_components() {
        // Chamber music recording without conductor — still
        // produces a stable id from the album + original_date.
        let id =
            recording_id_for(Some("Goldberg Variations"), None, Some("1981"));
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn aggregate_skips_tracks_without_work_tag() {
        // Track without Work tag MUST NOT appear in any
        // WorkSummary; the conservative grouping rule the
        // catalogue acceptance row pins.
        let entries = vec![
            file(
                "Pop/track.mp3",
                Some("Ed Sheeran"),
                None,
                None,
                Some("2016"),
                Some("Album"),
                Some(180_000),
            ),
            file(
                "Classical/sym5/i.flac",
                Some("Beethoven"),
                Some("Symphony No. 5"),
                Some("Karajan"),
                Some("1962"),
                Some("Karajan Beethoven"),
                Some(420_000),
            ),
        ];
        let agg = aggregate(&entries, |_| None);
        assert_eq!(agg.works.len(), 1);
        assert_eq!(agg.works[0].name, "Symphony No. 5");
        // total_tracks_with_composer counts BOTH tracks (the
        // Composer tag is set on both), but distinct_works
        // counts only the one with the Work tag.
        assert_eq!(agg.counters.total_tracks_with_composer, Some(2));
        assert_eq!(agg.counters.distinct_works, Some(1));
        assert_eq!(agg.counters.works_with_multiple_recordings, Some(0));
    }

    #[test]
    fn aggregate_groups_distinct_recordings_within_a_work() {
        // Two different recordings of the same Symphony No. 5:
        // Karajan/1962 and Bernstein/1979. Both should appear
        // under the same work_id; the works-with-multiple-
        // recordings counter should tick.
        let entries = vec![
            file(
                "Karajan_Beethoven_5/i.flac",
                Some("Beethoven"),
                Some("Symphony No. 5"),
                Some("Karajan"),
                Some("1962"),
                Some("Karajan Beethoven"),
                Some(420_000),
            ),
            file(
                "Karajan_Beethoven_5/ii.flac",
                Some("Beethoven"),
                Some("Symphony No. 5"),
                Some("Karajan"),
                Some("1962"),
                Some("Karajan Beethoven"),
                Some(540_000),
            ),
            file(
                "Bernstein_Beethoven_5/i.flac",
                Some("Beethoven"),
                Some("Symphony No. 5"),
                Some("Bernstein"),
                Some("1979"),
                Some("Bernstein Beethoven"),
                Some(440_000),
            ),
        ];
        let agg = aggregate(&entries, |_| None);
        assert_eq!(agg.works.len(), 1);
        assert_eq!(agg.works[0].recording_count, 2);
        assert_eq!(agg.counters.distinct_works, Some(1));
        assert_eq!(agg.counters.works_with_multiple_recordings, Some(1));
        let recordings = &agg.recordings_by_work[&agg.works[0].work_id];
        assert_eq!(recordings.len(), 2);
        // Recordings ordered by original_date ascending.
        assert_eq!(recordings[0].original_date.as_deref(), Some("1962"));
        assert_eq!(recordings[1].original_date.as_deref(), Some("1979"));
        // First recording carries two tracks; total duration
        // sums the contributing tracks.
        assert_eq!(recordings[0].track_uris.len(), 2);
        assert_eq!(recordings[0].total_duration_ms, Some(960_000));
    }

    #[test]
    fn counters_are_truth_or_null_by_default() {
        // A fresh ClassicalCounters is all-None — the
        // wire-shape-defaults-must-be-truth-or-null invariant.
        let c = ClassicalCounters::default();
        assert_eq!(c.total_tracks_with_composer, None);
        assert_eq!(c.distinct_works, None);
        assert_eq!(c.works_with_multiple_recordings, None);
    }

    #[test]
    fn aggregate_counters_populated_after_walk() {
        // After a walk completes, the counters are Some(_),
        // never None — the cascade goes from "not computed
        // yet" (None) to "computed" (Some(value)).
        let entries: Vec<MpdLibraryEntry> = vec![];
        let agg = aggregate(&entries, |_| None);
        assert_eq!(agg.counters.total_tracks_with_composer, Some(0));
        assert_eq!(agg.counters.distinct_works, Some(0));
        assert_eq!(agg.counters.works_with_multiple_recordings, Some(0));
    }
}
