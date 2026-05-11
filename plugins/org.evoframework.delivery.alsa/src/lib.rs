//! # org-evoframework-delivery-alsa
//!
//! Delivery-stage plugin for the evo audio data plane. Owns the
//! AAMPP-style modular ALSA pipeline (`pcm.evo`) and declares it
//! as the [`WriteEndpoint`] other audio-producing plugins write
//! into. Stocks the `audio.delivery` shelf at shape 2.
//!
//! ## What this plugin is
//!
//! A singleton respondent that occupies the terminal stage of the
//! audio data plane: source → composition → **delivery**. The
//! framework's reconciliation engine intersects this plugin's
//! declared `[capabilities.delivery].audio_formats` with each
//! upstream plugin's declared formats to pick a chain-wide format
//! per topology, then publishes per-stage endpoints to each
//! plugin via the [`AudioRouting`] handle stamped on each
//! plugin's [`LoadContext`].
//!
//! ## What this plugin does
//!
//! - **Owns the pcm.evo definition.** The plugin's declared
//!   `device = "alsa:evo"` names the ALSA pcm that audio bytes
//!   land at. The bootstrap script installs the canonical
//!   `/etc/asound.conf` (plug → hw:CARD=DAC,DEV=0); this plugin
//!   surfaces the active definition through the
//!   `delivery.active_endpoint` verb and (in follow-on chunks)
//!   re-renders it on operator-driven settings changes.
//!
//! - **Probes the host's audio hardware.** `delivery.list_cards`
//!   parses `aplay -L` to enumerate playback cards;
//!   `delivery.list_mixers` parses `amixer -c <N>` to enumerate
//!   per-card mixer controls. Used by the `playback.options`
//!   plugin (operator-facing settings) to drive output-device
//!   selection.
//!
//! - **Consumes its AudioRouting handle as Delivery role.** The
//!   framework hands the plugin a per-plugin
//!   [`AudioRouting`] at load time; the plugin uses
//!   [`AudioRouting::read_endpoint`] to learn what topology the
//!   framework's reconciliation engine has negotiated, and
//!   registers a [`RouteChangeCallback`] so it can re-render the
//!   asound.conf pipeline on every rewire (follow-on chunk).
//!
//! ## What this plugin does NOT do
//!
//! - **Move audio bytes.** ALSA's kernel + the asound chain in
//!   `/etc/asound.conf` are the byte mover. MPD (or any source)
//!   opens `pcm.evo` and writes bytes; the kernel routes through
//!   the chain to hardware. This plugin owns the chain
//!   _definition_, not the bytes flowing through it.
//!
//! - **Hold the operator's audiophile preferences.** That's the
//!   `org.evoframework.playback.options` plugin's job. The
//!   delivery plugin exposes mechanism (verbs that read host
//!   state, bind output devices); the options plugin exposes
//!   policy (which device to bind, resampling preference, mixer
//!   type, DOP, etc.).
//!
//! [`WriteEndpoint`]: evo_plugin_sdk::contract::audio_routing::WriteEndpoint
//! [`AudioRouting`]: evo_plugin_sdk::contract::audio_routing::AudioRouting
//! [`LoadContext`]: evo_plugin_sdk::contract::LoadContext
//! [`RouteChangeCallback`]: evo_plugin_sdk::contract::audio_routing::RouteChangeCallback

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// Same trait-shape constraint composition.alsa documents in its
// crate-level allow: the SDK's Plugin / Respondent trait
// methods return `impl Future<Output = _> + Send + '_` rather
// than `async fn` so the steward's multi-threaded tokio
// dispatch has a guaranteed Send bound. Clippy's
// `manual_async_fn` lint would push us off the SDK shape;
// keep it allowed crate-wide.
#![allow(clippy::manual_async_fn)]

use std::future::Future;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use evo_plugin_sdk::contract::audio_routing::AudioRouting;
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
pub const PLUGIN_NAME: &str = "org.evoframework.delivery.alsa";

/// Path to the system-wide ALSA configuration this plugin
/// surfaces through `delivery.active_endpoint`. Hardcoded to the
/// canonical Debian / Volumio location; vendor distributions
/// pointing at an alternative path will override this in a
/// future config-driven enhancement.
const ASOUND_CONF_PATH: &str = "/etc/asound.conf";

