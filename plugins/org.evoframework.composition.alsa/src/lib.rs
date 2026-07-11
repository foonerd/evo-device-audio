// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
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
//! ## Route-change reactor
//!
//! On every successful load, the plugin spawns a reactor
//! task that subscribes to topology rewires through the
//! routing handle's
//! [`on_route_change`](evo_plugin_sdk::contract::audio_routing::AudioRouting::on_route_change)
//! surface. Every framework-fired route change wakes the
//! reactor, which calls `composition_endpoints()` to fetch
//! the new pair and publishes it to a
//! [`tokio::sync::watch`] channel. Consumers (the byte-flow
//! worker, observability surfaces, tests) subscribe via
//! [`AlsaCompositionPlugin::subscribe_endpoints`] and react
//! to each new snapshot.
//!
//! The reactor terminates cleanly on unload — the plugin
//! signals shutdown, awaits the task handle, clears the
//! routing-side callback so the framework drops its
//! reference, and only then forgets the routing handle.
//!
//! ## Byte-flow worker
//!
//! Alongside the reactor, the plugin spawns a byte-flow
//! worker that consumes the endpoint snapshot stream the
//! reactor publishes. On every snapshot, the worker tears
//! down any previous substrate, opens the OS-native
//! primitives the framework configured for the new
//! endpoint pair, and runs the substrate's pump loop until
//! the next snapshot, an unrecoverable substrate error, or
//! shutdown. Worker status (`Idle` / `Running { kind }` /
//! `Failed { reason }` / `Unsupported { kind }`) is
//! published to a watch channel so observability surfaces
//! and tests can render the current substrate state.
//!
//! This build implements the `EndpointKind::NamedPipe`
//! substrate (filesystem FIFOs read+written via tokio
//! async I/O). The `EndpointKind::AlsaPcm` substrate is
//! hardware-gated on the libasound link and real-hardware
//! cross-target verification, not yet wired in this build.
//! `SharedMemory` and `JackPort` substrates are vendor-
//! distribution territory and report as unsupported.
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
    AudioRouting, AudioRoutingError, CompositionEndpoints, EndpointKind,
    RouteChange, RouteChangeCallback,
};
use evo_plugin_sdk::contract::{
    BuildInfo, ExternalAddressing, HealthReport, LoadContext, Plugin,
    PluginDescription, PluginError, PluginIdentity, Request, Respondent,
    Response, RuntimeCapabilities, SubjectStateStreamError,
};
use evo_plugin_sdk::Manifest;
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;

use crate::byte_flow::{run_substrate, ByteFlowError};

mod biquad;
mod byte_flow;
mod eq_dsp;

#[cfg(feature = "alsa-substrate")]
mod byte_flow_alsa;

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
const MODE_EQ_ONLY: &str = "eq_only";
const DECLARED_MODES: &[&str] = &[MODE_PASSTHROUGH, MODE_EQ_ONLY];

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

/// Operator-controlled EQ runtime state derived from the
/// `audio.options.settings` subject. The subject subscriber
/// pushes a fresh snapshot to `eq_state_tx`; the byte-flow
/// substrate observes it through `eq_state_rx` and
/// reconfigures the [`EqProcessor`] on every change.
#[derive(Debug, Clone, PartialEq)]
pub struct EqRuntimeState {
    /// `true` engages the EQ processing path; `false` pumps
    /// bytes through unchanged even when the active mode is
    /// `eq_only`. Operator A/B switch within the EQ mode.
    pub engaged: bool,
    /// 10 parametric band parameters. Schema-pinned count;
    /// consumer DSP cascades 10 biquads per channel.
    pub bands: [crate::eq_dsp::EqBandParams; crate::eq_dsp::EQ_BAND_COUNT],
}

impl Default for EqRuntimeState {
    fn default() -> Self {
        Self {
            engaged: false,
            bands: [crate::eq_dsp::EqBandParams::default();
                crate::eq_dsp::EQ_BAND_COUNT],
        }
    }
}

/// ALSA composition plugin.
pub struct AlsaCompositionPlugin {
    loaded: bool,
    /// Active composition mode token. Reset to
    /// [`MODE_PASSTHROUGH`] at every successful load.
    current_mode: String,
    /// Watch channel publishing the active mode token to the
    /// byte-flow substrate. The substrate's pump loop
    /// observes mode changes inline and branches between
    /// passthrough and eq_only processing without restarting
    /// the substrate lifecycle.
    mode_tx: watch::Sender<String>,
    /// Watch channel publishing the operator's EQ runtime
    /// state (engaged flag + 10 band parameters). Seeded with
    /// the audiophile-grade flat defaults at construction;
    /// updated by the `audio.options.settings` subscriber.
    eq_state_tx: watch::Sender<EqRuntimeState>,
    /// Audio routing handle pulled from
    /// [`LoadContext::audio_routing`] at load time. `None`
    /// before the first successful load and after every
    /// `unload`.
    audio_routing: Option<Arc<dyn AudioRouting>>,
    /// Cumulative `composition.select_mode` requests
    /// served, including refused ones. Surfaced for
    /// diagnostics; not part of the wire contract.
    requests_handled: u64,
    /// Route-change reactor handle. `Some` after a
    /// successful `Plugin::load`; `None` before first load,
    /// after `Plugin::unload`, and after a test path that
    /// stops at `install_routing`.
    reactor: Option<ReactorHandle>,
    /// Byte-flow worker handle. `Some` while the worker
    /// task is running. Spawned on load (after the
    /// reactor) and stopped on unload (before the reactor).
    worker: Option<WorkerHandle>,
}

/// Byte-flow worker status. Published to the worker's
/// watch channel for observability surfaces and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerStatus {
    /// No topology — no substrate is open.
    Idle,
    /// Substrate is running; pump is active. `kind` is the
    /// homogeneous substrate kind (input.kind ==
    /// output.kind for passthrough mode).
    Running {
        /// Substrate kind currently driving the pump.
        kind: EndpointKind,
    },
    /// Substrate exited with an error. The worker waits
    /// for the next route change to retry. `reason`
    /// carries the structured error message from
    /// [`ByteFlowError::Display`](crate::byte_flow::ByteFlowError).
    Failed {
        /// Operator-readable failure reason carrying the
        /// underlying [`ByteFlowError`] message.
        reason: String,
    },
    /// Substrate kind declared by the framework is not
    /// implemented in this build. Same recovery semantics
    /// as `Failed`; the worker stays in this state until
    /// the next route change.
    Unsupported {
        /// Endpoint substrate kind the framework selected.
        kind: EndpointKind,
    },
}

/// Handle on the byte-flow worker task. Owns the shutdown
/// signal, the join handle, and the receiver-end of the
/// worker-status channel.
struct WorkerHandle {
    task: JoinHandle<()>,
    shutdown: Arc<Notify>,
    status_rx: watch::Receiver<WorkerStatus>,
}

/// Handle on the reactor task spawned at load. Owns the
/// shutdown signal, the join handle, and the receiver-end
/// of the endpoint-snapshot channel.
struct ReactorHandle {
    task: JoinHandle<()>,
    shutdown: Arc<Notify>,
    endpoints_rx: watch::Receiver<Option<CompositionEndpoints>>,
    /// Reactor refresh counter — bumped after every
    /// successful endpoint fetch (configured or
    /// pre-reconciliation). Tests poll on this to observe
    /// reactor progress without racy sleeps. Production
    /// code does not read the counter; it is here so the
    /// reactor's `Arc` clone has a stable home for the
    /// plugin's lifetime.
    #[cfg_attr(not(test), allow(dead_code))]
    refresh_count: Arc<std::sync::atomic::AtomicU64>,
}

