// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Topology scoring + bit-perfect validation.
//!
//! The reconciliation engine evaluates candidate topologies
//! against the consolidated hardware profile and operator
//! policy and picks the highest-scoring one. The scorer is a
//! pure function over typed inputs; same inputs always produce
//! the same score, so the operator UI can replay the score
//! breakdown to answer "why did the engine pick X over Y".
//!
//! Scoring weights (per the architectural lock):
//!
//! - **Bit-perfectness**: +50 when every chain stage preserves
//!   bit-perfect.
//! - **Native-rate match**: +20 when the delivery stage's rate
//!   equals the source's rate (no resampling).
//! - **Native-format match**: +15 when the delivery stage's
//!   format equals the source's format (no codec conversion).
//! - **Minimum signal path**: +10 when the composition mode is
//!   passthrough (no intermediate processing stage).
//! - **Hardware volume engaged**: +5 when hardware volume is
//!   available and the chain engages it.
//!
//! Scoring penalties:
//!
//! - **Implicit resampler**: −30 when the chain inserts a
//!   resampler the source / delivery format pair did not
//!   request.
//! - **Software volume when hardware available**: −10 when the
//!   chain falls back to software volume despite the hardware
//!   profile reporting `HardwareVolumeCapability != None`.
//! - **DSD-to-PCM conversion**: −25 when the chain converts a
//!   DSD source to PCM (a destructive transformation that
//!   audiophile chains explicitly avoid when the delivery
//!   target supports DSD natively).
//!
//! The scorer never refuses a candidate — refusal lives in the
//! [`OperatorPolicy::StrictBitPerfect`] post-filter and the
//! [`validate_bit_perfect`] separate function. A
//! [`ScoreBreakdown`] with a low or negative `total` is the
//! engine's signal that the candidate is the best of a bad set;
//! the operator UI surfaces the breakdown so the operator sees
//! why.

use serde::{Deserialize, Serialize};

use crate::hardware_profile::HardwareProfile;
use evo::server::HardwareVolumeCapability;
use evo::server::{OperatorPolicy, ScoreBreakdown, VolumeMode};
use evo_plugin_sdk::audio::AudioFormat;

/// One stage of the audio chain — source / composition /
/// delivery — with the format the stage produces / accepts and
/// the bit-perfect contract it carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositionStage {
    /// Mode name as declared in the composition plugin's
    /// manifest (`"passthrough"` / `"eq_only"` / `"resampled"`
    /// / `"dsd_to_pcm"` / etc.). Treated opaquely except for
    /// `"passthrough"` which the scorer treats as the
    /// minimum-signal-path baseline.
    pub mode: String,
    /// `true` when this stage's mode preserves bit-perfect
    /// (the manifest's `preserves_bit_perfect` flag for the
    /// selected mode).
    pub preserves_bit_perfect: bool,
    /// Format on the input side of this stage.
    pub format_in: AudioFormat,
    /// Format on the output side of this stage. Equal to
    /// `format_in` for passthrough; differs for transforming
    /// modes.
    pub format_out: AudioFormat,
}

impl CompositionStage {
    /// Returns `true` when this stage transforms the audio
    /// (the input and output formats differ).
    pub fn transforms(&self) -> bool {
        self.format_in != self.format_out
    }

    /// Returns `true` when this stage applies an implicit
    /// resampler — the input rate differs from the output
    /// rate but the codec / channels are otherwise the same
    /// (a pure rate conversion that the source did not
    /// declare).
    pub fn applies_implicit_resampler(&self) -> bool {
        match (&self.format_in, &self.format_out) {
            (
                AudioFormat::Pcm {
                    codec: ic,
                    rate_hz: ir,
                    channels: ich,
                },
                AudioFormat::Pcm {
                    codec: oc,
                    rate_hz: or_,
                    channels: och,
                },
            ) => ic == oc && ich == och && ir != or_,
            _ => false,
        }
    }

    /// Returns `true` when this stage converts DSD source
    /// material to PCM output (a destructive transformation
    /// that audiophile chains avoid when the delivery target
    /// supports DSD natively).
    pub fn converts_dsd_to_pcm(&self) -> bool {
        matches!(
            (&self.format_in, &self.format_out),
            (AudioFormat::Dsd { .. }, AudioFormat::Pcm { .. })
        )
    }
}

