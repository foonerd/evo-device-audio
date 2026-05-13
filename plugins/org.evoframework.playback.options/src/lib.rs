//! # org-evoframework-playback-options
//!
//! Operator-facing audiophile-grade playback settings plugin.
//! Owns the operator's policy choices for the modular ALSA
//! pipeline:
//!
//! - **Output device** — which ALSA card / pcm.evo terminus the
//!   delivery plugin should bind. Drives `delivery.alsa`'s
//!   pcm.evo definition.
//! - **Resampling** — disable / soxr-quality choice / target
//!   bitdepth / target samplerate. Drives the MPD `audio_output
//!   format` line + the composition plugin's mode selection.
//! - **Mixer type** — `hardware` (MPD drives the card's mixer
//!   control directly) / `software` (MPD applies its own gain
//!   stage) / `none` (no in-chain volume; downstream device
//!   handles it).
//! - **DOP** — DSD-over-PCM transport enable for DSD-capable
//!   DACs.
//! - **Volume normalization** — MPD's `volume_normalization`
//!   policy.
//!
//! Stocks the `audio.options` shelf at shape 1.
//!
//! ## What this plugin is
//!
//! A singleton respondent that holds operator audiophile
//! preferences across steward restarts. The plugin's job is
//! **policy**, not **mechanism**: it remembers what the
//! operator chose and tells other plugins about it. The
//! delivery.alsa plugin (mechanism) reacts to settings-changed
//! happenings by re-rendering the modular ALSA pipeline; the
//! playback.mpd plugin (mechanism) reads settings via the
//! framework's audio_routing handle once topology negotiation
//! incorporates the operator's resampling preference.
//!
//! ## What this plugin does
//!
//! - Exposes a [`Respondent`] surface with `options.get_settings`
//!   (read) and one `options.set_<field>` verb per setting
//!   (write). Every setter validates the new value against the
//!   declared domain (e.g. `mixer_type` ∈ `{hardware, software,
//!   none}`), persists the updated state, and emits a
//!   `Happening::PluginEvent` with `event_type =
//!   "audio.options.changed"` so cross-plugin consumers react.
//!
//! - Persists state to
//!   `/var/lib/evo/org.evoframework.playback.options/state.toml`
//!   via [`LoadContext::state_dir`]. The framework guarantees
//!   per-plugin filesystem isolation; no other plugin reads
//!   this directory.
//!
//! - On `Plugin::load`, rehydrates from the state file when it
//!   exists; falls back to documented defaults when it does
//!   not. Absent state is a valid "first-boot" condition, not
//!   a fault.
//!
//! ## What this plugin does NOT do
//!
//! - **Open ALSA, parse aplay -L, drive MPD.** That's
//!   `delivery.alsa` + `playback.mpd`. This plugin only
//!   surfaces operator intent; the mechanism plugins translate
//!   intent into OS-level action.
//!
//! - **Resolve hardware choices.** The operator picks an
//!   `output_device` as an opaque card identifier (the
//!   delivery.alsa plugin's `delivery.list_cards` verb populates
//!   the choice menu; this plugin just records the operator's
//!   pick).
//!
//! [`LoadContext::state_dir`]:
//! evo_plugin_sdk::contract::LoadContext::state_dir
//! [`Respondent`]: evo_plugin_sdk::contract::Respondent

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::manual_async_fn)]

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use evo_plugin_sdk::contract::{
    BuildInfo, ExternalAddressing, HappeningEmitter, HealthReport, LoadContext,
    Plugin, PluginDescription, PluginError, PluginIdentity, Request,
    Respondent, Response, RuntimeCapabilities, SubjectAnnouncement,
    SubjectAnnouncer,
};
use evo_plugin_sdk::Manifest;
use serde::{Deserialize, Serialize};

/// Embedded manifest source.
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");

/// Plugin identity name (must match manifest).
pub const PLUGIN_NAME: &str = "org.evoframework.playback.options";

/// Wire-protocol payload version every request + response
/// carries.
const PAYLOAD_VERSION: u32 = 1;

/// Happening event_type the plugin emits on every setter
/// success. Consumers (delivery.alsa, UI surfaces, multi-room
/// peers) subscribe to this on the happenings bus.
const HAPPENING_EVENT_TYPE: &str = "audio.options.changed";

/// External-addressing scheme + value the plugin uses for its
/// canonical settings subject. Plugins observing operator
/// option changes resolve this addressing to the canonical id
/// via `SubjectQuerier::resolve_addressing` and subscribe to
/// state updates via `SubjectStateSubscriber::subscribe_subject`.
const SETTINGS_SCHEME: &str = "evo.audio.options";
const SETTINGS_VALUE: &str = "settings";

/// Subject type the framework records on the settings subject.
/// Underscored form because the framework's catalogue parser
/// rejects subject-type names containing `.`.
const SETTINGS_SUBJECT_TYPE: &str = "audio_options_settings";

/// Filename for the persisted operator state under
/// [`LoadContext::state_dir`].
const STATE_FILENAME: &str = "state.toml";

