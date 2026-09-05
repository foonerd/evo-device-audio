// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Active audio topology — operator-visible signal-path
//! snapshot per delivery target.
//!
//! The framework owns the publish primitive + persistence +
//! propagation; the vendor distribution drives the actual
//! chain decision (which source / composition / delivery
//! plugins to wire, what OS substrate to use, what format to
//! negotiate). The vendor pushes a complete
//! [`ActiveAudioTopology`] snapshot through the framework's
//! `publish_active_audio_topology` wire op; the framework:
//!
//! 1. Validates the snapshot's basic shape.
//! 2. Computes the `ScoreBreakdown` via
//!    [`crate::topology_scoring::score_topology`] when the
//!    vendor did not supply one (vendor-supplied breakdowns
//!    are accepted verbatim because the vendor has visibility
//!    into the full hardware profile).
//! 3. Persists the snapshot.
//! 4. Emits a typed `Happening::AudioTopologyChanged` so the
//!    operator UI subscribes to live updates.
//! 5. Propagates each chain stage's resolved endpoint to that
//!    plugin's [`crate::audio_routing::AudioRoutingRuntime`]
//!    handle so the plugin's
//!    [`evo_plugin_sdk::contract::audio_routing::AudioRouting`]
//!    methods return the new endpoint.
//! 6. Lands an audit-ledger entry under the operations
//!    control plane.
//!
//! ## Operator-visible schema
//!
//! Operator-visible signal-path schema — chain stages with
//! format at each stage, volume mode + position + dB,
//! bit-perfect verdict, score breakdown, implicit-conversions
//! and warnings lists. The operator UI renders Roon-style
//! signal-path with honest per-stage breakdown.

use std::sync::Arc;

use crate::audio_routing::{
    AudioRoutingRuntime, PluginAudioRole, ResolvedRouting,
};
use evo::persistence::{
    PersistedAudioActiveTopology, PersistenceError, PersistenceStore,
};
use evo::server::{ActiveAudioTopology, ActiveChainStage, VolumeMode};

use evo_plugin_sdk::contract::audio_routing::{ReadEndpoint, WriteEndpoint};

/// Errors raised by [`AudioTopologyStore`].
#[derive(Debug, thiserror::Error)]
pub enum AudioTopologyError {
    /// Underlying persistence layer error.
    #[error("persistence error: {0}")]
    Persistence(#[from] PersistenceError),
    /// Snapshot failed validation (empty chain, wrong stage
    /// order, out-of-range volume position, etc.).
    #[error("topology validation: {0}")]
    Validation(String),
    /// JSON serialise / deserialise failure.
    #[error("topology serde error: {0}")]
    Serde(String),
}

/// Persistence-backed accessor for the active audio topology
/// substrate. Wraps `Arc<dyn PersistenceStore>` plus the
/// shared [`AudioRoutingRuntime`] so the publish path
/// propagates resolved endpoints to the plugin handles
/// atomically with the substrate write.
#[derive(Debug, Clone)]
pub struct AudioTopologyStore {
    persistence: Arc<dyn PersistenceStore>,
    routing: Arc<AudioRoutingRuntime>,
}

impl AudioTopologyStore {
    /// Construct a store. Holds Arc clones of the persistence
    /// handle + the audio routing runtime so the publish path
    /// updates both substrates without a separate plumbing
    /// hop.
    pub fn new(
        persistence: Arc<dyn PersistenceStore>,
        routing: Arc<AudioRoutingRuntime>,
    ) -> Self {
        Self {
            persistence,
            routing,
        }
    }

