// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Disposition emitter — publishes typed autonomous-decision
//! records to the `audio_playback_disposition` subject + maintains
//! a durable ring buffer for operator-inspectable audit trail.
//!
//! See [`evo_plugin_sdk::contract::disposition::Disposition`] for
//! the shared wire shape. The emitter is the playback warden's
//! publish surface for skip-traversal decisions; cross-cutting
//! subsystems (multiroom leader-change, source-routing fallback)
//! emit dispositions on their own subjects using the same SDK
//! type.
//!
//! # Acceptance rows pinned at this layer
//!
//! - `audio.playback.v1` `disposition-emitted-on-autonomous-decisions`:
//!   every autonomous decision emits a Disposition; consecutive
//!   same-kind same-source skips MUST coalesce into a single
//!   `tracks_skipped_run` (coalescing logic lives in the
//!   skip-traversal module; the emitter publishes whatever it
//!   receives).
//! - `audio.playback.v1` `disposition-shape-is-shelf-agnostic-shared-sdk-type`:
//!   the Disposition struct + the four tagged-kind enums
//!   (`DispositionKind`, `DispositionAction`, `RecoveryHint`,
//!   `DispositionRun`) live in `evo-plugin-sdk`; the emitter
//!   wraps the SDK type without redefining it.
//!
//! # Durability + ring buffer
//!
//! The emitter holds the last
//! [`DISPOSITION_RING_BUFFER_CAPACITY`] dispositions in memory and
//! persists them to `dispositions.toml` under the plugin state
//! directory on every emission. On plugin restart the ring
//! buffer rehydrates so the operator's last-seen audit trail
//! survives reboots.
//!
//! # Subject publish
//!
//! On every emission the emitter:
//!
//! 1. Pushes the disposition onto the ring buffer (oldest evicted
//!    when capacity is reached).
//! 2. Persists the buffer (atomic write-tmp-then-rename).
//! 3. Publishes the buffer to the
//!    `audio_playback_disposition` subject so subscribers see
//!    the updated state.
//! 4. Best-effort: a persist or publish failure logs at warn
//!    but does not propagate — the emit operation is fire-and-
//!    forget per the playback-never-disrupted-by-emitter-failure
//!    discipline established by the existing subject emitters.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use evo_plugin_sdk::contract::{
    Disposition, ExternalAddressing, SubjectAnnouncement, SubjectAnnouncer,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Mutex;

/// How many dispositions the emitter keeps in its durable ring
/// buffer. Catalogue-pinned at 32 by the `audio.playback.v1`
/// `disposition-emitted-on-autonomous-decisions` acceptance row.
pub(crate) const DISPOSITION_RING_BUFFER_CAPACITY: usize = 32;

/// Wire-payload version for the subject's state shape +
/// persisted file. Bumped on any breaking change; additive
/// fields ride at v=1.
pub(crate) const DISPOSITION_STATE_PAYLOAD_VERSION: u32 = 1;

/// Catalogue-aligned subject type for the playback warden's
/// disposition surface. Must match the
/// `audio.playback.v1` schema's `[[subjects]]` declaration
/// verbatim.
const SUBJECT_TYPE_DISPOSITION: &str = "audio_playback_disposition";

/// Addressing scheme for the disposition subject. Same scheme
/// as the other audio.playback subjects.
const SCHEME_PLAYBACK: &str = "evo.audio.playback";

/// Addressing value for the disposition subject — singleton per
/// warden.
const VALUE_DISPOSITION: &str = "disposition";

const PLUGIN_NAME: &str = "org.evoframework.playback.mpd";

/// Disposition publisher with ring-buffer + durable persistence.
///
/// Cloneable cheaply (Arc bump). Multiple producers (skip-
/// traversal, future multiroom leader-change, source-routing
/// fallback) emit through one shared emitter so the audit trail
/// is unified per shelf.
#[derive(Clone)]
pub(crate) struct DispositionEmitter {
    inner: Arc<EmitterInner>,
}

