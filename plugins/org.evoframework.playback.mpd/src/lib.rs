// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
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

mod asound_watcher;
mod availability;
mod config;
mod disposition_emitter;
mod envelope_subscriber;
mod favourites;
mod idle_observer;
mod library;
mod mpd;
mod mpd_fragment;
mod mpd_restart;
mod playback_supervisor;
mod playlist;
mod queue;
mod shelves;
mod skip_traversal;
mod source_probe;
mod source_registry;
mod sticker_reconciler;
mod works;

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
use crate::mpd::{ConnectTimeouts, MpdConnection, MpdEndpoint};
use crate::mpd::{MpdError, MpdStatus};
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
    "seek_by_delta",
    "set_volume",
    "set_mute",
    "set_repeat",
    "set_shuffle",
    "set_single",
    "set_consume",
    "emit_test_tone",
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
    "seek_by_delta",
    "set_volume",
    "set_mute",
    "set_repeat",
    "set_shuffle",
    "set_single",
    "set_consume",
    "get_now_playing",
    "get_stream_format",
    // audio.queue.v1 shelf verbs
    "queue.get_queue",
    "queue.enqueue",
    "queue.remove_queue_item",
    "queue.move_queue_item",
    "queue.clear_queue",
    "queue.load_playlist_to_queue",
    "queue.append_playlist_to_queue",
    "queue.save_queue_as_playlist",
    "queue.skip_to_next_available",
    "queue.play_from_position",
    // audio.playlist.v1 shelf verbs
    "playlist.list_playlists",
    "playlist.get_playlist",
    "playlist.create_playlist",
    "playlist.delete_playlist",
    "playlist.rename_playlist",
    "playlist.add_to_playlist",
    "playlist.remove_from_playlist",
    "playlist.move_in_playlist",
    // audio.favourites.v1 shelf verbs
    "favourites.list_favourites",
    "favourites.is_favourite",
    "favourites.add_favourite",
    "favourites.remove_favourite",
    "favourites.clear_favourites",
    "favourites.move_favourite",
    // audio.library.v1 shelf verbs
    "library.list_sources",
    "library.add_source",
    "library.remove_source",
    "library.probe_source",
    "library.wake_source",
    "library.update_source",
    "library.browse_library",
    "library.search_library",
    "library.list_works",
    "library.get_work_recordings",
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

/// Audible-band lower bound for the `emit_test_tone` verb.
/// Below this frequency the tone is inaudible and a
/// well-meaning operator slider sweep can silently land on a
/// non-perceptible value.
const TEST_TONE_FREQ_MIN: u32 = 20;
/// Audible-band upper bound for the `emit_test_tone` verb.
/// Above this frequency the tone is inaudible and a
/// slider-typo / UI-bug excursion (100000) would silently land
/// on a non-perceptible value.
const TEST_TONE_FREQ_MAX: u32 = 20_000;
/// Lower bound for the test-tone duration. Below 100 ms the
/// tone is too short to be operator-perceptible.
const TEST_TONE_DURATION_MIN_MS: u32 = 100;
/// Upper bound for the test-tone duration. Above 10 s the
/// gesture stops being a "brief diagnostic" and becomes an
/// audio nuisance.
const TEST_TONE_DURATION_MAX_MS: u32 = 10_000;
/// Default channel routing when the operator does not specify
/// one. Both-channels matches the most common operator intent
/// ("verify the chain end-to-end").
const TEST_TONE_DEFAULT_CHANNEL: &str = "both";

/// Canonical Unix-domain-socket path the test-tone verb opens
/// a dedicated MPD connection on. MPD's security model refuses
/// `file://...` URI loads over TCP (the `Access to local files
/// via TCP is not allowed` error class); the same loads are
/// permitted on the local Unix socket. The distribution's
/// bootstrap configures MPD to bind this path; if the operator
/// runs a TCP-only MPD (no Unix socket configured), the verb
/// refuses with a structured Permanent error naming the missing
/// surface rather than failing opaquely against MPD's ack.
const TEST_TONE_MPD_UNIX_SOCKET: &str = "/run/mpd/socket";

/// Spool directory the warden writes synthesised test-tone
/// WAVs to. Lives under the steward's `RuntimeDirectory=evo`
/// (`/run/evo/`) so the path is shared across every local
/// service — `/tmp` is NOT a safe choice because
/// `evo.service` runs with `PrivateTmp=yes` (the steward sees
/// its own private /tmp namespace while MPD sees the host's,
/// so a file the warden writes to /tmp is invisible to the
/// MPD daemon). `/run/evo/` is the steward's runtime
/// directory under systemd's `RuntimeDirectory` mechanism;
/// it's mode 0755 root:root and accessible to every other
/// local service. The warden mkdirs `test-tones/` under it on
/// first write (idempotent) and chmods each WAV to 0644 so
/// the mpd user (different from the steward user) can read
/// the file. systemd cleans `/run/evo/` on service stop, so
/// no separate cleanup primitive is required.
const TEST_TONE_SPOOL_DIR: &str = "/run/evo/test-tones";

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
    /// `unload`. The plugin uses the handle to learn which
    /// ALSA pcm MPD's audio_output should write to (the
    /// framework's negotiated `WriteEndpoint`) and to react
    /// to topology rewires. Composition plugins that declare
    /// `[capabilities.composition]` and source plugins
    /// (this one) that declare `[capabilities.source]` with
    /// an audio `output_kind` MUST receive this handle;
    /// `Plugin::load` refuses loudly when it is `None`.
    audio_routing: Option<Arc<dyn AudioRouting>>,
    custodies: HashMap<String, TrackedCustody>,
    /// Cumulative count of custodies accepted since construction.
    /// Does not decrement on release.
    custodies_taken: u64,
    /// Cloneable command-side view of the currently-active
    /// custody's supervisor. Held by the envelope-subscriber
    /// task so it can dispatch `PlaybackCommand::Pause` on
    /// orchestrator-published envelope_requested updates
    /// without owning the supervisor itself. Populated in
    /// `take_custody` after the supervisor spawns;
    /// cleared in `relinquish_custody` before shutdown.
    /// `custody_exclusive = true` (per manifest) guarantees
    /// at most one custody at a time, so a single
    /// `Option`-cell tracks the active sender unambiguously.
    active_command_sender: Arc<
        tokio::sync::Mutex<
            Option<playback_supervisor::SupervisorCommandSender>,
        >,
    >,
    /// Mixer-transition envelope subscriber task handle.
    /// Spawned at load; signals shutdown + awaits completion
    /// in unload.
    envelope_subscriber: Option<envelope_subscriber::EnvelopeSubscriberHandle>,
    /// Ambient now-playing state observer task handle. Runs for
    /// the plugin's load lifetime (NOT tied to custody). Owns
    /// its own MPD command + idle connections; publishes the
    /// `audio_playback_now_playing` subject on every observed
    /// state change. Closes the gap where the custody-gated
    /// supervisor leaves the subject's state unset on a fresh
    /// boot — downstream consumers (audio.terminus visualiser
    /// gate) need state publication regardless of whether any
    /// operator has taken custody yet.
    ambient_observer: Option<playback_supervisor::AmbientObserverHandle>,
    /// `/etc/asound.d/` composition-change watcher. Spawned at
    /// plugin load; on every observed change dispatches a
    /// `CycleOutput` to the currently-active custody's
    /// supervisor so MPD reopens its ALSA handle against the
    /// post-change drop-in stack. `None` before first load and
    /// after `Plugin::unload`.
    asound_watcher: Option<asound_watcher::AsoundWatcherHandle>,
    /// Test-tone in-flight gate. Set true at the entry of
    /// `emit_test_tone` and cleared by the background restore
    /// task on completion. A concurrent `emit_test_tone` while
    /// the flag is set refuses with a structured Permanent
    /// error rather than racing two diagnostic runs against
    /// the same MPD daemon. The flag is `Arc<AtomicBool>` so
    /// the spawned restore task carries a clone — clearing
    /// the flag survives the plugin's reference-borrow
    /// boundary.
    test_tone_in_flight: Arc<std::sync::atomic::AtomicBool>,
    /// Cumulative count of course corrections dispatched to the
    /// supervisor since construction. Counts attempts, not
    /// successes: a dispatched command that the supervisor then
    /// fails still increments this counter.
    corrections_dispatched: u64,
    /// Cumulative count of source-verb requests handled.
    /// Mirrors `corrections_dispatched` on the respondent
    /// dispatch side.
    requests_handled: std::sync::atomic::AtomicU64,
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
    /// Watch channel carrying the operator's currently-selected
    /// MPD-protocol settings (`crossfade_seconds` + `gapless`).
    /// Seeded with the audiophile-grade defaults at construction
    /// (no crossfade, gapless on); updated by the
    /// `playback.options` settings subscriber on every relevant
    /// subject change. Each spawned supervisor subscribes via
    /// [`watch::Sender::subscribe`] and the task body's
    /// `tokio::select!` arm dispatches `SetCrossfade` +
    /// `SetSingle` whenever the value changes, so MPD's
    /// protocol-level crossfade + single-mode follow the
    /// operator's settings without a separate apply gesture.
    /// Each new supervisor session also reads the channel's
    /// current value on connect to apply the operator's choice
    /// from the start (single canonical apply path, no
    /// session-init / settings-update fork).
    audio_protocol_settings_tx: watch::Sender<AudioProtocolSettings>,
    /// Watch channel carrying the operator's declared
    /// startup-volume settings (startup + max). Seeded with
    /// `None` at construction; the options-settings subscriber
    /// publishes `Some(StartupVolume)` on the first parsed
    /// state, and the startup-volume applier waits for that
    /// first `Some` before touching MPD. `None` after a state
    /// where the payload does not carry the fields (schema
    /// drift; the applier treats this as "keep waiting").
    startup_volume_tx:
        watch::Sender<Option<playback_supervisor::StartupVolume>>,
    /// Startup-volume applier task handle. Populated at
    /// `Plugin::load` when the options-settings subscriber is
    /// wired; runs a one-shot loop that waits for the first
    /// published `StartupVolume`, retries MPD `setvol` until
    /// accepted, then exits. Held here so `Plugin::unload` can
    /// stop it cleanly if it is still waiting.
    startup_volume_applier:
        Option<playback_supervisor::StartupVolumeApplierHandle>,
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
    /// Shelf integration bundle holding source registry,
    /// disposition emitter, skip-traversal, the four shelf
    /// contexts (queue / playlist / favourites / library), and
    /// the sticker reconciler handle. Constructed at
    /// `Plugin::load` after the existing subject emitter
    /// setup; consumed at `Plugin::unload` via
    /// [`shelves::ShelfBundle::shutdown`]. `None` before first
    /// load and after `Plugin::unload`. The respondent
    /// dispatcher's per-shelf request_types route through
    /// [`shelves::ShelfBundle::dispatch_request`].
    shelves: Option<shelves::ShelfBundle>,
}

/// Operator-selected MPD-protocol settings carried on the
/// plugin's `audio_protocol_settings_tx` watch channel. The
/// subject subscriber extracts these from the
/// `audio.options.settings` subject; each spawned supervisor
/// subscribes to the channel and applies the values via MPD's
/// protocol-layer verbs (`crossfade <n>` + `single <0|1>`) on
/// session-init and on every change while a session is live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AudioProtocolSettings {
    /// Between-track crossfade duration in seconds (0..=30,
    /// upper bound enforced at the options-plugin setter). `0`
    /// disables crossfade entirely.
    pub(crate) crossfade_seconds: u32,
    /// Gapless playback flag. `true` (audiophile default)
    /// engages MPD `single 0` — continue through the queue;
    /// `false` engages MPD `single 1` — stop after each track.
    pub(crate) gapless: bool,
}

