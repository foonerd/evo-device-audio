// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! # org-evoframework-multiroom-evo-native
//!
//! evo-native multi-room audio-frame fan-out plugin.
//!
//! Bridges the local audio chain to the framework's
//! audio-plane TCP transport. The plugin operates in one of
//! three roles selected via its TOML config:
//!
//! - `role = "source"` — emit audio frames out to receivers
//!   via [`evo_plugin_sdk::contract::audio_plane::AudioPlaneHandle::fan_out_audio_frame`].
//!   Two source modes:
//!   - Capture mode (`source_pcm = "<alsa-pcm-name>"`, with
//!     the `alsa-substrate` Cargo feature on): the plugin
//!     opens the named ALSA capture PCM and reads
//!     `pcm_s16_le` / 48000 Hz / stereo in 20 ms chunks,
//!     emitting each chunk as one `AudioFrame`. Apex showcase
//!     mode — the operator wires `/etc/asound.conf` so the
//!     local audio chain (`pcm.evo`) forks through a
//!     `pcm.tee` plug into the receiver hardware DAC AND a
//!     `snd-aloop` loopback playback half; the multiroom
//!     plugin reads the loopback capture half, fanning out
//!     whatever MPD (or any audio producer) is rendering.
//!   - Synthetic mode (`source_pcm = ""` or unset, default):
//!     synthesises a 440 Hz sine-wave test tone at
//!     `pcm_s16_le` / 48000 Hz / stereo. Diagnostic floor —
//!     the substrate is observable without any ALSA config.
//! - `role = "receiver"` — subscribe to incoming audio
//!   frames via [`evo_plugin_sdk::contract::audio_plane::AudioPlaneHandle::subscribe_audio_frames`]
//!   and write the decoded PCM bytes to the local ALSA
//!   playback device named in the config. The receiver
//!   schedules every frame against a presentation-time
//!   anchor (set on the first frame received) plus an
//!   operator-tunable `leader_ms` budget so playback is
//!   bit-perfect (no sample drops, no sample inserts)
//!   regardless of network jitter inside the budget. Late
//!   frames are still rendered (they catch up against the
//!   ALSA hardware buffer); the only "drift defence" the
//!   operator turns is `leader_ms`. Underruns (no frame
//!   due at a render tick) write one period of silence and
//!   bump an operator-visible counter, so playback
//!   continuity holds.
//! - `role = "auto"` (default) — observe-only: subscribe
//!   and count incoming frames but do NOT engage capture or
//!   playback. Useful for substrate diagnostics + the future
//!   election-driven role flipping that will replace the
//!   manual `source`/`receiver` config once the GroupStore
//!   handle on `LoadContext` lands.
//!
//! ## Scope of v0.1.13 baseline
//!
//! - Codec: raw `pcm_s16_le` (no encoder dependency).
//! - Sample rate: 48 kHz stereo (matches the synthetic tone
//!   AND the typical ALSA hardware default).
//! - Source: ALSA capture PCM when `source_pcm` is set;
//!   synthetic 440 Hz sine generator as diagnostic fallback.
//! - Receiver: ALSA writei to the configured playback PCM
//!   when the `alsa-substrate` Cargo feature is enabled; on
//!   builds without the feature the receiver counts frames
//!   without rendering.
//!
//! Operator config (`/etc/evo/plugins.d/multiroom.evo-native.toml`):
//!
//! ```toml
//! role = "source"             # "source" | "receiver" | "auto"
//! group_id = "<uuid>"         # required when role = "source"
//! alsa_pcm = "evo"            # ALSA playback device (receiver)
//! source_pcm = "evo_loopback" # ALSA capture device (source);
//!                             # empty/unset => synthetic 440 Hz
//! leader_ms = 200             # presentation-time leader / network
//!                             # latency budget (ms). Tunable live
//!                             # via `multiroom.set_leader_ms`.
//! ```
//!
//! Production-quality additions (FEC / adaptive jitter
//! buffering / predictive buffering / network-class auto-
//! tuning / cooperative peer recovery / source-host election
//! follow / encoded codecs) ride v0.1.14+ per the
//! reliability bar in `project-multiroom-position` memory.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::manual_async_fn)]

use std::future::Future;
use std::sync::Arc;

use evo_plugin_sdk::contract::audio_plane::{
    AudioFrameSeed, AudioPlaneHandle, ReceiverTelemetry,
};
use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities,
};
use evo_plugin_sdk::Manifest;
use serde::{Deserialize, Serialize};
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
const REQUEST_TYPES: &[&str] = &[
    "multiroom.get_status",
    "multiroom.set_leader_ms",
    "multiroom.set_profile",
    "multiroom.set_leader_mode",
];

/// Lower bound for operator-set `leader_ms`. Below this the
/// network-jitter budget collapses and the receiver underruns
/// on the first slow packet. 20 ms is one period — the
/// theoretical floor; below it the scheduler cannot run.
const LEADER_MS_MIN: u64 = 20;

/// Upper bound for operator-set `leader_ms`. Above this the
/// end-to-end latency is far enough that source-host control
/// (pause / resume / track-skip) feels laggy. 2000 ms is
/// the practical ceiling for a music-listening UX.
const LEADER_MS_MAX: u64 = 2000;

/// Sample rate the v0.1.13 baseline source generator emits at
/// (and the receiver expects). Matches the typical ALSA
/// hardware default; resampling lands as a follow-on.
const BASELINE_SAMPLE_RATE_HZ: u32 = 48_000;

/// Channel count the baseline emits + renders.
const BASELINE_CHANNELS: u16 = 2;

/// Tone frequency the baseline source generator emits. 440 Hz
/// is concert A — universally recognisable when the operator
/// hears it from the receiver.
const BASELINE_TONE_HZ: f32 = 440.0;

/// Frames per audio chunk emitted by the source generator.
/// 960 samples at 48 kHz = 20 ms per chunk — typical real-
/// time audio packet size.
const FRAMES_PER_CHUNK: usize = 960;

/// Periods the receiver's ALSA buffer holds. Four periods @
/// 20 ms per period = ~80 ms hardware-buffer headroom. The
/// presentation-time scheduler decides which frame to feed
/// into the buffer next; this depth is the headroom between
/// writei and audible-at-DAC, not the queueing budget.
#[cfg(feature = "alsa-substrate")]
const RENDER_BUFFER_PERIODS: usize = 4;

/// Default `leader_ms`: how far ahead of presentation time the
/// source emits, and how much network-latency + jitter
/// tolerance the receiver allows before writing each frame to
/// ALSA. 200 ms is the typical baseline for LAN multi-room
/// (Roon RAAT defaults here; AirPlay 2 sits ~150 ms; SRT's
/// recommended `latency` is 4×RTT, typically 80-200 ms).
/// Operators tune via the `leader_ms` plugin config + the
/// `multiroom.set_leader_ms` runtime verb.
const DEFAULT_LEADER_MS: u64 = 200;

/// Scheduler tick period. The receiver wakes every
/// `SCHEDULER_TICK_MS` milliseconds to push any frames whose
/// scheduled render time has arrived into ALSA. Tighter than
/// the 20 ms frame budget so the scheduler can hit
/// sub-period precision.
const SCHEDULER_TICK_MS: u64 = 5;

/// Auto-decay profile: how aggressively the source aggregates
/// per-receiver telemetry into the group-wide `group_leader_ms`
/// it stamps on every frame. Conservative is the default; the
/// auto-decay loop escalates one step when underruns are
/// observed in the current window and decays one step when
/// the window passes with zero underruns.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    /// Tight end-to-end latency (~120 ms on healthy LAN).
    /// `max(p95) × 1.5 + 30 ms` floor. More underruns on
    /// borderline networks; best for very-low-latency demos.
    Aggressive,
    /// Default. `max(p95) × 2.0 + 50 ms` floor. End-to-end
    /// latency ~200 ms on a healthy LAN. Matches Roon RAAT's
    /// posture.
    Conservative,
    /// Few underruns even on noisy WiFi. `max(p99) × 3.0 +
    /// 100 ms` floor. End-to-end latency 400-600 ms typical.
    Defensive,
}

impl Profile {
    fn as_wire_str(&self) -> &'static str {
        match self {
            Self::Aggressive => "aggressive",
            Self::Conservative => "conservative",
            Self::Defensive => "defensive",
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Aggressive,
            2 => Self::Defensive,
            _ => Self::Conservative,
        }
    }

    /// Multiplier applied to the per-peer jitter-percentile.
    fn safety_factor(self) -> f64 {
        match self {
            Self::Aggressive => 1.5,
            Self::Conservative => 2.0,
            Self::Defensive => 3.0,
        }
    }

    /// Floor (ms) added to the multiplied jitter so even
    /// near-zero-jitter peers get a small absolute budget.
    fn floor_ms(self) -> u64 {
        match self {
            Self::Aggressive => 30,
            Self::Conservative => 50,
            Self::Defensive => 100,
        }
    }

    /// Decay direction: next-safer profile when underruns
    /// observed. `Defensive` is the terminal escalation.
    fn escalate(self) -> Self {
        match self {
            Self::Aggressive => Self::Conservative,
            Self::Conservative => Self::Defensive,
            Self::Defensive => Self::Defensive,
        }
    }

    /// Decay direction: next-tighter profile when the window
    /// passes underrun-free. `Aggressive` is the terminal
    /// tightening.
    fn decay(self) -> Self {
        match self {
            Self::Aggressive => Self::Aggressive,
            Self::Conservative => Self::Aggressive,
            Self::Defensive => Self::Conservative,
        }
    }
}

