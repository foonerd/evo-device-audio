// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Audio data plane routing — framework-side runtime broker.
//!
//! The framework configures audio chain topology — picks the
//! OS substrate (ALSA loopback / pipe / shm / JACK), creates
//! the endpoint, negotiates the format — and exposes the
//! resulting endpoint to each chain stage's plugin via the
//! [`evo_plugin_sdk::contract::audio_routing::AudioRouting`]
//! trait.
//!
//! ## What this module owns
//!
//! [`AudioRoutingRuntime`] is the framework-side broker. The
//! admission engine constructs one at boot, calls
//! [`AudioRoutingRuntime::handle_for_plugin`] for each
//! audio-capable plugin during admission to mint a per-plugin
//! handle that gets stamped on the plugin's
//! [`evo_plugin_sdk::contract::LoadContext::audio_routing`]
//! field, and (in sub-primitive F) the reconciliation engine
//! calls [`AudioRoutingRuntime::publish_topology`] when it
//! has resolved a new chain shape.
//!
//! [`RouterAudioRouting`] is the per-plugin wrapper. Each
//! audio-capable plugin gets one (with its canonical name +
//! its declared role — Source / Composition / Delivery
//! baked in). Calls to the trait methods route through the
//! shared runtime to fetch the resolved endpoint for that
//! plugin's stage.
//!
//! Audio bytes do NOT traverse this module. The trait returns
//! an endpoint identifier (path / port / shm region); the
//! plugin opens the OS primitive directly.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use evo_plugin_sdk::audio::AudioFormat;
use evo_plugin_sdk::contract::audio_routing::{
    AudioRouting, AudioRoutingError, AudioRoutingMethod, CompositionEndpoints,
    ReadEndpoint, RouteChange, RouteChangeCallback, WriteEndpoint,
};

/// Plugin's role in the audio chain — source / composition /
/// delivery. Used by the runtime to select which trait method
/// is meaningful for the plugin's stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginAudioRole {
    /// Source plugin — produces audio bytes into a
    /// [`WriteEndpoint`].
    Source,
    /// Composition plugin — reads from a [`ReadEndpoint`] and
    /// writes to a [`WriteEndpoint`].
    Composition,
    /// Delivery plugin — consumes audio bytes from a
    /// [`ReadEndpoint`] and writes to the underlying DAC.
    Delivery,
}

/// Resolved topology for one plugin — the endpoints + format
/// the framework has configured for that plugin's chain
/// stage. The reconciliation engine constructs this and hands
/// it to [`AudioRoutingRuntime::publish_topology`] on every
/// chain rewire.
#[derive(Debug, Clone)]
pub struct ResolvedRouting {
    /// Source-side write endpoint. Populated when the plugin
    /// is a [`PluginAudioRole::Source`] or
    /// [`PluginAudioRole::Composition`] (composition's
    /// output side).
    pub write: Option<WriteEndpoint>,
    /// Delivery-side read endpoint. Populated when the plugin
    /// is a [`PluginAudioRole::Delivery`] or
    /// [`PluginAudioRole::Composition`] (composition's input
    /// side).
    pub read: Option<ReadEndpoint>,
    /// Negotiated format for the plugin's chain stage.
    pub format: AudioFormat,
    /// Free-form operator-readable reason for this rewire.
    pub reason: String,
}

/// Internal per-plugin state held in the runtime's registry.
#[derive(Default)]
struct PluginRoutingState {
    /// The most recently published [`ResolvedRouting`]. `None`
    /// means the framework has not yet configured a topology
    /// for this plugin.
    resolved: Option<ResolvedRouting>,
    /// The plugin's currently-registered route-change
    /// callback. `None` means the plugin has not registered
    /// one (or has explicitly cleared it).
    on_change: Option<RouteChangeCallback>,
}

impl std::fmt::Debug for PluginRoutingState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginRoutingState")
            .field("resolved", &self.resolved)
            .field("on_change", &self.on_change.as_ref().map(|_| "<callback>"))
            .finish()
    }
}

