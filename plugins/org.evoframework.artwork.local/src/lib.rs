// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! # org-evoframework-artwork-local
//!
//! **Milestone 4** — `artwork.providers` singleton respondent. Resolves
//! local sidecar cover art for tracks announced by
//! `org.evoframework.playback.mpd`, using the same `mpd-path` / `mpd-album`
//! addressing scheme strings as [`ExternalAddressing`](
//! evo_plugin_sdk::contract::ExternalAddressing) (see
//! `resolve::SCHEME_MPD_PATH` / `resolve::SCHEME_MPD_ALBUM`).
//!
//! # `artwork.resolve` (JSON, UTF-8)
//!
//! Request (v1 only):
//! ```json
//! {"v":1,"target":{"scheme":"mpd-path","value":"Artist/Album/01.flac"}}
//! ```
//! Response always includes `"v":1` and a `status` field: `ok`,
//! `not_found`, `unsupported`, or `bad_request`, plus optional
//! `path`, `mime`, and `detail` as document in [`resolve::ArtworkResolveResponse`].
//!
//! - **`mpd-path`**: `value` is MPD’s `file` (relative to a configured
//!   [`config::PluginConfig::library_roots`] or absolute on disk). A cover
//!   file next to the resolved audio file is chosen from a fixed name list
//!   (`folder.jpg`, `cover.jpg`, …) in [`resolve::find_cover_beside_audio_file`].
//! - **`mpd-album`**: `value` is `"{artist}|{album}"` as emitted by
//!   `org.evoframework.playback.mpd` for the `album` subject. The respondent scans
//!   files under [library] roots and picks the **first** track (deterministic
//!   walk) whose primary tag artist and album match; it then uses the same
//!   cover logic as `mpd-path` for that file. Large libraries are bounded (see
//!   `evo_device_audio_shared::MAX_MPD_ALBUM_SCAN_CANDIDATES`).
//!
//! # Version alignment
//!
//! [`PluginIdentity::version`], the embedded `manifest.toml` `[plugin]`
//! section, and this crate’s `CARGO_PKG_VERSION` must match; see
//! [`plugin_crate_version`].
//!
//! # Reference
//!
//! [`evo_plugin_sdk::contract::Respondent`] and
//! `docs/engineering/PLUGIN_AUTHORING.md` (singleton respondent).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::manual_async_fn)]

mod asf;
mod config;
mod embedded;
mod resolve;
mod transcode;

use std::future::Future;

use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities,
};
use evo_plugin_sdk::Manifest;

use crate::config::PluginConfig;

/// Embedded manifest.
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");

/// Plugin reverse-DNS name; shared with the manifest and tests.
pub const PLUGIN_NAME: &str = "org.evoframework.artwork.local";

/// Request type: resolve cover / visual material for a subject.
const REQUEST_ARTWORK_RESOLVE: &str = "artwork.resolve";

/// Request type: wipe this plugin's on-disk artwork cache.
///
/// Recursively deletes `state_dir/artwork_cache/` — the
/// full-size embedded-cover extracts this plugin writes on
/// resolve. Idempotent: an absent cache directory is a no-op.
/// Runs on a blocking thread because filesystem walks are
/// synchronous.
///
/// The operator-facing intent is "the on-disk artwork data
/// looks wrong and I want it re-derived from source" — a
/// subsequent `artwork.resolve` call re-extracts the cover
/// from the audio file's tags.
const REQUEST_ARTWORK_LOCAL_CLEAR_CACHE: &str = "artwork.local.clear_cache";

/// Parse the embedded [`Manifest`].
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("org-evoframework-artwork-local: embedded manifest must parse")
}

/// Handle `artwork.local.clear_cache`.
///
/// Wipes `state_dir/artwork_cache/` recursively — the on-disk
/// store this plugin writes embedded-cover extracts into.
/// Returns a small JSON envelope describing the outcome:
/// `{ v: 1, status: "ok" | "no_state_dir", cleared_bytes:
/// <u64>, cleared_files: <u64>, path: <string> }`.
///
/// Idempotent — an absent cache directory returns
/// `status: ok` with zero counts.
/// Request payload for `artwork.local.clear_cache`.
///
/// `target` is optional: absent target = global wipe (the
/// Settings-panel "Clear all" button); present target =
/// scoped drop of one track's embedded-extract entry.
/// Absent-target behaviour is preserved verbatim.
#[derive(Debug, serde::Deserialize)]
struct LocalClearCacheRequest {
    #[allow(dead_code)]
    #[serde(default = "default_v")]
    v: u8,
    #[serde(default)]
    target: Option<LocalClearTarget>,
}

fn default_v() -> u8 {
    1
}

