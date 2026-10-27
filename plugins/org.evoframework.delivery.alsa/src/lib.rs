// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! # org-evoframework-delivery-alsa
//!
//! Delivery-stage plugin for the evo audio data plane. Owns the
//! modular ALSA pipeline (`pcm.evo`) and declares it as the
//! [`WriteEndpoint`] other audio-producing plugins write into.
//! Stocks the `audio.delivery` shelf at shape 2.
//!
//! Plugin-local modular contract reference:
//! `plugins/org.evoframework.delivery.alsa/docs/MODULAR_ALSA_PLUGIN_CONTRACT.md`.
//! Device-tier canonical standard:
//! `dist/MODULAR_ALSA_PIPELINE_STANDARD.md`.
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

mod eq_claim;
mod logical_outputs;
mod options_render;
mod outputs;
mod scenes;
mod zones;

use std::future::Future;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use eq_claim::EqClaim;
use evo_plugin_sdk::contract::audio_routing::{
    AudioRouting, AudioRoutingError, ReadEndpoint, RouteChange,
    RouteChangeCallback,
};
use evo_plugin_sdk::contract::{
    BuildInfo, ExternalAddressing, HappeningEmitter, HealthReport, LoadContext,
    Plugin, PluginDescription, PluginError, PluginIdentity, Request,
    Respondent, Response, RuntimeCapabilities, SubjectAnnouncement,
    SubjectAnnouncer, SubjectStateStreamError,
};
use evo_plugin_sdk::Manifest;
use logical_outputs::{LogicalOutput, LogicalOutputStore, LogicalOutputs};
use options_render::OptionsSettings;
use outputs::{
    Effect, GlobalMixer, OutputPolicy, OutputSnapshot, OutputStateStore,
};
use scenes::{Scene, SceneStore, Scenes};
use zones::{Zone, ZoneStore, Zones};

pub mod alsa_cards;
pub mod import;
pub mod kernel_introspection;
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
    "delivery.enable_output",
    "delivery.disable_output",
    "delivery.set_output_volume",
    "delivery.play_output",
    "delivery.pause_output",
    "delivery.add_logical_output",
    "delivery.update_logical_output",
    "delivery.remove_logical_output",
    "delivery.list_logical_outputs",
    "zone.add",
    "zone.update",
    "zone.remove",
    "zone.list",
    "zone.set_eq_engaged",
    "zone.set_eq_band",
    "scene.save",
    "scene.list",
    "scene.recall",
    "scene.remove",
];

/// Subject scheme + value for the resolved-outputs surface.
/// Published once at load and read back by the
/// `delivery.list_outputs` respondent verb. Hot-plug re-publish
/// and reactive subscription on hardware change are out of
/// scope of the load-time enumeration this surface owns; the
/// load-time publish is the canonical boot-state surface UI
/// consumes today.
const SUBJECT_SCHEME_DELIVERY: &str = "evo.audio.delivery";
const SUBJECT_VALUE_OUTPUTS: &str = "outputs";
const SUBJECT_TYPE_OUTPUTS: &str = "audio_delivery_outputs";

/// Subject scheme + value for the zone EQ claim: which zone's EQ
/// the one pipe should sound, if any. Announced at load and
/// updated whenever the playing terminus, a zone's EQ or
/// membership, or a logical output's target changes. The
/// composition plugin subscribes; a missing subject means it
/// keeps using the options EQ.
const SUBJECT_SCHEME_ZONES: &str = "evo.audio.zones";
const SUBJECT_VALUE_EQ_CLAIM: &str = "eq_claim";
const SUBJECT_TYPE_EQ_CLAIM: &str = "audio_zone_eq_claim";

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
    requests_handled: std::sync::atomic::AtomicU64,
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
    /// `pcm.evo` drop-in via [`DropInRenderer`],
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
    /// hot-plug re-enumeration is out of scope of this load-time
    /// surface.
    cached_outputs: Arc<RwLock<Vec<ResolvedAlsaOutput>>>,
    /// Per-output state and the chain binding the five output
    /// verbs steer. Shared with the drop-in renderer so the
    /// options observer renders through the same binding.
    outputs: Arc<RwLock<OutputPolicy>>,
    /// The operator's settings as last observed on the options
    /// subject, with the subject's canonical id. The output verbs
    /// render from these; `None` before the first update renders
    /// the bootstrap defaults.
    last_options: Arc<RwLock<Option<LastOptions>>>,
    /// The one lock every drop-in write takes. The options
    /// observer and the output verbs both render through
    /// [`DropInRenderer`]; this is what makes them one writer.
    drop_in_writer: Arc<tokio::sync::Mutex<()>>,
    /// Happening emitter captured at load; `None` before load, so
    /// renders triggered without a load context write the file
    /// and emit nothing.
    happening_emitter: Option<Arc<dyn HappeningEmitter>>,
    /// Persistence for enabled flags and volumes. `Some` after
    /// load named the state directory.
    outputs_state: Option<OutputStateStore>,
    /// How `delivery.set_output_volume` reaches the card's mixer.
    /// `amixer` in production; a recorder in tests.
    mixer: Arc<dyn MixerSetter>,
    /// Named outputs: a name the operator gave to one physical
    /// output, with a stable minted id.
    logical: Arc<RwLock<LogicalOutputs>>,
    /// Persistence for the named outputs. `Some` after load
    /// named the state directory.
    logical_state: Option<LogicalOutputStore>,
    /// Zones: a name over one or more logical outputs, with a
    /// stable minted id.
    zones: Arc<RwLock<Zones>>,
    /// Persistence for the zones. `Some` after load named the
    /// state directory.
    zones_state: Option<ZoneStore>,
    /// Scenes: a name and a snapshot of every zone.
    scenes: Arc<RwLock<Scenes>>,
    /// Persistence for the scenes. `Some` after load named the
    /// state directory.
    scenes_state: Option<SceneStore>,
    /// The last zone EQ claim published, so an unchanged claim is
    /// not republished.
    eq_claim_published: Arc<RwLock<Option<serde_json::Value>>>,
}

/// The operator's options as last observed, and where from.
#[derive(Debug, Clone, PartialEq)]
struct LastOptions {
    settings: OptionsSettings,
    subject_canonical_id: String,
}

/// Sets one simple mixer control on one card. The only way the
/// plugin moves a volume.
trait MixerSetter: Send + Sync {
    /// `amixer -c <card_idx> sset <control> <percent>%`, or its
    /// test double. `Err` carries the reason the card refused.
    fn set_volume<'a>(
        &'a self,
        card_idx: u32,
        control: &'a str,
        percent: u8,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

/// The production mixer setter: one `amixer` call.
struct AmixerSetter;

impl MixerSetter for AmixerSetter {
    fn set_volume<'a>(
        &'a self,
        card_idx: u32,
        control: &'a str,
        percent: u8,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>
    {
        Box::pin(async move {
            // `-M` maps the percentage the way alsamixer shows
            // it, so 50 means half as loud, not half the raw
            // register range.
            let output = tokio::process::Command::new("amixer")
                .arg("-M")
                .arg("-c")
                .arg(card_idx.to_string())
                .arg("sset")
                .arg(control)
                .arg(format!("{percent}%"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output()
                .await
                .map_err(|e| format!("amixer could not run: {e}"))?;
            if output.status.success() {
                Ok(())
            } else {
                Err(format!(
                    "amixer -c {card_idx} sset {control:?} {percent}% \
                     failed ({}): {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                ))
            }
        })
    }
}

/// What one drop-in render did.
#[derive(Debug, Clone)]
struct RenderOutcome {
    /// The subject the rendered settings came from, when they
    /// came from one.
    subject_canonical_id: Option<String>,
    /// Bytes rendered.
    render_bytes: usize,
    /// `Some` when the file was written; `None` when the bytes on
    /// disk already matched, or could not be read.
    write_outcome: Option<Result<(), String>>,
    /// The drop-in could not be read to compare, so it was left
    /// alone and its freshness is unknown.
    read_failed: bool,
}

impl RenderOutcome {
    /// True when the drop-in on disk is what was just rendered,
    /// whether this render wrote it or found it already matching.
    /// A read that could not be performed leaves that unknown,
    /// and unknown is not ok.
    fn write_ok(&self) -> bool {
        match &self.write_outcome {
            Some(r) => r.is_ok(),
            None => !self.read_failed,
        }
    }
}

/// The one path that renders and writes the options drop-in.
/// The options observer and the output verbs both hold a clone;
/// every render reads the same last-seen settings and the same
/// output binding, and every write takes the same lock.
#[derive(Clone)]
struct DropInRenderer {
    drop_in_path: PathBuf,
    asound_conf_path: PathBuf,
    last_options: Arc<RwLock<Option<LastOptions>>>,
    outputs: Arc<RwLock<OutputPolicy>>,
    writer: Arc<tokio::sync::Mutex<()>>,
    emitter: Option<Arc<dyn HappeningEmitter>>,
}

impl DropInRenderer {
    /// Render the drop-in from the last-seen settings and the
    /// current binding; write it when the bytes differ from what
    /// is on disk.
    async fn render_and_write(&self) -> RenderOutcome {
        let (settings, subject_canonical_id) =
            match self.last_options.read().await.as_ref() {
                Some(last) => (
                    last.settings.clone(),
                    Some(last.subject_canonical_id.clone()),
                ),
                None => (OptionsSettings::default(), None),
            };
        let binding = self.outputs.read().await.binding().clone();
        let render_target = AlsaDeliveryPlugin::detect_options_render_target(
            &self.asound_conf_path,
        );
        let body = options_render::render_drop_in_bound(
            &settings,
            render_target,
            &binding,
        );
        let render_bytes = body.len();
        let drop_in_path = &self.drop_in_path;

        // One writer: compare and write under the lock, so an
        // options update and an output verb cannot interleave
        // their compare-then-write.
        let _writing = self.writer.lock().await;
        // Compare before rewriting. Every rewrite is an
        // operator-visible "the pipeline definition changed"
        // happening and a reason for consumers to re-read; an
        // update that left this file alone should not look like
        // one that changed it. A read that fails for any reason
        // other than absence leaves the file alone, because
        // without the current bytes there is no evidence a
        // rewrite is needed.
        let mut read_failed = false;
        let rewrite =
            match options_render::drop_in_needs_rewrite(drop_in_path, &body)
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        plugin = PLUGIN_NAME,
                        drop_in_path = %drop_in_path.display(),
                        error = %e,
                        "audio-options drop-in could not be read; prior \
                         pipeline retained. LOGGING.md §2: the on-disk \
                         pipeline may be stale and this names why."
                    );
                    read_failed = true;
                    false
                }
            };
        if !rewrite {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                drop_in_path = %drop_in_path.display(),
                "audio-options drop-in unchanged; not rewritten"
            );
        }
        let write_outcome = if rewrite {
            Some(
                options_render::atomic_write_drop_in(drop_in_path, &body)
                    .await
                    .map_err(|e| e.to_string()),
            )
        } else {
            None
        };
        match &write_outcome {
            // Nothing was written, so nothing is reported as
            // written.
            None => {}
            Some(Ok(())) => {
                // LOGGING.md §2: info (operator-visible lifecycle
                // narrative — the pcm.evo pipeline definition
                // changed on disk).
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    drop_in_path = %drop_in_path.display(),
                    render_bytes,
                    binding = ?binding,
                    "audio-options drop-in rewritten"
                );
            }
            Some(Err(e)) => {
                // LOGGING.md §2: warn (recoverable; the previous
                // drop-in remains on disk, the operator's prior
                // pipeline keeps audio flowing).
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    drop_in_path = %drop_in_path.display(),
                    error = %e,
                    "audio-options drop-in write failed; prior pipeline \
                     retained"
                );
            }
        }
        RenderOutcome {
            subject_canonical_id,
            render_bytes,
            write_outcome,
            read_failed,
        }
    }

    /// Emit `delivery.options_rendered` for a render: the on-disk
    /// drop-in's freshness signal subscribers serialise their
    /// re-bind work against.
    async fn emit_rendered(
        &self,
        outcome: &RenderOutcome,
        observed_at_ms: u64,
    ) {
        let Some(emitter) = self.emitter.as_ref() else {
            return;
        };
        let rendered_payload = serde_json::json!({
            "subject_canonical_id": outcome.subject_canonical_id,
            "observed_at_ms": observed_at_ms,
            "drop_in_path": self.drop_in_path.display().to_string(),
            "render_bytes": outcome.render_bytes,
            "write_ok": outcome.write_ok(),
        });
        if let Err(e) = emitter
            .emit_plugin_event(
                "delivery.options_rendered".to_string(),
                rendered_payload,
            )
            .await
        {
            // LOGGING.md §2: warn (recoverable; diagnostic
            // surface).
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "emit delivery.options_rendered failed"
            );
        }
    }
}

/// Publishes the zone EQ claim. Holds clones of every registry
/// the claim depends on, so a handler and an observer task can
/// both publish after their change; the last publish wins and
/// an unchanged claim is not republished.
#[derive(Clone)]
struct EqClaimPublisher {
    outputs: Arc<RwLock<OutputPolicy>>,
    cached_outputs: Arc<RwLock<Vec<ResolvedAlsaOutput>>>,
    last_options: Arc<RwLock<Option<LastOptions>>>,
    logical: Arc<RwLock<LogicalOutputs>>,
    zones: Arc<RwLock<Zones>>,
    announcer: Option<Arc<dyn SubjectAnnouncer>>,
    last_published: Arc<RwLock<Option<serde_json::Value>>>,
}

impl EqClaimPublisher {
    fn addressing() -> ExternalAddressing {
        ExternalAddressing {
            scheme: SUBJECT_SCHEME_ZONES.to_string(),
            value: SUBJECT_VALUE_EQ_CLAIM.to_string(),
        }
    }

    /// The claim as things stand: the terminus the chain ends at,
    /// the logical outputs that point at it, and the one engaged
    /// zone that holds one of them. Locks are taken one at a time
    /// and logical before zones.
    async fn current(&self) -> EqClaim {
        let follow = self
            .last_options
            .read()
            .await
            .as_ref()
            .and_then(|l| l.settings.output_device.clone())
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| {
                options_render::DEFAULT_OUTPUT_DEVICE.to_string()
            });
        let outputs = self.cached_outputs.read().await.clone();
        let terminus = self.outputs.read().await.terminus(&outputs, &follow);
        let logical = self.logical.read().await.list();
        let zones = self.zones.read().await.list();
        eq_claim::compute_claim(terminus.as_deref(), &logical, &zones)
    }

    /// Announce the subject with the current claim. Load only.
    async fn announce(&self) {
        let Some(announcer) = self.announcer.as_ref() else {
            return;
        };
        let state = self.current().await.to_wire();
        let announcement = SubjectAnnouncement {
            subject_type: SUBJECT_TYPE_EQ_CLAIM.to_string(),
            addressings: vec![Self::addressing()],
            claims: Vec::new(),
            state: state.clone(),
            announced_at: std::time::SystemTime::now(),
        };
        match announcer.announce(announcement).await {
            Ok(()) => {
                *self.last_published.write().await = Some(state);
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    "evo.audio.zones:eq_claim announced"
                );
            }
            Err(e) => tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "evo.audio.zones:eq_claim announce failed"
            ),
        }
    }

    /// Publish the current claim when it differs from the last
    /// one published.
    async fn publish(&self) {
        let Some(announcer) = self.announcer.as_ref() else {
            return;
        };
        let state = self.current().await.to_wire();
        {
            let mut last = self.last_published.write().await;
            if last.as_ref() == Some(&state) {
                return;
            }
            *last = Some(state.clone());
        }
        if let Err(e) = announcer.update_state(Self::addressing(), state).await
        {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "evo.audio.zones:eq_claim update_state failed"
            );
        } else {
            tracing::info!(
                plugin = PLUGIN_NAME,
                "evo.audio.zones:eq_claim updated"
            );
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
/// "currently playing through: `<name>` via pcm.evo" without
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
    /// Bus topology declared by the hardware.audio-config catalogue
    /// row for the active DAC. None when no catalogue entry matched
    /// the on-disk overlay (the operator wrote a custom overlay, or
    /// the catalogue has changed). Consumed by output-class
    /// derivation as the authoritative classification — when
    /// present, the row's `output_class` is set directly from
    /// this declared value with no fallback to card-name string
    /// heuristics.
    #[serde(default)]
    pub interface: Option<String>,
    /// Boot-config path the active overlay was read from. Empty
    /// on board classes without a boot-config path.
    #[serde(default)]
    pub boot_config_path: String,
}

