//! Audio terminus plugin.
//!
//! Canonical owner of every post-mixer audio-derived signal.
//! Parallel-tapped from `pcm.evo` via `snd-aloop`; the primary
//! audio path (pcm.evo -> hw:CARD=<dac>) is unaffected by
//! terminus health (the floor invariant per the local-playback
//! invariant contract). The plugin owns:
//!
//! - **Spectrum FFT compute.** 256-bin mel-scale stereo Float32
//!   FFT at 30 Hz, with peak-hold per bin (perceptual decay),
//!   per-band onset event detection (sub_bass / bass / mid / high),
//!   and L/R correlation per bin for stereo-imaging visualisations.
//!   Emits the `audio_playback_spectrum_frame` subject on the
//!   audio.playback shelf at every successful frame compute.
//!
//! - **`get_spectrum_frame` read verb.** First-render parity with
//!   `get_now_playing` (operator surfaces that subscribe to the
//!   subject get the latest frame on connect without waiting up
//!   to ~33 ms for the next happening).
//!
//! - **Audio-plane capture surface (forward-decade scope).** The
//!   captured PCM frames are kept hot in a ring buffer so future
//!   cross-plugin consumers (loudness telemetry, fingerprinting,
//!   on-device transcription) can read post-mixer audio without
//!   each consumer running their own ALSA capture.
//!
//! Cadence is ALSA-paced: every successful FFT compute
//! (FFT_SIZE samples drawn at the configured sample rate)
//! emits one frame. At the canonical 48 kHz / 1024-point FFT
//! chain that is 47 Hz on the wire. The wire `rate_hz` field
//! carries this value (derived via `fft::frame_rate_hz`) so
//! subscribers see the cadence they actually receive frames at.
//!
//! Wire shape is constant: 256 bins stereo Float32 [0, 1] +
//! peak_hold + onsets + correlation.
//!
//! Lifecycle gates:
//! - Emission is silent unless `transport_state == "playing"`
//!   on the `audio_playback_now_playing` subject the plugin
//!   subscribes to. Stopped / paused -> no frames emitted (the
//!   capture loop continues so a resume picks up at the next
//!   tick without spawn latency).
//! - Multi-room: leader-authoritative emission. The plugin
//!   queries the multiroom-substrate at emission time; only
//!   emits when the local device is the leader of any active
//!   group OR no active group is configured (solo device).
//!   Followers do not emit a parallel spectrum subject under
//!   the same name; operator seats in a group receive the
//!   leader's wavefront regardless of which member device
//!   hosts the seat.
//!
//! Failure semantics:
//! - ALSA capture-open failure at admit: the plugin admits
//!   cleanly, declares the subject + read verb, but emits no
//!   frames; logs a warn so an operator inspecting the journal
//!   sees the cause.
//! - Capture-loop transport error: tight reconnect loop with
//!   bounded backoff (mirrors the playback.mpd supervisor's
//!   reconnect contract). On exhaustion the capture loop exits;
//!   the plugin's `health_check` surfaces unhealthy. A
//!   plugin restart re-opens.

use std::future::Future;
use std::sync::{Arc, Mutex};

use evo_plugin_sdk::contract::context::{LoadContext, SubjectAnnouncer};
use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, Plugin, PluginDescription, PluginError,
    PluginIdentity, Request, Respondent, Response, RuntimeCapabilities,
};
#[cfg(test)]
use evo_plugin_sdk::Manifest;
use serde::Deserialize;

mod fft;
mod spectrum_subject;

pub use fft::{PerceptualFrame, SpectrumAnalyser};
pub use spectrum_subject::{
    render_spectrum_frame, SpectrumEmitter, SPECTRUM_PAYLOAD_VERSION,
    SPECTRUM_SUBJECT_ADDRESSING_SCHEME, SPECTRUM_SUBJECT_ADDRESSING_VALUE,
    SPECTRUM_SUBJECT_TYPE,
};

#[cfg(feature = "alsa-substrate")]
mod capture;

const PLUGIN_NAME: &str = "org.evoframework.audio.terminus";

