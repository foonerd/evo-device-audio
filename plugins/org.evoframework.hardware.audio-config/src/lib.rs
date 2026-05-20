//! # org-evoframework-hardware-audio-config
//!
//! Reference-device hardware-audio configuration plugin. Owns the
//! audio device's I2S DAC catalogue, the boot-config dtoverlay block,
//! and the companion module drop-in (Pi-only). Publishes the
//! operator-visible capability surface that the rest of the audio
//! plugin population reacts against:
//!
//! - `evo.hardware.audio:capabilities` — the DAC catalogue filtered
//!   by the host's resolved board profile + the resolved profile
//!   itself.
//! - `evo.hardware.audio:active_config` — the currently-applied DAC
//!   identifier + the resolved alsacard / mixer hints downstream
//!   plugins (delivery.alsa, playback.mpd) use to bind their pieces
//!   without re-probing the host.
//! - `evo.hardware.audio:pending_reboot` — boolean flag flipped to
//!   true on every successful `select_dac` / `clear_dac` write,
//!   cleared by the operator once the host has booted with the new
//!   overlay.
//!
//! Stocks the `hardware.audio` shelf at shape 1.
//!
//! ## What this plugin is
//!
//! A singleton respondent. Its job is **enablement**: surfacing the
//! DAC catalogue, writing the boot-config dtoverlay block, and
//! publishing the resolution downstream plugins read. It does NOT
//! touch ALSA, MPD, or the steward's audio data plane — that is the
//! delivery + playback + composition plugins' work.
//!
//! ## What this plugin does
//!
//! - On load: resolves the host's board profile (via the provider
//!   trait), embeds + parses the bundled DAC catalogue, announces
//!   the three capability subjects, reads the host's current
//!   `dtoverlay=` token to seed `active_config`.
//!
//! - On `hardware.audio.select_dac`: validates the catalogue id,
//!   delegates the boot-config write to the provider, refreshes the
//!   `active_config` + `pending_reboot` subjects, emits a
//!   `hardware.audio.reboot_required` happening.
//!
//! - On `hardware.audio.clear_dac`: delegates the clear to the
//!   provider, refreshes the subjects, emits a happening when the
//!   on-disk state actually changed.
//!
//! - On `hardware.audio.current_config`: returns the live
//!   `ActiveConfig` via a read-through to the provider, enriched
//!   with catalogue lookup (catalogue id, alsacard hint, mixer
//!   hint).
//!
//! - On `hardware.audio.confirm_reboot_required`: returns the
//!   `pending_reboot` flag.
//!
//! [`LoadContext`]: evo_plugin_sdk::contract::LoadContext
//! [`Respondent`]: evo_plugin_sdk::contract::Respondent

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::manual_async_fn)]

pub mod dacs;
pub mod provider;
pub mod provider_pi;

use std::future::Future;
use std::sync::Arc;

use evo_plugin_sdk::contract::{
    BuildInfo, ExternalAddressing, HappeningEmitter, HealthReport, LoadContext,
    Plugin, PluginDescription, PluginError, PluginIdentity, Request,
    Respondent, Response, RuntimeCapabilities, SubjectAnnouncement,
    SubjectAnnouncer,
};
use evo_plugin_sdk::Manifest;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::dacs::{
    dac_list_for_profile, find_dac, parse_dacs, DacEntry, DacsFile,
};
use crate::provider::{
    ActiveConfig, ApplyOutcome, HardwareAudioProvider, NoopProvider,
};
use crate::provider_pi::PiProvider;

/// Embedded plugin manifest.
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");

/// Embedded DAC catalogue — frozen provenance snapshot of Volumio's
/// upstream `dacs.json`. Lives under `data/import/` to signal its
/// non-load-bearing build-time-only role. ADR-0132 retires runtime
/// Volumio JSON parsing in a follow-on commit; the current location
/// already enforces the source-of-truth-is-frozen invariant.
pub const EMBEDDED_DACS_JSON: &str =
    include_str!("../data/import/volumio-dacs.json");

/// Embedded Volumio dac_dsp.json — frozen provenance snapshot.
/// Names-only filter (10 entries, each carrying card name +
/// `dsp_options[]` ALSA mixer-control-name array). Joined with the
/// curated DSP control pool + runtime amixer introspection by the
/// DSP capability resolver landing in the next P0 sub-phase.
pub const EMBEDDED_VOLUMIO_DAC_DSP_JSON: &str =
    include_str!("../data/import/volumio-dac-dsp.json");

