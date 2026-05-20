//! # evo-device-audio-distribution
//!
//! Reference audio-domain steward binary. Composes evo-core's
//! steward (`evo::run`) with the evo-device-audio plugin set
//! admitted in-process, producing a single deployable binary
//! that exercises the framework's audio data plane on real
//! hardware.
//!
//! The binary follows the canonical
//! `evo-example-distribution` shape: a thin `main.rs` that
//! parses the steward's CLI arguments and delegates to
//! [`evo::run`] with a custom [`evo::AdmissionSetup`] that
//! admits the audio plugin set programmatically.
//!
//! ## Plugin set
//!
//! baseline ships:
//!
//! - `org.evoframework.composition.alsa` — substrate-aware
//!   composition stage; admits as a singleton respondent on
//!   the `audio.composition` shelf at shape 2.
//! - `org.evoframework.delivery.alsa` — delivery stage that
//!   owns the modular ALSA pipeline (`pcm.evo`) and declares
//!   the WriteEndpoint other audio-producing plugins write
//!   into.
//! - `org.evoframework.playback.mpd` — warden + source +
//!   respondent for MPD-backed playback; admits via the
//!   framework's
//!   [`AdmissionEngine::admit_singleton_warden_with_respondent`]
//!   path so both course_correct and source-verb dispatches
//!   route to the same plugin instance.
//! - `org.evoframework.playback.options` — operator-facing
//!   audiophile-grade settings; emits a `PluginEvent` on
//!   every change that delivery.alsa consumes to re-render
//!   the pipeline.
//! - `org.evoframework.network` — multi-source network
//!   surface; fans in from rtnetlink, NetworkManager D-Bus,
//!   and a universal polling floor under a per-platform
//!   source preset (env-overridable via
//!   `EVO_NETWORK_SUPERVISOR_PRESET`); consumes the framework's
//!   PPAG resolution for the `nmcli_invocation` intent to
//!   install an EUID-aware dispatch composite (direct under
//!   root, `sudo -n` under a non-root service user).
//! - `org.evoframework.multiroom.evo-native` — multi-room
//!   coordination plugin; consumes the audio-plane substrate
//!   for source-host fan-out and receiver-side rendering.
//! - `org.evoframework.metadata.local` — tag-based metadata
//!   over local-file libraries; sync I/O wrapped on the
//!   blocking thread pool.
//! - `org.evoframework.artwork.local` — cover-art resolver
//!   over local-file libraries; sync I/O wrapped on the
//!   blocking thread pool.
//!
//! ## Admission posture
//!
//! Two phases compose this distribution's admission setup:
//!
//! Phase 1 — programmatic compile-link baseline. The eight
//! reference plugins above are admitted in-process via the
//! framework's `admit_singleton_*` entry points. Compile-link
//! is one of four equal admission paths the framework
//! documents (PLUGIN_PACKAGING.md §1+§3) — the other three
//! being cdylib, OOP subprocess, and WASM. The compile-link
//! path is the natural shape for the curated reference set:
//! the plugins ship with the distribution binary and are
//! covered by its release tests.
//!
//! Phase 2 — filesystem discovery for operator-installed
//! plugins. After the baseline admits, the framework's
//! [`evo::plugin_discovery::discover_and_admit`] walks
//! `plugins.search_roots` (default `/opt/evo/plugins` then
//! `/var/lib/evo/plugins`) and admits any out-of-process
//! plugin bundle the operator has dropped in. Discovery
//! honours the operator-disabled persistence record from the
//! framework's `plugins.installed` registrar, so an operator
//! who has disabled an OOP plugin via the operator shelf
//! stays disabled across restarts. In-process bundles found
//! in `search_roots` are skipped with a warning — the
//! compile-link baseline is the only in-process source.
//!
//! ## Lifecycle property
//!
//! The architectural property the framework promises is
//! "every plugin's install / remove / update / enable /
//! disable lifecycle reaches its runtime effect". The
//! reach-without-restart property is delivered by the
//! framework already for plugins admitted through Phase 2
//! (filesystem discovery + the operator shelf's
//! `enable_plugin` / `disable_plugin` / `reload_plugin`
//! verbs). The Phase 1 baseline carries the constraint that
//! the compile-link path needs a steward restart for the
//! same operations, because the plugin code is linked into
//! the binary. The per-plugin closure criterion is: when a
//! reference plugin ships as a runtime-loadable artefact
//! (cdylib, OOP subprocess, or WASM module), its Phase 1
//! admit call here is removed; Phase 2 picks it up from
//! `search_roots` instead. The distribution then becomes a
//! thin packaging layer (default plugin set on disk,
//! systemd unit, bootstrap) rather than a binary with
//! plugins baked in.
//!
//! ## Catalogue + boundary
//!
//! The distribution's catalogue declares the `audio` rack
//! with `composition` + `playback` shelves; the steward
//! reads it at boot via `evo.toml`'s `[catalogue]` section.
//! Catalogue authoring + the systemd unit + the tier of
//! sudoers drop-ins (mpd restart, alsa group, etc.) are
//! distribution-tier provisioning, not plugin code; they
//! live in the deploy script's per-target setup.

