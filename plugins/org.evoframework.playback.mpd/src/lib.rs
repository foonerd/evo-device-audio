//! # org-evoframework-playback-mpd
//!
//! MPD playback warden for the evo audio domain. Stocks the
//! `audio.playback` shelf in any distribution catalogue that
//! declares it.
//!
//! The plugin connects to an MPD daemon at the configured endpoint,
//! takes custody of playback, applies course corrections (play,
//! pause, stop, next, previous, seek, set_volume) issued by the
//! steward, and announces `track` and `album` subjects with the
//! `album_of` relation as MPD reports song changes. Operator
//! configuration is provided through [`LoadContext::config`] (the
//! steward delivers the parsed table from
//! `/etc/evo/plugins.d/org.evoframework.playback.mpd.toml`); the
//! plugin applies it during `load()` to override the hardcoded
//! defaults set by [`MpdPlaybackPlugin::new`].
//!
//! The wire-transport binary lands after the in-process flow has
//! stabilised in any consuming distribution.
//!
//! ## Operator configuration
//!
//! The schema, defaults, validation rules, and error hierarchy
//! live in the [`config`] module. In brief:
//!
//! ```toml
//! [endpoint]
//! type = "tcp"           # "tcp" or "unix"
//! host = "127.0.0.1"     # for tcp
//! port = 6600            # for tcp
//! # path = "/run/mpd/socket"   # for unix
//!
//! [timeouts]
//! connect_ms = 5000      # 1..=60000
//! welcome_ms = 2000      # 1..=60000
//! command_ms = 3000      # 1..=300000
//! ```
//!
//! All fields optional. Missing sections or fields use the
//! defaults set by [`MpdPlaybackPlugin::new`]. An empty or absent
//! config file is a valid (default-only) configuration.
//!
//! ## Subject assertion
//!
//! On every song change, the warden announces two subjects and
//! one relation to the steward:
//!
//! - `track` subject, keyed by scheme `mpd-path`, value = MPD's
//!   `file` field (relative library path or stream URL).
//! - `album` subject, keyed by scheme `mpd-album`, value =
//!   `"{artist}|{album}"` where `artist` is the `Artist` tag if
//!   present and non-empty, else `"unknown"`. The pipe separator
//!   disambiguates same-titled albums from different artists.
//! - `album_of` relation from the track subject to the album
//!   subject.
//!
//! Emission is additive and best-effort: subjects and relations
//! accumulate in the steward's registry as they are played;
//! announcer errors are logged but do not disrupt playback. A
//! song whose `Album` tag is missing or empty produces only a
//! track subject (no album, no relation). See the
//! [`playback_supervisor::subject_emitter`] module for details.
//!
//! ## Course-correction payload encoding
//!
//! [`CourseCorrection::correction_type`] names the command;
//! [`CourseCorrection::payload`] carries parameters as UTF-8
//! text. Encoding table:
//!
//! | `correction_type` | payload              | maps to                     |
//! |-------------------|----------------------|-----------------------------|
//! | `play`            | empty                | [`PlaybackCommand::Play`]   |
//! | `play`            | `"3"` (u32)          | `PlayPosition(3)`           |
//! | `pause`           | `"1"` / `"true"`     | `Pause(true)`               |
//! | `pause`           | `"0"` / `"false"`   | `Pause(false)`              |
//! | `stop`            | empty                | `Stop`                      |
//! | `next`            | empty                | `Next`                      |
//! | `previous`        | empty                | `Previous`                  |
//! | `seek`            | `"1250"` (u64 ms)    | `Seek(Duration::from_millis(1250))` |
//! | `set_volume`      | `"50"` (u8)          | `SetVolume(50)`             |
//!
//! Unknown correction types, non-UTF-8 payloads, and unparseable
//! numeric values are rejected with [`PluginError::Permanent`]
//! before the supervisor is contacted.
//!
//! The shape of this crate mirrors the reference warden in
//! `evo-core/crates/evo-example-warden/`; deviations are confined
//! to identity (name, trust class, custody exclusivity).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// The SDK's plugin contract deliberately uses return-position
// `impl Future<Output = _> + Send + '_` rather than `async fn` for
// every trait method (see the module docs on
// `evo_plugin_sdk::contract`). The explicit `Send` bound is
// required for the multi-threaded tokio runtime the steward
// dispatches on; `async fn` in trait position would not produce
// it without unstable `return_type_notation`. Clippy's
// `manual_async_fn` lint would push us toward a form that either
// breaks Send auto-trait inference or diverges from the
// upstream reference warden (`evo-core/crates/evo-example-warden`),
// so the lint is allowed crate-wide. This is a trait-contract
// constraint, not a style preference; it applies uniformly to
// every `impl Plugin` / `impl Warden` method.
#![allow(clippy::manual_async_fn)]

mod config;
mod mpd;
mod mpd_fragment;
mod mpd_restart;
mod playback_supervisor;

#[cfg(test)]
mod test_support_routing;

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use evo_plugin_sdk::contract::audio_routing::{
    AudioRouting, AudioRoutingError, RouteChange, RouteChangeCallback,
    WriteEndpoint,
};
use evo_plugin_sdk::contract::{
    Assignment, BuildInfo, CourseCorrection, CustodyHandle, ExternalAddressing,
    HealthReport, LoadContext, Plugin, PluginDescription, PluginError,
    PluginIdentity, RelationAnnouncer, Request, Respondent, Response,
    RuntimeCapabilities, SubjectAnnouncer, SubjectStateStreamError, Warden,
};
use evo_plugin_sdk::Manifest;
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;

use crate::config::PluginConfig;
use crate::mpd::{ConnectTimeouts, MpdEndpoint};
use crate::mpd_fragment::{
    atomic_write_fragment, render_audio_output_fragment, MixerConfig,
};
use crate::mpd_restart::{
    AutoMpdRestarter, MpdRestarter, SudoSystemctlRestarter,
    INTENT_MPD_FRAGMENT_WRITE, INTENT_MPD_SYSTEMCTL_RESTART,
};
use crate::playback_supervisor::{
    PlaybackCommand, PlaybackError, SubjectEmitter, SupervisorHandle,
};

/// The plugin's embedded manifest, as a static string.
///
/// Available so callers can validate the manifest at test time or
/// admit the plugin without disk I/O.
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");

/// The plugin's canonical reverse-DNS name. Single source of truth
/// shared between the manifest and [`Plugin::describe`]; the
/// `identity_name_matches_manifest` test enforces parity.
pub const PLUGIN_NAME: &str = "org.evoframework.playback.mpd";

/// Default MPD host for a locally-running daemon.
const DEFAULT_MPD_HOST: &str = "127.0.0.1";
/// Default MPD TCP port (matches MPD's upstream default).
const DEFAULT_MPD_PORT: u16 = 6600;

/// Course-correction verbs the warden honours. Kept in
/// lockstep with `manifest.toml`'s
/// `[capabilities.warden].course_correct_verbs` entries
/// and with the `audio.playback` shape v1 schema-of-record;
/// admission would refuse a mismatch between the runtime's
/// declared list and the manifest's. The
/// `manifest_course_correct_verbs_match_runtime` test
/// enforces the lockstep.
const COURSE_CORRECT_VERBS: &[&str] = &[
    "play",
    "pause",
    "stop",
    "next",
    "previous",
    "seek",
    "set_volume",
];

/// Source verbs the plugin handles via the respondent
/// dispatch path. Mirrors
/// `manifest.toml`'s `[capabilities.respondent].request_types`
/// entries; admission would refuse a mismatch between the
/// runtime's declared list and the manifest's. Every verb
/// drives the active custody's supervisor through its
/// existing `PlaybackCommand` surface, so the warden's
/// `course_correct` path and the source-verb dispatch path
/// share the same execution machinery.
const SOURCE_REQUEST_TYPES: &[&str] = &[
    "play_now",
    "play",
    "pause",
    "resume",
    "stop",
    "next",
    "previous",
    "seek",
    "set_volume",
];

/// Wire-protocol payload version every source-verb request
/// and response carries. Independent of plugin SemVer; bumped
/// when the wire shape changes incompatibly.
const PAYLOAD_VERSION: u32 = 1;

/// URI scheme this source plugin owns. Items addressed
/// via `mpd-path:...` URIs are loaded into the local MPD
/// daemon's library and played; items in other schemes
/// dispatch elsewhere by the framework's URI-routing rules.
const URI_SCHEME_MPD_PATH: &str = "mpd-path";

/// Parse the embedded manifest into a [`Manifest`] struct.
///
/// Panics if the embedded manifest fails to parse. Such a failure
/// is a build-time bug, not a runtime condition, so panicking is
/// acceptable.
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("org-evoframework-playback-mpd's embedded manifest must parse")
}

/// Semver of this plugin crate, from the workspace/Cargo `version`
/// field. [`Plugin::describe`]'s [`PluginIdentity::version`],
/// [`BuildInfo::plugin_build`], and `manifest.toml` `[plugin].version`
/// must stay aligned (release tooling and tests assert this).
fn plugin_crate_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is valid semver")
}

/// Per-custody state retained for the lifetime of a custody.
///
/// Holds the [`SupervisorHandle`] returned by
/// [`playback_supervisor::spawn`] so [`Warden::course_correct`]
/// can dispatch commands and [`Warden::release_custody`] can shut
/// the supervisor down cleanly. `custody_type` is retained for
/// log breadcrumbs.
struct TrackedCustody {
    custody_type: String,
    supervisor: SupervisorHandle,
}

/// MPD playback warden plugin.
///
/// Construct via [`MpdPlaybackPlugin::new`] (default endpoint
/// `127.0.0.1:6600`, default timeouts, no subject emitter).
/// [`Plugin::load`] replaces the defaults with values from
/// [`LoadContext::config`] if the operator has supplied a config
/// file, and populates the [`SubjectEmitter`] from the load
/// context's announcer handles. Tests may also use
/// [`MpdPlaybackPlugin::with_endpoint`] to construct a plugin
/// pointing at a specific endpoint without going through the
/// `load` path; such tests set [`Self::subject_emitter`]
/// directly (typically to [`SubjectEmitter::null`]) before
/// exercising custody verbs.
pub struct MpdPlaybackPlugin {
    loaded: bool,
    endpoint: MpdEndpoint,
    timeouts: ConnectTimeouts,
    /// Bundle of subject and relation announcer handles used by
    /// [`Warden::take_custody`] to equip each spawned supervisor.
    /// `None` until [`Plugin::load`] populates from
    /// [`LoadContext`]; `take_custody` refuses to proceed when
    /// absent.
    subject_emitter: Option<SubjectEmitter>,
    /// Audio data plane routing handle pulled from
    /// [`LoadContext::audio_routing`] at load time. `None`
    /// before the first successful load and after every
    /// `unload`. The plugin uses the handle in chunk F3
    /// onwards to learn which ALSA pcm MPD's audio_output
    /// should write to (the framework's negotiated
    /// `WriteEndpoint`) and to react to topology rewires.
    /// Composition plugins that declare
    /// `[capabilities.composition]` and source plugins
    /// (this one) that declare `[capabilities.source]` with
    /// an audio `output_kind` MUST receive this handle;
    /// `Plugin::load` refuses loudly when it is `None`.
    audio_routing: Option<Arc<dyn AudioRouting>>,
    custodies: HashMap<String, TrackedCustody>,
    /// Cumulative count of custodies accepted since construction.
    /// Does not decrement on release.
    custodies_taken: u64,
    /// Cumulative count of course corrections dispatched to the
    /// supervisor since construction. Counts attempts, not
    /// successes: a dispatched command that the supervisor then
    /// fails still increments this counter.
    corrections_dispatched: u64,
    /// Cumulative count of source-verb requests handled.
    /// Mirrors `corrections_dispatched` on the respondent
    /// dispatch side.
    requests_handled: u64,
    /// Path the route-change reactor's fragment writer renders
    /// MPD's `audio_output` block to. Populated from the
    /// operator's config (or the hardcoded default
    /// `/etc/evo/mpd.conf`) at construction and refreshed at
    /// every `Plugin::load`. The dynamic shape supersedes the
    /// static fragment at `dist/mpd/evo-fragment.conf`.
    fragment_path: PathBuf,
    /// Restart strategy invoked after every fragment rewrite
    /// so MPD picks the new audio_output up. Production uses
    /// [`SudoSystemctlRestarter`]; tests inject a counting or
    /// failing stub via [`MpdPlaybackPlugin::with_restarter`].
    restarter: Arc<dyn MpdRestarter>,
    /// Route-change reactor task handle. `Some` after a
    /// successful `Plugin::load`; `None` before first load and
    /// after `Plugin::unload`.
    reactor: Option<ReactorHandle>,
    /// Fragment-writer worker task handle. `Some` after a
    /// successful `Plugin::load`; `None` before first load and
    /// after `Plugin::unload`. The worker subscribes to the
    /// reactor's snapshot channel, renders + atomic-writes the
    /// MPD audio_output fragment, and asks the restarter to
    /// recycle MPD on every snapshot.
    fragment_worker: Option<FragmentWorkerHandle>,
    /// Watch channel carrying the operator's currently-selected
    /// mixer configuration. Seeded with `MixerConfig::Software`
    /// at construction (the framework's bit-perfect-compatible
    /// default); updated by the `playback.options` settings
    /// subscriber when the operator changes mixer_type via the
    /// options plugin. The fragment-worker selects on both this
    /// channel AND the endpoint reactor's snapshot channel,
    /// re-rendering the mpd_fragment on either change.
    mixer_config_tx: watch::Sender<MixerConfig>,
    /// Concrete handle on the auto-restarter composite so a
    /// future capabilities-watch reactor can call `re_resolve`
    /// on it without going through the `Arc<dyn MpdRestarter>`
    /// erasure. Same underlying Arc as [`Self::restarter`] in
    /// production; `None` when tests inject a different
    /// restarter via [`MpdPlaybackPlugin::with_restarter`].
    auto_restarter: Option<Arc<AutoMpdRestarter>>,
    /// PPAG capabilities-watch reactor handle. `Some` when the
    /// framework's re-probe task is publishing live resolution
    /// updates to `LoadContext::capabilities_watch`; `None` on
    /// admission paths that did not wire the watch (test
    /// fixtures, OOP transports). Held here so
    /// `Plugin::unload` can stop it cleanly.
    capabilities_watcher: Option<CapabilitiesWatcherHandle>,
}

