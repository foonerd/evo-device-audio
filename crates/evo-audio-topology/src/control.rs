// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Adapters binding this crate's stores to the seams the
//! steward reaches them through.
//!
//! The steward names none of the types below, and none of them
//! hand it anything audio-shaped: a chain arrives and leaves as
//! the wire snapshot it already carries, a role arrives as the
//! opaque token the manifest declared, and a refusal arrives as
//! the two-way distinction the wire already draws.

use std::sync::Arc;

use evo::{
    AudioPolicyControl, AudioRoutingControl, AudioStoreFuture,
    AudioTopologyControl, AudioTopologyRefusal, HardwareProfileControl,
};
use evo_plugin_sdk::contract::audio_routing::AudioRouting;

use crate::audio_policy::{AudioPolicyError, AudioPolicyStore};
use crate::audio_routing::{
    install_audio_routing_forwarder, AudioRoutingRuntime, PluginAudioRole,
};
use crate::audio_topology::{AudioTopologyError, AudioTopologyStore};
use crate::hardware_profile::{HardwareProfileError, HardwareProfileStore};

impl From<AudioTopologyError> for AudioTopologyRefusal {
    fn from(e: AudioTopologyError) -> Self {
        match e {
            // The only refusal the operator can act on: the
            // chain they sent does not satisfy the installed
            // policy. Everything else is this device failing,
            // not the operator asking for the wrong thing.
            AudioTopologyError::Validation(msg) => Self::Validation(msg),
            other => Self::Other(other.to_string()),
        }
    }
}

impl From<AudioPolicyError> for AudioTopologyRefusal {
    fn from(e: AudioPolicyError) -> Self {
        // Neither variant is an operator mistake: a persistence
        // failure or a malformed stored row are both this
        // device failing.
        Self::Other(e.to_string())
    }
}

impl From<HardwareProfileError> for AudioTopologyRefusal {
    fn from(e: HardwareProfileError) -> Self {
        match e {
            // The one refusal the operator can act on: they
            // sent an override that sets nothing.
            HardwareProfileError::EmptyOverride => {
                Self::Validation(e.to_string())
            }
            other => Self::Other(other.to_string()),
        }
    }
}

/// Which routing role a manifest token names.
///
/// Unrecognised tokens resolve to `None` and the plugin gets no
/// routing handle. A manifest can declare anything; this plane
/// answers only for the roles it actually routes.
fn parse_role(token: &str) -> Option<PluginAudioRole> {
    match token {
        "source" => Some(PluginAudioRole::Source),
        "composition" => Some(PluginAudioRole::Composition),
        "delivery" => Some(PluginAudioRole::Delivery),
        _ => None,
    }
}

/// Binds [`AudioTopologyStore`] to [`AudioTopologyControl`].
#[derive(Debug)]
pub struct TopologyControl(Arc<AudioTopologyStore>);

impl TopologyControl {
    /// Wrap a store.
    pub fn new(store: Arc<AudioTopologyStore>) -> Self {
        Self(store)
    }
}

impl AudioTopologyControl for TopologyControl {
    fn get<'a>(
        &'a self,
        target_key: &'a str,
    ) -> AudioStoreFuture<'a, Option<evo::server::ActiveAudioTopology>> {
        Box::pin(async move { Ok(self.0.get(target_key).await?) })
    }

    fn list<'a>(
        &'a self,
    ) -> AudioStoreFuture<'a, Vec<evo::server::ActiveAudioTopology>> {
        Box::pin(async move { Ok(self.0.list().await?) })
    }

    fn publish<'a>(
        &'a self,
        topology: evo::server::ActiveAudioTopology,
        principal: &'a str,
    ) -> AudioStoreFuture<'a, evo::server::ActiveAudioTopology> {
        Box::pin(async move { Ok(self.0.publish(topology, principal).await?) })
    }

    fn clear<'a>(&'a self, target_key: &'a str) -> AudioStoreFuture<'a, ()> {
        Box::pin(async move { Ok(self.0.clear(target_key).await?) })
    }
}

/// Binds [`AudioRoutingRuntime`] to [`AudioRoutingControl`].
#[derive(Debug)]
pub struct RoutingControl(Arc<AudioRoutingRuntime>);

impl RoutingControl {
    /// Wrap a runtime.
    pub fn new(runtime: Arc<AudioRoutingRuntime>) -> Self {
        Self(runtime)
    }
}

impl AudioRoutingControl for RoutingControl {
    fn handle_for_plugin(
        &self,
        plugin_name: &str,
        role: &str,
    ) -> Option<Arc<dyn AudioRouting>> {
        let role = parse_role(role)?;
        Some(self.0.handle_for_plugin(plugin_name, role))
    }