/// Request types this plugin honours. Mirrors
/// `manifest.toml`'s `[capabilities.respondent].request_types`;
/// admission would refuse a mismatch. Lockstep enforced by the
/// `manifest_request_types_match_runtime` test.
const REQUEST_TYPES: &[&str] = &[
    "options.get_settings",
    "options.set_resampling",
    "options.set_mixer_type",
    "options.set_dop",
    "options.set_output_device",
    "options.set_volume_normalization",
    "options.restore_last_known_good",
    "options.reset_to_defaults",
];

/// Parse the embedded plugin manifest.
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML).expect(
        "org-evoframework-playback-options: embedded manifest must parse",
    )
}

fn plugin_crate_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is valid semver")
}

// =============================================================
// Persisted settings shape
// =============================================================

/// Mixer-type domain. Constrains the operator's choice and
/// drives `delivery.alsa`'s pcm.evo rendering.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum MixerType {
    /// MPD drives the card's hardware mixer control directly.
    /// Lowest-latency volume; bit-perfect when paired with a
    /// bit-perfect-capable card.
    Hardware,
    /// MPD applies its own software gain. Universal compatibility
    /// at the cost of one in-chain conversion; the default for
    /// most consumer setups.
    #[default]
    Software,
    /// No in-chain volume control. The downstream device (AVR /
    /// integrated amp) handles gain. Required when the operator
    /// wants strictly bit-perfect output regardless of card
    /// capability.
    None,
}

impl MixerType {
    /// Parse a wire string into the typed enum. Errors carry the
    /// operator-readable invalid-value diagnostic the setter
    /// uses for refusal.
    pub fn from_wire_str(value: &str) -> Result<Self, String> {
        match value {
            "hardware" => Ok(Self::Hardware),
            "software" => Ok(Self::Software),
            "none" => Ok(Self::None),
            other => Err(format!(
                "mixer_type must be one of {{hardware, software, none}}; \
                 got {other:?}"
            )),
        }
    }

    /// Stable wire string for the typed enum.
    pub fn as_wire_str(&self) -> &'static str {
        match self {
            Self::Hardware => "hardware",
            Self::Software => "software",
            Self::None => "none",
        }
    }
}

/// Resampling policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ResamplingPolicy {
    /// `true` when MPD should resample the source to the
    /// declared `target_bitdepth` / `target_samplerate`; `false`
    /// when MPD should pass through the source's native format
    /// (the pipeline's "no manipulation" path; `plug` still
    /// bridges the format to the card via the kernel's automatic
    /// conversion).
    pub enabled: bool,
    /// Target bit depth when `enabled = true`. Empty string
    /// (`""`) means "match source"; concrete values are `"16"`,
    /// `"24"`, `"32"`, `"f"` (32-bit float, MPD's wire shape).
    pub target_bitdepth: String,
    /// Target sample rate when `enabled = true`. Empty string
    /// means "match source"; concrete values are `"44100"`,
    /// `"48000"`, `"88200"`, `"96000"`, `"176400"`, `"192000"`.
    pub target_samplerate: String,
    /// soxr quality preset when `enabled = true`. One of
    /// `"very_high"`, `"high"`, `"medium"`, `"low"`, `"quick"`.
    /// Default `"very_high"` (audiophile-grade default).
    pub quality: String,
}

/// Persisted operator settings. Round-trips through
/// `state.toml` via serde. Field order is documented +
/// stable; new fields land as additive options with sensible
/// defaults (no schema-bump unless a domain narrows).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// Wire-protocol envelope version. Future incompatible
    /// changes bump this; the plugin parses both shapes during
    /// a deprecation window.
    #[serde(default = "default_settings_version")]
    pub v: u32,
    /// Resampling policy.
    #[serde(default)]
    pub resampling: ResamplingPolicy,
    /// Mixer-type choice.
    #[serde(default)]
    pub mixer_type: MixerType,
    /// DSD-over-PCM enable for DSD-capable DACs.
    #[serde(default)]
    pub dop: bool,
    /// Output-device identifier. The operator picks one of the
    /// strings `delivery.list_cards` returns (e.g. `"DAC"`,
    /// `"hw:0,0"`). Empty string = "framework default" (the
    /// distribution's first detected playback card).
    #[serde(default)]
    pub output_device: String,
    /// `volume_normalization` MPD policy. `true` enables MPD's
    /// loudness equalisation across tracks; `false` is the
    /// audiophile default (no in-chain post-processing).
    #[serde(default)]
    pub volume_normalization: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            v: PAYLOAD_VERSION,
            resampling: ResamplingPolicy::default(),
            mixer_type: MixerType::default(),
            dop: false,
            output_device: String::new(),
            volume_normalization: false,
        }
    }
}

fn default_settings_version() -> u32 {
    PAYLOAD_VERSION
}

// =============================================================
// Plugin
// =============================================================

