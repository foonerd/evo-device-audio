// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! `artwork.resolve` JSON payload, sidecar file discovery, and embedded tags.
//!
//! Request `target` uses the same `scheme` / `value` shape as
//! [`evo_plugin_sdk::contract::ExternalAddressing`], with schemes aligned to
//! `org.evoframework.playback.mpd` (`mpd-path`, `mpd-album`).

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::Serialize;

use crate::embedded;

/// `mpd-path` scheme: value is MPD's `file` (library-relative or absolute).
pub(crate) const SCHEME_MPD_PATH: &str = "mpd-path";
/// `mpd-album` scheme: value is `Artist|Album` (see MPD warden).
pub(crate) const SCHEME_MPD_ALBUM: &str = "mpd-album";
/// `mpd-directory` scheme: value is an MPD-visible directory path
/// (relative to a library root, or absolute on disk). The
/// plugin scans the directory itself — not the parent — for a
/// cover file matching the priority list, then falls back to
/// any image extension. Used by the browse-library folder
/// surface so operator-tagged folders render their sidecar
/// cover in the folder view, not a generic folder icon. Does
/// NOT walk subdirectories: the operator's per-folder cover is
/// always at the same level as the folder they clicked.
pub(crate) const SCHEME_MPD_DIRECTORY: &str = "mpd-directory";

/// Priority-ordered cover-art filenames in **lowercase**. The
/// directory walk lowercases each entry against this list, so
/// `Cover.JPG`, `FOLDER.jpg`, `CoVeR.JpG`, and Unicode-cased
/// variants all resolve without listing every permutation.
///
/// Ordering follows the established convention (cover > folder >
/// front > coverart > albumart > artist variants > scan > album)
/// and matches volumio-evo's reference list. `.webp` entries are
/// included so libraries already adopting modern formats are
/// served without operator action.
///
/// Operator-curated sidecars in the music tree work identically
/// whether the audio file resolves under `LocalInternal`,
/// `LocalUsb`, `NetworkNasSmb`, `NetworkNasNfs`, or any other
/// mount the framework's source registry exposes: the cascade
/// walks the audio file's parent directory regardless of which
/// source ID resolved the file. Network-bound sources are
/// preflight-gated by source state via the registry; the
/// walk never blocks on an Offline mount because the source
/// resolver refuses to materialise the path in the first place.
const COVER_FILE_NAMES: &[&str] = &[
    "cover.jpg",
    "folder.jpg",
    "cover.png",
    "folder.png",
    "coverart.jpg",
    "albumart.jpg",
    "coverart.png",
    "albumart.png",
    "artists.jpg",
    "artist.jpg",
    "artists.png",
    "artist.png",
    "front.jpg",
    "front.png",
    "album.jpg",
    "scan.jpg",
    "cover.webp",
    "folder.webp",
    "front.webp",
    "artists.webp",
];

/// Image file extensions accepted as last-resort cover candidates
/// when the priority list above misses. Matches volumio-evo's
/// fallback: any image in the audio file's parent directory is
/// treated as cover art unless it exceeds [`MAX_COVER_BYTES`].
const FALLBACK_IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp"];

/// Maximum sidecar file size accepted as cover art. Above this
/// the file is rejected as cover-art-too-large — embedded
/// extraction or online providers take over downstream.
/// Matches volumio-evo's 5 MB ceiling.
const MAX_COVER_BYTES: u64 = 5_000_000;

/// Request body for `artwork.resolve` (JSON, UTF-8).
#[derive(Debug, Deserialize)]
pub(crate) struct ArtworkResolveRequest {
    /// Schema version; `1` is the only value accepted.
    pub(crate) v: u8,
    /// Which subject to resolve art for; mirrors external addressing.
    pub(crate) target: ResolveTarget,
    /// Size variant requested. One of
    /// `tiny | medium | large | original`. Default `original`
    /// when absent (legacy callers receive the master bytes).
    /// Resized variants are encoded as WebP per the plugin's
    /// transcoding posture; original-size carries the source
    /// bytes verbatim.
    #[serde(default)]
    pub(crate) size: Option<String>,
}

/// Subject selector: must match a registered scheme from the playback warden.
#[derive(Debug, Deserialize)]
pub(crate) struct ResolveTarget {
    pub(crate) scheme: String,
    pub(crate) value: String,
}

