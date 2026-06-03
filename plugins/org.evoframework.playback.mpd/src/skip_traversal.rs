//! Skip-traversal — walks the queue past unreachable items and
//! emits coalesced [`Disposition`] records.
//!
//! Mixed-source queues + playlists are the normal case: a 100-
//! track playlist spans local + USB + NAS + cloud. When playback
//! advances to a track whose source is unreachable the device
//! MUST NOT crash (Volumio's documented failure mode); it MUST
//! skip to the next playable track + emit a typed Disposition
//! with a recovery hint. This is the load-bearing mixed-source
//! resilience contract the four operator-facing shelves rely on.
//!
//! # Algorithm
//!
//! Pre-flight + reactive split:
//!
//! - **Pre-flight** uses the source-state cache plus the
//!   per-item `available` flag the queue emitter projected
//!   from the `evo:available` sticker. Source-cached-Offline
//!   candidates skip without an MPD round-trip.
//! - **Reactive** is the authority for non-cached-Offline
//!   candidates: the warden calls
//!   [`crate::mpd::MpdConnection::play_position`] and classifies
//!   the MPD response — Ok → playing; ACK 50 → not found;
//!   ACK 53 → permission denied; ACK 55 → decoder failure;
//!   transport/timeout → pause-and-retry.
//!
//! # Coalescing
//!
//! Consecutive skips of the same `DispositionKind` from the
//! same `source_id` collapse into a single
//! [`DispositionKind::TracksSkippedRun`] with the
//! [`DispositionRun`] field carrying `{ count, from_position,
//! to_position }`. Per-item emission for runs is FORBIDDEN
//! per the `disposition-emitted-on-autonomous-decisions`
//! acceptance row. The run's recovery hint is the first
//! incident's hint (operator gesture to wake the source / etc.
//! applies to the run, not per-item).
//!
//! # Engineering bar invariant
//!
//! The traversal never leaves the warden in a half-state.
//! Every path returns one of the [`SkipOutcome`] variants;
//! no panic, no silent fall-through.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use evo_plugin_sdk::contract::{
    Disposition, DispositionAction, DispositionKind, DispositionRun,
    RecoveryHint,
};

use crate::disposition_emitter::DispositionEmitter;
use crate::mpd::{MpdConnection, MpdError};
use crate::source_registry::{SourceRegistry, SourceState};

/// One item in the queue as the skip-traversal sees it. Pre-
/// computed at queue-read time; carries the per-item
/// `available` flag the queue subject's wire envelope projects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlayableQueueItem {
    /// Zero-based queue position (MPD `Pos:`).
    pub(crate) position: u32,
    /// MPD songid (`Id:`); stable across queue reorderings
    /// within MPD's current lifetime.
    pub(crate) id: u32,
    /// MPD-relative file path or external URI for stream items.
    pub(crate) file_path: String,
    /// Library source id this item resolves under; `None` for
    /// items whose path does not match any registered source
    /// (transient stream URLs, etc.).
    pub(crate) source_id: Option<String>,
    /// Per-item availability derived from the song's
    /// `evo:available` sticker AND the resolved source's state
    /// at the queue-read snapshot. Pre-flight cache only —
    /// the reactive MPD play_position is the authority for
    /// available-flagged items.
    pub(crate) available: bool,
}

/// Outcome of one skip-traversal advance call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SkipOutcome {
    /// Playback resumed at the named position.
    Playing { position: u32 },
    /// The traversal hit a transient transport error mid-walk
    /// (broken pipe, timeout). The warden should re-attempt on
    /// the next operator gesture or autonomous advance trigger.
    /// `last_attempted` is the queue position at which the
    /// transient error surfaced.
    Paused { last_attempted: u32 },
    /// The traversal walked the entire queue past
    /// `from_position` and found nothing playable. A
    /// [`DispositionKind::QueueExhaustedNoPlayable`] has been
    /// emitted; the warden should stop transport.
    Stopped,
}

