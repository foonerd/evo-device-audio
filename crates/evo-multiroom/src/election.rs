// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Source-host election for multi-room groups.
//!
//! Within a multi-room group, exactly one member device is
//! the *source-host* — the device authoritative for sourcing
//! playback into the group. It runs the actual source plugin
//! (a streaming integration, USB DAC, network input); the
//! other group members are receivers.
//!
//! This module owns the framework's local-view election: each
//! evo node deterministically picks one member of each group
//! it participates in (or simply observes) as that group's
//! source-host, given the local node's view of the live device
//! universe.
//!
//! ## Election function
//!
//! For each group:
//!
//! 1. Compute the candidate set as the intersection of the
//!    group's member device ids with the local node's view of
//!    the live device universe (the local node itself plus
//!    every mDNS-SD-discovered peer whose `last_seen_ms` is
//!    fresher than the runtime's [`ElectionConfig::liveness_window`]).
//! 2. If the candidate set is empty, the source-host is
//!    `None` — no live device is reachable to source playback.
//! 3. Otherwise, the elected source-host is the candidate
//!    with the lexicographically lowest canonical UUIDv4.
//!
//! The function is deterministic on its inputs, so two
//! peers observing the same {membership, live} sets agree
//! without exchanging messages. Network partitions can
//! cause split-brain (each side independently elects a
//! different source-host on its own view); split-brain
//! resolution is the responsibility of a later sub-primitive
//! that adds a network heartbeat protocol.
//!
//! ## Triggers
//!
//! The runtime re-evaluates whenever the local view of
//! either input changes. Concretely, it subscribes to the
//! happenings bus and re-evaluates on:
//!
//! - [`Happening::PeerDiscovered`] / `PeerUpdated` /
//!   `PeerLost` (live set drift),
//! - [`Happening::GroupCreated`] / `GroupMembershipChanged`
//!   / `GroupDeleted` (membership drift).
//!
//! Plus a periodic safety-net tick whose cadence is
//! [`ElectionConfig::eval_interval`] so liveness drift past
//! the TTL boundary triggers re-election even if the
//! discovery runtime never emits a `PeerLost` (e.g. when a
//! peer was already absent at boot).
//!
//! Every transition writes through the substrate and emits
//! a [`Happening::SourceHostElected`] envelope describing the
//! prior + new source-host.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

use evo::groups::GroupStore;
use evo::happenings::{Happening, HappeningBus};
use evo::persistence::{
    PersistedSourceHostElection, PersistenceError, PersistenceStore,
};
use evo_primitives::DeviceId;

