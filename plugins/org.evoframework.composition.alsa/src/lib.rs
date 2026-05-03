//! # org-evoframework-composition-alsa
//!
//! Singleton respondent that composes a modular ALSA pipeline from ordered
//! module contributions. It accepts `alsa.pipeline.compose` requests and
//! returns a deterministic projection:
//!
//! - composed `asound.conf` text,
//! - final playback PCM alias,
//! - MPD `audio_output` snippet targeting that alias.
//!
//! ## Request JSON (`v=1`)
//!
//! ```json
//! {
//!   "v": 1,
//!   "output": { "pcm": "hw:0,0", "ctl": "0" },
//!   "modules": [
//!     {
//!       "plugin": "org.evoframework.resampler",
//!       "id": "soxr",
//!       "order": 10,
//!       "snippet_template": "pcm.soxr_out { ... \"{{input_pcm}}\" ... }",
//!       "output_pcm": "soxr_out"
//!     }
//!   ],
//!   "final_alias": "volumio_pipeline"
//! }
//! ```
//!
//! `modules` are sorted deterministically by `(order, plugin, id)`.
//! `snippet_template` must contain `{{input_pcm}}`, which is replaced
//! with the previous stage's PCM name.
//!
//! ## Response JSON
//!
//! - `status = "ok"`: `pipeline` is present.
//! - `status = "bad_request"`: `error` explains contract/validation failure.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::manual_async_fn)]

use std::collections::HashSet;
use std::future::Future;

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
const REQUEST_ALSA_PIPELINE_COMPOSE: &str = "alsa.pipeline.compose";
const PAYLOAD_VERSION: u32 = 1;
const DEFAULT_FINAL_ALIAS: &str = "evo_modular_pipeline";
const INPUT_PCM_PLACEHOLDER: &str = "{{input_pcm}}";

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
    requests_handled: u64,
}