/// JSON response (always serialised; business outcomes use `status`, not HTTP).
#[derive(Debug, Serialize)]
pub(crate) struct ArtworkResolveResponse {
    v: u8,
    status: ResponseStatus,
    /// Absolute path to an image file on this device, when `status` is
    /// [`ResponseStatus::Ok`]. Retained on the resolve response so
    /// in-process callers that prefer file paths to bytes keep
    /// working; HTTPS callers consume `content_hash` instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    /// SHA-256 (lowercase hex, 64 chars) of the resolved variant's
    /// bytes. Set when `status` is [`ResponseStatus::Ok`]. The
    /// framework's `/api/v1/audio/artwork/:content_hash` endpoint
    /// serves these bytes from the asset cache the plugin
    /// populated during resolve. UI consumers construct
    /// `<img src="/api/v1/audio/artwork/{content_hash}">` and
    /// browsers/CDNs treat the URL as forever-cacheable per the
    /// hash endpoint's immutable Cache-Control.
    #[serde(skip_serializing_if = "Option::is_none")]
    content_hash: Option<String>,
    /// MIME type for the resolved variant. `image/jpeg` /
    /// `image/png` / `image/webp` for `Original` size (matches
    /// the source); `image/webp` for resized variants per the
    /// plugin's transcoding posture.
    #[serde(skip_serializing_if = "Option::is_none")]
    mime: Option<String>,
    /// Size variant the response carries — round-trips with the
    /// request's `size` parameter (`tiny | medium | large |
    /// original`). Defaults to `original` on unset requests; set
    /// here so consumers correlate the response to the variant
    /// they asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<String>,
    /// Extra context for operators and UIs.
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    /// Which leaf source in the local cascade produced the
    /// resolved bytes. Stable identifiers:
    ///
    /// - `local_sidecar` — cover / folder / front / album image
    ///   file living beside the audio file (or, via the
    ///   folder-name fallback, in the album-named folder).
    /// - `local_embedded` — cover extracted from the audio
    ///   file's own tags (lofty-readable formats only).
    ///
    /// Absent on structured refusals (`NotFound`, `BadRequest`)
    /// and on the pre-2026-07-22 build (older clients tolerate
    /// the missing field via `serde(skip_serializing_if)`).
    ///
    /// The endpoint surfaces this via `X-Artwork-Provider` on
    /// the resolve response so operators (and the UI probe) see
    /// which cascade branch delivered each cover.
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_id: Option<String>,
    /// Identity — the `(artist, album)` pair the online cascade
    /// needs to key its lookup. Populated on:
    ///
    /// - `mpd-album` responses (Ok and NotFound): identity is the
    ///   parsed value.
    /// - `mpd-path` NotFound responses: identity is read from the
    ///   file's tags via `embedded::read_identity`.
    ///
    /// Absent when the file has no `(artist, album)` tags (or lofty
    /// cannot parse the format), and on `BadRequest` responses.
    ///
    /// The framework endpoint uses this to synthesise an
    /// mpd-album target for the online tier when the local cascade
    /// misses — so a now-playing surface with no embedded / sidecar
    /// art still falls through to the online provider chain.
    /// Without this field, the endpoint would forward the
    /// mpd-path target verbatim and artwork.online would refuse
    /// (its cascade is keyed on (artist, album), not on file
    /// paths); the operator memo of 2026-07-22 pinned this as the
    /// defect.
    #[serde(skip_serializing_if = "Option::is_none")]
    identity: Option<Identity>,
}

/// `(artist, album)` pair carried on the resolve response so the
/// endpoint can synthesise an mpd-album target for the online
/// cascade even when the original request came in as mpd-path.
///
/// Symmetric with the `artist|album` compound the MPD warden
/// emits — the endpoint reconstructs the pipe-joined value from
/// this struct's fields.
#[derive(Debug, Serialize)]
pub(crate) struct Identity {
    pub(crate) artist: String,
    pub(crate) album: String,
}

/// Outcome of a resolve attempt.
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResponseStatus {
    Ok,
    NotFound,
    /// Retained for forward-compatible JSON; not emitted by this version.
    #[allow(dead_code)]
    Unsupported,
    BadRequest,
}

impl ArtworkResolveResponse {
    pub(crate) fn json_bytes(self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(&self)
    }
}

/// Map a file extension to a MIME type for common cover art files.
fn mime_for_path(p: &Path) -> Option<&'static str> {
    p.extension().and_then(|e| e.to_str()).and_then(|e| {
        match e.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => Some("image/jpeg"),
            "png" => Some("image/png"),
            "webp" => Some("image/webp"),
            _ => None,
        }
    })
}

/// If `mpd_file` is a local audio file path, look for a cover
/// image in the same directory.
///
/// Resolution proceeds in two passes against the file's parent:
///
/// 1. **Priority pass** — directory entries are read once and
///    lowercased; the first match against [`COVER_FILE_NAMES`]
///    (in priority order) wins. This catches `Cover.JPG`,
///    `FOLDER.png`, mixed-case Unicode filenames, etc., without
///    listing every casing permutation.
/// 2. **Fallback pass** — if no priority name matched, the first
///    file in directory-traversal order with an extension in
///    [`FALLBACK_IMAGE_EXTENSIONS`] is returned, provided its
///    size is under [`MAX_COVER_BYTES`]. This handles libraries
///    where the operator names cover files after the album
///    (e.g. `Symphony No. 5.jpg`) rather than `cover.jpg`.
///
/// File sizes above [`MAX_COVER_BYTES`] are skipped so an
/// accidentally-dropped multi-megabyte PSD or scan PDF doesn't
/// override a smaller cover image elsewhere in the directory.
///
/// Source-kind agnostic: the parent-directory walk applies
/// identically whether `mpd_file` resolves to a local-internal
/// path, a USB mount, or a NAS-mounted share. The framework's
/// source registry preflights mount reachability before this
/// function is invoked.
pub(crate) fn find_cover_beside_audio_file(mpd_file: &Path) -> Option<PathBuf> {
    let dir = mpd_file.parent()?;
    find_cover_in_directory(dir)
}