impl AlsaCompositionPlugin {
    /// Construct a fresh plugin instance.
    pub fn new() -> Self {
        let (mode_tx, _) = watch::channel(MODE_PASSTHROUGH.to_string());
        let (eq_state_tx, _) = watch::channel(EqRuntimeState::default());
        Self {
            loaded: false,
            current_mode: MODE_PASSTHROUGH.to_string(),
            mode_tx,
            eq_state_tx,
            audio_routing: None,
            requests_handled: 0,
            reactor: None,
            worker: None,
        }
    }

    /// Subscribe to the byte-flow worker's status channel.
    /// Returns `None` when no worker is running.
    pub fn subscribe_worker_status(
        &self,
    ) -> Option<watch::Receiver<WorkerStatus>> {
        self.worker.as_ref().map(|w| w.status_rx.clone())
    }

    /// Subscribe to endpoint snapshots produced by the
    /// route-change reactor. Returns `None` when the
    /// plugin is not loaded (no reactor is running).
    ///
    /// The receiver yields the most recent
    /// [`CompositionEndpoints`] snapshot, or `None` for the
    /// pre-reconciliation state. Each topology rewire
    /// publishes one new value; consumers call
    /// [`watch::Receiver::changed`] to await the next
    /// rewire and [`watch::Receiver::borrow`] for the
    /// current snapshot.
    pub fn subscribe_endpoints(
        &self,
    ) -> Option<watch::Receiver<Option<CompositionEndpoints>>> {
        self.reactor.as_ref().map(|r| r.endpoints_rx.clone())
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

    /// Spawn the route-change reactor task. Must be called
    /// after [`Self::install_routing`] succeeds so the
    /// audio_routing handle is available; must be called
    /// inside a tokio runtime context (the framework's
    /// plugin host runs `Plugin::load` under tokio; tests
    /// drive this via `#[tokio::test]`).
    ///
    /// Registers a [`RouteChangeCallback`] on the routing
    /// handle, performs an initial endpoint fetch, and
    /// spawns the reactor that refreshes on every wake.
    async fn spawn_reactor(&mut self) -> Result<(), PluginError> {
        debug_assert!(
            self.audio_routing.is_some(),
            "spawn_reactor called before install_routing"
        );
        debug_assert!(
            self.reactor.is_none(),
            "spawn_reactor called while a reactor is already running"
        );

        let routing = Arc::clone(
            self.audio_routing
                .as_ref()
                .expect("audio_routing populated when loaded"),
        );

        let initial = match routing.composition_endpoints() {
            Ok(ep) => Some(ep),
            Err(AudioRoutingError::EndpointNotConfigured) => None,
            Err(other) => {
                tracing::warn!(
                    error = %other,
                    "audio_routing surface returned unexpected error during \
                     initial endpoint fetch; treating as pre-reconciliation"
                );
                None
            }
        };
        let (endpoints_tx, endpoints_rx) = watch::channel(initial);

        let wake = Arc::new(Notify::new());
        let shutdown = Arc::new(Notify::new());
        let refresh_count = Arc::new(std::sync::atomic::AtomicU64::new(0));

        // Register the route-change callback. The callback
        // notifies the reactor's wake signal; the reactor
        // picks up on its next select iteration. The
        // callback holds an Arc<Notify> rather than the
        // routing handle itself, so callback invocation
        // does not re-enter the trait.
        let wake_for_callback = Arc::clone(&wake);
        let callback: RouteChangeCallback =
            Arc::new(move |_event: &RouteChange| {
                wake_for_callback.notify_one();
            });
        routing.on_route_change(Some(callback));

        let task_routing = Arc::clone(&routing);
        let task_wake = Arc::clone(&wake);
        let task_shutdown = Arc::clone(&shutdown);
        let task_count = Arc::clone(&refresh_count);
        let task = tokio::spawn(async move {
            run_reactor(
                task_routing,
                task_wake,
                task_shutdown,
                endpoints_tx,
                task_count,
            )
            .await;
        });

        self.reactor = Some(ReactorHandle {
            task,
            shutdown,
            endpoints_rx,
            refresh_count,
        });
        Ok(())
    }

    /// Spawn the byte-flow worker task. Must be called
    /// after [`Self::spawn_reactor`] succeeds — the worker
    /// subscribes to the reactor's endpoint snapshot
    /// channel.
    async fn spawn_worker(&mut self) -> Result<(), PluginError> {
        debug_assert!(
            self.reactor.is_some(),
            "spawn_worker called before spawn_reactor"
        );
        debug_assert!(
            self.worker.is_none(),
            "spawn_worker called while a worker is already running"
        );

        let endpoints_rx = self
            .reactor
            .as_ref()
            .expect("reactor populated")
            .endpoints_rx
            .clone();
        let mode_rx = self.mode_tx.subscribe();
        let eq_state_rx = self.eq_state_tx.subscribe();
        let (status_tx, status_rx) = watch::channel(WorkerStatus::Idle);
        let shutdown = Arc::new(Notify::new());
        let task_shutdown = Arc::clone(&shutdown);
        let task = tokio::spawn(async move {
            run_worker(
                endpoints_rx,
                mode_rx,
                eq_state_rx,
                task_shutdown,
                status_tx,
            )
            .await;
        });

        self.worker = Some(WorkerHandle {
            task,
            shutdown,
            status_rx,
        });
        Ok(())
    }

    /// Wind down the byte-flow worker task. Idempotent.
    async fn stop_worker(&mut self) {
        if let Some(handle) = self.worker.take() {
            handle.shutdown.notify_one();
            let _ = handle.task.await;
        }
    }

    /// Subscribe to the `audio.options.settings` subject the
    /// `playback.options` plugin announces; extract operator EQ
    /// state (`eq_engaged` + `eq_bands`) on every change and
    /// push to `eq_state_tx`. The byte-flow worker observes the
    /// channel inline and recomputes biquad coefficients without
    /// restarting the substrate.
    ///
    /// Best-effort: silently skips when `subject_state_subscriber`
    /// / `subject_querier` are not populated (OOP pre-wire-surface
    /// or test fixtures); the composition stage continues to
    /// serve the passthrough mode and the eq_only mode reads the
    /// default EQ state (engaged = false, bands = flat). When the
    /// substrate is wired but the `audio.options` subject has
    /// not yet announced, retries with bounded exponential
    /// backoff — typical resolve succeeds on attempt 2 or 3.
    async fn spawn_options_settings_subscriber(&self, ctx: &LoadContext) {
        let Some(subscriber) = ctx.subject_state_subscriber.as_ref() else {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                "subject_state_subscriber not populated; skipping \
                 audio-options subscription"
            );
            return;
        };
        let Some(querier) = ctx.subject_querier.as_ref() else {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                "subject_querier not populated; skipping audio-options \
                 subscription"
            );
            return;
        };

        let subscriber = Arc::clone(subscriber);
        let querier = Arc::clone(querier);
        let eq_tx = self.eq_state_tx.clone();
        let addressing = ExternalAddressing {
            scheme: "evo.audio.options".to_string(),
            value: "settings".to_string(),
        };

