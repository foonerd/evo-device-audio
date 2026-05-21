//! # org-evoframework-delivery-alsa
//!
//! Delivery-stage plugin for the evo audio data plane. Owns the
//! modular ALSA pipeline (`pcm.evo`) and declares it as the
//! [`WriteEndpoint`] other audio-producing plugins write into.
//! Stocks the `audio.delivery` shelf at shape 2.
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
//!   `delivery.active_endpoint` verb and re-renders it on
//!   operator-driven settings changes when the renderer path
//!   is wired.
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
//!   asound.conf pipeline on every rewire once the renderer
//!   path is wired.
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

mod options_render;

use std::future::Future;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use evo_plugin_sdk::contract::audio_routing::{
    AudioRouting, AudioRoutingError, ReadEndpoint, RouteChange,
    RouteChangeCallback,
};
use evo_plugin_sdk::contract::{
    BuildInfo, ExternalAddressing, HealthReport, LoadContext, Plugin,
    PluginDescription, PluginError, PluginIdentity, Request, Respondent,
    Response, RuntimeCapabilities, SubjectAnnouncement, SubjectAnnouncer,
    SubjectStateStreamError,
};
use evo_plugin_sdk::Manifest;

pub mod alsa_cards;
pub mod output_enumeration;

use alsa_cards::AlsaCardCatalog;
use output_enumeration::ResolvedAlsaOutput;
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Notify, RwLock};
use tokio::task::JoinHandle;

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

/// Canonical filesystem path of the operator-options drop-in.
/// The reference distribution's bootstrap script installs the
/// directory (`/etc/asound.d/`) owned by the steward service
/// user so the delivery plugin can atomic-write this file
/// without sudoers escalation. The base `/etc/asound.conf`
/// includes this file (ALSA's `<configfile>` directive) so any
/// `pcm.evo` definition here overrides the bootstrap baseline.
const ASOUND_OPTIONS_DROP_IN_PATH: &str = "/etc/asound.d/evo-options.conf";

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
    "delivery.list_outputs",
];

/// Subject scheme + value for the resolved-outputs surface.
/// Published once at load and read back by the
/// `delivery.list_outputs` respondent verb. Hot-plug re-publish
/// and reactive subscription on hardware change land in a
/// follow-on chunk; the load-time publish is the canonical
/// boot-state surface UI consumes today.
const SUBJECT_SCHEME_DELIVERY: &str = "evo.audio.delivery";
const SUBJECT_VALUE_OUTPUTS: &str = "outputs";
const SUBJECT_TYPE_OUTPUTS: &str = "audio_delivery_outputs";

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
    /// Path the plugin writes the operator-options drop-in to
    /// on each `audio.options.settings` subject update.
    /// Defaults to [`ASOUND_OPTIONS_DROP_IN_PATH`]; tests
    /// override via `with_asound_options_drop_in_path`.
    asound_options_drop_in_path: PathBuf,
    /// Cumulative respondent requests handled since
    /// construction. Surfaced for diagnostics; not part of the
    /// wire contract.
    requests_handled: u64,
    /// Route-change reactor handle. `Some` after a successful
    /// `Plugin::load`; `None` before first load and after
    /// `Plugin::unload`. Mirrors the reactor shape composition
    /// .alsa and playback.mpd ship — every audio-tier plugin
    /// reacts to topology rewires through the same primitive.
    reactor: Option<ReactorHandle>,
    /// Options-subject observer task handle. `Some` after a
    /// successful `Plugin::load` when LoadContext supplied the
    /// subject_state_subscriber + the playback.options settings
    /// subject was resolvable; `None` otherwise. The task drains
    /// state updates from the
    /// `(scheme: "evo.audio.options", value: "settings")`
    /// subject. On each update the observer renders the
    /// `pcm.evo` drop-in via `options_render::render_drop_in`,
    /// atomic-writes it to the drop-in path, and emits both
    /// `delivery.options_observed` (the observer-visibility
    /// signal) and `delivery.options_rendered` (the on-disk
    /// drop-in's freshness signal). Subscribers serialise
    /// re-bind work against the rendered happening.
    options_observer: Option<OptionsObserverHandle>,
    /// Hardware-audio active-config observer task handle. `Some`
    /// after a successful `Plugin::load` when LoadContext supplied
    /// the subject_state_subscriber + the
    /// `evo.hardware.audio:active_config` subject was resolvable;
    /// `None` otherwise. The task drains state updates from the
    /// hardware.audio-config plugin and caches the resolved
    /// active-DAC snapshot in [`active_dac_config`] so the
    /// `delivery.active_endpoint` verb surfaces a unified
    /// "what is currently bound" snapshot to the operator UI
    /// without parsing `aplay -L` output.
    hardware_audio_observer: Option<HardwareAudioObserverHandle>,
    /// Cached hardware-audio active configuration snapshot. `None`
    /// before the first observer update; `Some(...)` once the
    /// observer has seen at least one state from the
    /// hardware-audio-config plugin. Read by the
    /// `delivery.active_endpoint` verb to surface the resolved
    /// alsacard / mixer hint downstream consumers can bind
    /// against.
    active_dac_config: Arc<RwLock<Option<ActiveDacConfig>>>,
    /// Embedded static ALSA card catalog. Loaded at admission
    /// from the baked-in `data/alsa-cards.toml`; provides the
    /// raw-card-name → operator-friendly-label resolution that
    /// `delivery.list_outputs` joins `aplay -l` rows against.
    card_catalog: Option<Arc<AlsaCardCatalog>>,
    /// Subject announcer pulled from
    /// [`LoadContext::subject_announcer`] at load. Used to
    /// publish the resolved-outputs subject once at load.
    subject_announcer: Option<Arc<dyn SubjectAnnouncer>>,
    /// Cached snapshot of the resolved ALSA outputs published at
    /// load time. `delivery.list_outputs` returns this verbatim;
    /// hot-plug re-enumeration lands in a follow-on chunk.
    cached_outputs: Arc<RwLock<Vec<ResolvedAlsaOutput>>>,
}

/// Handle on the route-change reactor task spawned at load.
/// Carries the shutdown signal, the join handle, and the
/// receiver-end of the endpoint-snapshot channel that
/// downstream consumers (e.g. an asound.conf re-renderer)
/// subscribe to.
struct ReactorHandle {
    task: JoinHandle<()>,
    shutdown: Arc<Notify>,
    endpoints_rx: watch::Receiver<Option<ReadEndpoint>>,
    /// Counter bumped on every wake-and-refetch the reactor
    /// performs. Tests poll on this to observe reactor
    /// progress without racy sleeps.
    #[cfg_attr(not(test), allow(dead_code))]
    refresh_count: Arc<std::sync::atomic::AtomicU64>,
}