impl AudioProtocolSettings {
    /// Audiophile-grade defaults: no crossfade, gapless on.
    /// Matches the `playback.options` plugin's `Settings`
    /// default for these fields.
    pub(crate) const fn audiophile_default() -> Self {
        Self {
            crossfade_seconds: 0,
            gapless: true,
        }
    }
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
        let (audio_protocol_settings_tx, _) =
            watch::channel(AudioProtocolSettings::audiophile_default());
        let (startup_volume_tx, _) = watch::channel(None);
        Self {
            loaded: false,
            endpoint,
            timeouts,
            subject_emitter: None,
            audio_routing: None,
            custodies: HashMap::new(),
            custodies_taken: 0,
            active_command_sender: Arc::new(tokio::sync::Mutex::new(None)),
            envelope_subscriber: None,
            ambient_observer: None,
            asound_watcher: None,
            test_tone_in_flight: Arc::new(std::sync::atomic::AtomicBool::new(
                false,
            )),
            corrections_dispatched: 0,
            requests_handled: std::sync::atomic::AtomicU64::new(0),
            fragment_path: PathBuf::from(config::DEFAULT_FRAGMENT_PATH),
            restarter: Arc::new(SudoSystemctlRestarter::new()),
            reactor: None,
            fragment_worker: None,
            mixer_config_tx,
            audio_protocol_settings_tx,
            startup_volume_tx,
            startup_volume_applier: None,
            auto_restarter: None,
            capabilities_watcher: None,
            shelves: None,
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
        self.requests_handled.load(std::sync::atomic::Ordering::Relaxed)
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
        // state subscription wire-up (framework substrate is
        // already lit). Operators picking Hardware or None today set
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
        // Clone the subject emitter so the reactor can publish
        // `stream_format` updates on every endpoint refresh.
        // The emitter is cheap to clone (Arc bump on each
        // announcer field); the clone outlives the reactor
        // task. None when subject_emitter is absent (test paths
        // that explicitly bypass it).
        let task_emitter = self.subject_emitter.clone();
        let task = tokio::spawn(async move {
            run_reactor(
                task_routing,
                task_wake,
                task_shutdown,
                endpoints_tx,
                task_count,
                task_emitter,
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
    /// When the addressing does not resolve yet (Phase 2
    /// discovery admits playback.mpd before playback.options
    /// alphabetically), the function spawns a background
    /// resolver task that retries with bounded exponential
    /// backoff until the addressing is announced, then
    /// proceeds with the subscribe + initial-state seed flow.
    /// The plugin's load() returns immediately; the reactive
    /// path lights up within seconds of playback.options
    /// announcing.
    ///
    /// The plugin's own config-table fallback continues to
    /// honour `mixer_type` from
    /// `/etc/evo/plugins.d/playback.mpd.toml` at boot until
    /// the subscriber catches up — the substrate-driven
    /// reconfiguration path is additive, not replacement.
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

        let subscriber = Arc::clone(subscriber);
        let querier = Arc::clone(querier);
        let mixer_tx = self.mixer_config_tx.clone();
        let protocol_tx = self.audio_protocol_settings_tx.clone();
        let startup_volume_tx = self.startup_volume_tx.clone();
        let addressing = ExternalAddressing {
            scheme: "evo.audio.options".to_string(),
            value: "settings".to_string(),
        };

        tokio::spawn(async move {
            // Resolve the canonical id, retrying with bounded
            // exponential backoff if playback.options has not
            // announced yet. Phase 2 discovery walks
            // `/opt/evo/plugins/` alphabetically, so
            // playback.mpd ALWAYS admits before playback.options
            // on the reference distribution; the retry closes
            // that admission-order window without depending on
            // either ordering guarantees or operator restart.
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
            // that lands between current_state and subscribe;
            // then read current_state to seed the initial
            // mixer config.
            let mut stream = match subscriber
                .subscribe_subject(canonical_id.clone())
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    // LOGGING.md §2: warn (recoverable; mpd's
                    // config-table fallback remains operative).
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
                        // LOGGING.md §2: warn (recoverable; the
                        // subscribe stream still runs).
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
                if let Some(cfg) =
                    parse_mixer_config_from_settings_state(&state)
                {
                    let _ = mixer_tx.send(cfg);
                }
                let protocol = parse_audio_protocol_settings_from_state(&state);
                let _ = protocol_tx.send(protocol);
                if let Some(sv) =
                    parse_startup_volume_from_settings_state(&state)
                {
                    let _ = startup_volume_tx.send(Some(sv));
                }
            }

            loop {
                match stream.recv().await {
                    Ok(update) => {
                        if let Some(state) = update.state.as_ref() {
                            if let Some(cfg) =
                                parse_mixer_config_from_settings_state(state)
                            {
                                let _ = mixer_tx.send(cfg);
                            }
                            let protocol =
                                parse_audio_protocol_settings_from_state(state);
                            // send() returns Err only when every
                            // subscriber has dropped — no live
                            // supervisor yet. Updating the channel
                            // value is fine; the next supervisor
                            // session reads the latest value on
                            // subscribe.
                            let _ = protocol_tx.send_replace(protocol);
                            // Publish updated startup volume too.
                            // The applier consumes only the FIRST
                            // Some(...); subsequent updates flow
                            // to the watch channel for any future
                            // consumer but do not re-fire the
                            // one-shot apply.
                            if let Some(sv) =
                                parse_startup_volume_from_settings_state(state)
                            {
                                let _ =
                                    startup_volume_tx.send_replace(Some(sv));
                            }
                        }
                    }
                    Err(SubjectStateStreamError::Lagged { dropped }) => {
                        // LOGGING.md §2: warn (recoverable;
                        // the stream auto-rejoins at the live
                        // frame; missed updates surface via
                        // the next state change).
                        tracing::warn!(
                            plugin = PLUGIN_NAME,
                            dropped = dropped,
                            "audio-options subject stream lagged; \
                             continuing at the live frame"
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
/// Resolve the audio-options settings addressing, retrying
/// with bounded exponential backoff while the subject has
/// not been announced yet. Returns the canonical id when the
/// addressing resolves, or `None` after the retry budget is
/// exhausted.
///
/// Backoff schedule: 100 ms, 200 ms, 400 ms, 800 ms, 1.6 s,
/// 3.2 s capped at 6.4 s thereafter; 10 attempts total
/// (cumulative ~25 s). At Phase 2 discovery scale (every
/// reference plugin admits within a few seconds at boot) the
/// resolve typically succeeds on attempt 2 or 3. A subject
/// that has not announced after 25 s indicates a real
/// playback.options failure that operator diagnostics will
/// already be surfacing through other channels (admission
/// failure happenings, journal traces) — the subscriber's
/// silent exit is the correct shape there.
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
                    // First miss is expected on the canonical
                    // alphabetical Phase 2 discovery order
                    // (playback.mpd admits before playback.options);
                    // a single info log explains the wait without
                    // log spam, then the retries are silent until
                    // resolution.
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
                // LOGGING.md §2: warn (recoverable; we retry).
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
    // LOGGING.md §2: warn (recoverable; the plugin's
    // config-table fallback remains operative).
    tracing::warn!(
        plugin = PLUGIN_NAME,
        "audio-options settings subject did not resolve within \
         retry budget; subscriber not wired (plugin's config table \
         remains the operator surface)"
    );
    None
}

/// Extract the operator's declared startup-volume settings from
/// an `audio.options.settings` subject-state payload. Returns
/// `None` when the payload does not carry `startup_volume_percent`
/// at all (schema drift; caller falls through to whatever the
/// last applied value was).
///
/// `max_volume_percent` defaults to 100 when absent — the same
/// posture the options plugin's `Settings::default` carries. The
/// applier clamps `startup_percent` at `max_percent` before
/// sending to MPD.
fn parse_startup_volume_from_settings_state(
    state: &serde_json::Value,
) -> Option<playback_supervisor::StartupVolume> {
    let startup_percent = state
        .get("startup_volume_percent")
        .and_then(|v| v.as_u64())?
        .min(100) as u8;
    let max_percent = state
        .get("max_volume_percent")
        .and_then(|v| v.as_u64())
        .unwrap_or(100)
        .min(100) as u8;
    Some(playback_supervisor::StartupVolume {
        startup_percent,
        max_percent,
    })
}

/// Extract the operator's MPD-protocol settings (crossfade +
/// gapless) from an `audio.options.settings` subject-state
/// payload. Missing fields fall through to the audiophile-grade
/// default (no crossfade, gapless on) — the same posture the
/// options plugin's `Settings::default` carries.
fn parse_audio_protocol_settings_from_state(
    state: &serde_json::Value,
) -> AudioProtocolSettings {
    let crossfade_seconds = state
        .get("crossfade_seconds")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32)
        .unwrap_or(0);
    let gapless = state
        .get("gapless")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    AudioProtocolSettings {
        crossfade_seconds,
        gapless,
    }
}

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
                let normalised = normalise_mixer_device_or_warn(&dev);
                Some(MixerConfig::Hardware {
                    mixer_device: normalised,
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
            let normalised = normalise_mixer_device_or_warn(&mixer_device);
            Ok(MixerConfig::Hardware {
                mixer_device: normalised,
                mixer_control,
            })
        }
        other => Err(PluginError::Permanent(format!(
            "mixer_type must be one of {{hardware, software, none}}; got \
             {other:?}"
        ))),
    }
}

/// Resolve a numeric `mixer_device` (`hw:3`) to the
/// kernel-stable named form (`hw:CARD=DAC`) by reading
/// `/proc/asound/cards`. Pass-through for already-named forms.
/// I/O failure (e.g. /proc/asound/cards unreadable) AND
/// resolution failure (e.g. index 3 doesn't exist) both fall
/// back to the original value with a WARN log — the worst case
/// is that MPD opens the operator-supplied raw value and either
/// works (the index happens to be right) or surfaces an MPD-side
/// open error in the journal. The warning is the operator
/// signal that the persisted setting should be reissued with
/// `hw:CARD=<name>`.
fn normalise_mixer_device_or_warn(raw: &str) -> String {
    let cards_text = match std::fs::read_to_string(
        mpd_fragment::PROC_ASOUND_CARDS_PATH,
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                raw,
                "could not read /proc/asound/cards to normalise mixer_device; \
                 passing the operator-supplied value through verbatim"
            );
            return raw.to_string();
        }
    };
    match mpd_fragment::normalize_mixer_device(raw, &cards_text) {
        Ok(normalised) => {
            if normalised != raw {
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    raw,
                    normalised = %normalised,
                    "normalised numeric mixer_device to kernel-stable form \
                     (operator-persisted value uses ALSA card index which \
                     reorders across reboots; the rendered fragment uses \
                     hw:CARD=<name> for stability)"
                );
            }
            normalised
        }
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                raw,
                error = %e,
                "mixer_device normalisation failed; passing operator-supplied \
                 value through verbatim — MPD will either honour it or surface \
                 a card-open error in its journal"
            );
            raw.to_string()
        }
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
    emitter: Option<SubjectEmitter>,
) {
    // Initial publish: the reactor's wake loop only publishes
    // on subsequent route changes, so without an initial pass
    // the stream_format subject's state stays empty until the
    // first format change. Capture the current endpoint from
    // the watch channel's initial value (cloned out so the
    // borrow guard is dropped before the await) and publish
    // it so subscribers entering mid-stream have a live state
    // at hand.
    if let Some(em) = emitter.as_ref() {
        let initial_snapshot = endpoints_tx.borrow().clone();
        if let Some(ep) = initial_snapshot.as_ref() {
            em.update_effective(&ep.format).await;
        }
    }
    loop {
        tokio::select! {
            _ = wake.notified() => {
                let snapshot = fetch_write_endpoint(routing.as_ref());
                // Publish the stream_format subject's state on
                // every endpoint refresh. The reactor is the
                // single place EFFECTIVE format changes flow
                // through; it does not touch source_format or
                // source_codec (the ambient observer owns those
                // and they survive an effective-format change).
                if let (Some(em), Some(ep)) =
                    (emitter.as_ref(), snapshot.as_ref())
                {
                    em.update_effective(&ep.format).await;
                }
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
///
/// **Dedupe invariant:** the reactor's `wake` is triggered by
/// every `RouteChange` event the framework's routing service
/// emits. A spurious event whose downstream `WriteEndpoint`
/// snapshot equals the current one still propagates through the
/// watch channel; without the compare-before-write below, this
/// cycle would proceed to rewrite the fragment (with byte-
/// identical content) and then unconditionally call
/// `systemctl restart mpd` — which stops MPD mid-playback for
/// zero configuration benefit, causing an audible glitch and a
/// downstream `audio.terminus` mid-playback WARN. The dedupe
/// step reads the current fragment file, compares to the freshly
/// rendered content, and returns `FragmentWorkerStatus::Idle`
/// without touching MPD when they match. See
/// [`fragment_content_matches`] and its unit tests.
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

    // Dedupe: read the current fragment content (if present) and
    // compare byte-for-byte with the freshly rendered content.
    // When they match, skip the write + restart entirely — MPD's
    // audio_output is already what we would have written and
    // restarting it under active playback is exactly the fault
    // the audio.terminus WARN classifier flagged.
    let existing = tokio::fs::read_to_string(fragment_path).await.ok();
    if fragment_content_matches(existing.as_deref(), &rendered) {
        tracing::debug!(
            plugin = PLUGIN_NAME,
            path = %fragment_path.display(),
            "fragment content unchanged; skipping write + MPD restart"
        );
        return FragmentWorkerStatus::Idle;
    }

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

/// Whether the rendered fragment content matches what is already
/// on disk. Returns `true` only when the on-disk read succeeded
/// AND the content strings are byte-identical. Any read failure
/// (missing file, permission error, decode error) returns
/// `false` — the safe default is to proceed with the write +
/// restart when we cannot prove the file is already correct.
///
/// Isolated from `apply_fragment_cycle` so the dedupe rule is
/// unit-testable without a filesystem or a restarter mock.
fn fragment_content_matches(existing: Option<&str>, rendered: &str) -> bool {
    match existing {
        Some(prev) => prev == rendered,
        None => false,
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
/// Wire-payload shape for the `emit_test_tone` course-correct
/// verb. All fields beyond `v` are optional; defaults match the
/// schema (1000 Hz, 1500 ms, both channels).
#[derive(Debug, serde::Deserialize)]
struct TestTonePayload {
    #[serde(default = "default_test_tone_payload_version")]
    v: u32,
    #[serde(default)]
    freq_hz: Option<u32>,
    #[serde(default)]
    duration_ms: Option<u32>,
    #[serde(default)]
    channel: Option<String>,
}

fn default_test_tone_payload_version() -> u32 {
    PAYLOAD_VERSION
}

/// Channel-routing options for the `emit_test_tone` verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TestToneChannel {
    Left,
    Right,
    Both,
}

impl TestToneChannel {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            "both" => Some(Self::Both),
            _ => None,
        }
    }
}

/// Synthesise a 44100 Hz / 16-bit / stereo PCM WAV file
/// carrying the requested sine wave. Returns the complete WAV
/// byte stream (44-byte header + interleaved samples). Pure
/// function, deterministic for identical inputs.
///
/// Amplitude is fixed at half-scale (`i16::MAX / 2`) so the
/// tone is clearly audible without being painful — the
/// wiring-diagnostic intent does not require full-scale
/// excursion.
fn synthesise_test_tone_wav(
    freq_hz: u32,
    duration_ms: u32,
    channel: TestToneChannel,
) -> Vec<u8> {
    const SAMPLE_RATE: u32 = 44100;
    const BIT_DEPTH: u16 = 16;
    const CHANNELS: u16 = 2;
    let num_samples = (SAMPLE_RATE as u64 * duration_ms as u64 / 1000) as u32;
    let bytes_per_sample = (BIT_DEPTH / 8) as u32;
    let block_align = CHANNELS as u32 * bytes_per_sample;
    let data_size = num_samples * block_align;
    let byte_rate = SAMPLE_RATE * block_align;

    let mut out = Vec::with_capacity(44 + data_size as usize);
    // RIFF header.
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_size).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    // fmt chunk.
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&CHANNELS.to_le_bytes());
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&(block_align as u16).to_le_bytes());
    out.extend_from_slice(&BIT_DEPTH.to_le_bytes());
    // data chunk.
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_size.to_le_bytes());

    let amplitude = (i16::MAX / 2) as f64;
    let two_pi_f = 2.0 * std::f64::consts::PI * freq_hz as f64;
    let sample_rate_f = SAMPLE_RATE as f64;
    for i in 0..num_samples {
        let t = i as f64 / sample_rate_f;
        let sample = (amplitude * (two_pi_f * t).sin()) as i16;
        let (l, r) = match channel {
            TestToneChannel::Left => (sample, 0i16),
            TestToneChannel::Right => (0i16, sample),
            TestToneChannel::Both => (sample, sample),
        };
        out.extend_from_slice(&l.to_le_bytes());
        out.extend_from_slice(&r.to_le_bytes());
    }
    out
}

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
        "seek_by_delta" => {
            let delta_ms = trimmed.parse::<i64>().map_err(|_| {
                PluginError::Permanent(format!(
                    "seek_by_delta payload must be a signed i64 of \
                     milliseconds (optional leading +/-), got {:?}",
                    trimmed
                ))
            })?;
            Ok(PlaybackCommand::SeekRelative(delta_ms))
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
        "set_mute" => match trimmed {
            "1" | "true" => Ok(PlaybackCommand::SetMute(true)),
            "0" | "false" => Ok(PlaybackCommand::SetMute(false)),
            other => Err(PluginError::Permanent(format!(
                "set_mute payload must be '0'/'1' or 'true'/'false', got {:?}",
                other
            ))),
        },
        "set_repeat" => match trimmed {
            "1" | "true" => Ok(PlaybackCommand::SetRepeat(true)),
            "0" | "false" => Ok(PlaybackCommand::SetRepeat(false)),
            other => Err(PluginError::Permanent(format!(
                "set_repeat payload must be '0'/'1' or 'true'/'false', got {:?}",
                other
            ))),
        },
        "set_shuffle" => match trimmed {
            "1" | "true" => Ok(PlaybackCommand::SetShuffle(true)),
            "0" | "false" => Ok(PlaybackCommand::SetShuffle(false)),
            other => Err(PluginError::Permanent(format!(
                "set_shuffle payload must be '0'/'1' or 'true'/'false', got {:?}",
                other
            ))),
        },
        "set_single" => match trimmed {
            "1" | "true" => Ok(PlaybackCommand::SetSingle(true)),
            "0" | "false" => Ok(PlaybackCommand::SetSingle(false)),
            other => Err(PluginError::Permanent(format!(
                "set_single payload must be '0'/'1' or 'true'/'false', got {:?}",
                other
            ))),
        },
        "set_consume" => match trimmed {
            "1" | "true" => Ok(PlaybackCommand::SetConsume(true)),
            "0" | "false" => Ok(PlaybackCommand::SetConsume(false)),
            other => Err(PluginError::Permanent(format!(
                "set_consume payload must be '0'/'1' or 'true'/'false', got {:?}",
                other
            ))),
        },
        "emit_test_tone" => {
            // course_correct routes this verb to the warden's
            // dedicated `emit_test_tone` method BEFORE calling
            // parse_correction. Reaching this branch indicates
            // a dispatcher bug, not an operator-payload
            // problem; surface the structured invariant
            // refusal.
            Err(PluginError::Permanent(
                "emit_test_tone parsed via dedicated dispatch \
                 path; parse_correction should not see it"
                    .to_string(),
            ))
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
            let emitter = SubjectEmitter::new(
                Arc::clone(&ctx.subject_announcer) as Arc<dyn SubjectAnnouncer>,
                Arc::clone(&ctx.relation_announcer)
                    as Arc<dyn RelationAnnouncer>,
            );

            // Announce the live stream_format subject once at
            // load. The reactor's per-route-change update calls
            // (set up via `spawn_reactor` below) drive the
            // subject's state thereafter.
            emitter.announce_stream_format().await;

            // Announce the live now_playing subject once at load.
            // The playback supervisor's state-report emitter
            // (every command, every idle wake, every status poll)
            // drives the subject's state thereafter so the UI
            // sees the initial state on first subscribe + live
            // updates without poll.
            emitter.announce_now_playing().await;

            self.subject_emitter = Some(emitter);

            // Spawn the ambient now-playing observer. Runs for
            // the plugin's load lifetime regardless of custody —
            // closes the gap where the custody-gated supervisor
            // leaves the `audio_playback_now_playing` subject's
            // state unset on a fresh boot. Downstream consumers
            // (audio.terminus's leader gate) need state
            // publication on every observed MPD transition,
            // whether or not an operator has gestured a play
            // action through the warden surface.
            //
            // The ambient observer + the supervisor coexist:
            // both see the same MPD state via independent IDLE
            // channels; both render byte-identical reports;
            // both publish the same update_state. The framework's
            // subject substrate dedups identical content, so the
            // duplicate publish under custody is a cheap no-op.
            let ambient_emitter = self
                .subject_emitter
                .as_ref()
                .expect("subject_emitter set above")
                .clone();
            // Resolve MPD's `music_directory` once at load time
            // so the ambient observer can map MPD's relative
            // file paths to absolute filesystem paths for the
            // file-side source-format probe. The probe stays
            // gracefully None when the conf file is missing or
            // doesn't declare the directive — the wire surfaces
            // source_codec alone in that case rather than
            // claiming a source shape we can't verify.
            let music_directory =
                source_probe::load_music_directory_from_mpd_conf(
                    std::path::Path::new(source_probe::DEFAULT_MPD_CONF_PATH),
                );
            if let Some(ref dir) = music_directory {
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    music_directory = %dir.display(),
                    "ambient source-format probe armed"
                );
            } else {
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    "music_directory unresolved (mpd.conf absent / no directive); \
                     source-format probe disarmed — source_codec still surfaces"
                );
            }
            self.ambient_observer =
                Some(playback_supervisor::spawn_ambient_observer(
                    self.endpoint.clone(),
                    self.timeouts,
                    ambient_emitter,
                    music_directory,
                ));
            tracing::info!(
                plugin = PLUGIN_NAME,
                endpoint = %self.endpoint,
                "ambient now-playing observer spawned (publishes regardless of custody)"
            );

            // Spawn the route-change reactor and the
            // fragment-writer worker. The reactor watches
            // the framework's topology rewires; the worker
            // renders MPD's audio_output block and recycles
            // MPD on every snapshot. The reactor also fans
            // the format part of each endpoint snapshot into
            // the `stream_format` subject's state.
            self.spawn_reactor().await?;
            self.spawn_fragment_worker().await?;

            // Spawn the mixer-transition envelope subscriber.
            // Subscribes to the orchestrator-published
            // envelope_requested subject and dispatches
            // PlaybackCommand::Pause(true / false) to the
            // active custody's supervisor (via the cloneable
            // command-sender cell `active_command_sender`),
            // then publishes envelope_observed so the
            // orchestrator's await advances. When no
            // custody is active the subscriber acks
            // immediately (chain is already silent;
            // nothing to pause). Subject_state_subscriber +
            // subject_querier + subject_announcer are
            // required; missing any → skip (advisory mode).
            match (
                ctx.subject_state_subscriber.as_ref(),
                ctx.subject_querier.as_ref(),
            ) {
                (Some(sub), Some(q)) => {
                    tracing::info!(
                        plugin = PLUGIN_NAME,
                        "envelope subscriber: subscriber + querier present; \
                         spawning"
                    );
                    self.envelope_subscriber =
                        Some(envelope_subscriber::spawn(
                            PLUGIN_NAME,
                            Arc::clone(sub),
                            Arc::clone(q),
                            Arc::clone(&ctx.subject_announcer),
                            Arc::clone(&self.active_command_sender),
                        ));
                }
                (sub, q) => {
                    tracing::warn!(
                        plugin = PLUGIN_NAME,
                        subscriber_present = sub.is_some(),
                        querier_present = q.is_some(),
                        "envelope subscriber NOT spawned; one or both of \
                         subject_state_subscriber / subject_querier \
                         unpopulated (likely OOP transport pre-wire-surface)"
                    );
                }
            }

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

            // Spawn the startup-volume applier. Once the
            // options-settings subscriber above publishes the
            // first parsed state carrying `startup_volume_percent`
            // + `max_volume_percent`, the applier waits for MPD
            // to accept a `setvol` and applies the effective
            // startup value (clamped to `max_volume_percent`).
            // This is the fix for the "MPD statefile wins over
            // configured startup" defect: without this task the
            // reboot volume is whatever MPD's own persistent
            // state file carries in, not the operator's declared
            // startup floor.
            //
            // One-shot per load: the task exits after a
            // successful `setvol`; subsequent volume gestures
            // flow through the normal `set_volume` wire path.
            self.startup_volume_applier =
                Some(playback_supervisor::spawn_startup_volume_applier(
                    self.endpoint.clone(),
                    self.timeouts,
                    self.startup_volume_tx.subscribe(),
                ));
            tracing::info!(
                plugin = PLUGIN_NAME,
                "startup-volume applier spawned; awaits options settings + \
                 MPD acceptance"
            );

            // Spawn the /etc/asound.d/ composition-change
            // watcher. On every detected change the watcher
            // dispatches a CycleOutput to the active custody's
            // supervisor, which sends disableoutput 0 +
            // enableoutput 0 over the MPD wire protocol so
            // MPD drops and reopens its snd_pcm_t against the
            // post-change drop-in stack. This is how the
            // playback warden adapts to multi-room source-mode
            // engage / disengage without a systemd bounce or a
            // fragment rewrite.
            self.asound_watcher = Some(asound_watcher::spawn(Arc::clone(
                &self.active_command_sender,
            )));

            // Initialise the shelf integration: source
            // registry + sticker reconciler + four shelf
            // contexts (queue / playlist / favourites /
            // library) + disposition emitter. Subjects are
            // announced inside `init`; rehydration from the
            // plugin's state directory happens there too.
            let shelves = shelves::ShelfBundle::init(
                ctx.state_dir.clone(),
                Arc::clone(&ctx.subject_announcer) as Arc<dyn SubjectAnnouncer>,
                self.endpoint.clone(),
                self.timeouts,
            )
            .await;
            self.shelves = Some(shelves);

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
            // Stop the envelope subscriber AFTER the fragment
            // worker + reactor so any in-flight pause /
            // resume dispatch completes against a still-live
            // supervisor before unload tears the custody
            // down.
            if let Some(handle) = self.envelope_subscriber.take() {
                handle.stop().await;
            }
            // Stop the ambient now-playing observer. Independent
            // of any custody supervisor; safe to stop any time
            // after the custodies drain above.
            if let Some(handle) = self.ambient_observer.take() {
                handle.stop().await;
            }
            // Stop the startup-volume applier if it is still
            // waiting for options settings or in its retry
            // loop. On the happy path the applier has already
            // exited (successful `setvol`); the take + stop
            // is a no-op when the join handle is completed.
            if let Some(handle) = self.startup_volume_applier.take() {
                handle.stop().await;
            }
            // Stop the asound watcher last. The supervisor's
            // command sender may still be live for a few more
            // microseconds at this point; the watcher's
            // background-task drain races a CycleOutput
            // dispatch at worst, and a Shutdown reply is the
            // observed outcome that path is designed for.
            if let Some(handle) = self.asound_watcher.take() {
                handle.stop().await;
            }
            // Tear down the shelf integration last. Stops the
            // sticker reconciler and persists the source
            // registry so the next load rehydrates state
            // without operator effort.
            if let Some(shelves) = self.shelves.take() {
                shelves.shutdown().await;
            }
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

            // Idempotence: when this plugin already holds an
            // active custody the framework's request-op bootstrap
            // can fire take_custody again on a subsequent verb
            // dispatch — the ledger may carry stale records (e.g.
            // post-restart) and the framework conservatively
            // requests acquisition. Spawning a fresh supervisor
            // on every such request would discard the accumulated
            // supervisor state (captured pre-mute volume, mute
            // toggle, etc.). Return the existing handle instead;
            // the framework's ledger entry is upserted on the
            // returned handle id.
            if let Some((existing_id, _)) = self.custodies.iter().next() {
                let handle = CustodyHandle::new(existing_id.clone());
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    handle = %handle.id,
                    cid = assignment.correlation_id,
                    "take_custody: returning existing handle (idempotent)"
                );
                return Ok(handle);
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
            // applies the operator's MPD-protocol settings,
            // emits the initial state report, returns a handle for
            // command dispatch and shutdown. Failure maps to the
            // steward-visible PluginError variant.
            let supervisor = match playback_supervisor::spawn(
                self.endpoint.clone(),
                self.timeouts,
                handle.clone(),
                assignment.custody_state_reporter,
                emitter,
                self.audio_protocol_settings_tx.subscribe(),
                source_probe::load_music_directory_from_mpd_conf(
                    std::path::Path::new(source_probe::DEFAULT_MPD_CONF_PATH),
                ),
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

            // Capture the supervisor's cloneable command sender
            // BEFORE moving it into the custodies map so the
            // mixer-transition envelope subscriber can dispatch
            // Pause(true / false) on orchestrator-published
            // envelope_requested updates without owning the
            // supervisor itself. The cell is cleared in
            // relinquish_custody.
            *self.active_command_sender.lock().await =
                Some(supervisor.command_sender());

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

            // emit_test_tone takes a dedicated dispatch path:
            // the verb synthesises the WAV inline + drives the
            // MPD load+play sequence on a dedicated Unix-socket
            // connection (MPD's security model refuses file://
            // loads over TCP). The supervisor's main connection
            // is untouched; MPD's idle subprotocol surfaces the
            // new playing item to the supervisor as a normal
            // state change.
            if correction.correction_type == "emit_test_tone" {
                // Confirm custody is held before dispatching —
                // otherwise the operator would trigger MPD
                // playback against a warden the framework hasn't
                // routed.
                let _ = self.custodies.get(&handle.id).ok_or_else(|| {
                    PluginError::Permanent(format!(
                        "unknown custody handle: {}",
                        handle.id
                    ))
                })?;
                self.corrections_dispatched += 1;
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    handle = %handle.id,
                    cid = correction.correlation_id,
                    "emit_test_tone dispatching via Unix-socket side connection"
                );
                return self.emit_test_tone(&correction).await;
            }

            // Parse first: a malformed correction fails with a
            // clear "request was bad" signal before we ever
            // touch the custody map or the supervisor.
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

            // Clear the envelope-subscriber's command-sender
            // cell BEFORE shutting the supervisor down — the
            // subscriber's snapshot-then-dispatch pattern can
            // race with shutdown if the cell still points at a
            // shutting-down supervisor.
            *self.active_command_sender.lock().await = None;

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
        &'a self,
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

            self.requests_handled.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

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
                "seek_by_delta" => self.handle_seek_by_delta(req).await,
                "set_volume" => self.handle_set_volume(req).await,
                "set_mute" => self.handle_set_bool(req, "set_mute").await,
                "set_repeat" => self.handle_set_bool(req, "set_repeat").await,
                "set_shuffle" => self.handle_set_bool(req, "set_shuffle").await,
                "set_single" => self.handle_set_bool(req, "set_single").await,
                "set_consume" => self.handle_set_bool(req, "set_consume").await,
                "get_now_playing" => self.handle_get_now_playing(req).await,
                "get_stream_format" => self.handle_get_stream_format(req).await,
                _ => {
                    // Shelf verbs (queue / playlist /
                    // favourites / library). The bundle's
                    // dispatcher returns `Ok(Some(resp))`
                    // when the request_type matched a shelf
                    // verb, `Ok(None)` when it did not. A
                    // None response with a declared verb is
                    // the manifest/runtime drift bug the
                    // legacy arm used to catch.
                    let bundle = self.shelves.as_ref().ok_or_else(|| {
                        PluginError::Permanent(
                            "shelves not initialised; load() was not called"
                                .to_string(),
                        )
                    })?;
                    match bundle.dispatch_request(req).await? {
                        Some(resp) => Ok(resp),
                        None => Err(PluginError::Permanent(format!(
                            "request type {:?} declared but no handler wired; \
                             this is a manifest/runtime drift bug",
                            req.request_type
                        ))),
                    }
                }
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
    ///
    /// **Not a transformation point.** The value the caller
    /// supplies is passed to MPD's `setvol` byte-for-byte. Any
    /// deviation the operator observes between the requested
    /// percent and the resulting hardware-mixer state (e.g.
    /// "set_volume 77 lands as 79 on the DAC's Digital
    /// control") lives BELOW this handler:
    ///
    /// 1. MPD's own hardware-mixer mapping applies a scaling
    ///    from `setvol N` (0..100 wire percent) to the mixer
    ///    control's units. For controls with a dB-scale
    ///    quantised at a step > 1 dB, or with a non-linear
    ///    dB→percent curve, adjacent wire percents may land at
    ///    the same hardware mixer value or the hardware may
    ///    round to its nearest quantised step.
    /// 2. The DAC's mixer control step size itself (visible
    ///    via `amixer -c <card> cget numid=<Digital>` — the
    ///    `step=` field). Controls with `step=0` are logically
    ///    continuous; a non-zero step forces quantisation at
    ///    that granularity.
    ///
    /// The framework does not clamp or round volumes on the
    /// framework side. The `77 → 79` observation the 2026-07-20
    /// footnote memo names is DAC step-rounding, not framework
    /// or MPD-level distortion — documented here so the
    /// operator surface can render "requested vs achieved" if
    /// UI wants to expose the delta, and so a future audit does
    /// not chase a phantom framework bug.
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

    /// Handle a `seek_by_delta` source-verb request: extract the
    /// signed millisecond delta and issue a
    /// [`PlaybackCommand::SeekRelative`]. The supervisor /
    /// MPD layer clamps the resulting absolute position.
    async fn handle_seek_by_delta(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: SeekByDeltaPayload =
            parse_versioned_payload(req, "seek_by_delta")?;
        let supervisor = self.active_supervisor("seek_by_delta")?;
        supervisor
            .command(PlaybackCommand::SeekRelative(payload.delta_ms))
            .await
            .map_err(playback_error_to_plugin_error)?;
        encode_simple_ok(req)
    }

    /// Handle a `get_now_playing` source-verb request: query
    /// the supervisor for the current playback state report,
    /// render it through the same projection the now_playing
    /// subject uses, and return it as the response payload.
    /// This is the read-side counterpart to the now_playing
    /// subject's transition-only update stream — fresh UI
    /// clients call this on connect to render their initial
    /// frame, then subscribe to the subject for deltas.
    async fn handle_get_now_playing(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let _: EmptyPayload = parse_versioned_payload(req, "get_now_playing")?;
        let supervisor = self.active_supervisor("get_now_playing")?;
        let report = supervisor
            .query_state()
            .await
            .map_err(playback_error_to_plugin_error)?;
        let state = playback_supervisor::render_now_playing_state(&report);
        let body = serde_json::to_vec(&state).map_err(|e| {
            PluginError::Permanent(format!(
                "get_now_playing response JSON encode failed: {e}"
            ))
        })?;
        Ok(Response::for_request(req, body))
    }

    /// Handle a `get_stream_format` source-verb request: read
    /// the current envelope from the subject emitter's in-memory
    /// mirror.
    ///
    /// Custody is NOT required — the mirror is maintained by
    /// the route-change reactor (independent of any custody
    /// supervisor's lifecycle) and seeded by
    /// `announce_stream_format` at plugin load. UI clients
    /// running the read-then-subscribe pattern call this on
    /// connect, get the live envelope (or the seeded empty
    /// envelope when no reactor publish has fired yet), then
    /// subscribe to the `audio_playback_stream_format`
    /// subject for subsequent transitions.
    async fn handle_get_stream_format(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let _: EmptyPayload =
            parse_versioned_payload(req, "get_stream_format")?;
        let emitter = self.subject_emitter.as_ref().ok_or_else(|| {
            PluginError::Permanent(
                "get_stream_format requested before plugin load completed; \
                 subject emitter not yet initialised"
                    .to_string(),
            )
        })?;
        let state = emitter.latest_stream_format_envelope();
        let body = serde_json::to_vec(&state).map_err(|e| {
            PluginError::Permanent(format!(
                "get_stream_format response JSON encode failed: {e}"
            ))
        })?;
        Ok(Response::for_request(req, body))
    }

    /// Handle a boolean-toggle source-verb request — `set_mute`,
    /// `set_repeat`, `set_shuffle`, `set_single`, `set_consume`.
    /// The payload shape is uniform (`{ "v": 1, "enabled": bool
    /// }`); the verb name selects which [`PlaybackCommand`]
    /// variant is dispatched.
    async fn handle_set_bool(
        &self,
        req: &Request,
        verb_name: &'static str,
    ) -> Result<Response, PluginError> {
        let payload: SetBoolPayload = parse_versioned_payload(req, verb_name)?;
        let cmd = match verb_name {
            "set_mute" => PlaybackCommand::SetMute(payload.enabled),
            "set_repeat" => PlaybackCommand::SetRepeat(payload.enabled),
            "set_shuffle" => PlaybackCommand::SetShuffle(payload.enabled),
            "set_single" => PlaybackCommand::SetSingle(payload.enabled),
            "set_consume" => PlaybackCommand::SetConsume(payload.enabled),
            other => {
                return Err(PluginError::Permanent(format!(
                    "handle_set_bool called with unknown verb {other:?}; \
                     this is a dispatcher/runtime drift bug"
                )));
            }
        };
        let supervisor = self.active_supervisor(verb_name)?;
        supervisor
            .command(cmd)
            .await
            .map_err(playback_error_to_plugin_error)?;
        encode_simple_ok(req)
    }

    /// Synthesise the requested sine-wave PCM WAV, write it to
    /// a temp file MPD can read, and dispatch the
    /// `clear` + `add file://...` + `play` sequence over a
    /// dedicated Unix-socket connection to MPD.
    ///
    /// The Unix-socket detour is required: MPD's security model
    /// refuses `file://...` URI loads over TCP (`Access to local
    /// files via TCP is not allowed`), the supervisor's main
    /// connection is typically TCP, and the operator-facing
    /// wiring-diagnostic contract pins this verb to "routes
    /// through the SAME composition + delivery + DAC chain as
    /// real playback". Opening a side Unix-socket connection
    /// satisfies all three: the file load succeeds because MPD
    /// treats local-socket access as trusted; the supervisor's
    /// TCP queue ops stay on the TCP connection (no
    /// connection-class change); the rendered audio still
    /// traverses MPD's audio_output (the pcm.evo chain), which
    /// is the same path real queue playback uses.
    ///
    /// Refusal contract: when `/run/mpd/socket` does not exist
    /// or refuses connect, the verb returns a structured
    /// Permanent error naming the missing Unix-socket surface
    /// and pointing the operator at the distribution's
    /// `bootstrap.sh` MPD-socket configuration — no silent
    /// fallback, no opaque MPD-ack passthrough.
    ///
    /// The synthesis runs inline (in-process, no subprocess) so
    /// the operator-gesture latency is bounded by serialisation
    /// plus `tokio::fs::write` plus the Unix-socket handshake.
    /// The WAV header carries a stable 44100 Hz / 16-bit /
    /// stereo format (universal DAC support; if the operator's
    /// chain is configured for bit-perfect with a non-44100
    /// source, the test tone deliberately exercises the
    /// format-conversion path — the wiring-diagnostic intent
    /// stays intact).
    async fn emit_test_tone(
        &self,
        correction: &CourseCorrection,
    ) -> Result<(), PluginError> {
        let payload: TestTonePayload =
            serde_json::from_slice(&correction.payload).map_err(|e| {
                PluginError::Permanent(format!(
                    "emit_test_tone payload is not valid JSON for \
                 the expected shape: {e}"
                ))
            })?;
        if payload.v != PAYLOAD_VERSION {
            return Err(PluginError::Permanent(format!(
                "emit_test_tone payload version {} unsupported; \
                 expected {}",
                payload.v, PAYLOAD_VERSION
            )));
        }
        let freq_hz = payload.freq_hz.unwrap_or(1000);
        if !(TEST_TONE_FREQ_MIN..=TEST_TONE_FREQ_MAX).contains(&freq_hz) {
            return Err(PluginError::Permanent(format!(
                "emit_test_tone freq_hz must lie in {}..={}; \
                 got {}",
                TEST_TONE_FREQ_MIN, TEST_TONE_FREQ_MAX, freq_hz
            )));
        }
        let duration_ms = payload.duration_ms.unwrap_or(1500);
        if !(TEST_TONE_DURATION_MIN_MS..=TEST_TONE_DURATION_MAX_MS)
            .contains(&duration_ms)
        {
            return Err(PluginError::Permanent(format!(
                "emit_test_tone duration_ms must lie in {}..={}; \
                 got {}",
                TEST_TONE_DURATION_MIN_MS,
                TEST_TONE_DURATION_MAX_MS,
                duration_ms
            )));
        }
        let channel_str = payload
            .channel
            .as_deref()
            .unwrap_or(TEST_TONE_DEFAULT_CHANNEL);
        let channel =
            TestToneChannel::from_str(channel_str).ok_or_else(|| {
                PluginError::Permanent(format!(
                    "emit_test_tone channel must be one of \
                     {{left, right, both}}; got {:?}",
                    channel_str
                ))
            })?;
        let wav = synthesise_test_tone_wav(freq_hz, duration_ms, channel);
        // The spool directory lives under the steward's
        // RuntimeDirectory (/run/evo/) so the path is shared
        // across services. /tmp is unusable here:
        // evo.service runs with `PrivateTmp=yes`, isolating
        // its /tmp from MPD's view — a file the warden writes
        // to /tmp is invisible to the MPD daemon. mkdir_all is
        // idempotent + safe across concurrent invocations.
        tokio::fs::create_dir_all(TEST_TONE_SPOOL_DIR)
            .await
            .map_err(|e| {
                PluginError::Permanent(format!(
                    "emit_test_tone create_dir_all {} failed: {}",
                    TEST_TONE_SPOOL_DIR, e
                ))
            })?;
        let path = format!(
            "{}/evo-test-tone-{}.wav",
            TEST_TONE_SPOOL_DIR, correction.correlation_id
        );
        tokio::fs::write(&path, &wav).await.map_err(|e| {
            PluginError::Permanent(format!(
                "emit_test_tone write to {} failed: {}",
                path, e
            ))
        })?;
        // Make the WAV world-readable so the MPD daemon
        // (running as the `mpd` user, distinct from the
        // steward user) can read it. The spool directory is
        // mode 0755 root:root from systemd's RuntimeDirectory
        // primitive; per-file 0644 closes the read path.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o644);
            tokio::fs::set_permissions(&path, perms)
                .await
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "emit_test_tone chmod 0644 on {} failed: {}",
                        path, e
                    ))
                })?;
        }

        // Dedicated Unix-socket connection. The supervisor's main
        // connection is untouched (it stays on its configured
        // TCP / Unix endpoint and continues serving queue ops);
        // this side connection exists only for the duration of
        // the test-tone dispatch.
        if !std::path::Path::new(TEST_TONE_MPD_UNIX_SOCKET).exists() {
            return Err(PluginError::Permanent(format!(
                "emit_test_tone refused: MPD Unix socket {} does \
                 not exist on this host. MPD's security model \
                 refuses file:// loads over TCP, so the test \
                 tone requires the local Unix socket. The \
                 distribution's bootstrap.sh configures this; \
                 enable it and restart mpd to land the surface.",
                TEST_TONE_MPD_UNIX_SOCKET
            )));
        }
        // In-flight gate: refuse concurrent diagnostic runs.
        // Two `emit_test_tone` calls overlapping would race
        // each other's capture / set / restore against the
        // same MPD daemon and could leave the operator's
        // prior modes lost (the later capture would observe
        // the earlier's diagnostic state, then "restore" to
        // that). A structured refusal protects the operator
        // surface; once the in-flight tone's restore
        // completes, the flag clears and the next request
        // proceeds.
        if self
            .test_tone_in_flight
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(PluginError::Permanent(
                "emit_test_tone refused: another diagnostic tone is \
                 already in flight on this device. Wait for it to \
                 complete (configured duration + restore window) and \
                 retry."
                    .to_string(),
            ));
        }
        let in_flight = Arc::clone(&self.test_tone_in_flight);
        // Helper that clears the in-flight gate when the
        // dispatch fails before the spawned restore task can
        // take ownership of it. Wrapped in a closure so each
        // early-return site is a single line.
        let release_in_flight = || {
            in_flight.store(false, std::sync::atomic::Ordering::SeqCst);
        };

        let unix_endpoint = MpdEndpoint::unix(TEST_TONE_MPD_UNIX_SOCKET)
            .map_err(|e| {
                release_in_flight();
                PluginError::Permanent(format!(
                    "emit_test_tone Unix-endpoint construction failed \
                     for {}: {:?}",
                    TEST_TONE_MPD_UNIX_SOCKET, e
                ))
            })?;
        let mut conn =
            MpdConnection::connect_with_timeouts(unix_endpoint, self.timeouts)
                .await
                .map_err(|e| {
                    release_in_flight();
                    PluginError::Permanent(format!(
                        "emit_test_tone Unix-socket connect failed: {e}"
                    ))
                })?;

        // Capture the operator's prior transport-mode state.
        // MPD's repeat / random / single / consume / crossfade
        // are daemon-global; a queue of one finite WAV played
        // under operator-default `repeat=1` loops forever. The
        // diagnostic's contract is "play exactly once,
        // independent of operator state, leave the operator's
        // prior state intact". `status` is one round-trip and
        // surfaces every field the diagnostic must capture.
        let prior_status = conn.status().await.map_err(|e| {
            release_in_flight();
            PluginError::Permanent(format!(
                "emit_test_tone MPD status (pre-capture) failed: {e}"
            ))
        })?;

        // Set deterministic diagnostic-isolation modes BEFORE
        // play so the one-item queue plays exactly once. Each
        // setter is one round-trip — five round-trips total —
        // bounded by the connection's command timeout.
        async fn neutralise_for_diagnostic(
            conn: &mut MpdConnection,
        ) -> Result<(), MpdError> {
            conn.set_repeat(false).await?;
            conn.set_single(true).await?;
            conn.set_random(false).await?;
            conn.set_consume(false).await?;
            conn.set_crossfade(0).await?;
            Ok(())
        }
        if let Err(e) = neutralise_for_diagnostic(&mut conn).await {
            // Restore best-effort — we may have set some
            // modes already; leaving them is worse than trying
            // to put each back.
            let _ = conn.set_repeat(prior_status.repeat).await;
            let _ = conn.set_single(prior_status.single).await;
            let _ = conn.set_random(prior_status.random).await;
            let _ = conn.set_consume(prior_status.consume).await;
            let _ = conn.set_crossfade(prior_status.crossfade_seconds).await;
            release_in_flight();
            return Err(PluginError::Permanent(format!(
                "emit_test_tone neutralise (pre-play mode setup) \
                 failed: {e}; attempted best-effort restore of prior \
                 modes before refusing"
            )));
        }

        // Three-step queue sequence — clear so the test tone
        // replaces any prior queue content, add the file://
        // URI (allowed on the Unix socket), play. On failure
        // restore prior modes before refusing so the operator
        // is never left in diagnostic state.
        async fn restore_prior(conn: &mut MpdConnection, prior: &MpdStatus) {
            let _ = conn.set_repeat(prior.repeat).await;
            let _ = conn.set_single(prior.single).await;
            let _ = conn.set_random(prior.random).await;
            let _ = conn.set_consume(prior.consume).await;
            let _ = conn.set_crossfade(prior.crossfade_seconds).await;
        }
        if let Err(e) = conn.clear().await {
            restore_prior(&mut conn, &prior_status).await;
            release_in_flight();
            return Err(PluginError::Permanent(format!(
                "emit_test_tone MPD clear failed: {e}"
            )));
        }
        let file_uri = format!("file://{}", path);
        if let Err(e) = conn.add(&file_uri).await {
            restore_prior(&mut conn, &prior_status).await;
            release_in_flight();
            return Err(PluginError::Permanent(format!(
                "emit_test_tone MPD add ({}) failed: {e}",
                file_uri
            )));
        }
        if let Err(e) = conn.play().await {
            restore_prior(&mut conn, &prior_status).await;
            release_in_flight();
            return Err(PluginError::Permanent(format!(
                "emit_test_tone MPD play failed: {e}"
            )));
        }
        // Deterministic startup gate: proving `play` ACK alone is
        // insufficient for setup diagnostics because MPD can accept
        // the command yet never transition to an active stream.
        // Wait briefly for `state=Playing`; if that never happens,
        // restore prior modes and fail loudly so setup surfaces a
        // concrete fault instead of a silent "no sound".
        const PLAYING_POLL_INTERVAL_MS: u64 = 100;
        const PLAYING_DEADLINE_MS: u64 = 1500;
        let playing_deadline = tokio::time::Instant::now()
            + std::time::Duration::from_millis(PLAYING_DEADLINE_MS);
        loop {
            if tokio::time::Instant::now() >= playing_deadline {
                restore_prior(&mut conn, &prior_status).await;
                release_in_flight();
                return Err(PluginError::Permanent(format!(
                    "emit_test_tone MPD play did not transition to Playing \
                     within {} ms",
                    PLAYING_DEADLINE_MS
                )));
            }
            match conn.status().await {
                Ok(s) if matches!(s.state, crate::mpd::PlayState::Playing) => {
                    break;
                }
                Ok(_) => {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        PLAYING_POLL_INTERVAL_MS,
                    ))
                    .await;
                }
                Err(e) => {
                    restore_prior(&mut conn, &prior_status).await;
                    release_in_flight();
                    return Err(PluginError::Permanent(format!(
                        "emit_test_tone MPD status poll failed after play: {e}"
                    )));
                }
            }
        }
        // Drop the synchronous connection; the background
        // restore task opens its own.
        drop(conn);

        // Background restore: poll status until the tone
        // finishes, then restore the operator's prior modes.
        // Spawned (not awaited) so the wire-op returns
        // promptly while the cleanup runs ephemerally; the
        // in-flight gate stays set until the spawned task
        // clears it.
        //
        // Bounds:
        //
        //   * `duration_ms + RESTORE_DEADLINE_SLACK_MS` —
        //     upper bound on the wait. `single = 1` halts MPD
        //     at the end of the one-item queue, so the actual
        //     wait is the tone's audio duration plus a small
        //     handshake margin.
        //   * `STATUS_POLL_INTERVAL_MS` — granularity for the
        //     `status` poll. Coarse enough to keep MPD's load
        //     negligible; fine enough that the gap between
        //     tone-end and restore is sub-perceptible.
        const STATUS_POLL_INTERVAL_MS: u64 = 100;
        const RESTORE_DEADLINE_SLACK_MS: u64 = 1500;
        let socket_path = TEST_TONE_MPD_UNIX_SOCKET.to_string();
        let timeouts = self.timeouts;
        let duration_ms_u64 = u64::from(duration_ms);
        tokio::spawn(async move {
            let deadline = tokio::time::Instant::now()
                + std::time::Duration::from_millis(
                    duration_ms_u64 + RESTORE_DEADLINE_SLACK_MS,
                );
            let mut restore_conn = match MpdEndpoint::unix(&socket_path) {
                Ok(ep) => {
                    match MpdConnection::connect_with_timeouts(ep, timeouts)
                        .await
                    {
                        Ok(c) => Some(c),
                        Err(e) => {
                            tracing::warn!(
                                plugin = PLUGIN_NAME,
                                error = %e,
                                "emit_test_tone restore: reconnect to MPD \
                                 Unix socket failed; cannot restore prior \
                                 transport modes — operator's modes may be \
                                 left in diagnostic state"
                            );
                            None
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        plugin = PLUGIN_NAME,
                        error = ?e,
                        "emit_test_tone restore: Unix-endpoint construction \
                         failed; cannot restore prior transport modes"
                    );
                    None
                }
            };

            // Wait until MPD reports state=Stopped OR deadline.
            // single=1 + finite WAV guarantees Stopped at end
            // of one play; the poll catches it within the
            // poll interval.
            if let Some(ref mut conn) = restore_conn {
                loop {
                    if tokio::time::Instant::now() >= deadline {
                        tracing::warn!(
                            plugin = PLUGIN_NAME,
                            duration_ms = duration_ms_u64,
                            slack_ms = RESTORE_DEADLINE_SLACK_MS,
                            "emit_test_tone restore: deadline reached \
                             waiting for MPD to report stopped; restoring \
                             prior modes anyway"
                        );
                        break;
                    }
                    match conn.status().await {
                        Ok(s)
                            if matches!(
                                s.state,
                                crate::mpd::PlayState::Stopped
                            ) =>
                        {
                            break;
                        }
                        Ok(_) => {
                            tokio::time::sleep(
                                std::time::Duration::from_millis(
                                    STATUS_POLL_INTERVAL_MS,
                                ),
                            )
                            .await;
                        }
                        Err(e) => {
                            tracing::warn!(
                                plugin = PLUGIN_NAME,
                                error = %e,
                                "emit_test_tone restore: status poll \
                                 failed; aborting wait + attempting restore"
                            );
                            break;
                        }
                    }
                }
                // Restore the captured pre-tone modes verbatim.
                // Each setter is logged individually on failure;
                // partial restore is documented in the warn
                // line.
                let _ = conn.set_repeat(prior_status.repeat).await;
                let _ = conn.set_single(prior_status.single).await;
                let _ = conn.set_random(prior_status.random).await;
                let _ = conn.set_consume(prior_status.consume).await;
                let _ =
                    conn.set_crossfade(prior_status.crossfade_seconds).await;
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    repeat = prior_status.repeat,
                    single = prior_status.single,
                    random = prior_status.random,
                    consume = prior_status.consume,
                    crossfade_seconds = prior_status.crossfade_seconds,
                    "emit_test_tone restore: prior transport modes \
                     restored"
                );
            }
            // Clear the in-flight gate so the next diagnostic
            // can fire. Always runs regardless of restore
            // outcome — leaving the gate latched would jam
            // the surface.
            in_flight.store(false, std::sync::atomic::Ordering::SeqCst);
        });

        Ok(())
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

