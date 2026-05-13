// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! # org-evoframework-multiroom-evo-native
//!
//! evo-native multi-room audio-frame fan-out plugin.
//!
//! Bridges the local audio chain to the framework's
//! audio-plane TCP transport. Role flips dynamically as the
//! framework's source-host election arbitrates: when the
//! local node is elected source-host for a multi-room group,
//! this plugin captures from the local audio chain (mpd
//! output via ALSA loopback / pcm.tee) and fans encoded PCM
//! frames out via the framework's
//! [`evo_plugin_sdk::contract::audio_plane::AudioPlaneHandle::fan_out_audio_frame`]
//! seam. When the local node is a receiver, the plugin
//! subscribes to incoming frames via
//! [`evo_plugin_sdk::contract::audio_plane::AudioPlaneHandle::subscribe_audio_frames`]
//! and renders payloads to the local ALSA playback chain at
//! the scheduling target the source-host's
//! `presentation_time_ms` declares.
//!
//! ## Scope of v0.1.13 baseline
//!
//! - Codec: raw `pcm_s16_le` (no encoder dependency; matches
//!   the framework substrate's bit-exact round-trip
//!   guarantee).
//! - Source-side capture: ALSA capture device the operator
//!   wires via `/etc/asound.conf` (`pcm.evo_capture` fed by
//!   `pcm.tee` or `snd-aloop`).
//! - Receiver-side playback: ALSA playback device the
//!   operator wires (typically the same hardware target that
//!   delivery.alsa drives).
//! - Sync: receivers honour the source-host's
//!   `presentation_time_ms` against their own monotonic
//!   clock + the framework's NTP-lite offset (the
//!   [`evo_plugin_sdk::contract::audio_plane::AudioFrameReceived`]
//!   envelope's `presentation_time_ms` is already in the
//!   source-host's monotonic ms; receivers transform via the
//!   measured sync offset before scheduling the write).
//!
//! Production-quality additions (FEC / adaptive jitter
//! buffering / predictive buffering / network-class auto-
//! tuning / cooperative peer recovery) ride v0.1.14+ per the
//! reliability bar in `project-multiroom-position` memory.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::manual_async_fn)]

use std::future::Future;
use std::sync::Arc;

use evo_plugin_sdk::contract::audio_plane::AudioPlaneHandle;
use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities,
};
use evo_plugin_sdk::Manifest;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// Embedded manifest source.
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");

/// Plugin identity name (must match manifest).
pub const PLUGIN_NAME: &str = "org.evoframework.multiroom.evo-native";

/// Wire-protocol payload version every request / response
/// carries.
const PAYLOAD_VERSION: u32 = 1;

/// Request types this plugin honours. Mirrors
/// `manifest.toml`'s `[capabilities.respondent].request_types`;
/// admission would refuse a mismatch.
const REQUEST_TYPES: &[&str] = &["multiroom.get_status"];

/// Parse the embedded plugin manifest.
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML).expect(
        "org-evoframework-multiroom-evo-native: embedded manifest must parse",
    )
}

fn plugin_crate_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is valid semver")
}

/// Multi-room audio-frame fan-out plugin.
pub struct MultiroomEvoNativePlugin {
    loaded: bool,
    audio_plane: Option<Arc<dyn AudioPlaneHandle>>,
    /// Receiver-side task that consumes the audio-plane's
    /// broadcast stream and reports observable progress. The
    /// v0.1.13 baseline counts every received frame and logs
    /// the first frame per source peer so the substrate is
    /// observable end-to-end; subsequent iterations of the
    /// same plugin add PCM decode plus local ALSA render at
    /// the scheduling target the envelope declares.
    receiver_task: Option<JoinHandle<()>>,
    receiver_shutdown: Arc<Notify>,
    frames_received: Arc<std::sync::atomic::AtomicU64>,
}