/// Same priority-and-fallback walk as
/// [`find_cover_beside_audio_file`] but takes the directory
/// path directly rather than a track path. Used by the
/// [`SCHEME_MPD_DIRECTORY`] resolve — the caller identifies a
/// directory subject and the plugin returns the cover file
/// (if any) at that directory's top level.
pub(crate) fn find_cover_in_directory(dir: &Path) -> Option<PathBuf> {
    let entries: Vec<std::fs::DirEntry> = match std::fs::read_dir(dir) {
        Ok(it) => it.filter_map(Result::ok).collect(),
        Err(_) => return None,
    };
    // Priority pass — exact case-insensitive match against the
    // ordered list. Build a lowercase→entry map so each priority
    // lookup is O(1) on the directory size.
    let mut by_lower: std::collections::HashMap<String, &std::fs::DirEntry> =
        std::collections::HashMap::with_capacity(entries.len());
    for e in &entries {
        if let Some(name) = e.file_name().to_str() {
            by_lower.insert(name.to_lowercase(), e);
        }
    }
    for priority_name in COVER_FILE_NAMES {
        if let Some(entry) = by_lower.get(*priority_name) {
            let path = entry.path();
            if cover_size_ok(&path) {
                return Some(path);
            }
        }
    }
    // Fallback pass — any image in the directory, first hit.
    for e in &entries {
        let path = e.path();
        let Some(ext) = path
            .extension()
            .and_then(|s| s.to_str())
            .map(str::to_lowercase)
        else {
            continue;
        };
        if !FALLBACK_IMAGE_EXTENSIONS.contains(&ext.as_str()) {
            continue;
        }
        if cover_size_ok(&path) {
            return Some(path);
        }
    }
    None
}

/// True when the candidate file exists and is below
/// [`MAX_COVER_BYTES`]. A metadata error (transient I/O,
/// permission denied) skips the candidate without aborting
/// the walk — the next priority entry gets a fair attempt.
fn cover_size_ok(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(m) => m.is_file() && m.len() <= MAX_COVER_BYTES,
        Err(_) => false,
    }
}

/// Resolve MPD `file` string to a local [`PathBuf`] if the file exists.
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

/// Build the JSON response body. Returns [`Err`] only for internal failures
/// (non-UTF-8 path, cache I/O) that should map to [`PluginError::Permanent`].
/// Output of [`resolve_artwork`].
///
/// Carries the wire-shape [`ArtworkResolveResponse`] (which goes
/// back to the caller as JSON) plus an optional
/// `(content_hash, bytes)` tuple the caller pushes into the
/// framework's asset cache asynchronously. The split keeps the
/// synchronous path here (decode + resize + WebP encode + hash)
/// inside the [`tokio::task::spawn_blocking`] wrapper the
/// caller already uses, while the cache write — which is async
/// in the SDK trait — runs in the outer async context.
pub(crate) struct ResolveOutput {
    pub(crate) response: ArtworkResolveResponse,
    /// `(content_hash, bytes)` pair to push to the asset cache,
    /// when resolve produced a transcoded variant. `None` when
    /// the resolve was a structured refusal (NotFound,
    /// BadRequest) or when transcode failed (the response
    /// degrades to a path-only payload so legacy in-process
    /// callers keep working).
    pub(crate) cache_payload: Option<(String, Vec<u8>)>,
}

pub(crate) fn resolve_artwork(
    library_roots: &[PathBuf],
    state_dir: Option<&Path>,
    payload: &[u8],
) -> Result<ResolveOutput, String> {
    if payload.is_empty() {
        return Ok(ResolveOutput {
            response: ArtworkResolveResponse {
                v: 1,
                status: ResponseStatus::BadRequest,
                path: None,
                content_hash: None,
                mime: None,
                size: None,
                provider_id: None,
                identity: None,
                detail: Some("empty payload".to_string()),
            },
            cache_payload: None,
        });
    }

    let text = match std::str::from_utf8(payload) {
        Ok(t) => t,
        Err(e) => {
            return Ok(ResolveOutput {
                response: ArtworkResolveResponse {
                    v: 1,
                    status: ResponseStatus::BadRequest,
                    path: None,
                    content_hash: None,
                    size: None,
                    provider_id: None,
                    identity: None,
                    mime: None,
                    detail: Some(format!("payload is not UTF-8: {e}")),
                },
                cache_payload: None,
            });
        }
    };

    let req: ArtworkResolveRequest = match serde_json::from_str(text) {
        Ok(r) => r,
        Err(e) => {
            return Ok(ResolveOutput {
                response: ArtworkResolveResponse {
                    v: 1,
                    status: ResponseStatus::BadRequest,
                    path: None,
                    content_hash: None,
                    size: None,
                    provider_id: None,
                    identity: None,
                    mime: None,
                    detail: Some(format!("invalid JSON: {e}")),
                },
                cache_payload: None,
            });
        }
    };
    if req.v != 1 {
        return Ok(ResolveOutput {
            response: ArtworkResolveResponse {
                v: 1,
                status: ResponseStatus::BadRequest,
                path: None,
                content_hash: None,
                mime: None,
                size: None,
                provider_id: None,
                identity: None,
                detail: Some(format!("unsupported request v: {}", req.v)),
            },
            cache_payload: None,
        });
    }
    // Parse the optional size; default to Original. An unknown
    // value surfaces as a structured BadRequest so the operator
    // UI sees a recognisable refusal rather than a silent
    // coercion to Original.
    let size_str = req.size.as_deref().unwrap_or("original");
    let size = match crate::transcode::ArtworkSize::parse(size_str) {
        Some(s) => s,
        None => {
            return Ok(ResolveOutput {
                response: ArtworkResolveResponse {
                    v: 1,
                    status: ResponseStatus::BadRequest,
                    path: None,
                    content_hash: None,
                    mime: None,
                    size: None,
                    provider_id: None,
                    identity: None,
                    detail: Some(format!(
                        "unknown size: {size_str} (expected small | medium | large | original; `tiny` accepted as alias for `small`)"
                    )),
                },
                cache_payload: None,
            });
        }
    };

    let inner_response = match req.target.scheme.as_str() {
        SCHEME_MPD_ALBUM => {
            resolve_mpd_album(library_roots, state_dir, &req.target.value)?
        }
        SCHEME_MPD_PATH => {
            resolve_mpd_path(library_roots, state_dir, &req.target.value)?
        }
        SCHEME_MPD_DIRECTORY => {
            resolve_mpd_directory(library_roots, &req.target.value)?
        }
        other => ArtworkResolveResponse {
            v: 1,
            status: ResponseStatus::BadRequest,
            path: None,
            content_hash: None,
            mime: None,
            size: None,
            provider_id: None,
            identity: None,
            detail: Some(format!("unknown target.scheme: {other}")),
        },
    };

    // Post-resolve transcode: when the inner resolver landed
    // an Ok response with a path, read the bytes, transcode
    // for the requested size, and produce the cache-ready
    // (content_hash, bytes) payload. On transcode failure the
    // response keeps its path-only payload — operator UI
    // degrades gracefully via the placeholder rule rather
    // than seeing a hard refusal for a structurally-valid
    // cover that just couldn't decode.
    let response_path = inner_response.path.clone();
    if inner_response.status == ResponseStatus::Ok {
        if let Some(path_str) = response_path {
            return finalise_with_transcode(inner_response, &path_str, size);
        }
    }
    Ok(ResolveOutput {
        response: inner_response,
        cache_payload: None,
    })
}