/// Errors raised by [`ElectionRuntime`].
#[derive(Debug, thiserror::Error)]
pub enum ElectionError {
    /// Underlying persistence error.
    #[error("persistence error: {0}")]
    Persistence(#[from] PersistenceError),
    /// Underlying group-store error during evaluation
    /// (membership read).
    #[error("group store error: {0}")]
    Groups(#[from] evo::groups::GroupError),
}

/// Configuration for the [`ElectionRuntime`].
#[derive(Debug, Clone)]
pub struct ElectionConfig {
    /// Cadence of the periodic safety-net re-evaluation
    /// tick. Default 30 seconds.
    pub eval_interval: Duration,
    /// Time window within which an audio-plane connection's
    /// inbound activity counts as a fresh liveness signal
    /// for election purposes. A peer with an open connection
    /// whose `last_channel_activity_ms` is within this
    /// window is treated as alive. Distinct from
    /// [`evo::discovery::DiscoveryConfig::peer_ttl`] so
    /// elections respond more eagerly than the discovery
    /// substrate's prune cadence. Default 60 seconds.
    pub liveness_window: Duration,
}

impl Default for ElectionConfig {
    fn default() -> Self {
        Self {
            eval_interval: Duration::from_secs(30),
            liveness_window: Duration::from_secs(60),
        }
    }
}

/// Source-host election record for one multi-room group.
/// Mirror of [`PersistedSourceHostElection`] one-to-one;
/// the in-memory shape is what the substrate persists.
pub type SourceHostElection = PersistedSourceHostElection;

/// Persistence-backed source-host election runtime. Owns
/// the periodic eval task, the happenings subscriber that
/// triggers eager re-evaluation, and the in-memory election
/// mirror.
pub struct ElectionRuntime {
    persistence: Arc<dyn PersistenceStore>,
    happenings: Arc<HappeningBus>,
    group_store: Arc<GroupStore>,
    /// Optional audio-plane runtime handle. When present,
    /// election's liveness predicate also accepts peers with
    /// an active audio-plane TCP connection as "alive" — a
    /// device that is actively exchanging audio with us is by
    /// definition operational regardless of mDNS-SD record
    /// staleness. mDNS-SD is a discovery mechanism and its
    /// natural re-resolve cadence is much slower than the
    /// election liveness window; relying on it alone would
    /// disqualify devices that are very much alive in an
    /// in-flight session. Audio-plane connection state is the
    /// authoritative real-time signal.
    ///
    /// Set via [`Self::with_audio_plane`] after construction.
    /// Optional rather than required so test rigs and contexts
    /// without the audio plane (CLI, isolated election unit
    /// tests) continue to compile and behave deterministically
    /// against mDNS-SD freshness alone.
    audio_plane: arc_swap::ArcSwapOption<evo::audio_plane::AudioPlaneRuntime>,
    local_device_id: DeviceId,
    config: ElectionConfig,
    inner: AsyncMutex<ElectionInner>,
}

impl std::fmt::Debug for ElectionRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ElectionRuntime")
            .field("local_device_id", &self.local_device_id)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct ElectionInner {
    eval_task: Option<JoinHandle<()>>,
    subscriber_task: Option<JoinHandle<()>>,
    /// In-memory mirror of every group's last election,
    /// keyed on group_id. The substrate is the source of
    /// truth; this is the fast read path for `list()` and
    /// `get()`.
    elections: HashMap<String, SourceHostElection>,
}

impl ElectionRuntime {
    /// Construct a runtime. Election does not begin until
    /// [`Self::start`] is called. The audio-plane runtime is
    /// optional and wired post-construction via
    /// [`Self::with_audio_plane`] when it becomes available
    /// (the audio-plane runtime is itself constructed against
    /// the election runtime's local-id input, so the two are
    /// brought up in a deliberate order at boot).
    pub fn new(
        persistence: Arc<dyn PersistenceStore>,
        happenings: Arc<HappeningBus>,
        group_store: Arc<GroupStore>,
        local_device_id: DeviceId,
        config: ElectionConfig,
    ) -> Self {
        Self {
            persistence,
            happenings,
            group_store,
            audio_plane: arc_swap::ArcSwapOption::const_empty(),
            local_device_id,
            config,
            inner: AsyncMutex::new(ElectionInner::default()),
        }
    }

    /// Inject the audio-plane runtime so election's liveness
    /// predicate can accept peers with an active audio-plane
    /// TCP connection as alive even when their mDNS-SD record
    /// has aged past [`ElectionConfig::liveness_window`].
    /// Called once at boot after the audio-plane runtime is
    /// constructed. Idempotent — calling repeatedly replaces
    /// the cached handle atomically.
    pub fn with_audio_plane(
        &self,
        audio_plane: Arc<evo::audio_plane::AudioPlaneRuntime>,
    ) {
        self.audio_plane.store(Some(audio_plane));
    }

    /// Rehydrate the in-memory election mirror from the
    /// substrate. Called once at boot before [`Self::start`]
    /// so consumers see the prior election state until the
    /// first re-evaluation completes.
    pub async fn rehydrate(&self) -> Result<(), ElectionError> {
        let rows = self.persistence.list_source_host_elections().await?;
        let mut g = self.inner.lock().await;
        g.elections =
            rows.into_iter().map(|r| (r.group_id.clone(), r)).collect();
        Ok(())
    }

    /// Start the election runtime: run an initial
    /// evaluation, spawn the happenings subscriber that
    /// triggers eager re-evaluation, and spawn the periodic
    /// safety-net eval tick.
    pub async fn start(self: &Arc<Self>) -> Result<(), ElectionError> {
        self.evaluate().await?;
        let mut g = self.inner.lock().await;
        if g.eval_task.is_some() {
            return Ok(());
        }

        let runtime = Arc::clone(self);
        let interval = runtime.config.eval_interval;
        let eval_task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                if let Err(e) = runtime.evaluate().await {
                    tracing::warn!(
                        error = %e,
                        "source-host election: periodic evaluate failed"
                    );
                }
            }
        });