/// Canonical input PCM the plugin captures from.
///
/// The base `/etc/asound.conf` defines `pcm.evo` as a
/// multi-slave tee whose terminus branch writes to the
/// snd-aloop pair at `hw:Loopback,DEV=0,SUBDEV=7`. snd-aloop
/// is paired: anything written to the playback side
/// `hw:Loopback,DEV=0,SUBDEV=N` is readable from the capture
/// side `hw:Loopback,DEV=1,SUBDEV=N`. The constant below is
/// the read side of that pair.
const TERMINUS_INPUT_PCM: &str = "hw:Loopback,1,7";

/// Embedded in-process manifest. Production OOP path uses
/// `manifest.oop.toml`; both share the same shelf + interaction
/// + capabilities surface. Consumed by tests that assert the
/// describe() identity matches the manifest declaration.
#[cfg(test)]
const EMBEDDED_MANIFEST: &str = include_str!("../manifest.toml");

/// Source-verb request types this plugin honours. Single entry
/// today; extend as the audio-terminus surface grows
/// (loudness telemetry read verb, etc.).
const REQUEST_TYPES: &[&str] = &["get_spectrum_frame"];

/// Operator-tunable plugin config. Loaded from
/// `/etc/evo/plugins.d/org.evoframework.audio.terminus.toml`
/// at admit; missing fields fall through to defaults that
/// match a standard 48 kHz/stereo audio chain.
///
/// Fields are consumed by the `alsa-substrate`-gated capture
/// loop. Without the feature, the plugin admits + declares its
/// subject but never opens the capture; the fields are stored
/// (so an operator config change at re-admit takes effect once
/// the feature build lands) but technically unread in the
/// non-alsa-substrate build. The `cfg_attr` keeps clippy quiet
/// in that build.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(not(feature = "alsa-substrate"), allow(dead_code))]
pub struct PluginConfig {
    /// ALSA capture PCM the terminus reads from. Defaults to
    /// the canonical Loopback subdev 7 the multi-slave tee in
    /// `/etc/asound.conf` writes to. Operators on non-standard
    /// hardware can override but the default works for every
    /// reference rig.
    #[serde(default = "default_input_pcm")]
    pub input_pcm: String,
    /// Capture sample rate in Hz. Defaults to 48 kHz which
    /// matches the chain rate every reference rig uses; 44.1
    /// kHz works too (the FFT mel projection is rate-aware via
    /// `SpectrumAnalyser::new(sample_rate_hz)`).
    #[serde(default = "default_sample_rate_hz")]
    pub sample_rate_hz: u32,
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            input_pcm: default_input_pcm(),
            sample_rate_hz: default_sample_rate_hz(),
        }
    }
}

fn default_input_pcm() -> String {
    TERMINUS_INPUT_PCM.to_string()
}

fn default_sample_rate_hz() -> u32 {
    48_000
}

/// The terminus plugin's mutable state.
pub struct AudioTerminusPlugin {
    loaded: bool,
    config: PluginConfig,
    subject_announcer: Option<Arc<dyn SubjectAnnouncer>>,
    /// Latest computed spectrum frame, refreshed by the capture
    /// loop on every successful FFT compute. The
    /// `get_spectrum_frame` read verb returns this; the subject
    /// emitter publishes it. None until the first frame computes.
    latest_frame: Arc<Mutex<Option<PerceptualFrame>>>,
    /// Capture-task shutdown signal. The capture loop respects
    /// this; the plugin's `unload` flips it and awaits the task.
    /// `None` before first load and after every unload.
    #[cfg(feature = "alsa-substrate")]
    capture_shutdown: Option<Arc<tokio::sync::Notify>>,
    /// Capture task handle. `Some` after a successful load that
    /// opened the input PCM; `None` if the open failed or after
    /// unload.
    #[cfg(feature = "alsa-substrate")]
    capture_task: Option<tokio::task::JoinHandle<()>>,
}

