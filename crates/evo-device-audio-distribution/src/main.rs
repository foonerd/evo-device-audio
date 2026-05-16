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
use org_evoframework_multiroom_evo_native::MultiroomEvoNativePlugin;
use org_evoframework_network::NetworkPlugin;
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

            // 3. playback.options: singleton respondent on
            //    audio.options shape 1. Operator-facing
            //    audiophile-grade settings (resampling /
            //    mixer_type / DOP / output_device /
            //    volume_normalization). Persists state
            //    across restarts; emits
            //    Happening::PluginEvent on every change AND
            //    publishes the settings as a subject so
            //    downstream consumers (playback.mpd's
            //    mixer-mode reactor, future UI plugins) can
            //    subscribe via the framework's
            //    SubjectStateSubscriber. Admit BEFORE
            //    playback.mpd so the subject already exists
            //    when playback.mpd's load-time resolve runs.
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
            //    networking.link shape 1. Multi-source
            //    networking surface (status / scan / intent /
            //    captive-portal / security / flight-mode);
            //    fans in from rtnetlink + NetworkManager
            //    D-Bus + universal polling floor under a
            //    per-platform source preset. Holds an
            //    [`NmcliDispatcher`] composite that the
            //    framework's PPAG runner stamps from the
            //    `nmcli_invocation` resolution at admission
            //    time, so under non-root service identities
            //    every nmcli invocation goes through
            //    `sudo -n nmcli` against the distribution's
            //    NOPASSWD drop-in without re-probing on each
            //    call.
            let network_nm_manifest =
                Manifest::from_toml(org_evoframework_network::MANIFEST_TOML)
                    .context("parsing network manifest")?;
            engine
                .admit_singleton_respondent(
                    NetworkPlugin::new(),
                    network_nm_manifest,
                )
                .await
                .context("admitting network")?;

            // 6. multiroom.evo-native: singleton respondent on
            //    audio.multiroom shape 1. Bridges the local
            //    audio chain to the framework's audio-plane
            //    TCP transport. Role flips dynamically per
            //    source-host election: source-host nodes
            //    capture from the local audio chain + fan
            //    frames out; receivers subscribe to incoming
            //    frames + render to local ALSA. The initial
            //    plugin shape lights up the receiver-observation
            //    + fan-out-substrate layer; the audio-chain
            //    capture + render bridges land as substrate
            //    iterations on the same plugin.
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
/// subsequent chunk wires delivery.alsa to consume the
/// happening, re-derive the topology from the operator's
/// settings, and re-publish via the same store.
/// Read the operator-supplied multi-room plugin role from
/// `/etc/evo/plugins.d/org.evoframework.multiroom.evo-native.toml`.
/// Returns `Some("source" | "receiver" | "auto")` when the
/// config file is present and contains a recognised `role`
/// key; `None` otherwise.
///
/// Used by the post-admission topology hook to publish a
/// role-aware default — the multi-room plugin's TOML is the
/// single source of truth for the device's role; the audio
/// topology mirrors it so MPD's audio_output device path is
/// chosen automatically.
fn read_multiroom_role() -> Option<String> {
    const PATH: &str =
        "/etc/evo/plugins.d/org.evoframework.multiroom.evo-native.toml";
    let raw = std::fs::read_to_string(PATH).ok()?;
    let parsed: toml::Value = toml::from_str(&raw).ok()?;
    parsed
        .get("role")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn audio_distribution_post_admission() -> evo::PostAdmissionSetup {
    Box::new(|ctx: evo::PostAdmissionContext| {
        Box::pin(async move {
            use evo::audio_topology::{ActiveAudioTopology, ActiveChainStage};
            use evo::topology_scoring::{ScoreBreakdown, VolumeMode};
            use evo_plugin_sdk::audio::{AudioFormat, PcmCodec};
            use evo_plugin_sdk::contract::audio_routing::EndpointKind;
            use std::path::PathBuf;

            // Always publish on post-admission, derived from
            // the current multi-room plugin TOML. The TOML is
            // the operator's single source of truth for the
            // device's role; if the role flipped between
            // boots (e.g. source -> receiver), the persisted
            // topology from the previous boot would otherwise
            // pin the wrong MPD audio_output device. The
            // store's publish path is idempotent on identical
            // shape — re-publish of the same topology emits
            // one route-change happening, which is acceptable
            // boot-time noise. The route-change reactor in
            // playback.mpd rewrites /etc/evo/mpd.conf
            // accordingly so the operator's intent in TOML is
            // realised on every boot.

            // Read the multi-room plugin's operator config to
            // decide whether this device is a source-host or a
            // local-playback / receiver node. The multi-room
            // plugin's TOML is the single source of truth for
            // the device's role; the audio topology mirrors it
            // so MPD's audio_output device path is correct
            // automatically — no operator wire-op call required.
            //
            // Source-host: MPD writes into the snd-aloop playback
            // half (`hw:Loopback,0,0`); the multi-room plugin's
            // source role captures from the snd-aloop capture
            // half (`hw:Loopback,1,0`) and broadcasts to
            // receivers. Local-playback: MPD writes directly to
            // `pcm.evo` → hardware DAC.
            let multiroom_role = read_multiroom_role();
            let source_endpoint_path = match multiroom_role.as_deref() {
                Some("source") => PathBuf::from("hw:Loopback,0,0"),
                _ => PathBuf::from("evo"),
            };
            let format = AudioFormat::Pcm {
                codec: PcmCodec::PcmS16Le,
                rate_hz: 44_100,
                channels: 2,
            };
            let source_stage = ActiveChainStage::Source {
                plugin: "org.evoframework.playback.mpd".to_string(),
                format: format.clone(),
                endpoint_kind: EndpointKind::AlsaPcm,
                endpoint_path: source_endpoint_path.clone(),
            };
            let delivery_stage = ActiveChainStage::Delivery {
                plugin: "org.evoframework.delivery.alsa".to_string(),
                format: format.clone(),
                endpoint_kind: EndpointKind::AlsaPcm,
                endpoint_path: PathBuf::from("evo"),
            };
            let display_suffix = match multiroom_role.as_deref() {
                Some("source") => " — multi-room source-host (MPD → snd-aloop)",
                _ => " — local playback / receiver",
            };
            let topology = ActiveAudioTopology {
                target_key: "evo-device-audio:default".to_string(),
                display_name: format!(
                    "Default delivery chain (44.1kHz/16-bit/stereo){}",
                    display_suffix,
                ),
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
                multiroom_role = ?multiroom_role,
                source_endpoint = %source_endpoint_path.display(),
                "post-admission: topology shape selected per multi-room role"
            );

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
