//! Subject and relation emission from MPD playback state.
//!
//! The playback warden announces `track` and `album` subjects and
//! the `album_of` relation between them whenever MPD reports a
//! current song. This gives album-art and metadata respondents a
//! graph to walk from "what is playing right now" to the records
//! that describe it.
//!
//! # Addressing schemes
//!
//! Two schemes, both owned by this plugin:
//!
//! - `mpd-path`: value is MPD's `file` field (relative library
//!   path or stream URL). Used for `track` subjects.
//! - `mpd-album`: value is `"{artist}|{album}"` where `artist` is
//!   the MPD `Artist` tag if present and non-empty, else
//!   `"unknown"`. Used for `album` subjects. The compound value
//!   disambiguates same-titled albums from different artists
//!   (e.g. the many albums titled "Greatest Hits"); the pipe
//!   separator is chosen because album names rarely contain
//!   pipes.
//!
//! An `AlbumArtist`-preferred variant is a natural future
//! refinement; today's [`crate::mpd::MpdSong`] carries
//! only an `artist` field, so the warden uses that. When the MPD
//! connection layer gains a distinct `album_artist` field, this
//! module's artist-resolution helper swaps in that preference
//! without a catalogue or emission-contract change.
//!
//! # Catalogue alignment
//!
//! Subject types (`track`, `album`) and the relation predicate
//! (`album_of`) are declared in the consuming distribution's
//! catalogue. The
//! steward validates names at admission; this module must match
//! the catalogue verbatim or subject announcements and relation
//! assertions will be rejected with
//! [`ReportError::Invalid`](evo_plugin_sdk::contract::ReportError::Invalid).
//!
//! # Ordering
//!
//! A relation assertion fails if either endpoint is not yet a
//! known subject. [`SubjectEmitter::emit_song`] therefore
//! announces in strict order:
//!
//! 1. Track subject (always, if `file_path` is non-empty).
//! 2. Album subject (only if the song has a non-empty `Album`
//!    tag).
//! 3. `album_of` relation (only if both subjects were
//!    successfully announced).
//!
//! A failure at any step aborts the remaining steps for that
//! song (the relation would be rejected anyway) and is logged at
//! warn level. Playback itself is never disrupted: subject
//! emission is best-effort infrastructure, not part of the
//! custody contract.
//!
//! # Retraction policy
//!
//! Phase 3.4 is additive only. Tracks and albums accumulate in
//! the steward's registry as they are played; relations
//! accumulate alongside. When a plugin deregisters, the steward
//! handles claimant cleanup. Per-song retractions are deferred
//! to a future phase where the cost-benefit is clear.

use std::sync::Arc;

use evo_plugin_sdk::contract::{
    ExternalAddressing, RelationAnnouncer, RelationAssertion,
    SubjectAnnouncement, SubjectAnnouncer,
};

use crate::mpd::MpdSong;
use crate::PLUGIN_NAME;

// ----- catalogue-aligned constants -----

/// Subject type for tracks. Must match the catalogue.
const SUBJECT_TYPE_TRACK: &str = "track";
/// Subject type for albums. Must match the catalogue.
const SUBJECT_TYPE_ALBUM: &str = "album";
/// Relation predicate for track -> album. Must match the catalogue.
const PREDICATE_ALBUM_OF: &str = "album_of";

/// Addressing scheme this plugin owns for MPD file paths.
const SCHEME_MPD_PATH: &str = "mpd-path";
/// Addressing scheme this plugin owns for MPD album identities.
const SCHEME_MPD_ALBUM: &str = "mpd-album";

/// Fallback artist value when the MPD `Artist` tag is missing or
/// empty. Using a concrete sentinel (rather than, say, skipping
/// the album entirely) keeps album-addressing stable for
/// compilations and tag-less imports: two tracks with the same
/// `Album` but no `Artist` still belong to the same album
/// subject.
const UNKNOWN_ARTIST: &str = "unknown";

/// Separator used in the compound `mpd-album` value
/// (`"{artist}|{album}"`). Pipe was chosen because album titles
/// rarely contain it; a title that does contain a pipe still
/// produces a valid (albeit slightly odd-looking) addressing.
const ALBUM_ADDRESSING_SEPARATOR: char = '|';

// ----- the emitter -----