/// Handle on the options-subject observer task spawned at load.
struct OptionsObserverHandle {
    task: JoinHandle<()>,
    shutdown: Arc<Notify>,
}

/// Handle on the hardware-audio active-config observer task
/// spawned at load.
struct HardwareAudioObserverHandle {
    task: JoinHandle<()>,
    shutdown: Arc<Notify>,
}

/// Cached snapshot of the hardware.audio-config plugin's
/// `evo.hardware.audio:active_config` subject, populated by the
/// observer task on every state update. Operator-facing wire-op
/// `delivery.active_endpoint` returns this verbatim under the
/// `active_dac_config` field so the UI can render
/// "currently playing through: <name> via pcm.evo" without
/// parsing `aplay -L` output.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ActiveDacConfig {
    /// Raw `dtoverlay=` token currently active in the managed
    /// block (empty when no managed block is present).
    #[serde(default)]
    pub overlay: String,
    /// Catalogue id whose `overlay` field matches the active
    /// overlay, or None if no catalogue entry matches.
    #[serde(default)]
    pub catalogue_id: Option<String>,
    /// Operator-friendly display name from the resolved catalogue
    /// entry, or None when no entry matches. Populated by
    /// hardware.audio-config when it enriches the active config
    /// against its DAC catalogue. Consumed by the outputs
    /// enumeration to render the friendly label on rows whose
    /// kernel card name otherwise resolves to a generic /
    /// unmapped token (e.g. `DAC` for any of several DAC HATs).
    #[serde(default)]
    pub display_name: Option<String>,
    /// ALSA card short id hint (e.g. `sndrpihifiberry`, `BossDAC`,
    /// `DAC`). The downstream pcm.evo binding uses this hint to
    /// resolve the real `hw:` card index without re-running
    /// `aplay -L`. None means the operator's selected DAC has no
    /// catalogue mixer hint (no in-DAC mixer; software-volume
    /// path is the only sensible option).
    #[serde(default)]
    pub alsacard_hint: Option<String>,
    /// In-DAC mixer-control hint (e.g. `Digital`, `Master`). None
    /// means no in-DAC mixer; the operator's mixer_type setting
    /// then constrains the chain (`software` or `none`).
    #[serde(default)]
    pub mixer_hint: Option<String>,
    /// Boot-config path the active overlay was read from. Empty
    /// on board classes without a boot-config path.
    #[serde(default)]
    pub boot_config_path: String,
}