        let runtime = Arc::clone(self);
        let mut rx = runtime.happenings.subscribe();
        let subscriber_task = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(h) => {
                        if !is_election_trigger(&h) {
                            continue;
                        }
                        if let Err(e) = runtime.evaluate().await {
                            tracing::warn!(
                                error = %e,
                                "source-host election: triggered \
                                 evaluate failed"
                            );
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(
                        _,
                    )) => {
                        // Subscriber fell behind; recover by
                        // re-evaluating against current state.
                        if let Err(e) = runtime.evaluate().await {
                            tracing::warn!(
                                error = %e,
                                "source-host election: catch-up evaluate \
                                 failed"
                            );
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return;
                    }
                }
            }
        });

        g.eval_task = Some(eval_task);
        g.subscriber_task = Some(subscriber_task);
        Ok(())
    }

    /// Shut the runtime down. Idempotent.
    pub async fn shutdown(&self) {
        let mut g = self.inner.lock().await;
        if let Some(t) = g.eval_task.take() {
            t.abort();
        }
        if let Some(t) = g.subscriber_task.take() {
            t.abort();
        }
    }

    /// Liveness window the election runtime uses to classify
    /// a peer as a live candidate. Other projections (the
    /// domain-members roster, operator-facing badges) MUST
    /// consult this accessor when reporting "currently
    /// advertising" so the operator-visible state matches what
    /// election actually considers electable. Returning the
    /// `Duration` rather than the raw milliseconds keeps the
    /// caller honest about units.
    pub fn liveness_window(&self) -> Duration {
        self.config.liveness_window
    }

    /// Read the election record for one group. Returns
    /// `None` when no election is recorded.
    pub async fn get(&self, group_id: &str) -> Option<SourceHostElection> {
        let g = self.inner.lock().await;
        g.elections.get(group_id).cloned()
    }

    /// List every recorded election ordered by group id.
    pub async fn list(&self) -> Vec<SourceHostElection> {
        let g = self.inner.lock().await;
        let mut rows: Vec<SourceHostElection> =
            g.elections.values().cloned().collect();
        rows.sort_by(|a, b| a.group_id.cmp(&b.group_id));
        rows
    }
}

#[async_trait::async_trait]
impl evo_primitives::ElectionState for ElectionRuntime {
    async fn source_host_for(&self, group_id: &str) -> Option<String> {
        ElectionRuntime::get(self, group_id)
            .await
            .and_then(|e| e.source_host_device_id)
    }

    async fn list_source_hosts(&self) -> Vec<(String, Option<String>)> {
        ElectionRuntime::list(self)
            .await
            .into_iter()
            .map(|e| (e.group_id, e.source_host_device_id))
            .collect()
    }

    async fn election_for(
        &self,
        group_id: &str,
    ) -> Option<evo_primitives::SourceHostElection> {
        ElectionRuntime::get(self, group_id).await
    }

    async fn list_elections(&self) -> Vec<evo_primitives::SourceHostElection> {
        ElectionRuntime::list(self).await
    }

    fn liveness_window_ms(&self) -> u64 {
        self.config.liveness_window.as_millis() as u64
    }
}

impl ElectionRuntime {
    // (Private helpers below — kept inside an additional impl block
    // so the trait impl directly above stays adjacent to the public
    // `get` / `list` methods it composes with.)

