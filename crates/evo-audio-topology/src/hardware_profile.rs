// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Hardware profile substrate.
//!
//! Per-delivery-target metadata the topology scorer consumes
//! to pick the audio chain. A hardware profile is the
//! consolidation of FOUR layered sources:
//!
//! - **Probed (live)**: pcm_native / dsd_native /
//!   exclusive_mode / hdmi_arc.negotiated. Read at admission
//!   and on hot-plug via ALSA `hw_params`, USB descriptors,
//!   HDMI EDID, Pi HAT EEPROM. Owned by the delivery plugin's
//!   probe routines; the framework consumes the output.
//! - **Declared by delivery plugin**: tier / post-processing
//!   claims / reclocked_downstream. Lives in the plugin's
//!   manifest under `[capabilities.delivery]`. The framework
//!   reads it at admission via the existing manifest path.
//! - **Database lookup**: tier / reclocking / post-processing
//!   / hardware volume capability for known SKUs. The database
//!   lives at the reference generic device tier
//!   (`evo-device-audio`) and is shipped in the vendor
//!   distribution; the framework consumes the lookup result
//!   without owning the database.
//! - **Operator override**: every overridable field. Operators
//!   force a tier, force-disable hardware volume, declare
//!   strict-purist mode, pin a topology. THIS is the substrate
//!   the framework owns and stores in the persistence layer.
//!
//! The composer (sub-primitive C — topology scoring) layers
//! the four sources on demand to produce a [`HardwareProfile`]
//! the topology scorer consumes. This module ships:
//!
//! - The typed [`HardwareProfile`] consolidated shape that the
//!   composer emits and the topology scorer / operator UI
//!   consumes.
//! - The typed [`HardwareProfileOverride`] sparse record the
//!   operator authors — every field is `Option<T>` so the
//!   operator overrides only what they explicitly intend, and
//!   the composer applies overrides on top of probed +
//!   declared + database.
//! - The [`HardwareProfileStore`] persistence-backed accessor
//!   for the operator-override layer.
//!
//! Compose / probe / database lookup are out of scope of this
//! module — they ride sub-primitive C alongside the topology
//! scorer (compose) and the vendor distribution
//! (probe + database).

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use evo::persistence::{
    PersistedHardwareProfileOverride, PersistenceError, PersistenceStore,
};
use evo::server::{
    HardwareIdentity, HardwareProfileOverride, HardwareProfileOverrideRecord,
    HardwareTier, HardwareVolumeCapability, PostProcessing,
    TopologyPreferences,
};
use evo_plugin_sdk::audio::{AudioFormat, DsdRate, DsdTransport};

/// PCM capability declaration — what rates / bit-depths /
/// channel counts the hardware supports for PCM input.
///
/// The probe layer populates this; the operator may not
/// override it (probed-live data is the truth source for
/// hardware capability — the override layer can force tier,
/// disable hardware volume, etc., but cannot lie about
/// supported PCM shapes).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PcmCapabilities {
    /// Supported sample rates in Hz.
    #[serde(default)]
    pub rates_hz: Vec<u32>,
    /// Supported bit depths (16 / 24 / 32).
    #[serde(default)]
    pub bit_depths: Vec<u8>,
    /// Supported channel counts (typically `[2]`; `[2, 6, 8]`
    /// for multi-channel HDMI).
    #[serde(default)]
    pub channels: Vec<u8>,
}

/// DSD capability declaration. Present iff the hardware
/// supports DSD natively (audiophile USB DACs) or via DoP
/// (most generic USB-Audio Class 2 interfaces).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DsdCapabilities {
    /// Supported DSD rates.
    #[serde(default)]
    pub rates: Vec<DsdRate>,
    /// Supported transport carriers.
    #[serde(default)]
    pub transports: Vec<DsdTransport>,
}

/// HDMI ARC negotiated state. Populated only for HDMI delivery
/// targets where the framework has run an ARC handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HdmiArcState {
    /// Format the ARC channel negotiated with the AVR. The
    /// topology subject reflects this so the operator sees
    /// the true downstream cap (`HDMI ARC limited to 48 kHz`
    /// when the source is 192 kHz).
    pub negotiated_format: AudioFormat,
}