/// Target selector for a scoped drop on artwork.local.
///
/// Supported schemes:
///
/// - `mpd-path` — `value` is an MPD-visible track path (the
///   same string this plugin's `artwork.resolve` accepts as a
///   subject). The plugin computes the same
///   `cache_basename_for_track` prefix it stored under and
///   deletes every extension variant (`.jpg` / `.png` /
///   `.webp` / `.gif`) that matched.
///
/// Rejected schemes (with `status: "bad_request"`):
///
/// - `mpd-album` — the on-disk cache is per-track (embedded
///   extractions from `read_embedded_cover`). Album covers
///   come from sidecar files on disk (`folder.jpg` etc) which
///   this plugin never persists a copy of — clearing a
///   per-album entry has no meaning here. The UI can iterate
///   `mpd-path` per track in the album if it needs the
///   embedded extracts wiped.
///
/// - Anything else — same shape refusal.
#[derive(Debug, serde::Deserialize)]
struct LocalClearTarget {
    scheme: String,
    value: String,
}

async fn handle_clear_cache(
    req: &Request,
    state_dir: Option<&std::path::Path>,
    library_roots: &[std::path::PathBuf],
) -> Result<Response, PluginError> {
    let state_dir = match state_dir {
        Some(d) => d.to_path_buf(),
        None => {
            let body = serde_json::to_vec(&serde_json::json!({
                "v": 1,
                "status": "no_state_dir",
                "cleared_bytes": 0u64,
                "cleared_files": 0u64,
                "path": null,
            }))
            .map_err(|e| {
                PluginError::Permanent(format!(
                    "artwork.local.clear_cache response JSON: {e}"
                ))
            })?;
            return Ok(Response::for_request(req, body));
        }
    };
    let cache_dir = state_dir.join("artwork_cache");

    let parsed: LocalClearCacheRequest = if req.payload.is_empty() {
        LocalClearCacheRequest { v: 1, target: None }
    } else {
        match serde_json::from_slice(&req.payload) {
            Ok(p) => p,
            Err(e) => {
                let body = serde_json::to_vec(&serde_json::json!({
                    "v": 1,
                    "status": "bad_request",
                    "detail": format!("artwork.local.clear_cache payload JSON: {e}"),
                }))
                .map_err(|se| PluginError::Permanent(format!(
                    "artwork.local.clear_cache error response JSON: {se}"
                )))?;
                return Ok(Response::for_request(req, body));
            }
        }
    };

    match parsed.target {
        None => {
            // Global wipe — pre-target behaviour preserved.
            let cache_dir_for_thread = cache_dir.clone();
            let (cleared_bytes, cleared_files) =
                tokio::task::spawn_blocking(move || {
                    wipe_dir_reporting(&cache_dir_for_thread)
                })
                .await
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "artwork.local.clear_cache blocking join failed: {e}"
                    ))
                })?
                .map_err(PluginError::Permanent)?;
            tracing::info!(
                plugin = PLUGIN_NAME,
                scope = "all",
                path = %cache_dir.display(),
                cleared_bytes,
                cleared_files,
                "artwork.local.clear_cache: wiped on-disk cache (global)"
            );
            let body = serde_json::to_vec(&serde_json::json!({
                "v": 1,
                "status": "ok",
                "scope": "all",
                "cleared_bytes": cleared_bytes,
                "cleared_files": cleared_files,
                "path": cache_dir.display().to_string(),
            }))
            .map_err(|e| {
                PluginError::Permanent(format!(
                    "artwork.local.clear_cache response JSON: {e}"
                ))
            })?;
            Ok(Response::for_request(req, body))
        }
        Some(target) => match target.scheme.as_str() {
            "mpd-path" => {
                let track_path = std::path::PathBuf::from(target.value.clone());
                let basename =
                    crate::embedded::cache_basename_for_track(&track_path);
                let cache_dir_for_thread = cache_dir.clone();
                let basename_for_thread = basename.clone();
                let (cleared_bytes, cleared_files) =
                    tokio::task::spawn_blocking(move || {
                        drop_by_basename(
                            &cache_dir_for_thread,
                            &basename_for_thread,
                        )
                    })
                    .await
                    .map_err(|e| {
                        PluginError::Permanent(format!(
                            "artwork.local.clear_cache blocking join failed: {e}"
                        ))
                    })?
                    .map_err(PluginError::Permanent)?;
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    scope = "targeted",
                    target_scheme = %target.scheme,
                    target_value = %target.value,
                    basename = %basename,
                    cleared_bytes,
                    cleared_files,
                    "artwork.local.clear_cache: targeted on-disk drop"
                );
                let body = serde_json::to_vec(&serde_json::json!({
                    "v": 1,
                    "status": "ok",
                    "scope": "targeted",
                    "target": {
                        "scheme": target.scheme,
                        "value": target.value,
                        "basename": basename,
                    },
                    "cleared_bytes": cleared_bytes,
                    "cleared_files": cleared_files,
                    "path": cache_dir.display().to_string(),
                }))
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "artwork.local.clear_cache response JSON: {e}"
                    ))
                })?;
                Ok(Response::for_request(req, body))
            }
            "mpd-album" => {
                // Best-effort per-album drop. The on-disk cache
                // is per-track (embedded extracts keyed on the
                // track's basename hash — see
                // `cache_basename_for_track`), so an album-
                // scoped drop resolves to "the set of per-track
                // entries for tracks in this album's directory."
                //
                // Approach: find the album directory by
                // walking library_roots for the first track
                // whose `(artist, album)` tags match, take its
                // parent directory, and drop the cache entry
                // for every audio file at that directory's top
                // level. Deterministic per-artist/album input;
                // no MPD round trip needed.
                //
                // Note on the framework AssetCache: the resized
                // WebP variants live under content-hash keys in
                // the framework's asset cache. That cache is
                // content-addressed and immutable — if the
                // source bytes change (operator replaces
                // folder.jpg), the next resolve produces a new
                // hash and a new URL, and the browser fetches
                // fresh bytes without any eviction step. If the
                // source bytes did not change but the operator
                // wants a re-cascade (e.g., online provider
                // updated its data), the framework endpoint
                // exposes `?refresh=1` on
                // `GET /api/v1/audio/artwork?scheme=mpd-album&value=…&refresh=1`
                // which evicts the negative memo and re-runs
                // the cascade.
                let parsed = evo_device_audio_shared::parse_mpd_album_value(
                    &target.value,
                );
                let (artist, album) = match parsed {
                    Ok(pair) => pair,
                    Err(_) => {
                        let body = serde_json::to_vec(&serde_json::json!({
                            "v": 1,
                            "status": "bad_request",
                            "detail": format!(
                                "artwork.local.clear_cache: invalid mpd-album value \
                                 {value:?}; expected \"artist|album\"",
                                value = target.value
                            ),
                        }))
                        .map_err(|e| PluginError::Permanent(format!(
                            "artwork.local.clear_cache error response JSON: {e}"
                        )))?;
                        return Ok(Response::for_request(req, body));
                    }
                };

                let library_roots_for_thread = library_roots.to_vec();
                let cache_dir_for_thread = cache_dir.clone();
                let artist_for_thread = artist.clone();
                let album_for_thread = album.clone();
                let dropped: Result<
                    (Option<std::path::PathBuf>, u64, u64),
                    String,
                > = tokio::task::spawn_blocking(move || {
                    clear_album_cache_entries(
                        &library_roots_for_thread,
                        &cache_dir_for_thread,
                        &artist_for_thread,
                        &album_for_thread,
                    )
                })
                .await
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "artwork.local.clear_cache blocking join failed: {e}"
                    ))
                })?;
                let (album_dir, cleared_bytes, cleared_files) =
                    dropped.map_err(PluginError::Permanent)?;
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    scope = "targeted",
                    target_scheme = %target.scheme,
                    target_value = %target.value,
                    album_dir = ?album_dir.as_ref().map(|p| p.display().to_string()),
                    cleared_bytes,
                    cleared_files,
                    "artwork.local.clear_cache: targeted mpd-album drop (best-effort per-track scan of album directory)"
                );
                let body = serde_json::to_vec(&serde_json::json!({
                    "v": 1,
                    "status": "ok",
                    "scope": "targeted",
                    "target": {
                        "scheme": target.scheme,
                        "value": target.value,
                        "album_dir": album_dir.as_ref().map(|p| p.display().to_string()),
                    },
                    "cleared_bytes": cleared_bytes,
                    "cleared_files": cleared_files,
                    "path": cache_dir.display().to_string(),
                    "refresh_url_hint": format!(
                        "GET /api/v1/audio/artwork?scheme=mpd-album&value={artist}|{album}&refresh=1 \
                         evicts the framework endpoint's negative memo and re-runs the five-source cascade; \
                         content-addressed bytes in the framework AssetCache are re-served only when the \
                         source produces different bytes (new hash → new URL, natural browser cache miss)"
                    ),
                }))
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "artwork.local.clear_cache response JSON: {e}"
                    ))
                })?;
                Ok(Response::for_request(req, body))
            }
            other => {
                let body = serde_json::to_vec(&serde_json::json!({
                    "v": 1,
                    "status": "bad_request",
                    "detail": format!(
                        "artwork.local.clear_cache: unknown target.scheme {other:?}; \
                         supported: \"mpd-path\" (value = MPD track path) or \
                         \"mpd-album\" (value = \"Artist|Album\")"
                    ),
                }))
                .map_err(|e| PluginError::Permanent(format!(
                    "artwork.local.clear_cache error response JSON: {e}"
                )))?;
                Ok(Response::for_request(req, body))
            }
        },
    }
}