impl AlsaDeliveryPlugin {
    /// Construct a fresh plugin instance.
    pub fn new() -> Self {
        Self {
            loaded: false,
            audio_routing: None,
            asound_conf_path: PathBuf::from(ASOUND_CONF_PATH),
            asound_options_drop_in_path: PathBuf::from(
                ASOUND_OPTIONS_DROP_IN_PATH,
            ),
            requests_handled: 0,
            reactor: None,
            options_observer: None,
            hardware_audio_observer: None,
            active_dac_config: Arc::new(RwLock::new(None)),
            card_catalog: None,
            subject_announcer: None,
            cached_outputs: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Replace the asound.conf path. Used by tests to point at a
    /// tempdir-backed file rather than `/etc/asound.conf`.
    #[cfg(test)]
    pub(crate) fn with_asound_conf_path(mut self, path: PathBuf) -> Self {
        self.asound_conf_path = path;
        self
    }

    /// Replace the asound-options drop-in path. Used by tests to
    /// point at a tempdir-backed file rather than
    /// `/etc/asound.d/evo-options.conf`.
    #[cfg(test)]
    pub(crate) fn with_asound_options_drop_in_path(
        mut self,
        path: PathBuf,
    ) -> Self {
        self.asound_options_drop_in_path = path;
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

    /// Spawn the route-change reactor task. Must be called
    /// after `install_routing` succeeds so the audio_routing
    /// handle is populated; must be called inside a tokio
    /// runtime context. Mirrors composition.alsa's reactor
    /// shape, consuming [`AudioRouting::read_endpoint`]
    /// (delivery is the chain terminus; its endpoint is the
    /// read side of the upstream stage's output).
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
        let initial = fetch_read_endpoint(routing.as_ref());
        let (endpoints_tx, endpoints_rx) = watch::channel(initial);

        let wake = Arc::new(Notify::new());
        let shutdown = Arc::new(Notify::new());
        let refresh_count = Arc::new(std::sync::atomic::AtomicU64::new(0));

        // Register the route-change callback on the routing
        // handle. The callback notifies the wake signal; the
        // reactor picks up on its next select iteration. The
        // callback holds an Arc<Notify> rather than the
        // routing handle itself so callback invocation does
        // not re-enter the trait.
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

    /// Wind down the reactor task and clear the route-change
    /// callback. Idempotent — calling on a plugin without an
    /// active reactor is a no-op.
    async fn stop_reactor(&mut self) {
        if let Some(routing) = self.audio_routing.as_ref() {
            // Drop the framework's reference to the callback
            // before signalling shutdown so the routing
            // handle releases its Arc and the callback
            // closure (and its captured wake notify) can be
            // dropped on schedule.
            routing.on_route_change(None);
        }
        if let Some(handle) = self.reactor.take() {
            handle.shutdown.notify_one();
            // Best-effort wait for the reactor to drain.
            let _ = handle.task.await;
        }
    }

    /// Subscribe to endpoint snapshots produced by the
    /// route-change reactor. Returns `None` when no reactor
    /// is running (plugin not loaded). Future
    /// asound.conf-re-rendering consumers subscribe here;
    /// today the channel is logged-only.
    pub fn subscribe_endpoints(
        &self,
    ) -> Option<watch::Receiver<Option<ReadEndpoint>>> {
        self.reactor.as_ref().map(|r| r.endpoints_rx.clone())
    }

    /// Returns the reactor's refresh counter. Tests poll on
    /// this to observe progress after firing a route change.
    /// Returns 0 when no reactor is running.
    #[cfg(test)]
    fn refresh_count(&self) -> u64 {
        self.reactor
            .as_ref()
            .map(|r| r.refresh_count.load(std::sync::atomic::Ordering::SeqCst))
            .unwrap_or(0)
    }

    /// Subscribe to the playback.options settings subject and
    /// spawn an observer task that emits a
    /// `delivery.options_observed` happening on every update.
    ///
    /// Returns silently when LoadContext doesn't expose the
    /// `subject_state_subscriber` or `subject_querier` (out-
    /// of-process transport before the wire surface lands), or
    /// when the options subject hasn't been announced yet
    /// (admission ordering puts delivery.alsa before
    /// playback.options) — in either case the observer is not
    /// wired this cycle and the operator surface continues to
    /// work without it. A subsequent steward restart re-attempts.
    ///
    /// On every received subject-state update the observer
    /// runs a four-stage pipeline: extract an
    /// [`OptionsSettings`] from the payload; render the
    /// asound.conf drop-in body; atomic-write the drop-in at
    /// [`AlsaDeliveryPlugin::asound_options_drop_in_path`];
    /// emit a `delivery.options_rendered` happening carrying
    /// the rendered byte length so subscribers can observe the
    /// reactive path firing.
    ///
    /// ALSA re-reads `/etc/asound.conf` (and its included drop-
    /// ins) on each PCM open, so the running playback chain
    /// picks up the new pipeline on the next play / pause-
    /// resume cycle.
    async fn spawn_options_observer(&mut self, ctx: &LoadContext) {
        let Some(subscriber) = ctx.subject_state_subscriber.as_ref() else {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                "subject_state_subscriber not populated; skipping \
                 audio-options observer"
            );
            return;
        };
        let Some(querier) = ctx.subject_querier.as_ref() else {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                "subject_querier not populated; skipping audio-options \
                 observer"
            );
            return;
        };
        let happening_emitter = Arc::clone(&ctx.happening_emitter);
        let drop_in_path = self.asound_options_drop_in_path.clone();
        let addressing = ExternalAddressing {
            scheme: "evo.audio.options".to_string(),
            value: "settings".to_string(),
        };
        let canonical_id =
            match querier.resolve_addressing(addressing.clone()).await {
                Ok(Some(id)) => id,
                Ok(None) => {
                    tracing::info!(
                        plugin = PLUGIN_NAME,
                        "audio-options settings subject not yet announced; \
                         observer not wired this cycle"
                    );
                    return;
                }
                Err(e) => {
                    tracing::warn!(
                        plugin = PLUGIN_NAME,
                        error = %e,
                        "resolve_addressing for audio-options settings failed"
                    );
                    return;
                }
            };
        let mut stream =
            match subscriber.subscribe_subject(canonical_id.clone()).await {
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
        let shutdown = Arc::new(Notify::new());
        let task_shutdown = Arc::clone(&shutdown);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = task_shutdown.notified() => {
                        tracing::debug!(
                            plugin = PLUGIN_NAME,
                            "audio-options observer: shutdown received"
                        );
                        return;
                    }
                    update = stream.recv() => {
                        match update {
                            Ok(state_update) => {
                                let observed_at_ms = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_millis() as u64)
                                    .unwrap_or(0);

                                // Extract → render → write. The
                                // observer-observed happening
                                // (kept for diagnostic-replay
                                // consumers) fires alongside the
                                // newer options_rendered happening
                                // (which signals subscribers that
                                // the on-disk pipeline definition
                                // has changed). Both happenings
                                // carry the canonical subject id
                                // so a single subscriber can
                                // correlate observer activity
                                // against pipeline rewrites.
                                let settings_state =
                                    state_update.state.unwrap_or(
                                        serde_json::Value::Null,
                                    );
                                let settings =
                                    options_render::extract_options_settings_from_state(
                                        &settings_state,
                                    );
                                let body = options_render::render_drop_in(
                                    &settings,
                                );
                                let render_bytes = body.len();
                                let write_outcome = options_render::atomic_write_drop_in(
                                    &drop_in_path,
                                    &body,
                                )
                                .await;
                                match &write_outcome {
                                    Ok(()) => {
                                        // LOGGING.md §2: info
                                        // (operator-visible
                                        // lifecycle narrative —
                                        // the pcm.evo pipeline
                                        // definition changed on
                                        // disk).
                                        tracing::info!(
                                            plugin = PLUGIN_NAME,
                                            drop_in_path = %drop_in_path.display(),
                                            render_bytes,
                                            "audio-options drop-in rewritten"
                                        );
                                    }
                                    Err(e) => {
                                        // LOGGING.md §2: warn
                                        // (recoverable; the
                                        // previous drop-in
                                        // remains on disk, the
                                        // operator's prior
                                        // pipeline keeps audio
                                        // flowing).
                                        tracing::warn!(
                                            plugin = PLUGIN_NAME,
                                            drop_in_path = %drop_in_path.display(),
                                            error = %e,
                                            "audio-options drop-in write failed; \
                                             prior pipeline retained"
                                        );
                                    }
                                }

                                let observed_payload = serde_json::json!({
                                    "subject_canonical_id":
                                        state_update.canonical_id,
                                    "observed_at_ms": observed_at_ms,
                                    "settings_state": settings_state,
                                });
                                if let Err(e) = happening_emitter
                                    .emit_plugin_event(
                                        "delivery.options_observed"
                                            .to_string(),
                                        observed_payload,
                                    )
                                    .await
                                {
                                    // LOGGING.md §2: warn
                                    // (recoverable; the disk
                                    // write was the load-bearing
                                    // side effect; this happening
                                    // is the diagnostic surface).
                                    tracing::warn!(
                                        plugin = PLUGIN_NAME,
                                        error = %e,
                                        "emit delivery.options_observed failed"
                                    );
                                }

                                let rendered_payload = serde_json::json!({
                                    "subject_canonical_id":
                                        state_update.canonical_id,
                                    "observed_at_ms": observed_at_ms,
                                    "drop_in_path":
                                        drop_in_path.display().to_string(),
                                    "render_bytes": render_bytes,
                                    "write_ok": write_outcome.is_ok(),
                                });
                                if let Err(e) = happening_emitter
                                    .emit_plugin_event(
                                        "delivery.options_rendered"
                                            .to_string(),
                                        rendered_payload,
                                    )
                                    .await
                                {
                                    // LOGGING.md §2: warn
                                    // (recoverable; diagnostic
                                    // surface).
                                    tracing::warn!(
                                        plugin = PLUGIN_NAME,
                                        error = %e,
                                        "emit delivery.options_rendered failed"
                                    );
                                }

                                // Mixer-transition lifecycle is
                                // owned by the orchestrator
                                // (the audio.options-shape
                                // plugin), per the schema's
                                // `mixer-transition-lifecycle-
                                // emitter-is-orchestrator`
                                // criterion. delivery.alsa
                                // observes the settings subject
                                // and renders the drop-in; the
                                // orchestrator's eight-step
                                // state machine emits the
                                // canonical
                                // `audio.mixer_transition.*`
                                // happenings (started / applied
                                // / rolled_back / failed). The
                                // earlier per-delivery emission
                                // of a `audio.mixer.transition_
                                // applied` happening is retired
                                // — it was a parallel-truth
                                // path competing with the
                                // orchestrator's canonical
                                // lifecycle stream.
                            }
                            Err(SubjectStateStreamError::Lagged {
                                dropped,
                            }) => {
                                tracing::warn!(
                                    plugin = PLUGIN_NAME,
                                    dropped = dropped,
                                    "audio-options observer stream lagged"
                                );
                            }
                            Err(SubjectStateStreamError::Closed) => {
                                tracing::debug!(
                                    plugin = PLUGIN_NAME,
                                    "audio-options observer stream closed"
                                );
                                return;
                            }
                        }
                    }
                }
            }
        });
        self.options_observer = Some(OptionsObserverHandle { task, shutdown });
        tracing::info!(
            plugin = PLUGIN_NAME,
            canonical_id = %canonical_id,
            "audio-options observer task spawned"
        );
    }

    async fn stop_options_observer(&mut self) {
        if let Some(handle) = self.options_observer.take() {
            handle.shutdown.notify_one();
            let _ = handle.task.await;
        }
    }

    /// Subscribe to the hardware.audio-config plugin's
    /// `evo.hardware.audio:active_config` subject and cache the
    /// most-recently-observed snapshot in
    /// [`active_dac_config`]. The observer task tolerates the
    /// load-order race between delivery.alsa and
    /// hardware.audio-config: when the upstream subject has not
    /// yet been announced at admission, the task backs off and
    /// retries `resolve_addressing` until it succeeds (or
    /// shutdown). This keeps the active-DAC enrichment path
    /// live regardless of which plugin admits first.
    async fn spawn_hardware_audio_observer(&mut self, ctx: &LoadContext) {
        let Some(subscriber) = ctx.subject_state_subscriber.as_ref() else {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                "subject_state_subscriber not populated; skipping \
                 hardware-audio observer"
            );
            return;
        };
        let Some(querier) = ctx.subject_querier.as_ref() else {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                "subject_querier not populated; skipping hardware-audio \
                 observer"
            );
            return;
        };
        let cache = Arc::clone(&self.active_dac_config);
        // Capture handles the task closure uses to re-enumerate
        // + republish on every active_config update. The catalog
        // and announcer are populated by
        // install_card_catalog_and_publish_outputs which runs
        // immediately before this; an Option<None> for either
        // means that step failed (catalog parse error or
        // subject_announcer not on the LoadContext) and the
        // republish path simply no-ops.
        let catalog_for_task = self.card_catalog.clone();
        let announcer_for_task = self.subject_announcer.clone();
        let cached_outputs_for_task = Arc::clone(&self.cached_outputs);
        let addressing = ExternalAddressing {
            scheme: "evo.hardware.audio".to_string(),
            value: "active_config".to_string(),
        };
        let querier_for_task = Arc::clone(querier);
        let subscriber_for_task = Arc::clone(subscriber);
        let shutdown = Arc::new(Notify::new());
        let task_shutdown = Arc::clone(&shutdown);
        let task = tokio::spawn(async move {
            // Stage 1: keep trying to resolve the active_config
            // addressing until the upstream subject is announced.
            // Backoff caps at 5 s so the loop adapts to delays
            // without flooding the querier; an immediate
            // shutdown short-circuits the wait.
            let mut backoff = Duration::from_millis(250);
            const BACKOFF_CAP: Duration = Duration::from_secs(5);
            let canonical_id = loop {
                match querier_for_task
                    .resolve_addressing(addressing.clone())
                    .await
                {
                    Ok(Some(id)) => break id,
                    Ok(None) => {
                        tracing::debug!(
                            plugin = PLUGIN_NAME,
                            backoff_ms = backoff.as_millis() as u64,
                            "hardware-audio active_config subject not yet \
                             announced; backing off and retrying"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            plugin = PLUGIN_NAME,
                            error = %e,
                            backoff_ms = backoff.as_millis() as u64,
                            "resolve_addressing for hardware-audio \
                             active_config errored; backing off and retrying"
                        );
                    }
                }
                tokio::select! {
                    _ = task_shutdown.notified() => {
                        tracing::debug!(
                            plugin = PLUGIN_NAME,
                            "hardware-audio observer: shutdown received \
                             during resolve-addressing wait"
                        );
                        return;
                    }
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = std::cmp::min(backoff * 2, BACKOFF_CAP);
            };
            // Stage 2: subscribe. A failure here means the
            // substrate refused the canonical id we just
            // resolved (very unusual); log and exit. A
            // re-spawn on next plugin load picks it back up.
            let mut stream = match subscriber_for_task
                .subscribe_subject(canonical_id.clone())
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        plugin = PLUGIN_NAME,
                        error = %e,
                        canonical_id = %canonical_id,
                        "subscribe to hardware-audio active_config subject \
                         failed; observer not wired this load cycle"
                    );
                    return;
                }
            };
            tracing::info!(
                plugin = PLUGIN_NAME,
                canonical_id = %canonical_id,
                "hardware-audio active_config observer task connected"
            );
            // Stage 3: receive updates + republish enriched
            // outputs on every state push.
            loop {
                tokio::select! {
                    _ = task_shutdown.notified() => {
                        tracing::debug!(
                            plugin = PLUGIN_NAME,
                            "hardware-audio observer: shutdown received"
                        );
                        return;
                    }
                    update = stream.recv() => {
                        match update {
                            Ok(state_update) => {
                                let active = extract_active_dac_config(
                                    state_update.state.as_ref(),
                                );
                                {
                                    let mut guard = cache.write().await;
                                    *guard = Some(active.clone());
                                }
                                tracing::info!(
                                    plugin = PLUGIN_NAME,
                                    overlay = %active.overlay,
                                    catalogue_id = %active
                                        .catalogue_id
                                        .as_deref()
                                        .unwrap_or("<unmatched>"),
                                    alsacard_hint = %active
                                        .alsacard_hint
                                        .as_deref()
                                        .unwrap_or("<none>"),
                                    "hardware-audio active_config cached"
                                );
                                // Re-enumerate + republish outputs
                                // with the freshly-cached active
                                // DAC config so DAC HATs whose
                                // kernel card name is generic
                                // (e.g. "DAC" shared across
                                // several DACs) carry the
                                // operator-friendly label and
                                // mixer control on the outputs
                                // subject.
                                republish_outputs_enriched(
                                    catalog_for_task.as_ref(),
                                    announcer_for_task.as_ref(),
                                    &cached_outputs_for_task,
                                    Some(&active),
                                )
                                .await;
                            }
                            Err(SubjectStateStreamError::Lagged {
                                dropped,
                            }) => {
                                tracing::warn!(
                                    plugin = PLUGIN_NAME,
                                    dropped = dropped,
                                    "hardware-audio observer stream lagged"
                                );
                            }
                            Err(SubjectStateStreamError::Closed) => {
                                tracing::debug!(
                                    plugin = PLUGIN_NAME,
                                    "hardware-audio observer stream closed"
                                );
                                return;
                            }
                        }
                    }
                }
            }
        });
        self.hardware_audio_observer =
            Some(HardwareAudioObserverHandle { task, shutdown });
        tracing::info!(
            plugin = PLUGIN_NAME,
            "hardware-audio active_config observer task spawned; \
             resolve-addressing retry loop running in-task"
        );
    }

    async fn stop_hardware_audio_observer(&mut self) {
        if let Some(handle) = self.hardware_audio_observer.take() {
            handle.shutdown.notify_one();
            let _ = handle.task.await;
        }
    }
}