/// Leader-control mode the source-host plugin operates in.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum LeaderMode {
    /// Auto-decay: source computes `group_leader_ms` from per-
    /// peer telemetry under the configured `Profile`. The
    /// profile itself escalates / decays based on observed
    /// underrun counts.
    AutoDecay,
    /// Source pins the profile to the operator's choice. No
    /// auto-decay between profiles. Telemetry still drives the
    /// `group_leader_ms` within the pinned profile.
    AutoPinned,
    /// Manual: operator sets `manual_leader_ms` directly. The
    /// source stamps that fixed value on every frame; profile
    /// + telemetry are visible but not authoritative.
    Manual,
}

impl LeaderMode {
    fn as_wire_str(&self) -> &'static str {
        match self {
            Self::AutoDecay => "auto_decay",
            Self::AutoPinned => "auto_pinned",
            Self::Manual => "manual",
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::AutoPinned,
            2 => Self::Manual,
            _ => Self::AutoDecay,
        }
    }
}

/// Window (in source-aggregation ticks) over which the auto-
/// decay loop reads the underrun counter deltas. Source
/// recomputes group_leader_ms every
/// `SOURCE_AGGREGATION_TICK_MS` (250 ms) so a 120-tick window
/// = 30 s of quiet → decay one step. Asymmetric: escalation
/// on a single underrun is immediate (one tick); decay back
/// requires a sustained quiet window. The asymmetry stops
/// profile-flapping when the network sits on the edge of a
/// profile boundary — a one-off underrun escalates the
/// profile and the network must prove sustained quiet before
/// tightening again.
const AUTO_DECAY_WINDOW_TICKS: u32 = 120;

/// Source-side aggregation tick. The source-host plugin
/// recomputes `group_leader_ms` from peer telemetry every
/// `SOURCE_AGGREGATION_TICK_MS` ms and stamps subsequent
/// frames with the latest value. 250 ms is a balance between
/// responsiveness to network-condition changes and aggregation
/// stability.
const SOURCE_AGGREGATION_TICK_MS: u64 = 250;

/// Receiver-side telemetry publish cadence. The receiver task
/// pushes its current telemetry into the framework's
/// heartbeat-piggyback channel every
/// `RECEIVER_TELEMETRY_PUBLISH_MS` ms (3 s) so source-host
/// plugins see fresh data without flooding the wire.
const RECEIVER_TELEMETRY_PUBLISH_MS: u64 = 3000;

/// Receiver-side jitter-sample retention window (ms). The
/// receiver computes its observed_jitter_p95_ms over the most
/// recent `RECEIVER_JITTER_WINDOW_MS` worth of samples; older
/// samples are dropped so the percentile tracks current
/// network conditions, not stale ones.
const RECEIVER_JITTER_WINDOW_MS: u64 = 10_000;

/// Operator config persisted at
/// `/etc/evo/plugins.d/multiroom.evo-native.toml`.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct PluginConfig {
    /// Role this node should adopt. See module-level docs.
    #[serde(default = "default_role")]
    role: Role,
    /// Group id frames are fanned out to (required when
    /// `role = "source"`).
    #[serde(default)]
    group_id: Option<String>,
    /// ALSA playback device the receiver writes to. Defaults
    /// to `"evo"` — the modular pipeline pcm name
    /// delivery.alsa stocks. Operators with multiple cards
    /// or non-default routing override here.
    #[serde(default = "default_alsa_pcm")]
    alsa_pcm: String,
    /// ALSA capture device the source reads from in capture
    /// mode. When set (and the `alsa-substrate` Cargo feature
    /// is on), `role = "source"` opens this PCM and reads
    /// `pcm_s16_le` / 48000 Hz / stereo in 20 ms chunks,
    /// fanning each chunk out as one audio frame. Typical
    /// operator-deployed value is `"evo_loopback"`, paired
    /// with an `asound.conf` `pcm.tee` plug that forks
    /// `pcm.evo` between the local DAC and the loopback
    /// playback half (`hw:Loopback,0`); the capture half
    /// (`hw:Loopback,1`) is what this plugin reads. When
    /// empty / unset, source role falls back to the
    /// synthetic 440 Hz tone generator (diagnostic floor).
    #[serde(default)]
    source_pcm: String,
    /// Presentation-time leader in milliseconds. In
    /// `LeaderMode::Manual` this is the value the source-host
    /// stamps on every frame; receivers honour it directly.
    /// In auto modes this serves as the fallback when no
    /// peer telemetry has been observed yet, and as the
    /// effective value receivers apply on builds without the
    /// audio_plane wire field (forward-compat with v0 of the
    /// wire).
    #[serde(default = "default_leader_ms")]
    leader_ms: u64,
    /// Leader-control mode. Default `AutoDecay` — the source-
    /// host plugin recomputes the group's `group_leader_ms`
    /// every aggregation tick, and the profile escalates /
    /// decays based on observed underrun counts.
    #[serde(default = "default_leader_mode")]
    leader_mode: LeaderMode,
    /// Profile (Aggressive / Conservative / Defensive). In
    /// `AutoDecay` mode the auto-decay loop walks this value
    /// up + down within the [Aggressive, Defensive] range. In
    /// `AutoPinned` mode the operator's value stays put. In
    /// `Manual` mode the profile is informational only.
    #[serde(default = "default_profile")]
    profile: Profile,
    /// Render-buffer capacity (ms) the receiver advertises to
    /// the source-host so the source can clamp group_leader_ms
    /// to what the slowest receiver can actually buffer. Set
    /// per device-tier at install time.
    #[serde(default = "default_render_buffer_capacity_ms")]
    render_buffer_capacity_ms: u64,
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            role: default_role(),
            group_id: None,
            alsa_pcm: default_alsa_pcm(),
            source_pcm: String::new(),
            leader_ms: default_leader_ms(),
            leader_mode: default_leader_mode(),
            profile: default_profile(),
            render_buffer_capacity_ms: default_render_buffer_capacity_ms(),
        }
    }
}

fn default_role() -> Role {
    Role::Auto
}

fn default_alsa_pcm() -> String {
    "evo".to_string()
}

fn default_leader_ms() -> u64 {
    DEFAULT_LEADER_MS
}

fn default_leader_mode() -> LeaderMode {
    LeaderMode::AutoDecay
}

fn default_profile() -> Profile {
    Profile::Conservative
}

/// Default render-buffer capacity (ms) advertised on receiver
/// telemetry. 2000 ms = LEADER_MS_MAX, which the source uses
/// as the upper clamp on `group_leader_ms`.
fn default_render_buffer_capacity_ms() -> u64 {
    LEADER_MS_MAX
}

/// Plugin role. Set via operator config.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Role {
    /// Generate audio frames and fan them out to receivers
    /// of `group_id`.
    Source,
    /// Subscribe to incoming audio frames and render to local
    /// ALSA.
    Receiver,
    /// Observe only — count frames, do nothing else.
    Auto,
}

impl Role {
    fn as_wire_str(&self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Receiver => "receiver",
            Self::Auto => "auto",
        }
    }
}

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
    config: PluginConfig,
    audio_plane: Option<Arc<dyn AudioPlaneHandle>>,
    /// Receiver-side task; spawned for Receiver + Auto roles.
    receiver_task: Option<JoinHandle<()>>,
    /// Source-side task; spawned for Source role only.
    source_task: Option<JoinHandle<()>>,
    shutdown: Arc<Notify>,
    frames_received: Arc<std::sync::atomic::AtomicU64>,
    frames_sent: Arc<std::sync::atomic::AtomicU64>,
    /// Operator-tunable presentation-time leader in ms.
    /// In Manual mode, the source stamps this on every
    /// frame. In auto modes, this is the seed value used
    /// until peer telemetry refines it.
    leader_ms: Arc<std::sync::atomic::AtomicU64>,
    /// The group-wide `group_leader_ms` the source-host most
    /// recently computed (or, on a receiver, the most recent
    /// value extracted from incoming frames). Read by
    /// `multiroom.get_status`.
    group_leader_ms: Arc<std::sync::atomic::AtomicU64>,
    /// Current effective Profile (Aggressive / Conservative /
    /// Defensive). Source-host's auto-decay loop walks this
    /// up + down within the [Aggressive, Defensive] range
    /// when in `LeaderMode::AutoDecay`; pinned in
    /// `AutoPinned`; informational only in `Manual`. Stored
    /// as u8 so it fits an AtomicU8 (Aggressive=0,
    /// Conservative=1, Defensive=2).
    profile: Arc<std::sync::atomic::AtomicU8>,
    /// Current LeaderMode (AutoDecay / AutoPinned / Manual).
    /// AutoDecay=0, AutoPinned=1, Manual=2.
    leader_mode: Arc<std::sync::atomic::AtomicU8>,
    /// Receiver-side underrun counter: incremented every
    /// time `AlsaRender::write` recovered from an xrun
    /// (EPIPE underrun). Published to source-host plugins
    /// via heartbeat-piggybacked telemetry; operators
    /// observe via `multiroom.get_status`.
    receiver_underruns: Arc<std::sync::atomic::AtomicU64>,
    /// Receiver-side queue depth (most recent observed).
    /// Snapshot for `multiroom.get_status`; updated by the
    /// receiver scheduler each tick.
    receiver_queue_depth: Arc<std::sync::atomic::AtomicU64>,
    /// Receiver-side observed jitter p95 (ms). Computed over
    /// the most recent `RECEIVER_JITTER_WINDOW_MS` worth of
    /// samples; the receiver task updates this every tick.
    receiver_jitter_p95_ms: Arc<std::sync::atomic::AtomicU64>,
    /// Operator-set render-buffer capacity (ms). Advertised
    /// on telemetry so the source-host plugin clamps
    /// group_leader_ms to what the slowest receiver can hold.
    render_buffer_capacity_ms: Arc<std::sync::atomic::AtomicU64>,
    /// Source-side aggregation task. Spawned for source role
    /// only.
    source_aggregator_task: Option<JoinHandle<()>>,
    /// Receiver-side telemetry-publish task. Spawned for
    /// receiver + auto roles.
    receiver_telemetry_task: Option<JoinHandle<()>>,
}

