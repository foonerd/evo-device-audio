// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Audio terminus plugin.
//!
//! Canonical owner of every post-mixer audio-derived signal.
//! Parallel-tapped from `pcm.evo` via `snd-aloop`; the primary
//! audio path (pcm.evo -> hw:CARD=<dac>) is unaffected by
//! terminus health (the floor invariant per the local-playback
//! invariant contract). The plugin owns:
//!
//! - **Spectrum FFT compute.** Demand-driven bin count + channel
//!   count + frequency scale (log / mel / linear; default log per
//!   the 2026-08-11 spectrum-frequency-scale ownership audit)
//!   Float32
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

mod demand;
mod emit_throttle;
mod fft;
mod local_role;
#[cfg(any(test, feature = "alsa-substrate"))]
mod read_fail_class;
mod spectrum_subject;
mod transport_gate;

pub use fft::{PerceptualFrame, SpectrumAnalyser};
pub use local_role::LocalRole;
pub use spectrum_subject::{
    render_spectrum_frame, SPECTRUM_PAYLOAD_VERSION,
    SPECTRUM_SUBJECT_ADDRESSING_SCHEME, SPECTRUM_SUBJECT_ADDRESSING_VALUE,
    SPECTRUM_SUBJECT_TYPE,
};
pub use transport_gate::TransportGate;

#[cfg(feature = "alsa-substrate")]
mod capture;
#[cfg(feature = "alsa-substrate")]
mod interest_subscriber;
#[cfg(feature = "alsa-substrate")]
mod local_role_subscriber;
#[cfg(feature = "alsa-substrate")]
mod now_playing_subscriber;

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
/// `manifest.oop.toml`; both share the same shelf, interaction,
/// and capabilities surface. Consumed by tests that assert the
/// describe() identity matches the manifest declaration.
#[cfg(test)]
const EMBEDDED_MANIFEST: &str = include_str!("../manifest.toml");

/// Source-verb request types this plugin honours. Single entry
/// today; extend as the audio-terminus surface grows
/// (loudness telemetry read verb, etc.).
const REQUEST_TYPES: &[&str] = &["get_spectrum_frame", demand::VERB_SET_DEMAND];

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
    // The terminus capture loop reads from the snd-aloop pair
    // whose playback side is opened by MPD's terminus
    // audio_output. snd-aloop locks the format at the rate the
    // playback side opens with, so the capture side MUST match.
    // The shared contract constant is the single source of
    // truth — MPD's terminus output's `format` directive uses
    // the matching `TERMINUS_LOOPBACK_MPD_FORMAT` from the
    // same module, so any future change happens in one place.
    //
    // Note: operators who override this field in their plugin
    // config to a non-contract value will get a capture-open
    // error at runtime because the loopback's playback half
    // has already been opened by MPD at the contract rate.
    // The field stays operator-tunable for symmetry with
    // future deployments that wire a different capture source,
    // but the canonical reference deployment pins to the
    // contract.
    evo_device_audio_shared::terminus_loopback::TERMINUS_LOOPBACK_RATE_HZ
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
    /// Transport-gate subscriber handle. Watches the
    /// `audio_playback_now_playing` subject's `transport_state`
    /// field and pushes `TransportGate` updates the capture loop
    /// reads before each emit. `Some` after a successful load;
    /// the plugin's `unload` notifies its shutdown + awaits the
    /// task.
    #[cfg(feature = "alsa-substrate")]
    transport_gate_subscriber: Option<now_playing_subscriber::SubscriberHandle>,
    #[cfg(feature = "alsa-substrate")]
    interest_subscriber: Option<interest_subscriber::SubscriberHandle>,
    /// Local-role subscriber handle. Watches the singleton
    /// `audio_multiroom_local_role` subject and pushes
    /// `LocalRole` updates the capture loop reads alongside the
    /// transport gate. `Some` after a successful load; the
    /// plugin's `unload` notifies its shutdown + awaits the
    /// task.
    #[cfg(feature = "alsa-substrate")]
    local_role_subscriber: Option<local_role_subscriber::SubscriberHandle>,
    /// Spectrum-demand store. Holds the current demand +
    /// broadcasts changes to the capture loop's outer gate. The
    /// `audio.spectrum.set_demand` verb writes through it. `Some`
    /// after a successful load; cleared on unload.
    demand_store: Option<demand::SpectrumDemandStore>,
}

