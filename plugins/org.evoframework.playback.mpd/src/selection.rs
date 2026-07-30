// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Multi-dimensional selection resolver — the source-agnostic
//! seam behind `queue.enqueue_selection` and
//! `playlist.save_selection`.
//!
//! # Contract
//!
//! A [`SelectionCriteria`] carries a dimension + value +
//! optional parent context — the same shape the browse drill
//! already consumes. The [`SelectionResolver`] trait is one
//! method: `resolve(criteria)` → [`ResolvedSelection`]. The
//! two verbs apply the resolved selection uniformly (with
//! mode-specific `replace` / `next` / `append` semantics) via
//! MPD's `command_list` batching — one atomic roundtrip per
//! verb call regardless of selection size.
//!
//! # Source-agnostic seam
//!
//! - **MPD-native** dimensions (`artist`, `genre`, exact
//!   `date`) resolve to [`ResolvedSelection::Filter`] so
//!   MPD's `findadd` / `searchadd` primitives handle the
//!   enqueue in one op — no per-track dispatch, no O(library)
//!   materialisation over the wire.
//! - **Substring** dimensions (`year` against `date`) resolve
//!   to [`ResolvedSelection::Filter`] with `substring = true`
//!   so `searchadd` runs.
//! - **Folder-anchored album** dimension resolves to
//!   [`ResolvedSelection::UriList`] via
//!   [`library::resolve_album_tiles`] so multi-disc rollup
//!   is honoured (canonical folder key merges sibling
//!   `(Disc 1)` / `(Disc 2)` folders into one selection).
//! - **Folder** dimension resolves to
//!   [`ResolvedSelection::UriList`] containing the folder
//!   path; MPD's `add DIR` (issued by the queue verb)
//!   recursively adds every file in the directory in one
//!   command.
//! - **Playlist** dimension resolves to
//!   [`ResolvedSelection::UriList`] by reading the stored
//!   playlist's file lines via MPD `listplaylistinfo`.
//!
//! Future sources (UPnP/DLNA ContentDirectory, non-MPD NAS
//! browsers) implement [`SelectionResolver`] and return
//! [`ResolvedSelection::UriList`] containing their own stream
//! URIs — the queue is source-agnostic and holds URIs from
//! any source. No MPD assumption leaks into the verb
//! contract.

use serde::Deserialize;

use crate::library;
use crate::mpd::{MpdConnection, MpdLibraryEntry};

/// Selection criteria — mirrors the shape the browse drill's
/// `BrowseSelector` already carries.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SelectionCriteria {
    /// Which dimension the value indexes.
    pub(crate) dimension: SelectionDimension,
    /// The dimension's value. For `album`, this is the tile's
    /// `album_id` (canonical folder path) when the UI has it,
    /// or the display title for backward-compat. For
    /// `playlist`, the stored playlist name.
    pub(crate) value: String,
    /// Optional parent context (e.g. album drilled under a
    /// specific `albumartist`). Same shape as
    /// `BrowseSelector::parent`.
    #[serde(default)]
    pub(crate) parent: Option<SelectionParent>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SelectionParent {
    pub(crate) tag: String,
    pub(crate) value: String,
}

/// The seven dimensions the browse facet + drill already
/// speak. `Folder` and `Playlist` are added for the
/// enqueue-selection contract; the browse-by-* facets cover
/// the other five.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SelectionDimension {
    Artist,
    Album,
    Genre,
    Year,
    Folder,
    Playlist,
}

impl SelectionDimension {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SelectionDimension::Artist => "artist",
            SelectionDimension::Album => "album",
            SelectionDimension::Genre => "genre",
            SelectionDimension::Year => "year",
            SelectionDimension::Folder => "folder",
            SelectionDimension::Playlist => "playlist",
        }
    }
}