    fn install_forwarder(
        &self,
        local_handle: Arc<dyn AudioRouting>,
        sink: evo::wire_client::AudioRoutingForwarderSink,
        plugin_name: String,
    ) {
        install_audio_routing_forwarder(
            Arc::clone(&self.0),
            local_handle,
            sink,
            plugin_name,
        );
    }
}

/// Binds [`AudioPolicyStore`] to [`AudioPolicyControl`].
#[derive(Debug)]
pub struct PolicyControl(Arc<AudioPolicyStore>);

impl PolicyControl {
    /// Wrap a store.
    pub fn new(store: Arc<AudioPolicyStore>) -> Self {
        Self(store)
    }
}

impl AudioPolicyControl for PolicyControl {
    fn get_policy<'a>(
        &'a self,
        target_key: &'a str,
    ) -> AudioStoreFuture<'a, Option<evo::server::AudioOperatorPolicyRecord>>
    {
        Box::pin(async move { Ok(self.0.get_policy(target_key).await?) })
    }

    fn list_policies<'a>(
        &'a self,
    ) -> AudioStoreFuture<'a, Vec<evo::server::AudioOperatorPolicyRecord>> {
        Box::pin(async move { Ok(self.0.list_policies().await?) })
    }

    fn put_policy<'a>(
        &'a self,
        target_key: &'a str,
        policy: evo::server::OperatorPolicy,
        principal: &'a str,
    ) -> AudioStoreFuture<'a, evo::server::AudioOperatorPolicyRecord> {
        Box::pin(async move {
            Ok(self.0.put_policy(target_key, policy, principal).await?)
        })
    }

    fn clear_policy<'a>(
        &'a self,
        target_key: &'a str,
    ) -> AudioStoreFuture<'a, ()> {
        Box::pin(async move { Ok(self.0.clear_policy(target_key).await?) })
    }

    fn get_volume_mode<'a>(
        &'a self,
        target_key: &'a str,
    ) -> AudioStoreFuture<'a, Option<evo::server::AudioVolumeModeRecord>> {
        Box::pin(async move { Ok(self.0.get_volume_mode(target_key).await?) })
    }

    fn list_volume_modes<'a>(
        &'a self,
    ) -> AudioStoreFuture<'a, Vec<evo::server::AudioVolumeModeRecord>> {
        Box::pin(async move { Ok(self.0.list_volume_modes().await?) })
    }

    fn put_volume_mode<'a>(
        &'a self,
        target_key: &'a str,
        volume_mode: evo::server::VolumeMode,
        principal: &'a str,
    ) -> AudioStoreFuture<'a, evo::server::AudioVolumeModeRecord> {
        Box::pin(async move {
            Ok(self
                .0
                .put_volume_mode(target_key, volume_mode, principal)
                .await?)
        })
    }

    fn clear_volume_mode<'a>(
        &'a self,
        target_key: &'a str,
    ) -> AudioStoreFuture<'a, ()> {
        Box::pin(async move { Ok(self.0.clear_volume_mode(target_key).await?) })
    }
}

/// Binds [`HardwareProfileStore`] to [`HardwareProfileControl`].
#[derive(Debug)]
pub struct HardwareControl(Arc<HardwareProfileStore>);

impl HardwareControl {
    /// Wrap a store.
    pub fn new(store: Arc<HardwareProfileStore>) -> Self {
        Self(store)
    }
}

impl HardwareProfileControl for HardwareControl {
    fn put<'a>(
        &'a self,
        identity: evo::server::HardwareIdentity,
        override_: evo::server::HardwareProfileOverride,
        principal: &'a str,
    ) -> AudioStoreFuture<'a, evo::server::HardwareProfileOverrideRecord> {
        Box::pin(async move {
            Ok(self.0.put_override(identity, override_, principal).await?)
        })
    }

    fn get<'a>(
        &'a self,
        key: &'a str,
    ) -> AudioStoreFuture<'a, Option<evo::server::HardwareProfileOverrideRecord>>
    {
        Box::pin(async move { Ok(self.0.get_override(key).await?) })
    }

    fn list<'a>(
        &'a self,
    ) -> AudioStoreFuture<'a, Vec<evo::server::HardwareProfileOverrideRecord>>
    {
        Box::pin(async move { Ok(self.0.list_overrides().await?) })
    }

    fn clear<'a>(&'a self, key: &'a str) -> AudioStoreFuture<'a, ()> {
        Box::pin(async move { Ok(self.0.clear_override(key).await?) })
    }
}
