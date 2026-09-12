// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Audio operator preferences substrate — per-delivery-target
//! policy and volume mode.
//!
//! The topology scorer (sub-primitive C) takes
//! [`OperatorPolicy`] and [`VolumeMode`] as inputs alongside
//! the consolidated hardware profile. Both are operator
//! intent; the framework stores them per-delivery-target
//! keyed by the canonical hardware-identity string.
//!
//! Defaults (read-time, when no row exists for a target):
//!
//! - **Policy**: [`OperatorPolicy::Auto`] — opportunistic
//!   best-fit. Engine picks the highest-scoring topology.
//! - **Volume mode**: [`VolumeMode::Software`] — the safe
//!   universal default. Operator opts into `Hardware` when the
//!   hardware profile reports a hardware volume capability,
//!   or `None` for fully-open digital tap into a downstream
//!   analog preamp.
//!
//! Two stores rather than one denser blob because policy and
//! volume mode have independent mutation cadences (policy
//! changes rarely; volume mode is operator-touched per
//! session) and different operator surfaces. Sharing the
//! identity-key column keeps the join efficient when the
//! topology subject pulls both in one read.

use std::sync::Arc;

use evo::persistence::{
    PersistedAudioOperatorPolicy, PersistedAudioVolumeMode, PersistenceError,
    PersistenceStore,
};
use evo::server::{
    AudioOperatorPolicyRecord, AudioVolumeModeRecord, OperatorPolicy,
    VolumeMode,
};

/// Errors raised by [`AudioPolicyStore`].
#[derive(Debug, thiserror::Error)]
pub enum AudioPolicyError {
    /// Underlying persistence layer error.
    #[error("persistence error: {0}")]
    Persistence(#[from] PersistenceError),
    /// JSON serialise / deserialise failure on a substrate
    /// row. Indicates substrate corruption — should not occur
    /// in normal operation.
    #[error("malformed audio-policy row in substrate: {0}")]
    Deserialise(String),
}

/// Persistence-backed accessor for the audio operator
/// preferences substrate. Constructed once at boot and shared
/// between the server (operator surface) and downstream
/// consumers (topology scorer).
#[derive(Debug, Clone)]
pub struct AudioPolicyStore {
    persistence: Arc<dyn PersistenceStore>,
}

impl AudioPolicyStore {
    /// Construct a store wrapping the supplied persistence
    /// handle.
    pub fn new(persistence: Arc<dyn PersistenceStore>) -> Self {
        Self { persistence }
    }

    /// Record an operator policy for the supplied target.
    /// Idempotent on the canonical target key.
    pub async fn put_policy(
        &self,
        target_key: &str,
        policy: OperatorPolicy,
        principal: &str,
    ) -> Result<AudioOperatorPolicyRecord, AudioPolicyError> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let policy_json = serde_json::to_string(&policy).map_err(|e| {
            AudioPolicyError::Deserialise(format!(
                "serialise OperatorPolicy: {e}"
            ))
        })?;
        let record = PersistedAudioOperatorPolicy {
            target_key: target_key.to_string(),
            policy_json,
            set_at_ms: now_ms,
            set_by_principal: principal.to_string(),
        };
        self.persistence
            .put_audio_operator_policy(record.clone())
            .await?;
        Ok(AudioOperatorPolicyRecord {
            target_key: record.target_key,
            policy,
            set_at_ms: record.set_at_ms,
            set_by_principal: record.set_by_principal,
        })
    }

    /// Fetch one operator policy by canonical target key.
    /// Returns `None` when no policy is recorded; the topology
    /// scorer treats that as the framework default
    /// ([`OperatorPolicy::Auto`]).
    pub async fn get_policy(
        &self,
        target_key: &str,
    ) -> Result<Option<AudioOperatorPolicyRecord>, AudioPolicyError> {
        let row = self
            .persistence
            .get_audio_operator_policy(target_key)
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let policy: OperatorPolicy = serde_json::from_str(&row.policy_json)
            .map_err(|e| {
                AudioPolicyError::Deserialise(format!(
                    "deserialise OperatorPolicy: {e}"
                ))
            })?;
        Ok(Some(AudioOperatorPolicyRecord {
            target_key: row.target_key,
            policy,
            set_at_ms: row.set_at_ms,
            set_by_principal: row.set_by_principal,
        }))
    }