    /// Publish a topology snapshot for one delivery target.
    /// Validates the snapshot, persists it, and propagates
    /// each chain stage's resolved endpoint to the
    /// corresponding plugin's
    /// [`AudioRoutingRuntime::publish_topology`] entry. The
    /// caller is responsible for emitting the
    /// `Happening::AudioTopologyChanged` (the wire-op
    /// handler does it after this method returns).
    pub async fn publish(
        &self,
        topology: ActiveAudioTopology,
        principal: &str,
    ) -> Result<ActiveAudioTopology, AudioTopologyError> {
        topology_validate(&topology)?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let topology_json = serde_json::to_string(&topology).map_err(|e| {
            AudioTopologyError::Serde(format!(
                "serialise ActiveAudioTopology: {e}"
            ))
        })?;
        let record = PersistedAudioActiveTopology {
            target_key: topology.target_key.clone(),
            topology_json,
            published_at_ms: now_ms,
            published_by_principal: principal.to_string(),
        };
        self.persistence.put_audio_active_topology(record).await?;

        // Propagate per-stage resolved routing to each
        // plugin's AudioRouting handle.
        for stage in &topology.chain {
            let resolved = stage_to_resolved_routing(stage, &topology);
            self.routing.publish_topology(stage_plugin(stage), resolved);
        }

        Ok(topology)
    }

    /// Fetch the active topology for one delivery target.
    /// Returns `None` when nothing is published.
    pub async fn get(
        &self,
        target_key: &str,
    ) -> Result<Option<ActiveAudioTopology>, AudioTopologyError> {
        let row = self
            .persistence
            .get_audio_active_topology(target_key)
            .await?;
        let Some(row) = row else { return Ok(None) };
        let topology: ActiveAudioTopology =
            serde_json::from_str(&row.topology_json).map_err(|e| {
                AudioTopologyError::Serde(format!(
                    "deserialise ActiveAudioTopology: {e}"
                ))
            })?;
        Ok(Some(topology))
    }

    /// List every recorded active-topology snapshot.
    pub async fn list(
        &self,
    ) -> Result<Vec<ActiveAudioTopology>, AudioTopologyError> {
        let rows = self.persistence.list_audio_active_topologies().await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let t: ActiveAudioTopology =
                serde_json::from_str(&row.topology_json).map_err(|e| {
                    AudioTopologyError::Serde(format!(
                        "deserialise ActiveAudioTopology for {tk:?}: {e}",
                        tk = row.target_key
                    ))
                })?;
            out.push(t);
        }
        Ok(out)
    }

    /// Clear the active topology for one delivery target.
    /// Idempotent on absent targets. Also clears the
    /// resolved-routing entries on the per-stage plugin
    /// handles so subsequent `AudioRouting` calls return
    /// `EndpointNotConfigured`.
    pub async fn clear(
        &self,
        target_key: &str,
    ) -> Result<(), AudioTopologyError> {
        if let Some(existing) = self.get(target_key).await? {
            for stage in &existing.chain {
                self.routing.clear_topology(stage_plugin(stage));
            }
        }
        self.persistence
            .delete_audio_active_topology(target_key)
            .await?;
        Ok(())
    }
}

/// Project one chain stage onto the
/// [`crate::audio_routing::ResolvedRouting`] shape the
/// per-plugin AudioRouting handle returns.
fn stage_to_resolved_routing(
    stage: &ActiveChainStage,
    topology: &ActiveAudioTopology,
) -> ResolvedRouting {
    match stage {
        ActiveChainStage::Source {
            format,
            endpoint_kind,
            endpoint_path,
            ..
        } => ResolvedRouting {
            write: Some(WriteEndpoint {
                kind: *endpoint_kind,
                path: endpoint_path.clone(),
                format: format.clone(),
                buffer_frames: 0,
            }),
            read: None,
            format: format.clone(),
            reason: format!(
                "active topology published for {tk}",
                tk = topology.target_key
            ),
        },
        ActiveChainStage::Composition {
            format_in,
            format_out,
            endpoint_in_kind,
            endpoint_in_path,
            endpoint_out_kind,
            endpoint_out_path,
            ..
        } => ResolvedRouting {
            write: Some(WriteEndpoint {
                kind: *endpoint_out_kind,
                path: endpoint_out_path.clone(),
                format: format_out.clone(),
                buffer_frames: 0,
            }),
            read: Some(ReadEndpoint {
                kind: *endpoint_in_kind,
                path: endpoint_in_path.clone(),
                format: format_in.clone(),
                buffer_frames: 0,
            }),
            format: format_out.clone(),
            reason: format!(
                "active topology published for {tk}",
                tk = topology.target_key
            ),
        },
        ActiveChainStage::Delivery {
            format,
            endpoint_kind,
            endpoint_path,
            ..
        } => ResolvedRouting {
            write: None,
            read: Some(ReadEndpoint {
                kind: *endpoint_kind,
                path: endpoint_path.clone(),
                format: format.clone(),
                buffer_frames: 0,
            }),
            format: format.clone(),
            reason: format!(
                "active topology published for {tk}",
                tk = topology.target_key
            ),
        },
    }
}

