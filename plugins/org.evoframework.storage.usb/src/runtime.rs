// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! `storage.usb` runtime — dispatch surface + mount lifecycle.
//!
//! # Responsibilities
//!
//! - Verb dispatch: `storage.usb.list_drives` (read) +
//!   `storage.usb.mount` (mutating). The remaining three
//!   mutating verbs (`safe_remove` / `repair_filesystem` /
//!   `rename`) return a stable `NotImplemented` response until
//!   the corresponding step lands.
//! - Coldplug at plugin load: enumerate every USB-transport
//!   block partition, classify (six-way role), derive stable-ids,
//!   auto-mount removable drives, dispatch `library.add_source`
//!   for each successful mount.
//! - Periodic reconcile: 5-second ticker re-runs the coldplug
//!   pipeline so plug/unplug transitions are absorbed within
//!   one tick (userspace udev/netlink lands in a follow-on).
//! - Subject `storage_usb_drives` announce + republish on every
//!   transition (mount / unmount / class change).
//!
//! # Trust boundary
//!
//! Every mount / umount / fsck / eject invocation dispatches
//! through the narrow root-only wrapper at
//! `/usr/local/bin/evo-usb-mount`. The plugin does NOT hold raw
//! sudo grants on the underlying tools; the wrapper's argv
//! allowlist is the last-mile runtime enforcement.

use crate::aliases::{AliasLookup, AliasStore};
use crate::classifier::{
    classify, ClassifiedPartition, ClassifierError, MountPolicy, PartitionRole,
};
use crate::fs_matrix::FsFamily;
use crate::stable_id::{derive, DerivationContext, DerivationInput};