/// Handle on the PPAG capabilities-watch reactor task. Spawned
/// once at load when `LoadContext::capabilities_watch` is `Some`;
/// observes the framework's re-probe publications and re-resolves
/// the auto-restarter's inner strategy on every change.
struct CapabilitiesWatcherHandle {
    task: JoinHandle<()>,
    shutdown: Arc<Notify>,
    /// Re-resolve counter — bumped after every observed map
    /// change. Kept on the handle so the counter's Arc clone
    /// in the task body retains a live observer for future
    /// reactor-progress tests.
    #[allow(dead_code)]
    refresh_count: Arc<std::sync::atomic::AtomicU64>,
}

/// Fragment-writer worker status published to the worker's
/// watch channel for observability surfaces and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FragmentWorkerStatus {
    /// No topology — no fragment has been rendered yet.
    Idle,
    /// Worker rendered the supplied [`WriteEndpoint`] and
    /// restarted MPD successfully.
    Restarted {
        /// The endpoint the active fragment file describes.
        endpoint: WriteEndpoint,
    },
    /// Render / write / restart leg failed. Worker keeps
    /// running and reattempts on the next route change. The
    /// previous fragment file (if any) is unaffected.
    Failed {
        /// Operator-readable failure reason — render error
        /// message, IO error description, or restarter error
        /// string verbatim.
        reason: String,
    },
}

/// Handle on the route-change reactor task spawned at load.
struct ReactorHandle {
    task: JoinHandle<()>,
    shutdown: Arc<Notify>,
    endpoints_rx: watch::Receiver<Option<WriteEndpoint>>,
    /// Reactor refresh counter — bumped after every endpoint
    /// fetch. Tests poll on this to observe reactor progress
    /// without racy sleeps.
    #[cfg_attr(not(test), allow(dead_code))]
    refresh_count: Arc<std::sync::atomic::AtomicU64>,
}

/// Handle on the fragment-writer worker task.
struct FragmentWorkerHandle {
    task: JoinHandle<()>,
    shutdown: Arc<Notify>,
    status_rx: watch::Receiver<FragmentWorkerStatus>,
}

impl MpdPlaybackPlugin {
    /// Construct a plugin pointing at the default local MPD
    /// endpoint (`127.0.0.1:6600`) with default connect / welcome
    /// / command timeouts. [`Plugin::load`] overrides these from
    /// the operator's on-disk config file if one exists.
    pub fn new() -> Self {
        let endpoint = MpdEndpoint::tcp(DEFAULT_MPD_HOST, DEFAULT_MPD_PORT)
            .expect("default MPD endpoint (127.0.0.1:6600) must be valid");
        Self::with_endpoint(endpoint, ConnectTimeouts::default())
    }

    /// Construct a plugin with an explicit endpoint and timeout
    /// budget. Used by tests (pointing at a mock MPD on an
    /// ephemeral loopback port) and, where needed, by crate-
    /// internal code that bypasses the config-file path.
    pub(crate) fn with_endpoint(
        endpoint: MpdEndpoint,
        timeouts: ConnectTimeouts,
    ) -> Self {
        let (mixer_config_tx, _) = watch::channel(MixerConfig::Software);
        Self {
            loaded: false,
            endpoint,
            timeouts,
            subject_emitter: None,
            audio_routing: None,
            custodies: HashMap::new(),
            custodies_taken: 0,
            corrections_dispatched: 0,
            requests_handled: 0,
            fragment_path: PathBuf::from(config::DEFAULT_FRAGMENT_PATH),
            restarter: Arc::new(SudoSystemctlRestarter::new()),
            reactor: None,
            fragment_worker: None,
            mixer_config_tx,
            auto_restarter: None,
            capabilities_watcher: None,
        }
    }

    /// Replace the MPD restart strategy. Used by tests to
    /// substitute a deterministic stub for the production
    /// `sudo systemctl restart mpd` invocation. Production
    /// builds use the default [`SudoSystemctlRestarter`]
    /// installed by [`MpdPlaybackPlugin::new`].
    #[cfg(test)]
    pub(crate) fn with_restarter(
        mut self,
        restarter: Arc<dyn MpdRestarter>,
    ) -> Self {
        self.restarter = restarter;
        self
    }

    /// Replace the fragment-output path. Used by tests so
    /// the fragment-writer worker writes into a tempdir
    /// rather than `/etc/evo/mpd.conf`.
    #[cfg(test)]
    pub(crate) fn with_fragment_path(mut self, path: PathBuf) -> Self {
        self.fragment_path = path;
        self
    }

    /// Subscribe to the fragment-writer worker's status
    /// channel. Returns `None` when no worker is running.
    pub fn subscribe_fragment_status(
        &self,
    ) -> Option<watch::Receiver<FragmentWorkerStatus>> {
        self.fragment_worker.as_ref().map(|w| w.status_rx.clone())
    }

    /// Subscribe to endpoint snapshots from the route-change
    /// reactor. Returns `None` when the plugin is not loaded
    /// (no reactor is running).
    pub fn subscribe_endpoints(
        &self,
    ) -> Option<watch::Receiver<Option<WriteEndpoint>>> {
        self.reactor.as_ref().map(|r| r.endpoints_rx.clone())
    }

    /// Cumulative count of source-verb requests handled
    /// since construction.
    pub fn requests_handled(&self) -> u64 {
        self.requests_handled
    }

    /// Load contract isolated to its testable inputs: the
    /// audio routing handle. The public [`Plugin::load`]
    /// entry pulls the handle off the context and forwards
    /// here; the split lets unit tests exercise the
    /// refuse-when-`None` contract without needing to
    /// construct a full [`LoadContext`] (which carries
    /// many mandatory trait-object fields).
    fn install_routing(
        &mut self,
        routing: Option<Arc<dyn AudioRouting>>,
    ) -> Result<(), PluginError> {
        let routing = routing.ok_or_else(|| {
            PluginError::Permanent(
                "playback.mpd plugin requires LoadContext::audio_routing; \
                 received None — manifest declares [capabilities.source] \
                 with an audio output_kind, so the framework MUST provision \
                 an audio_routing handle. Indicates a manifest / trust / \
                 admission misconfiguration."
                    .to_string(),
            )
        })?;
        self.audio_routing = Some(routing);
        Ok(())
    }

    /// Number of custodies currently held (taken but not yet
    /// released).
    pub fn active_custody_count(&self) -> usize {
        self.custodies.len()
    }

    /// Cumulative count of custodies accepted since construction.
    pub fn custodies_taken(&self) -> u64 {
        self.custodies_taken
    }

    /// Cumulative count of course corrections dispatched to a
    /// supervisor since construction.
    pub fn corrections_dispatched(&self) -> u64 {
        self.corrections_dispatched
    }

    /// Parse an operator config table into a [`PluginConfig`] and
    /// apply it to `self`, replacing the fields set by
    /// [`MpdPlaybackPlugin::new`].
    ///
    /// Shared between the [`Plugin::load`] path (which gets the
    /// table from [`LoadContext::config`]) and tests (which
    /// construct the table directly). Does not change the
    /// `loaded` flag; that is the caller's responsibility.
    fn apply_config_table(
        &mut self,
        table: &toml::Table,
    ) -> Result<(), PluginError> {
        let config = PluginConfig::from_toml_table(table).map_err(|e| {
            PluginError::Permanent(format!("invalid plugin config: {e}"))
        })?;
        self.endpoint = config.endpoint;
        self.timeouts = config.timeouts;
        self.fragment_path = config.fragment_path;
        // Seed the mixer-config watch channel from the operator's
        // configuration. Default (no config entry) is `Software`
        // to match the legacy hard-coded behaviour. The framework's
        // `playback.options` policy plugin owns the operator-
        // facing surface; dynamic propagation rides the subject-
        // state subscription wire-up (R-021 substrate already
        // lit). Operators picking Hardware or None today set
        // [mixer_type] in /etc/evo/plugins.d/playback.mpd.toml
        // and bounce the steward.
        let mixer_cfg = mixer_config_from_toml(table)?;
        let _ = self.mixer_config_tx.send(mixer_cfg);
        Ok(())
    }

    /// Spawn the route-change reactor task. Must be called
    /// after `install_routing` succeeds so `audio_routing` is
    /// populated; must be called inside a tokio runtime
    /// context. Mirrors composition.alsa's reactor shape but
    /// consumes [`AudioRouting::write_endpoint`] in place of
    /// `composition_endpoints` because playback.mpd is a
    /// source-plugin endpoint consumer.
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

        let initial = fetch_write_endpoint(routing.as_ref());
        let (endpoints_tx, endpoints_rx) = watch::channel(initial);

        let wake = Arc::new(Notify::new());
        let shutdown = Arc::new(Notify::new());
        let refresh_count = Arc::new(std::sync::atomic::AtomicU64::new(0));

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

    /// Spawn the fragment-writer worker task. Must be called
    /// after `spawn_reactor` succeeds — the worker subscribes
    /// to the reactor's endpoint snapshot channel.
    async fn spawn_fragment_worker(&mut self) -> Result<(), PluginError> {
        debug_assert!(
            self.reactor.is_some(),
            "spawn_fragment_worker called before spawn_reactor"
        );
        debug_assert!(
            self.fragment_worker.is_none(),
            "spawn_fragment_worker called while a worker is already running"
        );

        let endpoints_rx = self
            .reactor
            .as_ref()
            .expect("reactor populated")
            .endpoints_rx
            .clone();
        let mixer_rx = self.mixer_config_tx.subscribe();
        let (status_tx, status_rx) = watch::channel(FragmentWorkerStatus::Idle);
        let shutdown = Arc::new(Notify::new());
        let task_shutdown = Arc::clone(&shutdown);
        let task_fragment_path = self.fragment_path.clone();
        let task_restarter = Arc::clone(&self.restarter);
        let task = tokio::spawn(async move {
            run_fragment_worker(
                endpoints_rx,
                mixer_rx,
                task_shutdown,
                status_tx,
                task_fragment_path,
                task_restarter,
            )
            .await;
        });

        self.fragment_worker = Some(FragmentWorkerHandle {
            task,
            shutdown,
            status_rx,
        });
        Ok(())
    }

    /// Wind down the fragment-writer worker. Idempotent.
    async fn stop_fragment_worker(&mut self) {
        if let Some(handle) = self.fragment_worker.take() {
            handle.shutdown.notify_one();
            let _ = handle.task.await;
        }
    }