impl AlsaDeliveryPlugin {
    /// Resolve which options-render target to use for the active
    /// on-disk ALSA topology.
    ///
    /// Source-host templates define `pcm.evo_local`; receiver /
    /// standalone templates do not. The observer uses this marker
    /// to decide whether operator options should rewrite the main
    /// producer PCM (`pcm.evo`) or only the source-local renderer
    /// PCM (`pcm.evo_local`).
    fn detect_options_render_target(
        asound_conf_path: &std::path::Path,
    ) -> options_render::RenderTarget {
        match std::fs::read_to_string(asound_conf_path) {
            Ok(body) => {
                if body.contains("pcm.evo_local") {
                    options_render::RenderTarget::SourceLocal
                } else {
                    options_render::RenderTarget::Main
                }
            }
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    asound_conf_path = %asound_conf_path.display(),
                    error = %e,
                    "could not read asound.conf while selecting options-render target; defaulting to main pipeline"
                );
                options_render::RenderTarget::Main
            }
        }
    }

    /// Construct a fresh plugin instance.
    pub fn new() -> Self {
        Self {
            loaded: false,
            audio_routing: None,
            asound_conf_path: PathBuf::from(ASOUND_CONF_PATH),
            asound_options_drop_in_path: PathBuf::from(
                ASOUND_OPTIONS_DROP_IN_PATH,
            ),
            requests_handled: std::sync::atomic::AtomicU64::new(0),
            reactor: None,
            options_observer: None,
            hardware_audio_observer: None,
            active_dac_config: Arc::new(RwLock::new(None)),
            card_catalog: None,
            subject_announcer: None,
            cached_outputs: Arc::new(RwLock::new(Vec::new())),
            outputs: Arc::new(RwLock::new(OutputPolicy::default())),
            last_options: Arc::new(RwLock::new(None)),
            drop_in_writer: Arc::new(tokio::sync::Mutex::new(())),
            happening_emitter: None,
            outputs_state: None,
            mixer: Arc::new(AmixerSetter),
            logical: Arc::new(RwLock::new(LogicalOutputs::default())),
            logical_state: None,
            zones: Arc::new(RwLock::new(Zones::default())),
            zones_state: None,
            scenes: Arc::new(RwLock::new(Scenes::default())),
            scenes_state: None,
            eq_claim_published: Arc::new(RwLock::new(None)),
        }
    }

    /// The claim publisher over this plugin's registries.
    fn eq_claim_publisher(&self) -> EqClaimPublisher {
        EqClaimPublisher {
            outputs: Arc::clone(&self.outputs),
            cached_outputs: Arc::clone(&self.cached_outputs),
            last_options: Arc::clone(&self.last_options),
            logical: Arc::clone(&self.logical),
            zones: Arc::clone(&self.zones),
            announcer: self.subject_announcer.clone(),
            last_published: Arc::clone(&self.eq_claim_published),
        }
    }

    /// Publish the zone EQ claim if it changed.
    async fn publish_eq_claim(&self) {
        self.eq_claim_publisher().publish().await;
    }

    /// The renderer every drop-in write goes through.
    fn drop_in_renderer(&self) -> DropInRenderer {
        DropInRenderer {
            drop_in_path: self.asound_options_drop_in_path.clone(),
            asound_conf_path: self.asound_conf_path.clone(),
            last_options: Arc::clone(&self.last_options),
            outputs: Arc::clone(&self.outputs),
            writer: Arc::clone(&self.drop_in_writer),
            emitter: self.happening_emitter.clone(),
        }
    }

    /// Point the outputs state file somewhere and read it. Load
    /// does this with the state directory; tests with a tempdir.
    async fn install_outputs_state(&mut self, path: PathBuf) {
        let store = OutputStateStore::at(path);
        let persisted = store.load().await;
        self.outputs.write().await.restore(&persisted);
        self.outputs_state = Some(store);
    }

    /// Point the logical outputs file somewhere and read it.
    async fn install_logical_outputs(&mut self, path: PathBuf) {
        let store = LogicalOutputStore::at(path);
        let persisted = store.load().await;
        *self.logical.write().await = LogicalOutputs::from_persisted(persisted);
        self.logical_state = Some(store);
    }

    /// Write the named outputs. A failed write is logged, not
    /// refused: the registry already moved and the answer says
    /// what it is now.
    async fn persist_logical_outputs(&self) {
        let Some(store) = self.logical_state.as_ref() else {
            return;
        };
        let logical = self.logical.read().await;
        if let Err(e) = store.save(logical.persisted()).await {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                path = %store.path().display(),
                error = %e,
                "logical outputs could not be written; names will not \
                 survive a restart"
            );
        }
    }

    /// Point the zones file somewhere and read it.
    async fn install_zones(&mut self, path: PathBuf) {
        let store = ZoneStore::at(path);
        let persisted = store.load().await;
        *self.zones.write().await = Zones::from_persisted(persisted);
        self.zones_state = Some(store);
    }

    /// Point the scenes file somewhere and read it.
    async fn install_scenes(&mut self, path: PathBuf) {
        let store = SceneStore::at(path);
        let persisted = store.load().await;
        *self.scenes.write().await = Scenes::from_persisted(persisted);
        self.scenes_state = Some(store);
    }

    /// Write the scenes. A failed write is logged, not refused.
    async fn persist_scenes(&self) {
        let Some(store) = self.scenes_state.as_ref() else {
            return;
        };
        let scenes = self.scenes.read().await;
        if let Err(e) = store.save(scenes.persisted()).await {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                path = %store.path().display(),
                error = %e,
                "scenes could not be written; scenes will not survive a \
                 restart"
            );
        }
    }

    /// Write the zones. A failed write is logged, not refused:
    /// the registry already moved and the answer says what it is
    /// now.
    async fn persist_zones(&self) {
        let Some(store) = self.zones_state.as_ref() else {
            return;
        };
        let zones = self.zones.read().await;
        if let Err(e) = store.save(zones.persisted()).await {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                path = %store.path().display(),
                error = %e,
                "zones could not be written; zones will not survive a \
                 restart"
            );
        }
    }

    /// Write the enabled flags and volumes. A failed write is
    /// logged, not refused: the verb's effect already happened.
    async fn persist_outputs(&self) {
        let Some(store) = self.outputs_state.as_ref() else {
            return;
        };
        let persisted = self.outputs.read().await.persisted();
        if let Err(e) = store.save(&persisted).await {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                path = %store.path().display(),
                error = %e,
                "outputs state could not be written; enabled flags and \
                 volumes will not survive a restart"
            );
        }
    }

    /// Let the chain follow the operator's settings again and
    /// render once if a binding was in force, so the disk never
    /// carries a binding past the plugin's lifetime.
    async fn release_output_binding(&self) {
        let released = self.outputs.write().await.release_binding();
        if !released {
            return;
        }
        let renderer = self.drop_in_renderer();
        let outcome = renderer.render_and_write().await;
        renderer.emit_rendered(&outcome, now_ms()).await;
        self.publish_eq_claim().await;
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

    /// Replace the mixer setter. Tests record the calls instead
    /// of running `amixer`.
    #[cfg(test)]
    pub(crate) fn with_mixer_setter(
        mut self,
        mixer: Arc<dyn MixerSetter>,
    ) -> Self {
        self.mixer = mixer;
        self
    }

    /// Cumulative `handle_request` invocations.
    pub fn requests_handled(&self) -> u64 {
        self.requests_handled
            .load(std::sync::atomic::Ordering::Relaxed)
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
        let renderer = self.drop_in_renderer();
        let eq_claim = self.eq_claim_publisher();
        let happening_emitter = Arc::clone(&ctx.happening_emitter);
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
                                let observed_at_ms = now_ms();

                                // Extract → remember → render →
                                // write. The observer-observed
                                // happening (kept for diagnostic-
                                // replay consumers) fires alongside
                                // the options_rendered happening
                                // (which signals subscribers that
                                // the on-disk pipeline definition
                                // has changed). Both happenings
                                // carry the canonical subject id
                                // so a single subscriber can
                                // correlate observer activity
                                // against pipeline rewrites. The
                                // render goes through the same
                                // DropInRenderer the output verbs
                                // use, so the output binding is
                                // honoured here and the file has
                                // one writer.
                                let settings_state =
                                    state_update.state.unwrap_or(
                                        serde_json::Value::Null,
                                    );
                                let settings =
                                    options_render::extract_options_settings_from_state(
                                        &settings_state,
                                    );
                                *renderer.last_options.write().await =
                                    Some(LastOptions {
                                        settings,
                                        subject_canonical_id:
                                            state_update.canonical_id.clone(),
                                    });
                                let outcome = renderer.render_and_write().await;

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

                                renderer.emit_rendered(&outcome, observed_at_ms).await;
                                // The operator's output_device
                                // is the terminus under Follow:
                                // which zone's EQ sounds may
                                // have changed with it.
                                eq_claim.publish().await;

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
        let eq_claim_for_task = self.eq_claim_publisher();
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
            // Seed the cache with the current state. The
            // subscribe path only delivers updates AFTER
            // subscription; if the upstream plugin announced its
            // initial active_config before we connected, the
            // stream has no historical replay and we would never
            // see the current value until the operator gestures
            // a `select_dac`. Pulling current_state once at
            // subscribe time closes that gap so the active-DAC
            // enrichment of the outputs subject fires
            // immediately.
            match subscriber_for_task
                .current_state(canonical_id.clone())
                .await
            {
                Ok(Some(state)) => {
                    let active = extract_active_dac_config(Some(&state));
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
                        "hardware-audio active_config seeded from current_state"
                    );
                    republish_outputs_enriched(
                        catalog_for_task.as_ref(),
                        announcer_for_task.as_ref(),
                        &cached_outputs_for_task,
                        Some(&active),
                    )
                    .await;
                    // The outputs list moved: the terminus may
                    // resolve differently now.
                    eq_claim_for_task.publish().await;
                }
                Ok(None) => {
                    tracing::debug!(
                        plugin = PLUGIN_NAME,
                        "hardware-audio active_config current_state empty; \
                         awaiting first update"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        plugin = PLUGIN_NAME,
                        error = %e,
                        "hardware-audio active_config current_state read \
                         errored; awaiting first update"
                    );
                }
            }
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
                                // The outputs list moved: the terminus may
                                // resolve differently now.
                                eq_claim_for_task.publish().await;
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
            self.happening_emitter = Some(Arc::clone(&ctx.happening_emitter));
            self.subject_announcer = Some(Arc::clone(&ctx.subject_announcer));
            // Enabled flags and volumes the output verbs set
            // last time around; the chain binding is not
            // persisted and starts as Follow.
            self.install_outputs_state(
                ctx.state_dir.join(OutputStateStore::FILE_NAME),
            )
            .await;
            self.install_logical_outputs(
                ctx.state_dir.join(LogicalOutputStore::FILE_NAME),
            )
            .await;
            self.install_zones(ctx.state_dir.join(ZoneStore::FILE_NAME))
                .await;
            self.install_scenes(ctx.state_dir.join(SceneStore::FILE_NAME))
                .await;
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
            // pcm.evo drop-in (via render_drop_in_bound +
            // atomic_write_drop_in, through the same
            // DropInRenderer the output verbs use) followed by the
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
            // Announce which zone's EQ the one pipe should sound,
            // now that the outputs, the names and the zones are
            // all in: the composition plugin subscribes to this.
            self.eq_claim_publisher().announce().await;
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
                requests_handled = self.requests_handled(),
                "plugin unload"
            );
            self.stop_reactor().await;
            self.stop_options_observer().await;
            self.stop_hardware_audio_observer().await;
            // The observer is stopped, so this is the last
            // render: the drop-in follows the operator's
            // settings again and carries no output binding
            // into the next load.
            self.release_output_binding().await;
            self.happening_emitter = None;
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
        &'a self,
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
            self.requests_handled
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
                "delivery.enable_output"
                | "delivery.disable_output"
                | "delivery.play_output"
                | "delivery.pause_output" => self.handle_output_verb(req).await,
                "delivery.set_output_volume" => {
                    self.handle_set_output_volume(req).await
                }
                "delivery.add_logical_output" => {
                    self.handle_add_logical_output(req).await
                }
                "delivery.update_logical_output" => {
                    self.handle_update_logical_output(req).await
                }
                "delivery.remove_logical_output" => {
                    self.handle_remove_logical_output(req).await
                }
                "delivery.list_logical_outputs" => {
                    self.handle_list_logical_outputs(req).await
                }
                "zone.add" => self.handle_zone_add(req).await,
                "zone.update" => self.handle_zone_update(req).await,
                "zone.remove" => self.handle_zone_remove(req).await,
                "zone.list" => self.handle_zone_list(req).await,
                "zone.set_eq_engaged" => {
                    self.handle_zone_set_eq_engaged(req).await
                }
                "zone.set_eq_band" => self.handle_zone_set_eq_band(req).await,
                "scene.save" => self.handle_scene_save(req).await,
                "scene.list" => self.handle_scene_list(req).await,
                "scene.recall" => self.handle_scene_recall(req).await,
                "scene.remove" => self.handle_scene_remove(req).await,
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
    /// is out of scope of this load-time surface; today's
    /// surface is the boot-state snapshot.
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

    /// The operator's `output_device` as last observed, or the
    /// bootstrap default: what the chain follows when no output
    /// verb has bound it.
    async fn follow_device(&self) -> String {
        self.last_options
            .read()
            .await
            .as_ref()
            .and_then(|l| l.settings.output_device.clone())
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| {
                options_render::DEFAULT_OUTPUT_DEVICE.to_string()
            })
    }

    /// `delivery.enable_output`, `delivery.disable_output`,
    /// `delivery.play_output`, `delivery.pause_output` — one
    /// output, named by the id `delivery.list_outputs` gave it.
    /// The policy decides; this runs the one effect it asks for
    /// (a drop-in render when the chain's terminus moved),
    /// persists what survives a restart, and answers with that
    /// output's state.
    async fn handle_output_verb(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: OutputRequest = parse_versioned_payload(req)?;
        let outputs = self.cached_outputs.read().await.clone();
        let follow = self.follow_device().await;
        let id = payload.output_id.as_str();
        let (decision, snapshot) = {
            let mut policy = self.outputs.write().await;
            let decision = match req.request_type.as_str() {
                "delivery.enable_output" => policy.enable(&outputs, id),
                "delivery.disable_output" => {
                    policy.disable(&outputs, &follow, id)
                }
                "delivery.play_output" => policy.play(&outputs, &follow, id),
                "delivery.pause_output" => policy.pause(&outputs, &follow, id),
                other => {
                    return Err(PluginError::Permanent(format!(
                        "{other:?} is not an output verb"
                    )))
                }
            };
            (decision, policy.snapshot(&outputs, &follow, id))
        };
        let effect = decision.map_err(|refusal| {
            PluginError::Permanent(format!("{}: {refusal}", req.request_type))
        })?;
        match req.request_type.as_str() {
            "delivery.enable_output" | "delivery.disable_output" => {
                self.persist_outputs().await;
            }
            _ => {}
        }
        if effect == Effect::Rebind {
            let renderer = self.drop_in_renderer();
            let outcome = renderer.render_and_write().await;
            renderer.emit_rendered(&outcome, now_ms()).await;
            // The terminus moved: which zone's EQ sounds may have.
            self.publish_eq_claim().await;
            if let Some(Err(e)) = &outcome.write_outcome {
                return Err(PluginError::Transient(format!(
                    "{}: the chain was rebound but the drop-in could not \
                     be written: {e}",
                    req.request_type
                )));
            }
        }
        tracing::info!(
            plugin = PLUGIN_NAME,
            verb = %req.request_type,
            output_id = id,
            enabled = snapshot.enabled,
            playing = snapshot.playing,
            "output verb applied"
        );
        encode(
            req,
            &OutputResponse {
                v: PAYLOAD_VERSION,
                output: snapshot,
            },
        )
    }

    /// `delivery.set_output_volume` — one `amixer` call against
    /// the output's own mixer control. Refused when the output
    /// is unknown, has no control, the percentage is out of
    /// range, or the control is the chain's hardware mixer (the
    /// global volume). The volume is recorded only once the card
    /// accepted it.
    async fn handle_set_output_volume(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: OutputVolumeRequest = parse_versioned_payload(req)?;
        let outputs = self.cached_outputs.read().await.clone();
        let follow = self.follow_device().await;
        let global = self
            .last_options
            .read()
            .await
            .as_ref()
            .and_then(|l| GlobalMixer::from_settings(&l.settings));
        let id = payload.output_id.as_str();
        let target = self
            .outputs
            .read()
            .await
            .volume_target(&outputs, global.as_ref(), id, payload.volume)
            .map_err(|refusal| {
                PluginError::Permanent(format!(
                    "{}: {refusal}",
                    req.request_type
                ))
            })?;
        self.mixer
            .set_volume(target.card_idx, &target.control, target.percent)
            .await
            .map_err(|e| {
                PluginError::Transient(format!("{}: {e}", req.request_type))
            })?;
        let snapshot = {
            let mut policy = self.outputs.write().await;
            policy.record_volume(id, target.percent);
            policy.snapshot(&outputs, &follow, id)
        };
        self.persist_outputs().await;
        tracing::info!(
            plugin = PLUGIN_NAME,
            output_id = id,
            card_idx = target.card_idx,
            control = %target.control,
            percent = target.percent,
            "output volume set"
        );
        encode(
            req,
            &OutputResponse {
                v: PAYLOAD_VERSION,
                output: snapshot,
            },
        )
    }

    fn logical_refused(
        req: &Request,
        r: logical_outputs::Refusal,
    ) -> PluginError {
        PluginError::Permanent(format!("{}: {r}", req.request_type))
    }

    /// `delivery.add_logical_output` — a name for one physical
    /// output `delivery.list_outputs` names. Answers the new row
    /// with its minted id.
    async fn handle_add_logical_output(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: AddLogicalOutputRequest = parse_versioned_payload(req)?;
        let outputs = self.cached_outputs.read().await.clone();
        let row = self
            .logical
            .write()
            .await
            .add(&outputs, &payload.name, &payload.output_id)
            .map_err(|r| Self::logical_refused(req, r))?;
        self.persist_logical_outputs().await;
        self.publish_eq_claim().await;
        tracing::info!(
            plugin = PLUGIN_NAME,
            id = %row.id,
            name = %row.name,
            output_id = %row.output_id,
            "logical output added"
        );
        encode(
            req,
            &LogicalOutputResponse {
                v: PAYLOAD_VERSION,
                logical_output: row,
            },
        )
    }

    /// `delivery.update_logical_output` — a new name, a new
    /// physical output, or both; the id stays. Answers the row
    /// as it is now.
    async fn handle_update_logical_output(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: UpdateLogicalOutputRequest = parse_versioned_payload(req)?;
        let outputs = self.cached_outputs.read().await.clone();
        let row = self
            .logical
            .write()
            .await
            .update(
                &outputs,
                &payload.id,
                payload.name.as_deref(),
                payload.output_id.as_deref(),
            )
            .map_err(|r| Self::logical_refused(req, r))?;
        self.persist_logical_outputs().await;
        self.publish_eq_claim().await;
        tracing::info!(
            plugin = PLUGIN_NAME,
            id = %row.id,
            name = %row.name,
            output_id = %row.output_id,
            "logical output updated"
        );
        encode(
            req,
            &LogicalOutputResponse {
                v: PAYLOAD_VERSION,
                logical_output: row,
            },
        )
    }

    /// `delivery.remove_logical_output` — the name taken away.
    /// Answers the row that was removed. The physical output is
    /// untouched. Refused, naming the zone, while a zone holds
    /// the id: the logical output, the zone and the physical
    /// output all stay. Locks are taken logical-then-zones, the
    /// order every zone verb uses.
    async fn handle_remove_logical_output(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: RemoveLogicalOutputRequest = parse_versioned_payload(req)?;
        let row = {
            let mut logical = self.logical.write().await;
            if !logical.has(&payload.id) {
                return Err(Self::logical_refused(
                    req,
                    logical_outputs::Refusal::UnknownLogicalOutput(
                        payload.id.clone(),
                    ),
                ));
            }
            let held_by = self
                .zones
                .read()
                .await
                .holder_of(&payload.id)
                .map(|z| z.id.clone());
            if let Some(zone) = held_by {
                return Err(Self::logical_refused(
                    req,
                    logical_outputs::Refusal::HeldByZone {
                        logical_output_id: payload.id.clone(),
                        zone,
                    },
                ));
            }
            logical
                .remove(&payload.id)
                .map_err(|r| Self::logical_refused(req, r))?
        };
        self.persist_logical_outputs().await;
        self.publish_eq_claim().await;
        tracing::info!(
            plugin = PLUGIN_NAME,
            id = %row.id,
            name = %row.name,
            "logical output removed"
        );
        encode(
            req,
            &LogicalOutputResponse {
                v: PAYLOAD_VERSION,
                logical_output: row,
            },
        )
    }

    /// `delivery.list_logical_outputs` — every named output, in
    /// id order. Reads the registry and changes nothing: no row,
    /// no file, no physical output.
    async fn handle_list_logical_outputs(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        parse_versioned_payload::<EmptyPayload>(req)?;
        let logical_outputs = self.logical.read().await.list();
        encode(
            req,
            &ListLogicalOutputsResponse {
                v: PAYLOAD_VERSION,
                logical_outputs,
            },
        )
    }

    fn zone_refused(req: &Request, r: zones::Refusal) -> PluginError {
        PluginError::Permanent(format!("{}: {r}", req.request_type))
    }

    /// The logical output ids the plugin has, for the zone verbs
    /// to check a list against. Read only. Takes the ids from a
    /// guard the caller holds, so the membership check and the
    /// zone write happen under one logical read lock: a logical
    /// output cannot be removed between the two.
    fn known_logical_ids(logical: &LogicalOutputs) -> Vec<String> {
        logical.list().into_iter().map(|l| l.id).collect()
    }

    /// `zone.add` — a name over one or more logical outputs.
    /// Answers the new zone with its minted id. The logical read
    /// lock is held from the membership check through the zone
    /// write; locks are taken logical-then-zones, the order
    /// `delivery.remove_logical_output` takes too.
    async fn handle_zone_add(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: ZoneAddRequest = parse_versioned_payload(req)?;
        let zone = {
            let logical = self.logical.read().await;
            let known = Self::known_logical_ids(&logical);
            self.zones
                .write()
                .await
                .add(&known, &payload.name, &payload.logical_output_ids)
                .map_err(|r| Self::zone_refused(req, r))?
        };
        self.persist_zones().await;
        self.publish_eq_claim().await;
        tracing::info!(
            plugin = PLUGIN_NAME,
            id = %zone.id,
            name = %zone.name,
            logical_outputs = zone.logical_output_ids.len(),
            "zone added"
        );
        encode(
            req,
            &ZoneResponse {
                v: PAYLOAD_VERSION,
                zone,
            },
        )
    }

    /// `zone.update` — a new name, a new list of logical outputs,
    /// or both; the id stays. Answers the zone as it is now. The
    /// logical read lock is held from the membership check
    /// through the zone write, as in `zone.add`.
    async fn handle_zone_update(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: ZoneUpdateRequest = parse_versioned_payload(req)?;
        let zone = {
            let logical = self.logical.read().await;
            let known = Self::known_logical_ids(&logical);
            self.zones
                .write()
                .await
                .update(
                    &known,
                    &payload.id,
                    payload.name.as_deref(),
                    payload.logical_output_ids.as_deref(),
                )
                .map_err(|r| Self::zone_refused(req, r))?
        };
        self.persist_zones().await;
        self.publish_eq_claim().await;
        tracing::info!(
            plugin = PLUGIN_NAME,
            id = %zone.id,
            name = %zone.name,
            logical_outputs = zone.logical_output_ids.len(),
            "zone updated"
        );
        encode(
            req,
            &ZoneResponse {
                v: PAYLOAD_VERSION,
                zone,
            },
        )
    }

    /// `zone.remove` — the zone taken away. Answers it. Its
    /// logical outputs and the physical outputs are untouched.
    async fn handle_zone_remove(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: ZoneRemoveRequest = parse_versioned_payload(req)?;
        let zone = self
            .zones
            .write()
            .await
            .remove(&payload.id)
            .map_err(|r| Self::zone_refused(req, r))?;
        self.persist_zones().await;
        self.publish_eq_claim().await;
        tracing::info!(
            plugin = PLUGIN_NAME,
            id = %zone.id,
            name = %zone.name,
            "zone removed"
        );
        encode(
            req,
            &ZoneResponse {
                v: PAYLOAD_VERSION,
                zone,
            },
        )
    }

    /// `zone.set_eq_engaged` — the zone's EQ flag. Answers the
    /// zone. Stores and answers; renders nothing.
    async fn handle_zone_set_eq_engaged(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: ZoneSetEqEngagedRequest = parse_versioned_payload(req)?;
        let zone = self
            .zones
            .write()
            .await
            .set_eq_engaged(&payload.id, payload.engaged)
            .map_err(|r| Self::zone_refused(req, r))?;
        self.persist_zones().await;
        self.publish_eq_claim().await;
        tracing::info!(
            plugin = PLUGIN_NAME,
            id = %zone.id,
            engaged = zone.eq.engaged,
            "zone eq engaged set"
        );
        encode(
            req,
            &ZoneResponse {
                v: PAYLOAD_VERSION,
                zone,
            },
        )
    }

    /// `zone.set_eq_band` — one band of one zone's EQ. Answers
    /// the zone. Stores and answers; renders nothing.
    async fn handle_zone_set_eq_band(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: ZoneSetEqBandRequest = parse_versioned_payload(req)?;
        let zone = self
            .zones
            .write()
            .await
            .set_eq_band(
                &payload.id,
                payload.index,
                zones::EqBand {
                    freq_hz: payload.freq_hz,
                    gain_db: payload.gain_db,
                    q: payload.q,
                },
            )
            .map_err(|r| Self::zone_refused(req, r))?;
        self.persist_zones().await;
        self.publish_eq_claim().await;
        tracing::info!(
            plugin = PLUGIN_NAME,
            id = %zone.id,
            index = payload.index,
            "zone eq band set"
        );
        encode(
            req,
            &ZoneResponse {
                v: PAYLOAD_VERSION,
                zone,
            },
        )
    }

    fn scene_refused(req: &Request, r: scenes::Refusal) -> PluginError {
        PluginError::Permanent(format!("{}: {r}", req.request_type))
    }

    /// `scene.save` — the zones as they are, under a name. A
    /// scene of that name is replaced. Answers the scene.
    async fn handle_scene_save(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: SceneNameRequest = parse_versioned_payload(req)?;
        let zones = self.zones.read().await.list();
        let scene = self
            .scenes
            .write()
            .await
            .save(&payload.name, zones)
            .map_err(|r| Self::scene_refused(req, r))?;
        self.persist_scenes().await;
        tracing::info!(
            plugin = PLUGIN_NAME,
            name = %scene.name,
            zones = scene.zones.len(),
            "scene saved"
        );
        encode(
            req,
            &SceneResponse {
                v: PAYLOAD_VERSION,
                scene,
            },
        )
    }

    /// `scene.list` — the names, in the order first saved.
    async fn handle_scene_list(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        parse_versioned_payload::<EmptyPayload>(req)?;
        let scenes = self.scenes.read().await.names();
        encode(
            req,
            &SceneListResponse {
                v: PAYLOAD_VERSION,
                scenes,
            },
        )
    }

    /// `scene.recall` — replace the live zones with the snapshot.
    /// An unknown name and a bad snapshot are refused before any
    /// write; the logical outputs, the physical outputs and the
    /// options EQ are not touched. The logical read lock is held
    /// from the membership check through the zone write, as in
    /// `zone.add`, and the claim is published again because the
    /// zones changed. Answers the scene.
    async fn handle_scene_recall(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: SceneNameRequest = parse_versioned_payload(req)?;
        let scene = self
            .scenes
            .read()
            .await
            .get(&payload.name)
            .map_err(|r| Self::scene_refused(req, r))?
            .clone();
        {
            let logical = self.logical.read().await;
            let known = Self::known_logical_ids(&logical);
            self.zones
                .write()
                .await
                .recall(&known, &scene.zones)
                .map_err(|r| Self::zone_refused(req, r))?;
        }
        self.persist_zones().await;
        self.publish_eq_claim().await;
        tracing::info!(
            plugin = PLUGIN_NAME,
            name = %scene.name,
            zones = scene.zones.len(),
            "scene recalled"
        );
        encode(
            req,
            &SceneResponse {
                v: PAYLOAD_VERSION,
                scene,
            },
        )
    }

    /// `scene.remove` — the scene taken away. The live zones stay.
    async fn handle_scene_remove(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: SceneNameRequest = parse_versioned_payload(req)?;
        let scene = self
            .scenes
            .write()
            .await
            .remove(&payload.name)
            .map_err(|r| Self::scene_refused(req, r))?;
        self.persist_scenes().await;
        tracing::info!(plugin = PLUGIN_NAME, name = %scene.name, "scene removed");
        encode(
            req,
            &SceneResponse {
                v: PAYLOAD_VERSION,
                scene,
            },
        )
    }

    /// `zone.list` — every zone, in id order. Reads the registry
    /// and changes nothing.
    async fn handle_zone_list(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        parse_versioned_payload::<EmptyPayload>(req)?;
        let zones = self.zones.read().await.list();
        encode(
            req,
            &ZoneListResponse {
                v: PAYLOAD_VERSION,
                zones,
            },
        )
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

/// `delivery.enable_output` / `disable_output` / `play_output` /
/// `pause_output` request: one output.
#[derive(Debug, Deserialize)]
struct OutputRequest {
    #[serde(default = "default_payload_version")]
    v: u32,
    output_id: String,
}

impl HasPayloadVersion for OutputRequest {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

/// `delivery.set_output_volume` request: one output, one
/// percentage.
#[derive(Debug, Deserialize)]
struct OutputVolumeRequest {
    #[serde(default = "default_payload_version")]
    v: u32,
    output_id: String,
    volume: u8,
}

impl HasPayloadVersion for OutputVolumeRequest {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

/// The five output verbs answer with that output's state.
#[derive(Debug, Serialize)]
struct OutputResponse {
    v: u32,
    output: OutputSnapshot,
}

/// `delivery.add_logical_output` request.
#[derive(Debug, Deserialize)]
struct AddLogicalOutputRequest {
    #[serde(default = "default_payload_version")]
    v: u32,
    name: String,
    output_id: String,
}

impl HasPayloadVersion for AddLogicalOutputRequest {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

/// `delivery.update_logical_output` request: the id, and what
/// changes.
#[derive(Debug, Deserialize)]
struct UpdateLogicalOutputRequest {
    #[serde(default = "default_payload_version")]
    v: u32,
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    output_id: Option<String>,
}

impl HasPayloadVersion for UpdateLogicalOutputRequest {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

/// `delivery.remove_logical_output` request.
#[derive(Debug, Deserialize)]
struct RemoveLogicalOutputRequest {
    #[serde(default = "default_payload_version")]
    v: u32,
    id: String,
}

impl HasPayloadVersion for RemoveLogicalOutputRequest {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

/// The three logical-output verbs answer with the row.
#[derive(Debug, Serialize)]
struct LogicalOutputResponse {
    v: u32,
    logical_output: LogicalOutput,
}

/// `delivery.list_logical_outputs` answers every row, in id
/// order.
#[derive(Debug, Serialize)]
struct ListLogicalOutputsResponse {
    v: u32,
    logical_outputs: Vec<LogicalOutput>,
}

/// `zone.add` request.
#[derive(Debug, Deserialize)]
struct ZoneAddRequest {
    #[serde(default = "default_payload_version")]
    v: u32,
    name: String,
    logical_output_ids: Vec<String>,
}

impl HasPayloadVersion for ZoneAddRequest {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

/// `zone.update` request: the id, and what changes.
#[derive(Debug, Deserialize)]
struct ZoneUpdateRequest {
    #[serde(default = "default_payload_version")]
    v: u32,
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    logical_output_ids: Option<Vec<String>>,
}

impl HasPayloadVersion for ZoneUpdateRequest {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

/// `zone.remove` request.
#[derive(Debug, Deserialize)]
struct ZoneRemoveRequest {
    #[serde(default = "default_payload_version")]
    v: u32,
    id: String,
}

impl HasPayloadVersion for ZoneRemoveRequest {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

/// `zone.add`, `zone.update` and `zone.remove` answer with the
/// zone.
#[derive(Debug, Serialize)]
struct ZoneResponse {
    v: u32,
    zone: Zone,
}

/// `zone.list` answers every zone, in id order.
#[derive(Debug, Serialize)]
struct ZoneListResponse {
    v: u32,
    zones: Vec<Zone>,
}

/// `scene.save`, `scene.recall` and `scene.remove` request: a
/// name.
#[derive(Debug, Deserialize)]
struct SceneNameRequest {
    #[serde(default = "default_payload_version")]
    v: u32,
    name: String,
}

impl HasPayloadVersion for SceneNameRequest {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

/// `scene.save`, `scene.recall` and `scene.remove` answer with
/// the scene.
#[derive(Debug, Serialize)]
struct SceneResponse {
    v: u32,
    scene: Scene,
}

/// `scene.list` answers the names.
#[derive(Debug, Serialize)]
struct SceneListResponse {
    v: u32,
    scenes: Vec<String>,
}

/// `zone.set_eq_engaged` request.
#[derive(Debug, Deserialize)]
struct ZoneSetEqEngagedRequest {
    #[serde(default = "default_payload_version")]
    v: u32,
    id: String,
    engaged: bool,
}

impl HasPayloadVersion for ZoneSetEqEngagedRequest {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

/// `zone.set_eq_band` request: one band of one zone.
#[derive(Debug, Deserialize)]
struct ZoneSetEqBandRequest {
    #[serde(default = "default_payload_version")]
    v: u32,
    id: String,
    index: u32,
    freq_hz: u32,
    gain_db: f32,
    q: f32,
}

impl HasPayloadVersion for ZoneSetEqBandRequest {
    fn payload_version(&self) -> u32 {
        self.v
    }
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
    use evo_plugin_sdk::contract::{HealthStatus, ReportError};
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
        // Manifest declares the DEFAULT runtime posture — the
        // plugin's chain ships with a `plug` front-end, so neither
        // exclusive nor bit-perfect by static guarantee. The
        // bit-perfect render path is an operator-toggleable branch
        // in `options_render::render_drop_in`, not a static
        // manifest invariant.
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

    /// The four zone verbs are the `audio.zones` stocking and
    /// nothing else is; every other verb is the `audio.delivery`
    /// stocking; the two together are the plugin's whole verb
    /// set. Both manifests.
    #[test]
    fn zone_verbs_are_the_zones_stocking_and_not_the_delivery_one() {
        const ZONE_VERBS: &[&str] = &[
            "zone.add",
            "zone.update",
            "zone.remove",
            "zone.list",
            "zone.set_eq_engaged",
            "zone.set_eq_band",
            "scene.save",
            "scene.list",
            "scene.recall",
            "scene.remove",
        ];
        for (name, toml) in [
            ("manifest.toml", MANIFEST_TOML),
            ("manifest.oop.toml", include_str!("../manifest.oop.toml")),
        ] {
            let m = Manifest::from_toml(toml).expect(name);
            assert_eq!(m.stockings.len(), 2, "{name}: two stockings");
            let delivery = m
                .stockings
                .iter()
                .find(|s| s.shelf == "audio.delivery")
                .unwrap_or_else(|| panic!("{name}: audio.delivery stocking"));
            let zones = m
                .stockings
                .iter()
                .find(|s| s.shelf == "audio.zones")
                .unwrap_or_else(|| panic!("{name}: audio.zones stocking"));
            assert_eq!(delivery.shape, 2, "{name}");
            assert_eq!(zones.shape, 1, "{name}");
            assert_eq!(m.target.shelf, "audio.delivery", "{name}: primary");
            let mut zone_list: Vec<&str> =
                zones.request_types.iter().map(String::as_str).collect();
            zone_list.sort_unstable();
            let mut want = ZONE_VERBS.to_vec();
            want.sort_unstable();
            assert_eq!(
                zone_list, want,
                "{name}: the zones stocking is the four"
            );
            for verb in ZONE_VERBS {
                assert!(
                    !delivery.request_types.iter().any(|v| v == verb),
                    "{name}: {verb} is on the audio.delivery stocking"
                );
            }
            let mut union: Vec<&str> = delivery
                .request_types
                .iter()
                .chain(zones.request_types.iter())
                .map(String::as_str)
                .collect();
            union.sort_unstable();
            let mut all = REQUEST_TYPES.to_vec();
            all.sort_unstable();
            assert_eq!(
                union, all,
                "{name}: the two stockings are the whole set"
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
            principal_scope: None,
            has_step_up: false,
        }
    }

    #[tokio::test]
    async fn handle_request_refused_when_not_loaded() {
        let p = AlsaDeliveryPlugin::new();
        let err = p
            .handle_request(&request("delivery.list_cards", json!({ "v": 1 })))
            .await
            .unwrap_err();
        assert!(matches!(err, PluginError::Permanent(_)));
    }

    #[tokio::test]
    async fn unknown_verb_refused() {
        let p = loaded_plugin().await;
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
        let p = loaded_plugin().await;
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
        let p = loaded_plugin().await;
        // Missing `card` field — serde refuses with Permanent.
        let err = p
            .handle_request(&request("delivery.list_mixers", json!({ "v": 1 })))
            .await
            .unwrap_err();
        assert!(matches!(err, PluginError::Permanent(_)));
    }

    #[tokio::test]
    async fn active_endpoint_reports_no_framework_endpoint_pre_topology() {
        let p = loaded_plugin().await;
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
                interface: Some("i2s".into()),
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
        let p = loaded_plugin().await;
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

    // ----- output verbs: five verbs on one output -----

    use crate::outputs::tests::output as seeded_output;

    /// Records every mixer call instead of running `amixer`.
    #[derive(Default)]
    struct RecordingMixer {
        calls: Mutex<Vec<(u32, String, u8)>>,
        refuse: Mutex<bool>,
    }

    impl MixerSetter for RecordingMixer {
        fn set_volume<'a>(
            &'a self,
            card_idx: u32,
            control: &'a str,
            percent: u8,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<(), String>> + Send + 'a>,
        > {
            Box::pin(async move {
                if *self.refuse.lock().unwrap() {
                    return Err("card refused".to_string());
                }
                self.calls.lock().unwrap().push((
                    card_idx,
                    control.to_string(),
                    percent,
                ));
                Ok(())
            })
        }
    }

    /// A loaded plugin with two outputs, a tempdir drop-in and
    /// state file, and a recording mixer.
    /// Records every announcement and state update.
    #[derive(Default)]
    struct CapturingAnnouncer {
        announced: Mutex<Vec<SubjectAnnouncement>>,
        updates: Mutex<Vec<(ExternalAddressing, Value)>>,
    }

    impl CapturingAnnouncer {
        fn claim_updates(&self) -> Vec<Value> {
            self.updates
                .lock()
                .unwrap()
                .iter()
                .filter(|(a, _)| a.value == SUBJECT_VALUE_EQ_CLAIM)
                .map(|(_, v)| v.clone())
                .collect()
        }
    }

    impl SubjectAnnouncer for CapturingAnnouncer {
        fn announce<'a>(
            &'a self,
            announcement: SubjectAnnouncement,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>,
        > {
            Box::pin(async move {
                self.announced.lock().unwrap().push(announcement);
                Ok(())
            })
        }

        fn retract<'a>(
            &'a self,
            _addressing: ExternalAddressing,
            _reason: Option<String>,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>,
        > {
            Box::pin(async move { Ok(()) })
        }

        fn update_state<'a>(
            &'a self,
            addressing: ExternalAddressing,
            state: Value,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>,
        > {
            Box::pin(async move {
                self.updates.lock().unwrap().push((addressing, state));
                Ok(())
            })
        }
    }

    struct OutputRig {
        plugin: AlsaDeliveryPlugin,
        mixer: Arc<RecordingMixer>,
        announcer: Arc<CapturingAnnouncer>,
        dir: tempfile::TempDir,
    }

    impl OutputRig {
        async fn new() -> Self {
            let dir = tempdir().expect("tempdir");
            let mixer = Arc::new(RecordingMixer::default());
            let mut plugin = loaded_plugin()
                .await
                .with_asound_conf_path(dir.path().join("asound.conf"))
                .with_asound_options_drop_in_path(
                    dir.path().join("evo-options.conf"),
                )
                .with_mixer_setter(Arc::clone(&mixer) as Arc<dyn MixerSetter>);
            plugin
                .install_outputs_state(dir.path().join("outputs-state.toml"))
                .await;
            *plugin.cached_outputs.write().await = vec![
                seeded_output(0, "DAC", Some("Digital")),
                seeded_output(1, "HDMI", Some("PCM")),
            ];
            let announcer = Arc::new(CapturingAnnouncer::default());
            plugin.subject_announcer =
                Some(Arc::clone(&announcer) as Arc<dyn SubjectAnnouncer>);
            Self {
                plugin,
                mixer,
                announcer,
                dir,
            }
        }

        /// The last zone EQ claim published, if any.
        fn last_claim(&self) -> Option<Value> {
            self.announcer.claim_updates().last().cloned()
        }

        fn claim_count(&self) -> usize {
            self.announcer.claim_updates().len()
        }

        async fn with_options(self, settings: OptionsSettings) -> Self {
            *self.plugin.last_options.write().await = Some(LastOptions {
                settings,
                subject_canonical_id: "options-1".to_string(),
            });
            self
        }

        async fn call(
            &self,
            verb: &str,
            payload: Value,
        ) -> Result<Value, String> {
            match self.plugin.handle_request(&request(verb, payload)).await {
                Ok(resp) => Ok(serde_json::from_slice(&resp.payload).unwrap()),
                Err(e) => Err(format!("{e:?}")),
            }
        }

        async fn verb(&self, verb: &str, id: &str) -> Result<Value, String> {
            self.call(verb, json!({ "v": 1, "output_id": id })).await
        }

        async fn drop_in(&self) -> String {
            tokio::fs::read_to_string(&self.plugin.asound_options_drop_in_path)
                .await
                .unwrap_or_default()
        }
    }

    fn hardware_mixer_on_dac() -> OptionsSettings {
        OptionsSettings {
            mixer_type: Some(options_render::MixerType::Hardware),
            mixer_control: Some("Digital".to_string()),
            output_device: Some("hw:CARD=DAC,DEV=0".to_string()),
            ..Default::default()
        }
    }

    /// The five verbs refuse an output `list_outputs` does not
    /// name, and leave the mixer and the drop-in untouched.
    #[tokio::test]
    async fn an_unknown_output_is_refused_by_the_five_verbs() {
        let rig = OutputRig::new().await;
        for verb in [
            "delivery.enable_output",
            "delivery.disable_output",
            "delivery.play_output",
            "delivery.pause_output",
        ] {
            let err = rig.verb(verb, "hw:9,9").await.unwrap_err();
            assert!(err.contains("unknown output"), "{verb}: {err}");
        }
        let err = rig
            .call(
                "delivery.set_output_volume",
                json!({ "v": 1, "output_id": "hw:9,9", "volume": 10 }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("unknown output"), "{err}");
        assert!(rig.mixer.calls.lock().unwrap().is_empty());
        assert_eq!(rig.drop_in().await, "", "no drop-in rendered");
        assert!(!rig.dir.path().join("outputs-state.toml").exists());
    }

    /// Enabling, disabling or setting the volume of output A
    /// changes nothing about output B: B's reported state is the
    /// same before and after, and the one mixer call names A's
    /// card and control.
    #[tokio::test]
    async fn changing_one_output_leaves_the_other_as_it_was() {
        let rig = OutputRig::new().await;
        // B is read through pause_output: a no-op on an output
        // that is not playing, answering with its state unchanged.
        let b_before =
            rig.verb("delivery.pause_output", "hw:1,0").await.unwrap();
        assert_eq!(
            b_before["output"],
            json!({ "output_id": "hw:1,0", "enabled": true, "playing": false, "volume": null })
        );

        let a = rig.verb("delivery.disable_output", "hw:0,0").await.unwrap();
        assert_eq!(a["output"]["enabled"], false);
        let b = rig.verb("delivery.pause_output", "hw:1,0").await.unwrap();
        assert_eq!(
            b["output"], b_before["output"],
            "B untouched by A's disable"
        );

        let a = rig
            .call(
                "delivery.set_output_volume",
                json!({ "v": 1, "output_id": "hw:0,0", "volume": 40 }),
            )
            .await
            .unwrap();
        assert_eq!(a["output"]["volume"], 40);
        assert_eq!(
            *rig.mixer.calls.lock().unwrap(),
            vec![(0, "Digital".to_string(), 40)],
            "one call, A's card and control"
        );
        let b = rig.verb("delivery.pause_output", "hw:1,0").await.unwrap();
        assert_eq!(
            b["output"], b_before["output"],
            "B untouched by A's volume"
        );

        let a = rig.verb("delivery.enable_output", "hw:0,0").await.unwrap();
        assert_eq!(a["output"]["enabled"], true);
        assert_eq!(a["output"]["volume"], 40, "A's volume kept");
        let b = rig.verb("delivery.pause_output", "hw:1,0").await.unwrap();
        assert_eq!(
            b["output"], b_before["output"],
            "B untouched by A's enable"
        );

        // Both disabled: enabling A leaves B disabled.
        rig.verb("delivery.disable_output", "hw:0,0").await.unwrap();
        rig.verb("delivery.disable_output", "hw:1,0").await.unwrap();
        rig.verb("delivery.enable_output", "hw:0,0").await.unwrap();
        let b = rig.verb("delivery.pause_output", "hw:1,0").await.unwrap();
        assert_eq!(b["output"]["enabled"], false, "B stays disabled");
    }

    /// The chain plays one output at a time. The operator's
    /// device plays until it is paused; a second play is refused
    /// and the first keeps playing; pausing the playing output
    /// renders a null terminus and starts no other.
    #[tokio::test]
    async fn the_second_play_is_refused_and_the_first_keeps_playing() {
        let rig = OutputRig::new()
            .await
            .with_options(hardware_mixer_on_dac())
            .await;
        // DAC plays by the operator's choice.
        let a = rig.verb("delivery.enable_output", "hw:0,0").await.unwrap();
        assert_eq!(a["output"]["playing"], true);

        let err = rig
            .verb("delivery.play_output", "hw:1,0")
            .await
            .unwrap_err();
        assert!(
            err.contains("cannot play while \\\"hw:0,0\\\" is playing"),
            "{err}"
        );
        let a = rig.verb("delivery.enable_output", "hw:0,0").await.unwrap();
        assert_eq!(a["output"]["playing"], true, "the first keeps playing");
        assert_eq!(rig.drop_in().await, "", "a refused play renders nothing");

        // Pause DAC: the chain goes silent; HDMI did not start.
        let a = rig.verb("delivery.pause_output", "hw:0,0").await.unwrap();
        assert_eq!(a["output"]["playing"], false);
        let body = rig.drop_in().await;
        assert!(
            body.contains("pcm.evo_terminus {\n    type null\n}"),
            "{body}"
        );
        assert!(!body.contains("card \"1\""), "HDMI did not start: {body}");
        assert!(
            body.contains("ctl.evo {\n    type hw\n    card \"DAC\""),
            "global volume stays on the operator's card: {body}"
        );
        let b = rig.verb("delivery.enable_output", "hw:1,0").await.unwrap();
        assert_eq!(b["output"]["playing"], false);

        // Now HDMI may play, and DAC may not.
        let b = rig.verb("delivery.play_output", "hw:1,0").await.unwrap();
        assert_eq!(b["output"]["playing"], true);
        let body = rig.drop_in().await;
        assert!(body.contains("pcm.evo_terminus {\n    type hw\n    card \"1\"\n    device 0\n}"), "{body}");
        let err = rig
            .verb("delivery.play_output", "hw:0,0")
            .await
            .unwrap_err();
        assert!(
            err.contains("cannot play while \\\"hw:1,0\\\" is playing"),
            "{err}"
        );
        assert!(
            rig.drop_in().await.contains("card \"1\""),
            "HDMI keeps playing"
        );

        // Pausing the one that is not playing pauses nothing.
        let a = rig.verb("delivery.pause_output", "hw:0,0").await.unwrap();
        assert_eq!(a["output"]["playing"], false);
        let b = rig.verb("delivery.enable_output", "hw:1,0").await.unwrap();
        assert_eq!(b["output"]["playing"], true, "HDMI still playing");
        assert!(rig.drop_in().await.contains("card \"1\""));
    }

    /// A disabled output cannot play; disabling the playing
    /// output pauses it and starts no other.
    #[tokio::test]
    async fn a_disabled_output_cannot_play_and_disabling_the_playing_one_pauses_it(
    ) {
        let rig = OutputRig::new()
            .await
            .with_options(hardware_mixer_on_dac())
            .await;
        rig.verb("delivery.pause_output", "hw:0,0").await.unwrap();
        rig.verb("delivery.disable_output", "hw:1,0").await.unwrap();
        let err = rig
            .verb("delivery.play_output", "hw:1,0")
            .await
            .unwrap_err();
        assert!(err.contains("is disabled"), "{err}");

        rig.verb("delivery.enable_output", "hw:1,0").await.unwrap();
        let b = rig.verb("delivery.play_output", "hw:1,0").await.unwrap();
        assert_eq!(b["output"]["playing"], true);
        let b = rig.verb("delivery.disable_output", "hw:1,0").await.unwrap();
        assert_eq!(b["output"]["playing"], false);
        assert_eq!(b["output"]["enabled"], false);
        let body = rig.drop_in().await;
        assert!(body.contains("type null"), "{body}");
        assert!(
            !body.contains("card \"0\"") && !body.contains("card \"1\"\n"),
            "no output started: {body}"
        );
    }

    /// Volume on the chain's hardware mixer is the global volume
    /// and is refused; the other card's control is set with one
    /// call; no control and out of range are refused; a card
    /// that refuses records nothing.
    #[tokio::test]
    async fn volume_on_the_chains_hardware_mixer_is_refused() {
        let rig = OutputRig::new()
            .await
            .with_options(hardware_mixer_on_dac())
            .await;
        let err = rig
            .call(
                "delivery.set_output_volume",
                json!({ "v": 1, "output_id": "hw:0,0", "volume": 50 }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("global volume"), "{err}");
        assert!(rig.mixer.calls.lock().unwrap().is_empty());

        let b = rig
            .call(
                "delivery.set_output_volume",
                json!({ "v": 1, "output_id": "hw:1,0", "volume": 50 }),
            )
            .await
            .unwrap();
        assert_eq!(b["output"]["volume"], 50);
        assert_eq!(
            *rig.mixer.calls.lock().unwrap(),
            vec![(1, "PCM".to_string(), 50)]
        );

        let err = rig
            .call(
                "delivery.set_output_volume",
                json!({ "v": 1, "output_id": "hw:1,0", "volume": 101 }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("out of range"), "{err}");

        rig.plugin
            .cached_outputs
            .write()
            .await
            .push(seeded_output(2, "Bare", None));
        let err = rig
            .call(
                "delivery.set_output_volume",
                json!({ "v": 1, "output_id": "hw:2,0", "volume": 10 }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("no mixer control"), "{err}");

        *rig.mixer.refuse.lock().unwrap() = true;
        let err = rig
            .call(
                "delivery.set_output_volume",
                json!({ "v": 1, "output_id": "hw:1,0", "volume": 60 }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("card refused"), "{err}");
        let b = rig.verb("delivery.enable_output", "hw:1,0").await.unwrap();
        assert_eq!(
            b["output"]["volume"], 50,
            "a refused volume is not recorded"
        );
        assert_eq!(rig.mixer.calls.lock().unwrap().len(), 1);
    }

    /// Enabled flags and volumes survive a restart on the same
    /// state directory; the binding does not.
    #[tokio::test]
    async fn enabled_and_volume_survive_a_restart_and_the_binding_does_not() {
        let rig = OutputRig::new().await;
        rig.verb("delivery.disable_output", "hw:1,0").await.unwrap();
        rig.call(
            "delivery.set_output_volume",
            json!({ "v": 1, "output_id": "hw:0,0", "volume": 33 }),
        )
        .await
        .unwrap();
        rig.verb("delivery.pause_output", "hw:0,0").await.unwrap();
        rig.verb("delivery.enable_output", "hw:1,0").await.unwrap();
        rig.verb("delivery.play_output", "hw:1,0").await.unwrap();
        assert!(rig.drop_in().await.contains("card \"1\""));

        let mut again = loaded_plugin().await;
        again
            .install_outputs_state(rig.dir.path().join("outputs-state.toml"))
            .await;
        *again.cached_outputs.write().await =
            rig.plugin.cached_outputs.read().await.clone();
        let b: Value = serde_json::from_slice(
            &again
                .handle_request(&request(
                    "delivery.enable_output",
                    json!({ "v": 1, "output_id": "hw:1,0" }),
                ))
                .await
                .unwrap()
                .payload,
        )
        .unwrap();
        assert_eq!(b["output"]["enabled"], true);
        assert_eq!(
            b["output"]["volume"],
            Value::Null,
            "HDMI never had a volume"
        );
        let persisted =
            OutputStateStore::at(rig.dir.path().join("outputs-state.toml"))
                .load()
                .await;
        assert!(persisted
            .outputs
            .iter()
            .any(|o| o.output_id == "hw:0,0" && o.volume == Some(33)));
        assert_eq!(
            again.outputs.read().await.binding(),
            &options_render::OutputBinding::Follow
        );
    }

    /// A restart finds the disabled flag before any verb runs.
    #[tokio::test]
    async fn a_disabled_output_is_still_disabled_after_a_restart() {
        let rig = OutputRig::new().await;
        rig.verb("delivery.disable_output", "hw:1,0").await.unwrap();
        let mut again = loaded_plugin().await;
        again
            .install_outputs_state(rig.dir.path().join("outputs-state.toml"))
            .await;
        *again.cached_outputs.write().await =
            rig.plugin.cached_outputs.read().await.clone();
        let err = again
            .handle_request(&request(
                "delivery.play_output",
                json!({ "v": 1, "output_id": "hw:1,0" }),
            ))
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("is disabled"), "{err:?}");
    }

    /// Unload releases the binding: the drop-in on disk follows
    /// the operator's settings again.
    #[tokio::test]
    async fn unload_leaves_no_output_binding_on_disk() {
        let mut rig = OutputRig::new()
            .await
            .with_options(hardware_mixer_on_dac())
            .await;
        rig.verb("delivery.pause_output", "hw:0,0").await.unwrap();
        rig.verb("delivery.play_output", "hw:1,0").await.unwrap();
        assert!(rig.drop_in().await.contains("card \"1\""));
        rig.plugin.unload().await.unwrap();
        let body = rig.drop_in().await;
        assert!(body.contains("pcm.evo_terminus {\n    type hw\n    card \"DAC\"\n    device 0\n}"), "{body}");
        assert!(!body.contains("type null"), "{body}");
    }

    /// The options observer and the output verbs render through
    /// one renderer: a settings change rendered while an output
    /// is bound keeps the bound terminus, and the production
    /// source has exactly one drop-in write.
    #[tokio::test]
    async fn the_observer_renders_through_the_output_binding() {
        let rig = OutputRig::new()
            .await
            .with_options(hardware_mixer_on_dac())
            .await;
        rig.verb("delivery.pause_output", "hw:0,0").await.unwrap();
        rig.verb("delivery.play_output", "hw:1,0").await.unwrap();
        // The operator changes the mixer; the observer's render.
        *rig.plugin.last_options.write().await = Some(LastOptions {
            settings: OptionsSettings {
                mixer_type: Some(options_render::MixerType::Software),
                ..hardware_mixer_on_dac()
            },
            subject_canonical_id: "options-2".to_string(),
        });
        let outcome = rig.plugin.drop_in_renderer().render_and_write().await;
        assert!(outcome.write_ok());
        assert_eq!(outcome.subject_canonical_id.as_deref(), Some("options-2"));
        let body = rig.drop_in().await;
        assert!(body.contains("type softvol"), "the new setting: {body}");
        assert!(
            body.contains("card \"1\"\n    device 0"),
            "the bound output: {body}"
        );
        assert!(body.contains("control.card \"DAC\""), "{body}");

        let production = include_str!("lib.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        assert_eq!(
            production.matches("atomic_write_drop_in(").count(),
            1,
            "one writer for the drop-in"
        );
    }

    // ----- logical outputs: add, update, remove a named output -----

    impl OutputRig {
        async fn with_logical_state(mut self) -> Self {
            self.plugin
                .install_logical_outputs(
                    self.dir.path().join(LogicalOutputStore::FILE_NAME),
                )
                .await;
            self
        }
    }

    /// Add mints a stable id for a name on one physical output;
    /// an unknown physical output is refused and nothing is kept.
    #[tokio::test]
    async fn add_names_one_physical_output_and_refuses_an_unknown_one() {
        let rig = OutputRig::new().await.with_logical_state().await;
        let err = rig
            .call(
                "delivery.add_logical_output",
                json!({ "v": 1, "name": "Kitchen", "output_id": "hw:9,9" }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("unknown output"), "{err}");
        assert!(!rig.dir.path().join(LogicalOutputStore::FILE_NAME).exists());

        let a = rig
            .call(
                "delivery.add_logical_output",
                json!({ "v": 1, "name": " Kitchen ", "output_id": "hw:0,0" }),
            )
            .await
            .unwrap();
        assert_eq!(
            a["logical_output"],
            json!({ "id": "logical:1", "name": "Kitchen", "output_id": "hw:0,0" })
        );
        let err = rig
            .call(
                "delivery.add_logical_output",
                json!({ "v": 1, "name": "kitchen", "output_id": "hw:1,0" }),
            )
            .await
            .unwrap_err();
        assert!(
            err.contains("already carried by \\\"logical:1\\\""),
            "{err}"
        );
        let err = rig
            .call(
                "delivery.add_logical_output",
                json!({ "v": 1, "name": "  ", "output_id": "hw:1,0" }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("needs a name"), "{err}");
    }

    /// Update keeps the id and changes only what was named;
    /// remove answers the row taken away; both refuse an unknown
    /// id. The physical output is untouched throughout.
    #[tokio::test]
    async fn update_keeps_the_id_and_remove_takes_the_name_away() {
        let rig = OutputRig::new().await.with_logical_state().await;
        rig.call(
            "delivery.add_logical_output",
            json!({ "v": 1, "name": "Kitchen", "output_id": "hw:0,0" }),
        )
        .await
        .unwrap();
        let u = rig
            .call(
                "delivery.update_logical_output",
                json!({ "v": 1, "id": "logical:1", "name": "Pantry" }),
            )
            .await
            .unwrap();
        assert_eq!(
            u["logical_output"],
            json!({ "id": "logical:1", "name": "Pantry", "output_id": "hw:0,0" })
        );
        let u = rig
            .call(
                "delivery.update_logical_output",
                json!({ "v": 1, "id": "logical:1", "output_id": "hw:1,0" }),
            )
            .await
            .unwrap();
        assert_eq!(
            u["logical_output"],
            json!({ "id": "logical:1", "name": "Pantry", "output_id": "hw:1,0" })
        );
        let err = rig
            .call(
                "delivery.update_logical_output",
                json!({ "v": 1, "id": "logical:1" }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("neither"), "{err}");
        let err = rig
            .call(
                "delivery.update_logical_output",
                json!({ "v": 1, "id": "logical:7", "name": "X" }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("unknown logical output"), "{err}");

        let r = rig
            .call(
                "delivery.remove_logical_output",
                json!({ "v": 1, "id": "logical:1" }),
            )
            .await
            .unwrap();
        assert_eq!(r["logical_output"]["name"], "Pantry");
        let err = rig
            .call(
                "delivery.remove_logical_output",
                json!({ "v": 1, "id": "logical:1" }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("unknown logical output"), "{err}");

        // The physical outputs are as they were, and the mixer
        // and drop-in were never touched.
        let a = rig.verb("delivery.pause_output", "hw:1,0").await.unwrap();
        assert_eq!(
            a["output"],
            json!({ "output_id": "hw:1,0", "enabled": true, "playing": false, "volume": null })
        );
        assert!(rig.mixer.calls.lock().unwrap().is_empty());
        assert_eq!(rig.drop_in().await, "");
    }

    /// A logical id is not a physical output: the five output
    /// verbs refuse it.
    #[tokio::test]
    async fn the_output_verbs_refuse_a_logical_id() {
        let rig = OutputRig::new().await.with_logical_state().await;
        rig.call(
            "delivery.add_logical_output",
            json!({ "v": 1, "name": "Kitchen", "output_id": "hw:0,0" }),
        )
        .await
        .unwrap();
        for verb in [
            "delivery.enable_output",
            "delivery.disable_output",
            "delivery.play_output",
            "delivery.pause_output",
        ] {
            let err = rig.verb(verb, "logical:1").await.unwrap_err();
            assert!(err.contains("unknown output"), "{verb}: {err}");
        }
        let err = rig
            .call(
                "delivery.set_output_volume",
                json!({ "v": 1, "output_id": "logical:1", "volume": 10 }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("unknown output"), "{err}");
    }

    /// Names survive a restart on the same state directory, and
    /// a removed id is never minted again.
    #[tokio::test]
    async fn logical_outputs_survive_a_restart() {
        let rig = OutputRig::new().await.with_logical_state().await;
        rig.call(
            "delivery.add_logical_output",
            json!({ "v": 1, "name": "Kitchen", "output_id": "hw:0,0" }),
        )
        .await
        .unwrap();
        rig.call(
            "delivery.add_logical_output",
            json!({ "v": 1, "name": "Hall", "output_id": "hw:1,0" }),
        )
        .await
        .unwrap();
        rig.call(
            "delivery.remove_logical_output",
            json!({ "v": 1, "id": "logical:2" }),
        )
        .await
        .unwrap();

        let mut again = loaded_plugin().await;
        again
            .install_logical_outputs(
                rig.dir.path().join(LogicalOutputStore::FILE_NAME),
            )
            .await;
        *again.cached_outputs.write().await =
            rig.plugin.cached_outputs.read().await.clone();
        let err = again
            .handle_request(&request(
                "delivery.add_logical_output",
                json!({ "v": 1, "name": "KITCHEN", "output_id": "hw:1,0" }),
            ))
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("already carried"), "{err:?}");
        let v: Value = serde_json::from_slice(
            &again
                .handle_request(&request(
                    "delivery.add_logical_output",
                    json!({ "v": 1, "name": "Hall", "output_id": "hw:1,0" }),
                ))
                .await
                .unwrap()
                .payload,
        )
        .unwrap();
        assert_eq!(v["logical_output"]["id"], "logical:3");
    }

    /// The read lists what was added, drops what was removed,
    /// answers an empty set as an empty list, and leaves the
    /// physical output list, the rows and the state file as they
    /// were.
    #[tokio::test]
    async fn list_logical_outputs_reads_the_names_back_and_changes_nothing() {
        let rig = OutputRig::new().await.with_logical_state().await;
        let physical_before = rig
            .call("delivery.list_outputs", json!({ "v": 1 }))
            .await
            .unwrap();
        let empty = rig
            .call("delivery.list_logical_outputs", json!({ "v": 1 }))
            .await
            .unwrap();
        assert_eq!(empty, json!({ "v": 1, "logical_outputs": [] }));

        rig.call(
            "delivery.add_logical_output",
            json!({ "v": 1, "name": "Kitchen", "output_id": "hw:0,0" }),
        )
        .await
        .unwrap();
        rig.call(
            "delivery.add_logical_output",
            json!({ "v": 1, "name": "Hall", "output_id": "hw:1,0" }),
        )
        .await
        .unwrap();
        let listed = rig
            .call("delivery.list_logical_outputs", json!({ "v": 1 }))
            .await
            .unwrap();
        assert_eq!(
            listed["logical_outputs"],
            json!([
                { "id": "logical:1", "name": "Kitchen", "output_id": "hw:0,0" },
                { "id": "logical:2", "name": "Hall", "output_id": "hw:1,0" },
            ]),
            "an added name is listed, in id order"
        );

        rig.call(
            "delivery.remove_logical_output",
            json!({ "v": 1, "id": "logical:1" }),
        )
        .await
        .unwrap();
        let state_path = rig.dir.path().join(LogicalOutputStore::FILE_NAME);
        let file_before = tokio::fs::read(&state_path).await.unwrap();
        let listed = rig
            .call("delivery.list_logical_outputs", json!({ "v": 1 }))
            .await
            .unwrap();
        assert_eq!(
            listed["logical_outputs"],
            json!([{ "id": "logical:2", "name": "Hall", "output_id": "hw:1,0" }]),
            "a removed name is not listed"
        );
        // Reading twice answers the same and moves nothing.
        let again = rig
            .call("delivery.list_logical_outputs", json!({ "v": 1 }))
            .await
            .unwrap();
        assert_eq!(again, listed);
        assert_eq!(
            tokio::fs::read(&state_path).await.unwrap(),
            file_before,
            "the state file was not rewritten"
        );
        assert_eq!(rig.plugin.logical.read().await.rows().len(), 1);
        let physical_after = rig
            .call("delivery.list_outputs", json!({ "v": 1 }))
            .await
            .unwrap();
        assert_eq!(
            physical_after, physical_before,
            "the physical output list is unchanged"
        );
        assert_eq!(physical_after["outputs"].as_array().unwrap().len(), 2);
        assert!(rig.mixer.calls.lock().unwrap().is_empty());
        assert_eq!(rig.drop_in().await, "");
    }

    // ----- zones: a name over one or more logical outputs -----

    impl OutputRig {
        /// Logical state plus zone state, with two named outputs
        /// already there: `logical:1` on DAC and `logical:2` on
        /// HDMI.
        async fn with_zones(mut self) -> Self {
            self = self.with_logical_state().await;
            self.plugin
                .install_scenes(self.dir.path().join(SceneStore::FILE_NAME))
                .await;
            self.plugin
                .install_zones(self.dir.path().join(ZoneStore::FILE_NAME))
                .await;
            for (name, out) in [("Kitchen", "hw:0,0"), ("Hall", "hw:1,0")] {
                self.call(
                    "delivery.add_logical_output",
                    json!({ "v": 1, "name": name, "output_id": out }),
                )
                .await
                .unwrap();
            }
            self
        }

        async fn snapshot_outputs(&self) -> (Value, Value) {
            (
                self.call("delivery.list_outputs", json!({ "v": 1 }))
                    .await
                    .unwrap(),
                self.call("delivery.list_logical_outputs", json!({ "v": 1 }))
                    .await
                    .unwrap(),
            )
        }
    }

    /// A zone with no logical output is refused, on add and on
    /// update; nothing is minted or changed.
    #[tokio::test]
    async fn a_zone_with_no_logical_output_is_refused() {
        let rig = OutputRig::new().await.with_zones().await;
        let err = rig
            .call(
                "zone.add",
                json!({ "v": 1, "name": "Empty", "logical_output_ids": [] }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("at least one logical output"), "{err}");
        let listed = rig.call("zone.list", json!({ "v": 1 })).await.unwrap();
        assert_eq!(listed, json!({ "v": 1, "zones": [] }));
        assert!(!rig.dir.path().join(ZoneStore::FILE_NAME).exists());

        let z = rig
            .call("zone.add", json!({ "v": 1, "name": "Down", "logical_output_ids": ["logical:1"] }))
            .await
            .unwrap();
        assert_eq!(
            z["zone"],
            json!({ "id": "zone:1", "name": "Down", "logical_output_ids": ["logical:1"], "eq": flat_eq() })
        );
        let err = rig
            .call(
                "zone.update",
                json!({ "v": 1, "id": "zone:1", "logical_output_ids": [] }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("at least one logical output"), "{err}");
        let listed = rig.call("zone.list", json!({ "v": 1 })).await.unwrap();
        assert_eq!(
            listed["zones"][0]["logical_output_ids"],
            json!(["logical:1"])
        );
    }

    /// One logical output cannot sit in two zones: refused on add
    /// and on update, and the refused zone is not kept.
    #[tokio::test]
    async fn one_logical_output_cannot_sit_in_two_zones() {
        let rig = OutputRig::new().await.with_zones().await;
        rig.call("zone.add", json!({ "v": 1, "name": "Down", "logical_output_ids": ["logical:1"] }))
            .await
            .unwrap();
        let err = rig
            .call("zone.add", json!({ "v": 1, "name": "Up", "logical_output_ids": ["logical:2", "logical:1"] }))
            .await
            .unwrap_err();
        assert!(err.contains("already sits in zone \\\"zone:1\\\""), "{err}");
        let listed = rig.call("zone.list", json!({ "v": 1 })).await.unwrap();
        assert_eq!(
            listed["zones"].as_array().unwrap().len(),
            1,
            "the refused zone was not kept"
        );

        rig.call("zone.add", json!({ "v": 1, "name": "Up", "logical_output_ids": ["logical:2"] }))
            .await
            .unwrap();
        let err = rig
            .call("zone.update", json!({ "v": 1, "id": "zone:2", "logical_output_ids": ["logical:2", "logical:1"] }))
            .await
            .unwrap_err();
        assert!(err.contains("already sits in zone \\\"zone:1\\\""), "{err}");
        let listed = rig.call("zone.list", json!({ "v": 1 })).await.unwrap();
        assert_eq!(
            listed["zones"],
            json!([
                { "id": "zone:1", "name": "Down", "logical_output_ids": ["logical:1"], "eq": flat_eq() },
                { "id": "zone:2", "name": "Up", "logical_output_ids": ["logical:2"], "eq": flat_eq() },
            ])
        );
        // Named twice in one list is refused too.
        let err = rig
            .call("zone.update", json!({ "v": 1, "id": "zone:2", "logical_output_ids": ["logical:2", "logical:2"] }))
            .await
            .unwrap_err();
        assert!(err.contains("named twice"), "{err}");
    }

    /// Remove takes the zone away and leaves the logical outputs
    /// and the physical outputs exactly as they were; the freed
    /// logical output can join another zone.
    #[tokio::test]
    async fn remove_drops_no_logical_output_and_no_physical_output() {
        let rig = OutputRig::new().await.with_zones().await;
        let before = rig.snapshot_outputs().await;
        rig.call("zone.add", json!({ "v": 1, "name": "Down", "logical_output_ids": ["logical:1", "logical:2"] }))
            .await
            .unwrap();
        let removed = rig
            .call("zone.remove", json!({ "v": 1, "id": "zone:1" }))
            .await
            .unwrap();
        assert_eq!(
            removed["zone"],
            json!({ "id": "zone:1", "name": "Down", "logical_output_ids": ["logical:1", "logical:2"], "eq": flat_eq() })
        );
        assert_eq!(
            rig.snapshot_outputs().await,
            before,
            "logical and physical outputs untouched"
        );
        assert_eq!(before.1["logical_outputs"].as_array().unwrap().len(), 2);
        assert_eq!(before.0["outputs"].as_array().unwrap().len(), 2);
        let listed = rig.call("zone.list", json!({ "v": 1 })).await.unwrap();
        assert_eq!(listed, json!({ "v": 1, "zones": [] }));
        let err = rig
            .call("zone.remove", json!({ "v": 1, "id": "zone:1" }))
            .await
            .unwrap_err();
        assert!(err.contains("unknown zone"), "{err}");
        let again = rig
            .call("zone.add", json!({ "v": 1, "name": "Up", "logical_output_ids": ["logical:2"] }))
            .await
            .unwrap();
        assert_eq!(
            again["zone"]["id"], "zone:2",
            "a removed id is never reused"
        );
        assert!(rig.mixer.calls.lock().unwrap().is_empty());
        assert_eq!(rig.drop_in().await, "");
    }

    /// An unknown logical id, a physical id, an unknown zone, a
    /// taken name and an empty update are refused; update keeps
    /// the id.
    #[tokio::test]
    async fn zone_refusals_and_update_keeps_the_id() {
        let rig = OutputRig::new().await.with_zones().await;
        let err = rig
            .call("zone.add", json!({ "v": 1, "name": "Down", "logical_output_ids": ["logical:9"] }))
            .await
            .unwrap_err();
        assert!(
            err.contains("unknown logical output \\\"logical:9\\\""),
            "{err}"
        );
        let err = rig
            .call("zone.add", json!({ "v": 1, "name": "Down", "logical_output_ids": ["hw:0,0"] }))
            .await
            .unwrap_err();
        assert!(
            err.contains("unknown logical output \\\"hw:0,0\\\""),
            "{err}"
        );
        rig.call("zone.add", json!({ "v": 1, "name": "Down", "logical_output_ids": ["logical:1"] }))
            .await
            .unwrap();
        let err = rig
            .call("zone.add", json!({ "v": 1, "name": "down", "logical_output_ids": ["logical:2"] }))
            .await
            .unwrap_err();
        assert!(err.contains("already carried by \\\"zone:1\\\""), "{err}");
        let err = rig
            .call("zone.update", json!({ "v": 1, "id": "zone:1" }))
            .await
            .unwrap_err();
        assert!(err.contains("neither"), "{err}");
        let err = rig
            .call(
                "zone.update",
                json!({ "v": 1, "id": "zone:7", "name": "X" }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("unknown zone"), "{err}");
        let u = rig
            .call("zone.update", json!({ "v": 1, "id": "zone:1", "name": "Ground", "logical_output_ids": ["logical:2", "logical:1"] }))
            .await
            .unwrap();
        assert_eq!(
            u["zone"],
            json!({ "id": "zone:1", "name": "Ground", "logical_output_ids": ["logical:2", "logical:1"], "eq": flat_eq() })
        );
    }

    /// Zones survive a restart on the same state directory, in id
    /// order, and a removed id is never minted again.
    #[tokio::test]
    async fn zones_survive_a_restart() {
        let rig = OutputRig::new().await.with_zones().await;
        rig.call("zone.add", json!({ "v": 1, "name": "Down", "logical_output_ids": ["logical:1"] }))
            .await
            .unwrap();
        rig.call("zone.add", json!({ "v": 1, "name": "Up", "logical_output_ids": ["logical:2"] }))
            .await
            .unwrap();
        rig.call("zone.remove", json!({ "v": 1, "id": "zone:1" }))
            .await
            .unwrap();

        let mut again = loaded_plugin().await;
        again
            .install_logical_outputs(
                rig.dir.path().join(LogicalOutputStore::FILE_NAME),
            )
            .await;
        again
            .install_zones(rig.dir.path().join(ZoneStore::FILE_NAME))
            .await;
        *again.cached_outputs.write().await =
            rig.plugin.cached_outputs.read().await.clone();
        let listed: Value = serde_json::from_slice(
            &again
                .handle_request(&request("zone.list", json!({ "v": 1 })))
                .await
                .unwrap()
                .payload,
        )
        .unwrap();
        assert_eq!(
            listed["zones"],
            json!([{ "id": "zone:2", "name": "Up", "logical_output_ids": ["logical:2"], "eq": flat_eq() }])
        );
        let err = again
            .handle_request(&request("zone.add", json!({ "v": 1, "name": "Porch", "logical_output_ids": ["logical:2"] })))
            .await
            .unwrap_err();
        assert!(
            format!("{err:?}").contains("already sits in zone \\\"zone:2\\\""),
            "{err:?}"
        );
        let added: Value = serde_json::from_slice(
            &again
                .handle_request(&request("zone.add", json!({ "v": 1, "name": "Porch", "logical_output_ids": ["logical:1"] })))
                .await
                .unwrap()
                .payload,
        )
        .unwrap();
        assert_eq!(added["zone"]["id"], "zone:3");
    }

    /// A logical output a zone holds cannot be removed: the
    /// refusal names the zone; the logical output, the zone and
    /// the physical outputs stay. An unzoned one is removed as
    /// before, and so is the zoned one once its zone is gone.
    #[tokio::test]
    async fn a_zoned_logical_output_cannot_be_removed() {
        let rig = OutputRig::new().await.with_zones().await;
        rig.call("zone.add", json!({ "v": 1, "name": "Down", "logical_output_ids": ["logical:1"] }))
            .await
            .unwrap();
        let before = rig.snapshot_outputs().await;
        let zones_before =
            rig.call("zone.list", json!({ "v": 1 })).await.unwrap();

        let err = rig
            .call(
                "delivery.remove_logical_output",
                json!({ "v": 1, "id": "logical:1" }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("is held by zone \\\"zone:1\\\""), "{err}");
        assert_eq!(
            rig.snapshot_outputs().await,
            before,
            "logical and physical outputs stay"
        );
        assert_eq!(before.1["logical_outputs"].as_array().unwrap().len(), 2);
        assert_eq!(
            rig.call("zone.list", json!({ "v": 1 })).await.unwrap(),
            zones_before,
            "the zone stays"
        );

        // Unknown is still unknown, zoned or not.
        let err = rig
            .call(
                "delivery.remove_logical_output",
                json!({ "v": 1, "id": "logical:9" }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("unknown logical output"), "{err}");

        // An unzoned logical output is removed as before.
        let r = rig
            .call(
                "delivery.remove_logical_output",
                json!({ "v": 1, "id": "logical:2" }),
            )
            .await
            .unwrap();
        assert_eq!(r["logical_output"]["id"], "logical:2");

        // Once the zone is gone, the logical output can go.
        rig.call("zone.remove", json!({ "v": 1, "id": "zone:1" }))
            .await
            .unwrap();
        let r = rig
            .call(
                "delivery.remove_logical_output",
                json!({ "v": 1, "id": "logical:1" }),
            )
            .await
            .unwrap();
        assert_eq!(r["logical_output"]["id"], "logical:1");
        let listed = rig
            .call("delivery.list_logical_outputs", json!({ "v": 1 }))
            .await
            .unwrap();
        assert_eq!(listed["logical_outputs"], json!([]));
    }

    /// `zone.add` and `zone.update` hold the logical read lock
    /// from the membership check through the zone write. With
    /// the zone lock held from outside, each verb is parked at
    /// the zone write, and the logical registry cannot be taken
    /// for writing while it waits: a removal cannot slip between
    /// the check and the write. Once the zone lock is released
    /// the verb completes and the logical lock is free again.
    #[tokio::test]
    async fn zone_add_and_update_hold_the_logical_lock_until_the_zone_is_written(
    ) {
        let rig = Arc::new(OutputRig::new().await.with_zones().await);
        rig.call("zone.add", json!({ "v": 1, "name": "Seed", "logical_output_ids": ["logical:2"] }))
            .await
            .unwrap();
        for (verb, payload) in [
            (
                "zone.add",
                json!({ "v": 1, "name": "Down", "logical_output_ids": ["logical:1"] }),
            ),
            (
                "zone.update",
                json!({ "v": 1, "id": "zone:1", "name": "Seed renamed" }),
            ),
        ] {
            let zones_held = rig.plugin.zones.write().await;
            let task = {
                let rig = Arc::clone(&rig);
                let verb = verb.to_string();
                tokio::spawn(async move { rig.call(&verb, payload).await })
            };
            // Let the verb run up to the zone write it cannot take.
            for _ in 0..50 {
                tokio::task::yield_now().await;
            }
            assert!(
                !task.is_finished(),
                "{verb} must be parked at the zone write"
            );
            assert!(
                rig.plugin.logical.try_write().is_err(),
                "{verb} released the logical lock before the zone write"
            );
            drop(zones_held);
            task.await
                .unwrap()
                .unwrap_or_else(|e| panic!("{verb}: {e}"));
            assert!(
                rig.plugin.logical.try_write().is_ok(),
                "{verb} keeps the logical lock after answering"
            );
        }
    }

    // ----- zone EQ: stored and answered, not rendered -----

    fn flat_eq() -> Value {
        let band = json!({ "freq_hz": 1000, "gain_db": 0.0, "q": 1.0 });
        json!({ "engaged": false, "bands": vec![band; 10] })
    }

    /// A band write on one zone changes that zone's band and
    /// nothing else: not the other zone, not the name, not the
    /// membership, not the other bands, not the options EQ (the
    /// plugin has no path to it: no mixer call, no drop-in, no
    /// dispatch). Every zone answer carries the EQ, and a new
    /// zone starts flat and off.
    #[tokio::test]
    async fn a_band_write_on_one_zone_changes_nothing_else() {
        let rig = OutputRig::new().await.with_zones().await;
        let d = rig
            .call("zone.add", json!({ "v": 1, "name": "Down", "logical_output_ids": ["logical:1"] }))
            .await
            .unwrap();
        assert_eq!(
            d["zone"]["eq"],
            flat_eq(),
            "a new zone starts flat and off"
        );
        let u = rig
            .call("zone.add", json!({ "v": 1, "name": "Up", "logical_output_ids": ["logical:2"] }))
            .await
            .unwrap();

        let e = rig
            .call(
                "zone.set_eq_engaged",
                json!({ "v": 1, "id": "zone:1", "engaged": true }),
            )
            .await
            .unwrap();
        assert_eq!(e["zone"]["eq"]["engaged"], true);
        assert_eq!(e["zone"]["name"], "Down");
        let b = rig
            .call("zone.set_eq_band", json!({ "v": 1, "id": "zone:1", "index": 2, "freq_hz": 120, "gain_db": -3.0, "q": 1.4 }))
            .await
            .unwrap();
        assert_eq!(
            b["zone"]["eq"]["bands"][2],
            json!({ "freq_hz": 120, "gain_db": -3.0, "q": 1.4 })
        );
        assert_eq!(b["zone"]["eq"]["engaged"], true, "the flag stays");
        assert_eq!(b["zone"]["name"], "Down");
        assert_eq!(b["zone"]["logical_output_ids"], json!(["logical:1"]));
        for i in (0..10).filter(|i| *i != 2) {
            assert_eq!(
                b["zone"]["eq"]["bands"][i],
                json!({ "freq_hz": 1000, "gain_db": 0.0, "q": 1.0 }),
                "band {i} stays"
            );
        }

        let listed = rig.call("zone.list", json!({ "v": 1 })).await.unwrap();
        assert_eq!(
            listed["zones"][1], u["zone"],
            "the other zone is untouched, EQ included"
        );
        assert_eq!(listed["zones"][0], b["zone"], "the list carries the EQ");

        // Renaming leaves the EQ; the update answer carries it.
        let r = rig
            .call(
                "zone.update",
                json!({ "v": 1, "id": "zone:1", "name": "Ground" }),
            )
            .await
            .unwrap();
        assert_eq!(r["zone"]["eq"], b["zone"]["eq"]);

        // No path to the options EQ or the chain was taken.
        assert!(rig.mixer.calls.lock().unwrap().is_empty());
        assert_eq!(rig.drop_in().await, "");
    }

    /// Unknown zone, index out of range, and each field out of
    /// range are refused; the refusal names the field; the zone
    /// stays as it was.
    #[tokio::test]
    async fn an_eq_refusal_names_the_field_and_changes_nothing() {
        let rig = OutputRig::new().await.with_zones().await;
        rig.call("zone.add", json!({ "v": 1, "name": "Down", "logical_output_ids": ["logical:1"] }))
            .await
            .unwrap();
        let before = rig.call("zone.list", json!({ "v": 1 })).await.unwrap();
        let err = rig
            .call(
                "zone.set_eq_engaged",
                json!({ "v": 1, "id": "zone:9", "engaged": true }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("unknown zone"), "{err}");
        let err = rig
            .call("zone.set_eq_band", json!({ "v": 1, "id": "zone:1", "index": 10, "freq_hz": 1000, "gain_db": 0.0, "q": 1.0 }))
            .await
            .unwrap_err();
        assert!(err.contains("band index 10 is out of range"), "{err}");
        for (payload, field) in [
            (
                json!({ "freq_hz": 19, "gain_db": 0.0, "q": 1.0 }),
                "freq_hz",
            ),
            (
                json!({ "freq_hz": 1000, "gain_db": 15.5, "q": 1.0 }),
                "gain_db",
            ),
            (json!({ "freq_hz": 1000, "gain_db": 0.0, "q": 31.0 }), "q"),
        ] {
            let mut p = payload.clone();
            p["v"] = json!(1);
            p["id"] = json!("zone:1");
            p["index"] = json!(0);
            let err = rig.call("zone.set_eq_band", p).await.unwrap_err();
            assert!(err.contains(&format!("{field} ")), "{field}: {err}");
            assert!(err.contains("out of range"), "{err}");
        }
        assert_eq!(
            rig.call("zone.list", json!({ "v": 1 })).await.unwrap(),
            before
        );
    }

    /// The EQ survives a restart, and a zones file written before
    /// the EQ existed loads with the default EQ.
    #[tokio::test]
    async fn the_zone_eq_survives_a_restart_and_an_old_file_loads_flat() {
        let rig = OutputRig::new().await.with_zones().await;
        rig.call("zone.add", json!({ "v": 1, "name": "Down", "logical_output_ids": ["logical:1"] }))
            .await
            .unwrap();
        rig.call("zone.set_eq_band", json!({ "v": 1, "id": "zone:1", "index": 9, "freq_hz": 12000, "gain_db": 2.5, "q": 0.5 }))
            .await
            .unwrap();
        let again = |dir: PathBuf| async move {
            let mut p = loaded_plugin().await;
            p.install_zones(dir.join(ZoneStore::FILE_NAME)).await;
            p
        };
        let p = again(rig.dir.path().to_path_buf()).await;
        let v: Value = serde_json::from_slice(
            &p.handle_request(&request("zone.list", json!({ "v": 1 })))
                .await
                .unwrap()
                .payload,
        )
        .unwrap();
        assert_eq!(
            v["zones"][0]["eq"]["bands"][9],
            json!({ "freq_hz": 12000, "gain_db": 2.5, "q": 0.5 })
        );

        tokio::fs::write(
            rig.dir.path().join(ZoneStore::FILE_NAME),
            "v = 1\nnext_id = 2\n\n[[zones]]\nid = \"zone:1\"\nname = \"Old\"\nlogical_output_ids = [\"logical:1\"]\n",
        )
        .await
        .unwrap();
        let p = again(rig.dir.path().to_path_buf()).await;
        let v: Value = serde_json::from_slice(
            &p.handle_request(&request("zone.list", json!({ "v": 1 })))
                .await
                .unwrap()
                .payload,
        )
        .unwrap();
        assert_eq!(
            v["zones"][0]["eq"],
            flat_eq(),
            "an old file loads with the default EQ"
        );
    }

    /// The zone EQ is stored and answered and nothing more: the
    /// zone store and the two handlers reach no mixer, no
    /// drop-in, no dispatcher, no subject and no options verb.
    #[test]
    fn the_zone_eq_is_not_rendered() {
        // Code only: a comment may say "renders nothing".
        fn code(src: &str) -> String {
            src.lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n")
        }
        let zones = code(
            include_str!("zones.rs")
                .split("#[cfg(test)]")
                .next()
                .unwrap(),
        );
        let lib = include_str!("lib.rs");
        let start = lib.find("async fn handle_zone_set_eq_engaged").unwrap();
        let end = lib[start..].find("/// `zone.list` — every zone").unwrap();
        let handlers = code(&lib[start..start + end]);
        for forbidden in [
            "options.",
            "amixer",
            "drop_in",
            "render",
            "mixer",
            "dispatch(",
            "announce(",
            "emit_plugin_event",
            "eq_only",
            "softvol",
        ] {
            assert!(!zones.contains(forbidden), "zones.rs names {forbidden:?}");
            assert!(
                !handlers.contains(forbidden),
                "the eq handlers name {forbidden:?}"
            );
        }
    }

    // ----- the zone EQ claim: which zone's EQ the one pipe sounds -----

    fn claimed(zone: &str) -> Value {
        json!(zone)
    }

    /// The claim follows the terminus and the one engaged zone
    /// holding a logical output on it: engaging claims, a band
    /// write republishes the bands, pausing drops the claim,
    /// playing another output claims by that output's zone, and
    /// re-pointing a logical output moves the claim with it.
    #[tokio::test]
    async fn the_claim_follows_the_terminus_and_the_one_engaged_zone() {
        let rig = OutputRig::new()
            .await
            .with_zones()
            .await
            .with_options(hardware_mixer_on_dac())
            .await;
        rig.call("zone.add", json!({ "v": 1, "name": "Down", "logical_output_ids": ["logical:1"] })).await.unwrap();
        assert_eq!(
            rig.last_claim().map(|c| c["claimed"].clone()),
            Some(json!(false)),
            "a zone not engaged does not claim"
        );

        rig.call(
            "zone.set_eq_engaged",
            json!({ "v": 1, "id": "zone:1", "engaged": true }),
        )
        .await
        .unwrap();
        let c = rig.last_claim().unwrap();
        assert_eq!(c["claimed"], true);
        assert_eq!(c["zone_id"], claimed("zone:1"));
        assert_eq!(c["engaged"], true);
        assert_eq!(c["bands"].as_array().unwrap().len(), 10);
        assert_eq!(c["v"], 1);

        rig.call("zone.set_eq_band", json!({ "v": 1, "id": "zone:1", "index": 0, "freq_hz": 100, "gain_db": 4.0, "q": 1.0 })).await.unwrap();
        let c = rig.last_claim().unwrap();
        assert_eq!(
            c["bands"][0],
            json!({ "freq_hz": 100, "gain_db": 4.0, "q": 1.0 }),
            "a band write on the claiming zone republishes its bands"
        );

        rig.verb("delivery.pause_output", "hw:0,0").await.unwrap();
        assert_eq!(
            rig.last_claim().unwrap(),
            json!({ "v": 1, "claimed": false }),
            "a silent terminus does not claim"
        );

        rig.verb("delivery.play_output", "hw:1,0").await.unwrap();
        assert_eq!(
            rig.last_claim().unwrap()["claimed"],
            false,
            "no engaged zone on HDMI"
        );
        rig.call("zone.add", json!({ "v": 1, "name": "Up", "logical_output_ids": ["logical:2"] })).await.unwrap();
        rig.call(
            "zone.set_eq_engaged",
            json!({ "v": 1, "id": "zone:2", "engaged": true }),
        )
        .await
        .unwrap();
        assert_eq!(rig.last_claim().unwrap()["zone_id"], claimed("zone:2"));

        rig.call("zone.remove", json!({ "v": 1, "id": "zone:2" }))
            .await
            .unwrap();
        assert_eq!(
            rig.last_claim().unwrap()["claimed"],
            false,
            "the zone is gone"
        );

        // Kitchen now points at HDMI: Down holds it and is engaged.
        rig.call(
            "delivery.update_logical_output",
            json!({ "v": 1, "id": "logical:1", "output_id": "hw:1,0" }),
        )
        .await
        .unwrap();
        let c = rig.last_claim().unwrap();
        assert_eq!(
            c["zone_id"],
            claimed("zone:1"),
            "a logical target change moves the claim"
        );
        assert_eq!(c["bands"][0]["gain_db"], 4.0);
    }

    /// Two engaged zones on one terminus do not claim; the claim
    /// returns when one is disengaged.
    #[tokio::test]
    async fn two_engaged_zones_on_one_terminus_do_not_claim() {
        let rig = OutputRig::new()
            .await
            .with_zones()
            .await
            .with_options(hardware_mixer_on_dac())
            .await;
        rig.call(
            "delivery.add_logical_output",
            json!({ "v": 1, "name": "Porch", "output_id": "hw:0,0" }),
        )
        .await
        .unwrap();
        rig.call("zone.add", json!({ "v": 1, "name": "Down", "logical_output_ids": ["logical:1"] })).await.unwrap();
        rig.call("zone.add", json!({ "v": 1, "name": "Side", "logical_output_ids": ["logical:3"] })).await.unwrap();
        rig.call(
            "zone.set_eq_engaged",
            json!({ "v": 1, "id": "zone:1", "engaged": true }),
        )
        .await
        .unwrap();
        assert_eq!(rig.last_claim().unwrap()["zone_id"], claimed("zone:1"));
        rig.call(
            "zone.set_eq_engaged",
            json!({ "v": 1, "id": "zone:2", "engaged": true }),
        )
        .await
        .unwrap();
        assert_eq!(
            rig.last_claim().unwrap(),
            json!({ "v": 1, "claimed": false }),
            "two engaged zones on the terminus: the options eq sounds"
        );
        rig.call(
            "zone.set_eq_engaged",
            json!({ "v": 1, "id": "zone:1", "engaged": false }),
        )
        .await
        .unwrap();
        assert_eq!(rig.last_claim().unwrap()["zone_id"], claimed("zone:2"));
    }

    /// A band write on a zone that does not hold the claim
    /// publishes nothing new: the claim, and so the sounding
    /// bands, stay exactly as they were.
    #[tokio::test]
    async fn a_band_write_on_a_zone_without_the_claim_publishes_nothing_new() {
        let rig = OutputRig::new()
            .await
            .with_zones()
            .await
            .with_options(hardware_mixer_on_dac())
            .await;
        rig.call("zone.add", json!({ "v": 1, "name": "Down", "logical_output_ids": ["logical:1"] })).await.unwrap();
        rig.call(
            "zone.set_eq_engaged",
            json!({ "v": 1, "id": "zone:1", "engaged": true }),
        )
        .await
        .unwrap();
        rig.call("zone.add", json!({ "v": 1, "name": "Up", "logical_output_ids": ["logical:2"] })).await.unwrap();
        rig.call(
            "zone.set_eq_engaged",
            json!({ "v": 1, "id": "zone:2", "engaged": true }),
        )
        .await
        .unwrap();
        let before = rig.last_claim().unwrap();
        assert_eq!(before["zone_id"], claimed("zone:1"));
        let n = rig.claim_count();
        rig.call("zone.set_eq_band", json!({ "v": 1, "id": "zone:2", "index": 0, "freq_hz": 100, "gain_db": 9.0, "q": 1.0 })).await.unwrap();
        rig.call(
            "zone.update",
            json!({ "v": 1, "id": "zone:2", "name": "Upstairs" }),
        )
        .await
        .unwrap();
        assert_eq!(rig.claim_count(), n, "nothing new was published");
        assert_eq!(rig.last_claim().unwrap(), before);
    }

    /// The claim is computed from the operator's output device
    /// when no output verb has bound the chain, so an options
    /// change moves it; and the announcement at load carries the
    /// subject's type, addressing and current claim.
    #[tokio::test]
    async fn the_follow_device_moves_the_claim_and_load_announces_it() {
        let rig = OutputRig::new()
            .await
            .with_zones()
            .await
            .with_options(hardware_mixer_on_dac())
            .await;
        rig.call("zone.add", json!({ "v": 1, "name": "Down", "logical_output_ids": ["logical:1"] })).await.unwrap();
        rig.call(
            "zone.set_eq_engaged",
            json!({ "v": 1, "id": "zone:1", "engaged": true }),
        )
        .await
        .unwrap();
        assert_eq!(rig.last_claim().unwrap()["zone_id"], claimed("zone:1"));
        *rig.plugin.last_options.write().await = Some(LastOptions {
            settings: OptionsSettings {
                output_device: Some("hw:1,0".to_string()),
                ..hardware_mixer_on_dac()
            },
            subject_canonical_id: "options-2".to_string(),
        });
        rig.plugin.publish_eq_claim().await;
        assert_eq!(
            rig.last_claim().unwrap(),
            json!({ "v": 1, "claimed": false }),
            "the operator's device is HDMI now"
        );

        rig.plugin.eq_claim_publisher().announce().await;
        let a = rig
            .announcer
            .announced
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap();
        assert_eq!(a.subject_type, SUBJECT_TYPE_EQ_CLAIM);
        assert_eq!(
            a.addressings,
            vec![ExternalAddressing {
                scheme: "evo.audio.zones".to_string(),
                value: "eq_claim".to_string()
            }]
        );
        assert_eq!(a.state, json!({ "v": 1, "claimed": false }));
    }

    /// Every path that can move the claim republishes it: the
    /// eight logical-output and zone handlers, the output-verb
    /// rebind, the binding release at unload, the options
    /// observer after its render, and both hardware-observer
    /// republishes. Nothing in the claim path renders or mixes.
    #[test]
    fn every_claim_moving_path_republishes_and_the_claim_renders_nothing() {
        let lib = include_str!("lib.rs");
        let production = lib.split("#[cfg(test)]\nmod tests").next().unwrap();
        assert_eq!(
            production.matches("self.publish_eq_claim().await;").count(),
            11,
            "eight handlers, the rebind, the release, the scene recall"
        );
        let observer_start =
            production.find("async fn spawn_options_observer").unwrap();
        let observer_end = production[observer_start..]
            .find("async fn stop_options_observer")
            .unwrap();
        let observer =
            &production[observer_start..observer_start + observer_end];
        assert!(
            observer.contains("eq_claim.publish().await;"),
            "the options observer republishes after its render"
        );
        let hw_start = production
            .find("async fn spawn_hardware_audio_observer")
            .unwrap();
        let hw_end = production[hw_start..]
            .find("async fn stop_hardware_audio_observer")
            .unwrap();
        let hw = &production[hw_start..hw_start + hw_end];
        assert_eq!(
            hw.matches("eq_claim_for_task.publish().await;").count(),
            2,
            "both republishes of the outputs list"
        );
        // Code only: a comment may say nothing here renders.
        let claim: String = include_str!("eq_claim.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap()
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for forbidden in [
            "amixer", "drop_in", "render", "mixer", "softvol", "options.",
        ] {
            assert!(
                !claim.contains(forbidden),
                "eq_claim.rs names {forbidden:?}"
            );
        }
    }

    // ----- scenes: a name and a snapshot of every zone -----

    async fn zones_now(rig: &OutputRig) -> Value {
        rig.call("zone.list", json!({ "v": 1 })).await.unwrap()["zones"].clone()
    }

    /// Save keeps every zone as it is; recall brings it back
    /// after the zones changed, ids and EQ included, clears on an
    /// empty snapshot, and republishes the claim; list and remove
    /// answer as ordered; the logical outputs, the physical
    /// outputs and the options EQ are untouched throughout.
    #[tokio::test]
    async fn a_scene_saves_and_recalls_the_zones_and_nothing_else() {
        let rig = OutputRig::new()
            .await
            .with_zones()
            .await
            .with_options(hardware_mixer_on_dac())
            .await;
        let outputs_before = rig.snapshot_outputs().await;
        let options_before = rig.plugin.last_options.read().await.clone();
        // A scene saved before any zone exists is the empty one.
        rig.call("scene.save", json!({ "v": 1, "name": "Empty" }))
            .await
            .unwrap();
        rig.call("zone.add", json!({ "v": 1, "name": "Down", "logical_output_ids": ["logical:1"] })).await.unwrap();
        rig.call(
            "zone.set_eq_engaged",
            json!({ "v": 1, "id": "zone:1", "engaged": true }),
        )
        .await
        .unwrap();
        rig.call("zone.set_eq_band", json!({ "v": 1, "id": "zone:1", "index": 0, "freq_hz": 100, "gain_db": 5.0, "q": 1.0 })).await.unwrap();
        rig.call("zone.add", json!({ "v": 1, "name": "Up", "logical_output_ids": ["logical:2"] })).await.unwrap();
        let saved_zones = zones_now(&rig).await;

        let s = rig
            .call("scene.save", json!({ "v": 1, "name": " Evening " }))
            .await
            .unwrap();
        assert_eq!(s["scene"]["name"], "Evening");
        assert_eq!(s["scene"]["zones"], saved_zones);
        let listed = rig.call("scene.list", json!({ "v": 1 })).await.unwrap();
        assert_eq!(listed, json!({ "v": 1, "scenes": ["Empty", "Evening"] }));

        // Change the zones: remove one, add another, change the EQ.
        rig.call("zone.remove", json!({ "v": 1, "id": "zone:1" }))
            .await
            .unwrap();
        rig.call("zone.add", json!({ "v": 1, "name": "Porch", "logical_output_ids": ["logical:1"] })).await.unwrap();
        rig.call("zone.set_eq_band", json!({ "v": 1, "id": "zone:2", "index": 3, "freq_hz": 400, "gain_db": -2.0, "q": 2.0 })).await.unwrap();
        assert_ne!(zones_now(&rig).await, saved_zones);
        assert_eq!(
            rig.last_claim().unwrap()["claimed"],
            false,
            "Porch holds the terminus but is not engaged"
        );

        let n = rig.claim_count();
        let r = rig
            .call("scene.recall", json!({ "v": 1, "name": "evening" }))
            .await
            .unwrap();
        assert_eq!(r["scene"]["name"], "Evening");
        assert_eq!(
            zones_now(&rig).await,
            saved_zones,
            "the snapshot is live, ids and eq included"
        );
        assert!(rig.claim_count() > n, "the claim was published again");
        assert_eq!(
            rig.last_claim().unwrap()["zone_id"],
            "zone:1",
            "Down is engaged on the terminus again"
        );
        let added = rig.call("zone.add", json!({ "v": 1, "name": "Next", "logical_output_ids": ["logical:2"] })).await;
        assert!(added.is_err(), "logical:2 is Up's");
        rig.call("zone.remove", json!({ "v": 1, "id": "zone:2" }))
            .await
            .unwrap();
        let added = rig.call("zone.add", json!({ "v": 1, "name": "Next", "logical_output_ids": ["logical:2"] })).await.unwrap();
        assert_eq!(
            added["zone"]["id"], "zone:4",
            "next_id did not move backward"
        );

        // The empty scene clears the zones.
        rig.call("scene.recall", json!({ "v": 1, "name": "Empty" }))
            .await
            .unwrap();
        assert_eq!(zones_now(&rig).await, json!([]));
        assert_eq!(rig.last_claim().unwrap()["claimed"], false);

        // Nothing but zones moved.
        assert_eq!(
            rig.snapshot_outputs().await,
            outputs_before,
            "logical and physical outputs untouched"
        );
        assert_eq!(
            *rig.plugin.last_options.read().await,
            options_before,
            "the options eq untouched"
        );
        assert!(rig.mixer.calls.lock().unwrap().is_empty());
        assert_eq!(rig.drop_in().await, "");

        // Remove answers the scene; unknown is refused; the live
        // zones stay, so recall Evening first and watch it stay
        // live through the remove.
        rig.call("scene.recall", json!({ "v": 1, "name": "Evening" }))
            .await
            .unwrap();
        assert_eq!(zones_now(&rig).await, saved_zones);
        let gone = rig
            .call("scene.remove", json!({ "v": 1, "name": "Evening" }))
            .await
            .unwrap();
        assert_eq!(gone["scene"]["zones"], saved_zones);
        assert_eq!(zones_now(&rig).await, saved_zones, "the live zones stay");
        let err = rig
            .call("scene.remove", json!({ "v": 1, "name": "Evening" }))
            .await
            .unwrap_err();
        assert!(err.contains("unknown scene"), "{err}");
        assert_eq!(zones_now(&rig).await, saved_zones);
        assert_eq!(
            rig.call("scene.list", json!({ "v": 1 })).await.unwrap()["scenes"],
            json!(["Empty"])
        );
    }

    /// A refused recall changes no zone: an unknown name, a
    /// snapshot naming a logical output the plugin no longer has,
    /// and a snapshot with one logical output in two zones.
    #[tokio::test]
    async fn a_refused_recall_changes_no_zone() {
        let rig = OutputRig::new().await.with_zones().await;
        rig.call(
            "delivery.add_logical_output",
            json!({ "v": 1, "name": "Porch", "output_id": "hw:1,0" }),
        )
        .await
        .unwrap();
        rig.call("zone.add", json!({ "v": 1, "name": "Side", "logical_output_ids": ["logical:3"] })).await.unwrap();
        rig.call("scene.save", json!({ "v": 1, "name": "WithPorch" }))
            .await
            .unwrap();
        rig.call("zone.remove", json!({ "v": 1, "id": "zone:1" }))
            .await
            .unwrap();
        rig.call(
            "delivery.remove_logical_output",
            json!({ "v": 1, "id": "logical:3" }),
        )
        .await
        .unwrap();
        rig.call("zone.add", json!({ "v": 1, "name": "Down", "logical_output_ids": ["logical:1"] })).await.unwrap();
        let live = zones_now(&rig).await;

        let err = rig
            .call("scene.recall", json!({ "v": 1, "name": "Nope" }))
            .await
            .unwrap_err();
        assert!(err.contains("unknown scene"), "{err}");
        assert_eq!(zones_now(&rig).await, live);

        let err = rig
            .call("scene.recall", json!({ "v": 1, "name": "WithPorch" }))
            .await
            .unwrap_err();
        assert!(
            err.contains("unknown logical output \\\"logical:3\\\""),
            "{err}"
        );
        assert_eq!(
            zones_now(&rig).await,
            live,
            "a refused recall changed nothing"
        );

        // A snapshot with one logical output in two zones, as a
        // hand-edited scenes file could hold.
        let mut bad = Scenes::default();
        let zone = |id: &str, ids: &[&str]| Zone {
            id: id.to_string(),
            name: id.to_string(),
            logical_output_ids: ids.iter().map(|s| s.to_string()).collect(),
            eq: zones::ZoneEq::default(),
        };
        bad.save(
            "Shared",
            vec![
                zone("zone:7", &["logical:1"]),
                zone("zone:8", &["logical:2", "logical:1"]),
            ],
        )
        .unwrap();
        *rig.plugin.scenes.write().await = bad;
        let err = rig
            .call("scene.recall", json!({ "v": 1, "name": "Shared" }))
            .await
            .unwrap_err();
        assert!(
            err.contains("in both \\\"zone:7\\\" and \\\"zone:8\\\""),
            "{err}"
        );
        assert_eq!(zones_now(&rig).await, live);
    }

    /// At most 32 scenes: the 33rd save is refused and nothing is
    /// written; a save of an existing name replaces it. Scenes
    /// survive a restart; a malformed file is no scenes and the
    /// zones stay.
    #[tokio::test]
    async fn scenes_are_capped_replaced_and_persisted() {
        let rig = OutputRig::new().await.with_zones().await;
        rig.call("zone.add", json!({ "v": 1, "name": "Down", "logical_output_ids": ["logical:1"] })).await.unwrap();
        for i in 0..32 {
            rig.call(
                "scene.save",
                json!({ "v": 1, "name": format!("Scene {i}") }),
            )
            .await
            .unwrap();
        }
        let path = rig.dir.path().join(SceneStore::FILE_NAME);
        let file_before = tokio::fs::read(&path).await.unwrap();
        let err = rig
            .call("scene.save", json!({ "v": 1, "name": "Thirty-three" }))
            .await
            .unwrap_err();
        assert!(err.contains("already 32 scenes"), "{err}");
        assert_eq!(
            tokio::fs::read(&path).await.unwrap(),
            file_before,
            "nothing written"
        );
        assert_eq!(
            rig.call("scene.list", json!({ "v": 1 })).await.unwrap()["scenes"]
                .as_array()
                .unwrap()
                .len(),
            32
        );

        rig.call(
            "zone.set_eq_engaged",
            json!({ "v": 1, "id": "zone:1", "engaged": true }),
        )
        .await
        .unwrap();
        let replaced = rig
            .call("scene.save", json!({ "v": 1, "name": "scene 5" }))
            .await
            .unwrap();
        assert_eq!(
            replaced["scene"]["zones"][0]["eq"]["engaged"], true,
            "replaced with the zones as they are now"
        );
        assert_eq!(
            rig.call("scene.list", json!({ "v": 1 })).await.unwrap()["scenes"]
                .as_array()
                .unwrap()
                .len(),
            32
        );

        let mut again = loaded_plugin().await;
        again.install_scenes(path.clone()).await;
        let listed: Value = serde_json::from_slice(
            &again
                .handle_request(&request("scene.list", json!({ "v": 1 })))
                .await
                .unwrap()
                .payload,
        )
        .unwrap();
        assert_eq!(listed["scenes"].as_array().unwrap().len(), 32);
        assert_eq!(listed["scenes"][5], "scene 5");

        tokio::fs::write(&path, "v = \"nope\"\n[[scenes")
            .await
            .unwrap();
        let mut broken = loaded_plugin().await;
        broken
            .install_zones(rig.dir.path().join(ZoneStore::FILE_NAME))
            .await;
        broken.install_scenes(path).await;
        let listed: Value = serde_json::from_slice(
            &broken
                .handle_request(&request("scene.list", json!({ "v": 1 })))
                .await
                .unwrap()
                .payload,
        )
        .unwrap();
        assert_eq!(
            listed["scenes"],
            json!([]),
            "a file that does not parse is no scenes"
        );
        let zones: Value = serde_json::from_slice(
            &broken
                .handle_request(&request("zone.list", json!({ "v": 1 })))
                .await
                .unwrap()
                .payload,
        )
        .unwrap();
        assert_eq!(
            zones["zones"].as_array().unwrap().len(),
            1,
            "and the live zones stay"
        );
    }
}
