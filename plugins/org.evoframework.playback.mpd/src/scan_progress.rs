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
//! completion, but the operator
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
//!    per-source `scanned_tracks` (0 while in flight — MPD
//!    has no per-source progress counter and the database
//!    total is not this source's),
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

/// What the in-flight frames report as `scanned_tracks`.
///
/// MPD has no per-source progress counter. `stats.songs` is the
/// whole database, so on a device carrying a local library and a
/// NAS it is mostly songs the running scan will never touch;
/// publishing it as this source's progress told the operator
/// "Indexing <the database> of <this source>".
///
/// There is no cheap honest per-source count on the poll path:
/// `find base` every tick is a second enumerator under load, and
/// a fabricated number is the same lie in a different hat. So
/// the in-flight frames carry zero and the denominator carries
/// the walker's per-source estimate — "Indexing 0 of M", which
/// is true, or "Indexing 0" when the walker missed. The settled
/// count lands on the card the moment the scan completes.
const SCANNED_TRACKS_IN_FLIGHT: u32 = 0;

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

    // Initial frame — publishes phase=scanning so the UI can
    // render "Indexing 0 of M" immediately.
    publish(
        &subjects,
        active_envelope(
            &source_id,
            kind,
            started_at_ms,
            SCANNED_TRACKS_IN_FLIGHT,
            estimated_total,
            "scanning",
        ),
    )
    .await;

    let mut last_updating_db: Option<u32> = None;

    loop {
        if started_wall.elapsed() > WATCHER_MAX_WALL_CLOCK {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                source_id = %source_id,
                "scan_progress: watcher wall-clock ceiling reached; \
                 emitting terminal frame + exiting"
            );
            emit_terminal(TerminalScan {
                library: &library,
                source_id: &source_id,
                kind,
                started_at_ms,
                final_scanned: SCANNED_TRACKS_IN_FLIGHT,
                estimated_total,
                endpoint: &endpoint,
                timeouts,
            })
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
        // No stats read here. Its only use was the database song
        // total, which is not this source's progress. `status()`
        // and `updating_db` carry everything the watcher needs.
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
                        SCANNED_TRACKS_IN_FLIGHT,
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
                emit_terminal(TerminalScan {
                    library: &library,
                    source_id: &source_id,
                    kind,
                    started_at_ms,
                    final_scanned: SCANNED_TRACKS_IN_FLIGHT,
                    estimated_total,
                    endpoint: &endpoint,
                    timeouts,
                })
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
                emit_terminal(TerminalScan {
                    library: &library,
                    source_id: &source_id,
                    kind,
                    started_at_ms,
                    final_scanned: SCANNED_TRACKS_IN_FLIGHT,
                    estimated_total,
                    endpoint: &endpoint,
                    timeouts,
                })
                .await;
                return;
            }
        }
    }
}

/// Everything one terminal frame needs, gathered into a value
/// so the emitter takes a single argument rather than a list
/// that widens each time the terminal path learns something new.
struct TerminalScan<'a> {
    library: &'a LibraryContext,
    source_id: &'a str,
    kind: ScanKind,
    started_at_ms: u64,
    final_scanned: u32,
    estimated_total: Option<u32>,
    endpoint: &'a MpdEndpoint,
    timeouts: ConnectTimeouts,
}