#![forbid(unsafe_code)]
#![allow(missing_docs)]

use clap::Parser as _;
use std::sync::Arc;

use anyhow::Context;
use evo::admission::AdmissionEngine;
use evo::config::StewardConfig;
use evo::{AdmissionSetup, RuntimeSetup, RuntimeSetupContext};
use evo_plugin_sdk::Manifest;
use org_evoframework_composition_alsa::AlsaCompositionPlugin;
use org_evoframework_delivery_alsa::AlsaDeliveryPlugin;
use org_evoframework_multiroom_evo_native::MultiroomEvoNativePlugin;
use org_evoframework_playback_mpd::MpdPlaybackPlugin;
use org_evoframework_playback_options::PlaybackOptionsPlugin;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = evo::cli::Args::parse();
    let opts = evo::RunOptions::new(args, audio_distribution_admission())
        .with_post_admission(audio_distribution_post_admission())
        .with_runtime_setup(audio_distribution_runtime_setup());
    evo::run(opts).await
}

/// Build the audio distribution's runtime-setup closure. The
/// framework invokes this once during boot after its data-
/// plane substrates exist and the audio plane has started but
/// before admission begins. We construct the multi-room
/// crate's `ElectionRuntime` against the framework substrates
/// exposed in [`RuntimeSetupContext`], rehydrate it from
/// persistence, attach the audio-plane runtime (so election's
/// liveness predicate sees in-flight channel activity), start
/// it, install it into the framework's shared election-state
/// handle, and register an async shutdown closure into the
/// supplied registry. From this point every framework consumer
/// (audio_plane, group_topology, server wire ops) reads
/// election state through the multi-room runtime; the
/// framework crate itself has no production dep on
/// `evo-multiroom`.
fn audio_distribution_runtime_setup() -> RuntimeSetup {
    Box::new(|ctx: RuntimeSetupContext| {
        Box::pin(async move {
            let RuntimeSetupContext {
                bus,
                persistence,
                group_store,
                device_id,
                shared_election_state,
                shared_role_store,
                multiroom_substrate_slot,
                audio_plane_runtime,
                shutdown_registry,
                ..
            } = ctx;

            let election_runtime =
                Arc::new(evo_multiroom::ElectionRuntime::new(
                    Arc::clone(&persistence),
                    Arc::clone(&bus),
                    Arc::clone(&group_store),
                    device_id,
                    evo_multiroom::ElectionConfig::default(),
                ));

            if let Err(e) = election_runtime.rehydrate().await {
                tracing::warn!(
                    error = %e,
                    "election runtime: rehydrate failed; substrate may be \
                     empty or corrupt"
                );
            }

            // Attach the audio-plane runtime so election's
            // liveness predicate accepts peers with an active
            // audio-plane TCP connection as alive even when
            // mDNS-SD record freshness has aged past the
            // liveness window.
            election_runtime.with_audio_plane(Arc::clone(&audio_plane_runtime));

            if let Err(e) = election_runtime.start().await {
                tracing::warn!(
                    error = %e,
                    "election runtime: start failed; source-host election \
                     will not function on this boot"
                );
            } else {
                let elections = election_runtime.list().await.len();
                tracing::info!(elections, "election runtime: ready");
            }

            // Install the concrete election runtime into the
            // framework's shared handle. All framework
            // consumers (audio_plane, group_topology, server
            // wire ops) see the swap on their next
            // `current()` read; no further framework-side
            // rewiring is needed.
            shared_election_state.set(Arc::clone(&election_runtime)
                as Arc<dyn evo_primitives::ElectionState>);

            // Register the shutdown closure. The framework's
            // drain path invokes every registered hook in
            // registration order before returning.
            let runtime_for_shutdown = Arc::clone(&election_runtime);
            shutdown_registry.register(Box::new(move || {
                Box::pin(async move {
                    runtime_for_shutdown.shutdown().await;
                    tracing::info!("election runtime: stopped");
                })
            }));

            // Per-device multi-room role substrate. Operator-
            // declared role (Source / Receiver / Auto); the
            // substrate is the source of truth for the
            // reactive-only multi-room plugin's role-transition
            // state machine. Substrate-empty reads return
            // `Auto`, so a fresh-boot device admits with the
            // output substrate free until the operator
            // engages multi-room.
            let role_store = Arc::new(evo_multiroom::RoleStore::new(
                Arc::clone(&persistence),
                Arc::clone(&bus),
            ));
            shared_role_store.set(Arc::clone(&role_store)
                as Arc<dyn evo_primitives::RoleStoreHandle>);
            tracing::info!("role store: ready");

            // Multi-room substrate adapter. Implements the
            // SDK's `MultiroomSubstrateHandle` trait over
            // `GroupStore` + `RoleStore`. Reactive-only
            // multi-room plugins consume this through
            // `LoadContext.multiroom_substrate`.
            let multiroom_substrate =
                evo_multiroom::MultiroomSubstrateAdapter::new(
                    Arc::clone(&group_store),
                    Arc::clone(&role_store),
                );
            multiroom_substrate_slot.set(multiroom_substrate);
            tracing::info!("multi-room substrate adapter: ready");

            Ok(())
        })
    })
}

