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
//! ## Initial scope
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
//! follow / encoded codecs) ride later iterations per the
//! reliability bar in `project-multiroom-position` memory.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::manual_async_fn)]

use std::future::Future;
use std::sync::Arc;

use evo_plugin_sdk::contract::audio_plane::{AudioFrameSeed, AudioPlaneHandle};
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
    "audio.multiroom.frame_trace.snapshot",
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

/// Sample rate the baseline source generator emits at
/// (and the receiver expects). Matches the typical ALSA
/// hardware default; resampling lands as a later improvement.
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
    /// Presentation-time leader in milliseconds: how far
    /// ahead of audible render the source emits each frame,
    /// and the network-latency + jitter budget the receiver
    /// allocates before scheduling each frame's writei into
    /// ALSA. Lower = lower end-to-end latency, less
    /// tolerance for slow networks. Higher = more tolerance
    /// for slow networks, slightly higher latency. The
    /// receiver schedules every frame against its
    /// presentation_time_ms anchor so playback is
    /// bit-perfect (no sample drops, no sample inserts)
    /// regardless of jitter inside the budget. Operators
    /// tune live via `multiroom.set_leader_ms`.
    #[serde(default = "default_leader_ms")]
    leader_ms: u64,
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            role: default_role(),
            group_id: None,
            alsa_pcm: default_alsa_pcm(),
            source_pcm: String::new(),
            leader_ms: default_leader_ms(),
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
    /// Shared with the source + receiver tasks so live
    /// updates via `multiroom.set_leader_ms` take effect
    /// without a plugin reload.
    leader_ms: Arc<std::sync::atomic::AtomicU64>,
    /// Receiver-side underrun counter: incremented every
    /// time the scheduler reaches a render tick with no
    /// frame in the queue at or before the due time. Each
    /// underrun is one period of silence written to ALSA
    /// to keep playback continuous.
    receiver_underruns: Arc<std::sync::atomic::AtomicU64>,
    /// Receiver-side queue depth (most recent observed).
    /// Snapshot for `multiroom.get_status`; updated by the
    /// receiver scheduler each tick.
    receiver_queue_depth: Arc<std::sync::atomic::AtomicU64>,
    /// Source-host audible-time trace aggregator state.
    /// Populated when the plugin loads in source role; the
    /// wire-op `audio.multiroom.frame_trace.snapshot` reads
    /// from here; the source-capture task writes through.
    #[cfg(feature = "alsa-substrate")]
    trace_state: Option<Arc<TraceState>>,
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
            receiver_underruns: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            receiver_queue_depth: Arc::new(std::sync::atomic::AtomicU64::new(
                0,
            )),
            #[cfg(feature = "alsa-substrate")]
            trace_state: None,
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
                    let task = if source_pcm.is_empty() {
                        tokio::spawn(async move {
                            run_source_tone_generator(
                                handle, group_id, sent, shutdown,
                            )
                            .await;
                        })
                    } else {
                        #[cfg(feature = "alsa-substrate")]
                        {
                            let pcm = source_pcm.clone();
                            // Construct the audible-time trace
                            // aggregator state for this source-
                            // role admission. Window size 100
                            // records (one record per (sequence,
                            // receiver) tuple); operator-tunable
                            // via plugin config in a future commit.
                            let trace_state = Arc::new(TraceState::new(100));
                            self.trace_state = Some(Arc::clone(&trace_state));
                            let task_trace_state =
                                Some(Arc::clone(&trace_state));
                            tokio::spawn(async move {
                                run_source_capture_task(
                                    handle,
                                    group_id,
                                    sent,
                                    shutdown,
                                    pcm,
                                    task_trace_state,
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
                            tokio::spawn(async move {
                                run_source_tone_generator(
                                    handle, group_id, sent, shutdown,
                                )
                                .await;
                            })
                        }
                    };
                    self.source_task = Some(task);
                    if source_pcm.is_empty() {
                        tracing::info!(
                            plugin = PLUGIN_NAME,
                            group_id = %self.config.group_id.as_deref().unwrap_or(""),
                            "source role engaged: synthetic 440 Hz tone fan-out running"
                        );
                    } else {
                        tracing::info!(
                            plugin = PLUGIN_NAME,
                            group_id = %self.config.group_id.as_deref().unwrap_or(""),
                            source_pcm = %source_pcm,
                            "source role engaged: ALSA capture fan-out running"
                        );
                    }
                    // One-renderer-pipeline: when the source
                    // role is configured with an alsa_pcm
                    // (the source-local DAC target), ALSO
                    // spawn the receiver task. The framework
                    // self-loopbacks source frames onto
                    // `audio_frame_tx` with from_device_id =
                    // local_id; the receiver task subscribed
                    // here renders them through the same
                    // scheduler the remote receivers use, so
                    // source-local DAC + every remote
                    // receiver share one render path. The
                    // MPD config that feeds snd-aloop should
                    // narrow to a single audio_output
                    // (evo-aloop only); the source-local DAC
                    // is no longer driven by a parallel MPD
                    // audio_output but by this receiver task
                    // reading the same scheduled frames.
                    if !self.config.alsa_pcm.is_empty() {
                        let counter = Arc::clone(&self.frames_received);
                        let recv_shutdown = Arc::clone(&self.shutdown);
                        let recv_handle = Arc::clone(&audio_plane);
                        let alsa_pcm = self.config.alsa_pcm.clone();
                        let role = self.config.role;
                        let leader_ms = Arc::clone(&self.leader_ms);
                        let underruns = Arc::clone(&self.receiver_underruns);
                        let queue_depth =
                            Arc::clone(&self.receiver_queue_depth);
                        let task = tokio::spawn(async move {
                            run_receiver_task(
                                recv_handle,
                                counter,
                                recv_shutdown,
                                alsa_pcm,
                                role,
                                leader_ms,
                                underruns,
                                queue_depth,
                            )
                            .await;
                        });
                        self.receiver_task = Some(task);
                        tracing::info!(
                            plugin = PLUGIN_NAME,
                            alsa_pcm = %self.config.alsa_pcm,
                            leader_ms = self.config.leader_ms,
                            "source-local DAC receiver-task engaged: \
                             one-renderer-pipeline (source frames \
                             self-loopback onto the local broadcast)"
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
                    let underruns = Arc::clone(&self.receiver_underruns);
                    let queue_depth = Arc::clone(&self.receiver_queue_depth);
                    let task = tokio::spawn(async move {
                        run_receiver_task(
                            handle,
                            counter,
                            shutdown,
                            alsa_pcm,
                            role,
                            leader_ms,
                            underruns,
                            queue_depth,
                        )
                        .await;
                    });
                    self.receiver_task = Some(task);
                    tracing::info!(
                        plugin = PLUGIN_NAME,
                        role = self.config.role.as_wire_str(),
                        alsa_pcm = %self.config.alsa_pcm,
                        leader_ms = self.config.leader_ms,
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
                    let payload = serde_json::json!({
                        "v": PAYLOAD_VERSION,
                        "role": self.config.role.as_wire_str(),
                        "group_id": self.config.group_id,
                        "alsa_pcm": self.config.alsa_pcm,
                        "source_pcm": self.config.source_pcm,
                        "leader_ms": self.leader_ms(),
                        "leader_ms_min": LEADER_MS_MIN,
                        "leader_ms_max": LEADER_MS_MAX,
                        "frames_sent": self.frames_sent(),
                        "frames_received": self.frames_received(),
                        "receiver_queue_depth": self.receiver_queue_depth(),
                        "receiver_underruns": self.receiver_underruns(),
                    });
                    let body = serde_json::to_vec(&payload).map_err(|e| {
                        PluginError::Permanent(format!(
                            "encode multiroom.get_status response: {e}"
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
                "audio.multiroom.frame_trace.snapshot" => {
                    #[cfg(feature = "alsa-substrate")]
                    let (records, window_size) = match &self.trace_state {
                        Some(state) => (state.snapshot(), state.window_size),
                        None => (Vec::new(), 0),
                    };
                    #[cfg(not(feature = "alsa-substrate"))]
                    let (records, window_size): (
                        Vec<serde_json::Value>,
                        usize,
                    ) = (Vec::new(), 0);
                    let payload = serde_json::json!({
                        "v": PAYLOAD_VERSION,
                        "group_id": self.config.group_id,
                        "window_size": window_size,
                        "records": records,
                        "last_update_at_ms": std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0),
                    });
                    let body = serde_json::to_vec(&payload).map_err(|e| {
                        PluginError::Permanent(format!(
                            "encode frame_trace.snapshot response: {e}"
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
        // emission. `elapsed` already advances at the emit
        // cadence (one tick = chunk_period); adding
        // `sequence * 20` on top double-counts and stretches
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
    trace_state: Option<Arc<TraceState>>,
) {
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;

    // Capacity covers ~0.5 s of frames; if we fall further
    // behind than that the loopback playback half is corrupt
    // already.
    // Channel item carries the captured chunk + the source-
    // side audible-time-trace stage timestamps the capture
    // thread observed at its own call sites: stage 3a is
    // the moment `io.readi` returned with this chunk's
    // samples; stage 3b is the moment immediately before
    // `tx.send` queues the chunk onto this channel. Both
    // are computed via `audio_plane.monotonic_ns()` so the
    // capture-thread's timestamps reference the same epoch
    // every other audible-time-trace observation on this
    // node uses (the framework runtime's epoch).
    let (tx, mut rx) = tokio::sync::mpsc::channel::<CaptureChunk>(32);

    let capture_shutdown = Arc::clone(&shutdown);
    let capture_pcm = source_pcm.clone();
    let capture_audio_plane = Arc::clone(&audio_plane);
    let capture_thread = std::thread::Builder::new()
        .name("multiroom-capture".into())
        .spawn(move || {
            run_capture_thread(
                capture_pcm,
                tx,
                capture_shutdown,
                capture_audio_plane,
            );
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

    // Audible-time trace state. The source task captures
    // stages 3a / 3b (via the CaptureChunk it receives), 4a /
    // 4b / 5a (locally), then subscribes to the framework's
    // FrameSendEvent broadcast to refine stage 5a per
    // recipient + the FrameTraceReport broadcast to complete
    // each (sequence, receiver) record with stages 5b / 6 /
    // 7. Completed records land in the shared TraceState's
    // rolling window — the wire-op `audio.multiroom.frame_
    // trace.snapshot` reads from there. The state Arc is
    // injected via `trace_state`; when `None` (e.g. unit
    // tests bypassing the aggregator wire-up) the per-frame
    // accounting is skipped without affecting fan-out.
    use std::collections::HashMap;

    let mut source_pending: HashMap<u64, SourceTracePartial> = HashMap::new();
    let mut recipient_pending: HashMap<(u64, String), RecipientTracePartial> =
        HashMap::new();

    let mut frame_send_rx = if trace_state.is_some() {
        match audio_plane.subscribe_frame_send_events().await {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "subscribe_frame_send_events failed; trace records \
                     will omit stage 5a (wire_send_ns)"
                );
                None
            }
        }
    } else {
        None
    };
    let mut frame_trace_rx = if trace_state.is_some() {
        match audio_plane.subscribe_frame_trace_reports().await {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "subscribe_frame_trace_reports failed; trace records \
                     will be source-only"
                );
                None
            }
        }
    } else {
        None
    };

    // Bounded eviction: at most TRACE_PENDING_MAX entries in
    // each pending map; oldest sequence drops first when an
    // entry is inserted past the bound. Keeps memory + Map
    // ops constant-time regardless of how long the source
    // role runs.
    const TRACE_PENDING_MAX: usize = 256;

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
                let CaptureChunk {
                    capture_readi_return_ns,
                    mpsc_send_ns,
                    pcm,
                } = match chunk {
                    Some(c) => c,
                    None => {
                        tracing::debug!(
                            plugin = PLUGIN_NAME,
                            "capture channel closed; source task exiting"
                        );
                        break;
                    }
                };
                // Audible-time trace stage 4a: `rx.recv`
                // returned with the chunk.
                let mpsc_recv_ns = audio_plane.monotonic_ns();
                // PTS = source-local monotonic time at this
                // frame's emission. See run_source_tone_generator
                // for the bit-perfect contract — `elapsed`
                // already advances at the emit cadence.
                let presentation_time_ms =
                    start_monotonic.elapsed().as_millis() as u64;
                let seed = AudioFrameSeed {
                    sequence,
                    presentation_time_ms,
                    codec: "pcm_s16_le".to_string(),
                    rate_hz: BASELINE_SAMPLE_RATE_HZ,
                    channels: BASELINE_CHANNELS,
                    payload_b64: B64.encode(&pcm),
                };
                // Audible-time trace stage 4b: immediately
                // before invoking `fan_out_audio_frame`.
                let fanout_enter_ns = audio_plane.monotonic_ns();
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
                // Record the source-side partial. The aggregator
                // matches FrameSendEvent + FrameTraceReport
                // entries against this by sequence.
                if trace_state.is_some() {
                    if source_pending.len() >= TRACE_PENDING_MAX {
                        if let Some(&oldest_seq) =
                            source_pending.keys().min()
                        {
                            source_pending.remove(&oldest_seq);
                        }
                    }
                    source_pending.insert(
                        sequence,
                        SourceTracePartial {
                            presentation_time_ms,
                            capture_readi_return_ns,
                            mpsc_send_ns,
                            mpsc_recv_ns,
                            fanout_enter_ns,
                        },
                    );
                }
                sequence = sequence.saturating_add(1);
            }
            // FrameSendEvent — per-recipient stage 5a (wire_send_ns).
            Ok(ev) = async {
                match frame_send_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            }, if frame_send_rx.is_some() => {
                let key = (ev.sequence, ev.receiver_device_id.clone());
                if recipient_pending.len() >= TRACE_PENDING_MAX {
                    if let Some(oldest) =
                        recipient_pending.keys().min_by_key(|k| k.0).cloned()
                    {
                        recipient_pending.remove(&oldest);
                    }
                }
                let entry = recipient_pending
                    .entry(key.clone())
                    .or_insert_with(RecipientTracePartial::default);
                entry.wire_send_ns = Some(ev.wire_send_ns);
                try_complete_record(
                    &key,
                    &source_pending,
                    &mut recipient_pending,
                    trace_state.as_deref(),
                ).await;
            }
            // FrameTraceReport — receiver back-report stages 5b / 6 / 7.
            Ok(rep) = async {
                match frame_trace_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            }, if frame_trace_rx.is_some() => {
                let key = (rep.sequence, rep.from_device_id.clone());
                if recipient_pending.len() >= TRACE_PENDING_MAX {
                    if let Some(oldest) =
                        recipient_pending.keys().min_by_key(|k| k.0).cloned()
                    {
                        recipient_pending.remove(&oldest);
                    }
                }
                let entry = recipient_pending
                    .entry(key.clone())
                    .or_insert_with(RecipientTracePartial::default);
                entry.wire_recv_ns = Some(rep.wire_recv_ns);
                entry.scheduler_dequeue_ns = Some(rep.scheduler_dequeue_ns);
                entry.writei_return_ns = Some(rep.writei_return_ns);
                try_complete_record(
                    &key,
                    &source_pending,
                    &mut recipient_pending,
                    trace_state.as_deref(),
                ).await;
            }
        }
    }

    // Do NOT join the capture thread on the async unload
    // path. Joining would block the async task waiting for
    // the OS thread to exit; the thread polls `tx.is_closed()`
    // between reads to detect that exit signal — but `rx`
    // lives in THIS task's frame, so closing the channel
    // only happens after this function returns. Joining
    // here is a deadlock: thread waits on the channel
    // closing, channel closing waits on the function
    // returning, function returning waits on the join. The
    // framework's 10 s plugin-shutdown deadline expires,
    // SIGKILL fires, systemd's 90 s TimeoutStopSec runs
    // out, the restart takes ~90 s.
    //
    // Drop the JoinHandle instead — std::thread detaches.
    // When this function returns, the local `rx` drops,
    // `tx.is_closed()` becomes true, and the thread exits
    // on its next loop iteration. With cooperative ALSA
    // capture (non-blocking PCM + `pcm.wait(100ms)`), the
    // thread polls the closed-channel signal within 100 ms
    // regardless of whether MPD is still feeding the
    // loopback. ALSA PCM handle drops at thread scope exit.
    drop(capture_thread);
}

/// One capture-thread chunk plus the source-side stage 3a /
/// 3b audible-time trace timestamps the capture thread
/// observed at its own call sites. The async source-capture
/// task picks up the chunk + the two timestamps from the
/// channel and adds stage 4a / 4b at its own call sites
/// before handing the encoded payload to
/// `fan_out_audio_frame`.
#[cfg(feature = "alsa-substrate")]
#[derive(Debug, Clone)]
struct CaptureChunk {
    /// Framework-monotonic ns at which `io.readi(&buf)`
    /// returned with this chunk's samples. Stage 3a.
    capture_readi_return_ns: u64,
    /// Framework-monotonic ns immediately before
    /// `tx.try_send(this_chunk)` queued the chunk onto the
    /// async channel. Stage 3b.
    mpsc_send_ns: u64,
    /// PCM samples (pcm_s16_le, interleaved).
    pcm: Vec<u8>,
}

/// Source-side partial trace record observed at the moment
/// the source-capture async task hands the chunk to
/// `fan_out_audio_frame`. Carries every stage the source
/// node observes for this sequence; the per-recipient
/// `wire_send_ns` + the receiver-back-reported triple
/// (wire_recv_ns / scheduler_dequeue_ns / writei_return_ns)
/// arrive separately via the audio-plane's broadcast streams
/// and are joined in [`TraceState`].
#[derive(Debug, Clone)]
struct SourceTracePartial {
    presentation_time_ms: u64,
    capture_readi_return_ns: u64,
    mpsc_send_ns: u64,
    mpsc_recv_ns: u64,
    fanout_enter_ns: u64,
}

/// Per-recipient partial trace record. Built incrementally
/// as the source observes the framework's `FrameSendEvent`
/// for this `(sequence, receiver_device_id)` pair (stage 5a)
/// and then as the receiver back-reports its post-decode +
/// post-dequeue + post-writei timestamps (stages 5b / 6 / 7).
#[derive(Debug, Clone, Default)]
struct RecipientTracePartial {
    wire_send_ns: Option<u64>,
    wire_recv_ns: Option<u64>,
    scheduler_dequeue_ns: Option<u64>,
    writei_return_ns: Option<u64>,
}

/// Complete per-frame, per-recipient audible-time trace
/// record. The rolling-window state in [`TraceState`]
/// publishes these via the `audio.multiroom.frame_trace`
/// subject + the `audio.multiroom.frame_trace.snapshot`
/// wire-op.
#[derive(Debug, Clone, serde::Serialize)]
struct FrameTraceRecord {
    sequence: u64,
    receiver_device_id: String,
    presentation_time_ms: u64,
    source_capture_readi_return_ns: u64,
    source_mpsc_send_ns: u64,
    source_mpsc_recv_ns: u64,
    source_fanout_enter_ns: u64,
    source_wire_send_ns: u64,
    receiver_wire_recv_ns: u64,
    receiver_scheduler_dequeue_ns: u64,
    receiver_writei_return_ns: u64,
    clock_offset_ns: i64,
}

/// Source-host audible-time trace aggregator state. Holds a
/// bounded rolling window of completed [`FrameTraceRecord`]
/// instances. The source-capture task writes through here;
/// the wire-op handler `audio.multiroom.frame_trace.snapshot`
/// reads from it; a separate publisher task observes its
/// updates and emits the canonical
/// `audio.multiroom.frame_trace` subject value.
#[derive(Debug)]
struct TraceState {
    window: std::sync::Mutex<std::collections::VecDeque<FrameTraceRecord>>,
    /// Maximum count of records retained in the rolling
    /// window. Operator-configurable in a future commit;
    /// default 100 today.
    window_size: usize,
}

#[cfg(feature = "alsa-substrate")]
impl TraceState {
    fn new(window_size: usize) -> Self {
        Self {
            window: std::sync::Mutex::new(
                std::collections::VecDeque::with_capacity(window_size),
            ),
            window_size,
        }
    }

    fn push(&self, rec: FrameTraceRecord) {
        if let Ok(mut w) = self.window.lock() {
            if w.len() >= self.window_size {
                w.pop_front();
            }
            w.push_back(rec);
        }
    }

    fn snapshot(&self) -> Vec<FrameTraceRecord> {
        self.window
            .lock()
            .map(|w| w.iter().cloned().collect())
            .unwrap_or_default()
    }
}

/// Helper used by the source-capture task on every
/// `FrameSendEvent` and every `FrameTraceReport` arrival.
/// Looks at the `(sequence, receiver_device_id)` recipient
/// partial: if every receiver-side field is populated AND the
/// source-side partial for the same sequence still exists,
/// composes a [`FrameTraceRecord`], pushes into the rolling
/// window, and removes the recipient entry. The source-side
/// partial stays in place because multiple recipients may
/// reference the same sequence; the bounded eviction in the
/// caller handles the per-sequence cleanup.
#[cfg(feature = "alsa-substrate")]
async fn try_complete_record(
    key: &(u64, String),
    source_pending: &std::collections::HashMap<u64, SourceTracePartial>,
    recipient_pending: &mut std::collections::HashMap<
        (u64, String),
        RecipientTracePartial,
    >,
    trace_state: Option<&TraceState>,
) {
    let Some(state) = trace_state else { return };
    let Some(rec_partial) = recipient_pending.get(key) else {
        return;
    };
    let (
        Some(wire_send_ns),
        Some(wire_recv_ns),
        Some(scheduler_dequeue_ns),
        Some(writei_return_ns),
    ) = (
        rec_partial.wire_send_ns,
        rec_partial.wire_recv_ns,
        rec_partial.scheduler_dequeue_ns,
        rec_partial.writei_return_ns,
    )
    else {
        return;
    };
    let Some(src_partial) = source_pending.get(&key.0) else {
        return;
    };
    let record = FrameTraceRecord {
        sequence: key.0,
        receiver_device_id: key.1.clone(),
        presentation_time_ms: src_partial.presentation_time_ms,
        source_capture_readi_return_ns: src_partial.capture_readi_return_ns,
        source_mpsc_send_ns: src_partial.mpsc_send_ns,
        source_mpsc_recv_ns: src_partial.mpsc_recv_ns,
        source_fanout_enter_ns: src_partial.fanout_enter_ns,
        source_wire_send_ns: wire_send_ns,
        receiver_wire_recv_ns: wire_recv_ns,
        receiver_scheduler_dequeue_ns: scheduler_dequeue_ns,
        receiver_writei_return_ns: writei_return_ns,
        // TODO: source the sync probe's per-peer offset from
        // the audio-plane's ClockSyncRuntime when the SDK
        // surfaces it for plugins. For now this field reports
        // 0 — the same-node deltas (capture -> fanout, etc.)
        // are useful without it; cross-node analyses subtract
        // it manually from the sync-probe wire-op until then.
        clock_offset_ns: 0,
    };
    // High-frequency per-frame trace surface. Canonical
    // operator path is the `audio.multiroom.frame_trace`
    // published subject + the `audio.multiroom.frame_trace.
    // snapshot` wire-op (operator CLI: `evo-plugin-tool
    // admin group frame-trace`). This `trace!` exists as an
    // opt-in debugging surface only, enabled by
    // `RUST_LOG=org_evoframework_multiroom_evo_native=trace`;
    // it must NOT fire at default log levels because at
    // 50 fps × N receivers it would flood the journal and
    // contend with the realtime audio runtime.
    tracing::trace!(
        plugin = PLUGIN_NAME,
        seq = record.sequence,
        recv = %record.receiver_device_id,
        pts_ms = record.presentation_time_ms,
        s3a_ns = record.source_capture_readi_return_ns,
        s3b_ns = record.source_mpsc_send_ns,
        s4a_ns = record.source_mpsc_recv_ns,
        s4b_ns = record.source_fanout_enter_ns,
        s5a_ns = record.source_wire_send_ns,
        s5b_ns = record.receiver_wire_recv_ns,
        s6_ns = record.receiver_scheduler_dequeue_ns,
        s7_ns = record.receiver_writei_return_ns,
        clk_off_ns = record.clock_offset_ns,
        "frame-trace record completed"
    );
    state.push(record);
    recipient_pending.remove(key);
}

/// OS-thread body that owns the ALSA capture handle. Loops
/// reading `FRAMES_PER_CHUNK` frames at a time, pushing each
/// chunk onto the async-side channel. Drops the oldest chunk
/// on channel pressure rather than blocking the capture loop
/// — see `run_source_capture_task`'s docblock for why.
#[cfg(feature = "alsa-substrate")]
fn run_capture_thread(
    source_pcm: String,
    tx: tokio::sync::mpsc::Sender<CaptureChunk>,
    shutdown: Arc<Notify>,
    audio_plane: Arc<dyn AudioPlaneHandle>,
) {
    // Open the capture PCM NON-BLOCKING. In blocking mode
    // `io.readi()` parks indefinitely waiting for samples
    // (e.g. when MPD stops feeding the loopback playback
    // half); the thread can never reach the `tx.is_closed()`
    // poll between iterations and the async unload path
    // hangs until the framework's 10 s plugin-shutdown
    // deadline fires SIGKILL, then systemd's 90 s
    // TimeoutStopSec runs out. Combined with `pcm.wait`
    // below, this thread polls cooperatively every 100 ms.
    let pcm = match alsa::PCM::new(&source_pcm, alsa::Direction::Capture, true)
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
        // Explicit period + buffer time on capture. Without
        // this snd-aloop's defaults yield a ~10-second
        // capture buffer (524288 frames @ 48 kHz observed
        // empirically) which makes capture-side audible
        // latency dependent on a startup-timing race between
        // MPD's first writei and the capture-thread's first
        // readi — sometimes ~10 ms (Perfect), sometimes
        // ~1 second (way behind), nothing deterministic in
        // between. Target ALSA period+buffer in TIME (us)
        // so each device tier picks its tightest natively-
        // supported size; snd-aloop honours 20 ms / 80 ms
        // cleanly. The audible-latency budget is now
        // structural, not random.
        if let Err(e) = hwp.set_period_time_near(20_000, alsa::ValueOr::Nearest)
        {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "set_period_time_near (capture) failed"
            );
            return;
        }
        if let Err(e) = hwp.set_buffer_time_near(80_000, alsa::ValueOr::Nearest)
        {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "set_buffer_time_near (capture) failed"
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
        // Read back the actually-negotiated values + log.
        // Operators see capture-side audible latency =
        // buffer_ms here. Combined with leader_ms (200 ms)
        // and the renderer's own buffer_ms, this is the
        // honest end-to-end budget.
        match pcm.hw_params_current() {
            Ok(current) => {
                let pf = current.get_period_size().unwrap_or(0);
                let bf = current.get_buffer_size().unwrap_or(0);
                let pm = (pf as u64 * 1000) / BASELINE_SAMPLE_RATE_HZ as u64;
                let bm = (bf as u64 * 1000) / BASELINE_SAMPLE_RATE_HZ as u64;
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    source_pcm = %source_pcm,
                    period_frames = pf,
                    period_ms = pm,
                    buffer_frames = bf,
                    buffer_ms = bm,
                    "ALSA capture hw_params negotiated"
                );
            }
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "hw_params_current readback (capture) failed"
                );
            }
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
    // Non-blocking capture requires explicit start() to
    // transition from Prepared to Running. In blocking mode
    // the first `readi()` implicitly starts the stream; in
    // non-blocking mode `readi()` returns EAGAIN immediately
    // without starting, so `wait()` perpetually times out
    // and no audio ever flows.
    if let Err(e) = pcm.start() {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            error = %e,
            "pcm.start (capture) failed"
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

    // shutdown.notified() is an async Notify — we cannot
    // await it from this sync std::thread. The cooperative
    // shutdown signal is the closed channel (`tx.is_closed()`
    // becomes true when the async receiver_task returns and
    // drops `rx`). `pcm.wait(Some(100))` parks for at most
    // 100 ms before returning, so the closed-channel check
    // runs at least every 100 ms even when the loopback
    // playback half has no producer (MPD stopped).
    let _ = &shutdown;
    let mut buf: Vec<i16> =
        vec![0; FRAMES_PER_CHUNK * BASELINE_CHANNELS as usize];
    loop {
        if tx.is_closed() {
            break;
        }
        match pcm.wait(Some(100)) {
            Ok(true) => {
                // Data ready; non-blocking readi returns
                // whatever's available (or EAGAIN if the
                // wait/read raced).
                match io.readi(&mut buf) {
                    Ok(frames_read) if frames_read > 0 => {
                        // Audible-time trace stage 3a:
                        // `io.readi` returned with samples.
                        let capture_readi_return_ns =
                            audio_plane.monotonic_ns();
                        let mut pcm_bytes = Vec::with_capacity(
                            frames_read * BASELINE_CHANNELS as usize * 2,
                        );
                        for s in
                            &buf[..frames_read * BASELINE_CHANNELS as usize]
                        {
                            pcm_bytes.extend_from_slice(&s.to_le_bytes());
                        }
                        // Audible-time trace stage 3b:
                        // immediately before queueing onto
                        // the async channel.
                        let mpsc_send_ns = audio_plane.monotonic_ns();
                        // Soft-drop on channel full: we are
                        // the producer of a real-time stream;
                        // back-pressuring would corrupt the
                        // loopback playback half upstream.
                        let _ = tx.try_send(CaptureChunk {
                            capture_readi_return_ns,
                            mpsc_send_ns,
                            pcm: pcm_bytes,
                        });
                    }
                    Ok(_) => {
                        // Zero frames — try again on next
                        // wait cycle.
                    }
                    Err(e) => {
                        // EAGAIN (errno 11) is expected on
                        // non-blocking PCM when no data is
                        // ready — silent skip. Other errors
                        // (EPIPE underrun / ESTRPIPE suspend
                        // / etc.) recover via prepare().
                        if e.errno() != 11 {
                            tracing::warn!(
                                plugin = PLUGIN_NAME,
                                error = %e,
                                "ALSA readi (capture) failed; recovering"
                            );
                            let _ = pcm.prepare();
                            let _ = pcm.start();
                        }
                    }
                }
            }
            Ok(false) => {
                // wait timeout — loop, recheck closed
                // channel, wait again. This is the
                // cooperative-shutdown path when no audio
                // is flowing through the loopback.
            }
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "ALSA pcm.wait (capture) failed; recovering"
                );
                let _ = pcm.prepare();
                let _ = pcm.start();
            }
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
    underruns: Arc<std::sync::atomic::AtomicU64>,
    queue_depth: Arc<std::sync::atomic::AtomicU64>,
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
    let mut alsa_render = if !alsa_pcm.is_empty() {
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
                let leader = leader_ms.load(
                    std::sync::atomic::Ordering::Relaxed,
                );
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
                    let frame = queue.pop_front().unwrap();
                    queue_depth.store(
                        queue.len() as u64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    rendered_this_tick += 1;
                    // Audible-time trace stage 6: scheduler
                    // dequeue moment. Captured before the
                    // writei call so the stage_6 -> stage_7
                    // delta isolates the writei cost from
                    // the scheduler-internal cost.
                    let scheduler_dequeue_ns = audio_plane.monotonic_ns();
                    #[cfg(feature = "alsa-substrate")]
                    if let Some(render) = alsa_render.as_mut() {
                        if let Err(e) = render.write(&frame.payload) {
                            tracing::warn!(
                                plugin = PLUGIN_NAME,
                                error = %e,
                                "ALSA writei failed (scheduled render)"
                            );
                        }
                    }
                    #[cfg(not(feature = "alsa-substrate"))]
                    let _ = &frame;
                    // Audible-time trace stage 7: writei
                    // return. Receiver back-reports the
                    // (wire_recv_ns from the frame envelope,
                    // scheduler_dequeue_ns, writei_return_ns)
                    // triple to the source-host so the
                    // source-host's aggregator can complete
                    // each per-frame record.
                    let writei_return_ns = audio_plane.monotonic_ns();
                    let report = evo_plugin_sdk::contract::audio_plane::ReceiverFrameTraceReport {
                        source_device_id: frame.from_device_id.clone(),
                        group_id: frame.group_id.clone(),
                        sequence: frame.sequence,
                        wire_recv_ns: frame.wire_recv_ns,
                        scheduler_dequeue_ns,
                        writei_return_ns,
                    };
                    if let Err(e) = audio_plane
                        .report_frame_trace(report)
                        .await
                    {
                        tracing::debug!(
                            plugin = PLUGIN_NAME,
                            error = %e,
                            "report_frame_trace failed; continuing"
                        );
                    }
                }
                // Underrun guard: if the anchor is established
                // and we ticked past at least one period budget
                // without rendering anything, write silence so
                // ALSA stays primed.
                #[cfg(feature = "alsa-substrate")]
                if rendered_this_tick == 0
                    && anchor_local.is_some()
                    && alsa_render.is_some()
                {
                    if let Some(render) = alsa_render.as_mut() {
                        if render
                            .queued_frames_below(FRAMES_PER_CHUNK as i64)
                        {
                            let silence =
                                vec![
                                    0u8;
                                    FRAMES_PER_CHUNK
                                        * BASELINE_CHANNELS as usize
                                        * 2
                                ];
                            if let Err(e) = render.write(&silence) {
                                tracing::warn!(
                                    plugin = PLUGIN_NAME,
                                    error = %e,
                                    "ALSA silence write failed (underrun)"
                                );
                            } else {
                                underruns.fetch_add(
                                    1,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                            }
                        }
                    }
                }
                #[cfg(not(feature = "alsa-substrate"))]
                let _ = &underruns;
                let _ = rendered_this_tick;
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
        Ok(Self { pcm })
    }

    /// `snd_pcm_status::get_delay` — frames currently queued
    /// in the ALSA playback buffer that have not yet been
    /// rendered to the DAC. Returns `i64` because ALSA's
    /// delay can be slightly negative during initial-fill /
    /// xrun recovery; the scheduler treats negative as
    /// "needs priming".
    fn queued_frames(&self) -> i64 {
        self.pcm.status().map(|s| s.get_delay() as i64).unwrap_or(0)
    }

    /// Convenience: `true` when the ALSA queue is shallower
    /// than `threshold` frames — the scheduler's signal to
    /// write a silence period to keep playback continuous.
    fn queued_frames_below(&self, threshold: i64) -> bool {
        self.queued_frames() < threshold
    }

    fn write(&mut self, payload: &[u8]) -> Result<(), String> {
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
                // pcm.prepare() and a retry. baseline
                // treats every writei error as recoverable-
                // once; production hardening adds explicit
                // discrimination + escalation.
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