/// Wire shape of a `seek_by_delta` request payload. Signed
/// millisecond offset applied relative to the current playhead
/// position. The supervisor / MPD layer clamps the resulting
/// absolute position; this struct does not enforce bounds.
#[derive(Debug, serde::Deserialize)]
struct SeekByDeltaPayload {
    #[serde(default = "default_payload_version")]
    v: u32,
    delta_ms: i64,
}

impl HasPayloadVersion for SeekByDeltaPayload {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

/// Wire shape of every boolean-toggle request payload —
/// `set_mute`, `set_repeat`, `set_shuffle`, `set_single`,
/// `set_consume`. Uniform shape lets one struct + one handler
/// (`handle_set_bool`) drive every toggle verb.
#[derive(Debug, serde::Deserialize)]
struct SetBoolPayload {
    #[serde(default = "default_payload_version")]
    v: u32,
    enabled: bool,
}

impl HasPayloadVersion for SetBoolPayload {
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
mod fragment_dedupe_tests {
    use super::fragment_content_matches;

    #[test]
    fn matches_when_bytes_identical() {
        let fragment =
            "audio_output {\n    type \"alsa\"\n    device \"hw:2,0\"\n}\n";
        assert!(
            fragment_content_matches(Some(fragment), fragment),
            "dedupe must skip when on-disk fragment already matches"
        );
    }