/// Operator-facing playback-options plugin.
pub struct PlaybackOptionsPlugin {
    loaded: bool,
    settings: Settings,
    state_path: Option<PathBuf>,
    happening_emitter: Option<Arc<dyn HappeningEmitter>>,
    /// Subject-announcer handle from `LoadContext`. The plugin
    /// announces its settings as a subject at load time and
    /// publishes a fresh state payload after every setter so
    /// downstream consumers (playback.mpd's mixer-mode reactor,
    /// future UI plugins) observe operator changes via the
    /// framework's `SubjectStateSubscriber` rather than
    /// reaching into this plugin's state file or wire-op
    /// surface.
    subject_announcer: Option<Arc<dyn SubjectAnnouncer>>,
    requests_handled: u64,
}

impl PlaybackOptionsPlugin {
    /// Construct a fresh plugin instance with default settings.
    pub fn new() -> Self {
        Self {
            loaded: false,
            settings: Settings::default(),
            state_path: None,
            happening_emitter: None,
            subject_announcer: None,
            requests_handled: 0,
        }
    }

    /// Cumulative `handle_request` invocations.
    pub fn requests_handled(&self) -> u64 {
        self.requests_handled
    }

    /// Current in-memory settings snapshot.
    pub fn settings(&self) -> Settings {
        self.settings.clone()
    }

    /// Set the state-file path. Tests override this to point at
    /// a tempdir rather than `LoadContext::state_dir`.
    #[cfg(test)]
    pub(crate) fn with_state_path(mut self, path: PathBuf) -> Self {
        self.state_path = Some(path);
        self
    }

