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
//! v0.1.13 baseline ships:
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
//! - `org.evoframework.network.nm` — NetworkManager-backed
//!   networking surface; consumes the framework's PPAG
//!   resolution for the `nmcli_invocation` intent to install
//!   an EUID-aware dispatch composite (direct under root,
//!   `sudo -n` under a non-root service user).
//!
//! The remaining plugins (metadata.local / artwork.local)
//! join the admission setup as their reshape lands.
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

use anyhow::Context;
use evo::admission::AdmissionEngine;
use evo::config::StewardConfig;
use evo::AdmissionSetup;
use evo_plugin_sdk::Manifest;
use org_evoframework_composition_alsa::AlsaCompositionPlugin;
use org_evoframework_delivery_alsa::AlsaDeliveryPlugin;
use org_evoframework_network_nm::NetworkNmPlugin;
use org_evoframework_playback_mpd::MpdPlaybackPlugin;
use org_evoframework_playback_options::PlaybackOptionsPlugin;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = evo::cli::Args::parse();
    let opts = evo::RunOptions::new(args, audio_distribution_admission())
        .with_post_admission(audio_distribution_post_admission());
    evo::run(opts).await
}

/// Build the audio distribution's in-process admission setup.
fn audio_distribution_admission() -> AdmissionSetup {
    Box::new(|engine: &mut AdmissionEngine, _config: &StewardConfig| {
        Box::pin(async move {
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

            // 2. delivery.alsa: singleton respondent on
            //    audio.delivery shape 2. Owns the modular ALSA
            //    pipeline (pcm.evo definition in
            //    /etc/asound.conf); declares the WriteEndpoint
            //    upstream plugins write into; exposes
            //    operator-facing hardware probing verbs
            //    consumed by the playback.options plugin.
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

            // 3. playback.mpd: warden + respondent on
            //    audio.playback shape 1. Owns the `mpd-path`
            //    URI scheme; the framework's source-verb
            //    dispatcher routes play_now / etc. to its
            //    respondent surface, while the steward's
            //    custody-aware dispatcher routes
            //    course_correct verbs (play / pause / seek /
            //    set_volume / etc.) to the warden surface.
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

            // 4. playback.options: singleton respondent on
            //    audio.options shape 1. Operator-facing
            //    audiophile-grade settings (resampling /
            //    mixer_type / DOP / output_device /
            //    volume_normalization). Persists state
            //    across restarts; emits
            //    Happening::PluginEvent on every change so
            //    delivery.alsa can re-render the modular ALSA
            //    pipeline. Admit AFTER delivery.alsa so the
            //    cross-plugin reaction chain is in place
            //    when the first settings-changed happening
            //    fires.
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

            // 5. network.nm: singleton respondent on
            //    networking.link shape 1. NetworkManager-
            //    backed networking surface (status / scan /
            //    intent / captive-portal / security /
            //    flight-mode). Holds an
            //    [`NmcliDispatcher`] composite that the
            //    framework's PPAG runner stamps from the
            //    `nmcli_invocation` resolution at admission
            //    time, so under non-root service identities
            //    every nmcli invocation goes through
            //    `sudo -n nmcli` against the distribution's
            //    NOPASSWD drop-in without re-probing on each
            //    call.
            let network_nm_manifest =
                Manifest::from_toml(org_evoframework_network_nm::MANIFEST_TOML)
                    .context("parsing network.nm manifest")?;
            engine
                .admit_singleton_respondent(
                    NetworkNmPlugin::new(),
                    network_nm_manifest,
                )
                .await
                .context("admitting network.nm")?;

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
/// publish a new `audio.options.changed` happening; a
/// follow-on chunk wires delivery.alsa to consume the
/// happening, re-derive the topology from the operator's
/// settings, and re-publish via the same store.
fn audio_distribution_post_admission() -> evo::PostAdmissionSetup {
    Box::new(|ctx: evo::PostAdmissionContext| {
        Box::pin(async move {
            use evo::audio_topology::{ActiveAudioTopology, ActiveChainStage};
            use evo::topology_scoring::{ScoreBreakdown, VolumeMode};
            use evo_plugin_sdk::audio::{AudioFormat, PcmCodec};
            use evo_plugin_sdk::contract::audio_routing::EndpointKind;
            use std::path::PathBuf;

            // Skip if the store already has a topology persisted
            // (the framework's rehydrate-from-substrate path
            // already propagated it through the routing
            // runtime). Re-publishing on top of that would emit
            // a redundant route-change.
            let existing = match ctx.audio.topology_store.list().await {
                Ok(rows) => rows.len(),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "post-admission: audio_topology_store.list failed; \
                         attempting default-publish anyway"
                    );
                    0
                }
            };
            if existing > 0 {
                tracing::info!(
                    topologies = existing,
                    "post-admission: persisted audio topology present; \
                     skipping default publish"
                );
                return Ok(());
            }

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
                "post-admission: publishing default audio topology"
            );
            ctx.audio
                .topology_store
                .publish(topology, "evo-device-audio:post-admission")
                .await
                .context("publishing default audio topology")?;
            tracing::info!(
                "post-admission: default audio topology published; \
                 route-change reactor cycle is now live"
            );

            Ok(())
        })
    })
}