/// Consolidated hardware profile — the topology scorer's
/// input. Composed on-demand from probe + manifest declared +
/// database lookup + operator override (sub-primitive C).
/// Stored only as a UI cache on the topology subject; never
/// the substrate's authoritative form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardwareProfile {
    /// Identity of the underlying hardware.
    pub identity: HardwareIdentity,
    /// Tier classification.
    pub tier: HardwareTier,
    /// Native PCM capability (probed).
    pub pcm_native: PcmCapabilities,
    /// Native DSD capability (probed). `None` when the
    /// hardware does not support DSD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dsd_native: Option<DsdCapabilities>,
    /// `true` when the delivery plugin opens the device in
    /// exclusive / hardware-direct mode.
    pub exclusive_mode: bool,
    /// `true` when the chain has an external reclocker
    /// downstream (IAN Canada FIFO, etc.). The topology
    /// scorer skips reclocking shims when this is true so
    /// the framework does not fight the external reclocker.
    pub reclocked_downstream: bool,
    /// Hardware volume capability.
    pub hardware_volume: HardwareVolumeCapability,
    /// `true` when the hardware offers internal DSP / room
    /// correction.
    pub hardware_dsp: bool,
    /// Post-processing claims.
    pub post_processing: PostProcessing,
    /// HDMI ARC negotiated state. Populated only for HDMI
    /// delivery targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hdmi_arc: Option<HdmiArcState>,
    /// Topology preferences.
    pub prefer: TopologyPreferences,
}

/// Errors raised by [`HardwareProfileStore`].
#[derive(Debug, thiserror::Error)]
pub enum HardwareProfileError {
    /// Underlying persistence layer error.
    #[error("persistence error: {0}")]
    Persistence(#[from] PersistenceError),
    /// Operator submitted an empty override. The store
    /// refuses no-op rows to keep the substrate clean.
    #[error("override has no fields set; submit at least one override field")]
    EmptyOverride,
    /// JSON deserialise failure on a substrate row. Indicates
    /// substrate corruption — should not occur in normal
    /// operation.
    #[error("malformed override row in substrate: {0}")]
    Deserialise(String),
}

/// Persistence-backed accessor for the hardware-profile
/// override substrate.
/// Probe-side snapshot of one delivery target — what the
/// delivery plugin's probe routine reports to the framework at
/// admission and on hot-plug. Probed data is the truth source
/// for hardware capability (PCM rates / bit-depths / DSD / ARC
/// negotiation); the override layer cannot lie about supported
/// shapes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbedHardwareData {
    /// Native PCM capability.
    pub pcm_native: PcmCapabilities,
    /// Native DSD capability. `None` when the hardware does
    /// not support DSD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dsd_native: Option<DsdCapabilities>,
    /// `true` when the device opens in exclusive mode.
    #[serde(default)]
    pub exclusive_mode: bool,
    /// HDMI ARC negotiated state (HDMI delivery only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hdmi_arc: Option<HdmiArcState>,
}

/// Manifest-declared snapshot — the bits the delivery plugin
/// declares about its underlying hardware in
/// `[capabilities.delivery]`. The plugin's manifest is the
/// authoring surface; the framework reads via the existing
/// manifest path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredHardwareData {
    /// Plugin-declared tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<HardwareTier>,
    /// Plugin-declared post-processing claims.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_processing: Option<PostProcessing>,
    /// `true` when the chain has an external reclocker
    /// downstream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reclocked_downstream: Option<bool>,
    /// `true` when the hardware offers internal DSP / room
    /// correction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hardware_dsp: Option<bool>,
}

/// Vendor-database snapshot — the bits the vendor distribution
/// (`evo-device-audio`) ships for known SKUs. The database is
/// data, not code; vendor PRs add new entries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseHardwareData {
    /// Database-classified tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<HardwareTier>,
    /// Database-recorded reclocking flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reclocked_downstream: Option<bool>,
    /// Database-recorded post-processing claims.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_processing: Option<PostProcessing>,
    /// Database-recorded hardware-volume capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hardware_volume: Option<HardwareVolumeCapability>,
    /// Database-recorded hardware-DSP flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hardware_dsp: Option<bool>,
    /// Database-suggested topology preferences.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefer: Option<TopologyPreferences>,
}