/// Plugin identity name (must match manifest).
pub const PLUGIN_NAME: &str = "org.evoframework.hardware.audio-config";

/// Wire-protocol payload version every request + response carries.
const PAYLOAD_VERSION: u32 = 1;

/// Happening event_type emitted on every successful write that flips
/// the on-disk dtoverlay state.
const REBOOT_REQUIRED_EVENT: &str = "hardware.audio.reboot_required";

/// Happening event_type emitted after a successful `select_dac`.
const SELECTED_EVENT: &str = "hardware.audio.selected";

/// Happening event_type emitted after a successful `clear_dac`.
const CLEARED_EVENT: &str = "hardware.audio.cleared";

/// Subject scheme — single root for the plugin's three subjects.
const SUBJECT_SCHEME: &str = "evo.hardware.audio";
const SUBJECT_VALUE_CAPABILITIES: &str = "capabilities";
const SUBJECT_VALUE_ACTIVE_CONFIG: &str = "active_config";
const SUBJECT_VALUE_PENDING_REBOOT: &str = "pending_reboot";

/// Subject types the framework records. Underscored form because
/// the catalogue parser rejects subject-type names containing `.`.
const SUBJECT_TYPE_CAPABILITIES: &str = "hardware_audio_capabilities";
const SUBJECT_TYPE_ACTIVE_CONFIG: &str = "hardware_audio_active_config";
const SUBJECT_TYPE_PENDING_REBOOT: &str = "hardware_audio_pending_reboot";

/// Request types this plugin honours. Lockstep-matched against
/// `manifest.toml` [capabilities.respondent].request_types by the
/// `manifest_request_types_match_runtime` test.
const REQUEST_TYPES: &[&str] = &[
    "hardware.audio.list_dac_catalogue",
    "hardware.audio.select_dac",
    "hardware.audio.clear_dac",
    "hardware.audio.current_config",
    "hardware.audio.confirm_reboot_required",
];

/// Parse the embedded plugin manifest.
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML).expect(
        "org-evoframework-hardware-audio-config: embedded manifest must parse",
    )
}

fn plugin_crate_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is valid semver")
}

// =============================================================
// Plugin
// =============================================================

/// Hardware-audio configuration plugin.
pub struct HardwareAudioConfigPlugin {
    loaded: bool,
    catalogue: Option<DacsFile>,
    profile: String,
    profile_pinned: bool,
    provider: Arc<dyn HardwareAudioProvider>,
    provider_injected: bool,
    pending_reboot: Arc<RwLock<PendingRebootState>>,
    happening_emitter: Option<Arc<dyn HappeningEmitter>>,
    subject_announcer: Option<Arc<dyn SubjectAnnouncer>>,
    requests_handled: u64,
}

/// `evo.hardware.audio:pending_reboot` subject payload + the in-
/// memory state the plugin holds between writes.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PendingRebootState {
    /// `true` when the operator must reboot for the most recent
    /// dtoverlay change to take effect; `false` when no change is
    /// pending.
    pub pending: bool,
    /// Operator-readable cause of the pending state. Empty when
    /// `pending = false`.
    pub cause: String,
    /// Plugin-side timestamp (epoch milliseconds) of the most
    /// recent write that flipped this flag. `0` when never set.
    pub set_at_ms: u64,
}

impl HardwareAudioConfigPlugin {
    /// Construct a fresh plugin instance. The provider is selected
    /// against the resolved board profile at `load` time; pre-load
    /// the field with a [`NoopProvider`] so `describe` works before
    /// load.
    pub fn new() -> Self {
        Self {
            loaded: false,
            catalogue: None,
            profile: "Unknown".into(),
            profile_pinned: false,
            provider: Arc::new(NoopProvider::default()),
            provider_injected: false,
            pending_reboot: Arc::new(
                RwLock::new(PendingRebootState::default()),
            ),
            happening_emitter: None,
            subject_announcer: None,
            requests_handled: 0,
        }
    }

    /// Cumulative `handle_request` invocations.
    pub fn requests_handled(&self) -> u64 {
        self.requests_handled
    }

