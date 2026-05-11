//! Test-only [`AudioRouting`] stub for the source-plugin
//! contract.
//!
//! Composition plugins consume `composition_endpoints()` and
//! source plugins consume `write_endpoint()` — different
//! corners of the [`AudioRouting`] surface. The composition
//! sibling at
//! [`crate::composition`](`evo-device-audio`/plugins/org.evoframework.composition.alsa/src/test_support.rs)
//! covers the composition corner; this stub mirrors that
//! shape for the source corner so playback.mpd's F3 reactor
//! can be tested deterministically.
//!
//! By default the stub returns
//! [`AudioRoutingError::EndpointNotConfigured`] for every
//! accessor. Tests publish a [`WriteEndpoint`] via
//! [`StubSourceAudioRouting::set_write_endpoint`] and fire
//! route-change events via
//! [`StubSourceAudioRouting::fire_route_change`].

use std::path::PathBuf;
use std::sync::Mutex;

use evo_plugin_sdk::audio::{AudioFormat, PcmCodec};
use evo_plugin_sdk::contract::audio_routing::{
    AudioRouting, AudioRoutingError, AudioRoutingMethod, CompositionEndpoints,
    EndpointKind, ReadEndpoint, RouteChange, RouteChangeCallback,
    WriteEndpoint,
};

#[derive(Default)]
struct StubInner {
    endpoint: Option<WriteEndpoint>,
    callback: Option<RouteChangeCallback>,
}

/// Test substrate stub implementing the source-plugin slice of
/// [`AudioRouting`].
#[derive(Default)]
pub(crate) struct StubSourceAudioRouting {
    inner: Mutex<StubInner>,
}

impl std::fmt::Debug for StubSourceAudioRouting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StubSourceAudioRouting")
            .finish_non_exhaustive()
    }
}

impl StubSourceAudioRouting {
    /// Construct a stub with no published topology — every
    /// accessor returns
    /// [`AudioRoutingError::EndpointNotConfigured`] until
    /// [`Self::set_write_endpoint`] publishes one.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Publish a new write endpoint. Returns the previous
    /// endpoint if one was published.
    pub(crate) fn set_write_endpoint(
        &self,
        endpoint: WriteEndpoint,
    ) -> Option<WriteEndpoint> {
        let mut g = self.inner.lock().unwrap();
        g.endpoint.replace(endpoint)
    }

    /// Fire the registered route-change callback (if any)
    /// with the given event. Returns `true` when a callback
    /// was registered and invoked.
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
    pub(crate) fn has_route_change_callback(&self) -> bool {
        self.inner.lock().unwrap().callback.is_some()
    }
}

impl AudioRouting for StubSourceAudioRouting {
    fn write_endpoint(&self) -> Result<WriteEndpoint, AudioRoutingError> {
        self.inner
            .lock()
            .unwrap()
            .endpoint
            .clone()
            .ok_or(AudioRoutingError::EndpointNotConfigured)
    }

    fn read_endpoint(&self) -> Result<ReadEndpoint, AudioRoutingError> {
        Err(AudioRoutingError::WrongStage {
            kind: AudioRoutingMethod::ReadEndpoint,
        })
    }

    fn composition_endpoints(
        &self,
    ) -> Result<CompositionEndpoints, AudioRoutingError> {
        Err(AudioRoutingError::NotCompositionPlugin)
    }

    fn current_format(&self) -> Result<AudioFormat, AudioRoutingError> {
        match &self.inner.lock().unwrap().endpoint {
            Some(ep) => Ok(ep.format.clone()),
            None => Err(AudioRoutingError::EndpointNotConfigured),
        }
    }

    fn on_route_change(&self, callback: Option<RouteChangeCallback>) {
        self.inner.lock().unwrap().callback = callback;
    }
}

/// Build a default-shape ALSA `WriteEndpoint` for the
/// I-Sabre Q2M DAC reference target. Tests that don't care
/// about the specific format pass this in.
pub(crate) fn default_alsa_write_endpoint() -> WriteEndpoint {
    WriteEndpoint {
        kind: EndpointKind::AlsaPcm,
        path: PathBuf::from("hw:2,0"),
        format: AudioFormat::Pcm {
            codec: PcmCodec::PcmS16Le,
            rate_hz: 44_100,
            channels: 2,
        },
        buffer_frames: 1024,
    }
}

/// Build a `RouteChange` event carrying the supplied format
/// and a stable reason string. Tests pass this into
/// [`StubSourceAudioRouting::fire_route_change`].
pub(crate) fn route_change(new_format: AudioFormat) -> RouteChange {
    RouteChange {
        new_format,
        reason: "test-injected route change".to_string(),
    }
}