use evo_plugin_sdk::contract::{
    ExternalAddressing, ShelfRequestDispatcher, SubjectAnnouncement,
    SubjectAnnouncer,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tokio::sync::Mutex;

/// Path to the narrow root-only wrapper installed by
/// `bootstrap.sh` Step 1g.
pub const USB_WRAPPER_PATH: &str = "/usr/local/bin/evo-usb-mount";

/// Mount root under which every media USB volume mounts.
pub const USB_MOUNT_ROOT: &str = "/var/lib/evo/music/USB";

/// Reactive subject type published by this plugin.
pub const STORAGE_USB_DRIVES_SUBJECT_TYPE: &str = "storage_usb_drives";

/// Canonical addressing for the singleton `storage_usb_drives`
/// subject. Matches the schema declaration
/// `evo.storage.usb.drives:local`.
pub fn storage_usb_drives_addressing() -> ExternalAddressing {
    ExternalAddressing::new("evo.storage.usb.drives", "local")
}

/// Default reconcile cadence (ms) — a 5-second poll approximates
/// hotplug without a udev netlink subscriber.
pub const DEFAULT_RECONCILE_CADENCE_MS: u64 = 5_000;

/// Verb list for this shelf. Kept in this crate so
/// [`crate::StorageUsbPlugin::describe`] + the manifest stay
/// aligned by construction (asserted by test).
pub const STORAGE_USB_VERBS: &[&str] = &[
    "storage.usb.list_drives",
    "storage.usb.mount",
    "storage.usb.safe_remove",
    "storage.usb.repair_filesystem",
    "storage.usb.rename",
];

/// True if the argument matches one of the [`STORAGE_USB_VERBS`].
pub fn is_storage_usb_verb(v: &str) -> bool {
    STORAGE_USB_VERBS.contains(&v)
}

// --------------------------------------------------------------
// Runtime singleton
// --------------------------------------------------------------

/// The runtime singleton.
pub struct StorageUsbRuntime {
    service_uid: u32,
    service_gid: u32,
    needs_sudo: bool,
    command_runner: Arc<dyn CommandRunner>,
    input_source: Arc<dyn ClassifierInputSource>,
    inner: Mutex<RuntimeInner>,
    publisher: StdMutex<Option<StoragePublisher>>,
    shelf_dispatcher: StdMutex<Option<Arc<dyn ShelfRequestDispatcher>>>,
    aliases: StdMutex<Arc<AliasStore>>,
}

struct RuntimeInner {
    drives: BTreeMap<String, DriveRecord>,
    last_update_at_ms: i64,
}

struct StoragePublisher {
    announcer: Arc<dyn SubjectAnnouncer>,
}

impl StorageUsbRuntime {
    /// New runtime with production input/command sources.
    pub fn new(service_uid: u32, service_gid: u32, needs_sudo: bool) -> Self {
        Self::with_sources(
            service_uid,
            service_gid,
            needs_sudo,
            Arc::new(ProcfsAndLsblkSource),
            Arc::new(RealCommandRunner),
        )
    }

    /// New runtime with caller-supplied sources (test path).
    pub fn with_sources(
        service_uid: u32,
        service_gid: u32,
        needs_sudo: bool,
        input_source: Arc<dyn ClassifierInputSource>,
        command_runner: Arc<dyn CommandRunner>,
    ) -> Self {
        Self {
            service_uid,
            service_gid,
            needs_sudo,
            command_runner,
            input_source,
            inner: Mutex::new(RuntimeInner {
                drives: BTreeMap::new(),
                last_update_at_ms: 0,
            }),
            publisher: StdMutex::new(None),
            shelf_dispatcher: StdMutex::new(None),
            aliases: StdMutex::new(Arc::new(AliasStore::empty(
                std::path::Path::new("/tmp"),
            ))),
        }
    }

    /// Bind the alias store loaded from
    /// `<state_dir>/state/aliases.toml`.
    pub fn attach_alias_store(&self, store: Arc<AliasStore>) {
        let mut slot = self
            .aliases
            .lock()
            .expect("storage.usb: aliases lock poisoned on attach");
        *slot = store;
    }

    /// Bind the framework's cross-plugin dispatcher.
    pub fn attach_shelf_dispatcher(
        &self,
        dispatcher: Arc<dyn ShelfRequestDispatcher>,
    ) {
        let mut slot = self
            .shelf_dispatcher
            .lock()
            .expect("storage.usb: dispatcher lock poisoned on attach");
        *slot = Some(dispatcher);
    }

    /// Attach a [`SubjectAnnouncer`] and announce the initial
    /// (usually empty) envelope so subscribers connecting before
    /// the first reconcile see a well-formed seed.
    pub async fn attach_subject_publisher(
        &self,
        announcer: Arc<dyn SubjectAnnouncer>,
    ) -> Result<(), evo_plugin_sdk::contract::ReportError> {
        let envelope = self.compose_envelope().await;
        announcer
            .announce(SubjectAnnouncement {
                subject_type: STORAGE_USB_DRIVES_SUBJECT_TYPE.to_string(),
                addressings: vec![storage_usb_drives_addressing()],
                claims: Vec::new(),
                state: serde_json::to_value(&envelope)
                    .unwrap_or(serde_json::Value::Null),
                announced_at: SystemTime::now(),
            })
            .await?;
        let mut slot = self
            .publisher
            .lock()
            .expect("storage.usb: publisher lock poisoned on attach");
        *slot = Some(StoragePublisher { announcer });
        Ok(())
    }

    /// Getter for tests + evidence.
    pub fn service_uid(&self) -> u32 {
        self.service_uid
    }
    /// Getter for tests + evidence.
    pub fn service_gid(&self) -> u32 {
        self.service_gid
    }
    /// Getter for tests + evidence.
    pub fn needs_sudo(&self) -> bool {
        self.needs_sudo
    }

    // ----------------------------------------------------------
    // Verb dispatch
    // ----------------------------------------------------------

    /// Dispatch a decoded request.
    pub async fn dispatch_verb(
        &self,
        verb: &str,
        payload: &[u8],
    ) -> Result<Vec<u8>, VerbDispatchError> {
        match verb {
            "storage.usb.list_drives" => self.handle_list_drives().await,
            "storage.usb.mount" => self.handle_mount(payload).await,
            "storage.usb.safe_remove" => self.handle_safe_remove(payload).await,
            "storage.usb.repair_filesystem" => {
                self.handle_repair_filesystem(payload).await
            }
            "storage.usb.rename" => self.handle_rename(payload).await,
            other => Err(VerbDispatchError::UnknownRequestType {
                verb: other.to_string(),
            }),
        }
    }

    async fn handle_list_drives(&self) -> Result<Vec<u8>, VerbDispatchError> {
        if let Err(e) = self.reconcile_once().await {
            tracing::warn!(
                plugin = "storage.usb",
                error = %e,
                "reconcile during list_drives failed; returning last known state"
            );
        }
        let envelope = self.compose_envelope().await;
        serde_json::to_vec(&envelope)
            .map_err(|e| VerbDispatchError::ResponseSerialise(e.to_string()))
    }

    async fn handle_mount(
        &self,
        payload: &[u8],
    ) -> Result<Vec<u8>, VerbDispatchError> {
        let req: MountRequest = serde_json::from_slice(payload)
            .map_err(|e| VerbDispatchError::PayloadDecode(e.to_string()))?;

        self.reconcile_once().await?;

        let record = {
            let inner = self.inner.lock().await;
            inner.drives.get(&req.stable_id).cloned()
        }
        .ok_or_else(|| {
            VerbDispatchError::MountRefused(MountRefuseClass::UnknownStableId {
                stable_id: req.stable_id.clone(),
            })
        })?;

        // Idempotent — already mounted-*.
        if record.class == DriveClass::MountedClean
            || record.class == DriveClass::MountedDirty
        {
            let resp = MountResponse {
                v: 1,
                mounted_at: record.mount_root.clone().unwrap_or_default(),
                class: record.class.wire_str().to_string(),
                library_source_id: record.library_source_id.clone(),
            };
            return serde_json::to_vec(&resp).map_err(|e| {
                VerbDispatchError::ResponseSerialise(e.to_string())
            });
        }

        // Role refuse.
        if record.role.is_system_live() {
            return Err(VerbDispatchError::MountRefused(
                MountRefuseClass::SystemLivePartition {
                    stable_id: req.stable_id,
                    role: role_wire_string(record.role),
                },
            ));
        }
        if record.role == PartitionRole::SystemAdjacent
            && record.mount_policy != MountPolicy::Auto
        {
            return Err(VerbDispatchError::MountRefused(
                MountRefuseClass::SystemAdjacentNotOptedIn {
                    stable_id: req.stable_id,
                },
            ));
        }

        // FS + size checks.
        let family = FsFamily::from_lsblk(&record.fs_type);
        if family == FsFamily::Unsupported {
            return Err(VerbDispatchError::MountRefused(
                MountRefuseClass::UnsupportedFs {
                    stable_id: req.stable_id,
                    fs_type: record.fs_type.clone(),
                },
            ));
        }
        if let Some(cap) = family.size_cap_bytes() {
            if record.size_bytes > cap {
                self.mark_drive_class(
                    &req.stable_id,
                    DriveClass::MountFailedOversizedVfat,
                )
                .await;
                return Err(VerbDispatchError::MountRefused(
                    MountRefuseClass::MountFailedOversizedVfat {
                        stable_id: req.stable_id,
                        size_bytes: record.size_bytes,
                        cap_bytes: cap,
                    },
                ));
            }
        }

        // Dispatch wrapper.
        let opts = family.mount_options(self.service_uid, self.service_gid);
        let fs_arg = family.wrapper_fs_arg().unwrap_or(&record.fs_type);
        let argv = vec![
            "mount".to_string(),
            req.stable_id.clone(),
            fs_arg.to_string(),
            record.device_node.clone(),
            opts.clone(),
        ];
        let outcome = self
            .command_runner
            .run_wrapper(self.needs_sudo, &argv)
            .await
            .map_err(|e| VerbDispatchError::SubprocessIo(e.to_string()))?;
        if outcome.status != 0 {
            self.mark_drive_class(&req.stable_id, DriveClass::MountFailedOther)
                .await;
            return Err(VerbDispatchError::MountRefused(
                MountRefuseClass::MountSubprocessFailed {
                    stable_id: req.stable_id,
                    exit_code: outcome.status,
                    stderr: outcome.stderr,
                },
            ));
        }

        let mount_root = format!("{USB_MOUNT_ROOT}/{}", req.stable_id);
        let mut library_source_id: Option<String> = None;

        if let Some(dispatcher) = self.shelf_dispatcher_clone() {
            match self
                .dispatch_library_add_source(
                    dispatcher,
                    &req.stable_id,
                    &record.device_node,
                    record.display_name.as_deref().unwrap_or(&req.stable_id),
                    &mount_root,
                )
                .await
            {
                Ok(id) => library_source_id = id,
                Err(e) => tracing::warn!(
                    plugin = "storage.usb",
                    stable_id = %req.stable_id,
                    error = %e,
                    "library.add_source local_usb dispatch failed; mount kept"
                ),
            }
        }

        {
            let mut inner = self.inner.lock().await;
            if let Some(rec) = inner.drives.get_mut(&req.stable_id) {
                rec.class = DriveClass::MountedClean;
                rec.mount_root = Some(mount_root.clone());
                rec.library_source_id = library_source_id.clone();
                rec.last_transition_at_ms = Some(now_ms());
            }
        }
        self.republish_envelope().await;

        let resp = MountResponse {
            v: 1,
            mounted_at: mount_root,
            class: DriveClass::MountedClean.wire_str().to_string(),
            library_source_id,
        };
        serde_json::to_vec(&resp)
            .map_err(|e| VerbDispatchError::ResponseSerialise(e.to_string()))
    }

    // ----------------------------------------------------------
    // Reconcile
    // ----------------------------------------------------------

    /// Enumerate every USB-transport partition, classify, derive
    /// stable-ids, and auto-mount removable drives. Called on
    /// plugin load, on every reconcile tick, and at the start of
    /// every verb handler (cheap in-memory op + lsblk poll).
    pub async fn reconcile_once(&self) -> Result<(), VerbDispatchError> {
        let inputs = self
            .input_source
            .read_inputs()
            .await
            .map_err(|e| VerbDispatchError::InputSource(e.to_string()))?;
        let classified =
            classify(&inputs.mountinfo, &inputs.swaps, &inputs.lsblk_json)
                .map_err(VerbDispatchError::Classify)?;

        let alias_store = self.aliases_clone();

        // Sort deterministically before deriving so enumeration
        // order is stable across boots.
        let mut sorted = classified;
        sorted.sort_by(|a, b| a.device_node.cmp(&b.device_node));

        // Rebuild the registry from fresh classifier output;
        // preserve mount_root + library_source_id + class for
        // surviving stable_ids.
        let mut inner = self.inner.lock().await;
        let mut fresh: BTreeMap<String, DriveRecord> = BTreeMap::new();
        let mut in_use_ids: Vec<String> = Vec::new();

        for part in &sorted {
            let alias = alias_store.lookup(&AliasLookup {
                vendor: part.vendor.as_deref(),
                model: part.model.as_deref(),
                serial_short: part.serial_short.as_deref().unwrap_or_default(),
                partuuid: part.partuuid.as_deref(),
                partition_index: part.partition_index,
            });
            let alias_set = alias.is_some();
            let derived = derive(
                &DerivationInput {
                    label: part.label.as_deref(),
                    vendor: part.vendor.as_deref(),
                    model: part.model.as_deref(),
                    serial_short: part.serial_short.as_deref(),
                    partition_index: part.partition_index,
                    partition_count: part.partition_count,
                    partuuid: part.partuuid.as_deref(),
                },
                &DerivationContext {
                    operator_alias: alias,
                    in_use_stable_ids: &in_use_ids,
                },
            );
            in_use_ids.push(derived.stable_id.clone());
            let mut rec =
                DriveRecord::from_partition(part, &derived, alias_set);
            if let Some(prior) = inner.drives.get(&derived.stable_id) {
                if prior.class == DriveClass::MountedClean
                    || prior.class == DriveClass::MountedDirty
                {
                    rec.class = prior.class;
                    rec.mount_root = prior.mount_root.clone();
                    rec.library_source_id = prior.library_source_id.clone();
                }
            }
            fresh.insert(derived.stable_id.clone(), rec);
        }

        inner.drives = fresh;
        inner.last_update_at_ms = now_ms();
        drop(inner);

        // Auto-mount removable drives that are not yet mounted.
        let candidates: Vec<String> = {
            let inner = self.inner.lock().await;
            inner
                .drives
                .iter()
                .filter(|(_, r)| {
                    r.role == PartitionRole::Removable
                        && r.mount_policy == MountPolicy::Auto
                        && (r.class == DriveClass::Unmounted
                            || r.class == DriveClass::MountFailedOther)
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in &candidates {
            let payload = serde_json::to_vec(&MountRequest {
                stable_id: id.clone(),
            })
            .unwrap_or_default();
            // Bypass the recursive reconcile_once call —
            // handle_mount would loop otherwise.
            if let Err(e) = self.mount_attempt_no_reconcile(&payload).await {
                tracing::debug!(
                    plugin = "storage.usb",
                    stable_id = %id,
                    error = %e,
                    "auto-mount attempt failed"
                );
            }
        }

        self.republish_envelope().await;
        Ok(())
    }

    /// Internal mount attempt without the pre-reconcile pass —
    /// used by [`Self::reconcile_once`] to avoid infinite
    /// recursion on the auto-mount loop.
    async fn mount_attempt_no_reconcile(
        &self,
        payload: &[u8],
    ) -> Result<Vec<u8>, VerbDispatchError> {
        let req: MountRequest = serde_json::from_slice(payload)
            .map_err(|e| VerbDispatchError::PayloadDecode(e.to_string()))?;
        let record = {
            let inner = self.inner.lock().await;
            inner.drives.get(&req.stable_id).cloned()
        }
        .ok_or_else(|| {
            VerbDispatchError::MountRefused(MountRefuseClass::UnknownStableId {
                stable_id: req.stable_id.clone(),
            })
        })?;
        // Same body as handle_mount from the state check onward.
        // Extract to a helper to keep the diff obvious.
        self.mount_dispatch(req, record).await
    }

    async fn mount_dispatch(
        &self,
        req: MountRequest,
        record: DriveRecord,
    ) -> Result<Vec<u8>, VerbDispatchError> {
        if record.class == DriveClass::MountedClean
            || record.class == DriveClass::MountedDirty
        {
            let resp = MountResponse {
                v: 1,
                mounted_at: record.mount_root.clone().unwrap_or_default(),
                class: record.class.wire_str().to_string(),
                library_source_id: record.library_source_id.clone(),
            };
            return serde_json::to_vec(&resp).map_err(|e| {
                VerbDispatchError::ResponseSerialise(e.to_string())
            });
        }
        if record.role.is_system_live() {
            return Err(VerbDispatchError::MountRefused(
                MountRefuseClass::SystemLivePartition {
                    stable_id: req.stable_id,
                    role: role_wire_string(record.role),
                },
            ));
        }
        if record.role == PartitionRole::SystemAdjacent
            && record.mount_policy != MountPolicy::Auto
        {
            return Err(VerbDispatchError::MountRefused(
                MountRefuseClass::SystemAdjacentNotOptedIn {
                    stable_id: req.stable_id,
                },
            ));
        }
        let family = FsFamily::from_lsblk(&record.fs_type);
        if family == FsFamily::Unsupported {
            return Err(VerbDispatchError::MountRefused(
                MountRefuseClass::UnsupportedFs {
                    stable_id: req.stable_id,
                    fs_type: record.fs_type.clone(),
                },
            ));
        }
        if let Some(cap) = family.size_cap_bytes() {
            if record.size_bytes > cap {
                self.mark_drive_class(
                    &req.stable_id,
                    DriveClass::MountFailedOversizedVfat,
                )
                .await;
                return Err(VerbDispatchError::MountRefused(
                    MountRefuseClass::MountFailedOversizedVfat {
                        stable_id: req.stable_id,
                        size_bytes: record.size_bytes,
                        cap_bytes: cap,
                    },
                ));
            }
        }
        let opts = family.mount_options(self.service_uid, self.service_gid);
        let fs_arg = family.wrapper_fs_arg().unwrap_or(&record.fs_type);
        let argv = vec![
            "mount".to_string(),
            req.stable_id.clone(),
            fs_arg.to_string(),
            record.device_node.clone(),
            opts,
        ];
        let outcome = self
            .command_runner
            .run_wrapper(self.needs_sudo, &argv)
            .await
            .map_err(|e| VerbDispatchError::SubprocessIo(e.to_string()))?;
        if outcome.status != 0 {
            self.mark_drive_class(&req.stable_id, DriveClass::MountFailedOther)
                .await;
            return Err(VerbDispatchError::MountRefused(
                MountRefuseClass::MountSubprocessFailed {
                    stable_id: req.stable_id,
                    exit_code: outcome.status,
                    stderr: outcome.stderr,
                },
            ));
        }
        let mount_root = format!("{USB_MOUNT_ROOT}/{}", req.stable_id);
        let mut library_source_id: Option<String> = None;
        if let Some(dispatcher) = self.shelf_dispatcher_clone() {
            match self
                .dispatch_library_add_source(
                    dispatcher,
                    &req.stable_id,
                    &record.device_node,
                    record.display_name.as_deref().unwrap_or(&req.stable_id),
                    &mount_root,
                )
                .await
            {
                Ok(id) => library_source_id = id,
                Err(e) => tracing::warn!(
                    plugin = "storage.usb",
                    stable_id = %req.stable_id,
                    error = %e,
                    "library.add_source local_usb dispatch failed; mount kept"
                ),
            }
        }
        {
            let mut inner = self.inner.lock().await;
            if let Some(rec) = inner.drives.get_mut(&req.stable_id) {
                rec.class = DriveClass::MountedClean;
                rec.mount_root = Some(mount_root.clone());
                rec.library_source_id = library_source_id.clone();
                rec.last_transition_at_ms = Some(now_ms());
            }
        }
        let resp = MountResponse {
            v: 1,
            mounted_at: mount_root,
            class: DriveClass::MountedClean.wire_str().to_string(),
            library_source_id,
        };
        serde_json::to_vec(&resp)
            .map_err(|e| VerbDispatchError::ResponseSerialise(e.to_string()))
    }

    async fn mark_drive_class(&self, stable_id: &str, class: DriveClass) {
        let mut inner = self.inner.lock().await;
        if let Some(rec) = inner.drives.get_mut(stable_id) {
            rec.class = class;
            rec.last_transition_at_ms = Some(now_ms());
        }
    }

    // ----------------------------------------------------------
    // safe_remove verb
    // ----------------------------------------------------------

    /// `storage.usb.safe_remove` handler. Consumer-stop-first
    /// discipline: dispatch `library.remove_source` before
    /// touching the mount to give MPD time to release its
    /// file handles.
    ///
    /// Sequence (per USB-STORAGE.md §9):
    ///
    /// 1. Refuse if role is `system-*` live.
    /// 2. If mounted: dispatch `library.remove_source` for the
    ///    drive's `library_source_id` (best-effort — MPD not
    ///    reachable is logged, not fatal).
    /// 3. `sync` on the parent disk (flush kernel dirty pages).
    /// 4. Wrapper `umount <stable-id>`. On EBUSY (wrapper exit 4):
    ///      - `force: false` (default) → return `Busy { holders }`
    ///        with a fuser-derived holder list.
    ///      - `force: true` → wrapper `umount-force <stable-id>`
    ///        (lazy detach `-l`).
    /// 5. Wrapper `eject <parent-disk>` (best-effort — some
    ///    drives ignore the ioctl; failure logged, not fatal).
    /// 6. Retract from the in-memory registry + republish subject.
    ///
    /// Payload: `{ v: 1, stable_id, force?: bool }`
    /// Response: `{ v: 1, removed: true, forced?: bool, holders?: [...] }`
    async fn handle_safe_remove(
        &self,
        payload: &[u8],
    ) -> Result<Vec<u8>, VerbDispatchError> {
        let req: SafeRemoveRequest = serde_json::from_slice(payload)
            .map_err(|e| VerbDispatchError::PayloadDecode(e.to_string()))?;

        // Refresh before decision.
        self.reconcile_once().await?;

        let record = {
            let inner = self.inner.lock().await;
            inner.drives.get(&req.stable_id).cloned()
        }
        .ok_or_else(|| {
            VerbDispatchError::SafeRemoveRefused(
                SafeRemoveRefuseClass::UnknownStableId {
                    stable_id: req.stable_id.clone(),
                },
            )
        })?;

        if record.role.is_system_live() {
            return Err(VerbDispatchError::SafeRemoveRefused(
                SafeRemoveRefuseClass::SystemLivePartition {
                    stable_id: req.stable_id,
                    role: role_wire_string(record.role),
                },
            ));
        }

        // Idempotent: already unmounted → return success without
        // touching the wrapper.
        if record.class == DriveClass::Unmounted
            || record.class == DriveClass::Unsupported
            || record.class == DriveClass::MountFailedOversizedVfat
            || record.class == DriveClass::MountFailedDirty
            || record.class == DriveClass::MountFailedOther
        {
            // Drop from registry so subject republish reflects
            // removal; the periodic reconciler would do this
            // anyway on next detach event but explicit is safer.
            {
                let mut inner = self.inner.lock().await;
                inner.drives.remove(&req.stable_id);
            }
            self.republish_envelope().await;
            let resp = SafeRemoveResponse {
                v: 1,
                removed: true,
                forced: Some(false),
                holders: None,
            };
            return serde_json::to_vec(&resp).map_err(|e| {
                VerbDispatchError::ResponseSerialise(e.to_string())
            });
        }

        // 1. Consumer-stop — library.remove_source. Best-effort;
        //    MPD-unreachable is logged, not fatal.
        if let Some(source_id) = record.library_source_id.as_ref() {
            if let Some(dispatcher) = self.shelf_dispatcher_clone() {
                let payload = serde_json::json!({
                    "v": 1,
                    "source_id": source_id,
                });
                if let Ok(bytes) = serde_json::to_vec(&payload) {
                    if let Err(e) = dispatcher
                        .dispatch(
                            "audio.library",
                            "library.remove_source",
                            bytes,
                            None,
                        )
                        .await
                    {
                        tracing::warn!(
                            plugin = "storage.usb",
                            stable_id = %req.stable_id,
                            source_id = %source_id,
                            error = %e,
                            "library.remove_source dispatch failed; \
                             proceeding with umount (best-effort)"
                        );
                    }
                }
            }
        }

        // 2. sync — flush kernel dirty pages on the parent disk.
        //    Best-effort — we shell out to `sync <parent-disk>`
        //    directly since sync doesn't need the wrapper's
        //    privilege grant. Ignore errors; umount reveals any
        //    inconsistency.
        let _ = tokio::process::Command::new("sync")
            .arg(&record.parent_disk)
            .output()
            .await;

        // 3. Try a clean umount first. The wrapper distinguishes
        //    EBUSY (exit 4) from other subprocess failures
        //    (exit 3) so we know when to escalate.
        let mut forced = false;
        let mut holders: Option<Vec<String>> = None;
        let umount_argv = vec!["umount".to_string(), req.stable_id.clone()];
        let umount_outcome = self
            .command_runner
            .run_wrapper(self.needs_sudo, &umount_argv)
            .await
            .map_err(|e| VerbDispatchError::SubprocessIo(e.to_string()))?;

        match umount_outcome.status {
            0 => {
                // Clean umount succeeded.
            }
            4 => {
                // EBUSY. Populate holders (fuser -m best-effort)
                // for operator diagnostics.
                let derived = self.fuser_holders(&record.mount_root).await;
                holders = Some(derived.clone());
                if !req.force.unwrap_or(false) {
                    return Err(VerbDispatchError::SafeRemoveRefused(
                        SafeRemoveRefuseClass::Busy {
                            stable_id: req.stable_id,
                            holders: derived,
                        },
                    ));
                }
                // Force: escalate to lazy detach.
                let force_argv =
                    vec!["umount-force".to_string(), req.stable_id.clone()];
                let force_outcome = self
                    .command_runner
                    .run_wrapper(self.needs_sudo, &force_argv)
                    .await
                    .map_err(|e| {
                        VerbDispatchError::SubprocessIo(e.to_string())
                    })?;
                if force_outcome.status != 0 {
                    return Err(VerbDispatchError::SafeRemoveRefused(
                        SafeRemoveRefuseClass::UmountSubprocessFailed {
                            stable_id: req.stable_id,
                            exit_code: force_outcome.status,
                            stderr: force_outcome.stderr,
                        },
                    ));
                }
                forced = true;
                tracing::warn!(
                    plugin = "storage.usb",
                    stable_id = %req.stable_id,
                    holders = ?holders,
                    "safe-remove forced with lazy detach; \
                     any open file handles will lose their \
                     backing on the last close"
                );
            }
            other => {
                return Err(VerbDispatchError::SafeRemoveRefused(
                    SafeRemoveRefuseClass::UmountSubprocessFailed {
                        stable_id: req.stable_id,
                        exit_code: other,
                        stderr: umount_outcome.stderr,
                    },
                ));
            }
        }

        // 4. Best-effort SCSI eject via wrapper. Some drives
        //    (Samsung T7, many SSD enclosures) simply don't
        //    respond to the ioctl. Failure logged, not fatal.
        let eject_argv = vec!["eject".to_string(), record.parent_disk.clone()];
        match self
            .command_runner
            .run_wrapper(self.needs_sudo, &eject_argv)
            .await
        {
            Ok(o) if o.status == 0 => {}
            Ok(o) => tracing::info!(
                plugin = "storage.usb",
                stable_id = %req.stable_id,
                parent_disk = %record.parent_disk,
                exit_code = o.status,
                stderr = %o.stderr,
                "eject failed (best-effort; safe-remove still succeeds)"
            ),
            Err(e) => tracing::info!(
                plugin = "storage.usb",
                stable_id = %req.stable_id,
                parent_disk = %record.parent_disk,
                error = %e,
                "eject subprocess I/O failed (best-effort)"
            ),
        }

        // 5. Retract from the in-memory registry + republish.
        //    The periodic reconciler would do this on next detach
        //    event; explicit removal here keeps the subject
        //    envelope monotonic (no ghost row while the reconciler
        //    is between ticks).
        {
            let mut inner = self.inner.lock().await;
            inner.drives.remove(&req.stable_id);
        }
        self.republish_envelope().await;

        let resp = SafeRemoveResponse {
            v: 1,
            removed: true,
            forced: Some(forced),
            holders,
        };
        serde_json::to_vec(&resp)
            .map_err(|e| VerbDispatchError::ResponseSerialise(e.to_string()))
    }

    /// Best-effort fuser -m enumeration of processes holding
    /// open files under the mount point. Returns a `Vec<String>`
    /// of `"<pid>:<comm>"` entries so operator diagnostics can
    /// point at "which process kept the drive busy". Empty
    /// vector when fuser is absent / returns no holders.
    async fn fuser_holders(&self, mount_root: &Option<String>) -> Vec<String> {
        let root = match mount_root {
            Some(r) => r,
            None => return Vec::new(),
        };
        let out = tokio::process::Command::new("fuser")
            .args(["-m", root])
            .output()
            .await;
        let out = match out {
            Ok(o) => o,
            Err(_) => return Vec::new(),
        };
        // fuser prints pids to stderr, one per whitespace-separated
        // token, with a trailing newline. Parse defensively.
        let text = String::from_utf8_lossy(&out.stderr).to_string();
        let mut result = Vec::new();
        for tok in text.split_whitespace() {
            if let Ok(pid) = tok.parse::<u32>() {
                let comm =
                    tokio::fs::read_to_string(format!("/proc/{}/comm", pid))
                        .await
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                if comm.is_empty() {
                    result.push(pid.to_string());
                } else {
                    result.push(format!("{pid}:{comm}"));
                }
            }
        }
        result
    }

    async fn compose_envelope(&self) -> ListDrivesEnvelope {
        let inner = self.inner.lock().await;
        ListDrivesEnvelope {
            v: 1,
            drives: inner.drives.values().cloned().collect(),
            last_update_at_ms: inner.last_update_at_ms,
        }
    }

    // ----------------------------------------------------------
    // rename verb
    // ----------------------------------------------------------

    /// `storage.usb.rename` handler. Binds an operator-supplied
    /// friendly name to the drive's identity tuple + runs the
    /// full remount cycle so the mount path (`/var/lib/evo/music/
    /// USB/<alias>`) IS the friendly id.
    ///
    /// Sequence (per USB-STORAGE.md §4):
    ///
    /// 1. Sanitise + validate alias. Empty alias → clear the
    ///    persisted entry so the derivation ladder falls back
    ///    to the next rule (fs label / vendor+model / etc.).
    /// 2. Refuse if role is `system-*` live.
    /// 3. Collision check: sanitised alias must not collide
    ///    with a foreign physical volume's current stable_id.
    ///    Same physical volume aliasing back to its own current
    ///    id is a no-op success.
    /// 4. Consumer-stop: `library.remove_source` (best-effort).
    /// 5. `sync` on the parent disk.
    /// 6. Wrapper `umount <old-id>`.
    /// 7. Persist the alias to `aliases.toml` (or clear on
    ///    empty alias).
    /// 8. Reload the alias store into the runtime + reconcile
    ///    to recompute the drive's stable_id with rule 0 in
    ///    effect.
    /// 9. Wrapper `mount <new-id>` + `library.add_source
    ///    local_usb` + republish subject.
    ///
    /// Payload: `{ v: 1, stable_id, alias, mount_policy?: string }`
    /// Response: `{ v: 1, new_stable_id, class }`
    async fn handle_rename(
        &self,
        payload: &[u8],
    ) -> Result<Vec<u8>, VerbDispatchError> {
        let req: RenameRequest = serde_json::from_slice(payload)
            .map_err(|e| VerbDispatchError::PayloadDecode(e.to_string()))?;

        // Sanitise alias. Two distinct paths:
        //   raw trim-empty → CLEAR (removes any persisted alias;
        //     derivation falls back to fs_label / vendor+model)
        //   raw non-empty but sanitises to empty → InvalidAlias
        //     refuse (operator gave all-symbol garbage; specific
        //     feedback rather than silent no-op)
        let raw_trimmed = req.alias.trim();
        let clearing = raw_trimmed.is_empty();
        let sanitised = if clearing {
            String::new()
        } else {
            crate::stable_id::sanitise(&req.alias)
        };
        if !clearing && sanitised.is_empty() {
            return Err(VerbDispatchError::RenameRefused(
                RenameRefuseClass::InvalidAlias {
                    stable_id: req.stable_id,
                    raw: req.alias,
                },
            ));
        }

        self.reconcile_once().await?;
        let record = {
            let inner = self.inner.lock().await;
            inner.drives.get(&req.stable_id).cloned()
        }
        .ok_or_else(|| {
            VerbDispatchError::RenameRefused(
                RenameRefuseClass::UnknownStableId {
                    stable_id: req.stable_id.clone(),
                },
            )
        })?;
        if record.role.is_system_live() {
            return Err(VerbDispatchError::RenameRefused(
                RenameRefuseClass::SystemLivePartition {
                    stable_id: req.stable_id,
                    role: role_wire_string(record.role),
                },
            ));
        }
        // Serial short is required for the alias identity tuple —
        // without it we cannot match on replug.
        let serial = record.serial_short.as_deref().ok_or_else(|| {
            VerbDispatchError::RenameRefused(
                RenameRefuseClass::MissingIdentity {
                    stable_id: req.stable_id.clone(),
                    missing: "serial_short",
                },
            )
        })?;

        // Collision check: would the new stable_id (sanitised alias
        // + partition suffix if the parent has >1 partition) collide
        // with another drive's current stable_id? Skip when the
        // alias is being cleared (post-clear stable_id derives from
        // rule 1/2/3/4; collision may still fire via those rules but
        // that's handled by the normal deconflict path).
        if !clearing {
            let inner = self.inner.lock().await;
            let candidate_stable_id = if record.partition_count > 1 {
                format!("{sanitised}-p{}", record.partition_index)
            } else {
                sanitised.clone()
            };
            for (other_id, other_rec) in inner.drives.iter() {
                if *other_id == req.stable_id {
                    continue;
                }
                if *other_id == candidate_stable_id {
                    return Err(VerbDispatchError::RenameRefused(
                        RenameRefuseClass::AliasWouldCollide {
                            stable_id: req.stable_id.clone(),
                            requested_alias: sanitised.clone(),
                            colliding_stable_id: other_rec.stable_id.clone(),
                        },
                    ));
                }
            }
        }

        // 1. Consumer-stop (best-effort).
        if let Some(source_id) = record.library_source_id.as_ref() {
            if let Some(dispatcher) = self.shelf_dispatcher_clone() {
                let stop_payload = serde_json::json!({
                    "v": 1,
                    "source_id": source_id,
                });
                if let Ok(bytes) = serde_json::to_vec(&stop_payload) {
                    if let Err(e) = dispatcher
                        .dispatch(
                            "audio.library",
                            "library.remove_source",
                            bytes,
                            None,
                        )
                        .await
                    {
                        tracing::warn!(
                            plugin = "storage.usb",
                            stable_id = %req.stable_id,
                            source_id = %source_id,
                            error = %e,
                            "library.remove_source dispatch failed before rename; \
                             proceeding (best-effort)"
                        );
                    }
                }
            }
        }

        // 2. sync + umount OLD path (if mounted).
        let _ = tokio::process::Command::new("sync")
            .arg(&record.parent_disk)
            .output()
            .await;
        if record.class == DriveClass::MountedClean
            || record.class == DriveClass::MountedDirty
        {
            let umount_argv = vec!["umount".to_string(), req.stable_id.clone()];
            let out = self
                .command_runner
                .run_wrapper(self.needs_sudo, &umount_argv)
                .await
                .map_err(|e| VerbDispatchError::SubprocessIo(e.to_string()))?;
            if out.status != 0 {
                return Err(VerbDispatchError::RenameRefused(
                    RenameRefuseClass::UmountBeforeRenameFailed {
                        stable_id: req.stable_id,
                        exit_code: out.status,
                        stderr: out.stderr,
                    },
                ));
            }
        }

        // 3. Persist alias — set or clear.
        {
            let current = self.aliases_clone();
            let mut mutated = (*current).clone();
            if clearing {
                mutated.clear_alias(
                    record.vendor.as_deref(),
                    record.model.as_deref(),
                    serial,
                    record.partuuid.as_deref(),
                );
            } else {
                mutated.set_alias(
                    record.vendor.as_deref(),
                    record.model.as_deref(),
                    serial,
                    record.partuuid.as_deref(),
                    record.partition_index,
                    &sanitised,
                    now_ms(),
                );
            }
            mutated.save().map_err(|e| {
                VerbDispatchError::RenameRefused(
                    RenameRefuseClass::AliasPersistFailed {
                        stable_id: req.stable_id.clone(),
                        message: e.to_string(),
                    },
                )
            })?;
            self.attach_alias_store(Arc::new(mutated));
        }

        // 4. Reconcile — the classifier re-runs, the derivation
        //    ladder picks up the new alias (or the ladder falls
        //    through when clearing), and the drive gets a fresh
        //    stable_id. The reconcile also handles auto-mount so
        //    we typically don't need an explicit mount call.
        self.reconcile_once().await?;

        // 5. Find the drive's new stable_id via identity match on
        //    (vendor, model, serial_short, partuuid).
        let (new_stable_id, new_class) = {
            let inner = self.inner.lock().await;
            let mut found: Option<(String, DriveClass)> = None;
            for (id, rec) in inner.drives.iter() {
                let same_serial = rec
                    .serial_short
                    .as_deref()
                    .map(|s| s == serial)
                    .unwrap_or(false);
                let same_partuuid = opt_eq_str(
                    rec.partuuid.as_deref(),
                    record.partuuid.as_deref(),
                );
                let same_dev = rec.device_node == record.device_node;
                if same_serial && same_partuuid && same_dev {
                    found = Some((id.clone(), rec.class));
                    break;
                }
            }
            found
        }
        .ok_or_else(|| {
            VerbDispatchError::RenameRefused(
                RenameRefuseClass::PostRenameLookupFailed {
                    stable_id: req.stable_id.clone(),
                },
            )
        })?;

        // 6. Best-effort empty-only rmdir on the OLD mount root
        //    if it wasn't the same as the new (rename to same
        //    sanitised token is a no-op path).
        if new_stable_id != req.stable_id {
            if let Some(old_root) = record.mount_root.as_deref() {
                let _ = tokio::fs::remove_dir(old_root).await;
            }
        }

        let resp = RenameResponse {
            v: 1,
            new_stable_id,
            class: new_class.wire_str().to_string(),
        };
        serde_json::to_vec(&resp)
            .map_err(|e| VerbDispatchError::ResponseSerialise(e.to_string()))
    }

    // ----------------------------------------------------------
    // repair_filesystem verb
    // ----------------------------------------------------------

    /// `storage.usb.repair_filesystem` handler. Consumer-stop
    /// before fsck, mirroring the shares MPD-stop-before-mutation
    /// pattern. No fsck runs while MPD holds files open.
    ///
    /// Sequence (per USB-STORAGE.md §8):
    ///
    /// 1. Refuse if role is `system-*` live (would corrupt the
    ///    live FS).
    /// 2. Refuse if class is `mounted-dirty-hiberfile` (NTFS
    ///    hiberfile — ntfsfix would refuse anyway; operator must
    ///    resume + shut down Windows cleanly first).
    /// 3. Refuse if FS is unsupported.
    /// 4. Dispatch `library.remove_source` for the drive's
    ///    `library_source_id` (best-effort MPD stop).
    /// 5. `sync` on the parent disk.
    /// 6. Wrapper `umount <stable-id>` (if currently mounted).
    /// 7. Wrapper `fsck <stable-id> <fs-type> <device-node>
    ///    [escalate]`. Distinguishes success (exit 0), dirty-
    ///    remaining (exit 5), hiberfile (exit 6), other subprocess
    ///    failure.
    /// 8. On repair success: wrapper `mount` again + `library.
    ///    add_source local_usb` + republish subject with
    ///    `class: mounted-clean`.
    /// 9. On repair failure: republish subject with
    ///    `class: mount-failed-dirty` and structured error class.
    ///
    /// Payload: `{ v: 1, stable_id, escalate?: bool }`
    /// Response: `{ v: 1, repaired: true, before_class, after_class }`
    async fn handle_repair_filesystem(
        &self,
        payload: &[u8],
    ) -> Result<Vec<u8>, VerbDispatchError> {
        let req: RepairRequest = serde_json::from_slice(payload)
            .map_err(|e| VerbDispatchError::PayloadDecode(e.to_string()))?;

        self.reconcile_once().await?;

        let record = {
            let inner = self.inner.lock().await;
            inner.drives.get(&req.stable_id).cloned()
        }
        .ok_or_else(|| {
            VerbDispatchError::RepairRefused(
                RepairRefuseClass::UnknownStableId {
                    stable_id: req.stable_id.clone(),
                },
            )
        })?;

        if record.role.is_system_live() {
            return Err(VerbDispatchError::RepairRefused(
                RepairRefuseClass::SystemLivePartition {
                    stable_id: req.stable_id,
                    role: role_wire_string(record.role),
                },
            ));
        }
        if record.class == DriveClass::MountedDirtyHiberfile {
            return Err(VerbDispatchError::RepairRefused(
                RepairRefuseClass::NtfsHiberfile {
                    stable_id: req.stable_id,
                },
            ));
        }
        let family = FsFamily::from_lsblk(&record.fs_type);
        if family == FsFamily::Unsupported {
            return Err(VerbDispatchError::RepairRefused(
                RepairRefuseClass::UnsupportedFs {
                    stable_id: req.stable_id,
                    fs_type: record.fs_type.clone(),
                },
            ));
        }

        let before_class = record.class;

        // 1. Consumer-stop — library.remove_source.
        if let Some(source_id) = record.library_source_id.as_ref() {
            if let Some(dispatcher) = self.shelf_dispatcher_clone() {
                let payload = serde_json::json!({
                    "v": 1,
                    "source_id": source_id,
                });
                if let Ok(bytes) = serde_json::to_vec(&payload) {
                    if let Err(e) = dispatcher
                        .dispatch(
                            "audio.library",
                            "library.remove_source",
                            bytes,
                            None,
                        )
                        .await
                    {
                        tracing::warn!(
                            plugin = "storage.usb",
                            stable_id = %req.stable_id,
                            source_id = %source_id,
                            error = %e,
                            "library.remove_source dispatch failed before fsck; \
                             proceeding (best-effort)"
                        );
                    }
                }
            }
        }

        // 2. sync — flush kernel dirty pages on the parent disk.
        let _ = tokio::process::Command::new("sync")
            .arg(&record.parent_disk)
            .output()
            .await;

        // 3. Umount if currently mounted. Idempotent: wrapper
        // umount on a non-mounted target returns exit 0.
        if record.class == DriveClass::MountedClean
            || record.class == DriveClass::MountedDirty
        {
            let umount_argv = vec!["umount".to_string(), req.stable_id.clone()];
            let out = self
                .command_runner
                .run_wrapper(self.needs_sudo, &umount_argv)
                .await
                .map_err(|e| VerbDispatchError::SubprocessIo(e.to_string()))?;
            if out.status != 0 {
                // EBUSY (4) or other. Repair requires unmounted;
                // fail with structured class + let operator run
                // safe_remove --force first.
                return Err(VerbDispatchError::RepairRefused(
                    RepairRefuseClass::UmountBeforeRepairFailed {
                        stable_id: req.stable_id,
                        exit_code: out.status,
                        stderr: out.stderr,
                    },
                ));
            }
        }

        // 4. Wrapper fsck (repair).
        let fs_arg = family
            .wrapper_fs_arg()
            .unwrap_or(&record.fs_type)
            .to_string();
        let mut fsck_argv = vec![
            "fsck".to_string(),
            req.stable_id.clone(),
            fs_arg.clone(),
            record.device_node.clone(),
        ];
        if req.escalate.unwrap_or(false) {
            fsck_argv.push("escalate".to_string());
        }
        let fsck_out = self
            .command_runner
            .run_wrapper(self.needs_sudo, &fsck_argv)
            .await
            .map_err(|e| VerbDispatchError::SubprocessIo(e.to_string()))?;

        match fsck_out.status {
            0 => {
                // Repair succeeded. Re-mount + re-add to library.
            }
            5 => {
                // Dirty-remaining. Republish subject with
                // mount-failed-dirty.
                self.mark_drive_class(
                    &req.stable_id,
                    DriveClass::MountFailedDirty,
                )
                .await;
                self.republish_envelope().await;
                return Err(VerbDispatchError::RepairRefused(
                    RepairRefuseClass::RepairFailed {
                        stable_id: req.stable_id,
                        fs_family: fs_arg,
                        stderr: fsck_out.stderr,
                    },
                ));
            }
            6 => {
                // NTFS hiberfile — mark and refuse.
                self.mark_drive_class(
                    &req.stable_id,
                    DriveClass::MountedDirtyHiberfile,
                )
                .await;
                self.republish_envelope().await;
                return Err(VerbDispatchError::RepairRefused(
                    RepairRefuseClass::NtfsHiberfile {
                        stable_id: req.stable_id,
                    },
                ));
            }
            other => {
                self.mark_drive_class(
                    &req.stable_id,
                    DriveClass::MountFailedDirty,
                )
                .await;
                self.republish_envelope().await;
                return Err(VerbDispatchError::RepairRefused(
                    RepairRefuseClass::RepairSubprocessFailed {
                        stable_id: req.stable_id,
                        exit_code: other,
                        stderr: fsck_out.stderr,
                    },
                ));
            }
        }

        // 5. Re-mount via wrapper.
        let opts = family.mount_options(self.service_uid, self.service_gid);
        let mount_argv = vec![
            "mount".to_string(),
            req.stable_id.clone(),
            fs_arg.clone(),
            record.device_node.clone(),
            opts,
        ];
        let mount_out = self
            .command_runner
            .run_wrapper(self.needs_sudo, &mount_argv)
            .await
            .map_err(|e| VerbDispatchError::SubprocessIo(e.to_string()))?;
        if mount_out.status != 0 {
            self.mark_drive_class(&req.stable_id, DriveClass::MountFailedOther)
                .await;
            self.republish_envelope().await;
            return Err(VerbDispatchError::RepairRefused(
                RepairRefuseClass::PostRepairMountFailed {
                    stable_id: req.stable_id,
                    exit_code: mount_out.status,
                    stderr: mount_out.stderr,
                },
            ));
        }

        // 6. Re-add to library (best-effort).
        let mount_root = format!("{USB_MOUNT_ROOT}/{}", req.stable_id);
        let mut library_source_id: Option<String> = None;
        if let Some(dispatcher) = self.shelf_dispatcher_clone() {
            match self
                .dispatch_library_add_source(
                    dispatcher,
                    &req.stable_id,
                    &record.device_node,
                    record.display_name.as_deref().unwrap_or(&req.stable_id),
                    &mount_root,
                )
                .await
            {
                Ok(id) => library_source_id = id,
                Err(e) => tracing::warn!(
                    plugin = "storage.usb",
                    stable_id = %req.stable_id,
                    error = %e,
                    "library.add_source local_usb dispatch failed \
                     after repair; drive is mounted but not in \
                     library until next reconcile"
                ),
            }
        }

        // 7. Update registry + republish.
        {
            let mut inner = self.inner.lock().await;
            if let Some(rec) = inner.drives.get_mut(&req.stable_id) {
                rec.class = DriveClass::MountedClean;
                rec.mount_root = Some(mount_root);
                rec.library_source_id = library_source_id;
                rec.last_transition_at_ms = Some(now_ms());
            }
        }
        self.republish_envelope().await;

        let resp = RepairResponse {
            v: 1,
            repaired: true,
            before_class: before_class.wire_str().to_string(),
            after_class: DriveClass::MountedClean.wire_str().to_string(),
        };
        serde_json::to_vec(&resp)
            .map_err(|e| VerbDispatchError::ResponseSerialise(e.to_string()))
    }

    async fn republish_envelope(&self) {
        let envelope = self.compose_envelope().await;
        let publisher = {
            let slot = self
                .publisher
                .lock()
                .expect("storage.usb: publisher lock poisoned on republish");
            slot.as_ref().map(|p| Arc::clone(&p.announcer))
        };
        if let Some(announcer) = publisher {
            let state = serde_json::to_value(&envelope)
                .unwrap_or(serde_json::Value::Null);
            if let Err(e) = announcer
                .update_state(storage_usb_drives_addressing(), state)
                .await
            {
                tracing::debug!(
                    plugin = "storage.usb",
                    error = %e,
                    "subject republish failed"
                );
            }
        }
    }

    fn shelf_dispatcher_clone(
        &self,
    ) -> Option<Arc<dyn ShelfRequestDispatcher>> {
        let slot = self
            .shelf_dispatcher
            .lock()
            .expect("storage.usb: dispatcher lock poisoned on read");
        slot.as_ref().cloned()
    }

    fn aliases_clone(&self) -> Arc<AliasStore> {
        let slot = self
            .aliases
            .lock()
            .expect("storage.usb: aliases lock poisoned on read");
        Arc::clone(&slot)
    }

    async fn dispatch_library_add_source(
        &self,
        dispatcher: Arc<dyn ShelfRequestDispatcher>,
        stable_id: &str,
        device_node: &str,
        display_name: &str,
        mount_root: &str,
    ) -> Result<Option<String>, String> {
        let payload = serde_json::json!({
            "v": 1,
            "display_name": display_name,
            "kind": {
                "kind": "local_usb",
                "device_node": device_node,
                "label": stable_id,
            },
            "mount_path": mount_root,
        });
        let bytes = serde_json::to_vec(&payload).map_err(|e| e.to_string())?;
        let response = dispatcher
            .dispatch("audio.library", "library.add_source", bytes, None)
            .await
            .map_err(|e| e.to_string())?;
        let parsed: serde_json::Value = serde_json::from_slice(&response)
            .unwrap_or(serde_json::Value::Null);
        Ok(parsed
            .get("source_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()))
    }
}

/// Spawn the periodic reconciler.
pub fn spawn_reconcile_task(
    runtime: Arc<StorageUsbRuntime>,
    cadence: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(cadence);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            if let Err(e) = runtime.reconcile_once().await {
                tracing::debug!(
                    plugin = "storage.usb",
                    error = %e,
                    "periodic reconcile failed; will retry next tick"
                );
            }
        }
    })
}

// --------------------------------------------------------------
// Wire-shape records
// --------------------------------------------------------------

/// Envelope returned by `storage.usb.list_drives` + carried on
/// the `storage_usb_drives` subject.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListDrivesEnvelope {
    /// Envelope shape version.
    pub v: u32,
    /// One record per classified USB-transport partition.
    pub drives: Vec<DriveRecord>,
    /// Wall-clock ms of the last enumeration.
    pub last_update_at_ms: i64,
}

/// One drive record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriveRecord {
    /// Stable id (mount path leaf).
    pub stable_id: String,
    /// Display token — stable-id sans partition suffix.
    pub display_name: Option<String>,
    /// Which rule in the derivation ladder produced the id.
    pub id_source: Option<String>,
    /// Partition device node.
    pub device_node: String,
    /// Parent disk device node.
    pub parent_disk: String,
    /// 1-based partition index.
    pub partition_index: u32,
    /// Total mountable partitions on the parent disk.
    pub partition_count: u32,
    /// Filesystem label.
    pub label: Option<String>,
    /// Filesystem UUID.
    pub uuid: Option<String>,
    /// GPT PARTUUID.
    pub partuuid: Option<String>,
    /// Udev vendor.
    pub vendor: Option<String>,
    /// Udev model.
    pub model: Option<String>,
    /// Udev serial short.
    pub serial_short: Option<String>,
    /// Filesystem family.
    pub fs_type: String,
    /// Byte size.
    pub size_bytes: u64,
    /// Six-way role.
    #[serde(with = "role_serde")]
    pub role: PartitionRole,
    /// Mount policy.
    #[serde(with = "policy_serde")]
    pub mount_policy: MountPolicy,
    /// Current class.
    #[serde(with = "class_serde")]
    pub class: DriveClass,
    /// Mount point (present when class is `mounted-*`).
    pub mount_root: Option<String>,
    /// `library.add_source` result when class is `mounted-*`.
    pub library_source_id: Option<String>,
    /// True when the drive has an operator-set alias.
    pub alias_set: bool,
    /// Wall-clock ms of the last state change.
    pub last_transition_at_ms: Option<i64>,
}

impl DriveRecord {
    /// Build a record from a fresh classifier output + derived id.
    pub fn from_partition(
        p: &ClassifiedPartition,
        derived: &crate::stable_id::DerivedId,
        alias_set: bool,
    ) -> Self {
        let class = if p.role.is_system_live() {
            DriveClass::SystemDisk
        } else if let Some(mp) = &p.current_mount {
            if mp.starts_with(USB_MOUNT_ROOT) {
                DriveClass::MountedClean
            } else {
                DriveClass::Unmounted
            }
        } else if FsFamily::from_lsblk(&p.fs_type) == FsFamily::Unsupported {
            DriveClass::Unsupported
        } else {
            DriveClass::Unmounted
        };
        let mount_root = if class == DriveClass::MountedClean {
            p.current_mount.clone()
        } else {
            None
        };
        Self {
            stable_id: derived.stable_id.clone(),
            display_name: Some(derived.display_name.clone()),
            id_source: Some(derived.id_source.wire_str().to_string()),
            device_node: p.device_node.clone(),
            parent_disk: p.parent_disk.clone(),
            partition_index: p.partition_index,
            partition_count: p.partition_count,
            label: p.label.clone(),
            uuid: p.uuid.clone(),
            partuuid: p.partuuid.clone(),
            vendor: p.vendor.clone(),
            model: p.model.clone(),
            serial_short: p.serial_short.clone(),
            fs_type: p.fs_type.clone(),
            size_bytes: p.size_bytes,
            role: p.role,
            mount_policy: p.mount_policy,
            class,
            mount_root,
            library_source_id: None,
            alias_set,
            last_transition_at_ms: None,
        }
    }
}

/// DriveClass — reflects the mount lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveClass {
    /// Live-system partition — never mounts here.
    SystemDisk,
    /// FS type not in the support matrix.
    Unsupported,
    /// Detected, not yet mounted.
    Unmounted,
    /// Mounted, no dirty flag.
    MountedClean,
    /// Mounted, dirty flag on.
    MountedDirty,
    /// NTFS hiberfile present — mount refused.
    MountedDirtyHiberfile,
    /// Mount refused due to dirty state.
    MountFailedDirty,
    /// FAT32 volume > 2 TiB — mount refused per §2.
    MountFailedOversizedVfat,
    /// Mount errno other than dirty / oversized.
    MountFailedOther,
}