/// Build the audio distribution's in-process admission setup.
fn audio_distribution_admission() -> AdmissionSetup {
    Box::new(|engine: &mut AdmissionEngine, config: &StewardConfig| {
        Box::pin(async move {
            // 0. Tier 2 reference-device UI substrate: register
            //    the six audio shelves (playback transport / queue
            //    / metering / browse / search / signal path) and
            //    the six widget kinds the renderer paints onto
            //    them. Runs BEFORE plugin admissions so the
            //    admission gate validates `[[ui.stocks]]`
            //    declarations against the combined Tier 1 + Tier
            //    2 set. The framework's
            //    `describe_ui_stockings` wire op then projects
            //    all 15 shelves + all 29 widget kinds in one
            //    round trip; the schema-first UI consumes the
            //    response directly.
            let audio_shelves =
                evo_device_audio_shared::audio_ui_pack::audio_shelves();
            engine
                .register_ui_shelves(&audio_shelves)
                .await
                .context("registering Tier 2 audio shelves")?;
            let audio_widget_kinds =
                evo_device_audio_shared::audio_ui_pack::audio_widget_kinds();
            engine
                .register_ui_widget_kinds(&audio_widget_kinds)
                .await
                .context("registering Tier 2 audio widget kinds")?;

            // 1. composition.alsa: singleton respondent on
            //    audio.composition shape 2.
            let composition_manifest = Manifest::from_toml(
                org_evoframework_composition_alsa::MANIFEST_TOML,
            )
            .context("parsing composition.alsa manifest")?;
            engine
                .admit_singleton_respondent(
                    AlsaCompositionPlugin::new(),
                    composition_manifest,
                )
                .await
                .context("admitting composition.alsa")?;

            // 2. playback.options: singleton respondent on
            //    audio.options shape 1. Operator-facing
            //    audiophile-grade settings (resampling /
            //    mixer_type / DOP / output_device /
            //    volume_normalization). Persists state
            //    across restarts; emits
            //    Happening::PluginEvent on every change AND
            //    publishes the settings as a subject so
            //    downstream consumers can subscribe via the
            //    framework's SubjectStateSubscriber. Admit
            //    BEFORE delivery.alsa + playback.mpd so the
            //    settings subject already exists when those
            //    plugins' load-time resolve runs (delivery
            //    observes the subject per audit Finding F5;
            //    playback.mpd consumes mixer_type from it).
            let options_manifest = Manifest::from_toml(
                org_evoframework_playback_options::MANIFEST_TOML,
            )
            .context("parsing playback.options manifest")?;
            engine
                .admit_singleton_respondent(
                    PlaybackOptionsPlugin::new(),
                    options_manifest,
                )
                .await
                .context("admitting playback.options")?;

            // 3. delivery.alsa: singleton respondent on
            //    audio.delivery shape 2. Owns the modular ALSA
            //    pipeline (pcm.evo definition in
            //    /etc/asound.conf); declares the WriteEndpoint
            //    upstream plugins write into; exposes
            //    operator-facing hardware probing verbs
            //    consumed by the playback.options plugin. At
            //    load time, subscribes to the options subject
            //    (now announced by step 2 above) and emits
            //    `delivery.options_observed` happenings on
            //    every settings change.
            let delivery_manifest = Manifest::from_toml(
                org_evoframework_delivery_alsa::MANIFEST_TOML,
            )
            .context("parsing delivery.alsa manifest")?;
            engine
                .admit_singleton_respondent(
                    AlsaDeliveryPlugin::new(),
                    delivery_manifest,
                )
                .await
                .context("admitting delivery.alsa")?;

            // 4. playback.mpd: warden + respondent on
            //    audio.playback shape 1. Owns the `mpd-path`
            //    URI scheme; the framework's source-verb
            //    dispatcher routes play_now / etc. to its
            //    respondent surface, while the steward's
            //    custody-aware dispatcher routes
            //    course_correct verbs (play / pause / seek /
            //    set_volume / etc.) to the warden surface.
            //    Admit AFTER playback.options so the
            //    audio.options.settings subject already
            //    exists when playback.mpd subscribes.
            let playback_manifest = Manifest::from_toml(
                org_evoframework_playback_mpd::MANIFEST_TOML,
            )
            .context("parsing playback.mpd manifest")?;
            engine
                .admit_singleton_warden_with_respondent(
                    MpdPlaybackPlugin::new(),
                    playback_manifest,
                )
                .await
                .context("admitting playback.mpd")?;

            // 5. network: singleton respondent on
            //    network moved to out-of-process shipping
            //    form. The plugin is no longer admitted via
            //    Phase 1 compile-link; the
            //    `dist/scripts/deploy-distribution.sh` flow
            //    cross-builds its wire binary
            //    (`network-wire`), stages + signs a bundle
            //    from
            //    `plugins/org.evoframework.network/manifest.oop.toml`,
            //    and installs it at
            //    `/opt/evo/plugins/org.evoframework.network/`.
            //    Phase 2 discovery (below) admits it from
            //    the filesystem search root, the framework's
            //    PPAG runner probes the `nmcli` / `iw` /
            //    `rfkill` NOPASSWD drop-ins (provisioned by
            //    `bootstrap.sh` Steps 1 + 1b), stamps the
            //    dispatcher resolution on
            //    `LoadContext::capabilities`, and the
            //    plugin's subprocess reads the resolution to
            //    pick its invocation strategy.

            // 6. multiroom.evo-native: singleton respondent on
            //    audio.multiroom shape 1. Bridges the local
            //    audio chain to the framework's audio-plane
            //    TCP transport. Role flips dynamically per
            //    source-host election: source-host nodes
            //    capture from the local audio chain + fan
            //    frames out; receivers subscribe to incoming
            //    frames + render to local ALSA.
            let multiroom_manifest = Manifest::from_toml(
                org_evoframework_multiroom_evo_native::MANIFEST_TOML,
            )
            .context("parsing multiroom.evo-native manifest")?;
            engine
                .admit_singleton_respondent(
                    MultiroomEvoNativePlugin::new(),
                    multiroom_manifest,
                )
                .await
                .context("admitting multiroom.evo-native")?;

            // 7. metadata.local moved to out-of-process shipping
            //    form. The plugin is no longer admitted via
            //    Phase 1 compile-link; the
            //    `dist/scripts/deploy-distribution.sh` flow
            //    cross-builds its wire binary
            //    (`metadata-local-wire`), stages + signs a
            //    bundle from
            //    `plugins/org.evoframework.metadata.local/manifest.oop.toml`,
            //    and installs it at
            //    `/opt/evo/plugins/org.evoframework.metadata.local/`.
            //    Phase 2 discovery (below) admits it from the
            //    filesystem search root.

            // 8. artwork.local moved to out-of-process shipping
            //    form. The plugin is no longer admitted via
            //    Phase 1 compile-link; the
            //    `dist/scripts/deploy-distribution.sh` flow
            //    cross-builds its wire binary
            //    (`artwork-local-wire`), stages a bundle from
            //    `plugins/org.evoframework.artwork.local/manifest.oop.toml`,
            //    and installs it at
            //    `/opt/evo/plugins/org.evoframework.artwork.local/`.
            //    Phase 2 discovery (below) admits it from the
            //    filesystem search root, and the operator's
            //    install / remove / update / enable / disable
            //    lifecycle reaches the plugin without a steward
            //    restart.

            // Phase 2 — filesystem discovery for operator-
            // installed out-of-process plugins. Walks
            // config.plugins.search_roots (default
            // /opt/evo/plugins then /var/lib/evo/plugins) and
            // admits each bundle whose manifest declares
            // transport.kind = OutOfProcess. Honours the
            // operator-disabled persistence record from the
            // framework's plugins.installed registrar.
            // In-process bundles found under search_roots are
            // skipped with a warning — the compile-link
            // baseline above is the only in-process source.
            // Per-plugin admission failure inside discovery is
            // already non-fatal at framework level (the failing
            // bundle is skipped with a structured happening and
            // boot continues), so the distribution baseline
            // stays available even if an operator-installed
            // plugin is broken.
            evo::plugin_discovery::discover_and_admit(engine, config)
                .await
                .context("operator-installed plugin discovery")?;

            Ok(())
        })
    })
}