/// Wire-protocol payload version every respondent payload and
/// response carries. Independent of plugin SemVer; bumped on
/// incompatible wire-shape changes.
const PAYLOAD_VERSION: u32 = 1;

/// Request types this plugin honours. Mirrors
/// `manifest.toml`'s `[capabilities.respondent].request_types`;
/// admission would refuse a mismatch between the runtime's
/// declared list and the manifest's. The
/// `manifest_request_types_match_runtime` test enforces the
/// lockstep.
const REQUEST_TYPES: &[&str] = &[
    "delivery.probe_hardware",
    "delivery.list_cards",
    "delivery.list_mixers",
    "delivery.active_endpoint",
];

/// Parse the embedded plugin manifest.
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("org-evoframework-delivery-alsa: embedded manifest must parse")
}

fn plugin_crate_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is valid semver")
}

/// ALSA delivery plugin.
pub struct AlsaDeliveryPlugin {
    loaded: bool,
    /// Audio routing handle pulled from
    /// [`LoadContext::audio_routing`] at load time. `None` before
    /// the first successful load and after every `unload`.
    /// Delivery plugins MUST receive this handle; `Plugin::load`
    /// refuses loudly when it is `None`.
    audio_routing: Option<Arc<dyn AudioRouting>>,
    /// Path to the asound.conf the active pcm.evo definition
    /// lives at. Defaults to [`ASOUND_CONF_PATH`].
    asound_conf_path: PathBuf,
    /// Cumulative respondent requests handled since
    /// construction. Surfaced for diagnostics; not part of the
    /// wire contract.
    requests_handled: u64,
}

impl AlsaDeliveryPlugin {
    /// Construct a fresh plugin instance.
    pub fn new() -> Self {
        Self {
            loaded: false,
            audio_routing: None,
            asound_conf_path: PathBuf::from(ASOUND_CONF_PATH),
            requests_handled: 0,
        }
    }

    /// Replace the asound.conf path. Used by tests to point at a
    /// tempdir-backed file rather than `/etc/asound.conf`.
    #[cfg(test)]
    pub(crate) fn with_asound_conf_path(mut self, path: PathBuf) -> Self {
        self.asound_conf_path = path;
        self
    }

    /// Cumulative `handle_request` invocations.
    pub fn requests_handled(&self) -> u64 {
        self.requests_handled
    }

    /// Load contract isolated to its testable inputs: the audio
    /// routing handle. The public [`Plugin::load`] entry pulls
    /// the handle off the context and forwards here; the split
    /// lets unit tests exercise the refuse-when-None contract
    /// without needing to construct a full [`LoadContext`]
    /// (which carries many mandatory trait-object fields).
    fn install_routing(
        &mut self,
        routing: Option<Arc<dyn AudioRouting>>,
    ) -> Result<(), PluginError> {
        let routing = routing.ok_or_else(|| {
            PluginError::Permanent(
                "delivery.alsa plugin requires LoadContext::audio_routing; \
                 received None — manifest declares [capabilities.delivery] \
                 so the framework MUST provision an audio_routing handle. \
                 Indicates a manifest / trust / admission misconfiguration."
                    .to_string(),
            )
        })?;
        self.audio_routing = Some(routing);
        self.loaded = true;
        Ok(())
    }
}

impl Default for AlsaDeliveryPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for AlsaDeliveryPlugin {
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
            tracing::info!(
                plugin = PLUGIN_NAME,
                config_keys = ctx.config.len(),
                asound_conf_path = %self.asound_conf_path.display(),
                "plugin load beginning"
            );
            self.install_routing(ctx.audio_routing.clone())?;
            tracing::info!(
                plugin = PLUGIN_NAME,
                "plugin loaded; AAMPP delivery surface ready"
            );
            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            tracing::info!(
                plugin = PLUGIN_NAME,
                requests_handled = self.requests_handled,
                "plugin unload"
            );
            self.audio_routing = None;
            self.loaded = false;
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            if !self.loaded {
                return HealthReport::unhealthy(
                    "delivery.alsa plugin not loaded",
                );
            }
            HealthReport::healthy()
        }
    }
}