    /// Run the election function over the current local
    /// view: walk every recorded group, compute the
    /// elected source-host, write through transitions,
    /// emit `SourceHostElected` happenings on every
    /// transition. Drops in-memory election entries for
    /// groups that have been deleted (the substrate FK
    /// cascades; the in-memory mirror catches up here).
    /// Idempotent — calling twice in succession against the
    /// same view is a no-op.
    pub async fn evaluate(&self) -> Result<(), ElectionError> {
        let groups = self.group_store.list().await?;
        let live = self.live_device_set().await;
        let now_ms = now_ms();

        // Clean up in-memory entries for groups no longer
        // present. The SQLite FK on source_host_elections
        // cascades on group delete; the memory persistence
        // store does the same in delete_group. The runtime's
        // own in-memory mirror has to mirror that.
        let live_group_ids: std::collections::HashSet<String> =
            groups.iter().map(|g| g.group_id.0.clone()).collect();
        {
            let mut g = self.inner.lock().await;
            g.elections.retain(|k, _| live_group_ids.contains(k));
        }

        for group in groups {
            let candidates: Vec<&str> = group
                .members
                .iter()
                .filter(|m| live.contains(*m))
                .map(String::as_str)
                .collect();
            // Operator-pinned source-host overrides the
            // canonical-min election rule while the pinned
            // device remains a live group member. When the
            // pinned device is offline / no longer a member,
            // election falls back to the candidate-min rule.
            let new_source_host = match group.pinned_source_host.as_deref() {
                Some(pin) if candidates.contains(&pin) => Some(pin.to_string()),
                _ => candidates.iter().min().map(|s| (*s).to_string()),
            };
            let candidate_count = candidates.len() as u32;

            let (prior_source_host, prior_at_ms) = {
                let g = self.inner.lock().await;
                match g.elections.get(group.group_id.as_str()) {
                    Some(e) => {
                        (e.source_host_device_id.clone(), Some(e.elected_at_ms))
                    }
                    None => (None, None),
                }
            };

            if prior_source_host == new_source_host && prior_at_ms.is_some() {
                continue;
            }

            let record = PersistedSourceHostElection {
                group_id: group.group_id.0.clone(),
                source_host_device_id: new_source_host.clone(),
                candidate_count,
                elected_at_ms: now_ms,
            };
            self.persistence
                .put_source_host_election(record.clone())
                .await?;
            {
                let mut g = self.inner.lock().await;
                g.elections.insert(group.group_id.0.clone(), record);
            }
            let happening = Happening::SourceHostElected {
                group_id: group.group_id.0.clone(),
                display_name: group.display_name.clone(),
                source_host_device_id: new_source_host.clone(),
                prior_source_host_device_id: prior_source_host,
                candidate_count,
                at: std::time::SystemTime::now(),
            };
            if let Err(e) = self.happenings.emit_durable(happening).await {
                tracing::warn!(
                    error = %e,
                    group_id = %group.group_id.0,
                    "source-host election: emit happening failed"
                );
            }
            tracing::info!(
                group_id = %group.group_id.0,
                display_name = %group.display_name,
                source_host = ?new_source_host,
                candidates = candidate_count,
                "source-host election: transition"
            );
        }
        Ok(())
    }

