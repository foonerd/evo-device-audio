//! # org-evoframework-composition-alsa
//!
//! Substrate-aware composition plugin for the audio data
//! plane. Stocks the `audio.composition` shelf at shape 2.
//!
//! ## What this plugin is
//!
//! A singleton respondent that occupies the middle stage of
//! the audio data plane: source → composition → delivery.
//! The framework configures topology — endpoint substrate
//! (ALSA pcm name, named pipe, shared-memory region, JACK
//! port) plus negotiated [`AudioFormat`] — per active
//! source / delivery pair, and hands this plugin a typed
//! [`CompositionEndpoints`] pair via
//! [`LoadContext::audio_routing`]. Audio bytes flow
//! through the OS-native primitive the framework selected;
//! they NEVER traverse the wire protocol or any SDK
//! callback.
//!
//! ## What this plugin does
//!
//! - Declares typed
//!   [`[capabilities.composition]`](`evo_plugin_sdk::manifest::CompositionCapabilities`)
//!   with `input_kind = "audio.pcm"`, `output_kind =
//!   "audio.pcm"`, a non-empty mode list, and a
//!   `default_mode`.
//! - Consumes
//!   [`LoadContext::audio_routing`](evo_plugin_sdk::contract::LoadContext::audio_routing)
//!   at load; refuses load loudly when the handle is
//!   `None` — composition plugins MUST receive a routing
//!   handle, and absence indicates a manifest / trust
//!   misconfiguration.
//! - Exposes one respondent surface,
//!   `composition.select_mode`, that the framework calls
//!   when the reconciliation engine selects a new mode for
//!   the active topology. The plugin validates the
//!   requested mode against its declared list and rotates
//!   the worker.
//!
//! ## Modes declared by this build
//!
//! - `passthrough` — byte-identical copy from input
//!   endpoint to output endpoint; preserves bit-perfect.
//!
//! Subsequent commits layer further modes (`eq_only`,
//! `resampler`, `dsd_to_pcm`) onto this same plugin without
//! requiring a shape bump. The reconciliation engine picks
//! one mode per topology after intersecting the source-
//! produced format with the delivery-accepted format and
//! applying operator policy.
//!
//! ## Request / response shape
//!
//! See `docs/COMPOSITION_SELECT_MODE_V1.md` for the wire
//! contract.
//!
//! ## What this chunk does NOT yet implement
//!
//! - `RouteChangeCallback` registration (chunk C lands the
//!   reopen-on-rewire reactor).
//! - The byte-flow worker that drives the OS-native
//!   primitives (chunk D lands the ALSA loopback worker).
//!
//! [`AudioFormat`]: evo_plugin_sdk::audio::AudioFormat
//! [`CompositionEndpoints`]: evo_plugin_sdk::contract::audio_routing::CompositionEndpoints
//! [`LoadContext::audio_routing`]: evo_plugin_sdk::contract::LoadContext::audio_routing

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::manual_async_fn)]

use std::future::Future;
use std::sync::Arc;

use evo_plugin_sdk::contract::audio_routing::{
    AudioRouting, AudioRoutingError,
};
use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities,
};
use evo_plugin_sdk::Manifest;
use serde::{Deserialize, Serialize};

/// Embedded manifest source.
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");
/// Plugin identity name (must match manifest).
pub const PLUGIN_NAME: &str = "org.evoframework.composition.alsa";

/// Sole respondent surface this plugin exposes.
const REQUEST_COMPOSITION_SELECT_MODE: &str = "composition.select_mode";

/// Wire-protocol payload version for the request/response
/// envelope.
const PAYLOAD_VERSION: u32 = 1;

/// Mode tokens this build declares. Kept in lockstep with
/// `manifest.toml`'s `[[capabilities.composition.modes]]`
/// entries; admission would refuse a mismatch between the
/// runtime's declared list and the manifest's.
const MODE_PASSTHROUGH: &str = "passthrough";
const DECLARED_MODES: &[&str] = &[MODE_PASSTHROUGH];