impl Respondent for AlsaDeliveryPlugin {
    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "delivery.alsa plugin not loaded".to_string(),
                ));
            }
            if req.is_past_deadline() {
                return Err(PluginError::Transient(
                    "request deadline already expired".to_string(),
                ));
            }
            if !REQUEST_TYPES.contains(&req.request_type.as_str()) {
                return Err(PluginError::Permanent(format!(
                    "unknown request type: {:?} (declared: {:?})",
                    req.request_type, REQUEST_TYPES
                )));
            }
            self.requests_handled += 1;
            match req.request_type.as_str() {
                "delivery.probe_hardware" => {
                    self.handle_probe_hardware(req).await
                }
                "delivery.list_cards" => self.handle_list_cards(req).await,
                "delivery.list_mixers" => self.handle_list_mixers(req).await,
                "delivery.active_endpoint" => {
                    self.handle_active_endpoint(req).await
                }
                other => Err(PluginError::Permanent(format!(
                    "request type {other:?} declared but no handler wired; \
                     manifest/runtime drift bug"
                ))),
            }
        }
    }
}

impl AlsaDeliveryPlugin {
    /// `delivery.probe_hardware` — run both `aplay -L` and
    /// `amixer` against every detected card, returning a unified
    /// snapshot of the host's audio hardware. Operator-facing
    /// settings UI calls this once at startup to populate the
    /// device-selection model.
    async fn handle_probe_hardware(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        parse_versioned_payload::<EmptyPayload>(req)?;
        let cards = run_aplay_l().await;
        let mut probe = HardwareProbe {
            v: PAYLOAD_VERSION,
            cards: Vec::with_capacity(cards.len()),
        };
        for card in cards {
            let mixers = run_amixer_controls(&card.alsa_id).await;
            probe.cards.push(HardwareCard {
                index: card.alsa_id.clone(),
                name: card.display_name.clone(),
                mixers,
            });
        }
        encode(req, &probe)
    }

    /// `delivery.list_cards` — lighter-weight than
    /// `probe_hardware`; returns just the card list without
    /// invoking `amixer` per card. Used by callers that only
    /// need to enumerate hardware.
    async fn handle_list_cards(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        parse_versioned_payload::<EmptyPayload>(req)?;
        let cards = run_aplay_l().await;
        let payload = CardList {
            v: PAYLOAD_VERSION,
            cards: cards
                .into_iter()
                .map(|c| CardEntry {
                    index: c.alsa_id,
                    name: c.display_name,
                })
                .collect(),
        };
        encode(req, &payload)
    }

    /// `delivery.list_mixers` — enumerate the simple-mixer
    /// controls available on the requested card. Caller passes
    /// the card index (e.g. `"0"`, `"DAC"`); the plugin invokes
    /// `amixer -c <index> scontrols` and parses the output.
    async fn handle_list_mixers(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: ListMixersRequest = parse_versioned_payload(req)?;
        let controls = run_amixer_controls(&payload.card).await;
        encode(
            req,
            &MixerList {
                v: PAYLOAD_VERSION,
                card: payload.card,
                controls,
            },
        )
    }

    /// `delivery.active_endpoint` — surface the current AAMPP
    /// pipeline state: the asound.conf path, whether it contains
    /// a `pcm.evo` definition, and the WriteEndpoint the
    /// framework has currently published (if any) to this
    /// plugin. Settings UI uses this to render "currently
    /// playing through: ALSA card X via pipeline Y".
    async fn handle_active_endpoint(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        parse_versioned_payload::<EmptyPayload>(req)?;
        let asound_conf =
            tokio::fs::read_to_string(&self.asound_conf_path).await.ok();
        let pcm_evo_defined = asound_conf
            .as_deref()
            .map(asound_conf_defines_pcm_evo)
            .unwrap_or(false);
        let framework_endpoint = match self.audio_routing.as_ref() {
            Some(routing) => match routing.read_endpoint() {
                Ok(ep) => Some(ReadEndpointSummary {
                    kind: format!("{:?}", ep.kind),
                    path: ep.path.display().to_string(),
                    format: format!("{:?}", ep.format),
                    buffer_frames: ep.buffer_frames,
                }),
                Err(_) => None,
            },
            None => None,
        };
        let payload = ActiveEndpoint {
            v: PAYLOAD_VERSION,
            asound_conf_path: self.asound_conf_path.display().to_string(),
            pcm_evo_defined,
            framework_endpoint,
        };
        encode(req, &payload)
    }
}

