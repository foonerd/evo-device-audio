// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Library triage — operator-visible drift detection + reconcile.
//!
//! # Purpose
//!
//! A wipe, a mount loss, a stale cache — each leaves a
//! fingerprint on the library plane: MPD's tag_cache disagrees
//! with files-on-disk; the SourceRegistry's persisted track
//! counts disagree with wire truth; the queue / favourites /
//! stored playlists carry URIs that no longer resolve.
//! Individually each drift class fires reactively somewhere
//! else (rehydrate, [`crate::gone_curation`], sticker
//! reconciler). Collectively they must be *visible* to the
//! operator — a single surface that reports what drifted, what
//! was reconciled automatically, and what still needs a
//! gesture.
//!
//! # Design
//!
//! - Publishes the `audio_library_triage` subject on every
//!   sweep.
//! - Runs on plugin warm-start AND on every idle
//!   `Database` / `Update` burst. Every wave-1 drift class is
//!   safe to auto-reconcile; the `auto_reconciled` flag on each
//!   finding records what happened so the operator sees the
//!   history without needing to fire the verb.
//! - Verb `library.get_triage` returns the last-published
//!   snapshot with zero side effects.
//! - Verb `library.reconcile_triage` re-runs the sweep + fires
//!   every reconcile action + republishes the subject.
//!
//! # Drift classes (wave 1)
//!
//! | Class | Detection | Reconcile | Auto |
//! | --- | --- | --- | --- |
//! | `mpd_db_empty_but_music_present` | `music_directory` has children on disk AND `listallinfo("")` reports no File entries | `conn.update(None)` | yes |
//! | `registry_count_diverges` | Floor source `track_count` ≠ MPD `stats.songs` | `apply_track_counts_with_scan_time` + `persist` + `library::publish_subjects` | yes |
//! | `curation_carries_gone_uris` | ≥1 gone URI in queue / favourites / stored playlists under an Online/Degraded source | [`crate::gone_curation::prune_gone_from_curation`] | yes |
//!
//! Extension classes (per-source count divergence, USB-plane
//! stale mount, DLNA identity staleness, network-share mount
//! loss) land as new arms as their substrates come online.
//! The classifier + subject shape are ready — every new class
//! adds one arm here and one row in `LIBRARY-TRIAGE.md`.
//!
//! # Concurrency
//!
//! A process-wide gate serialises concurrent triage passes so
//! two idle bursts cannot interleave (the reconcile actions
//! themselves — `mpc update`, registry apply+persist, gone
//! prune — are each idempotent, but the finding-list mutex is
//! the operator-visible record and must not race).

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::json;

use evo_plugin_sdk::contract::{
    ExternalAddressing, SubjectAnnouncement, SubjectAnnouncer,
};

use crate::favourites::FavouritesContext;
use crate::gone_curation;
use crate::library::{self, LibraryContext};
use crate::mpd::{MpdConnection, MpdLibraryEntry};
use crate::playlist::PlaylistContext;
use crate::queue::QueueContext;
use crate::source_registry::{SourceRegistry, LOCAL_INTERNAL_SOURCE_ID};

const PLUGIN_NAME: &str = "org.evoframework.playback.mpd";
const SUBJECT_TYPE_TRIAGE: &str = "audio_library_triage";
const SCHEME_LIBRARY: &str = "evo.audio.library";
const VALUE_TRIAGE: &str = "triage";
pub(crate) const TRIAGE_PAYLOAD_VERSION: u32 = 1;

/// Stable operator-visible key for one drift class. Every entry
/// here has a matching row in
/// `plugins/org.evoframework.playback.mpd/docs/LIBRARY-TRIAGE.md`.
pub(crate) mod class {
    pub(crate) const MPD_DB_EMPTY_BUT_MUSIC_PRESENT: &str =
        "mpd_db_empty_but_music_present";
    pub(crate) const REGISTRY_COUNT_DIVERGES: &str = "registry_count_diverges";
    pub(crate) const CURATION_CARRIES_GONE_URIS: &str =
        "curation_carries_gone_uris";
}