/// Skip-traversal owner. Cloneable cheaply (Arc bumps on
/// fields).
#[derive(Clone)]
pub(crate) struct SkipTraversal {
    registry: SourceRegistry,
    emitter: DispositionEmitter,
}

impl SkipTraversal {
    /// Construct a traversal handle holding shared registry +
    /// disposition emitter Arcs.
    pub(crate) fn new(
        registry: SourceRegistry,
        emitter: DispositionEmitter,
    ) -> Self {
        Self { registry, emitter }
    }

    /// Advance from `from_position` (inclusive) to the next
    /// playable queue item via the skip-traversal algorithm.
    /// `from_position == queue.len() as i64` means "advance past
    /// the end" — the algorithm immediately emits
    /// `QueueExhaustedNoPlayable` and returns `Stopped`.
    ///
    /// Negative `from_position` (caller's convention for
    /// "before the queue") is accepted and starts the walk at
    /// position 0.
    pub(crate) async fn advance_to_next_playable(
        &self,
        conn: &mut MpdConnection,
        from_position: i64,
        queue: &[PlayableQueueItem],
    ) -> SkipOutcome {
        let mut candidate: usize = if from_position < 0 {
            0
        } else {
            (from_position as usize).saturating_add(0)
        };
        let mut coalesce: Option<RunBuilder> = None;

        while candidate < queue.len() {
            let item = &queue[candidate];

            // Pre-flight: source-state cache + per-item available
            // flag from sticker.
            let source_state = self.source_state_for(&item.source_id).await;

            if !item.available && !source_state.is_reachable() {
                accumulate_or_flush(
                    &mut coalesce,
                    DispositionKind::TrackSkippedSourceOffline,
                    item.source_id.clone(),
                    item.position,
                    RecoveryHint::WakeSource {
                        source_id: item.source_id.clone().unwrap_or_default(),
                    },
                    &self.emitter,
                )
                .await;
                candidate = candidate.saturating_add(1);
                continue;
            }

            // Source pre-flight says reachable. Flush any pending
            // run before the reactive attempt.
            flush_run_if_any(&mut coalesce, &self.emitter).await;

            match conn.play_position(item.position).await {
                Ok(()) => {
                    return SkipOutcome::Playing {
                        position: item.position,
                    };
                }
                Err(MpdError::Ack { code: 50, .. }) => {
                    emit_single(
                        &self.emitter,
                        item,
                        DispositionKind::TrackSkippedFileNotFound,
                        RecoveryHint::RescanSource {
                            source_id: item
                                .source_id
                                .clone()
                                .unwrap_or_default(),
                        },
                    )
                    .await;
                }
                Err(MpdError::Ack { code: 53, .. }) => {
                    emit_single(
                        &self.emitter,
                        item,
                        DispositionKind::TrackSkippedPermissionDenied,
                        RecoveryHint::CheckMountPermissions {
                            source_id: item
                                .source_id
                                .clone()
                                .unwrap_or_default(),
                        },
                    )
                    .await;
                }
                Err(MpdError::Ack { code: 55, .. }) => {
                    emit_single(
                        &self.emitter,
                        item,
                        DispositionKind::TrackSkippedDecoderFailure,
                        RecoveryHint::InspectTrack {
                            uri: item.file_path.clone(),
                        },
                    )
                    .await;
                }
                Err(MpdError::Transport(_) | MpdError::Timeout { .. }) => {
                    // Transient transport / timeout: pause-and-retry.
                    return SkipOutcome::Paused {
                        last_attempted: item.position,
                    };
                }
                Err(_other) => {
                    // Other ACK code or protocol error — treat as
                    // file-not-found-class (skip + log via the
                    // disposition).
                    emit_single(
                        &self.emitter,
                        item,
                        DispositionKind::TrackSkippedDecoderFailure,
                        RecoveryHint::InspectTrack {
                            uri: item.file_path.clone(),
                        },
                    )
                    .await;
                }
            }
            candidate = candidate.saturating_add(1);
        }

        // Exhausted queue. Flush any pending run, then emit
        // QueueExhaustedNoPlayable + return Stopped.
        flush_run_if_any(&mut coalesce, &self.emitter).await;
        let d = Disposition::new(
            now_ms(),
            DispositionKind::QueueExhaustedNoPlayable,
            DispositionAction::Stop,
        )
        .with_recovery_hint(RecoveryHint::AddPlayableTrack);
        self.emitter.emit(d).await;
        SkipOutcome::Stopped
    }