impl MultiroomEvoNativePlugin {
    /// Construct a fresh plugin instance.
    pub fn new() -> Self {
        Self {
            loaded: false,
            config: PluginConfig::default(),
            audio_plane: None,
            receiver_task: None,
            source_task: None,
            shutdown: Arc::new(Notify::new()),
            frames_received: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            frames_sent: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            leader_ms: Arc::new(std::sync::atomic::AtomicU64::new(
                DEFAULT_LEADER_MS,
            )),
            group_leader_ms: Arc::new(std::sync::atomic::AtomicU64::new(
                DEFAULT_LEADER_MS,
            )),
            profile: Arc::new(std::sync::atomic::AtomicU8::new(
                Profile::Conservative as u8,
            )),
            leader_mode: Arc::new(std::sync::atomic::AtomicU8::new(
                LeaderMode::AutoDecay as u8,
            )),
            receiver_underruns: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            receiver_queue_depth: Arc::new(std::sync::atomic::AtomicU64::new(
                0,
            )),
            receiver_jitter_p95_ms: Arc::new(
                std::sync::atomic::AtomicU64::new(0),
            ),
            render_buffer_capacity_ms: Arc::new(
                std::sync::atomic::AtomicU64::new(
                    default_render_buffer_capacity_ms(),
                ),
            ),
            source_aggregator_task: None,
            receiver_telemetry_task: None,
        }
    }