impl AlsaCompositionPlugin {
    /// Construct a fresh plugin instance.
    pub fn new() -> Self {
        Self {
            loaded: false,
            requests_handled: 0,
        }
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
                        REQUEST_ALSA_PIPELINE_COMPOSE.to_string()
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
        _ctx: &'a LoadContext,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
        async move {
            self.loaded = true;
            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            self.loaded = false;
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            if self.loaded {
                HealthReport::healthy()
            } else {
                HealthReport::unhealthy("alsa composition plugin not loaded")
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
            if req.request_type != REQUEST_ALSA_PIPELINE_COMPOSE {
                return Err(PluginError::Permanent(format!(
                    "unknown request type: {:?} (not one of: {:?})",
                    req.request_type,
                    [REQUEST_ALSA_PIPELINE_COMPOSE]
                )));
            }

            self.requests_handled += 1;

            let payload =
                match serde_json::from_slice::<ComposeRequest>(&req.payload) {
                    Ok(v) => v,
                    Err(e) => {
                        let out = ComposeResponse::bad_request(format!(
                            "invalid JSON payload: {e}"
                        ));
                        let body =
                            serde_json::to_vec(&out).map_err(|serr| {
                                PluginError::Permanent(format!(
                                    "serialize bad_request response: {serr}"
                                ))
                            })?;
                        return Ok(Response::for_request(req, body));
                    }
                };

            let out = if payload.v != PAYLOAD_VERSION {
                ComposeResponse::bad_request(format!(
                    "unsupported payload version: {}; expected {}",
                    payload.v, PAYLOAD_VERSION
                ))
            } else {
                match compose_pipeline(&payload) {
                    Ok(pipeline) => ComposeResponse::ok(pipeline),
                    Err(e) => ComposeResponse::bad_request(e),
                }
            };

            let body = serde_json::to_vec(&out).map_err(|e| {
                PluginError::Permanent(format!(
                    "response JSON encode failed: {e}"
                ))
            })?;
            Ok(Response::for_request(req, body))
        }
    }
}

#[derive(Debug, Deserialize)]
struct ComposeRequest {
    /// Request schema version. Must equal [`PAYLOAD_VERSION`].
    v: u32,
    /// Base output target before modular stages.
    output: OutputTarget,
    /// Ordered plugin module contributions.
    #[serde(default)]
    modules: Vec<ModuleContribution>,
    /// Final alias exposed as `pcm.<final_alias>`.
    #[serde(default)]
    final_alias: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OutputTarget {
    /// Base PCM target (for example `hw:0,0`).
    pcm: String,
    /// Optional ALSA control card token for `ctl.<final_alias>`.
    #[serde(default)]
    ctl: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ModuleContribution {
    /// Contributing plugin name (reverse-DNS).
    plugin: String,
    /// Stable plugin-local module id.
    id: String,
    /// Lower value means closer to hardware.
    order: i32,
    /// ALSA snippet template containing `{{input_pcm}}`.
    snippet_template: String,
    /// Output PCM name produced by this module stage.
    output_pcm: String,
}

#[derive(Debug, Serialize)]
struct ComposeResponse {
    v: u32,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pipeline: Option<PipelineProjection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl ComposeResponse {
    fn ok(pipeline: PipelineProjection) -> Self {
        Self {
            v: PAYLOAD_VERSION,
            status: "ok",
            pipeline: Some(pipeline),
            error: None,
        }
    }

    fn bad_request(error: String) -> Self {
        Self {
            v: PAYLOAD_VERSION,
            status: "bad_request",
            pipeline: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Serialize)]
struct PipelineProjection {
    signature: String,
    final_pcm: String,
    asound_conf: String,
    mpd_audio_output: String,
    modules_applied: Vec<AppliedModule>,
}

#[derive(Debug, Serialize)]
struct AppliedModule {
    plugin: String,
    id: String,
    order: i32,
    output_pcm: String,
}

fn compose_pipeline(
    req: &ComposeRequest,
) -> Result<PipelineProjection, String> {
    let base_pcm = req.output.pcm.trim();
    if base_pcm.is_empty() {
        return Err("output.pcm must not be empty".to_string());
    }
    validate_pcm_reference(base_pcm, "output.pcm")?;

    let final_alias = req
        .final_alias
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_FINAL_ALIAS)
        .to_string();
    validate_alias_token(&final_alias, "final_alias")?;

    let mut modules: Vec<&ModuleContribution> = req.modules.iter().collect();
    modules.sort_by(|a, b| {
        a.order
            .cmp(&b.order)
            .then(a.plugin.cmp(&b.plugin))
            .then(a.id.cmp(&b.id))
    });

    let mut seen = HashSet::new();
    let mut rendered = String::new();
    let mut applied = Vec::with_capacity(modules.len());

    let mut current_pcm = base_pcm.to_string();
    for module in modules {
        let plugin = module.plugin.trim();
        if plugin.is_empty() {
            return Err("module plugin must not be empty".to_string());
        }
        let module_id = module.id.trim();
        if module_id.is_empty() {
            return Err(format!("module from {} has empty id", plugin));
        }
        validate_alias_token(module_id, "module.id")?;

        let key = format!("{}::{}", plugin, module_id);
        if !seen.insert(key.clone()) {
            return Err(format!("duplicate module identity: {key}"));
        }

        let output_pcm = module.output_pcm.trim();
        if output_pcm.is_empty() {
            return Err(format!("module {} has empty output_pcm", key));
        }
        validate_alias_token(output_pcm, "module.output_pcm")?;
        if module.snippet_template.trim().is_empty() {
            return Err(format!("module {} has empty snippet_template", key));
        }
        if !module.snippet_template.contains(INPUT_PCM_PLACEHOLDER) {
            return Err(format!(
                "module {} snippet_template must contain {}",
                key, INPUT_PCM_PLACEHOLDER
            ));
        }

        let snippet = module
            .snippet_template
            .replace(INPUT_PCM_PLACEHOLDER, &current_pcm)
            .trim()
            .to_string();
        rendered.push_str(&format!(
            "# module {} (order {})\n{}\n\n",
            key, module.order, snippet
        ));

        current_pcm = output_pcm.to_string();
        applied.push(AppliedModule {
            plugin: plugin.to_string(),
            id: module_id.to_string(),
            order: module.order,
            output_pcm: current_pcm.clone(),
        });
    }

    let mut asound_conf = String::new();
    asound_conf.push_str(
        "# generated by org.evoframework.composition.alsa\n\
         # lower order modules sit closer to hardware\n\n",
    );
    asound_conf.push_str(&rendered);
    asound_conf.push_str(&format!(
        "pcm.{alias} {{\n\
         \ttype plug\n\
         \tslave.pcm \"{pcm}\"\n\
         }}\n",
        alias = final_alias,
        pcm = current_pcm
    ));
    if let Some(ctl) = req
        .output
        .ctl
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        validate_ctl_reference(ctl, "output.ctl")?;
        asound_conf.push_str(&format!(
            "\nctl.{alias} {{\n\
             \ttype hw\n\
             \tcard \"{ctl}\"\n\
             }}\n",
            alias = final_alias,
            ctl = ctl
        ));
    }

    let mpd_audio_output = format!(
        "audio_output {{\n\
         \ttype \"alsa\"\n\
         \tname \"Evo Modular Pipeline\"\n\
         \tdevice \"{alias}\"\n\
         \tmixer_type \"none\"\n\
         }}",
        alias = final_alias
    );

    let signature = format!(
        "base={};chain={};final={}",
        base_pcm,
        applied
            .iter()
            .map(|m| format!("{}:{}@{}", m.plugin, m.id, m.order))
            .collect::<Vec<_>>()
            .join(","),
        current_pcm
    );

    Ok(PipelineProjection {
        signature,
        final_pcm: final_alias,
        asound_conf,
        mpd_audio_output,
        modules_applied: applied,
    })
}

fn is_alias_token_valid(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn validate_alias_token(value: &str, field: &str) -> Result<(), String> {
    if is_alias_token_valid(value) {
        Ok(())
    } else {
        Err(format!(
            "{} must match [A-Za-z0-9_.-]+, got {:?}",
            field, value
        ))
    }
}

fn is_pcm_reference_valid(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '_' | '-' | '.' | ':' | ',')
        })
}

fn validate_pcm_reference(value: &str, field: &str) -> Result<(), String> {
    if is_pcm_reference_valid(value) {
        Ok(())
    } else {
        Err(format!(
            "{} contains invalid characters, got {:?}",
            field, value
        ))
    }
}

fn validate_ctl_reference(value: &str, field: &str) -> Result<(), String> {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
    {
        Ok(())
    } else {
        Err(format!(
            "{} contains invalid characters, got {:?}",
            field, value
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use evo_plugin_sdk::contract::HealthStatus;
    use serde_json::{json, Value};

    #[test]
    fn embedded_manifest_parses() {
        let m = manifest();
        assert_eq!(m.plugin.name, PLUGIN_NAME);
        assert_eq!(m.target.shelf, "audio.composition");
    }

    #[tokio::test]
    async fn health_before_and_after_load() {
        let mut p = AlsaCompositionPlugin::new();
        assert!(matches!(
            p.health_check().await.status,
            HealthStatus::Unhealthy
        ));
        p.loaded = true;
        assert!(matches!(
            p.health_check().await.status,
            HealthStatus::Healthy
        ));
    }

    fn compose_request_payload() -> Vec<u8> {
        json!({
            "v": 1,
            "output": {
                "pcm": "hw:0,0",
                "ctl": "0"
            },
            "modules": [
                {
                    "plugin": "org.evoframework.eq",
                    "id": "peq",
                    "order": 20,
                    "snippet_template": "pcm.eq_out {\n\ttype plug\n\tslave.pcm \"{{input_pcm}}\"\n}",
                    "output_pcm": "eq_out"
                },
                {
                    "plugin": "org.evoframework.resampler",
                    "id": "soxr",
                    "order": 10,
                    "snippet_template": "pcm.soxr_out {\n\ttype plug\n\tslave.pcm \"{{input_pcm}}\"\n}",
                    "output_pcm": "soxr_out"
                }
            ],
            "final_alias": "volumio_pipeline"
        })
        .to_string()
        .into_bytes()
    }

    fn decode_payload(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes).expect("response payload is valid JSON")
    }

    #[tokio::test]
    async fn handle_request_composes_sorted_pipeline() {
        let mut p = AlsaCompositionPlugin::new();
        p.loaded = true;

        let req = Request {
            request_type: REQUEST_ALSA_PIPELINE_COMPOSE.to_string(),
            payload: compose_request_payload(),
            correlation_id: 1,
            deadline: None,
            instance_id: None,
        };
        let out = p.handle_request(&req).await.unwrap();
        let v = decode_payload(&out.payload);

        assert_eq!(v["status"], "ok");
        assert_eq!(v["pipeline"]["final_pcm"], "volumio_pipeline");
        assert_eq!(
            v["pipeline"]["signature"],
            "base=hw:0,0;chain=org.evoframework.resampler:soxr@10,org.evoframework.eq:peq@20;final=eq_out"
        );
        let conf = v["pipeline"]["asound_conf"].as_str().unwrap();
        let expected_conf =
            "# generated by org.evoframework.composition.alsa\n\
# lower order modules sit closer to hardware\n\n\
# module org.evoframework.resampler::soxr (order 10)\n\
pcm.soxr_out {\n\
\ttype plug\n\
\tslave.pcm \"hw:0,0\"\n\
}\n\n\
# module org.evoframework.eq::peq (order 20)\n\
pcm.eq_out {\n\
\ttype plug\n\
\tslave.pcm \"soxr_out\"\n\
}\n\n\
pcm.volumio_pipeline {\n\
\ttype plug\n\
\tslave.pcm \"eq_out\"\n\
}\n\
\n\
ctl.volumio_pipeline {\n\
\ttype hw\n\
\tcard \"0\"\n\
}\n";
        assert_eq!(conf, expected_conf);
        assert!(conf.contains("pcm.soxr_out"));
        assert!(conf.contains("slave.pcm \"hw:0,0\""));
        assert!(conf.contains("pcm.eq_out"));
        assert!(conf.contains("slave.pcm \"soxr_out\""));
        assert!(conf.contains("pcm.volumio_pipeline"));
        assert!(conf.contains("slave.pcm \"eq_out\""));
    }

    #[tokio::test]
    async fn no_modules_uses_default_alias_and_base_pcm() {
        let mut p = AlsaCompositionPlugin::new();
        p.loaded = true;
        let payload = json!({
            "v": 1,
            "output": { "pcm": "plughw:2,0" },
            "modules": []
        })
        .to_string()
        .into_bytes();
        let req = Request {
            request_type: REQUEST_ALSA_PIPELINE_COMPOSE.to_string(),
            payload,
            correlation_id: 11,
            deadline: None,
            instance_id: None,
        };
        let out = p.handle_request(&req).await.unwrap();
        let v = decode_payload(&out.payload);
        assert_eq!(v["status"], "ok");
        assert_eq!(v["pipeline"]["final_pcm"], DEFAULT_FINAL_ALIAS);
        assert_eq!(
            v["pipeline"]["signature"],
            "base=plughw:2,0;chain=;final=plughw:2,0"
        );
        let conf = v["pipeline"]["asound_conf"].as_str().unwrap();
        assert!(conf.contains("pcm.evo_modular_pipeline"));
        assert!(conf.contains("slave.pcm \"plughw:2,0\""));
    }

    #[tokio::test]
    async fn compose_with_equal_order_uses_plugin_id_tie_breaker() {
        let mut p = AlsaCompositionPlugin::new();
        p.loaded = true;
        let payload = json!({
            "v": 1,
            "output": { "pcm": "hw:0,0" },
            "modules": [
                {
                    "plugin": "org.z",
                    "id": "b",
                    "order": 10,
                    "snippet_template": "pcm.zb { type plug slave.pcm \"{{input_pcm}}\" }",
                    "output_pcm": "zb"
                },
                {
                    "plugin": "org.a",
                    "id": "a",
                    "order": 10,
                    "snippet_template": "pcm.aa { type plug slave.pcm \"{{input_pcm}}\" }",
                    "output_pcm": "aa"
                }
            ]
        })
        .to_string()
        .into_bytes();
        let req = Request {
            request_type: REQUEST_ALSA_PIPELINE_COMPOSE.to_string(),
            payload,
            correlation_id: 12,
            deadline: None,
            instance_id: None,
        };
        let out = p.handle_request(&req).await.unwrap();
        let v = decode_payload(&out.payload);
        assert_eq!(v["status"], "ok");
        assert_eq!(
            v["pipeline"]["signature"],
            "base=hw:0,0;chain=org.a:a@10,org.z:b@10;final=zb"
        );
    }

    #[tokio::test]
    async fn handle_request_bad_json_returns_bad_request() {
        let mut p = AlsaCompositionPlugin::new();
        p.loaded = true;
        let req = Request {
            request_type: REQUEST_ALSA_PIPELINE_COMPOSE.to_string(),
            payload: b"{not-json".to_vec(),
            correlation_id: 2,
            deadline: None,
            instance_id: None,
        };
        let out = p.handle_request(&req).await.unwrap();
        let v = decode_payload(&out.payload);
        assert_eq!(v["status"], "bad_request");
        assert!(v["error"]
            .as_str()
            .unwrap()
            .contains("invalid JSON payload"));
    }

    #[tokio::test]
    async fn duplicate_modules_return_bad_request() {
        let mut p = AlsaCompositionPlugin::new();
        p.loaded = true;
        let payload = json!({
            "v": 1,
            "output": { "pcm": "hw:0,0" },
            "modules": [
                {
                    "plugin": "org.evoframework.eq",
                    "id": "peq",
                    "order": 10,
                    "snippet_template": "pcm.eq_out { type plug slave.pcm \"{{input_pcm}}\" }",
                    "output_pcm": "eq_out"
                },
                {
                    "plugin": "org.evoframework.eq",
                    "id": "peq",
                    "order": 20,
                    "snippet_template": "pcm.eq2_out { type plug slave.pcm \"{{input_pcm}}\" }",
                    "output_pcm": "eq2_out"
                }
            ]
        })
        .to_string()
        .into_bytes();
        let req = Request {
            request_type: REQUEST_ALSA_PIPELINE_COMPOSE.to_string(),
            payload,
            correlation_id: 3,
            deadline: None,
            instance_id: None,
        };
        let out = p.handle_request(&req).await.unwrap();
        let v = decode_payload(&out.payload);
        assert_eq!(v["status"], "bad_request");
        assert!(v["error"]
            .as_str()
            .unwrap()
            .contains("duplicate module identity"));
    }

    #[tokio::test]
    async fn invalid_version_returns_bad_request() {
        let mut p = AlsaCompositionPlugin::new();
        p.loaded = true;
        let payload = json!({
            "v": 2,
            "output": { "pcm": "hw:0,0" }
        })
        .to_string()
        .into_bytes();
        let req = Request {
            request_type: REQUEST_ALSA_PIPELINE_COMPOSE.to_string(),
            payload,
            correlation_id: 4,
            deadline: None,
            instance_id: None,
        };
        let out = p.handle_request(&req).await.unwrap();
        let v = decode_payload(&out.payload);
        assert_eq!(v["status"], "bad_request");
        assert!(v["error"]
            .as_str()
            .unwrap()
            .contains("unsupported payload version"));
    }

    #[tokio::test]
    async fn invalid_final_alias_returns_bad_request() {
        let mut p = AlsaCompositionPlugin::new();
        p.loaded = true;
        let payload = json!({
            "v": 1,
            "output": { "pcm": "hw:0,0" },
            "final_alias": "bad alias"
        })
        .to_string()
        .into_bytes();
        let req = Request {
            request_type: REQUEST_ALSA_PIPELINE_COMPOSE.to_string(),
            payload,
            correlation_id: 5,
            deadline: None,
            instance_id: None,
        };
        let out = p.handle_request(&req).await.unwrap();
        let v = decode_payload(&out.payload);
        assert_eq!(v["status"], "bad_request");
        assert!(v["error"]
            .as_str()
            .unwrap()
            .contains("final_alias must match"));
    }

    #[tokio::test]
    async fn module_template_without_placeholder_returns_bad_request() {
        let mut p = AlsaCompositionPlugin::new();
        p.loaded = true;
        let payload = json!({
            "v": 1,
            "output": { "pcm": "hw:0,0" },
            "modules": [
                {
                    "plugin": "org.evoframework.eq",
                    "id": "peq",
                    "order": 10,
                    "snippet_template": "pcm.eq_out { type plug slave.pcm \"eq\" }",
                    "output_pcm": "eq_out"
                }
            ]
        })
        .to_string()
        .into_bytes();
        let req = Request {
            request_type: REQUEST_ALSA_PIPELINE_COMPOSE.to_string(),
            payload,
            correlation_id: 6,
            deadline: None,
            instance_id: None,
        };
        let out = p.handle_request(&req).await.unwrap();
        let v = decode_payload(&out.payload);
        assert_eq!(v["status"], "bad_request");
        assert!(v["error"]
            .as_str()
            .unwrap()
            .contains("snippet_template must contain"));
    }

    #[tokio::test]
    async fn module_output_pcm_with_invalid_chars_returns_bad_request() {
        let mut p = AlsaCompositionPlugin::new();
        p.loaded = true;
        let payload = json!({
            "v": 1,
            "output": { "pcm": "hw:0,0" },
            "modules": [
                {
                    "plugin": "org.evoframework.eq",
                    "id": "peq",
                    "order": 10,
                    "snippet_template": "pcm.eq_out { type plug slave.pcm \"{{input_pcm}}\" }",
                    "output_pcm": "bad alias"
                }
            ]
        })
        .to_string()
        .into_bytes();
        let req = Request {
            request_type: REQUEST_ALSA_PIPELINE_COMPOSE.to_string(),
            payload,
            correlation_id: 7,
            deadline: None,
            instance_id: None,
        };
        let out = p.handle_request(&req).await.unwrap();
        let v = decode_payload(&out.payload);
        assert_eq!(v["status"], "bad_request");
        assert!(v["error"]
            .as_str()
            .unwrap()
            .contains("module.output_pcm must match"));
    }
}