impl AudioTerminusPlugin {
    /// Construct an unloaded plugin. State is empty until
    /// [`Plugin::load`] runs; calling any [`Respondent`] verb
    /// before load returns [`PluginError::Permanent`].
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
            #[cfg(feature = "alsa-substrate")]
            transport_gate_subscriber: None,
            #[cfg(feature = "alsa-substrate")]
            interest_subscriber: None,
            #[cfg(feature = "alsa-substrate")]
            local_role_subscriber: None,
            demand_store: None,
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

            // Announce the spectrum subject once at load with a
            // seeded empty-frame envelope. The announcement carries
            // the full wire shape (bins / channels / rate_hz +
            // zero-valued magnitudes / peak_hold / onsets /
            // correlation); the framework stores it on the subject
            // record so subscribers connecting before the first FFT
            // compute see the wire shape immediately. After the
            // capture loop's first frame the empty-frame state is
            // replaced by real spectra via `emit_frame`.
            // Seed shape mirrors the disabled-default demand.
            // Consumers subscribing before the first
            // set_demand write see the shape they'll get on the
            // first live frame after the operator opts in.
            let seed_demand = demand::SpectrumDemand::disabled_default();
            spectrum_subject::announce_initial_state(
                self.subject_announcer
                    .as_ref()
                    .expect("subject_announcer set above"),
                fft::frame_rate_hz(self.config.sample_rate_hz),
                seed_demand.bins,
                seed_demand.channels,
            )
            .await;

            // Acquire the subscribe + query handles once. Both
            // the demand store's rehydrate-on-load and the ALSA
            // capture path's subscribers read through them, so
            // they are hoisted here rather than duplicated inside
            // the alsa-substrate feature gate below. Manifest
            // declares `capabilities.subscribe_subjects = true`;
            // admission populates both handles.
            let subject_state_subscriber = Arc::clone(
                ctx.subject_state_subscriber.as_ref().ok_or_else(|| {
                    PluginError::Permanent(
                        "LoadContext.subject_state_subscriber is None; \
                         manifest must declare \
                         capabilities.subscribe_subjects = true"
                            .to_string(),
                    )
                })?,
            );
            let subject_querier =
                Arc::clone(ctx.subject_querier.as_ref().ok_or_else(|| {
                    PluginError::Permanent(
                        "LoadContext.subject_querier is None; \
                         manifest must declare \
                         capabilities.subscribe_subjects = true"
                            .to_string(),
                    )
                })?);

            // Construct + announce the spectrum-demand subject.
            // The store rehydrates its initial state from the
            // framework's durable subject-state mirror; if this
            // device has a prior applied demand it is restored
            // (operator intent survives reboot + terminus reload
            // without a UI re-push). If not, `disabled_default`
            // — mirrors the pre-supersession wire behaviour and
            // gives evo-ui-runtime's apply bridge a well-defined
            // starting point on any device that has never had
            // `ui.visualizer.*` touched.
            let demand_store = demand::SpectrumDemandStore::announce_initial(
                Arc::clone(
                    self.subject_announcer
                        .as_ref()
                        .expect("subject_announcer set above"),
                ),
                Arc::clone(&subject_state_subscriber),
                Arc::clone(&subject_querier),
            )
            .await;
            self.demand_store = Some(demand_store);