    /// List every recorded operator policy. Order is
    /// `target_key` ascending.
    pub async fn list_policies(
        &self,
    ) -> Result<Vec<AudioOperatorPolicyRecord>, AudioPolicyError> {
        let rows = self.persistence.list_audio_operator_policies().await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let policy: OperatorPolicy = serde_json::from_str(&row.policy_json)
                .map_err(|e| {
                    AudioPolicyError::Deserialise(format!(
                        "deserialise OperatorPolicy for target {tk:?}: {e}",
                        tk = row.target_key
                    ))
                })?;
            out.push(AudioOperatorPolicyRecord {
                target_key: row.target_key,
                policy,
                set_at_ms: row.set_at_ms,
                set_by_principal: row.set_by_principal,
            });
        }
        Ok(out)
    }

    /// Clear an operator policy by canonical target key.
    /// Idempotent on absent keys (no-op).
    pub async fn clear_policy(
        &self,
        target_key: &str,
    ) -> Result<(), AudioPolicyError> {
        self.persistence
            .delete_audio_operator_policy(target_key)
            .await?;
        Ok(())
    }

    /// Record a volume-mode preference for the supplied
    /// target. Idempotent on the canonical target key.
    pub async fn put_volume_mode(
        &self,
        target_key: &str,
        volume_mode: VolumeMode,
        principal: &str,
    ) -> Result<AudioVolumeModeRecord, AudioPolicyError> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let token = volume_mode_token(volume_mode);
        let record = PersistedAudioVolumeMode {
            target_key: target_key.to_string(),
            volume_mode: token.to_string(),
            set_at_ms: now_ms,
            set_by_principal: principal.to_string(),
        };
        self.persistence
            .put_audio_volume_mode(record.clone())
            .await?;
        Ok(AudioVolumeModeRecord {
            target_key: record.target_key,
            volume_mode,
            set_at_ms: record.set_at_ms,
            set_by_principal: record.set_by_principal,
        })
    }

    /// Fetch one volume-mode preference by canonical target
    /// key. Returns `None` when no preference is recorded; the
    /// topology scorer treats that as the framework default
    /// ([`VolumeMode::Software`]).
    pub async fn get_volume_mode(
        &self,
        target_key: &str,
    ) -> Result<Option<AudioVolumeModeRecord>, AudioPolicyError> {
        let row = self.persistence.get_audio_volume_mode(target_key).await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let volume_mode = parse_volume_mode_token(&row.volume_mode)?;
        Ok(Some(AudioVolumeModeRecord {
            target_key: row.target_key,
            volume_mode,
            set_at_ms: row.set_at_ms,
            set_by_principal: row.set_by_principal,
        }))
    }

    /// List every recorded volume-mode preference.
    pub async fn list_volume_modes(
        &self,
    ) -> Result<Vec<AudioVolumeModeRecord>, AudioPolicyError> {
        let rows = self.persistence.list_audio_volume_modes().await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let volume_mode = parse_volume_mode_token(&row.volume_mode)?;
            out.push(AudioVolumeModeRecord {
                target_key: row.target_key,
                volume_mode,
                set_at_ms: row.set_at_ms,
                set_by_principal: row.set_by_principal,
            });
        }
        Ok(out)
    }

    /// Clear a volume-mode preference by canonical target
    /// key. Idempotent on absent keys.
    pub async fn clear_volume_mode(
        &self,
        target_key: &str,
    ) -> Result<(), AudioPolicyError> {
        self.persistence
            .delete_audio_volume_mode(target_key)
            .await?;
        Ok(())
    }
}

/// Convert a typed [`VolumeMode`] to its canonical lowercase
/// ASCII storage token (`software` / `hardware` / `none`).
fn volume_mode_token(mode: VolumeMode) -> &'static str {
    match mode {
        VolumeMode::Software => "software",
        VolumeMode::Hardware => "hardware",
        VolumeMode::None => "none",
    }
}