/// Parse the embedded plugin manifest.
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML).expect(
        "org-evoframework-composition-alsa: embedded manifest must parse",
    )
}

fn plugin_crate_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is valid semver")
}

/// ALSA composition plugin.
pub struct AlsaCompositionPlugin {
    loaded: bool,
    /// Active composition mode token. Reset to
    /// [`MODE_PASSTHROUGH`] at every successful load.
    current_mode: String,
    /// Audio routing handle pulled from
    /// [`LoadContext::audio_routing`] at load time. `None`
    /// before the first successful load and after every
    /// `unload`.
    audio_routing: Option<Arc<dyn AudioRouting>>,
    /// Cumulative `composition.select_mode` requests
    /// served, including refused ones. Surfaced for
    /// diagnostics; not part of the wire contract.
    requests_handled: u64,
}

impl AlsaCompositionPlugin {
    /// Construct a fresh plugin instance.
    pub fn new() -> Self {
        Self {
            loaded: false,
            current_mode: MODE_PASSTHROUGH.to_string(),
            audio_routing: None,
            requests_handled: 0,
        }
    }

    /// Cumulative `handle_request` invocations.
    pub fn requests_handled(&self) -> u64 {
        self.requests_handled
    }

    /// Currently active composition mode.
    pub fn current_mode(&self) -> &str {
        &self.current_mode
    }

    /// Load contract isolated to its testable inputs. The
    /// public [`Plugin::load`] entry pulls the routing
    /// handle off the context and forwards here; the split
    /// lets unit tests exercise the refuse-when-`None`
    /// contract without needing to construct a full
    /// [`LoadContext`] (which carries many mandatory
    /// trait-object fields).
    fn install_routing(
        &mut self,
        routing: Option<Arc<dyn AudioRouting>>,
    ) -> Result<(), PluginError> {
        let routing = routing.ok_or_else(|| {
            PluginError::Permanent(
                "composition plugin requires LoadContext::audio_routing; \
                 received None — manifest declares \
                 [capabilities.composition] but framework did not \
                 provision a handle. Indicates a manifest / trust / \
                 admission misconfiguration."
                    .to_string(),
            )
        })?;
        self.audio_routing = Some(routing);
        self.current_mode = MODE_PASSTHROUGH.to_string();
        self.loaded = true;
        Ok(())
    }
}

impl Default for AlsaCompositionPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for AlsaCompositionPlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: PLUGIN_NAME.to_string(),
                    version: plugin_crate_version(),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: vec![
                        REQUEST_COMPOSITION_SELECT_MODE.to_string()
                    ],
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
        async move { self.install_routing(ctx.audio_routing.clone()) }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            self.audio_routing = None;
            self.loaded = false;
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            if !self.loaded {
                return HealthReport::unhealthy(
                    "alsa composition plugin not loaded",
                );
            }
            // Probe the routing surface for diagnostics.
            // EndpointNotConfigured is a benign pre-
            // reconciliation state, not a fault — health
            // reflects the plugin's own readiness, not the
            // framework's reconciliation progress.
            let routing = self
                .audio_routing
                .as_ref()
                .expect("audio_routing populated when loaded");
            match routing.composition_endpoints() {
                Ok(_) => HealthReport::healthy(),
                Err(AudioRoutingError::EndpointNotConfigured) => {
                    HealthReport::healthy()
                }
                Err(other) => HealthReport::unhealthy(format!(
                    "audio routing surface returned an unexpected error: {other}"
                )),
            }
        }
    }
}