/// Read bytes from the resolved cover path, transcode to the
/// requested size, and stamp `content_hash` + `size` + `mime`
/// onto the response. Returns the `(content_hash, bytes)`
/// pair for the caller to push into the asset cache.
fn finalise_with_transcode(
    mut response: ArtworkResolveResponse,
    cover_path: &str,
    size: crate::transcode::ArtworkSize,
) -> Result<ResolveOutput, String> {
    let bytes = match std::fs::read(cover_path) {
        Ok(b) => b,
        Err(e) => {
            // Read failure degrades the response to path-only
            // (caller's framework hash endpoint will 404 but
            // the operator UI's placeholder rule takes over).
            tracing::warn!(
                cover_path = %cover_path,
                error = %e,
                "artwork.resolve: cover file read failed; degrading to path-only"
            );
            return Ok(ResolveOutput {
                response,
                cache_payload: None,
            });
        }
    };
    let source_mime = response.mime.clone().unwrap_or_else(|| {
        // Fall back to a generic image MIME so the encoder
        // sees a non-empty type; transcode picks an output
        // MIME independent of this hint for resized variants.
        "application/octet-stream".to_string()
    });
    match crate::transcode::transcode(bytes, &source_mime, size) {
        Ok(transcoded) => {
            response.content_hash = Some(transcoded.content_hash.clone());
            response.size = Some(size.as_str().to_string());
            response.mime = Some(transcoded.mime);
            Ok(ResolveOutput {
                response,
                cache_payload: Some((
                    transcoded.content_hash,
                    transcoded.bytes,
                )),
            })
        }
        Err(e) => {
            // Transcode failure degrades the same way as read
            // failure; the path field is still set so legacy
            // callers keep going.
            tracing::warn!(
                cover_path = %cover_path,
                error = %e,
                "artwork.resolve: transcode failed; degrading to path-only"
            );
            Ok(ResolveOutput {
                response,
                cache_payload: None,
            })
        }
    }
}