struct EmitterInner {
    subjects: Arc<dyn SubjectAnnouncer>,
    ring: Mutex<VecDeque<Disposition>>,
    state_path: Option<PathBuf>,
}

impl DispositionEmitter {
    /// Construct an emitter backed by a live subject announcer.
    /// No persistence — use [`Self::with_state_path`] to attach
    /// a state file.
    #[allow(dead_code)]
    pub(crate) fn new(subjects: Arc<dyn SubjectAnnouncer>) -> Self {
        Self {
            inner: Arc::new(EmitterInner {
                subjects,
                ring: Mutex::new(VecDeque::with_capacity(
                    DISPOSITION_RING_BUFFER_CAPACITY,
                )),
                state_path: None,
            }),
        }
    }

    /// Construct an emitter that persists to the given path on
    /// every emission. Rehydrated via [`Self::load_from_disk`].
    pub(crate) fn with_state_path(
        subjects: Arc<dyn SubjectAnnouncer>,
        state_path: PathBuf,
    ) -> Self {
        Self {
            inner: Arc::new(EmitterInner {
                subjects,
                ring: Mutex::new(VecDeque::with_capacity(
                    DISPOSITION_RING_BUFFER_CAPACITY,
                )),
                state_path: Some(state_path),
            }),
        }
    }

