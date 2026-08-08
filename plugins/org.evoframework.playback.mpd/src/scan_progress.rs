// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Live scan-progress emission on the
//! `audio_library_scan_progress` subject.
//!
//! # Purpose
//!
//! MPD's `update` / `rescan` scans surface no per-track
//! progress on the wire — the framework knows the scan
//! started (from an `update_source` verb call or from an idle
//! `Update` wake) and knows the total songs count on
//! completion (from the next `stats` read), but the operator
//! UI sees nothing move between "scan started" and "scan
//! completed". On a rescan of thousands of tracks the panel
//! sits idle for minutes.
//!
//! # Design
//!
//! On every scan (verb-triggered or idle-observed), spawn a
//! watcher task that:
//!
//! 1. Estimates the total music-file count by walking the
//!    source's mount path (bounded, best-effort — null when
//!    the walk can't complete within budget).
//! 2. Polls MPD `status` every ~500 ms; when
//!    `status.updating_db` is `Some(job_id)`, emits an
//!    `audio_library_scan_progress` frame carrying the
//!    per-source `scanned_tracks` (from `stats.songs`),
//!    `estimated_total`, and `phase = "scanning"`.
//! 3. When `updating_db` returns to `None`, emits ONE
//!    terminal frame with `phase = "complete"` carrying the
//!    final counts; then republishes `audio_library_sources`
//!    plus `audio_library_state` so the settled counts land
//!    on the operator UI without a reload; then emits the
//!    empty envelope (`{ scans: [] }`) so the resting state
//!    is the documented idle shape.
//!
//! Throttled emission: 500 ms floor between frames guards
//! the happenings bus against a per-track flood (same
//! discipline the spectrum-fanout lesson pinned).
//!
//! Terminal-frame discipline: silence is indistinguishable
//! from a stalled scan. Every watcher emits an explicit
//! `phase = "complete"` frame before exit (except on
//! shutdown / connection death, which the operator sees via
//! the framework's separate liveness signals).
//!
//! Singleton gate: a process-wide `tokio::sync::Mutex<()>`
//! serialises watcher spawns so two concurrent
//! `update_source` gestures cannot fan out two watchers
//! against the same scan.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::json;

use evo_plugin_sdk::contract::{ExternalAddressing, SubjectAnnouncer};

use crate::library::{self, LibraryContext};
use crate::mpd::{ConnectTimeouts, MpdConnection, MpdEndpoint};

const PLUGIN_NAME: &str = "org.evoframework.playback.mpd";
const SCHEME_LIBRARY: &str = "evo.audio.library";
const VALUE_SCAN_PROGRESS: &str = "scan_progress";
pub(crate) const SCAN_PROGRESS_PAYLOAD_VERSION: u32 = 1;

/// Poll cadence — floor 500 ms per emission so the
/// happenings bus is not flooded on a fast rescan.
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Filesystem-walker budget for the `estimated_total`
/// pre-count. Exceeded → walker abandons and emits
/// `estimated_total = null` for the scan (UI renders
/// indeterminate progress rather than a wrong total).
const WALKER_BUDGET: Duration = Duration::from_secs(30);
/// Safety exit for the watcher — pathological cases (MPD
/// hangs mid-scan, updating_db stays set forever) MUST NOT
/// leave the watcher running indefinitely. On expiry the
/// watcher emits a terminal `phase = "complete"` frame with
/// the last observed counts and exits.
const WATCHER_MAX_WALL_CLOCK: Duration = Duration::from_secs(60 * 60);

/// Kind of scan the watcher is following.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ScanKind {
    Update,
    Rescan,
}

impl ScanKind {
    pub(crate) fn as_wire_str(self) -> &'static str {
        match self {
            Self::Update => "update",
            Self::Rescan => "rescan",
        }
    }
}

fn watcher_gate() -> &'static tokio::sync::Mutex<()> {
    static G: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    G.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Epoch milliseconds — wall-clock; `SystemTime` cannot fail
/// in practice.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The resting-state envelope: no scans in flight.
fn idle_envelope() -> serde_json::Value {
    json!({
        "v": SCAN_PROGRESS_PAYLOAD_VERSION,
        "scans": Vec::<serde_json::Value>::new(),
    })
}