/// Pull an [`ActiveDacConfig`] out of the published-state JSON the
/// hardware.audio-config plugin emits. The wire shape carries the
/// active config nested under `active`:
///
/// ```json
/// {
///   "v": 1,
///   "active": {
///     "overlay": "hifiberry-dacplus",
///     "catalogue_id": "hifiberry-dacplus",
///     "alsacard_hint": "sndrpihifiberry",
///     "mixer_hint": "Digital",
///     "boot_config_path": "/boot/firmware/config.txt"
///   }
/// }
/// ```
///
/// Missing fields default to the empty / None variant via
/// [`ActiveDacConfig`]'s serde defaults. Returns the default
/// (`overlay = ""`) when `state` is None / not an object.
fn extract_active_dac_config(
    state: Option<&serde_json::Value>,
) -> ActiveDacConfig {
    let Some(state) = state else {
        return ActiveDacConfig::default();
    };
    let active = state.get("active").unwrap_or(state);
    serde_json::from_value(active.clone()).unwrap_or_default()
}

/// Re-enumerate `aplay -l`, enrich rows against the supplied
/// active DAC config, update the cached-outputs snapshot, and
/// publish a fresh `evo.audio.delivery:outputs` state. Called
/// by the hardware-audio observer task on every active_config
/// update so DAC HATs whose kernel card name is generic carry
/// the operator-friendly label and mixer control once the
/// hardware.audio-config plugin has resolved its active DAC.
///
/// Best-effort: when catalog or announcer is None
/// (install_card_catalog_and_publish_outputs failed earlier in
/// load) the call is a no-op. When enumeration fails (aplay -l
/// missing / non-zero exit) the cached outputs are left
/// unchanged and the update is skipped — a stale-but-valid
/// snapshot survives a transient enumeration failure.
async fn republish_outputs_enriched(
    catalog: Option<&Arc<AlsaCardCatalog>>,
    announcer: Option<&Arc<dyn SubjectAnnouncer>>,
    cached_outputs: &Arc<RwLock<Vec<ResolvedAlsaOutput>>>,
    active_dac_config: Option<&ActiveDacConfig>,
) {
    let Some(catalog) = catalog else {
        return;
    };
    let Some(announcer) = announcer else {
        return;
    };
    let outputs =
        match output_enumeration::enumerate_outputs(catalog, active_dac_config)
            .await
        {
            Ok(outs) => outs,
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "outputs re-enumeration on active_config update failed; \
                     leaving cached snapshot in place"
                );
                return;
            }
        };
    *cached_outputs.write().await = outputs.clone();
    let addressing = ExternalAddressing {
        scheme: SUBJECT_SCHEME_DELIVERY.to_string(),
        value: SUBJECT_VALUE_OUTPUTS.to_string(),
    };
    let state =
        serde_json::to_value(&outputs).unwrap_or(serde_json::Value::Null);
    if let Err(e) = announcer.update_state(addressing, state).await {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            error = %e,
            "evo.audio.delivery:outputs update_state failed"
        );
    } else {
        tracing::info!(
            plugin = PLUGIN_NAME,
            output_count = outputs.len(),
            "evo.audio.delivery:outputs republished with enriched active DAC"
        );
    }
}