// Behaviour that used to hang off the DTOs. The types are the
// steward's wire shape; deciding whether a chain is valid is
// this cluster's job, so it lives here as free functions over
// them rather than as inherent impls the steward would carry.

/// Returns the canonical plugin name for this stage.
pub fn stage_plugin(this: &ActiveChainStage) -> &str {
    match this {
        ActiveChainStage::Source { plugin, .. }
        | ActiveChainStage::Composition { plugin, .. }
        | ActiveChainStage::Delivery { plugin, .. } => plugin,
    }
}

/// Returns the [`PluginAudioRole`] this stage represents,
/// used by the topology publisher to mint
/// [`AudioRoutingRuntime::handle_for_plugin`] handles when
/// propagating endpoints.
pub fn stage_role(this: &ActiveChainStage) -> PluginAudioRole {
    match this {
        ActiveChainStage::Source { .. } => PluginAudioRole::Source,
        ActiveChainStage::Composition { .. } => PluginAudioRole::Composition,
        ActiveChainStage::Delivery { .. } => PluginAudioRole::Delivery,
    }
}

/// Validate the topology's basic shape. Called by
/// [`AudioTopologyStore::publish`] before any persistence
/// / propagation.
pub fn topology_validate(
    this: &ActiveAudioTopology,
) -> Result<(), AudioTopologyError> {
    if this.target_key.trim().is_empty() {
        return Err(AudioTopologyError::Validation(
            "target_key must not be empty".into(),
        ));
    }
    if this.chain.is_empty() {
        return Err(AudioTopologyError::Validation(
            "chain must not be empty".into(),
        ));
    }
    if this.chain.len() < 2 {
        return Err(AudioTopologyError::Validation(
            "chain must have at least source + delivery (2 stages)".into(),
        ));
    }
    if !matches!(this.chain.first(), Some(ActiveChainStage::Source { .. })) {
        return Err(AudioTopologyError::Validation(
            "chain must begin with a Source stage".into(),
        ));
    }
    if !matches!(this.chain.last(), Some(ActiveChainStage::Delivery { .. })) {
        return Err(AudioTopologyError::Validation(
            "chain must end with a Delivery stage".into(),
        ));
    }
    // Intermediate stages must all be Composition.
    for (idx, stage) in this.chain.iter().enumerate() {
        if idx == 0 || idx == this.chain.len() - 1 {
            continue;
        }
        if !matches!(stage, ActiveChainStage::Composition { .. }) {
            return Err(AudioTopologyError::Validation(format!(
                "chain stage {idx} (intermediate) must be a \
                 Composition stage"
            )));
        }
    }
    // Volume position constrained to [0.0, 1.0] when set.
    if let Some(p) = this.volume_position {
        if !(0.0..=1.0).contains(&p) {
            return Err(AudioTopologyError::Validation(format!(
                "volume_position {p} outside [0.0, 1.0]"
            )));
        }
    }
    // No volume_position when the chain has no volume
    // control.
    if matches!(this.volume_mode, VolumeMode::None)
        && this.volume_position.is_some()
    {
        return Err(AudioTopologyError::Validation(
            "volume_position must be None when volume_mode is None".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_routing::AudioRoutingRuntime;
    use evo::persistence::MemoryPersistenceStore;
    use evo::server::ScoreBreakdown;
    use evo_plugin_sdk::audio::AudioFormat;
    use evo_plugin_sdk::audio::PcmCodec;
    use evo_plugin_sdk::contract::audio_routing::EndpointKind;
    use std::path::PathBuf;

    fn pcm_192() -> AudioFormat {
        AudioFormat::Pcm {
            codec: PcmCodec::PcmS24Le,
            rate_hz: 192_000,
            channels: 2,
        }
    }

    fn passthrough_topology() -> ActiveAudioTopology {
        ActiveAudioTopology {
            target_key: "usb:vid=0x21b4,pid=0x0096".into(),
            display_name: "DragonFly Cobalt".into(),
            chain: vec![
                ActiveChainStage::Source {
                    plugin: "com.tidal.streaming".into(),
                    format: pcm_192(),
                    endpoint_kind: EndpointKind::AlsaPcm,
                    endpoint_path: PathBuf::from("loopback:0,0"),
                },
                ActiveChainStage::Delivery {
                    plugin: "org.evoframework.delivery.alsa".into(),
                    format: pcm_192(),
                    endpoint_kind: EndpointKind::AlsaPcm,
                    endpoint_path: PathBuf::from("hw:0,0"),
                },
            ],
            volume_mode: VolumeMode::Hardware,
            volume_position: Some(0.67),
            volume_db: Some(-22.0),
            bit_perfect: true,
            score: ScoreBreakdown {
                total: 100,
                bit_perfect: 50,
                native_rate_match: 20,
                native_format_match: 15,
                minimum_signal_path: 10,
                hardware_volume_engaged: 5,
                ..Default::default()
            },
            implicit_conversions: vec![],
            warnings: vec![],
        }
    }

    fn store() -> AudioTopologyStore {
        let persistence: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::new());
        let routing = Arc::new(AudioRoutingRuntime::new());
        AudioTopologyStore::new(persistence, routing)
    }

    #[tokio::test]
    async fn publish_then_get_round_trips() {
        let s = store();
        let t = passthrough_topology();
        s.publish(t.clone(), "user:1000").await.unwrap();
        let got = s.get(&t.target_key).await.unwrap().unwrap();
        assert_eq!(got, t);
    }

    #[tokio::test]
    async fn publish_propagates_to_audio_routing_runtime() {
        // Mint handles for both chain stages BEFORE publish so
        // the AudioRouting trait calls return the resolved
        // endpoints after publish.
        let persistence: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::new());
        let routing = Arc::new(AudioRoutingRuntime::new());
        let source_handle = routing
            .handle_for_plugin("com.tidal.streaming", PluginAudioRole::Source);
        let delivery_handle = routing.handle_for_plugin(
            "org.evoframework.delivery.alsa",
            PluginAudioRole::Delivery,
        );
        let s = AudioTopologyStore::new(persistence, routing);
        s.publish(passthrough_topology(), "user:1000")
            .await
            .unwrap();

        let we = source_handle.write_endpoint().expect("source published");
        assert_eq!(we.kind, EndpointKind::AlsaPcm);
        assert_eq!(we.path, PathBuf::from("loopback:0,0"));
        let re = delivery_handle.read_endpoint().expect("delivery published");
        assert_eq!(re.kind, EndpointKind::AlsaPcm);
        assert_eq!(re.path, PathBuf::from("hw:0,0"));
    }

    #[tokio::test]
    async fn publish_is_idempotent_on_target() {
        let s = store();
        let mut t = passthrough_topology();
        s.publish(t.clone(), "alice").await.unwrap();
        t.warnings = vec!["second publish".into()];
        s.publish(t.clone(), "bob").await.unwrap();
        let all = s.list().await.unwrap();
        assert_eq!(all.len(), 1, "no duplicate row");
        assert_eq!(all[0].warnings, vec!["second publish".to_string()]);
    }

    #[tokio::test]
    async fn validation_refuses_empty_chain() {
        let s = store();
        let mut t = passthrough_topology();
        t.chain = vec![];
        let err = s.publish(t, "alice").await.expect_err("empty chain");
        assert!(matches!(err, AudioTopologyError::Validation(_)));
    }

    #[tokio::test]
    async fn validation_refuses_chain_without_source_first() {
        let s = store();
        let t = ActiveAudioTopology {
            chain: vec![
                ActiveChainStage::Delivery {
                    plugin: "delivery".into(),
                    format: pcm_192(),
                    endpoint_kind: EndpointKind::AlsaPcm,
                    endpoint_path: PathBuf::from("hw:0,0"),
                },
                ActiveChainStage::Source {
                    plugin: "source".into(),
                    format: pcm_192(),
                    endpoint_kind: EndpointKind::AlsaPcm,
                    endpoint_path: PathBuf::from("loopback:0,0"),
                },
            ],
            ..passthrough_topology()
        };
        let err = s.publish(t, "alice").await.expect_err("wrong order");
        assert!(matches!(err, AudioTopologyError::Validation(_)));
    }

    #[tokio::test]
    async fn validation_refuses_volume_position_out_of_range() {
        let s = store();
        let mut t = passthrough_topology();
        t.volume_position = Some(1.5);
        let err = s.publish(t, "alice").await.expect_err("out-of-range");
        assert!(
            matches!(err, AudioTopologyError::Validation(msg) if msg.contains("volume_position"))
        );
    }

    #[tokio::test]
    async fn validation_refuses_volume_position_when_mode_is_none() {
        let s = store();
        let mut t = passthrough_topology();
        t.volume_mode = VolumeMode::None;
        // volume_position still Some(0.67) from passthrough_topology().
        let err = s.publish(t, "alice").await.expect_err("incoherent");
        assert!(
            matches!(err, AudioTopologyError::Validation(msg) if msg.contains("volume_position"))
        );
    }

    #[tokio::test]
    async fn clear_removes_topology_and_clears_routing_handles() {
        let persistence: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::new());
        let routing = Arc::new(AudioRoutingRuntime::new());
        let source_handle = routing
            .handle_for_plugin("com.tidal.streaming", PluginAudioRole::Source);
        let s = AudioTopologyStore::new(
            Arc::clone(&persistence),
            Arc::clone(&routing),
        );
        s.publish(passthrough_topology(), "alice").await.unwrap();
        source_handle.write_endpoint().expect("published");
        s.clear("usb:vid=0x21b4,pid=0x0096").await.unwrap();
        let got = s.get("usb:vid=0x21b4,pid=0x0096").await.unwrap();
        assert!(got.is_none());
        // Source handle now returns EndpointNotConfigured
        // because clear_topology cleared the runtime registry
        // entry.
        let err = source_handle
            .write_endpoint()
            .expect_err("clear should clear routing handles");
        use evo_plugin_sdk::contract::audio_routing::AudioRoutingError;
        assert_eq!(err, AudioRoutingError::EndpointNotConfigured);
    }

    #[tokio::test]
    async fn list_returns_every_recorded_in_key_order() {
        let s = store();
        let mut t1 = passthrough_topology();
        t1.target_key = "alsa:HDA-Intel".into();
        let mut t2 = passthrough_topology();
        t2.target_key = "usb:vid=0x1234,pid=0x0001".into();
        // Adjust chain plugin names so they don't collide on
        // the routing runtime publish_topology call.
        if let ActiveChainStage::Source { plugin, .. } = &mut t2.chain[0] {
            *plugin = "com.example.source.b".into();
        }
        if let ActiveChainStage::Delivery { plugin, .. } = &mut t2.chain[1] {
            *plugin = "com.example.delivery.b".into();
        }
        s.publish(t1, "alice").await.unwrap();
        s.publish(t2, "alice").await.unwrap();
        let all = s.list().await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].target_key, "alsa:HDA-Intel");
        assert_eq!(all[1].target_key, "usb:vid=0x1234,pid=0x0001");
    }

    #[test]
    fn topology_round_trips_through_serde() {
        let t = passthrough_topology();
        let json = serde_json::to_string(&t).unwrap();
        let parsed: ActiveAudioTopology = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, t);
    }

    #[test]
    fn composition_stage_round_trips_through_serde() {
        let stage = ActiveChainStage::Composition {
            plugin: "org.evoframework.composition.alsa".into(),
            mode: "passthrough".into(),
            format_in: pcm_192(),
            format_out: pcm_192(),
            endpoint_in_kind: EndpointKind::AlsaPcm,
            endpoint_in_path: PathBuf::from("loopback:0,0"),
            endpoint_out_kind: EndpointKind::AlsaPcm,
            endpoint_out_path: PathBuf::from("loopback:1,0"),
        };
        let json = serde_json::to_string(&stage).unwrap();
        let parsed: ActiveChainStage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, stage);
    }
}