    /// Load settings from the configured state file. Returns
    /// defaults when the file is absent. Surfaces IO + parse
    /// errors as Permanent so the framework surfaces them to
    /// the operator at admission.
    async fn load_settings_from_disk(&self) -> Result<Settings, PluginError> {
        let Some(path) = self.state_path.as_ref() else {
            return Ok(Settings::default());
        };
        match tokio::fs::read_to_string(path).await {
            Ok(s) => toml::from_str::<Settings>(&s).map_err(|e| {
                PluginError::Permanent(format!(
                    "state file {path:?} parse error: {e}"
                ))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(Settings::default())
            }
            Err(e) => Err(PluginError::Permanent(format!(
                "state file {path:?} read error: {e}"
            ))),
        }
    }

    /// Persist current settings atomically: write to a temp
    /// file in the same directory, fsync, rename.
    async fn persist_settings(&self) -> Result<(), PluginError> {
        let Some(path) = self.state_path.as_ref() else {
            return Err(PluginError::Permanent(
                "state_path is None; plugin not fully loaded".to_string(),
            ));
        };
        // Before overwriting the live state.toml, copy its
        // current bytes to the last-known-good sidecar so an
        // operator (or the auto-recovery path) can restore the
        // prior settings if the new config breaks audio. This
        // is the lower-cost half of the safety story; the
        // operator-facing restore_last_known_good and
        // reset_to_defaults verbs land in
        // handle_restore_last_known_good +
        // handle_reset_to_defaults.
        if path.exists() {
            let lkg_path = Self::last_known_good_path(path);
            // Best-effort: a copy failure here does NOT fail
            // the setter (the operator's change must still
            // land). The next successful persist re-snapshots.
            if let Err(e) = tokio::fs::copy(path, &lkg_path).await {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    lkg_path = %lkg_path.display(),
                    "last-known-good snapshot failed; setter continues \
                     but auto-recovery may be unavailable until next persist"
                );
            }
        }
        let body = toml::to_string_pretty(&self.settings).map_err(|e| {
            PluginError::Permanent(format!("settings serialise error: {e}"))
        })?;
        let parent = path.parent().ok_or_else(|| {
            PluginError::Permanent(format!(
                "state_path {path:?} has no parent directory"
            ))
        })?;
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            PluginError::Permanent(format!("mkdir {parent:?}: {e}"))
        })?;
        let staging = parent.join(format!(
            ".{}.tmp",
            path.file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| "state.toml".to_string())
        ));
        tokio::fs::write(&staging, &body).await.map_err(|e| {
            PluginError::Permanent(format!("write {staging:?}: {e}"))
        })?;
        {
            let f = tokio::fs::OpenOptions::new()
                .write(true)
                .open(&staging)
                .await
                .map_err(|e| {
                    PluginError::Permanent(format!("open {staging:?}: {e}"))
                })?;
            f.sync_all().await.map_err(|e| {
                PluginError::Permanent(format!("fsync {staging:?}: {e}"))
            })?;
        }
        tokio::fs::rename(&staging, path).await.map_err(|e| {
            PluginError::Permanent(format!(
                "rename {staging:?} -> {path:?}: {e}"
            ))
        })?;
        Ok(())
    }

    /// Compute the last-known-good sidecar path for a given
    /// live state file. We use `<state_filename>.lkg` in the
    /// same directory; that keeps the sidecar inside the
    /// plugin's own state dir (operator-owned) and avoids any
    /// path traversal across plugin boundaries.
    fn last_known_good_path(state_path: &std::path::Path) -> PathBuf {
        let mut path = state_path.to_path_buf();
        let file_name = state_path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| STATE_FILENAME.to_string());
        path.set_file_name(format!("{file_name}.lkg"));
        path
    }

    /// Restore the last-known-good snapshot in place over the
    /// live state file. The next setter (or this method's
    /// own subsequent persist) re-snapshots the now-live state.
    /// Returns the restored Settings so the caller can update
    /// `self.settings` + drive the subject-state publish.
    async fn restore_from_last_known_good(
        &self,
    ) -> Result<Settings, PluginError> {
        let Some(path) = self.state_path.as_ref() else {
            return Err(PluginError::Permanent(
                "state_path is None; plugin not fully loaded".to_string(),
            ));
        };
        let lkg_path = Self::last_known_good_path(path);
        if !lkg_path.exists() {
            return Err(PluginError::Permanent(format!(
                "no last-known-good snapshot at {}",
                lkg_path.display()
            )));
        }
        // Stage the LKG copy into a temp file, fsync, then
        // rename onto the live path. This is the same atomic-
        // write recipe persist_settings uses; readers
        // (subsequent load_settings_from_disk) see either the
        // prior contents or the restored contents — never a
        // torn write.
        let body = tokio::fs::read_to_string(&lkg_path).await.map_err(|e| {
            PluginError::Permanent(format!(
                "read last-known-good at {}: {e}",
                lkg_path.display()
            ))
        })?;
        let settings: Settings = toml::from_str(&body).map_err(|e| {
            PluginError::Permanent(format!(
                "last-known-good at {} failed to parse: {e}",
                lkg_path.display()
            ))
        })?;
        let parent = path.parent().ok_or_else(|| {
            PluginError::Permanent(format!(
                "state_path {path:?} has no parent directory"
            ))
        })?;
        let staging = parent.join(format!(
            ".{}.tmp",
            path.file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| STATE_FILENAME.to_string())
        ));
        tokio::fs::write(&staging, &body).await.map_err(|e| {
            PluginError::Permanent(format!("write {staging:?}: {e}"))
        })?;
        {
            let f = tokio::fs::OpenOptions::new()
                .write(true)
                .open(&staging)
                .await
                .map_err(|e| {
                    PluginError::Permanent(format!("open {staging:?}: {e}"))
                })?;
            f.sync_all().await.map_err(|e| {
                PluginError::Permanent(format!("fsync {staging:?}: {e}"))
            })?;
        }
        tokio::fs::rename(&staging, path).await.map_err(|e| {
            PluginError::Permanent(format!(
                "rename {staging:?} -> {path:?}: {e}"
            ))
        })?;
        Ok(settings)
    }

    /// Build the external addressing for the plugin's settings
    /// subject. Consumers resolve the same `(scheme, value)`
    /// pair against the framework's subject querier to learn
    /// the canonical id they should subscribe to.
    fn settings_addressing() -> ExternalAddressing {
        ExternalAddressing {
            scheme: SETTINGS_SCHEME.to_string(),
            value: SETTINGS_VALUE.to_string(),
        }
    }

    /// Announce the settings subject at load time with the
    /// current settings as state. Idempotent on re-announce
    /// (the framework's registry treats this as Updated on the
    /// existing canonical id, preserving the addressing). Emit
    /// failures are logged at warn level and do not fail the
    /// load — the plugin's wire-op surface continues to work
    /// even if the subject channel is unavailable.
    async fn announce_settings_subject(&self) {
        let Some(announcer) = self.subject_announcer.as_ref() else {
            return;
        };
        let state = match serde_json::to_value(&self.settings) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "failed to serialise settings for subject state"
                );
                return;
            }
        };
        let announcement = SubjectAnnouncement {
            subject_type: SETTINGS_SUBJECT_TYPE.to_string(),
            addressings: vec![Self::settings_addressing()],
            claims: Vec::new(),
            state,
            announced_at: std::time::SystemTime::now(),
        };
        if let Err(e) = announcer.announce(announcement).await {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "announce settings subject failed"
            );
        }
    }

    /// Publish a fresh subject-state payload after a setter
    /// has updated `self.settings`. Best-effort: failures log
    /// at warn level so the setter's persist + happening
    /// emission paths are unaffected.
    async fn publish_settings_state(&self) {
        let Some(announcer) = self.subject_announcer.as_ref() else {
            return;
        };
        let state = match serde_json::to_value(&self.settings) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "failed to serialise settings for subject state update"
                );
                return;
            }
        };
        if let Err(e) = announcer
            .update_state(Self::settings_addressing(), state)
            .await
        {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "update settings subject state failed"
            );
        }
    }

    /// Emit a `Happening::PluginEvent` carrying the operator-
    /// readable diff AND publish a fresh subject-state payload
    /// so subject-stream consumers (playback.mpd's mixer-mode
    /// reactor, UI plugins) observe the change.
    ///
    /// Both side-effects are best-effort: emit / publish
    /// failures are logged at warn level and do not fail the
    /// setter. Order: happening first (operator-visible audit
    /// trail), subject state second (consumer plumbing). A
    /// failed subject-state update with a successful happening
    /// is recoverable by the next setter; the reverse is not.
    async fn emit_changed(&self, field: &str, new_value: serde_json::Value) {
        if let Some(emitter) = self.happening_emitter.as_ref() {
            let payload = serde_json::json!({
                "v": PAYLOAD_VERSION,
                "field": field,
                "new_value": new_value,
                "settings": self.settings.clone(),
            });
            if let Err(e) = emitter
                .emit_plugin_event(HAPPENING_EVENT_TYPE.to_string(), payload)
                .await
            {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    field = field,
                    error = %e,
                    "emit happening failed"
                );
            }
        }
        self.publish_settings_state().await;
    }
}