/// One drift finding surfaced to the operator + audit log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct TriageFinding {
    /// Stable machine-readable key — one of the [`class`]
    /// constants. UIs key on this to render the correct row.
    pub(crate) class: &'static str,
    /// `info` | `warn` | `critical`. Wave-1 classes are all
    /// `warn` — auto-reconciled, but the drift itself is
    /// operationally significant.
    pub(crate) severity: &'static str,
    /// Human-readable one-line summary for the operator UI.
    pub(crate) description: String,
    /// True when this pass reconciled the drift automatically.
    /// False when the sweep detected the class but reconcile
    /// failed or is not safe to fire without an operator
    /// gesture (no wave-1 classes fall here).
    pub(crate) auto_reconciled: bool,
    /// Wall-clock ms when the reconcile completed. `None` when
    /// `auto_reconciled` is false.
    pub(crate) reconciled_at_ms: Option<u64>,
    /// Class-specific evidence blob. Never operator-actionable
    /// on its own — feeds diagnostics + the audit trail.
    pub(crate) evidence: serde_json::Value,
}

/// Sweep + reconcile context. Cheaply cloneable — every field
/// is either Clone-derived or Arc-wrapped internally. The
/// bundle keeps one canonical instance; the verb dispatcher
/// borrows it read-only.
#[derive(Clone)]
pub(crate) struct TriageContext {
    pub(crate) subjects: Arc<dyn SubjectAnnouncer>,
    pub(crate) music_directory: PathBuf,
    pub(crate) registry: SourceRegistry,
    /// Cloned from the shared LibraryContext so
    /// `library::publish_subjects` after a count reconcile
    /// updates the same shared mirrors. Every field on
    /// LibraryContext is Arc-wrapped, so the clone shares
    /// state, not just a copy.
    pub(crate) library: LibraryContext,
    /// Findings from the last sweep — read by `get_triage`,
    /// mutated by every `run_triage`.
    pub(crate) findings: Arc<tokio::sync::Mutex<Vec<TriageFinding>>>,
    /// Wall-clock ms of the last sweep completion.
    pub(crate) last_run_at_ms: Arc<tokio::sync::Mutex<u64>>,
}

impl TriageContext {
    pub(crate) fn new(
        subjects: Arc<dyn SubjectAnnouncer>,
        music_directory: PathBuf,
        registry: SourceRegistry,
        library: LibraryContext,
    ) -> Self {
        Self {
            subjects,
            music_directory,
            registry,
            library,
            findings: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            last_run_at_ms: Arc::new(tokio::sync::Mutex::new(0)),
        }
    }
}

fn triage_gate() -> &'static tokio::sync::Mutex<()> {
    static G: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    G.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Epoch milliseconds. Wall-clock; `SystemTime` cannot fail in
/// practice (would require the system clock to be pre-1970).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Announce the `audio_library_triage` subject with the
/// current envelope. Call at plugin load AND at the end of
/// every sweep — the framework upserts on repeat announces.
pub(crate) async fn announce_triage(ctx: &TriageContext) {
    let env = render_envelope(ctx).await;
    let announcement = SubjectAnnouncement::new(
        SUBJECT_TYPE_TRIAGE,
        vec![ExternalAddressing::new(SCHEME_LIBRARY, VALUE_TRIAGE)],
    )
    .with_state(env);
    if let Err(e) = ctx.subjects.announce(announcement).await {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            error = %e,
            "audio_library_triage subject announce failed"
        );
    }
}

async fn render_envelope(ctx: &TriageContext) -> serde_json::Value {
    let findings = ctx.findings.lock().await.clone();
    let last_run_at_ms = *ctx.last_run_at_ms.lock().await;
    envelope_from_parts(&findings, last_run_at_ms)
}

/// Pure envelope build — kept factored out so unit tests can
/// assert on the wire shape without constructing a full
/// TriageContext.
fn envelope_from_parts(
    findings: &[TriageFinding],
    last_run_at_ms: u64,
) -> serde_json::Value {
    let auto_reconciled_count =
        findings.iter().filter(|f| f.auto_reconciled).count() as u32;
    let prompt_required_count =
        findings.iter().filter(|f| !f.auto_reconciled).count() as u32;
    json!({
        "v": TRIAGE_PAYLOAD_VERSION,
        "findings": findings,
        "auto_reconciled_count": auto_reconciled_count,
        "prompt_required_count": prompt_required_count,
        "last_run_at_ms": last_run_at_ms,
    })
}