    /// Compute the live device set. A peer is considered
    /// alive for election when EITHER:
    ///
    /// - its audio-plane connection has any inbound activity
    ///   within [`ElectionConfig::liveness_window`] (the
    ///   strong, in-flight signal — sync probes are the
    ///   always-on clock-domain primitive and their
    ///   responses keep this signal bright through
    ///   playback pauses and long idle sessions), OR
    /// - a TCP-connect-probe against the peer's advertised
    ///   control port succeeds within
    ///   [`ElectionConfig::election_probe_deadline`] (the
    ///   fresh-truth fallback for peers without an
    ///   already-open audio-plane connection — first-boot,
    ///   pre-flow, post-partition reconnect).
    ///
    /// The mDNS-SD `last_seen_ms` field is NOT a primary
    /// signal for election. The discovery library
    /// deduplicates `ServiceResolved` events on
    /// identical-data responses, so the field does not
    /// advance in steady state for stable peers; reading it
    /// would produce stale-truth and election would treat
    /// reachable peers as gone. The TCP-connect-probe is
    /// the authoritative reachability check, mirroring the
    /// roster-snap marauder primitive applied here to the
    /// election layer.
    ///
    /// Probes run concurrently across all candidate peers,
    /// so the election cycle's wall-clock cost is bounded
    /// by the slowest single probe, not the sum of all
    /// probes. On a healthy rig with full audio-plane
    /// connectivity, no probes fire — the channel-activity
    /// path carries every domain-member peer.
    ///
    /// Connection-open alone is NOT a liveness signal — a
    /// TCP socket can remain open through a network
    /// partition long after the remote stops responding.
    /// The channel-activity timestamp
    /// (`last_channel_activity_ms`) is the freshness proof:
    /// it advances on every inbound message (audio frame,
    /// sync response, hello, goodbye, legacy heartbeat), so
    /// a connection whose peer is alive and answering sync
    /// probes stays bright, and a connection whose peer has
    /// disappeared falls silent and triggers a fresh-truth
    /// probe.
    ///
    /// The local node's canonical id is always in the set —
    /// the local node is, by definition, alive to itself.
    async fn live_device_set(&self) -> HashSet<String> {
        let mut set = HashSet::new();
        set.insert(self.local_device_id.0.clone());

        let cutoff = now_ms()
            .saturating_sub(self.config.liveness_window.as_millis() as u64);

        // Single liveness source: audio-plane channel-activity.
        // Available when the audio-plane runtime has been
        // injected via `with_audio_plane`; absent in test
        // rigs that bring up the election runtime in
        // isolation. Peers with an active audio-plane
        // connection whose inbound activity is within the
        // liveness window are by definition operational —
        // the fastest signal because it is the peer's own
        // bytes in transit (50 Hz audio frames + sync probes
        // keep the channel-activity field bright through any
        // in-flight session).
        //
        // Previously the runtime also consumed a heartbeat-
        // based PresenceCorrelator state as a second source.
        // The heartbeat substrate has been retired in favour
        // of chain announces on UDP/5354 (the runtime
        // constructs a dormant HeartbeatRuntime that never
        // starts, so the correlator observed zero heartbeats
        // and classified every peer as Absent). Reading from
        // the dormant correlator was dual-truth wiring with
        // no liveness contribution; election now reads only
        // from the audio-plane channel-activity signal, which
        // covers every peer the framework has an active
        // session with. Peers with no audio-plane connection
        // (no group join, no flow) are not candidates for
        // source-host election regardless.
        if let Some(audio_plane) = self.audio_plane.load_full() {
            for conn in audio_plane.list_connections().await {
                if conn.last_channel_activity_ms >= cutoff {
                    set.insert(conn.remote_device_id);
                }
            }
        }

        set
    }
}

