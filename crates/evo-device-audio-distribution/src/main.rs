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
//! baseline ships two reshaped plugins:
//!
//! - `org.evoframework.composition.alsa` — substrate-aware
//!   composition stage; admits as a singleton respondent on
//!   the `audio.composition` shelf at shape 2.
//! - `org.evoframework.playback.mpd` — warden + source +
//!   respondent for MPD-backed playback; admits via the
//!   framework's
//!   [`AdmissionEngine::admit_singleton_warden_with_respondent`]
//!   path so both course_correct and source-verb dispatches
//!   route to the same plugin instance.
//!
//! The remaining plugins (metadata.local / artwork.local /
//! network.nm) join the admission setup as their reshape
//! lands.
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
use org_evoframework_playback_mpd::MpdPlaybackPlugin;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = evo::cli::Args::parse();
    evo::run(evo::RunOptions::new(args, audio_distribution_admission())).await
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
            //    audio.delivery shape 2. Owns the AAMPP
            //    modular ALSA pipeline (pcm.evo definition in
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

            Ok(())
        })
    })
}