// ===== wire payload types =====

trait HasPayloadVersion {
    fn payload_version(&self) -> u32;
}

fn parse_versioned_payload<T>(req: &Request) -> Result<T, PluginError>
where
    T: serde::de::DeserializeOwned + HasPayloadVersion,
{
    let parsed: T = serde_json::from_slice(&req.payload).map_err(|e| {
        PluginError::Permanent(format!(
            "{:?} payload is not valid JSON for the expected shape: {e}",
            req.request_type
        ))
    })?;
    if parsed.payload_version() != PAYLOAD_VERSION {
        return Err(PluginError::Permanent(format!(
            "{:?} payload version {} unsupported; expected {}",
            req.request_type,
            parsed.payload_version(),
            PAYLOAD_VERSION
        )));
    }
    Ok(parsed)
}

fn default_payload_version() -> u32 {
    PAYLOAD_VERSION
}

fn encode<T: Serialize>(
    req: &Request,
    payload: &T,
) -> Result<Response, PluginError> {
    let body = serde_json::to_vec(payload).map_err(|e| {
        PluginError::Permanent(format!(
            "{:?} response JSON encode failed: {e}",
            req.request_type
        ))
    })?;
    Ok(Response::for_request(req, body))
}

#[derive(Debug, Deserialize)]
struct EmptyPayload {
    #[serde(default = "default_payload_version")]
    v: u32,
}