impl Respondent for AlsaCompositionPlugin {
    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "alsa composition plugin not loaded".to_string(),
                ));
            }
            if req.is_past_deadline() {
                return Err(PluginError::Transient(
                    "request deadline already expired".to_string(),
                ));
            }
            if req.request_type != REQUEST_COMPOSITION_SELECT_MODE {
                return Err(PluginError::Permanent(format!(
                    "unknown request type: {:?} (not one of: {:?})",
                    req.request_type,
                    [REQUEST_COMPOSITION_SELECT_MODE]
                )));
            }

            self.requests_handled += 1;

            let payload =
                match serde_json::from_slice::<SelectModeRequest>(&req.payload)
                {
                    Ok(v) => v,
                    Err(e) => {
                        return encode_response(
                            req,
                            SelectModeResponse::bad_request(format!(
                                "invalid JSON payload: {e}"
                            )),
                        );
                    }
                };

            if payload.v != PAYLOAD_VERSION {
                return encode_response(
                    req,
                    SelectModeResponse::bad_request(format!(
                        "unsupported payload version: {}; expected {}",
                        payload.v, PAYLOAD_VERSION
                    )),
                );
            }

            let mode = payload.mode.trim();
            if mode.is_empty() {
                return encode_response(
                    req,
                    SelectModeResponse::bad_request(
                        "mode must not be empty".to_string(),
                    ),
                );
            }
            if !DECLARED_MODES.contains(&mode) {
                return encode_response(
                    req,
                    SelectModeResponse::bad_request(format!(
                        "unknown mode {:?}; declared modes: {:?}",
                        mode, DECLARED_MODES
                    )),
                );
            }

            self.current_mode = mode.to_string();
            encode_response(
                req,
                SelectModeResponse::ok(self.current_mode.clone()),
            )
        }
    }
}

fn encode_response(
    req: &Request,
    out: SelectModeResponse,
) -> Result<Response, PluginError> {
    let body = serde_json::to_vec(&out).map_err(|e| {
        PluginError::Permanent(format!("response JSON encode failed: {e}"))
    })?;
    Ok(Response::for_request(req, body))
}

#[derive(Debug, Deserialize)]
struct SelectModeRequest {
    /// Request envelope version. Must equal
    /// [`PAYLOAD_VERSION`].
    v: u32,
    /// Requested mode token; must match a name in
    /// [`DECLARED_MODES`].
    mode: String,
}

#[derive(Debug, Serialize)]
struct SelectModeResponse {
    v: u32,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    active_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl SelectModeResponse {
    fn ok(active_mode: String) -> Self {
        Self {
            v: PAYLOAD_VERSION,
            status: "ok",
            active_mode: Some(active_mode),
            error: None,
        }
    }

    fn bad_request(error: String) -> Self {
        Self {
            v: PAYLOAD_VERSION,
            status: "bad_request",
            active_mode: None,
            error: Some(error),
        }
    }
}

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod tests {
    use super::test_support::StubAudioRouting;
    use super::*;

    use evo_plugin_sdk::contract::HealthStatus;
    use serde_json::{json, Value};

    fn decode_payload(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes).expect("response payload is valid JSON")
    }

    #[test]
    fn embedded_manifest_parses() {
        let m = manifest();
        assert_eq!(m.plugin.name, PLUGIN_NAME);
        assert_eq!(m.target.shelf, "audio.composition");
        assert_eq!(m.target.shape, 2);
        let composition = m
            .capabilities
            .composition
            .as_ref()
            .expect("manifest declares [capabilities.composition]");
        assert_eq!(composition.default_mode, MODE_PASSTHROUGH);
        assert!(composition
            .modes
            .iter()
            .any(|m| m.name == MODE_PASSTHROUGH && m.preserves_bit_perfect));
    }

    #[test]
    fn declared_modes_match_manifest_modes() {
        let m = manifest();
        let composition = m.capabilities.composition.unwrap();
        let manifest_names: Vec<&str> =
            composition.modes.iter().map(|x| x.name.as_str()).collect();
        // Round-trip: every const-table mode appears in the
        // manifest, and every manifest mode appears in the
        // const table. Drift between these two is caught
        // here at unit-test time rather than at admission.
        for declared in DECLARED_MODES {
            assert!(
                manifest_names.contains(declared),
                "DECLARED_MODES entry {:?} missing from manifest modes {:?}",
                declared,
                manifest_names
            );
        }
        for name in &manifest_names {
            assert!(
                DECLARED_MODES.contains(name),
                "manifest mode {:?} missing from DECLARED_MODES {:?}",
                name,
                DECLARED_MODES
            );
        }
    }