    /// Subscribe to the `audio.options.settings` subject the
    /// `playback.options` plugin announces; pipe operator
    /// mixer-mode changes into `mixer_config_tx` so the
    /// fragment-writer worker re-renders mpd.conf on every
    /// change.
    ///
    /// Reads the initial settings via the subject querier so
    /// the worker has the operator's choice on first render,
    /// then loops on the state stream for subsequent changes.
    /// Hardware-mode degrade: a `MixerType::Hardware` choice
    /// with an empty `mixer_device` or `mixer_control` is
    /// translated to `MixerConfig::Software` plus an operator-
    /// visible WARN-level log; the framework's
    /// happening-emitter / observability layer surfaces the
    /// downgrade through the audit chain.
    ///
    /// Best-effort: if the subscriber handle is absent (OOP
    /// transport before the wire surface lands) or the
    /// addressing does not resolve yet (admission ordering
    /// race where playback.mpd loads before playback.options),
    /// the function logs and returns. The plugin's own
    /// config-table fallback continues to honour `mixer_type`
    /// from `/etc/evo/plugins.d/playback.mpd.toml`.
    async fn spawn_options_settings_subscriber(&self, ctx: &LoadContext) {
        let Some(subscriber) = ctx.subject_state_subscriber.as_ref() else {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                "subject_state_subscriber not populated (OOP transport \
                 pre-wire-surface); skipping audio-options subscription"
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

        let addressing = ExternalAddressing {
            scheme: "evo.audio.options".to_string(),
            value: "settings".to_string(),
        };
        let canonical_id =
            match querier.resolve_addressing(addressing.clone()).await {
                Ok(Some(id)) => id,
                Ok(None) => {
                    // playback.options has not announced yet —
                    // expected on admission orderings where
                    // playback.mpd admits first. The subject-
                    // state subscriber accepts subscriptions for
                    // future canonical ids, but resolve_addressing
                    // returns None until announce lands. For v1
                    // close-out we skip; operator mixer-mode
                    // changes via wire op after both plugins are
                    // up rely on the next steward restart picking
                    // up the subscription. The dynamic-update
                    // surface for THIS startup cycle stays on the
                    // plugin's own config-table fallback.
                    tracing::info!(
                        plugin = PLUGIN_NAME,
                        "audio-options settings subject not yet announced; \
                     subscriber not wired this cycle (plugin's config \
                     table remains the operator surface)"
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

        // Subscribe FIRST so we cannot miss a state change
        // that lands between current_state and subscribe; then
        // read current_state to seed the initial mixer config.
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

        let mixer_tx = self.mixer_config_tx.clone();
        // Seed mixer_config_tx from the initial subject state
        // (if any) BEFORE spawning the loop. This guarantees
        // the fragment-writer renders the operator's choice on
        // its first cycle.
        if let Some(state) = initial_state {
            if let Some(cfg) = parse_mixer_config_from_settings_state(&state) {
                let _ = mixer_tx.send(cfg);
            }
        }

        tokio::spawn(async move {
            loop {
                match stream.recv().await {
                    Ok(update) => {
                        if let Some(state) = update.state.as_ref() {
                            if let Some(cfg) =
                                parse_mixer_config_from_settings_state(state)
                            {
                                let _ = mixer_tx.send(cfg);
                            }
                        }
                    }
                    Err(SubjectStateStreamError::Lagged { dropped }) => {
                        tracing::warn!(
                            plugin = PLUGIN_NAME,
                            dropped = dropped,
                            "audio-options subject stream lagged; \
                             continuing at the live frame"
                        );
                        // Stream auto-rejoins on next recv;
                        // missed updates surface via the next
                        // state change.
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

    /// Spawn the PPAG capabilities-watch reactor.
    fn spawn_capabilities_watcher(
        &mut self,
        auto: Arc<AutoMpdRestarter>,
        mut rx: tokio::sync::watch::Receiver<
            Arc<evo_plugin_sdk::privileges::CapabilityResolutionMap>,
        >,
    ) {
        let shutdown = Arc::new(Notify::new());
        let task_shutdown = Arc::clone(&shutdown);
        let refresh_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let task_refresh = Arc::clone(&refresh_count);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    changed = rx.changed() => {
                        if changed.is_err() {
                            tracing::debug!(
                                plugin = PLUGIN_NAME,
                                "capabilities-watch sender dropped; \
                                 reactor exiting"
                            );
                            break;
                        }
                        let new_map = rx.borrow_and_update().clone();
                        auto.re_resolve(&new_map);
                        task_refresh.fetch_add(
                            1,
                            std::sync::atomic::Ordering::Relaxed,
                        );
                        tracing::info!(
                            plugin = PLUGIN_NAME,
                            strategy = auto.current_strategy_name(),
                            rationale = %auto.rationale(),
                            "MPD restart strategy re-resolved from \
                             PPAG update"
                        );
                    }
                    _ = task_shutdown.notified() => {
                        tracing::debug!(
                            plugin = PLUGIN_NAME,
                            "capabilities-watch reactor received \
                             shutdown signal; exiting"
                        );
                        break;
                    }
                }
            }
        });
        self.capabilities_watcher = Some(CapabilitiesWatcherHandle {
            task,
            shutdown,
            refresh_count,
        });
    }

    /// Wind down the capabilities-watch reactor. Idempotent.
    async fn stop_capabilities_watcher(&mut self) {
        if let Some(handle) = self.capabilities_watcher.take() {
            handle.shutdown.notify_one();
            let _ = handle.task.await;
        }
        self.auto_restarter = None;
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
            let _ = handle.task.await;
        }
    }

    /// Returns the reactor's refresh counter. Tests poll on
    /// this to observe the reactor making progress after
    /// firing a route change. Returns 0 when no reactor is
    /// running.
    #[cfg(test)]
    fn refresh_count(&self) -> u64 {
        self.reactor
            .as_ref()
            .map(|r| r.refresh_count.load(std::sync::atomic::Ordering::SeqCst))
            .unwrap_or(0)
    }
}

impl Default for MpdPlaybackPlugin {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract a [`MixerConfig`] from the `audio.options.settings`
/// subject state payload. Returns `None` when the payload
/// has no mixer block or the block is malformed.
///
/// Hardware-mode degrade: if `mixer_type = "Hardware"` but the
/// payload's `output_device` does not include both an ALSA
/// device path AND a non-empty mixer-control name, the
/// function returns `MixerConfig::Software` with a WARN log.
/// This matches the Volumio Rust port's safety net at
/// `volumio-evo/crates/core/src/playback_options.rs:184-187`
/// and 196-199 — operators should not lose audio output to a
/// misconfigured hardware-mixer choice.
fn parse_mixer_config_from_settings_state(
    state: &serde_json::Value,
) -> Option<MixerConfig> {
    // The playback.options Settings struct serialises as a TOML
    // table; serde_json::to_value picks the same field names.
    // Mixer config in v1 lives under the top-level fields
    // `mixer_type` / `mixer_device` / `mixer_control` (the
    // latter two are absent in the v1 playback.options schema
    // but the parser is forward-compatible: if they appear,
    // they wire Hardware mode; if not, Hardware degrades).
    let mixer_type = state
        .get("mixer_type")
        .and_then(|v| v.as_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| "software".to_string());
    let mixer_device = state
        .get("mixer_device")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let mixer_control = state
        .get("mixer_control")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    match mixer_type.as_str() {
        "software" => Some(MixerConfig::Software),
        "none" => Some(MixerConfig::None),
        "hardware" => match (mixer_device, mixer_control) {
            (Some(dev), Some(ctrl)) if !dev.is_empty() && !ctrl.is_empty() => {
                Some(MixerConfig::Hardware {
                    mixer_device: dev,
                    mixer_control: ctrl,
                })
            }
            _ => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    "operator selected mixer_type = Hardware without a \
                     mixer_device + mixer_control; degrading to Software \
                     to keep audio output (matches Volumio Rust port's \
                     safety net)"
                );
                Some(MixerConfig::Software)
            }
        },
        other => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                mixer_type = other,
                "operator settings carry an unknown mixer_type; \
                 falling back to Software"
            );
            Some(MixerConfig::Software)
        }
    }
}

/// Parse the operator's mixer-mode selection out of the
/// plugin config TOML table. Three flat keys are read at the
/// top of the table:
///
/// - `mixer_type` ∈ `{ "hardware", "software", "none" }`
///   (default: `"software"` matching legacy behaviour).
/// - `mixer_device` — required when `mixer_type = "hardware"`;
///   passed verbatim to MPD as the `mixer_device` line. Typical
///   shape `"hw:<card>"` matching the card name in
///   `/etc/asound.conf`.
/// - `mixer_control` — required when `mixer_type = "hardware"`;
///   passed verbatim as the `mixer_control` line. Typical
///   values `"Master"`, `"PCM"`, or DAC-specific control names
///   visible via `amixer scontrols`.
///
/// Refuses Hardware mode without `mixer_device` + `mixer_control`
/// rather than silently degrading: the operator picked Hardware
/// for a reason; a missing knob is a config error to surface.
fn mixer_config_from_toml(
    table: &toml::Table,
) -> Result<MixerConfig, PluginError> {
    let raw = match table.get("mixer_type").and_then(|v| v.as_str()) {
        Some(s) => s.to_ascii_lowercase(),
        None => return Ok(MixerConfig::Software),
    };
    match raw.as_str() {
        "software" => Ok(MixerConfig::Software),
        "none" => Ok(MixerConfig::None),
        "hardware" => {
            let mixer_device = table
                .get("mixer_device")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    PluginError::Permanent(
                        "mixer_type = \"hardware\" requires `mixer_device` \
                         in plugin config (e.g. mixer_device = \"hw:0\")"
                            .into(),
                    )
                })?
                .to_string();
            let mixer_control = table
                .get("mixer_control")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    PluginError::Permanent(
                        "mixer_type = \"hardware\" requires `mixer_control` \
                         in plugin config (e.g. mixer_control = \"Master\")"
                            .into(),
                    )
                })?
                .to_string();
            Ok(MixerConfig::Hardware {
                mixer_device,
                mixer_control,
            })
        }
        other => Err(PluginError::Permanent(format!(
            "mixer_type must be one of {{hardware, software, none}}; got \
             {other:?}"
        ))),
    }
}

/// One-shot endpoint fetch over the AudioRouting handle.
/// Returns `Some(endpoint)` when topology is configured,
/// `None` for the benign pre-reconciliation state, and `None`
/// (with a warning log) for any other error — the reactor
/// treats unexpected errors as transient and re-polls on the
/// next wake.
fn fetch_write_endpoint(routing: &dyn AudioRouting) -> Option<WriteEndpoint> {
    match routing.write_endpoint() {
        Ok(ep) => Some(ep),
        Err(AudioRoutingError::EndpointNotConfigured) => None,
        Err(other) => {
            tracing::warn!(
                error = %other,
                "audio_routing.write_endpoint returned unexpected error; \
                 treating as pre-reconciliation"
            );
            None
        }
    }
}

/// Reactor loop. Awakens on the wake signal (route changes)
/// or the shutdown signal (unload). Each wake triggers a
/// refetch of the routing handle's `write_endpoint`,
/// publishes the new value (or `None` for pre-reconciliation
/// state) on the watch channel, and bumps the refresh counter
/// so tests can observe progress.
async fn run_reactor(
    routing: Arc<dyn AudioRouting>,
    wake: Arc<Notify>,
    shutdown: Arc<Notify>,
    endpoints_tx: watch::Sender<Option<WriteEndpoint>>,
    refresh_count: Arc<std::sync::atomic::AtomicU64>,
) {
    loop {
        tokio::select! {
            _ = wake.notified() => {
                let snapshot = fetch_write_endpoint(routing.as_ref());
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

/// Fragment-writer worker loop. Subscribes to the reactor's
/// endpoint snapshot channel; on each new snapshot, renders
/// the MPD `audio_output` block, atomic-writes it to the
/// configured fragment path, and asks the restarter to
/// recycle MPD. Worker status (Idle / Restarted / Failed) is
/// published to the watch channel for observability.
async fn run_fragment_worker(
    mut endpoints_rx: watch::Receiver<Option<WriteEndpoint>>,
    mut mixer_rx: watch::Receiver<MixerConfig>,
    shutdown: Arc<Notify>,
    status_tx: watch::Sender<FragmentWorkerStatus>,
    fragment_path: PathBuf,
    restarter: Arc<dyn MpdRestarter>,
) {
    loop {
        let endpoint_snapshot = endpoints_rx.borrow_and_update().clone();
        let mixer_snapshot = mixer_rx.borrow_and_update().clone();
        match endpoint_snapshot {
            None => {
                let _ = status_tx.send(FragmentWorkerStatus::Idle);
            }
            Some(endpoint) => {
                let status = apply_fragment_cycle(
                    &endpoint,
                    &mixer_snapshot,
                    &fragment_path,
                    restarter.as_ref(),
                )
                .await;
                let _ = status_tx.send(status);
            }
        }

        tokio::select! {
            biased;
            _ = shutdown.notified() => return,
            res = endpoints_rx.changed() => {
                if res.is_err() {
                    return;
                }
            }
            res = mixer_rx.changed() => {
                if res.is_err() {
                    return;
                }
            }
        }
    }
}

/// One render + write + restart cycle. Returns the worker
/// status the caller should publish.
async fn apply_fragment_cycle(
    endpoint: &WriteEndpoint,
    mixer: &MixerConfig,
    fragment_path: &std::path::Path,
    restarter: &dyn MpdRestarter,
) -> FragmentWorkerStatus {
    let rendered = match render_audio_output_fragment(endpoint, mixer) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                ?endpoint,
                "fragment render failed; keeping previous fragment file"
            );
            return FragmentWorkerStatus::Failed {
                reason: format!("render: {e}"),
            };
        }
    };

    if let Err(e) = atomic_write_fragment(fragment_path, &rendered).await {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            error = %e,
            path = %fragment_path.display(),
            "fragment atomic-write failed; keeping previous fragment file"
        );
        return FragmentWorkerStatus::Failed {
            reason: format!("write: {e}"),
        };
    }

    if let Err(reason) = restarter.restart().await {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            reason = %reason,
            "MPD restart failed after fragment rewrite; new fragment is on \
             disk but MPD has not picked it up yet"
        );
        return FragmentWorkerStatus::Failed { reason };
    }

    tracing::info!(
        plugin = PLUGIN_NAME,
        path = %fragment_path.display(),
        device = %endpoint.path.display(),
        "MPD audio_output fragment rewritten and MPD restarted"
    );
    FragmentWorkerStatus::Restarted {
        endpoint: endpoint.clone(),
    }
}

/// Parse a [`CourseCorrection`] into the concrete
/// [`PlaybackCommand`] the supervisor understands.
///
/// See the module-level documentation for the encoding table.
/// Errors classify at the warden boundary: every rejection from
/// this function maps to [`PluginError::Permanent`] because the
/// correction is malformed and the same bytes will fail the same
/// way on retry.
fn parse_correction(
    correction: &CourseCorrection,
) -> Result<PlaybackCommand, PluginError> {
    let payload_str =
        std::str::from_utf8(&correction.payload).map_err(|_| {
            PluginError::Permanent(
                "course correction payload is not valid UTF-8".to_string(),
            )
        })?;
    let trimmed = payload_str.trim();

    match correction.correction_type.as_str() {
        "play" => {
            if trimmed.is_empty() {
                Ok(PlaybackCommand::Play)
            } else {
                let pos = trimmed.parse::<u32>().map_err(|_| {
                    PluginError::Permanent(format!(
                        "play position must be a non-negative u32, got {:?}",
                        trimmed
                    ))
                })?;
                Ok(PlaybackCommand::PlayPosition(pos))
            }
        }
        "pause" => match trimmed {
            "1" | "true" => Ok(PlaybackCommand::Pause(true)),
            "0" | "false" => Ok(PlaybackCommand::Pause(false)),
            other => Err(PluginError::Permanent(format!(
                "pause payload must be '0'/'1' or 'true'/'false', got {:?}",
                other
            ))),
        },
        "stop" => Ok(PlaybackCommand::Stop),
        "next" => Ok(PlaybackCommand::Next),
        "previous" => Ok(PlaybackCommand::Previous),
        "seek" => {
            let ms = trimmed.parse::<u64>().map_err(|_| {
                PluginError::Permanent(format!(
                    "seek payload must be a non-negative u64 of milliseconds, got {:?}",
                    trimmed
                ))
            })?;
            Ok(PlaybackCommand::Seek(Duration::from_millis(ms)))
        }
        "set_volume" => {
            let v = trimmed.parse::<u8>().map_err(|_| {
                PluginError::Permanent(format!(
                    "set_volume payload must be a u8 (0-255), got {:?}",
                    trimmed
                ))
            })?;
            Ok(PlaybackCommand::SetVolume(v))
        }
        other => Err(PluginError::Permanent(format!(
            "unknown course correction type: {:?}",
            other
        ))),
    }
}

/// Map a [`PlaybackError`] from the supervisor into the
/// [`PluginError`] variant the steward expects.
///
/// - [`PlaybackError::Ack`] is command-level: the connection is
///   healthy, MPD said no. Retrying will get the same answer.
///   Maps to [`PluginError::Permanent`].
/// - [`PlaybackError::ConnectionExhausted`] is transient: MPD was
///   unreachable across all reconnect attempts. The steward can
///   retry at a higher level. Maps to [`PluginError::Transient`].
/// - [`PlaybackError::Protocol`] is fatal: MPD is not speaking
///   the protocol correctly. Maps to [`PluginError::Fatal`] via
///   the SDK's `fatal(context, source)` helper, with the
///   [`PlaybackError`] itself as the source (it implements
///   [`std::error::Error`] via `thiserror`).
/// - [`PlaybackError::Shutdown`] means the supervisor is gone.
///   Maps to [`PluginError::Permanent`]; the caller should
///   release and re-take.
fn playback_error_to_plugin_error(e: PlaybackError) -> PluginError {
    match e {
        PlaybackError::Ack { code, message } => PluginError::Permanent(
            format!("MPD rejected command: [{}] {}", code, message),
        ),
        PlaybackError::ConnectionExhausted { attempts } => {
            PluginError::Transient(format!(
                "MPD unreachable after {} reconnect attempts",
                attempts
            ))
        }
        err @ PlaybackError::Protocol(_) => {
            PluginError::fatal("MPD protocol violation", err)
        }
        PlaybackError::Shutdown => PluginError::Permanent(
            "playback supervisor is shut down".to_string(),
        ),
    }
}

