//! Test-only substrate stub for the audio routing surface.
//!
//! `StubAudioRouting` implements
//! [`evo_plugin_sdk::contract::audio_routing::AudioRouting`]
//! with reconfigurable internal state so unit tests can
//! exercise the plugin against a known-shape topology
//! without needing the framework's reconciliation engine.
//!
//! By default the stub returns
//! [`AudioRoutingError::EndpointNotConfigured`] — the
//! benign pre-reconciliation state. Tests that need a
//! configured topology call
//! [`StubAudioRouting::set_endpoints`] to publish an
//! endpoint pair + format. The most-recently-registered
//! `RouteChangeCallback` is captured for inspection by
//! tests that exercise route-change reactions
//! (chunk C onwards).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use evo_plugin_sdk::audio::{AudioFormat, PcmCodec};
use evo_plugin_sdk::contract::audio_routing::{
    AudioRouting, AudioRoutingError, AudioRoutingMethod, CompositionEndpoints,
    EndpointKind, ReadEndpoint, RouteChange, RouteChangeCallback,
    WriteEndpoint,
};

/// Inner state guarded by a single mutex so the stub stays
/// `Send + Sync` per the trait's bound.
#[derive(Default)]
struct StubInner {
    endpoints: Option<CompositionEndpoints>,
    callback: Option<RouteChangeCallback>,
}

/// Test substrate stub for the [`AudioRouting`] trait.
#[derive(Default)]
pub(crate) struct StubAudioRouting {
    inner: Mutex<StubInner>,
}

impl std::fmt::Debug for StubAudioRouting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StubAudioRouting").finish_non_exhaustive()
    }
}

impl StubAudioRouting {
    /// Construct a stub with no published topology — every
    /// endpoint accessor returns
    /// [`AudioRoutingError::EndpointNotConfigured`] until
    /// [`Self::set_endpoints`] publishes one.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Convenience constructor pre-loaded with a typical
    /// 44.1 kHz / 16-bit / stereo PCM ALSA-loopback
    /// topology. Used by chunk-C and onward tests that
    /// need an active topology without caring about the
    /// specific format.
    #[allow(dead_code)]
    pub(crate) fn with_default_alsa_topology() -> Self {
        let s = Self::new();
        s.set_endpoints(default_alsa_endpoints());
        s
    }

    /// Publish a new endpoint pair. Returns the previous
    /// pair if one was published.
    pub(crate) fn set_endpoints(
        &self,
        endpoints: CompositionEndpoints,
    ) -> Option<CompositionEndpoints> {
        let mut g = self.inner.lock().unwrap();
        g.endpoints.replace(endpoints)
    }

    /// Fire the registered route-change callback (if any)
    /// with the given event. Returns `true` when a
    /// callback was registered and invoked.
    #[allow(dead_code)]
    pub(crate) fn fire_route_change(&self, event: RouteChange) -> bool {
        let cb = self.inner.lock().unwrap().callback.clone();
        match cb {
            Some(callback) => {
                callback(&event);
                true
            }
            None => false,
        }
    }

    /// Returns `true` when a route-change callback is
    /// currently registered.
    #[allow(dead_code)]
    pub(crate) fn has_route_change_callback(&self) -> bool {
        self.inner.lock().unwrap().callback.is_some()
    }
}

impl AudioRouting for StubAudioRouting {
    fn write_endpoint(&self) -> Result<WriteEndpoint, AudioRoutingError> {
        Err(AudioRoutingError::WrongStage {
            kind: AudioRoutingMethod::WriteEndpoint,
        })
    }

    fn read_endpoint(&self) -> Result<ReadEndpoint, AudioRoutingError> {
        Err(AudioRoutingError::WrongStage {
            kind: AudioRoutingMethod::ReadEndpoint,
        })
    }

    fn composition_endpoints(
        &self,
    ) -> Result<CompositionEndpoints, AudioRoutingError> {
        self.inner
            .lock()
            .unwrap()
            .endpoints
            .clone()
            .ok_or(AudioRoutingError::EndpointNotConfigured)
    }

    fn current_format(&self) -> Result<AudioFormat, AudioRoutingError> {
        match &self.inner.lock().unwrap().endpoints {
            Some(ep) => Ok(ep.output.format.clone()),
            None => Err(AudioRoutingError::EndpointNotConfigured),
        }
    }

    fn on_route_change(&self, callback: Option<RouteChangeCallback>) {
        self.inner.lock().unwrap().callback = callback;
    }
}

/// Default 44.1 kHz / 16-bit / stereo PCM ALSA-loopback
/// endpoint pair used by tests that need a published
/// topology without caring about the specific shape.
#[allow(dead_code)]
pub(crate) fn default_alsa_endpoints() -> CompositionEndpoints {
    let format = AudioFormat::Pcm {
        codec: PcmCodec::PcmS16Le,
        rate_hz: 44_100,
        channels: 2,
    };
    CompositionEndpoints {
        input: ReadEndpoint {
            kind: EndpointKind::AlsaPcm,
            path: PathBuf::from("hw:Loopback,0,0"),
            format: format.clone(),
            buffer_frames: 1024,
        },
        output: WriteEndpoint {
            kind: EndpointKind::AlsaPcm,
            path: PathBuf::from("hw:Loopback,1,0"),
            format,
            buffer_frames: 1024,
        },
    }
}

/// Trivial helper: build a `RouteChange` carrying the
/// supplied format and a stable reason string. Tests
/// pass this into [`StubAudioRouting::fire_route_change`].
#[allow(dead_code)]
pub(crate) fn route_change(new_format: AudioFormat) -> RouteChange {
    RouteChange {
        new_format,
        reason: "test-injected route change".to_string(),
    }
}

/// Cast a stub into `Arc<dyn AudioRouting>` for tests
/// that pass it to plugin code expecting the trait
/// object. Saves repetitive `Arc::new(...) as _`
/// boilerplate at call sites.
#[allow(dead_code)]
pub(crate) fn as_routing_arc(stub: StubAudioRouting) -> Arc<dyn AudioRouting> {
    Arc::new(stub)
}