/// Delete every file under `cache_dir` whose stem equals
/// `basename` (extension varies with the source image's MIME
/// — `.jpg` / `.png` / `.webp` / `.gif`). Returns
/// `(cleared_bytes, cleared_files)` identical to
/// [`wipe_dir_reporting`] so the caller reports the same
/// shape for global and targeted paths. Missing cache
/// directory returns `(0, 0)` — same idempotent shape as the
/// global wipe.
fn drop_by_basename(
    cache_dir: &std::path::Path,
    basename: &str,
) -> Result<(u64, u64), String> {
    if !cache_dir.exists() {
        return Ok((0, 0));
    }
    let mut bytes: u64 = 0;
    let mut files: u64 = 0;
    let entries = std::fs::read_dir(cache_dir)
        .map_err(|e| format!("read_dir {}: {e}", cache_dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| {
            format!("read_dir entry in {}: {e}", cache_dir.display())
        })?;
        let path = entry.path();
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s,
            None => continue,
        };
        if stem != basename {
            continue;
        }
        let meta = std::fs::metadata(&path)
            .map_err(|e| format!("stat {}: {e}", path.display()))?;
        if meta.is_file() {
            bytes = bytes.saturating_add(meta.len());
            files = files.saturating_add(1);
        }
        std::fs::remove_file(&path)
            .map_err(|e| format!("remove_file {}: {e}", path.display()))?;
    }
    Ok((bytes, files))
}