    /// Resolve the cached source state for the given source id.
    /// `None` source_id (transient stream URL) returns
    /// `SourceState::Probing` — the optimistic
    /// "try-MPD-and-classify-the-response" path applies.
    async fn source_state_for(
        &self,
        source_id: &Option<String>,
    ) -> SourceState {
        match source_id {
            Some(id) => self
                .registry
                .get(id)
                .await
                .map(|r| r.state)
                .unwrap_or(SourceState::Probing),
            None => SourceState::Probing,
        }
    }
}

// ----- coalescing helpers -----

/// In-flight coalesced run state. Held by the
/// [`SkipTraversal::advance_to_next_playable`] loop and flushed
/// when the kind/source changes, when a reactive Ok lands, or
/// when the queue exhausts.
struct RunBuilder {
    kind: DispositionKind,
    source_id: Option<String>,
    from_position: u32,
    to_position: u32,
    count: u32,
    /// Recovery hint from the FIRST incident in the run. The
    /// catalogue contract: tracks_skipped_run carries the
    /// first disposition's recovery_hint.
    first_recovery_hint: RecoveryHint,
}

async fn accumulate_or_flush(
    coalesce: &mut Option<RunBuilder>,
    kind: DispositionKind,
    source_id: Option<String>,
    position: u32,
    recovery_hint: RecoveryHint,
    emitter: &DispositionEmitter,
) {
    if let Some(run) = coalesce.as_mut() {
        if run.kind == kind && run.source_id == source_id {
            run.to_position = position;
            run.count = run.count.saturating_add(1);
            return;
        }
        // Different kind or source — flush this run before
        // starting a new one.
        let to_flush = coalesce.take().unwrap();
        flush_run(to_flush, emitter).await;
    }
    *coalesce = Some(RunBuilder {
        kind,
        source_id,
        from_position: position,
        to_position: position,
        count: 1,
        first_recovery_hint: recovery_hint,
    });
}

async fn flush_run_if_any(
    coalesce: &mut Option<RunBuilder>,
    emitter: &DispositionEmitter,
) {
    if let Some(run) = coalesce.take() {
        flush_run(run, emitter).await;
    }
}

async fn flush_run(run: RunBuilder, emitter: &DispositionEmitter) {
    if run.count >= 2 {
        // Coalesced run: emit TracksSkippedRun.
        let mut d = Disposition::new(
            now_ms(),
            DispositionKind::TracksSkippedRun,
            DispositionAction::SkipForward,
        )
        .with_runs(DispositionRun {
            count: run.count,
            from_position: run.from_position,
            to_position: run.to_position,
        })
        .with_recovery_hint(run.first_recovery_hint);
        if let Some(sid) = run.source_id {
            d = d.with_source_id(sid);
        }
        emitter.emit(d).await;
    } else {
        // Single skip — emit the underlying kind.
        let mut d = Disposition::new(
            now_ms(),
            run.kind,
            DispositionAction::SkipForward,
        )
        .with_queue_position(run.from_position)
        .with_recovery_hint(run.first_recovery_hint);
        if let Some(sid) = run.source_id {
            d = d.with_source_id(sid);
        }
        emitter.emit(d).await;
    }
}