/// Bundle of subject and relation announcer handles.
///
/// Held by the playback supervisor for the life of a custody.
/// Cloned cheaply (Arc bump on each field) when passed between
/// tasks. Tests use [`SubjectEmitter::null`] (test-only) to
/// construct a no-op emitter that records nothing.
#[derive(Clone)]
pub(crate) struct SubjectEmitter {
    subjects: Arc<dyn SubjectAnnouncer>,
    relations: Arc<dyn RelationAnnouncer>,
}

impl SubjectEmitter {
    /// Construct a new emitter backed by live announcer handles.
    /// Called from [`crate::MpdPlaybackPlugin`] at `take_custody`
    /// time with the Arcs that arrived in
    /// [`evo_plugin_sdk::contract::LoadContext`] at `load` time.
    pub(crate) fn new(
        subjects: Arc<dyn SubjectAnnouncer>,
        relations: Arc<dyn RelationAnnouncer>,
    ) -> Self {
        Self {
            subjects,
            relations,
        }
    }

    /// Emit track + album + relation for a song.
    ///
    /// See the module-level docs for ordering, retraction
    /// policy, and error handling. Best-effort: errors from the
    /// announcers are logged but not propagated. Playback is
    /// never disrupted by announcer failures.
    pub(crate) async fn emit_song(&self, song: &MpdSong) {
        if song.file_path.is_empty() {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                "emit_song: empty file_path (no current song); nothing to emit"
            );
            return;
        }

        let track_addressing =
            ExternalAddressing::new(SCHEME_MPD_PATH, &song.file_path);
        let track_announcement = SubjectAnnouncement::new(
            SUBJECT_TYPE_TRACK,
            vec![track_addressing.clone()],
        );

        if let Err(e) = self.subjects.announce(track_announcement).await {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                file = %song.file_path,
                "track subject announcement failed; skipping album and relation"
            );
            return;
        }

        // No album tag, or empty album tag: track announced,
        // nothing more to do. Not an error; many files (streams
        // in particular) legitimately lack an album.
        let album_name = match song.album.as_deref() {
            Some(s) if !s.is_empty() => s,
            _ => {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    file = %song.file_path,
                    "no album tag; emitted track only"
                );
                return;
            }
        };

        let album_value = build_album_value(song.artist.as_deref(), album_name);
        let album_addressing =
            ExternalAddressing::new(SCHEME_MPD_ALBUM, album_value);
        let album_announcement = SubjectAnnouncement::new(
            SUBJECT_TYPE_ALBUM,
            vec![album_addressing.clone()],
        );

        if let Err(e) = self.subjects.announce(album_announcement).await {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                file = %song.file_path,
                "album subject announcement failed; skipping relation"
            );
            return;
        }

        let relation = RelationAssertion::new(
            track_addressing,
            PREDICATE_ALBUM_OF,
            album_addressing,
        );

        if let Err(e) = self.relations.assert(relation).await {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                file = %song.file_path,
                "album_of relation assertion failed; subjects remain"
            );
        }
    }
}

/// Compose the `mpd-album` value from optional artist and a
/// known album name. Empty artist collapses to the UNKNOWN_ARTIST
/// sentinel so the compound value is always a well-formed pair.
fn build_album_value(artist: Option<&str>, album: &str) -> String {
    let artist = artist.filter(|s| !s.is_empty()).unwrap_or(UNKNOWN_ARTIST);
    format!("{}{}{}", artist, ALBUM_ADDRESSING_SEPARATOR, album)
}

// ----- test-only null emitter -----

#[cfg(test)]
impl SubjectEmitter {
    /// A null emitter for tests that are not exercising subject
    /// emission directly. Calls to [`Self::emit_song`] succeed
    /// silently; no announcer invocations are recorded. Tests
    /// that *do* want to assert on emitter behaviour use the
    /// capturing announcers in
    /// [`crate::playback_supervisor::test_mock`].
    pub(crate) fn null() -> Self {
        Self {
            subjects: Arc::new(NullSubjectAnnouncer),
            relations: Arc::new(NullRelationAnnouncer),
        }
    }
}

#[cfg(test)]
struct NullSubjectAnnouncer;

#[cfg(test)]
impl SubjectAnnouncer for NullSubjectAnnouncer {
    fn announce<'a>(
        &'a self,
        _: SubjectAnnouncement,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<(), evo_plugin_sdk::contract::ReportError>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async { Ok(()) })
    }

    fn retract<'a>(
        &'a self,
        _: ExternalAddressing,
        _: Option<String>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<(), evo_plugin_sdk::contract::ReportError>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async { Ok(()) })
    }
}

#[cfg(test)]
struct NullRelationAnnouncer;