fn resolve_mpd_album(
    library_roots: &[PathBuf],
    state_dir: Option<&Path>,
    value: &str,
) -> Result<ArtworkResolveResponse, String> {
    let (artist, album) =
        match evo_device_audio_shared::parse_mpd_album_value(value) {
            Ok(p) => p,
            Err(_) => {
                return Ok(ArtworkResolveResponse {
                v: 1,
                status: ResponseStatus::BadRequest,
                path: None,
                content_hash: None,
                size: None,
                provider_id: None,
                identity: None,
                mime: None,
                detail: Some(
                    "invalid mpd-album value: expected \"artist|album\" (see \
                     org.evoframework.playback.mpd subject emission)"
                        .to_string(),
                ),
            });
            }
        };
    // Cascade — the operator memo mandates that a 404 is
    // legitimate ONLY when every strategy on every source
    // genuinely has nothing:
    //
    //   1. tag-walk match: walk library_roots looking for a
    //      file whose lofty-readable tags match (artist,
    //      album). Works for MP3/FLAC/M4A/OGG/etc.; fails
    //      for formats lofty can't parse (DSD DSF/DFF today)
    //      and for files without artist/album tags.
    //   2. folder-name match (fallback): walk library_roots
    //      looking for a directory whose basename contains the
    //      normalised album name; return ANY file inside such
    //      a directory so the cascade below reaches folder-
    //      sidecar (folder.jpg/cover.jpg) art without needing
    //      readable tags.
    //   3. cascade the found path through
    //      `resolve_cover_for_audio_file` → sidecar → embedded
    //      → NotFound.
    //
    // Step 2 covers DSD folders (lofty can't tag-read DSF today)
    // and every other format where tag reads fail but the
    // folder still carries operator-visible cover art.
    let tag_walk_result = evo_device_audio_shared::first_matching_audio_path(
        library_roots,
        &artist,
        &album,
    );
    let found = match tag_walk_result {
        Ok(Some(p)) => Some(p),
        Ok(None) => None,
        Err(evo_device_audio_shared::MatchError::LimitExceeded) => {
            return Ok(ArtworkResolveResponse {
                v: 1,
                status: ResponseStatus::NotFound,
                path: None,
                content_hash: None,
                size: None,
                provider_id: None,
                identity: Some(Identity { artist: artist.clone(), album: album.clone() }),
                mime: None,
                detail: Some(format!(
                    "mpd_album: tag-walk scan limit ({} files) reached under [library] roots",
                    evo_device_audio_shared::MAX_MPD_ALBUM_SCAN_CANDIDATES
                )),
            });
        }
        Err(evo_device_audio_shared::MatchError::Io(m)) => {
            return Ok(ArtworkResolveResponse {
                v: 1,
                status: ResponseStatus::NotFound,
                path: None,
                content_hash: None,
                size: None,
                provider_id: None,
                identity: Some(Identity {
                    artist: artist.clone(),
                    album: album.clone(),
                }),
                mime: None,
                detail: Some(m),
            });
        }
    };
    let track_path = match found {
        Some(p) => p,
        None => {
            // tag-walk missed — fall through to folder-name
            // match. This is the cascade step that covers DSD
            // + tag-less libraries. Returns ANY file inside a
            // matching folder; the cascade below picks up the
            // sidecar / embedded from there.
            match evo_device_audio_shared::first_file_in_album_named_folder(
                library_roots,
                &artist,
                &album,
            ) {
                Ok(Some(p)) => p,
                Ok(None) => {
                    return Ok(ArtworkResolveResponse {
                        v: 1,
                        status: ResponseStatus::NotFound,
                        path: None,
                        content_hash: None,
                        mime: None,
                        size: None,
                        provider_id: None,
                        identity: Some(Identity {
                            artist: artist.clone(),
                            album: album.clone(),
                        }),
                        detail: Some(
                            "mpd_album: no file under [library] roots \
                             with matching tags AND no directory whose \
                             name matches the album value"
                                .to_string(),
                        ),
                    });
                }
                Err(evo_device_audio_shared::MatchError::LimitExceeded) => {
                    return Ok(ArtworkResolveResponse {
                        v: 1,
                        status: ResponseStatus::NotFound,
                        path: None,
                        content_hash: None,
                        size: None,
                        provider_id: None,
                        identity: Some(Identity { artist: artist.clone(), album: album.clone() }),
                        mime: None,
                        detail: Some(format!(
                            "mpd_album: folder-name scan limit ({} directories) reached",
                            evo_device_audio_shared::MAX_ALBUM_FOLDER_SCAN_DIRECTORIES
                        )),
                    });
                }
                Err(evo_device_audio_shared::MatchError::Io(m)) => {
                    return Ok(ArtworkResolveResponse {
                        v: 1,
                        status: ResponseStatus::NotFound,
                        path: None,
                        content_hash: None,
                        size: None,
                        provider_id: None,
                        identity: Some(Identity {
                            artist: artist.clone(),
                            album: album.clone(),
                        }),
                        mime: None,
                        detail: Some(m),
                    });
                }
            }
        }
    };
    resolve_cover_for_audio_file(
        state_dir,
        &track_path,
        Some(Identity {
            artist: artist.clone(),
            album: album.clone(),
        }),
    )
}

/// Sidecar and embedded art for a resolved on-disk track path.
///
/// `known_identity` is the caller-supplied `(artist, album)` when
/// the request scheme already carries that pair — mpd-album's
/// parsed value. Passed through verbatim to the NotFound
/// response so the endpoint's online-cascade synth uses the
/// user's original intent rather than whatever tags the file
/// happens to carry (which may drift from the mpd-album value
/// when the operator has re-tagged tracks or when the file
/// matched via folder-name fallback).
///
/// When `None`, the mpd-path caller path applies: on NotFound,
/// identity is read from the file's own tags via
/// `embedded::read_identity`. Absent when the file has no
/// `(artist, album)` tags or lofty can't parse the format —
/// endpoint short-circuits online in that case.
fn resolve_cover_for_audio_file(
    state_dir: Option<&Path>,
    track_path: &Path,
    known_identity: Option<Identity>,
) -> Result<ArtworkResolveResponse, String> {
    if let Some(cover) = find_cover_beside_audio_file(track_path) {
        // Sidecar cover: cover.jpg / folder.jpg / etc. living in
        // the audio file's parent directory (or, via the folder-
        // name fallback, in the album-named folder).
        return ok_from_path(cover, PROVIDER_LOCAL_SIDECAR);
    }

    if let Some(img) = embedded::read_embedded_cover(track_path) {
        let Some(dir) = state_dir else {
            return Ok(ArtworkResolveResponse {
                v: 1,
                status: ResponseStatus::NotFound,
                path: None,
                content_hash: None,
                size: None,
                provider_id: None,
                identity: identity_or_read_from_file(
                    known_identity,
                    track_path,
                ),
                mime: None,
                detail: Some(
                    "embedded cover in tags but no state_dir to write cache; cannot expose path"
                        .to_string(),
                ),
            });
        };
        let cached = embedded::write_embedded_to_cache(dir, track_path, &img)?;
        // Embedded cover: bytes extracted from the file's ID3 /
        // Vorbis / FLAC / MP4 tags via lofty and cached beside
        // the audio file.
        return ok_from_path(cached, PROVIDER_LOCAL_EMBEDDED);
    }

    Ok(ArtworkResolveResponse {
        v: 1,
        status: ResponseStatus::NotFound,
        path: None,
        content_hash: None,
        size: None,
        provider_id: None,
        identity: identity_or_read_from_file(known_identity, track_path),
        mime: None,
        detail: Some("no sidecar or embedded cover for this track".to_string()),
    })
}