/// Detect + reconcile every wave-1 drift class. Best-effort per
/// class: an MPD transport error on one class does not abort
/// the sweep. The final envelope is announced regardless.
///
/// Contract: this is the single entry point for every code path
/// that used to call [`gone_curation::prune_gone_from_curation`]
/// directly (warm-start + idle `Database`/`Update`). Prune is
/// now one arm inside the sweep so the operator sees it as a
/// triage finding.
pub(crate) async fn run_triage(
    ctx: &TriageContext,
    conn: &mut MpdConnection,
    queue: &QueueContext,
    playlist: &PlaylistContext,
    favourites: &FavouritesContext,
) {
    let Ok(_guard) = triage_gate().try_lock() else {
        tracing::debug!(
            plugin = PLUGIN_NAME,
            "triage skipped: another pass in flight"
        );
        return;
    };

    let mut findings: Vec<TriageFinding> = Vec::new();

    detect_mpd_db_empty_but_music_present(
        conn,
        &ctx.music_directory,
        &mut findings,
    )
    .await;
    detect_registry_count_diverges(ctx, conn, &mut findings).await;
    detect_curation_carries_gone_uris(
        ctx,
        conn,
        queue,
        playlist,
        favourites,
        &mut findings,
    )
    .await;

    {
        let mut g = ctx.findings.lock().await;
        *g = findings.clone();
    }
    {
        let mut g = ctx.last_run_at_ms.lock().await;
        *g = now_ms();
    }
    announce_triage(ctx).await;

    if !findings.is_empty() {
        let auto = findings.iter().filter(|f| f.auto_reconciled).count();
        let pending = findings.len() - auto;
        tracing::info!(
            plugin = PLUGIN_NAME,
            findings = findings.len(),
            auto_reconciled = auto,
            prompt_required = pending,
            "library triage: drift classified"
        );
    }
}

// -----------------------------------------------------------------
// class 1 — mpd_db_empty_but_music_present
// -----------------------------------------------------------------

async fn detect_mpd_db_empty_but_music_present(
    conn: &mut MpdConnection,
    music_directory: &Path,
    findings: &mut Vec<TriageFinding>,
) {
    let disk_has_children = music_directory_has_children(music_directory).await;
    if !disk_has_children {
        // Empty triad is a legitimate empty-library state, not
        // a drift class. The audit-parity wipe already reset
        // MPD state to match.
        return;
    }
    let entries = match conn.listallinfo("").await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "triage: listallinfo failed on class 1 detection"
            );
            return;
        }
    };
    let mpd_has_files = entries
        .iter()
        .any(|e| matches!(e, MpdLibraryEntry::File { .. }));
    if mpd_has_files {
        return;
    }

    let update_ok = match conn.update(None).await {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "triage: mpc update failed under class 1 reconcile"
            );
            false
        }
    };
    let reconciled_at = now_ms();
    findings.push(TriageFinding {
        class: class::MPD_DB_EMPTY_BUT_MUSIC_PRESENT,
        severity: "warn",
        description: format!(
            "Music tree under {} has content but MPD's database is empty. \
             Triggered a full library scan.",
            music_directory.display()
        ),
        auto_reconciled: update_ok,
        reconciled_at_ms: update_ok.then_some(reconciled_at),
        evidence: json!({
            "music_directory": music_directory.display().to_string(),
            "mpd_files_before": 0,
        }),
    });
}

/// True when `root` (typically `/var/lib/evo/music`) has a
/// non-empty descendant. Only the triad children (INTERNAL /
/// USB / NAS) are inspected — one level deep is enough to
/// distinguish "no music at all" from "music present but MPD
/// missed the scan".
async fn music_directory_has_children(root: &Path) -> bool {
    let mut top = match tokio::fs::read_dir(root).await {
        Ok(r) => r,
        Err(_) => return false,
    };
    while let Ok(Some(entry)) = top.next_entry().await {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Ok(mut inner) = tokio::fs::read_dir(&path).await {
            if let Ok(Some(_)) = inner.next_entry().await {
                return true;
            }
        }
    }
    false
}

// -----------------------------------------------------------------
// class 2 — registry_count_diverges
// -----------------------------------------------------------------