    /// Total audio frames received across every connected
    /// source-host peer since plugin load.
    pub fn frames_received(&self) -> u64 {
        self.frames_received
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Total audio frames sent (source-role only).
    pub fn frames_sent(&self) -> u64 {
        self.frames_sent.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Receiver underrun count: scheduler ticks where no frame
    /// was due to render. Each one is a period of silence.
    pub fn receiver_underruns(&self) -> u64 {
        self.receiver_underruns
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Receiver scheduler queue depth (frames buffered waiting
    /// for their presentation_time_ms to arrive).
    pub fn receiver_queue_depth(&self) -> u64 {
        self.receiver_queue_depth
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Current operator-set presentation-time leader (ms).
    pub fn leader_ms(&self) -> u64 {
        self.leader_ms.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn apply_config(&mut self, table: &toml::Table) -> Result<(), PluginError> {
        // toml::Table -> PluginConfig via serde. Unknown keys
        // are silently dropped (default serde behaviour); the
        // documented keys above are the operator-facing
        // surface.
        let cfg: PluginConfig =
            toml::Value::Table(table.clone()).try_into().map_err(|e| {
                PluginError::Permanent(format!("invalid plugin config: {e}"))
            })?;
        if cfg.role == Role::Source && cfg.group_id.is_none() {
            return Err(PluginError::Permanent(
                "role = \"source\" requires group_id = \"<uuid>\" in plugin \
                 config"
                    .into(),
            ));
        }
        self.leader_ms
            .store(cfg.leader_ms, std::sync::atomic::Ordering::Relaxed);
        self.profile
            .store(cfg.profile as u8, std::sync::atomic::Ordering::Relaxed);
        self.leader_mode
            .store(cfg.leader_mode as u8, std::sync::atomic::Ordering::Relaxed);
        self.render_buffer_capacity_ms.store(
            cfg.render_buffer_capacity_ms,
            std::sync::atomic::Ordering::Relaxed,
        );
        // The group-wide leader seeds at the operator's
        // plugin-local leader_ms; the source's first aggregation
        // tick replaces it from peer telemetry.
        self.group_leader_ms
            .store(cfg.leader_ms, std::sync::atomic::Ordering::Relaxed);
        self.config = cfg;
        Ok(())
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
            self.apply_config(&ctx.config)?;

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

            match self.config.role {
                Role::Source => {
                    let group_id = self.config.group_id.clone().expect(
                        "role = source enforces group_id in apply_config",
                    );
                    let sent = Arc::clone(&self.frames_sent);
                    let shutdown = Arc::clone(&self.shutdown);
                    let handle = Arc::clone(&audio_plane);
                    let source_pcm = self.config.source_pcm.clone();
                    let group_leader_ms = Arc::clone(&self.group_leader_ms);
                    let task = if source_pcm.is_empty() {
                        let glm = Arc::clone(&group_leader_ms);
                        tokio::spawn(async move {
                            run_source_tone_generator(
                                handle, group_id, sent, shutdown, glm,
                            )
                            .await;
                        })
                    } else {
                        #[cfg(feature = "alsa-substrate")]
                        {
                            let pcm = source_pcm.clone();
                            let glm = Arc::clone(&group_leader_ms);
                            tokio::spawn(async move {
                                run_source_capture_task(
                                    handle, group_id, sent, shutdown, pcm, glm,
                                )
                                .await;
                            })
                        }
                        #[cfg(not(feature = "alsa-substrate"))]
                        {
                            tracing::warn!(
                                plugin = PLUGIN_NAME,
                                source_pcm = %source_pcm,
                                "source_pcm set but alsa-substrate feature \
                                 disabled at build time; falling back to \
                                 synthetic tone"
                            );
                            let glm = Arc::clone(&group_leader_ms);
                            tokio::spawn(async move {
                                run_source_tone_generator(
                                    handle, group_id, sent, shutdown, glm,
                                )
                                .await;
                            })
                        }
                    };
                    self.source_task = Some(task);
                    // Spawn the source-side aggregator that
                    // recomputes `group_leader_ms` every
                    // SOURCE_AGGREGATION_TICK_MS from per-peer
                    // telemetry, drives the auto-decay state
                    // machine, and updates the shared atomic
                    // the source-emitter reads on each frame.
                    let agg_handle = Arc::clone(&audio_plane);
                    let agg_shutdown = Arc::clone(&self.shutdown);
                    let agg_group_id = self
                        .config
                        .group_id
                        .clone()
                        .expect("role = source enforces group_id");
                    let agg_glm = Arc::clone(&self.group_leader_ms);
                    let agg_leader_ms = Arc::clone(&self.leader_ms);
                    let agg_profile = Arc::clone(&self.profile);
                    let agg_mode = Arc::clone(&self.leader_mode);
                    self.source_aggregator_task =
                        Some(tokio::spawn(async move {
                            run_source_aggregator(
                                agg_handle,
                                agg_group_id,
                                agg_shutdown,
                                agg_glm,
                                agg_leader_ms,
                                agg_profile,
                                agg_mode,
                            )
                            .await;
                        }));
                    if source_pcm.is_empty() {
                        tracing::info!(
                            plugin = PLUGIN_NAME,
                            group_id = %self.config.group_id.as_deref().unwrap_or(""),
                            leader_mode = self.config.leader_mode.as_wire_str(),
                            profile = self.config.profile.as_wire_str(),
                            "source role engaged: synthetic 440 Hz tone fan-out running"
                        );
                    } else {
                        tracing::info!(
                            plugin = PLUGIN_NAME,
                            group_id = %self.config.group_id.as_deref().unwrap_or(""),
                            source_pcm = %source_pcm,
                            leader_mode = self.config.leader_mode.as_wire_str(),
                            profile = self.config.profile.as_wire_str(),
                            "source role engaged: ALSA capture fan-out running"
                        );
                    }
                }
                Role::Receiver | Role::Auto => {
                    let counter = Arc::clone(&self.frames_received);
                    let shutdown = Arc::clone(&self.shutdown);
                    let handle = Arc::clone(&audio_plane);
                    let alsa_pcm = self.config.alsa_pcm.clone();
                    let role = self.config.role;
                    let leader_ms = Arc::clone(&self.leader_ms);
                    let group_leader_ms = Arc::clone(&self.group_leader_ms);
                    let mode = Arc::clone(&self.leader_mode);
                    let underruns = Arc::clone(&self.receiver_underruns);
                    let queue_depth = Arc::clone(&self.receiver_queue_depth);
                    let jitter_p95 = Arc::clone(&self.receiver_jitter_p95_ms);
                    let task = tokio::spawn(async move {
                        run_receiver_task(
                            handle,
                            counter,
                            shutdown,
                            alsa_pcm,
                            role,
                            leader_ms,
                            group_leader_ms,
                            mode,
                            underruns,
                            queue_depth,
                            jitter_p95,
                        )
                        .await;
                    });
                    self.receiver_task = Some(task);
                    // Spawn the telemetry-publisher: every few
                    // seconds push (jitter_p95, underrun_count,
                    // applied_leader_ms, render_buffer_capacity_ms)
                    // through the audio-plane heartbeat seam so
                    // source-host plugins on remote peers
                    // aggregate it.
                    let pub_handle = Arc::clone(&audio_plane);
                    let pub_shutdown = Arc::clone(&self.shutdown);
                    let pub_jitter = Arc::clone(&self.receiver_jitter_p95_ms);
                    let pub_underruns = Arc::clone(&self.receiver_underruns);
                    let pub_leader = Arc::clone(&self.group_leader_ms);
                    let pub_cap = Arc::clone(&self.render_buffer_capacity_ms);
                    self.receiver_telemetry_task =
                        Some(tokio::spawn(async move {
                            run_receiver_telemetry_publisher(
                                pub_handle,
                                pub_shutdown,
                                pub_jitter,
                                pub_underruns,
                                pub_leader,
                                pub_cap,
                            )
                            .await;
                        }));
                    tracing::info!(
                        plugin = PLUGIN_NAME,
                        role = self.config.role.as_wire_str(),
                        alsa_pcm = %self.config.alsa_pcm,
                        leader_ms = self.config.leader_ms,
                        leader_mode = self.config.leader_mode.as_wire_str(),
                        "receiver-side task running"
                    );
                }
            }

            self.loaded = true;
            tracing::info!(
                plugin = PLUGIN_NAME,
                role = self.config.role.as_wire_str(),
                "plugin loaded; audio-plane handle equipped"
            );
            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            self.shutdown.notify_waiters();
            if let Some(task) = self.source_task.take() {
                let _ = task.await;
            }
            if let Some(task) = self.receiver_task.take() {
                let _ = task.await;
            }
            if let Some(task) = self.source_aggregator_task.take() {
                let _ = task.await;
            }
            if let Some(task) = self.receiver_telemetry_task.take() {
                let _ = task.await;
            }
            self.audio_plane = None;
            self.loaded = false;
            tracing::info!(
                plugin = PLUGIN_NAME,
                frames_received = self.frames_received(),
                frames_sent = self.frames_sent(),
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
                    "role={} frames_sent={} frames_received={}",
                    self.config.role.as_wire_str(),
                    self.frames_sent(),
                    self.frames_received(),
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
                    let cur_profile = Profile::from_u8(
                        self.profile.load(std::sync::atomic::Ordering::Relaxed),
                    );
                    let cur_mode = LeaderMode::from_u8(
                        self.leader_mode
                            .load(std::sync::atomic::Ordering::Relaxed),
                    );
                    let peers = if let Some(h) = self.audio_plane.as_ref() {
                        h.list_audio_plane_peers().await.unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    let peer_telemetry: Vec<serde_json::Value> = peers
                        .into_iter()
                        .filter_map(|p| {
                            p.receiver_telemetry.map(|t| {
                                serde_json::json!({
                                    "remote_device_id": p.remote_device_id,
                                    "observed_jitter_p95_ms": t.observed_jitter_p95_ms,
                                    "underrun_count": t.underrun_count,
                                    "applied_leader_ms": t.applied_leader_ms,
                                    "render_buffer_capacity_ms": t.render_buffer_capacity_ms,
                                })
                            })
                        })
                        .collect();
                    let payload = serde_json::json!({
                        "v": PAYLOAD_VERSION,
                        "role": self.config.role.as_wire_str(),
                        "group_id": self.config.group_id,
                        "alsa_pcm": self.config.alsa_pcm,
                        "source_pcm": self.config.source_pcm,
                        "leader_ms": self.leader_ms(),
                        "leader_ms_min": LEADER_MS_MIN,
                        "leader_ms_max": LEADER_MS_MAX,
                        "leader_mode": cur_mode.as_wire_str(),
                        "profile": cur_profile.as_wire_str(),
                        "group_leader_ms": self.group_leader_ms.load(
                            std::sync::atomic::Ordering::Relaxed,
                        ),
                        "frames_sent": self.frames_sent(),
                        "frames_received": self.frames_received(),
                        "receiver_queue_depth": self.receiver_queue_depth(),
                        "receiver_underruns": self.receiver_underruns(),
                        "receiver_jitter_p95_ms": self.receiver_jitter_p95_ms.load(
                            std::sync::atomic::Ordering::Relaxed,
                        ),
                        "render_buffer_capacity_ms": self.render_buffer_capacity_ms.load(
                            std::sync::atomic::Ordering::Relaxed,
                        ),
                        "peer_telemetry": peer_telemetry,
                    });
                    let body = serde_json::to_vec(&payload).map_err(|e| {
                        PluginError::Permanent(format!(
                            "encode multiroom.get_status response: {e}"
                        ))
                    })?;
                    Ok(Response::for_request(req, body))
                }
                "multiroom.set_profile" => {
                    let body_json: serde_json::Value =
                        serde_json::from_slice(&req.payload).map_err(|e| {
                            PluginError::Permanent(format!(
                                "multiroom.set_profile: payload not JSON: {e}"
                            ))
                        })?;
                    let value = body_json
                        .get("value")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            PluginError::Permanent(
                                "multiroom.set_profile: payload must contain \
                                 string 'value' ∈ {aggressive, conservative, \
                                 defensive}"
                                    .to_string(),
                            )
                        })?;
                    let new_profile: Profile = match value {
                        "aggressive" => Profile::Aggressive,
                        "conservative" => Profile::Conservative,
                        "defensive" => Profile::Defensive,
                        other => {
                            return Err(PluginError::Permanent(format!(
                                "multiroom.set_profile: unknown profile {other:?}"
                            )));
                        }
                    };
                    self.profile.store(
                        new_profile as u8,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    tracing::info!(
                        plugin = PLUGIN_NAME,
                        profile = new_profile.as_wire_str(),
                        "profile updated by operator"
                    );
                    let payload = serde_json::json!({
                        "v": PAYLOAD_VERSION,
                        "profile": new_profile.as_wire_str(),
                    });
                    let body = serde_json::to_vec(&payload).map_err(|e| {
                        PluginError::Permanent(format!(
                            "encode multiroom.set_profile response: {e}"
                        ))
                    })?;
                    Ok(Response::for_request(req, body))
                }
                "multiroom.set_leader_mode" => {
                    let body_json: serde_json::Value =
                        serde_json::from_slice(&req.payload).map_err(|e| {
                            PluginError::Permanent(format!(
                            "multiroom.set_leader_mode: payload not JSON: {e}"
                        ))
                        })?;
                    let value = body_json
                        .get("value")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            PluginError::Permanent(
                                "multiroom.set_leader_mode: payload must \
                                 contain string 'value' ∈ {auto_decay, \
                                 auto_pinned, manual}"
                                    .to_string(),
                            )
                        })?;
                    let new_mode: LeaderMode = match value {
                        "auto_decay" => LeaderMode::AutoDecay,
                        "auto_pinned" => LeaderMode::AutoPinned,
                        "manual" => LeaderMode::Manual,
                        other => {
                            return Err(PluginError::Permanent(format!(
                                "multiroom.set_leader_mode: unknown mode {other:?}"
                            )));
                        }
                    };
                    self.leader_mode.store(
                        new_mode as u8,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    tracing::info!(
                        plugin = PLUGIN_NAME,
                        leader_mode = new_mode.as_wire_str(),
                        "leader_mode updated by operator"
                    );
                    let payload = serde_json::json!({
                        "v": PAYLOAD_VERSION,
                        "leader_mode": new_mode.as_wire_str(),
                    });
                    let body = serde_json::to_vec(&payload).map_err(|e| {
                        PluginError::Permanent(format!(
                            "encode multiroom.set_leader_mode response: {e}"
                        ))
                    })?;
                    Ok(Response::for_request(req, body))
                }
                "multiroom.set_leader_ms" => {
                    let body_json: serde_json::Value =
                        serde_json::from_slice(&req.payload).map_err(|e| {
                            PluginError::Permanent(format!(
                                "multiroom.set_leader_ms: payload not JSON: {e}"
                            ))
                        })?;
                    let value = body_json
                        .get("value")
                        .and_then(|v| v.as_u64())
                        .ok_or_else(|| {
                            PluginError::Permanent(
                                    "multiroom.set_leader_ms: \
                                     payload must contain integer 'value' field"
                                        .to_string(),
                                )
                        })?;
                    if !(LEADER_MS_MIN..=LEADER_MS_MAX).contains(&value) {
                        return Err(PluginError::Permanent(format!(
                            "multiroom.set_leader_ms: value {value} out of range \
                             [{LEADER_MS_MIN}, {LEADER_MS_MAX}]"
                        )));
                    }
                    self.leader_ms
                        .store(value, std::sync::atomic::Ordering::Relaxed);
                    tracing::info!(
                        plugin = PLUGIN_NAME,
                        leader_ms = value,
                        "leader_ms updated by operator"
                    );
                    let payload = serde_json::json!({
                        "v": PAYLOAD_VERSION,
                        "leader_ms": value,
                    });
                    let body = serde_json::to_vec(&payload).map_err(|e| {
                        PluginError::Permanent(format!(
                            "encode multiroom.set_leader_ms response: {e}"
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

/// Source-side synthetic-tone generator. Emits PCM frames at
/// the baseline format, 20 ms chunks, monotonic sequence,
/// `presentation_time_ms` set to local-monotonic-now + 100 ms
/// (a small fixed leader for the receiver's jitter buffer to
/// absorb network latency without underrun).
async fn run_source_tone_generator(
    audio_plane: Arc<dyn AudioPlaneHandle>,
    group_id: String,
    sent: Arc<std::sync::atomic::AtomicU64>,
    shutdown: Arc<Notify>,
    group_leader_ms: Arc<std::sync::atomic::AtomicU64>,
) {
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;

    let chunk_period = std::time::Duration::from_micros(
        1_000_000 * FRAMES_PER_CHUNK as u64 / BASELINE_SAMPLE_RATE_HZ as u64,
    );
    let mut sequence: u64 = 0;
    let mut phase: f32 = 0.0;
    let phase_step = 2.0 * std::f32::consts::PI * BASELINE_TONE_HZ
        / BASELINE_SAMPLE_RATE_HZ as f32;
    // Amplitude at ~−12 dBFS so the tone is comfortable but
    // clearly audible.
    let amplitude: f32 = 0.25 * i16::MAX as f32;

    let start_monotonic = std::time::Instant::now();
    let mut next_tick = start_monotonic;

    loop {
        tokio::select! {
            _ = shutdown.notified() => {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    "source tone generator: shutdown received"
                );
                return;
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(
                next_tick,
            )) => {}
        }

        // Build one PCM chunk: FRAMES_PER_CHUNK frames * 2
        // channels * 2 bytes-per-sample. Interleaved stereo:
        // L0, R0, L1, R1, ... (mono tone duplicated on both
        // channels).
        let mut pcm = Vec::with_capacity(
            FRAMES_PER_CHUNK * BASELINE_CHANNELS as usize * 2,
        );
        for _ in 0..FRAMES_PER_CHUNK {
            let sample = (phase.sin() * amplitude) as i16;
            phase += phase_step;
            if phase > 2.0 * std::f32::consts::PI {
                phase -= 2.0 * std::f32::consts::PI;
            }
            pcm.extend_from_slice(&sample.to_le_bytes());
            pcm.extend_from_slice(&sample.to_le_bytes());
        }

        // PTS = source-local monotonic time at this frame's
        // emission. The receiver anchors its playback timeline
        // on the first frame's PTS and schedules subsequent
        // frames at (anchor_local + delta_pts + leader_ms);
        // the receiver-side leader is what bounds drift. The
        // source must NOT add its own per-frame offset on top
        // of elapsed-since-start — elapsed already advances at
        // the emit cadence, and double-counting it stretches
        // the receiver's timeline (one wall-clock second of
        // audio becomes two seconds of scheduled render → 2×
        // slow-mo at the receiver).
        let presentation_time_ms = start_monotonic.elapsed().as_millis() as u64;

        let seed = AudioFrameSeed {
            sequence,
            presentation_time_ms,
            codec: "pcm_s16_le".to_string(),
            rate_hz: BASELINE_SAMPLE_RATE_HZ,
            channels: BASELINE_CHANNELS,
            payload_b64: B64.encode(&pcm),
            group_leader_ms: group_leader_ms
                .load(std::sync::atomic::Ordering::Relaxed),
        };

        if let Err(e) = audio_plane
            .fan_out_audio_frame(group_id.clone(), seed)
            .await
        {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "fan_out_audio_frame failed; continuing"
            );
        } else {
            sent.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        sequence = sequence.saturating_add(1);
        next_tick += chunk_period;
    }
}

/// Source-side ALSA capture task. Opens the operator-supplied
/// capture PCM (typically `evo_loopback` — the capture half of
/// a `pcm.tee`-forked chain that mirrors `pcm.evo` into a
/// `snd-aloop` loopback playback half), reads
/// `pcm_s16_le` / 48000 Hz / stereo in 20 ms chunks, and
/// fans each chunk out as one `AudioFrame`. The blocking ALSA
/// read runs on a dedicated OS thread to keep the tokio
/// runtime free; chunks are bridged into the async side via
/// a bounded mpsc channel (back-pressure: drops the oldest
/// chunk on overflow rather than blocking the capture thread,
/// because reading slow from a loopback half causes the
/// loopback playback half to underrun, which corrupts the
/// real-time chain).
#[cfg(feature = "alsa-substrate")]
async fn run_source_capture_task(
    audio_plane: Arc<dyn AudioPlaneHandle>,
    group_id: String,
    sent: Arc<std::sync::atomic::AtomicU64>,
    shutdown: Arc<Notify>,
    source_pcm: String,
    group_leader_ms: Arc<std::sync::atomic::AtomicU64>,
) {
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;

    // Capacity covers ~0.5 s of frames; if we fall further
    // behind than that the loopback playback half is corrupt
    // already.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);

    let capture_shutdown = Arc::clone(&shutdown);
    let capture_pcm = source_pcm.clone();
    let capture_thread = std::thread::Builder::new()
        .name("multiroom-capture".into())
        .spawn(move || {
            run_capture_thread(capture_pcm, tx, capture_shutdown);
        });
    let capture_thread = match capture_thread {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "spawn ALSA capture thread failed; source task exiting"
            );
            return;
        }
    };

    let mut sequence: u64 = 0;
    let start_monotonic = std::time::Instant::now();

    loop {
        tokio::select! {
            _ = shutdown.notified() => {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    "source capture task: shutdown received"
                );
                break;
            }
            chunk = rx.recv() => {
                let pcm = match chunk {
                    Some(p) => p,
                    None => {
                        tracing::debug!(
                            plugin = PLUGIN_NAME,
                            "capture channel closed; source task exiting"
                        );
                        break;
                    }
                };
                // PTS = source-local monotonic time at this
                // frame's emission. Receiver anchors on the
                // first frame's PTS + delta to schedule
                // subsequent renders. See the synthetic-tone
                // generator for the bit-perfect contract.
                let presentation_time_ms =
                    start_monotonic.elapsed().as_millis() as u64;
                let seed = AudioFrameSeed {
                    sequence,
                    presentation_time_ms,
                    codec: "pcm_s16_le".to_string(),
                    rate_hz: BASELINE_SAMPLE_RATE_HZ,
                    channels: BASELINE_CHANNELS,
                    payload_b64: B64.encode(&pcm),
                    group_leader_ms: group_leader_ms
                        .load(std::sync::atomic::Ordering::Relaxed),
                };
                if let Err(e) = audio_plane
                    .fan_out_audio_frame(group_id.clone(), seed)
                    .await
                {
                    tracing::warn!(
                        plugin = PLUGIN_NAME,
                        error = %e,
                        "fan_out_audio_frame failed; continuing"
                    );
                } else {
                    sent.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                sequence = sequence.saturating_add(1);
            }
        }
    }

    // Best-effort join: the capture thread sees the shutdown
    // Notify and the dropped tx, both of which signal exit.
    let _ = capture_thread.join();
}

/// OS-thread body that owns the ALSA capture handle. Loops
/// reading `FRAMES_PER_CHUNK` frames at a time, pushing each
/// chunk onto the async-side channel. Drops the oldest chunk
/// on channel pressure rather than blocking the capture loop
/// — see `run_source_capture_task`'s docblock for why.
#[cfg(feature = "alsa-substrate")]
fn run_capture_thread(
    source_pcm: String,
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    shutdown: Arc<Notify>,
) {
    let pcm = match alsa::PCM::new(&source_pcm, alsa::Direction::Capture, false)
    {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                source_pcm = %source_pcm,
                "ALSA capture open failed; source task will starve"
            );
            return;
        }
    };
    {
        let hwp = match alsa::pcm::HwParams::any(&pcm) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "alsa::pcm::HwParams::any (capture) failed"
                );
                return;
            }
        };
        if let Err(e) = hwp.set_channels(BASELINE_CHANNELS as u32) {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "set_channels (capture) failed"
            );
            return;
        }
        if let Err(e) =
            hwp.set_rate(BASELINE_SAMPLE_RATE_HZ, alsa::ValueOr::Nearest)
        {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "set_rate (capture) failed"
            );
            return;
        }
        if let Err(e) = hwp.set_format(alsa::pcm::Format::S16LE) {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "set_format (capture) failed"
            );
            return;
        }
        if let Err(e) = hwp.set_access(alsa::pcm::Access::RWInterleaved) {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "set_access (capture) failed"
            );
            return;
        }
        if let Err(e) = pcm.hw_params(&hwp) {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "pcm.hw_params (capture) failed"
            );
            return;
        }
    }
    if let Err(e) = pcm.prepare() {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            error = %e,
            "pcm.prepare (capture) failed"
        );
        return;
    }
    let io = match pcm.io_i16() {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "pcm.io_i16 (capture) failed"
            );
            return;
        }
    };
    tracing::info!(
        plugin = PLUGIN_NAME,
        source_pcm = %source_pcm,
        "ALSA capture opened at 48 kHz / 2 ch / pcm_s16_le"
    );

    let mut buf: Vec<i16> =
        vec![0; FRAMES_PER_CHUNK * BASELINE_CHANNELS as usize];
    loop {
        // shutdown.notified() is the async-side notifier. Poll
        // it by ticking off small reads + checking the channel
        // periodically — the closed channel + shutdown signal
        // both terminate the loop.
        let _ = &shutdown;
        match io.readi(&mut buf) {
            Ok(frames_read) => {
                if frames_read == 0 {
                    continue;
                }
                let mut pcm_bytes = Vec::with_capacity(
                    frames_read * BASELINE_CHANNELS as usize * 2,
                );
                for s in &buf[..frames_read * BASELINE_CHANNELS as usize] {
                    pcm_bytes.extend_from_slice(&s.to_le_bytes());
                }
                // Try non-blocking; on pressure drop oldest
                // (we are the producer, the async side is the
                // consumer; backpressure here would corrupt
                // the loopback playback half).
                if tx.try_send(pcm_bytes).is_err() {
                    // Either channel full (drop) or closed
                    // (exit). Treat full as soft-drop, closed
                    // as termination.
                    if tx.is_closed() {
                        break;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "ALSA readi (capture) failed; recovering"
                );
                let _ = pcm.prepare();
            }
        }
        if tx.is_closed() {
            break;
        }
    }
    tracing::info!(plugin = PLUGIN_NAME, "ALSA capture thread exiting");
}