/// Layer the four hardware-profile sources into a single
/// consolidated [`HardwareProfile`]. Precedence (highest wins
/// per field):
///
/// 1. **Operator override** — every overridable field.
/// 2. **Vendor database** — tier / reclocking / post-processing
///    / hardware volume / DSP / preferences.
/// 3. **Manifest declared** — tier / post-processing /
///    reclocked / DSP.
/// 4. **Probe** — capability data only (pcm_native / dsd_native
///    / exclusive_mode / hdmi_arc).
///
/// PCM / DSD capabilities, exclusive-mode, and HDMI-ARC state
/// come exclusively from the probe layer (the override cannot
/// lie about supported shapes). Tier defaults to `Mainstream`
/// when no source supplies one. `hardware_volume` defaults to
/// `None` (software-only) when no source supplies it. The
/// composer is a pure function — same inputs always produce
/// the same output, deterministic for forensics.
pub fn compose_profile(
    identity: HardwareIdentity,
    probed: ProbedHardwareData,
    declared: DeclaredHardwareData,
    database: DatabaseHardwareData,
    override_: Option<HardwareProfileOverride>,
) -> HardwareProfile {
    let override_ = override_.unwrap_or_default();

    // Tier: override > database > declared > Mainstream
    let tier = override_
        .tier
        .or(database.tier)
        .or(declared.tier)
        .unwrap_or(HardwareTier::Mainstream);

    // Hardware volume: override > database > None
    let hardware_volume = override_
        .hardware_volume
        .or(database.hardware_volume)
        .unwrap_or(HardwareVolumeCapability::None);

    // Reclocked downstream: override > database > declared > false
    let reclocked_downstream = override_
        .reclocked_downstream
        .or(database.reclocked_downstream)
        .or(declared.reclocked_downstream)
        .unwrap_or(false);

    // Hardware DSP: override > database > declared > false
    let hardware_dsp = override_
        .hardware_dsp
        .or(database.hardware_dsp)
        .or(declared.hardware_dsp)
        .unwrap_or(false);

    // Post-processing: override > database > declared > default
    let post_processing = override_
        .post_processing
        .or(database.post_processing)
        .or(declared.post_processing)
        .unwrap_or_default();

    // Topology preferences: override > database > default
    let prefer = override_.prefer.or(database.prefer).unwrap_or_default();

    // Display name: override > identity (already set by probe)
    let display_name = override_
        .display_name
        .unwrap_or_else(|| identity.display_name.clone());

    HardwareProfile {
        identity: HardwareIdentity {
            display_name,
            ..identity
        },
        tier,
        pcm_native: probed.pcm_native,
        dsd_native: probed.dsd_native,
        exclusive_mode: probed.exclusive_mode,
        reclocked_downstream,
        hardware_volume,
        hardware_dsp,
        post_processing,
        hdmi_arc: probed.hdmi_arc,
        prefer,
    }
}

/// Persistence-backed accessor for the hardware-profile
/// override substrate. Constructed once at boot and shared
/// between the server (operator surface) and downstream
/// consumers (composer + topology scorer).
#[derive(Debug, Clone)]
pub struct HardwareProfileStore {
    persistence: Arc<dyn PersistenceStore>,
}

impl HardwareProfileStore {
    /// Construct a store wrapping the supplied persistence
    /// handle.
    pub fn new(persistence: Arc<dyn PersistenceStore>) -> Self {
        Self { persistence }
    }