/// Return the caller-supplied identity when present; otherwise
/// read `(artist, album)` from the file's tags. Returns `None`
/// when both are absent — the endpoint short-circuits online
/// in that case (structured 404 rather than a half-identity
/// online lookup that could match unrelated releases).
fn identity_or_read_from_file(
    known_identity: Option<Identity>,
    track_path: &Path,
) -> Option<Identity> {
    if known_identity.is_some() {
        return known_identity;
    }
    embedded::read_identity(track_path)
        .map(|(artist, album)| Identity { artist, album })
}

/// Stable identifier for the local sidecar cascade leaf. Surfaced
/// via `X-Artwork-Provider` on the endpoint's resolve response so
/// operators (and rig probes) can distinguish "cover came from
/// folder.jpg beside the audio file" from "cover came from the
/// audio file's own tags".
const PROVIDER_LOCAL_SIDECAR: &str = "local_sidecar";

/// Stable identifier for the local embedded-cover cascade leaf.
const PROVIDER_LOCAL_EMBEDDED: &str = "local_embedded";

fn ok_from_path(
    cover: PathBuf,
    provider_id: &'static str,
) -> Result<ArtworkResolveResponse, String> {
    let mime = mime_for_path(&cover).map(str::to_string);
    let path = cover
        .to_str()
        .ok_or("cover path is not valid UTF-8; cannot represent in JSON")?
        .to_string();
    Ok(ArtworkResolveResponse {
        v: 1,
        status: ResponseStatus::Ok,
        path: Some(path),
        content_hash: None,
        mime,
        size: None,
        provider_id: Some(provider_id.to_string()),
        detail: None,
        identity: None,
    })
}

fn resolve_mpd_path(
    library_roots: &[PathBuf],
    state_dir: Option<&Path>,
    value: &str,
) -> Result<ArtworkResolveResponse, String> {
    if value.is_empty() {
        return Ok(ArtworkResolveResponse {
            v: 1,
            status: ResponseStatus::BadRequest,
            path: None,
            content_hash: None,
            mime: None,
            size: None,
            provider_id: None,
            identity: None,
            detail: Some("empty mpd-path value".to_string()),
        });
    }

    let Some(track_path) = resolve_audio_path(library_roots, value) else {
        return Ok(ArtworkResolveResponse {
            v: 1,
            status: ResponseStatus::NotFound,
            path: None,
            content_hash: None,
            mime: None,
            size: None,
            provider_id: None,
            identity: None,
            detail: Some("audio file not found for mpd_path".to_string()),
        });
    };

    resolve_cover_for_audio_file(state_dir, &track_path, None)
}

/// MIME inference from file extension for sidecar covers.
/// The set matches the priority list (`.jpg` / `.jpeg` /
/// `.png` / `.webp` / `.gif`); anything else falls back to
/// `image/jpeg` — the framework transcode step re-encodes
/// resized variants to WebP regardless, so a wrong MIME on
/// the source only affects the pass-through case.
fn mime_from_extension(path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(str::to_lowercase);
    match ext.as_deref() {
        Some("png") => "image/png".into(),
        Some("webp") => "image/webp".into(),
        Some("gif") => "image/gif".into(),
        _ => "image/jpeg".into(),
    }
}

/// Resolve MPD-visible directory string to an on-disk
/// [`PathBuf`] if the directory exists. Symmetric with
/// [`resolve_audio_path`] but for directories.
fn resolve_directory_path(
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
        return p.is_dir().then(|| p.to_path_buf());
    }
    for root in library_roots {
        let joined = root.join(value);
        if joined.is_dir() {
            return Some(joined);
        }
    }
    None
}