impl Plugin for MpdPlaybackPlugin {
    fn probe_plans(&self) -> Vec<evo_plugin_sdk::privileges::ProbePlan> {
        use evo_plugin_sdk::privileges::{
            AccessMode, FilesystemAccessProbe, ProbePlan, SudoersCommandProbe,
        };

        let mut plans: Vec<ProbePlan> = Vec::with_capacity(2);

        // mpd_systemctl_restart — strategy depends on EUID:
        // root → DirectSystemctlRestarter (no sudo); non-root →
        // SudoSystemctlRestarter (NOPASSWD sudo). When running
        // as root, probing `sudo -l -n` is misleading (root can
        // sudo anything), so we synthesise an Available
        // resolution via a BinaryPresentProbe on systemctl. When
        // non-root, we probe the sudoers entry directly.
        let systemctl_bin = std::env::var("EVO_SYSTEMCTL")
            .unwrap_or_else(|_| "/usr/bin/systemctl".to_string());
        // Reuse the plugin's existing EUID detector
        // (`/proc/self/status` on Linux, `EVO_RUNTIME_USER`
        // elsewhere) so the probe-side strategy hint and the
        // legacy fallback path observe identical mechanics.
        let needs_sudo = crate::mpd_restart::process_needs_sudo();
        if !needs_sudo {
            plans.push(ProbePlan {
                intent_id: INTENT_MPD_SYSTEMCTL_RESTART.to_string(),
                probe: Box::new(
                    evo_plugin_sdk::privileges::BinaryPresentProbe::new(
                        systemctl_bin.clone(),
                    ),
                ),
                strategy_hint: Some("direct".to_string()),
                remedy: format!(
                    "install systemd ({systemctl_bin} not on PATH); MPD \
                     restart leg disabled until present"
                ),
            });
        } else if let Some(probe) =
            SudoersCommandProbe::new([systemctl_bin.as_str(), "restart", "mpd"])
        {
            plans.push(ProbePlan {
                intent_id: INTENT_MPD_SYSTEMCTL_RESTART.to_string(),
                probe: Box::new(probe),
                strategy_hint: Some("sudo".to_string()),
                remedy: format!(
                    "install the distribution bootstrap sudoers drop-in \
                     granting NOPASSWD `{systemctl_bin} restart mpd` to the \
                     steward service user"
                ),
            });
        }

        // mpd_fragment_write — checks write access on the fragment
        // path the worker will emit to. No strategy hint: the
        // worker treats the resolution as Available / Unavailable
        // and publishes FragmentWorkerStatus::Failed when the path
        // is unwritable.
        plans.push(ProbePlan {
            intent_id: INTENT_MPD_FRAGMENT_WRITE.to_string(),
            probe: Box::new(FilesystemAccessProbe::new(
                &self.fragment_path,
                AccessMode::Writable,
            )),
            strategy_hint: None,
            remedy: format!(
                "ensure {} (and its parent directory) is writable by the \
                 steward service user; run the distribution bootstrap to \
                 chown /etc/evo to the service user",
                self.fragment_path.display()
            ),
        });

        plans
    }

    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: PLUGIN_NAME.to_string(),
                    version: plugin_crate_version(),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: SOURCE_REQUEST_TYPES
                        .iter()
                        .map(|s| (*s).to_string())
                        .collect(),
                    accepts_custody: true,
                    flags: Default::default(),
                    course_correct_verbs: COURSE_CORRECT_VERBS
                        .iter()
                        .map(|s| (*s).to_string())
                        .collect(),
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
                "plugin load beginning"
            );

            self.apply_config_table(&ctx.config)?;

            // Resolve the MPD restart strategy from the
            // framework's preflight result. AutoMpdRestarter
            // inspects the capability-resolution map for the
            // `mpd_systemctl_restart` intent and picks the
            // right concrete strategy (Direct / Sudo /
            // disabled). When the framework's runner is not
            // yet wired (the map is empty) the composite
            // falls back to /proc/self/status EUID detection
            // — same shape volumio-evo has run in production.
            // Production code path swaps the
            // `SudoSystemctlRestarter` default the constructor
            // installed; tests that constructed the plugin
            // through `with_restarter` keep their injected
            // strategy because they never invoke `Plugin::load`.
            let auto = Arc::new(AutoMpdRestarter::resolve(&ctx.capabilities));
            tracing::info!(
                plugin = PLUGIN_NAME,
                strategy = auto.current_strategy_name(),
                rationale = %auto.rationale(),
                "MPD restart strategy resolved"
            );
            self.restarter = auto.clone() as Arc<dyn MpdRestarter>;
            self.auto_restarter = Some(auto.clone());

            // Spawn the PPAG capabilities-watch reactor when
            // the framework's hot-tightening re-probe task is
            // publishing live updates.
            if let Some(rx) = ctx.capabilities_watch.clone() {
                self.spawn_capabilities_watcher(auto, rx);
            }

            // Engage the audio data plane. The plugin is a
            // source plugin (declared via
            // [capabilities.source] with output_kind =
            // "audio.pcm"); admission MUST hand it an
            // audio_routing handle. install_routing refuses
            // loudly when the handle is None — that
            // indicates manifest / trust / admission
            // misconfiguration.
            self.install_routing(ctx.audio_routing.clone())?;

            // Equip the subject emitter from the announcer
            // handles the steward supplied. The Arcs are cloned
            // cheaply; the emitter clones them again per custody
            // (one clone per spawn() call).
            self.subject_emitter = Some(SubjectEmitter::new(
                Arc::clone(&ctx.subject_announcer) as Arc<dyn SubjectAnnouncer>,
                Arc::clone(&ctx.relation_announcer)
                    as Arc<dyn RelationAnnouncer>,
            ));

            // Spawn the route-change reactor and the
            // fragment-writer worker. The reactor watches
            // the framework's topology rewires; the worker
            // renders MPD's audio_output block and recycles
            // MPD on every snapshot.
            self.spawn_reactor().await?;
            self.spawn_fragment_worker().await?;

            // Subscribe to the audio-options settings subject
            // so operator mixer-mode changes propagate to the
            // fragment-writer without restarting the steward.
            // The framework's subject_state_subscriber is
            // populated for in-process plugins; OOP plugins
            // see None until the wire surface lands. Failure
            // to wire the subscription does NOT fail the
            // load — operators can still pick a mode via the
            // plugin's own config table.
            self.spawn_options_settings_subscriber(ctx).await;

            self.loaded = true;

            tracing::info!(
                plugin = PLUGIN_NAME,
                endpoint = %self.endpoint,
                connect_ms = self.timeouts.connect.as_millis() as u64,
                welcome_ms = self.timeouts.welcome.as_millis() as u64,
                command_ms = self.timeouts.command.as_millis() as u64,
                fragment_path = %self.fragment_path.display(),
                "plugin loaded; config applied; subject emitter equipped; \
                 route-change reactor + fragment-writer worker running"
            );

            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            let active = self.custodies.len();
            tracing::info!(
                plugin = PLUGIN_NAME,
                active = active,
                taken = self.custodies_taken,
                dispatched = self.corrections_dispatched,
                "plugin unload; draining active custodies"
            );

            // Drain and shut down each supervisor in sequence.
            let custodies = std::mem::take(&mut self.custodies);
            for (id, tracked) in custodies {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    handle = %id,
                    custody_type = %tracked.custody_type,
                    "shutting down supervisor during unload"
                );
                tracked.supervisor.shutdown().await;
            }

            // Stop the fragment-writer worker first — it
            // subscribes to the reactor's snapshot channel,
            // so tearing the reactor down before the worker
            // would race the worker against a closed
            // channel. Then stop the reactor (which also
            // clears the framework-held callback). Finally
            // release the routing handle.
            self.stop_capabilities_watcher().await;
            self.stop_fragment_worker().await;
            self.stop_reactor().await;
            self.audio_routing = None;

            self.loaded = false;
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            if self.loaded {
                HealthReport::healthy()
            } else {
                HealthReport::unhealthy("playback plugin not loaded")
            }
        }
    }
}

impl Warden for MpdPlaybackPlugin {
    fn take_custody(
        &mut self,
        assignment: Assignment,
    ) -> impl Future<Output = Result<CustodyHandle, PluginError>> + Send + '_
    {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "playback plugin not loaded".to_string(),
                ));
            }

            // Defense in depth: load() populates the emitter
            // alongside setting `loaded = true`, so the two gates
            // are coupled in practice. An explicit check here
            // makes the invariant local and survives any future
            // restructuring of load().
            let emitter = match self.subject_emitter.as_ref() {
                Some(e) => e.clone(),
                None => {
                    return Err(PluginError::Permanent(
                        "subject emitter not initialised; load() was not called".to_string(),
                    ));
                }
            };

            let handle = CustodyHandle::new(format!(
                "custody-{}",
                assignment.correlation_id
            ));

            // Spawn the supervisor. Opens two MPD connections,
            // emits the initial state report, returns a handle for
            // command dispatch and shutdown. Failure maps to the
            // steward-visible PluginError variant.
            let supervisor = match playback_supervisor::spawn(
                self.endpoint.clone(),
                self.timeouts,
                handle.clone(),
                assignment.custody_state_reporter,
                emitter,
            )
            .await
            {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!(
                        plugin = PLUGIN_NAME,
                        handle = %handle.id,
                        error = %e,
                        "supervisor spawn failed; rejecting custody"
                    );
                    return Err(playback_error_to_plugin_error(e));
                }
            };

            self.custodies.insert(
                handle.id.clone(),
                TrackedCustody {
                    custody_type: assignment.custody_type.clone(),
                    supervisor,
                },
            );
            self.custodies_taken += 1;

            tracing::info!(
                plugin = PLUGIN_NAME,
                handle = %handle.id,
                custody_type = %assignment.custody_type,
                cid = assignment.correlation_id,
                "custody accepted"
            );

            Ok(handle)
        }
    }

    fn course_correct<'a>(
        &'a mut self,
        handle: &'a CustodyHandle,
        correction: CourseCorrection,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "playback plugin not loaded".to_string(),
                ));
            }

            // Parse first: a malformed correction fails with a
            // clear "request was bad" signal before we ever touch
            // the custody map or the supervisor.
            let cmd = parse_correction(&correction)?;

            let tracked = self.custodies.get(&handle.id).ok_or_else(|| {
                PluginError::Permanent(format!(
                    "unknown custody handle: {}",
                    handle.id
                ))
            })?;

            self.corrections_dispatched += 1;

            tracing::info!(
                plugin = PLUGIN_NAME,
                handle = %handle.id,
                correction_type = %correction.correction_type,
                cid = correction.correlation_id,
                "course correction dispatching to supervisor"
            );

            tracked
                .supervisor
                .command(cmd)
                .await
                .map_err(playback_error_to_plugin_error)
        }
    }

    fn release_custody(
        &mut self,
        handle: CustodyHandle,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "playback plugin not loaded".to_string(),
                ));
            }

            let tracked =
                self.custodies.remove(&handle.id).ok_or_else(|| {
                    PluginError::Permanent(format!(
                        "unknown custody handle: {}",
                        handle.id
                    ))
                })?;

            tracing::info!(
                plugin = PLUGIN_NAME,
                handle = %handle.id,
                custody_type = %tracked.custody_type,
                "custody releasing; shutting down supervisor"
            );

            tracked.supervisor.shutdown().await;

            tracing::info!(
                plugin = PLUGIN_NAME,
                handle = %handle.id,
                "custody released"
            );

            Ok(())
        }
    }
}

impl Respondent for MpdPlaybackPlugin {
    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "playback plugin not loaded".to_string(),
                ));
            }
            if req.is_past_deadline() {
                return Err(PluginError::Transient(
                    "request deadline already expired".to_string(),
                ));
            }
            if !SOURCE_REQUEST_TYPES.contains(&req.request_type.as_str()) {
                return Err(PluginError::Permanent(format!(
                    "unknown request type: {:?} (declared types: {:?})",
                    req.request_type, SOURCE_REQUEST_TYPES
                )));
            }

            self.requests_handled += 1;

            match req.request_type.as_str() {
                "play_now" => self.handle_play_now(req).await,
                "play" => {
                    self.handle_simple_command(req, PlaybackCommand::Play).await
                }
                "pause" => {
                    self.handle_simple_command(
                        req,
                        PlaybackCommand::Pause(true),
                    )
                    .await
                }
                "resume" => {
                    self.handle_simple_command(
                        req,
                        PlaybackCommand::Pause(false),
                    )
                    .await
                }
                "stop" => {
                    self.handle_simple_command(req, PlaybackCommand::Stop).await
                }
                "next" => {
                    self.handle_simple_command(req, PlaybackCommand::Next).await
                }
                "previous" => {
                    self.handle_simple_command(req, PlaybackCommand::Previous)
                        .await
                }
                "seek" => self.handle_seek(req).await,
                "set_volume" => self.handle_set_volume(req).await,
                other => Err(PluginError::Permanent(format!(
                    "request type {other:?} declared but no handler wired; \
                     this is a manifest/runtime drift bug"
                ))),
            }
        }
    }
}

impl MpdPlaybackPlugin {
    /// Handle a `play_now` source-verb request: parse the
    /// payload, verify the URI scheme this plugin owns,
    /// extract the library path, and dispatch
    /// [`PlaybackCommand::LoadAndPlay`] through the active
    /// custody's supervisor.
    async fn handle_play_now(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: PlayNowPayload = parse_versioned_payload(req, "play_now")?;
        let path = parse_mpd_path_uri(&payload.uri)?;
        let supervisor = self.active_supervisor("play_now")?;
        supervisor
            .command(PlaybackCommand::LoadAndPlay(path.to_string()))
            .await
            .map_err(playback_error_to_plugin_error)?;
        encode_play_now_ok(req, payload.uri)
    }

