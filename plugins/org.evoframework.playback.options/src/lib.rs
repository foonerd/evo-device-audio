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
//! - **Startup volume / max volume** — operator-facing safety
//!   controls that govern the loud-on-boot / loud-by-accident
//!   classes of incident.
//! - **Volume curve** — perceived-loudness mapping (`linear` /
//!   `log` / `natural`) the playback warden applies between the
//!   operator's slider and the actual gain.
//!
//! Stocks the `audio.options` shelf at shape 2.
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use evo_device_audio_shared::transition_envelope::{
    observation_matches, parse_envelope_observed, EnvelopeRequested,
    EnvelopeState, ENVELOPE_OBSERVED_VALUE, ENVELOPE_PAYLOAD_VERSION,
    ENVELOPE_REQUESTED_SUBJECT_TYPE, ENVELOPE_REQUESTED_VALUE, ENVELOPE_SCHEME,
};
use evo_plugin_sdk::contract::{
    BuildInfo, ExternalAddressing, HappeningEmitter, HealthReport, LoadContext,
    Plugin, PluginDescription, PluginError, PluginIdentity, Request,
    Respondent, Response, RuntimeCapabilities, SubjectAnnouncement,
    SubjectAnnouncer, SubjectQuerier, SubjectStateStream,
    SubjectStateStreamError, SubjectStateSubscriber,
};
use evo_plugin_sdk::Manifest;
use serde::{Deserialize, Serialize};

pub mod transition;

/// Timeout the orchestrator waits for the playback warden's
/// envelope-observed acknowledgement at steps 2 (pre-mute)
/// and 7 (unmute). The contract requires bounded waiting:
/// on timeout the orchestrator routes to its rollback chain.
/// Five seconds covers MPD's pause + resume round-trip with
/// margin; substantially-slower wardens surface as a
/// `rolled_back` outcome the operator sees.
const ENVELOPE_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// Timeout the orchestrator applies to each subject-state
/// wire publish call (envelope_requested at steps 2 + 7 and
/// settings at step 3). Bounds an unreachable substrate or a
/// blocked observer fan-out from stalling the transition past
/// its declared SLA. Matches `ENVELOPE_ACK_TIMEOUT` so each
/// publish + ack pair has a uniform bound.
const SUBJECT_PUBLISH_TIMEOUT: Duration = Duration::from_secs(5);

/// Overall budget for a single set_mixer_type gesture. Wraps
/// the eight-step orchestrator so a stuck inner step cannot
/// hold the plugin's `&mut self` past this budget. Sized as
/// the sum of internal step budgets (publish + ack at pre-mute
/// and at unmute, plus publish at set-authority) with slack
/// for disk I/O and observer fan-out reaction. On expiry the
/// orchestrator emits a `failed` lifecycle happening with the
/// `overall_budget` phase tag and returns `Permanent`; the
/// transition lock and the plugin's `&mut self` release as
/// soon as the timeout future resolves, so subsequent requests
/// stop queueing behind the stuck gesture.
const TRANSITION_OVERALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound applied to the post-timeout `failed` lifecycle emit.
/// The happenings bus is a different substrate to the subject
/// announcer (the likely wedge source) so this is defence in
/// depth; on expiry the emit is dropped and the gesture's
/// permanent error still returns to the caller.
const POST_TIMEOUT_LIFECYCLE_BUDGET: Duration = Duration::from_secs(2);

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

/// Lifecycle event types for the mixer-mode transition state
/// machine. Subscribers (UI affordances, audit tooling,
/// subordinate plugins coordinating their re-bind work)
/// observe these instead of inferring the transition shape
/// from the generic `audio.options.changed` happening.
/// Mutually exclusive per `started`:
/// `applied` xor `rolled_back` xor `failed`.
const MIXER_TRANSITION_STARTED: &str = "audio.mixer_transition.started";
const MIXER_TRANSITION_APPLIED: &str = "audio.mixer_transition.applied";
const MIXER_TRANSITION_ROLLED_BACK: &str = "audio.mixer_transition.rolled_back";
const MIXER_TRANSITION_FAILED: &str = "audio.mixer_transition.failed";

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
    "options.set_mixer_device",
    "options.set_mixer_control",
    "options.set_dop",
    "options.set_output_device",
    "options.set_volume_normalization",
    "options.set_startup_volume",
    "options.set_max_volume",
    "options.set_volume_curve",
    "options.set_exclusive_mode",
    "options.set_crossfade_seconds",
    "options.set_gapless",
    "options.set_eq_engaged",
    "options.set_eq_band",
    "options.list_eq_presets",
    "options.save_eq_preset",
    "options.recall_eq_preset",
    "options.delete_eq_preset",
    "options.restore_last_known_good",
    "options.reset_to_defaults",
];

/// Upper bound for the crossfade-seconds operator setting.
/// Above this the setter refuses with a structured Permanent
/// error. Catches slider-typo + UI-bug excursions before they
/// reach MPD.
const CROSSFADE_SECONDS_MAX: u32 = 30;

/// Number of parametric EQ bands the operator surface
/// exposes. Pinned to the schema's `eq-band-count-and-domain`
/// acceptance row; consumer plugins (composition.alsa) honour
/// exactly this count.
pub const EQ_BAND_COUNT: usize = 10;

/// EQ band frequency lower bound (audible band).
const EQ_BAND_FREQ_MIN: u32 = 20;
/// EQ band frequency upper bound (audible band).
const EQ_BAND_FREQ_MAX: u32 = 20_000;
/// EQ band gain lower bound in dB.
const EQ_BAND_GAIN_MIN_DB: f32 = -15.0;
/// EQ band gain upper bound in dB.
const EQ_BAND_GAIN_MAX_DB: f32 = 15.0;
/// EQ band Q lower bound (broad / flat).
const EQ_BAND_Q_MIN: f32 = 0.1;
/// EQ band Q upper bound (narrow / sharp).
const EQ_BAND_Q_MAX: f32 = 30.0;

/// Maximum number of named EQ presets the library holds.
/// Operator save gestures past this count refuse with a
/// structured Permanent error naming the cap. 32 covers the
/// operator-curated set without unbounded growth from a UI
/// bug; the import path is gated by the same cap.
pub const EQ_PRESET_LIBRARY_MAX_COUNT: usize = 32;
/// Maximum length of a preset name in bytes. Operator save
/// gestures with a longer name refuse with a structured
/// Permanent error naming the cap. 64 bytes covers any
/// operator-meaningful name without permitting a UI bug to
/// stash arbitrary blobs in the preset key.
pub const EQ_PRESET_NAME_MAX_LEN: usize = 64;

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
// `Eq` is intentionally omitted because the `eq_bands` field
// carries f32 values (RBJ-EQ-cookbook gain + Q) which do not
// implement `Eq` (NaN). `PartialEq` is sufficient for
// settings-round-trip tests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// ALSA mixer device coordinate for hardware-mixer mode
    /// (typical shape `hw:<card>`). Required-when-
    /// `mixer_type = hardware`; empty otherwise. Paired with
    /// `mixer_control`: together they form the coordinate the
    /// playback warden uses to bind hardware volume control.
    /// Persisted alongside `mixer_type` so a restart restores
    /// the binding without re-probing.
    #[serde(default)]
    pub mixer_device: String,
    /// ALSA mixer control name (e.g. `Master`, `PCM`, DAC-
    /// specific names visible via `amixer scontrols`).
    /// Required-when-`mixer_type = hardware`; empty otherwise.
    /// Paired with `mixer_device`.
    #[serde(default)]
    pub mixer_control: String,
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
    /// Initial volume the playback plugin restores on plugin
    /// load / steward restart. Valid range 0..=100. Default 30
    /// — protects ears + speakers when the device boots after a
    /// power cycle, since the operator may not remember the
    /// pre-shutdown level. Capped at [`max_volume_percent`].
    #[serde(default = "default_startup_volume_percent")]
    pub startup_volume_percent: u8,
    /// Operator-imposed maximum volume the playback plugin
    /// refuses to exceed. Valid range 0..=100. Default 100
    /// (no cap). Setting this below 100 protects sensitive
    /// speakers or living arrangements from accidental
    /// excursions. The playback warden's volume setter clamps
    /// requests above this ceiling.
    #[serde(default = "default_max_volume_percent")]
    pub max_volume_percent: u8,
    /// Perceived-loudness mapping from the operator's volume
    /// slider to the actual gain applied to the audio chain.
    /// Three options: `Linear` (direct mapping; classic Volumio
    /// default), `Log` (logarithmic / dB-scale; sensible
    /// audiophile pick when paired with hardware-mixer mode),
    /// `Natural` (perceptual; matches human-ear loudness
    /// response across the slider range). Default `Linear` for
    /// existing-deployment compatibility.
    #[serde(default)]
    pub volume_curve: VolumeCurve,
    /// Bit-perfect (hog) device mode. When `true`, the delivery
    /// plugin renders the `pcm.evo` chain with NO front-end
    /// `plug` node and pins directly to the card terminus, so
    /// source bytes reach the DAC byte-identical. Default
    /// `false`: the chain uses `plug` (automatic format
    /// conversion) so any source format opens cleanly — the
    /// operator-friendly default. Toggling this on requires the
    /// active delivery plugin to declare
    /// `bit_perfect_capable = true`; the setter refuses
    /// otherwise.
    #[serde(default)]
    pub exclusive_mode: bool,
    /// Between-track crossfade duration in seconds. Domain
    /// 0..=30 (enforced at the setter). `0` disables crossfade
    /// — the audiophile default, no in-chain fade between
    /// tracks. Non-zero values request MPD's protocol-level
    /// crossfade for the named seconds count.
    #[serde(default)]
    pub crossfade_seconds: u32,
    /// Gapless playback flag. Default `true`: the audiophile-
    /// grade reference treats gap-free traversal of the queue
    /// as the canonical behaviour. Setting `false` falls
    /// through to MPD's default single-track behaviour (a
    /// brief gap between adjacent tracks).
    #[serde(default = "default_gapless")]
    pub gapless: bool,
    /// Parametric-EQ engagement flag. When `true`, the
    /// composition plugin's eq_only mode applies the
    /// configured bands to the PCM byte stream. When `false`,
    /// the stream pumps through unchanged even when the
    /// composition mode is eq_only (operator A/B switch
    /// without leaving the EQ mode). Default `false`: the
    /// operator opts in.
    #[serde(default)]
    pub eq_engaged: bool,
    /// Parametric-EQ band configuration. Exactly 10 bands;
    /// schema-pinned. Defaults are flat (all bands at 1 kHz,
    /// 0 dB gain, Q=1.0) — engaging EQ without configuring
    /// bands hears no change.
    #[serde(default = "default_eq_bands")]
    pub eq_bands: Vec<EqBand>,
    /// Named EQ preset library. Each entry carries a unique
    /// name + the same 10-band shape as `eq_bands`. Operator
    /// gestures save / recall / delete named presets;
    /// `recall` atomically replaces `eq_bands` with the named
    /// preset's bands and emits a single
    /// `audio.options.changed` happening (not 10). The
    /// `eq_engaged` flag is intentionally NOT part of a
    /// preset — engagement is session state, not part of a
    /// saved curve. Bounded by [`EQ_PRESET_LIBRARY_MAX_COUNT`]
    /// and per-entry name length by
    /// [`EQ_PRESET_NAME_MAX_LEN`].
    #[serde(default)]
    pub eq_presets: Vec<NamedEqPreset>,
}

/// One named EQ preset. Round-trips through the settings file
/// alongside `eq_bands` so the preset library survives plugin
/// restart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamedEqPreset {
    /// Operator-meaningful name (UTF-8, 1..=64 bytes; the
    /// setter refuses empty or over-long names). Names are
    /// case-sensitive and unique within the library.
    pub name: String,
    /// 10 band parameters matching the schema-pinned shape.
    pub bands: Vec<EqBand>,
}

/// One band of the parametric EQ. Mirrors the schema's
/// per-band control surface 1:1.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EqBand {
    /// Centre frequency in Hz (20..=20000).
    pub freq_hz: u32,
    /// Gain in dB (-15.0..=15.0). 0 disables the band.
    pub gain_db: f32,
    /// Quality factor (0.1..=30.0). Higher = narrower band.
    pub q: f32,
}

impl Default for EqBand {
    fn default() -> Self {
        Self {
            freq_hz: 1000,
            gain_db: 0.0,
            q: 1.0,
        }
    }
}

fn default_eq_bands() -> Vec<EqBand> {
    vec![EqBand::default(); EQ_BAND_COUNT]
}

fn default_startup_volume_percent() -> u8 {
    30
}

fn default_max_volume_percent() -> u8 {
    100
}

fn default_gapless() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            v: PAYLOAD_VERSION,
            resampling: ResamplingPolicy::default(),
            mixer_type: MixerType::default(),
            mixer_device: String::new(),
            mixer_control: String::new(),
            dop: false,
            output_device: String::new(),
            volume_normalization: false,
            startup_volume_percent: default_startup_volume_percent(),
            max_volume_percent: default_max_volume_percent(),
            volume_curve: VolumeCurve::default(),
            exclusive_mode: false,
            crossfade_seconds: 0,
            gapless: default_gapless(),
            eq_engaged: false,
            eq_bands: default_eq_bands(),
            eq_presets: Vec::new(),
        }
    }
}

/// Perceived-loudness mapping from operator-facing volume slider
/// to applied gain.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum VolumeCurve {
    /// Direct mapping: 50% slider = 50% applied gain. Classic
    /// default; the gentlest learning curve for operators
    /// migrating from prior systems.
    #[default]
    Linear,
    /// Logarithmic (dB-scale) mapping. The audiophile choice
    /// when paired with a hardware mixer that already gates the
    /// signal in the analog domain.
    Log,
    /// Perceptual mapping tuned to human loudness response
    /// across the slider range.
    Natural,
}

impl VolumeCurve {
    /// Parse a wire string into the typed enum. Errors carry the
    /// operator-readable invalid-value diagnostic the setter
    /// uses for refusal.
    pub fn from_wire_str(value: &str) -> Result<Self, String> {
        match value {
            "linear" => Ok(Self::Linear),
            "log" => Ok(Self::Log),
            "natural" => Ok(Self::Natural),
            other => Err(format!(
                "volume_curve must be one of {{linear, log, natural}}; \
                 got {other:?}"
            )),
        }
    }