        tokio::spawn(async move {
            let canonical_id = match resolve_options_addressing_with_backoff(
                querier.as_ref(),
                &addressing,
            )
            .await
            {
                Some(id) => id,
                None => return,
            };

            // Subscribe FIRST so we cannot miss a state change
            // that lands between current_state and subscribe.
            let mut stream = match subscriber
                .subscribe_subject(canonical_id.clone())
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        plugin = PLUGIN_NAME,
                        error = %e,
                        canonical_id = %canonical_id,
                        "subscribe to audio-options settings subject failed"
                    );
                    return;
                }
            };
            let initial_state =
                match subscriber.current_state(canonical_id.clone()).await {
                    Ok(state) => state,
                    Err(e) => {
                        tracing::warn!(
                            plugin = PLUGIN_NAME,
                            error = %e,
                            canonical_id = %canonical_id,
                            "read audio-options settings current_state failed; \
                             subscription continues without initial seed"
                        );
                        None
                    }
                };
            if let Some(state) = initial_state {
                let new_state = parse_eq_runtime_state_from_state(&state);
                let _ = eq_tx.send_replace(new_state);
            }
            loop {
                match stream.recv().await {
                    Ok(update) => {
                        if let Some(state) = update.state.as_ref() {
                            let new_state =
                                parse_eq_runtime_state_from_state(state);
                            let _ = eq_tx.send_replace(new_state);
                        }
                    }
                    Err(SubjectStateStreamError::Lagged { dropped }) => {
                        tracing::warn!(
                            plugin = PLUGIN_NAME,
                            dropped = dropped,
                            "audio-options subject stream lagged; continuing"
                        );
                    }
                    Err(SubjectStateStreamError::Closed) => {
                        tracing::debug!(
                            plugin = PLUGIN_NAME,
                            "audio-options subject stream closed; \
                             subscriber task exiting"
                        );
                        return;
                    }
                }
            }
        });
    }

    /// Wind down the reactor task and clear the
    /// route-change callback. Idempotent — calling on a
    /// plugin without an active reactor is a no-op.
    async fn stop_reactor(&mut self) {
        if let Some(routing) = self.audio_routing.as_ref() {
            // Drop the framework's reference to the
            // callback before signalling shutdown so the
            // routing handle releases its Arc and the
            // callback closure (and its captured wake
            // notify) can be dropped on schedule.
            routing.on_route_change(None);
        }
        if let Some(handle) = self.reactor.take() {
            handle.shutdown.notify_one();
            // Best-effort wait for the reactor to drain.
            // If the task panicked (it should not), we
            // don't propagate — the plugin is unloading
            // and tracing has already captured the panic.
            let _ = handle.task.await;
        }
    }

    /// Returns the reactor's refresh counter. Tests poll
    /// on this to observe the reactor making progress
    /// after firing a route change. Returns 0 when no
    /// reactor is running.
    #[cfg(test)]
    fn refresh_count(&self) -> u64 {
        self.reactor
            .as_ref()
            .map(|r| r.refresh_count.load(std::sync::atomic::Ordering::SeqCst))
            .unwrap_or(0)
    }
}

/// Byte-flow worker loop. Subscribes to the reactor's
/// endpoint snapshot channel; on each new snapshot, runs
/// the substrate appropriate to the endpoint kind until
/// the next snapshot, an unrecoverable substrate error,
/// or shutdown. Worker status is published to the watch
/// channel for observability.
///
/// The worker spawns each substrate run as its own
/// sub-task with a cancel signal so endpoint changes can
/// preempt an in-flight pump cleanly: the worker fires
/// cancel and awaits the run task before opening the next
/// substrate, avoiding double-open of the same path.
/// Inspect a composition-endpoint pair against the EQ DSP's
/// supported sample-format coverage. Returns `Some(reason)`
/// when the negotiated format would refuse engagement (any
/// PcmCodec outside `S16Le` / `F32` or channel count outside
/// 1..=2). Returns `None` for supported formats. The reason
/// string is operator-readable and names the offending field.
fn check_eq_only_format_support(
    endpoints: &CompositionEndpoints,
) -> Option<String> {
    use evo_plugin_sdk::audio::{AudioFormat, PcmCodec};
    match &endpoints.input.format {
        AudioFormat::Pcm {
            codec, channels, ..
        } => {
            let codec_supported =
                matches!(codec, PcmCodec::PcmS16Le | PcmCodec::PcmF32);
            if !codec_supported {
                return Some(format!(
                    "eq_only refused: PCM codec {:?} not supported by the \
                     EQ DSP (supported: PcmS16Le, PcmF32); operator must \
                     choose a different composition mode",
                    codec
                ));
            }
            if !(1..=2).contains(channels) {
                return Some(format!(
                    "eq_only refused: channel count {} not supported by the \
                     EQ DSP (supported: 1..=2); operator must choose a \
                     different composition mode",
                    channels
                ));
            }
            None
        }
        other => Some(format!(
            "eq_only refused: stream format {:?} not supported by the EQ \
             DSP (supported: PCM s16le / f32le, mono / stereo)",
            other
        )),
    }
}

/// Extract the operator's EQ runtime state from an
/// `audio.options.settings` subject-state payload. Missing
/// fields fall through to audiophile-grade defaults (engaged
/// false, 10 flat bands at 1 kHz / 0 dB / Q=1.0).
fn parse_eq_runtime_state_from_state(
    state: &serde_json::Value,
) -> EqRuntimeState {
    let engaged = state
        .get("eq_engaged")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mut bands =
        [crate::eq_dsp::EqBandParams::default(); crate::eq_dsp::EQ_BAND_COUNT];
    if let Some(arr) = state.get("eq_bands").and_then(|v| v.as_array()) {
        for (i, entry) in
            arr.iter().take(crate::eq_dsp::EQ_BAND_COUNT).enumerate()
        {
            let freq_hz = entry
                .get("freq_hz")
                .and_then(|v| v.as_u64())
                .map(|n| n as u32)
                .unwrap_or(bands[i].freq_hz);
            let gain_db = entry
                .get("gain_db")
                .and_then(|v| v.as_f64())
                .map(|n| n as f32)
                .unwrap_or(bands[i].gain_db);
            let q = entry
                .get("q")
                .and_then(|v| v.as_f64())
                .map(|n| n as f32)
                .unwrap_or(bands[i].q);
            bands[i] = crate::eq_dsp::EqBandParams {
                freq_hz,
                gain_db,
                q,
            };
        }
    }
    EqRuntimeState { engaged, bands }
}

/// Resolve the `audio.options.settings` addressing with
/// bounded exponential backoff. Mirrors the playback.mpd
/// pattern: the canonical Phase 2 discovery order admits
/// composition.alsa before playback.options on the reference
/// distribution, so the first resolve attempt typically
/// returns `Ok(None)` and the retry succeeds on attempt 2 or
/// 3.
async fn resolve_options_addressing_with_backoff(
    querier: &dyn evo_plugin_sdk::contract::SubjectQuerier,
    addressing: &ExternalAddressing,
) -> Option<String> {
    const MAX_ATTEMPTS: u32 = 10;
    const INITIAL_DELAY_MS: u64 = 100;
    const MAX_DELAY_MS: u64 = 6_400;
    let mut delay_ms = INITIAL_DELAY_MS;
    for attempt in 0..MAX_ATTEMPTS {
        match querier.resolve_addressing(addressing.clone()).await {
            Ok(Some(id)) => {
                if attempt > 0 {
                    tracing::info!(
                        plugin = PLUGIN_NAME,
                        attempt = attempt + 1,
                        canonical_id = %id,
                        "audio-options settings subject resolved after \
                         admission-order retry"
                    );
                }
                return Some(id);
            }
            Ok(None) => {
                if attempt == 0 {
                    tracing::info!(
                        plugin = PLUGIN_NAME,
                        delay_ms,
                        "audio-options settings subject not yet announced; \
                         retrying with exponential backoff"
                    );
                }
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms))
                    .await;
                delay_ms = (delay_ms * 2).min(MAX_DELAY_MS);
            }
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    attempt = attempt + 1,
                    "resolve_addressing for audio-options settings failed; \
                     retrying"
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms))
                    .await;
                delay_ms = (delay_ms * 2).min(MAX_DELAY_MS);
            }
        }
    }
    tracing::warn!(
        plugin = PLUGIN_NAME,
        "audio-options settings subject did not resolve within retry budget; \
         eq state subscriber not wired (eq_only mode reads defaults until \
         operator settings land)"
    );
    None
}