fn is_election_trigger(h: &Happening) -> bool {
    matches!(
        h,
        Happening::PeerDiscovered { .. }
            | Happening::PeerUpdated { .. }
            | Happening::PeerLost { .. }
            | Happening::GroupCreated { .. }
            | Happening::GroupMembershipChanged { .. }
            | Happening::GroupDeleted { .. }
    )
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Pure election function exposed for testing without
/// constructing a full runtime. Computes the elected source-
/// host for one group given the candidate member set + the
/// live device universe.
pub fn elect_source_host(
    members: &[String],
    live: &HashSet<String>,
) -> (Option<String>, u32) {
    let mut candidates: Vec<&str> = members
        .iter()
        .filter(|m| live.contains(*m))
        .map(String::as_str)
        .collect();
    candidates.sort();
    let elected = candidates.first().map(|s| (*s).to_string());
    (elected, candidates.len() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo::persistence::MemoryPersistenceStore;

    fn make_live(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn elect_picks_lowest_id_among_live() {
        let members = vec![
            "z-device".to_string(),
            "a-device".to_string(),
            "m-device".to_string(),
        ];
        let live = make_live(&["a-device", "z-device", "m-device"]);
        let (elected, count) = elect_source_host(&members, &live);
        assert_eq!(elected.as_deref(), Some("a-device"));
        assert_eq!(count, 3);
    }

    #[test]
    fn elect_filters_unlive_candidates() {
        let members = vec!["a-device".to_string(), "b-device".to_string()];
        let live = make_live(&["b-device"]);
        let (elected, count) = elect_source_host(&members, &live);
        assert_eq!(elected.as_deref(), Some("b-device"));
        assert_eq!(count, 1);
    }

    #[test]
    fn elect_returns_none_when_no_candidate_is_live() {
        let members = vec!["a-device".to_string(), "b-device".to_string()];
        let live = make_live(&["c-device"]);
        let (elected, count) = elect_source_host(&members, &live);
        assert_eq!(elected, None);
        assert_eq!(count, 0);
    }

    #[test]
    fn elect_is_deterministic_across_member_order() {
        let live = make_live(&["a", "b", "c"]);
        let m1 = vec!["b".into(), "a".into(), "c".into()];
        let m2 = vec!["c".into(), "a".into(), "b".into()];
        assert_eq!(
            elect_source_host(&m1, &live),
            elect_source_host(&m2, &live)
        );
    }

    fn build_runtime(local_id: &str) -> Arc<ElectionRuntime> {
        let persistence: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::default());
        let bus = Arc::new(HappeningBus::with_capacity(64));
        let groups = Arc::new(GroupStore::new(
            Arc::clone(&persistence),
            Arc::clone(&bus),
        ));
        Arc::new(ElectionRuntime::new(
            persistence,
            bus,
            groups,
            DeviceId(local_id.to_string()),
            ElectionConfig::default(),
        ))
    }

    #[tokio::test]
    async fn evaluate_elects_local_for_self_only_group() {
        let runtime = build_runtime("local-id");
        runtime
            .group_store
            .create("Solo", &["local-id".to_string()])
            .await
            .unwrap();
        runtime.evaluate().await.unwrap();
        let elections = runtime.list().await;
        assert_eq!(elections.len(), 1);
        assert_eq!(
            elections[0].source_host_device_id.as_deref(),
            Some("local-id")
        );
        assert_eq!(elections[0].candidate_count, 1);
    }

    #[tokio::test]
    async fn evaluate_returns_none_when_only_unreachable_peer_in_group() {
        let runtime = build_runtime("local-id");
        runtime
            .group_store
            .create("Remote-only", &["unreachable-peer".to_string()])
            .await
            .unwrap();
        runtime.evaluate().await.unwrap();
        let elections = runtime.list().await;
        assert_eq!(elections.len(), 1);
        assert_eq!(elections[0].source_host_device_id, None);
        assert_eq!(elections[0].candidate_count, 0);
    }

    #[tokio::test]
    async fn evaluate_drops_in_memory_entry_on_group_delete() {
        let runtime = build_runtime("local-id");
        let g = runtime
            .group_store
            .create("Solo", &["local-id".to_string()])
            .await
            .unwrap();
        runtime.evaluate().await.unwrap();
        assert_eq!(runtime.list().await.len(), 1);
        runtime
            .group_store
            .delete(g.group_id.as_str())
            .await
            .unwrap();
        runtime.evaluate().await.unwrap();
        assert!(runtime.list().await.is_empty());
    }

    #[tokio::test]
    async fn evaluate_is_idempotent_on_unchanged_view() {
        let runtime = build_runtime("local-id");
        runtime
            .group_store
            .create("Solo", &["local-id".to_string()])
            .await
            .unwrap();
        runtime.evaluate().await.unwrap();
        let first = runtime.list().await[0].elected_at_ms;
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        runtime.evaluate().await.unwrap();
        let second = runtime.list().await[0].elected_at_ms;
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn rehydrate_loads_substrate_into_memory() {
        let persistence: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::default());
        persistence
            .put_source_host_election(PersistedSourceHostElection {
                group_id: "g1".into(),
                source_host_device_id: Some("d1".into()),
                candidate_count: 1,
                elected_at_ms: 1000,
            })
            .await
            .unwrap();
        let bus = Arc::new(HappeningBus::with_capacity(64));
        let groups = Arc::new(GroupStore::new(
            Arc::clone(&persistence),
            Arc::clone(&bus),
        ));
        let runtime = ElectionRuntime::new(
            persistence,
            bus,
            groups,
            DeviceId("local-id".into()),
            ElectionConfig::default(),
        );
        runtime.rehydrate().await.unwrap();
        let elections = runtime.list().await;
        assert_eq!(elections.len(), 1);
        assert_eq!(elections[0].group_id, "g1");
    }

    #[test]
    fn is_election_trigger_recognises_relevant_happenings() {
        let now = std::time::SystemTime::now();
        assert!(is_election_trigger(&Happening::PeerDiscovered {
            device_id: "x".into(),
            display_name: "y".into(),
            addresses: vec![],
            at: now,
        }));
        assert!(is_election_trigger(&Happening::GroupMembershipChanged {
            group_id: "g".into(),
            display_name: "d".into(),
            members: vec![],
            added: vec![],
            removed: vec![],
            at: now,
        }));
        assert!(!is_election_trigger(&Happening::PluginEvent {
            plugin: "p".into(),
            event_type: "e".into(),
            payload: serde_json::Value::Null,
            at: now,
        }));
    }
}