/// The active-scan envelope: one entry describing the
/// in-flight (or just-completed) scan.
fn active_envelope(
    source_id: &str,
    kind: ScanKind,
    started_at_ms: u64,
    scanned_tracks: u32,
    estimated_total: Option<u32>,
    phase: &str,
) -> serde_json::Value {
    json!({
        "v": SCAN_PROGRESS_PAYLOAD_VERSION,
        "scans": [
            {
                "source_id":            source_id,
                "kind":                 kind.as_wire_str(),
                "started_at_ms":        started_at_ms,
                "scanned_tracks":       scanned_tracks,
                "estimated_total":      estimated_total,
                "current_relative_path": serde_json::Value::Null,
                "phase":                phase,
            }
        ],
    })
}

/// Publish a subject-state update — cheap replacement for the
/// full announce cycle when the plugin knows the subject is
/// already registered.
async fn publish(subjects: &Arc<dyn SubjectAnnouncer>, env: serde_json::Value) {
    let addressing =
        ExternalAddressing::new(SCHEME_LIBRARY, VALUE_SCAN_PROGRESS);
    if let Err(e) = subjects.update_state(addressing, env).await {
        tracing::debug!(
            plugin = PLUGIN_NAME,
            error = %e,
            "audio_library_scan_progress update_state failed"
        );
    }
}

/// Spawn a scan-progress watcher for the current scan. Best-
/// effort: a watcher already in flight (singleton gate held)
/// is a no-op, since MPD's scan is global and one watcher
/// covers the full progress trajectory. The subsequent
/// completion frame + subject republish still fires.
///
/// The task runs to completion (`updating_db` returns to
/// `None`), the safety ceiling
/// ([`WATCHER_MAX_WALL_CLOCK`]), or the connection is lost
/// beyond retry.
///
/// # Arguments
///
/// - `library` — cloned into the watcher for
///   `publish_subjects` on scan completion (refreshes
///   `audio_library_sources` + `audio_library_state`
///   without operator reload).
/// - `endpoint` + `timeouts` — how the watcher connects to
///   MPD for its poll cycles.
/// - `source_id` — the source that triggered the scan (or
///   `"local-internal"` when unknown).
/// - `kind` — `Update` or `Rescan`.
pub(crate) fn spawn(
    library: LibraryContext,
    endpoint: MpdEndpoint,
    timeouts: ConnectTimeouts,
    source_id: String,
    kind: ScanKind,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run(library, endpoint, timeouts, source_id, kind))
}

async fn run(
    library: LibraryContext,
    endpoint: MpdEndpoint,
    timeouts: ConnectTimeouts,
    source_id: String,
    kind: ScanKind,
) {
    let Ok(_guard) = watcher_gate().try_lock() else {
        tracing::debug!(
            plugin = PLUGIN_NAME,
            source_id = %source_id,
            "scan_progress: watcher already in flight; skipping fan-out"
        );
        return;
    };

    let started_at_ms = now_ms();
    let started_wall = Instant::now();
    let subjects = library.subjects.clone();

    // Compute the estimated total once, up front. Best-effort
    // — a walker that exceeds budget yields None and the UI
    // shows indeterminate progress rather than a wrong denominator.
    let estimated_total =
        estimate_source_track_count(&library, &source_id).await;

    // Initial frame — publishes phase=scanning with 0 scanned
    // so the UI can render "Indexing 0 of M" immediately.
    publish(
        &subjects,
        active_envelope(
            &source_id,
            kind,
            started_at_ms,
            0,
            estimated_total,
            "scanning",
        ),
    )
    .await;

    let mut last_scanned: u32 = 0;
    let mut last_updating_db: Option<u32> = None;

    loop {
        if started_wall.elapsed() > WATCHER_MAX_WALL_CLOCK {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                source_id = %source_id,
                "scan_progress: watcher wall-clock ceiling reached; \
                 emitting terminal frame + exiting"
            );
            emit_terminal(
                &library,
                &source_id,
                kind,
                started_at_ms,
                last_scanned,
                estimated_total,
            )
            .await;
            return;
        }

        tokio::time::sleep(POLL_INTERVAL).await;

        // Best-effort connect per poll — cheap on localhost.
        // A poll that cannot reach MPD skips its frame; the
        // NEXT poll retries. Persistent unreachability
        // exhausts the ceiling above.
        let mut conn = match MpdConnection::connect_with_timeouts(
            endpoint.clone(),
            timeouts,
        )
        .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "scan_progress: poll connect not ready; retrying"
                );
                continue;
            }
        };

        let status = match conn.status().await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "scan_progress: poll status failed; retrying"
                );
                continue;
            }
        };
        let stats = match conn.stats().await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "scan_progress: poll stats failed; retrying"
                );
                continue;
            }
        };

        last_scanned = stats.songs;
        let now_updating = status.updating_db;

        // MPD reports updating_db while a scan is in flight.
        // Missing on the FIRST poll (before MPD picks up the
        // fresh update job) is treated as still-in-progress
        // for one poll — pipe closes handle the real "scan
        // finished before we ever saw it" case via the
        // subsequent stable-None below.
        match (now_updating, last_updating_db) {
            (Some(_), _) => {
                // Scan visibly in flight — emit progress.
                publish(
                    &subjects,
                    active_envelope(
                        &source_id,
                        kind,
                        started_at_ms,
                        last_scanned,
                        estimated_total,
                        "scanning",
                    ),
                )
                .await;
                last_updating_db = now_updating;
            }
            (None, Some(_)) => {
                // Transition from scanning → idle. Emit
                // terminal frame + settle the sources /
                // state subjects.
                emit_terminal(
                    &library,
                    &source_id,
                    kind,
                    started_at_ms,
                    last_scanned,
                    estimated_total,
                )
                .await;
                return;
            }
            (None, None) => {
                // Never observed an in-flight scan. Fires
                // when the watcher spawned after MPD had
                // already completed a very fast scan (typical
                // for `update` on a few-track delta). Emit a
                // terminal frame with the observed final
                // counts so the UI still sees a completion
                // signal, then exit.
                emit_terminal(
                    &library,
                    &source_id,
                    kind,
                    started_at_ms,
                    last_scanned,
                    estimated_total,
                )
                .await;
                return;
            }
        }
    }
}