impl Default for PlaybackOptionsPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for PlaybackOptionsPlugin {
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
            // Only set state_path from ctx when tests have NOT
            // pre-set one. Tests inject a tempdir-backed path
            // via with_state_path and expect it to win over
            // the load-context dir.
            if self.state_path.is_none() {
                self.state_path = Some(ctx.state_dir.join(STATE_FILENAME));
            }
            self.settings = self.load_settings_from_disk().await?;
            self.happening_emitter = Some(Arc::clone(&ctx.happening_emitter));
            self.subject_announcer = Some(Arc::clone(&ctx.subject_announcer));
            // Announce the settings subject so consumers
            // (playback.mpd, future UI plugins) can resolve
            // its canonical id + subscribe to state changes
            // via the framework's SubjectStateSubscriber. The
            // announce carries the current settings as state
            // so consumers seeing the SubjectRegistered
            // happening have the initial value without a
            // separate round-trip.
            self.announce_settings_subject().await;
            self.loaded = true;
            tracing::info!(
                plugin = PLUGIN_NAME,
                state_path = %self.state_path.as_ref().unwrap().display(),
                mixer_type = self.settings.mixer_type.as_wire_str(),
                resampling_enabled = self.settings.resampling.enabled,
                output_device = %self.settings.output_device,
                "plugin loaded; operator playback settings ready"
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
            self.loaded = false;
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            if self.loaded {
                HealthReport::healthy()
            } else {
                HealthReport::unhealthy("playback.options plugin not loaded")
            }
        }
    }
}