    /// Announce the singleton `audio_playback_disposition` subject
    /// at plugin load. Seeds the announcement state with the
    /// current ring-buffer contents (empty on cold start;
    /// rehydrated entries on warm start).
    ///
    /// Best-effort: announcer errors are logged but not
    /// propagated.
    pub(crate) async fn announce(&self) {
        let addressing =
            ExternalAddressing::new(SCHEME_PLAYBACK, VALUE_DISPOSITION);
        let state = self.render_envelope().await;
        let announcement = SubjectAnnouncement::new(
            SUBJECT_TYPE_DISPOSITION,
            vec![addressing],
        )
        .with_state(state);
        if let Err(e) = self.inner.subjects.announce(announcement).await {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "disposition subject announce failed; \
                 operator-facing audit trail will be unavailable \
                 until a future re-announce attempt"
            );
        }
    }

    /// Emit one disposition. Pushes onto the ring buffer (oldest
    /// evicted at capacity), persists the buffer, and publishes
    /// the subject's new state.
    ///
    /// Best-effort: any failure (persist, publish) logs at warn
    /// but the emit call returns Ok — the audit trail's
    /// availability is not a hard contract that can stop
    /// playback.
    pub(crate) async fn emit(&self, disposition: Disposition) {
        {
            let mut ring = self.inner.ring.lock().await;
            if ring.len() >= DISPOSITION_RING_BUFFER_CAPACITY {
                ring.pop_front();
            }
            ring.push_back(disposition);
        }
        if let Err(e) = self.persist().await {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "disposition persist failed; operator audit trail \
                 may be lost on next plugin restart"
            );
        }
        self.publish().await;
    }

    /// Publish the current ring-buffer state to the
    /// `audio_playback_disposition` subject. Internal: callers
    /// use [`Self::emit`] which publishes as part of the emit
    /// cycle.
    async fn publish(&self) {
        let addressing =
            ExternalAddressing::new(SCHEME_PLAYBACK, VALUE_DISPOSITION);
        let state = self.render_envelope().await;
        if let Err(e) =
            self.inner.subjects.update_state(addressing, state).await
        {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "disposition subject update_state failed; \
                 operator UI may not see the latest entry"
            );
        }
    }

    /// Render the ring buffer's current contents as the subject's
    /// wire payload. Used by both [`Self::announce`] and
    /// [`Self::publish`]; the same shape covers seeded-empty
    /// announcement + every subsequent update.
    async fn render_envelope(&self) -> serde_json::Value {
        let ring = self.inner.ring.lock().await;
        let dispositions: Vec<&Disposition> = ring.iter().rev().collect(); // most recent first
        json!({
            "v": DISPOSITION_STATE_PAYLOAD_VERSION,
            "dispositions": dispositions,
        })
    }

    /// Read-only snapshot of the ring buffer for the
    /// `get_dispositions` read verb (not yet wired into the
    /// dispatcher). Most-recent-first order matches the
    /// subject state's wire shape.
    #[allow(dead_code)]
    pub(crate) async fn snapshot(&self) -> Vec<Disposition> {
        let ring = self.inner.ring.lock().await;
        ring.iter().rev().cloned().collect()
    }

    /// Rehydrate the ring buffer from the state file. Returns
    /// `Ok(0)` when no state path is configured or the file
    /// doesn't exist yet.
    pub(crate) async fn load_from_disk(
        &self,
    ) -> Result<usize, DispositionEmitterError> {
        let Some(path) = self.inner.state_path.as_ref() else {
            return Ok(0);
        };
        match tokio::fs::read_to_string(path).await {
            Ok(s) => {
                let persisted: PersistedDispositions = toml::from_str(&s)
                    .map_err(|e| DispositionEmitterError::Persist {
                        reason: format!("parse {path:?}: {e}"),
                    })?;
                let count = persisted.dispositions.len();
                let mut ring = self.inner.ring.lock().await;
                ring.clear();
                for d in persisted.dispositions {
                    if ring.len() >= DISPOSITION_RING_BUFFER_CAPACITY {
                        ring.pop_front();
                    }
                    ring.push_back(d);
                }
                Ok(count)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(DispositionEmitterError::Persist {
                reason: format!("read {path:?}: {e}"),
            }),
        }
    }

    /// Persist the ring buffer atomically. No-op when no state
    /// path is configured.
    async fn persist(&self) -> Result<(), DispositionEmitterError> {
        let Some(path) = self.inner.state_path.as_ref() else {
            return Ok(());
        };
        let snapshot = {
            let ring = self.inner.ring.lock().await;
            PersistedDispositions {
                v: DISPOSITION_STATE_PAYLOAD_VERSION,
                dispositions: ring.iter().cloned().collect(),
            }
        };
        let body = toml::to_string_pretty(&snapshot).map_err(|e| {
            DispositionEmitterError::Persist {
                reason: format!("serialise: {e}"),
            }
        })?;
        let parent =
            path.parent()
                .ok_or_else(|| DispositionEmitterError::Persist {
                    reason: format!("state path {path:?} has no parent"),
                })?;
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            DispositionEmitterError::Persist {
                reason: format!("mkdir {parent:?}: {e}"),
            }
        })?;
        let staging = parent.join(format!(
            ".{}.tmp",
            path.file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| "dispositions.toml".to_string())
        ));
        tokio::fs::write(&staging, body).await.map_err(|e| {
            DispositionEmitterError::Persist {
                reason: format!("write {staging:?}: {e}"),
            }
        })?;
        tokio::fs::rename(&staging, path).await.map_err(|e| {
            DispositionEmitterError::Persist {
                reason: format!("rename {staging:?} -> {path:?}: {e}"),
            }
        })?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedDispositions {
    v: u32,
    #[serde(default)]
    dispositions: Vec<Disposition>,
}