async fn detect_registry_count_diverges(
    ctx: &TriageContext,
    conn: &mut MpdConnection,
    findings: &mut Vec<TriageFinding>,
) {
    let stats = match conn.stats().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "triage: mpd stats failed on class 2 detection"
            );
            return;
        }
    };
    let Some(record) = ctx.registry.get(LOCAL_INTERNAL_SOURCE_ID).await else {
        return;
    };
    if record.track_count == stats.songs {
        return;
    }

    let apply_ok = ctx
        .registry
        .apply_track_counts_with_scan_time(
            LOCAL_INTERNAL_SOURCE_ID,
            stats.songs,
            stats.songs,
            stats.db_update_unix_s.map(|s| s * 1000),
        )
        .await
        .is_ok();
    let persist_ok = if apply_ok {
        ctx.registry.persist().await.is_ok()
    } else {
        false
    };
    if apply_ok && persist_ok {
        library::publish_subjects(&ctx.library).await;
    }
    let ok = apply_ok && persist_ok;
    let reconciled_at = now_ms();
    findings.push(TriageFinding {
        class: class::REGISTRY_COUNT_DIVERGES,
        severity: "warn",
        description: format!(
            "Source registry track_count for '{}' ({}) diverged from MPD wire \
             truth ({}). Reconciled to wire truth.",
            LOCAL_INTERNAL_SOURCE_ID, record.track_count, stats.songs
        ),
        auto_reconciled: ok,
        reconciled_at_ms: ok.then_some(reconciled_at),
        evidence: json!({
            "source_id": LOCAL_INTERNAL_SOURCE_ID,
            "registry_before": record.track_count,
            "wire_truth": stats.songs,
        }),
    });
}

// -----------------------------------------------------------------
// class 3 — curation_carries_gone_uris
// -----------------------------------------------------------------

async fn detect_curation_carries_gone_uris(
    ctx: &TriageContext,
    conn: &mut MpdConnection,
    queue: &QueueContext,
    playlist: &PlaylistContext,
    favourites: &FavouritesContext,
    findings: &mut Vec<TriageFinding>,
) {
    let stats = gone_curation::prune_gone_from_curation(
        conn,
        &ctx.music_directory,
        &ctx.registry,
        queue,
        playlist,
        favourites,
    )
    .await;
    let total = stats.queue_removed
        + stats.favourites_removed
        + stats.playlist_entries_removed;
    if total == 0 {
        return;
    }
    let reconciled_at = now_ms();
    findings.push(TriageFinding {
        class: class::CURATION_CARRIES_GONE_URIS,
        severity: "warn",
        description: format!(
            "{} track reference(s) in the queue, favourites, or stored \
             playlists no longer exist in the library. Pruned from curation.",
            total
        ),
        auto_reconciled: true,
        reconciled_at_ms: Some(reconciled_at),
        evidence: json!({
            "queue_removed":            stats.queue_removed,
            "favourites_removed":       stats.favourites_removed,
            "playlist_entries_removed": stats.playlist_entries_removed,
            "playlists_touched":        stats.playlists_touched,
        }),
    });
}

// -----------------------------------------------------------------
// verb handlers
// -----------------------------------------------------------------

/// `library.get_triage` — read-only snapshot of the last-run
/// sweep. Does not touch MPD. Returns the same envelope shape
/// as the subject.
pub(crate) async fn handle_get_triage(
    ctx: &TriageContext,
) -> serde_json::Value {
    render_envelope(ctx).await
}

/// `library.reconcile_triage` — re-run the full sweep + fire
/// every reconcile action + republish the subject. Returns the
/// resulting envelope.
pub(crate) async fn handle_reconcile_triage(
    ctx: &TriageContext,
    conn: &mut MpdConnection,
    queue: &QueueContext,
    playlist: &PlaylistContext,
    favourites: &FavouritesContext,
) -> serde_json::Value {
    run_triage(ctx, conn, queue, playlist, favourites).await;
    render_envelope(ctx).await
}