/// Framework-side broker for audio-data-plane routing. One
/// instance per steward; the admission engine clones the Arc
/// for every audio-capable plugin admission and the
/// reconciliation engine clones the Arc for every topology
/// rewire.
#[derive(Debug, Default)]
pub struct AudioRoutingRuntime {
    /// Per-plugin routing state, keyed by canonical plugin
    /// name. Held behind a `RwLock` because the read path
    /// (trait method calls) is hot relative to the write path
    /// (topology rewires).
    plugins: RwLock<HashMap<String, PluginRoutingState>>,
}

impl AudioRoutingRuntime {
    /// Construct an empty runtime. The reconciliation engine
    /// populates it via [`Self::publish_topology`] as it
    /// resolves chain shapes.
    pub fn new() -> Self {
        Self {
            plugins: RwLock::new(HashMap::new()),
        }
    }

    /// Mint a per-plugin [`AudioRouting`] handle for the
    /// supplied plugin + role. Stamped on the plugin's
    /// LoadContext at admission time. Subsequent calls for
    /// the same `plugin_name` return new handle Arcs that
    /// share the same per-plugin state — re-admission of a
    /// plugin (after Live reload) does not lose the
    /// previously-resolved topology.
    pub fn handle_for_plugin(
        self: &Arc<Self>,
        plugin_name: &str,
        role: PluginAudioRole,
    ) -> Arc<dyn AudioRouting> {
        // Ensure an entry exists in the registry so trait
        // calls before the first publish_topology see the
        // EndpointNotConfigured shape rather than panicking.
        {
            let mut guard = self.plugins.write().expect("plugins lock");
            guard.entry(plugin_name.to_string()).or_default();
        }
        Arc::new(RouterAudioRouting {
            runtime: Arc::clone(self),
            plugin_name: plugin_name.to_string(),
            role,
        })
    }

    /// Publish a resolved topology for one plugin. The
    /// reconciliation engine calls this on every chain
    /// rewire; the runtime stores the resolution and (if the
    /// plugin has registered one) fires the route-change
    /// callback synchronously.
    ///
    /// Returns the previous resolved routing (if any) so the
    /// caller can audit / log the transition.
    pub fn publish_topology(
        &self,
        plugin_name: &str,
        resolved: ResolvedRouting,
    ) -> Option<ResolvedRouting> {
        let (previous, callback) = {
            let mut guard = self.plugins.write().expect("plugins lock");
            let entry = guard.entry(plugin_name.to_string()).or_default();
            let previous = entry.resolved.replace(resolved.clone());
            (previous, entry.on_change.clone())
        };
        if let Some(cb) = callback {
            cb(&RouteChange {
                new_format: resolved.format.clone(),
                reason: resolved.reason.clone(),
            });
        }
        previous
    }

    /// Clear the resolved topology for one plugin. Used on
    /// plugin unload / disable / topology teardown so a
    /// later trait call sees `EndpointNotConfigured` rather
    /// than stale data.
    pub fn clear_topology(&self, plugin_name: &str) {
        let mut guard = self.plugins.write().expect("plugins lock");
        if let Some(entry) = guard.get_mut(plugin_name) {
            entry.resolved = None;
        }
    }

    /// Forget a plugin's registry entry entirely (callback +
    /// resolved topology). Used on plugin uninstall.
    pub fn forget_plugin(&self, plugin_name: &str) {
        let mut guard = self.plugins.write().expect("plugins lock");
        guard.remove(plugin_name);
    }

    /// Read-only access to a plugin's resolved routing —
    /// surfaces the current topology for diagnostics. Returns
    /// `None` when no topology is published.
    pub fn resolved_for(&self, plugin_name: &str) -> Option<ResolvedRouting> {
        let guard = self.plugins.read().expect("plugins lock");
        guard.get(plugin_name).and_then(|s| s.resolved.clone())
    }
}

/// Project the framework's [`ResolvedRouting`] onto the SDK's
/// wire-shaped equivalent. The wire frame carries the rewire
/// `reason` separately so the SDK type omits it.
fn resolved_to_wire(
    r: ResolvedRouting,
) -> evo_plugin_sdk::contract::audio_routing::ResolvedRouting {
    evo_plugin_sdk::contract::audio_routing::ResolvedRouting {
        write: r.write,
        read: r.read,
        format: r.format,
    }
}