/// Errors specific to the disposition emitter's persistence
/// path. Publish errors are logged + swallowed in the emit
/// flow per the playback-never-disrupted-by-emitter-failure
/// contract, so they don't surface here.
#[derive(Debug, Clone, thiserror::Error)]
pub(crate) enum DispositionEmitterError {
    /// File I/O or serialisation failed.
    #[error("disposition persist error: {reason}")]
    Persist { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_plugin_sdk::contract::disposition::{
        DispositionAction, DispositionKind, DispositionRun, RecoveryHint,
    };
    use std::pin::Pin;
    use tempfile::TempDir;

    // ----- test-only capturing announcer -----

    #[derive(Default)]
    struct CapturingAnnouncer {
        announces: std::sync::Mutex<Vec<SubjectAnnouncement>>,
        updates: std::sync::Mutex<Vec<(ExternalAddressing, serde_json::Value)>>,
    }

    impl CapturingAnnouncer {
        fn announce_count(&self) -> usize {
            self.announces.lock().unwrap().len()
        }
        fn update_count(&self) -> usize {
            self.updates.lock().unwrap().len()
        }
        fn last_update_payload(&self) -> Option<serde_json::Value> {
            self.updates.lock().unwrap().last().map(|(_, v)| v.clone())
        }
        fn last_announce(&self) -> Option<SubjectAnnouncement> {
            self.announces.lock().unwrap().last().cloned()
        }
    }

    impl SubjectAnnouncer for CapturingAnnouncer {
        fn announce<'a>(
            &'a self,
            a: SubjectAnnouncement,
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
            Box::pin(async move {
                self.announces.lock().unwrap().push(a.clone());
                Ok(())
            })
        }

        fn retract<'a>(
            &'a self,
            _addressing: ExternalAddressing,
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
            addressing: ExternalAddressing,
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
            Box::pin(async move {
                self.updates.lock().unwrap().push((addressing, state));
                Ok(())
            })
        }
    }

    fn make_disposition(kind: DispositionKind, at_ms: u64) -> Disposition {
        Disposition::new(at_ms, kind, DispositionAction::SkipForward)
    }

    #[tokio::test]
    async fn announce_seeds_empty_envelope_into_announcement_state() {
        let cap = Arc::new(CapturingAnnouncer::default());
        let emitter = DispositionEmitter::new(cap.clone());
        emitter.announce().await;
        assert_eq!(cap.announce_count(), 1);
        let ann = cap.last_announce().unwrap();
        assert_eq!(ann.subject_type, "audio_playback_disposition");
        assert_eq!(ann.addressings.len(), 1);
        assert_eq!(ann.addressings[0].scheme, "evo.audio.playback");
        assert_eq!(ann.addressings[0].value, "disposition");
        let state = &ann.state;
        assert_eq!(state["v"], 1);
        assert!(state["dispositions"].is_array());
        assert_eq!(state["dispositions"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn emit_publishes_and_updates_state() {
        let cap = Arc::new(CapturingAnnouncer::default());
        let emitter = DispositionEmitter::new(cap.clone());
        let d =
            make_disposition(DispositionKind::TrackSkippedSourceOffline, 100);
        emitter.emit(d).await;
        assert_eq!(cap.update_count(), 1);
        let payload = cap.last_update_payload().unwrap();
        assert_eq!(payload["v"], 1);
        let dispositions = payload["dispositions"].as_array().unwrap();
        assert_eq!(dispositions.len(), 1);
        assert_eq!(
            dispositions[0]["kind"]["kind"],
            "track_skipped_source_offline"
        );
        assert_eq!(dispositions[0]["at_ms"], 100);
    }

    #[tokio::test]
    async fn ring_buffer_evicts_oldest_at_capacity() {
        let cap = Arc::new(CapturingAnnouncer::default());
        let emitter = DispositionEmitter::new(cap.clone());
        for i in 0..(DISPOSITION_RING_BUFFER_CAPACITY as u64 + 5) {
            emitter
                .emit(make_disposition(
                    DispositionKind::TrackSkippedSourceOffline,
                    i,
                ))
                .await;
        }
        let snap = emitter.snapshot().await;
        // Most-recent-first ordering; capacity bounds the snapshot.
        assert_eq!(snap.len(), DISPOSITION_RING_BUFFER_CAPACITY);
        // The oldest 5 should have been evicted.
        assert_eq!(snap.last().unwrap().at_ms, 5);
        assert_eq!(
            snap.first().unwrap().at_ms,
            DISPOSITION_RING_BUFFER_CAPACITY as u64 + 4
        );
    }

    #[tokio::test]
    async fn snapshot_returns_most_recent_first_order() {
        let cap = Arc::new(CapturingAnnouncer::default());
        let emitter = DispositionEmitter::new(cap.clone());
        emitter
            .emit(make_disposition(
                DispositionKind::TrackSkippedSourceOffline,
                1,
            ))
            .await;
        emitter
            .emit(make_disposition(
                DispositionKind::TrackSkippedFileNotFound,
                2,
            ))
            .await;
        emitter
            .emit(make_disposition(
                DispositionKind::PlaybackPausedSourceOffline,
                3,
            ))
            .await;
        let snap = emitter.snapshot().await;
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].at_ms, 3);
        assert_eq!(snap[1].at_ms, 2);
        assert_eq!(snap[2].at_ms, 1);
    }

    #[tokio::test]
    async fn persisted_then_rehydrated_preserves_order_and_count() {
        let cap = Arc::new(CapturingAnnouncer::default());
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("dispositions.toml");
        let emitter =
            DispositionEmitter::with_state_path(cap.clone(), path.clone());
        for i in 1..=5 {
            emitter
                .emit(make_disposition(
                    DispositionKind::TrackSkippedSourceOffline,
                    i,
                ))
                .await;
        }
        // Fresh emitter rehydrates the same ring.
        let cap2 = Arc::new(CapturingAnnouncer::default());
        let emitter2 =
            DispositionEmitter::with_state_path(cap2.clone(), path.clone());
        let count = emitter2.load_from_disk().await.unwrap();
        assert_eq!(count, 5);
        let snap = emitter2.snapshot().await;
        assert_eq!(snap.len(), 5);
        // Most-recent-first ordering after rehydrate.
        assert_eq!(snap[0].at_ms, 5);
        assert_eq!(snap[4].at_ms, 1);
    }

    #[tokio::test]
    async fn rehydrate_with_no_state_file_returns_empty() {
        let cap = Arc::new(CapturingAnnouncer::default());
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("dispositions.toml");
        let emitter = DispositionEmitter::with_state_path(cap.clone(), path);
        let count = emitter.load_from_disk().await.unwrap();
        assert_eq!(count, 0);
        assert!(emitter.snapshot().await.is_empty());
    }

    #[tokio::test]
    async fn emit_disposition_with_recovery_hint_round_trips_on_wire() {
        let cap = Arc::new(CapturingAnnouncer::default());
        let emitter = DispositionEmitter::new(cap.clone());
        let d = Disposition::new(
            42,
            DispositionKind::TrackSkippedSourceOffline,
            DispositionAction::SkipForward,
        )
        .with_queue_position(3)
        .with_source_id("nas-uuid")
        .with_recovery_hint(RecoveryHint::WakeSource {
            source_id: "nas-uuid".into(),
        });
        emitter.emit(d).await;
        let payload = cap.last_update_payload().unwrap();
        let arr = payload["dispositions"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        let item = &arr[0];
        assert_eq!(item["queue_position"], 3);
        assert_eq!(item["source_id"], "nas-uuid");
        assert_eq!(item["recovery_hint"]["kind"], "wake_source");
        assert_eq!(item["recovery_hint"]["source_id"], "nas-uuid");
    }

    #[tokio::test]
    async fn emit_coalesced_run_disposition_carries_runs_field() {
        let cap = Arc::new(CapturingAnnouncer::default());
        let emitter = DispositionEmitter::new(cap.clone());
        let d = Disposition::new(
            0,
            DispositionKind::TracksSkippedRun,
            DispositionAction::SkipForward,
        )
        .with_source_id("nas-uuid")
        .with_runs(DispositionRun::from_count(5, 10));
        emitter.emit(d).await;
        let payload = cap.last_update_payload().unwrap();
        let arr = payload["dispositions"].as_array().unwrap();
        assert_eq!(arr[0]["kind"]["kind"], "tracks_skipped_run");
        assert_eq!(arr[0]["runs"]["count"], 10);
        assert_eq!(arr[0]["runs"]["from_position"], 5);
        assert_eq!(arr[0]["runs"]["to_position"], 14);
    }
}