/// Recursively delete `dir` and every file / subdirectory
/// under it, tallying the bytes and file count removed. An
/// absent `dir` is a no-op that returns `(0, 0)`. Errors
/// bubble as `String` for the caller to wrap in
/// `PluginError::Permanent`.
/// Best-effort per-album cache eviction.
///
/// Walks `library_roots` looking for the first track whose
/// `(artist, album)` tags match the supplied pair, uses its
/// parent directory as the album directory, then drops the
/// per-track cache entry (via [`drop_by_basename`]) for every
/// audio file at that directory's top level. Returns
/// `(album_dir, cleared_bytes, cleared_files)`. On no
/// matching track the album directory is `None` and both
/// counts are zero — the operator asked to drop artwork for
/// an album the plugin has never seen, which is honest zero,
/// not an error.
///
/// Subdirectory scan is intentionally shallow: multi-disc
/// box-sets typically nest one level deep, and a shallow scan
/// keeps the cost bounded for large libraries. Operators with
/// deeply nested layouts can iterate `mpd-path` per track
/// through the same verb for the same effect.
fn clear_album_cache_entries(
    library_roots: &[std::path::PathBuf],
    cache_dir: &std::path::Path,
    artist: &str,
    album: &str,
) -> Result<(Option<std::path::PathBuf>, u64, u64), String> {
    let tag_walk_result = evo_device_audio_shared::first_matching_audio_path(
        library_roots,
        artist,
        album,
    );
    let first_track = match tag_walk_result {
        Ok(Some(p)) => p,
        Ok(None) => return Ok((None, 0, 0)),
        Err(e) => {
            return Err(format!(
                "artwork.local.clear_cache mpd-album tag-walk error: {e:?}"
            ));
        }
    };
    let Some(album_dir) =
        first_track.parent().map(std::path::Path::to_path_buf)
    else {
        return Ok((None, 0, 0));
    };
    if !album_dir.is_dir() {
        return Ok((Some(album_dir), 0, 0));
    }
    let mut total_bytes: u64 = 0;
    let mut total_files: u64 = 0;
    let entries = match std::fs::read_dir(&album_dir) {
        Ok(it) => it,
        Err(e) => {
            return Err(format!("read_dir {}: {e}", album_dir.display()));
        }
    };
    for entry in entries {
        let entry = entry.map_err(|e| {
            format!("read_dir entry in {}: {e}", album_dir.display())
        })?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // Filter for audio-file extensions the plugin's
        // embedded extractor knows how to write cache
        // entries for.
        let is_audio = path
            .extension()
            .and_then(|s| s.to_str())
            .map(str::to_lowercase)
            .is_some_and(|ext| {
                matches!(
                    ext.as_str(),
                    "mp3"
                        | "flac"
                        | "m4a"
                        | "ogg"
                        | "opus"
                        | "wav"
                        | "aiff"
                        | "aif"
                        | "wma"
                        | "alac"
                        | "ape"
                        | "mp4"
                )
            });
        if !is_audio {
            continue;
        }
        let basename = crate::embedded::cache_basename_for_track(&path);
        let (bytes, files) = drop_by_basename(cache_dir, &basename)?;
        total_bytes = total_bytes.saturating_add(bytes);
        total_files = total_files.saturating_add(files);
    }
    Ok((Some(album_dir), total_bytes, total_files))
}