    /// Handle a source-verb request whose payload is the
    /// bare envelope (`{ "v": 1 }`) and whose effect is one
    /// fixed [`PlaybackCommand`]. Covers `play` / `pause` /
    /// `resume` / `stop` / `next` / `previous`.
    async fn handle_simple_command(
        &self,
        req: &Request,
        cmd: PlaybackCommand,
    ) -> Result<Response, PluginError> {
        let _: EmptyPayload =
            parse_versioned_payload(req, req.request_type.as_str())?;
        let supervisor = self.active_supervisor(req.request_type.as_str())?;
        supervisor
            .command(cmd)
            .await
            .map_err(playback_error_to_plugin_error)?;
        encode_simple_ok(req)
    }

    /// Handle a `seek` source-verb request: extract the
    /// target millisecond position and issue a
    /// [`PlaybackCommand::Seek`].
    async fn handle_seek(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: SeekPayload = parse_versioned_payload(req, "seek")?;
        let supervisor = self.active_supervisor("seek")?;
        supervisor
            .command(PlaybackCommand::Seek(Duration::from_millis(
                payload.position_ms,
            )))
            .await
            .map_err(playback_error_to_plugin_error)?;
        encode_simple_ok(req)
    }

    /// Handle a `set_volume` source-verb request: clamp /
    /// validate the volume byte and issue a
    /// [`PlaybackCommand::SetVolume`].
    async fn handle_set_volume(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: SetVolumePayload =
            parse_versioned_payload(req, "set_volume")?;
        let supervisor = self.active_supervisor("set_volume")?;
        supervisor
            .command(PlaybackCommand::SetVolume(payload.volume))
            .await
            .map_err(playback_error_to_plugin_error)?;
        encode_simple_ok(req)
    }

    /// Pick the active custody's supervisor.
    /// `custody_exclusive = true` in the manifest means at
    /// most one custody exists at any time; the framework's
    /// source-verb dispatcher acquires custody before
    /// invoking `handle_request` so the slot is populated.
    /// Zero custodies indicates a race or framework
    /// misconfiguration; refuse loudly rather than silently
    /// no-op. `SupervisorHandle` is not `Clone` (it owns the
    /// shutdown signal half), so dispatch through a
    /// reference rather than copying the handle.
    fn active_supervisor(
        &self,
        verb: &str,
    ) -> Result<&SupervisorHandle, PluginError> {
        self.custodies
            .values()
            .next()
            .map(|t| &t.supervisor)
            .ok_or_else(|| {
                PluginError::Permanent(format!(
                    "{verb:?} received but no active custody on the warden — \
                     the framework's source-verb dispatcher should have \
                     acquired custody before invoking handle_request"
                ))
            })
    }
}

/// Parse the request's payload as a JSON envelope of type `T`
/// and validate the `v` field equals [`PAYLOAD_VERSION`].
/// Every source-verb payload struct embeds `v: u32` so the
/// version check is uniform across the surface.
fn parse_versioned_payload<T>(
    req: &Request,
    verb: &str,
) -> Result<T, PluginError>
where
    T: serde::de::DeserializeOwned + HasPayloadVersion,
{
    let parsed: T = serde_json::from_slice(&req.payload).map_err(|e| {
        PluginError::Permanent(format!(
            "{verb:?} payload is not valid JSON for the expected shape: {e}"
        ))
    })?;
    if parsed.payload_version() != PAYLOAD_VERSION {
        return Err(PluginError::Permanent(format!(
            "{verb:?} payload version {} unsupported; expected {}",
            parsed.payload_version(),
            PAYLOAD_VERSION
        )));
    }
    Ok(parsed)
}

/// Common shape across every source-verb payload struct: a
/// `v: u32` envelope field. Implemented mechanically.
trait HasPayloadVersion {
    fn payload_version(&self) -> u32;
}

fn encode_simple_ok(req: &Request) -> Result<Response, PluginError> {
    let body = serde_json::to_vec(&SimpleResponse {
        v: PAYLOAD_VERSION,
        status: "ok",
    })
    .map_err(|e| {
        PluginError::Permanent(format!(
            "{verb} response JSON encode failed: {e}",
            verb = req.request_type
        ))
    })?;
    Ok(Response::for_request(req, body))
}

fn encode_play_now_ok(
    req: &Request,
    uri: String,
) -> Result<Response, PluginError> {
    let body = serde_json::to_vec(&PlayNowResponse {
        v: PAYLOAD_VERSION,
        status: "ok",
        uri,
    })
    .map_err(|e| {
        PluginError::Permanent(format!(
            "play_now response JSON encode failed: {e}"
        ))
    })?;
    Ok(Response::for_request(req, body))
}

/// Wire shape of the `play_now` request payload. Carries
/// the envelope `v` and the full URI; the plugin validates
/// the scheme prefix matches one it owns and strips the
/// prefix to form an MPD library-relative path.
///
/// `v` defaults to [`PAYLOAD_VERSION`] when absent so the
/// plugin accepts both the legacy F2-era `{ uri }` shape
/// and the F4 versioned `{ v, uri }` shape. The framework's
/// source-verb dispatcher is updated in lockstep to emit the
/// versioned shape; the defaulted-`v` is the
/// backwards-compatibility bridge against older framework
/// builds and against on-disk plan files that pre-date the
/// envelope.
#[derive(Debug, serde::Deserialize)]
struct PlayNowPayload {
    #[serde(default = "default_payload_version")]
    v: u32,
    uri: String,
}