/// Candidate topology — one source → composition → delivery
/// chain shape. The reconciliation engine builds candidates by
/// walking format declarations across stages; the scorer ranks
/// them; the operator policy filters refusals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Topology {
    /// Source plugin's produced format.
    pub source_format: AudioFormat,
    /// Composition stage. `None` means the chain has no
    /// intermediate composition (source → delivery direct).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composition: Option<CompositionStage>,
    /// Delivery target's accepted format.
    pub delivery_format: AudioFormat,
    /// Volume mode for the chain.
    pub volume_mode: VolumeMode,
}

impl Topology {
    /// Returns `true` when every stage of the chain preserves
    /// bit-perfect.
    ///
    /// - Source format equals delivery format (no implicit
    ///   conversion at the chain boundaries).
    /// - Composition stage (if any) declares
    ///   `preserves_bit_perfect = true` AND does not transform
    ///   the format.
    /// - Volume mode is `Hardware` or `None` (software volume
    ///   at <100% truncates).
    ///
    /// Software volume at 100% IS bit-perfect, but the chain
    /// shape itself (engaging the software gain stage) cannot
    /// be statically verified to stay at 100%; the
    /// architectural choice is to treat `VolumeMode::Software`
    /// as not-bit-perfect by topology shape regardless of
    /// runtime level.
    pub fn is_bit_perfect(&self) -> bool {
        if self.source_format != self.delivery_format {
            return false;
        }
        if let Some(stage) = &self.composition {
            if !stage.preserves_bit_perfect || stage.transforms() {
                return false;
            }
        }
        !matches!(self.volume_mode, VolumeMode::Software)
    }

    /// Returns `true` when the chain inserts an implicit
    /// resampler (any composition stage with mismatched
    /// rate that the source did not declare).
    pub fn has_implicit_resampler(&self) -> bool {
        self.composition
            .as_ref()
            .is_some_and(|s| s.applies_implicit_resampler())
    }

    /// Returns `true` when the chain converts DSD to PCM at
    /// any composition stage.
    pub fn has_dsd_to_pcm_conversion(&self) -> bool {
        self.composition
            .as_ref()
            .is_some_and(|s| s.converts_dsd_to_pcm())
    }

    /// Returns `true` when the source rate equals the delivery
    /// rate.
    pub fn delivery_matches_source_rate(&self) -> bool {
        match (&self.source_format, &self.delivery_format) {
            (
                AudioFormat::Pcm { rate_hz: sr, .. },
                AudioFormat::Pcm { rate_hz: dr, .. },
            ) => sr == dr,
            (
                AudioFormat::Dsd { rate: sr, .. },
                AudioFormat::Dsd { rate: dr, .. },
            ) => sr == dr,
            (
                AudioFormat::EncodedPassthrough { rate_hz: sr, .. },
                AudioFormat::EncodedPassthrough { rate_hz: dr, .. },
            ) => sr == dr,
            _ => false,
        }
    }

    /// Returns `true` when the source format kind / codec
    /// match the delivery format kind / codec (rate may
    /// differ).
    pub fn delivery_matches_source_format(&self) -> bool {
        matches!(
            (&self.source_format, &self.delivery_format),
            (
                AudioFormat::Pcm { codec: sc, channels: sch, .. },
                AudioFormat::Pcm { codec: dc, channels: dch, .. },
            ) if sc == dc && sch == dch
        ) || matches!(
            (&self.source_format, &self.delivery_format),
            (
                AudioFormat::Dsd { transport: st, channels: sch, .. },
                AudioFormat::Dsd { transport: dt, channels: dch, .. },
            ) if st == dt && sch == dch
        ) || matches!(
            (&self.source_format, &self.delivery_format),
            (
                AudioFormat::EncodedPassthrough { codec: sc, channels: sch, .. },
                AudioFormat::EncodedPassthrough { codec: dc, channels: dch, .. },
            ) if sc == dc && sch == dch
        )
    }

    /// Returns `true` when the composition is passthrough
    /// (preserves bit-perfect AND does not transform).
    pub fn is_passthrough_composition(&self) -> bool {
        self.composition
            .as_ref()
            .is_none_or(|s| s.mode == "passthrough" && !s.transforms())
    }
}