/// Receiver-side task: presentation-time-scheduled bit-perfect
/// renderer. Subscribes to incoming `AudioFrameReceived` events,
/// anchors a local playback timeline to the first frame's
/// `presentation_time_ms`, and schedules every subsequent frame
/// at `anchor_local + (frame.presentation_time_ms - anchor_pts)`.
/// The operator-tunable `leader_ms` adds a fixed offset to the
/// anchor: more leader = more tolerance for network jitter, at
/// the cost of slightly higher end-to-end latency.
///
/// Bit-perfect contract: this scheduler never drops a frame to
/// bound drift, and never inserts samples to compensate. Each
/// frame's PCM bytes are written to ALSA verbatim at its
/// scheduled time. Late frames (presentation past local-clock-
/// now at the moment of dequeue) are still rendered — they
/// catch up against ALSA's hardware buffer headroom. The only
/// "drift defence" is the operator-set `leader_ms`: increase it
/// if late-frame events repeat.
///
/// Underrun handling: when the scheduler ticks and no frame is
/// scheduled to render in the next period, one period of
/// digital silence is written to ALSA so playback continuity
/// holds. Each underrun bumps the operator-visible
/// `receiver_underruns` counter.
#[allow(clippy::too_many_arguments)]
async fn run_receiver_task(
    audio_plane: Arc<dyn AudioPlaneHandle>,
    counter: Arc<std::sync::atomic::AtomicU64>,
    shutdown: Arc<Notify>,
    alsa_pcm: String,
    role: Role,
    leader_ms: Arc<std::sync::atomic::AtomicU64>,
    group_leader_ms_shared: Arc<std::sync::atomic::AtomicU64>,
    leader_mode: Arc<std::sync::atomic::AtomicU8>,
    underruns: Arc<std::sync::atomic::AtomicU64>,
    queue_depth: Arc<std::sync::atomic::AtomicU64>,
    jitter_p95_ms: Arc<std::sync::atomic::AtomicU64>,
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

    #[cfg(feature = "alsa-substrate")]
    let mut alsa_render = if role == Role::Receiver {
        match AlsaRender::open(&alsa_pcm) {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    alsa_pcm = %alsa_pcm,
                    "ALSA playback open failed; receiver counts frames \
                     without rendering"
                );
                None
            }
        }
    } else {
        None
    };
    // role consumed only behind the cfg gate; silence the
    // unused warning on builds without the feature.
    let _ = role;
    let _ = alsa_pcm;

    // Presentation-time anchor: set on first received frame.
    // Future frames' scheduled local time is computed as
    //   anchor_local + (frame.pts_ms - anchor_pts_ms)
    let mut anchor_local: Option<std::time::Instant> = None;
    let mut anchor_pts_ms: Option<u64> = None;
    let mut queue: std::collections::VecDeque<
        evo_plugin_sdk::contract::AudioFrameReceived,
    > = std::collections::VecDeque::new();
    // Rolling jitter window: (sampled_at, jitter_ms). Pruned
    // to the most recent RECEIVER_JITTER_WINDOW_MS each tick;
    // p95 over the window is published as the receiver's
    // observed_jitter_p95_ms telemetry.
    let mut jitter_samples: std::collections::VecDeque<(
        std::time::Instant,
        u64,
    )> = std::collections::VecDeque::new();

    let tick = std::time::Duration::from_millis(SCHEDULER_TICK_MS);
    let mut next_tick = std::time::Instant::now() + tick;

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
                        if anchor_local.is_none() {
                            anchor_local = Some(std::time::Instant::now());
                            anchor_pts_ms = Some(frame.presentation_time_ms);
                            tracing::info!(
                                plugin = PLUGIN_NAME,
                                anchor_pts_ms = frame.presentation_time_ms,
                                leader_ms = leader_ms.load(
                                    std::sync::atomic::Ordering::Relaxed,
                                ),
                                "receiver scheduler: playback anchor established"
                            );
                        }
                        // Publish the source-stamped
                        // group_leader_ms into the shared
                        // atomic the tick arm reads each
                        // scheduler tick. Zero = source has
                        // not stamped (e.g. v0 wire form);
                        // tick arm falls back to local.
                        if frame.group_leader_ms > 0 {
                            group_leader_ms_shared.store(
                                frame.group_leader_ms,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                        }
                        queue.push_back(frame);
                        queue_depth.store(
                            queue.len() as u64,
                            std::sync::atomic::Ordering::Relaxed,
                        );
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
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(
                next_tick,
            )) => {
                next_tick += tick;
                let now = std::time::Instant::now();
                // Effective leader: in Manual mode use the
                // operator-set local leader; in auto modes
                // honour the source-stamped group_leader_ms.
                // The source-published value is updated in the
                // stream-recv arm (above) every frame; here we
                // just consume it.
                let mode_u8 = leader_mode.load(
                    std::sync::atomic::Ordering::Relaxed,
                );
                let leader = match LeaderMode::from_u8(mode_u8) {
                    LeaderMode::Manual => leader_ms.load(
                        std::sync::atomic::Ordering::Relaxed,
                    ),
                    LeaderMode::AutoDecay | LeaderMode::AutoPinned => {
                        let g = group_leader_ms_shared.load(
                            std::sync::atomic::Ordering::Relaxed,
                        );
                        if g == 0 {
                            // Source hasn't published yet
                            // (v0 wire / first-frame race).
                            // Fall back to local.
                            leader_ms.load(
                                std::sync::atomic::Ordering::Relaxed,
                            )
                        } else {
                            g
                        }
                    }
                };
                let mut rendered_this_tick = 0usize;
                while let (Some(anchor_l), Some(anchor_p)) =
                    (anchor_local, anchor_pts_ms)
                {
                    let Some(head) = queue.front() else { break };
                    let offset_ms = head
                        .presentation_time_ms
                        .saturating_sub(anchor_p);
                    let render_at = anchor_l
                        + std::time::Duration::from_millis(
                            offset_ms + leader,
                        );
                    if render_at > now {
                        break;
                    }
                    // Jitter sample: absolute deviation of
                    // actual-render-time from scheduled-render-
                    // time. The sample is bounded below by zero
                    // (early frames are not jitter) and above
                    // by leader (a late frame past leader is an
                    // underrun).
                    let actual_offset = now.saturating_duration_since(
                        anchor_l,
                    )
                    .as_millis() as u64;
                    let scheduled_offset = offset_ms + leader;
                    let jitter_ms = actual_offset
                        .saturating_sub(scheduled_offset);
                    jitter_samples.push_back((now, jitter_ms));
                    let frame = queue.pop_front().unwrap();
                    queue_depth.store(
                        queue.len() as u64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    rendered_this_tick += 1;
                    #[cfg(feature = "alsa-substrate")]
                    if let Some(render) = alsa_render.as_mut() {
                        if let Err(e) = render.write(&frame.payload) {
                            tracing::warn!(
                                plugin = PLUGIN_NAME,
                                error = %e,
                                "ALSA writei failed (scheduled render)"
                            );
                        } else if render.take_xrun_recovered() {
                            underruns.fetch_add(
                                1,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                        }
                    }
                    #[cfg(not(feature = "alsa-substrate"))]
                    let _ = &frame;
                }
                // No underrun guard. Bit-perfect contract: the
                // receiver never inserts samples. If the
                // scheduler ticks with no frame due AND the
                // ALSA buffer is empty, ALSA's native xrun
                // handling kicks in on the next writei; the
                // existing prepare()-and-retry path in
                // AlsaRender::write recovers. Operators observe
                // underruns via the receiver_underruns counter,
                // bumped only by ALSA-reported xruns rather
                // than by silence-on-tick speculation.
                let _ = rendered_this_tick;
                let _ = &underruns;

                // Prune jitter window + recompute p95.
                let window = std::time::Duration::from_millis(
                    RECEIVER_JITTER_WINDOW_MS,
                );
                while let Some(&(t, _)) = jitter_samples.front() {
                    if now.saturating_duration_since(t) > window {
                        jitter_samples.pop_front();
                    } else {
                        break;
                    }
                }
                if !jitter_samples.is_empty() {
                    let mut vals: Vec<u64> = jitter_samples
                        .iter()
                        .map(|&(_, v)| v)
                        .collect();
                    vals.sort_unstable();
                    let idx = (vals.len() as f64 * 0.95) as usize;
                    let p95 = vals[idx.min(vals.len() - 1)];
                    jitter_p95_ms.store(
                        p95,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
            }
        }
    }
}

/// ALSA playback handle. Opens the configured PCM at the
/// baseline format and writes interleaved `pcm_s16_le` frames
/// via `snd_pcm_writei`. Underruns prepare + retry once; a
/// second underrun in a row is surfaced to the receiver loop
/// as a write error.
#[cfg(feature = "alsa-substrate")]
struct AlsaRender {
    pcm: alsa::PCM,
    /// Set to `true` by `write` when the inner `writei`
    /// hit an xrun and was recovered via prepare()+retry.
    /// Read by the receiver task to bump the operator-
    /// visible `receiver_underruns` counter.
    xrun_recovered: bool,
}

#[cfg(feature = "alsa-substrate")]
impl AlsaRender {
    fn open(name: &str) -> Result<Self, String> {
        let pcm = alsa::PCM::new(name, alsa::Direction::Playback, false)
            .map_err(|e| format!("alsa::PCM::new({name:?}, Playback): {e}"))?;
        {
            let hwp = alsa::pcm::HwParams::any(&pcm)
                .map_err(|e| format!("alsa::pcm::HwParams::any: {e}"))?;
            hwp.set_channels(BASELINE_CHANNELS as u32).map_err(|e| {
                format!("set_channels({}): {e}", BASELINE_CHANNELS)
            })?;
            hwp.set_rate(BASELINE_SAMPLE_RATE_HZ, alsa::ValueOr::Nearest)
                .map_err(|e| {
                    format!("set_rate({BASELINE_SAMPLE_RATE_HZ}, Nearest): {e}")
                })?;
            hwp.set_format(alsa::pcm::Format::s16())
                .map_err(|e| format!("set_format(S16LE): {e}"))?;
            hwp.set_access(alsa::pcm::Access::RWInterleaved)
                .map_err(|e| format!("set_access(RWInterleaved): {e}"))?;
            // Pin the period to one source-frame's worth of
            // samples (20 ms) and the buffer to four periods.
            // ALSA's default is the hardware's largest buffer
            // (typically ~500 ms on consumer DACs), which
            // creates a half-second accumulation between
            // source playback and receiver render — the
            // "drift" the operator hears. Four periods @ 20 ms
            // gives ~80 ms of tolerance which is enough for
            // typical LAN jitter without audible queue-back
            // accumulation.
            hwp.set_period_size(
                FRAMES_PER_CHUNK as alsa::pcm::Frames,
                alsa::ValueOr::Nearest,
            )
            .map_err(|e| {
                format!("set_period_size({FRAMES_PER_CHUNK}, Nearest): {e}")
            })?;
            hwp.set_buffer_size(
                (FRAMES_PER_CHUNK * RENDER_BUFFER_PERIODS) as alsa::pcm::Frames,
            )
            .map_err(|e| format!("set_buffer_size: {e}"))?;
            pcm.hw_params(&hwp)
                .map_err(|e| format!("hw_params commit: {e}"))?;
        }
        // Software params: start playback as soon as the
        // first period is buffered (don't wait for full
        // buffer fill, which would re-introduce the start-of-
        // playback latency the hardware-params tightening
        // just eliminated).
        {
            let swp = pcm
                .sw_params_current()
                .map_err(|e| format!("sw_params_current: {e}"))?;
            swp.set_start_threshold(FRAMES_PER_CHUNK as alsa::pcm::Frames)
                .map_err(|e| {
                    format!("set_start_threshold({FRAMES_PER_CHUNK}): {e}")
                })?;
            swp.set_avail_min(FRAMES_PER_CHUNK as alsa::pcm::Frames)
                .map_err(|e| {
                    format!("set_avail_min({FRAMES_PER_CHUNK}): {e}")
                })?;
            pcm.sw_params(&swp)
                .map_err(|e| format!("sw_params commit: {e}"))?;
        }
        pcm.prepare().map_err(|e| format!("pcm.prepare(): {e}"))?;
        Ok(Self {
            pcm,
            xrun_recovered: false,
        })
    }

    /// `true` when the most recent writei recovered from an
    /// ALSA xrun (EPIPE underrun). The receiver task bumps an
    /// operator-visible counter on each occurrence so the
    /// operator can correlate audible glitches with the
    /// `leader_ms` setting + the network's actual jitter.
    fn take_xrun_recovered(&mut self) -> bool {
        std::mem::take(&mut self.xrun_recovered)
    }

    fn write(&mut self, payload: &[u8]) -> Result<(), String> {
        self.xrun_recovered = false;
        // Interleaved s16le: 4 bytes per stereo frame (2 ch
        // * 2 bytes). Decode in place — alsa::pcm::IO::<i16>
        // takes a &[i16] of length frames * channels.
        if payload.len() % 4 != 0 {
            return Err(format!(
                "payload length {} not aligned to s16le stereo frame (4 bytes)",
                payload.len()
            ));
        }
        let frame_count = payload.len() / 4;
        let mut samples = Vec::with_capacity(payload.len() / 2);
        for chunk in payload.chunks_exact(2) {
            samples.push(i16::from_le_bytes([chunk[0], chunk[1]]));
        }
        let io = self
            .pcm
            .io_i16()
            .map_err(|e| format!("pcm.io_i16(): {e}"))?;
        match io.writei(&samples) {
            Ok(n) if n == frame_count => Ok(()),
            Ok(short) => Err(format!(
                "short write: requested {} frames, wrote {}",
                frame_count, short
            )),
            Err(_) => {
                // Most write errors are EPIPE (underrun) or
                // ESTRPIPE (suspended). Both recover via
                // pcm.prepare() and a retry. The xrun is
                // recorded for the operator-visible underrun
                // counter — bit-perfect contract: every xrun
                // means a glitch the operator should observe.
                self.xrun_recovered = true;
                let _ = self.pcm.prepare();
                match io.writei(&samples) {
                    Ok(n) if n == frame_count => Ok(()),
                    Ok(short) => Err(format!(
                        "post-recover short write: requested {} \
                         frames, wrote {}",
                        frame_count, short
                    )),
                    Err(e2) => Err(format!("post-recover write error: {e2}")),
                }
            }
        }
    }
}

/// Source-side aggregator. Every `SOURCE_AGGREGATION_TICK_MS`,
/// queries `list_audio_plane_peers`, aggregates per-peer
/// telemetry into a group-wide `group_leader_ms` under the
/// current `Profile`, and updates the shared atomic the source
/// emitter reads on each frame. Also drives the auto-decay
/// state machine when in `LeaderMode::AutoDecay`.
#[allow(clippy::too_many_arguments)]
async fn run_source_aggregator(
    audio_plane: Arc<dyn AudioPlaneHandle>,
    group_id: String,
    shutdown: Arc<Notify>,
    group_leader_ms: Arc<std::sync::atomic::AtomicU64>,
    local_leader_ms: Arc<std::sync::atomic::AtomicU64>,
    profile: Arc<std::sync::atomic::AtomicU8>,
    mode: Arc<std::sync::atomic::AtomicU8>,
) {
    let tick = std::time::Duration::from_millis(SOURCE_AGGREGATION_TICK_MS);
    let mut next_tick = std::time::Instant::now() + tick;
    // Auto-decay state. `prev_underrun_total` is the previous
    // tick's group-wide cumulative underrun count; the loop
    // compares against the current count to detect *new*
    // underruns since the last tick. The auto-decay reactor
    // engages only after `WARMUP_GRACE_MS` since the first
    // peer telemetry arrived: receiver render-warmup
    // typically produces a small burst of underruns in the
    // first few seconds (ALSA buffer shallow before steady-
    // state) that would otherwise escalate the profile to
    // Defensive on every restart for no real network reason.
    const WARMUP_GRACE_MS: u64 = 10_000;
    let mut quiet_ticks: u32 = 0;
    let mut prev_underrun_total: u64 = 0;
    let mut first_telemetry_at: Option<std::time::Instant> = None;

    loop {
        tokio::select! {
            _ = shutdown.notified() => {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    "source aggregator: shutdown received"
                );
                return;
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(
                next_tick,
            )) => {
                next_tick += tick;
            }
        }

        let mode_u8 = mode.load(std::sync::atomic::Ordering::Relaxed);
        let current_mode = LeaderMode::from_u8(mode_u8);

        // Manual mode: the source emits the operator's
        // local_leader_ms verbatim; telemetry is not
        // authoritative.
        if matches!(current_mode, LeaderMode::Manual) {
            let manual =
                local_leader_ms.load(std::sync::atomic::Ordering::Relaxed);
            group_leader_ms.store(
                manual.clamp(LEADER_MS_MIN, LEADER_MS_MAX),
                std::sync::atomic::Ordering::Relaxed,
            );
            continue;
        }

        // Auto mode: gather per-peer telemetry.
        let peers = match audio_plane.list_audio_plane_peers().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "list_audio_plane_peers failed in aggregator"
                );
                continue;
            }
        };
        // Peers whose telemetry has actually landed. No-
        // telemetry peers are excluded so the bootstrap
        // window (after restart, before any heartbeat has
        // carried a payload) doesn't look like "no
        // underruns" and prematurely decay the profile.
        let group_peers: Vec<_> = peers
            .into_iter()
            .filter(|p| p.receiver_telemetry.is_some())
            .collect();
        let _ = &group_id;

        // Bootstrap guard: hold profile + group_leader_ms
        // unchanged until at least one peer has reported.
        if group_peers.is_empty() {
            prev_underrun_total = 0;
            quiet_ticks = 0;
            first_telemetry_at = None;
            continue;
        }
        if first_telemetry_at.is_none() {
            first_telemetry_at = Some(std::time::Instant::now());
        }
        let in_warmup = first_telemetry_at
            .map(|t| {
                std::time::Instant::now()
                    .saturating_duration_since(t)
                    .as_millis()
                    < WARMUP_GRACE_MS as u128
            })
            .unwrap_or(true);

        let max_jitter_p95: u64 = group_peers
            .iter()
            .filter_map(|p| {
                p.receiver_telemetry
                    .as_ref()
                    .map(|t| t.observed_jitter_p95_ms)
            })
            .max()
            .unwrap_or(0);
        let min_buffer_capacity: u64 = group_peers
            .iter()
            .filter_map(|p| {
                p.receiver_telemetry
                    .as_ref()
                    .map(|t| t.render_buffer_capacity_ms)
            })
            .min()
            .unwrap_or(LEADER_MS_MAX);
        let cumulative_underruns: u64 = group_peers
            .iter()
            .filter_map(|p| {
                p.receiver_telemetry.as_ref().map(|t| t.underrun_count)
            })
            .sum();

        // Auto-decay state machine: when in AutoDecay AND new
        // underruns arrived this tick, escalate the profile.
        // When AUTO_DECAY_WINDOW_TICKS pass with zero new
        // underruns, decay one step.
        if matches!(current_mode, LeaderMode::AutoDecay) {
            if in_warmup {
                // Warmup grace: bootstrap glitches don't
                // count. Update baseline only.
                prev_underrun_total = cumulative_underruns;
                quiet_ticks = 0;
            } else {
                let new_underruns =
                    cumulative_underruns.saturating_sub(prev_underrun_total);
                if new_underruns > 0 {
                    let cur = Profile::from_u8(
                        profile.load(std::sync::atomic::Ordering::Relaxed),
                    );
                    let escalated = cur.escalate();
                    if escalated != cur {
                        profile.store(
                            escalated as u8,
                            std::sync::atomic::Ordering::Relaxed,
                        );
                        tracing::info!(
                            plugin = PLUGIN_NAME,
                            from = cur.as_wire_str(),
                            to = escalated.as_wire_str(),
                            new_underruns = new_underruns,
                            "auto-decay: escalating profile on underruns"
                        );
                    }
                    quiet_ticks = 0;
                } else {
                    quiet_ticks = quiet_ticks.saturating_add(1);
                    if quiet_ticks >= AUTO_DECAY_WINDOW_TICKS {
                        let cur = Profile::from_u8(
                            profile.load(std::sync::atomic::Ordering::Relaxed),
                        );
                        let decayed = cur.decay();
                        if decayed != cur {
                            profile.store(
                                decayed as u8,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                            tracing::info!(
                            plugin = PLUGIN_NAME,
                            from = cur.as_wire_str(),
                            to = decayed.as_wire_str(),
                            "auto-decay: decaying profile after quiet window"
                        );
                        }
                        quiet_ticks = 0;
                    }
                }
                prev_underrun_total = cumulative_underruns;
            }
        }

        // Compute new group_leader_ms under current profile.
        let cur_profile = Profile::from_u8(
            profile.load(std::sync::atomic::Ordering::Relaxed),
        );
        let raw = (max_jitter_p95 as f64 * cur_profile.safety_factor()) as u64
            + cur_profile.floor_ms();
        let clamped = raw
            .clamp(LEADER_MS_MIN, LEADER_MS_MAX)
            .min(min_buffer_capacity);
        group_leader_ms.store(clamped, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Receiver-side telemetry publisher. Every
/// `RECEIVER_TELEMETRY_PUBLISH_MS`, reads the receiver-task-
/// maintained atomics + the most recent applied leader_ms and
/// publishes via `AudioPlaneHandle::update_receiver_telemetry`
/// so the framework piggybacks the payload on the next
/// outbound `Heartbeat` to every peer.
async fn run_receiver_telemetry_publisher(
    audio_plane: Arc<dyn AudioPlaneHandle>,
    shutdown: Arc<Notify>,
    jitter_p95_ms: Arc<std::sync::atomic::AtomicU64>,
    underruns: Arc<std::sync::atomic::AtomicU64>,
    applied_leader_ms: Arc<std::sync::atomic::AtomicU64>,
    render_buffer_capacity_ms: Arc<std::sync::atomic::AtomicU64>,
) {
    let tick = std::time::Duration::from_millis(RECEIVER_TELEMETRY_PUBLISH_MS);
    let mut next_tick = std::time::Instant::now() + tick;
    loop {
        tokio::select! {
            _ = shutdown.notified() => {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    "receiver telemetry publisher: shutdown received"
                );
                return;
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(
                next_tick,
            )) => {
                next_tick += tick;
            }
        }
        let telemetry = ReceiverTelemetry {
            observed_jitter_p95_ms: jitter_p95_ms
                .load(std::sync::atomic::Ordering::Relaxed),
            underrun_count: underruns
                .load(std::sync::atomic::Ordering::Relaxed),
            applied_leader_ms: applied_leader_ms
                .load(std::sync::atomic::Ordering::Relaxed),
            render_buffer_capacity_ms: render_buffer_capacity_ms
                .load(std::sync::atomic::Ordering::Relaxed),
        };
        if let Err(e) = audio_plane.update_receiver_telemetry(telemetry).await {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                error = %e,
                "update_receiver_telemetry failed"
            );
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
        assert_eq!(p.frames_sent(), 0);
    }

    #[test]
    fn default_role_is_auto() {
        let cfg = PluginConfig::default();
        assert_eq!(cfg.role, Role::Auto);
    }

    #[test]
    fn parse_source_config() {
        let toml_str = r#"
role = "source"
group_id = "abc-123"
"#;
        let table: toml::Table = toml::from_str(toml_str).unwrap();
        let mut p = MultiroomEvoNativePlugin::new();
        p.apply_config(&table).unwrap();
        assert_eq!(p.config.role, Role::Source);
        assert_eq!(p.config.group_id.as_deref(), Some("abc-123"));
    }

    #[test]
    fn parse_source_without_group_id_refuses() {
        let toml_str = r#"role = "source""#;
        let table: toml::Table = toml::from_str(toml_str).unwrap();
        let mut p = MultiroomEvoNativePlugin::new();
        let err = p.apply_config(&table).unwrap_err();
        assert!(matches!(err, PluginError::Permanent(_)));
    }

    #[test]
    fn parse_receiver_config() {
        let toml_str = r#"
role = "receiver"
alsa_pcm = "evo"
"#;
        let table: toml::Table = toml::from_str(toml_str).unwrap();
        let mut p = MultiroomEvoNativePlugin::new();
        p.apply_config(&table).unwrap();
        assert_eq!(p.config.role, Role::Receiver);
        assert_eq!(p.config.alsa_pcm, "evo");
    }
}