            // Spawn the ALSA capture loop when the alsa-substrate
            // feature is on. Without the feature, the plugin
            // admits cleanly and declares its subject + verb, but
            // emits no frames. Mirrors the multi-room plugin's
            // alsa-substrate gating.
            #[cfg(feature = "alsa-substrate")]
            {
                // Construct the transport-gate channel. Initial
                // value `NotPlaying`: emission stays silent until
                // the subscriber seeds the gate from the now-playing
                // subject's current state OR a stream update
                // delivers `transport_state == "playing"`. This is
                // the conservative default that honours the
                // documented contract ("emission is silent unless
                // transport_state == playing").
                let (gate_tx, gate_rx) =
                    tokio::sync::watch::channel(TransportGate::NotPlaying);

                // Spawn the now-playing subscriber. Owns clones
                // of the SDK's SubjectStateSubscriber +
                // SubjectQuerier handles (acquired above for the
                // demand-store rehydrate; shared here) plus the
                // watch::Sender; pushes gate updates the capture
                // loop reads via the cloned Receiver.
                let subscriber = Arc::clone(&subject_state_subscriber);
                let querier = Arc::clone(&subject_querier);
                let subscriber_handle = now_playing_subscriber::spawn(
                    Arc::clone(&subscriber),
                    Arc::clone(&querier),
                    gate_tx,
                );
                self.transport_gate_subscriber = Some(subscriber_handle);

                // Local-role gate channel. Initial value `Auto`
                // — the permissive default for the operational
                // majority case where no multi-room plugin is
                // admitted on this node, or the local node is
                // a solo device. Only an explicit `Receiver`
                // value from the singleton local-role subject
                // closes this half of the combined gate.
                let (role_tx, role_rx) =
                    tokio::sync::watch::channel(LocalRole::Auto);
                let role_subscriber_handle = local_role_subscriber::spawn(
                    Arc::clone(&subscriber),
                    Arc::clone(&querier),
                    role_tx,
                );
                self.local_role_subscriber = Some(role_subscriber_handle);

                // Seed the framework's per-type interest subject
                // at `evo.system:subscription_interest.audio_
                // playback_spectrum_frame` at `{count:0, at_ms:0}`
                // if it doesn't already exist. Without this,
                // interest_subscriber's resolve loop spins every
                // 500 ms until the first consumer arrives and
                // triggers the lazy-announce. With this, the
                // subject exists at plugin-load time and the
                // subscriber resolves + attaches on first
                // attempt. Idempotent — safe even if a consumer
                // races the seed (existing count is preserved).
                if let Err(e) = self
                    .subject_announcer
                    .as_ref()
                    .expect("subject_announcer set above")
                    .seed_interest_zero(
                        spectrum_subject::SPECTRUM_SUBJECT_TYPE.to_string(),
                    )
                    .await
                {
                    tracing::warn!(
                        plugin = PLUGIN_NAME,
                        error = %e,
                        "seed_interest_zero for spectrum failed; \
                         interest_subscriber will fall back to \
                         resolve-retry loop until first consumer"
                    );
                }

                // Subscription-interest gate channel. Initial
                // value 0 — the produce-iff-consumed default is
                // to STAY parked until at least one subscriber
                // is observed on the framework's projection-ws
                // surface for `audio_playback_spectrum_frame`.
                let (interest_tx, interest_rx) =
                    tokio::sync::watch::channel(0u32);
                let interest_subscriber_handle = interest_subscriber::spawn(
                    subscriber,
                    querier,
                    interest_tx,
                );
                self.interest_subscriber = Some(interest_subscriber_handle);

                let shutdown = Arc::new(tokio::sync::Notify::new());
                let demand_rx = self
                    .demand_store
                    .as_ref()
                    .expect("demand_store constructed above")
                    .watch();
                let capture_handle = capture::spawn(
                    self.config.clone(),
                    Arc::clone(&self.latest_frame),
                    Arc::clone(
                        self.subject_announcer
                            .as_ref()
                            .expect("subject_announcer set above"),
                    ),
                    Arc::clone(&shutdown),
                    gate_rx,
                    role_rx,
                    demand_rx,
                    interest_rx,
                );
                self.capture_shutdown = Some(shutdown);
                self.capture_task = Some(capture_handle);
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    input_pcm = %self.config.input_pcm,
                    sample_rate_hz = self.config.sample_rate_hz,
                    "capture task spawned + transport-gate subscriber spawned"
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

            // Each `handle.await` below joins a `spawn_blocking`
            // thread that ran an ALSA capture loop, a
            // transport-gate subscriber, or a local-role
            // subscriber. Under KillMode=mixed, the plugin
            // subprocess stays alive until the steward's
            // wire-Unload arrives — at which point this future
            // runs and MUST return within the framework's
            // shutdown budget (10s default global_deadline in
            // `admission::ShutdownConfig`) or the steward
            // SIGKILLs the child, producing a zero-fail-in-logs
            // violation.
            //
            // A blocking-thread task that is mid-syscall (e.g.
            // `snd_pcm_readi` waiting for the next PCM period)
            // does not unwind on `shutdown.notify_waiters()` —
            // the shutdown Notify only wakes the async
            // `handle.block_on(select! { shutdown | ... })`
            // arms, and libc syscalls are not part of the tokio
            // scheduler.
            //
            // Bounding each join with `tokio::time::timeout`
            // caps unload wall-time regardless of how deep the
            // hang is. On timeout we log at info (this is a
            // normal-under-real-hardware lifecycle event, not
            // an anomaly), abandon the JoinHandle, and continue.
            // The tokio runtime tears down at process exit; the
            // OS reaps any orphaned blocking threads. The
            // framework's wire-close (fired after this future
            // returns Ok) still causes the plugin's dispatch
            // loop to break on EOF, so the process exits
            // cleanly under the systemd unit's TimeoutStopSec
            // budget.
            #[cfg(feature = "alsa-substrate")]
            {
                const JOIN_BUDGET: std::time::Duration =
                    std::time::Duration::from_secs(3);
                if let Some(shutdown) = self.capture_shutdown.take() {
                    shutdown.notify_waiters();
                }
                if let Some(handle) = self.capture_task.take() {
                    if tokio::time::timeout(JOIN_BUDGET, handle).await.is_err()
                    {
                        tracing::info!(
                            plugin = PLUGIN_NAME,
                            join_budget_ms = JOIN_BUDGET.as_millis() as u64,
                            task = "capture",
                            "capture task join budget elapsed; abandoning \
                             blocking thread (runtime tears down at process \
                             exit; OS reaps)"
                        );
                    }
                }
                if let Some(sub) = self.transport_gate_subscriber.take() {
                    sub.shutdown.notify_waiters();
                    if tokio::time::timeout(JOIN_BUDGET, sub.task)
                        .await
                        .is_err()
                    {
                        tracing::info!(
                            plugin = PLUGIN_NAME,
                            join_budget_ms = JOIN_BUDGET.as_millis() as u64,
                            task = "transport_gate_subscriber",
                            "subscriber join budget elapsed; abandoning"
                        );
                    }
                }
                if let Some(sub) = self.interest_subscriber.take() {
                    sub.shutdown.notify_waiters();
                    if tokio::time::timeout(JOIN_BUDGET, sub.task)
                        .await
                        .is_err()
                    {
                        tracing::info!(
                            plugin = PLUGIN_NAME,
                            join_budget_ms = JOIN_BUDGET.as_millis() as u64,
                            task = "interest_subscriber",
                            "subscriber join budget elapsed; abandoning"
                        );
                    }
                }
                if let Some(sub) = self.local_role_subscriber.take() {
                    sub.shutdown.notify_waiters();
                    if tokio::time::timeout(JOIN_BUDGET, sub.task)
                        .await
                        .is_err()
                    {
                        tracing::info!(
                            plugin = PLUGIN_NAME,
                            join_budget_ms = JOIN_BUDGET.as_millis() as u64,
                            task = "local_role_subscriber",
                            "subscriber join budget elapsed; abandoning"
                        );
                    }
                }
            }

            self.subject_announcer = None;
            self.demand_store = None;
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
        &'a self,
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
                verb if verb == demand::VERB_SET_DEMAND => {
                    let store =
                        self.demand_store.as_ref().ok_or_else(|| {
                            PluginError::Permanent(
                                "demand_store unavailable; plugin not \
                             fully loaded"
                                    .to_string(),
                            )
                        })?;
                    store.handle_set_demand(req).await
                }
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
        // Empty-frame shape mirrors the current demand — so a
        // consumer calling `get_spectrum_frame` on a disabled
        // (or freshly-loaded) device sees the shape the first
        // live frame will carry after the operator opts in.
        // Fall back to the disabled-default if the demand store
        // is not yet constructed (transient at load; unlikely
        // in steady state).
        let seed_demand = self
            .demand_store
            .as_ref()
            .map(|s| s.current())
            .unwrap_or_else(demand::SpectrumDemand::disabled_default);
        let payload = match frame_guard.as_ref() {
            Some(frame) => render_spectrum_frame(frame, rate_hz),
            None => spectrum_subject::render_empty_frame(
                rate_hz,
                seed_demand.bins,
                seed_demand.channels,
            ),
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
        assert_eq!(desc.identity.contract, mf.plugin.contract);
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
        let plugin = AudioTerminusPlugin::new();
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
