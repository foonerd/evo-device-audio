// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Adapter implementing the SDK's `MultiroomSubstrateHandle`
//! trait over the framework's `GroupStore` + `RoleStore`.
//!
//! The adapter exists so plugins consume multi-room substrate
//! state via the SDK's stable trait without depending on the
//! framework's internal types. The adapter:
//!
//! 1. Translates read calls (`get_role` / `get_group` /
//!    `list_*`) to the internal store APIs and translates the
//!    returned types into SDK DTOs.
//! 2. Spawns two background translation tasks that drain the
//!    internal substrate broadcast channels and re-emit the
//!    events on the adapter's own SDK-typed broadcast
//!    channels. Plugins subscribe to the adapter's channels.
//!
//! ## Why translate events rather than expose the internal channel
//!
//! The internal channel carries `crate::role_store::RoleChange`
//! and `crate::groups::GroupChange` — framework-private types
//! the SDK cannot reference (the SDK is the API stable
//! surface; the framework is the implementation; the
//! dependency goes from framework → SDK, not the other way).
//! Translation gives plugins a stable DTO and isolates the
//! framework to refactor its internal types without breaking
//! the plugin contract.
//!
//! ## Resource posture
//!
//! - Two long-lived tokio tasks per adapter instance (one per
//!   substrate's translation pump). The framework boots one
//!   adapter for the process; tasks live for the process
//!   lifetime.
//! - Two broadcast channels (capacity 32 each); plugins'
//!   receivers clone from these. Drop-oldest on overflow per
//!   the broadcast contract.
//! - Per-translation work: one allocation per event (DTO
//!   construction). The mutation rate is operator-paced
//!   (rare); per-tick allocation churn is negligible.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::broadcast;

use evo_plugin_sdk::multiroom_substrate::{
    GroupChange as SdkGroupChange, GroupChangeReceiver,
    GroupRecord as SdkGroupRecord, MultiroomSubstrateError as SdkError,
    MultiroomSubstrateHandle, Role as SdkRole, RoleChange as SdkRoleChange,
    RoleChangeReceiver,
};

use evo::groups::{Group, GroupChange, GroupStore};

use crate::role_store::{Role, RoleChange, RoleStore};

/// Broadcast channel capacity for the adapter's translated
/// streams. 32 matches the underlying substrate channels'
/// capacity; pumps never block on a slow subscriber (drop-
/// oldest).
const TRANSLATED_CHANNEL_CAPACITY: usize = 32;

/// Adapter implementing [`MultiroomSubstrateHandle`] over the
/// framework's `GroupStore` + `RoleStore`. Construct once at
/// boot and clone the `Arc` into every plugin's `LoadContext`.
pub struct MultiroomSubstrateAdapter {
    group_store: Arc<GroupStore>,
    role_store: Arc<RoleStore>,
    sdk_role_tx: broadcast::Sender<SdkRoleChange>,
    sdk_group_tx: broadcast::Sender<SdkGroupChange>,
}

impl std::fmt::Debug for MultiroomSubstrateAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MultiroomSubstrateAdapter")
            .finish_non_exhaustive()
    }
}