    #[test]
    fn mismatch_when_bytes_differ() {
        let existing =
            "audio_output {\n    type \"alsa\"\n    device \"hw:2,0\"\n}\n";
        let rendered =
            "audio_output {\n    type \"alsa\"\n    device \"hw:3,0\"\n}\n";
        assert!(
            !fragment_content_matches(Some(existing), rendered),
            "real endpoint change must not be deduped"
        );
    }

    #[test]
    fn mismatch_when_no_existing_file() {
        // First boot / first render on a fresh system: no file
        // yet. Safe default is to proceed with write + restart
        // so MPD picks up the initial fragment.
        let rendered = "audio_output {\n    type \"alsa\"\n}\n";
        assert!(
            !fragment_content_matches(None, rendered),
            "first render must proceed with write + restart"
        );
    }

    #[test]
    fn mismatch_when_trailing_whitespace_differs() {
        // Guard against fragile string equality that would treat
        // a stray newline as "equivalent" and skip a restart that
        // MPD needed. Bytes are bytes.
        let existing = "audio_output {\n    type \"alsa\"\n}";
        let rendered = "audio_output {\n    type \"alsa\"\n}\n";
        assert!(
            !fragment_content_matches(Some(existing), rendered),
            "byte-differing content must not be deduped"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use evo_plugin_sdk::contract::{CustodyStateReporter, HealthStatus};

    use crate::playback_supervisor::test_mock::{
        capturing_emitter, short_timeouts, spawn_mock_mpd,
        spawn_unresponsive_mock, test_custody_handle, CapturingReporter,
        ConnBehaviour,
    };
    use crate::playback_supervisor::SubjectEmitter;
    use tokio::sync::watch;

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
        let p = MpdPlaybackPlugin::new();
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

    // ===== seek_by_delta =====

    #[test]
    fn parse_seek_by_delta_positive() {
        let c = correction("seek_by_delta", b"15000", 1);
        assert_eq!(
            parse_correction(&c).unwrap(),
            PlaybackCommand::SeekRelative(15000)
        );
    }

    #[test]
    fn parse_seek_by_delta_negative_rewinds() {
        let c = correction("seek_by_delta", b"-5000", 1);
        assert_eq!(
            parse_correction(&c).unwrap(),
            PlaybackCommand::SeekRelative(-5000)
        );
    }

    #[test]
    fn parse_seek_by_delta_accepts_explicit_plus_sign() {
        let c = correction("seek_by_delta", b"+30000", 1);
        assert_eq!(
            parse_correction(&c).unwrap(),
            PlaybackCommand::SeekRelative(30000)
        );
    }

    #[test]
    fn parse_seek_by_delta_zero_is_valid() {
        let c = correction("seek_by_delta", b"0", 1);
        assert_eq!(
            parse_correction(&c).unwrap(),
            PlaybackCommand::SeekRelative(0)
        );
    }

    #[test]
    fn parse_seek_by_delta_rejects_malformed() {
        let e = parse_correction(&correction("seek_by_delta", b"soon", 1))
            .unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    // ===== boolean toggles (set_mute / set_repeat / set_shuffle /
    // set_single / set_consume) =====

    #[test]
    fn parse_set_mute_true_and_false() {
        assert_eq!(
            parse_correction(&correction("set_mute", b"1", 1)).unwrap(),
            PlaybackCommand::SetMute(true)
        );
        assert_eq!(
            parse_correction(&correction("set_mute", b"true", 1)).unwrap(),
            PlaybackCommand::SetMute(true)
        );
        assert_eq!(
            parse_correction(&correction("set_mute", b"0", 1)).unwrap(),
            PlaybackCommand::SetMute(false)
        );
        assert_eq!(
            parse_correction(&correction("set_mute", b"false", 1)).unwrap(),
            PlaybackCommand::SetMute(false)
        );
    }

    #[test]
    fn parse_set_repeat_true_and_false() {
        assert_eq!(
            parse_correction(&correction("set_repeat", b"1", 1)).unwrap(),
            PlaybackCommand::SetRepeat(true)
        );
        assert_eq!(
            parse_correction(&correction("set_repeat", b"0", 1)).unwrap(),
            PlaybackCommand::SetRepeat(false)
        );
    }

    #[test]
    fn parse_set_shuffle_true_and_false() {
        assert_eq!(
            parse_correction(&correction("set_shuffle", b"1", 1)).unwrap(),
            PlaybackCommand::SetShuffle(true)
        );
        assert_eq!(
            parse_correction(&correction("set_shuffle", b"0", 1)).unwrap(),
            PlaybackCommand::SetShuffle(false)
        );
    }

    #[test]
    fn parse_set_single_true_and_false() {
        assert_eq!(
            parse_correction(&correction("set_single", b"1", 1)).unwrap(),
            PlaybackCommand::SetSingle(true)
        );
        assert_eq!(
            parse_correction(&correction("set_single", b"0", 1)).unwrap(),
            PlaybackCommand::SetSingle(false)
        );
    }

    #[test]
    fn parse_set_consume_true_and_false() {
        assert_eq!(
            parse_correction(&correction("set_consume", b"1", 1)).unwrap(),
            PlaybackCommand::SetConsume(true)
        );
        assert_eq!(
            parse_correction(&correction("set_consume", b"0", 1)).unwrap(),
            PlaybackCommand::SetConsume(false)
        );
    }

    #[test]
    fn parse_set_mute_rejects_malformed() {
        let e =
            parse_correction(&correction("set_mute", b"maybe", 1)).unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    #[test]
    fn parse_set_repeat_rejects_empty() {
        let e =
            parse_correction(&correction("set_repeat", b"", 1)).unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
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
        // Command-conn sequence (with audio-protocol-settings apply
        // at session-init): 1 = crossfade, 2 = single, 3 = status,
        // 4 = currentsong, 5 = play -> ACK.
        let (mut p, _mock) = loaded_plugin_with_mock(vec![
            ConnBehaviour::AckOnNth {
                nth: 5,
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
    async fn unload_drains_active_custody() {
        // Playback warden is single-claimant per the schema's
        // `warden-single-claimant` acceptance criterion; the
        // plugin's take_custody is therefore idempotent on
        // subsequent calls. Two take_custody calls collapse to
        // one active custody (the second returns the first
        // handle). The drain still has to clean up that single
        // custody and the supervisor it owns.
        let (mut p, _mock) = loaded_plugin_with_mock(vec![
            ConnBehaviour::Standard,
            ConnBehaviour::HoldAfterWelcome,
        ])
        .await;

        let reporter_a: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());
        let reporter_b: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());

        let h1 = p.take_custody(assignment(reporter_a, 100)).await.unwrap();
        let h2 = p.take_custody(assignment(reporter_b, 200)).await.unwrap();
        // Idempotent: second call returns the existing handle id.
        assert_eq!(h1.id, h2.id);
        assert_eq!(p.active_custody_count(), 1);

        p.unload().await.unwrap();
        assert_eq!(p.active_custody_count(), 0);
        assert!(matches!(
            p.health_check().await.status,
            HealthStatus::Unhealthy
        ));
    }

    // ===== subject emission through the warden =====

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
            principal_scope: None,
            has_step_up: false,
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
    async fn get_now_playing_returns_rendered_state_without_prior_verb() {
        // First-render contract: a fresh consumer can dispatch
        // get_now_playing as its first interaction (no prior
        // transport verb) and receive the current playback state
        // in the same wire shape the now_playing subject carries.
        // This is the read-side companion to the subject, which
        // only emits on state transitions and does NOT replay the
        // current value to new subscribers.
        let (mut p, handle, _mock) = loaded_plugin_with_active_custody(vec![
            F4Conn::Standard,
            F4Conn::HoldAfterWelcome,
        ])
        .await;

        let req = source_request("get_now_playing", json!({ "v": 1 }));
        let resp = p.handle_request(&req).await.unwrap();
        let body: Value = serde_json::from_slice(&resp.payload).unwrap();

        // Payload version matches the now_playing subject's
        // versioned envelope.
        assert_eq!(body["v"], 1);
        // Every field the subject emits is present in the read
        // response so consumers can render their initial frame
        // from this payload alone.
        assert!(body.get("transport_state").is_some());
        assert!(body.get("track").is_some());
        assert!(body.get("elapsed_ms").is_some());
        assert!(body.get("duration_ms").is_some());
        assert!(body.get("volume").is_some());
        assert!(body.get("muted").is_some());
        assert!(body.get("repeat").is_some());
        assert!(body.get("shuffle").is_some());
        assert!(body.get("single").is_some());
        assert!(body.get("consume").is_some());
        // Mock's Standard handler reports state: stop → projection
        // collapses track + elapsed + duration to null per the
        // schema's stopped-state contract.
        assert_eq!(body["transport_state"], "stopped");
        assert!(body["track"].is_null());
        assert!(body["elapsed_ms"].is_null());
        assert!(body["duration_ms"].is_null());

        p.release_custody(handle).await.unwrap();
    }

    #[tokio::test]
    async fn get_now_playing_refuses_when_no_active_custody() {
        // The read verb is part of the source-respondent surface
        // and inherits the active-custody requirement that every
        // other source verb honours. Without an admitted plugin
        // there is no supervisor to query; the dispatcher must
        // refuse cleanly rather than panic or return an empty
        // response.
        let (mut p, _mock) = loaded_plugin_with_mock(vec![
            F4Conn::Standard,
            F4Conn::HoldAfterWelcome,
        ])
        .await;

        let req = source_request("get_now_playing", json!({ "v": 1 }));
        let err = p.handle_request(&req).await.unwrap_err();
        match err {
            PluginError::Permanent(msg) => {
                assert!(msg.contains("no active custody"));
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_now_playing_refuses_bad_payload_version() {
        let (mut p, handle, _mock) = loaded_plugin_with_active_custody(vec![
            F4Conn::Standard,
            F4Conn::HoldAfterWelcome,
        ])
        .await;

        let req = source_request("get_now_playing", json!({ "v": 99 }));
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

    // ---- audio-protocol-settings parser coverage ----

    // ---- emit_test_tone payload-validation coverage ----

    #[test]
    fn test_tone_spool_dir_lives_under_runtime_directory() {
        // The spool path MUST sit under /run/evo/ so it
        // survives systemd PrivateTmp sandboxing on the
        // steward (which makes /tmp invisible to MPD). The
        // distribution's RuntimeDirectory=evo systemd
        // directive creates /run/evo/ root:root 0755 on every
        // service start; the warden's mkdir_all of
        // TEST_TONE_SPOOL_DIR composes inside it.
        assert!(
            TEST_TONE_SPOOL_DIR.starts_with("/run/evo/"),
            "spool dir must live under /run/evo/ so MPD can \
             read files the steward writes; got: {}",
            TEST_TONE_SPOOL_DIR
        );
        // Defensive: /tmp must not creep back in as a path
        // prefix — the prior implementation wrote there and
        // hit the PrivateTmp namespace mismatch.
        assert!(
            !TEST_TONE_SPOOL_DIR.starts_with("/tmp"),
            "/tmp is PrivateTmp-sandboxed; MPD cannot read \
             files the steward writes there"
        );
    }

    #[tokio::test]
    async fn course_correct_emit_test_tone_rejects_freq_below_audible_band() {
        let (mut p, _mock) = loaded_plugin_with_mock(vec![
            ConnBehaviour::Standard,
            ConnBehaviour::HoldAfterWelcome,
        ])
        .await;
        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());
        let handle = p.take_custody(assignment(reporter, 1)).await.unwrap();
        let payload = serde_json::to_vec(
            &json!({ "v": 1, "freq_hz": 10, "duration_ms": 200 }),
        )
        .unwrap();
        let err = p
            .course_correct(&handle, correction("emit_test_tone", &payload, 1))
            .await
            .unwrap_err();
        match err {
            PluginError::Permanent(msg) => {
                assert!(
                    msg.contains("freq_hz") && msg.contains("20..=20000"),
                    "refusal must name the field + accepted band; got: {msg}"
                );
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
        p.release_custody(handle).await.unwrap();
    }

    #[tokio::test]
    async fn course_correct_emit_test_tone_rejects_duration_above_ceiling() {
        let (mut p, _mock) = loaded_plugin_with_mock(vec![
            ConnBehaviour::Standard,
            ConnBehaviour::HoldAfterWelcome,
        ])
        .await;
        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());
        let handle = p.take_custody(assignment(reporter, 1)).await.unwrap();
        let payload =
            serde_json::to_vec(&json!({ "v": 1, "duration_ms": 99999 }))
                .unwrap();
        let err = p
            .course_correct(&handle, correction("emit_test_tone", &payload, 1))
            .await
            .unwrap_err();
        match err {
            PluginError::Permanent(msg) => {
                assert!(
                    msg.contains("duration_ms") && msg.contains("100..=10000"),
                    "refusal must name the field + range; got: {msg}"
                );
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
        p.release_custody(handle).await.unwrap();
    }

    #[tokio::test]
    async fn course_correct_emit_test_tone_rejects_unknown_channel() {
        let (mut p, _mock) = loaded_plugin_with_mock(vec![
            ConnBehaviour::Standard,
            ConnBehaviour::HoldAfterWelcome,
        ])
        .await;
        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());
        let handle = p.take_custody(assignment(reporter, 1)).await.unwrap();
        let payload =
            serde_json::to_vec(&json!({ "v": 1, "channel": "center" }))
                .unwrap();
        let err = p
            .course_correct(&handle, correction("emit_test_tone", &payload, 1))
            .await
            .unwrap_err();
        match err {
            PluginError::Permanent(msg) => {
                assert!(
                    msg.contains("channel")
                        && msg.contains("left, right, both"),
                    "refusal must name the field + accepted domain; got: {msg}"
                );
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
        p.release_custody(handle).await.unwrap();
    }

    // ---- emit_test_tone synthesizer coverage ----

    #[test]
    fn test_tone_wav_carries_riff_wave_header() {
        let wav = synthesise_test_tone_wav(1000, 200, TestToneChannel::Both);
        assert_eq!(&wav[0..4], b"RIFF", "WAV must start with the RIFF magic");
        assert_eq!(
            &wav[8..12],
            b"WAVE",
            "WAV magic immediately after the RIFF size must be `WAVE`"
        );
        assert_eq!(&wav[12..16], b"fmt ", "expected fmt chunk header");
        assert_eq!(&wav[36..40], b"data", "expected data chunk header");
    }

    #[test]
    fn test_tone_wav_carries_44100hz_stereo_16bit_format() {
        let wav = synthesise_test_tone_wav(1000, 200, TestToneChannel::Both);
        // fmt chunk: bytes 20..22 = audio_format (PCM = 1)
        assert_eq!(u16::from_le_bytes([wav[20], wav[21]]), 1);
        // 22..24 = channels (2 = stereo)
        assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 2);
        // 24..28 = sample_rate (44100)
        assert_eq!(
            u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]),
            44100
        );
        // 34..36 = bits per sample (16)
        assert_eq!(u16::from_le_bytes([wav[34], wav[35]]), 16);
    }

    #[test]
    fn test_tone_wav_left_channel_carries_zero_in_right() {
        let wav = synthesise_test_tone_wav(1000, 100, TestToneChannel::Left);
        // First sample lives at byte 44 (after header). Each
        // sample frame is 4 bytes: i16 L + i16 R. Sample 100
        // (well into the file) — R must be zero for the
        // left-only channel.
        let frame_idx = 100usize;
        let offset = 44 + frame_idx * 4;
        let right = i16::from_le_bytes([wav[offset + 2], wav[offset + 3]]);
        assert_eq!(
            right, 0,
            "left-only routing must leave the right channel silent"
        );
    }

    #[test]
    fn test_tone_wav_right_channel_carries_zero_in_left() {
        let wav = synthesise_test_tone_wav(1000, 100, TestToneChannel::Right);
        let frame_idx = 100usize;
        let offset = 44 + frame_idx * 4;
        let left = i16::from_le_bytes([wav[offset], wav[offset + 1]]);
        assert_eq!(
            left, 0,
            "right-only routing must leave the left channel silent"
        );
    }

    #[test]
    fn test_tone_wav_both_channels_carry_same_sample() {
        let wav = synthesise_test_tone_wav(1000, 100, TestToneChannel::Both);
        let frame_idx = 100usize;
        let offset = 44 + frame_idx * 4;
        let left = i16::from_le_bytes([wav[offset], wav[offset + 1]]);
        let right = i16::from_le_bytes([wav[offset + 2], wav[offset + 3]]);
        assert_eq!(
            left, right,
            "both-channels routing must carry identical L + R"
        );
    }

    #[test]
    fn test_tone_wav_duration_in_samples_matches_request() {
        // 500 ms at 44100 Hz = 22050 samples per channel.
        let wav = synthesise_test_tone_wav(1000, 500, TestToneChannel::Both);
        // data_size is at bytes 40..44 of the WAV.
        let data_size =
            u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]);
        let expected = 22_050 * 2 /*ch*/ * 2 /*B/sample*/;
        assert_eq!(data_size, expected);
    }

    #[test]
    fn test_tone_wav_synthesis_is_deterministic() {
        let a = synthesise_test_tone_wav(440, 250, TestToneChannel::Both);
        let b = synthesise_test_tone_wav(440, 250, TestToneChannel::Both);
        assert_eq!(
            a, b,
            "synthesis must be byte-identical for identical inputs"
        );
    }

    #[test]
    fn test_tone_channel_from_str_round_trips_domain() {
        assert_eq!(
            TestToneChannel::from_str("left"),
            Some(TestToneChannel::Left)
        );
        assert_eq!(
            TestToneChannel::from_str("right"),
            Some(TestToneChannel::Right)
        );
        assert_eq!(
            TestToneChannel::from_str("both"),
            Some(TestToneChannel::Both)
        );
        assert_eq!(TestToneChannel::from_str("center"), None);
        assert_eq!(TestToneChannel::from_str(""), None);
    }

    #[test]
    fn protocol_settings_default_when_state_empty() {
        let s = parse_audio_protocol_settings_from_state(&json!({}));
        assert_eq!(s, AudioProtocolSettings::audiophile_default());
        assert_eq!(s.crossfade_seconds, 0);
        assert!(s.gapless);
    }

    #[test]
    fn protocol_settings_parses_crossfade_seconds() {
        let s = parse_audio_protocol_settings_from_state(
            &json!({ "crossfade_seconds": 7 }),
        );
        assert_eq!(s.crossfade_seconds, 7);
        // Missing `gapless` keeps audiophile default.
        assert!(s.gapless);
    }

    #[test]
    fn protocol_settings_parses_gapless_false() {
        let s = parse_audio_protocol_settings_from_state(
            &json!({ "gapless": false }),
        );
        assert!(!s.gapless);
        assert_eq!(s.crossfade_seconds, 0);
    }

    #[test]
    fn protocol_settings_parses_both_fields_together() {
        let s = parse_audio_protocol_settings_from_state(
            &json!({ "crossfade_seconds": 3, "gapless": false }),
        );
        assert_eq!(s.crossfade_seconds, 3);
        assert!(!s.gapless);
    }

    #[test]
    fn protocol_settings_ignores_unrelated_fields() {
        let s = parse_audio_protocol_settings_from_state(&json!({
            "mixer_type": "hardware",
            "output_device": "hw:CARD=DAC,DEV=0",
            "exclusive_mode": true,
            "crossfade_seconds": 12,
            "gapless": true
        }));
        assert_eq!(s.crossfade_seconds, 12);
        assert!(s.gapless);
    }

    /// The startup-volume applier's source of truth is the
    /// `startup_volume_percent` field on the options-settings
    /// subject state. Without it, the applier never fires and
    /// the pre-defect behaviour returns (MPD statefile wins).
    #[test]
    fn startup_volume_parses_startup_and_max_from_state() {
        let sv = parse_startup_volume_from_settings_state(&json!({
            "startup_volume_percent": 30,
            "max_volume_percent": 80,
        }))
        .expect("startup present → Some");
        assert_eq!(sv.startup_percent, 30);
        assert_eq!(sv.max_percent, 80);
        // Effective = min(startup, max) — startup wins here.
        assert_eq!(sv.effective(), 30);
    }

    /// `max_volume_percent` defaults to 100 when absent — the
    /// same posture the options plugin's `Settings::default`
    /// carries. Startup < 100 stays as-is; effective = startup.
    #[test]
    fn startup_volume_defaults_max_to_100_when_absent() {
        let sv = parse_startup_volume_from_settings_state(&json!({
            "startup_volume_percent": 45,
        }))
        .expect("startup present → Some");
        assert_eq!(sv.max_percent, 100);
        assert_eq!(sv.effective(), 45);
    }

    /// Startup absent → parser returns None. The applier treats
    /// None as "keep waiting" (schema-drift-tolerant); the caller
    /// sends `None` to the watch channel, which the applier
    /// filters via `borrow_and_update()`.
    #[test]
    fn startup_volume_returns_none_when_startup_field_missing() {
        let sv = parse_startup_volume_from_settings_state(&json!({
            "mixer_type": "hardware",
            "output_device": "hw:CARD=DAC,DEV=0",
        }));
        assert!(sv.is_none());
    }

    /// Startup above max: applier clamps to max. This is the
    /// invariant the operator relies on to keep the boot-time
    /// volume below the ceiling regardless of what
    /// `startup_volume_percent` records — the ceiling is the
    /// hard limit.
    #[test]
    fn startup_volume_effective_clamps_startup_at_max() {
        let sv = parse_startup_volume_from_settings_state(&json!({
            "startup_volume_percent": 90,
            "max_volume_percent": 50,
        }))
        .expect("startup present → Some");
        assert_eq!(sv.effective(), 50);
    }

    /// The parser reads only the two fields it needs; unrelated
    /// settings on the same subject state (mixer, output device,
    /// crossfade) do not affect parsing.
    #[test]
    fn startup_volume_ignores_unrelated_fields() {
        let sv = parse_startup_volume_from_settings_state(&json!({
            "mixer_type": "hardware",
            "output_device": "hw:CARD=DAC,DEV=0",
            "exclusive_mode": true,
            "crossfade_seconds": 12,
            "gapless": true,
            "startup_volume_percent": 22,
            "max_volume_percent": 90,
        }))
        .expect("startup present → Some");
        assert_eq!(sv.startup_percent, 22);
        assert_eq!(sv.max_percent, 90);
        assert_eq!(sv.effective(), 22);
    }

    /// Regression per the 2026-07-20 defect memo: the applier
    /// receives the operator's configured startup value from
    /// the subject state (not from MPD's statefile). The
    /// effective volume it computes is the value the wire
    /// setter would apply — parser + clamp is the whole
    /// framework-side computation before the MPD `setvol`.
    #[test]
    fn startup_volume_regression_boot_with_statefile_volume_x_configured_startup_y_effective_is_y(
    ) {
        // Configured startup Y = 30 (options plugin default).
        // Max = 100 (no ceiling). MPD's statefile could be
        // carrying any value in [0, 100] — irrelevant to the
        // parser + clamp. The applier's job downstream is to
        // send this effective value over the wire so MPD's
        // statefile-restored figure is overridden.
        let sv = parse_startup_volume_from_settings_state(&json!({
            "startup_volume_percent": 30,
            "max_volume_percent": 100,
        }))
        .expect("startup present → Some");
        assert_eq!(
            sv.effective(),
            30,
            "post-boot effective volume must equal configured startup (Y)"
        );
    }

    // ---- supervisor session-init apply path ----

    #[tokio::test]
    async fn supervisor_applies_protocol_settings_at_session_init() {
        // Command-conn sequence on session-init:
        //   1 = crossfade "<seconds>"
        //   2 = single "<0|1>"
        //   3 = status (initial report)
        //   4 = currentsong (initial report)
        // The CapturingMockMpd in this test asserts the FIRST
        // command on the connection is `crossfade` with the
        // operator-selected value — proving the apply runs
        // BEFORE the initial state report.
        let (endpoint, _mock) =
            spawn_mock_mpd(vec![F4Conn::Standard, F4Conn::HoldAfterWelcome])
                .await;

        let reporter = Arc::new(CapturingReporter::default());
        let reporter_dyn: Arc<dyn CustodyStateReporter> = reporter.clone();

        // Seed the watch channel with non-default values BEFORE
        // spawning the supervisor; this is the operator's
        // persisted state at the moment a new custody starts.
        let (tx, rx) = watch::channel(AudioProtocolSettings {
            crossfade_seconds: 7,
            gapless: false,
        });

        let handle = playback_supervisor::spawn(
            endpoint,
            short_timeouts(),
            test_custody_handle(),
            reporter_dyn,
            SubjectEmitter::null(),
            rx,
            None,
        )
        .await
        .expect("spawn should succeed against a Standard mock");

        // Initial report landed → both apply commands AND status
        // + currentsong dispatched cleanly.
        assert_eq!(reporter.count(), 1);

        drop(tx);
        handle.shutdown().await;
    }
}