async fn emit_single(
    emitter: &DispositionEmitter,
    item: &PlayableQueueItem,
    kind: DispositionKind,
    recovery_hint: RecoveryHint,
) {
    let mut d =
        Disposition::new(now_ms(), kind, DispositionAction::SkipForward)
            .with_queue_position(item.position)
            .with_track_uri(item.file_path.clone())
            .with_recovery_hint(recovery_hint);
    if let Some(sid) = &item.source_id {
        d = d.with_source_id(sid.clone());
    }
    emitter.emit(d).await;
}

/// Build a [`Disposition`] for the
/// `playback_paused_source_offline` case (the currently-playing
/// track's source went offline mid-track, MPD's buffer drained,
/// transport stopped). Distinct from skip-traversal because the
/// pause happens BEFORE the next advance — the warden's
/// transport state machine emits this when its mid-track
/// detection fires.
///
/// Exposed as a free helper so callers outside the warden's
/// advance loop (the ambient observer's source-offline-on-
/// playing-song detection) can emit it through the shared
/// disposition emitter.
#[allow(dead_code)]
pub(crate) async fn emit_playback_paused_source_offline(
    emitter: &DispositionEmitter,
    source_id: String,
    track_uri: String,
    queue_position: u32,
) {
    let d = Disposition::new(
        now_ms(),
        DispositionKind::PlaybackPausedSourceOffline,
        DispositionAction::Pause,
    )
    .with_queue_position(queue_position)
    .with_track_uri(track_uri)
    .with_source_id(source_id.clone())
    .with_recovery_hint(RecoveryHint::WakeSource { source_id });
    emitter.emit(d).await;
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// Silence Clippy on the broadcast use we don't yet consume —
// will go away when the queue module wires up.
#[allow(dead_code)]
fn _arc_consumer(_a: Arc<()>) {}

// ----- tests -----

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_registry::{
        ScanPolicy, SourceKind, SourceRecord, SourceRegistry,
    };
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::Arc;

    // Shared CapturingAnnouncer for the emitter tests.
    #[derive(Default)]
    struct Cap {
        updates: std::sync::Mutex<Vec<serde_json::Value>>,
    }

    impl Cap {
        fn last(&self) -> Option<serde_json::Value> {
            self.updates.lock().unwrap().last().cloned()
        }
        fn all(&self) -> Vec<serde_json::Value> {
            self.updates.lock().unwrap().clone()
        }
    }

    impl evo_plugin_sdk::contract::SubjectAnnouncer for Cap {
        fn announce<'a>(
            &'a self,
            _a: evo_plugin_sdk::contract::SubjectAnnouncement,
        ) -> Pin<
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
        ) -> Pin<
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
            state: serde_json::Value,
        ) -> Pin<
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
            self.updates.lock().unwrap().push(state);
            Box::pin(async { Ok(()) })
        }
    }

    async fn setup() -> (SkipTraversal, Arc<Cap>) {
        let cap = Arc::new(Cap::default());
        let emitter = DispositionEmitter::new(cap.clone());
        let registry = SourceRegistry::new();
        let st = SkipTraversal::new(registry, emitter);
        (st, cap)
    }

    fn local_source(id: &str, state: SourceState) -> SourceRecord {
        SourceRecord {
            id: id.into(),
            display_name: "test".into(),
            kind: SourceKind::LocalInternal,
            mount_path: PathBuf::from("/var/lib/evo/music/INTERNAL"),
            mpd_storage_name: None,
            state,
            last_seen_online_at_ms: None,
            probe_cadence_ms: 60_000,
            scan_policy: ScanPolicy::EagerIncremental {
                on_online: true,
                on_mount_event: false,
            },
            track_count: 0,
            track_count_available: 0,
            last_scan_at_ms: None,
        }
    }

    fn item(
        position: u32,
        source: Option<&str>,
        available: bool,
    ) -> PlayableQueueItem {
        PlayableQueueItem {
            position,
            id: position + 1,
            file_path: format!("INTERNAL/track{position}.flac"),
            source_id: source.map(str::to_string),
            available,
        }
    }

    #[tokio::test]
    async fn cached_offline_single_item_emits_single_disposition() {
        let (st, cap) = setup().await;
        st.registry
            .register(local_source(
                "nas",
                SourceState::Offline {
                    reason: "test".into(),
                    since_ms: 0,
                },
            ))
            .await
            .unwrap();
        let queue = [item(0, Some("nas"), false)];
        // We can't call advance without a real MPD; the cached-
        // Offline path doesn't reach MPD before the queue
        // exhausts, but the exhausted path still emits a
        // QueueExhaustedNoPlayable. We assert two emissions: the
        // single TrackSkippedSourceOffline + the exhausted
        // marker.
        // The advance call needs an MpdConnection. Re-shape by
        // exercising the coalescing helpers directly to avoid
        // an MPD dependency in this unit test.
        let mut coalesce = None;
        accumulate_or_flush(
            &mut coalesce,
            DispositionKind::TrackSkippedSourceOffline,
            Some("nas".into()),
            queue[0].position,
            RecoveryHint::WakeSource {
                source_id: "nas".into(),
            },
            &st.emitter,
        )
        .await;
        flush_run_if_any(&mut coalesce, &st.emitter).await;

        let payload = cap.last().unwrap();
        let dispositions = payload["dispositions"].as_array().unwrap();
        assert_eq!(dispositions.len(), 1);
        let d = &dispositions[0];
        assert_eq!(d["kind"]["kind"], "track_skipped_source_offline");
        assert_eq!(d["queue_position"], 0);
        assert_eq!(d["source_id"], "nas");
        assert_eq!(d["recovery_hint"]["kind"], "wake_source");
    }

    #[tokio::test]
    async fn consecutive_same_kind_same_source_coalesces_into_run() {
        let (st, cap) = setup().await;
        st.registry
            .register(local_source(
                "nas",
                SourceState::Offline {
                    reason: "test".into(),
                    since_ms: 0,
                },
            ))
            .await
            .unwrap();
        let mut coalesce = None;
        for pos in 5..=12 {
            accumulate_or_flush(
                &mut coalesce,
                DispositionKind::TrackSkippedSourceOffline,
                Some("nas".into()),
                pos,
                RecoveryHint::WakeSource {
                    source_id: "nas".into(),
                },
                &st.emitter,
            )
            .await;
        }
        flush_run_if_any(&mut coalesce, &st.emitter).await;

        let payload = cap.last().unwrap();
        let dispositions = payload["dispositions"].as_array().unwrap();
        // ONE TracksSkippedRun should land, not 8 individual.
        assert_eq!(dispositions.len(), 1);
        let d = &dispositions[0];
        assert_eq!(d["kind"]["kind"], "tracks_skipped_run");
        assert_eq!(d["runs"]["count"], 8);
        assert_eq!(d["runs"]["from_position"], 5);
        assert_eq!(d["runs"]["to_position"], 12);
        assert_eq!(d["source_id"], "nas");
        // Recovery hint carries the first incident's hint.
        assert_eq!(d["recovery_hint"]["kind"], "wake_source");
    }

    #[tokio::test]
    async fn different_kind_breaks_coalescing_and_flushes_first_run() {
        let (st, cap) = setup().await;
        let mut coalesce = None;
        // Run of 3 source-offline skips.
        for pos in 0..3 {
            accumulate_or_flush(
                &mut coalesce,
                DispositionKind::TrackSkippedSourceOffline,
                Some("nas".into()),
                pos,
                RecoveryHint::WakeSource {
                    source_id: "nas".into(),
                },
                &st.emitter,
            )
            .await;
        }
        // Now a different kind — should flush the run first.
        accumulate_or_flush(
            &mut coalesce,
            DispositionKind::TrackSkippedFileNotFound,
            Some("nas".into()),
            5,
            RecoveryHint::RescanSource {
                source_id: "nas".into(),
            },
            &st.emitter,
        )
        .await;
        flush_run_if_any(&mut coalesce, &st.emitter).await;

        let dispositions = cap.all();
        // 2 emit() calls total: one for the run, one for the
        // single TrackSkippedFileNotFound.
        assert_eq!(dispositions.len(), 2);
        let first = &dispositions[0]["dispositions"][0];
        assert_eq!(first["kind"]["kind"], "tracks_skipped_run");
        assert_eq!(first["runs"]["count"], 3);
        let last = &dispositions[1]["dispositions"][0];
        assert_eq!(last["kind"]["kind"], "track_skipped_file_not_found");
        assert_eq!(last["queue_position"], 5);
    }

    #[tokio::test]
    async fn different_source_breaks_coalescing() {
        let (st, cap) = setup().await;
        let mut coalesce = None;
        // 2 from source A.
        for pos in 0..2 {
            accumulate_or_flush(
                &mut coalesce,
                DispositionKind::TrackSkippedSourceOffline,
                Some("a".into()),
                pos,
                RecoveryHint::WakeSource {
                    source_id: "a".into(),
                },
                &st.emitter,
            )
            .await;
        }
        // 1 from source B (different source -> flush run + start
        // new run-of-one).
        accumulate_or_flush(
            &mut coalesce,
            DispositionKind::TrackSkippedSourceOffline,
            Some("b".into()),
            5,
            RecoveryHint::WakeSource {
                source_id: "b".into(),
            },
            &st.emitter,
        )
        .await;
        flush_run_if_any(&mut coalesce, &st.emitter).await;
        let emitted = cap.all();
        // First emission: the source-A run (count=2).
        let first = &emitted[0]["dispositions"][0];
        assert_eq!(first["kind"]["kind"], "tracks_skipped_run");
        assert_eq!(first["source_id"], "a");
        // Second emission: the single source-B skip.
        let second = &emitted[1]["dispositions"][0];
        assert_eq!(second["kind"]["kind"], "track_skipped_source_offline");
        assert_eq!(second["source_id"], "b");
    }

    #[tokio::test]
    async fn emit_playback_paused_carries_source_recovery_hint() {
        let (st, cap) = setup().await;
        emit_playback_paused_source_offline(
            &st.emitter,
            "nas".into(),
            "INTERNAL/song.flac".into(),
            7,
        )
        .await;
        let payload = cap.last().unwrap();
        let d = &payload["dispositions"][0];
        assert_eq!(d["kind"]["kind"], "playback_paused_source_offline");
        assert_eq!(d["action_taken"]["kind"], "pause");
        assert_eq!(d["queue_position"], 7);
        assert_eq!(d["track_uri"], "INTERNAL/song.flac");
        assert_eq!(d["recovery_hint"]["kind"], "wake_source");
        assert_eq!(d["recovery_hint"]["source_id"], "nas");
    }

    #[tokio::test]
    async fn source_state_for_none_source_id_returns_probing() {
        let (st, _) = setup().await;
        let s = st.source_state_for(&None).await;
        assert!(matches!(s, SourceState::Probing));
    }

    #[tokio::test]
    async fn source_state_for_unknown_source_id_returns_probing() {
        let (st, _) = setup().await;
        let s = st.source_state_for(&Some("does-not-exist".into())).await;
        assert!(matches!(s, SourceState::Probing));
    }

    #[tokio::test]
    async fn source_state_for_registered_returns_recorded_state() {
        let (st, _) = setup().await;
        st.registry
            .register(local_source(
                "a",
                SourceState::Offline {
                    reason: "x".into(),
                    since_ms: 100,
                },
            ))
            .await
            .unwrap();
        let s = st.source_state_for(&Some("a".into())).await;
        match s {
            SourceState::Offline { reason, since_ms } => {
                assert_eq!(reason, "x");
                assert_eq!(since_ms, 100);
            }
            other => panic!("expected Offline, got {other:?}"),
        }
    }
}