impl MultiroomSubstrateAdapter {
    /// Construct the adapter. Spawns the two translation
    /// pumps that drain the underlying substrate broadcast
    /// channels and re-emit on the SDK-typed broadcast
    /// channels. The returned `Arc` is the handle plugins
    /// consume.
    pub fn new(
        group_store: Arc<GroupStore>,
        role_store: Arc<RoleStore>,
    ) -> Arc<Self> {
        let (sdk_role_tx, _) = broadcast::channel(TRANSLATED_CHANNEL_CAPACITY);
        let (sdk_group_tx, _) = broadcast::channel(TRANSLATED_CHANNEL_CAPACITY);
        let adapter = Arc::new(Self {
            group_store: Arc::clone(&group_store),
            role_store: Arc::clone(&role_store),
            sdk_role_tx: sdk_role_tx.clone(),
            sdk_group_tx: sdk_group_tx.clone(),
        });

        // Role-substrate translation pump. Drains the internal
        // RoleStore broadcast channel and re-emits SDK-typed
        // events on the adapter's channel.
        let mut role_rx = role_store.subscribe();
        let role_tx = sdk_role_tx;
        tokio::spawn(async move {
            loop {
                match role_rx.recv().await {
                    Ok(event) => {
                        let _ = role_tx.send(translate_role_change(event));
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Slow subscriber on the upstream
                        // channel — keep pumping; downstream
                        // subscribers re-read via list_* on a
                        // missed-event lag.
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::debug!(
                            "multiroom substrate adapter: role channel \
                             closed; pump shutting down"
                        );
                        return;
                    }
                }
            }
        });

        // Group-substrate translation pump.
        let mut group_rx = group_store.subscribe();
        let group_tx = sdk_group_tx;
        tokio::spawn(async move {
            loop {
                match group_rx.recv().await {
                    Ok(event) => {
                        let _ = group_tx.send(translate_group_change(event));
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::debug!(
                            "multiroom substrate adapter: group channel \
                             closed; pump shutting down"
                        );
                        return;
                    }
                }
            }
        });

        adapter
    }
}

fn translate_role(r: Role) -> SdkRole {
    match r {
        Role::Source => SdkRole::Source,
        Role::Receiver => SdkRole::Receiver,
        Role::Auto => SdkRole::Auto,
    }
}

fn translate_role_change(event: RoleChange) -> SdkRoleChange {
    match event {
        RoleChange::Set {
            device_id,
            prior_role,
            new_role,
        } => SdkRoleChange::Set {
            device_id,
            prior_role: prior_role.map(translate_role),
            new_role: translate_role(new_role),
        },
        RoleChange::Cleared {
            device_id,
            prior_role,
        } => SdkRoleChange::Cleared {
            device_id,
            prior_role: translate_role(prior_role),
        },
    }
}

fn translate_group(g: Group) -> SdkGroupRecord {
    SdkGroupRecord {
        group_id: g.group_id.0,
        display_name: g.display_name,
        members: g.members,
        pinned_source_host: g.pinned_source_host,
        leader_ms: g.leader_ms,
        modified_at_ms: g.modified_at_ms,
    }
}

fn translate_group_change(event: GroupChange) -> SdkGroupChange {
    match event {
        GroupChange::Created(g) => SdkGroupChange::Created(translate_group(g)),
        GroupChange::Updated(g) => SdkGroupChange::Updated(translate_group(g)),
        GroupChange::Deleted(id) => SdkGroupChange::Deleted(id.0),
    }
}

impl MultiroomSubstrateHandle for MultiroomSubstrateAdapter {
    fn get_role<'a>(
        &'a self,
        device_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<SdkRole, SdkError>> + Send + 'a>>
    {
        Box::pin(async move {
            match self.role_store.get_role(device_id).await {
                Ok(role) => Ok(translate_role(role)),
                Err(e) => Err(role_error_to_sdk(e)),
            }
        })
    }