    #[tokio::test]
    async fn install_routing_refuses_when_handle_is_none() {
        let mut p = AlsaCompositionPlugin::new();
        let err = p
            .install_routing(None)
            .expect_err("composition plugin must refuse load without routing");
        match err {
            PluginError::Permanent(msg) => {
                assert!(
                    msg.contains("audio_routing"),
                    "refusal message must name the missing field: {msg:?}"
                );
            }
            other => panic!("expected Permanent error, got {other:?}"),
        }
        assert!(!p.loaded);
        assert!(p.audio_routing.is_none());
    }

    #[tokio::test]
    async fn install_routing_accepts_handle_and_resets_mode() {
        let mut p = AlsaCompositionPlugin::new();
        p.current_mode = "stale_value".to_string();
        let routing: Arc<dyn AudioRouting> = Arc::new(StubAudioRouting::new());
        p.install_routing(Some(Arc::clone(&routing)))
            .expect("install_routing must accept a Some handle");
        assert!(p.loaded);
        assert_eq!(p.current_mode, MODE_PASSTHROUGH);
        assert!(p.audio_routing.is_some());
    }

    #[tokio::test]
    async fn unload_clears_routing_and_loaded() {
        let mut p = AlsaCompositionPlugin::new();
        let routing: Arc<dyn AudioRouting> = Arc::new(StubAudioRouting::new());
        p.install_routing(Some(routing)).unwrap();
        assert!(p.loaded);
        p.unload().await.unwrap();
        assert!(!p.loaded);
        assert!(p.audio_routing.is_none());
    }

    #[tokio::test]
    async fn health_unhealthy_before_load() {
        let p = AlsaCompositionPlugin::new();
        assert!(matches!(
            p.health_check().await.status,
            HealthStatus::Unhealthy
        ));
    }

    #[tokio::test]
    async fn health_healthy_when_topology_pending() {
        // EndpointNotConfigured is a benign pre-
        // reconciliation state — health stays healthy
        // because the plugin's own surface is fine.
        let mut p = AlsaCompositionPlugin::new();
        p.install_routing(Some(Arc::new(StubAudioRouting::new()) as _))
            .unwrap();
        let report = p.health_check().await;
        assert!(matches!(report.status, HealthStatus::Healthy));
    }

    #[tokio::test]
    async fn select_mode_passthrough_succeeds() {
        let mut p = AlsaCompositionPlugin::new();
        p.install_routing(Some(Arc::new(StubAudioRouting::new()) as _))
            .unwrap();
        let req = Request {
            request_type: REQUEST_COMPOSITION_SELECT_MODE.to_string(),
            payload: json!({ "v": 1, "mode": "passthrough" })
                .to_string()
                .into_bytes(),
            correlation_id: 1,
            deadline: None,
            instance_id: None,
        };
        let resp = p.handle_request(&req).await.unwrap();
        let v = decode_payload(&resp.payload);
        assert_eq!(v["status"], "ok");
        assert_eq!(v["active_mode"], "passthrough");
        assert_eq!(p.current_mode(), "passthrough");
    }