/// Score one candidate topology against the consolidated
/// hardware profile and operator policy. Pure function;
/// deterministic.
pub fn score_topology(
    candidate: &Topology,
    profile: &HardwareProfile,
    _operator_policy: &OperatorPolicy,
) -> ScoreBreakdown {
    let bit_perfect = if candidate.is_bit_perfect() { 50 } else { 0 };
    let native_rate_match = if candidate.delivery_matches_source_rate() {
        20
    } else {
        0
    };
    let native_format_match = if candidate.delivery_matches_source_format() {
        15
    } else {
        0
    };
    let minimum_signal_path = if candidate.is_passthrough_composition() {
        10
    } else {
        0
    };
    let hardware_volume_available =
        !matches!(profile.hardware_volume, HardwareVolumeCapability::None);
    let hardware_volume_engaged = if hardware_volume_available
        && matches!(candidate.volume_mode, VolumeMode::Hardware)
    {
        5
    } else {
        0
    };
    let implicit_resampler_penalty = if candidate.has_implicit_resampler() {
        -30
    } else {
        0
    };
    let software_volume_when_hardware_available_penalty =
        if hardware_volume_available
            && matches!(candidate.volume_mode, VolumeMode::Software)
        {
            -10
        } else {
            0
        };
    let dsd_to_pcm_penalty = if candidate.has_dsd_to_pcm_conversion() {
        -25
    } else {
        0
    };

    let total = bit_perfect
        + native_rate_match
        + native_format_match
        + minimum_signal_path
        + hardware_volume_engaged
        + implicit_resampler_penalty
        + software_volume_when_hardware_available_penalty
        + dsd_to_pcm_penalty;

    ScoreBreakdown {
        total,
        bit_perfect,
        native_rate_match,
        native_format_match,
        minimum_signal_path,
        hardware_volume_engaged,
        implicit_resampler_penalty,
        software_volume_when_hardware_available_penalty,
        dsd_to_pcm_penalty,
    }
}

/// Bit-perfect violation reason. Returned by
/// [`validate_bit_perfect`] to explain WHY a chain that claims
/// bit-perfect actually breaks the contract.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BitPerfectViolation {
    /// Source format and delivery format differ AND no
    /// composition stage in the chain transforms — the
    /// implicit conversion at the chain boundary breaks
    /// bit-perfect.
    #[error(
        "source format and delivery format differ ({source_format:?} vs \
         {delivery_format:?}) but no composition stage in the chain \
         transforms — the implicit conversion at the chain \
         boundary would break bit-perfect"
    )]
    BoundaryConversionRequired {
        /// The source format.
        source_format: AudioFormat,
        /// The delivery format.
        delivery_format: AudioFormat,
    },
    /// Composition stage transforms the format but its mode
    /// declares `preserves_bit_perfect = true` — the manifest
    /// claim is incoherent with the runtime behaviour.
    #[error(
        "composition stage {mode:?} transforms the format \
         ({format_in:?} → {format_out:?}) but the manifest \
         declares preserves_bit_perfect = true"
    )]
    CompositionLiesAboutBitPerfect {
        /// Mode name from the composition stage.
        mode: String,
        /// Format on the input side.
        format_in: AudioFormat,
        /// Format on the output side.
        format_out: AudioFormat,
    },
    /// Volume mode is software — the gain stage at <100%
    /// introduces dither / truncation.
    #[error(
        "volume_mode = software introduces dither / truncation at \
         <100% — the chain shape itself cannot be statically \
         verified to preserve bit-perfect regardless of runtime \
         level"
    )]
    SoftwareVolumeNotBitPerfect,
}