impl DriveClass {
    /// Stable wire string matching the schema enum.
    pub fn wire_str(self) -> &'static str {
        match self {
            DriveClass::SystemDisk => "system-disk",
            DriveClass::Unsupported => "unsupported",
            DriveClass::Unmounted => "unmounted",
            DriveClass::MountedClean => "mounted-clean",
            DriveClass::MountedDirty => "mounted-dirty",
            DriveClass::MountedDirtyHiberfile => "mounted-dirty-hiberfile",
            DriveClass::MountFailedDirty => "mount-failed-dirty",
            DriveClass::MountFailedOversizedVfat => {
                "mount-failed-oversized-vfat"
            }
            DriveClass::MountFailedOther => "mount-failed-other",
        }
    }
}

mod role_serde {
    use super::PartitionRole;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(
        r: &PartitionRole,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        super::role_wire_string(*r).serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<PartitionRole, D::Error> {
        let s = String::deserialize(d)?;
        match s.as_str() {
            "system-root" => Ok(PartitionRole::SystemRoot),
            "system-boot" => Ok(PartitionRole::SystemBoot),
            "system-efi" => Ok(PartitionRole::SystemEfi),
            "system-swap" => Ok(PartitionRole::SystemSwap),
            "system-adjacent" => Ok(PartitionRole::SystemAdjacent),
            "removable" => Ok(PartitionRole::Removable),
            other => Err(serde::de::Error::custom(format!(
                "unknown PartitionRole wire string {other:?}"
            ))),
        }
    }
}

mod policy_serde {
    use super::MountPolicy;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(
        p: &MountPolicy,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        super::mount_policy_wire_string(*p).serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<MountPolicy, D::Error> {
        let s = String::deserialize(d)?;
        match s.as_str() {
            "auto" => Ok(MountPolicy::Auto),
            "opt-in-required" => Ok(MountPolicy::OptInRequired),
            "refused-system-live" => Ok(MountPolicy::RefusedSystemLive),
            other => Err(serde::de::Error::custom(format!(
                "unknown MountPolicy wire string {other:?}"
            ))),
        }
    }
}

mod class_serde {
    use super::DriveClass;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(
        c: &DriveClass,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        c.wire_str().serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<DriveClass, D::Error> {
        let s = String::deserialize(d)?;
        match s.as_str() {
            "system-disk" => Ok(DriveClass::SystemDisk),
            "unsupported" => Ok(DriveClass::Unsupported),
            "unmounted" => Ok(DriveClass::Unmounted),
            "mounted-clean" => Ok(DriveClass::MountedClean),
            "mounted-dirty" => Ok(DriveClass::MountedDirty),
            "mounted-dirty-hiberfile" => Ok(DriveClass::MountedDirtyHiberfile),
            "mount-failed-dirty" => Ok(DriveClass::MountFailedDirty),
            "mount-failed-oversized-vfat" => {
                Ok(DriveClass::MountFailedOversizedVfat)
            }
            "mount-failed-other" => Ok(DriveClass::MountFailedOther),
            other => Err(serde::de::Error::custom(format!(
                "unknown DriveClass wire string {other:?}"
            ))),
        }
    }
}

pub(crate) fn role_wire_string(r: PartitionRole) -> String {
    match r {
        PartitionRole::SystemRoot => "system-root",
        PartitionRole::SystemBoot => "system-boot",
        PartitionRole::SystemEfi => "system-efi",
        PartitionRole::SystemSwap => "system-swap",
        PartitionRole::SystemAdjacent => "system-adjacent",
        PartitionRole::Removable => "removable",
    }
    .to_string()
}

pub(crate) fn mount_policy_wire_string(p: MountPolicy) -> String {
    match p {
        MountPolicy::Auto => "auto",
        MountPolicy::OptInRequired => "opt-in-required",
        MountPolicy::RefusedSystemLive => "refused-system-live",
    }
    .to_string()
}

/// `storage.usb.mount` request payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountRequest {
    /// Stable-id of the drive to mount.
    pub stable_id: String,
}

/// `storage.usb.mount` response payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountResponse {
    /// Envelope shape version.
    pub v: u32,
    /// Mount point (`/var/lib/evo/music/USB/<stable-id>`).
    pub mounted_at: String,
    /// DriveClass wire string.
    pub class: String,
    /// `library.add_source` result.
    pub library_source_id: Option<String>,
}

/// `storage.usb.rename` request payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameRequest {
    /// Stable-id of the drive whose alias is being set/cleared.
    pub stable_id: String,
    /// The operator-supplied friendly name. Sanitised per the
    /// stable-id token rule (`^[A-Za-z0-9][A-Za-z0-9_-]{0,31}$`).
    /// Empty string / whitespace-only clears the persisted alias
    /// so the derivation ladder falls back to the next rule.
    pub alias: String,
    /// Optional mount-policy override for `system-adjacent` drives.
    /// `"opt-in"` opts the sibling partition in for auto-mount;
    /// omitting or setting `"opt-in-required"` keeps the gate
    /// closed. No-op for `removable` drives (already `auto`)
    /// and refused for `system-*` live drives (rename refuse
    /// fires earlier). Not yet wired — mount-policy override
    /// lands with the policy-mutation UI in a follow-on.
    #[serde(default)]
    pub mount_policy: Option<String>,
}

