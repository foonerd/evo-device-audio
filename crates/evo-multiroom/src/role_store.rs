// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Per-device multi-room role substrate.
//!
//! Stores the operator-declared role each known device plays
//! in the multi-room topology. The substrate is the single
//! source of truth for role state — the multi-room plugin
//! subscribes to changes and reconfigures DAC + capture +
//! audio-plane connections in place, no plugin reload required.
//!
//! ## Role semantics
//!
//! - `Source`: operator's preferred source-host for its group.
//!   The framework's election machinery treats this as a
//!   declared preference; multiple `Source` devices in the
//!   same group resolve via the canonical-min election rule
//!   per the existing source-host election runtime.
//! - `Receiver`: rendering-only. The device opens `alsa_pcm`
//!   and subscribes to audio-plane frames for its group.
//! - `Auto`: no multi-room engagement. The DAC stays free for
//!   local-only playback (MPD on the device itself); the
//!   plugin does not open audio-plane connections nor register
//!   as an election candidate.
//!
//! ## Substrate-empty default
//!
//! Devices without an operator-gestured role have no row in
//! `device_role`. Reads return `None`; consumers treat that as
//! `Auto`. This makes fresh-boot non-disruptive — a device
//! admits the multi-room plugin cleanly with no multi-room
//! work to do until operator gestures arrive.
//!
//! ## Resource posture
//!
//! - Per-role write: one row UPSERT plus one broadcast
//!   `RoleChange` event. No per-tick allocation.
//! - Per-role read: one HashMap-equivalent SQL lookup.
//! - Subscriber broadcast channel capacity 32; drop-oldest on
//!   slow plugin; subscribers re-read the store after a
//!   missed event to recover state.

use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::broadcast;

use evo::happenings::{Happening, HappeningBus};
use evo::persistence::{PersistedDeviceRole, PersistenceStore};
pub use evo_primitives::{Role, RoleChange, RoleStoreError};

/// Subscription broadcast channel capacity. Sized for the
/// operator-paced role-mutation rate (rare) with headroom for
/// reactive-only plugins consuming events on their own task;
/// on overflow the channel drops oldest events.
const ROLE_SUBSCRIBER_CAPACITY: usize = 32;

/// Per-device multi-room role substrate.
pub struct RoleStore {
    persistence: Arc<dyn PersistenceStore>,
    happenings: Arc<HappeningBus>,
    change_tx: broadcast::Sender<RoleChange>,
}

impl std::fmt::Debug for RoleStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoleStore").finish_non_exhaustive()
    }
}

impl RoleStore {
    /// Construct a store wrapping the supplied persistence
    /// handle and happenings bus.
    pub fn new(
        persistence: Arc<dyn PersistenceStore>,
        happenings: Arc<HappeningBus>,
    ) -> Self {
        let (change_tx, _) = broadcast::channel(ROLE_SUBSCRIBER_CAPACITY);
        Self {
            persistence,
            happenings,
            change_tx,
        }
    }

    /// Subscribe to RoleChange events. Reactive-only plugins
    /// consume the channel to run their role-transition state
    /// machine in place.
    pub fn subscribe(&self) -> broadcast::Receiver<RoleChange> {
        self.change_tx.subscribe()
    }

    /// Set the operator-declared role for a device.
    /// Idempotent on unchanged value (no happening, no
    /// broadcast). Emits `DeviceRoleChanged` happening and a
    /// `RoleChange::Set` event on real change.
    ///
    /// The `set_by` parameter is an optional operator /
    /// surface identifier (CLI session, wire-op caller); it
    /// surfaces in the happening for audit.
    pub async fn set_role(
        &self,
        device_id: &str,
        role: Role,
        set_by: Option<String>,
    ) -> Result<(), RoleStoreError> {
        let device_id = validate_device_id(device_id)?;
        let prior = self
            .persistence
            .get_device_role(&device_id)
            .await?
            .and_then(|r| Role::from_str(&r.role).ok());
        if prior == Some(role) {
            // Idempotent no-op — substrate already at target.
            return Ok(());
        }
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.persistence
            .put_device_role(PersistedDeviceRole {
                device_id: device_id.clone(),
                role: role.as_str().to_string(),
                set_at_ms: now_ms,
                set_by: set_by.clone(),
            })
            .await?;
        self.emit(Happening::DeviceRoleChanged {
            device_id: device_id.clone(),
            prior_role: prior.map(|r| r.as_str().to_string()),
            new_role: role.as_str().to_string(),
            set_by,
            at: SystemTime::now(),
        })
        .await;
        self.broadcast(RoleChange::Set {
            device_id,
            prior_role: prior,
            new_role: role,
        });
        Ok(())
    }