/// One-shot endpoint fetch over the AudioRouting handle.
/// Returns `Some(endpoint)` when topology is configured,
/// `None` for the benign pre-reconciliation state, and `None`
/// (with a warning log) for any other error.
fn fetch_read_endpoint(routing: &dyn AudioRouting) -> Option<ReadEndpoint> {
    match routing.read_endpoint() {
        Ok(ep) => Some(ep),
        Err(AudioRoutingError::EndpointNotConfigured) => None,
        Err(other) => {
            tracing::warn!(
                error = %other,
                "audio_routing.read_endpoint returned unexpected error; \
                 treating as pre-reconciliation"
            );
            None
        }
    }
}

/// Reactor loop. Awakens on the wake signal (route changes)
/// or the shutdown signal (unload). Each wake triggers a
/// refetch of the routing handle's `read_endpoint`, publishes
/// the new value (or `None`) on the watch channel, and bumps
/// the refresh counter.
///
/// Operator-readable trace: every refetch logs the endpoint
/// kind + path + format at INFO so the journal carries an
/// audit trail of every topology rewire the delivery plugin
/// saw.
async fn run_reactor(
    routing: Arc<dyn AudioRouting>,
    wake: Arc<Notify>,
    shutdown: Arc<Notify>,
    endpoints_tx: watch::Sender<Option<ReadEndpoint>>,
    refresh_count: Arc<std::sync::atomic::AtomicU64>,
) {
    loop {
        tokio::select! {
            _ = wake.notified() => {
                let snapshot = fetch_read_endpoint(routing.as_ref());
                match &snapshot {
                    Some(ep) => tracing::info!(
                        plugin = PLUGIN_NAME,
                        endpoint_kind = ?ep.kind,
                        endpoint_path = %ep.path.display(),
                        endpoint_format = ?ep.format,
                        "topology rewire observed; new ReadEndpoint received"
                    ),
                    None => tracing::info!(
                        plugin = PLUGIN_NAME,
                        "topology rewire observed; endpoint cleared \
                         (pre-reconciliation state)"
                    ),
                }
                if endpoints_tx.send(snapshot).is_err() {
                    // Receiver side dropped — nobody reads
                    // these snapshots anymore. Plugin is on
                    // its way out; exit the reactor.
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
            // Spawn the route-change reactor. The framework's
            // audio_topology_store publishes topology rewires
            // via every plugin's RouteChangeCallback; the
            // delivery plugin's reactor logs every rewire +
            // publishes the snapshot to a watch channel that
            // future asound.conf re-render consumers
            // subscribe to. Same shape composition.alsa +
            // playback.mpd already ship.
            self.spawn_reactor().await?;
            // Spawn the options-subject observer. Subscribes to
            // the playback.options settings subject; on every
            // operator change to mixer_type / output_device /
            // resampling / etc. drives a re-render of the
            // pcm.evo drop-in (via render_drop_in +
            // atomic_write_drop_in) followed by the
            // observer-visibility + drop-in-freshness
            // happenings so cross-plugin consumers can
            // serialise their re-bind work against
            // delivery.alsa's on-disk pipeline definition.
            self.spawn_options_observer(ctx).await;
            // Spawn the hardware.audio-config plugin's
            // active_config-subject observer. Caches the
            // resolved active-DAC snapshot (overlay token,
            // catalogue id, alsacard / mixer hints) so the
            // delivery.active_endpoint verb surfaces a unified
            // "what is bound" answer to the operator UI without
            // re-parsing aplay -L output. Best-effort: if the
            // hardware.audio-config plugin has not yet admitted /
            // announced its subject on this load cycle, the
            // observer is not wired and the cache stays None;
            // the verb continues to respond with the baseline
            // payload.
            // Load the embedded ALSA card catalog + enumerate
            // outputs at admission. The publish is best-effort:
            // a catalog parse error (shipping defect) or an
            // `aplay -l` failure is logged but does not refuse
            // the plugin's load, because the rest of the
            // delivery surface (pcm.evo, options observer, etc.)
            // is independent of the outputs surface.
            //
            // Runs BEFORE the hardware-audio observer spawns so
            // the observer's task closure can capture the
            // installed card_catalog + subject_announcer +
            // cached_outputs handles and trigger an enriched
            // republish on every active_config update.
            self.install_card_catalog_and_publish_outputs(ctx).await;
            self.spawn_hardware_audio_observer(ctx).await;
            tracing::info!(
                plugin = PLUGIN_NAME,
                "plugin loaded; modular ALSA delivery surface ready; \
                 route-change reactor + options-subject observer + \
                 hardware-audio active_config observer running; \
                 outputs subject published"
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
            self.stop_reactor().await;
            self.stop_options_observer().await;
            self.stop_hardware_audio_observer().await;
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
                "delivery.list_outputs" => self.handle_list_outputs(req).await,
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

    /// `delivery.active_endpoint` — surface the current modular
    /// ALSA pipeline state: the asound.conf path, whether it
    /// contains a `pcm.evo` definition, and the WriteEndpoint the
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
        let active_dac_config = self.active_dac_config.read().await.clone();
        let payload = ActiveEndpoint {
            v: PAYLOAD_VERSION,
            asound_conf_path: self.asound_conf_path.display().to_string(),
            pcm_evo_defined,
            framework_endpoint,
            active_dac_config,
        };
        encode(req, &payload)
    }

    /// `delivery.list_outputs` — return the cached resolved-
    /// outputs list captured at load. Hot-plug re-enumeration
    /// lands in a follow-on chunk; today's surface is the
    /// boot-state snapshot.
    async fn handle_list_outputs(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        parse_versioned_payload::<EmptyPayload>(req)?;
        let outputs = self.cached_outputs.read().await.clone();
        let payload = ListOutputsResponse {
            v: PAYLOAD_VERSION,
            outputs,
        };
        encode(req, &payload)
    }

    /// Load the embedded ALSA card catalog, enumerate the host's
    /// ALSA outputs via `aplay -l`, cache the resolved list, and
    /// publish the snapshot on `evo.audio.delivery:outputs`. The
    /// subject_announcer is captured from `ctx` so subsequent
    /// re-publish paths (hot-plug, operator-triggered refresh)
    /// can reuse it once those chunks land.
    async fn install_card_catalog_and_publish_outputs(
        &mut self,
        ctx: &LoadContext,
    ) {
        let catalog = match AlsaCardCatalog::load_embedded() {
            Ok(c) => Arc::new(c),
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "alsa-cards catalog: parse failed; outputs surface \
                     unavailable on this boot"
                );
                return;
            }
        };
        tracing::info!(
            plugin = PLUGIN_NAME,
            card_catalog_rows = catalog.len(),
            "alsa-cards catalog loaded"
        );
        self.card_catalog = Some(Arc::clone(&catalog));
        self.subject_announcer = Some(Arc::clone(&ctx.subject_announcer));

        // Read the cached active DAC config (populated by the
        // hardware-audio observer task on every active_config
        // subject update). At initial load the cache may still
        // be None if the observer has not received its first
        // state push yet; in that case the initial publish ships
        // unenriched rows, and the observer's first update will
        // trigger a re-enumerate + update_state republish below.
        let active_dac = self.active_dac_config.read().await.clone();
        let outputs = match output_enumeration::enumerate_outputs(
            &catalog,
            active_dac.as_ref(),
        )
        .await
        {
            Ok(outs) => outs,
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "alsa output enumeration failed; outputs subject \
                     will publish an empty list this boot"
                );
                Vec::new()
            }
        };
        *self.cached_outputs.write().await = outputs.clone();

        let announcement = SubjectAnnouncement {
            subject_type: SUBJECT_TYPE_OUTPUTS.to_string(),
            addressings: vec![ExternalAddressing {
                scheme: SUBJECT_SCHEME_DELIVERY.to_string(),
                value: SUBJECT_VALUE_OUTPUTS.to_string(),
            }],
            claims: Vec::new(),
            state: serde_json::to_value(&outputs)
                .unwrap_or(serde_json::Value::Null),
            announced_at: std::time::SystemTime::now(),
        };
        if let Err(e) = self
            .subject_announcer
            .as_ref()
            .unwrap()
            .announce(announcement)
            .await
        {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "evo.audio.delivery:outputs announce failed"
            );
        } else {
            tracing::info!(
                plugin = PLUGIN_NAME,
                output_count = outputs.len(),
                "evo.audio.delivery:outputs announced"
            );
        }
    }
}