    /// Resolved board profile (e.g. `Raspberry PI`). `Unknown`
    /// until `load` runs.
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// Test-only override: swap the provider before `load` so unit
    /// tests can exercise the plugin against a hand-seeded in-
    /// memory boot config.
    #[cfg(test)]
    pub fn with_provider(
        mut self,
        provider: Arc<dyn HardwareAudioProvider>,
    ) -> Self {
        self.provider = provider;
        self.provider_injected = true;
        self
    }

    /// Test-only override: force the resolved profile (skips the
    /// board-probe path).
    #[cfg(test)]
    pub fn with_profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = profile.into();
        self.profile_pinned = true;
        self
    }

    /// Per-profile catalogue list. Empty when no catalogue is
    /// loaded or the resolved profile has no entries.
    fn list_catalogue(&self) -> Vec<DacEntry> {
        match self.catalogue.as_ref() {
            Some(c) => dac_list_for_profile(c, &self.profile),
            None => Vec::new(),
        }
    }

    /// Enrich an [`ActiveConfig`] read from the provider with
    /// catalogue lookups (catalogue id, alsacard hint, mixer hint).
    /// Pure function over the in-memory catalogue.
    fn enrich_active_config(&self, mut cfg: ActiveConfig) -> ActiveConfig {
        if cfg.overlay.is_empty() {
            return cfg;
        }
        if let Some(catalogue) = self.catalogue.as_ref() {
            for entry in dac_list_for_profile(catalogue, &self.profile) {
                if entry.overlay == cfg.overlay {
                    cfg.catalogue_id = Some(entry.id);
                    cfg.alsacard_hint = if entry.alsacard.is_empty() {
                        None
                    } else {
                        Some(entry.alsacard)
                    };
                    cfg.mixer_hint = if entry.mixer.is_empty() {
                        None
                    } else {
                        Some(entry.mixer)
                    };
                    break;
                }
            }
        }
        cfg
    }

    fn capabilities_addressing() -> ExternalAddressing {
        ExternalAddressing {
            scheme: SUBJECT_SCHEME.to_string(),
            value: SUBJECT_VALUE_CAPABILITIES.to_string(),
        }
    }

    fn active_config_addressing() -> ExternalAddressing {
        ExternalAddressing {
            scheme: SUBJECT_SCHEME.to_string(),
            value: SUBJECT_VALUE_ACTIVE_CONFIG.to_string(),
        }
    }

    fn pending_reboot_addressing() -> ExternalAddressing {
        ExternalAddressing {
            scheme: SUBJECT_SCHEME.to_string(),
            value: SUBJECT_VALUE_PENDING_REBOOT.to_string(),
        }
    }

    fn capabilities_state(&self) -> serde_json::Value {
        serde_json::json!({
            "v": PAYLOAD_VERSION,
            "profile": self.profile,
            "catalogue": self.list_catalogue(),
        })
    }

    async fn pending_reboot_state(&self) -> serde_json::Value {
        let pending = self.pending_reboot.read().await.clone();
        serde_json::to_value(pending).unwrap_or(serde_json::Value::Null)
    }

    async fn announce_subjects(&self) {
        let Some(announcer) = self.subject_announcer.as_ref() else {
            return;
        };
        let now = std::time::SystemTime::now();
        let announces = [
            (
                SUBJECT_TYPE_CAPABILITIES,
                Self::capabilities_addressing(),
                self.capabilities_state(),
            ),
            (
                SUBJECT_TYPE_ACTIVE_CONFIG,
                Self::active_config_addressing(),
                self.read_and_enrich_active_config_value().await,
            ),
            (
                SUBJECT_TYPE_PENDING_REBOOT,
                Self::pending_reboot_addressing(),
                self.pending_reboot_state().await,
            ),
        ];
        for (subject_type, addressing, state) in announces {
            let announcement = SubjectAnnouncement {
                subject_type: subject_type.to_string(),
                addressings: vec![addressing],
                claims: Vec::new(),
                state,
                announced_at: now,
            };
            if let Err(e) = announcer.announce(announcement).await {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    subject_type = subject_type,
                    error = %e,
                    "announce subject failed"
                );
            }
        }
    }

    async fn read_and_enrich_active_config_value(&self) -> serde_json::Value {
        let active = match self.provider.current_config().await {
            Ok(a) => self.enrich_active_config(a),
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "provider current_config read failed; announcing unset"
                );
                ActiveConfig::unset(String::new())
            }
        };
        serde_json::json!({
            "v": PAYLOAD_VERSION,
            "active": active,
        })
    }

    async fn republish_active_config(&self) {
        let Some(announcer) = self.subject_announcer.as_ref() else {
            return;
        };
        let state = self.read_and_enrich_active_config_value().await;
        if let Err(e) = announcer
            .update_state(Self::active_config_addressing(), state)
            .await
        {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "update active_config subject state failed"
            );
        }
    }

    async fn republish_pending_reboot(&self) {
        let Some(announcer) = self.subject_announcer.as_ref() else {
            return;
        };
        let state = self.pending_reboot_state().await;
        if let Err(e) = announcer
            .update_state(Self::pending_reboot_addressing(), state)
            .await
        {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "update pending_reboot subject state failed"
            );
        }
    }

    async fn flip_pending_reboot(&self, cause: &str) {
        let mut state = self.pending_reboot.write().await;
        state.pending = true;
        state.cause = cause.to_string();
        state.set_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
    }

    async fn emit_happening(
        &self,
        event_type: &str,
        payload: serde_json::Value,
    ) {
        let Some(emitter) = self.happening_emitter.as_ref() else {
            return;
        };
        if let Err(e) = emitter
            .emit_plugin_event(event_type.to_string(), payload)
            .await
        {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                event_type = event_type,
                error = %e,
                "emit happening failed"
            );
        }
    }
}