// -----------------------------------------------------------------
// tests
// -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn music_directory_has_children_false_on_empty_tree() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!music_directory_has_children(tmp.path()).await);
    }

    #[tokio::test]
    async fn music_directory_has_children_false_on_empty_triad() {
        let tmp = tempfile::tempdir().unwrap();
        for child in ["INTERNAL", "USB", "NAS"] {
            tokio::fs::create_dir(tmp.path().join(child)).await.unwrap();
        }
        assert!(!music_directory_has_children(tmp.path()).await);
    }

    #[tokio::test]
    async fn music_directory_has_children_true_when_any_child_populated() {
        let tmp = tempfile::tempdir().unwrap();
        for child in ["INTERNAL", "USB", "NAS"] {
            tokio::fs::create_dir(tmp.path().join(child)).await.unwrap();
        }
        tokio::fs::write(tmp.path().join("INTERNAL").join("track.flac"), b"x")
            .await
            .unwrap();
        assert!(music_directory_has_children(tmp.path()).await);
    }

    #[test]
    fn triage_gate_is_singleton() {
        let a = triage_gate() as *const _;
        let b = triage_gate() as *const _;
        assert_eq!(a, b);
    }

    #[test]
    fn class_keys_are_stable_and_snake_case() {
        for k in [
            class::MPD_DB_EMPTY_BUT_MUSIC_PRESENT,
            class::REGISTRY_COUNT_DIVERGES,
            class::CURATION_CARRIES_GONE_URIS,
        ] {
            assert!(
                k.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "class key '{k}' must be snake_case"
            );
            assert!(!k.is_empty());
        }
    }

    fn sample_finding(class: &'static str, auto: bool) -> TriageFinding {
        TriageFinding {
            class,
            severity: "warn",
            description: format!("test finding for {class}"),
            auto_reconciled: auto,
            reconciled_at_ms: auto.then_some(1_700_000_000_000),
            evidence: json!({ "class": class }),
        }
    }

    #[test]
    fn envelope_zero_findings_reports_all_counters_zero() {
        let env = envelope_from_parts(&[], 0);
        assert_eq!(env["v"], TRIAGE_PAYLOAD_VERSION);
        assert_eq!(env["findings"].as_array().unwrap().len(), 0);
        assert_eq!(env["auto_reconciled_count"], 0);
        assert_eq!(env["prompt_required_count"], 0);
        assert_eq!(env["last_run_at_ms"], 0);
    }

    #[test]
    fn envelope_counters_split_auto_vs_pending() {
        let findings = vec![
            sample_finding(class::CURATION_CARRIES_GONE_URIS, true),
            sample_finding(class::REGISTRY_COUNT_DIVERGES, true),
            sample_finding(class::MPD_DB_EMPTY_BUT_MUSIC_PRESENT, false),
        ];
        let env = envelope_from_parts(&findings, 1_720_000_000_000);
        assert_eq!(env["findings"].as_array().unwrap().len(), 3);
        assert_eq!(env["auto_reconciled_count"], 2);
        assert_eq!(env["prompt_required_count"], 1);
        assert_eq!(env["last_run_at_ms"], 1_720_000_000_000_u64);
    }

    #[test]
    fn envelope_findings_carry_operator_fields() {
        let findings =
            vec![sample_finding(class::CURATION_CARRIES_GONE_URIS, true)];
        let env = envelope_from_parts(&findings, 1);
        let entry = &env["findings"][0];
        // Every operator-visible field the UI keys on must be
        // present + serialised under a stable JSON key. This
        // pins the wire shape so refactors surface a shape
        // change at test time rather than at UI render time.
        for key in [
            "class",
            "severity",
            "description",
            "auto_reconciled",
            "reconciled_at_ms",
            "evidence",
        ] {
            assert!(entry.get(key).is_some(), "wire key '{key}' missing");
        }
        assert_eq!(entry["class"], class::CURATION_CARRIES_GONE_URIS);
        assert_eq!(entry["severity"], "warn");
        assert_eq!(entry["auto_reconciled"], true);
    }

    #[test]
    fn envelope_versioned_root_key_stable() {
        // The `v` field is the wire-payload version. Any change
        // to the envelope shape must bump the version const AND
        // extend this assertion so downstream consumers see a
        // clear signal.
        let env = envelope_from_parts(&[], 0);
        assert_eq!(env["v"], 1);
        assert_eq!(TRIAGE_PAYLOAD_VERSION, 1);
    }
}