async fn run_worker(
    mut endpoints_rx: watch::Receiver<Option<CompositionEndpoints>>,
    mode_rx: watch::Receiver<String>,
    eq_state_rx: watch::Receiver<EqRuntimeState>,
    shutdown: Arc<Notify>,
    status_tx: watch::Sender<WorkerStatus>,
) {
    loop {
        // The borrow_and_update marks the current value as
        // seen so a subsequent `changed()` only fires for
        // the next publication.
        let snapshot = endpoints_rx.borrow_and_update().clone();

        let outcome = match snapshot {
            None => {
                let _ = status_tx.send(WorkerStatus::Idle);
                wait_for_next_event(&mut endpoints_rx, &shutdown).await
            }
            Some(endpoints) => {
                run_substrate_lifecycle(
                    endpoints,
                    &mut endpoints_rx,
                    mode_rx.clone(),
                    eq_state_rx.clone(),
                    Arc::clone(&shutdown),
                    &status_tx,
                )
                .await
            }
        };

        if matches!(outcome, EventOutcome::Shutdown) {
            return;
        }
    }
}

/// Outcome of a wait inside the worker loop. Drives the
/// outer loop's decision to continue or terminate.
enum EventOutcome {
    /// Endpoint snapshot changed (or the channel closed —
    /// treated identically).
    EndpointChanged,
    /// Shutdown was signalled.
    Shutdown,
}

/// Wait for the next worker event: either the endpoint
/// snapshot changes (or the channel closes) or shutdown
/// is signalled. Returns the outcome so the outer loop
/// knows whether to terminate.
async fn wait_for_next_event(
    endpoints_rx: &mut watch::Receiver<Option<CompositionEndpoints>>,
    shutdown: &Notify,
) -> EventOutcome {
    tokio::select! {
        biased;
        _ = shutdown.notified() => EventOutcome::Shutdown,
        _ = endpoints_rx.changed() => EventOutcome::EndpointChanged,
    }
}

/// Run a single substrate lifecycle: spawn the substrate
/// run task, wait for run-completion / endpoint-change /
/// shutdown, signal cancel, and drain the run task. On
/// return, the outer worker loop picks up the next
/// snapshot.
async fn run_substrate_lifecycle(
    endpoints: CompositionEndpoints,
    endpoints_rx: &mut watch::Receiver<Option<CompositionEndpoints>>,
    mode_rx: watch::Receiver<String>,
    eq_state_rx: watch::Receiver<EqRuntimeState>,
    shutdown: Arc<Notify>,
    status_tx: &watch::Sender<WorkerStatus>,
) -> EventOutcome {
    // Pre-flight: reject a snapshot whose substrate kind
    // is not implemented. The worker stays in the
    // Unsupported state and waits for the next route
    // change.
    if let Err(ByteFlowError::UnsupportedKind(kind)) =
        precheck_substrate(&endpoints)
    {
        let _ = status_tx.send(WorkerStatus::Unsupported { kind });
        return wait_for_next_event(endpoints_rx, &shutdown).await;
    }
    if let Err(ByteFlowError::MixedSubstrate { input, output }) =
        precheck_substrate(&endpoints)
    {
        let _ = status_tx.send(WorkerStatus::Failed {
            reason: format!(
                "input/output substrate kinds differ: input={input:?} output={output:?}"
            ),
        });
        return wait_for_next_event(endpoints_rx, &shutdown).await;
    }

    let kind = endpoints.input.kind;
    let _ = status_tx.send(WorkerStatus::Running { kind });

    let cancel = Arc::new(Notify::new());
    let cancel_for_run = Arc::clone(&cancel);
    let endpoints_for_run = endpoints.clone();
    let mode_rx_for_run = mode_rx.clone();
    let eq_state_rx_for_run = eq_state_rx.clone();
    let mut run_handle = tokio::spawn(async move {
        run_substrate(
            &endpoints_for_run,
            mode_rx_for_run,
            eq_state_rx_for_run,
            cancel_for_run,
        )
        .await
    });

    tokio::select! {
        biased;
        _ = shutdown.notified() => {
            cancel.notify_one();
            let _ = (&mut run_handle).await;
            EventOutcome::Shutdown
        }
        res = endpoints_rx.changed() => {
            cancel.notify_one();
            let _ = (&mut run_handle).await;
            if res.is_err() {
                EventOutcome::Shutdown
            } else {
                EventOutcome::EndpointChanged
            }
        }
        result = &mut run_handle => {
            match result {
                Ok(Ok(())) => {
                    let _ = status_tx.send(WorkerStatus::Idle);
                }
                Ok(Err(e)) => {
                    let _ = status_tx.send(WorkerStatus::Failed {
                        reason: e.to_string(),
                    });
                }
                Err(join_err) => {
                    let _ = status_tx.send(WorkerStatus::Failed {
                        reason: format!(
                            "substrate task panicked: {join_err}"
                        ),
                    });
                }
            }
            wait_for_next_event(endpoints_rx, &shutdown).await
        }
    }
}

/// Inspect the snapshot for substrate kinds the worker
/// declines to attempt and for input/output kind mismatch.
/// Returns `Ok(())` for kinds the worker will drive; the
/// `Err` variants let the caller publish the appropriate
/// status without spawning a substrate task.
fn precheck_substrate(
    endpoints: &CompositionEndpoints,
) -> Result<(), ByteFlowError> {
    if endpoints.input.kind != endpoints.output.kind {
        return Err(ByteFlowError::MixedSubstrate {
            input: endpoints.input.kind,
            output: endpoints.output.kind,
        });
    }
    match endpoints.input.kind {
        EndpointKind::NamedPipe => Ok(()),
        #[cfg(feature = "alsa-substrate")]
        EndpointKind::AlsaPcm => Ok(()),
        #[cfg(not(feature = "alsa-substrate"))]
        EndpointKind::AlsaPcm => {
            Err(ByteFlowError::UnsupportedKind(EndpointKind::AlsaPcm))
        }
        kind @ (EndpointKind::SharedMemory | EndpointKind::JackPort) => {
            Err(ByteFlowError::UnsupportedKind(kind))
        }
    }
}