impl Default for HardwareAudioConfigPlugin {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================
// Plugin trait
// =============================================================

impl Plugin for HardwareAudioConfigPlugin {
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
                state_dir = %ctx.state_dir.display(),
                "plugin load beginning"
            );
            self.catalogue = match parse_dacs(EMBEDDED_DACS_JSON) {
                Ok(c) => Some(c),
                Err(e) => {
                    return Err(PluginError::Permanent(format!(
                        "embedded dacs.json parse error: {e}"
                    )));
                }
            };
            if !self.profile_pinned {
                self.profile = provider_pi::resolve_board_profile().await;
            }
            // Provider selection: tests pre-set provider via
            // with_provider; honour the override. Otherwise the Pi
            // profile gets the PiProvider; every other class
            // retains the NoopProvider that surfaces an empty
            // catalogue + NotApplicable on writes.
            if !self.provider_injected && self.profile == "Raspberry PI" {
                self.provider = Arc::new(PiProvider::new());
            }
            self.happening_emitter = Some(Arc::clone(&ctx.happening_emitter));
            self.subject_announcer = Some(Arc::clone(&ctx.subject_announcer));
            self.announce_subjects().await;
            self.loaded = true;
            tracing::info!(
                plugin = PLUGIN_NAME,
                profile = %self.profile,
                catalogue_entries = self.list_catalogue().len(),
                "plugin loaded; hardware audio capabilities ready"
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
            self.happening_emitter = None;
            self.subject_announcer = None;
            self.loaded = false;
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            if self.loaded {
                HealthReport::healthy()
            } else {
                HealthReport::unhealthy(
                    "hardware.audio-config plugin not loaded",
                )
            }
        }
    }
}

// =============================================================
// Respondent
// =============================================================

impl Respondent for HardwareAudioConfigPlugin {
    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "hardware.audio-config plugin not loaded".to_string(),
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
                "hardware.audio.list_dac_catalogue" => {
                    self.handle_list_dac_catalogue(req).await
                }
                "hardware.audio.select_dac" => {
                    self.handle_select_dac(req).await
                }
                "hardware.audio.clear_dac" => self.handle_clear_dac(req).await,
                "hardware.audio.current_config" => {
                    self.handle_current_config(req).await
                }
                "hardware.audio.confirm_reboot_required" => {
                    self.handle_confirm_reboot_required(req).await
                }
                other => Err(PluginError::Permanent(format!(
                    "request type {other:?} declared but no handler wired"
                ))),
            }
        }
    }
}

// =============================================================
// Handlers
// =============================================================

impl HardwareAudioConfigPlugin {
    async fn handle_list_dac_catalogue(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        parse_versioned::<EmptyPayload>(req)?;
        let catalogue = self.list_catalogue();
        encode(
            req,
            &serde_json::json!({
                "v": PAYLOAD_VERSION,
                "profile": self.profile,
                "catalogue": catalogue,
            }),
        )
    }