/// `storage.usb.rename` response payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameResponse {
    /// Envelope shape version.
    pub v: u32,
    /// The drive's new stable_id after alias resolution +
    /// reconcile. May equal the original stable_id when the
    /// sanitised alias resolves to the same token (no-op).
    pub new_stable_id: String,
    /// DriveClass wire string after the remount cycle.
    pub class: String,
}

/// `storage.usb.repair_filesystem` request payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairRequest {
    /// Stable-id of the drive to repair.
    pub stable_id: String,
    /// Escalate the repair tool to its more-aggressive mode
    /// (`e2fsck -y` instead of `-p`). Default `false`. Operator
    /// acknowledges the risk explicitly at the UI confirm modal
    /// before setting this to `true`.
    #[serde(default)]
    pub escalate: Option<bool>,
}

/// `storage.usb.repair_filesystem` response payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairResponse {
    /// Envelope shape version.
    pub v: u32,
    /// Always `true` on success (error path returns
    /// [`RepairRefuseClass`] instead).
    pub repaired: bool,
    /// DriveClass wire string BEFORE the repair (usually
    /// `mounted-dirty` or `mount-failed-dirty`).
    pub before_class: String,
    /// DriveClass wire string AFTER the repair (`mounted-clean`
    /// on success).
    pub after_class: String,
}