impl HasPayloadVersion for EmptyPayload {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

#[derive(Debug, Deserialize)]
struct ListMixersRequest {
    #[serde(default = "default_payload_version")]
    v: u32,
    card: String,
}

impl HasPayloadVersion for ListMixersRequest {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

#[derive(Debug, Serialize)]
struct HardwareProbe {
    v: u32,
    cards: Vec<HardwareCard>,
}

#[derive(Debug, Serialize)]
struct HardwareCard {
    index: String,
    name: String,
    mixers: Vec<String>,
}

#[derive(Debug, Serialize)]
struct CardList {
    v: u32,
    cards: Vec<CardEntry>,
}

#[derive(Debug, Serialize)]
struct CardEntry {
    index: String,
    name: String,
}

#[derive(Debug, Serialize)]
struct MixerList {
    v: u32,
    card: String,
    controls: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ActiveEndpoint {
    v: u32,
    asound_conf_path: String,
    pcm_evo_defined: bool,
    framework_endpoint: Option<ReadEndpointSummary>,
}

#[derive(Debug, Serialize)]
struct ReadEndpointSummary {
    kind: String,
    path: String,
    format: String,
    buffer_frames: u32,
}

// ===== hardware probing =====

/// One row from `aplay -L`'s structured output. The full output
/// includes both PCM names (left-aligned) and their descriptions
/// (indented). The parser pairs each PCM name with its first
/// description line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AplayCard {
    /// The ALSA pcm identifier as `aplay -L` prints it (e.g.
    /// `"hw:CARD=DAC,DEV=0"`, `"plughw:CARD=DAC,DEV=0"`,
    /// `"default"`).
    alsa_id: String,
    /// Operator-readable description (e.g. `"I-Sabre Q2M DAC,
    /// I-Sabre Q2M DAC i-sabre-codec-dai-0"`).
    display_name: String,
}

/// Run `aplay -L` and parse the output into a card list. Returns
/// an empty list when `aplay` is not on PATH or the process
/// failed — the plugin admits regardless (the probe surface is
/// best-effort).
async fn run_aplay_l() -> Vec<AplayCard> {
    let output = tokio::process::Command::new("aplay")
        .arg("-L")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await;
    let Ok(out) = output else {
        tracing::debug!(
            plugin = PLUGIN_NAME,
            "aplay -L unavailable on this host; returning empty card list"
        );
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    parse_aplay_l(&String::from_utf8_lossy(&out.stdout))
}

/// Parse `aplay -L` output. Each card is two consecutive lines:
/// a left-aligned PCM name then an indented description.
/// Non-card lines (the leading `null` / `default` / `sysdefault`
/// entries; HDMI dummies) are kept if they pair with a
/// description — the consumer filters on substring content.
fn parse_aplay_l(stdout: &str) -> Vec<AplayCard> {
    let mut cards = Vec::new();
    let mut pending_name: Option<String> = None;
    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        let is_indented =
            line.starts_with(char::is_whitespace) || line.starts_with('\t');
        if !is_indented {
            // New PCM name. Flush any pending name without a
            // description (rare; the trailing `null` /
            // `default` lines fall through with no detail line).
            if let Some(name) = pending_name.take() {
                cards.push(AplayCard {
                    alsa_id: name,
                    display_name: String::new(),
                });
            }
            pending_name = Some(line.trim().to_string());
        } else if let Some(name) = pending_name.take() {
            let detail = line.trim().to_string();
            cards.push(AplayCard {
                alsa_id: name,
                display_name: detail,
            });
        }
    }
    if let Some(name) = pending_name.take() {
        cards.push(AplayCard {
            alsa_id: name,
            display_name: String::new(),
        });
    }
    cards
}

/// Run `amixer -c <card> scontrols` and parse the simple-mixer
/// control names. Empty list on failure or when amixer is
/// absent.
async fn run_amixer_controls(card: &str) -> Vec<String> {
    let output = tokio::process::Command::new("amixer")
        .arg("-c")
        .arg(card)
        .arg("scontrols")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await;
    let Ok(out) = output else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    parse_amixer_scontrols(&String::from_utf8_lossy(&out.stdout))
}

/// Parse `amixer scontrols` output. Each line looks like:
///
/// ```text
/// Simple mixer control 'Master',0
/// Simple mixer control 'PCM',0
/// ```
///
/// Returns the names between single quotes.
fn parse_amixer_scontrols(stdout: &str) -> Vec<String> {
    let mut controls = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with("Simple mixer control") {
            continue;
        }
        if let Some(start) = line.find('\'') {
            if let Some(end) = line[start + 1..].find('\'') {
                let name = &line[start + 1..start + 1 + end];
                controls.push(name.to_string());
            }
        }
    }
    controls
}

/// Substring-test `asound.conf` content for a `pcm.evo`
/// definition. The check is deliberately loose — operators may
/// embed `pcm.evo` inside a larger config with comments,
/// includes, or other pcm definitions surrounding it; the test
/// only confirms the entry exists.
fn asound_conf_defines_pcm_evo(asound_conf: &str) -> bool {
    asound_conf.lines().any(|line| {
        let line = line.trim_start();
        if line.starts_with('#') {
            return false;
        }
        // ALSA syntax accepts `pcm.evo { ... }` or `pcm.evo
        // "name"` for aliases. Match either start.
        line.starts_with("pcm.evo ") || line.starts_with("pcm.evo\t")
    })
}

// =============================================================
// tests
// =============================================================

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use evo_plugin_sdk::audio::{AudioFormat, PcmCodec};
    use evo_plugin_sdk::contract::audio_routing::{
        AudioRoutingError, AudioRoutingMethod, CompositionEndpoints,
        EndpointKind, ReadEndpoint, RouteChangeCallback, WriteEndpoint,
    };
    use evo_plugin_sdk::contract::HealthStatus;
    use serde_json::{json, Value};
    use std::path::PathBuf;
    use tempfile::tempdir;

    // ----- AudioRouting stub for Delivery role -----

    #[derive(Default)]
    struct StubInner {
        read_endpoint: Option<ReadEndpoint>,
        callback: Option<RouteChangeCallback>,
    }

    pub(crate) struct StubDeliveryAudioRouting {
        inner: Mutex<StubInner>,
    }

