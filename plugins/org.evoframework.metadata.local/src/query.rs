//! `metadata.query` v1: resolve tag metadata for a track or album subject (`mpd-path`, `mpd-album`).
//! Response includes grouped fields for classical, credits, MusicBrainz, dates, and file properties.
//!
//! Full wire field catalogue: `docs/METADATA_QUERY_V1.md` (in this plugin directory).

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use lofty::file::{AudioFile, TaggedFileExt};
use lofty::properties::FileProperties;
use lofty::read_from_path;
use lofty::tag::{Accessor, ItemKey, ItemValue, Tag};
use serde::Deserialize;
use serde::Serialize;

use crate::config::MetadataProfile;

/// `mpd-path`: MPD's `file` (library-relative or absolute).
pub(crate) const SCHEME_MPD_PATH: &str = "mpd-path";
/// `mpd-album`: `Artist|Album` — first matching track under [library] roots (tag scan via `evo_plugins_audio_shared`).
pub(crate) const SCHEME_MPD_ALBUM: &str = "mpd-album";

/// Truncate very large tag values (e.g. embedded lyrics) for stable memory on devices.
const MAX_TEXT_FIELD_BYTES: usize = 512_000;

/// Request (JSON v1; UTF-8). `target` matches `ExternalAddressing`-style schemes
/// from `org.evoframework.playback.mpd`, aligned with `org.evoframework.artwork.local`.
#[derive(Debug, Deserialize)]
pub(crate) struct MetadataQueryRequest {
    pub(crate) v: u8,
    pub(crate) target: QueryTarget,
}

#[derive(Debug, Deserialize)]
pub(crate) struct QueryTarget {
    pub(crate) scheme: String,
    pub(crate) value: String,
}

/// JSON body returned to the steward (v1). Flat fields are stable for simple UIs; nested groups
/// add classical, credits, identifiers, and technical data (Picard / MusicBrainz–friendly).
#[derive(Debug, Serialize)]
pub(crate) struct MetadataQueryResponse {
    v: u8,
    status: ResponseStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    /// On `status: ok`, the operator `metadata` profile that filtered this payload
    /// (`[metadata] profile` in the plugin TOML).
    #[serde(skip_serializing_if = "Option::is_none")]
    active_profile: Option<String>,

    // —— common flat fields (backward compatible) ——
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    /// Primary `artist` (first performer line); use `artists` for all values.
    #[serde(skip_serializing_if = "Option::is_none")]
    artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    album: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    album_artist: Option<String>,
    /// All per-tag artist lines (e.g. Vorbis `ARTIST` or ID3 `ARTISTS` / TPE1 repeats).
    #[serde(skip_serializing_if = "Option::is_none")]
    artists: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    genre: Option<String>,
    /// 1-based
    #[serde(skip_serializing_if = "Option::is_none")]
    track: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    track_total: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disc: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disc_total: Option<u32>,
    /// Parsed four-digit year when available (from year / date tags).
    #[serde(skip_serializing_if = "Option::is_none")]
    year: Option<u32>,
    /// Duration from the audio container.
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    subtitle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    script: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    comment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mood: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    initial_key: Option<String>,
    /// BPM as in tags (integer or decimal string, format-dependent).
    #[serde(skip_serializing_if = "Option::is_none")]
    bpm: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    lyrics: Option<String>,

    // —— nested (classical, credits, …) ——
    #[serde(skip_serializing_if = "Option::is_none")]
    sort: Option<SortMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    credits: Option<CreditsMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    classical: Option<ClassicalMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    original: Option<OriginalMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dates: Option<DatesMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    identifiers: Option<IdentifiersMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    replay_gain: Option<ReplayGainMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<FileMetadata>,

    #[serde(skip_serializing_if = "Option::is_none")]
    compilation: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    podcast: Option<bool>,

    /// Id3 TXXX-style and other [`ItemKey::Unknown`] frames: key = wire name, value = text; duplicate
    /// keys become `NAME@1`, `NAME@2`, … Binary values appear as `<binary: N bytes>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    extras: Option<BTreeMap<String, String>>,
}

/// Picard-style sort keys for media library ordering.
#[derive(Debug, Serialize)]
pub(crate) struct SortMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    track_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    album: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    track_artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    album_artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    composer: Option<String>,
}