/// `storage.usb.safe_remove` request payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafeRemoveRequest {
    /// Stable-id of the drive to safe-remove.
    pub stable_id: String,
    /// Force lazy detach on EBUSY. Default false — first attempt
    /// returns a `Busy` refusal with a fuser-derived holder
    /// list; operator retries with `force: true` after
    /// acknowledging the data-loss risk in the UI modal.
    #[serde(default)]
    pub force: Option<bool>,
}

/// `storage.usb.safe_remove` response payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafeRemoveResponse {
    /// Envelope shape version.
    pub v: u32,
    /// Always `true` on success (error path returns
    /// [`SafeRemoveRefuseClass`] instead).
    pub removed: bool,
    /// `Some(true)` when the operator's `force: true` triggered
    /// the lazy-detach fallback; `Some(false)` on clean umount.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forced: Option<bool>,
    /// Populated only on the `Busy { holders }` refuse path
    /// (returned as a `SafeRemoveRefuseClass`, not here); kept
    /// as an option on the success shape so consumers see a
    /// consistent field for logging.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub holders: Option<Vec<String>>,
}

// --------------------------------------------------------------
// Error taxonomy
// --------------------------------------------------------------

/// Fine-grained mount refusal classes.
#[derive(Debug, Clone, thiserror::Error)]
pub enum MountRefuseClass {
    /// Stable-id not in the classifier's current output.
    #[error("unknown stable_id {stable_id:?}")]
    UnknownStableId {
        /// Requested id.
        stable_id: String,
    },
    /// Role is `system-*` live — non-negotiable refuse.
    #[error(
        "stable_id {stable_id:?} is a live system partition (role={role})"
    )]
    SystemLivePartition {
        /// Requested id.
        stable_id: String,
        /// Role wire string.
        role: String,
    },
    /// Role is `system-adjacent` but no operator opt-in.
    #[error(
        "stable_id {stable_id:?} is system-adjacent; operator opt-in required"
    )]
    SystemAdjacentNotOptedIn {
        /// Requested id.
        stable_id: String,
    },
    /// FS not in the support matrix.
    #[error("stable_id {stable_id:?} has unsupported fs_type {fs_type:?}")]
    UnsupportedFs {
        /// Requested id.
        stable_id: String,
        /// FS type reported by the classifier.
        fs_type: String,
    },
    /// FAT32 volume exceeds the 2 TiB spec cap.
    #[error(
        "stable_id {stable_id:?} is FAT32 at {size_bytes} bytes, over the {cap_bytes} byte spec cap"
    )]
    MountFailedOversizedVfat {
        /// Requested id.
        stable_id: String,
        /// Volume size reported by lsblk.
        size_bytes: u64,
        /// Cap enforced by the FS matrix.
        cap_bytes: u64,
    },
    /// The wrapper subprocess exited non-zero.
    #[error(
        "stable_id {stable_id:?} mount subprocess exit {exit_code}: {stderr}"
    )]
    MountSubprocessFailed {
        /// Requested id.
        stable_id: String,
        /// Exit code from the wrapper.
        exit_code: i32,
        /// Captured stderr for operator diagnostics.
        stderr: String,
    },
}