impl AudioTerminusPlugin {
    pub fn new() -> Self {
        Self {
            loaded: false,
            config: PluginConfig::default(),
            subject_announcer: None,
            latest_frame: Arc::new(Mutex::new(None)),
            #[cfg(feature = "alsa-substrate")]
            capture_shutdown: None,
            #[cfg(feature = "alsa-substrate")]
            capture_task: None,
        }
    }

    fn apply_config(
        &mut self,
        config_table: &toml::Table,
    ) -> Result<(), PluginError> {
        let raw = toml::Value::Table(config_table.clone());
        let parsed: PluginConfig = raw.try_into().map_err(|e| {
            PluginError::Permanent(format!(
                "config table did not parse against PluginConfig: {e}"
            ))
        })?;
        self.config = parsed;
        Ok(())
    }
}

impl Default for AudioTerminusPlugin {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse the embedded manifest. Test-only — production
/// describe() returns the values directly from
/// `CARGO_PKG_VERSION` + `REQUEST_TYPES` rather than re-parsing
/// at every call.
#[cfg(test)]
fn parse_embedded_manifest() -> Manifest {
    toml::from_str(EMBEDDED_MANIFEST)
        .expect("embedded manifest must parse against Manifest schema")
}

fn plugin_crate_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is valid semver")
}

impl Plugin for AudioTerminusPlugin {
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

            self.apply_config(&ctx.config)?;

            self.subject_announcer = Some(Arc::clone(&ctx.subject_announcer));

            // Announce the spectrum subject once at load. The
            // subject's state is null-ish until the first frame
            // computes; emission of the real shape begins on the
            // capture loop's first successful FFT. Subscribers
            // connecting before the first frame receive the
            // subject's nullable shape (transport_state=stopped
            // analog: empty magnitudes); after the first frame
            // they receive real spectra.
            spectrum_subject::announce_initial_state(
                self.subject_announcer
                    .as_ref()
                    .expect("subject_announcer set above"),
            )
            .await;

            // Spawn the ALSA capture loop when the alsa-substrate
            // feature is on. Without the feature, the plugin
            // admits cleanly and declares its subject + verb, but
            // emits no frames. Mirrors the multi-room plugin's
            // alsa-substrate gating.
            #[cfg(feature = "alsa-substrate")]
            {
                let shutdown = Arc::new(tokio::sync::Notify::new());
                let capture_handle = capture::spawn(
                    self.config.clone(),
                    Arc::clone(&self.latest_frame),
                    Arc::clone(
                        self.subject_announcer
                            .as_ref()
                            .expect("subject_announcer set above"),
                    ),
                    Arc::clone(&shutdown),
                );
                self.capture_shutdown = Some(shutdown);
                self.capture_task = Some(capture_handle);
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    input_pcm = %self.config.input_pcm,
                    sample_rate_hz = self.config.sample_rate_hz,
                    "capture task spawned"
                );
            }
            #[cfg(not(feature = "alsa-substrate"))]
            {
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    "alsa-substrate feature disabled at build time; \
                     subject declared but no frames will emit until the \
                     plugin is rebuilt with --features alsa-substrate"
                );
            }

            self.loaded = true;
            tracing::info!(plugin = PLUGIN_NAME, "plugin loaded");
            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            tracing::info!(plugin = PLUGIN_NAME, "plugin unload beginning");

            #[cfg(feature = "alsa-substrate")]
            {
                if let Some(shutdown) = self.capture_shutdown.take() {
                    shutdown.notify_waiters();
                }
                if let Some(handle) = self.capture_task.take() {
                    let _ = handle.await;
                }
            }

            self.subject_announcer = None;
            self.loaded = false;
            // Clear the latest frame so a subsequent re-load
            // starts fresh; a stale snapshot would mislead read
            // consumers that arrived during the down window.
            if let Ok(mut guard) = self.latest_frame.lock() {
                *guard = None;
            }

            tracing::info!(plugin = PLUGIN_NAME, "plugin unloaded");
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            if self.loaded {
                HealthReport::healthy()
            } else {
                HealthReport::unhealthy("plugin not loaded")
            }
        }
    }
}