/// Performers, creators, and label — critical for classical and jazz credits.
#[derive(Debug, Serialize)]
pub(crate) struct CreditsMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    composer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    conductor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lyricist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    arranger: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    writer: Option<String>,
    /// Orchestra / soloist / ensemble (TAG `PERFORMER`, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    performer: Option<String>,
    /// Multiple performer lines if present in the file.
    #[serde(skip_serializing_if = "Option::is_none")]
    performers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    producer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mix_engineer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    engineer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    publisher: Option<String>,
    /// Often “remixed by” / TPE4.
    #[serde(skip_serializing_if = "Option::is_none")]
    remixer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    director: Option<String>,
}

/// Work, movement, opera/show grouping — for classical, theatre, and large works.
#[derive(Debug, Serialize)]
pub(crate) struct ClassicalMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    work: Option<String>,
    /// Movement name (e.g. `Allegro`, not only the number).
    #[serde(skip_serializing_if = "Option::is_none")]
    movement: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    movement_number: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    movement_total: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    show_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_group: Option<String>,
}

/// “Original” album/artist (common on classical reissues).
#[derive(Debug, Serialize)]
pub(crate) struct OriginalMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    album: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lyricist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    release_date: Option<String>,
}

/// Full date strings (Picard) vs numeric `year` on the top-level.
#[derive(Debug, Serialize)]
pub(crate) struct DatesMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    recording: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    release: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    original_release: Option<String>,
}

/// Commercial and MusicBrainz identifiers.
#[derive(Debug, Serialize)]
pub(crate) struct IdentifiersMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    isrc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    catalog_number: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    barcode: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    musicbrainz_recording_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    musicbrainz_track_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    musicbrainz_release_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    musicbrainz_release_group_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    musicbrainz_artist_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    musicbrainz_release_artist_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    musicbrainz_work_id: Option<String>,
}

/// Replay gain from tags (strings as stored, e.g. `-7.24 dB`).
#[derive(Debug, Serialize)]
pub(crate) struct ReplayGainMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    track_gain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    track_peak: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    album_gain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    album_peak: Option<String>,
}

/// Measured from the file / stream, not from tags.
#[derive(Debug, Serialize)]
pub(crate) struct FileMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sample_rate_hz: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bit_depth: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    channels: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    channel_mask_bits: Option<u32>,
    /// Overall (container) bitrate in kbit/s if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    overall_bitrate_kbps: Option<u32>,
    /// Audio stream bitrate in kbit/s if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    audio_bitrate_kbps: Option<u32>,
}

/// Outcome of a query.
#[derive(Debug, Serialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResponseStatus {
    Ok,
    NotFound,
    /// JSON wire value retained for forward compatibility; not used by this plugin.
    #[allow(dead_code)]
    Unsupported,
    BadRequest,
}

impl MetadataQueryResponse {
    pub(crate) fn json_bytes(self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(&self)
    }

    fn v1_error(status: ResponseStatus, detail: Option<String>) -> Self {
        Self {
            v: 1,
            status,
            detail,
            active_profile: None,
            title: None,
            artist: None,
            album: None,
            album_artist: None,
            artists: None,
            genre: None,
            track: None,
            track_total: None,
            disc: None,
            disc_total: None,
            year: None,
            duration_ms: None,
            subtitle: None,
            language: None,
            script: None,
            comment: None,
            mood: None,
            initial_key: None,
            bpm: None,
            lyrics: None,
            sort: None,
            credits: None,
            classical: None,
            original: None,
            dates: None,
            identifiers: None,
            replay_gain: None,
            file: None,
            compilation: None,
            podcast: None,
            extras: None,
        }
    }
}

// ---- extraction helpers ---------------------------------------------------

fn opt_s(tag: &Tag, key: &ItemKey) -> Option<String> {
    tag_s(tag, key, MAX_TEXT_FIELD_BYTES)
}

/// Multiline comments: first line in `comment`, all lines in a dedicated block would need another
/// type; for now we join with newline if multiple `Comment` items.
fn first_comment_line(tag: &Tag) -> Option<String> {
    let mut it = tag.get_strings(&ItemKey::Comment);
    it.next().map(String::from)
}