/// Errors returned by [`StorageUsbRuntime::dispatch_verb`].
#[derive(Debug, thiserror::Error)]
pub enum VerbDispatchError {
    /// Verb string is not one of [`STORAGE_USB_VERBS`].
    #[error("storage.usb: unknown verb {verb:?}")]
    UnknownRequestType {
        /// The unrecognised verb.
        verb: String,
    },
    /// Verb declared but its implementation lands in a later step.
    #[error("storage.usb: verb {verb:?} not implemented yet")]
    NotImplemented {
        /// The declared-but-unwired verb.
        verb: String,
    },
    /// Payload deserialisation failed.
    #[error("storage.usb: payload decode failed: {0}")]
    PayloadDecode(String),
    /// Classifier failed.
    #[error("storage.usb: classifier failed: {0}")]
    Classify(#[from] ClassifierError),
    /// Input source failed.
    #[error("storage.usb: input source failed: {0}")]
    InputSource(String),
    /// Wrapper subprocess spawn/wait failed at the OS layer.
    #[error("storage.usb: subprocess I/O failed: {0}")]
    SubprocessIo(String),
    /// Response serialisation failed.
    #[error("storage.usb: response serialise failed: {0}")]
    ResponseSerialise(String),
    /// Mount refused per one of the fine-grained classes.
    #[error("storage.usb: mount refused: {0}")]
    MountRefused(#[source] MountRefuseClass),
    /// Safe-remove refused per one of the fine-grained classes.
    #[error("storage.usb: safe-remove refused: {0}")]
    SafeRemoveRefused(#[source] SafeRemoveRefuseClass),
    /// Repair refused per one of the fine-grained classes.
    #[error("storage.usb: repair refused: {0}")]
    RepairRefused(#[source] RepairRefuseClass),
    /// Rename refused per one of the fine-grained classes.
    #[error("storage.usb: rename refused: {0}")]
    RenameRefused(#[source] RenameRefuseClass),
}

/// Fine-grained rename refusal classes.
#[derive(Debug, Clone, thiserror::Error)]
pub enum RenameRefuseClass {
    /// Stable-id not in the classifier's current output.
    #[error("unknown stable_id {stable_id:?}")]
    UnknownStableId {
        /// Requested id.
        stable_id: String,
    },
    /// Role is `system-*` live — rename refused.
    #[error(
        "stable_id {stable_id:?} is a live system partition (role={role}); rename refused"
    )]
    SystemLivePartition {
        /// Requested id.
        stable_id: String,
        /// Role wire string.
        role: String,
    },
    /// The raw alias is non-empty but sanitises to empty (all
    /// symbol chars stripped). Operator gets specific feedback
    /// instead of a silent no-op.
    #[error("stable_id {stable_id:?} alias {raw:?} sanitises to empty")]
    InvalidAlias {
        /// Requested id.
        stable_id: String,
        /// The raw alias input.
        raw: String,
    },
    /// Identity tuple lacks a serial_short — the alias key
    /// requires it (drive would not match on replug otherwise).
    /// Very rare — udev's ID_SERIAL_SHORT is set for
    /// mass-storage-class devices per the USB spec.
    #[error(
        "stable_id {stable_id:?} missing identity field {missing:?}; rename refused"
    )]
    MissingIdentity {
        /// Requested id.
        stable_id: String,
        /// Which identity field is absent.
        missing: &'static str,
    },
    /// The sanitised alias collides with a foreign physical
    /// volume's current stable_id. Operator picks a different
    /// name.
    #[error(
        "stable_id {stable_id:?} alias {requested_alias:?} collides with {colliding_stable_id:?}"
    )]
    AliasWouldCollide {
        /// Requested id.
        stable_id: String,
        /// The requested sanitised alias.
        requested_alias: String,
        /// The other drive's stable_id that collides.
        colliding_stable_id: String,
    },
    /// Wrapper's umount (before rename) exited non-zero.
    #[error(
        "stable_id {stable_id:?} umount before rename failed: exit {exit_code}: {stderr}"
    )]
    UmountBeforeRenameFailed {
        /// Requested id.
        stable_id: String,
        /// Wrapper exit code.
        exit_code: i32,
        /// Captured stderr.
        stderr: String,
    },
    /// The atomic tmp+rename write of aliases.toml failed.
    #[error("stable_id {stable_id:?} alias persist failed: {message}")]
    AliasPersistFailed {
        /// Requested id.
        stable_id: String,
        /// AliasStoreError message.
        message: String,
    },
    /// After reconcile + rename the drive could not be re-
    /// located by identity tuple. Shouldn't happen in practice
    /// — indicates the drive was unplugged mid-rename.
    #[error(
        "stable_id {stable_id:?} could not be re-located after rename reconcile; drive unplugged?"
    )]
    PostRenameLookupFailed {
        /// Requested id.
        stable_id: String,
    },
}

/// Fine-grained repair refusal classes.
#[derive(Debug, Clone, thiserror::Error)]
pub enum RepairRefuseClass {
    /// Stable-id not in the classifier's current output.
    #[error("unknown stable_id {stable_id:?}")]
    UnknownStableId {
        /// Requested id.
        stable_id: String,
    },
    /// Role is `system-*` live — repair would corrupt live FS.
    #[error(
        "stable_id {stable_id:?} is a live system partition (role={role}); \
         repair refused (offer schedule_next_boot in a future verb instead)"
    )]
    SystemLivePartition {
        /// Requested id.
        stable_id: String,
        /// Role wire string.
        role: String,
    },
    /// NTFS hiberfile present — ntfsfix refuses to touch;
    /// operator must resume + shut down Windows cleanly first.
    #[error(
        "stable_id {stable_id:?} has active Windows hiberfile; \
         resume + shut down Windows cleanly before repair"
    )]
    NtfsHiberfile {
        /// Requested id.
        stable_id: String,
    },
    /// FS not in the support matrix — no repair tool available.
    #[error("stable_id {stable_id:?} has unsupported fs_type {fs_type:?}")]
    UnsupportedFs {
        /// Requested id.
        stable_id: String,
        /// FS type reported by the classifier.
        fs_type: String,
    },
    /// Wrapper's umount (before repair) exited non-zero. Repair
    /// requires an unmounted device; operator must safe-remove
    /// --force first if consumers are still holding the drive.
    #[error(
        "stable_id {stable_id:?} umount before repair failed: exit {exit_code}: {stderr}"
    )]
    UmountBeforeRepairFailed {
        /// Requested id.
        stable_id: String,
        /// Wrapper exit code.
        exit_code: i32,
        /// Captured stderr.
        stderr: String,
    },
    /// Wrapper's fsck action exited with the per-FS "dirty
    /// still remaining" code (5). Operator options: escalate
    /// (with data-loss acknowledgement), reformat, or restore
    /// from backup.
    #[error(
        "stable_id {stable_id:?} fsck on {fs_family:?} left drive dirty: {stderr}"
    )]
    RepairFailed {
        /// Requested id.
        stable_id: String,
        /// FS family wrapper argv.
        fs_family: String,
        /// Captured stderr for diagnostics.
        stderr: String,
    },
    /// Wrapper's fsck action exited non-zero for a reason other
    /// than "still dirty" or "hiberfile" — missing binary,
    /// argv-allowlist failure, etc.
    #[error(
        "stable_id {stable_id:?} fsck subprocess exit {exit_code}: {stderr}"
    )]
    RepairSubprocessFailed {
        /// Requested id.
        stable_id: String,
        /// Wrapper exit code.
        exit_code: i32,
        /// Captured stderr.
        stderr: String,
    },
    /// Repair succeeded but the subsequent re-mount failed.
    /// Drive is clean; operator can attempt manual mount via
    /// `storage.usb.mount`.
    #[error(
        "stable_id {stable_id:?} post-repair mount failed: exit {exit_code}: {stderr}"
    )]
    PostRepairMountFailed {
        /// Requested id.
        stable_id: String,
        /// Wrapper exit code.
        exit_code: i32,
        /// Captured stderr.
        stderr: String,
    },
}

/// Fine-grained safe-remove refusal classes surfaced via
/// [`VerbDispatchError::SafeRemoveRefused`]. Maps to acceptance
/// rows in `storage.usb.v1`.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SafeRemoveRefuseClass {
    /// Stable-id not in the classifier's current output.
    #[error("unknown stable_id {stable_id:?}")]
    UnknownStableId {
        /// Requested id.
        stable_id: String,
    },
    /// Role is `system-*` live — safe-remove would kill running
    /// OS or is meaningless for swap.
    #[error(
        "stable_id {stable_id:?} is a live system partition (role={role}); safe-remove refused"
    )]
    SystemLivePartition {
        /// Requested id.
        stable_id: String,
        /// Role wire string.
        role: String,
    },
    /// Clean umount hit EBUSY and operator did not pass
    /// `force: true`. The holders vector is fuser-derived
    /// `"<pid>:<comm>"` records for operator diagnostics.
    #[error(
        "stable_id {stable_id:?} umount EBUSY; holders={holders:?} — stop consumers or retry with force"
    )]
    Busy {
        /// Requested id.
        stable_id: String,
        /// Best-effort holder list from `fuser -m` +
        /// `/proc/<pid>/comm`.
        holders: Vec<String>,
    },
    /// The wrapper's umount / umount-force subprocess exited
    /// non-zero for a reason other than EBUSY.
    #[error(
        "stable_id {stable_id:?} umount subprocess exit {exit_code}: {stderr}"
    )]
    UmountSubprocessFailed {
        /// Requested id.
        stable_id: String,
        /// Exit code from the wrapper.
        exit_code: i32,
        /// Captured stderr.
        stderr: String,
    },
}

// --------------------------------------------------------------
// Input source (classifier upstream)
// --------------------------------------------------------------

/// Triple of classifier inputs.
pub struct ClassifierInputs {
    /// `/proc/self/mountinfo`.
    pub mountinfo: String,
    /// `/proc/swaps`.
    pub swaps: String,
    /// `lsblk -J -b -o ...`.
    pub lsblk_json: String,
}

/// Abstraction over the classifier input path.
#[async_trait::async_trait]
pub trait ClassifierInputSource: Send + Sync {
    /// Read the current classifier inputs.
    async fn read_inputs(&self) -> anyhow::Result<ClassifierInputs>;
}

/// Production input source.
pub struct ProcfsAndLsblkSource;

#[async_trait::async_trait]
impl ClassifierInputSource for ProcfsAndLsblkSource {
    async fn read_inputs(&self) -> anyhow::Result<ClassifierInputs> {
        let mountinfo =
            tokio::fs::read_to_string("/proc/self/mountinfo").await?;
        let swaps = tokio::fs::read_to_string("/proc/swaps")
            .await
            .unwrap_or_default();
        let output = Command::new("lsblk")
            .args([
                "-J",
                "-b",
                "-o",
                "NAME,PKNAME,MOUNTPOINT,TRAN,TYPE,UUID,LABEL,FSTYPE,PARTUUID,VENDOR,MODEL,SERIAL,SIZE",
            ])
            .output()
            .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("lsblk exited {}: {}", output.status, stderr);
        }
        let lsblk_json = String::from_utf8(output.stdout)?;
        Ok(ClassifierInputs {
            mountinfo,
            swaps,
            lsblk_json,
        })
    }
}

// --------------------------------------------------------------
// Command runner (wrapper invocation)
// --------------------------------------------------------------

/// Result of one wrapper invocation.
#[derive(Debug, Clone)]
pub struct CommandOutcome {
    /// Process exit status.
    pub status: i32,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
}

/// Abstraction over wrapper subprocess dispatch.
#[async_trait::async_trait]
pub trait CommandRunner: Send + Sync {
    /// Run the wrapper with the supplied argv.
    async fn run_wrapper(
        &self,
        needs_sudo: bool,
        argv: &[String],
    ) -> anyhow::Result<CommandOutcome>;
}

/// Production command runner.
pub struct RealCommandRunner;

#[async_trait::async_trait]
impl CommandRunner for RealCommandRunner {
    async fn run_wrapper(
        &self,
        needs_sudo: bool,
        argv: &[String],
    ) -> anyhow::Result<CommandOutcome> {
        let mut full: Vec<String> = if needs_sudo {
            vec![
                "sudo".to_string(),
                "-n".to_string(),
                USB_WRAPPER_PATH.to_string(),
            ]
        } else {
            vec![USB_WRAPPER_PATH.to_string()]
        };
        full.extend_from_slice(argv);
        let output = Command::new(&full[0]).args(&full[1..]).output().await?;
        Ok(CommandOutcome {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }
}

/// Resolve the plugin's effective uid + gid from
/// `/proc/self/status`.
pub fn detect_service_uid_gid() -> anyhow::Result<(u32, u32)> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    let mut uid: Option<u32> = None;
    let mut gid: Option<u32> = None;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            if let Some(eff) = rest.split_whitespace().nth(1) {
                uid = eff.parse().ok();
            }
        }
        if let Some(rest) = line.strip_prefix("Gid:") {
            if let Some(eff) = rest.split_whitespace().nth(1) {
                gid = eff.parse().ok();
            }
        }
    }
    match (uid, gid) {
        (Some(u), Some(g)) => Ok((u, g)),
        _ => anyhow::bail!("/proc/self/status missing Uid: / Gid:"),
    }
}