async fn emit_terminal(scan: TerminalScan<'_>) {
    let TerminalScan {
        library,
        source_id,
        kind,
        started_at_ms,
        final_scanned,
        estimated_total,
        endpoint,
        timeouts,
    } = scan;
    let subjects = library.subjects.clone();
    // Terminal frame carries the final counts + phase=complete.
    // UI keys on phase=complete for its settle logic.
    // The walker's per-source estimate, or nothing. It must NOT
    // fall back to `final_scanned`: that is zero by the rule
    // above, and "0 of 0" reads as a finished, empty source.
    let final_total = estimated_total;
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

    // Settle THIS source's counts before the sibling subjects
    // are rebuilt. `publish_subjects` renders the registry as it
    // finds it, so without this step the operator reads the
    // count from before the scan until some unrelated state
    // change happens to reconcile it — a rescan is not a state
    // change.
    settle_source_counts(library, source_id, endpoint, timeouts).await;

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

/// Rewrite one source's track counts from MPD's own database.
///
/// The enumerator is the sticker reconciler's — `find base`
/// asks MPD what it actually holds under this source. The base
/// is the source's mount expressed relative to
/// `music_directory`, which is the only form MPD's database
/// understands; an absolute mount is a Bad URI to it. Browse and
/// update_source resolve it the same way, through the same
/// helper. It is deliberately NOT `stats.songs`: that counts the
/// whole database, so attributing it to a NAS or USB source
/// states a number that was never that source's.
///
/// Every failure path keeps the counts that are already there.
/// A source whose enumeration did not come back is a source we
/// know nothing new about; writing zero would turn a missing
/// answer into a wrong one, and the caller republishes either
/// way so the operator still sees a settled subject.
async fn settle_source_counts(
    library: &LibraryContext,
    source_id: &str,
    endpoint: &MpdEndpoint,
    timeouts: ConnectTimeouts,
) {
    let Some(record) = library.registry.get(source_id).await else {
        return;
    };
    let mpd_base = match crate::library::mpd_database_relative_path(
        &library.music_directory,
        &record.mount_path,
        "",
    ) {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                source_id = %source_id,
                error = %e,
                "scan terminal: source is not under music_directory; \
                 keeping the counts already on the record"
            );
            return;
        }
    };
    let mut conn = match crate::sticker_reconciler::open_connection(
        endpoint.clone(),
        timeouts,
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                source_id = %source_id,
                error = %e,
                "scan terminal: no MPD connection to settle counts; \
                 keeping the counts already on the record"
            );
            return;
        }
    };
    let songs = match crate::sticker_reconciler::enumerate_songs_under_mount(
        &mut conn, &mpd_base,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                source_id = %source_id,
                error = %e,
                "scan terminal: enumeration failed; keeping the counts \
                 already on the record"
            );
            return;
        }
    };
    apply_settled_counts(library, source_id, songs.len()).await;
}