    /// Record an operator override for the supplied identity.
    /// Idempotent on the identity key — re-putting advances
    /// `updated_at_ms` and replaces the override record
    /// without duplicating the row. Refuses an empty override.
    pub async fn put_override(
        &self,
        identity: HardwareIdentity,
        override_: HardwareProfileOverride,
        principal: &str,
    ) -> Result<HardwareProfileOverrideRecord, HardwareProfileError> {
        if override_.is_empty() {
            return Err(HardwareProfileError::EmptyOverride);
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let override_json = serde_json::to_string(&override_).map_err(|e| {
            HardwareProfileError::Deserialise(format!(
                "serialise override: {e}"
            ))
        })?;
        let record = PersistedHardwareProfileOverride {
            key: identity.key(),
            identity: identity.clone(),
            override_json,
            updated_at_ms: now_ms,
            updated_by_principal: principal.to_string(),
        };
        self.persistence
            .put_hardware_profile_override(record.clone())
            .await?;
        Ok(record.into())
    }

    /// Fetch one override by identity key. Returns `None`
    /// when no override is recorded.
    pub async fn get_override(
        &self,
        key: &str,
    ) -> Result<Option<HardwareProfileOverrideRecord>, HardwareProfileError>
    {
        let row = self.persistence.get_hardware_profile_override(key).await?;
        Ok(row.map(Into::into))
    }

    /// List every recorded override across all identities.
    /// Order is `key` ascending.
    pub async fn list_overrides(
        &self,
    ) -> Result<Vec<HardwareProfileOverrideRecord>, HardwareProfileError> {
        let rows = self.persistence.list_hardware_profile_overrides().await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Clear an operator override by identity key. Idempotent
    /// on absent keys (no-op).
    pub async fn clear_override(
        &self,
        key: &str,
    ) -> Result<(), HardwareProfileError> {
        self.persistence
            .delete_hardware_profile_override(key)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo::persistence::MemoryPersistenceStore;
    use evo::server::VolumePreferenceClass;
    use evo_plugin_sdk::audio::PcmCodec;

    fn fixture() -> HardwareProfileStore {
        HardwareProfileStore::new(Arc::new(MemoryPersistenceStore::default()))
    }

    fn usb_identity(vid: u16, pid: u16) -> HardwareIdentity {
        HardwareIdentity {
            usb_vid_pid: Some((vid, pid)),
            alsa_card_name: "DragonFly Cobalt".into(),
            hat_eeprom_signature: None,
            hdmi_sink_id: None,
            display_name: "AudioQuest DragonFly Cobalt".into(),
        }
    }

    #[test]
    fn identity_key_picks_usb_when_present() {
        let id = usb_identity(0x21b4, 0x0096);
        assert_eq!(id.key(), "usb:vid=0x21b4,pid=0x0096");
    }

    #[test]
    fn identity_key_falls_back_to_alsa_when_no_other_discriminator() {
        let id = HardwareIdentity {
            usb_vid_pid: None,
            alsa_card_name: "HDA-Intel".into(),
            hat_eeprom_signature: None,
            hdmi_sink_id: None,
            display_name: "On-board".into(),
        };
        assert_eq!(id.key(), "alsa:HDA-Intel");
    }

    #[test]
    fn identity_key_prefers_hat_over_alsa() {
        let id = HardwareIdentity {
            usb_vid_pid: None,
            alsa_card_name: "snd_rpi_hifiberry_dac".into(),
            hat_eeprom_signature: Some("hifiberry-dac+".into()),
            hdmi_sink_id: None,
            display_name: "HiFiBerry DAC+".into(),
        };
        assert_eq!(id.key(), "hat:hifiberry-dac+");
    }

    #[test]
    fn identity_key_prefers_hdmi_over_alsa() {
        let id = HardwareIdentity {
            usb_vid_pid: None,
            alsa_card_name: "HDMI-Audio".into(),
            hat_eeprom_signature: None,
            hdmi_sink_id: Some("Onkyo TX-NR797".into()),
            display_name: "Onkyo AVR".into(),
        };
        assert_eq!(id.key(), "hdmi:Onkyo TX-NR797");
    }

    #[test]
    fn override_is_empty_when_no_fields_set() {
        let o = HardwareProfileOverride::default();
        assert!(o.is_empty());
    }

    #[test]
    fn override_is_not_empty_when_one_field_set() {
        let o = HardwareProfileOverride {
            tier: Some(HardwareTier::Audiophile),
            ..Default::default()
        };
        assert!(!o.is_empty());
    }

    #[tokio::test]
    async fn put_then_get_round_trips() {
        let s = fixture();
        let id = usb_identity(0x21b4, 0x0096);
        let key = id.key();
        let override_ = HardwareProfileOverride {
            tier: Some(HardwareTier::Audiophile),
            hardware_volume: Some(HardwareVolumeCapability::AnalogOnly),
            note: Some("known-good audiophile config".into()),
            ..Default::default()
        };
        let record = s
            .put_override(id, override_.clone(), "user:1000")
            .await
            .expect("put_override");
        assert_eq!(record.override_, override_);
        assert_eq!(record.updated_by_principal, "user:1000");

        let got = s.get_override(&key).await.expect("get").expect("present");
        assert_eq!(got.override_, override_);
    }

    #[tokio::test]
    async fn put_refuses_empty_override() {
        let s = fixture();
        let id = usb_identity(0x21b4, 0x0096);
        let err = s
            .put_override(id, HardwareProfileOverride::default(), "user:1000")
            .await
            .expect_err("empty override must be refused");
        assert!(matches!(err, HardwareProfileError::EmptyOverride));
    }

    #[tokio::test]
    async fn put_is_idempotent_on_identity_key() {
        let s = fixture();
        let id = usb_identity(0x21b4, 0x0096);
        let key = id.key();
        s.put_override(
            id.clone(),
            HardwareProfileOverride {
                tier: Some(HardwareTier::Audiophile),
                ..Default::default()
            },
            "alice",
        )
        .await
        .unwrap();
        s.put_override(
            id,
            HardwareProfileOverride {
                tier: Some(HardwareTier::Reference),
                note: Some("re-classified".into()),
                ..Default::default()
            },
            "bob",
        )
        .await
        .unwrap();

        let got = s.get_override(&key).await.unwrap().unwrap();
        assert_eq!(got.override_.tier, Some(HardwareTier::Reference));
        assert_eq!(got.override_.note.as_deref(), Some("re-classified"));
        assert_eq!(got.updated_by_principal, "bob");

        let all = s.list_overrides().await.unwrap();
        assert_eq!(all.len(), 1, "no duplicate row on re-put");
    }

    #[tokio::test]
    async fn list_returns_every_recorded_override_in_key_order() {
        let s = fixture();
        s.put_override(
            HardwareIdentity {
                usb_vid_pid: Some((0x1234, 0x0001)),
                alsa_card_name: "x".into(),
                hat_eeprom_signature: None,
                hdmi_sink_id: None,
                display_name: "X".into(),
            },
            HardwareProfileOverride {
                tier: Some(HardwareTier::Audiophile),
                ..Default::default()
            },
            "alice",
        )
        .await
        .unwrap();
        s.put_override(
            HardwareIdentity {
                usb_vid_pid: None,
                alsa_card_name: "y".into(),
                hat_eeprom_signature: Some("yhat".into()),
                hdmi_sink_id: None,
                display_name: "Y".into(),
            },
            HardwareProfileOverride {
                tier: Some(HardwareTier::Reference),
                ..Default::default()
            },
            "alice",
        )
        .await
        .unwrap();

        let rows = s.list_overrides().await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].identity.key(), "hat:yhat");
        assert_eq!(rows[1].identity.key(), "usb:vid=0x1234,pid=0x0001");
    }

    #[tokio::test]
    async fn clear_removes_recorded_override() {
        let s = fixture();
        let id = usb_identity(0x21b4, 0x0096);
        let key = id.key();
        s.put_override(
            id,
            HardwareProfileOverride {
                tier: Some(HardwareTier::Audiophile),
                ..Default::default()
            },
            "alice",
        )
        .await
        .unwrap();
        s.clear_override(&key).await.unwrap();
        let got = s.get_override(&key).await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn clear_absent_key_is_noop() {
        let s = fixture();
        s.clear_override("usb:vid=0xdead,pid=0xbeef")
            .await
            .expect("clearing absent key is a no-op");
    }

    #[test]
    fn hdmi_arc_state_round_trips_through_serde() {
        let state = HdmiArcState {
            negotiated_format: AudioFormat::Pcm {
                codec: PcmCodec::PcmS16Le,
                rate_hz: 48_000,
                channels: 6,
            },
        };
        let json = serde_json::to_string(&state).unwrap();
        let parsed: HdmiArcState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, state);
    }

    #[test]
    fn hardware_volume_capability_both_serialises_with_preference() {
        let cap = HardwareVolumeCapability::Both {
            prefer: VolumePreferenceClass::Analog,
        };
        let json = serde_json::to_string(&cap).unwrap();
        // Tagged-serde shape: kind + nested fields.
        assert!(json.contains("\"kind\":\"both\""));
        assert!(json.contains("\"prefer\":\"analog\""));
    }
}