/// Reactor loop. Awakens on the wake signal (route changes)
/// or the shutdown signal (unload). Each wake triggers a
/// refetch of the routing handle's `composition_endpoints`,
/// publishes the new value (or `None` for pre-reconciliation
/// state) on the watch channel, and bumps the refresh
/// counter so tests can observe progress.
async fn run_reactor(
    routing: Arc<dyn AudioRouting>,
    wake: Arc<Notify>,
    shutdown: Arc<Notify>,
    endpoints_tx: watch::Sender<Option<CompositionEndpoints>>,
    refresh_count: Arc<std::sync::atomic::AtomicU64>,
) {
    loop {
        tokio::select! {
            _ = wake.notified() => {
                let snapshot = match routing.composition_endpoints() {
                    Ok(ep) => Some(ep),
                    Err(AudioRoutingError::EndpointNotConfigured) => None,
                    Err(other) => {
                        tracing::warn!(
                            error = %other,
                            "audio_routing surface returned unexpected error \
                             during route-change refresh; preserving previous \
                             snapshot"
                        );
                        refresh_count
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        continue;
                    }
                };
                if endpoints_tx.send(snapshot).is_err() {
                    // Receiver side dropped — nobody reads
                    // these snapshots anymore. The plugin
                    // is on its way out; exit the reactor.
                    break;
                }
                refresh_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            _ = shutdown.notified() => {
                break;
            }
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
        async move {
            self.install_routing(ctx.audio_routing.clone())?;
            self.spawn_reactor().await?;
            self.spawn_worker().await?;
            // Subscribe to `audio.options.settings` so operator
            // EQ gestures (set_eq_engaged + set_eq_band) reach
            // the byte-flow worker. Best-effort; the subscriber
            // logs + exits silently when the substrate is not
            // wired (e.g. OOP transport pre-wire-surface) so the
            // composition stage stays available for the
            // passthrough mode without the operator surface.
            self.spawn_options_settings_subscriber(ctx).await;
            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            // Stop worker first — it consumes the
            // reactor's snapshot channel; tearing the
            // reactor down before the worker would race
            // the worker against a closed channel.
            self.stop_worker().await;
            self.stop_reactor().await;
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

            // eq_only refuses at the mode-select gesture when the
            // current topology's negotiated format is known and
            // unsupported by the EQ DSP (anything other than
            // s16le or f32le PCM, mono / stereo). When the
            // topology has not yet been published
            // (EndpointNotConfigured), accept the mode — the
            // worker's pump loop emits a structured Failed
            // status when a non-supported topology lands later,
            // so the failure surfaces observably either way.
            // Schema acceptance row
            // `eq-only-sample-format-coverage` pins this
            // contract.
            if mode == MODE_EQ_ONLY {
                if let Some(routing) = self.audio_routing.as_ref() {
                    if let Ok(endpoints) = routing.composition_endpoints() {
                        if let Some(reason) =
                            check_eq_only_format_support(&endpoints)
                        {
                            return encode_response(
                                req,
                                SelectModeResponse::bad_request(reason),
                            );
                        }
                    }
                }
            }

            self.current_mode = mode.to_string();
            // Publish the new mode to the byte-flow substrate.
            // The substrate's pump loop observes mode changes
            // inline and branches between passthrough and
            // eq_only processing without restarting the
            // substrate lifecycle. send_replace ignores the
            // empty-receiver case (no worker yet) — the next
            // worker spawn reads the current value on
            // subscribe.
            self.mode_tx.send_replace(self.current_mode.clone());
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

    #[tokio::test]
    async fn describe_matches_embedded_manifest() {
        let p = AlsaCompositionPlugin::new();
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
        let p = AlsaCompositionPlugin::new();
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
        let stub = Arc::new(StubAudioRouting::new());
        p.install_routing(Some(Arc::clone(&stub) as _)).unwrap();
        p.spawn_reactor().await.unwrap();
        assert!(p.loaded);
        assert!(stub.has_route_change_callback());
        p.unload().await.unwrap();
        assert!(!p.loaded);
        assert!(p.audio_routing.is_none());
        assert!(p.reactor.is_none());
        assert!(
            !stub.has_route_change_callback(),
            "unload must clear the route-change callback so the framework's \
             reference is released"
        );
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
            principal_scope: None,
            has_step_up: false,
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
        // `resampler` is a future mode token reserved in the
        // schema but not implemented in this build; the runtime
        // mode list (`DECLARED_MODES`) carries `passthrough` +
        // `eq_only` and the refusal must name the unknown token
        // explicitly.
        let req = Request {
            request_type: REQUEST_COMPOSITION_SELECT_MODE.to_string(),
            payload: json!({ "v": 1, "mode": "resampler" })
                .to_string()
                .into_bytes(),
            correlation_id: 2,
            deadline: None,
            instance_id: None,
            principal_scope: None,
            has_step_up: false,
        };
        let resp = p.handle_request(&req).await.unwrap();
        let v = decode_payload(&resp.payload);
        assert_eq!(v["status"], "bad_request");
        let err = v["error"].as_str().unwrap();
        assert!(err.contains("unknown mode"), "got: {err}");
        assert!(err.contains("resampler"), "got: {err}");
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
            principal_scope: None,
            has_step_up: false,
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
            principal_scope: None,
            has_step_up: false,
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
            principal_scope: None,
            has_step_up: false,
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
            principal_scope: None,
            has_step_up: false,
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
            principal_scope: None,
            has_step_up: false,
        };
        let err = p.handle_request(&req).await.unwrap_err();
        match err {
            PluginError::Permanent(msg) => {
                assert!(msg.contains("unknown request type"));
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    // -- Chunk C: route-change reactor ---------------------------------

    use super::test_support::{default_alsa_endpoints, route_change};
    use evo_plugin_sdk::audio::{AudioFormat, PcmCodec};

    /// Wait until the reactor's refresh counter advances
    /// from `prior` to at least `prior + advances`. Bounded
    /// to keep CI happy if the reactor is wedged.
    async fn wait_for_refresh(
        plugin: &AlsaCompositionPlugin,
        prior: u64,
        advances: u64,
    ) {
        let target = prior + advances;
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(500);
        loop {
            if plugin.refresh_count() >= target {
                return;
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "reactor refresh counter did not advance from {prior} to \
                     {target} within 500ms"
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }

    #[tokio::test]
    async fn spawn_reactor_registers_route_change_callback() {
        let mut p = AlsaCompositionPlugin::new();
        let stub = Arc::new(StubAudioRouting::new());
        assert!(!stub.has_route_change_callback());
        p.install_routing(Some(Arc::clone(&stub) as _)).unwrap();
        p.spawn_reactor().await.unwrap();
        assert!(stub.has_route_change_callback());
        // unload tears down both the reactor and the
        // callback registration
        p.unload().await.unwrap();
        assert!(!stub.has_route_change_callback());
    }

    #[tokio::test]
    async fn spawn_reactor_publishes_initial_endpoints_when_topology_present() {
        let mut p = AlsaCompositionPlugin::new();
        let stub = Arc::new(StubAudioRouting::new());
        stub.set_endpoints(default_alsa_endpoints());
        p.install_routing(Some(Arc::clone(&stub) as _)).unwrap();
        p.spawn_reactor().await.unwrap();

        let rx = p.subscribe_endpoints().expect("reactor running");
        let snapshot = rx.borrow().clone();
        assert!(
            snapshot.is_some(),
            "initial endpoint fetch should pick up the published topology"
        );
        assert_eq!(snapshot.unwrap(), default_alsa_endpoints());

        p.unload().await.unwrap();
    }

    #[tokio::test]
    async fn spawn_reactor_publishes_none_when_topology_absent() {
        let mut p = AlsaCompositionPlugin::new();
        let stub = Arc::new(StubAudioRouting::new());
        p.install_routing(Some(Arc::clone(&stub) as _)).unwrap();
        p.spawn_reactor().await.unwrap();

        let rx = p.subscribe_endpoints().expect("reactor running");
        assert!(
            rx.borrow().is_none(),
            "EndpointNotConfigured must publish None, not propagate as error"
        );

        p.unload().await.unwrap();
    }

    // ---- eq_only format-refusal coverage ----

    #[tokio::test]
    async fn select_mode_eq_only_refuses_on_unsupported_pcm_codec() {
        // Reuse the default ALSA endpoints from test_support but
        // swap the codec to s24le which the EQ DSP does not
        // support.
        let mut p = AlsaCompositionPlugin::new();
        let stub = Arc::new(StubAudioRouting::new());
        let mut endpoints = crate::test_support::default_alsa_endpoints();
        if let AudioFormat::Pcm { codec, .. } = &mut endpoints.input.format {
            *codec = PcmCodec::PcmS24Le;
        }
        if let AudioFormat::Pcm { codec, .. } = &mut endpoints.output.format {
            *codec = PcmCodec::PcmS24Le;
        }
        stub.set_endpoints(endpoints);
        p.install_routing(Some(Arc::clone(&stub) as _)).unwrap();
        let req = Request {
            request_type: REQUEST_COMPOSITION_SELECT_MODE.to_string(),
            payload: json!({ "v": 1, "mode": "eq_only" })
                .to_string()
                .into_bytes(),
            correlation_id: 1,
            deadline: None,
            instance_id: None,
            principal_scope: None,
            has_step_up: false,
        };
        let resp = p.handle_request(&req).await.unwrap();
        let v = decode_payload(&resp.payload);
        assert_eq!(v["status"], "bad_request");
        let err = v["error"].as_str().unwrap();
        assert!(
            err.contains("eq_only refused") && err.contains("PcmS24Le"),
            "refusal must name eq_only + the unsupported codec; got: {err}"
        );
        // Current mode unchanged.
        assert_eq!(p.current_mode(), MODE_PASSTHROUGH);
    }

    #[tokio::test]
    async fn select_mode_eq_only_accepts_supported_pcm_codec() {
        let mut p = AlsaCompositionPlugin::new();
        let stub = Arc::new(StubAudioRouting::new());
        let mut endpoints = crate::test_support::default_alsa_endpoints();
        if let AudioFormat::Pcm { codec, .. } = &mut endpoints.input.format {
            *codec = PcmCodec::PcmF32;
        }
        if let AudioFormat::Pcm { codec, .. } = &mut endpoints.output.format {
            *codec = PcmCodec::PcmF32;
        }
        stub.set_endpoints(endpoints);
        p.install_routing(Some(Arc::clone(&stub) as _)).unwrap();
        let req = Request {
            request_type: REQUEST_COMPOSITION_SELECT_MODE.to_string(),
            payload: json!({ "v": 1, "mode": "eq_only" })
                .to_string()
                .into_bytes(),
            correlation_id: 1,
            deadline: None,
            instance_id: None,
            principal_scope: None,
            has_step_up: false,
        };
        let resp = p.handle_request(&req).await.unwrap();
        let v = decode_payload(&resp.payload);
        assert_eq!(v["status"], "ok");
        assert_eq!(p.current_mode(), MODE_EQ_ONLY);
    }

    #[tokio::test]
    async fn select_mode_eq_only_accepts_when_topology_not_yet_configured() {
        // No topology published — the helper returns None
        // (EndpointNotConfigured), the setter accepts the mode.
        // The worker emits Failed when an unsupported topology
        // lands later, so the failure remains observable.
        let mut p = AlsaCompositionPlugin::new();
        let stub = Arc::new(StubAudioRouting::new());
        // No set_endpoints — composition_endpoints returns
        // EndpointNotConfigured.
        p.install_routing(Some(Arc::clone(&stub) as _)).unwrap();
        let req = Request {
            request_type: REQUEST_COMPOSITION_SELECT_MODE.to_string(),
            payload: json!({ "v": 1, "mode": "eq_only" })
                .to_string()
                .into_bytes(),
            correlation_id: 1,
            deadline: None,
            instance_id: None,
            principal_scope: None,
            has_step_up: false,
        };
        let resp = p.handle_request(&req).await.unwrap();
        let v = decode_payload(&resp.payload);
        assert_eq!(v["status"], "ok");
        assert_eq!(p.current_mode(), MODE_EQ_ONLY);
    }

    // ---- audio.options.settings parser coverage ----

    #[test]
    fn parse_eq_runtime_state_default_when_empty() {
        let s = parse_eq_runtime_state_from_state(&json!({}));
        assert!(!s.engaged);
        assert_eq!(s.bands.len(), crate::eq_dsp::EQ_BAND_COUNT);
        for b in s.bands.iter() {
            assert_eq!(b.freq_hz, 1000);
            assert_eq!(b.gain_db, 0.0);
            assert_eq!(b.q, 1.0);
        }
    }

    #[test]
    fn parse_eq_runtime_state_extracts_engaged_flag() {
        let s = parse_eq_runtime_state_from_state(&json!({"eq_engaged": true}));
        assert!(s.engaged);
    }

    #[test]
    fn parse_eq_runtime_state_extracts_band_array() {
        // Two bands configured; the rest default to flat.
        let s = parse_eq_runtime_state_from_state(&json!({
            "eq_engaged": true,
            "eq_bands": [
                { "freq_hz": 100, "gain_db": 3.0, "q": 0.7 },
                { "freq_hz": 1000, "gain_db": -3.0, "q": 1.41 }
            ]
        }));
        assert!(s.engaged);
        assert_eq!(s.bands[0].freq_hz, 100);
        assert!((s.bands[0].gain_db - 3.0).abs() < 1e-6);
        assert!((s.bands[0].q - 0.7).abs() < 1e-6);
        assert_eq!(s.bands[1].freq_hz, 1000);
        assert!((s.bands[1].gain_db - -3.0).abs() < 1e-6);
        assert!((s.bands[1].q - 1.41).abs() < 1e-6);
        // Remaining bands fall through to defaults.
        for i in 2..crate::eq_dsp::EQ_BAND_COUNT {
            assert_eq!(s.bands[i], crate::eq_dsp::EqBandParams::default());
        }
    }

    #[test]
    fn parse_eq_runtime_state_caps_at_band_count() {
        // 15 bands sent; parser caps at EQ_BAND_COUNT (10).
        let extra = (0..15)
            .map(|i| {
                json!({
                    "freq_hz": 100 + i * 100,
                    "gain_db": 0.0,
                    "q": 1.0,
                })
            })
            .collect::<Vec<_>>();
        let s = parse_eq_runtime_state_from_state(&json!({
            "eq_engaged": false,
            "eq_bands": extra,
        }));
        assert_eq!(s.bands.len(), crate::eq_dsp::EQ_BAND_COUNT);
        // Band 9 (last accepted) carries the corresponding
        // payload row.
        assert_eq!(s.bands[9].freq_hz, 100 + 9 * 100);
    }

    #[tokio::test]
    async fn route_change_refreshes_endpoints_via_reactor() {
        let mut p = AlsaCompositionPlugin::new();
        let stub = Arc::new(StubAudioRouting::new());
        // Start with a published topology so the initial
        // fetch is meaningful.
        stub.set_endpoints(default_alsa_endpoints());
        p.install_routing(Some(Arc::clone(&stub) as _)).unwrap();
        p.spawn_reactor().await.unwrap();

        let mut rx = p.subscribe_endpoints().expect("reactor running");
        let prior_refresh = p.refresh_count();
        let prior_snapshot = rx.borrow().clone();
        assert!(prior_snapshot.is_some());

        // Publish a new topology at a different format and
        // fire the route change. The reactor must refetch
        // and republish.
        let new_format = AudioFormat::Pcm {
            codec: PcmCodec::PcmS24Le,
            rate_hz: 192_000,
            channels: 2,
        };
        let mut new_endpoints = default_alsa_endpoints();
        new_endpoints.input.format = new_format.clone();
        new_endpoints.output.format = new_format.clone();
        stub.set_endpoints(new_endpoints.clone());
        assert!(stub.fire_route_change(route_change(new_format.clone())));

        wait_for_refresh(&p, prior_refresh, 1).await;
        rx.changed().await.expect("watch channel still alive");
        let snapshot = rx.borrow().clone();
        assert_eq!(snapshot, Some(new_endpoints));

        p.unload().await.unwrap();
    }

    #[tokio::test]
    async fn many_route_changes_do_not_leak_or_deadlock() {
        let mut p = AlsaCompositionPlugin::new();
        let stub = Arc::new(StubAudioRouting::new());
        stub.set_endpoints(default_alsa_endpoints());
        p.install_routing(Some(Arc::clone(&stub) as _)).unwrap();
        p.spawn_reactor().await.unwrap();

        let format = AudioFormat::Pcm {
            codec: PcmCodec::PcmS16Le,
            rate_hz: 48_000,
            channels: 2,
        };
        for _ in 0..32 {
            let prior_refresh = p.refresh_count();
            assert!(stub.fire_route_change(route_change(format.clone())));
            wait_for_refresh(&p, prior_refresh, 1).await;
        }

        // Reactor still healthy: another fire must still
        // be processed.
        let final_refresh = p.refresh_count();
        assert!(stub.fire_route_change(route_change(format)));
        wait_for_refresh(&p, final_refresh, 1).await;

        p.unload().await.unwrap();
    }

    #[tokio::test]
    async fn unload_terminates_reactor_promptly() {
        let mut p = AlsaCompositionPlugin::new();
        let stub = Arc::new(StubAudioRouting::new());
        p.install_routing(Some(Arc::clone(&stub) as _)).unwrap();
        p.spawn_reactor().await.unwrap();

        let started = std::time::Instant::now();
        p.unload().await.unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(200),
            "unload must drain the reactor quickly; took {elapsed:?}"
        );
        assert!(p.reactor.is_none());
    }

    // -- Chunk D: byte-flow worker -------------------------------------

    use super::test_support::{make_fifo_pair, named_pipe_endpoints};
    use evo_plugin_sdk::contract::audio_routing::{
        ReadEndpoint, WriteEndpoint,
    };
    use std::path::PathBuf;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Wait until the worker's status channel reports a
    /// state matching the predicate. Bounded so a wedged
    /// worker doesn't hang CI.
    async fn wait_for_worker_status<F>(
        rx: &mut watch::Receiver<WorkerStatus>,
        deadline_ms: u64,
        mut predicate: F,
    ) -> WorkerStatus
    where
        F: FnMut(&WorkerStatus) -> bool,
    {
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(deadline_ms);
        // Check current value first so the test doesn't
        // stall waiting for a change that already
        // happened.
        if predicate(&rx.borrow()) {
            return rx.borrow().clone();
        }
        loop {
            if std::time::Instant::now() >= deadline {
                panic!(
                    "worker did not reach the expected status within \
                     {deadline_ms}ms; current = {:?}",
                    rx.borrow()
                );
            }
            tokio::select! {
                _ = rx.changed() => {
                    if predicate(&rx.borrow()) {
                        return rx.borrow().clone();
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
            }
        }
    }

    /// Concurrently open the test side of a FIFO pair —
    /// writer on input (test acts as upstream source),
    /// reader on output (test acts as downstream
    /// delivery). Both opens are async and the FIFO open
    /// path blocks until both ends connect, so the worker
    /// must already be on its way to opening the other
    /// ends.
    async fn open_test_fifo_sides(
        input_path: PathBuf,
        output_path: PathBuf,
    ) -> (tokio::fs::File, tokio::fs::File) {
        let mut write_opts = tokio::fs::OpenOptions::new();
        write_opts.write(true);
        let mut read_opts = tokio::fs::OpenOptions::new();
        read_opts.read(true);
        let writer_fut = write_opts.open(input_path);
        let reader_fut = read_opts.open(output_path);
        let (writer, reader) = tokio::join!(writer_fut, reader_fut);
        (
            writer.expect("test-side open input fifo"),
            reader.expect("test-side open output fifo"),
        )
    }

    #[tokio::test]
    async fn worker_idle_when_topology_absent() {
        let mut p = AlsaCompositionPlugin::new();
        let stub = Arc::new(StubAudioRouting::new());
        p.install_routing(Some(Arc::clone(&stub) as _)).unwrap();
        p.spawn_reactor().await.unwrap();
        p.spawn_worker().await.unwrap();

        let mut rx = p.subscribe_worker_status().expect("worker running");
        wait_for_worker_status(&mut rx, 500, |s| {
            matches!(s, WorkerStatus::Idle)
        })
        .await;

        p.unload().await.unwrap();
    }

    #[tokio::test]
    async fn worker_unsupported_when_substrate_kind_unimplemented() {
        let mut p = AlsaCompositionPlugin::new();
        let stub = Arc::new(StubAudioRouting::new());
        // Default ALSA endpoints point at AlsaPcm — the
        // libasound link is not wired in this build. Worker
        // must publish Unsupported, not Failed or Running.
        stub.set_endpoints(crate::test_support::default_alsa_endpoints());
        p.install_routing(Some(Arc::clone(&stub) as _)).unwrap();
        p.spawn_reactor().await.unwrap();
        p.spawn_worker().await.unwrap();

        let mut rx = p.subscribe_worker_status().expect("worker running");
        let status = wait_for_worker_status(&mut rx, 500, |s| {
            matches!(s, WorkerStatus::Unsupported { .. })
        })
        .await;
        match status {
            WorkerStatus::Unsupported { kind } => {
                assert_eq!(kind, EndpointKind::AlsaPcm);
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }

        p.unload().await.unwrap();
    }

    #[tokio::test]
    async fn worker_running_when_named_pipe_substrate_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (input_path, output_path) = make_fifo_pair(dir.path());

        let mut p = AlsaCompositionPlugin::new();
        let stub = Arc::new(StubAudioRouting::new());
        stub.set_endpoints(named_pipe_endpoints(
            input_path.clone(),
            output_path.clone(),
        ));
        p.install_routing(Some(Arc::clone(&stub) as _)).unwrap();
        p.spawn_reactor().await.unwrap();
        p.spawn_worker().await.unwrap();

        // Connect the test sides; the worker is already
        // attempting to open its sides.
        let (mut writer, mut reader) =
            open_test_fifo_sides(input_path, output_path).await;

        let mut status_rx =
            p.subscribe_worker_status().expect("worker running");
        wait_for_worker_status(&mut status_rx, 500, |s| {
            matches!(
                s,
                WorkerStatus::Running {
                    kind: EndpointKind::NamedPipe
                }
            )
        })
        .await;

        // Pump a frame through and assert byte-identical
        // delivery on the output.
        let payload: [u8; 8] = [0x01, 0x02, 0x03, 0x04, 0xAA, 0xBB, 0xCC, 0xDD];
        writer.write_all(&payload).await.expect("write payload");
        writer.flush().await.expect("flush payload");

        let mut received = [0u8; 8];
        reader
            .read_exact(&mut received)
            .await
            .expect("read echoed payload");
        assert_eq!(payload, received);

        p.unload().await.unwrap();
    }

    #[tokio::test]
    async fn worker_eq_only_engaged_amplifies_centre_band() {
        // End-to-end EQ integration: mode = eq_only, engaged
        // = true, band 0 = peaking @ 1 kHz + 12 dB / Q=1.0.
        // Pump a 1 kHz sine through f32le stereo named-pipe.
        // The output amplitude must be substantially boosted
        // relative to the input — proving the worker has
        // actually routed the bytes through the EqProcessor.
        let dir = tempfile::tempdir().expect("tempdir");
        let (input_path, output_path) =
            crate::test_support::make_fifo_pair(dir.path());

        let mut p = AlsaCompositionPlugin::new();
        let stub = Arc::new(StubAudioRouting::new());
        let f32_endpoints = CompositionEndpoints {
            input: ReadEndpoint {
                kind: EndpointKind::NamedPipe,
                path: input_path.clone(),
                format: AudioFormat::Pcm {
                    codec: PcmCodec::PcmF32,
                    rate_hz: 44_100,
                    channels: 2,
                },
                buffer_frames: 1024,
            },
            output: WriteEndpoint {
                kind: EndpointKind::NamedPipe,
                path: output_path.clone(),
                format: AudioFormat::Pcm {
                    codec: PcmCodec::PcmF32,
                    rate_hz: 44_100,
                    channels: 2,
                },
                buffer_frames: 1024,
            },
        };
        stub.set_endpoints(f32_endpoints);
        p.install_routing(Some(Arc::clone(&stub) as _)).unwrap();
        p.spawn_reactor().await.unwrap();

        // Pre-publish the operator settings BEFORE spawning
        // the worker so the substrate reads the engaged +
        // boosted state at substrate-open time.
        p.mode_tx.send_replace(MODE_EQ_ONLY.to_string());
        let mut bands = [crate::eq_dsp::EqBandParams::default();
            crate::eq_dsp::EQ_BAND_COUNT];
        bands[0] = crate::eq_dsp::EqBandParams {
            freq_hz: 1000,
            gain_db: 12.0,
            q: 1.0,
        };
        p.eq_state_tx.send_replace(EqRuntimeState {
            engaged: true,
            bands,
        });

        p.spawn_worker().await.unwrap();
        let (mut writer, mut reader) =
            open_test_fifo_sides(input_path, output_path).await;

        let mut status_rx =
            p.subscribe_worker_status().expect("worker running");
        wait_for_worker_status(&mut status_rx, 500, |s| {
            matches!(
                s,
                WorkerStatus::Running {
                    kind: EndpointKind::NamedPipe
                }
            )
        })
        .await;

        // Build a 1 kHz sine + write to the input pipe.
        // 4410 frames = 100 ms at 44.1 kHz.
        let two_pi_f = 2.0 * std::f64::consts::PI * 1000.0;
        let mut input_bytes = Vec::with_capacity(4410 * 2 * 4);
        for i in 0..4410 {
            let t = i as f64 / 44_100.0;
            let s = (two_pi_f * t).sin() as f32 * 0.3; // -10 dB headroom
            for _ in 0..2 {
                input_bytes.extend_from_slice(&s.to_le_bytes());
            }
        }
        writer.write_all(&input_bytes).await.expect("write");
        writer.flush().await.expect("flush");

        let mut output_bytes = vec![0u8; input_bytes.len()];
        reader.read_exact(&mut output_bytes).await.expect("read");

        // Measure peak amplitude on the second half (filter
        // has settled). Boost should be ≥ 1.5x relative to
        // the input peak.
        let mut peak_in: f32 = 0.0;
        let mut peak_out: f32 = 0.0;
        for f in 2205..4410 {
            let off = f * 2 * 4;
            let s_in = f32::from_le_bytes([
                input_bytes[off],
                input_bytes[off + 1],
                input_bytes[off + 2],
                input_bytes[off + 3],
            ]);
            let s_out = f32::from_le_bytes([
                output_bytes[off],
                output_bytes[off + 1],
                output_bytes[off + 2],
                output_bytes[off + 3],
            ]);
            peak_in = peak_in.max(s_in.abs());
            peak_out = peak_out.max(s_out.abs());
        }
        assert!(
            peak_out > peak_in * 1.5,
            "engaged EQ must boost the centre-frequency sine \
             end-to-end through the worker; \
             peak_in={peak_in} peak_out={peak_out}"
        );

        p.unload().await.unwrap();
    }

    #[tokio::test]
    async fn worker_eq_only_disengaged_passes_bytes_unchanged() {
        // Mirror of the previous test but with engaged =
        // false. The worker still runs eq_only mode but the
        // pump loop branches to passthrough; bytes must
        // arrive byte-identical on the output.
        let dir = tempfile::tempdir().expect("tempdir");
        let (input_path, output_path) =
            crate::test_support::make_fifo_pair(dir.path());

        let mut p = AlsaCompositionPlugin::new();
        let stub = Arc::new(StubAudioRouting::new());
        let f32_endpoints = CompositionEndpoints {
            input: ReadEndpoint {
                kind: EndpointKind::NamedPipe,
                path: input_path.clone(),
                format: AudioFormat::Pcm {
                    codec: PcmCodec::PcmF32,
                    rate_hz: 44_100,
                    channels: 2,
                },
                buffer_frames: 1024,
            },
            output: WriteEndpoint {
                kind: EndpointKind::NamedPipe,
                path: output_path.clone(),
                format: AudioFormat::Pcm {
                    codec: PcmCodec::PcmF32,
                    rate_hz: 44_100,
                    channels: 2,
                },
                buffer_frames: 1024,
            },
        };
        stub.set_endpoints(f32_endpoints);
        p.install_routing(Some(Arc::clone(&stub) as _)).unwrap();
        p.spawn_reactor().await.unwrap();
        // Mode is eq_only but engaged is false.
        p.mode_tx.send_replace(MODE_EQ_ONLY.to_string());
        // EqRuntimeState::default has engaged = false.

        p.spawn_worker().await.unwrap();
        let (mut writer, mut reader) =
            open_test_fifo_sides(input_path, output_path).await;
        let mut status_rx =
            p.subscribe_worker_status().expect("worker running");
        wait_for_worker_status(&mut status_rx, 500, |s| {
            matches!(
                s,
                WorkerStatus::Running {
                    kind: EndpointKind::NamedPipe
                }
            )
        })
        .await;

        let payload: [u8; 16] = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0xAA, 0xBB, 0xCC,
            0xDD, 0xEE, 0xFF, 0x11, 0x22,
        ];
        writer.write_all(&payload).await.expect("write");
        writer.flush().await.expect("flush");

        let mut received = [0u8; 16];
        reader.read_exact(&mut received).await.expect("read");
        assert_eq!(
            payload, received,
            "eq_only mode with engaged=false must pass bytes \
             through unchanged"
        );

        p.unload().await.unwrap();
    }

    #[tokio::test]
    async fn worker_failed_on_mixed_substrate_kinds() {
        let mut p = AlsaCompositionPlugin::new();
        let stub = Arc::new(StubAudioRouting::new());
        // Build a deliberately mismatched endpoint pair:
        // input AlsaPcm, output NamedPipe. Passthrough
        // mode requires homogeneous substrate.
        let mut endpoints = crate::test_support::default_alsa_endpoints();
        endpoints.output.kind = EndpointKind::NamedPipe;
        stub.set_endpoints(endpoints);
        p.install_routing(Some(Arc::clone(&stub) as _)).unwrap();
        p.spawn_reactor().await.unwrap();
        p.spawn_worker().await.unwrap();

        let mut rx = p.subscribe_worker_status().expect("worker running");
        let status = wait_for_worker_status(&mut rx, 500, |s| {
            matches!(s, WorkerStatus::Failed { .. })
        })
        .await;
        match status {
            WorkerStatus::Failed { reason } => {
                assert!(
                    reason.contains("substrate kinds differ"),
                    "expected mixed-substrate diagnostic, got {reason}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }

        p.unload().await.unwrap();
    }

    #[tokio::test]
    async fn worker_terminates_promptly_on_unload() {
        let mut p = AlsaCompositionPlugin::new();
        let stub = Arc::new(StubAudioRouting::new());
        p.install_routing(Some(Arc::clone(&stub) as _)).unwrap();
        p.spawn_reactor().await.unwrap();
        p.spawn_worker().await.unwrap();

        let mut rx = p.subscribe_worker_status().expect("worker running");
        wait_for_worker_status(&mut rx, 500, |s| {
            matches!(s, WorkerStatus::Idle)
        })
        .await;

        let started = std::time::Instant::now();
        p.unload().await.unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(200),
            "unload must drain the worker quickly; took {elapsed:?}"
        );
        assert!(p.worker.is_none());
        assert!(p.reactor.is_none());
    }
}