    /// Read the operator-declared role for a device. Returns
    /// `Role::Auto` when the device has no row in the
    /// substrate — the non-disruptive substrate-empty default.
    pub async fn get_role(
        &self,
        device_id: &str,
    ) -> Result<Role, RoleStoreError> {
        let device_id = validate_device_id(device_id)?;
        Ok(self
            .persistence
            .get_device_role(&device_id)
            .await?
            .and_then(|r| Role::from_str(&r.role).ok())
            .unwrap_or(Role::Auto))
    }

    /// List every device with an explicit operator-gestured
    /// role. Devices in the substrate-empty / `Auto` default
    /// are NOT enumerated here (they have no row to surface).
    /// Operator surface renders the explicit set; absent
    /// devices are implied `Auto`.
    pub async fn list_explicit_roles(
        &self,
    ) -> Result<Vec<(String, Role)>, RoleStoreError> {
        let rows = self.persistence.list_device_roles().await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let role = Role::from_str(&r.role)?;
            out.push((r.device_id, role));
        }
        Ok(out)
    }

    /// Clear the operator-declared role for a device. The
    /// device returns to the substrate-empty `Auto` default.
    /// Idempotent on devices that already have no explicit
    /// row.
    pub async fn clear_role(
        &self,
        device_id: &str,
    ) -> Result<(), RoleStoreError> {
        let device_id = validate_device_id(device_id)?;
        let prior = self
            .persistence
            .get_device_role(&device_id)
            .await?
            .and_then(|r| Role::from_str(&r.role).ok());
        let Some(prior_role) = prior else {
            // Already cleared — idempotent no-op.
            return Ok(());
        };
        self.persistence.delete_device_role(&device_id).await?;
        self.emit(Happening::DeviceRoleChanged {
            device_id: device_id.clone(),
            prior_role: Some(prior_role.as_str().to_string()),
            new_role: Role::Auto.as_str().to_string(),
            set_by: None,
            at: SystemTime::now(),
        })
        .await;
        self.broadcast(RoleChange::Cleared {
            device_id,
            prior_role,
        });
        Ok(())
    }

    fn broadcast(&self, change: RoleChange) {
        let _ = self.change_tx.send(change);
    }

    async fn emit(&self, happening: Happening) {
        if let Err(e) = self.happenings.emit_durable(happening).await {
            tracing::warn!(
                error = %e,
                "role store: emit happening failed"
            );
        }
    }
}

#[async_trait::async_trait]
impl evo_primitives::RoleStoreHandle for RoleStore {
    async fn set_role(
        &self,
        device_id: &str,
        role: Role,
        set_by: Option<String>,
    ) -> Result<(), RoleStoreError> {
        RoleStore::set_role(self, device_id, role, set_by).await
    }

    async fn get_role(&self, device_id: &str) -> Result<Role, RoleStoreError> {
        RoleStore::get_role(self, device_id).await
    }

    async fn list_explicit_roles(
        &self,
    ) -> Result<Vec<(String, Role)>, RoleStoreError> {
        RoleStore::list_explicit_roles(self).await
    }

    async fn clear_role(&self, device_id: &str) -> Result<(), RoleStoreError> {
        RoleStore::clear_role(self, device_id).await
    }
}

