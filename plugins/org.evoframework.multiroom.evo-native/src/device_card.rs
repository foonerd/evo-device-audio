// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Per-device card envelope subject publication.
//!
//! The multi-room plugin publishes one
//! `audio.multiroom.device_card` subject instance per local
//! entity it cares for (this device's own card when not under
//! a source-host, plus per-member cards when the local
//! admission holds the source-host role for a group). Each
//! instance carries a [`MultiroomDeviceCardEnvelope`] value
//! shaped per the schema declaration in
//! `evo-catalogue-schemas/schemas/org.evoframework/audio/
//! multiroom.v1.toml`.
//!
//! The envelope unifies the rendering surface across every
//! card case (solo device, group leader, group member,
//! subgroup, master group); UI cards subscribe once per
//! entity and receive every transition reactively through
//! the framework's subject-state subscription substrate.
//!
//! Initial publication happens at plugin load with the
//! current observable state. Subsequent updates fire on
//! every relevant audio-plane / group / source-host /
//! clock-sync transition; the publication path is
//! best-effort — a failed announce / update logs at warn
//! but never panics or fails plugin load.

use serde::Serialize;
use std::time::SystemTime;

/// External-addressing scheme for per-device card subjects.
/// Consumers resolve the canonical id by querying the
/// framework's subject querier with `(scheme =
/// DEVICE_CARD_SCHEME, value = <device-or-group-id>)`.
pub const DEVICE_CARD_SCHEME: &str = "evo.audio.multiroom.device_card";

/// Subject type registered with the framework. Matches the
/// `[[subjects]]` declaration in the catalogue-schemas repo.
pub const DEVICE_CARD_SUBJECT_TYPE: &str = "audio_multiroom_device_card";

/// Default placeholder URL when no theme is active. Operators
/// see a neutral evo glyph fallback until a theme contributes
/// its own device-themed placeholder per the universal
/// artwork-first-or-icon rule.
pub const FRAMEWORK_DEFAULT_PLACEHOLDER_URL: &str =
    "evo://theme/placeholder/device.svg";

/// Role badge discriminator. Distinguishes between the
/// role-tier badges (solo / leader / member / subgroup /
/// master); the transport-state-tier visual is carried
/// separately by [`TransportState`].
///
/// Variants beyond the three the initial publish path uses
/// (Solo / LeaderOfGroup / MemberOfGroup) carry the
/// publish-on-happening surface for the master-group
/// hierarchy; the constructed-at-publish set grows as the
/// happening-driven publish iterations land.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StateBadge {
    /// Entity is a solo device (not a member of any group).
    Solo,
    /// Entity is the source-host (leader) of a group.
    LeaderOfGroup,
    /// Entity is a non-leader member of a group.
    MemberOfGroup,
    /// Entity is a subgroup within a master group
    /// (master-group hierarchy lands in a follow-on release).
    SubgroupOfMaster,
    /// Entity is itself a master group (master-group
    /// hierarchy lands in a follow-on release).
    MasterGroup,
}

/// Current transport state of the entity. Drives card
/// content per the state-to-visual mapping in the public
/// design-doc mirror.
///
/// Variants beyond the initial-publish floor (Idle) become
/// constructed as the publish-on-happening surface lands
/// (Playing / Paused from the playback subject's `state`
/// transitions, Offline from audio-plane peer-connection
/// loss, etc.). The full set is fixed by the schema
/// declaration so the wire shape stays stable as the
/// publisher's source state grows richer.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TransportState {
    /// Audio is playing.
    Playing,
    /// Audio is loaded and paused.
    Paused,
    /// Audio playback is stopped.
    Stopped,
    /// No source loaded — the initial-publish default.
    #[default]
    Idle,
    /// Entity has not been observed on the audio-plane
    /// recently.
    Offline,
    /// Entity is discovered via mDNS but not yet admitted
    /// to the household.
    Unpaired,
    /// Entity was previously admitted and has been
    /// operator-revoked.
    Revoked,
}

/// Track summary carried in the card envelope when the
/// transport state is Playing or Paused. None for the other
/// states.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TrackSummary {
    /// URL of the track artwork served by the framework's
    /// HTTPS artwork endpoint (size discriminator on the URL
    /// path; the multi-size pipeline). None falls back to
    /// the theme placeholder per the artwork-first-or-icon
    /// rule.
    pub artwork_url: Option<String>,
    /// Track title from the playback source's
    /// `audio.playback.current` subject.
    pub title: String,
    /// Track artist when known. None for sources that do
    /// not surface artist metadata (radio streams, sample
    /// tracks).
    pub artist: Option<String>,
    /// Track album when known.
    pub album: Option<String>,
}