fn bpm_string(tag: &Tag) -> Option<String> {
    [ItemKey::Bpm, ItemKey::IntegerBpm]
        .iter()
        .find_map(|k| tag_s(tag, k, 64))
}

fn tag_s(tag: &Tag, key: &ItemKey, cap: usize) -> Option<String> {
    let s = tag.get_string(key)?;
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let t = t.to_string();
    if t.len() <= cap {
        return Some(t);
    }
    Some(
        t.char_indices()
            .take_while(|(i, _)| *i < cap)
            .map(|(_, c)| c)
            .collect(),
    )
}

fn parse_u32_tag(tag: &Tag, key: &ItemKey) -> Option<u32> {
    let s = tag.get_string(key)?;
    s.parse::<u32>().ok()
}

fn parse_compilation_bool(tag: &Tag) -> Option<bool> {
    let s = tag.get_string(&ItemKey::FlagCompilation)?;
    match s.trim() {
        "1" | "true" | "True" | "yes" | "Yes" => Some(true),
        "0" | "false" | "False" | "no" | "No" => Some(false),
        _ => None,
    }
}

fn parse_podcast_bool(tag: &Tag) -> Option<bool> {
    let s = tag.get_string(&ItemKey::FlagPodcast)?;
    match s.trim() {
        "1" | "true" | "True" | "yes" | "Yes" => Some(true),
        "0" | "false" | "False" | "no" | "No" => Some(false),
        _ => None,
    }
}