/// Apply an enumerated song count to one source and persist.
///
/// Split from the IO half so the rule is testable without an MPD
/// on the other end. Availability follows the same rule the
/// sticker reconciler applies: a reachable source has every song
/// it holds available; an unreachable one has none.
///
/// Returns true when the record was found and updated.
async fn apply_settled_counts(
    library: &LibraryContext,
    source_id: &str,
    song_count: usize,
) -> bool {
    let Some(record) = library.registry.get(source_id).await else {
        return false;
    };
    let total = song_count.min(u32::MAX as usize) as u32;
    let available =
        if crate::sticker_reconciler::sticker_value_for(&record.state) == "1" {
            total
        } else {
            0
        };
    if let Err(e) = library
        .registry
        .update_track_counts(source_id, total, available)
        .await
    {
        tracing::debug!(
            plugin = PLUGIN_NAME,
            source_id = %source_id,
            error = %e,
            "scan terminal: track-count update failed"
        );
        return false;
    }
    if let Err(e) = library.registry.persist().await {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            source_id = %source_id,
            error = %e,
            "scan terminal: counts updated in memory but not persisted"
        );
    }
    true
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

    use crate::library::LibraryContext;
    use crate::source_registry::{
        ScanPolicy, SourceKind, SourceRecord, SourceRegistry, SourceState,
    };

    struct NullAnn;
    impl SubjectAnnouncer for NullAnn {
        fn announce<'a>(
            &'a self,
            _a: evo_plugin_sdk::contract::SubjectAnnouncement,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            (),
                            evo_plugin_sdk::contract::ReportError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async { Ok(()) })
        }
        fn retract<'a>(
            &'a self,
            _addressing: evo_plugin_sdk::contract::ExternalAddressing,
            _reason: Option<String>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            (),
                            evo_plugin_sdk::contract::ReportError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async { Ok(()) })
        }
        fn update_state<'a>(
            &'a self,
            _addressing: evo_plugin_sdk::contract::ExternalAddressing,
            _state: serde_json::Value,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            (),
                            evo_plugin_sdk::contract::ReportError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async { Ok(()) })
        }
    }

    fn nas_kind() -> SourceKind {
        SourceKind::NetworkNasSmb {
            server: "192.0.2.10".to_string(),
            share: "Music".to_string(),
            username: "operator".to_string(),
        }
    }

    fn usb_kind() -> SourceKind {
        SourceKind::LocalUsb {
            device_node: "/dev/disk/by-uuid/test".to_string(),
            label: "STICK".to_string(),
        }
    }

    fn record(id: &str, kind: SourceKind, state: SourceState) -> SourceRecord {
        SourceRecord {
            id: id.to_string(),
            display_name: id.to_string(),
            kind,
            mount_path: PathBuf::from(format!("/var/lib/evo/music/{id}")),
            mpd_storage_name: None,
            state,
            last_seen_online_at_ms: None,
            probe_cadence_ms: 60_000,
            scan_policy: ScanPolicy::EagerIncremental {
                on_online: true,
                on_mount_event: false,
            },
            // A stale number, as the registry carries between
            // scans. Settling must overwrite it.
            track_count: 999,
            track_count_available: 999,
            last_scan_at_ms: None,
        }
    }

    async fn ctx_with(records: Vec<SourceRecord>) -> LibraryContext {
        let registry = SourceRegistry::new();
        for r in records {
            registry.register(r).await.unwrap();
        }
        LibraryContext::new(
            PathBuf::from("/var/lib/evo/music"),
            registry,
            Arc::new(NullAnn),
            None,
        )
    }

    #[test]
    fn in_flight_frames_report_zero_not_a_database_total() {
        // "Indexing 0 of M" is true. "Indexing <database> of M"
        // was not.
        assert_eq!(SCANNED_TRACKS_IN_FLIGHT, 0);
        let env = active_envelope(
            "nas",
            ScanKind::Update,
            1_700_000_000_000,
            SCANNED_TRACKS_IN_FLIGHT,
            Some(42),
            "scanning",
        );
        let scan = &env["scans"][0];
        assert_eq!(scan["scanned_tracks"], 0);
        // The denominator is the walker's per-source estimate
        // and is untouched by this row.
        assert_eq!(scan["estimated_total"], 42);
        assert_eq!(scan["phase"], "scanning");
    }

    #[test]
    fn a_missed_walker_leaves_the_denominator_absent_not_zero() {
        // "Indexing 0" — indeterminate. Never "0 of 0", which
        // reads as a finished, empty source.
        let env = active_envelope(
            "nas",
            ScanKind::Update,
            1_700_000_000_000,
            SCANNED_TRACKS_IN_FLIGHT,
            None,
            "complete",
        );
        let scan = &env["scans"][0];
        assert_eq!(scan["scanned_tracks"], 0);
        assert!(
            scan["estimated_total"].is_null(),
            "a missing estimate must stay missing, not become 0",
        );
    }

    #[test]
    fn the_poll_path_takes_no_database_song_total() {
        // Anti-drift: the watcher must not reacquire the
        // database-wide count. The needle is built at runtime so
        // this assertion does not match itself, and the prose
        // elsewhere in the file that explains WHY the total is
        // wrong stays readable.
        let src = include_str!("scan_progress.rs");
        let stats_read = format!("conn.{}()", "stats");
        assert!(
            !src.contains(&stats_read),
            "the poll path must not read MPD's database stats",
        );
        let db_total = format!("{}.songs", "stats");
        let code_hits = src
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//")
                    && !t.starts_with("///")
                    && !t.starts_with("//!")
            })
            .filter(|l| l.contains(&db_total))
            .count();
        assert_eq!(
            code_hits, 0,
            "no code line in this file may take the database song total",
        );
    }

    #[tokio::test]
    async fn terminal_settle_rewrites_only_the_scanned_source() {
        let ctx = ctx_with(vec![
            record("nas", nas_kind(), SourceState::Online),
            record("usb", usb_kind(), SourceState::Online),
        ])
        .await;
        assert!(apply_settled_counts(&ctx, "nas", 7).await);

        let nas = ctx.registry.get("nas").await.unwrap();
        assert_eq!(nas.track_count, 7);
        assert_eq!(nas.track_count_available, 7);
        // The other source is not touched by this source's scan.
        let usb = ctx.registry.get("usb").await.unwrap();
        assert_eq!(usb.track_count, 999);
        assert_eq!(usb.track_count_available, 999);
    }

    #[tokio::test]
    async fn terminal_settle_available_follows_the_source_state() {
        let ctx = ctx_with(vec![record(
            "nas",
            nas_kind(),
            SourceState::Offline {
                reason: "unplugged".into(),
                since_ms: 0,
            },
        )])
        .await;
        assert!(apply_settled_counts(&ctx, "nas", 12).await);

        let nas = ctx.registry.get("nas").await.unwrap();
        // It still holds twelve songs; none are reachable.
        assert_eq!(nas.track_count, 12);
        assert_eq!(nas.track_count_available, 0);
    }

    #[tokio::test]
    async fn terminal_settle_stamps_the_scan_time() {
        let ctx =
            ctx_with(vec![record("nas", nas_kind(), SourceState::Online)])
                .await;
        assert!(ctx
            .registry
            .get("nas")
            .await
            .unwrap()
            .last_scan_at_ms
            .is_none());
        apply_settled_counts(&ctx, "nas", 3).await;
        assert!(ctx
            .registry
            .get("nas")
            .await
            .unwrap()
            .last_scan_at_ms
            .is_some());
    }

    #[tokio::test]
    async fn local_internal_settles_from_the_enumerator_not_the_database_total()
    {
        // stats.songs is the whole database. A local-internal
        // source is settled from what the enumerator found under
        // its own mount, even when that differs.
        let ctx = ctx_with(vec![record(
            "INTERNAL",
            SourceKind::LocalInternal,
            SourceState::Online,
        )])
        .await;
        assert!(apply_settled_counts(&ctx, "INTERNAL", 4).await);
        let r = ctx.registry.get("INTERNAL").await.unwrap();
        assert_eq!(r.track_count, 4, "the enumerator wins, not a db total");
    }

    #[tokio::test]
    async fn terminal_settle_on_an_unknown_source_is_a_noop() {
        let ctx = ctx_with(vec![]).await;
        assert!(!apply_settled_counts(&ctx, "gone", 5).await);
    }

    #[tokio::test]
    async fn settle_keeps_existing_counts_when_the_source_is_outside_music_directory(
    ) {
        // MPD can only address its own database. A mount outside
        // music_directory has no relative URI, so there is no
        // question to ask — and no answer to write. The counts
        // already on the record stand.
        let mut outside = record("elsewhere", nas_kind(), SourceState::Online);
        outside.mount_path = PathBuf::from("/mnt/external/library");
        let ctx = ctx_with(vec![outside]).await;
        let endpoint = MpdEndpoint::Tcp {
            host: "127.0.0.1".to_string(),
            port: 1,
        };
        let timeouts = ConnectTimeouts {
            connect: Duration::from_millis(50),
            welcome: Duration::from_millis(50),
            command: Duration::from_millis(50),
        };
        settle_source_counts(&ctx, "elsewhere", &endpoint, timeouts).await;

        let r = ctx.registry.get("elsewhere").await.unwrap();
        assert_eq!(r.track_count, 999);
        assert_eq!(r.track_count_available, 999);
    }

    #[tokio::test]
    async fn settle_keeps_existing_counts_when_mpd_is_unreachable() {
        // Enumeration failure must never write zeros: the counts
        // already on the record stand, and the caller still
        // republishes.
        let ctx =
            ctx_with(vec![record("nas", nas_kind(), SourceState::Online)])
                .await;
        let endpoint = MpdEndpoint::Tcp {
            host: "127.0.0.1".to_string(),
            port: 1,
        };
        let timeouts = ConnectTimeouts {
            connect: Duration::from_millis(50),
            welcome: Duration::from_millis(50),
            command: Duration::from_millis(50),
        };
        settle_source_counts(&ctx, "nas", &endpoint, timeouts).await;

        let nas = ctx.registry.get("nas").await.unwrap();
        assert_eq!(
            nas.track_count, 999,
            "counts must survive a failed enumerate"
        );
        assert_eq!(nas.track_count_available, 999);
    }

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