impl HasPayloadVersion for PlayNowPayload {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

/// Wire shape of a `seek` request payload. `v` defaults to
/// [`PAYLOAD_VERSION`] when absent, mirroring
/// [`PlayNowPayload`]'s tolerance.
#[derive(Debug, serde::Deserialize)]
struct SeekPayload {
    #[serde(default = "default_payload_version")]
    v: u32,
    position_ms: u64,
}

impl HasPayloadVersion for SeekPayload {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

/// Wire shape of a `set_volume` request payload. `v`
/// defaults to [`PAYLOAD_VERSION`] when absent.
#[derive(Debug, serde::Deserialize)]
struct SetVolumePayload {
    #[serde(default = "default_payload_version")]
    v: u32,
    volume: u8,
}

impl HasPayloadVersion for SetVolumePayload {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

/// Wire shape of a bare-envelope request payload (`{ "v":
/// 1 }` or `{}`). Used by every source verb whose action
/// carries no parameters: `play` / `pause` / `resume` /
/// `stop` / `next` / `previous`. `v` defaults to
/// [`PAYLOAD_VERSION`] when absent.
#[derive(Debug, serde::Deserialize)]
struct EmptyPayload {
    #[serde(default = "default_payload_version")]
    v: u32,
}

impl HasPayloadVersion for EmptyPayload {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

/// Default function for serde's `default = "..."` attribute
/// on every source-verb payload's `v` field. Returns the
/// current [`PAYLOAD_VERSION`] so absent fields are treated
/// as "this payload is on the current wire shape" rather
/// than a hard-coded literal.
fn default_payload_version() -> u32 {
    PAYLOAD_VERSION
}

/// Wire shape of the `play_now` success response. Echoes
/// the URI back so the caller can confirm the dispatch
/// landed against the URI it sent (cheap correctness
/// check; useful for dispatch-tracing diagnostics).
#[derive(Debug, serde::Serialize)]
struct PlayNowResponse {
    v: u32,
    status: &'static str,
    uri: String,
}

/// Wire shape of the bare-envelope success response every
/// non-`play_now` source verb returns. Caller correlates
/// against the request via the framework's correlation_id;
/// the response body confirms the verb executed without
/// echoing any verb-specific data.
#[derive(Debug, serde::Serialize)]
struct SimpleResponse {
    v: u32,
    status: &'static str,
}

/// Strip the `mpd-path:` URI scheme prefix and return the
/// remaining library path. Refuses URIs that don't bear
/// the expected scheme — those routed here through a
/// framework-side URI-routing mistake; surface the
/// problem rather than silently treating the URI as a
/// library path.
fn parse_mpd_path_uri(uri: &str) -> Result<&str, PluginError> {
    let prefix = format!("{URI_SCHEME_MPD_PATH}:");
    if let Some(path) = uri.strip_prefix(&prefix) {
        if path.is_empty() {
            return Err(PluginError::Permanent(format!(
                "play_now URI {uri:?} has empty path component after scheme"
            )));
        }
        Ok(path)
    } else {
        Err(PluginError::Permanent(format!(
            "play_now URI {uri:?} does not bear the {URI_SCHEME_MPD_PATH:?} \
             scheme this plugin owns; framework's URI router should not \
             have dispatched it here"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use evo_plugin_sdk::contract::{CustodyStateReporter, HealthStatus};

    use crate::playback_supervisor::test_mock::{
        capturing_emitter, short_timeouts, spawn_mock_mpd,
        spawn_unresponsive_mock, CapturingReporter, ConnBehaviour,
    };
    use crate::playback_supervisor::SubjectEmitter;

    // ----- helpers -----

    fn assignment(
        reporter: Arc<dyn CustodyStateReporter>,
        correlation_id: u64,
    ) -> Assignment {
        Assignment {
            custody_type: "playback-session".into(),
            payload: b"track-1".to_vec(),
            correlation_id,
            deadline: None,
            custody_state_reporter: reporter,
        }
    }

    fn correction(
        correction_type: &str,
        payload: &[u8],
        correlation_id: u64,
    ) -> CourseCorrection {
        CourseCorrection {
            correction_type: correction_type.to_string(),
            payload: payload.to_vec(),
            correlation_id,
        }
    }

    async fn loaded_plugin_with_mock(
        behaviours: Vec<ConnBehaviour>,
    ) -> (MpdPlaybackPlugin, tokio::task::JoinHandle<()>) {
        let (endpoint, mock_task) = spawn_mock_mpd(behaviours).await;
        let mut p =
            MpdPlaybackPlugin::with_endpoint(endpoint, short_timeouts());
        p.loaded = true;
        // Tests using this helper are not exercising the subject
        // emission pipeline; equip a null emitter to satisfy
        // take_custody's gate without recording anything.
        p.subject_emitter = Some(SubjectEmitter::null());
        (p, mock_task)
    }

    // ===== surface / manifest tests (pure) =====

    #[test]
    fn embedded_manifest_parses() {
        let m = manifest();
        assert_eq!(m.plugin.name, PLUGIN_NAME);
        assert_eq!(m.plugin.contract, 1);
        assert_eq!(
            m.kind
                .as_ref()
                .expect("manifest must declare [kind]")
                .interaction,
            evo_plugin_sdk::manifest::InteractionShape::Warden
        );
    }

    #[test]
    fn manifest_course_correct_verbs_match_runtime() {
        let m = manifest();
        let warden = m
            .capabilities
            .warden
            .as_ref()
            .expect("manifest must declare [capabilities.warden]");
        let manifest_verbs: Vec<&str> = warden
            .course_correct_verbs
            .as_ref()
            .expect(
                "manifest must declare \
                 [capabilities.warden].course_correct_verbs",
            )
            .iter()
            .map(String::as_str)
            .collect();
        // Round-trip: every const-table verb appears in
        // the manifest, and every manifest verb appears
        // in the const table. Drift between these two is
        // caught here at unit-test time rather than at
        // admission.
        for declared in COURSE_CORRECT_VERBS {
            assert!(
                manifest_verbs.contains(declared),
                "COURSE_CORRECT_VERBS entry {:?} missing from \
                 manifest verbs {:?}",
                declared,
                manifest_verbs
            );
        }
        for verb in &manifest_verbs {
            assert!(
                COURSE_CORRECT_VERBS.contains(verb),
                "manifest verb {:?} missing from \
                 COURSE_CORRECT_VERBS {:?}",
                verb,
                COURSE_CORRECT_VERBS
            );
        }
    }

    #[test]
    fn manifest_request_types_match_runtime() {
        let m = manifest();
        let respondent = m
            .capabilities
            .respondent
            .as_ref()
            .expect("manifest must declare [capabilities.respondent]");
        let manifest_types: Vec<&str> = respondent
            .request_types
            .iter()
            .map(String::as_str)
            .collect();
        // Round-trip: every const-table type appears in the
        // manifest, and every manifest type appears in the
        // const table. Drift caught at unit-test time
        // rather than at admission.
        for declared in SOURCE_REQUEST_TYPES {
            assert!(
                manifest_types.contains(declared),
                "SOURCE_REQUEST_TYPES entry {:?} missing from \
                 manifest types {:?}",
                declared,
                manifest_types
            );
        }
        for t in &manifest_types {
            assert!(
                SOURCE_REQUEST_TYPES.contains(t),
                "manifest type {:?} missing from \
                 SOURCE_REQUEST_TYPES {:?}",
                t,
                SOURCE_REQUEST_TYPES
            );
        }
    }

    #[tokio::test]
    async fn identity_name_and_version_match_manifest() {
        let p = MpdPlaybackPlugin::new();
        let d = p.describe().await;
        let m = manifest();
        assert_eq!(d.identity.name, m.plugin.name);
        assert_eq!(d.identity.name, PLUGIN_NAME);
        assert_eq!(
            d.identity.version, m.plugin.version,
            "CARGO_PKG_VERSION / describe() / manifest [plugin].version must match"
        );
    }

    #[tokio::test]
    async fn describe_returns_expected_identity() {
        let p = MpdPlaybackPlugin::new();
        let d = p.describe().await;
        assert_eq!(d.identity.name, PLUGIN_NAME);
        assert_eq!(d.identity.version, plugin_crate_version());
        assert_eq!(d.build_info.plugin_build, d.identity.version.to_string());
        assert_eq!(d.identity.contract, 1);
        assert!(d.runtime_capabilities.accepts_custody);
        assert_eq!(
            d.runtime_capabilities.request_types,
            SOURCE_REQUEST_TYPES
                .iter()
                .map(|s| (*s).to_string())
                .collect::<Vec<_>>(),
        );
        assert_eq!(
            d.runtime_capabilities.course_correct_verbs,
            COURSE_CORRECT_VERBS
                .iter()
                .map(|s| (*s).to_string())
                .collect::<Vec<_>>(),
        );
    }

    #[tokio::test]
    async fn health_is_unhealthy_before_load() {
        let p = MpdPlaybackPlugin::new();
        let r = p.health_check().await;
        assert!(matches!(r.status, HealthStatus::Unhealthy));
    }

    #[tokio::test]
    async fn load_unload_cycle_with_no_custodies() {
        let mut p = MpdPlaybackPlugin::new();
        p.loaded = true;
        assert!(matches!(
            p.health_check().await.status,
            HealthStatus::Healthy
        ));
        p.unload().await.unwrap();
        assert!(matches!(
            p.health_check().await.status,
            HealthStatus::Unhealthy
        ));
        assert_eq!(p.active_custody_count(), 0);
    }

    #[test]
    fn new_uses_default_endpoint_and_timeouts() {
        let p = MpdPlaybackPlugin::new();
        assert_eq!(p.endpoint, MpdEndpoint::tcp("127.0.0.1", 6600).unwrap());
        let d = ConnectTimeouts::default();
        assert_eq!(p.timeouts.connect, d.connect);
        assert_eq!(p.timeouts.welcome, d.welcome);
        assert_eq!(p.timeouts.command, d.command);
    }

    // ===== apply_config_table tests =====

    #[test]
    fn apply_config_table_empty_keeps_defaults() {
        let mut p = MpdPlaybackPlugin::new();
        let before_endpoint = p.endpoint.clone();
        let before_connect = p.timeouts.connect;

        let table: toml::Table = "".parse().unwrap();
        p.apply_config_table(&table).unwrap();

        assert_eq!(p.endpoint, before_endpoint);
        assert_eq!(p.timeouts.connect, before_connect);
    }

    #[test]
    fn apply_config_table_tcp_overrides_endpoint() {
        let mut p = MpdPlaybackPlugin::new();

        let table: toml::Table = r#"
            [endpoint]
            type = "tcp"
            host = "mpd.example"
            port = 6700
        "#
        .parse()
        .unwrap();
        p.apply_config_table(&table).unwrap();

        assert_eq!(p.endpoint, MpdEndpoint::tcp("mpd.example", 6700).unwrap());
    }

    #[test]
    fn apply_config_table_unix_overrides_endpoint() {
        let mut p = MpdPlaybackPlugin::new();

        let table: toml::Table = r#"
            [endpoint]
            type = "unix"
            path = "/run/mpd/socket"
        "#
        .parse()
        .unwrap();
        p.apply_config_table(&table).unwrap();

        assert_eq!(p.endpoint, MpdEndpoint::unix("/run/mpd/socket").unwrap());
    }

    #[test]
    fn apply_config_table_overrides_timeouts() {
        let mut p = MpdPlaybackPlugin::new();

        let table: toml::Table = r#"
            [timeouts]
            connect_ms = 1234
            welcome_ms = 567
            command_ms = 8910
        "#
        .parse()
        .unwrap();
        p.apply_config_table(&table).unwrap();

        assert_eq!(p.timeouts.connect, Duration::from_millis(1234));
        assert_eq!(p.timeouts.welcome, Duration::from_millis(567));
        assert_eq!(p.timeouts.command, Duration::from_millis(8910));
    }

    #[test]
    fn apply_config_table_invalid_config_returns_permanent() {
        let mut p = MpdPlaybackPlugin::new();

        let table: toml::Table = r#"
            [endpoint]
            type = "carrier-pigeon"
        "#
        .parse()
        .unwrap();
        let e = p.apply_config_table(&table).unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));

        // Failed apply leaves state unchanged.
        assert_eq!(p.endpoint, MpdEndpoint::tcp("127.0.0.1", 6600).unwrap());
    }

    #[test]
    fn apply_config_table_wraps_error_message() {
        let mut p = MpdPlaybackPlugin::new();

        let table: toml::Table = r#"
            [endpoint]
            port = 0
        "#
        .parse()
        .unwrap();
        let e = p.apply_config_table(&table).unwrap_err();
        match e {
            PluginError::Permanent(msg) => {
                assert!(
                    msg.contains("invalid plugin config"),
                    "message should namespace the error: {msg:?}"
                );
                assert!(
                    msg.contains("port"),
                    "message should mention the offending field: {msg:?}"
                );
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    // ===== gate tests (pure) =====

    #[tokio::test]
    async fn take_custody_rejects_before_load() {
        let mut p = MpdPlaybackPlugin::new();
        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());
        let a = assignment(reporter, 1);
        let e = p.take_custody(a).await.unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    #[tokio::test]
    async fn course_correct_rejects_before_load() {
        let mut p = MpdPlaybackPlugin::new();
        let handle = CustodyHandle::new("custody-1");
        let e = p
            .course_correct(&handle, correction("play", b"", 1))
            .await
            .unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    #[tokio::test]
    async fn course_correct_rejects_unknown_handle() {
        let mut p = MpdPlaybackPlugin::new();
        p.loaded = true;
        let handle = CustodyHandle::new("custody-does-not-exist");
        let e = p
            .course_correct(&handle, correction("play", b"", 1))
            .await
            .unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
        assert_eq!(p.corrections_dispatched(), 0);
    }

    #[tokio::test]
    async fn release_custody_rejects_unknown_handle() {
        let mut p = MpdPlaybackPlugin::new();
        p.loaded = true;
        let handle = CustodyHandle::new("custody-phantom");
        let e = p.release_custody(handle).await.unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    // ===== F2 substrate consumption tests =====

    #[tokio::test]
    async fn install_routing_refuses_when_handle_is_none() {
        let mut p = MpdPlaybackPlugin::new();
        let err = p.install_routing(None).expect_err(
            "playback.mpd plugin must refuse load without audio_routing",
        );
        match err {
            PluginError::Permanent(msg) => {
                assert!(
                    msg.contains("audio_routing"),
                    "refusal message must name the missing field: {msg:?}"
                );
            }
            other => panic!("expected Permanent error, got {other:?}"),
        }
        assert!(p.audio_routing.is_none());
    }

    // ===== F2 play_now URI parsing tests (pure) =====

    #[test]
    fn parse_mpd_path_uri_strips_scheme() {
        let path = parse_mpd_path_uri("mpd-path:Music/Album/Track.flac")
            .expect("scheme strip");
        assert_eq!(path, "Music/Album/Track.flac");
    }

    #[test]
    fn parse_mpd_path_uri_refuses_unknown_scheme() {
        let err = parse_mpd_path_uri("file:/Music/Track.flac")
            .expect_err("non-mpd-path scheme must refuse");
        match err {
            PluginError::Permanent(msg) => {
                assert!(msg.contains("mpd-path"));
                assert!(msg.contains("file:"));
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    #[test]
    fn parse_mpd_path_uri_refuses_empty_path() {
        let err = parse_mpd_path_uri("mpd-path:")
            .expect_err("empty path component must refuse");
        match err {
            PluginError::Permanent(msg) => {
                assert!(msg.contains("empty path"));
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    // ===== parse_correction tests (pure) =====

    #[test]
    fn parse_play_empty_payload_returns_play() {
        let c = correction("play", b"", 1);
        assert_eq!(parse_correction(&c).unwrap(), PlaybackCommand::Play);
    }

    #[test]
    fn parse_play_with_position() {
        let c = correction("play", b"3", 1);
        assert_eq!(
            parse_correction(&c).unwrap(),
            PlaybackCommand::PlayPosition(3)
        );
    }

    #[test]
    fn parse_pause_accepts_one_and_true() {
        for variant in [b"1" as &[u8], b"true"] {
            let c = correction("pause", variant, 1);
            assert_eq!(
                parse_correction(&c).unwrap(),
                PlaybackCommand::Pause(true)
            );
        }
    }

    #[test]
    fn parse_pause_accepts_zero_and_false() {
        for variant in [b"0" as &[u8], b"false"] {
            let c = correction("pause", variant, 1);
            assert_eq!(
                parse_correction(&c).unwrap(),
                PlaybackCommand::Pause(false)
            );
        }
    }

    #[test]
    fn parse_stop_next_previous_with_empty_payload() {
        assert_eq!(
            parse_correction(&correction("stop", b"", 1)).unwrap(),
            PlaybackCommand::Stop
        );
        assert_eq!(
            parse_correction(&correction("next", b"", 1)).unwrap(),
            PlaybackCommand::Next
        );
        assert_eq!(
            parse_correction(&correction("previous", b"", 1)).unwrap(),
            PlaybackCommand::Previous
        );
    }

    #[test]
    fn parse_seek_with_milliseconds() {
        let c = correction("seek", b"1250", 1);
        assert_eq!(
            parse_correction(&c).unwrap(),
            PlaybackCommand::Seek(Duration::from_millis(1250))
        );
    }

    #[test]
    fn parse_seek_with_zero_is_valid() {
        let c = correction("seek", b"0", 1);
        assert_eq!(
            parse_correction(&c).unwrap(),
            PlaybackCommand::Seek(Duration::from_millis(0))
        );
    }

    #[test]
    fn parse_set_volume() {
        let c = correction("set_volume", b"50", 1);
        assert_eq!(
            parse_correction(&c).unwrap(),
            PlaybackCommand::SetVolume(50)
        );
    }

    #[test]
    fn parse_set_volume_accepts_bounds() {
        assert_eq!(
            parse_correction(&correction("set_volume", b"0", 1)).unwrap(),
            PlaybackCommand::SetVolume(0)
        );
        assert_eq!(
            parse_correction(&correction("set_volume", b"255", 1)).unwrap(),
            PlaybackCommand::SetVolume(255)
        );
    }

    #[test]
    fn parse_rejects_unknown_correction_type() {
        let e = parse_correction(&correction("jitter", b"", 1)).unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    #[test]
    fn parse_rejects_non_utf8_payload() {
        let c = correction("play", &[0xff, 0xfe], 1);
        let e = parse_correction(&c).unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    #[test]
    fn parse_rejects_malformed_play_position() {
        let e = parse_correction(&correction("play", b"not-a-number", 1))
            .unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    #[test]
    fn parse_rejects_negative_play_position() {
        let e = parse_correction(&correction("play", b"-1", 1)).unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    #[test]
    fn parse_rejects_malformed_pause_value() {
        let e =
            parse_correction(&correction("pause", b"maybe", 1)).unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    #[test]
    fn parse_rejects_malformed_seek_value() {
        let e = parse_correction(&correction("seek", b"soon", 1)).unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    #[test]
    fn parse_rejects_malformed_volume_value() {
        let e = parse_correction(&correction("set_volume", b"loud", 1))
            .unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    #[test]
    fn parse_trims_payload_whitespace() {
        let c = correction("play", b"  3\n", 1);
        assert_eq!(
            parse_correction(&c).unwrap(),
            PlaybackCommand::PlayPosition(3)
        );
    }

    // ===== error mapping tests =====

    #[test]
    fn ack_maps_to_permanent() {
        let e = playback_error_to_plugin_error(PlaybackError::Ack {
            code: 2,
            message: "Bad song index".to_string(),
        });
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    #[test]
    fn exhausted_maps_to_transient() {
        let e = playback_error_to_plugin_error(
            PlaybackError::ConnectionExhausted { attempts: 10 },
        );
        assert!(matches!(e, PluginError::Transient(_)));
    }

    #[test]
    fn protocol_maps_to_fatal() {
        let e = playback_error_to_plugin_error(PlaybackError::Protocol(
            "unexpected token".to_string(),
        ));
        assert!(e.is_fatal());
    }

    #[test]
    fn shutdown_maps_to_permanent() {
        let e = playback_error_to_plugin_error(PlaybackError::Shutdown);
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    // ===== integration tests (mock MPD) =====

    #[tokio::test]
    async fn take_custody_spawns_supervisor_and_emits_toml_initial_report() {
        let (mut p, _mock) = loaded_plugin_with_mock(vec![
            ConnBehaviour::Standard,
            ConnBehaviour::HoldAfterWelcome,
        ])
        .await;

        let reporter = Arc::new(CapturingReporter::default());
        let reporter_dyn: Arc<dyn CustodyStateReporter> = reporter.clone();

        let handle =
            p.take_custody(assignment(reporter_dyn, 42)).await.unwrap();
        assert_eq!(handle.id, "custody-42");
        assert_eq!(p.active_custody_count(), 1);
        assert_eq!(p.custodies_taken(), 1);

        assert_eq!(reporter.count(), 1);
        let payload = reporter.last_payload().unwrap();
        let text = String::from_utf8(payload).unwrap();
        assert!(
            text.contains("state = \"stopped\""),
            "expected TOML state field in initial report: {text:?}"
        );

        p.release_custody(handle).await.unwrap();
    }

    #[tokio::test]
    async fn take_custody_maps_exhausted_to_transient() {
        let (endpoint, _mock) = spawn_unresponsive_mock().await;
        let mut p =
            MpdPlaybackPlugin::with_endpoint(endpoint, short_timeouts());
        p.loaded = true;
        p.subject_emitter = Some(SubjectEmitter::null());

        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());
        let e = p.take_custody(assignment(reporter, 1)).await.unwrap_err();
        assert!(
            matches!(e, PluginError::Transient(_)),
            "expected Transient, got {e:?}"
        );
        assert_eq!(p.active_custody_count(), 0);
        assert_eq!(p.custodies_taken(), 0);
    }

    #[tokio::test]
    async fn course_correct_play_reaches_supervisor() {
        let (mut p, _mock) = loaded_plugin_with_mock(vec![
            ConnBehaviour::Standard,
            ConnBehaviour::HoldAfterWelcome,
        ])
        .await;

        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());
        let handle = p.take_custody(assignment(reporter, 7)).await.unwrap();

        p.course_correct(&handle, correction("play", b"", 99))
            .await
            .unwrap();
        assert_eq!(p.corrections_dispatched(), 1);

        p.release_custody(handle).await.unwrap();
    }

    #[tokio::test]
    async fn course_correct_maps_ack_to_permanent() {
        let (mut p, _mock) = loaded_plugin_with_mock(vec![
            ConnBehaviour::AckOnNth {
                nth: 3,
                code: 2,
                message: "Bad song index".to_string(),
            },
            ConnBehaviour::HoldAfterWelcome,
        ])
        .await;

        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());
        let handle = p.take_custody(assignment(reporter, 11)).await.unwrap();

        let e = p
            .course_correct(&handle, correction("play", b"", 1))
            .await
            .unwrap_err();
        assert!(
            matches!(e, PluginError::Permanent(_)),
            "expected Permanent from Ack, got {e:?}"
        );
        assert_eq!(p.corrections_dispatched(), 1);
        p.release_custody(handle).await.unwrap();
    }

    #[tokio::test]
    async fn release_custody_shuts_down_supervisor_and_removes_from_tracking() {
        let (mut p, _mock) = loaded_plugin_with_mock(vec![
            ConnBehaviour::Standard,
            ConnBehaviour::HoldAfterWelcome,
        ])
        .await;

        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());
        let handle = p.take_custody(assignment(reporter, 5)).await.unwrap();
        assert_eq!(p.active_custody_count(), 1);

        p.release_custody(handle).await.unwrap();
        assert_eq!(p.active_custody_count(), 0);
        assert_eq!(p.custodies_taken(), 1);
    }

    #[tokio::test]
    async fn unload_drains_active_custodies() {
        let (mut p, _mock) = loaded_plugin_with_mock(vec![
            ConnBehaviour::Standard,
            ConnBehaviour::HoldAfterWelcome,
            ConnBehaviour::Standard,
            ConnBehaviour::HoldAfterWelcome,
        ])
        .await;

        let reporter_a: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());
        let reporter_b: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());

        let _h1 = p.take_custody(assignment(reporter_a, 100)).await.unwrap();
        let _h2 = p.take_custody(assignment(reporter_b, 200)).await.unwrap();
        assert_eq!(p.active_custody_count(), 2);

        p.unload().await.unwrap();
        assert_eq!(p.active_custody_count(), 0);
        assert!(matches!(
            p.health_check().await.status,
            HealthStatus::Unhealthy
        ));
    }

    // ===== Phase 3.4: subject emission through the warden =====

    #[tokio::test]
    async fn take_custody_rejects_when_subject_emitter_not_initialised() {
        // Simulate the path where loaded=true has been set
        // manually (e.g. by pre-3.4 legacy test code) but
        // subject_emitter was not populated. Defense-in-depth
        // gate should catch this.
        let (endpoint, _mock) = spawn_mock_mpd(vec![
            ConnBehaviour::Standard,
            ConnBehaviour::HoldAfterWelcome,
        ])
        .await;
        let mut p =
            MpdPlaybackPlugin::with_endpoint(endpoint, short_timeouts());
        p.loaded = true;
        // subject_emitter is intentionally left as None.

        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());
        let e = p.take_custody(assignment(reporter, 1)).await.unwrap_err();
        match e {
            PluginError::Permanent(msg) => {
                assert!(
                    msg.contains("subject emitter"),
                    "error should mention the emitter gate, got {msg:?}"
                );
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
        assert_eq!(p.active_custody_count(), 0);
    }

    #[tokio::test]
    async fn take_custody_with_populated_song_emits_subjects() {
        let (endpoint, mock_task) = spawn_mock_mpd(vec![
            ConnBehaviour::StandardWithSong {
                file: "library/a/b/01.flac".to_string(),
                title: "Track".to_string(),
                artist: "Artist".to_string(),
                album: "Album".to_string(),
            },
            ConnBehaviour::HoldAfterWelcome,
        ])
        .await;
        let mut p =
            MpdPlaybackPlugin::with_endpoint(endpoint, short_timeouts());
        p.loaded = true;

        // Equip a capturing emitter so this test can verify the
        // announcement actually reached the SDK surface.
        let (subjects, relations, emitter) = capturing_emitter();
        p.subject_emitter = Some(emitter);

        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());
        let handle = p.take_custody(assignment(reporter, 77)).await.unwrap();

        // Initial emission from spawn's emit_initial_report.
        assert_eq!(subjects.count(), 2, "track + album at take-custody");
        assert_eq!(relations.count(), 1, "album_of at take-custody");

        let track = subjects.at(0).unwrap();
        assert_eq!(track.subject_type, "track");
        assert_eq!(track.addressings[0].scheme, "mpd-path");
        assert_eq!(track.addressings[0].value, "library/a/b/01.flac");

        let album = subjects.at(1).unwrap();
        assert_eq!(album.subject_type, "album");
        assert_eq!(album.addressings[0].scheme, "mpd-album");
        assert_eq!(album.addressings[0].value, "Artist|Album");

        let rel = relations.at(0).unwrap();
        assert_eq!(rel.predicate, "album_of");
        assert_eq!(rel.source.value, "library/a/b/01.flac");
        assert_eq!(rel.target.value, "Artist|Album");

        p.release_custody(handle).await.unwrap();
        drop(mock_task);
    }

    // ===== F3 fragment-writer + reactor tests =====

    use super::test_support_routing::{
        default_alsa_write_endpoint, route_change as source_route_change,
        StubSourceAudioRouting,
    };
    use crate::mpd_restart::{FailingRestarter, NoOpRestarter};
    use evo_plugin_sdk::audio::{
        AudioFormat as F3AudioFormat, PcmCodec as F3PcmCodec,
    };
    use evo_plugin_sdk::contract::audio_routing::{
        AudioRouting as F3AudioRouting, EndpointKind as F3EndpointKind,
        WriteEndpoint as F3WriteEndpoint,
    };

    /// Wait until the reactor's refresh counter advances from
    /// `prior` to at least `prior + advances`. Bounded so a
    /// wedged reactor does not hang CI.
    async fn wait_for_refresh(
        plugin: &MpdPlaybackPlugin,
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

    /// Wait until the worker status channel reports a state
    /// matching the predicate.
    async fn wait_for_fragment_status<F>(
        rx: &mut watch::Receiver<FragmentWorkerStatus>,
        deadline_ms: u64,
        mut predicate: F,
    ) -> FragmentWorkerStatus
    where
        F: FnMut(&FragmentWorkerStatus) -> bool,
    {
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(deadline_ms);
        if predicate(&rx.borrow()) {
            return rx.borrow().clone();
        }
        loop {
            if std::time::Instant::now() >= deadline {
                panic!(
                    "fragment worker did not reach the expected status within \
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

    /// Convenience: build a loaded plugin wired to a fresh
    /// `StubSourceAudioRouting`, a tempdir-backed fragment
    /// path, and a `NoOpRestarter`. Returns the plugin, the
    /// stub (for publishing endpoints / firing route changes),
    /// the restarter (for asserting call count), and the
    /// tempdir + fragment path (for inspecting written
    /// content). Caller drives `spawn_reactor` +
    /// `spawn_fragment_worker` directly so each test can
    /// observe intermediate states.
    fn fragment_test_plugin() -> (
        MpdPlaybackPlugin,
        Arc<StubSourceAudioRouting>,
        Arc<NoOpRestarter>,
        tempfile::TempDir,
        PathBuf,
    ) {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let fragment_path = tempdir.path().join("mpd.conf");
        let restarter = Arc::new(NoOpRestarter::new());
        let mut p = MpdPlaybackPlugin::new()
            .with_restarter(Arc::clone(&restarter) as Arc<dyn MpdRestarter>)
            .with_fragment_path(fragment_path.clone());
        let stub = Arc::new(StubSourceAudioRouting::new());
        p.install_routing(Some(Arc::clone(&stub) as Arc<dyn F3AudioRouting>))
            .unwrap();
        (p, stub, restarter, tempdir, fragment_path)
    }

    #[tokio::test]
    async fn spawn_reactor_registers_route_change_callback() {
        let (mut p, stub, _restarter, _td, _fp) = fragment_test_plugin();
        assert!(!stub.has_route_change_callback());
        p.spawn_reactor().await.unwrap();
        assert!(stub.has_route_change_callback());
        p.stop_reactor().await;
        assert!(!stub.has_route_change_callback());
    }

    #[tokio::test]
    async fn spawn_reactor_publishes_initial_endpoint_when_topology_present() {
        let (mut p, stub, _restarter, _td, _fp) = fragment_test_plugin();
        stub.set_write_endpoint(default_alsa_write_endpoint());
        p.spawn_reactor().await.unwrap();

        let rx = p.subscribe_endpoints().expect("reactor running");
        assert_eq!(rx.borrow().clone(), Some(default_alsa_write_endpoint()));
        p.stop_reactor().await;
    }

    #[tokio::test]
    async fn spawn_reactor_publishes_none_when_topology_absent() {
        let (mut p, _stub, _restarter, _td, _fp) = fragment_test_plugin();
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
        let (mut p, stub, _restarter, _td, _fp) = fragment_test_plugin();
        stub.set_write_endpoint(default_alsa_write_endpoint());
        p.spawn_reactor().await.unwrap();

        let mut rx = p.subscribe_endpoints().expect("reactor running");
        let prior_refresh = p.refresh_count();

        // Publish a new topology at a different format and
        // fire the route change. The reactor must refetch.
        let new_format = F3AudioFormat::Pcm {
            codec: F3PcmCodec::PcmS24Le,
            rate_hz: 192_000,
            channels: 2,
        };
        let new_ep = F3WriteEndpoint {
            kind: F3EndpointKind::AlsaPcm,
            path: PathBuf::from("hw:3,0"),
            format: new_format.clone(),
            buffer_frames: 1024,
        };
        stub.set_write_endpoint(new_ep.clone());
        assert!(stub.fire_route_change(source_route_change(new_format)));

        wait_for_refresh(&p, prior_refresh, 1).await;
        rx.changed().await.expect("watch channel still alive");
        assert_eq!(rx.borrow().clone(), Some(new_ep));

        p.stop_reactor().await;
    }

    #[tokio::test]
    async fn fragment_worker_renders_and_restarts_on_initial_endpoint() {
        let (mut p, stub, restarter, _td, fragment_path) =
            fragment_test_plugin();
        stub.set_write_endpoint(default_alsa_write_endpoint());
        p.spawn_reactor().await.unwrap();
        p.spawn_fragment_worker().await.unwrap();

        let mut status_rx =
            p.subscribe_fragment_status().expect("worker running");
        wait_for_fragment_status(&mut status_rx, 1000, |s| {
            matches!(s, FragmentWorkerStatus::Restarted { .. })
        })
        .await;

        // Restarter was invoked once for the initial
        // endpoint.
        assert_eq!(restarter.call_count(), 1);

        // Fragment file is on disk with the expected
        // content.
        let body = tokio::fs::read_to_string(&fragment_path).await.unwrap();
        assert!(body.contains("device          \"hw:2,0\""));
        assert!(body.contains("format          \"44100:16:2\""));
        assert!(body.contains("mixer_type      \"software\""));

        p.stop_fragment_worker().await;
        p.stop_reactor().await;
    }

    #[tokio::test]
    async fn fragment_worker_rewrites_and_restarts_on_route_change() {
        let (mut p, stub, restarter, _td, fragment_path) =
            fragment_test_plugin();
        stub.set_write_endpoint(default_alsa_write_endpoint());
        p.spawn_reactor().await.unwrap();
        p.spawn_fragment_worker().await.unwrap();

        let mut status_rx =
            p.subscribe_fragment_status().expect("worker running");
        wait_for_fragment_status(&mut status_rx, 1000, |s| {
            matches!(s, FragmentWorkerStatus::Restarted { .. })
        })
        .await;
        let initial_restart_count = restarter.call_count();
        assert!(initial_restart_count >= 1);

        // Publish a new endpoint and fire route change. The
        // worker must re-render, re-write, and re-restart.
        let new_format = F3AudioFormat::Pcm {
            codec: F3PcmCodec::PcmS24Le,
            rate_hz: 96_000,
            channels: 2,
        };
        let new_ep = F3WriteEndpoint {
            kind: F3EndpointKind::AlsaPcm,
            path: PathBuf::from("hw:4,0"),
            format: new_format.clone(),
            buffer_frames: 1024,
        };
        stub.set_write_endpoint(new_ep.clone());
        let prior_refresh = p.refresh_count();
        assert!(stub.fire_route_change(source_route_change(new_format)));
        wait_for_refresh(&p, prior_refresh, 1).await;

        wait_for_fragment_status(&mut status_rx, 1000, |s| match s {
            FragmentWorkerStatus::Restarted { endpoint } => {
                endpoint.path == std::path::Path::new("hw:4,0")
            }
            _ => false,
        })
        .await;

        // Restarter was invoked again for the new endpoint.
        assert!(restarter.call_count() > initial_restart_count);

        // Fragment file now describes the new device.
        let body = tokio::fs::read_to_string(&fragment_path).await.unwrap();
        assert!(body.contains("device          \"hw:4,0\""));
        assert!(body.contains("format          \"96000:24:2\""));

        p.stop_fragment_worker().await;
        p.stop_reactor().await;
    }

    #[tokio::test]
    async fn fragment_worker_publishes_failed_when_restart_fails() {
        let tempdir = tempfile::tempdir().unwrap();
        let fragment_path = tempdir.path().join("mpd.conf");
        let restarter = Arc::new(FailingRestarter::new("test failure"));
        let mut p = MpdPlaybackPlugin::new()
            .with_restarter(Arc::clone(&restarter) as Arc<dyn MpdRestarter>)
            .with_fragment_path(fragment_path.clone());
        let stub = Arc::new(StubSourceAudioRouting::new());
        stub.set_write_endpoint(default_alsa_write_endpoint());
        p.install_routing(Some(Arc::clone(&stub) as Arc<dyn F3AudioRouting>))
            .unwrap();
        p.spawn_reactor().await.unwrap();
        p.spawn_fragment_worker().await.unwrap();

        let mut status_rx =
            p.subscribe_fragment_status().expect("worker running");
        let status = wait_for_fragment_status(&mut status_rx, 1000, |s| {
            matches!(s, FragmentWorkerStatus::Failed { .. })
        })
        .await;
        match status {
            FragmentWorkerStatus::Failed { reason } => {
                assert!(
                    reason.contains("test failure"),
                    "expected restarter reason to propagate, got {reason}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }

        // Fragment file IS on disk (render + write
        // succeeded); only the restart failed. The previous
        // state is undisturbed because there was no
        // previous state.
        assert!(fragment_path.exists());

        p.stop_fragment_worker().await;
        p.stop_reactor().await;
    }

    #[tokio::test]
    async fn fragment_worker_failed_when_endpoint_kind_unsupported() {
        let tempdir = tempfile::tempdir().unwrap();
        let fragment_path = tempdir.path().join("mpd.conf");
        let restarter = Arc::new(NoOpRestarter::new());
        let mut p = MpdPlaybackPlugin::new()
            .with_restarter(Arc::clone(&restarter) as Arc<dyn MpdRestarter>)
            .with_fragment_path(fragment_path.clone());
        let stub = Arc::new(StubSourceAudioRouting::new());
        // Publish a NamedPipe endpoint — out of scope for
        // F3's MPD audio_output fragment renderer.
        let unsupported = F3WriteEndpoint {
            kind: F3EndpointKind::NamedPipe,
            path: PathBuf::from("/tmp/evo.fifo"),
            format: F3AudioFormat::Pcm {
                codec: F3PcmCodec::PcmS16Le,
                rate_hz: 44_100,
                channels: 2,
            },
            buffer_frames: 1024,
        };
        stub.set_write_endpoint(unsupported);
        p.install_routing(Some(Arc::clone(&stub) as Arc<dyn F3AudioRouting>))
            .unwrap();
        p.spawn_reactor().await.unwrap();
        p.spawn_fragment_worker().await.unwrap();

        let mut status_rx =
            p.subscribe_fragment_status().expect("worker running");
        let status = wait_for_fragment_status(&mut status_rx, 1000, |s| {
            matches!(s, FragmentWorkerStatus::Failed { .. })
        })
        .await;
        match status {
            FragmentWorkerStatus::Failed { reason } => {
                assert!(
                    reason.contains("render"),
                    "expected render failure in reason, got {reason}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }

        // Restarter was NOT invoked — render failed before
        // the restart leg.
        assert_eq!(restarter.call_count(), 0);
        // No fragment file was written (atomic-write was
        // never reached).
        assert!(!fragment_path.exists());

        p.stop_fragment_worker().await;
        p.stop_reactor().await;
    }

    #[tokio::test]
    async fn unload_terminates_reactor_and_worker_promptly() {
        let (mut p, stub, _restarter, _td, _fp) = fragment_test_plugin();
        stub.set_write_endpoint(default_alsa_write_endpoint());
        // Drive through Plugin::unload to verify the full
        // teardown path — same shape composition.alsa
        // exercises.
        p.spawn_reactor().await.unwrap();
        p.spawn_fragment_worker().await.unwrap();
        p.loaded = true;
        // Equip a null subject emitter to satisfy any
        // future invariants the unload path may add; not
        // strictly required for this teardown shape today.
        p.subject_emitter = Some(SubjectEmitter::null());

        let started = std::time::Instant::now();
        p.unload().await.unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(300),
            "unload must drain reactor + worker quickly; took {elapsed:?}"
        );
        assert!(p.reactor.is_none());
        assert!(p.fragment_worker.is_none());
        assert!(p.audio_routing.is_none());
        assert!(
            !stub.has_route_change_callback(),
            "unload must release the route-change callback"
        );
    }

    #[tokio::test]
    async fn take_custody_with_empty_song_emits_no_subjects() {
        let (mut p, _mock) = loaded_plugin_with_mock(vec![
            ConnBehaviour::Standard,
            ConnBehaviour::HoldAfterWelcome,
        ])
        .await;

        // Replace the null emitter the helper installed with a
        // capturing one so we can verify nothing was announced.
        let (subjects, relations, emitter) = capturing_emitter();
        p.subject_emitter = Some(emitter);

        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());
        let handle = p.take_custody(assignment(reporter, 9)).await.unwrap();

        // Standard mock returns empty currentsong; no subjects.
        assert_eq!(subjects.count(), 0);
        assert_eq!(relations.count(), 0);

        p.release_custody(handle).await.unwrap();
    }

    // ===== F4 source-verb surface tests =====

    use crate::playback_supervisor::test_mock::ConnBehaviour as F4Conn;
    use serde_json::{json, Value};

    /// Build a respondent request for the supplied verb +
    /// JSON payload. Mirrors how the framework's source-verb
    /// dispatcher constructs the wire envelope.
    fn source_request(verb: &str, payload: Value) -> Request {
        Request {
            request_type: verb.to_string(),
            payload: payload.to_string().into_bytes(),
            correlation_id: 1,
            deadline: None,
            instance_id: None,
        }
    }

    /// Spawn a mock-MPD-backed plugin with one supervisor
    /// custody equipped. Used by every F4 source-verb test
    /// that needs to dispatch through the active custody.
    async fn loaded_plugin_with_active_custody(
        behaviours: Vec<F4Conn>,
    ) -> (
        MpdPlaybackPlugin,
        CustodyHandle,
        tokio::task::JoinHandle<()>,
    ) {
        let (mut p, mock) = loaded_plugin_with_mock(behaviours).await;
        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());
        let handle = p.take_custody(assignment(reporter, 1)).await.unwrap();
        (p, handle, mock)
    }

    #[tokio::test]
    async fn play_now_dispatches_load_and_play() {
        let (mut p, handle, _mock) = loaded_plugin_with_active_custody(vec![
            F4Conn::Standard,
            F4Conn::HoldAfterWelcome,
        ])
        .await;

        let req = source_request(
            "play_now",
            json!({ "v": 1, "uri": "mpd-path:Music/A/01.flac" }),
        );
        let resp = p.handle_request(&req).await.unwrap();
        let body: Value = serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(body["status"], "ok");
        assert_eq!(body["v"], 1);
        assert_eq!(body["uri"], "mpd-path:Music/A/01.flac");

        p.release_custody(handle).await.unwrap();
    }

    #[tokio::test]
    async fn play_now_refuses_bad_payload_version() {
        let (mut p, handle, _mock) = loaded_plugin_with_active_custody(vec![
            F4Conn::Standard,
            F4Conn::HoldAfterWelcome,
        ])
        .await;

        let req =
            source_request("play_now", json!({ "v": 99, "uri": "mpd-path:x" }));
        let err = p.handle_request(&req).await.unwrap_err();
        match err {
            PluginError::Permanent(msg) => {
                assert!(msg.contains("payload version 99 unsupported"));
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
        p.release_custody(handle).await.unwrap();
    }

    #[tokio::test]
    async fn play_now_refuses_wrong_scheme() {
        let (mut p, handle, _mock) = loaded_plugin_with_active_custody(vec![
            F4Conn::Standard,
            F4Conn::HoldAfterWelcome,
        ])
        .await;

        let req = source_request(
            "play_now",
            json!({ "v": 1, "uri": "file:/Music/x.flac" }),
        );
        let err = p.handle_request(&req).await.unwrap_err();
        assert!(matches!(err, PluginError::Permanent(_)));
        p.release_custody(handle).await.unwrap();
    }

    #[tokio::test]
    async fn play_refuses_when_no_active_custody() {
        let (mut p, _mock) = loaded_plugin_with_mock(vec![
            F4Conn::Standard,
            F4Conn::HoldAfterWelcome,
        ])
        .await;
        // Intentionally skip take_custody so the source-verb
        // dispatcher has nothing to route into.

        let req = source_request("play", json!({ "v": 1 }));
        let err = p.handle_request(&req).await.unwrap_err();
        match err {
            PluginError::Permanent(msg) => {
                assert!(msg.contains("no active custody"));
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn play_now_accepts_legacy_payload_without_version_field() {
        // Backwards-compatibility regression guard. The
        // framework's source-verb dispatcher (before the
        // matching framework update) emitted `{ "uri": ... }`
        // without a `v` field. The plugin's
        // default_payload_version() lets such payloads parse
        // with v = PAYLOAD_VERSION so older framework builds
        // and on-disk plan files keep working.
        let (mut p, handle, _mock) = loaded_plugin_with_active_custody(vec![
            F4Conn::Standard,
            F4Conn::HoldAfterWelcome,
        ])
        .await;

        let req = source_request(
            "play_now",
            json!({ "uri": "mpd-path:Music/A/01.flac" }),
        );
        let resp = p.handle_request(&req).await.unwrap();
        let body: Value = serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(body["status"], "ok");
        assert_eq!(body["uri"], "mpd-path:Music/A/01.flac");
        p.release_custody(handle).await.unwrap();
    }

    #[tokio::test]
    async fn bare_envelope_verbs_accept_payload_without_version_field() {
        // Same backwards-compatibility shape as
        // play_now_accepts_legacy_payload_without_version_field
        // but for the bare-envelope verbs. The framework's
        // dispatcher emits `{}` for stop/pause/resume/next/
        // previous; without the default, every one would
        // refuse with "missing field `v`".
        for verb in ["play", "pause", "resume", "stop", "next", "previous"] {
            let (mut p, handle, _mock) =
                loaded_plugin_with_active_custody(vec![
                    F4Conn::Standard,
                    F4Conn::HoldAfterWelcome,
                ])
                .await;

            let req = source_request(verb, json!({}));
            let resp = p.handle_request(&req).await.unwrap_or_else(|e| {
                panic!("verb {verb} failed unexpectedly: {e:?}")
            });
            let body: Value = serde_json::from_slice(&resp.payload).unwrap();
            assert_eq!(body["status"], "ok", "verb {verb}");
            p.release_custody(handle).await.unwrap();
        }
    }

    /// Verify every bare-envelope verb round-trips: parses
    /// the payload, dispatches through the supervisor,
    /// returns the typed `SimpleResponse`. Exercises the
    /// shared `handle_simple_command` path against each of
    /// the six verbs.
    #[tokio::test]
    async fn bare_envelope_verbs_dispatch_and_respond() {
        for verb in ["play", "pause", "resume", "stop", "next", "previous"] {
            let (mut p, handle, _mock) =
                loaded_plugin_with_active_custody(vec![
                    F4Conn::Standard,
                    F4Conn::HoldAfterWelcome,
                ])
                .await;

            let req = source_request(verb, json!({ "v": 1 }));
            let resp = p.handle_request(&req).await.unwrap_or_else(|e| {
                panic!("verb {verb} failed unexpectedly: {e:?}")
            });
            let body: Value = serde_json::from_slice(&resp.payload).unwrap();
            assert_eq!(body["status"], "ok", "verb {verb}");
            assert_eq!(body["v"], 1, "verb {verb}");

            p.release_custody(handle).await.unwrap();
        }
    }

    #[tokio::test]
    async fn bare_envelope_verbs_refuse_bad_version() {
        for verb in ["play", "pause", "resume", "stop", "next", "previous"] {
            let (mut p, handle, _mock) =
                loaded_plugin_with_active_custody(vec![
                    F4Conn::Standard,
                    F4Conn::HoldAfterWelcome,
                ])
                .await;

            let req = source_request(verb, json!({ "v": 99 }));
            let err = p.handle_request(&req).await.unwrap_err();
            assert!(
                matches!(err, PluginError::Permanent(_)),
                "verb {verb} expected Permanent, got {err:?}"
            );
            p.release_custody(handle).await.unwrap();
        }
    }

    #[tokio::test]
    async fn seek_dispatches_with_position_ms() {
        let (mut p, handle, _mock) = loaded_plugin_with_active_custody(vec![
            F4Conn::Standard,
            F4Conn::HoldAfterWelcome,
        ])
        .await;

        let req =
            source_request("seek", json!({ "v": 1, "position_ms": 1250 }));
        let resp = p.handle_request(&req).await.unwrap();
        let body: Value = serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(body["status"], "ok");
        p.release_custody(handle).await.unwrap();
    }

    #[tokio::test]
    async fn seek_refuses_missing_position() {
        let (mut p, handle, _mock) = loaded_plugin_with_active_custody(vec![
            F4Conn::Standard,
            F4Conn::HoldAfterWelcome,
        ])
        .await;

        let req = source_request("seek", json!({ "v": 1 }));
        let err = p.handle_request(&req).await.unwrap_err();
        assert!(matches!(err, PluginError::Permanent(_)));
        p.release_custody(handle).await.unwrap();
    }

    #[tokio::test]
    async fn set_volume_dispatches_with_clamped_byte() {
        let (mut p, handle, _mock) = loaded_plugin_with_active_custody(vec![
            F4Conn::Standard,
            F4Conn::HoldAfterWelcome,
        ])
        .await;

        let req = source_request("set_volume", json!({ "v": 1, "volume": 50 }));
        let resp = p.handle_request(&req).await.unwrap();
        let body: Value = serde_json::from_slice(&resp.payload).unwrap();
        assert_eq!(body["status"], "ok");
        p.release_custody(handle).await.unwrap();
    }

    #[tokio::test]
    async fn set_volume_refuses_out_of_range() {
        let (mut p, handle, _mock) = loaded_plugin_with_active_custody(vec![
            F4Conn::Standard,
            F4Conn::HoldAfterWelcome,
        ])
        .await;

        // u8 max is 255; 256 doesn't fit -> serde
        // deserialization error -> Permanent.
        let req =
            source_request("set_volume", json!({ "v": 1, "volume": 256 }));
        let err = p.handle_request(&req).await.unwrap_err();
        assert!(matches!(err, PluginError::Permanent(_)));
        p.release_custody(handle).await.unwrap();
    }

    #[tokio::test]
    async fn unknown_verb_refused() {
        let (mut p, handle, _mock) = loaded_plugin_with_active_custody(vec![
            F4Conn::Standard,
            F4Conn::HoldAfterWelcome,
        ])
        .await;

        let req = source_request("jitter", json!({ "v": 1 }));
        let err = p.handle_request(&req).await.unwrap_err();
        match err {
            PluginError::Permanent(msg) => {
                assert!(msg.contains("unknown request type"));
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
        p.release_custody(handle).await.unwrap();
    }

    #[tokio::test]
    async fn requests_handled_counter_advances_per_verb() {
        let (mut p, handle, _mock) = loaded_plugin_with_active_custody(vec![
            F4Conn::Standard,
            F4Conn::HoldAfterWelcome,
        ])
        .await;

        assert_eq!(p.requests_handled(), 0);
        let req = source_request("play", json!({ "v": 1 }));
        p.handle_request(&req).await.unwrap();
        assert_eq!(p.requests_handled(), 1);
        let req = source_request("pause", json!({ "v": 1 }));
        p.handle_request(&req).await.unwrap();
        assert_eq!(p.requests_handled(), 2);
        p.release_custody(handle).await.unwrap();
    }
}