/// Master-group context carried in the card envelope when
/// the entity is a member of a master group. None for the
/// other states. Lands populated in a follow-on release; the
/// current release's first cut never sets it (flat groups
/// only).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MasterGroupContext {
    /// Canonical id of the master group.
    pub master_group_id: String,
    /// Operator-visible display name of the master group.
    pub master_group_name: String,
    /// Operator-configured per-member delay for this entity
    /// within the master group. Negative = entity plays
    /// earlier than the group reference; positive = later.
    pub delay_ms: i32,
    /// Audible-time-honest computed per-member delay,
    /// honouring the universal honest-degradation contract.
    pub audible_time_ms: i32,
}

/// Per-device card envelope. One instance per device-or-group
/// entity in the household; published on the
/// `audio.multiroom.device_card` subject. Shape matches the
/// schema declaration in the catalogue-schemas repo.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MultiroomDeviceCardEnvelope {
    /// Subject-instance key. Canonical id of the device or
    /// group the card represents.
    pub device_or_group_id: String,
    /// Operator-visible name.
    pub display_name: String,
    /// Role badge per the universal state-to-visual mapping.
    pub state_badge: StateBadge,
    /// Current transport state of the entity.
    pub transport_state: TransportState,
    /// Track currently loaded when transport_state is
    /// Playing or Paused. None otherwise.
    pub current_track: Option<TrackSummary>,
    /// URL of the theme-contributed device-placeholder
    /// artwork. Rendered when transport_state is not Playing
    /// / Paused, or when `current_track.artwork_url` is
    /// None.
    pub theme_placeholder_artwork_url: String,
    /// Set when the entity is a member of a master group.
    /// None in the current release (flat groups only).
    pub master_group_context: Option<MasterGroupContext>,
    /// Wall-clock timestamp of the last envelope refresh.
    /// Operator surfaces render `<n>s ago`-style staleness
    /// indicators; reactive consumers ignore the field (they
    /// receive transitions on the underlying happenings).
    pub last_update_at: SystemTime,
}

impl MultiroomDeviceCardEnvelope {
    /// Build the initial-publish envelope for the local
    /// device. The envelope reflects what the multi-room
    /// plugin can observe at load time — the local node's
    /// own card, with transport_state defaulting to Idle
    /// pending the playback-subject subscription that will
    /// drive transitions in subsequent updates.
    ///
    /// Group / source-host state derives from the plugin's
    /// own configuration (the operator-set role + group_id);
    /// richer state (peer connection liveness, source-host
    /// election outcome, clock-sync offset) folds in as the
    /// publish-on-happening paths land in iteration.
    pub fn initial_for_local_device(
        device_id: String,
        display_name: String,
        role_badge: StateBadge,
    ) -> Self {
        Self {
            device_or_group_id: device_id,
            display_name,
            state_badge: role_badge,
            transport_state: TransportState::Idle,
            current_track: None,
            theme_placeholder_artwork_url: FRAMEWORK_DEFAULT_PLACEHOLDER_URL
                .to_string(),
            master_group_context: None,
            last_update_at: SystemTime::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_envelope_carries_supplied_identity() {
        let env = MultiroomDeviceCardEnvelope::initial_for_local_device(
            "device-uuid-1".to_string(),
            "Living Room".to_string(),
            StateBadge::Solo,
        );
        assert_eq!(env.device_or_group_id, "device-uuid-1");
        assert_eq!(env.display_name, "Living Room");
        assert_eq!(env.state_badge, StateBadge::Solo);
        assert_eq!(env.transport_state, TransportState::Idle);
        assert!(env.current_track.is_none());
        assert!(env.master_group_context.is_none());
    }

    #[test]
    fn envelope_serialises_to_json_with_schema_field_names() {
        let env = MultiroomDeviceCardEnvelope::initial_for_local_device(
            "device-uuid-1".to_string(),
            "Living Room".to_string(),
            StateBadge::LeaderOfGroup,
        );
        let json = serde_json::to_value(&env).expect("serialise envelope");
        assert_eq!(json["device_or_group_id"], "device-uuid-1");
        assert_eq!(json["display_name"], "Living Room");
        assert_eq!(json["state_badge"], "leader_of_group");
        assert_eq!(json["transport_state"], "idle");
        assert!(json["current_track"].is_null());
        assert!(json["master_group_context"].is_null());
        assert!(json["theme_placeholder_artwork_url"].is_string());
    }

    #[test]
    fn transport_state_default_is_idle() {
        assert_eq!(TransportState::default(), TransportState::Idle);
    }

    #[test]
    fn state_badge_serialises_snake_case() {
        let json =
            serde_json::to_value(StateBadge::MemberOfGroup).expect("serialise");
        assert_eq!(json, serde_json::Value::String("member_of_group".into()));
    }
}