fn wipe_dir_reporting(dir: &std::path::Path) -> Result<(u64, u64), String> {
    if !dir.exists() {
        return Ok((0, 0));
    }
    let mut bytes: u64 = 0;
    let mut files: u64 = 0;
    for entry in walk_files(dir)? {
        let meta = std::fs::metadata(&entry)
            .map_err(|e| format!("stat {}: {e}", entry.display()))?;
        if meta.is_file() {
            bytes = bytes.saturating_add(meta.len());
            files = files.saturating_add(1);
        }
    }
    std::fs::remove_dir_all(dir)
        .map_err(|e| format!("remove_dir_all {}: {e}", dir.display()))?;
    Ok((bytes, files))
}

/// Depth-first walk producing every entry (files + dirs)
/// under `root`. Used only for the tally before removal.
fn walk_files(
    root: &std::path::Path,
) -> Result<Vec<std::path::PathBuf>, String> {
    let mut stack = vec![root.to_path_buf()];
    let mut out = Vec::new();
    while let Some(path) = stack.pop() {
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            let entries = std::fs::read_dir(&path)
                .map_err(|e| format!("read_dir {}: {e}", path.display()))?;
            for e in entries.flatten() {
                stack.push(e.path());
            }
        }
        out.push(path);
    }
    Ok(out)
}

fn plugin_crate_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is valid semver")
}

/// Local artwork respondent: optional `[library]` roots in
/// `LoadContext::config`, sidecar files, embedded tag images, and
/// `state_dir` cache for the latter.
pub struct ArtworkLocalPlugin {
    /// `true` after a successful [`Plugin::load`].
    loaded: bool,
    /// Merged from [`PluginConfig::from_toml_table`].
    config: PluginConfig,
    /// `LoadContext::state_dir`; used for embedded cover cache.
    state_dir: Option<std::path::PathBuf>,
    /// `LoadContext::asset_cache`; populated during resolve so
    /// the framework's `/api/v1/audio/artwork/:content_hash`
    /// endpoint serves bytes for every size variant the plugin
    /// produces. `None` when the framework did not wire the
    /// cache (test harnesses, degraded boot); resolve still
    /// returns the file path so legacy in-process callers keep
    /// working, just without cache-hosted bytes.
    asset_cache:
        Option<std::sync::Arc<dyn evo_plugin_sdk::contract::AssetCache>>,
    /// Count of `handle_request` invocations.
    requests_handled: std::sync::atomic::AtomicU64,
}

impl ArtworkLocalPlugin {
    /// New plugin, not yet [`Plugin::load`]ed.
    pub fn new() -> Self {
        Self {
            loaded: false,
            config: PluginConfig::defaults(),
            state_dir: None,
            asset_cache: None,
            requests_handled: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Cumulative `handle_request` invocations.
    pub fn requests_handled(&self) -> u64 {
        self.requests_handled
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// For unit tests: simulate load without a real [`LoadContext`].
    #[cfg(test)]
    fn set_loaded_with_config(
        &mut self,
        config: PluginConfig,
        state_dir: std::path::PathBuf,
    ) {
        self.loaded = true;
        self.config = config;
        self.state_dir = Some(state_dir);
    }
}

impl Default for ArtworkLocalPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for ArtworkLocalPlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: PLUGIN_NAME.to_string(),
                    version: plugin_crate_version(),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: vec![
                        REQUEST_ARTWORK_RESOLVE.to_string(),
                        REQUEST_ARTWORK_LOCAL_CLEAR_CACHE.to_string(),
                    ],
                    accepts_custody: false,
                    flags: Default::default(),
                    course_correct_verbs: Vec::new(),
                },
                build_info: BuildInfo {
                    plugin_build: env!("CARGO_PKG_VERSION").to_string(),
                    sdk_version: evo_plugin_sdk::VERSION.to_string(),
                    rustc_version: None,
                    built_at: None,
                },
            }
        }
    }

    fn load<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
        async move {
            tracing::info!(
                plugin = PLUGIN_NAME,
                config_keys = ctx.config.len(),
                "artwork local plugin load"
            );
            self.config =
                PluginConfig::from_toml_table(&ctx.config).map_err(|e| {
                    PluginError::Permanent(format!(
                        "invalid plugin config: {e}"
                    ))
                })?;
            if !self.config.library_roots.is_empty() {
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    n = self.config.library_roots.len(),
                    "library search roots configured"
                );
            }
            self.state_dir = Some(ctx.state_dir.clone());
            // Asset-cache handle is plumbed by the framework's
            // admission engine into LoadContext.asset_cache.
            // Capture it so resolve can push transcoded bytes
            // into the content-hash store; the framework's
            // existing /api/v1/audio/artwork/:hash endpoint
            // serves whatever lands here.
            self.asset_cache = ctx.asset_cache.clone();
            tracing::info!(
                plugin = PLUGIN_NAME,
                asset_cache_wired = self.asset_cache.is_some(),
                "load complete"
            );
            self.loaded = true;
            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            self.loaded = false;
            self.config = PluginConfig::defaults();
            self.state_dir = None;
            self.asset_cache = None;
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            if self.loaded {
                HealthReport::healthy()
            } else {
                HealthReport::unhealthy("artwork plugin not loaded")
            }
        }
    }
}