#[derive(Debug, Serialize)]
struct ListOutputsResponse {
    v: u32,
    outputs: Vec<ResolvedAlsaOutput>,
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
    /// Cached snapshot from the hardware.audio-config plugin's
    /// `evo.hardware.audio:active_config` subject, observed via
    /// the plugin's subject subscription. `None` when the
    /// subject has not yet been seen on this load cycle (e.g.
    /// the hardware.audio-config plugin admitted after
    /// delivery.alsa and the subject was not yet announced when
    /// delivery.alsa tried to subscribe). Operator UI surfaces
    /// the resolved alsacard / mixer hints from this field.
    active_dac_config: Option<ActiveDacConfig>,
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

        fn set_read_endpoint(&self, ep: ReadEndpoint) {
            self.inner.lock().unwrap().read_endpoint = Some(ep);
        }

        fn fire_route_change(&self, event: RouteChange) -> bool {
            let cb = self.inner.lock().unwrap().callback.clone();
            match cb {
                Some(callback) => {
                    callback(&event);
                    true
                }
                None => false,
            }
        }

        fn has_route_change_callback(&self) -> bool {
            self.inner.lock().unwrap().callback.is_some()
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
        let p = AlsaDeliveryPlugin::new();
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

    #[test]
    fn extract_active_dac_config_reads_nested_active_shape() {
        let state = serde_json::json!({
            "v": 1,
            "active": {
                "overlay": "hifiberry-dacplus",
                "catalogue_id": "hifiberry-dacplus",
                "alsacard_hint": "sndrpihifiberry",
                "mixer_hint": "Digital",
                "boot_config_path": "/boot/firmware/config.txt",
            },
        });
        let parsed = extract_active_dac_config(Some(&state));
        assert_eq!(parsed.overlay, "hifiberry-dacplus");
        assert_eq!(parsed.catalogue_id.as_deref(), Some("hifiberry-dacplus"));
        assert_eq!(parsed.alsacard_hint.as_deref(), Some("sndrpihifiberry"));
        assert_eq!(parsed.mixer_hint.as_deref(), Some("Digital"));
        assert_eq!(parsed.boot_config_path, "/boot/firmware/config.txt");
    }

    #[test]
    fn extract_active_dac_config_accepts_flat_shape_for_resilience() {
        let state = serde_json::json!({
            "overlay": "i-sabre-q2m",
            "alsacard_hint": "DAC",
            "mixer_hint": "Digital",
            "boot_config_path": "/boot/config.txt",
        });
        let parsed = extract_active_dac_config(Some(&state));
        assert_eq!(parsed.overlay, "i-sabre-q2m");
        assert_eq!(parsed.alsacard_hint.as_deref(), Some("DAC"));
        assert_eq!(parsed.boot_config_path, "/boot/config.txt");
    }

    #[test]
    fn extract_active_dac_config_returns_default_on_none() {
        let parsed = extract_active_dac_config(None);
        assert_eq!(parsed, ActiveDacConfig::default());
        assert!(parsed.overlay.is_empty());
        assert!(parsed.alsacard_hint.is_none());
    }

    #[test]
    fn extract_active_dac_config_returns_default_on_unset_active() {
        let state = serde_json::json!({
            "v": 1,
            "active": {
                "overlay": "",
                "catalogue_id": null,
                "alsacard_hint": null,
                "mixer_hint": null,
                "boot_config_path": "",
            },
        });
        let parsed = extract_active_dac_config(Some(&state));
        assert!(parsed.overlay.is_empty());
        assert!(parsed.catalogue_id.is_none());
        assert!(parsed.alsacard_hint.is_none());
    }

    #[tokio::test]
    async fn active_endpoint_returns_cached_active_dac_config() {
        let mut p = AlsaDeliveryPlugin::new();
        let routing: Arc<dyn AudioRouting> =
            Arc::new(StubDeliveryAudioRouting::new());
        p.install_routing(Some(routing)).unwrap();
        // Seed the cache as if the hardware-audio observer had fired.
        {
            let mut guard = p.active_dac_config.write().await;
            *guard = Some(ActiveDacConfig {
                overlay: "hifiberry-dacplus".into(),
                catalogue_id: Some("hifiberry-dacplus".into()),
                display_name: Some("HiFiBerry DAC+".into()),
                alsacard_hint: Some("sndrpihifiberry".into()),
                mixer_hint: Some("Digital".into()),
                boot_config_path: "/boot/firmware/config.txt".into(),
            });
        }
        let resp = p
            .handle_request(&request(
                "delivery.active_endpoint",
                json!({ "v": 1 }),
            ))
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(v["active_dac_config"]["overlay"], "hifiberry-dacplus");
        assert_eq!(v["active_dac_config"]["alsacard_hint"], "sndrpihifiberry");
        assert_eq!(v["active_dac_config"]["mixer_hint"], "Digital");
    }

    #[tokio::test]
    async fn active_endpoint_omits_cached_dac_config_when_unobserved() {
        let mut p = AlsaDeliveryPlugin::new();
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
        assert!(v["active_dac_config"].is_null());
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

    // ----- route-change reactor tests -----

    use evo_plugin_sdk::contract::audio_routing::RouteChange;

    /// Wait until the reactor's refresh counter advances from
    /// `prior` to at least `prior + advances`. Bounded so a
    /// wedged reactor does not hang CI.
    async fn wait_for_refresh(
        plugin: &AlsaDeliveryPlugin,
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
                    "reactor refresh counter did not advance from {prior} \
                     to {target} within 500ms"
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }

    fn route_change(new_format: AudioFormat) -> RouteChange {
        RouteChange {
            new_format,
            reason: "test-injected route change".to_string(),
        }
    }

    #[tokio::test]
    async fn spawn_reactor_registers_route_change_callback() {
        let mut p = AlsaDeliveryPlugin::new();
        let stub = Arc::new(StubDeliveryAudioRouting::new());
        assert!(!stub.has_route_change_callback());
        p.install_routing(Some(Arc::clone(&stub) as Arc<dyn AudioRouting>))
            .unwrap();
        p.spawn_reactor().await.unwrap();
        assert!(stub.has_route_change_callback());
        p.stop_reactor().await;
        assert!(!stub.has_route_change_callback());
    }

    #[tokio::test]
    async fn spawn_reactor_publishes_initial_endpoint_when_topology_present() {
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
        let stub = Arc::new(
            StubDeliveryAudioRouting::new()
                .with_read_endpoint(endpoint.clone()),
        );
        p.install_routing(Some(Arc::clone(&stub) as Arc<dyn AudioRouting>))
            .unwrap();
        p.spawn_reactor().await.unwrap();

        let rx = p.subscribe_endpoints().expect("reactor running");
        assert_eq!(rx.borrow().clone(), Some(endpoint));
        p.stop_reactor().await;
    }

    #[tokio::test]
    async fn spawn_reactor_publishes_none_when_topology_absent() {
        let mut p = AlsaDeliveryPlugin::new();
        let stub = Arc::new(StubDeliveryAudioRouting::new());
        p.install_routing(Some(Arc::clone(&stub) as Arc<dyn AudioRouting>))
            .unwrap();
        p.spawn_reactor().await.unwrap();

        let rx = p.subscribe_endpoints().expect("reactor running");
        assert!(
            rx.borrow().is_none(),
            "EndpointNotConfigured must publish None"
        );
        p.stop_reactor().await;
    }

    #[tokio::test]
    async fn route_change_refreshes_endpoint_via_reactor() {
        let mut p = AlsaDeliveryPlugin::new();
        let initial = ReadEndpoint {
            kind: EndpointKind::AlsaPcm,
            path: PathBuf::from("evo"),
            format: AudioFormat::Pcm {
                codec: PcmCodec::PcmS16Le,
                rate_hz: 44_100,
                channels: 2,
            },
            buffer_frames: 1024,
        };
        let stub = Arc::new(
            StubDeliveryAudioRouting::new().with_read_endpoint(initial),
        );
        p.install_routing(Some(Arc::clone(&stub) as Arc<dyn AudioRouting>))
            .unwrap();
        p.spawn_reactor().await.unwrap();

        let mut rx = p.subscribe_endpoints().expect("reactor running");
        let prior_refresh = p.refresh_count();

        // Publish a new topology + fire route_change. The
        // reactor must refetch + republish.
        let new_format = AudioFormat::Pcm {
            codec: PcmCodec::PcmS24Le,
            rate_hz: 192_000,
            channels: 2,
        };
        let new_endpoint = ReadEndpoint {
            kind: EndpointKind::AlsaPcm,
            path: PathBuf::from("evo"),
            format: new_format.clone(),
            buffer_frames: 1024,
        };
        stub.set_read_endpoint(new_endpoint.clone());
        assert!(stub.fire_route_change(route_change(new_format)));

        wait_for_refresh(&p, prior_refresh, 1).await;
        rx.changed().await.expect("watch channel still alive");
        assert_eq!(rx.borrow().clone(), Some(new_endpoint));

        p.stop_reactor().await;
    }

    #[tokio::test]
    async fn unload_terminates_reactor_promptly() {
        let mut p = AlsaDeliveryPlugin::new();
        let stub = Arc::new(StubDeliveryAudioRouting::new());
        p.install_routing(Some(Arc::clone(&stub) as Arc<dyn AudioRouting>))
            .unwrap();
        p.spawn_reactor().await.unwrap();
        assert!(stub.has_route_change_callback());

        let started = std::time::Instant::now();
        p.unload().await.unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(200),
            "unload must drain the reactor quickly; took {elapsed:?}"
        );
        assert!(p.reactor.is_none());
        assert!(!stub.has_route_change_callback());
    }

    // ----- Options→delivery re-render integration -----
    //
    // The observer body's per-update path is `extract →
    // render → atomic_write`. The renderer and the
    // atomic-write helper have their own unit tests in
    // `options_render::tests`; this integration test
    // exercises the three-stage composition end-to-end and
    // asserts the on-disk drop-in matches the renderer's
    // output for the given subject-state payload. The
    // observer task itself is not driven (that would require
    // a full LoadContext + SubjectStateSubscriber stub
    // surface); the composition under test is the same code
    // path the observer body invokes.

    #[test]
    fn with_asound_options_drop_in_path_overrides_default() {
        let p = AlsaDeliveryPlugin::new()
            .with_asound_options_drop_in_path(PathBuf::from("/tmp/test.conf"));
        assert_eq!(
            p.asound_options_drop_in_path,
            PathBuf::from("/tmp/test.conf"),
            "test override must replace the canonical drop-in path"
        );
    }

    #[tokio::test]
    async fn observer_pipeline_extracts_renders_and_atomic_writes_drop_in() {
        let dir = tempdir().expect("tempdir");
        let drop_in_path = dir.path().join("evo-options.conf");

        // Simulate the subject-state payload the
        // playback.options publisher emits for a hardware-
        // mixer choice with a named control.
        let state = json!({
            "mixer_type": "hardware",
            "mixer_control": "Digital",
            "output_device": "hw:CARD=DAC,DEV=0",
        });

        let settings =
            options_render::extract_options_settings_from_state(&state);
        let body = options_render::render_drop_in(&settings);
        options_render::atomic_write_drop_in(&drop_in_path, &body)
            .await
            .expect("atomic_write_drop_in");

        let read_back = tokio::fs::read_to_string(&drop_in_path)
            .await
            .expect("read drop-in back");
        assert_eq!(read_back, body, "on-disk drop-in matches renderer output");
        assert!(read_back.contains("card \"DAC\""));
        assert!(
            read_back.contains("# hardware-mixer control: \"Digital\""),
            "hardware mixer control surfaces as ctl.evo comment hint"
        );
        // Hardware mode without softvol — bit-perfect path.
        assert!(!read_back.contains("type softvol"));
    }

    #[tokio::test]
    async fn observer_pipeline_rewrites_drop_in_on_subsequent_updates() {
        let dir = tempdir().expect("tempdir");
        let drop_in_path = dir.path().join("evo-options.conf");

        // First update: software mixer.
        let first_state = json!({
            "mixer_type": "software",
            "output_device": "hw:CARD=DAC,DEV=0",
        });
        let first_settings =
            options_render::extract_options_settings_from_state(&first_state);
        options_render::atomic_write_drop_in(
            &drop_in_path,
            &options_render::render_drop_in(&first_settings),
        )
        .await
        .expect("first atomic_write");

        let first_contents = tokio::fs::read_to_string(&drop_in_path)
            .await
            .expect("read first");
        assert!(first_contents.contains("type softvol"));

        // Second update: hardware mixer with control. The
        // drop-in must rewrite end-to-end; no residual softvol
        // node from the first render may survive.
        let second_state = json!({
            "mixer_type": "hardware",
            "mixer_control": "Master",
            "output_device": "hw:CARD=DAC,DEV=0",
        });
        let second_settings =
            options_render::extract_options_settings_from_state(&second_state);
        options_render::atomic_write_drop_in(
            &drop_in_path,
            &options_render::render_drop_in(&second_settings),
        )
        .await
        .expect("second atomic_write");

        let second_contents = tokio::fs::read_to_string(&drop_in_path)
            .await
            .expect("read second");
        assert!(!second_contents.contains("type softvol"));
        assert!(
            second_contents.contains("# hardware-mixer control: \"Master\"")
        );
    }

    #[tokio::test]
    async fn observer_pipeline_handles_resampling_node_change() {
        let dir = tempdir().expect("tempdir");
        let drop_in_path = dir.path().join("evo-options.conf");

        // First: 96 kHz target.
        let state_96k = json!({
            "resampling": { "enabled": true, "target_rate_hz": 96000 }
        });
        let settings_96k =
            options_render::extract_options_settings_from_state(&state_96k);
        options_render::atomic_write_drop_in(
            &drop_in_path,
            &options_render::render_drop_in(&settings_96k),
        )
        .await
        .expect("96k write");
        let body_96k = tokio::fs::read_to_string(&drop_in_path).await.unwrap();
        assert!(body_96k.contains("rate 96000"));

        // Second: operator disables resampling.
        let state_off = json!({
            "resampling": { "enabled": false, "target_rate_hz": 96000 }
        });
        let settings_off =
            options_render::extract_options_settings_from_state(&state_off);
        options_render::atomic_write_drop_in(
            &drop_in_path,
            &options_render::render_drop_in(&settings_off),
        )
        .await
        .expect("off write");
        let body_off = tokio::fs::read_to_string(&drop_in_path).await.unwrap();
        assert!(!body_off.contains("rate 96000"));
        assert!(!body_off.contains("pcm.evo_rate"));
    }
}
