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
//! async I/O). The `EndpointKind::AlsaPcm` substrate lands
//! in the next chunk together with the libasound link and
//! reference target cross-compile + real-hardware verification.
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
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities,
};
use evo_plugin_sdk::Manifest;
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;

use crate::byte_flow::{run_substrate, ByteFlowError};

mod byte_flow;

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
const DECLARED_MODES: &[&str] = &[MODE_PASSTHROUGH];

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
    /// Active composition mode token. Reset to
    /// [`MODE_PASSTHROUGH`] at every successful load.
    current_mode: String,
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
        Self {
            loaded: false,
            current_mode: MODE_PASSTHROUGH.to_string(),
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
        let (status_tx, status_rx) = watch::channel(WorkerStatus::Idle);
        let shutdown = Arc::new(Notify::new());
        let task_shutdown = Arc::clone(&shutdown);
        let task = tokio::spawn(async move {
            run_worker(endpoints_rx, task_shutdown, status_tx).await;
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
async fn run_worker(
    mut endpoints_rx: watch::Receiver<Option<CompositionEndpoints>>,
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
    let mut run_handle = tokio::spawn(async move {
        run_substrate(&endpoints_for_run, cancel_for_run).await
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
            self.spawn_worker().await
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

            self.current_mode = mode.to_string();
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
        let req = Request {
            request_type: REQUEST_COMPOSITION_SELECT_MODE.to_string(),
            payload: json!({ "v": 1, "mode": "eq_only" })
                .to_string()
                .into_bytes(),
            correlation_id: 2,
            deadline: None,
            instance_id: None,
        };
        let resp = p.handle_request(&req).await.unwrap();
        let v = decode_payload(&resp.payload);
        assert_eq!(v["status"], "bad_request");
        let err = v["error"].as_str().unwrap();
        assert!(err.contains("unknown mode"), "got: {err}");
        assert!(err.contains("eq_only"), "got: {err}");
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
        // Default ALSA endpoints point at AlsaPcm — not
        // implemented in chunk D. Worker must publish
        // Unsupported, not Failed or Running.
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