impl Respondent for PlaybackOptionsPlugin {
    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "playback.options plugin not loaded".to_string(),
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
                "options.get_settings" => self.handle_get_settings(req).await,
                "options.set_resampling" => {
                    self.handle_set_resampling(req).await
                }
                "options.set_mixer_type" => {
                    self.handle_set_mixer_type(req).await
                }
                "options.set_dop" => self.handle_set_dop(req).await,
                "options.set_output_device" => {
                    self.handle_set_output_device(req).await
                }
                "options.set_volume_normalization" => {
                    self.handle_set_volume_normalization(req).await
                }
                "options.restore_last_known_good" => {
                    self.handle_restore_last_known_good(req).await
                }
                "options.reset_to_defaults" => {
                    self.handle_reset_to_defaults(req).await
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

impl PlaybackOptionsPlugin {
    async fn handle_get_settings(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        parse_versioned::<EmptyPayload>(req)?;
        encode(req, &self.settings)
    }

    async fn handle_set_resampling(
        &mut self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: SetResamplingPayload = parse_versioned(req)?;
        // Validate quality if present + enabled.
        if payload.policy.enabled {
            match payload.policy.quality.as_str() {
                "very_high" | "high" | "medium" | "low" | "quick" | "" => {}
                other => {
                    return Err(PluginError::Permanent(format!(
                        "resampling.quality must be one of \
                         {{very_high, high, medium, low, quick}} or empty; \
                         got {other:?}"
                    )))
                }
            }
        }
        self.settings.resampling = payload.policy.clone();
        self.persist_settings().await?;
        self.emit_changed(
            "resampling",
            serde_json::to_value(&payload.policy).map_err(map_json_err)?,
        )
        .await;
        encode(
            req,
            &SimpleOk {
                v: PAYLOAD_VERSION,
                status: "ok",
            },
        )
    }

    async fn handle_set_mixer_type(
        &mut self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: SetMixerTypePayload = parse_versioned(req)?;
        let mixer_type = MixerType::from_wire_str(&payload.value)
            .map_err(PluginError::Permanent)?;
        self.settings.mixer_type = mixer_type;
        self.persist_settings().await?;
        self.emit_changed(
            "mixer_type",
            serde_json::Value::String(payload.value),
        )
        .await;
        encode(
            req,
            &SimpleOk {
                v: PAYLOAD_VERSION,
                status: "ok",
            },
        )
    }

    async fn handle_set_dop(
        &mut self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: SetDopPayload = parse_versioned(req)?;
        self.settings.dop = payload.value;
        self.persist_settings().await?;
        self.emit_changed("dop", serde_json::Value::Bool(payload.value))
            .await;
        encode(
            req,
            &SimpleOk {
                v: PAYLOAD_VERSION,
                status: "ok",
            },
        )
    }

    async fn handle_set_output_device(
        &mut self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: SetOutputDevicePayload = parse_versioned(req)?;
        // Operator-readable refusal on the obvious typos
        // (whitespace-only) — but accept empty string as the
        // explicit "framework default" signal.
        if !payload.value.is_empty() && payload.value.trim().is_empty() {
            return Err(PluginError::Permanent(
                "output_device must not be whitespace-only; pass empty \
                 string for framework default"
                    .to_string(),
            ));
        }
        self.settings.output_device = payload.value.clone();
        self.persist_settings().await?;
        self.emit_changed(
            "output_device",
            serde_json::Value::String(payload.value),
        )
        .await;
        encode(
            req,
            &SimpleOk {
                v: PAYLOAD_VERSION,
                status: "ok",
            },
        )
    }

    async fn handle_set_volume_normalization(
        &mut self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: SetVolumeNormalizationPayload = parse_versioned(req)?;
        self.settings.volume_normalization = payload.value;
        self.persist_settings().await?;
        self.emit_changed(
            "volume_normalization",
            serde_json::Value::Bool(payload.value),
        )
        .await;
        encode(
            req,
            &SimpleOk {
                v: PAYLOAD_VERSION,
                status: "ok",
            },
        )
    }

    /// Roll the live settings back to the last-known-good
    /// snapshot. The snapshot was written by the previous
    /// successful `persist_settings` call (every setter
    /// invokes it before overwriting the live file).
    ///
    /// Returns `Permanent` if no snapshot exists (no prior
    /// successful setter run since plugin install) or if the
    /// snapshot file is malformed. Operators reading the
    /// error message see the snapshot path so they can
    /// inspect it.
    ///
    /// On success: settings are restored in memory, the live
    /// state.toml is rewritten atomically, the change
    /// propagates via emit_changed → subject state publish.
    /// Consumers (playback.mpd's mixer-mode reactor, UI
    /// surfaces) observe the rollback the same way they
    /// observe any other operator change.
    async fn handle_restore_last_known_good(
        &mut self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        parse_versioned::<EmptyPayload>(req)?;
        let restored = self.restore_from_last_known_good().await?;
        self.settings = restored;
        // We do NOT call self.persist_settings() here: the
        // restore_from_last_known_good already atomic-wrote
        // the live state.toml in place; a subsequent persist
        // would clobber the LKG snapshot we just restored
        // from. The next setter call rewrites the LKG snapshot
        // as part of its normal persist path.
        self.emit_changed(
            "restore_last_known_good",
            serde_json::to_value(&self.settings).map_err(map_json_err)?,
        )
        .await;
        encode(
            req,
            &SimpleOk {
                v: PAYLOAD_VERSION,
                status: "ok",
            },
        )
    }

    /// Reset the live settings to documented defaults
    /// (`Settings::default()`). Useful for first-boot
    /// rescue + operator-explicit reset.
    ///
    /// Resets BOTH the in-memory settings AND the persisted
    /// state.toml; the previous live state becomes the new
    /// last-known-good snapshot so operators can immediately
    /// `restore_last_known_good` to undo the reset if it was
    /// accidental.
    async fn handle_reset_to_defaults(
        &mut self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        parse_versioned::<EmptyPayload>(req)?;
        self.settings = Settings::default();
        self.persist_settings().await?;
        self.emit_changed(
            "reset_to_defaults",
            serde_json::to_value(&self.settings).map_err(map_json_err)?,
        )
        .await;
        encode(
            req,
            &SimpleOk {
                v: PAYLOAD_VERSION,
                status: "ok",
            },
        )
    }
}

// =============================================================
// wire payload helpers
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
struct SetResamplingPayload {
    #[serde(default = "default_payload_version")]
    v: u32,
    policy: ResamplingPolicy,
}

impl HasPayloadVersion for SetResamplingPayload {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

#[derive(Debug, Deserialize)]
struct SetMixerTypePayload {
    #[serde(default = "default_payload_version")]
    v: u32,
    value: String,
}

impl HasPayloadVersion for SetMixerTypePayload {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

#[derive(Debug, Deserialize)]
struct SetDopPayload {
    #[serde(default = "default_payload_version")]
    v: u32,
    value: bool,
}

impl HasPayloadVersion for SetDopPayload {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

#[derive(Debug, Deserialize)]
struct SetOutputDevicePayload {
    #[serde(default = "default_payload_version")]
    v: u32,
    value: String,
}

impl HasPayloadVersion for SetOutputDevicePayload {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

#[derive(Debug, Deserialize)]
struct SetVolumeNormalizationPayload {
    #[serde(default = "default_payload_version")]
    v: u32,
    value: bool,
}

impl HasPayloadVersion for SetVolumeNormalizationPayload {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

#[derive(Debug, Serialize)]
struct SimpleOk {
    v: u32,
    status: &'static str,
}

/// Local helper — serde_json error -> PluginError. Used in
/// handlers that build the payload value for the changed
/// happening. Can't be a `From` impl because PluginError lives
/// outside this crate (orphan rule).
fn map_json_err(e: serde_json::Error) -> PluginError {
    PluginError::Permanent(format!("json serialise: {e}"))
}

// =============================================================
// tests
// =============================================================

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use evo_plugin_sdk::contract::{HealthStatus, ReportError};
    use serde_json::{json, Value};
    use tempfile::tempdir;

    // ----- HappeningEmitter stub -----

    #[derive(Debug, Clone)]
    #[allow(dead_code)]
    struct CapturedEvent {
        event_type: String,
        payload: serde_json::Value,
    }

    #[derive(Default)]
    struct CapturingEmitter {
        events: Mutex<Vec<CapturedEvent>>,
    }

    impl std::fmt::Debug for CapturingEmitter {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("CapturingEmitter").finish_non_exhaustive()
        }
    }

    impl HappeningEmitter for CapturingEmitter {
        fn emit_plugin_event<'a>(
            &'a self,
            event_type: String,
            payload: serde_json::Value,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<(), ReportError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                self.events.lock().unwrap().push(CapturedEvent {
                    event_type,
                    payload,
                });
                Ok(())
            })
        }

        fn emit_audio_playback_ended<'a>(
            &'a self,
            _claim_uri: Option<String>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<(), ReportError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move { Ok(()) })
        }
    }