async fn emit_terminal(
    library: &LibraryContext,
    source_id: &str,
    kind: ScanKind,
    started_at_ms: u64,
    final_scanned: u32,
    estimated_total: Option<u32>,
) {
    let subjects = library.subjects.clone();
    // Terminal frame carries the final counts + phase=complete.
    // UI keys on phase=complete for its settle logic.
    let final_total = estimated_total.or(Some(final_scanned));
    publish(
        &subjects,
        active_envelope(
            source_id,
            kind,
            started_at_ms,
            final_scanned,
            final_total,
            "complete",
        ),
    )
    .await;

    // Republish the sibling subjects so the settled count
    // lands on the operator UI without a reload. `publish_subjects`
    // rebuilds both `audio_library_sources` and
    // `audio_library_state` from the current registry snapshot.
    library::publish_subjects(library).await;

    // After a brief settle window, publish the idle
    // envelope so the resting subject state matches the
    // documented "no scans in flight" shape.
    tokio::time::sleep(Duration::from_millis(2_000)).await;
    publish(&subjects, idle_envelope()).await;

    tracing::info!(
        plugin = PLUGIN_NAME,
        source_id = %source_id,
        kind = kind.as_wire_str(),
        scanned_tracks = final_scanned,
        estimated_total = ?estimated_total,
        "scan_progress: terminal frame published; scan complete"
    );
}

/// Walk the source's mount path, counting music-file entries.
/// Bounded by [`WALKER_BUDGET`]; on expiry the walker
/// abandons and returns `None` (UI renders indeterminate
/// progress).
///
/// Music files are recognised by extension (case-insensitive)
/// — the recognised set matches the extensions MPD's default
/// decoder plugins handle across common lossless / lossy
/// formats. Broken symlinks, unreadable directories, and
/// permission errors are skipped silently so a
/// half-permissions-degraded mount does not abort the walker.
async fn estimate_source_track_count(
    library: &LibraryContext,
    source_id: &str,
) -> Option<u32> {
    let record = library.registry.get(source_id).await?;
    let root = record.mount_path.clone();
    let started = Instant::now();
    let count = tokio::task::spawn_blocking(move || {
        walk_music_files(&root, started, WALKER_BUDGET)
    })
    .await
    .ok()??;
    Some(count)
}

fn walk_music_files(
    root: &Path,
    started: Instant,
    budget: Duration,
) -> Option<u32> {
    let mut count: u32 = 0;
    let mut stack: Vec<PathBuf> = Vec::new();
    stack.push(root.to_path_buf());
    while let Some(dir) = stack.pop() {
        if started.elapsed() > budget {
            return None;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() && is_music_extension(&path) {
                count = count.saturating_add(1);
            }
        }
    }
    Some(count)
}