/// The output of a [`SelectionResolver::resolve`] call.
///
/// The two verbs (`queue.enqueue_selection`,
/// `playlist.save_selection`) apply this uniformly.
#[derive(Debug, Clone)]
pub(crate) enum ResolvedSelection {
    /// MPD-native tag filter — the enqueue verb applies via
    /// `findadd` (exact match) or `searchadd` (substring).
    /// One MPD command adds every match to the queue in
    /// MPD's canonical order.
    ///
    /// `pairs` is the list of `(TAG, VALUE)` pairs joined by
    /// AND at the MPD wire (`findadd artist "X" album "Y"`).
    /// Empty pairs means the resolver decided the selection
    /// has no MPD-side representation.
    Filter {
        pairs: Vec<(String, String)>,
        substring: bool,
    },
    /// Explicit URI list to enqueue in order — used for
    /// folder-anchored album (multi-disc rollup),
    /// `Folder` dimension (a single directory path MPD's
    /// `add` recursively expands), `Playlist` dimension
    /// (tracks read from the stored playlist), and any
    /// future non-MPD source (UPnP/DLNA stream URIs).
    UriList(Vec<String>),
}

impl ResolvedSelection {
    /// True when the resolved selection would match no
    /// tracks. The verb caller surfaces this as an explicit
    /// empty response — not a silent no-op, not a cleared
    /// queue.
    ///
    /// NOTE: for `Filter` selections this only detects the
    /// no-pairs shape, NOT the "pairs present but zero
    /// matches" case. The verb callers additionally run MPD
    /// `count` on a Filter to confirm zero-match; the retained
    /// helper answers the trivially-empty case (empty URI
    /// list, no-pairs Filter) without a wire roundtrip.
    #[allow(dead_code)]
    pub(crate) fn is_empty(&self) -> bool {
        match self {
            ResolvedSelection::Filter { pairs, .. } => pairs.is_empty(),
            ResolvedSelection::UriList(list) => list.is_empty(),
        }
    }
}

/// Source-resolver seam. Each source (MPD, future UPnP/DLNA,
/// future non-MPD NAS browsers) implements this. The queue
/// and playlist verbs pick the resolver by `source_id` from
/// the plugin's source registry.
#[async_trait::async_trait]
pub(crate) trait SelectionResolver: Send + Sync {
    /// Resolve a selection to a source-native track set.
    ///
    /// Returns `Err(SelectionError::SourceUnreachable)` when
    /// the source cannot be contacted (offline NAS, UPnP
    /// timeout, MPD ack). NEVER swallow a transient failure
    /// as an empty result — the verb caller surfaces
    /// `Unavailable`, distinct from a genuine zero-match.
    async fn resolve(
        &self,
        conn: &mut MpdConnection,
        criteria: &SelectionCriteria,
    ) -> Result<ResolvedSelection, SelectionError>;
}

/// Error class for [`SelectionResolver::resolve`].
#[derive(Debug, thiserror::Error)]
pub(crate) enum SelectionError {
    /// Dimension is not supported by this resolver (e.g. a
    /// future UPnP resolver refusing a `Playlist` selection
    /// whose value names an MPD stored playlist). Reserved
    /// for cross-source calls when the resolver dispatch
    /// lands.
    #[error("selection dimension {dimension} not supported: {reason}")]
    #[allow(dead_code)]
    UnsupportedDimension {
        dimension: &'static str,
        reason: String,
    },
    /// Source could not be reached — MPD ack, network error,
    /// UPnP timeout. Never negatively cached; the verb
    /// caller returns `Unavailable`.
    #[error("source unreachable: {0}")]
    SourceUnreachable(String),
}

/// MPD-backed selection resolver.
///
/// Handles every dimension the audio distribution's local MPD
/// index knows about. Folder-anchored album identity honoured
/// via [`library::resolve_album_tiles`] so multi-disc rollup
/// produces the correct URI set on both `enqueue_selection`
/// and `save_selection`.
pub(crate) struct MpdSelectionResolver;