/// Install the OOP-admission audio-routing state-change
/// forwarder for a freshly-admitted plugin.
///
/// Out-of-process plugins receive their `AudioRouting` handle
/// from the SDK's `WireAudioRouting` proxy, not from the
/// framework's local `RouterAudioRouting` — the local handle
/// in the steward's `LoadContext.audio_routing` slot exists
/// solely to act as the framework's edge of the forwarder.
/// This function attaches that edge:
///
/// 1. Registers a callback on the local handle so every
///    [`Self::publish_topology`] hit fans out across the wire
///    as a [`evo_plugin_sdk::wire::WireFrame::AudioRoutingStateChanged`]
///    frame. The SDK proxy's reader loop ingests the frame,
///    updates its cache, and fires the plugin's registered
///    `on_route_change` callback.
/// 2. Sends one initial state-change frame if reconciliation
///    has already published a topology for this plugin. A
///    plugin admitted before any rewire sees no initial push;
///    its proxy stays in `EndpointNotConfigured` until the
///    first `publish_topology` call.
///
/// The forwarder is `Fn + Send + Sync + 'static` and stays
/// installed for the lifetime of the plugin. Plugin unload
/// drops the SDK side; the local handle's callback slot
/// remains populated but harmlessly pushes onto a closed
/// channel (the `WireClient`'s outbound sender is dropped on
/// adapter teardown), which surfaces as the warning log in
/// [`evo::wire_client::AudioRoutingForwarderSink::push`].
pub fn install_audio_routing_forwarder(
    runtime: Arc<AudioRoutingRuntime>,
    local_handle: Arc<dyn AudioRouting>,
    sink: evo::wire_client::AudioRoutingForwarderSink,
    plugin_name: String,
) {
    let runtime_cb = Arc::clone(&runtime);
    let plugin_for_cb = plugin_name.clone();
    let sink_for_cb = sink.clone();
    local_handle.on_route_change(Some(Arc::new(
        move |change: &RouteChange| {
            // The runtime has already written the new resolved
            // routing into its entry before firing this callback;
            // re-reading it is the simplest way to obtain the full
            // snapshot (RouteChange only carries the format + the
            // operator-readable reason). Race-free: publish_topology
            // takes the write lock, updates the resolved field,
            // releases the lock, then fires the callback — we
            // read after that.
            let snapshot = runtime_cb.resolved_for(&plugin_for_cb);
            let sdk_resolved = snapshot.map(resolved_to_wire);
            sink_for_cb.push(sdk_resolved, change.reason.clone());
        },
    )));

    if let Some(initial) = runtime.resolved_for(&plugin_name) {
        let reason = initial.reason.clone();
        let sdk_resolved = resolved_to_wire(initial);
        sink.push(Some(sdk_resolved), reason);
    }
}

/// Per-plugin handle stamped on
/// [`evo_plugin_sdk::contract::LoadContext::audio_routing`].
/// Routes trait calls through the shared runtime to fetch the
/// resolved endpoint for the plugin's chain stage.
#[derive(Debug)]
pub struct RouterAudioRouting {
    runtime: Arc<AudioRoutingRuntime>,
    plugin_name: String,
    role: PluginAudioRole,
}

impl RouterAudioRouting {
    fn resolved(&self) -> Result<ResolvedRouting, AudioRoutingError> {
        self.runtime
            .resolved_for(&self.plugin_name)
            .ok_or(AudioRoutingError::EndpointNotConfigured)
    }
}

impl AudioRouting for RouterAudioRouting {
    fn write_endpoint(&self) -> Result<WriteEndpoint, AudioRoutingError> {
        if !matches!(
            self.role,
            PluginAudioRole::Source | PluginAudioRole::Composition
        ) {
            return Err(AudioRoutingError::WrongStage {
                kind: AudioRoutingMethod::WriteEndpoint,
            });
        }
        let r = self.resolved()?;
        r.write.ok_or(AudioRoutingError::EndpointNotConfigured)
    }

