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
//!   playback device named in the config. baseline
//!   does NOT yet schedule against `presentation_time_ms`
//!   (no jitter buffer); receivers play frames as they
//!   arrive. Synced playback alignment + jitter buffer +
//!   adaptive cadence (matching the reliability bar) ride
//!   the next iteration.
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
const REQUEST_TYPES: &[&str] = &["multiroom.get_status"];

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
/// 20 ms per period = ~80 ms tolerance, enough for typical
/// LAN jitter without audible queue-back drift between
/// source-host playback and receiver render.
#[cfg(feature = "alsa-substrate")]
const RENDER_BUFFER_PERIODS: usize = 4;

/// Backlog threshold (in ALSA frames available-to-write
/// queue depth) above which the receiver drops the inbound
/// frame instead of writing it. When the ALSA queue is
/// already deeper than this, writing more would compound
/// drift; dropping shortens the latency at the cost of
/// momentary discontinuity. Set to one period less than the
/// configured buffer so the drop kicks in only when the
/// buffer is nearly full.
#[cfg(feature = "alsa-substrate")]
const RENDER_BACKLOG_DROP_FRAMES: usize =
    FRAMES_PER_CHUNK * (RENDER_BUFFER_PERIODS - 1);

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
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            role: default_role(),
            group_id: None,
            alsa_pcm: default_alsa_pcm(),
            source_pcm: String::new(),
        }
    }
}

fn default_role() -> Role {
    Role::Auto
}

fn default_alsa_pcm() -> String {
    "evo".to_string()
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
                            tokio::spawn(async move {
                                run_source_capture_task(
                                    handle, group_id, sent, shutdown, pcm,
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
                }
                Role::Receiver | Role::Auto => {
                    let counter = Arc::clone(&self.frames_received);
                    let shutdown = Arc::clone(&self.shutdown);
                    let handle = Arc::clone(&audio_plane);
                    let alsa_pcm = self.config.alsa_pcm.clone();
                    let role = self.config.role;
                    let task = tokio::spawn(async move {
                        run_receiver_task(
                            handle, counter, shutdown, alsa_pcm, role,
                        )
                        .await;
                    });
                    self.receiver_task = Some(task);
                    tracing::info!(
                        plugin = PLUGIN_NAME,
                        role = self.config.role.as_wire_str(),
                        alsa_pcm = %self.config.alsa_pcm,
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
                        "frames_sent": self.frames_sent(),
                        "frames_received": self.frames_received(),
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

        let presentation_time_ms = (start_monotonic.elapsed().as_millis()
            as u64)
            .saturating_add(sequence.saturating_mul(20).saturating_add(100));

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
                let presentation_time_ms =
                    (start_monotonic.elapsed().as_millis() as u64)
                        .saturating_add(
                            sequence
                                .saturating_mul(20)
                                .saturating_add(100),
                        );
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

/// Receiver-side task: subscribe to incoming audio frames,
/// count them, and (when the `alsa-substrate` Cargo feature
/// is on) write each frame's PCM payload to the configured
/// ALSA playback device.
async fn run_receiver_task(
    audio_plane: Arc<dyn AudioPlaneHandle>,
    counter: Arc<std::sync::atomic::AtomicU64>,
    shutdown: Arc<Notify>,
    alsa_pcm: String,
    role: Role,
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
                        #[cfg(feature = "alsa-substrate")]
                        if let Some(render) = alsa_render.as_mut() {
                            match render.write(&frame.payload) {
                                Ok(WriteOutcome::Rendered) => {}
                                Ok(WriteOutcome::DroppedDeepBacklog {
                                    queued_frames,
                                }) => {
                                    // Drift defence kicked in.
                                    // Log at debug so the trace
                                    // is available without
                                    // spamming the log on a
                                    // sustained drift event.
                                    tracing::debug!(
                                        plugin = PLUGIN_NAME,
                                        queued_frames = queued_frames,
                                        drop_threshold =
                                            RENDER_BACKLOG_DROP_FRAMES,
                                        "audio frame dropped at receiver: \
                                         ALSA backlog deeper than drift \
                                         threshold"
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        plugin = PLUGIN_NAME,
                                        error = %e,
                                        "ALSA writei failed"
                                    );
                                }
                            }
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

    /// Frames currently queued in the ALSA playback buffer
    /// (i.e. audio not yet rendered). Used by the receiver
    /// loop to detect backlog accumulation and drop frames
    /// when the queue is deep enough to introduce audible
    /// drift between source-host playback and local render.
    fn delay_frames(&self) -> alsa::pcm::Frames {
        self.pcm.status().map(|s| s.get_delay()).unwrap_or(0)
    }

    fn write(&mut self, payload: &[u8]) -> Result<WriteOutcome, String> {
        // Interleaved s16le: 4 bytes per stereo frame (2 ch
        // * 2 bytes). Decode in place — alsa::pcm::IO::<i16>
        // takes a &[i16] of length frames * channels.
        if payload.len() % 4 != 0 {
            return Err(format!(
                "payload length {} not aligned to s16le stereo frame (4 bytes)",
                payload.len()
            ));
        }
        // Backlog-aware drift defence. If the ALSA playback
        // queue already holds more than RENDER_BACKLOG_DROP_FRAMES
        // worth of unrendered audio, the source-host's
        // emission cadence has run ahead of the receiver's
        // playback cadence (clock skew, network jitter burst,
        // or initial-fill effect). Writing this frame would
        // compound the drift; dropping shortens the latency.
        // The next frame is checked on the same basis.
        let queued = self.delay_frames();
        if queued > RENDER_BACKLOG_DROP_FRAMES as alsa::pcm::Frames {
            return Ok(WriteOutcome::DroppedDeepBacklog {
                queued_frames: queued as usize,
            });
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
            Ok(n) if n == frame_count => Ok(WriteOutcome::Rendered),
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
                    Ok(n) if n == frame_count => Ok(WriteOutcome::Rendered),
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

/// Receiver-side write outcome. Either the frame went into
/// the ALSA buffer for render, or the buffer's queue was
/// already deeper than the drop threshold and the frame was
/// dropped to bound drift.
#[cfg(feature = "alsa-substrate")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteOutcome {
    Rendered,
    DroppedDeepBacklog { queued_frames: usize },
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