/// Local `Option<&str>` equality helper — used by the rename
/// verb's post-rename identity match. Two `None`s compare equal.
fn opt_eq_str(a: Option<&str>, b: Option<&str>) -> bool {
    a == b
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeInputSource {
        mountinfo: String,
        swaps: String,
        lsblk_json: String,
    }

    #[async_trait::async_trait]
    impl ClassifierInputSource for FakeInputSource {
        async fn read_inputs(&self) -> anyhow::Result<ClassifierInputs> {
            Ok(ClassifierInputs {
                mountinfo: self.mountinfo.clone(),
                swaps: self.swaps.clone(),
                lsblk_json: self.lsblk_json.clone(),
            })
        }
    }

    #[derive(Clone)]
    struct FakeCommandRunner {
        outcomes: Arc<StdMutex<Vec<CommandOutcome>>>,
        seen_argv: Arc<StdMutex<Vec<Vec<String>>>>,
    }

    impl FakeCommandRunner {
        fn new(outcomes: Vec<CommandOutcome>) -> Self {
            Self {
                outcomes: Arc::new(StdMutex::new(outcomes)),
                seen_argv: Arc::new(StdMutex::new(Vec::new())),
            }
        }
    }

    #[async_trait::async_trait]
    impl CommandRunner for FakeCommandRunner {
        async fn run_wrapper(
            &self,
            _needs_sudo: bool,
            argv: &[String],
        ) -> anyhow::Result<CommandOutcome> {
            self.seen_argv.lock().unwrap().push(argv.to_vec());
            let mut outcomes = self.outcomes.lock().unwrap();
            if outcomes.is_empty() {
                Ok(CommandOutcome {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                })
            } else {
                Ok(outcomes.remove(0))
            }
        }
    }

    fn removable_stick_lsblk() -> &'static str {
        r#"{
          "blockdevices": [
            {"name":"sda","type":"disk","tran":"usb","vendor":"SanDisk","model":"Cruzer","serial":"4C530",
             "size":32000000000,
             "children":[
               {"name":"sda1","type":"part","fstype":"vfat","label":"MUSIC","size":32000000000,"partuuid":"aaaa-01"}
             ]}
          ]
        }"#
    }

    fn build_runtime(
        lsblk: &str,
        outcomes: Vec<CommandOutcome>,
    ) -> Arc<StorageUsbRuntime> {
        Arc::new(StorageUsbRuntime::with_sources(
            1000,
            1000,
            true,
            Arc::new(FakeInputSource {
                mountinfo: String::new(),
                swaps: String::new(),
                lsblk_json: lsblk.to_string(),
            }),
            Arc::new(FakeCommandRunner::new(outcomes)),
        ))
    }

    #[test]
    fn verb_recognition() {
        for v in STORAGE_USB_VERBS {
            assert!(is_storage_usb_verb(v));
        }
        assert!(!is_storage_usb_verb("storage.usb.bogus"));
    }

    #[test]
    fn verb_list_matches_manifest() {
        let m = crate::manifest();
        let resp = m
            .capabilities
            .respondent
            .as_ref()
            .expect("respondent capabilities present");
        for v in STORAGE_USB_VERBS {
            assert!(
                resp.request_types.iter().any(|s| s == v),
                "manifest request_types missing {v:?}"
            );
        }
        assert_eq!(resp.request_types.len(), STORAGE_USB_VERBS.len());
    }

    #[tokio::test]
    async fn list_drives_returns_stable_id_populated_records() {
        let rt = build_runtime(removable_stick_lsblk(), Vec::new());
        let bytes = rt
            .dispatch_verb("storage.usb.list_drives", b"{}")
            .await
            .expect("dispatch");
        let env: ListDrivesEnvelope =
            serde_json::from_slice(&bytes).expect("json");
        assert_eq!(env.v, 1);
        assert_eq!(env.drives.len(), 1);
        assert_eq!(env.drives[0].stable_id, "MUSIC");
        assert_eq!(env.drives[0].id_source.as_deref(), Some("fs_label"));
    }

    #[tokio::test]
    async fn mount_verb_returns_mounted_clean_after_reconcile_automount() {
        // Reconcile auto-mounts the removable stick before the
        // explicit mount call. The explicit mount then observes
        // the already-mounted state (idempotent path).
        let rt = build_runtime(
            removable_stick_lsblk(),
            vec![
                CommandOutcome {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
                CommandOutcome {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
            ],
        );
        rt.dispatch_verb("storage.usb.list_drives", b"{}")
            .await
            .expect("list");
        let payload = serde_json::to_vec(&MountRequest {
            stable_id: "MUSIC".to_string(),
        })
        .unwrap();
        let bytes = rt
            .dispatch_verb("storage.usb.mount", &payload)
            .await
            .expect("mount");
        let resp: MountResponse = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(resp.class, "mounted-clean");
        assert!(resp.mounted_at.ends_with("/MUSIC"));
    }

    #[tokio::test]
    async fn mount_refuses_system_live_partition() {
        let lsblk = r#"{
          "blockdevices": [
            {"name":"sda","type":"disk","tran":"usb","vendor":"Samsung","model":"T7","serial":"S6P5",
             "size":1000000000000,
             "children":[
               {"name":"sda1","type":"part","fstype":"vfat","label":"EFI","size":536870912,"partuuid":"cccc-01"},
               {"name":"sda2","type":"part","fstype":"ext4","label":"root","size":999000000000,"partuuid":"cccc-02"}
             ]}
          ]
        }"#;
        let mi = "\
27 22 8:1 / /boot/efi rw - vfat /dev/sda1 rw
28 22 8:2 / / rw - ext4 /dev/sda2 rw
";
        let rt = Arc::new(StorageUsbRuntime::with_sources(
            1000,
            1000,
            true,
            Arc::new(FakeInputSource {
                mountinfo: mi.to_string(),
                swaps: String::new(),
                lsblk_json: lsblk.to_string(),
            }),
            Arc::new(FakeCommandRunner::new(vec![])),
        ));
        let env: ListDrivesEnvelope = serde_json::from_slice(
            &rt.dispatch_verb("storage.usb.list_drives", b"{}")
                .await
                .unwrap(),
        )
        .unwrap();
        let root_id = env
            .drives
            .iter()
            .find(|d| d.role == PartitionRole::SystemRoot)
            .map(|d| d.stable_id.clone())
            .expect("root partition present");
        let payload =
            serde_json::to_vec(&MountRequest { stable_id: root_id }).unwrap();
        let err = rt
            .dispatch_verb("storage.usb.mount", &payload)
            .await
            .unwrap_err();
        match err {
            VerbDispatchError::MountRefused(
                MountRefuseClass::SystemLivePartition { .. },
            ) => {}
            other => panic!("expected SystemLivePartition, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mount_refuses_system_adjacent_without_opt_in() {
        let lsblk = r#"{
          "blockdevices": [
            {"name":"sda","type":"disk","tran":"usb","vendor":"Samsung","model":"T7","serial":"S6P5",
             "size":1000000000000,
             "children":[
               {"name":"sda1","type":"part","fstype":"vfat","label":"EFI","size":536870912,"partuuid":"cccc-01"},
               {"name":"sda2","type":"part","fstype":"ext4","label":"root","size":500000000000,"partuuid":"cccc-02"},
               {"name":"sda3","type":"part","fstype":"ext4","label":"DATA","size":499000000000,"partuuid":"cccc-03"}
             ]}
          ]
        }"#;
        let mi = "\
27 22 8:1 / /boot/efi rw - vfat /dev/sda1 rw
28 22 8:2 / / rw - ext4 /dev/sda2 rw
";
        let rt = Arc::new(StorageUsbRuntime::with_sources(
            1000,
            1000,
            true,
            Arc::new(FakeInputSource {
                mountinfo: mi.to_string(),
                swaps: String::new(),
                lsblk_json: lsblk.to_string(),
            }),
            Arc::new(FakeCommandRunner::new(vec![])),
        ));
        let env: ListDrivesEnvelope = serde_json::from_slice(
            &rt.dispatch_verb("storage.usb.list_drives", b"{}")
                .await
                .unwrap(),
        )
        .unwrap();
        let adj_id = env
            .drives
            .iter()
            .find(|d| d.role == PartitionRole::SystemAdjacent)
            .map(|d| d.stable_id.clone())
            .expect("adjacent partition present");
        let payload =
            serde_json::to_vec(&MountRequest { stable_id: adj_id }).unwrap();
        let err = rt
            .dispatch_verb("storage.usb.mount", &payload)
            .await
            .unwrap_err();
        match err {
            VerbDispatchError::MountRefused(
                MountRefuseClass::SystemAdjacentNotOptedIn { .. },
            ) => {}
            other => panic!("expected SystemAdjacentNotOptedIn, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mount_refuses_oversized_fat32() {
        let lsblk = r#"{
          "blockdevices": [
            {"name":"sdb","type":"disk","tran":"usb","vendor":"WD","model":"MyPassport","serial":"WCC7K1",
             "size":4000000000000,
             "children":[
               {"name":"sdb1","type":"part","fstype":"vfat","label":"BIGGY","size":4000000000000,"partuuid":"dddd-01"}
             ]}
          ]
        }"#;
        let rt = build_runtime(lsblk, vec![]);
        rt.dispatch_verb("storage.usb.list_drives", b"{}")
            .await
            .unwrap();
        let payload = serde_json::to_vec(&MountRequest {
            stable_id: "BIGGY".to_string(),
        })
        .unwrap();
        let err = rt
            .dispatch_verb("storage.usb.mount", &payload)
            .await
            .unwrap_err();
        match err {
            VerbDispatchError::MountRefused(
                MountRefuseClass::MountFailedOversizedVfat {
                    size_bytes,
                    cap_bytes,
                    ..
                },
            ) => {
                assert!(size_bytes > cap_bytes);
            }
            other => panic!("expected MountFailedOversizedVfat, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn all_mutating_verbs_now_implemented() {
        // All five shelf verbs are wired (Steps 3-6). Any call
        // with a bogus payload gets a structured refuse class,
        // never NotImplemented.
        let rt = build_runtime(r#"{"blockdevices":[]}"#, vec![]);
        for verb in [
            "storage.usb.mount",
            "storage.usb.safe_remove",
            "storage.usb.repair_filesystem",
            "storage.usb.rename",
        ] {
            let err = rt
                .dispatch_verb(verb, br#"{"stable_id":"missing","alias":""}"#)
                .await
                .unwrap_err();
            if let VerbDispatchError::NotImplemented { .. } = err {
                panic!("verb {verb} still marked NotImplemented");
            }
        }
    }

    #[tokio::test]
    async fn rename_refuses_unknown_stable_id() {
        let rt = build_runtime(r#"{"blockdevices":[]}"#, vec![]);
        let payload = serde_json::to_vec(&RenameRequest {
            stable_id: "no-such-drive".to_string(),
            alias: "My-Music".to_string(),
            mount_policy: None,
        })
        .unwrap();
        let err = rt
            .dispatch_verb("storage.usb.rename", &payload)
            .await
            .unwrap_err();
        match err {
            VerbDispatchError::RenameRefused(
                RenameRefuseClass::UnknownStableId { .. },
            ) => {}
            other => panic!("expected UnknownStableId, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rename_refuses_alias_that_sanitises_to_empty() {
        let rt = build_runtime(
            removable_stick_lsblk(),
            vec![CommandOutcome {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            }],
        );
        rt.dispatch_verb("storage.usb.list_drives", b"{}")
            .await
            .unwrap();
        let payload = serde_json::to_vec(&RenameRequest {
            stable_id: "MUSIC".to_string(),
            alias: "!!! @@@ ###".to_string(),
            mount_policy: None,
        })
        .unwrap();
        let err = rt
            .dispatch_verb("storage.usb.rename", &payload)
            .await
            .unwrap_err();
        match err {
            VerbDispatchError::RenameRefused(
                RenameRefuseClass::InvalidAlias { .. },
            ) => {}
            other => panic!("expected InvalidAlias, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rename_refuses_system_live_partition() {
        let lsblk = r#"{
          "blockdevices": [
            {"name":"sda","type":"disk","tran":"usb","vendor":"Samsung","model":"T7","serial":"S6P5",
             "size":1000000000000,
             "children":[
               {"name":"sda2","type":"part","fstype":"ext4","label":"root","size":999000000000,"partuuid":"cccc-02"}
             ]}
          ]
        }"#;
        let mi = "28 22 8:2 / / rw - ext4 /dev/sda2 rw\n";
        let rt = Arc::new(StorageUsbRuntime::with_sources(
            1000,
            1000,
            true,
            Arc::new(FakeInputSource {
                mountinfo: mi.to_string(),
                swaps: String::new(),
                lsblk_json: lsblk.to_string(),
            }),
            Arc::new(FakeCommandRunner::new(vec![])),
        ));
        let env: ListDrivesEnvelope = serde_json::from_slice(
            &rt.dispatch_verb("storage.usb.list_drives", b"{}")
                .await
                .unwrap(),
        )
        .unwrap();
        let root_id = env
            .drives
            .iter()
            .find(|d| d.role == PartitionRole::SystemRoot)
            .map(|d| d.stable_id.clone())
            .expect("root partition present");
        let payload = serde_json::to_vec(&RenameRequest {
            stable_id: root_id,
            alias: "MyRoot".to_string(),
            mount_policy: None,
        })
        .unwrap();
        let err = rt
            .dispatch_verb("storage.usb.rename", &payload)
            .await
            .unwrap_err();
        match err {
            VerbDispatchError::RenameRefused(
                RenameRefuseClass::SystemLivePartition { .. },
            ) => {}
            other => panic!("expected SystemLivePartition, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rename_refuses_alias_that_would_collide() {
        // Two identical sticks plugged; each derives MUSIC / MUSIC-2
        // via deconflict. Renaming MUSIC-2 to MUSIC (the sibling's
        // current id) must refuse.
        let lsblk = r#"{
          "blockdevices": [
            {"name":"sda","type":"disk","tran":"usb","vendor":"SanDisk","model":"Cruzer","serial":"4C530",
             "size":32000000000,
             "children":[
               {"name":"sda1","type":"part","fstype":"vfat","label":"MUSIC","size":32000000000,"partuuid":"aaaa-01"}
             ]},
            {"name":"sdb","type":"disk","tran":"usb","vendor":"SanDisk","model":"Cruzer","serial":"7B221",
             "size":32000000000,
             "children":[
               {"name":"sdb1","type":"part","fstype":"vfat","label":"MUSIC","size":32000000000,"partuuid":"bbbb-01"}
             ]}
          ]
        }"#;
        let rt = build_runtime(
            lsblk,
            vec![
                CommandOutcome {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
                CommandOutcome {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
            ],
        );
        rt.dispatch_verb("storage.usb.list_drives", b"{}")
            .await
            .unwrap();
        // The second drive got MUSIC-2 via deconflict; try to rename
        // it to MUSIC (colliding with the first drive).
        let payload = serde_json::to_vec(&RenameRequest {
            stable_id: "MUSIC-2".to_string(),
            alias: "MUSIC".to_string(),
            mount_policy: None,
        })
        .unwrap();
        let err = rt
            .dispatch_verb("storage.usb.rename", &payload)
            .await
            .unwrap_err();
        match err {
            VerbDispatchError::RenameRefused(
                RenameRefuseClass::AliasWouldCollide { .. },
            ) => {}
            other => panic!("expected AliasWouldCollide, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn repair_refuses_unknown_stable_id() {
        let rt = build_runtime(r#"{"blockdevices":[]}"#, vec![]);
        let payload = serde_json::to_vec(&RepairRequest {
            stable_id: "no-such-drive".to_string(),
            escalate: None,
        })
        .unwrap();
        let err = rt
            .dispatch_verb("storage.usb.repair_filesystem", &payload)
            .await
            .unwrap_err();
        match err {
            VerbDispatchError::RepairRefused(
                RepairRefuseClass::UnknownStableId { .. },
            ) => {}
            other => panic!("expected UnknownStableId, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn repair_refuses_system_live_partition() {
        let lsblk = r#"{
          "blockdevices": [
            {"name":"sda","type":"disk","tran":"usb","vendor":"Samsung","model":"T7","serial":"S6P5",
             "size":1000000000000,
             "children":[
               {"name":"sda2","type":"part","fstype":"ext4","label":"root","size":999000000000,"partuuid":"cccc-02"}
             ]}
          ]
        }"#;
        let mi = "28 22 8:2 / / rw - ext4 /dev/sda2 rw\n";
        let rt = Arc::new(StorageUsbRuntime::with_sources(
            1000,
            1000,
            true,
            Arc::new(FakeInputSource {
                mountinfo: mi.to_string(),
                swaps: String::new(),
                lsblk_json: lsblk.to_string(),
            }),
            Arc::new(FakeCommandRunner::new(vec![])),
        ));
        let env: ListDrivesEnvelope = serde_json::from_slice(
            &rt.dispatch_verb("storage.usb.list_drives", b"{}")
                .await
                .unwrap(),
        )
        .unwrap();
        let root_id = env
            .drives
            .iter()
            .find(|d| d.role == PartitionRole::SystemRoot)
            .map(|d| d.stable_id.clone())
            .expect("root partition present");
        let payload = serde_json::to_vec(&RepairRequest {
            stable_id: root_id,
            escalate: None,
        })
        .unwrap();
        let err = rt
            .dispatch_verb("storage.usb.repair_filesystem", &payload)
            .await
            .unwrap_err();
        match err {
            VerbDispatchError::RepairRefused(
                RepairRefuseClass::SystemLivePartition { .. },
            ) => {}
            other => panic!("expected SystemLivePartition, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn repair_happy_path_clean_umount_fsck_remount() {
        // Wrapper outcomes: auto-mount (0), umount (0), fsck (0),
        // re-mount (0). Result: repaired=true, before=mounted-clean
        // (since MUSIC comes up clean via reconcile automount),
        // after=mounted-clean.
        let rt = build_runtime(
            removable_stick_lsblk(),
            vec![
                CommandOutcome {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
                CommandOutcome {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
                CommandOutcome {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
                CommandOutcome {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
            ],
        );
        rt.dispatch_verb("storage.usb.list_drives", b"{}")
            .await
            .unwrap();
        let payload = serde_json::to_vec(&RepairRequest {
            stable_id: "MUSIC".to_string(),
            escalate: None,
        })
        .unwrap();
        let bytes = rt
            .dispatch_verb("storage.usb.repair_filesystem", &payload)
            .await
            .expect("repair");
        let resp: RepairResponse = serde_json::from_slice(&bytes).unwrap();
        assert!(resp.repaired);
        assert_eq!(resp.after_class, "mounted-clean");
    }

    #[tokio::test]
    async fn repair_fsck_dirty_remaining_returns_repair_failed() {
        // Wrapper outcomes: auto-mount (0), umount (0), fsck exits
        // 5 (dirty remaining). Runtime returns RepairFailed and
        // marks drive class=mount-failed-dirty.
        let rt = build_runtime(
            removable_stick_lsblk(),
            vec![
                CommandOutcome {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
                CommandOutcome {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
                CommandOutcome {
                    status: 5,
                    stdout: String::new(),
                    stderr: "fsck failed".to_string(),
                },
            ],
        );
        rt.dispatch_verb("storage.usb.list_drives", b"{}")
            .await
            .unwrap();
        let payload = serde_json::to_vec(&RepairRequest {
            stable_id: "MUSIC".to_string(),
            escalate: None,
        })
        .unwrap();
        let err = rt
            .dispatch_verb("storage.usb.repair_filesystem", &payload)
            .await
            .unwrap_err();
        match err {
            VerbDispatchError::RepairRefused(
                RepairRefuseClass::RepairFailed { .. },
            ) => {}
            other => panic!("expected RepairFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn repair_ntfs_hiberfile_returns_ntfs_hiberfile() {
        // NTFS drive; fsck wrapper exits 6 (hiberfile). Runtime
        // marks class=mounted-dirty-hiberfile and refuses.
        let lsblk = r#"{
          "blockdevices": [
            {"name":"sdc","type":"disk","tran":"usb","vendor":"WD","model":"Elements","serial":"WCC9",
             "size":500000000000,
             "children":[
               {"name":"sdc1","type":"part","fstype":"ntfs","label":"WINDATA","size":500000000000,"partuuid":"eeee-01"}
             ]}
          ]
        }"#;
        let rt = build_runtime(
            lsblk,
            vec![
                CommandOutcome {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
                CommandOutcome {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
                CommandOutcome {
                    status: 6,
                    stdout: String::new(),
                    stderr: "hiberfil.sys detected".to_string(),
                },
            ],
        );
        rt.dispatch_verb("storage.usb.list_drives", b"{}")
            .await
            .unwrap();
        let payload = serde_json::to_vec(&RepairRequest {
            stable_id: "WINDATA".to_string(),
            escalate: None,
        })
        .unwrap();
        let err = rt
            .dispatch_verb("storage.usb.repair_filesystem", &payload)
            .await
            .unwrap_err();
        match err {
            VerbDispatchError::RepairRefused(
                RepairRefuseClass::NtfsHiberfile { .. },
            ) => {}
            other => panic!("expected NtfsHiberfile, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn safe_remove_refuses_unknown_stable_id() {
        let rt = build_runtime(r#"{"blockdevices":[]}"#, vec![]);
        let payload = serde_json::to_vec(&SafeRemoveRequest {
            stable_id: "not-a-real-drive".to_string(),
            force: None,
        })
        .unwrap();
        let err = rt
            .dispatch_verb("storage.usb.safe_remove", &payload)
            .await
            .unwrap_err();
        match err {
            VerbDispatchError::SafeRemoveRefused(
                SafeRemoveRefuseClass::UnknownStableId { .. },
            ) => {}
            other => panic!("expected UnknownStableId, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn safe_remove_refuses_system_live_partition() {
        let lsblk = r#"{
          "blockdevices": [
            {"name":"sda","type":"disk","tran":"usb","vendor":"Samsung","model":"T7","serial":"S6P5",
             "size":1000000000000,
             "children":[
               {"name":"sda2","type":"part","fstype":"ext4","label":"root","size":999000000000,"partuuid":"cccc-02"}
             ]}
          ]
        }"#;
        let mi = "28 22 8:2 / / rw - ext4 /dev/sda2 rw\n";
        let rt = Arc::new(StorageUsbRuntime::with_sources(
            1000,
            1000,
            true,
            Arc::new(FakeInputSource {
                mountinfo: mi.to_string(),
                swaps: String::new(),
                lsblk_json: lsblk.to_string(),
            }),
            Arc::new(FakeCommandRunner::new(vec![])),
        ));
        let env: ListDrivesEnvelope = serde_json::from_slice(
            &rt.dispatch_verb("storage.usb.list_drives", b"{}")
                .await
                .unwrap(),
        )
        .unwrap();
        let root_id = env
            .drives
            .iter()
            .find(|d| d.role == PartitionRole::SystemRoot)
            .map(|d| d.stable_id.clone())
            .expect("root partition present");
        let payload = serde_json::to_vec(&SafeRemoveRequest {
            stable_id: root_id,
            force: None,
        })
        .unwrap();
        let err = rt
            .dispatch_verb("storage.usb.safe_remove", &payload)
            .await
            .unwrap_err();
        match err {
            VerbDispatchError::SafeRemoveRefused(
                SafeRemoveRefuseClass::SystemLivePartition { .. },
            ) => {}
            other => panic!("expected SystemLivePartition, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn safe_remove_umount_ebusy_returns_busy_without_force() {
        // Reconcile auto-mount runs one wrapper call → success.
        // Then explicit safe_remove umount → EBUSY (exit 4).
        // Since force=false, the runtime returns Busy without
        // calling umount-force.
        let rt = build_runtime(
            removable_stick_lsblk(),
            vec![
                CommandOutcome {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
                CommandOutcome {
                    status: 4,
                    stdout: String::new(),
                    stderr: "target is busy".to_string(),
                },
            ],
        );
        // Prime the registry — reconcile mounts MUSIC via first
        // outcome (exit 0).
        rt.dispatch_verb("storage.usb.list_drives", b"{}")
            .await
            .unwrap();
        let payload = serde_json::to_vec(&SafeRemoveRequest {
            stable_id: "MUSIC".to_string(),
            force: Some(false),
        })
        .unwrap();
        let err = rt
            .dispatch_verb("storage.usb.safe_remove", &payload)
            .await
            .unwrap_err();
        match err {
            VerbDispatchError::SafeRemoveRefused(
                SafeRemoveRefuseClass::Busy { .. },
            ) => {}
            other => panic!("expected Busy, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn safe_remove_force_escalates_to_lazy_detach() {
        // Wrapper outcomes: auto-mount (0) → clean umount fails
        // EBUSY (4) → umount-force succeeds (0) → eject best-effort
        // (0). Result: removed=true, forced=Some(true).
        let rt = build_runtime(
            removable_stick_lsblk(),
            vec![
                CommandOutcome {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
                CommandOutcome {
                    status: 4,
                    stdout: String::new(),
                    stderr: "target is busy".to_string(),
                },
                CommandOutcome {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
                CommandOutcome {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
            ],
        );
        rt.dispatch_verb("storage.usb.list_drives", b"{}")
            .await
            .unwrap();
        let payload = serde_json::to_vec(&SafeRemoveRequest {
            stable_id: "MUSIC".to_string(),
            force: Some(true),
        })
        .unwrap();
        let bytes = rt
            .dispatch_verb("storage.usb.safe_remove", &payload)
            .await
            .expect("safe_remove force");
        let resp: SafeRemoveResponse = serde_json::from_slice(&bytes).unwrap();
        assert!(resp.removed);
        assert_eq!(resp.forced, Some(true));
    }

    #[tokio::test]
    async fn safe_remove_clean_umount_success() {
        let rt = build_runtime(
            removable_stick_lsblk(),
            vec![
                CommandOutcome {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
                CommandOutcome {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
                CommandOutcome {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                },
            ],
        );
        rt.dispatch_verb("storage.usb.list_drives", b"{}")
            .await
            .unwrap();
        let payload = serde_json::to_vec(&SafeRemoveRequest {
            stable_id: "MUSIC".to_string(),
            force: None,
        })
        .unwrap();
        let bytes = rt
            .dispatch_verb("storage.usb.safe_remove", &payload)
            .await
            .expect("safe_remove");
        let resp: SafeRemoveResponse = serde_json::from_slice(&bytes).unwrap();
        assert!(resp.removed);
        assert_eq!(resp.forced, Some(false));
    }

    #[test]
    fn subject_addressing_shape_stable() {
        let a = storage_usb_drives_addressing();
        assert_eq!(a.scheme, "evo.storage.usb.drives");
        assert_eq!(a.value, "local");
    }

    #[test]
    fn drive_class_wire_strings() {
        assert_eq!(DriveClass::SystemDisk.wire_str(), "system-disk");
        assert_eq!(DriveClass::MountedClean.wire_str(), "mounted-clean");
        assert_eq!(
            DriveClass::MountFailedOversizedVfat.wire_str(),
            "mount-failed-oversized-vfat"
        );
    }
}