    async fn handle_select_dac(
        &mut self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: SelectDacPayload = parse_versioned(req)?;
        let catalogue = self.catalogue.as_ref().ok_or_else(|| {
            PluginError::Permanent("dac catalogue not loaded".to_string())
        })?;
        let entry = find_dac(catalogue, &self.profile, &payload.id)
            .ok_or_else(|| {
                PluginError::Permanent(format!(
                    "unknown dac id {:?} for profile {:?}",
                    payload.id, self.profile
                ))
            })?
            .clone();
        let outcome = self.provider.apply(&entry).await.map_err(|e| {
            PluginError::Permanent(format!("provider apply failed: {e}"))
        })?;
        let cause = format!("select_dac {} ({})", entry.id, outcome.overlay);
        self.flip_pending_reboot(&cause).await;
        self.republish_active_config().await;
        self.republish_pending_reboot().await;
        self.emit_happening(
            SELECTED_EVENT,
            serde_json::json!({
                "v": PAYLOAD_VERSION,
                "dac_id": entry.id,
                "outcome": outcome,
            }),
        )
        .await;
        if outcome.reboot_required {
            self.emit_happening(
                REBOOT_REQUIRED_EVENT,
                serde_json::json!({
                    "v": PAYLOAD_VERSION,
                    "cause": cause,
                    "outcome": outcome,
                }),
            )
            .await;
        }
        encode(
            req,
            &serde_json::json!({
                "v": PAYLOAD_VERSION,
                "status": "ok",
                "outcome": outcome,
            }),
        )
    }

    async fn handle_clear_dac(
        &mut self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        parse_versioned::<EmptyPayload>(req)?;
        let outcome: ApplyOutcome =
            self.provider.clear().await.map_err(|e| {
                PluginError::Permanent(format!("provider clear failed: {e}"))
            })?;
        if outcome.reboot_required {
            self.flip_pending_reboot("clear_dac").await;
        }
        self.republish_active_config().await;
        self.republish_pending_reboot().await;
        self.emit_happening(
            CLEARED_EVENT,
            serde_json::json!({
                "v": PAYLOAD_VERSION,
                "outcome": outcome,
            }),
        )
        .await;
        if outcome.reboot_required {
            self.emit_happening(
                REBOOT_REQUIRED_EVENT,
                serde_json::json!({
                    "v": PAYLOAD_VERSION,
                    "cause": "clear_dac",
                    "outcome": outcome,
                }),
            )
            .await;
        }
        encode(
            req,
            &serde_json::json!({
                "v": PAYLOAD_VERSION,
                "status": "ok",
                "outcome": outcome,
            }),
        )
    }

    async fn handle_current_config(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        parse_versioned::<EmptyPayload>(req)?;
        let active = self.provider.current_config().await.map_err(|e| {
            PluginError::Permanent(format!(
                "provider current_config failed: {e}"
            ))
        })?;
        let enriched = self.enrich_active_config(active);
        encode(
            req,
            &serde_json::json!({
                "v": PAYLOAD_VERSION,
                "active": enriched,
            }),
        )
    }

    async fn handle_confirm_reboot_required(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        parse_versioned::<EmptyPayload>(req)?;
        let state = self.pending_reboot.read().await.clone();
        encode(
            req,
            &serde_json::json!({
                "v": PAYLOAD_VERSION,
                "pending_reboot": state,
            }),
        )
    }
}

// =============================================================
// Wire payload + helper plumbing
// =============================================================

trait HasPayloadVersion {
    fn payload_version(&self) -> u32;
}

fn parse_versioned<T>(req: &Request) -> Result<T, PluginError>
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
            "{:?} response encode failed: {e}",
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
struct SelectDacPayload {
    #[serde(default = "default_payload_version")]
    v: u32,
    /// Catalogue id (e.g. `hifiberry-dacplus`).
    id: String,
}