    impl std::fmt::Debug for StubDeliveryAudioRouting {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("StubDeliveryAudioRouting")
                .finish_non_exhaustive()
        }
    }

    impl StubDeliveryAudioRouting {
        fn new() -> Self {
            Self {
                inner: Mutex::new(StubInner::default()),
            }
        }

        fn with_read_endpoint(self, ep: ReadEndpoint) -> Self {
            self.inner.lock().unwrap().read_endpoint = Some(ep);
            self
        }
    }

    impl AudioRouting for StubDeliveryAudioRouting {
        fn write_endpoint(&self) -> Result<WriteEndpoint, AudioRoutingError> {
            Err(AudioRoutingError::WrongStage {
                kind: AudioRoutingMethod::WriteEndpoint,
            })
        }

        fn read_endpoint(&self) -> Result<ReadEndpoint, AudioRoutingError> {
            self.inner
                .lock()
                .unwrap()
                .read_endpoint
                .clone()
                .ok_or(AudioRoutingError::EndpointNotConfigured)
        }

        fn composition_endpoints(
            &self,
        ) -> Result<CompositionEndpoints, AudioRoutingError> {
            Err(AudioRoutingError::NotCompositionPlugin)
        }

        fn current_format(&self) -> Result<AudioFormat, AudioRoutingError> {
            match &self.inner.lock().unwrap().read_endpoint {
                Some(ep) => Ok(ep.format.clone()),
                None => Err(AudioRoutingError::EndpointNotConfigured),
            }
        }

        fn on_route_change(&self, callback: Option<RouteChangeCallback>) {
            self.inner.lock().unwrap().callback = callback;
        }
    }

    // ----- manifest / surface tests -----

    #[test]
    fn embedded_manifest_parses() {
        let m = manifest();
        assert_eq!(m.plugin.name, PLUGIN_NAME);
        assert_eq!(m.target.shelf, "audio.delivery");
        assert_eq!(m.target.shape, 2);
        let delivery = m
            .capabilities
            .delivery
            .as_ref()
            .expect("manifest declares [capabilities.delivery]");
        assert_eq!(delivery.input_kind, "audio.pcm");
        assert_eq!(delivery.device, "alsa:evo");
        assert!(!delivery.bit_perfect_capable);
        assert!(!delivery.exclusive_mode);
    }

    #[test]
    fn manifest_request_types_match_runtime() {
        let m = manifest();
        let respondent = m
            .capabilities
            .respondent
            .as_ref()
            .expect("manifest declares [capabilities.respondent]");
        let manifest_types: Vec<&str> = respondent
            .request_types
            .iter()
            .map(String::as_str)
            .collect();
        for declared in REQUEST_TYPES {
            assert!(
                manifest_types.contains(declared),
                "REQUEST_TYPES entry {:?} missing from manifest {:?}",
                declared,
                manifest_types
            );
        }
        for ty in &manifest_types {
            assert!(
                REQUEST_TYPES.contains(ty),
                "manifest type {:?} missing from REQUEST_TYPES {:?}",
                ty,
                REQUEST_TYPES
            );
        }
    }

    #[tokio::test]
    async fn identity_matches_manifest() {
        let p = AlsaDeliveryPlugin::new();
        let d = p.describe().await;
        let m = manifest();
        assert_eq!(d.identity.name, m.plugin.name);
        assert_eq!(d.identity.version, m.plugin.version);
        assert_eq!(d.identity.contract, 1);
        assert!(!d.runtime_capabilities.accepts_custody);
    }

    #[tokio::test]
    async fn health_unhealthy_before_load() {
        let p = AlsaDeliveryPlugin::new();
        let r = p.health_check().await;
        assert!(matches!(r.status, HealthStatus::Unhealthy));
    }

    #[tokio::test]
    async fn install_routing_refuses_when_handle_is_none() {
        let mut p = AlsaDeliveryPlugin::new();
        let err = p
            .install_routing(None)
            .expect_err("delivery plugin must refuse load without routing");
        match err {
            PluginError::Permanent(msg) => {
                assert!(msg.contains("audio_routing"));
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
        assert!(!p.loaded);
    }

    #[tokio::test]
    async fn install_routing_accepts_handle() {
        let mut p = AlsaDeliveryPlugin::new();
        let routing: Arc<dyn AudioRouting> =
            Arc::new(StubDeliveryAudioRouting::new());
        p.install_routing(Some(routing)).unwrap();
        assert!(p.loaded);
    }

    // ----- aplay -L parser tests -----

    #[test]
    fn parse_aplay_l_empty() {
        assert!(parse_aplay_l("").is_empty());
    }

    #[test]
    fn parse_aplay_l_pairs_names_with_descriptions() {
        // Verbatim excerpt from reference target's `aplay -L` output.
        let raw = "\
hw:CARD=DAC,DEV=0
    I-Sabre Q2M DAC, I-Sabre Q2M DAC i-sabre-codec-dai-0
    Direct hardware device without any conversions
plughw:CARD=DAC,DEV=0
    I-Sabre Q2M DAC, I-Sabre Q2M DAC i-sabre-codec-dai-0
    Hardware device with all software conversions
";
        let cards = parse_aplay_l(raw);
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].alsa_id, "hw:CARD=DAC,DEV=0");
        assert!(cards[0].display_name.contains("I-Sabre Q2M DAC"));
        assert_eq!(cards[1].alsa_id, "plughw:CARD=DAC,DEV=0");
    }

    #[test]
    fn parse_aplay_l_handles_unpaired_names() {
        // `null` has no description line.
        let raw = "\
null
default
    Default Audio Device
";
        let cards = parse_aplay_l(raw);
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].alsa_id, "null");
        assert!(cards[0].display_name.is_empty());
        assert_eq!(cards[1].alsa_id, "default");
        assert_eq!(cards[1].display_name, "Default Audio Device");
    }

    // ----- amixer parser tests -----

    #[test]
    fn parse_amixer_scontrols_extracts_names_between_quotes() {
        let raw = "\
Simple mixer control 'Master',0
Simple mixer control 'PCM',0
Simple mixer control 'Digital',1
";
        let controls = parse_amixer_scontrols(raw);
        assert_eq!(controls, vec!["Master", "PCM", "Digital"]);
    }

    #[test]
    fn parse_amixer_scontrols_skips_non_matching_lines() {
        let raw = "\
amixer: Mixer attach hw:99 error: No such file or directory
";
        assert!(parse_amixer_scontrols(raw).is_empty());
    }

    // ----- asound.conf inspection -----

    #[test]
    fn pcm_evo_defined_when_present() {
        let conf = r#"
# Comment line
pcm.evo {
    type plug
    slave.pcm "hw:CARD=DAC,DEV=0"
}
"#;
        assert!(asound_conf_defines_pcm_evo(conf));
    }

    #[test]
    fn pcm_evo_undefined_when_absent() {
        let conf = "pcm.!default { type hw; card 0 }\n";
        assert!(!asound_conf_defines_pcm_evo(conf));
    }

    #[test]
    fn pcm_evo_definition_ignored_inside_comment() {
        let conf = "# pcm.evo { type plug; ... }\n";
        assert!(!asound_conf_defines_pcm_evo(conf));
    }

    // ----- respondent handler tests (do NOT exec aplay/amixer) -----

    async fn loaded_plugin() -> AlsaDeliveryPlugin {
        let mut p = AlsaDeliveryPlugin::new();
        let routing: Arc<dyn AudioRouting> =
            Arc::new(StubDeliveryAudioRouting::new());
        p.install_routing(Some(routing)).unwrap();
        p
    }

    fn request(verb: &str, payload: Value) -> Request {
        Request {
            request_type: verb.to_string(),
            payload: payload.to_string().into_bytes(),
            correlation_id: 1,
            deadline: None,
            instance_id: None,
        }
    }

    #[tokio::test]
    async fn handle_request_refused_when_not_loaded() {
        let mut p = AlsaDeliveryPlugin::new();
        let err = p
            .handle_request(&request("delivery.list_cards", json!({ "v": 1 })))
            .await
            .unwrap_err();
        assert!(matches!(err, PluginError::Permanent(_)));
    }

    #[tokio::test]
    async fn unknown_verb_refused() {
        let mut p = loaded_plugin().await;
        let err = p
            .handle_request(&request("delivery.nonsense", json!({ "v": 1 })))
            .await
            .unwrap_err();
        match err {
            PluginError::Permanent(msg) => {
                assert!(msg.contains("unknown request type"));
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_cards_accepts_legacy_payload_without_version_field() {
        let mut p = loaded_plugin().await;
        // Empty payload (no `v`) parses with default version.
        let resp = p
            .handle_request(&request("delivery.list_cards", json!({})))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(v["v"], 1);
        assert!(v["cards"].is_array());
    }

    #[tokio::test]
    async fn list_mixers_requires_card_field() {
        let mut p = loaded_plugin().await;
        // Missing `card` field — serde refuses with Permanent.
        let err = p
            .handle_request(&request("delivery.list_mixers", json!({ "v": 1 })))
            .await
            .unwrap_err();
        assert!(matches!(err, PluginError::Permanent(_)));
    }

    #[tokio::test]
    async fn active_endpoint_reports_no_framework_endpoint_pre_topology() {
        let mut p = loaded_plugin().await;
        let resp = p
            .handle_request(&request(
                "delivery.active_endpoint",
                json!({ "v": 1 }),
            ))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(v["v"], 1);
        assert!(v["framework_endpoint"].is_null());
    }

    #[tokio::test]
    async fn active_endpoint_reports_framework_endpoint_when_published() {
        let mut p = AlsaDeliveryPlugin::new();
        let endpoint = ReadEndpoint {
            kind: EndpointKind::AlsaPcm,
            path: PathBuf::from("evo"),
            format: AudioFormat::Pcm {
                codec: PcmCodec::PcmS16Le,
                rate_hz: 44_100,
                channels: 2,
            },
            buffer_frames: 1024,
        };
        let routing: Arc<dyn AudioRouting> = Arc::new(
            StubDeliveryAudioRouting::new().with_read_endpoint(endpoint),
        );
        p.install_routing(Some(routing)).unwrap();
        let resp = p
            .handle_request(&request(
                "delivery.active_endpoint",
                json!({ "v": 1 }),
            ))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&resp.payload).unwrap();
        assert!(v["framework_endpoint"].is_object());
        assert_eq!(v["framework_endpoint"]["path"], "evo");
        assert_eq!(v["framework_endpoint"]["buffer_frames"], 1024);
    }

    #[tokio::test]
    async fn active_endpoint_reads_asound_conf_for_pcm_evo() {
        let dir = tempdir().unwrap();
        let asound = dir.path().join("asound.conf");
        tokio::fs::write(
            &asound,
            "pcm.evo {\n    type plug\n    slave.pcm \"hw:0,0\"\n}\n",
        )
        .await
        .unwrap();

        let mut p = AlsaDeliveryPlugin::new().with_asound_conf_path(asound);
        let routing: Arc<dyn AudioRouting> =
            Arc::new(StubDeliveryAudioRouting::new());
        p.install_routing(Some(routing)).unwrap();

        let resp = p
            .handle_request(&request(
                "delivery.active_endpoint",
                json!({ "v": 1 }),
            ))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(v["pcm_evo_defined"], true);
    }

    #[tokio::test]
    async fn active_endpoint_missing_asound_conf_reports_not_defined() {
        let mut p = AlsaDeliveryPlugin::new()
            .with_asound_conf_path(PathBuf::from("/nonexistent/asound.conf"));
        let routing: Arc<dyn AudioRouting> =
            Arc::new(StubDeliveryAudioRouting::new());
        p.install_routing(Some(routing)).unwrap();
        let resp = p
            .handle_request(&request(
                "delivery.active_endpoint",
                json!({ "v": 1 }),
            ))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(v["pcm_evo_defined"], false);
    }

    #[tokio::test]
    async fn requests_handled_counter_advances_per_verb() {
        let mut p = loaded_plugin().await;
        assert_eq!(p.requests_handled(), 0);
        p.handle_request(&request("delivery.list_cards", json!({ "v": 1 })))
            .await
            .unwrap();
        assert_eq!(p.requests_handled(), 1);
        p.handle_request(&request(
            "delivery.active_endpoint",
            json!({ "v": 1 }),
        ))
        .await
        .unwrap();
        assert_eq!(p.requests_handled(), 2);
    }
}