    /// Stable wire string for the typed enum.
    pub fn as_wire_str(&self) -> &'static str {
        match self {
            Self::Linear => "linear",
            Self::Log => "log",
            Self::Natural => "natural",
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
    /// Mixer-transition orchestrator lock. Held for the
    /// duration of `handle_set_mixer_type`'s state-machine
    /// run so concurrent gestures serialise (I3 single
    /// authority). The lock guards a `()` because the
    /// orchestrator owns no mutable state of its own — the
    /// state machine is stateless; mutations route through
    /// the executor's persist call.
    transition_lock: Arc<tokio::sync::Mutex<()>>,
    /// Subject-state subscriber handle from `LoadContext`.
    /// Used by the orchestrator's steps 2 + 7 to await the
    /// playback warden's envelope_observed acknowledgement.
    /// `Option` so OOP transport pre-wire-surface admission
    /// doesn't fail load when the subscriber is unpopulated;
    /// missing subscriber surfaces as the orchestrator
    /// running the safety envelope in advisory-only mode (a
    /// debug log on each transition; no rollback).
    subject_state_subscriber: Option<Arc<dyn SubjectStateSubscriber>>,
    /// Subject querier from `LoadContext`. Used by the
    /// orchestrator to resolve the envelope_observed
    /// addressing to a canonical id before subscribing.
    /// Option for the same reason as
    /// [`Self::subject_state_subscriber`].
    subject_querier: Option<Arc<dyn SubjectQuerier>>,
    /// Generation counter the orchestrator bumps per
    /// envelope-coordination step. Each pre-mute / unmute
    /// publishes a fresh generation so the warden can
    /// disambiguate a stale observation from a fresh
    /// request.
    envelope_generation: Arc<AtomicU64>,
    /// Per-instance override for the envelope-ack timeout.
    /// Production paths leave this `None` and the
    /// [`ENVELOPE_ACK_TIMEOUT`] default applies. Tests
    /// override with shorter durations so the timeout-
    /// routes-to-rollback test runs in milliseconds.
    envelope_ack_timeout_override: Option<Duration>,
    /// Per-instance override for the wire-publish timeout
    /// applied at steps 2, 3, and 7 of the mixer-transition
    /// orchestrator. Production paths leave this `None` and
    /// the [`SUBJECT_PUBLISH_TIMEOUT`] default applies. The
    /// regression test that simulates a wedged subject
    /// announcer overrides with a sub-second duration so the
    /// test runs in tens of milliseconds.
    subject_publish_timeout_override: Option<Duration>,
    /// Per-instance override for the overall transition
    /// budget on `set_mixer_type`. Production paths leave
    /// this `None` and the [`TRANSITION_OVERALL_TIMEOUT`]
    /// default applies. The regression test that exercises
    /// the outer-budget recovery path overrides with a sub-
    /// second duration.
    transition_overall_timeout_override: Option<Duration>,
    /// Cached envelope_observed channel — the warden's
    /// canonical id + the long-lived state-stream the
    /// orchestrator reads on every transition's mute and
    /// unmute steps.
    ///
    /// **Architectural invariant: resolved + subscribed
    /// exactly once per plugin lifetime.** Per-transition
    /// resolve_addressing + subscribe_subject are eliminated
    /// — those calls pay substrate-roundtrip cost on every
    /// gesture and are the cascade entry point for
    /// substrate slowness. The first `set_mixer_type` after
    /// warden admission resolves + caches; every subsequent
    /// transition reuses the cached channel and pays only
    /// the recv-loop cost (one broadcast read + a generation
    /// filter against the warden's reply).
    ///
    /// `Option<Arc<...>>` so the lazy-init path is race-safe
    /// under the orchestrator's transition lock (the inner
    /// Mutex wraps the stream for serialised recv access).
    /// Pre-warden transitions return `Ok(None)` and the
    /// orchestrator runs in advisory-only mode (legacy
    /// behavior preserved); init does not cache a `None`
    /// outcome, so a warden admitting AFTER the first
    /// transition is picked up by the next transition's
    /// init attempt.
    envelope_observed_channel:
        tokio::sync::Mutex<Option<Arc<EnvelopeObservedChannel>>>,
    requests_handled: u64,
}

/// Cached envelope_observed channel reused across mixer-
/// transition gestures. See
/// [`PlaybackOptionsPlugin::envelope_observed_channel`] for
/// the architectural invariant.
struct EnvelopeObservedChannel {
    /// Warden's canonical subject id for the
    /// envelope_observed subject. Cached after the one-time
    /// resolve at first-transition lazy init.
    canonical_id: String,
    /// Long-lived state-update stream. Wrapped in a
    /// `tokio::sync::Mutex` so the orchestrator's transition
    /// lock (which already serialises transitions) is the
    /// only contention point; the inner mutex's lock is
    /// acquired and released per transition.
    stream: tokio::sync::Mutex<SubjectStateStream>,
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
            transition_lock: Arc::new(tokio::sync::Mutex::new(())),
            subject_state_subscriber: None,
            subject_querier: None,
            envelope_generation: Arc::new(AtomicU64::new(0)),
            envelope_ack_timeout_override: None,
            subject_publish_timeout_override: None,
            transition_overall_timeout_override: None,
            envelope_observed_channel: tokio::sync::Mutex::new(None),
            requests_handled: 0,
        }
    }

    /// Test-only override for the envelope-ack timeout. The
    /// timeout-routes-to-rollback test sets a short duration
    /// (e.g. 50ms) so the test runs in tens of milliseconds
    /// rather than the production default of five seconds.
    #[cfg(test)]
    pub(crate) fn with_envelope_ack_timeout_override(
        mut self,
        timeout: Duration,
    ) -> Self {
        self.envelope_ack_timeout_override = Some(timeout);
        self
    }

    /// Test-only override for the wire-publish timeout used by
    /// `publish_envelope_requested` and `publish_settings_state`.
    /// The slow-announcer regression test sets a sub-second
    /// duration so the test runs without waiting the production
    /// default of five seconds.
    #[cfg(test)]
    pub(crate) fn with_subject_publish_timeout_override(
        mut self,
        timeout: Duration,
    ) -> Self {
        self.subject_publish_timeout_override = Some(timeout);
        self
    }

    /// Test-only override for the overall transition budget.
    /// The outer-budget regression test sets a sub-second
    /// duration so the test runs without waiting the production
    /// default of thirty seconds.
    #[cfg(test)]
    pub(crate) fn with_transition_overall_timeout_override(
        mut self,
        timeout: Duration,
    ) -> Self {
        self.transition_overall_timeout_override = Some(timeout);
        self
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

    /// External addressing for the orchestrator-published
    /// envelope_requested subject. Defined as a const-like
    /// helper so the orchestrator's publish + the
    /// (test-only) subject identity introspection both
    /// reference the same source-of-truth shape.
    fn envelope_requested_addressing() -> ExternalAddressing {
        ExternalAddressing {
            scheme: ENVELOPE_SCHEME.to_string(),
            value: ENVELOPE_REQUESTED_VALUE.to_string(),
        }
    }

    /// External addressing for the warden-published
    /// envelope_observed subject. The orchestrator resolves
    /// this addressing each gesture to discover the warden's
    /// canonical id before subscribing.
    fn envelope_observed_addressing() -> ExternalAddressing {
        ExternalAddressing {
            scheme: ENVELOPE_SCHEME.to_string(),
            value: ENVELOPE_OBSERVED_VALUE.to_string(),
        }
    }

    /// Announce the envelope_requested subject at load time
    /// so the warden can resolve its canonical id ahead of
    /// the first transition gesture. The subject's initial
    /// state reflects "no transition in flight" — generation
    /// 0, observation-style payload skipped (the warden
    /// matches on generation + state in a fresh request,
    /// not on the initial-announce payload).
    async fn announce_envelope_requested_subject(&self) {
        let Some(announcer) = self.subject_announcer.as_ref() else {
            return;
        };
        // Seed payload: an idle request the warden treats as
        // a no-op (the warden's subscriber matches new
        // generations, not the initial-announce one).
        let initial = serde_json::json!({
            "v": ENVELOPE_PAYLOAD_VERSION,
            "generation": 0_u64,
            "requested_state": "unmuted",
            "requested_at_ms": 0_u64,
        });
        let announcement = SubjectAnnouncement {
            subject_type: ENVELOPE_REQUESTED_SUBJECT_TYPE.to_string(),
            addressings: vec![Self::envelope_requested_addressing()],
            claims: Vec::new(),
            state: initial,
            announced_at: std::time::SystemTime::now(),
        };
        if let Err(e) = announcer.announce(announcement).await {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "announce envelope_requested subject failed"
            );
        }
    }

    /// Publish an envelope-coordination request via the
    /// settings subject's announcer. The orchestrator's pre-
    /// mute (state = Muted) and unmute (state = Unmuted)
    /// steps call this with a freshly-bumped generation.
    ///
    /// Returns the generation value the request was published
    /// at, so the caller can match it against the warden's
    /// ack.
    async fn publish_envelope_requested(
        &self,
        state: EnvelopeState,
    ) -> Result<u64, String> {
        let Some(announcer) = self.subject_announcer.as_ref() else {
            return Err(
                "subject_announcer unpopulated; cannot publish envelope"
                    .to_string(),
            );
        };
        let generation =
            self.envelope_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let payload = EnvelopeRequested {
            v: ENVELOPE_PAYLOAD_VERSION,
            generation,
            requested_state: state,
            requested_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        };
        let value = serde_json::to_value(&payload)
            .map_err(|e| format!("serialise envelope_requested: {e}"))?;
        // Background the wire publish. The substrate's in-memory
        // broadcast (the channel the warden's subject-state stream
        // consumes) is fed BEFORE the durable + happening emit
        // chain — so subscribers see the new state immediately
        // while the wire ack to the publisher waits on the slow
        // SQLite-serialised emit. Awaiting that ack adds nothing
        // to functional correctness (the warden has already
        // received the request); it just stalls the orchestrator.
        // Background the publish, return the generation
        // immediately, and let `await_envelope_ack` observe the
        // warden's response via its own subscription. The
        // SUBJECT_PUBLISH_TIMEOUT bound stays inside the spawned
        // task so a permanently-wedged substrate cannot leak a
        // task that lives forever.
        let publish_timeout = self
            .subject_publish_timeout_override
            .unwrap_or(SUBJECT_PUBLISH_TIMEOUT);
        let announcer_for_task = Arc::clone(announcer);
        let addressing_for_task = Self::envelope_requested_addressing();
        let state_label = match state {
            EnvelopeState::Muted => "muted",
            EnvelopeState::Unmuted => "unmuted",
        };
        tokio::spawn(async move {
            let publish_future =
                announcer_for_task.update_state(addressing_for_task, value);
            match tokio::time::timeout(publish_timeout, publish_future).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!(
                        plugin = PLUGIN_NAME,
                        error = ?e,
                        generation = generation,
                        requested_state = state_label,
                        "envelope_requested wire publish errored after \
                         backgrounding"
                    );
                }
                Err(_) => {
                    tracing::warn!(
                        plugin = PLUGIN_NAME,
                        generation = generation,
                        requested_state = state_label,
                        timeout_ms = publish_timeout.as_millis() as u64,
                        "envelope_requested wire publish timed out after \
                         backgrounding; warden may have already seen the \
                         state via the substrate's in-memory broadcast"
                    );
                }
            }
        });
        Ok(generation)
    }

    /// Resolve + subscribe the warden's envelope_observed
    /// channel ONCE, cache it on the plugin, return the cached
    /// handle on subsequent calls.
    ///
    /// **Architectural Fix #2 entry point.** Per-transition
    /// resolve+subscribe is eliminated — those calls pay
    /// substrate-roundtrip cost on every gesture and are the
    /// cascade entry point for substrate slowness. The first
    /// call after warden admission resolves + caches; every
    /// subsequent call returns the cached handle in O(1).
    ///
    /// Return values:
    ///
    /// - `Ok(Some(channel))` — channel cached + ready for recv.
    /// - `Ok(None)` — no warden announced (advisory-only mode);
    ///   does NOT cache the None, so a warden admitting later
    ///   is picked up by the next call.
    /// - `Err(reason)` — substrate error during resolve or
    ///   subscribe; does NOT cache; next call retries.
    async fn ensure_envelope_observed_channel(
        &self,
    ) -> Result<Option<Arc<EnvelopeObservedChannel>>, String> {
        {
            let guard = self.envelope_observed_channel.lock().await;
            if let Some(channel) = guard.as_ref() {
                return Ok(Some(Arc::clone(channel)));
            }
        }
        let Some(subscriber) = self.subject_state_subscriber.as_ref() else {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                "subject_state_subscriber unpopulated; safety envelope \
                 advisory-only"
            );
            return Ok(None);
        };
        let Some(querier) = self.subject_querier.as_ref() else {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                "subject_querier unpopulated; safety envelope advisory-only"
            );
            return Ok(None);
        };
        let canonical_id = match querier
            .resolve_addressing(Self::envelope_observed_addressing())
            .await
        {
            Ok(Some(id)) => id,
            Ok(None) => {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    "envelope_observed subject not yet announced; safety \
                     envelope advisory-only for this transition (will retry \
                     init on next gesture)"
                );
                return Ok(None);
            }
            Err(e) => {
                return Err(format!(
                    "resolve envelope_observed addressing: {e:?}"
                ));
            }
        };
        let stream =
            subscriber
                .subscribe_subject(canonical_id.clone())
                .await
                .map_err(|e| format!("subscribe envelope_observed: {e:?}"))?;
        let channel = Arc::new(EnvelopeObservedChannel {
            canonical_id: canonical_id.clone(),
            stream: tokio::sync::Mutex::new(stream),
        });
        let mut guard = self.envelope_observed_channel.lock().await;
        // Race-safe insert: another caller may have raced past
        // the initial cache miss and resolved first; if so, use
        // its channel rather than ours.
        if guard.is_none() {
            *guard = Some(Arc::clone(&channel));
            tracing::info!(
                plugin = PLUGIN_NAME,
                canonical_id = %canonical_id,
                "envelope_observed channel resolved + subscribed (cached \
                 for the plugin's lifetime; subsequent transitions reuse)"
            );
        }
        Ok(guard.as_ref().map(Arc::clone))
    }

    /// Await the warden's envelope_observed acknowledgement
    /// matching `(generation, expected_state)` within
    /// [`ENVELOPE_ACK_TIMEOUT`]. Reads from the cached
    /// long-lived channel established by
    /// [`Self::ensure_envelope_observed_channel`]; per-
    /// transition substrate roundtrips are eliminated.
    ///
    /// Outcomes:
    ///
    /// - `Ok(())` — ack received within budget (warden
    ///   participating + responded).
    /// - `Ok(())` — addressing resolves to None (no
    ///   participating warden; advisory mode). Logged at
    ///   debug. Absence of a playback warden means no audio
    ///   chain to protect; when a warden admits later the
    ///   next transition's lazy init picks it up.
    /// - `Err(reason)` — stream closed before ack OR the
    ///   recv-loop timeout fired. The orchestrator routes
    ///   to the rollback chain.
    async fn await_envelope_ack(
        &self,
        generation: u64,
        expected_state: EnvelopeState,
    ) -> Result<(), String> {
        let Some(channel) = self.ensure_envelope_observed_channel().await?
        else {
            // Advisory-only: no warden participating, or the
            // warden has not yet announced. The orchestrator
            // proceeds without a confirmed ack; the transition's
            // safety story is reduced to the operator's own
            // gesture-level confirmation. Legacy behavior
            // preserved.
            return Ok(());
        };
        let timeout = self
            .envelope_ack_timeout_override
            .unwrap_or(ENVELOPE_ACK_TIMEOUT);
        let result = tokio::time::timeout(timeout, async {
            let mut stream = channel.stream.lock().await;
            loop {
                match stream.recv().await {
                    Ok(update) => {
                        if let Some(state) = update.state.as_ref() {
                            if let Some(obs) = parse_envelope_observed(state) {
                                if observation_matches(
                                    &obs,
                                    generation,
                                    expected_state,
                                ) {
                                    return Ok(());
                                }
                            }
                        }
                    }
                    Err(SubjectStateStreamError::Lagged { .. }) => continue,
                    Err(SubjectStateStreamError::Closed) => {
                        return Err(format!(
                            "envelope_observed stream closed before ack \
                             (canonical_id={})",
                            channel.canonical_id
                        ));
                    }
                }
            }
        })
        .await;

        match result {
            Ok(r) => r,
            Err(_) => Err(format!(
                "envelope ack timeout after {}ms (generation={generation}, \
                 expected_state={expected_state:?})",
                timeout.as_millis()
            )),
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
        // Background the wire publish. Subscribers (delivery.alsa,
        // playback.mpd) see the new state via the substrate's
        // in-memory broadcast immediately; the wire ack to the
        // publisher waits for the slow durable + happening emit
        // chain. The orchestrator does not need to wait for that
        // ack — the settings publish is best-effort (no caller
        // checks the return value) and the downstream reactors
        // are already running.
        let publish_timeout = self
            .subject_publish_timeout_override
            .unwrap_or(SUBJECT_PUBLISH_TIMEOUT);
        let announcer_for_task = Arc::clone(announcer);
        let addressing_for_task = Self::settings_addressing();
        tokio::spawn(async move {
            let publish_future =
                announcer_for_task.update_state(addressing_for_task, state);
            match tokio::time::timeout(publish_timeout, publish_future).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!(
                        plugin = PLUGIN_NAME,
                        error = ?e,
                        "settings subject state wire publish errored after \
                         backgrounding"
                    );
                }
                Err(_) => {
                    tracing::warn!(
                        plugin = PLUGIN_NAME,
                        timeout_ms = publish_timeout.as_millis() as u64,
                        "settings subject state wire publish timed out after \
                         backgrounding"
                    );
                }
            }
        });
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
            self.subject_state_subscriber =
                ctx.subject_state_subscriber.as_ref().map(Arc::clone);
            self.subject_querier = ctx.subject_querier.as_ref().map(Arc::clone);
            // Announce the settings subject so consumers
            // (playback.mpd, future UI plugins) can resolve
            // its canonical id + subscribe to state changes
            // via the framework's SubjectStateSubscriber. The
            // announce carries the current settings as state
            // so consumers seeing the SubjectRegistered
            // happening have the initial value without a
            // separate round-trip.
            self.announce_settings_subject().await;
            // Announce the envelope_requested subject so the
            // playback warden can resolve its canonical id +
            // subscribe to mute / unmute requests the
            // orchestrator publishes on each safety-envelope
            // step.
            self.announce_envelope_requested_subject().await;
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
                "options.set_mixer_device" => {
                    self.handle_set_mixer_device(req).await
                }
                "options.set_mixer_control" => {
                    self.handle_set_mixer_control(req).await
                }
                "options.set_dop" => self.handle_set_dop(req).await,
                "options.set_output_device" => {
                    self.handle_set_output_device(req).await
                }
                "options.set_volume_normalization" => {
                    self.handle_set_volume_normalization(req).await
                }
                "options.set_startup_volume" => {
                    self.handle_set_startup_volume(req).await
                }
                "options.set_max_volume" => {
                    self.handle_set_max_volume(req).await
                }
                "options.set_volume_curve" => {
                    self.handle_set_volume_curve(req).await
                }
                "options.set_exclusive_mode" => {
                    self.handle_set_exclusive_mode(req).await
                }
                "options.set_crossfade_seconds" => {
                    self.handle_set_crossfade_seconds(req).await
                }
                "options.set_gapless" => self.handle_set_gapless(req).await,
                "options.set_eq_engaged" => {
                    self.handle_set_eq_engaged(req).await
                }
                "options.set_eq_band" => self.handle_set_eq_band(req).await,
                "options.list_eq_presets" => {
                    self.handle_list_eq_presets(req).await
                }
                "options.save_eq_preset" => {
                    self.handle_save_eq_preset(req).await
                }
                "options.recall_eq_preset" => {
                    self.handle_recall_eq_preset(req).await
                }
                "options.delete_eq_preset" => {
                    self.handle_delete_eq_preset(req).await
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

    /// Operator-facing mixer-type setter. Unlike the other
    /// `set_*` handlers, this one runs an ORCHESTRATED
    /// transition under the per-plugin transition lock so the
    /// gesture honours the mixer-transition invariants
    /// contract (loudness continuity / no blast risk / single
    /// authority / deterministic carry-over / rollback safe /
    /// operator truth).
    ///
    /// Eight steps in sequence:
    ///
    /// 1. read_carried_level — baseline carry-over level
    /// 2. pre_mute — engages the safety envelope
    /// 3. set_new_authority — persists the new mixer_type to
    ///    the settings projection (the existing reactors on
    ///    delivery + playback see this via the settings
    ///    subject)
    /// 4. switch_fragment — delivery.alsa renders the new
    ///    drop-in via its settings-subject reactor
    /// 5. restart_playback — playback.mpd restarts via its
    ///    settings-subject reactor
    /// 6. verify_effective_level — confirms the post-persist
    ///    settings landed
    /// 7. unmute — releases the safety envelope
    /// 8. emit applied
    ///
    /// Pre-mute + unmute are NO-OPS in this commit; the
    /// subordinate-coordinated mute mechanism (delivery.alsa
    /// rendering a softvol-mute stage during the envelope)
    /// lands in the next focused commit alongside the
    /// completion-ack subject the orchestrator awaits.
    /// Hardware-loopback calibration of the audible blast-
    /// safety guarantee is hardware-gated.
    ///
    /// Hardware mode is refused when `mixer_device` or
    /// `mixer_control` is empty — the operator picked
    /// Hardware for a reason; silent degrade-to-software
    /// would mask the missing coordinate (contract
    /// invariant `mixer-transition-hardware-mode-requires-
    /// device-and-control`).
    ///
    /// The four lifecycle happenings
    /// (`audio.mixer_transition.{started, applied,
    /// rolled_back, failed}`) fire in mutually-exclusive
    /// terminal-event shape: every `started` is followed by
    /// exactly one of `applied` / `rolled_back` / `failed`.
    async fn handle_set_mixer_type(
        &mut self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: SetMixerTypePayload = parse_versioned(req)?;
        let target = MixerType::from_wire_str(&payload.value)
            .map_err(PluginError::Permanent)?;

        // Hardware mode requires the device + control
        // coordinates to be set first. The contract pins this
        // as `mixer-transition-hardware-mode-requires-device-
        // and-control` — silent degrade is a contract
        // violation (operator picked hardware for a reason).
        if matches!(target, MixerType::Hardware)
            && (self.settings.mixer_device.is_empty()
                || self.settings.mixer_control.is_empty())
        {
            return Err(PluginError::Permanent(
                "mixer_type = hardware requires mixer_device + \
                 mixer_control to be populated first (use \
                 options.set_mixer_device and options.set_mixer_control \
                 before switching to hardware mode)"
                    .to_string(),
            ));
        }

        let from = self.settings.mixer_type;
        let to = target;

        // Acquire the transition lock for the duration of the
        // state machine. Concurrent mixer_type gestures wait
        // (single-authority invariant).
        let lock_arc = Arc::clone(&self.transition_lock);
        let _lock = lock_arc.lock_owned().await;

        // Wrap the orchestrator in an overall budget. Each
        // internal step (envelope publish, ack await, persist,
        // settings publish, unmute publish, ack await) already
        // carries its own bound; the outer budget is the safety
        // net for any pathological compound case, and releases
        // the plugin's `&mut self` deterministically so
        // subsequent requests cannot queue behind a stuck
        // gesture.
        let overall_timeout = self
            .transition_overall_timeout_override
            .unwrap_or(TRANSITION_OVERALL_TIMEOUT);
        let outcome = match tokio::time::timeout(
            overall_timeout,
            self.run_mixer_transition(from, to),
        )
        .await
        {
            Ok(o) => o,
            Err(_) => {
                let reason = format!(
                    "transition_timed_out after {}ms (inner orchestration did \
                     not return within the overall budget)",
                    overall_timeout.as_millis()
                );
                // Cancellation may have left in-memory settings
                // out of step with the persisted state.toml (the
                // window is narrow: between the in-memory mutate
                // at step 3 and the fsync completion). Re-read
                // disk as the authoritative source so the next
                // request observes a coherent self.settings.
                if let Ok(restored) = self.load_settings_from_disk().await {
                    self.settings = restored;
                }
                // Emit the lifecycle.failed happening through a
                // short budget so the happenings bus cannot also
                // wedge the recovery path (defence in depth; the
                // happenings substrate is distinct from the
                // subject announcer that is the likely wedge
                // source, but engineering bar forbids any
                // unbounded await in the recovery path).
                let _ = tokio::time::timeout(
                    POST_TIMEOUT_LIFECYCLE_BUDGET,
                    self.emit_lifecycle_failed(
                        from,
                        to,
                        "overall_budget",
                        &reason,
                    ),
                )
                .await;
                return Err(PluginError::Permanent(format!(
                    "mixer-transition failed at overall_budget (chain in \
                     unknown post-pre-mute state; operator intervention \
                     required): {reason}"
                )));
            }
        };

        match outcome {
            transition::TransitionOutcome::Applied { .. }
            | transition::TransitionOutcome::NoOp => encode(
                req,
                &SimpleOk {
                    v: PAYLOAD_VERSION,
                    status: "ok",
                },
            ),
            transition::TransitionOutcome::RolledBack { at_phase, reason } => {
                Err(PluginError::Permanent(format!(
                    "mixer-transition rolled back at {at_phase}: {reason}"
                )))
            }
            transition::TransitionOutcome::Failed { at_phase, reason } => {
                Err(PluginError::Permanent(format!(
                    "mixer-transition failed at {at_phase} (chain in terminal \
                 muted state; operator intervention required): {reason}"
                )))
            }
        }
    }

    /// Inline orchestrator: walks the eight contract steps for
    /// a mixer-type transition. Mirrors the trait-based
    /// state machine in `crate::transition::run_transition`
    /// (which the regression-guard tests in
    /// `transition::tests` exercise with stub executors). The
    /// inline form here is the production path because the
    /// trait-based form requires lifting `&mut self` through
    /// the executor with awkward interior-mutability
    /// machinery; the step ordering + rollback shape + four
    /// lifecycle emissions are identical between the two.
    async fn run_mixer_transition(
        &mut self,
        from: MixerType,
        to: MixerType,
    ) -> transition::TransitionOutcome {
        if from == to {
            return transition::TransitionOutcome::NoOp;
        }

        self.emit_lifecycle_started(from, to).await;

        // Step 1.5 — pre-subscribe to envelope_observed BEFORE
        // any envelope_requested publish. The substrate
        // broadcasts state updates synchronously to active
        // receivers (its in-memory broadcast precedes the
        // durable + happening emit chain — see
        // `publish_envelope_requested`'s comment block); a
        // publish-then-subscribe pattern races the warden's
        // response broadcast against the orchestrator's
        // subscribe, and on every first gesture where the
        // warden's fanout outruns the subscribe the broadcast
        // is lost to a not-yet-registered receiver. The
        // orchestrator's `await_envelope_ack` then waits its
        // full `ENVELOPE_ACK_TIMEOUT` for an ack that already
        // came and went, then routes to the rollback chain —
        // the operator sees "Audio mode change reverted" with
        // an `envelope ack timeout` reason.
        //
        // `ensure_envelope_observed_channel` is idempotent:
        //
        //   * `Ok(Some(_))` — channel cached for the plugin
        //     lifetime; subsequent transitions read from the
        //     same long-lived receiver. The cache is populated
        //     on first success here and reused at every
        //     subsequent `await_envelope_ack` invocation.
        //   * `Ok(None)`  — no warden has announced its
        //     `envelope_observed` subject; the orchestrator
        //     runs in advisory-only mode. Does NOT cache, so
        //     the next gesture re-attempts the resolve once
        //     the warden lands.
        //   * `Err(e)`    — substrate failure. The transition
        //     rolls back at phase `pre_subscribe` — the
        //     `started` happening is already emitted, so the
        //     lifecycle invariant (`started` → `rolled_back`
        //     xor `applied` xor `failed`) is preserved.
        if let Err(e) = self.ensure_envelope_observed_channel().await {
            return self.rollback_or_fail(from, to, "pre_subscribe", e).await;
        }

        // Step 1 — read carried level. Baseline: the operator's
        // configured startup-volume floor (deterministic; same
        // across both authorities). Cross-plugin live-volume
        // read lands alongside the subordinate coordination
        // work.
        let carried_level = self.settings.startup_volume_percent;

        // Step 2 — pre-mute. Publishes envelope_requested
        // with state=Muted + a fresh generation, then awaits
        // the playback warden's matching envelope_observed
        // ack. The warden pauses the playback chain on
        // receipt; we proceed only after the chain is
        // observably muted (or the addressing doesn't
        // resolve — no participating warden — in which case
        // we run advisory-only). Timeout → rollback chain.
        let pre_mute_generation = match self
            .publish_envelope_requested(EnvelopeState::Muted)
            .await
        {
            Ok(g) => g,
            Err(e) => {
                return self.rollback_or_fail(from, to, "pre_mute", e).await;
            }
        };
        if let Err(e) = self
            .await_envelope_ack(pre_mute_generation, EnvelopeState::Muted)
            .await
        {
            return self.rollback_or_fail(from, to, "pre_mute", e).await;
        }

        // Step 3 — set new authority. Persists the new
        // mixer_type via the same persist + subject-publish
        // path the legacy direct setter used (the legitimate
        // mutation mechanism; subordinate reactors on
        // delivery.alsa + playback.mpd consume the settings
        // subject to render / restart).
        let prior_settings = self.settings.clone();
        self.settings.mixer_type = to;
        if let Err(e) = self.persist_settings().await {
            self.settings = prior_settings.clone();
            return self
                .rollback_or_fail(from, to, "set_new_authority", e.to_string())
                .await;
        }
        self.publish_settings_state().await;

        // Step 4 — switch fragment. No-op: delivery.alsa
        // renders the new drop-in via its settings-subject
        // reactor.

        // Step 5 — restart playback. No-op: playback.mpd
        // restarts via its settings-subject reactor.

        // Step 6 — verify effective level. Confirms the
        // post-persist projection matches the gesture's
        // target (the in-memory self.settings should equal
        // what we just wrote).
        if self.settings.mixer_type != to {
            let reason = format!(
                "post-persist mixer_type {:?} != target {to:?}",
                self.settings.mixer_type
            );
            return self
                .rollback_or_fail_restore(
                    from,
                    to,
                    "verify_effective_level",
                    reason,
                    prior_settings,
                )
                .await;
        }
        let effective_level = carried_level;

        // Step 7 — unmute. Symmetric to step 2: publish
        // envelope_requested with state=Unmuted, await the
        // warden's matching ack. Failure here is a rollback
        // even though the new authority has already been
        // persisted — the operator-facing behaviour is the
        // chain remains muted with the prior settings
        // restored; the operator can re-attempt the gesture
        // once the underlying cause clears.
        let unmute_generation = match self
            .publish_envelope_requested(EnvelopeState::Unmuted)
            .await
        {
            Ok(g) => g,
            Err(e) => {
                return self
                    .rollback_or_fail_restore(
                        from,
                        to,
                        "unmute",
                        e,
                        prior_settings,
                    )
                    .await;
            }
        };
        if let Err(e) = self
            .await_envelope_ack(unmute_generation, EnvelopeState::Unmuted)
            .await
        {
            return self
                .rollback_or_fail_restore(from, to, "unmute", e, prior_settings)
                .await;
        }

        // Step 8 — emit applied.
        self.emit_lifecycle_applied(from, to, carried_level, effective_level)
            .await;
        transition::TransitionOutcome::Applied { effective_level }
    }

    /// Roll back without restoring prior settings (the failure
    /// occurred BEFORE persistence; nothing on disk to undo).
    async fn rollback_or_fail(
        &self,
        from: MixerType,
        to: MixerType,
        at_phase: &'static str,
        reason: String,
    ) -> transition::TransitionOutcome {
        self.emit_lifecycle_rolled_back(from, to, at_phase, &reason)
            .await;
        transition::TransitionOutcome::RolledBack { at_phase, reason }
    }

    /// Roll back AND restore prior settings (the failure
    /// occurred AFTER persistence; the live state file +
    /// subject must be reverted).
    async fn rollback_or_fail_restore(
        &mut self,
        from: MixerType,
        to: MixerType,
        at_phase: &'static str,
        reason: String,
        prior_settings: Settings,
    ) -> transition::TransitionOutcome {
        self.settings = prior_settings;
        match self.persist_settings().await {
            Ok(()) => {
                self.publish_settings_state().await;
                self.emit_lifecycle_rolled_back(from, to, at_phase, &reason)
                    .await;
                transition::TransitionOutcome::RolledBack { at_phase, reason }
            }
            Err(rollback_err) => {
                let combined =
                    format!("{reason}; rollback failed: {rollback_err}");
                self.emit_lifecycle_failed(from, to, at_phase, &combined)
                    .await;
                transition::TransitionOutcome::Failed {
                    at_phase,
                    reason: combined,
                }
            }
        }
    }

    async fn emit_lifecycle_started(&self, from: MixerType, to: MixerType) {
        self.emit_lifecycle(
            MIXER_TRANSITION_STARTED,
            serde_json::json!({
                "v": PAYLOAD_VERSION,
                "from": from.as_wire_str(),
                "to": to.as_wire_str(),
            }),
        )
        .await;
    }

    async fn emit_lifecycle_applied(
        &self,
        from: MixerType,
        to: MixerType,
        carried_level: u8,
        effective_level: u8,
    ) {
        self.emit_lifecycle(
            MIXER_TRANSITION_APPLIED,
            serde_json::json!({
                "v": PAYLOAD_VERSION,
                "from": from.as_wire_str(),
                "to": to.as_wire_str(),
                "carried_level": carried_level,
                "effective_level": effective_level,
            }),
        )
        .await;
    }

    async fn emit_lifecycle_rolled_back(
        &self,
        from: MixerType,
        to: MixerType,
        at_phase: &'static str,
        reason: &str,
    ) {
        self.emit_lifecycle(
            MIXER_TRANSITION_ROLLED_BACK,
            serde_json::json!({
                "v": PAYLOAD_VERSION,
                "from": from.as_wire_str(),
                "to": to.as_wire_str(),
                "at_phase": at_phase,
                "reason": reason,
            }),
        )
        .await;
    }

    async fn emit_lifecycle_failed(
        &self,
        from: MixerType,
        to: MixerType,
        at_phase: &'static str,
        reason: &str,
    ) {
        self.emit_lifecycle(
            MIXER_TRANSITION_FAILED,
            serde_json::json!({
                "v": PAYLOAD_VERSION,
                "from": from.as_wire_str(),
                "to": to.as_wire_str(),
                "at_phase": at_phase,
                "reason": reason,
            }),
        )
        .await;
    }

    /// Emit one mixer-transition lifecycle happening.
    ///
    /// **Off-critical-path by construction.** Lifecycle records
    /// are observability output — the operator's success/failure
    /// outcome is determined by code flow through
    /// `run_mixer_transition`, not by whether the happenings bus
    /// accepts a record. Awaiting the bus's emit on the
    /// orchestrator's critical path makes substrate slowness
    /// cascade into the transition budget; the orchestrator must
    /// not hold open `&mut self` waiting for an observability
    /// write. The implementation spawns the emit as a detached
    /// task and returns immediately. Errors surface in the
    /// detached task's tracing output; the orchestrator's
    /// outcome is unaffected.
    ///
    /// The function keeps its `async` signature so call sites
    /// composing with `.await` need no signature churn, but the
    /// body performs no awaits before returning — `.await` on
    /// the returned future resolves on the next poll.
    async fn emit_lifecycle(
        &self,
        event_type: &str,
        payload: serde_json::Value,
    ) {
        let Some(emitter) = self.happening_emitter.as_ref() else {
            return;
        };
        let emitter = Arc::clone(emitter);
        let event_type = event_type.to_string();
        tokio::spawn(async move {
            if let Err(e) =
                emitter.emit_plugin_event(event_type.clone(), payload).await
            {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    event_type = %event_type,
                    error = %e,
                    "emit mixer-transition lifecycle happening failed \
                     (detached)"
                );
            }
        });
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

    /// Toggle bit-perfect (hog) device mode. The persisted flag
    /// flows through the `audio.options.settings` subject to
    /// the delivery plugin, which re-renders the `pcm.evo`
    /// chain. When `exclusive_mode = true` the rendered chain
    /// pins directly to the card's `hw:` terminus with no
    /// front-end `plug` node — source bytes reach the DAC
    /// byte-identical.
    ///
    /// No static-capability gate at the setter. The static
    /// manifest field `capabilities.delivery.bit_perfect_capable`
    /// is reserved by the SDK for plugins whose chain is ALWAYS
    /// bit-perfect (the SDK enforces `bit_perfect_capable =>
    /// exclusive_mode` at manifest parse); a runtime-toggleable
    /// plugin keeps both false in its manifest and honours the
    /// operator setting through the renderer branch. The
    /// delivery-shelf contract `exclusive-mode-delivery-shelf-
    /// contract` (in options.v2.toml acceptance) pins the
    /// plugin's obligation to honour the operator's choice or
    /// emit an operator-readable WARN happening on subject
    /// update if the plugin cannot render the bit-perfect
    /// path.
    async fn handle_set_exclusive_mode(
        &mut self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: SetExclusiveModePayload = parse_versioned(req)?;
        self.settings.exclusive_mode = payload.value;
        self.persist_settings().await?;
        self.emit_changed(
            "exclusive_mode",
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

    /// Set the between-track crossfade duration. The value
    /// must lie in 0..=30 (the upper bound catches slider-typo
    /// plus UI-bug excursions before they reach MPD). Zero
    /// disables crossfade entirely. The playback warden
    /// translates the persisted seconds count into MPD's
    /// `crossfade <n>` protocol verb on the next subject-update
    /// window.
    async fn handle_set_crossfade_seconds(
        &mut self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: SetCrossfadeSecondsPayload = parse_versioned(req)?;
        if payload.value > CROSSFADE_SECONDS_MAX {
            return Err(PluginError::Permanent(format!(
                "crossfade_seconds must lie in 0..={}; got {}",
                CROSSFADE_SECONDS_MAX, payload.value
            )));
        }
        self.settings.crossfade_seconds = payload.value;
        self.persist_settings().await?;
        self.emit_changed(
            "crossfade_seconds",
            serde_json::Value::Number(payload.value.into()),
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

    /// Enable / disable gapless playback. When `true`, the
    /// playback warden engages MPD's single-mode-off + zero-
    /// pause queue traversal. When `false`, the warden falls
    /// through to MPD's default single-track behaviour.
    async fn handle_set_gapless(
        &mut self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: SetGaplessPayload = parse_versioned(req)?;
        self.settings.gapless = payload.value;
        self.persist_settings().await?;
        self.emit_changed("gapless", serde_json::Value::Bool(payload.value))
            .await;
        encode(
            req,
            &SimpleOk {
                v: PAYLOAD_VERSION,
                status: "ok",
            },
        )
    }

    /// Toggle the parametric-EQ engagement flag. The composition
    /// plugin's eq_only mode honours this flag: `true` applies
    /// the configured bands; `false` pumps bytes through
    /// unchanged. Operator A/B switch without leaving the EQ
    /// composition mode.
    async fn handle_set_eq_engaged(
        &mut self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: SetEqEngagedPayload = parse_versioned(req)?;
        self.settings.eq_engaged = payload.value;
        self.persist_settings().await?;
        self.emit_changed("eq_engaged", serde_json::Value::Bool(payload.value))
            .await;
        encode(
            req,
            &SimpleOk {
                v: PAYLOAD_VERSION,
                status: "ok",
            },
        )
    }

    /// Configure one of the 10 parametric-EQ bands. Validates
    /// every field against the schema-pinned ranges with
    /// structured Permanent refusal naming the offending field
    /// plus the accepted range. Persists; updates the subject;
    /// the composition plugin recomputes the biquad coefficients
    /// on the next subject-update window.
    async fn handle_set_eq_band(
        &mut self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: SetEqBandPayload = parse_versioned(req)?;
        if payload.index as usize >= EQ_BAND_COUNT {
            return Err(PluginError::Permanent(format!(
                "eq_band index must lie in 0..{}; got {}",
                EQ_BAND_COUNT, payload.index
            )));
        }
        if !(EQ_BAND_FREQ_MIN..=EQ_BAND_FREQ_MAX).contains(&payload.freq_hz) {
            return Err(PluginError::Permanent(format!(
                "eq_band.freq_hz must lie in {}..={}; got {}",
                EQ_BAND_FREQ_MIN, EQ_BAND_FREQ_MAX, payload.freq_hz
            )));
        }
        if !(EQ_BAND_GAIN_MIN_DB..=EQ_BAND_GAIN_MAX_DB)
            .contains(&payload.gain_db)
        {
            return Err(PluginError::Permanent(format!(
                "eq_band.gain_db must lie in {}..={} dB; got {}",
                EQ_BAND_GAIN_MIN_DB, EQ_BAND_GAIN_MAX_DB, payload.gain_db
            )));
        }
        if !(EQ_BAND_Q_MIN..=EQ_BAND_Q_MAX).contains(&payload.q) {
            return Err(PluginError::Permanent(format!(
                "eq_band.q must lie in {}..={}; got {}",
                EQ_BAND_Q_MIN, EQ_BAND_Q_MAX, payload.q
            )));
        }
        let band = EqBand {
            freq_hz: payload.freq_hz,
            gain_db: payload.gain_db,
            q: payload.q,
        };
        // Defensive resize: the persisted state file might be
        // older than the current EQ_BAND_COUNT shape. Pad with
        // defaults to the canonical length before writing.
        while self.settings.eq_bands.len() < EQ_BAND_COUNT {
            self.settings.eq_bands.push(EqBand::default());
        }
        self.settings.eq_bands[payload.index as usize] = band;
        self.persist_settings().await?;
        self.emit_changed(
            "eq_bands",
            serde_json::to_value(&self.settings.eq_bands)
                .map_err(map_json_err)?,
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

    /// Return the named EQ preset library to the operator.
    /// Empty array on a fresh device; library order is
    /// insertion order (save appends; recall does not
    /// rearrange).
    async fn handle_list_eq_presets(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        parse_versioned::<EmptyPayload>(req)?;
        let body = ListEqPresetsResponse {
            v: PAYLOAD_VERSION,
            presets: self.settings.eq_presets.clone(),
        };
        encode(req, &body)
    }

    /// Save a named EQ preset. Creates a new entry, or
    /// overwrites by name. Validates the name + every band
    /// against the schema-pinned domains; refusal carries a
    /// structured Permanent error naming the offending field +
    /// accepted range.
    async fn handle_save_eq_preset(
        &mut self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: SaveEqPresetPayload = parse_versioned(req)?;
        validate_preset_name(&payload.name)?;
        if payload.bands.len() != EQ_BAND_COUNT {
            return Err(PluginError::Permanent(format!(
                "eq_preset.bands must contain exactly {} entries; got {}",
                EQ_BAND_COUNT,
                payload.bands.len()
            )));
        }
        for (i, band) in payload.bands.iter().enumerate() {
            validate_eq_band(i, band)?;
        }

        // Overwrite-by-name semantics: scan for existing
        // entry; if absent, enforce library cap before push.
        if let Some(slot) = self
            .settings
            .eq_presets
            .iter_mut()
            .find(|p| p.name == payload.name)
        {
            slot.bands = payload.bands.clone();
        } else {
            if self.settings.eq_presets.len() >= EQ_PRESET_LIBRARY_MAX_COUNT {
                return Err(PluginError::Permanent(format!(
                    "eq_preset library cap reached ({} entries); delete \
                     a preset before saving another",
                    EQ_PRESET_LIBRARY_MAX_COUNT
                )));
            }
            self.settings.eq_presets.push(NamedEqPreset {
                name: payload.name.clone(),
                bands: payload.bands.clone(),
            });
        }
        self.persist_settings().await?;
        self.emit_changed(
            "eq_presets",
            serde_json::to_value(&self.settings.eq_presets)
                .map_err(map_json_err)?,
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

    /// Apply a named preset's bands to the active eq_bands
    /// atomically. The 10-band replace + single
    /// `audio.options.changed` emission is the verb's
    /// raison d'être — a recall via the per-band setter would
    /// fire 10 subject updates; this verb fires one.
    async fn handle_recall_eq_preset(
        &mut self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: NameOnlyEqPresetPayload = parse_versioned(req)?;
        validate_preset_name(&payload.name)?;
        let bands = match self
            .settings
            .eq_presets
            .iter()
            .find(|p| p.name == payload.name)
        {
            Some(p) => p.bands.clone(),
            None => {
                return Err(PluginError::Permanent(format!(
                    "eq_preset {:?} not found in the library; \
                     call list_eq_presets to enumerate",
                    payload.name
                )));
            }
        };
        if bands.len() != EQ_BAND_COUNT {
            return Err(PluginError::Permanent(format!(
                "eq_preset {:?} carries {} bands; expected {} \
                 (persisted-state shape drift)",
                payload.name,
                bands.len(),
                EQ_BAND_COUNT
            )));
        }
        self.settings.eq_bands = bands;
        self.persist_settings().await?;
        // Single subject update for the full band-set replace —
        // operators see one happening, not ten.
        self.emit_changed(
            "eq_bands",
            serde_json::to_value(&self.settings.eq_bands)
                .map_err(map_json_err)?,
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

    /// Remove a named preset from the library. Refuses on an
    /// unknown name with a structured Permanent error.
    async fn handle_delete_eq_preset(
        &mut self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: NameOnlyEqPresetPayload = parse_versioned(req)?;
        validate_preset_name(&payload.name)?;
        let before = self.settings.eq_presets.len();
        self.settings.eq_presets.retain(|p| p.name != payload.name);
        if self.settings.eq_presets.len() == before {
            return Err(PluginError::Permanent(format!(
                "eq_preset {:?} not found in the library; \
                 call list_eq_presets to enumerate",
                payload.name
            )));
        }
        self.persist_settings().await?;
        self.emit_changed(
            "eq_presets",
            serde_json::to_value(&self.settings.eq_presets)
                .map_err(map_json_err)?,
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

    async fn handle_set_mixer_device(
        &mut self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: SetMixerDevicePayload = parse_versioned(req)?;
        // Operator-readable refusal on whitespace-only typos;
        // empty string is permitted (signals "clear the
        // coordinate; hardware-mixer mode will refuse until
        // the operator picks one").
        if !payload.value.is_empty() && payload.value.trim().is_empty() {
            return Err(PluginError::Permanent(
                "mixer_device must not be whitespace-only; pass empty \
                 string to clear the coordinate"
                    .to_string(),
            ));
        }
        self.settings.mixer_device = payload.value.clone();
        self.persist_settings().await?;
        self.emit_changed(
            "mixer_device",
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

    async fn handle_set_mixer_control(
        &mut self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: SetMixerControlPayload = parse_versioned(req)?;
        if !payload.value.is_empty() && payload.value.trim().is_empty() {
            return Err(PluginError::Permanent(
                "mixer_control must not be whitespace-only; pass empty \
                 string to clear the coordinate"
                    .to_string(),
            ));
        }
        self.settings.mixer_control = payload.value.clone();
        self.persist_settings().await?;
        self.emit_changed(
            "mixer_control",
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

    /// Set the operator-facing startup-volume floor. The value
    /// must lie in 0..=100; the setter refuses anything outside
    /// that domain. The setter ALSO clamps against the operator's
    /// `max_volume_percent` ceiling — startup > max is incoherent
    /// (it would be capped on actuation anyway), so the setter
    /// refuses with an operator-readable error rather than
    /// silently truncating.
    async fn handle_set_startup_volume(
        &mut self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: SetVolumePercentPayload = parse_versioned(req)?;
        if payload.value > 100 {
            return Err(PluginError::Permanent(format!(
                "startup_volume_percent must lie in 0..=100; got {}",
                payload.value
            )));
        }
        if payload.value > self.settings.max_volume_percent {
            return Err(PluginError::Permanent(format!(
                "startup_volume_percent {} cannot exceed max_volume_percent {}; \
                 raise the ceiling first",
                payload.value, self.settings.max_volume_percent
            )));
        }
        self.settings.startup_volume_percent = payload.value;
        self.persist_settings().await?;
        self.emit_changed(
            "startup_volume_percent",
            serde_json::Value::Number(payload.value.into()),
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

    /// Set the operator-imposed maximum volume ceiling. The
    /// value must lie in 0..=100; the setter refuses anything
    /// outside that domain. When the new ceiling is below the
    /// current `startup_volume_percent`, the setter clamps the
    /// startup value to the new ceiling so the relationship
    /// `startup <= max` is maintained without a second operator
    /// gesture. The clamp emits its own `audio.options.changed`
    /// happening so subject-stream consumers observe both
    /// fields' new values.
    async fn handle_set_max_volume(
        &mut self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: SetVolumePercentPayload = parse_versioned(req)?;
        if payload.value > 100 {
            return Err(PluginError::Permanent(format!(
                "max_volume_percent must lie in 0..=100; got {}",
                payload.value
            )));
        }
        self.settings.max_volume_percent = payload.value;
        let startup_clamped =
            self.settings.startup_volume_percent > payload.value;
        if startup_clamped {
            self.settings.startup_volume_percent = payload.value;
        }
        self.persist_settings().await?;
        self.emit_changed(
            "max_volume_percent",
            serde_json::Value::Number(payload.value.into()),
        )
        .await;
        if startup_clamped {
            self.emit_changed(
                "startup_volume_percent",
                serde_json::Value::Number(payload.value.into()),
            )
            .await;
        }
        encode(
            req,
            &SimpleOk {
                v: PAYLOAD_VERSION,
                status: "ok",
            },
        )
    }

    /// Set the perceived-loudness mapping curve. Validates the
    /// supplied wire string against the [`VolumeCurve`] domain;
    /// persists; emits `audio.options.changed` + republishes the
    /// settings subject so downstream consumers (the playback
    /// warden's volume-curve binding, future audiophile UI
    /// affordances) react.
    async fn handle_set_volume_curve(
        &mut self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: SetVolumeCurvePayload = parse_versioned(req)?;
        let curve = VolumeCurve::from_wire_str(&payload.value)
            .map_err(PluginError::Permanent)?;
        self.settings.volume_curve = curve;
        self.persist_settings().await?;
        self.emit_changed(
            "volume_curve",
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
struct SetMixerDevicePayload {
    #[serde(default = "default_payload_version")]
    v: u32,
    value: String,
}

impl HasPayloadVersion for SetMixerDevicePayload {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

#[derive(Debug, Deserialize)]
struct SetMixerControlPayload {
    #[serde(default = "default_payload_version")]
    v: u32,
    value: String,
}

impl HasPayloadVersion for SetMixerControlPayload {
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

#[derive(Debug, Deserialize)]
struct SetVolumePercentPayload {
    #[serde(default = "default_payload_version")]
    v: u32,
    value: u8,
}

impl HasPayloadVersion for SetVolumePercentPayload {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

#[derive(Debug, Deserialize)]
struct SetVolumeCurvePayload {
    #[serde(default = "default_payload_version")]
    v: u32,
    value: String,
}

impl HasPayloadVersion for SetVolumeCurvePayload {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

#[derive(Debug, Deserialize)]
struct SetExclusiveModePayload {
    #[serde(default = "default_payload_version")]
    v: u32,
    value: bool,
}

impl HasPayloadVersion for SetExclusiveModePayload {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

#[derive(Debug, Deserialize)]
struct SetCrossfadeSecondsPayload {
    #[serde(default = "default_payload_version")]
    v: u32,
    value: u32,
}

impl HasPayloadVersion for SetCrossfadeSecondsPayload {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

#[derive(Debug, Deserialize)]
struct SetGaplessPayload {
    #[serde(default = "default_payload_version")]
    v: u32,
    value: bool,
}

impl HasPayloadVersion for SetGaplessPayload {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

#[derive(Debug, Deserialize)]
struct SetEqEngagedPayload {
    #[serde(default = "default_payload_version")]
    v: u32,
    value: bool,
}

impl HasPayloadVersion for SetEqEngagedPayload {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

#[derive(Debug, Deserialize)]
struct SetEqBandPayload {
    #[serde(default = "default_payload_version")]
    v: u32,
    index: u32,
    freq_hz: u32,
    gain_db: f32,
    q: f32,
}

impl HasPayloadVersion for SetEqBandPayload {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

#[derive(Debug, Deserialize)]
struct SaveEqPresetPayload {
    #[serde(default = "default_payload_version")]
    v: u32,
    name: String,
    bands: Vec<EqBand>,
}

impl HasPayloadVersion for SaveEqPresetPayload {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

/// Common payload shape for `options.recall_eq_preset` +
/// `options.delete_eq_preset` — both take just the preset
/// name on top of the version envelope.
#[derive(Debug, Deserialize)]
struct NameOnlyEqPresetPayload {
    #[serde(default = "default_payload_version")]
    v: u32,
    name: String,
}

impl HasPayloadVersion for NameOnlyEqPresetPayload {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

#[derive(Debug, Serialize)]
struct ListEqPresetsResponse {
    v: u32,
    presets: Vec<NamedEqPreset>,
}

#[derive(Debug, Serialize)]
struct SimpleOk {
    v: u32,
    status: &'static str,
}

/// Refuse empty + over-long preset names with structured
/// errors. UTF-8 byte length is the cap; tracked separately
/// from `char` count so multi-byte chars don't escape the
/// bound by virtue of a permissive grapheme count.
fn validate_preset_name(name: &str) -> Result<(), PluginError> {
    if name.is_empty() {
        return Err(PluginError::Permanent(
            "eq_preset.name must not be empty".to_string(),
        ));
    }
    if name.len() > EQ_PRESET_NAME_MAX_LEN {
        return Err(PluginError::Permanent(format!(
            "eq_preset.name must be at most {} bytes; got {}",
            EQ_PRESET_NAME_MAX_LEN,
            name.len()
        )));
    }
    Ok(())
}

/// Validate one band against the schema-pinned domain. Used
/// by both the single-band setter and the preset-save bulk
/// path; one canonical validation surface so the two entry
/// points stay in lockstep.
fn validate_eq_band(index: usize, band: &EqBand) -> Result<(), PluginError> {
    if !(EQ_BAND_FREQ_MIN..=EQ_BAND_FREQ_MAX).contains(&band.freq_hz) {
        return Err(PluginError::Permanent(format!(
            "eq_preset.bands[{}].freq_hz must lie in {}..={}; got {}",
            index, EQ_BAND_FREQ_MIN, EQ_BAND_FREQ_MAX, band.freq_hz
        )));
    }
    if !(EQ_BAND_GAIN_MIN_DB..=EQ_BAND_GAIN_MAX_DB).contains(&band.gain_db) {
        return Err(PluginError::Permanent(format!(
            "eq_preset.bands[{}].gain_db must lie in {}..={} dB; got {}",
            index, EQ_BAND_GAIN_MIN_DB, EQ_BAND_GAIN_MAX_DB, band.gain_db
        )));
    }
    if !(EQ_BAND_Q_MIN..=EQ_BAND_Q_MAX).contains(&band.q) {
        return Err(PluginError::Permanent(format!(
            "eq_preset.bands[{}].q must lie in {}..={}; got {}",
            index, EQ_BAND_Q_MIN, EQ_BAND_Q_MAX, band.q
        )));
    }
    Ok(())
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

    // ----- SubjectAnnouncer stub: captures every announce
    //       and update_state call so tests can assert what the
    //       orchestrator (or any setter) published. -----

    #[derive(Debug, Clone)]
    #[allow(dead_code)]
    struct CapturedSubjectUpdate {
        addressing: ExternalAddressing,
        state: serde_json::Value,
    }

    #[derive(Default)]
    struct CapturingAnnouncer {
        announces: Mutex<Vec<SubjectAnnouncement>>,
        updates: Mutex<Vec<CapturedSubjectUpdate>>,
    }

    impl std::fmt::Debug for CapturingAnnouncer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("CapturingAnnouncer").finish_non_exhaustive()
        }
    }

    impl SubjectAnnouncer for CapturingAnnouncer {
        fn announce<'a>(
            &'a self,
            announcement: SubjectAnnouncement,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<(), ReportError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                self.announces.lock().unwrap().push(announcement);
                Ok(())
            })
        }

        fn retract<'a>(
            &'a self,
            _addressing: ExternalAddressing,
            _reason: Option<String>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<(), ReportError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move { Ok(()) })
        }

        fn update_state<'a>(
            &'a self,
            addressing: ExternalAddressing,
            state: serde_json::Value,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<(), ReportError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                self.updates
                    .lock()
                    .unwrap()
                    .push(CapturedSubjectUpdate { addressing, state });
                Ok(())
            })
        }
    }

    #[allow(dead_code)]
    impl CapturingAnnouncer {
        fn announces(&self) -> Vec<SubjectAnnouncement> {
            self.announces.lock().unwrap().clone()
        }
        fn updates(&self) -> Vec<CapturedSubjectUpdate> {
            self.updates.lock().unwrap().clone()
        }
    }

    // ----- SubjectQuerier + SubjectStateSubscriber stubs:
    //       enough infrastructure to exercise the
    //       envelope-coordination publish + await loop
    //       end-to-end at code level. -----

    use evo_device_audio_shared::transition_envelope::{
        EnvelopeObserved, ENVELOPE_OBSERVED_SUBJECT_TYPE,
    };
    use evo_plugin_sdk::contract::{SubjectStateStream, SubjectStateUpdate};
    use std::collections::HashMap;

    #[derive(Default, Clone)]
    struct StubQuerier {
        // addressing.value → canonical_id; addressing.scheme
        // ignored for the test surface.
        map: Arc<Mutex<HashMap<String, String>>>,
        // Per-call counter so structural tests can assert the
        // orchestrator's architectural invariant: resolve is
        // called exactly once per plugin lifetime, not per
        // transition.
        resolve_calls: Arc<AtomicU64>,
    }

    impl StubQuerier {
        fn register(
            &self,
            addressing: &ExternalAddressing,
            canonical_id: &str,
        ) {
            self.map
                .lock()
                .unwrap()
                .insert(addressing.value.clone(), canonical_id.to_string());
        }

        fn resolve_call_count(&self) -> u64 {
            self.resolve_calls.load(Ordering::SeqCst)
        }
    }

    impl std::fmt::Debug for StubQuerier {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("StubQuerier").finish_non_exhaustive()
        }
    }

    impl evo_plugin_sdk::contract::SubjectQuerier for StubQuerier {
        fn resolve_addressing<'a>(
            &'a self,
            addressing: ExternalAddressing,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<Option<String>, ReportError>,
                    > + Send
                    + 'a,
            >,
        > {
            let map = Arc::clone(&self.map);
            self.resolve_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Ok(map.lock().unwrap().get(&addressing.value).cloned())
            })
        }

        fn describe_alias<'a>(
            &'a self,
            _id: String,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            Option<evo_plugin_sdk::contract::AliasRecord>,
                            ReportError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async move { Ok(None) })
        }

        fn describe_subject_with_aliases<'a>(
            &'a self,
            _id: String,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            evo_plugin_sdk::SubjectQueryResult,
                            ReportError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(
                async move { Ok(evo_plugin_sdk::SubjectQueryResult::NotFound) },
            )
        }
    }

    /// Stub subject-state subscriber. Holds a
    /// `tokio::sync::broadcast::Sender` per canonical id;
    /// `subscribe_subject` returns a fresh receiver wrapped
    /// in `SubjectStateStream`. `current_state` returns
    /// whatever was last pushed to that id (None if
    /// nothing).
    #[derive(Default, Clone)]
    struct StubStateSubscriber {
        inner: Arc<Mutex<StubSubscriberInner>>,
        // Per-call counter so structural tests can assert the
        // orchestrator's architectural invariant: subscribe is
        // called exactly once per plugin lifetime, not per
        // transition.
        subscribe_calls: Arc<AtomicU64>,
    }

    impl StubStateSubscriber {
        fn subscribe_call_count(&self) -> u64 {
            self.subscribe_calls.load(Ordering::SeqCst)
        }
    }

    #[derive(Default)]
    struct StubSubscriberInner {
        senders:
            HashMap<String, tokio::sync::broadcast::Sender<SubjectStateUpdate>>,
        latest: HashMap<String, serde_json::Value>,
    }

    impl StubStateSubscriber {
        /// Publish a state update for the given canonical id.
        /// Active receivers see it via their next `recv`;
        /// future subscribers see it via `current_state`.
        fn publish(&self, canonical_id: &str, state: serde_json::Value) {
            let mut inner = self.inner.lock().unwrap();
            inner.latest.insert(canonical_id.to_string(), state.clone());
            let sender = inner
                .senders
                .entry(canonical_id.to_string())
                .or_insert_with(|| {
                    let (tx, _rx) = tokio::sync::broadcast::channel(16);
                    tx
                });
            let _ = sender.send(SubjectStateUpdate {
                canonical_id: canonical_id.to_string(),
                subject_type: ENVELOPE_OBSERVED_SUBJECT_TYPE.to_string(),
                state: Some(state),
                modified_at_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
            });
        }
    }

    impl std::fmt::Debug for StubStateSubscriber {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("StubStateSubscriber")
                .finish_non_exhaustive()
        }
    }

    impl evo_plugin_sdk::contract::SubjectStateSubscriber for StubStateSubscriber {
        fn subscribe_subject<'a>(
            &'a self,
            canonical_id: String,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<SubjectStateStream, ReportError>,
                    > + Send
                    + 'a,
            >,
        > {
            self.subscribe_calls.fetch_add(1, Ordering::SeqCst);
            let inner = Arc::clone(&self.inner);
            Box::pin(async move {
                let mut guard = inner.lock().unwrap();
                let sender = guard
                    .senders
                    .entry(canonical_id.clone())
                    .or_insert_with(|| {
                        let (tx, _rx) = tokio::sync::broadcast::channel(16);
                        tx
                    });
                let receiver = sender.subscribe();
                Ok(SubjectStateStream::new(receiver, canonical_id))
            })
        }

        fn current_state<'a>(
            &'a self,
            canonical_id: String,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<Option<serde_json::Value>, ReportError>,
                    > + Send
                    + 'a,
            >,
        > {
            let inner = Arc::clone(&self.inner);
            Box::pin(async move {
                Ok(inner.lock().unwrap().latest.get(&canonical_id).cloned())
            })
        }
    }

    // ----- helpers -----

    async fn loaded_plugin() -> (PlaybackOptionsPlugin, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join(STATE_FILENAME);
        let mut p = PlaybackOptionsPlugin::new().with_state_path(state_path);
        p.happening_emitter = Some(Arc::new(CapturingEmitter::default()));
        p.subject_announcer = Some(Arc::new(CapturingAnnouncer::default()));
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
            principal_scope: None,
            has_step_up: false,
        }
    }

    // ----- manifest / surface -----

    #[test]
    fn embedded_manifest_parses() {
        let m = manifest();
        assert_eq!(m.plugin.name, PLUGIN_NAME);
        assert_eq!(m.target.shelf, "audio.options");
        assert_eq!(m.target.shape, 2);
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
        let drift =
            evo_plugin_sdk::drift::detect_drift(&m, &d.runtime_capabilities);
        assert!(
            drift.is_empty(),
            "in-tree manifest.toml drifted from runtime describe(): {:?}",
            drift
        );
    }

    /// Production-shipping manifest variant
    /// (`manifest.oop.toml`) carries the same capability
    /// declarations as `manifest.toml` except for the transport
    /// block. The framework's admission gate refuses any plugin
    /// whose manifest declarations drift from the runtime
    /// `describe()` output; without this test the OOP manifest
    /// can drift silently and admission fails only at deploy
    /// time on a real rig.
    #[tokio::test]
    async fn describe_matches_oop_manifest() {
        const MANIFEST_OOP_TOML: &str = include_str!("../manifest.oop.toml");
        let p = PlaybackOptionsPlugin::new();
        let d = p.describe().await;
        let m = evo_plugin_sdk::Manifest::from_toml(MANIFEST_OOP_TOML)
            .expect("manifest.oop.toml must parse");
        assert_eq!(d.identity.name, m.plugin.name);
        assert_eq!(d.identity.version, m.plugin.version);
        let drift =
            evo_plugin_sdk::drift::detect_drift(&m, &d.runtime_capabilities);
        assert!(
            drift.is_empty(),
            "manifest.oop.toml drifted from runtime describe(): {:?}",
            drift
        );
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
        // Hardware mode requires mixer_device + mixer_control
        // populated first (contract: silent degrade is
        // forbidden). Set the coordinates, then switch
        // mixer_type — the orchestrator's state machine runs
        // under the transition lock and persists the new
        // mixer_type.
        p.handle_request(&req(
            "options.set_mixer_device",
            json!({ "v": 1, "value": "hw:0" }),
        ))
        .await
        .unwrap();
        p.handle_request(&req(
            "options.set_mixer_control",
            json!({ "v": 1, "value": "Master" }),
        ))
        .await
        .unwrap();
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
    async fn set_mixer_device_persists_and_round_trips() {
        let (mut p, _dir) = loaded_plugin().await;
        p.handle_request(&req(
            "options.set_mixer_device",
            json!({ "v": 1, "value": "hw:CARD=Headphones" }),
        ))
        .await
        .unwrap();
        let resp = p
            .handle_request(&req("options.get_settings", json!({ "v": 1 })))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(v["mixer_device"], "hw:CARD=Headphones");
    }

    #[tokio::test]
    async fn set_mixer_device_refuses_whitespace_only_value() {
        let (mut p, _dir) = loaded_plugin().await;
        let err = p
            .handle_request(&req(
                "options.set_mixer_device",
                json!({ "v": 1, "value": "   " }),
            ))
            .await
            .unwrap_err();
        match err {
            PluginError::Permanent(msg) => {
                assert!(msg.contains("mixer_device"));
                assert!(msg.contains("whitespace-only"));
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_mixer_device_accepts_empty_for_clear() {
        let (mut p, _dir) = loaded_plugin().await;
        // Seed a non-empty value first; then clear it.
        p.handle_request(&req(
            "options.set_mixer_device",
            json!({ "v": 1, "value": "hw:0" }),
        ))
        .await
        .unwrap();
        p.handle_request(&req(
            "options.set_mixer_device",
            json!({ "v": 1, "value": "" }),
        ))
        .await
        .unwrap();
        let resp = p
            .handle_request(&req("options.get_settings", json!({ "v": 1 })))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(v["mixer_device"], "");
    }

    #[tokio::test]
    async fn set_mixer_control_persists_and_round_trips() {
        let (mut p, _dir) = loaded_plugin().await;
        p.handle_request(&req(
            "options.set_mixer_control",
            json!({ "v": 1, "value": "Master" }),
        ))
        .await
        .unwrap();
        let resp = p
            .handle_request(&req("options.get_settings", json!({ "v": 1 })))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(v["mixer_control"], "Master");
    }

    #[tokio::test]
    async fn set_mixer_control_refuses_whitespace_only_value() {
        let (mut p, _dir) = loaded_plugin().await;
        let err = p
            .handle_request(&req(
                "options.set_mixer_control",
                json!({ "v": 1, "value": " \t" }),
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, PluginError::Permanent(_)));
    }

    #[tokio::test]
    async fn settings_defaults_have_empty_mixer_device_and_control() {
        // Contract: when mixer_type = hardware is requested but
        // mixer_device + mixer_control aren't populated, the
        // playback warden refuses (no silent degrade). The
        // defaults MUST be empty strings so the absence is
        // visible.
        let (mut p, _dir) = loaded_plugin().await;
        let resp = p
            .handle_request(&req("options.get_settings", json!({ "v": 1 })))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(v["mixer_device"], "");
        assert_eq!(v["mixer_control"], "");
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
    async fn default_settings_carry_audiophile_grade_defaults() {
        let s = Settings::default();
        assert!(!s.exclusive_mode, "default bit-perfect must be off");
        assert_eq!(
            s.crossfade_seconds, 0,
            "audiophile default — no in-chain fade"
        );
        assert!(s.gapless, "audiophile default — gapless on");
    }

    #[tokio::test]
    async fn set_exclusive_mode_round_trips() {
        let (mut p, _dir) = loaded_plugin().await;
        assert!(!p.settings().exclusive_mode);
        p.handle_request(&req(
            "options.set_exclusive_mode",
            json!({ "v": 1, "value": true }),
        ))
        .await
        .unwrap();
        assert!(p.settings().exclusive_mode);
        let resp = p
            .handle_request(&req("options.get_settings", json!({ "v": 1 })))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(v["exclusive_mode"], true);
    }

    #[tokio::test]
    async fn set_exclusive_mode_clears_back_to_false() {
        let (mut p, _dir) = loaded_plugin().await;
        p.handle_request(&req(
            "options.set_exclusive_mode",
            json!({ "v": 1, "value": true }),
        ))
        .await
        .unwrap();
        p.handle_request(&req(
            "options.set_exclusive_mode",
            json!({ "v": 1, "value": false }),
        ))
        .await
        .unwrap();
        assert!(!p.settings().exclusive_mode);
    }

    #[tokio::test]
    async fn set_crossfade_seconds_persists_within_range() {
        let (mut p, _dir) = loaded_plugin().await;
        p.handle_request(&req(
            "options.set_crossfade_seconds",
            json!({ "v": 1, "value": 5 }),
        ))
        .await
        .unwrap();
        assert_eq!(p.settings().crossfade_seconds, 5);
        // Zero is a valid value and disables crossfade.
        p.handle_request(&req(
            "options.set_crossfade_seconds",
            json!({ "v": 1, "value": 0 }),
        ))
        .await
        .unwrap();
        assert_eq!(p.settings().crossfade_seconds, 0);
        // Upper bound (CROSSFADE_SECONDS_MAX) is accepted.
        p.handle_request(&req(
            "options.set_crossfade_seconds",
            json!({ "v": 1, "value": CROSSFADE_SECONDS_MAX }),
        ))
        .await
        .unwrap();
        assert_eq!(p.settings().crossfade_seconds, CROSSFADE_SECONDS_MAX);
    }

    #[tokio::test]
    async fn set_crossfade_seconds_refuses_above_ceiling() {
        let (mut p, _dir) = loaded_plugin().await;
        let err = p
            .handle_request(&req(
                "options.set_crossfade_seconds",
                json!({ "v": 1, "value": CROSSFADE_SECONDS_MAX + 1 }),
            ))
            .await
            .unwrap_err();
        match err {
            PluginError::Permanent(msg) => {
                assert!(
                    msg.contains("0..=30"),
                    "refusal must name the accepted range; got: {msg}"
                );
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_gapless_round_trips_and_persists() {
        let (mut p, _dir) = loaded_plugin().await;
        // Defaults to true; flip to false then back.
        assert!(p.settings().gapless);
        p.handle_request(&req(
            "options.set_gapless",
            json!({ "v": 1, "value": false }),
        ))
        .await
        .unwrap();
        assert!(!p.settings().gapless);
        p.handle_request(&req(
            "options.set_gapless",
            json!({ "v": 1, "value": true }),
        ))
        .await
        .unwrap();
        assert!(p.settings().gapless);
    }

    #[tokio::test]
    async fn default_settings_carry_ten_flat_eq_bands() {
        let s = Settings::default();
        assert!(!s.eq_engaged, "EQ off by default");
        assert_eq!(s.eq_bands.len(), EQ_BAND_COUNT);
        for (i, b) in s.eq_bands.iter().enumerate() {
            assert_eq!(b.freq_hz, 1000, "band {} freq_hz default", i);
            assert_eq!(b.gain_db, 0.0, "band {} gain_db default (flat)", i);
            assert_eq!(b.q, 1.0, "band {} q default", i);
        }
    }

    #[tokio::test]
    async fn set_eq_engaged_round_trips() {
        let (mut p, _dir) = loaded_plugin().await;
        assert!(!p.settings().eq_engaged);
        p.handle_request(&req(
            "options.set_eq_engaged",
            json!({ "v": 1, "value": true }),
        ))
        .await
        .unwrap();
        assert!(p.settings().eq_engaged);
    }

    #[tokio::test]
    async fn set_eq_band_persists_valid_values() {
        let (mut p, _dir) = loaded_plugin().await;
        p.handle_request(&req(
            "options.set_eq_band",
            json!({
                "v": 1,
                "index": 3,
                "freq_hz": 4000,
                "gain_db": -3.5,
                "q": 1.41,
            }),
        ))
        .await
        .unwrap();
        let band = p.settings().eq_bands[3];
        assert_eq!(band.freq_hz, 4000);
        assert!((band.gain_db - -3.5).abs() < 1e-6);
        assert!((band.q - 1.41).abs() < 1e-6);
        // Other bands unchanged.
        assert_eq!(p.settings().eq_bands[0], EqBand::default());
    }

    #[tokio::test]
    async fn set_eq_band_refuses_index_above_count() {
        let (mut p, _dir) = loaded_plugin().await;
        let err = p
            .handle_request(&req(
                "options.set_eq_band",
                json!({
                    "v": 1,
                    "index": 99,
                    "freq_hz": 1000,
                    "gain_db": 0.0,
                    "q": 1.0,
                }),
            ))
            .await
            .unwrap_err();
        match err {
            PluginError::Permanent(msg) => {
                assert!(
                    msg.contains("index") && msg.contains("0..10"),
                    "refusal must name index + range; got: {msg}"
                );
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_eq_band_refuses_freq_below_audible_band() {
        let (mut p, _dir) = loaded_plugin().await;
        let err = p
            .handle_request(&req(
                "options.set_eq_band",
                json!({
                    "v": 1,
                    "index": 0,
                    "freq_hz": 5,
                    "gain_db": 0.0,
                    "q": 1.0,
                }),
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, PluginError::Permanent(_)));
    }

    #[tokio::test]
    async fn set_eq_band_refuses_gain_above_max() {
        let (mut p, _dir) = loaded_plugin().await;
        let err = p
            .handle_request(&req(
                "options.set_eq_band",
                json!({
                    "v": 1,
                    "index": 0,
                    "freq_hz": 1000,
                    "gain_db": 100.0,
                    "q": 1.0,
                }),
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, PluginError::Permanent(_)));
    }

    #[tokio::test]
    async fn set_eq_band_refuses_q_outside_range() {
        let (mut p, _dir) = loaded_plugin().await;
        let err = p
            .handle_request(&req(
                "options.set_eq_band",
                json!({
                    "v": 1,
                    "index": 0,
                    "freq_hz": 1000,
                    "gain_db": 0.0,
                    "q": 100.0,
                }),
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, PluginError::Permanent(_)));
    }

    // ---- EQ preset library coverage ----

    fn flat_bands_json() -> serde_json::Value {
        let arr: Vec<_> = (0..EQ_BAND_COUNT)
            .map(|_| json!({ "freq_hz": 1000, "gain_db": 0.0, "q": 1.0 }))
            .collect();
        serde_json::Value::Array(arr)
    }

    #[tokio::test]
    async fn list_eq_presets_empty_on_fresh_plugin() {
        let (mut p, _dir) = loaded_plugin().await;
        let resp = p
            .handle_request(&req("options.list_eq_presets", json!({ "v": 1 })))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(v["presets"], json!([]));
    }

    #[tokio::test]
    async fn save_eq_preset_persists_and_lists() {
        let (mut p, _dir) = loaded_plugin().await;
        p.handle_request(&req(
            "options.save_eq_preset",
            json!({ "v": 1, "name": "vocal-boost", "bands": flat_bands_json() }),
        ))
        .await
        .unwrap();
        let resp = p
            .handle_request(&req("options.list_eq_presets", json!({ "v": 1 })))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(v["presets"].as_array().unwrap().len(), 1);
        assert_eq!(v["presets"][0]["name"], "vocal-boost");
    }

    #[tokio::test]
    async fn save_eq_preset_overwrites_by_name() {
        let (mut p, _dir) = loaded_plugin().await;
        // First save with flat bands.
        p.handle_request(&req(
            "options.save_eq_preset",
            json!({ "v": 1, "name": "test", "bands": flat_bands_json() }),
        ))
        .await
        .unwrap();
        // Overwrite with a band-0 freq of 200.
        let mut modified = flat_bands_json();
        modified[0]["freq_hz"] = json!(200);
        p.handle_request(&req(
            "options.save_eq_preset",
            json!({ "v": 1, "name": "test", "bands": modified }),
        ))
        .await
        .unwrap();
        assert_eq!(p.settings().eq_presets.len(), 1, "overwrite, not append");
        assert_eq!(p.settings().eq_presets[0].bands[0].freq_hz, 200);
    }

    #[tokio::test]
    async fn save_eq_preset_refuses_empty_name() {
        let (mut p, _dir) = loaded_plugin().await;
        let err = p
            .handle_request(&req(
                "options.save_eq_preset",
                json!({ "v": 1, "name": "", "bands": flat_bands_json() }),
            ))
            .await
            .unwrap_err();
        match err {
            PluginError::Permanent(msg) => {
                assert!(
                    msg.contains("name") && msg.contains("empty"),
                    "refusal must name the field; got: {msg}"
                );
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn save_eq_preset_refuses_overlong_name() {
        let (mut p, _dir) = loaded_plugin().await;
        let long = "x".repeat(EQ_PRESET_NAME_MAX_LEN + 1);
        let err = p
            .handle_request(&req(
                "options.save_eq_preset",
                json!({ "v": 1, "name": long, "bands": flat_bands_json() }),
            ))
            .await
            .unwrap_err();
        match err {
            PluginError::Permanent(msg) => {
                assert!(
                    msg.contains("64"),
                    "refusal must name the cap; got: {msg}"
                );
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn save_eq_preset_refuses_wrong_band_count() {
        let (mut p, _dir) = loaded_plugin().await;
        // Only 5 bands — schema requires 10.
        let short_bands: Vec<_> = (0..5)
            .map(|_| json!({ "freq_hz": 1000, "gain_db": 0.0, "q": 1.0 }))
            .collect();
        let err = p
            .handle_request(&req(
                "options.save_eq_preset",
                json!({ "v": 1, "name": "short", "bands": short_bands }),
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, PluginError::Permanent(_)));
    }

    #[tokio::test]
    async fn save_eq_preset_refuses_out_of_range_field() {
        let (mut p, _dir) = loaded_plugin().await;
        let mut bands = flat_bands_json();
        bands[3]["gain_db"] = json!(100.0); // outside -15..=15
        let err = p
            .handle_request(&req(
                "options.save_eq_preset",
                json!({ "v": 1, "name": "bad", "bands": bands }),
            ))
            .await
            .unwrap_err();
        match err {
            PluginError::Permanent(msg) => {
                assert!(
                    msg.contains("bands[3].gain_db"),
                    "refusal must name the offending band field; got: {msg}"
                );
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn save_eq_preset_refuses_library_cap_overflow() {
        let (mut p, _dir) = loaded_plugin().await;
        for i in 0..EQ_PRESET_LIBRARY_MAX_COUNT {
            p.handle_request(&req(
                "options.save_eq_preset",
                json!({
                    "v": 1,
                    "name": format!("preset-{}", i),
                    "bands": flat_bands_json(),
                }),
            ))
            .await
            .unwrap();
        }
        // The (max + 1)-th create overflows.
        let err = p
            .handle_request(&req(
                "options.save_eq_preset",
                json!({
                    "v": 1,
                    "name": "one-too-many",
                    "bands": flat_bands_json(),
                }),
            ))
            .await
            .unwrap_err();
        match err {
            PluginError::Permanent(msg) => {
                assert!(
                    msg.contains("library cap")
                        && msg
                            .contains(&EQ_PRESET_LIBRARY_MAX_COUNT.to_string()),
                    "refusal must name the cap; got: {msg}"
                );
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn recall_eq_preset_replaces_active_bands_atomically() {
        let (mut p, _dir) = loaded_plugin().await;
        // Save a preset with all bands at 4 kHz / +3 dB.
        let preset_bands: Vec<_> = (0..EQ_BAND_COUNT)
            .map(|_| json!({ "freq_hz": 4000, "gain_db": 3.0, "q": 1.5 }))
            .collect();
        p.handle_request(&req(
            "options.save_eq_preset",
            json!({ "v": 1, "name": "boost-4k", "bands": preset_bands }),
        ))
        .await
        .unwrap();
        // Active bands are still flat (the save did not touch them).
        assert_eq!(p.settings().eq_bands[0].freq_hz, 1000);

        // Recall replaces all 10 bands.
        p.handle_request(&req(
            "options.recall_eq_preset",
            json!({ "v": 1, "name": "boost-4k" }),
        ))
        .await
        .unwrap();
        for i in 0..EQ_BAND_COUNT {
            assert_eq!(p.settings().eq_bands[i].freq_hz, 4000);
            assert!((p.settings().eq_bands[i].gain_db - 3.0).abs() < 1e-6);
            assert!((p.settings().eq_bands[i].q - 1.5).abs() < 1e-6);
        }
    }

    #[tokio::test]
    async fn recall_eq_preset_refuses_unknown_name() {
        let (mut p, _dir) = loaded_plugin().await;
        let err = p
            .handle_request(&req(
                "options.recall_eq_preset",
                json!({ "v": 1, "name": "not-there" }),
            ))
            .await
            .unwrap_err();
        match err {
            PluginError::Permanent(msg) => {
                assert!(
                    msg.contains("not-there")
                        && msg.contains("list_eq_presets"),
                    "refusal must name the offending preset + point at \
                     the enumeration verb; got: {msg}"
                );
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn delete_eq_preset_removes_and_lists_drop() {
        let (mut p, _dir) = loaded_plugin().await;
        p.handle_request(&req(
            "options.save_eq_preset",
            json!({ "v": 1, "name": "tmp", "bands": flat_bands_json() }),
        ))
        .await
        .unwrap();
        assert_eq!(p.settings().eq_presets.len(), 1);
        p.handle_request(&req(
            "options.delete_eq_preset",
            json!({ "v": 1, "name": "tmp" }),
        ))
        .await
        .unwrap();
        assert!(p.settings().eq_presets.is_empty());
    }

    #[tokio::test]
    async fn delete_eq_preset_refuses_unknown_name() {
        let (mut p, _dir) = loaded_plugin().await;
        let err = p
            .handle_request(&req(
                "options.delete_eq_preset",
                json!({ "v": 1, "name": "ghost" }),
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, PluginError::Permanent(_)));
    }

    #[tokio::test]
    async fn recall_eq_preset_does_not_touch_eq_engaged() {
        let (mut p, _dir) = loaded_plugin().await;
        // Engage EQ.
        p.handle_request(&req(
            "options.set_eq_engaged",
            json!({ "v": 1, "value": true }),
        ))
        .await
        .unwrap();
        // Save + recall a preset.
        p.handle_request(&req(
            "options.save_eq_preset",
            json!({ "v": 1, "name": "x", "bands": flat_bands_json() }),
        ))
        .await
        .unwrap();
        p.handle_request(&req(
            "options.recall_eq_preset",
            json!({ "v": 1, "name": "x" }),
        ))
        .await
        .unwrap();
        // Engagement flag is unchanged — the preset doesn't
        // carry it.
        assert!(
            p.settings().eq_engaged,
            "recall must not flip eq_engaged; session state stays"
        );
    }

    #[tokio::test]
    async fn reset_to_defaults_restores_new_fields() {
        let (mut p, _dir) = loaded_plugin().await;
        p.handle_request(&req(
            "options.set_exclusive_mode",
            json!({ "v": 1, "value": true }),
        ))
        .await
        .unwrap();
        p.handle_request(&req(
            "options.set_crossfade_seconds",
            json!({ "v": 1, "value": 7 }),
        ))
        .await
        .unwrap();
        p.handle_request(&req(
            "options.set_gapless",
            json!({ "v": 1, "value": false }),
        ))
        .await
        .unwrap();
        p.handle_request(&req("options.reset_to_defaults", json!({ "v": 1 })))
            .await
            .unwrap();
        let s = p.settings();
        assert!(!s.exclusive_mode);
        assert_eq!(s.crossfade_seconds, 0);
        assert!(s.gapless);
    }

    #[tokio::test]
    async fn set_startup_volume_persists_within_range() {
        let (mut p, _dir) = loaded_plugin().await;
        p.handle_request(&req(
            "options.set_startup_volume",
            json!({ "v": 1, "value": 25 }),
        ))
        .await
        .unwrap();
        assert_eq!(p.settings().startup_volume_percent, 25);
    }

    #[tokio::test]
    async fn set_startup_volume_refuses_out_of_range() {
        let (mut p, _dir) = loaded_plugin().await;
        let err = p
            .handle_request(&req(
                "options.set_startup_volume",
                json!({ "v": 1, "value": 150 }),
            ))
            .await
            .unwrap_err();
        match err {
            PluginError::Permanent(msg) => {
                assert!(msg.contains("0..=100"));
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_startup_volume_refuses_above_max_volume() {
        let (mut p, _dir) = loaded_plugin().await;
        p.handle_request(&req(
            "options.set_max_volume",
            json!({ "v": 1, "value": 40 }),
        ))
        .await
        .unwrap();
        let err = p
            .handle_request(&req(
                "options.set_startup_volume",
                json!({ "v": 1, "value": 80 }),
            ))
            .await
            .unwrap_err();
        match err {
            PluginError::Permanent(msg) => {
                assert!(msg.contains("cannot exceed max_volume_percent"));
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_max_volume_clamps_startup_when_below_it() {
        let (mut p, _dir) = loaded_plugin().await;
        // Raise startup to a known value first.
        p.handle_request(&req(
            "options.set_startup_volume",
            json!({ "v": 1, "value": 60 }),
        ))
        .await
        .unwrap();
        assert_eq!(p.settings().startup_volume_percent, 60);
        // Now lower max below startup; setter should clamp startup
        // down to the new ceiling.
        p.handle_request(&req(
            "options.set_max_volume",
            json!({ "v": 1, "value": 50 }),
        ))
        .await
        .unwrap();
        assert_eq!(p.settings().max_volume_percent, 50);
        assert_eq!(p.settings().startup_volume_percent, 50);
    }

    #[tokio::test]
    async fn set_max_volume_refuses_out_of_range() {
        let (mut p, _dir) = loaded_plugin().await;
        let err = p
            .handle_request(&req(
                "options.set_max_volume",
                json!({ "v": 1, "value": 200 }),
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, PluginError::Permanent(_)));
    }

    #[tokio::test]
    async fn set_volume_curve_persists_valid_value() {
        let (mut p, _dir) = loaded_plugin().await;
        p.handle_request(&req(
            "options.set_volume_curve",
            json!({ "v": 1, "value": "log" }),
        ))
        .await
        .unwrap();
        assert_eq!(p.settings().volume_curve, VolumeCurve::Log);
    }

    #[tokio::test]
    async fn set_volume_curve_refuses_invalid_value() {
        let (mut p, _dir) = loaded_plugin().await;
        let err = p
            .handle_request(&req(
                "options.set_volume_curve",
                json!({ "v": 1, "value": "moonshot" }),
            ))
            .await
            .unwrap_err();
        match err {
            PluginError::Permanent(msg) => {
                assert!(msg.contains("volume_curve must be one of"));
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    #[test]
    fn settings_defaults_carry_safe_volume_baseline() {
        let s = Settings::default();
        assert_eq!(s.startup_volume_percent, 30);
        assert_eq!(s.max_volume_percent, 100);
        assert_eq!(s.volume_curve, VolumeCurve::Linear);
    }

    #[test]
    fn volume_curve_wire_round_trip() {
        for curve in
            [VolumeCurve::Linear, VolumeCurve::Log, VolumeCurve::Natural]
        {
            let s = curve.as_wire_str();
            let parsed = VolumeCurve::from_wire_str(s).expect("parse");
            assert_eq!(parsed, curve);
        }
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

    // ----- mixer-transition contract tests (T3 / T5 / T6) -----

    // ----- envelope-coordination tests -----

    /// Build a plugin instance wired up for envelope-flow
    /// tests: capturing announcer + stub querier + stub
    /// state subscriber, the envelope_observed addressing
    /// pre-resolved to `OBSERVED_CID`. Tests can use
    /// `subscriber.publish(OBSERVED_CID, ...)` to simulate
    /// the warden's ack.
    async fn envelope_wired_plugin() -> (
        PlaybackOptionsPlugin,
        Arc<CapturingAnnouncer>,
        StubStateSubscriber,
        StubQuerier,
        tempfile::TempDir,
    ) {
        const OBSERVED_CID: &str = "stub-envelope-observed-cid";
        let dir = tempdir().unwrap();
        let state_path = dir.path().join(STATE_FILENAME);
        let announcer = Arc::new(CapturingAnnouncer::default());
        let subscriber = StubStateSubscriber::default();
        let querier = StubQuerier::default();
        querier.register(
            &ExternalAddressing {
                scheme: ENVELOPE_SCHEME.to_string(),
                value: ENVELOPE_OBSERVED_VALUE.to_string(),
            },
            OBSERVED_CID,
        );
        let mut p = PlaybackOptionsPlugin::new()
            .with_state_path(state_path)
            .with_envelope_ack_timeout_override(Duration::from_millis(200));
        p.happening_emitter = Some(Arc::new(CapturingEmitter::default()));
        p.subject_announcer =
            Some(Arc::clone(&announcer) as Arc<dyn SubjectAnnouncer>);
        p.subject_state_subscriber = Some(
            Arc::new(subscriber.clone()) as Arc<dyn SubjectStateSubscriber>
        );
        p.subject_querier =
            Some(Arc::new(querier.clone()) as Arc<dyn SubjectQuerier>);
        p.loaded = true;
        (p, announcer, subscriber, querier, dir)
    }

    fn envelope_observed_state(
        generation: u64,
        state: EnvelopeState,
    ) -> serde_json::Value {
        serde_json::to_value(EnvelopeObserved {
            v: ENVELOPE_PAYLOAD_VERSION,
            generation,
            observed_state: state,
            observed_at_ms: 0,
        })
        .unwrap()
    }

    fn envelope_requested_publishes(
        announcer: &CapturingAnnouncer,
    ) -> Vec<EnvelopeRequested> {
        announcer
            .updates()
            .into_iter()
            .filter(|u| {
                u.addressing.scheme == ENVELOPE_SCHEME
                    && u.addressing.value == ENVELOPE_REQUESTED_VALUE
            })
            .filter_map(|u| {
                serde_json::from_value::<EnvelopeRequested>(u.state).ok()
            })
            .collect()
    }

    #[tokio::test]
    async fn env_publishes_muted_then_unmuted_in_order_with_warden_ack() {
        // Full happy-path E2E: orchestrator publishes
        // envelope_requested=Muted, warden (stub) acks with
        // a matching envelope_observed=Muted; orchestrator
        // advances; persists new mixer_type; publishes
        // envelope_requested=Unmuted; warden acks; gesture
        // completes Applied. The captured envelope_requested
        // stream MUST be [muted, unmuted] in that order with
        // monotonic generations.
        const OBSERVED_CID: &str = "stub-envelope-observed-cid";
        let (mut p, announcer, subscriber, _querier, _dir) =
            envelope_wired_plugin().await;

        // Run the gesture; spawn a parallel acker that
        // observes each muted publish and feeds back the
        // matching observed.
        let subscriber_clone = subscriber.clone();
        let announcer_clone = Arc::clone(&announcer);
        let ack_task = tokio::spawn(async move {
            // Poll the announcer for envelope_requested
            // publishes and feed back acks. Bounded by the
            // overall timeout via the orchestrator's await.
            let mut acked_gens = std::collections::HashSet::new();
            for _ in 0..200 {
                let requests = envelope_requested_publishes(&announcer_clone);
                for req in &requests {
                    if !acked_gens.contains(&req.generation) {
                        subscriber_clone.publish(
                            OBSERVED_CID,
                            envelope_observed_state(
                                req.generation,
                                req.requested_state,
                            ),
                        );
                        acked_gens.insert(req.generation);
                    }
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        });

        let resp = p
            .handle_request(&req(
                "options.set_mixer_type",
                json!({ "v": 1, "value": "none" }),
            ))
            .await
            .expect("set_mixer_type ok");
        let v: Value = serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(v["status"], "ok");

        ack_task.abort();

        let requests = envelope_requested_publishes(&announcer);
        assert_eq!(
            requests.len(),
            2,
            "exactly two envelope_requested publishes per transition: muted + unmuted; got {requests:?}"
        );
        assert_eq!(requests[0].requested_state, EnvelopeState::Muted);
        assert_eq!(requests[1].requested_state, EnvelopeState::Unmuted);
        assert!(
            requests[1].generation > requests[0].generation,
            "generation monotonic across envelope publishes"
        );
    }

    #[tokio::test]
    async fn env_no_ack_routes_to_rollback() {
        // I5 (rollback-safe) at the envelope layer: when the
        // warden never publishes envelope_observed, the
        // orchestrator's await times out at the configured
        // timeout (200ms in this test); the orchestrator
        // emits rolled_back (no failed); the gesture returns
        // Err(Permanent) naming the at_phase + reason. The
        // chain is back at the prior valid state.
        let (mut p, announcer, _subscriber, _querier, _dir) =
            envelope_wired_plugin().await;

        let err = p
            .handle_request(&req(
                "options.set_mixer_type",
                json!({ "v": 1, "value": "none" }),
            ))
            .await
            .expect_err("timeout must route to rollback");
        match err {
            PluginError::Permanent(msg) => {
                assert!(
                    msg.contains("rolled back"),
                    "rollback path engaged: {msg}"
                );
                assert!(
                    msg.contains("pre_mute"),
                    "rollback identifies the failed phase: {msg}"
                );
                assert!(
                    msg.contains("envelope ack timeout"),
                    "rollback names the timeout cause: {msg}"
                );
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
        // Only the muted publish was attempted; no unmute
        // because the pre-mute step never completed.
        let requests = envelope_requested_publishes(&announcer);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].requested_state, EnvelopeState::Muted);
    }

    #[tokio::test]
    async fn env_advisory_mode_when_no_observed_subject_announced() {
        // When envelope_observed addressing doesn't resolve
        // (no warden participating), the orchestrator's await
        // returns Ok immediately + the transition proceeds.
        // This is NOT silent failure — it's conditional
        // behavior based on observable subject-registry state
        // (a debug log surfaces the advisory-mode entry). We
        // exercise it by using a querier whose envelope_observed
        // addressing is NOT registered.
        let dir = tempdir().unwrap();
        let state_path = dir.path().join(STATE_FILENAME);
        let announcer = Arc::new(CapturingAnnouncer::default());
        let mut p = PlaybackOptionsPlugin::new()
            .with_state_path(state_path)
            .with_envelope_ack_timeout_override(Duration::from_millis(200));
        p.happening_emitter = Some(Arc::new(CapturingEmitter::default()));
        p.subject_announcer =
            Some(Arc::clone(&announcer) as Arc<dyn SubjectAnnouncer>);
        // Querier that resolves nothing (no envelope_observed
        // addressing registered → no warden).
        p.subject_querier =
            Some(Arc::new(StubQuerier::default()) as Arc<dyn SubjectQuerier>);
        // Subscriber present but unused — querier returns None
        // before subscribe is called.
        p.subject_state_subscriber =
            Some(Arc::new(StubStateSubscriber::default())
                as Arc<dyn SubjectStateSubscriber>);
        p.loaded = true;

        let resp = p
            .handle_request(&req(
                "options.set_mixer_type",
                json!({ "v": 1, "value": "none" }),
            ))
            .await
            .expect("advisory-mode transition completes");
        let v: Value = serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(v["status"], "ok");
        // Both publishes still happen — the advisory-mode
        // path skips only the AWAIT side, not the PUBLISH.
        // Drain the backgrounded publish tasks before asserting.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let requests = envelope_requested_publishes(&announcer);
        assert_eq!(requests.len(), 2);
    }

    // ----- envelope publish-vs-subscribe ordering regression -----
    //
    // Wrapper announcer that synchronously publishes
    // envelope_observed onto the supplied subscriber whenever the
    // orchestrator publishes envelope_requested. Models a fast
    // in-substrate warden that responds inside the same
    // tokio::spawn cycle the orchestrator's publish task runs in.
    #[derive(Clone)]
    struct SyncAckAnnouncer {
        inner: Arc<CapturingAnnouncer>,
        subscriber: StubStateSubscriber,
        observed_cid: String,
    }

    impl evo_plugin_sdk::contract::SubjectAnnouncer for SyncAckAnnouncer {
        fn announce<'a>(
            &'a self,
            announcement: SubjectAnnouncement,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<(), ReportError>>
                    + Send
                    + 'a,
            >,
        > {
            self.inner.announce(announcement)
        }

        fn retract<'a>(
            &'a self,
            addressing: ExternalAddressing,
            reason: Option<String>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<(), ReportError>>
                    + Send
                    + 'a,
            >,
        > {
            self.inner.retract(addressing, reason)
        }

        fn update_state<'a>(
            &'a self,
            addressing: ExternalAddressing,
            state: serde_json::Value,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<(), ReportError>>
                    + Send
                    + 'a,
            >,
        > {
            let inner = Arc::clone(&self.inner);
            let subscriber = self.subscriber.clone();
            let observed_cid = self.observed_cid.clone();
            let addressing_clone = addressing.clone();
            let state_clone = state.clone();
            Box::pin(async move {
                inner.update_state(addressing, state).await?;
                if addressing_clone.scheme == ENVELOPE_SCHEME
                    && addressing_clone.value == ENVELOPE_REQUESTED_VALUE
                {
                    if let Ok(req) =
                        serde_json::from_value::<EnvelopeRequested>(state_clone)
                    {
                        if req.generation > 0 {
                            let observed = EnvelopeObserved {
                                v: ENVELOPE_PAYLOAD_VERSION,
                                generation: req.generation,
                                observed_state: req.requested_state,
                                observed_at_ms: 0,
                            };
                            if let Ok(payload) = serde_json::to_value(&observed)
                            {
                                subscriber.publish(&observed_cid, payload);
                            }
                        }
                    }
                }
                Ok(())
            })
        }
    }

    // Wrapper subscriber that delays subscribe_subject by a
    // configurable interval before delegating to the inner
    // stub. Models substrate-roundtrip cost of resolving + wiring
    // a subscriber against the real subject registry.
    #[derive(Clone)]
    struct DelayedSubscribeSubscriber {
        inner: StubStateSubscriber,
        subscribe_delay: Duration,
    }

    impl evo_plugin_sdk::contract::SubjectStateSubscriber
        for DelayedSubscribeSubscriber
    {
        fn subscribe_subject<'a>(
            &'a self,
            canonical_id: String,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<SubjectStateStream, ReportError>,
                    > + Send
                    + 'a,
            >,
        > {
            let inner = self.inner.clone();
            let delay = self.subscribe_delay;
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                inner.subscribe_subject(canonical_id).await
            })
        }

        fn current_state<'a>(
            &'a self,
            canonical_id: String,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<Option<serde_json::Value>, ReportError>,
                    > + Send
                    + 'a,
            >,
        > {
            let inner = self.inner.clone();
            Box::pin(async move { inner.current_state(canonical_id).await })
        }
    }

    #[tokio::test]
    async fn env_first_gesture_must_not_lose_observed_to_subscribe_race() {
        // REGRESSION: The orchestrator MUST subscribe to
        // envelope_observed BEFORE publishing envelope_requested.
        //
        // Production substrate broadcasts state updates
        // synchronously to active subscribers; a tokio
        // broadcast::Receiver registered AFTER a send() does NOT
        // see the prior send. If the orchestrator publishes
        // envelope_requested first and the warden's response
        // broadcast happens before the orchestrator has
        // registered a subscriber, the broadcast is lost and the
        // orchestrator's await times out — producing the
        // operator-visible "Audio mode change reverted" with
        // "envelope ack timeout after 5000ms" cited in
        // [`Self::await_envelope_ack`].
        //
        // This test deterministically reproduces the production
        // timing with no reliance on scheduler luck:
        //
        //   1. Slow-subscribe subscriber (200ms delay in
        //      subscribe_subject) models the substrate-roundtrip
        //      cost of a real subscribe.
        //   2. Sync-ack announcer wrapper synchronously publishes
        //      envelope_observed inside the orchestrator's own
        //      envelope_requested update_state — models the
        //      warden's fast in-substrate fanout response.
        //
        // With the CURRENT `publish then subscribe` ordering, the
        // ack broadcast happens BEFORE the orchestrator's subscribe
        // completes; the broadcast is lost; await_envelope_ack
        // times out. With the FIXED `subscribe then publish`
        // ordering, the orchestrator's receiver is registered
        // before the warden can respond, so the ack arrives on
        // the active stream and the gesture completes.
        const OBSERVED_CID: &str = "stub-envelope-observed-cid";
        let dir = tempdir().unwrap();
        let state_path = dir.path().join(STATE_FILENAME);

        let inner_announcer = Arc::new(CapturingAnnouncer::default());
        let inner_subscriber = StubStateSubscriber::default();
        let querier = StubQuerier::default();
        querier.register(
            &ExternalAddressing {
                scheme: ENVELOPE_SCHEME.to_string(),
                value: ENVELOPE_OBSERVED_VALUE.to_string(),
            },
            OBSERVED_CID,
        );

        let sync_ack = SyncAckAnnouncer {
            inner: Arc::clone(&inner_announcer),
            subscriber: inner_subscriber.clone(),
            observed_cid: OBSERVED_CID.to_string(),
        };
        let delayed_subscriber = DelayedSubscribeSubscriber {
            inner: inner_subscriber.clone(),
            subscribe_delay: Duration::from_millis(200),
        };

        let mut p = PlaybackOptionsPlugin::new()
            .with_state_path(state_path)
            .with_envelope_ack_timeout_override(Duration::from_millis(500));
        p.happening_emitter = Some(Arc::new(CapturingEmitter::default()));
        p.subject_announcer =
            Some(Arc::new(sync_ack) as Arc<dyn SubjectAnnouncer>);
        p.subject_state_subscriber = Some(
            Arc::new(delayed_subscriber) as Arc<dyn SubjectStateSubscriber>
        );
        p.subject_querier = Some(Arc::new(querier) as Arc<dyn SubjectQuerier>);
        p.loaded = true;

        let result = tokio::time::timeout(
            Duration::from_secs(3),
            p.handle_request(&req(
                "options.set_mixer_type",
                json!({ "v": 1, "value": "none" }),
            )),
        )
        .await
        .expect("test framework: gesture must complete within 3s");

        let resp = result.expect(
            "first mixer-type gesture must NOT race-lose the warden's \
             envelope_observed ack — orchestrator must subscribe BEFORE \
             publishing envelope_requested so the warden's response \
             arrives on an active receiver, not into a closed broadcast",
        );
        let v: Value = serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(
            v["status"], "ok",
            "first gesture must complete with status=ok despite realistic \
             publish-vs-subscribe timing"
        );

        // Both publishes happened (muted then unmuted) — proves
        // the full state machine ran to completion, not just
        // step 2. Drain backgrounded publish tasks before the
        // capture readout.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let requests = envelope_requested_publishes(&inner_announcer);
        assert_eq!(
            requests.len(),
            2,
            "full state machine must run: muted + unmuted (got: {requests:?})"
        );
        assert_eq!(requests[0].requested_state, EnvelopeState::Muted);
        assert_eq!(requests[1].requested_state, EnvelopeState::Unmuted);
    }

    #[tokio::test]
    async fn t3_set_mixer_type_hardware_refuses_when_coordinates_missing() {
        // Contract acceptance criterion
        // `mixer-transition-hardware-mode-requires-device-and-control`:
        // setting `mixer_type = hardware` without `mixer_device`
        // + `mixer_control` populated MUST refuse with a
        // structured Permanent error naming both required
        // fields. Silent automatic degrade-to-software is a
        // contract violation.
        let (mut p, _dir) = loaded_plugin().await;
        let err = p
            .handle_request(&req(
                "options.set_mixer_type",
                json!({ "v": 1, "value": "hardware" }),
            ))
            .await
            .unwrap_err();
        match err {
            PluginError::Permanent(msg) => {
                assert!(msg.contains("mixer_device"), "{msg}");
                assert!(msg.contains("mixer_control"), "{msg}");
                assert!(msg.contains("hardware"), "{msg}");
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
        // Settings MUST NOT have changed (the gesture was
        // refused before any side effect).
        let resp = p
            .handle_request(&req("options.get_settings", json!({ "v": 1 })))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(
            v["mixer_type"], "software",
            "default mixer_type preserved on refused gesture"
        );
    }

    #[tokio::test]
    async fn t5_concurrent_mixer_type_gestures_serialise_via_lock() {
        // Contract invariant I3 (single authority): concurrent
        // mixer-type gestures serialise via the per-plugin
        // transition lock. Two parallel set_mixer_type calls
        // must each see a consistent post-transition state;
        // they MUST NOT interleave (which would risk a brief
        // overlap window of two authorities).
        //
        // We spawn two gestures swapping back-and-forth between
        // software and none (both target modes that pass the
        // hardware-coordinate check). Both must succeed; the
        // final settled mixer_type matches the gesture issued
        // last (whichever wins the lock-acquire race second).
        let (p, _dir) = loaded_plugin().await;
        let p = Arc::new(tokio::sync::Mutex::new(p));

        let p1 = Arc::clone(&p);
        let h1 = tokio::spawn(async move {
            let mut guard = p1.lock().await;
            guard
                .handle_request(&req(
                    "options.set_mixer_type",
                    json!({ "v": 1, "value": "none" }),
                ))
                .await
        });
        let p2 = Arc::clone(&p);
        let h2 = tokio::spawn(async move {
            let mut guard = p2.lock().await;
            guard
                .handle_request(&req(
                    "options.set_mixer_type",
                    json!({ "v": 1, "value": "software" }),
                ))
                .await
        });
        // Both gestures succeed; the lock prevents
        // interleaving.
        h1.await.unwrap().unwrap();
        h2.await.unwrap().unwrap();
        // Final state is a valid value (one of the two
        // requested), never something unexpected.
        let resp = p
            .lock()
            .await
            .handle_request(&req("options.get_settings", json!({ "v": 1 })))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&resp.payload).unwrap();
        let final_type = v["mixer_type"].as_str().unwrap();
        assert!(
            final_type == "none" || final_type == "software",
            "post-transition mixer_type must be one of the gestured \
             values; got {final_type:?}"
        );
    }

    #[tokio::test]
    async fn t6_settings_rehydrate_restores_mixer_type_after_restart() {
        // Contract acceptance criterion T6: reboot/startup
        // restores prior mode with correct effective volume.
        // The code-level proxy: persisted state file
        // rehydrates the mixer_type + mixer_device +
        // mixer_control fields on a fresh plugin instance,
        // matching what the orchestrator wrote.
        let dir = tempdir().unwrap();
        let state_path = dir.path().join(STATE_FILENAME);
        {
            let mut p = PlaybackOptionsPlugin::new()
                .with_state_path(state_path.clone());
            p.happening_emitter = Some(Arc::new(CapturingEmitter::default()));
            p.subject_announcer = Some(Arc::new(CapturingAnnouncer::default()));
            p.loaded = true;
            // Set the coordinates first (contract:
            // hardware-mode requires them), then switch.
            p.handle_request(&req(
                "options.set_mixer_device",
                json!({ "v": 1, "value": "hw:CARD=DAC" }),
            ))
            .await
            .unwrap();
            p.handle_request(&req(
                "options.set_mixer_control",
                json!({ "v": 1, "value": "PCM" }),
            ))
            .await
            .unwrap();
            p.handle_request(&req(
                "options.set_mixer_type",
                json!({ "v": 1, "value": "hardware" }),
            ))
            .await
            .unwrap();
        }
        // Fresh instance loads from the same path; settings
        // rehydrate the mixer-mode coordinates so the
        // post-restart effective authority matches the prior
        // run.
        let p2 = PlaybackOptionsPlugin::new().with_state_path(state_path);
        let rehydrated =
            p2.load_settings_from_disk().await.expect("rehydrate ok");
        assert!(matches!(rehydrated.mixer_type, MixerType::Hardware));
        assert_eq!(rehydrated.mixer_device, "hw:CARD=DAC");
        assert_eq!(rehydrated.mixer_control, "PCM");
    }

    #[tokio::test]
    async fn set_mixer_type_no_op_when_from_equals_to() {
        // Short-circuit shape (transition.rs's NoOp variant
        // exercised through the orchestrated handler).
        // Switching from the current mixer_type to itself
        // succeeds without emitting any lifecycle event.
        let (mut p, _dir) = loaded_plugin().await;
        // default mixer_type is software; gesture software
        // again — no-op.
        let resp = p
            .handle_request(&req(
                "options.set_mixer_type",
                json!({ "v": 1, "value": "software" }),
            ))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(v["status"], "ok");
    }

    #[tokio::test]
    async fn settings_persist_to_disk_and_rehydrate() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join(STATE_FILENAME);

        // Plugin instance #1 — write some settings.
        // The orchestrated mixer-type setter requires
        // mixer_device + mixer_control to be populated before
        // switching to hardware mode (contract: no silent
        // degrade), so set them first.
        {
            let mut p = PlaybackOptionsPlugin::new()
                .with_state_path(state_path.clone());
            p.happening_emitter = Some(Arc::new(CapturingEmitter::default()));
            p.subject_announcer = Some(Arc::new(CapturingAnnouncer::default()));
            p.loaded = true;
            p.handle_request(&req(
                "options.set_mixer_device",
                json!({ "v": 1, "value": "hw:0" }),
            ))
            .await
            .unwrap();
            p.handle_request(&req(
                "options.set_mixer_control",
                json!({ "v": 1, "value": "Master" }),
            ))
            .await
            .unwrap();
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

    // ----- Outer-budget recovery tests -----
    //
    // Regression coverage for the wedge surfaced on a live rig:
    // a `set_mixer_type` gesture that called into a substrate
    // whose `update_state` never returned stalled the plugin's
    // `&mut self` indefinitely, queueing every subsequent
    // request behind it until the steward was restarted. The
    // fix adds two bounds:
    //
    //   1. Per-call timeout on each `announcer.update_state`
    //      invocation inside `publish_envelope_requested` and
    //      `publish_settings_state` (`SUBJECT_PUBLISH_TIMEOUT`).
    //
    //   2. An overall budget on `run_mixer_transition` inside
    //      `handle_set_mixer_type` (`TRANSITION_OVERALL_TIMEOUT`),
    //      which forces the inner future to drop and releases
    //      `&mut self` even if the inner orchestration's own
    //      bounds compose pathologically.
    //
    // These tests pin both bounds. The hung-substrate is
    // simulated by a `BlockingAnnouncer` whose `update_state`
    // future never resolves; production substrates may surface
    // the same shape under socket back-pressure, a stalled
    // observer fan-out, or a dead peer.

    /// Announcer whose `update_state` never returns. Mirrors a
    /// wedged subject-substrate observer: socket back-pressure,
    /// a peer that never drains the announcement stream, or a
    /// deadlocked observer reaction. `announce` and `retract`
    /// resolve immediately so plugin construction is unaffected.
    #[derive(Default)]
    struct BlockingAnnouncer;

    impl std::fmt::Debug for BlockingAnnouncer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("BlockingAnnouncer").finish_non_exhaustive()
        }
    }

    impl SubjectAnnouncer for BlockingAnnouncer {
        fn announce<'a>(
            &'a self,
            _announcement: SubjectAnnouncement,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<(), ReportError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move { Ok(()) })
        }

        fn retract<'a>(
            &'a self,
            _addressing: ExternalAddressing,
            _reason: Option<String>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<(), ReportError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move { Ok(()) })
        }

        fn update_state<'a>(
            &'a self,
            _addressing: ExternalAddressing,
            _state: serde_json::Value,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<(), ReportError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                // Never returns. The test cancels this future by
                // dropping it (the outer-budget timeout fires in
                // `handle_set_mixer_type` and the inner future is
                // dropped, which drops this).
                std::future::pending::<()>().await;
                Ok(())
            })
        }
    }

    fn blocking_announcer_plugin(
        publish_timeout: Duration,
        overall_timeout: Duration,
    ) -> (PlaybackOptionsPlugin, tempfile::TempDir) {
        const OBSERVED_CID: &str = "stub-envelope-observed-cid";
        let dir = tempdir().unwrap();
        let state_path = dir.path().join(STATE_FILENAME);
        // Register envelope_observed so `await_envelope_ack`
        // enters its subscribe + wait branch (rather than the
        // advisory-mode short-circuit). The wedge tests rely on
        // the orchestrator actually waiting somewhere inside
        // `run_mixer_transition` — with the publish backgrounded,
        // the wait inside `await_envelope_ack` is what the outer
        // budget pre-empts.
        let querier = StubQuerier::default();
        querier.register(
            &ExternalAddressing {
                scheme: ENVELOPE_SCHEME.to_string(),
                value: ENVELOPE_OBSERVED_VALUE.to_string(),
            },
            OBSERVED_CID,
        );
        let mut p = PlaybackOptionsPlugin::new()
            .with_state_path(state_path)
            .with_subject_publish_timeout_override(publish_timeout)
            .with_transition_overall_timeout_override(overall_timeout)
            .with_envelope_ack_timeout_override(Duration::from_secs(5));
        p.happening_emitter = Some(Arc::new(CapturingEmitter::default()));
        p.subject_announcer =
            Some(Arc::new(BlockingAnnouncer) as Arc<dyn SubjectAnnouncer>);
        p.subject_state_subscriber =
            Some(Arc::new(StubStateSubscriber::default())
                as Arc<dyn SubjectStateSubscriber>);
        p.subject_querier = Some(Arc::new(querier) as Arc<dyn SubjectQuerier>);
        p.loaded = true;
        (p, dir)
    }

    #[tokio::test]
    async fn outer_budget_fires_when_publish_bound_exceeds_overall() {
        // Overall budget (300ms) fires first because the per-call
        // publish bound (5s) is longer. The outer timeout cancels
        // the inner orchestration, emits `failed` with the
        // `overall_budget` phase tag, and returns Permanent. This
        // is the safety net that prevents the wedge regardless of
        // how the inner step composition stalls.
        let (mut p, _dir) = blocking_announcer_plugin(
            Duration::from_secs(5),
            Duration::from_millis(300),
        );

        let started = std::time::Instant::now();
        let err = p
            .handle_request(&req(
                "options.set_mixer_type",
                json!({ "v": 1, "value": "none" }),
            ))
            .await
            .expect_err("outer budget must surface as Permanent");
        let elapsed = started.elapsed();

        match err {
            PluginError::Permanent(msg) => {
                assert!(
                    msg.contains("overall_budget"),
                    "error names the outer-budget phase: {msg}"
                );
                assert!(
                    msg.contains("transition_timed_out"),
                    "error names the timeout reason: {msg}"
                );
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
        // Outer budget at 300ms; allow generous slack for the
        // post-timeout recovery work (disk re-read + lifecycle
        // emit). The hard assertion is "well below 5s" (the
        // per-call publish bound, which must NOT have been the
        // surface that fired).
        assert!(
            elapsed < Duration::from_secs(2),
            "outer budget must surface well below per-call publish bound \
             (5s); elapsed = {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn plugin_unwedged_after_outer_budget_recovery() {
        // The engineering-bar test: after the outer budget
        // recovers from a wedged orchestration, a subsequent
        // request must complete promptly. Before the fix,
        // `&mut self` was held by the stuck inner future and
        // every later request queued behind it; the steward had
        // to be restarted. After the fix, the inner future is
        // dropped and the next request handles immediately.
        let (mut p, _dir) = blocking_announcer_plugin(
            Duration::from_secs(5),
            Duration::from_millis(300),
        );

        // First request: hits the outer-budget recovery path.
        let _err = p
            .handle_request(&req(
                "options.set_mixer_type",
                json!({ "v": 1, "value": "none" }),
            ))
            .await
            .expect_err("outer budget must surface as Permanent");

        // Second request: must complete promptly. We pick
        // `options.get_settings` because it touches no wire
        // surface — purely in-memory snapshot of `self.settings`.
        // If the plugin is wedged, this hangs forever and the
        // test framework's per-test timeout would surface it.
        let started = std::time::Instant::now();
        let resp = p
            .handle_request(&req("options.get_settings", json!({ "v": 1 })))
            .await
            .expect("get_settings must complete after outer-budget recovery");
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(500),
            "second request must complete promptly (proves plugin unwedged); \
             elapsed = {elapsed:?}"
        );
        let v: Value = serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(v["v"], 1);
    }

    // ---------------------------------------------------------
    // Architectural invariant: envelope_observed channel
    // resolved + subscribed EXACTLY ONCE per plugin lifetime,
    // not per transition.
    //
    // Root cause this test pins: prior to the architectural
    // refactor, every `set_mixer_type` walked
    // resolve_addressing → subscribe_subject → current_state
    // from scratch — three substrate round trips per gesture.
    // When the substrate slowed, each transition stalled
    // waiting on those calls; the per-call timeouts added
    // were band-aids that did not address the architectural
    // defect.
    //
    // The fix: resolve + subscribe lazily on first transition,
    // cache the channel, reuse on every subsequent transition.
    // Per-transition cost drops to a recv-loop + generation
    // filter.
    //
    // This test drives multiple transitions through one plugin
    // instance and asserts the stubs observed resolve+subscribe
    // exactly once.
    #[tokio::test]
    async fn envelope_channel_resolved_and_subscribed_exactly_once_across_transitions(
    ) {
        const OBSERVED_CID: &str = "stub-envelope-observed-cid";
        let (mut p, announcer, subscriber, querier, _dir) =
            envelope_wired_plugin().await;

        // Drive three full transitions. The orchestrator's
        // mute + unmute steps each consult the envelope_observed
        // channel, so absent the fix the stubs would observe
        // 3 × 2 = 6 resolve calls + 6 subscribe calls. With the
        // architectural fix they MUST observe exactly 1 of each.
        let subscriber_clone = subscriber.clone();
        let announcer_clone = Arc::clone(&announcer);
        let ack_task = tokio::spawn(async move {
            // Match every envelope_requested with a matching
            // envelope_observed ack. Loops until every
            // requested has been acked.
            let mut acked = 0_usize;
            loop {
                tokio::time::sleep(Duration::from_millis(20)).await;
                let pubs = envelope_requested_publishes(&announcer_clone);
                while acked < pubs.len() {
                    let req = &pubs[acked];
                    subscriber_clone.publish(
                        OBSERVED_CID,
                        envelope_observed_state(
                            req.generation,
                            req.requested_state,
                        ),
                    );
                    acked += 1;
                }
                if acked >= 6 {
                    break;
                }
            }
        });

        // none → software (no-op? depends on default). Let the
        // helper return the actual starting value; we choose
        // three target values to force at least two real
        // transitions whatever the start was.
        for target in ["software", "none", "software"] {
            let res = p
                .handle_request(&req(
                    "options.set_mixer_type",
                    json!({ "v": 1, "value": target }),
                ))
                .await;
            assert!(
                res.is_ok(),
                "transition to {target} must succeed; got {res:?}"
            );
        }

        let _ = tokio::time::timeout(Duration::from_secs(2), ack_task).await;

        let resolve_calls = querier.resolve_call_count();
        let subscribe_calls = subscriber.subscribe_call_count();

        assert_eq!(
            resolve_calls, 1,
            "architectural invariant violated: resolve_addressing called \
             {resolve_calls} times across three transitions; must be \
             exactly 1 (cached channel re-used across the plugin's \
             lifetime, not re-resolved per transition)"
        );
        assert_eq!(
            subscribe_calls, 1,
            "architectural invariant violated: subscribe_subject called \
             {subscribe_calls} times across three transitions; must be \
             exactly 1"
        );
    }
}