fn validate_device_id(s: &str) -> Result<String, RoleStoreError> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(RoleStoreError::InvalidDeviceId);
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo::persistence::{MemoryPersistenceStore, PersistenceStore};

    fn store() -> RoleStore {
        let persistence: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::default());
        let happenings = Arc::new(HappeningBus::with_capacity(64));
        RoleStore::new(persistence, happenings)
    }

    #[test]
    fn role_round_trips_through_string() {
        for r in [Role::Source, Role::Receiver, Role::Auto] {
            assert_eq!(Role::from_str(r.as_str()).unwrap(), r);
        }
    }

    #[test]
    fn from_str_case_insensitive() {
        assert_eq!(Role::from_str("SOURCE").unwrap(), Role::Source);
        assert_eq!(Role::from_str("Receiver").unwrap(), Role::Receiver);
    }

    #[test]
    fn from_str_rejects_unknown() {
        assert!(matches!(
            Role::from_str("controller"),
            Err(RoleStoreError::InvalidRole(_))
        ));
    }

    #[tokio::test]
    async fn empty_store_returns_auto_default() {
        let s = store();
        let role = s.get_role("dev-a").await.unwrap();
        assert_eq!(role, Role::Auto);
    }

    #[tokio::test]
    async fn set_role_persists_and_reads_back() {
        let s = store();
        s.set_role("dev-a", Role::Source, Some("cli".into()))
            .await
            .unwrap();
        let read = s.get_role("dev-a").await.unwrap();
        assert_eq!(read, Role::Source);
    }

    #[tokio::test]
    async fn set_role_overwrites_existing() {
        let s = store();
        s.set_role("dev-a", Role::Source, None).await.unwrap();
        s.set_role("dev-a", Role::Receiver, None).await.unwrap();
        let read = s.get_role("dev-a").await.unwrap();
        assert_eq!(read, Role::Receiver);
    }

    #[tokio::test]
    async fn set_role_refuses_empty_device_id() {
        let s = store();
        let err = s.set_role("  ", Role::Source, None).await.unwrap_err();
        assert!(matches!(err, RoleStoreError::InvalidDeviceId));
    }

    #[tokio::test]
    async fn list_explicit_roles_returns_only_set_devices() {
        let s = store();
        s.set_role("dev-a", Role::Source, None).await.unwrap();
        s.set_role("dev-b", Role::Receiver, None).await.unwrap();
        // dev-c left at substrate-empty default — should not
        // appear in the list.
        let list = s.list_explicit_roles().await.unwrap();
        assert_eq!(list.len(), 2);
        let mut by_id: std::collections::BTreeMap<String, Role> =
            list.into_iter().collect();
        assert_eq!(by_id.remove("dev-a"), Some(Role::Source));
        assert_eq!(by_id.remove("dev-b"), Some(Role::Receiver));
    }

    #[tokio::test]
    async fn clear_role_returns_to_auto() {
        let s = store();
        s.set_role("dev-a", Role::Source, None).await.unwrap();
        s.clear_role("dev-a").await.unwrap();
        let read = s.get_role("dev-a").await.unwrap();
        assert_eq!(read, Role::Auto);
        // Cleared device drops out of the explicit list.
        let list = s.list_explicit_roles().await.unwrap();
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn clear_role_idempotent_on_unset_device() {
        let s = store();
        // No prior set — clearing is a no-op.
        s.clear_role("dev-a").await.unwrap();
    }

    #[tokio::test]
    async fn subscribe_emits_set_on_first_set() {
        let s = store();
        let mut rx = s.subscribe();
        s.set_role("dev-a", Role::Receiver, None).await.unwrap();
        let change = rx.try_recv().expect("emit on first set");
        match change {
            RoleChange::Set {
                device_id,
                prior_role,
                new_role,
            } => {
                assert_eq!(device_id, "dev-a");
                assert_eq!(prior_role, None);
                assert_eq!(new_role, Role::Receiver);
            }
            other => panic!("expected Set, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn subscribe_emits_set_with_prior_on_transition() {
        let s = store();
        s.set_role("dev-a", Role::Source, None).await.unwrap();
        let mut rx = s.subscribe();
        s.set_role("dev-a", Role::Receiver, None).await.unwrap();
        let change = rx.try_recv().expect("emit on transition");
        match change {
            RoleChange::Set {
                prior_role,
                new_role,
                ..
            } => {
                assert_eq!(prior_role, Some(Role::Source));
                assert_eq!(new_role, Role::Receiver);
            }
            other => panic!("expected Set, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn subscribe_emits_cleared_on_clear() {
        let s = store();
        s.set_role("dev-a", Role::Source, None).await.unwrap();
        let mut rx = s.subscribe();
        s.clear_role("dev-a").await.unwrap();
        let change = rx.try_recv().expect("emit on clear");
        match change {
            RoleChange::Cleared {
                device_id,
                prior_role,
            } => {
                assert_eq!(device_id, "dev-a");
                assert_eq!(prior_role, Role::Source);
            }
            other => panic!("expected Cleared, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn subscribe_silent_on_idempotent_set() {
        let s = store();
        s.set_role("dev-a", Role::Source, None).await.unwrap();
        let mut rx = s.subscribe();
        // Re-set with the same role — should be silent.
        s.set_role("dev-a", Role::Source, None).await.unwrap();
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn subscribe_silent_on_clear_of_unset_device() {
        let s = store();
        let mut rx = s.subscribe();
        s.clear_role("dev-a").await.unwrap();
        assert!(rx.try_recv().is_err());
    }
}