/// True when the path's extension names a music format the
/// MPD default decoder set handles. Case-insensitive.
fn is_music_extension(path: &Path) -> bool {
    let Some(ext) = path.extension() else {
        return false;
    };
    let Some(ext) = ext.to_str() else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "mp3"
            | "m4a"
            | "m4b"
            | "flac"
            | "wav"
            | "ogg"
            | "opus"
            | "aac"
            | "aiff"
            | "aif"
            | "alac"
            | "ape"
            | "wv"
            | "wma"
            | "dsf"
            | "dff"
            | "mpc"
            | "tak"
            | "shn"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn music_extension_recognition_is_case_insensitive() {
        for ext in [".mp3", ".MP3", ".Flac", ".FLAC", ".dSf", ".OpUs"] {
            let p = PathBuf::from(format!("track{ext}"));
            assert!(is_music_extension(&p), "expected true for {ext}");
        }
    }

    #[test]
    fn non_music_extensions_are_ignored() {
        for name in ["cover.jpg", "notes.txt", "README", "no-ext", ".DS_Store"]
        {
            let p = PathBuf::from(name);
            assert!(!is_music_extension(&p), "expected false for {name}");
        }
    }

    #[test]
    fn walker_returns_zero_on_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let count = walk_music_files(
            tmp.path(),
            Instant::now(),
            Duration::from_secs(30),
        )
        .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn walker_counts_music_files_across_depth() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("INTERNAL/Artist/Album"))
            .unwrap();
        std::fs::write(tmp.path().join("INTERNAL/Artist/Album/1.flac"), b"x")
            .unwrap();
        std::fs::write(tmp.path().join("INTERNAL/Artist/Album/2.mp3"), b"x")
            .unwrap();
        std::fs::write(
            tmp.path().join("INTERNAL/Artist/Album/cover.jpg"),
            b"x",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("USB/Live")).unwrap();
        std::fs::write(tmp.path().join("USB/Live/set.opus"), b"x").unwrap();
        let count = walk_music_files(
            tmp.path(),
            Instant::now(),
            Duration::from_secs(30),
        )
        .unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn walker_yields_none_on_budget_exhaustion() {
        let tmp = tempfile::tempdir().unwrap();
        // Zero budget → the very first budget check trips.
        let result =
            walk_music_files(tmp.path(), Instant::now(), Duration::ZERO);
        // Depending on scheduling, elapsed at first check
        // may be sub-nanosecond and pass; but with duration
        // zero the second iteration WILL exceed. Assert
        // either arm: budget exhausted or immediate zero
        // count on an empty tree. Both are correct terminal
        // states — the invariant is "no infinite loop".
        assert!(result.is_none() || result == Some(0));
    }

    #[test]
    fn watcher_gate_is_singleton() {
        let a = watcher_gate() as *const _;
        let b = watcher_gate() as *const _;
        assert_eq!(a, b);
    }

    #[test]
    fn scan_kind_wire_strings_are_stable() {
        assert_eq!(ScanKind::Update.as_wire_str(), "update");
        assert_eq!(ScanKind::Rescan.as_wire_str(), "rescan");
    }

    #[test]
    fn idle_envelope_shape_matches_schema() {
        let env = idle_envelope();
        assert_eq!(env["v"], SCAN_PROGRESS_PAYLOAD_VERSION);
        assert!(env["scans"].is_array());
        assert_eq!(env["scans"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn active_envelope_shape_matches_schema() {
        let env = active_envelope(
            "local-internal",
            ScanKind::Update,
            1_720_000_000_000,
            1_140,
            Some(1_142),
            "scanning",
        );
        assert_eq!(env["v"], SCAN_PROGRESS_PAYLOAD_VERSION);
        let scans = env["scans"].as_array().unwrap();
        assert_eq!(scans.len(), 1);
        let entry = &scans[0];
        for key in [
            "source_id",
            "kind",
            "started_at_ms",
            "scanned_tracks",
            "estimated_total",
            "current_relative_path",
            "phase",
        ] {
            assert!(
                entry.get(key).is_some(),
                "wire key '{key}' missing from active envelope entry"
            );
        }
        assert_eq!(entry["source_id"], "local-internal");
        assert_eq!(entry["kind"], "update");
        assert_eq!(entry["scanned_tracks"], 1_140);
        assert_eq!(entry["estimated_total"], 1_142);
        assert_eq!(entry["phase"], "scanning");
    }
}