/// Collect multiple `ItemKey` lines (e.g. `PERFORMER` × N).
fn tag_string_list(tag: &Tag, key: &ItemKey) -> Option<Vec<String>> {
    let v: Vec<String> = tag
        .get_strings(key)
        .map(String::from)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

fn sort_block(tag: &Tag) -> Option<SortMetadata> {
    let s = SortMetadata {
        track_title: opt_s(tag, &ItemKey::TrackTitleSortOrder),
        album: opt_s(tag, &ItemKey::AlbumTitleSortOrder),
        track_artist: opt_s(tag, &ItemKey::TrackArtistSortOrder),
        album_artist: opt_s(tag, &ItemKey::AlbumArtistSortOrder),
        composer: opt_s(tag, &ItemKey::ComposerSortOrder),
    };
    if s.track_title.is_none()
        && s.album.is_none()
        && s.track_artist.is_none()
        && s.album_artist.is_none()
        && s.composer.is_none()
    {
        return None;
    }
    Some(s)
}

fn credits_block(tag: &Tag) -> Option<CreditsMetadata> {
    let performers = tag_string_list(tag, &ItemKey::Performer);
    let c = CreditsMetadata {
        composer: opt_s(tag, &ItemKey::Composer),
        conductor: opt_s(tag, &ItemKey::Conductor),
        lyricist: opt_s(tag, &ItemKey::Lyricist),
        arranger: opt_s(tag, &ItemKey::Arranger),
        writer: opt_s(tag, &ItemKey::Writer),
        performer: performers
            .as_ref()
            .and_then(|p| p.first().cloned())
            .or_else(|| opt_s(tag, &ItemKey::Performer)),
        performers,
        producer: opt_s(tag, &ItemKey::Producer),
        mix_engineer: opt_s(tag, &ItemKey::MixEngineer),
        engineer: opt_s(tag, &ItemKey::Engineer),
        label: opt_s(tag, &ItemKey::Label),
        publisher: opt_s(tag, &ItemKey::Publisher),
        remixer: opt_s(tag, &ItemKey::Remixer),
        director: opt_s(tag, &ItemKey::Director),
    };
    if c.composer.is_none()
        && c.conductor.is_none()
        && c.lyricist.is_none()
        && c.arranger.is_none()
        && c.writer.is_none()
        && c.performer.is_none()
        && c.performers.is_none()
        && c.producer.is_none()
        && c.mix_engineer.is_none()
        && c.engineer.is_none()
        && c.label.is_none()
        && c.publisher.is_none()
        && c.remixer.is_none()
        && c.director.is_none()
    {
        return None;
    }
    Some(c)
}

fn classical_block(tag: &Tag) -> Option<ClassicalMetadata> {
    let c = ClassicalMetadata {
        work: opt_s(tag, &ItemKey::Work),
        movement: opt_s(tag, &ItemKey::Movement),
        movement_number: parse_u32_tag(tag, &ItemKey::MovementNumber),
        movement_total: parse_u32_tag(tag, &ItemKey::MovementTotal),
        show_name: opt_s(tag, &ItemKey::ShowName),
        content_group: opt_s(tag, &ItemKey::ContentGroup),
    };
    if c.work.is_none()
        && c.movement.is_none()
        && c.movement_number.is_none()
        && c.movement_total.is_none()
        && c.show_name.is_none()
        && c.content_group.is_none()
    {
        return None;
    }
    Some(c)
}

fn original_block(tag: &Tag) -> Option<OriginalMetadata> {
    let c = OriginalMetadata {
        album: opt_s(tag, &ItemKey::OriginalAlbumTitle),
        artist: opt_s(tag, &ItemKey::OriginalArtist),
        lyricist: opt_s(tag, &ItemKey::OriginalLyricist),
        release_date: opt_s(tag, &ItemKey::OriginalReleaseDate),
    };
    if c.album.is_none()
        && c.artist.is_none()
        && c.lyricist.is_none()
        && c.release_date.is_none()
    {
        return None;
    }
    Some(c)
}

/// Recording, release, and original-release date strings (Picard-style).
fn dates_block(tag: &Tag) -> Option<DatesMetadata> {
    let c = DatesMetadata {
        recording: opt_s(tag, &ItemKey::RecordingDate),
        release: opt_s(tag, &ItemKey::ReleaseDate),
        original_release: opt_s(tag, &ItemKey::OriginalReleaseDate),
    };
    if c.recording.is_none()
        && c.release.is_none()
        && c.original_release.is_none()
    {
        return None;
    }
    Some(c)
}

fn identifiers_block(tag: &Tag) -> Option<IdentifiersMetadata> {
    let c = IdentifiersMetadata {
        isrc: opt_s(tag, &ItemKey::Isrc),
        catalog_number: opt_s(tag, &ItemKey::CatalogNumber),
        barcode: opt_s(tag, &ItemKey::Barcode),
        musicbrainz_recording_id: opt_s(tag, &ItemKey::MusicBrainzRecordingId),
        musicbrainz_track_id: opt_s(tag, &ItemKey::MusicBrainzTrackId),
        musicbrainz_release_id: opt_s(tag, &ItemKey::MusicBrainzReleaseId),
        musicbrainz_release_group_id: opt_s(
            tag,
            &ItemKey::MusicBrainzReleaseGroupId,
        ),
        musicbrainz_artist_id: opt_s(tag, &ItemKey::MusicBrainzArtistId),
        musicbrainz_release_artist_id: opt_s(
            tag,
            &ItemKey::MusicBrainzReleaseArtistId,
        ),
        musicbrainz_work_id: opt_s(tag, &ItemKey::MusicBrainzWorkId),
    };
    if c.isrc.is_none()
        && c.catalog_number.is_none()
        && c.barcode.is_none()
        && c.musicbrainz_recording_id.is_none()
        && c.musicbrainz_track_id.is_none()
        && c.musicbrainz_release_id.is_none()
        && c.musicbrainz_release_group_id.is_none()
        && c.musicbrainz_artist_id.is_none()
        && c.musicbrainz_release_artist_id.is_none()
        && c.musicbrainz_work_id.is_none()
    {
        return None;
    }
    Some(c)
}

fn replay_gain_block(tag: &Tag) -> Option<ReplayGainMetadata> {
    let c = ReplayGainMetadata {
        track_gain: opt_s(tag, &ItemKey::ReplayGainTrackGain),
        track_peak: opt_s(tag, &ItemKey::ReplayGainTrackPeak),
        album_gain: opt_s(tag, &ItemKey::ReplayGainAlbumGain),
        album_peak: opt_s(tag, &ItemKey::ReplayGainAlbumPeak),
    };
    if c.track_gain.is_none()
        && c.track_peak.is_none()
        && c.album_gain.is_none()
        && c.album_peak.is_none()
    {
        return None;
    }
    Some(c)
}

fn file_block_from_props(
    p: &FileProperties,
    duration: Duration,
) -> FileMetadata {
    let dms = duration.as_millis() as u64;
    FileMetadata {
        duration_ms: (dms > 0).then_some(dms),
        sample_rate_hz: p.sample_rate(),
        bit_depth: p.bit_depth(),
        channels: p.channels(),
        channel_mask_bits: p.channel_mask().map(|m| m.bits()),
        overall_bitrate_kbps: p.overall_bitrate().filter(|b| *b > 0),
        audio_bitrate_kbps: p.audio_bitrate().filter(|b| *b > 0),
    }
}

/// Build a full OK response from tag + `FileProperties`.
/// Unmapped / vendor tag items ([`ItemKey::Unknown`]) for a complete lossless view of custom frames.
fn collect_extras(tag: &Tag) -> Option<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    let mut per_name: HashMap<String, u32> = HashMap::new();

    for item in tag.items() {
        let ItemKey::Unknown(name) = item.key() else {
            continue;
        };
        if name.is_empty() {
            continue;
        }

        let val = match item.value() {
            ItemValue::Text(s) | ItemValue::Locator(s) => {
                let t = s.trim();
                if t.is_empty() {
                    continue;
                }
                let t = t.to_string();
                if t.len() <= MAX_TEXT_FIELD_BYTES {
                    t
                } else {
                    t.char_indices()
                        .take_while(|(i, _)| *i < MAX_TEXT_FIELD_BYTES)
                        .map(|(_, c)| c)
                        .collect()
                }
            }
            ItemValue::Binary(b) => {
                if b.is_empty() {
                    continue;
                }
                format!("<binary: {} bytes>", b.len())
            }
        };

        let c = per_name.entry(name.clone()).or_default();
        let idx = *c;
        *c += 1;
        let key = if idx == 0 {
            name.clone()
        } else {
            format!("{name}@{idx}")
        };
        out.insert(key, val);
    }

    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn ok_response(
    tag: &Tag,
    file_props: &FileProperties,
    extra_detail: Option<String>,
) -> MetadataQueryResponse {
    let duration = file_props.duration();
    let duration_ms = duration.as_millis() as u64;
    let artists: Option<Vec<String>> =
        tag_string_list(tag, &ItemKey::TrackArtists);

    MetadataQueryResponse {
        v: 1,
        status: ResponseStatus::Ok,
        detail: extra_detail,
        active_profile: None,
        title: opt_cow(tag.title()),
        artist: opt_cow(tag.artist()),
        album: opt_cow(tag.album()),
        album_artist: opt_s(tag, &ItemKey::AlbumArtist),
        artists,
        genre: opt_cow(tag.genre()),
        track: tag.track(),
        track_total: tag.track_total(),
        disc: tag.disk(),
        disc_total: tag.disk_total(),
        year: tag.year(),
        duration_ms: (duration_ms > 0).then_some(duration_ms),
        subtitle: opt_s(tag, &ItemKey::TrackSubtitle),
        language: opt_s(tag, &ItemKey::Language),
        script: opt_s(tag, &ItemKey::Script),
        comment: first_comment_line(tag),
        mood: opt_s(tag, &ItemKey::Mood),
        initial_key: opt_s(tag, &ItemKey::InitialKey),
        bpm: bpm_string(tag),
        lyrics: tag_s(tag, &ItemKey::Lyrics, MAX_TEXT_FIELD_BYTES),
        sort: sort_block(tag),
        credits: credits_block(tag),
        classical: classical_block(tag),
        original: original_block(tag),
        dates: dates_block(tag),
        identifiers: identifiers_block(tag),
        replay_gain: replay_gain_block(tag),
        file: Some(file_block_from_props(file_props, duration)),
        compilation: parse_compilation_bool(tag),
        podcast: parse_podcast_bool(tag),
        extras: collect_extras(tag),
    }
}

fn opt_cow(c: Option<std::borrow::Cow<'_, str>>) -> Option<String> {
    c.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// No tag, only container metadata.
fn ok_from_properties_only(
    file_props: &FileProperties,
) -> MetadataQueryResponse {
    let duration = file_props.duration();
    let duration_ms = duration.as_millis() as u64;
    let mut m = MetadataQueryResponse::v1_error(ResponseStatus::Ok, None);
    m.status = ResponseStatus::Ok;
    m.duration_ms = (duration_ms > 0).then_some(duration_ms);
    m.file = Some(file_block_from_props(file_props, duration));
    m
}

// ---- public/test API ------------------------------------------------------

/// Test / harness helper: build an OK response with explicit [`FileProperties`].
#[allow(dead_code)]
pub(crate) fn build_ok_response(
    tag: &Tag,
    file_props: &FileProperties,
    extra_detail: Option<String>,
) -> MetadataQueryResponse {
    let mut r = ok_response(tag, file_props, extra_detail);
    apply_metadata_profile(&mut r, MetadataProfile::Extended);
    r
}

/// Test / harness helper: synthetic [`FileProperties`] with `duration_ms` only.
#[allow(dead_code)]
pub(crate) fn response_from_tag(
    tag: &Tag,
    duration_ms: u64,
    detail: Option<String>,
) -> MetadataQueryResponse {
    let p = FileProperties::new(
        Duration::from_millis(duration_ms),
        None,
        None,
        None,
        None,
        None,
        None,
    );
    let mut r = ok_response(tag, &p, detail);
    apply_metadata_profile(&mut r, MetadataProfile::Extended);
    r
}

/// Read tags and duration with lofty, then apply [`MetadataProfile`].
fn read_file_metadata(
    path: &Path,
    profile: MetadataProfile,
) -> Result<MetadataQueryResponse, String> {
    let tagged =
        read_from_path(path).map_err(|e| format!("read audio file: {e}"))?;
    let props: &FileProperties = tagged.properties();
    if let Some(t) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        let mut r = ok_response(t, props, None);
        apply_metadata_profile(&mut r, profile);
        return Ok(r);
    }
    let mut m = ok_from_properties_only(props);
    apply_metadata_profile(&mut m, profile);
    Ok(m)
}

/// Strips fields not present in the operator’s [`MetadataProfile`]. For `ok` responses, sets
/// `active_profile` to the effective profile wire string.
pub(crate) fn apply_metadata_profile(
    r: &mut MetadataQueryResponse,
    profile: MetadataProfile,
) {
    if r.status != ResponseStatus::Ok {
        return;
    }
    r.active_profile = Some(profile.as_wire().to_string());
    if profile == MetadataProfile::Extended {
        return;
    }
    r.sort = None;
    r.credits = None;
    r.classical = None;
    r.original = None;
    r.dates = None;
    r.identifiers = None;
    r.replay_gain = None;
    r.file = None;
    r.extras = None;
    r.lyrics = None;
}

// ---- request handling ------------------------------------------------------

/// Handle a `metadata.query` payload: parse JSON, resolve `mpd-path` or `mpd-album`, read tags.
pub(crate) fn query_metadata(
    profile: MetadataProfile,
    library_roots: &[PathBuf],
    payload: &[u8],
) -> Result<MetadataQueryResponse, String> {
    if payload.is_empty() {
        return Ok(MetadataQueryResponse::v1_error(
            ResponseStatus::BadRequest,
            Some("empty payload".to_string()),
        ));
    }

    let text = match std::str::from_utf8(payload) {
        Ok(t) => t,
        Err(e) => {
            return Ok(MetadataQueryResponse::v1_error(
                ResponseStatus::BadRequest,
                Some(format!("payload is not UTF-8: {e}")),
            ));
        }
    };

    let req: MetadataQueryRequest = match serde_json::from_str(text) {
        Ok(r) => r,
        Err(e) => {
            return Ok(MetadataQueryResponse::v1_error(
                ResponseStatus::BadRequest,
                Some(format!("invalid JSON: {e}")),
            ));
        }
    };

    if req.v != 1 {
        return Ok(MetadataQueryResponse::v1_error(
            ResponseStatus::BadRequest,
            Some(format!("unsupported request v: {}", req.v)),
        ));
    }

    match req.target.scheme.as_str() {
        SCHEME_MPD_ALBUM => {
            let (artist, album) =
                match evo_plugins_audio_shared::parse_mpd_album_value(
                    &req.target.value,
                ) {
                    Ok(p) => p,
                    Err(_) => {
                        return Ok(MetadataQueryResponse::v1_error(
                            ResponseStatus::BadRequest,
                            Some(
                                "invalid mpd-album value: expected \"artist|album\" (see \
                             org.evoframework.playback.mpd subject emission)"
                                    .to_string(),
                            ),
                        ));
                    }
                };
            let found =
                match evo_plugins_audio_shared::first_matching_audio_path(
                    library_roots,
                    &artist,
                    &album,
                ) {
                    Ok(p) => p,
                    Err(
                        evo_plugins_audio_shared::MatchError::LimitExceeded,
                    ) => {
                        return Ok(MetadataQueryResponse::v1_error(
                        ResponseStatus::NotFound,
                        Some(format!(
                            "mpd_album: scan limit ({} files) reached under [library] roots",
                            evo_plugins_audio_shared::MAX_MPD_ALBUM_SCAN_CANDIDATES
                        )),
                    ));
                    }
                    Err(evo_plugins_audio_shared::MatchError::Io(m)) => {
                        return Ok(MetadataQueryResponse::v1_error(
                            ResponseStatus::NotFound,
                            Some(m),
                        ));
                    }
                };
            let Some(path) = found else {
                return Ok(MetadataQueryResponse::v1_error(
                    ResponseStatus::NotFound,
                    Some(
                        "mpd_album: no file under [library] roots with matching track artist and \
                         album tags"
                            .to_string(),
                    ),
                ));
            };
            let mut r =
                read_file_metadata(&path, profile).unwrap_or_else(|e| {
                    MetadataQueryResponse::v1_error(
                        ResponseStatus::NotFound,
                        Some(e),
                    )
                });
            if r.status == ResponseStatus::Ok {
                r.detail = Some(
                    "target mpd-album: first matching file under [library] roots (local tag scan)"
                        .to_string(),
                );
            }
            Ok(r)
        }
        SCHEME_MPD_PATH => {
            if req.target.value.is_empty() {
                return Ok(MetadataQueryResponse::v1_error(
                    ResponseStatus::BadRequest,
                    Some("empty mpd-path value".to_string()),
                ));
            }
            let Some(path) =
                resolve_audio_path(library_roots, &req.target.value)
            else {
                return Ok(MetadataQueryResponse::v1_error(
                    ResponseStatus::NotFound,
                    Some("audio file not found for mpd_path".to_string()),
                ));
            };
            let r = read_file_metadata(&path, profile).unwrap_or_else(|e| {
                MetadataQueryResponse::v1_error(
                    ResponseStatus::NotFound,
                    Some(e),
                )
            });
            Ok(r)
        }
        other => Ok(MetadataQueryResponse::v1_error(
            ResponseStatus::BadRequest,
            Some(format!("unknown target.scheme: {other}")),
        )),
    }
}

fn resolve_audio_path(
    library_roots: &[PathBuf],
    value: &str,
) -> Option<PathBuf> {
    if value
        .get(..7)
        .map(|p| p.eq_ignore_ascii_case("http://"))
        .unwrap_or(false)
        || value
            .get(..8)
            .map(|p| p.eq_ignore_ascii_case("https://"))
            .unwrap_or(false)
    {
        return None;
    }

    let p = Path::new(value);
    if p.is_absolute() {
        return p.is_file().then(|| p.to_path_buf());
    }
    for root in library_roots {
        let joined = root.join(value);
        if joined.is_file() {
            return Some(joined);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MetadataProfile;
    use lofty::tag::Tag;
    use lofty::tag::TagType;

    #[test]
    fn response_from_tag_maps_core_fields() {
        let mut tag = Tag::new(TagType::Id3v2);
        tag.set_title("Song".to_string());
        tag.set_artist("Band".to_string());
        tag.set_album("LP".to_string());
        tag.set_track(3);
        tag.set_genre("Rock".to_string());
        let r = response_from_tag(&tag, 120_000, None);
        assert_eq!(r.status, ResponseStatus::Ok);
        assert_eq!(r.title.as_deref(), Some("Song"));
        assert_eq!(r.artist.as_deref(), Some("Band"));
        assert_eq!(r.album.as_deref(), Some("LP"));
        assert_eq!(r.genre.as_deref(), Some("Rock"));
        assert_eq!(r.track, Some(3));
        assert_eq!(r.duration_ms, Some(120_000));
        assert!(r.file.is_some());
        assert_eq!(r.file.as_ref().and_then(|f| f.duration_ms), Some(120_000));
    }

    #[test]
    fn classical_credits_serialize() {
        // Vorbis comment mapping holds PERFORMER + work fields; ID3v2 has no single frame for
        // `ItemKey::Performer` (orchestras are usually Vorbis/FLAC `PERFORMER` lines).
        let mut tag = Tag::new(TagType::VorbisComments);
        tag.insert_text(ItemKey::Composer, "Johann Sebastian Bach".to_string());
        tag.insert_text(ItemKey::Conductor, "J. S. Taktstock".to_string());
        assert!(tag.insert_text(
            ItemKey::Performer,
            "Dresden State Orchestra".to_string(),
        ));
        assert!(tag.insert_text(
            ItemKey::Work,
            "BWV 1001 — Sonatas and Partitas".to_string(),
        ));
        let r = response_from_tag(&tag, 0, None);
        let c = r.credits.as_ref().unwrap();
        assert_eq!(c.composer.as_deref(), Some("Johann Sebastian Bach"));
        assert_eq!(c.conductor.as_deref(), Some("J. S. Taktstock"));
        assert_eq!(c.performer.as_deref(), Some("Dresden State Orchestra"));
        let k = r.classical.as_ref().unwrap();
        assert_eq!(k.work.as_deref(), Some("BWV 1001 — Sonatas and Partitas"));
    }

    #[test]
    fn not_found_for_http_url() {
        let r = query_metadata(
            MetadataProfile::default(),
            &[],
            r#"{"v":1,"target":{"scheme":"mpd-path","value":"http://x/a.flac"}}"#.as_bytes(),
        )
        .unwrap();
        assert_eq!(r.status, ResponseStatus::NotFound);
    }

    #[test]
    fn standard_profile_strips_nested_and_sets_active_profile() {
        let mut tag = Tag::new(TagType::VorbisComments);
        assert!(tag.insert_text(ItemKey::Composer, "C".to_string(),));
        let mut r = response_from_tag(&tag, 0, None);
        assert!(r.credits.is_some());
        apply_metadata_profile(&mut r, MetadataProfile::Standard);
        assert_eq!(r.active_profile.as_deref(), Some("standard"));
        assert!(r.credits.is_none());
        assert!(r.classical.is_none());
        assert!(r.extras.is_none());
    }

    #[test]
    fn extras_surfaces_unknown_frames() {
        use lofty::tag::TagItem;

        let mut tag = Tag::new(TagType::VorbisComments);
        tag.insert_unchecked(TagItem::new(
            ItemKey::Unknown("TXXX:my_vendor".to_string()),
            ItemValue::Text("opaque value".to_string()),
        ));
        let r = response_from_tag(&tag, 0, None);
        let e = r.extras.as_ref().expect("extras");
        assert_eq!(
            e.get("TXXX:my_vendor").map(String::as_str),
            Some("opaque value")
        );
    }

    #[test]
    fn mpd_album_resolves_first_matching_track() {
        use lofty::config::WriteOptions;
        use lofty::tag::Accessor;
        use lofty::tag::TagExt;
        use lofty::tag::TagType;

        const MINI_MP3: &[u8] = include_bytes!(
            "../../../crates/evo-plugins-audio-shared/assets/minimal.mp3"
        );
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("ScanA").join("ScanB");
        std::fs::create_dir_all(&sub).unwrap();
        let mp3 = sub.join("1.mp3");
        std::fs::write(&mp3, MINI_MP3).unwrap();
        let mut tag = Tag::new(TagType::Id3v2);
        tag.set_artist("ScanA".to_string());
        tag.set_album("ScanB".to_string());
        tag.set_title("T1".to_string());
        tag.save_to_path(&mp3, WriteOptions::new().preferred_padding(0))
            .expect("tag save");
        let body = r##"{"v":1,"target":{"scheme":"mpd-album","value":"ScanA|ScanB"}}"##;
        let r = query_metadata(
            MetadataProfile::Extended,
            &[dir.path().to_path_buf()],
            body.as_bytes(),
        )
        .unwrap();
        assert_eq!(r.status, ResponseStatus::Ok);
        assert_eq!(r.title.as_deref(), Some("T1"));
        assert_eq!(r.active_profile.as_deref(), Some("extended"));
        assert!(r.detail.as_deref().unwrap_or("").contains("mpd-album"));
    }
}