/// Build the audio distribution's post-admission hook.
///
/// Invoked by `evo::run` after every plugin has admitted. The
/// hook publishes a default `ActiveAudioTopology` against the
/// framework's audio_topology_store so the reconciliation
/// cycle (route-change reactor in playback.mpd +
/// fragment-writer worker) fires from boot. Without this, the
/// audio_routing handles each plugin receives return
/// `EndpointNotConfigured` until an operator manually
/// publishes a topology via the wire op — the reference audio
/// chain's dynamic configuration stays inert.
///
/// The default topology is intentionally minimal:
///
/// - Source: `org.evoframework.playback.mpd` writing
///   PCM/s16le/44.1k/2ch to `pcm.evo` (the pipeline entry the
///   delivery plugin declares).
/// - Delivery: `org.evoframework.delivery.alsa` reading the
///   same format from the same endpoint.
/// - No composition stage (passthrough; mirrors the F3 static
///   fixture's bit-for-bit shape).
/// - Volume mode: `Software` (matches playback.options'
///   default).
/// - Score: zeroed `ScoreBreakdown` (the operator-driven
///   topology-scoring engine fills this in once the
///   reconciliation engine lands; for boot it's diagnostic-
///   only).
///
/// Subsequent operator changes via playback.options' setters
/// publish a new `audio.options.changed` happening;
/// delivery.alsa consumes the happening once the consumer
/// path is wired, re-derives the topology from the operator's
/// settings, and re-publishes via the same store.
fn audio_distribution_post_admission() -> evo::PostAdmissionSetup {
    Box::new(|ctx: evo::PostAdmissionContext| {
        Box::pin(async move {
            use evo::audio_topology::{ActiveAudioTopology, ActiveChainStage};
            use evo::topology_scoring::{ScoreBreakdown, VolumeMode};
            use evo_plugin_sdk::audio::{AudioFormat, PcmCodec};
            use evo_plugin_sdk::contract::audio_routing::EndpointKind;
            use std::path::PathBuf;

            // Publish a stable, role-agnostic default topology
            // pointing at the canonical PCM device `evo`. The
            // operator's role choice manifests in `/etc/asound.conf`
            // at bootstrap time, NOT in this binary: bootstrap
            // wires `pcm.evo` to either the snd-aloop playback
            // half (source role) or the hardware DAC
            // (receiver/auto role). The topology here names the
            // device by alias; ALSA resolves the alias per the
            // operator's role-specific asound.conf.
            //
            // This binary intentionally does NOT read the
            // multi-room plugin's TOML to decide endpoint
            // wiring. Per docs/engineering/BOUNDARY.md, the
            // framework binary does not own named-subsystem
            // semantics (role / source / receiver / snd-aloop /
            // hardware DAC); the multi-room plugin owns its own
            // role state and engages role-specific tasks
            // through its own substrate subscriptions. The
            // route-change reactor in playback.mpd consumes
            // this topology and rewrites mpd.conf to point at
            // `pcm.evo`; the asound.conf the operator installed
            // determines where `pcm.evo` ultimately resolves.
            let format = AudioFormat::Pcm {
                codec: PcmCodec::PcmS16Le,
                rate_hz: 44_100,
                channels: 2,
            };
            let source_stage = ActiveChainStage::Source {
                plugin: "org.evoframework.playback.mpd".to_string(),
                format: format.clone(),
                endpoint_kind: EndpointKind::AlsaPcm,
                endpoint_path: PathBuf::from("evo"),
            };
            let delivery_stage = ActiveChainStage::Delivery {
                plugin: "org.evoframework.delivery.alsa".to_string(),
                format: format.clone(),
                endpoint_kind: EndpointKind::AlsaPcm,
                endpoint_path: PathBuf::from("evo"),
            };
            let topology = ActiveAudioTopology {
                target_key: "evo-device-audio:default".to_string(),
                display_name: "Default delivery chain (44.1kHz/16-bit/stereo)"
                    .to_string(),
                chain: vec![source_stage, delivery_stage],
                volume_mode: VolumeMode::Software,
                volume_position: Some(0.5),
                volume_db: None,
                bit_perfect: false,
                score: ScoreBreakdown::default(),
                implicit_conversions: Vec::new(),
                warnings: Vec::new(),
            };
            tracing::info!(
                target_key = %topology.target_key,
                source = "org.evoframework.playback.mpd",
                delivery = "org.evoframework.delivery.alsa",
                "post-admission: publishing role-agnostic default audio topology"
            );
            ctx.audio
                .topology_store
                .publish(topology, "evo-device-audio:post-admission")
                .await
                .context("publishing default audio topology")?;
            Ok(())
        })
    })
}