    fn read_endpoint(&self) -> Result<ReadEndpoint, AudioRoutingError> {
        if !matches!(
            self.role,
            PluginAudioRole::Delivery | PluginAudioRole::Composition
        ) {
            return Err(AudioRoutingError::WrongStage {
                kind: AudioRoutingMethod::ReadEndpoint,
            });
        }
        let r = self.resolved()?;
        r.read.ok_or(AudioRoutingError::EndpointNotConfigured)
    }

    fn composition_endpoints(
        &self,
    ) -> Result<CompositionEndpoints, AudioRoutingError> {
        if !matches!(self.role, PluginAudioRole::Composition) {
            return Err(AudioRoutingError::NotCompositionPlugin);
        }
        let r = self.resolved()?;
        let read = r.read.ok_or(AudioRoutingError::EndpointNotConfigured)?;
        let write = r.write.ok_or(AudioRoutingError::EndpointNotConfigured)?;
        Ok(CompositionEndpoints {
            input: read,
            output: write,
        })
    }

    fn current_format(&self) -> Result<AudioFormat, AudioRoutingError> {
        Ok(self.resolved()?.format)
    }

    fn on_route_change(&self, callback: Option<RouteChangeCallback>) {
        let mut guard = self.runtime.plugins.write().expect("plugins lock");
        if let Some(entry) = guard.get_mut(&self.plugin_name) {
            entry.on_change = callback;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_plugin_sdk::audio::PcmCodec;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn pcm() -> AudioFormat {
        AudioFormat::Pcm {
            codec: PcmCodec::PcmS24Le,
            rate_hz: 192_000,
            channels: 2,
        }
    }

    fn we() -> WriteEndpoint {
        WriteEndpoint {
            kind:
                evo_plugin_sdk::contract::audio_routing::EndpointKind::AlsaPcm,
            path: PathBuf::from("hw:0,0"),
            format: pcm(),
            buffer_frames: 1024,
        }
    }

    fn re_() -> ReadEndpoint {
        ReadEndpoint {
            kind:
                evo_plugin_sdk::contract::audio_routing::EndpointKind::AlsaPcm,
            path: PathBuf::from("loopback:0,0"),
            format: pcm(),
            buffer_frames: 1024,
        }
    }

    #[test]
    fn handle_before_publish_returns_endpoint_not_configured() {
        let rt = Arc::new(AudioRoutingRuntime::new());
        let h =
            rt.handle_for_plugin("com.example.source", PluginAudioRole::Source);
        let err = h.write_endpoint().expect_err("no topology published yet");
        assert_eq!(err, AudioRoutingError::EndpointNotConfigured);
    }

    #[test]
    fn source_publish_then_write_endpoint_round_trips() {
        let rt = Arc::new(AudioRoutingRuntime::new());
        let h =
            rt.handle_for_plugin("com.example.source", PluginAudioRole::Source);
        rt.publish_topology(
            "com.example.source",
            ResolvedRouting {
                write: Some(we()),
                read: None,
                format: pcm(),
                reason: "first publish".into(),
            },
        );
        let got = h.write_endpoint().expect("published");
        assert_eq!(got, we());
    }

    #[test]
    fn delivery_publish_then_read_endpoint_round_trips() {
        let rt = Arc::new(AudioRoutingRuntime::new());
        let h = rt.handle_for_plugin(
            "com.example.delivery",
            PluginAudioRole::Delivery,
        );
        rt.publish_topology(
            "com.example.delivery",
            ResolvedRouting {
                write: None,
                read: Some(re_()),
                format: pcm(),
                reason: "first publish".into(),
            },
        );
        let got = h.read_endpoint().expect("published");
        assert_eq!(got, re_());
    }

    #[test]
    fn composition_publish_then_endpoints_pair_round_trips() {
        let rt = Arc::new(AudioRoutingRuntime::new());
        let h = rt.handle_for_plugin(
            "com.example.composition",
            PluginAudioRole::Composition,
        );
        rt.publish_topology(
            "com.example.composition",
            ResolvedRouting {
                write: Some(we()),
                read: Some(re_()),
                format: pcm(),
                reason: "first publish".into(),
            },
        );
        let got = h.composition_endpoints().expect("published");
        assert_eq!(got.input, re_());
        assert_eq!(got.output, we());
    }

    #[test]
    fn source_calling_read_endpoint_refuses_with_wrong_stage() {
        let rt = Arc::new(AudioRoutingRuntime::new());
        let h =
            rt.handle_for_plugin("com.example.source", PluginAudioRole::Source);
        let err = h
            .read_endpoint()
            .expect_err("source role does not have a read endpoint");
        assert!(matches!(
            err,
            AudioRoutingError::WrongStage {
                kind: AudioRoutingMethod::ReadEndpoint
            }
        ));
    }

    #[test]
    fn non_composition_calling_composition_endpoints_refuses() {
        let rt = Arc::new(AudioRoutingRuntime::new());
        let h = rt.handle_for_plugin(
            "com.example.delivery",
            PluginAudioRole::Delivery,
        );
        let err = h.composition_endpoints().expect_err(
            "non-composition role does not have a composition endpoints pair",
        );
        assert_eq!(err, AudioRoutingError::NotCompositionPlugin);
    }

    #[test]
    fn route_change_callback_fires_on_publish() {
        let rt = Arc::new(AudioRoutingRuntime::new());
        let h =
            rt.handle_for_plugin("com.example.source", PluginAudioRole::Source);
        let counter = Arc::new(AtomicU32::new(0));
        let counter2 = Arc::clone(&counter);
        h.on_route_change(Some(Arc::new(move |_change: &RouteChange| {
            counter2.fetch_add(1, Ordering::SeqCst);
        })));
        rt.publish_topology(
            "com.example.source",
            ResolvedRouting {
                write: Some(we()),
                read: None,
                format: pcm(),
                reason: "first publish".into(),
            },
        );
        rt.publish_topology(
            "com.example.source",
            ResolvedRouting {
                write: Some(we()),
                read: None,
                format: pcm(),
                reason: "rewire after format change".into(),
            },
        );
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn route_change_callback_can_be_cleared() {
        let rt = Arc::new(AudioRoutingRuntime::new());
        let h =
            rt.handle_for_plugin("com.example.source", PluginAudioRole::Source);
        let counter = Arc::new(AtomicU32::new(0));
        let counter2 = Arc::clone(&counter);
        h.on_route_change(Some(Arc::new(move |_change: &RouteChange| {
            counter2.fetch_add(1, Ordering::SeqCst);
        })));
        rt.publish_topology(
            "com.example.source",
            ResolvedRouting {
                write: Some(we()),
                read: None,
                format: pcm(),
                reason: "first publish".into(),
            },
        );
        h.on_route_change(None);
        rt.publish_topology(
            "com.example.source",
            ResolvedRouting {
                write: Some(we()),
                read: None,
                format: pcm(),
                reason: "second publish, callback cleared".into(),
            },
        );
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn clear_topology_returns_endpoint_to_not_configured() {
        let rt = Arc::new(AudioRoutingRuntime::new());
        let h =
            rt.handle_for_plugin("com.example.source", PluginAudioRole::Source);
        rt.publish_topology(
            "com.example.source",
            ResolvedRouting {
                write: Some(we()),
                read: None,
                format: pcm(),
                reason: "first publish".into(),
            },
        );
        h.write_endpoint().expect("published");
        rt.clear_topology("com.example.source");
        let err = h.write_endpoint().expect_err("topology cleared");
        assert_eq!(err, AudioRoutingError::EndpointNotConfigured);
    }

    #[test]
    fn current_format_returns_published_format() {
        let rt = Arc::new(AudioRoutingRuntime::new());
        let h =
            rt.handle_for_plugin("com.example.source", PluginAudioRole::Source);
        rt.publish_topology(
            "com.example.source",
            ResolvedRouting {
                write: Some(we()),
                read: None,
                format: pcm(),
                reason: "first publish".into(),
            },
        );
        let f = h.current_format().expect("published");
        assert_eq!(f, pcm());
    }

    #[test]
    fn forget_plugin_drops_state_entirely() {
        let rt = Arc::new(AudioRoutingRuntime::new());
        let h =
            rt.handle_for_plugin("com.example.source", PluginAudioRole::Source);
        rt.publish_topology(
            "com.example.source",
            ResolvedRouting {
                write: Some(we()),
                read: None,
                format: pcm(),
                reason: "first publish".into(),
            },
        );
        rt.forget_plugin("com.example.source");
        let err = h.write_endpoint().expect_err("plugin forgotten");
        assert_eq!(err, AudioRoutingError::EndpointNotConfigured);
    }

    // -------------------------------------------------------------
    // publish -> forwarder -> sink
    //
    // The other half of the wire path. The steward owns sink ->
    // plugin and tests it there; what this plane owns is that a
    // publish reaches the sink at all, in order, once per
    // publish. Driving a plain channel keeps the assertion on
    // the ordering rather than on a wire connection.
    // -------------------------------------------------------------

    /// A publish should fan out through the installed forwarder
    /// and land as a frame carrying the resolved routing and the
    /// operator-readable reason.
    #[tokio::test]
    async fn publish_reaches_the_sink_through_the_forwarder() {
        let plugin_name = "org.test.audio.delivery".to_string();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let sink = evo::wire_client::AudioRoutingForwarderSink::new(
            tx,
            plugin_name.clone(),
        );

        let runtime = Arc::new(AudioRoutingRuntime::new());
        let handle =
            runtime.handle_for_plugin(&plugin_name, PluginAudioRole::Delivery);

        // Nothing published yet, so installing the forwarder
        // emits no initial push.
        install_audio_routing_forwarder(
            Arc::clone(&runtime),
            Arc::clone(&handle),
            sink,
            plugin_name.clone(),
        );
        assert!(rx.try_recv().is_err(), "no publish, no frame");

        runtime.publish_topology(
            &plugin_name,
            ResolvedRouting {
                write: None,
                read: Some(re_()),
                format: pcm(),
                reason: "first publish".into(),
            },
        );

        let frame = rx.recv().await.expect("forwarder should have pushed");
        let evo_plugin_sdk::wire::WireFrame::AudioRoutingStateChanged {
            plugin,
            resolved,
            reason,
            ..
        } = frame
        else {
            panic!("expected an audio-routing state change");
        };
        assert_eq!(plugin, plugin_name);
        assert_eq!(reason, "first publish");
        let resolved = resolved.expect("a published chain resolves");
        assert_eq!(resolved.read, Some(re_()));
        assert_eq!(resolved.format, pcm());

        // The callback stays registered: a second publish fans
        // out too, and in order.
        runtime.publish_topology(
            &plugin_name,
            ResolvedRouting {
                write: None,
                read: Some(re_()),
                format: pcm(),
                reason: "second publish".into(),
            },
        );
        let frame = rx.recv().await.expect("second publish should push");
        let evo_plugin_sdk::wire::WireFrame::AudioRoutingStateChanged {
            reason,
            ..
        } = frame
        else {
            panic!("expected an audio-routing state change");
        };
        assert_eq!(reason, "second publish");
    }

    /// A topology published BEFORE the forwarder is installed
    /// still reaches the sink: installing emits the current
    /// state as an initial push, which is what a plugin that
    /// admits after reconciliation depends on.
    #[tokio::test]
    async fn install_pushes_state_published_before_it() {
        let plugin_name = "org.test.audio.delivery.initial".to_string();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let sink = evo::wire_client::AudioRoutingForwarderSink::new(
            tx,
            plugin_name.clone(),
        );

        let runtime = Arc::new(AudioRoutingRuntime::new());
        let handle =
            runtime.handle_for_plugin(&plugin_name, PluginAudioRole::Delivery);

        runtime.publish_topology(
            &plugin_name,
            ResolvedRouting {
                write: None,
                read: Some(re_()),
                format: pcm(),
                reason: "pre-admission topology".into(),
            },
        );

        install_audio_routing_forwarder(
            Arc::clone(&runtime),
            Arc::clone(&handle),
            sink,
            plugin_name.clone(),
        );

        let frame = rx.recv().await.expect("install should push current state");
        let evo_plugin_sdk::wire::WireFrame::AudioRoutingStateChanged {
            resolved,
            reason,
            ..
        } = frame
        else {
            panic!("expected an audio-routing state change");
        };
        assert_eq!(reason, "pre-admission topology");
        assert_eq!(resolved.expect("resolves").read, Some(re_()));
    }
}