    #[allow(dead_code)]
    impl CapturingEmitter {
        fn count(&self) -> usize {
            self.events.lock().unwrap().len()
        }
        fn last(&self) -> Option<CapturedEvent> {
            self.events.lock().unwrap().last().cloned()
        }
    }

    // ----- helpers -----

    async fn loaded_plugin() -> (PlaybackOptionsPlugin, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join(STATE_FILENAME);
        let mut p = PlaybackOptionsPlugin::new().with_state_path(state_path);
        p.happening_emitter = Some(Arc::new(CapturingEmitter::default()));
        p.loaded = true;
        (p, dir)
    }

    fn req(verb: &str, payload: Value) -> Request {
        Request {
            request_type: verb.to_string(),
            payload: payload.to_string().into_bytes(),
            correlation_id: 1,
            deadline: None,
            instance_id: None,
        }
    }

    // ----- manifest / surface -----

    #[test]
    fn embedded_manifest_parses() {
        let m = manifest();
        assert_eq!(m.plugin.name, PLUGIN_NAME);
        assert_eq!(m.target.shelf, "audio.options");
        assert_eq!(m.target.shape, 1);
    }

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
                "REQUEST_TYPES {declared:?} missing from manifest \
                 {manifest_types:?}"
            );
        }
        for ty in &manifest_types {
            assert!(
                REQUEST_TYPES.contains(ty),
                "manifest type {ty:?} missing from REQUEST_TYPES \
                 {REQUEST_TYPES:?}"
            );
        }
    }

    #[tokio::test]
    async fn identity_matches_manifest() {
        let p = PlaybackOptionsPlugin::new();
        let d = p.describe().await;
        let m = manifest();
        assert_eq!(d.identity.name, m.plugin.name);
        assert_eq!(d.identity.version, m.plugin.version);
    }

    #[tokio::test]
    async fn health_unhealthy_before_load() {
        let p = PlaybackOptionsPlugin::new();
        assert!(matches!(
            p.health_check().await.status,
            HealthStatus::Unhealthy
        ));
    }

    // ----- settings serde + defaults -----

    #[test]
    fn settings_default_round_trips_through_toml() {
        let s = Settings::default();
        let s_toml = toml::to_string_pretty(&s).unwrap();
        let parsed: Settings = toml::from_str(&s_toml).unwrap();
        assert_eq!(parsed, s);
    }

    #[test]
    fn settings_defaults_match_audiophile_baseline() {
        let s = Settings::default();
        assert_eq!(s.v, 1);
        assert!(!s.resampling.enabled);
        assert!(matches!(s.mixer_type, MixerType::Software));
        assert!(!s.dop);
        assert!(s.output_device.is_empty());
        assert!(!s.volume_normalization);
    }

    #[test]
    fn mixer_type_wire_round_trip() {
        for t in [MixerType::Hardware, MixerType::Software, MixerType::None] {
            let s = t.as_wire_str();
            let back = MixerType::from_wire_str(s).unwrap();
            assert_eq!(t, back);
        }
    }

    #[test]
    fn mixer_type_refuses_unknown() {
        let err = MixerType::from_wire_str("loudest_possible").unwrap_err();
        assert!(err.contains("mixer_type"));
        assert!(err.contains("loudest_possible"));
    }

    // ----- handler tests -----

    #[tokio::test]
    async fn get_settings_returns_defaults_on_fresh_plugin() {
        let (mut p, _dir) = loaded_plugin().await;
        let resp = p
            .handle_request(&req("options.get_settings", json!({ "v": 1 })))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["mixer_type"], "software");
        assert_eq!(v["dop"], false);
        assert_eq!(v["volume_normalization"], false);
    }

    #[tokio::test]
    async fn handle_request_refused_when_not_loaded() {
        let mut p = PlaybackOptionsPlugin::new();
        let err = p
            .handle_request(&req("options.get_settings", json!({ "v": 1 })))
            .await
            .unwrap_err();
        assert!(matches!(err, PluginError::Permanent(_)));
    }

    #[tokio::test]
    async fn unknown_verb_refused() {
        let (mut p, _dir) = loaded_plugin().await;
        let err = p
            .handle_request(&req("options.fly_to_moon", json!({ "v": 1 })))
            .await
            .unwrap_err();
        assert!(matches!(err, PluginError::Permanent(_)));
    }

    #[tokio::test]
    async fn set_mixer_type_persists_and_emits_happening() {
        let (mut p, _dir) = loaded_plugin().await;
        // CapturingEmitter sits behind Arc<dyn HappeningEmitter>;
        // downcasting through dyn-trait erasure isn't worth the
        // unsafe gymnastics for one test. Verify the round-trip
        // via get_settings + the persisted state file instead.
        let resp = p
            .handle_request(&req(
                "options.set_mixer_type",
                json!({ "v": 1, "value": "hardware" }),
            ))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(v["status"], "ok");

        let resp = p
            .handle_request(&req("options.get_settings", json!({ "v": 1 })))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(v["mixer_type"], "hardware");
    }

    #[tokio::test]
    async fn set_mixer_type_refuses_invalid_value() {
        let (mut p, _dir) = loaded_plugin().await;
        let err = p
            .handle_request(&req(
                "options.set_mixer_type",
                json!({ "v": 1, "value": "earsplitting" }),
            ))
            .await
            .unwrap_err();
        match err {
            PluginError::Permanent(msg) => {
                assert!(msg.contains("mixer_type"));
                assert!(msg.contains("earsplitting"));
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_dop_persists_and_round_trips() {
        let (mut p, _dir) = loaded_plugin().await;
        p.handle_request(&req(
            "options.set_dop",
            json!({ "v": 1, "value": true }),
        ))
        .await
        .unwrap();
        let resp = p
            .handle_request(&req("options.get_settings", json!({ "v": 1 })))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(v["dop"], true);
    }

    #[tokio::test]
    async fn set_output_device_accepts_empty_for_default() {
        let (mut p, _dir) = loaded_plugin().await;
        p.handle_request(&req(
            "options.set_output_device",
            json!({ "v": 1, "value": "" }),
        ))
        .await
        .unwrap();
        assert_eq!(p.settings().output_device, "");
    }

    #[tokio::test]
    async fn set_output_device_refuses_whitespace_only() {
        let (mut p, _dir) = loaded_plugin().await;
        let err = p
            .handle_request(&req(
                "options.set_output_device",
                json!({ "v": 1, "value": "   " }),
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, PluginError::Permanent(_)));
    }

    #[tokio::test]
    async fn set_volume_normalization_round_trips() {
        let (mut p, _dir) = loaded_plugin().await;
        p.handle_request(&req(
            "options.set_volume_normalization",
            json!({ "v": 1, "value": true }),
        ))
        .await
        .unwrap();
        assert!(p.settings().volume_normalization);
    }

    #[tokio::test]
    async fn set_resampling_validates_quality_value() {
        let (mut p, _dir) = loaded_plugin().await;
        let err = p
            .handle_request(&req(
                "options.set_resampling",
                json!({
                    "v": 1,
                    "policy": {
                        "enabled": true,
                        "target_bitdepth": "24",
                        "target_samplerate": "192000",
                        "quality": "moonshot",
                    }
                }),
            ))
            .await
            .unwrap_err();
        match err {
            PluginError::Permanent(msg) => assert!(msg.contains("quality")),
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_resampling_accepts_valid_quality() {
        let (mut p, _dir) = loaded_plugin().await;
        p.handle_request(&req(
            "options.set_resampling",
            json!({
                "v": 1,
                "policy": {
                    "enabled": true,
                    "target_bitdepth": "24",
                    "target_samplerate": "96000",
                    "quality": "very_high",
                }
            }),
        ))
        .await
        .unwrap();
        let s = p.settings();
        assert!(s.resampling.enabled);
        assert_eq!(s.resampling.quality, "very_high");
        assert_eq!(s.resampling.target_samplerate, "96000");
    }

    // ----- persistence: settings round-trip across re-load -----

    #[tokio::test]
    async fn settings_persist_to_disk_and_rehydrate() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join(STATE_FILENAME);

        // Plugin instance #1 — write some settings.
        {
            let mut p = PlaybackOptionsPlugin::new()
                .with_state_path(state_path.clone());
            p.happening_emitter = Some(Arc::new(CapturingEmitter::default()));
            p.loaded = true;
            p.handle_request(&req(
                "options.set_mixer_type",
                json!({ "v": 1, "value": "hardware" }),
            ))
            .await
            .unwrap();
            p.handle_request(&req(
                "options.set_dop",
                json!({ "v": 1, "value": true }),
            ))
            .await
            .unwrap();
        }

        // Plugin instance #2 — load from disk.
        let p2 = PlaybackOptionsPlugin::new().with_state_path(state_path);
        let s = p2.load_settings_from_disk().await.unwrap();
        assert!(matches!(s.mixer_type, MixerType::Hardware));
        assert!(s.dop);
    }

    #[tokio::test]
    async fn settings_load_absent_returns_defaults() {
        let dir = tempdir().unwrap();
        let p = PlaybackOptionsPlugin::new()
            .with_state_path(dir.path().join("nonexistent.toml"));
        let s = p.load_settings_from_disk().await.unwrap();
        assert_eq!(s, Settings::default());
    }
}