#[async_trait::async_trait]
impl SelectionResolver for MpdSelectionResolver {
    async fn resolve(
        &self,
        conn: &mut MpdConnection,
        criteria: &SelectionCriteria,
    ) -> Result<ResolvedSelection, SelectionError> {
        let value = criteria.value.trim();
        if value.is_empty() {
            return Ok(ResolvedSelection::UriList(Vec::new()));
        }
        match criteria.dimension {
            SelectionDimension::Artist => Ok(build_filter(
                mpd_tag_for_artist_dimension(criteria),
                value,
                criteria.parent.as_ref(),
                false,
            )),
            SelectionDimension::Genre => Ok(build_filter(
                "genre",
                value,
                criteria.parent.as_ref(),
                false,
            )),
            SelectionDimension::Year => {
                Ok(build_filter("date", value, criteria.parent.as_ref(), true))
            }
            SelectionDimension::Album => {
                let tiles = library::resolve_album_tiles(
                    conn,
                    "queue.enqueue_selection",
                )
                .await
                .map_err(|e| {
                    SelectionError::SourceUnreachable(format!(
                        "MPD listallinfo failed while resolving album selection: {e:?}"
                    ))
                })?;
                let parent_artist_fold =
                    criteria.parent.as_ref().and_then(|p| {
                        if matches!(
                            p.tag.trim().to_ascii_lowercase().as_str(),
                            "albumartist" | "artist"
                        ) {
                            Some(library::artist_fold_key(&p.value))
                        } else {
                            None
                        }
                    });
                let uris: Vec<String> = tiles
                    .iter()
                    .filter(|t| {
                        let by_value = t.canonical_folder == value
                            || t.display_title.eq_ignore_ascii_case(value);
                        let by_parent = match &parent_artist_fold {
                            Some(k) if !k.is_empty() => {
                                library::artist_fold_key(&t.display_artist)
                                    == *k
                            }
                            _ => true,
                        };
                        by_value && by_parent
                    })
                    .flat_map(|t| {
                        t.tracks.iter().filter_map(|e| match e {
                            MpdLibraryEntry::File { path, .. } => {
                                Some(path.clone())
                            }
                            _ => None,
                        })
                    })
                    .collect();
                Ok(ResolvedSelection::UriList(uris))
            }
            SelectionDimension::Folder => {
                // MPD's `add DIR` recursively adds every file
                // under the directory in one command. The verb
                // path passes this URI list as a single-element
                // vec so the queue applies exactly one `add`.
                Ok(ResolvedSelection::UriList(vec![value.to_string()]))
            }
            SelectionDimension::Playlist => {
                let entries =
                    conn.listplaylistinfo(value).await.map_err(|e| {
                        SelectionError::SourceUnreachable(format!(
                            "MPD listplaylistinfo {value:?}: {e}"
                        ))
                    })?;
                let uris: Vec<String> =
                    entries.into_iter().map(|e| e.file_path).collect();
                Ok(ResolvedSelection::UriList(uris))
            }
        }
    }
}

/// Pick the MPD tag to filter for an `artist` dimension. The
/// browse-by-artist facet dispatches with tag `albumartist`
/// so the default lines up with what the UI clicked; a
/// per-track `artist` filter (for feature-guest matches)
/// requires an explicit `parent.tag = "artist"` context.
fn mpd_tag_for_artist_dimension(criteria: &SelectionCriteria) -> &'static str {
    if let Some(parent) = criteria.parent.as_ref() {
        if parent.tag.trim().eq_ignore_ascii_case("artist") {
            return "artist";
        }
    }
    "albumartist"
}

/// Build a [`ResolvedSelection::Filter`] from a primary
/// dimension tag + value plus an optional parent context that
/// narrows the filter via AND.
fn build_filter(
    tag: &str,
    value: &str,
    parent: Option<&SelectionParent>,
    substring: bool,
) -> ResolvedSelection {
    let mut pairs: Vec<(String, String)> =
        vec![(tag.to_string(), value.to_string())];
    if let Some(p) = parent {
        let parent_tag = p.tag.trim().to_ascii_lowercase();
        let parent_value = p.value.trim();
        let mapped = match parent_tag.as_str() {
            "album" => Some("album"),
            "albumartist" => Some("albumartist"),
            "artist" => Some("artist"),
            "genre" => Some("genre"),
            _ => None,
        };
        if let (Some(t), false) = (mapped, parent_value.is_empty()) {
            pairs.push((t.to_string(), parent_value.to_string()));
        }
    }
    ResolvedSelection::Filter { pairs, substring }
}