    fn list_explicit_roles<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<(String, SdkRole)>, SdkError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            match self.role_store.list_explicit_roles().await {
                Ok(entries) => Ok(entries
                    .into_iter()
                    .map(|(id, r)| (id, translate_role(r)))
                    .collect()),
                Err(e) => Err(role_error_to_sdk(e)),
            }
        })
    }

    fn subscribe_role_changes(&self) -> RoleChangeReceiver {
        RoleChangeReceiver(self.sdk_role_tx.subscribe())
    }

    fn get_group<'a>(
        &'a self,
        group_id: &'a str,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<SdkGroupRecord>, SdkError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            match self.group_store.get(group_id).await {
                Ok(Some(g)) => Ok(Some(translate_group(g))),
                Ok(None) => Ok(None),
                Err(e) => Err(SdkError::Substrate(e.to_string())),
            }
        })
    }

    fn list_groups<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<SdkGroupRecord>, SdkError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            match self.group_store.list().await {
                Ok(groups) => {
                    Ok(groups.into_iter().map(translate_group).collect())
                }
                Err(e) => Err(SdkError::Substrate(e.to_string())),
            }
        })
    }

    fn list_groups_for_device<'a>(
        &'a self,
        device_id: &'a str,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<SdkGroupRecord>, SdkError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            match self.group_store.list_for_device(device_id).await {
                Ok(groups) => {
                    Ok(groups.into_iter().map(translate_group).collect())
                }
                Err(e) => Err(SdkError::Substrate(e.to_string())),
            }
        })
    }

    fn subscribe_group_changes(&self) -> GroupChangeReceiver {
        GroupChangeReceiver(self.sdk_group_tx.subscribe())
    }
}