impl Respondent for ArtworkLocalPlugin {
    fn handle_request<'a>(
        &'a self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "artwork plugin not loaded".to_string(),
                ));
            }

            if req.is_past_deadline() {
                return Err(PluginError::Transient(
                    "request deadline already expired".to_string(),
                ));
            }

            self.requests_handled
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            if req.request_type == REQUEST_ARTWORK_LOCAL_CLEAR_CACHE {
                return handle_clear_cache(
                    req,
                    self.state_dir.as_deref(),
                    &self.config.library_roots,
                )
                .await;
            }

            if req.request_type != REQUEST_ARTWORK_RESOLVE {
                return Err(PluginError::Permanent(format!(
                    "unknown request type: {:?} (not one of: {:?})",
                    req.request_type,
                    [
                        REQUEST_ARTWORK_RESOLVE,
                        REQUEST_ARTWORK_LOCAL_CLEAR_CACHE,
                    ]
                )));
            }

            tracing::debug!(
                plugin = PLUGIN_NAME,
                request_type = %req.request_type,
                cid = req.correlation_id,
                payload_len = req.payload.len(),
                "artwork.resolve"
            );

            // F6: image decode + lofty + std::fs are
            // synchronous and the embedded-cover path opens
            // arbitrary audio files for tag extraction. Run
            // on a blocking thread so the shared tokio runtime
            // isn't stalled.
            let library_roots = self.config.library_roots.clone();
            let state_dir = self.state_dir.clone();
            let payload = req.payload.clone();
            let resolve_output = match tokio::task::spawn_blocking(move || {
                resolve::resolve_artwork(
                    &library_roots,
                    state_dir.as_deref(),
                    &payload,
                )
            })
            .await
            {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => return Err(PluginError::Permanent(e)),
                Err(e) => {
                    return Err(PluginError::Permanent(format!(
                        "artwork.resolve blocking task join failed: {e}"
                    )));
                }
            };
            // Push the resolved bytes to the framework's
            // content-hash asset cache. The /api/v1/audio/
            // artwork/:content_hash endpoint serves whatever
            // lands here. On cache-write failure the response
            // still carries the content_hash; subsequent fetch
            // attempts surface as 404 and the UI's placeholder
            // rule covers — we DON'T fail the verb because
            // partial-success is better than a wholesale
            // refusal for a structurally-valid resolve.
            if let Some((content_hash, bytes)) = resolve_output.cache_payload {
                if let Some(cache) = &self.asset_cache {
                    if let Err(e) = cache.put(&content_hash, bytes).await {
                        tracing::warn!(
                            plugin = PLUGIN_NAME,
                            content_hash = %content_hash,
                            error = %e,
                            "artwork.resolve: asset cache put failed; response still carries content_hash"
                        );
                    }
                } else {
                    tracing::debug!(
                        plugin = PLUGIN_NAME,
                        content_hash = %content_hash,
                        "artwork.resolve: no asset cache wired; content_hash returned for path-only consumers"
                    );
                }
            }
            let body = resolve_output.response.json_bytes().map_err(|e| {
                PluginError::Permanent(format!("artwork response JSON: {e}"))
            })?;
            Ok(Response::for_request(req, body))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use evo_plugin_sdk::contract::HealthStatus;
    use evo_plugin_sdk::manifest::InteractionShape;
    use serde_json::Value;

    fn sample_mpd_path_payload(value: &str) -> Vec<u8> {
        format!(
            r#"{{"v":1,"target":{{"scheme":"{}","value":{}}}}}"#,
            resolve::SCHEME_MPD_PATH,
            serde_json::to_string(value).unwrap()
        )
        .into_bytes()
    }

    #[test]
    fn manifest_parses() {
        let m = manifest();
        assert_eq!(m.plugin.name, PLUGIN_NAME);
        assert_eq!(m.plugin.contract, 1);
        assert_eq!(
            m.kind
                .as_ref()
                .expect("manifest must declare [kind]")
                .interaction,
            InteractionShape::Respondent
        );
        let cap = m
            .capabilities
            .respondent
            .as_ref()
            .expect("manifest must have respondent capabilities");
        assert!(cap
            .request_types
            .iter()
            .any(|s| s == REQUEST_ARTWORK_RESOLVE));
    }

    #[tokio::test]
    async fn describe_matches_embedded_manifest() {
        let p = ArtworkLocalPlugin::new();
        let d = p.describe().await;
        let m = manifest();
        assert_eq!(d.identity.name, m.plugin.name);
        assert_eq!(
            d.identity.version, m.plugin.version,
            "CARGO_PKG_VERSION / describe / manifest [plugin].version must match"
        );
        assert!(!d.runtime_capabilities.accepts_custody);
        assert_eq!(
            d.runtime_capabilities.request_types,
            vec![REQUEST_ARTWORK_RESOLVE, REQUEST_ARTWORK_LOCAL_CLEAR_CACHE,]
        );
        let drift =
            evo_plugin_sdk::drift::detect_drift(&m, &d.runtime_capabilities);
        assert!(
            drift.is_empty(),
            "in-tree manifest.toml drifted from runtime describe(): {:?}",
            drift
        );
    }

    /// OOP manifest must declare an explicit lifecycle mode
    /// matching its operator-reload intent. `LifecycleMode`
    /// defaults to `Frozen`; a manifest carrying only the
    /// legacy `hot_reload = "restart"` field still inherits
    /// the `Frozen` default and the framework refuses
    /// operator reload gestures with `PluginIsFrozen`.
    /// artwork.local serves operator-tunable cache state +
    /// filesystem-rooted artwork providers, so its reload
    /// shape is teardown + re-admit (`reload-cleanable`).
    #[test]
    fn oop_manifest_declares_reload_cleanable_mode() {
        const MANIFEST_OOP_TOML: &str = include_str!("../manifest.oop.toml");
        let m = Manifest::from_toml(MANIFEST_OOP_TOML)
            .expect("manifest.oop.toml must parse");
        let lifecycle = m
            .lifecycle
            .as_ref()
            .expect("manifest.oop.toml must declare [lifecycle]");
        assert_eq!(
            lifecycle.mode,
            evo_plugin_sdk::manifest::LifecycleMode::ReloadCleanable,
            "manifest.oop.toml [lifecycle].mode must be 'reload-cleanable' \
             (legacy `hot_reload = \"restart\"` alone parses to the Frozen \
             default and refuses operator reload gestures)"
        );
    }

    /// Production-shipping manifest variant
    /// (`manifest.oop.toml`) carries the same capability
    /// declarations as `manifest.toml` except for the transport
    /// block. The framework's admission gate refuses any plugin
    /// whose manifest declarations drift from the runtime
    /// `describe()` output; without this test the OOP manifest
    /// can drift silently and admission fails only at deploy
    /// time on a real rig.
    #[tokio::test]
    async fn describe_matches_oop_manifest() {
        const MANIFEST_OOP_TOML: &str = include_str!("../manifest.oop.toml");
        let p = ArtworkLocalPlugin::new();
        let d = p.describe().await;
        let m = Manifest::from_toml(MANIFEST_OOP_TOML)
            .expect("manifest.oop.toml must parse");
        assert_eq!(d.identity.name, m.plugin.name);
        assert_eq!(d.identity.version, m.plugin.version);
        let drift =
            evo_plugin_sdk::drift::detect_drift(&m, &d.runtime_capabilities);
        assert!(
            drift.is_empty(),
            "manifest.oop.toml drifted from runtime describe(): {:?}",
            drift
        );
    }

    #[tokio::test]
    async fn health_unhealthy_before_load() {
        let p = ArtworkLocalPlugin::new();
        assert!(matches!(
            p.health_check().await.status,
            HealthStatus::Unhealthy
        ));
    }

    #[tokio::test]
    async fn handle_rejects_before_load() {
        let p = ArtworkLocalPlugin::new();
        let r = Request {
            request_type: REQUEST_ARTWORK_RESOLVE.to_string(),
            payload: vec![],
            correlation_id: 1,
            deadline: None,
            instance_id: None,
            principal_scope: None,
            has_step_up: false,
        };
        let e = p.handle_request(&r).await.unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
        assert_eq!(p.requests_handled(), 0);
    }

    #[tokio::test]
    async fn handle_unknown_request_type() {
        let mut p = ArtworkLocalPlugin::new();
        let tmp = tempfile::tempdir().unwrap();
        p.set_loaded_with_config(
            PluginConfig::defaults(),
            tmp.path().to_path_buf(),
        );
        let r = Request {
            request_type: "metadata.query".to_string(),
            payload: vec![],
            correlation_id: 2,
            deadline: None,
            instance_id: None,
            principal_scope: None,
            has_step_up: false,
        };
        let e = p.handle_request(&r).await.unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
        assert_eq!(p.requests_handled(), 1);
    }

    #[tokio::test]
    async fn handle_resolve_bad_request_invalid_json() {
        let mut p = ArtworkLocalPlugin::new();
        let tmp = tempfile::tempdir().unwrap();
        p.set_loaded_with_config(
            PluginConfig::defaults(),
            tmp.path().to_path_buf(),
        );
        let r = Request {
            request_type: REQUEST_ARTWORK_RESOLVE.to_string(),
            payload: b"{not json".to_vec(),
            correlation_id: 3,
            deadline: None,
            instance_id: None,
            principal_scope: None,
            has_step_up: false,
        };
        let out = p.handle_request(&r).await.unwrap();
        let v: Value = serde_json::from_slice(&out.payload).unwrap();
        assert_eq!(v["status"], "bad_request");
        assert_eq!(p.requests_handled(), 1);
    }

    #[tokio::test]
    async fn handle_resolve_not_found() {
        let mut p = ArtworkLocalPlugin::new();
        let tmp = tempfile::tempdir().unwrap();
        p.set_loaded_with_config(
            PluginConfig::defaults(),
            tmp.path().to_path_buf(),
        );
        let r = Request {
            request_type: REQUEST_ARTWORK_RESOLVE.to_string(),
            payload: sample_mpd_path_payload("/no/such/absolute.flac"),
            correlation_id: 4,
            deadline: None,
            instance_id: None,
            principal_scope: None,
            has_step_up: false,
        };
        let out = p.handle_request(&r).await.unwrap();
        let v: Value = serde_json::from_slice(&out.payload).unwrap();
        assert_eq!(v["status"], "not_found");
        assert_eq!(p.requests_handled(), 1);
    }

    #[tokio::test]
    async fn handle_resolve_ok_with_cover() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("A").join("B");
        std::fs::create_dir_all(&sub).unwrap();
        let flac = sub.join("t.flac");
        std::fs::write(&flac, b"x").unwrap();
        std::fs::write(sub.join("folder.jpg"), b"fakejpeg").unwrap();

        let rel = "A/B/t.flac";
        let mut p = ArtworkLocalPlugin::new();
        p.set_loaded_with_config(
            PluginConfig {
                library_roots: vec![dir.path().to_path_buf()],
            },
            dir.path().join("state"),
        );

        let r = Request {
            request_type: REQUEST_ARTWORK_RESOLVE.to_string(),
            payload: sample_mpd_path_payload(rel),
            correlation_id: 5,
            deadline: None,
            instance_id: None,
            principal_scope: None,
            has_step_up: false,
        };
        let out = p.handle_request(&r).await.unwrap();
        let v: Value = serde_json::from_slice(&out.payload).unwrap();
        assert_eq!(v["status"], "ok");
        let pstr = v["path"].as_str().unwrap();
        let pb = PathBuf::from(pstr);
        assert!(pb.ends_with("folder.jpg"), "{pstr}");
    }

    #[test]
    fn drop_by_basename_removes_matching_stem_only() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("artwork_cache");
        std::fs::create_dir_all(&cache).unwrap();
        // Two variants of the same basename (jpg + webp); a
        // different basename; and a mismatched-name file.
        std::fs::write(cache.join("abc_track1.jpg"), b"jpeg-bytes").unwrap();
        std::fs::write(cache.join("abc_track1.webp"), b"webp-bytes-longer")
            .unwrap();
        std::fs::write(cache.join("def_track2.jpg"), b"other").unwrap();
        std::fs::write(cache.join("unrelated.jpg"), b"unrelated").unwrap();

        let (bytes, files) = drop_by_basename(&cache, "abc_track1").unwrap();
        assert_eq!(files, 2, "both jpg + webp variants removed");
        assert_eq!(
            bytes,
            (b"jpeg-bytes".len() + b"webp-bytes-longer".len()) as u64
        );

        // The targeted files are gone.
        assert!(!cache.join("abc_track1.jpg").exists());
        assert!(!cache.join("abc_track1.webp").exists());
        // The untouched files remain.
        assert!(cache.join("def_track2.jpg").exists());
        assert!(cache.join("unrelated.jpg").exists());
    }

    #[test]
    fn drop_by_basename_missing_cache_dir_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("does_not_exist");
        let (bytes, files) = drop_by_basename(&cache, "anything").unwrap();
        assert_eq!(bytes, 0);
        assert_eq!(files, 0);
    }

    #[test]
    fn drop_by_basename_missing_basename_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("artwork_cache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("keep_me.jpg"), b"stay").unwrap();

        let (bytes, files) =
            drop_by_basename(&cache, "no_such_basename").unwrap();
        assert_eq!(bytes, 0);
        assert_eq!(files, 0);
        assert!(cache.join("keep_me.jpg").exists());
    }

    #[tokio::test]
    async fn handle_past_deadline() {
        let mut p = ArtworkLocalPlugin::new();
        let tmp = tempfile::tempdir().unwrap();
        p.set_loaded_with_config(
            PluginConfig::defaults(),
            tmp.path().to_path_buf(),
        );
        let r = Request {
            request_type: REQUEST_ARTWORK_RESOLVE.to_string(),
            payload: vec![],
            correlation_id: 6,
            deadline: Some(
                std::time::Instant::now() - std::time::Duration::from_secs(1),
            ),
            instance_id: None,
            principal_scope: None,
            has_step_up: false,
        };
        let e = p.handle_request(&r).await.unwrap_err();
        assert!(matches!(e, PluginError::Transient(_)));
        assert_eq!(p.requests_handled(), 0);
    }
}