#[cfg(test)]
impl RelationAnnouncer for NullRelationAnnouncer {
    fn assert<'a>(
        &'a self,
        _: RelationAssertion,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<(), evo_plugin_sdk::contract::ReportError>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async { Ok(()) })
    }

    fn retract<'a>(
        &'a self,
        _: evo_plugin_sdk::contract::RelationRetraction,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<(), evo_plugin_sdk::contract::ReportError>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async { Ok(()) })
    }
}

// ----- tests -----

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::playback_supervisor::test_mock::{
        capturing_emitter, CapturingRelationAnnouncer,
        CapturingSubjectAnnouncer,
    };

    fn song_with(
        file_path: &str,
        title: Option<&str>,
        artist: Option<&str>,
        album: Option<&str>,
    ) -> MpdSong {
        MpdSong {
            file_path: file_path.to_string(),
            title: title.map(String::from),
            artist: artist.map(String::from),
            album: album.map(String::from),
            duration: Some(Duration::from_secs(180)),
        }
    }

    // ===== build_album_value (pure helper) =====

    #[test]
    fn build_album_value_uses_artist_when_present() {
        assert_eq!(
            build_album_value(Some("Pink Floyd"), "The Wall"),
            "Pink Floyd|The Wall"
        );
    }

    #[test]
    fn build_album_value_uses_unknown_when_artist_none() {
        assert_eq!(build_album_value(None, "The Wall"), "unknown|The Wall");
    }

    #[test]
    fn build_album_value_uses_unknown_when_artist_empty() {
        assert_eq!(build_album_value(Some(""), "The Wall"), "unknown|The Wall");
    }

    #[test]
    fn build_album_value_preserves_unusual_characters_in_album() {
        // Pipes in album titles are rare but valid; they produce
        // an unusual-but-stable compound value.
        assert_eq!(build_album_value(Some("A"), "B|C"), "A|B|C");
    }

    // ===== emit_song acceptance paths =====

    #[tokio::test]
    async fn emit_song_full_path_announces_both_and_relation() {
        let (subjects, relations, emitter) = capturing_emitter();

        emitter
            .emit_song(&song_with(
                "library/pf/thewall/01.flac",
                Some("In the Flesh?"),
                Some("Pink Floyd"),
                Some("The Wall"),
            ))
            .await;

        assert_eq!(subjects.count(), 2, "expected track + album announcements");
        assert_eq!(relations.count(), 1, "expected 1 album_of relation");

        // Inspect the track announcement.
        let track = subjects.at(0).unwrap();
        assert_eq!(track.subject_type, "track");
        assert_eq!(track.addressings.len(), 1);
        assert_eq!(track.addressings[0].scheme, "mpd-path");
        assert_eq!(track.addressings[0].value, "library/pf/thewall/01.flac");

        // Inspect the album announcement.
        let album = subjects.at(1).unwrap();
        assert_eq!(album.subject_type, "album");
        assert_eq!(album.addressings.len(), 1);
        assert_eq!(album.addressings[0].scheme, "mpd-album");
        assert_eq!(album.addressings[0].value, "Pink Floyd|The Wall");

        // Inspect the relation.
        let rel = relations.at(0).unwrap();
        assert_eq!(rel.predicate, "album_of");
        assert_eq!(rel.source.scheme, "mpd-path");
        assert_eq!(rel.source.value, "library/pf/thewall/01.flac");
        assert_eq!(rel.target.scheme, "mpd-album");
        assert_eq!(rel.target.value, "Pink Floyd|The Wall");
    }

    #[tokio::test]
    async fn emit_song_stream_url_is_a_valid_track_path() {
        let (subjects, relations, emitter) = capturing_emitter();

        emitter
            .emit_song(&song_with(
                "http://radio.example.com/stream.mp3",
                None,
                None,
                None,
            ))
            .await;

        // Stream URL, no album: track announced, nothing else.
        assert_eq!(subjects.count(), 1);
        assert_eq!(relations.count(), 0);
        assert_eq!(
            subjects.at(0).unwrap().addressings[0].value,
            "http://radio.example.com/stream.mp3"
        );
    }

    // ===== missing-tag graceful degradation =====

    #[tokio::test]
    async fn emit_song_missing_album_announces_track_only() {
        let (subjects, relations, emitter) = capturing_emitter();

        emitter
            .emit_song(&song_with(
                "library/single.flac",
                Some("A Single"),
                Some("Artist X"),
                None,
            ))
            .await;

        assert_eq!(subjects.count(), 1);
        assert_eq!(relations.count(), 0);
        assert_eq!(subjects.at(0).unwrap().subject_type, "track");
    }

    #[tokio::test]
    async fn emit_song_empty_album_tag_announces_track_only() {
        let (subjects, relations, emitter) = capturing_emitter();

        emitter
            .emit_song(&song_with(
                "library/single.flac",
                Some("A Single"),
                Some("Artist X"),
                Some(""),
            ))
            .await;

        assert_eq!(subjects.count(), 1);
        assert_eq!(relations.count(), 0);
    }

    #[tokio::test]
    async fn emit_song_missing_artist_uses_unknown() {
        let (subjects, relations, emitter) = capturing_emitter();

        emitter
            .emit_song(&song_with(
                "library/mystery.flac",
                Some("A Track"),
                None,
                Some("A Compilation"),
            ))
            .await;

        assert_eq!(subjects.count(), 2);
        assert_eq!(relations.count(), 1);
        assert_eq!(
            subjects.at(1).unwrap().addressings[0].value,
            "unknown|A Compilation"
        );
    }

    #[tokio::test]
    async fn emit_song_empty_artist_uses_unknown() {
        let (subjects, _relations, emitter) = capturing_emitter();

        emitter
            .emit_song(&song_with(
                "library/mystery.flac",
                None,
                Some(""),
                Some("A Compilation"),
            ))
            .await;

        assert_eq!(
            subjects.at(1).unwrap().addressings[0].value,
            "unknown|A Compilation"
        );
    }

    // ===== empty file path is a no-op =====

    #[tokio::test]
    async fn emit_song_with_empty_file_path_emits_nothing() {
        let (subjects, relations, emitter) = capturing_emitter();

        emitter.emit_song(&song_with("", None, None, None)).await;

        assert_eq!(subjects.count(), 0);
        assert_eq!(relations.count(), 0);
    }

    // ===== ordering =====

    #[tokio::test]
    async fn emit_song_announces_track_before_album() {
        let (subjects, _relations, emitter) = capturing_emitter();

        emitter
            .emit_song(&song_with("a.flac", None, Some("A"), Some("B")))
            .await;

        assert_eq!(subjects.at(0).unwrap().subject_type, "track");
        assert_eq!(subjects.at(1).unwrap().subject_type, "album");
    }

    #[tokio::test]
    async fn emit_song_asserts_relation_only_after_both_subjects() {
        let (subjects, relations, emitter) = capturing_emitter();

        emitter
            .emit_song(&song_with("a.flac", None, Some("A"), Some("B")))
            .await;

        // The capturing announcers record in call order across
        // their timelines; we cross-check by counts.
        assert_eq!(subjects.count(), 2);
        assert_eq!(relations.count(), 1);
    }

    // ===== announcer error handling =====

    #[tokio::test]
    async fn emit_song_swallows_subject_announce_errors() {
        use std::sync::Arc;

        let failing_subjects =
            Arc::new(CapturingSubjectAnnouncer::failing_with_invalid());
        let relations = Arc::new(CapturingRelationAnnouncer::default());

        let emitter =
            SubjectEmitter::new(failing_subjects.clone(), relations.clone());

        // Must not panic, must not propagate.
        emitter
            .emit_song(&song_with("a.flac", None, Some("A"), Some("B")))
            .await;

        // Because the track announcement failed, no album or
        // relation follow-up was attempted.
        assert_eq!(relations.count(), 0);
    }

    #[tokio::test]
    async fn emit_song_swallows_relation_assert_errors() {
        use std::sync::Arc;

        let subjects = Arc::new(CapturingSubjectAnnouncer::default());
        let failing_relations =
            Arc::new(CapturingRelationAnnouncer::failing_with_invalid());

        let emitter =
            SubjectEmitter::new(subjects.clone(), failing_relations.clone());

        // Both subjects announce OK; relation assert fails; emit
        // returns cleanly.
        emitter
            .emit_song(&song_with("a.flac", None, Some("A"), Some("B")))
            .await;

        assert_eq!(subjects.count(), 2);
        // The relation was attempted even though it failed.
        assert_eq!(failing_relations.count(), 1);
    }

    // ===== null emitter is a true no-op =====

    #[tokio::test]
    async fn null_emitter_does_not_panic() {
        let e = SubjectEmitter::null();
        e.emit_song(&song_with("a.flac", Some("T"), Some("A"), Some("B")))
            .await;
    }
}