/// Resolve the [`SCHEME_MPD_DIRECTORY`] scheme.
///
/// The caller identifies a directory subject (an MPD-visible
/// path — library-relative or absolute); the plugin scans the
/// directory itself (NOT subdirectories) for a sidecar cover
/// file matching the priority list, then falls back to any
/// image extension. Returns `NotFound` when the directory
/// exists but carries no cover file; `BadRequest` when the
/// path is empty or a URL; passes an unknown directory through
/// as `NotFound` (symmetric with `mpd-path`).
///
/// Used by `library.browse_library` to emit a `cover_url` on
/// directory entries — the operator's folder-browser view
/// then renders folder art instead of a generic folder icon.
fn resolve_mpd_directory(
    library_roots: &[PathBuf],
    value: &str,
) -> Result<ArtworkResolveResponse, String> {
    if value.is_empty() {
        return Ok(ArtworkResolveResponse {
            v: 1,
            status: ResponseStatus::BadRequest,
            path: None,
            content_hash: None,
            mime: None,
            size: None,
            provider_id: None,
            identity: None,
            detail: Some("empty mpd-directory value".to_string()),
        });
    }

    let Some(dir_path) = resolve_directory_path(library_roots, value) else {
        return Ok(ArtworkResolveResponse {
            v: 1,
            status: ResponseStatus::NotFound,
            path: None,
            content_hash: None,
            mime: None,
            size: None,
            provider_id: None,
            identity: None,
            detail: Some(format!(
                "directory not found for mpd-directory value {value:?}"
            )),
        });
    };

    match find_cover_in_directory(&dir_path) {
        Some(cover) => {
            let mime = mime_from_extension(&cover);
            Ok(ArtworkResolveResponse {
                v: 1,
                status: ResponseStatus::Ok,
                path: Some(cover.to_string_lossy().into_owned()),
                content_hash: None,
                mime: Some(mime),
                size: None,
                provider_id: Some("local_sidecar".into()),
                identity: None,
                detail: None,
            })
        }
        None => Ok(ArtworkResolveResponse {
            v: 1,
            status: ResponseStatus::NotFound,
            path: None,
            content_hash: None,
            mime: None,
            size: None,
            provider_id: None,
            identity: None,
            detail: Some(format!(
                "no sidecar cover in directory {}",
                dir_path.display()
            )),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_cover_prefers_first_name() {
        let dir = tempfile::tempdir().unwrap();
        let flac = dir.path().join("1.flac");
        std::fs::write(&flac, b"x").unwrap();
        let _ = std::fs::write(dir.path().join("folder.jpg"), b"jpeg");
        let f2 = dir.path().join("cover.jpg");
        std::fs::write(&f2, b"j2").unwrap();
        // COVER_FILE_NAMES has cover.jpg before folder.jpg
        let got = find_cover_beside_audio_file(&flac).unwrap();
        assert_eq!(got.file_name().unwrap(), "cover.jpg");
    }

    #[test]
    fn find_cover_case_insensitive_match() {
        // Cover.JPG / FOLDER.png / mixed-case Unicode all match
        // the priority list — operator-side casing is irrelevant.
        let dir = tempfile::tempdir().unwrap();
        let flac = dir.path().join("track.flac");
        std::fs::write(&flac, b"x").unwrap();
        std::fs::write(dir.path().join("Cover.JPG"), b"j").unwrap();
        let got = find_cover_beside_audio_file(&flac).unwrap();
        assert_eq!(got.file_name().unwrap(), "Cover.JPG");
    }

    #[test]
    fn find_cover_serves_webp_when_only_webp_present() {
        // Modern libraries adopting WebP for cover art are
        // served without operator action.
        let dir = tempfile::tempdir().unwrap();
        let flac = dir.path().join("track.flac");
        std::fs::write(&flac, b"x").unwrap();
        std::fs::write(dir.path().join("cover.webp"), b"webp").unwrap();
        let got = find_cover_beside_audio_file(&flac).unwrap();
        assert_eq!(got.file_name().unwrap(), "cover.webp");
    }

    #[test]
    fn find_cover_fallback_to_arbitrary_image_when_no_priority_name() {
        // Operator named the file after the album rather than
        // using a conventional name — the fallback walk picks
        // the first image-typed entry in the directory.
        let dir = tempfile::tempdir().unwrap();
        let flac = dir.path().join("track.flac");
        std::fs::write(&flac, b"x").unwrap();
        std::fs::write(dir.path().join("Symphony No. 5.jpg"), b"j").unwrap();
        let got = find_cover_beside_audio_file(&flac).unwrap();
        assert!(
            got.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("Symphony No. 5"),
            "fallback walk picked {:?}",
            got
        );
    }

    #[test]
    fn find_cover_rejects_oversize_sidecar() {
        // A 6 MB sidecar (operator-dropped scan / PSD) exceeds
        // MAX_COVER_BYTES and is skipped so smaller priority
        // entries still win.
        let dir = tempfile::tempdir().unwrap();
        let flac = dir.path().join("track.flac");
        std::fs::write(&flac, b"x").unwrap();
        let oversize = vec![0u8; 6_000_000];
        std::fs::write(dir.path().join("cover.jpg"), &oversize).unwrap();
        std::fs::write(dir.path().join("folder.png"), b"ok").unwrap();
        let got = find_cover_beside_audio_file(&flac).unwrap();
        // cover.jpg is the higher priority entry but is rejected
        // on size; folder.png wins.
        assert_eq!(got.file_name().unwrap(), "folder.png");
    }

    #[test]
    fn find_cover_returns_none_when_directory_empty() {
        let dir = tempfile::tempdir().unwrap();
        let flac = dir.path().join("track.flac");
        std::fs::write(&flac, b"x").unwrap();
        assert!(find_cover_beside_audio_file(&flac).is_none());
    }

    #[test]
    fn cover_priority_widened_to_match_volumio_evo_reference() {
        // Regression test pinning the catalogue-of-expected
        // names; the conventional set must include the
        // historical Volumio names AND modern WebP variants so
        // libraries migrated from earlier reference systems
        // resolve without re-tagging.
        let expected_subset = [
            "cover.jpg",
            "folder.jpg",
            "cover.png",
            "folder.png",
            "coverart.jpg",
            "albumart.jpg",
            "front.jpg",
            "cover.webp",
            "folder.webp",
        ];
        for name in expected_subset {
            assert!(
                COVER_FILE_NAMES.contains(&name),
                "missing priority name: {}",
                name
            );
        }
    }

    #[test]
    fn resolve_mpd_path_with_root() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("Artist").join("Alb");
        std::fs::create_dir_all(&sub).unwrap();
        let flac = sub.join("1.flac");
        std::fs::write(&flac, b"x").unwrap();
        let _ = std::fs::write(sub.join("folder.jpg"), b"jpeg");

        let body = format!(
            r#"{{"v":1,"target":{{"scheme":"{}","value":"Artist/Alb/1.flac"}}}}"#,
            SCHEME_MPD_PATH
        );
        let r =
            resolve_artwork(&[dir.path().to_path_buf()], None, body.as_bytes())
                .unwrap();
        assert_eq!(r.response.status, ResponseStatus::Ok);
        assert!(r.response.path.as_ref().unwrap().ends_with("folder.jpg"));
        assert_eq!(r.response.mime.as_deref(), Some("image/jpeg"));
    }

    #[test]
    fn http_mpd_path_not_found() {
        let r = resolve_artwork(
            &[],
            None,
            r#"{"v":1,"target":{"scheme":"mpd-path","value":"http://x/a.flac"}}"#.as_bytes(),
        )
        .unwrap();
        assert_eq!(r.response.status, ResponseStatus::NotFound);
    }

    #[test]
    fn resolve_mpd_album_sidecar() {
        use lofty::config::WriteOptions;
        use lofty::tag::Accessor;
        use lofty::tag::Tag;
        use lofty::tag::TagExt;
        use lofty::tag::TagType;

        const MINI_MP3: &[u8] = include_bytes!(
            "../../../crates/evo-device-audio-shared/assets/minimal.mp3"
        );
        let dir = tempfile::tempdir().unwrap();
        let album_dir = dir.path().join("ArtZ").join("AlbZ");
        std::fs::create_dir_all(&album_dir).unwrap();
        let flac = album_dir.join("t.mp3");
        std::fs::write(&flac, MINI_MP3).unwrap();
        let mut tag = Tag::new(TagType::Id3v2);
        tag.set_artist("ArtZ".to_string());
        tag.set_album("AlbZ".to_string());
        tag.save_to_path(&flac, WriteOptions::new().preferred_padding(0))
            .expect("tag save");
        std::fs::write(album_dir.join("folder.jpg"), b"jpeg").unwrap();

        let body =
            r##"{"v":1,"target":{"scheme":"mpd-album","value":"ArtZ|AlbZ"}}"##;
        let r =
            resolve_artwork(&[dir.path().to_path_buf()], None, body.as_bytes())
                .unwrap();
        assert_eq!(r.response.status, ResponseStatus::Ok);
        assert!(r.response.path.as_ref().unwrap().ends_with("folder.jpg"));
        assert_eq!(r.response.mime.as_deref(), Some("image/jpeg"));
    }

    #[test]
    fn resolve_mpd_album_cascades_to_folder_name_when_tag_walk_finds_nothing() {
        // Pins the operator's Gap 1 regression verbatim: a DSD
        // album (lofty can't tag-parse .dsf today) whose folder
        // has folder.jpg beside it MUST resolve via the
        // folder-name-fallback cascade — not 404 because tags
        // couldn't be read.
        //
        // Fixture shape mirrors the actual DSD library on the
        // rigs (`INTERNAL/[DSD64] Fiona Joy - Signature Solo/`
        // + folder.jpg + one .dsf file) so the test proves the
        // cascade closes the exact defect the memo names.
        let dir = tempfile::tempdir().unwrap();
        let album_dir = dir.path().join("[DSD64] Fiona Joy - Signature Solo");
        std::fs::create_dir_all(&album_dir).unwrap();
        std::fs::write(
            album_dir.join("01. Ceremony.dsf"),
            b"not-a-real-dsf-just-bytes-lofty-cant-parse",
        )
        .unwrap();
        std::fs::write(album_dir.join("folder.jpg"), b"jpg").unwrap();

        // mpd-album value the emitter would emit for this album.
        let body = r##"{"v":1,"target":{"scheme":"mpd-album","value":"Fiona Joy|Signature - Solo"}}"##;
        let r =
            resolve_artwork(&[dir.path().to_path_buf()], None, body.as_bytes())
                .expect("resolve_artwork must not error");
        assert_eq!(
            r.response.status,
            ResponseStatus::Ok,
            "cascade MUST return Ok — DSD folder with folder.jpg cannot 404"
        );
        assert!(
            r.response.path.as_ref().unwrap().ends_with("folder.jpg"),
            "cascade must land on folder.jpg via
             folder-name-match → resolve_cover_for_audio_file → sidecar"
        );
    }

    #[test]
    fn resolve_mpd_album_returns_not_found_when_no_strategy_hits() {
        // With no matching tags AND no folder name that
        // contains the album value, the resolver honestly
        // returns NotFound — this is the only legitimate 404
        // per the memo's "A 404 is legitimate ONLY when every
        // strategy on every source genuinely has nothing".
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("Different").join("Album"))
            .unwrap();
        // The one folder has no folder.jpg, no matching name,
        // no audio file with matching tags.
        let body = r##"{"v":1,"target":{"scheme":"mpd-album","value":"Absent|Album"}}"##;
        let r =
            resolve_artwork(&[dir.path().to_path_buf()], None, body.as_bytes())
                .expect("resolve_artwork must not error");
        assert_eq!(r.response.status, ResponseStatus::NotFound);
    }
}