fn role_error_to_sdk(e: crate::role_store::RoleStoreError) -> SdkError {
    use crate::role_store::RoleStoreError;
    match e {
        RoleStoreError::InvalidRole(s) => SdkError::InvalidRole(s),
        RoleStoreError::InvalidDeviceId => SdkError::InvalidDeviceId,
        RoleStoreError::Persistence(p) => SdkError::Substrate(p.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo::happenings::HappeningBus;
    use evo::persistence::{MemoryPersistenceStore, PersistenceStore};
    use std::str::FromStr;

    fn fixture() -> (Arc<GroupStore>, Arc<RoleStore>) {
        let persistence: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::default());
        let happenings = Arc::new(HappeningBus::with_capacity(64));
        let group_store = Arc::new(GroupStore::new(
            Arc::clone(&persistence),
            Arc::clone(&happenings),
        ));
        let role_store = Arc::new(RoleStore::new(persistence, happenings));
        (group_store, role_store)
    }

    #[test]
    fn role_translation_round_trips() {
        // Internal Role ↔ SDK Role identifiers must agree.
        for (internal, sdk_expected) in [
            (Role::Source, SdkRole::Source),
            (Role::Receiver, SdkRole::Receiver),
            (Role::Auto, SdkRole::Auto),
        ] {
            assert_eq!(translate_role(internal), sdk_expected);
            assert_eq!(internal.as_str(), sdk_expected.as_str());
        }
    }

    #[test]
    fn role_identifiers_match_framework_and_sdk_at_string_level() {
        // The framework's substrate uses these strings in SQL
        // CHECK constraints; the SDK uses them in serde
        // serialisation. Divergence between them would break
        // wire/store interop silently. Test refuses the
        // divergence loudly.
        assert_eq!(Role::Source.as_str(), SdkRole::Source.as_str());
        assert_eq!(Role::Receiver.as_str(), SdkRole::Receiver.as_str());
        assert_eq!(Role::Auto.as_str(), SdkRole::Auto.as_str());
    }

    #[test]
    fn role_from_str_via_sdk_matches_internal_parsing() {
        // The SDK's FromStr should accept the same strings the
        // internal Role accepts (the operator-facing surface
        // and the substrate-internal surface must agree).
        for s in ["source", "receiver", "auto", "SOURCE", "  source  "] {
            let internal = Role::from_str(s).expect("internal accepts");
            let sdk = SdkRole::from_str(s).expect("sdk accepts");
            assert_eq!(translate_role(internal), sdk);
        }
    }

    #[tokio::test]
    async fn adapter_get_role_returns_auto_for_empty_substrate() {
        let (gs, rs) = fixture();
        let adapter = MultiroomSubstrateAdapter::new(gs, rs);
        let role = adapter.get_role("dev-unknown").await.unwrap();
        assert_eq!(role, SdkRole::Auto);
    }

    #[tokio::test]
    async fn adapter_get_role_returns_set_value() {
        let (gs, rs) = fixture();
        rs.set_role("dev-a", Role::Source, Some("test".into()))
            .await
            .unwrap();
        let adapter = MultiroomSubstrateAdapter::new(gs, rs);
        let role = adapter.get_role("dev-a").await.unwrap();
        assert_eq!(role, SdkRole::Source);
    }

    #[tokio::test]
    async fn adapter_list_explicit_roles_skips_default() {
        let (gs, rs) = fixture();
        rs.set_role("dev-a", Role::Source, None).await.unwrap();
        rs.set_role("dev-b", Role::Receiver, None).await.unwrap();
        // dev-c remains at default (auto) — should NOT be in
        // the list.
        let adapter = MultiroomSubstrateAdapter::new(gs, rs);
        let list = adapter.list_explicit_roles().await.unwrap();
        assert_eq!(list.len(), 2);
        let by_id: std::collections::BTreeMap<String, SdkRole> =
            list.into_iter().collect();
        assert_eq!(by_id.get("dev-a"), Some(&SdkRole::Source));
        assert_eq!(by_id.get("dev-b"), Some(&SdkRole::Receiver));
    }

    #[tokio::test]
    async fn adapter_get_group_returns_translated_record() {
        let (gs, rs) = fixture();
        let g = gs.create("Living", &["dev-a".into()]).await.unwrap();
        let adapter = MultiroomSubstrateAdapter::new(gs, rs);
        let read = adapter
            .get_group(g.group_id.as_str())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read.group_id, g.group_id.0);
        assert_eq!(read.display_name, "Living");
        assert_eq!(read.members, vec!["dev-a".to_string()]);
        assert_eq!(read.leader_ms, 200);
    }

    #[tokio::test]
    async fn adapter_get_group_returns_none_for_unknown() {
        let (gs, rs) = fixture();
        let adapter = MultiroomSubstrateAdapter::new(gs, rs);
        let read = adapter.get_group("missing").await.unwrap();
        assert!(read.is_none());
    }

    #[tokio::test]
    async fn adapter_list_groups_returns_translated_records() {
        let (gs, rs) = fixture();
        gs.create("Living", &["dev-a".into()]).await.unwrap();
        gs.create("Den", &["dev-b".into()]).await.unwrap();
        let adapter = MultiroomSubstrateAdapter::new(gs, rs);
        let list = adapter.list_groups().await.unwrap();
        assert_eq!(list.len(), 2);
    }

    #[tokio::test]
    async fn adapter_propagates_role_set_via_subscribe_channel() {
        let (gs, rs) = fixture();
        let adapter = MultiroomSubstrateAdapter::new(gs, Arc::clone(&rs));
        let mut rcv = adapter.subscribe_role_changes();
        // Allow the translation pump to subscribe to the
        // upstream channel before we publish.
        tokio::task::yield_now().await;
        rs.set_role("dev-a", Role::Source, None).await.unwrap();
        // Pump has to wake + translate + republish.
        let event = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            rcv.recv(),
        )
        .await
        .expect("receiver got the event in time")
        .expect("channel did not close");
        match event {
            SdkRoleChange::Set {
                device_id,
                prior_role,
                new_role,
            } => {
                assert_eq!(device_id, "dev-a");
                assert_eq!(prior_role, None);
                assert_eq!(new_role, SdkRole::Source);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[tokio::test]
    async fn adapter_propagates_group_create_via_subscribe_channel() {
        let (gs, rs) = fixture();
        let adapter = MultiroomSubstrateAdapter::new(Arc::clone(&gs), rs);
        let mut rcv = adapter.subscribe_group_changes();
        tokio::task::yield_now().await;
        gs.create("Living", &["dev-a".into()]).await.unwrap();
        let event = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            rcv.recv(),
        )
        .await
        .expect("receiver got the event in time")
        .expect("channel did not close");
        match event {
            SdkGroupChange::Created(g) => {
                assert_eq!(g.display_name, "Living");
                assert_eq!(g.members, vec!["dev-a".to_string()]);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }
}