/// Validate that the supplied topology is genuinely bit-perfect.
/// Returns `Ok(())` for chains that preserve bit-perfect;
/// returns the first detected violation otherwise.
///
/// Used by:
///
/// - [`OperatorPolicy::StrictBitPerfect`] post-filter (refuse
///   any candidate that fails this check).
/// - The active-topology subject publisher (sub-primitive F)
///   to populate the topology subject's
///   `implicit_conversions` and `warnings` fields.
/// - Operator UI to render Roon-style signal-path explanations
///   when bit-perfect fails.
pub fn validate_bit_perfect(
    topology: &Topology,
) -> Result<(), BitPerfectViolation> {
    if topology.source_format != topology.delivery_format {
        match &topology.composition {
            None => {
                return Err(BitPerfectViolation::BoundaryConversionRequired {
                    source_format: topology.source_format.clone(),
                    delivery_format: topology.delivery_format.clone(),
                });
            }
            Some(stage) => {
                if stage.preserves_bit_perfect && stage.transforms() {
                    return Err(
                        BitPerfectViolation::CompositionLiesAboutBitPerfect {
                            mode: stage.mode.clone(),
                            format_in: stage.format_in.clone(),
                            format_out: stage.format_out.clone(),
                        },
                    );
                }
            }
        }
    }
    if matches!(topology.volume_mode, VolumeMode::Software) {
        return Err(BitPerfectViolation::SoftwareVolumeNotBitPerfect);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware_profile::{
        compose_profile, DatabaseHardwareData, DeclaredHardwareData,
        PcmCapabilities, ProbedHardwareData,
    };
    use evo::server::{
        HardwareIdentity, HardwareProfileOverride, HardwareTier,
        TopologyPreferences,
    };
    use evo_plugin_sdk::audio::PcmCodec;

    fn ident_usb() -> HardwareIdentity {
        HardwareIdentity {
            usb_vid_pid: Some((0x21b4, 0x0096)),
            alsa_card_name: "DragonFly Cobalt".into(),
            hat_eeprom_signature: None,
            hdmi_sink_id: None,
            display_name: "AudioQuest DragonFly Cobalt".into(),
        }
    }

    fn pcm(rate: u32) -> AudioFormat {
        AudioFormat::Pcm {
            codec: PcmCodec::PcmS24Le,
            rate_hz: rate,
            channels: 2,
        }
    }

    fn profile_audiophile_with_hw_volume() -> HardwareProfile {
        compose_profile(
            ident_usb(),
            ProbedHardwareData {
                pcm_native: PcmCapabilities {
                    rates_hz: vec![44100, 48000, 96000, 192000],
                    bit_depths: vec![16, 24],
                    channels: vec![2],
                },
                exclusive_mode: true,
                ..Default::default()
            },
            DeclaredHardwareData::default(),
            DatabaseHardwareData {
                tier: Some(HardwareTier::Audiophile),
                hardware_volume: Some(HardwareVolumeCapability::AnalogOnly),
                ..Default::default()
            },
            None,
        )
    }

    #[test]
    fn compose_layers_override_above_database_above_declared() {
        let identity = ident_usb();
        let probed = ProbedHardwareData::default();
        let declared = DeclaredHardwareData {
            tier: Some(HardwareTier::Reference),
            ..Default::default()
        };
        let database = DatabaseHardwareData {
            tier: Some(HardwareTier::Mainstream),
            ..Default::default()
        };
        let override_ = HardwareProfileOverride {
            tier: Some(HardwareTier::Audiophile),
            ..Default::default()
        };
        let p = compose_profile(
            identity.clone(),
            probed.clone(),
            declared.clone(),
            database.clone(),
            Some(override_.clone()),
        );
        assert_eq!(p.tier, HardwareTier::Audiophile);
        // Without override: database wins over declared.
        let p2 = compose_profile(identity, probed, declared, database, None);
        assert_eq!(p2.tier, HardwareTier::Mainstream);
    }

    #[test]
    fn compose_falls_back_to_mainstream_tier_when_no_layer_supplies_one() {
        let p = compose_profile(
            ident_usb(),
            ProbedHardwareData::default(),
            DeclaredHardwareData::default(),
            DatabaseHardwareData::default(),
            None,
        );
        assert_eq!(p.tier, HardwareTier::Mainstream);
    }

    #[test]
    fn compose_falls_back_to_no_hardware_volume_when_no_layer_supplies_one() {
        let p = compose_profile(
            ident_usb(),
            ProbedHardwareData::default(),
            DeclaredHardwareData::default(),
            DatabaseHardwareData::default(),
            None,
        );
        assert_eq!(p.hardware_volume, HardwareVolumeCapability::None);
    }

    #[test]
    fn compose_carries_pcm_native_from_probe_unconditionally() {
        // PCM capabilities are probed-truth; even with override
        // present, probed data passes through.
        let probed = ProbedHardwareData {
            pcm_native: PcmCapabilities {
                rates_hz: vec![44100, 192000, 384000],
                bit_depths: vec![16, 24, 32],
                channels: vec![2],
            },
            ..Default::default()
        };
        let p = compose_profile(
            ident_usb(),
            probed.clone(),
            DeclaredHardwareData::default(),
            DatabaseHardwareData::default(),
            Some(HardwareProfileOverride {
                tier: Some(HardwareTier::Audiophile),
                ..Default::default()
            }),
        );
        assert_eq!(p.pcm_native, probed.pcm_native);
    }

    #[test]
    fn compose_override_can_rename_display_name() {
        let p = compose_profile(
            ident_usb(),
            ProbedHardwareData::default(),
            DeclaredHardwareData::default(),
            DatabaseHardwareData::default(),
            Some(HardwareProfileOverride {
                display_name: Some("Listening Room DAC".into()),
                tier: Some(HardwareTier::Audiophile),
                ..Default::default()
            }),
        );
        assert_eq!(p.identity.display_name, "Listening Room DAC");
    }

    #[test]
    fn compose_database_topology_preferences_propagate() {
        let prefs = TopologyPreferences {
            prefer_native_rate: true,
            prefer_native_format: true,
            minimum_signal_path: true,
        };
        let p = compose_profile(
            ident_usb(),
            ProbedHardwareData::default(),
            DeclaredHardwareData::default(),
            DatabaseHardwareData {
                prefer: Some(prefs),
                ..Default::default()
            },
            None,
        );
        assert_eq!(p.prefer, prefs);
    }

    #[test]
    fn topology_passthrough_at_native_rate_scores_full() {
        // Source = delivery = 192/24, no composition, hardware
        // volume engaged on an audiophile DAC.
        let topo = Topology {
            source_format: pcm(192_000),
            composition: None,
            delivery_format: pcm(192_000),
            volume_mode: VolumeMode::Hardware,
        };
        let profile = profile_audiophile_with_hw_volume();
        let breakdown = score_topology(&topo, &profile, &OperatorPolicy::Auto);
        assert_eq!(breakdown.bit_perfect, 50);
        assert_eq!(breakdown.native_rate_match, 20);
        assert_eq!(breakdown.native_format_match, 15);
        assert_eq!(breakdown.minimum_signal_path, 10);
        assert_eq!(breakdown.hardware_volume_engaged, 5);
        assert_eq!(breakdown.implicit_resampler_penalty, 0);
        assert_eq!(
            breakdown.software_volume_when_hardware_available_penalty,
            0
        );
        assert_eq!(breakdown.dsd_to_pcm_penalty, 0);
        assert_eq!(breakdown.total, 100);
    }

    #[test]
    fn topology_with_implicit_resampler_takes_30_penalty() {
        // Source = 192/24, delivery = 48/24, composition
        // resamples (preserves_bit_perfect = false).
        let topo = Topology {
            source_format: pcm(192_000),
            composition: Some(CompositionStage {
                mode: "resampled".into(),
                preserves_bit_perfect: false,
                format_in: pcm(192_000),
                format_out: pcm(48_000),
            }),
            delivery_format: pcm(48_000),
            volume_mode: VolumeMode::Hardware,
        };
        let profile = profile_audiophile_with_hw_volume();
        let breakdown = score_topology(&topo, &profile, &OperatorPolicy::Auto);
        assert_eq!(breakdown.bit_perfect, 0);
        assert_eq!(breakdown.implicit_resampler_penalty, -30);
    }

    #[test]
    fn topology_with_software_volume_on_hw_volume_dac_takes_10_penalty() {
        let topo = Topology {
            source_format: pcm(192_000),
            composition: None,
            delivery_format: pcm(192_000),
            volume_mode: VolumeMode::Software,
        };
        let profile = profile_audiophile_with_hw_volume();
        let breakdown = score_topology(&topo, &profile, &OperatorPolicy::Auto);
        assert_eq!(
            breakdown.software_volume_when_hardware_available_penalty,
            -10
        );
        // Software volume is also not-bit-perfect by topology
        // shape regardless of runtime level.
        assert_eq!(breakdown.bit_perfect, 0);
    }

    #[test]
    fn topology_with_dsd_to_pcm_conversion_takes_25_penalty() {
        let topo = Topology {
            source_format: AudioFormat::Dsd {
                rate: evo_plugin_sdk::audio::DsdRate::Dsd64,
                transport: evo_plugin_sdk::audio::DsdTransport::NativeUsb,
                channels: 2,
            },
            composition: Some(CompositionStage {
                mode: "dsd_to_pcm".into(),
                preserves_bit_perfect: false,
                format_in: AudioFormat::Dsd {
                    rate: evo_plugin_sdk::audio::DsdRate::Dsd64,
                    transport: evo_plugin_sdk::audio::DsdTransport::NativeUsb,
                    channels: 2,
                },
                format_out: pcm(176_400),
            }),
            delivery_format: pcm(176_400),
            volume_mode: VolumeMode::Hardware,
        };
        let profile = profile_audiophile_with_hw_volume();
        let breakdown = score_topology(&topo, &profile, &OperatorPolicy::Auto);
        assert_eq!(breakdown.dsd_to_pcm_penalty, -25);
        assert_eq!(breakdown.bit_perfect, 0);
    }

    #[test]
    fn validate_bit_perfect_passes_for_passthrough_at_native_rate() {
        let topo = Topology {
            source_format: pcm(192_000),
            composition: None,
            delivery_format: pcm(192_000),
            volume_mode: VolumeMode::Hardware,
        };
        validate_bit_perfect(&topo).expect("passthrough is bit-perfect");
    }

    #[test]
    fn validate_bit_perfect_refuses_software_volume() {
        let topo = Topology {
            source_format: pcm(192_000),
            composition: None,
            delivery_format: pcm(192_000),
            volume_mode: VolumeMode::Software,
        };
        let err = validate_bit_perfect(&topo)
            .expect_err("software volume is not bit-perfect by topology shape");
        assert_eq!(err, BitPerfectViolation::SoftwareVolumeNotBitPerfect);
    }

    #[test]
    fn validate_bit_perfect_refuses_boundary_format_mismatch() {
        let topo = Topology {
            source_format: pcm(192_000),
            composition: None,
            delivery_format: pcm(48_000),
            volume_mode: VolumeMode::Hardware,
        };
        let err = validate_bit_perfect(&topo).expect_err(
            "boundary format mismatch with no composition is not bit-perfect",
        );
        assert!(matches!(
            err,
            BitPerfectViolation::BoundaryConversionRequired { .. }
        ));
    }

    #[test]
    fn validate_bit_perfect_refuses_lying_composition() {
        // Composition declares preserves_bit_perfect = true but
        // the input/output formats differ — the manifest claim
        // is incoherent with the runtime behaviour.
        let topo = Topology {
            source_format: pcm(192_000),
            composition: Some(CompositionStage {
                mode: "passthrough".into(),
                preserves_bit_perfect: true,
                format_in: pcm(192_000),
                format_out: pcm(48_000),
            }),
            delivery_format: pcm(48_000),
            volume_mode: VolumeMode::Hardware,
        };
        let err = validate_bit_perfect(&topo)
            .expect_err("composition cannot transform AND claim bit-perfect");
        assert!(matches!(
            err,
            BitPerfectViolation::CompositionLiesAboutBitPerfect { .. }
        ));
    }

    #[test]
    fn validate_bit_perfect_passes_for_passthrough_composition() {
        let topo = Topology {
            source_format: pcm(192_000),
            composition: Some(CompositionStage {
                mode: "passthrough".into(),
                preserves_bit_perfect: true,
                format_in: pcm(192_000),
                format_out: pcm(192_000),
            }),
            delivery_format: pcm(192_000),
            volume_mode: VolumeMode::Hardware,
        };
        validate_bit_perfect(&topo)
            .expect("passthrough composition is bit-perfect");
    }

    #[test]
    fn topology_volume_mode_none_is_bit_perfect() {
        // operator-managed downstream (analog preamp); the
        // chain has no scaling.
        let topo = Topology {
            source_format: pcm(192_000),
            composition: None,
            delivery_format: pcm(192_000),
            volume_mode: VolumeMode::None,
        };
        assert!(topo.is_bit_perfect());
        validate_bit_perfect(&topo).expect("volume_mode::None is bit-perfect");
    }

    #[test]
    fn operator_policy_pinned_round_trips_through_serde() {
        let policy = OperatorPolicy::Pinned {
            source_plugin: "com.tidal.streaming".into(),
            composition_plugin: Some(
                "org.evoframework.composition.alsa".into(),
            ),
            delivery_plugin: "org.evoframework.delivery.alsa".into(),
        };
        let json = serde_json::to_string(&policy).unwrap();
        assert!(json.contains("\"kind\":\"pinned\""));
        let parsed: OperatorPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, policy);
    }
}