impl Respondent for AudioTerminusPlugin {
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
                "get_spectrum_frame" => self.handle_get_spectrum_frame(req),
                other => Err(PluginError::Permanent(format!(
                    "unknown request type {other:?}; this plugin honours {:?}",
                    REQUEST_TYPES
                ))),
            }
        }
    }
}

impl AudioTerminusPlugin {
    fn handle_get_spectrum_frame(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let frame_guard = self.latest_frame.lock().map_err(|_| {
            PluginError::Permanent("latest_frame mutex poisoned".to_string())
        })?;
        // Same cadence the capture loop emits with. Single source
        // of truth via `fft::frame_rate_hz` so the read-verb path
        // and the subject-emit path never disagree on the wire.
        let rate_hz = fft::frame_rate_hz(self.config.sample_rate_hz);
        let payload = match frame_guard.as_ref() {
            Some(frame) => render_spectrum_frame(frame, rate_hz),
            None => spectrum_subject::render_empty_frame(rate_hz),
        };
        let body = serde_json::to_vec(&payload).map_err(|e| {
            PluginError::Permanent(format!(
                "get_spectrum_frame response JSON encode failed: {e}"
            ))
        })?;
        Ok(Response::for_request(req, body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses() {
        let _ = parse_embedded_manifest();
    }

    #[tokio::test]
    async fn describe_matches_embedded_manifest() {
        let plugin = AudioTerminusPlugin::new();
        let desc = plugin.describe().await;
        let mf = parse_embedded_manifest();
        assert_eq!(desc.identity.name, mf.plugin.name);
        assert_eq!(desc.identity.contract, mf.plugin.contract as u32);
        assert!(!desc.runtime_capabilities.accepts_custody);
        // Every declared request type in the manifest must
        // appear in the runtime capabilities.
        for declared in &mf
            .capabilities
            .respondent
            .as_ref()
            .expect("manifest must declare respondent capabilities")
            .request_types
        {
            assert!(
                desc.runtime_capabilities
                    .request_types
                    .iter()
                    .any(|r| r == declared),
                "declared request type {declared:?} missing from \
                 RuntimeCapabilities.request_types"
            );
        }
    }

    #[tokio::test]
    async fn describe_matches_oop_manifest() {
        let oop_text = include_str!("../manifest.oop.toml");
        let mf: Manifest =
            toml::from_str(oop_text).expect("oop manifest must parse");
        let plugin = AudioTerminusPlugin::new();
        let desc = plugin.describe().await;
        assert_eq!(desc.identity.name, mf.plugin.name);
        for declared in &mf
            .capabilities
            .respondent
            .as_ref()
            .expect("manifest must declare respondent capabilities")
            .request_types
        {
            assert!(
                desc.runtime_capabilities
                    .request_types
                    .iter()
                    .any(|r| r == declared),
                "declared request type {declared:?} missing from OOP \
                 RuntimeCapabilities"
            );
        }
    }

    #[tokio::test]
    async fn handle_request_refuses_before_load() {
        let mut plugin = AudioTerminusPlugin::new();
        let req = Request {
            request_type: "get_spectrum_frame".to_string(),
            payload: serde_json::to_vec(&serde_json::json!({"v": 1})).unwrap(),
            correlation_id: 1,
            deadline: None,
            instance_id: None,
            principal_scope: None,
            has_step_up: false,
        };
        let err = plugin.handle_request(&req).await.unwrap_err();
        match err {
            PluginError::Permanent(msg) => {
                assert!(msg.contains("not loaded"));
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handle_request_refuses_unknown_verb_after_load() {
        let mut plugin = AudioTerminusPlugin::new();
        plugin.loaded = true;
        let req = Request {
            request_type: "set_volume".to_string(),
            payload: vec![],
            correlation_id: 1,
            deadline: None,
            instance_id: None,
            principal_scope: None,
            has_step_up: false,
        };
        let err = plugin.handle_request(&req).await.unwrap_err();
        match err {
            PluginError::Permanent(msg) => {
                assert!(msg.contains("unknown request type"));
                assert!(msg.contains("get_spectrum_frame"));
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }
}