impl HasPayloadVersion for SelectDacPayload {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

// =============================================================
// Tests
// =============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_request_types_match_runtime() {
        let m = manifest();
        let manifest_types: Vec<&str> = m
            .capabilities
            .respondent
            .as_ref()
            .expect("respondent declared")
            .request_types
            .iter()
            .map(String::as_str)
            .collect();
        for declared in REQUEST_TYPES {
            assert!(
                manifest_types.contains(declared),
                "REQUEST_TYPES {declared:?} missing from manifest {manifest_types:?}"
            );
        }
        for ty in &manifest_types {
            assert!(
                REQUEST_TYPES.contains(ty),
                "manifest type {ty:?} missing from REQUEST_TYPES {REQUEST_TYPES:?}"
            );
        }
    }

    #[test]
    fn embedded_manifest_parses() {
        let _ = manifest();
    }

    #[test]
    fn embedded_catalogue_parses() {
        let parsed =
            parse_dacs(EMBEDDED_DACS_JSON).expect("embedded dacs.json parses");
        assert!(parsed
            .devices
            .iter()
            .any(|d| d.name == "Raspberry PI" && !d.data.is_empty()));
    }

    #[test]
    fn list_catalogue_filters_by_profile() {
        let plugin =
            HardwareAudioConfigPlugin::new().with_profile("Raspberry PI");
        // Pre-load the catalogue without calling load() (which
        // would also touch the provider + announcer).
        let mut p = plugin;
        p.catalogue = parse_dacs(EMBEDDED_DACS_JSON).ok();
        let list = p.list_catalogue();
        assert!(!list.is_empty(), "Pi profile non-empty");
        assert!(list.iter().any(|e| e.id == "hifiberry-dacplus"));

        let mut other =
            HardwareAudioConfigPlugin::new().with_profile("Unknown");
        other.catalogue = parse_dacs(EMBEDDED_DACS_JSON).ok();
        assert!(other.list_catalogue().is_empty());
    }

    #[test]
    fn enrich_active_config_resolves_catalogue_hints() {
        // `allo-katana-dac-audio` is a unique overlay in the
        // catalogue (one row; no aliasing). Multiple catalogue
        // rows can share the same dtoverlay token (e.g. various
        // HiFiBerry boards all use `hifiberry-dacplus`); resolving
        // catalogue id from overlay alone is best-effort and
        // returns the first matching row.
        let mut p =
            HardwareAudioConfigPlugin::new().with_profile("Raspberry PI");
        p.catalogue = parse_dacs(EMBEDDED_DACS_JSON).ok();
        let cfg = ActiveConfig {
            overlay: "allo-katana-dac-audio".into(),
            catalogue_id: None,
            alsacard_hint: None,
            mixer_hint: None,
            boot_config_path: "/boot/firmware/config.txt".into(),
        };
        let enriched = p.enrich_active_config(cfg);
        assert_eq!(enriched.catalogue_id.as_deref(), Some("allo-katana-dac"));
        assert_eq!(enriched.alsacard_hint.as_deref(), Some("Katana"));
        assert_eq!(enriched.mixer_hint.as_deref(), Some("Master"));
    }

    #[test]
    fn enrich_active_config_leaves_unmatched_overlay_unenriched() {
        let mut p =
            HardwareAudioConfigPlugin::new().with_profile("Raspberry PI");
        p.catalogue = parse_dacs(EMBEDDED_DACS_JSON).ok();
        let cfg = ActiveConfig {
            overlay: "not-in-catalogue".into(),
            catalogue_id: None,
            alsacard_hint: None,
            mixer_hint: None,
            boot_config_path: "/boot/firmware/config.txt".into(),
        };
        let enriched = p.enrich_active_config(cfg.clone());
        assert!(enriched.catalogue_id.is_none());
        assert!(enriched.alsacard_hint.is_none());
        assert!(enriched.mixer_hint.is_none());
        assert_eq!(enriched.overlay, cfg.overlay);
    }

    #[test]
    fn enrich_active_config_unset_passes_through() {
        let mut p =
            HardwareAudioConfigPlugin::new().with_profile("Raspberry PI");
        p.catalogue = parse_dacs(EMBEDDED_DACS_JSON).ok();
        let unset = ActiveConfig::unset("/boot/config.txt");
        let enriched = p.enrich_active_config(unset.clone());
        assert_eq!(enriched, unset);
    }

    #[tokio::test]
    async fn flip_pending_reboot_records_cause_and_timestamp() {
        let p = HardwareAudioConfigPlugin::new();
        p.flip_pending_reboot("select_dac hifiberry-dacplus").await;
        let state = p.pending_reboot.read().await;
        assert!(state.pending);
        assert_eq!(state.cause, "select_dac hifiberry-dacplus");
        assert!(state.set_at_ms > 0);
    }
}