    #[tokio::test]
    async fn select_mode_unknown_mode_refuses() {
        let mut p = AlsaCompositionPlugin::new();
        p.install_routing(Some(Arc::new(StubAudioRouting::new()) as _))
            .unwrap();
        let req = Request {
            request_type: REQUEST_COMPOSITION_SELECT_MODE.to_string(),
            payload: json!({ "v": 1, "mode": "eq_only" })
                .to_string()
                .into_bytes(),
            correlation_id: 2,
            deadline: None,
            instance_id: None,
        };
        let resp = p.handle_request(&req).await.unwrap();
        let v = decode_payload(&resp.payload);
        assert_eq!(v["status"], "bad_request");
        let err = v["error"].as_str().unwrap();
        assert!(err.contains("unknown mode"), "got: {err}");
        assert!(err.contains("eq_only"), "got: {err}");
        assert_eq!(p.current_mode(), MODE_PASSTHROUGH);
    }

    #[tokio::test]
    async fn select_mode_empty_mode_refuses() {
        let mut p = AlsaCompositionPlugin::new();
        p.install_routing(Some(Arc::new(StubAudioRouting::new()) as _))
            .unwrap();
        let req = Request {
            request_type: REQUEST_COMPOSITION_SELECT_MODE.to_string(),
            payload: json!({ "v": 1, "mode": "  " }).to_string().into_bytes(),
            correlation_id: 3,
            deadline: None,
            instance_id: None,
        };
        let resp = p.handle_request(&req).await.unwrap();
        let v = decode_payload(&resp.payload);
        assert_eq!(v["status"], "bad_request");
        assert!(v["error"].as_str().unwrap().contains("must not be empty"));
    }

    #[tokio::test]
    async fn select_mode_bad_version_refuses() {
        let mut p = AlsaCompositionPlugin::new();
        p.install_routing(Some(Arc::new(StubAudioRouting::new()) as _))
            .unwrap();
        let req = Request {
            request_type: REQUEST_COMPOSITION_SELECT_MODE.to_string(),
            payload: json!({ "v": 2, "mode": "passthrough" })
                .to_string()
                .into_bytes(),
            correlation_id: 4,
            deadline: None,
            instance_id: None,
        };
        let resp = p.handle_request(&req).await.unwrap();
        let v = decode_payload(&resp.payload);
        assert_eq!(v["status"], "bad_request");
        assert!(v["error"]
            .as_str()
            .unwrap()
            .contains("unsupported payload version"));
    }

    #[tokio::test]
    async fn select_mode_bad_json_refuses() {
        let mut p = AlsaCompositionPlugin::new();
        p.install_routing(Some(Arc::new(StubAudioRouting::new()) as _))
            .unwrap();
        let req = Request {
            request_type: REQUEST_COMPOSITION_SELECT_MODE.to_string(),
            payload: b"{not-json".to_vec(),
            correlation_id: 5,
            deadline: None,
            instance_id: None,
        };
        let resp = p.handle_request(&req).await.unwrap();
        let v = decode_payload(&resp.payload);
        assert_eq!(v["status"], "bad_request");
        assert!(v["error"]
            .as_str()
            .unwrap()
            .contains("invalid JSON payload"));
    }

    #[tokio::test]
    async fn handle_request_refused_when_not_loaded() {
        let mut p = AlsaCompositionPlugin::new();
        let req = Request {
            request_type: REQUEST_COMPOSITION_SELECT_MODE.to_string(),
            payload: json!({ "v": 1, "mode": "passthrough" })
                .to_string()
                .into_bytes(),
            correlation_id: 6,
            deadline: None,
            instance_id: None,
        };
        let err = p.handle_request(&req).await.unwrap_err();
        match err {
            PluginError::Permanent(msg) => {
                assert!(msg.contains("not loaded"));
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_request_type_refused() {
        let mut p = AlsaCompositionPlugin::new();
        p.install_routing(Some(Arc::new(StubAudioRouting::new()) as _))
            .unwrap();
        let req = Request {
            request_type: "alsa.pipeline.compose".to_string(),
            payload: b"{}".to_vec(),
            correlation_id: 7,
            deadline: None,
            instance_id: None,
        };
        let err = p.handle_request(&req).await.unwrap_err();
        match err {
            PluginError::Permanent(msg) => {
                assert!(msg.contains("unknown request type"));
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }
}