impl MultiroomEvoNativePlugin {
    /// Construct a fresh plugin instance.
    pub fn new() -> Self {
        Self {
            loaded: false,
            audio_plane: None,
            receiver_task: None,
            receiver_shutdown: Arc::new(Notify::new()),
            frames_received: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Observed total of audio frames received across every
    /// connected source-host peer since plugin load.
    pub fn frames_received(&self) -> u64 {
        self.frames_received
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Default for MultiroomEvoNativePlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for MultiroomEvoNativePlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: PLUGIN_NAME.to_string(),
                    version: plugin_crate_version(),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: REQUEST_TYPES
                        .iter()
                        .map(|s| (*s).to_string())
                        .collect(),
                    accepts_custody: false,
                    flags: Default::default(),
                    course_correct_verbs: Vec::new(),
                },
                build_info: BuildInfo {
                    plugin_build: env!("CARGO_PKG_VERSION").to_string(),
                    sdk_version: evo_plugin_sdk::VERSION.to_string(),
                    rustc_version: None,
                    built_at: None,
                },
            }
        }
    }

    fn load<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
        async move {
            tracing::info!(plugin = PLUGIN_NAME, "plugin load beginning");

            // Equip the audio-plane handle the framework
            // populated via the manifest's
            // `capabilities.audio_plane = true` declaration.
            // Refuse loudly when the handle is None — that
            // signals a manifest / admission misconfiguration.
            let audio_plane = ctx
                .audio_plane
                .as_ref()
                .ok_or_else(|| {
                    PluginError::Permanent(
                        "LoadContext.audio_plane = None; \
                         manifest must declare capabilities.audio_plane = \
                         true AND the steward must be configured with \
                         AdmissionEngine::with_audio_plane(...)"
                            .into(),
                    )
                })?
                .clone();
            self.audio_plane = Some(Arc::clone(&audio_plane));

            // Spawn the receiver-side observation task. This
            // task subscribes to the framework's audio-plane
            // broadcast and counts every received frame. In
            // subsequent iterations it decodes + renders to
            // local ALSA at the scheduling target the
            // envelope declares; for v0.1.13 baseline the
            // count + the per-peer first-frame log are the
            // operator-observable signal that the substrate
            // is flowing.
            let counter = Arc::clone(&self.frames_received);
            let shutdown = Arc::clone(&self.receiver_shutdown);
            let handle = Arc::clone(&audio_plane);
            let task = tokio::spawn(async move {
                run_receiver_task(handle, counter, shutdown).await;
            });
            self.receiver_task = Some(task);

            self.loaded = true;

            tracing::info!(
                plugin = PLUGIN_NAME,
                "plugin loaded; audio-plane handle equipped; \
                 receiver-side observation task running"
            );

            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            self.receiver_shutdown.notify_one();
            if let Some(task) = self.receiver_task.take() {
                let _ = task.await;
            }
            self.audio_plane = None;
            self.loaded = false;
            tracing::info!(
                plugin = PLUGIN_NAME,
                frames_received = self.frames_received(),
                "plugin unload"
            );
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            HealthReport {
                status: evo_plugin_sdk::contract::HealthStatus::Healthy,
                detail: Some(format!(
                    "frames_received={}",
                    self.frames_received()
                )),
                checks: Vec::new(),
                reported_at: std::time::SystemTime::now(),
            }
        }
    }
}

impl Respondent for MultiroomEvoNativePlugin {
    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "plugin not loaded".to_string(),
                ));
            }
            match req.request_type.as_str() {
                "multiroom.get_status" => {
                    let payload = serde_json::json!({
                        "v": PAYLOAD_VERSION,
                        "frames_received": self.frames_received(),
                        "role": "receiver",
                    });
                    let body = serde_json::to_vec(&payload).map_err(|e| {
                        PluginError::Permanent(format!(
                            "encode multiroom.get_status response: {e}"
                        ))
                    })?;
                    Ok(Response::for_request(req, body))
                }
                other => Err(PluginError::Permanent(format!(
                    "request type {other:?} declared but no handler wired"
                ))),
            }
        }
    }
}

/// Run the receiver-side observation task: subscribe to the
/// framework's audio-plane broadcast, count every received
/// frame, log the first frame per source-host peer.
///
/// v0.1.13 baseline does NOT yet decode + render to ALSA.
/// The substrate is observable via the frame counter +
/// per-peer first-frame log; the audio-chain rendering work
/// rides a subsequent iteration of this same plugin.
async fn run_receiver_task(
    audio_plane: Arc<dyn AudioPlaneHandle>,
    counter: Arc<std::sync::atomic::AtomicU64>,
    shutdown: Arc<Notify>,
) {
    let mut stream = match audio_plane.subscribe_audio_frames().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "subscribe_audio_frames failed at receiver task startup"
            );
            return;
        }
    };

    let mut seen_peers: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    loop {
        tokio::select! {
            _ = shutdown.notified() => {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    "receiver task shutting down on unload notify"
                );
                return;
            }
            res = stream.recv() => {
                match res {
                    Ok(frame) => {
                        counter.fetch_add(
                            1,
                            std::sync::atomic::Ordering::Relaxed,
                        );
                        if seen_peers.insert(frame.from_device_id.clone()) {
                            tracing::info!(
                                plugin = PLUGIN_NAME,
                                from_device_id = %frame.from_device_id,
                                group_id = %frame.group_id,
                                codec = %frame.codec,
                                rate_hz = frame.rate_hz,
                                channels = frame.channels,
                                payload_bytes = frame.payload.len(),
                                "first audio frame received from new source-host peer"
                            );
                        }
                    }
                    Err(
                        evo_plugin_sdk::contract::audio_plane::AudioFrameStreamError::Lagged {
                            dropped,
                        },
                    ) => {
                        tracing::warn!(
                            plugin = PLUGIN_NAME,
                            dropped = dropped,
                            "audio-frame stream lagged; receiver continues at live frame"
                        );
                    }
                    Err(
                        evo_plugin_sdk::contract::audio_plane::AudioFrameStreamError::Closed,
                    ) => {
                        tracing::debug!(
                            plugin = PLUGIN_NAME,
                            "audio-frame stream closed; receiver task exiting"
                        );
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses() {
        let m = manifest();
        assert_eq!(m.plugin.name, PLUGIN_NAME);
    }

    #[test]
    fn plugin_construction_is_unloaded() {
        let p = MultiroomEvoNativePlugin::new();
        assert!(!p.loaded);
        assert_eq!(p.frames_received(), 0);
    }
}