/// Parse a substrate volume-mode token back into the typed
/// [`VolumeMode`]. Returns
/// [`AudioPolicyError::Deserialise`] on an unrecognised token
/// — should not occur because the substrate's CHECK
/// constraint enforces the closed set.
fn parse_volume_mode_token(s: &str) -> Result<VolumeMode, AudioPolicyError> {
    match s {
        "software" => Ok(VolumeMode::Software),
        "hardware" => Ok(VolumeMode::Hardware),
        "none" => Ok(VolumeMode::None),
        other => Err(AudioPolicyError::Deserialise(format!(
            "unrecognised volume_mode token in substrate: {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo::persistence::MemoryPersistenceStore;

    fn fixture() -> AudioPolicyStore {
        AudioPolicyStore::new(Arc::new(MemoryPersistenceStore::default()))
    }

    #[tokio::test]
    async fn put_then_get_policy_round_trips() {
        let s = fixture();
        let key = "usb:vid=0x21b4,pid=0x0096";
        let record = s
            .put_policy(key, OperatorPolicy::StrictBitPerfect, "user:1000")
            .await
            .unwrap();
        assert_eq!(record.policy, OperatorPolicy::StrictBitPerfect);
        let got = s.get_policy(key).await.unwrap().unwrap();
        assert_eq!(got.policy, OperatorPolicy::StrictBitPerfect);
        assert_eq!(got.set_by_principal, "user:1000");
    }

    #[tokio::test]
    async fn put_policy_is_idempotent_on_target() {
        let s = fixture();
        let key = "usb:vid=0x21b4,pid=0x0096";
        s.put_policy(key, OperatorPolicy::Auto, "alice")
            .await
            .unwrap();
        s.put_policy(key, OperatorPolicy::StrictBitPerfect, "bob")
            .await
            .unwrap();
        let got = s.get_policy(key).await.unwrap().unwrap();
        assert_eq!(got.policy, OperatorPolicy::StrictBitPerfect);
        assert_eq!(got.set_by_principal, "bob");
        let all = s.list_policies().await.unwrap();
        assert_eq!(all.len(), 1, "no duplicate row");
    }

    #[tokio::test]
    async fn pinned_policy_round_trips_through_substrate() {
        let s = fixture();
        let key = "usb:vid=0x21b4,pid=0x0096";
        let pinned = OperatorPolicy::Pinned {
            source_plugin: "com.tidal.streaming".into(),
            composition_plugin: Some(
                "org.evoframework.composition.alsa".into(),
            ),
            delivery_plugin: "org.evoframework.delivery.alsa".into(),
        };
        s.put_policy(key, pinned.clone(), "alice").await.unwrap();
        let got = s.get_policy(key).await.unwrap().unwrap();
        assert_eq!(got.policy, pinned);
    }

    #[tokio::test]
    async fn get_absent_policy_returns_none() {
        let s = fixture();
        let got = s.get_policy("usb:vid=0xdead,pid=0xbeef").await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn clear_policy_removes_recorded_row() {
        let s = fixture();
        let key = "usb:vid=0x21b4,pid=0x0096";
        s.put_policy(key, OperatorPolicy::Auto, "alice")
            .await
            .unwrap();
        s.clear_policy(key).await.unwrap();
        let got = s.get_policy(key).await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn put_then_get_volume_mode_round_trips() {
        let s = fixture();
        let key = "usb:vid=0x21b4,pid=0x0096";
        let record = s
            .put_volume_mode(key, VolumeMode::Hardware, "user:1000")
            .await
            .unwrap();
        assert_eq!(record.volume_mode, VolumeMode::Hardware);
        let got = s.get_volume_mode(key).await.unwrap().unwrap();
        assert_eq!(got.volume_mode, VolumeMode::Hardware);
    }

    #[tokio::test]
    async fn put_volume_mode_is_idempotent_on_target() {
        let s = fixture();
        let key = "usb:vid=0x21b4,pid=0x0096";
        s.put_volume_mode(key, VolumeMode::Software, "alice")
            .await
            .unwrap();
        s.put_volume_mode(key, VolumeMode::Hardware, "bob")
            .await
            .unwrap();
        let got = s.get_volume_mode(key).await.unwrap().unwrap();
        assert_eq!(got.volume_mode, VolumeMode::Hardware);
        assert_eq!(got.set_by_principal, "bob");
    }

    #[tokio::test]
    async fn list_returns_every_recorded_row_in_key_order() {
        let s = fixture();
        s.put_policy(
            "usb:vid=0x1234,pid=0x0001",
            OperatorPolicy::Auto,
            "alice",
        )
        .await
        .unwrap();
        s.put_policy(
            "alsa:HDA-Intel",
            OperatorPolicy::StrictBitPerfect,
            "alice",
        )
        .await
        .unwrap();
        let rows = s.list_policies().await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].target_key, "alsa:HDA-Intel");
        assert_eq!(rows[1].target_key, "usb:vid=0x1234,pid=0x0001");
    }

    #[tokio::test]
    async fn volume_mode_none_round_trips() {
        let s = fixture();
        let key = "alsa:downstream-preamp";
        s.put_volume_mode(key, VolumeMode::None, "alice")
            .await
            .unwrap();
        let got = s.get_volume_mode(key).await.unwrap().unwrap();
        assert_eq!(got.volume_mode, VolumeMode::None);
    }
}
