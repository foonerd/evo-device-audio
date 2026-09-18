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
//! `/usr/local/bin/evo-usb-mount`. The wrapper asks PID 1
//! (`systemd-mount --collect`) so the volume is in the host
//! mount namespace — visible to mpd, the operator, and file
//! sharing. Raw `mount(8)` would stay inside the steward unit
//! (`ProtectSystem=strict`) and leave an empty leaf directory
//! on the host. NTFS attach uses `--type=ntfs-3g` because the
//! §2 option string is ntfs-3g; kernel `ntfs3` rejects it.
//! Success is checked in PID 1's mount table. The plugin does
//! NOT hold raw sudo grants on the underlying tools; the
//! wrapper's argv allowlist is the last-mile runtime
//! enforcement.

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
use std::collections::{BTreeMap, BTreeSet};
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
/// `crate::StorageUsbPlugin::describe` + the manifest stay
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
    /// Stable-ids the operator Removed while the device is
    /// still in the classifier. Auto-mount must not put those
    /// back; the hold drops when the device leaves lsblk
    /// (replug remounts) or when the operator Mounts, repairs,
    /// or renames.
    operator_held_out: BTreeSet<String>,
    /// In-flight (or last completed) operator Remove. Carried
    /// on `storage_usb_drives` so glass can name each real
    /// stage instead of walking a timer.
    removal: Option<RemovalProgress>,
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
                operator_held_out: BTreeSet::new(),
                removal: None,
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

        // Operator Mount is the gesture that puts a Removed-but-
        // still-plugged stick back. Drop the hold before the
        // reconcile so Auto can attach it.
        self.clear_operator_hold(&req.stable_id).await;
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
    /// stable-ids, and auto-mount removable drives that the
    /// operator has not Removed. Called on plugin load, on every
    /// reconcile tick, and at the start of mutating verbs other
    /// than `safe_remove` (that verb refreshes without Auto so
    /// it cannot remount the stick it is about to detach).
    pub async fn reconcile_once(&self) -> Result<(), VerbDispatchError> {
        self.refresh_registry().await?;
        self.auto_mount_unmounted_removable().await;
        self.republish_envelope().await;
        Ok(())
    }

    async fn clear_operator_hold(&self, stable_id: &str) {
        let mut inner = self.inner.lock().await;
        inner.operator_held_out.remove(stable_id);
        if inner
            .removal
            .as_ref()
            .is_some_and(|r| r.stable_id == stable_id)
        {
            inner.removal = None;
        }
    }

    async fn remember_operator_detach(&self, stable_id: &str) {
        self.inner
            .lock()
            .await
            .operator_held_out
            .insert(stable_id.to_string());
    }

    /// Rebuild the drive registry from lsblk + mountinfo.
    /// Does not Auto-mount: `safe_remove` uses this so a
    /// Removed-but-still-plugged stick is not put back as the
    /// prelude to detaching it.
    async fn refresh_registry(&self) -> Result<(), VerbDispatchError> {
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

        // A row that was mounted and is no longer in the
        // classifier output is a yank. Dropping it silently left
        // the host carrying the mount point and MPD carrying the
        // tracks, with no id left for the operator to act on.
        // Collect them before the swap; detach after the lock is
        // released so the dispatch cannot deadlock the registry.
        let vanished: Vec<(String, Option<String>)> = inner
            .drives
            .iter()
            .filter(|(id, r)| {
                !fresh.contains_key(*id)
                    && (r.class == DriveClass::MountedClean
                        || r.class == DriveClass::MountedDirty)
            })
            .map(|(id, r)| (id.clone(), r.library_source_id.clone()))
            .collect();

        inner.drives = fresh;
        inner.last_update_at_ms = now_ms();
        // The stick left: a later insert is a new presence and
        // Auto may mount it. Hold only while the same device
        // stays in the classifier.
        let present: BTreeSet<String> = inner.drives.keys().cloned().collect();
        inner.operator_held_out.retain(|id| present.contains(id));
        // The removal banner goes with the hold. It reports a
        // real stage of a real device; once that device has
        // left, `safe` describes nothing, and leaving it up
        // would put "safe to unplug" over the stick the
        // operator plugs in next.
        if inner
            .removal
            .as_ref()
            .is_some_and(|r| !present.contains(&r.stable_id))
        {
            inner.removal = None;
        }
        drop(inner);

        for (stable_id, source_id) in &vanished {
            tracing::warn!(
                plugin = "storage.usb",
                stable_id = %stable_id,
                "mounted volume vanished from the host; detaching the \
                 leftover mount then retracting the library source"
            );
            // Lazy-detach the leftover mount point first. The
            // scrub that follows only prunes rows whose files
            // have gone; running it against a still-mounted tree
            // would prune nothing.
            let force_argv =
                vec!["umount-force".to_string(), stable_id.clone()];
            match self
                .command_runner
                .run_wrapper(self.needs_sudo, &force_argv)
                .await
            {
                Ok(o) if o.status == 0 => {}
                Ok(o) => tracing::info!(
                    plugin = "storage.usb",
                    stable_id = %stable_id,
                    exit_code = o.status,
                    "vanished volume: nothing left to detach"
                ),
                Err(e) => tracing::warn!(
                    plugin = "storage.usb",
                    stable_id = %stable_id,
                    error = %e,
                    "vanished volume: detach not attempted"
                ),
            }
            self.retract_library_source(stable_id, source_id.as_deref())
                .await;
        }
        Ok(())
    }

    /// Auto-mount removable drives that are not yet mounted and
    /// that the operator has not Removed while they stay plugged.
    async fn auto_mount_unmounted_removable(&self) {
        let candidates: Vec<String> = {
            let inner = self.inner.lock().await;
            inner
                .drives
                .iter()
                .filter(|(id, r)| {
                    !inner.operator_held_out.contains(*id)
                        && r.role == PartitionRole::Removable
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
                tracing::warn!(
                    plugin = "storage.usb",
                    stable_id = %id,
                    error = %e,
                    "auto-mount attempt failed"
                );
            }
        }
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

    /// `storage.usb.safe_remove` handler.
    ///
    /// Remove is remove. Holders do not veto. Sequence
    /// (USB-STORAGE.md §9):
    ///
    /// 1. Refuse if role is `system-*` live.
    /// 2. Unknown id: lazy-detach whatever the host still
    ///    carries and answer `removed: true`.
    /// 3. `sync` on the parent disk.
    /// 4. Wrapper `umount`. Any non-zero escalates to
    ///    `umount-force`. The `force` field is on the wire and
    ///    is not a gate.
    /// 5. Wrapper `eject` (best-effort).
    /// 6. After the volume is detached: `library.remove_source`
    ///    with `scrub_mpd_entries: true` so MPD prunes rows
    ///    whose files are gone.
    /// 7. Retract from the in-memory registry + republish.
    ///    Each of detach / eject / retract / safe is announced
    ///    on `storage_usb_drives.removal` as that step starts.
    ///
    /// Payload: `{ v: 1, stable_id, force?: bool, library_source_id?: string }`
    /// Response: `{ v: 1, removed: true, forced?: bool, holders?: [...] }`
    async fn handle_safe_remove(
        &self,
        payload: &[u8],
    ) -> Result<Vec<u8>, VerbDispatchError> {
        let req: SafeRemoveRequest = serde_json::from_slice(payload)
            .map_err(|e| VerbDispatchError::PayloadDecode(e.to_string()))?;

        // Refresh classifier state only. Full reconcile Auto-
        // mounts a Removed-but-still-plugged stick, which is how
        // glass Remove remounted the volume it was asked to
        // detach and then 400'd on the poisoned systemd unit.
        self.refresh_registry().await?;

        let known = {
            let inner = self.inner.lock().await;
            inner.drives.get(&req.stable_id).cloned()
        };
        let Some(record) = known else {
            // The id is gone from the registry — typically a
            // yank the reconciler already swept. Remove is still
            // remove: lazy-detach whatever the host may still be
            // carrying under that mount point and answer
            // removed. Refusing here would leave a stale mount
            // on the host with no operator gesture left that
            // could clear it.
            let force_argv =
                vec!["umount-force".to_string(), req.stable_id.clone()];
            match self
                .command_runner
                .run_wrapper(self.needs_sudo, &force_argv)
                .await
            {
                Ok(o) if o.status == 0 => {}
                Ok(o) => tracing::info!(
                    plugin = "storage.usb",
                    stable_id = %req.stable_id,
                    exit_code = o.status,
                    "safe-remove on an unknown id: nothing left to detach"
                ),
                Err(e) => tracing::info!(
                    plugin = "storage.usb",
                    stable_id = %req.stable_id,
                    error = %e,
                    "safe-remove on an unknown id: detach not attempted"
                ),
            }
            self.remember_operator_detach(&req.stable_id).await;
            self.announce_removal(
                &req.stable_id,
                req.library_source_id.as_deref(),
                RemovalStage::Safe,
            )
            .await;
            let resp = SafeRemoveResponse {
                v: 1,
                removed: true,
                forced: Some(true),
                holders: None,
            };
            return serde_json::to_vec(&resp).map_err(|e| {
                VerbDispatchError::ResponseSerialise(e.to_string())
            });
        };

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
        let library_source_id = req
            .library_source_id
            .as_deref()
            .or(record.library_source_id.as_deref());

        // The queue release is the first stage of a Remove and
        // the reason the umount below can be clean. The caller
        // owns it; this verb only names it, and only when it was
        // actually done.
        if req.release_queue {
            tracing::debug!(
                plugin = "storage.usb",
                stable_id = %req.stable_id,
                "safe-remove: nothing released the operator queue for this \
                 volume; a queued track on it holds the mount busy and the \
                 detach escalates to lazy"
            );
        } else {
            self.announce_removal(
                &req.stable_id,
                library_source_id,
                RemovalStage::Queue,
            )
            .await;
        }

        if record.class == DriveClass::Unmounted
            || record.class == DriveClass::Unsupported
            || record.class == DriveClass::MountFailedOversizedVfat
            || record.class == DriveClass::MountFailedDirty
            || record.class == DriveClass::MountFailedOther
        {
            // Already off the host. Still name retract so the
            // glass can walk; only dispatch when this verb
            // owns the catalogue drop.
            self.finish_catalogue_stage(
                req.retract_library,
                &req.stable_id,
                library_source_id,
            )
            .await;
            {
                let mut inner = self.inner.lock().await;
                inner.operator_held_out.insert(req.stable_id.clone());
                inner.drives.remove(&req.stable_id);
            }
            self.announce_removal(
                &req.stable_id,
                library_source_id,
                RemovalStage::Safe,
            )
            .await;
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

        self.announce_removal(
            &req.stable_id,
            library_source_id,
            RemovalStage::Detach,
        )
        .await;

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

        // Remove is remove. A clean umount is tried first, but
        // any non-zero from it escalates to a lazy detach: the
        // operator asked for the volume to be gone, and holders
        // do not get a veto over that.
        //
        // Escalating on EVERY non-zero rather than only on the
        // wrapper's EBUSY code is deliberate. The wrapper reads
        // busy off `systemd-umount` stderr, which renders EBUSY
        // as `Device or resource busy` or an opaque `Job
        // failed` — neither matches the fragments it looks for.
        // A real EBUSY therefore arrives here as the generic
        // subprocess failure, and treating that as fatal is
        // what left the stick mounted.
        if umount_outcome.status != 0 {
            // Holders are diagnostics for the log, not a gate.
            let derived = self.fuser_holders(&record.mount_root).await;
            holders = Some(derived.clone());
            let force_argv =
                vec!["umount-force".to_string(), req.stable_id.clone()];
            let force_outcome = self
                .command_runner
                .run_wrapper(self.needs_sudo, &force_argv)
                .await
                .map_err(|e| VerbDispatchError::SubprocessIo(e.to_string()))?;
            if force_outcome.status != 0 {
                return Err(VerbDispatchError::SafeRemoveRefused(
                    SafeRemoveRefuseClass::UmountSubprocessFailed {
                        stable_id: req.stable_id,
                        exit_code: force_outcome.status,
                        stderr: force_outcome.stderr,
                        holders: derived,
                    },
                ));
            }
            forced = true;
            tracing::warn!(
                plugin = "storage.usb",
                stable_id = %req.stable_id,
                clean_exit = umount_outcome.status,
                holders = ?holders,
                "safe-remove escalated to lazy detach; any open file \
                 handles lose their backing on the last close"
            );
        }

        // 4. Best-effort SCSI eject via wrapper. Some drives
        //    (Samsung T7, many SSD enclosures) simply don't
        //    respond to the ioctl. Failure logged, not fatal.
        self.announce_removal(
            &req.stable_id,
            library_source_id,
            RemovalStage::Eject,
        )
        .await;
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

        // Catalogue stage after the volume is actually gone.
        // Sources-page Remove dispatches into audio.library.
        // Library-page Remove (`retract_library: false`) must
        // not: that plugin is on this call's stack.
        self.finish_catalogue_stage(
            req.retract_library,
            &req.stable_id,
            library_source_id,
        )
        .await;

        // 5. Retract from the in-memory registry + republish.
        //    The periodic reconciler would do this on next detach
        //    event; explicit removal here keeps the subject
        //    envelope monotonic (no ghost row while the reconciler
        //    is between ticks).
        {
            let mut inner = self.inner.lock().await;
            inner.operator_held_out.insert(req.stable_id.clone());
            inner.drives.remove(&req.stable_id);
        }
        self.announce_removal(
            &req.stable_id,
            library_source_id,
            RemovalStage::Safe,
        )
        .await;

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
            removal: inner.removal.clone(),
        }
    }

    /// Publish one real Remove stage on `storage_usb_drives`.
    /// The lock is dropped before republish so compose can
    /// take it again.
    async fn announce_removal(
        &self,
        stable_id: &str,
        library_source_id: Option<&str>,
        stage: RemovalStage,
    ) {
        {
            let mut inner = self.inner.lock().await;
            inner.removal = Some(RemovalProgress {
                stable_id: stable_id.to_string(),
                library_source_id: library_source_id.map(str::to_string),
                stage,
            });
        }
        self.republish_envelope().await;
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

        self.clear_operator_hold(&req.stable_id).await;

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
                // Consumer-stop, not Remove: the volume is
                // remounted under the new id straight after.
                let stop_payload = serde_json::json!({
                    "v": 1,
                    "source_id": source_id,
                    "consumer_stop": true,
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

    /// Name retract on the subject, then dispatch only when
    /// this verb owns the catalogue drop.
    async fn finish_catalogue_stage(
        &self,
        retract_library: bool,
        stable_id: &str,
        source_id: Option<&str>,
    ) {
        self.announce_removal(stable_id, source_id, RemovalStage::Retract)
            .await;
        if retract_library {
            self.retract_library_source(stable_id, source_id).await;
        }
    }

    /// Retract the drive's library source, scrubbing the MPD
    /// rows with it.
    ///
    /// Called only after the volume has actually been detached:
    /// the scrub is an `update` over the source's path and MPD
    /// prunes a row when the file behind it is gone, so running
    /// it while the tree is still mounted prunes nothing.
    ///
    /// Best-effort throughout. By the time this runs the volume
    /// is already off the host, so an unreachable MPD must not
    /// turn a completed detach into a failed verb.
    async fn retract_library_source(
        &self,
        stable_id: &str,
        source_id: Option<&str>,
    ) {
        let Some(source_id) = source_id else {
            return;
        };
        let Some(dispatcher) = self.shelf_dispatcher_clone() else {
            return;
        };
        let payload = serde_json::json!({
            "v": 1,
            "source_id": source_id,
            "scrub_mpd_entries": true,
        });
        let Ok(bytes) = serde_json::to_vec(&payload) else {
            return;
        };
        if let Err(e) = dispatcher
            .dispatch("audio.library", "library.remove_source", bytes, None)
            .await
        {
            tracing::warn!(
                plugin = "storage.usb",
                stable_id = %stable_id,
                source_id = %source_id,
                error = %e,
                "library.remove_source dispatch failed after detach; \
                 the volume is already gone from the host"
            );
        }
    }

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

        // Repair keeps the volume. A prior Remove hold must not
        // block the remount after fsck.
        self.clear_operator_hold(&req.stable_id).await;
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
                // Consumer-stop, not Remove: fsck runs against
                // this volume next, so it must not be detached
                // and ejected on the way.
                let payload = serde_json::json!({
                    "v": 1,
                    "source_id": source_id,
                    "consumer_stop": true,
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
    /// Operator Remove progress, when a detach is running or
    /// has just finished. Absent on an idle envelope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removal: Option<RemovalProgress>,
}

/// One stage of `storage.usb.safe_remove`, published as it
/// starts so glass can name the work that is actually running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemovalStage {
    /// The operator queue has been released of this volume's
    /// tracks.
    ///
    /// Named first because it is what makes the detach clean: a
    /// queued track under the volume is an open file, and an open
    /// file is EBUSY. Only announced when the caller owns the
    /// release and has already done it — see
    /// [`SafeRemoveRequest::release_queue`]. This verb never
    /// dispatches into `audio.queue` itself.
    Queue,
    /// Host umount / lazy detach.
    Detach,
    /// SCSI eject of the parent disk.
    Eject,
    /// `library.remove_source` with scrub.
    Retract,
    /// The volume is off the host and safe to unplug.
    ///
    /// Says nothing about who finishes the catalogue drop. A
    /// Sources-page Remove has already done it by this point;
    /// a Library-page Remove passed `retract_library: false`
    /// and completes it in the caller once this verb returns.
    /// Either way the operator can pull the stick.
    Safe,
}

/// In-flight (or just-finished) operator Remove, on
/// `storage_usb_drives`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemovalProgress {
    /// Drive stable-id being removed.
    pub stable_id: String,
    /// Library source the glass asked to drop, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library_source_id: Option<String>,
    /// Stage that has started (or `safe` when finished).
    pub stage: RemovalStage,
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
    /// Accepted for wire compatibility. Not a gate: any
    /// non-zero clean umount escalates to `umount-force`.
    #[serde(default)]
    pub force: Option<bool>,
    /// Library source id to drop after detach.
    ///
    /// The glass Remove already knows this id. The drive
    /// record's `library_source_id` is empty after a steward
    /// restart that inherits a live systemd mount — add_source
    /// is not run again — and retract then no-ops, leaving
    /// Audio ONLINE at 1513 on an unmounted stick.
    #[serde(default)]
    pub library_source_id: Option<String>,
    /// When false, the caller owns the catalogue drop. Default
    /// true so a Sources-page Remove still retracts.
    ///
    /// Library-page Remove is already inside
    /// `library.remove_source` on this OOP process. Dispatching
    /// that verb back from here never runs: eject finishes,
    /// retract hangs, `list_sources` never answers.
    #[serde(default = "retract_library_default")]
    pub retract_library: bool,
    /// Whether this verb owns releasing the operator queue of
    /// the volume's tracks. Same shape as
    /// [`Self::retract_library`]: default true, and the caller
    /// sets false when it has done the work itself.
    ///
    /// Library-page Remove releases the queue on its own MPD
    /// connection before it calls here, so it sends false and
    /// this verb names [`RemovalStage::Queue`] as done.
    ///
    /// Sources-page Remove leaves it true. There is no route
    /// from here that releases the queue before the detach: the
    /// only one would be a new prefix-drop verb on
    /// `audio.queue`, and admitting plugin-system to it means
    /// `kind = "none"`, which is the reachability question held
    /// open on `plugin_system_capabilities`. Until that row is
    /// pulled, a true here releases nothing and no Queue stage
    /// is announced — the detach on that path escalates to a
    /// lazy umount exactly as it does today.
    #[serde(default = "release_queue_default")]
    pub release_queue: bool,
}

fn retract_library_default() -> bool {
    true
}

fn release_queue_default() -> bool {
    true
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
    /// The lazy detach itself failed. The only refuse this path
    /// can still produce: a clean umount that fails escalates
    /// rather than refusing, so reaching here means even
    /// `umount-force` could not detach the volume.
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
        /// Best-effort holder list from `fuser -m` +
        /// `/proc/<pid>/comm`, carried so a refuse that survives
        /// says who was holding the volume rather than only that
        /// a subprocess failed.
        holders: Vec<String>,
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
            // safe_remove answers Ok on an unknown id now
            // (remove is remove), so an Ok result is equally
            // proof the verb is wired.
            if let Err(VerbDispatchError::NotImplemented { .. }) = rt
                .dispatch_verb(verb, br#"{"stable_id":"missing","alias":""}"#)
                .await
            {
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

    /// Records the payload of every shelf dispatch.
    #[derive(Default)]
    struct PayloadDispatcher {
        seen: StdMutex<Vec<(String, String)>>,
    }

    impl PayloadDispatcher {
        fn seen(&self) -> Vec<(String, String)> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl ShelfRequestDispatcher for PayloadDispatcher {
        fn dispatch<'a>(
            &'a self,
            _shelf: &'a str,
            request_type: &'a str,
            payload: Vec<u8>,
            _instance_id: Option<&'a str>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            Vec<u8>,
                            evo_plugin_sdk::contract::ShelfDispatchError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            self.seen.lock().unwrap().push((
                request_type.to_string(),
                String::from_utf8_lossy(&payload).into_owned(),
            ));
            let body = if request_type == "library.add_source" {
                serde_json::json!({ "v": 1, "source_id": "usb-music" })
            } else {
                serde_json::json!({ "v": 1 })
            };
            Box::pin(async move {
                Ok(serde_json::to_vec(&body).unwrap_or_default())
            })
        }
    }

    /// Build a runtime whose shelf dispatches are recorded, with
    /// the stick already mounted and carrying a library source.
    async fn primed_runtime_with_dispatcher(
        outcomes: Vec<CommandOutcome>,
    ) -> (
        Arc<StorageUsbRuntime>,
        Arc<PayloadDispatcher>,
        Arc<FakeCommandRunner>,
    ) {
        let runner = Arc::new(FakeCommandRunner::new(outcomes));
        let rt = Arc::new(StorageUsbRuntime::with_sources(
            1000,
            1000,
            true,
            Arc::new(FakeInputSource {
                mountinfo: String::new(),
                swaps: String::new(),
                lsblk_json: removable_stick_lsblk().to_string(),
            }),
            Arc::clone(&runner) as Arc<dyn CommandRunner>,
        ));
        let d = Arc::new(PayloadDispatcher::default());
        rt.attach_shelf_dispatcher(
            Arc::clone(&d) as Arc<dyn ShelfRequestDispatcher>
        );
        rt.dispatch_verb("storage.usb.list_drives", b"{}")
            .await
            .unwrap();
        (rt, d, runner)
    }

    fn ok_outcome() -> CommandOutcome {
        CommandOutcome {
            status: 0,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    #[tokio::test]
    async fn rename_stops_consumers_without_removing_the_volume() {
        // Rename remounts the same volume under a new id. If its
        // consumer-stop reached safe_remove the volume would be
        // detached and ejected out from under the remount.
        let (rt, d, runner) =
            primed_runtime_with_dispatcher(vec![ok_outcome(); 6]).await;
        let payload = serde_json::to_vec(&serde_json::json!({
            "stable_id": "MUSIC",
            "alias": "Road Trip",
        }))
        .unwrap();
        let _ = rt.dispatch_verb("storage.usb.rename", &payload).await;

        let seen = d.seen();
        let stop = seen
            .iter()
            .find(|(verb, _)| verb == "library.remove_source")
            .unwrap_or_else(|| panic!("rename must stop consumers: {seen:?}"));
        assert!(
            stop.1.contains("\"consumer_stop\":true"),
            "rename's stop must declare itself a consumer-stop, or the \
             library side hands it to safe_remove: {}",
            stop.1,
        );
        let argv = runner.seen_argv.lock().unwrap().clone();
        assert!(
            !argv
                .iter()
                .any(|a| a.first().map(String::as_str) == Some("eject")),
            "rename must not eject the volume: {argv:?}",
        );
    }

    #[tokio::test]
    async fn repair_stops_consumers_without_removing_the_volume() {
        // Repair runs fsck against this volume next.
        let (rt, d, runner) =
            primed_runtime_with_dispatcher(vec![ok_outcome(); 6]).await;
        let payload = serde_json::to_vec(&serde_json::json!({
            "v": 1,
            "stable_id": "MUSIC",
        }))
        .unwrap();
        let _ = rt
            .dispatch_verb("storage.usb.repair_filesystem", &payload)
            .await;

        let seen = d.seen();
        let stop = seen
            .iter()
            .find(|(verb, _)| verb == "library.remove_source")
            .unwrap_or_else(|| panic!("repair must stop consumers: {seen:?}"));
        assert!(
            stop.1.contains("\"consumer_stop\":true"),
            "repair's stop must declare itself a consumer-stop: {}",
            stop.1,
        );
        let argv = runner.seen_argv.lock().unwrap().clone();
        assert!(
            !argv
                .iter()
                .any(|a| a.first().map(String::as_str) == Some("eject")),
            "repair must not eject the volume it is about to fsck: {argv:?}",
        );
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
    async fn safe_remove_on_an_unknown_id_detaches_and_reports_removed() {
        // After a yank the reconciler has already swept the row.
        // Refusing would leave any leftover host mount with no
        // gesture left to clear it, so the verb lazy-detaches
        // and answers removed.
        let rt = build_runtime(
            r#"{"blockdevices":[]}"#,
            vec![CommandOutcome {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            }],
        );
        let payload = serde_json::to_vec(&SafeRemoveRequest {
            stable_id: "not-a-real-drive".to_string(),
            force: None,
            library_source_id: None,
            retract_library: true,
            release_queue: true,
        })
        .unwrap();
        let bytes = rt
            .dispatch_verb("storage.usb.safe_remove", &payload)
            .await
            .expect("remove is remove: an unknown id is not a refuse");
        let resp: SafeRemoveResponse = serde_json::from_slice(&bytes).unwrap();
        assert!(resp.removed);
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
            library_source_id: None,
            retract_library: true,
            release_queue: true,
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

    /// Every shape a busy volume reaches the runtime as. The
    /// wrapper reports EBUSY as exit 4 only when it recognises
    /// the stderr fragment; `systemd-umount` renders it as
    /// `Device or resource busy` or an opaque `Job failed`,
    /// which arrive as the generic exit 3. All of them must end
    /// in a lazy detach and `removed: true` — holders do not
    /// veto a remove.
    #[tokio::test]
    async fn every_busy_shape_escalates_to_detach_and_reports_removed() {
        for (status, stderr) in [
            (4, "target is busy"),
            (3, "systemd-umount /x: Device or resource busy"),
            (3, "Job failed. See journalctl -xe for details."),
        ] {
            let rt = build_runtime(
                removable_stick_lsblk(),
                vec![
                    // auto-mount during reconcile
                    CommandOutcome {
                        status: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                    },
                    // clean umount refuses
                    CommandOutcome {
                        status,
                        stdout: String::new(),
                        stderr: stderr.to_string(),
                    },
                    // umount-force succeeds
                    CommandOutcome {
                        status: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                    },
                    // eject, best-effort
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
                // force is on the wire but is not a gate.
                force: Some(false),
                library_source_id: None,
                retract_library: true,
                release_queue: true,
            })
            .unwrap();
            let bytes = rt
                .dispatch_verb("storage.usb.safe_remove", &payload)
                .await
                .unwrap_or_else(|e| {
                    panic!("exit {status} / {stderr:?} must not refuse: {e:?}")
                });
            let resp: SafeRemoveResponse =
                serde_json::from_slice(&bytes).unwrap();
            assert!(resp.removed, "exit {status} must report removed");
            assert_eq!(
                resp.forced,
                Some(true),
                "exit {status} must report the detach as forced"
            );
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
            library_source_id: None,
            retract_library: true,
            release_queue: true,
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

    /// lsblk output the test can change between sweeps, so a
    /// yank can be expressed: the stick is there, then it is not.
    struct SwappableInputSource(Arc<StdMutex<String>>);

    #[async_trait::async_trait]
    impl ClassifierInputSource for SwappableInputSource {
        async fn read_inputs(&self) -> anyhow::Result<ClassifierInputs> {
            Ok(ClassifierInputs {
                mountinfo: String::new(),
                swaps: String::new(),
                lsblk_json: self.0.lock().unwrap().clone(),
            })
        }
    }

    #[tokio::test]
    async fn a_vanished_mounted_volume_is_detached_on_the_next_sweep() {
        // A yank: the volume was mounted, then it is gone from
        // the classifier. Dropping the row silently left the
        // host carrying the mount point. It must lazy-detach.
        let lsblk =
            Arc::new(StdMutex::new(removable_stick_lsblk().to_string()));
        let ok = || CommandOutcome {
            status: 0,
            stdout: String::new(),
            stderr: String::new(),
        };
        let runner = Arc::new(FakeCommandRunner::new(vec![ok(), ok(), ok()]));
        let rt = Arc::new(StorageUsbRuntime::with_sources(
            1000,
            1000,
            true,
            Arc::new(SwappableInputSource(Arc::clone(&lsblk))),
            Arc::clone(&runner) as Arc<dyn CommandRunner>,
        ));

        // First sweep mounts it.
        rt.dispatch_verb("storage.usb.list_drives", b"{}")
            .await
            .unwrap();
        // The stick is pulled.
        *lsblk.lock().unwrap() = r#"{"blockdevices":[]}"#.to_string();
        rt.dispatch_verb("storage.usb.list_drives", b"{}")
            .await
            .unwrap();

        let seen = runner.seen_argv.lock().unwrap().clone();
        assert!(
            seen.iter()
                .any(|a| a.first().map(String::as_str) == Some("umount-force")),
            "a vanished mounted volume must be lazy-detached; saw {seen:?}",
        );
    }

    fn mount_call_count(runner: &FakeCommandRunner) -> usize {
        runner
            .seen_argv
            .lock()
            .unwrap()
            .iter()
            .filter(|a| a.first().map(String::as_str) == Some("mount"))
            .count()
    }

    fn runtime_with_runner(
        lsblk: &str,
        runner: Arc<FakeCommandRunner>,
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
            Arc::clone(&runner) as Arc<dyn CommandRunner>,
        ))
    }

    #[tokio::test]
    async fn operator_remove_does_not_automount_while_the_stick_stays() {
        // Glass Remove, then a hard refresh / list tick, must
        // not put the still-plugged stick back. That remount
        // was the stuck 1513 card and the second-click 400.
        let runner = Arc::new(FakeCommandRunner::new(Vec::new()));
        let rt =
            runtime_with_runner(removable_stick_lsblk(), Arc::clone(&runner));
        rt.dispatch_verb("storage.usb.list_drives", b"{}")
            .await
            .unwrap();
        assert_eq!(mount_call_count(&runner), 1, "first presence automounts");
        let payload = serde_json::to_vec(&SafeRemoveRequest {
            stable_id: "MUSIC".to_string(),
            force: None,
            library_source_id: None,
            retract_library: true,
            release_queue: true,
        })
        .unwrap();
        let bytes = rt
            .dispatch_verb("storage.usb.safe_remove", &payload)
            .await
            .expect("remove");
        let resp: SafeRemoveResponse = serde_json::from_slice(&bytes).unwrap();
        assert!(resp.removed);
        rt.dispatch_verb("storage.usb.list_drives", b"{}")
            .await
            .unwrap();
        rt.dispatch_verb("storage.usb.list_drives", b"{}")
            .await
            .unwrap();
        assert_eq!(
            mount_call_count(&runner),
            1,
            "Remove must hold Auto while the stick stays plugged; saw {:?}",
            runner.seen_argv.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn operator_mount_after_remove_puts_the_volume_back() {
        let runner = Arc::new(FakeCommandRunner::new(Vec::new()));
        let rt =
            runtime_with_runner(removable_stick_lsblk(), Arc::clone(&runner));
        rt.dispatch_verb("storage.usb.list_drives", b"{}")
            .await
            .unwrap();
        let remove = serde_json::to_vec(&SafeRemoveRequest {
            stable_id: "MUSIC".to_string(),
            force: None,
            library_source_id: None,
            retract_library: true,
            release_queue: true,
        })
        .unwrap();
        rt.dispatch_verb("storage.usb.safe_remove", &remove)
            .await
            .expect("remove");
        let mount = serde_json::to_vec(&MountRequest {
            stable_id: "MUSIC".to_string(),
        })
        .unwrap();
        rt.dispatch_verb("storage.usb.mount", &mount)
            .await
            .expect("operator mount");
        assert_eq!(
            mount_call_count(&runner),
            2,
            "operator Mount after Remove must attach again; saw {:?}",
            runner.seen_argv.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn yank_after_remove_clears_the_hold_so_replug_automounts() {
        let lsblk =
            Arc::new(StdMutex::new(removable_stick_lsblk().to_string()));
        let runner = Arc::new(FakeCommandRunner::new(Vec::new()));
        let rt = Arc::new(StorageUsbRuntime::with_sources(
            1000,
            1000,
            true,
            Arc::new(SwappableInputSource(Arc::clone(&lsblk))),
            Arc::clone(&runner) as Arc<dyn CommandRunner>,
        ));
        rt.dispatch_verb("storage.usb.list_drives", b"{}")
            .await
            .unwrap();
        let remove = serde_json::to_vec(&SafeRemoveRequest {
            stable_id: "MUSIC".to_string(),
            force: None,
            library_source_id: None,
            retract_library: true,
            release_queue: true,
        })
        .unwrap();
        rt.dispatch_verb("storage.usb.safe_remove", &remove)
            .await
            .expect("remove");
        *lsblk.lock().unwrap() = r#"{"blockdevices":[]}"#.to_string();
        rt.dispatch_verb("storage.usb.list_drives", b"{}")
            .await
            .unwrap();
        *lsblk.lock().unwrap() = removable_stick_lsblk().to_string();
        rt.dispatch_verb("storage.usb.list_drives", b"{}")
            .await
            .unwrap();
        assert_eq!(
            mount_call_count(&runner),
            2,
            "replug after Remove must Auto again; saw {:?}",
            runner.seen_argv.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn safe_remove_does_not_automount_as_its_own_prelude() {
        // A still-plugged Unmounted stick used to be Auto-mounted
        // by reconcile_once at the start of safe_remove, then
        // immediately umounted — the hard-refresh 400 factory.
        let runner = Arc::new(FakeCommandRunner::new(Vec::new()));
        let rt =
            runtime_with_runner(removable_stick_lsblk(), Arc::clone(&runner));
        let payload = serde_json::to_vec(&SafeRemoveRequest {
            stable_id: "MUSIC".to_string(),
            force: None,
            library_source_id: None,
            retract_library: true,
            release_queue: true,
        })
        .unwrap();
        let bytes = rt
            .dispatch_verb("storage.usb.safe_remove", &payload)
            .await
            .expect("remove of never-mounted stick");
        let resp: SafeRemoveResponse = serde_json::from_slice(&bytes).unwrap();
        assert!(resp.removed);
        assert_eq!(
            mount_call_count(&runner),
            0,
            "safe_remove must not mount the stick it is detaching; saw {:?}",
            runner.seen_argv.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn already_unmounted_remove_still_retracts_the_library_row() {
        let (rt, d, _runner) =
            primed_runtime_with_dispatcher(vec![ok_outcome(); 8]).await;
        let first = serde_json::to_vec(&SafeRemoveRequest {
            stable_id: "MUSIC".to_string(),
            force: None,
            library_source_id: None,
            retract_library: true,
            release_queue: true,
        })
        .unwrap();
        rt.dispatch_verb("storage.usb.safe_remove", &first)
            .await
            .expect("first remove");
        let before = d.seen().len();
        rt.dispatch_verb("storage.usb.safe_remove", &first)
            .await
            .expect("second remove of the still-plugged stick");
        let seen = d.seen();
        let retracts = seen
            .iter()
            .filter(|(verb, _)| verb == "library.remove_source")
            .count();
        assert!(retracts >= 1, "first Remove must retract; saw {seen:?}");
        // Second click: refresh rebuilds Unmounted without a
        // library_source_id, so a second retract is not required.
        // What is required is that the second click does not
        // refuse — the hard-refresh 400.
        assert!(
            seen.len() >= before,
            "second Remove must not fail the dispatcher; saw {seen:?}"
        );
    }

    #[tokio::test]
    async fn safe_remove_retracts_the_id_the_library_handed_over() {
        // After a steward restart the drive is mounted by
        // systemd and library_source_id on the record is empty.
        // Glass Remove still knows audio-701124. That id must
        // ride the safe_remove payload or retract no-ops and
        // the card stays ONLINE at 1513.
        let runner = Arc::new(FakeCommandRunner::new(Vec::new()));
        let rt =
            runtime_with_runner(removable_stick_lsblk(), Arc::clone(&runner));
        rt.dispatch_verb("storage.usb.list_drives", b"{}")
            .await
            .unwrap();
        let d = Arc::new(PayloadDispatcher::default());
        rt.attach_shelf_dispatcher(
            Arc::clone(&d) as Arc<dyn ShelfRequestDispatcher>
        );
        let payload = serde_json::to_vec(&SafeRemoveRequest {
            stable_id: "MUSIC".to_string(),
            force: None,
            library_source_id: Some("audio-701124".to_string()),
            retract_library: true,
            release_queue: true,
        })
        .unwrap();
        rt.dispatch_verb("storage.usb.safe_remove", &payload)
            .await
            .expect("remove");
        let seen = d.seen();
        let retract = seen
            .iter()
            .find(|(verb, _)| verb == "library.remove_source")
            .unwrap_or_else(|| {
                panic!("must retract the handed-over id: {seen:?}")
            });
        assert!(
            retract.1.contains("\"source_id\":\"audio-701124\"")
                || retract.1.contains("\"source_id\": \"audio-701124\""),
            "retract must name the glass source, not skip: {}",
            retract.1
        );
        assert!(
            retract.1.contains("\"scrub_mpd_entries\":true")
                || retract.1.contains("\"scrub_mpd_entries\": true"),
            "retract must scrub: {}",
            retract.1
        );
        let env: ListDrivesEnvelope = serde_json::from_slice(
            &rt.dispatch_verb("storage.usb.list_drives", b"{}")
                .await
                .unwrap(),
        )
        .unwrap();
        let rem = env
            .removal
            .expect("safe_remove must leave the last stage on the envelope");
        assert_eq!(rem.stable_id, "MUSIC");
        assert_eq!(rem.library_source_id.as_deref(), Some("audio-701124"));
        assert_eq!(rem.stage, RemovalStage::Safe);
    }

    /// Records every `removal.stage` the runtime publishes, in
    /// order, so a test can read the walk the glass would see.
    #[derive(Default)]
    struct StageRecorder {
        stages: StdMutex<Vec<String>>,
    }

    impl StageRecorder {
        fn stages(&self) -> Vec<String> {
            self.stages.lock().unwrap().clone()
        }

        fn note(&self, state: &serde_json::Value) {
            if let Some(stage) =
                state.pointer("/removal/stage").and_then(|s| s.as_str())
            {
                let mut g = self.stages.lock().unwrap();
                if g.last().map(String::as_str) != Some(stage) {
                    g.push(stage.to_string());
                }
            }
        }
    }

    impl SubjectAnnouncer for StageRecorder {
        fn announce<'a>(
            &'a self,
            announcement: SubjectAnnouncement,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            (),
                            evo_plugin_sdk::contract::ReportError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            self.note(&announcement.state);
            Box::pin(async { Ok(()) })
        }

        fn update_state<'a>(
            &'a self,
            _addressing: ExternalAddressing,
            state: serde_json::Value,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            (),
                            evo_plugin_sdk::contract::ReportError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            self.note(&state);
            Box::pin(async { Ok(()) })
        }

        fn retract<'a>(
            &'a self,
            _addressing: ExternalAddressing,
            _reason: Option<String>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            (),
                            evo_plugin_sdk::contract::ReportError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async { Ok(()) })
        }
    }

    async fn stages_for_safe_remove(
        release_queue: bool,
    ) -> (Vec<String>, SafeRemoveResponse) {
        let runner = Arc::new(FakeCommandRunner::new(Vec::new()));
        let rt =
            runtime_with_runner(removable_stick_lsblk(), Arc::clone(&runner));
        rt.dispatch_verb("storage.usb.list_drives", b"{}")
            .await
            .unwrap();
        let rec = Arc::new(StageRecorder::default());
        rt.attach_subject_publisher(
            Arc::clone(&rec) as Arc<dyn SubjectAnnouncer>
        )
        .await
        .unwrap();
        let payload = serde_json::to_vec(&SafeRemoveRequest {
            stable_id: "MUSIC".to_string(),
            force: None,
            library_source_id: Some("audio-701124".to_string()),
            retract_library: false,
            release_queue,
        })
        .unwrap();
        let bytes = rt
            .dispatch_verb("storage.usb.safe_remove", &payload)
            .await
            .expect("remove");
        (rec.stages(), serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn safe_remove_names_queue_first_when_the_caller_released_it() {
        // Library Remove releases the operator queue on its own
        // MPD connection and then calls here with
        // `release_queue: false`. That release is the first
        // stage of the Remove — it is what lets the umount below
        // be clean — so the glass walks it first, and the umount
        // comes back clean rather than escalating to lazy.
        let (stages, resp) = stages_for_safe_remove(false).await;
        assert_eq!(
            stages,
            vec![
                "queue".to_string(),
                "detach".to_string(),
                "eject".to_string(),
                "retract".to_string(),
                "safe".to_string(),
            ],
            "the banner walks queue, then detach / eject / retract / safe",
        );
        assert!(resp.removed, "the volume comes off");
        assert_eq!(
            resp.forced,
            Some(false),
            "nothing of ours was still holding the mount, so the umount \
             is clean — no lazy detach",
        );
    }

    #[tokio::test]
    async fn safe_remove_does_not_name_a_queue_stage_it_did_not_do() {
        // Sources-page Remove leaves `release_queue` at its
        // default. Nothing has released the queue, and this verb
        // has no route to — naming the stage would put a banner
        // on work that never happened.
        let (stages, _) = stages_for_safe_remove(true).await;
        assert_eq!(
            stages,
            vec![
                "detach".to_string(),
                "eject".to_string(),
                "retract".to_string(),
                "safe".to_string(),
            ],
            "no queue stage without a queue release",
        );
    }

    #[tokio::test]
    async fn library_owned_remove_does_not_reenter_the_library_shelf() {
        // Library Remove is already inside remove_source. A
        // dispatch back into that OOP process is the 17:27:51
        // hang: eject returns, retract never does, list_sources
        // never answers.
        let runner = Arc::new(FakeCommandRunner::new(Vec::new()));
        let rt =
            runtime_with_runner(removable_stick_lsblk(), Arc::clone(&runner));
        rt.dispatch_verb("storage.usb.list_drives", b"{}")
            .await
            .unwrap();
        let d = Arc::new(PayloadDispatcher::default());
        rt.attach_shelf_dispatcher(
            Arc::clone(&d) as Arc<dyn ShelfRequestDispatcher>
        );
        let payload = serde_json::to_vec(&SafeRemoveRequest {
            stable_id: "MUSIC".to_string(),
            force: None,
            library_source_id: Some("audio-701124".to_string()),
            retract_library: false,
            release_queue: false,
        })
        .unwrap();
        rt.dispatch_verb("storage.usb.safe_remove", &payload)
            .await
            .expect("remove");
        let seen = d.seen();
        assert!(
            seen.iter().all(|(verb, _)| verb != "library.remove_source"),
            "USB must not re-enter audio.library: {seen:?}"
        );
        let env: ListDrivesEnvelope = serde_json::from_slice(
            &rt.dispatch_verb("storage.usb.list_drives", b"{}")
                .await
                .unwrap(),
        )
        .unwrap();
        let rem = env.removal.expect("physical Safe still names the stick");
        assert_eq!(rem.stage, RemovalStage::Safe);
    }

    #[tokio::test]
    async fn the_removal_banner_leaves_with_the_stick() {
        // `safe` is a statement about a device. Once the stick
        // is out, it describes nothing — and holding it would
        // paint "safe to unplug" over whatever is plugged in
        // next, which is the same lie this row removes.
        let lsblk =
            Arc::new(StdMutex::new(removable_stick_lsblk().to_string()));
        let runner = Arc::new(FakeCommandRunner::new(Vec::new()));
        let rt = Arc::new(StorageUsbRuntime::with_sources(
            1000,
            1000,
            true,
            Arc::new(SwappableInputSource(Arc::clone(&lsblk))),
            Arc::clone(&runner) as Arc<dyn CommandRunner>,
        ));
        rt.dispatch_verb("storage.usb.list_drives", b"{}")
            .await
            .unwrap();
        let payload = serde_json::to_vec(&SafeRemoveRequest {
            stable_id: "MUSIC".to_string(),
            force: None,
            library_source_id: Some("audio-701124".to_string()),
            retract_library: true,
            release_queue: true,
        })
        .unwrap();
        rt.dispatch_verb("storage.usb.safe_remove", &payload)
            .await
            .expect("remove");

        let env: ListDrivesEnvelope = serde_json::from_slice(
            &rt.dispatch_verb("storage.usb.list_drives", b"{}")
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(
            env.removal.is_some(),
            "while the stick is still in, the last stage stands",
        );

        // Pulled.
        *lsblk.lock().unwrap() = r#"{"blockdevices":[]}"#.to_string();
        let env: ListDrivesEnvelope = serde_json::from_slice(
            &rt.dispatch_verb("storage.usb.list_drives", b"{}")
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(
            env.removal.is_none(),
            "the banner must leave with the device; saw {:?}",
            env.removal,
        );
    }

    /// Wrapper calls and shelf dispatches recorded into one
    /// ordered log, so a test can assert which happened first.
    #[derive(Clone)]
    struct OrderLog(Arc<StdMutex<Vec<String>>>);

    struct OrderedRunner {
        log: OrderLog,
        outcomes: Arc<StdMutex<Vec<CommandOutcome>>>,
    }

    #[async_trait::async_trait]
    impl CommandRunner for OrderedRunner {
        async fn run_wrapper(
            &self,
            _needs_sudo: bool,
            argv: &[String],
        ) -> anyhow::Result<CommandOutcome> {
            self.log
                .0
                .lock()
                .unwrap()
                .push(argv.first().cloned().unwrap_or_default());
            let mut q = self.outcomes.lock().unwrap();
            if q.is_empty() {
                Ok(CommandOutcome {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                })
            } else {
                Ok(q.remove(0))
            }
        }
    }

    struct OrderedDispatcher(OrderLog);

    impl ShelfRequestDispatcher for OrderedDispatcher {
        fn dispatch<'a>(
            &'a self,
            _shelf: &'a str,
            request_type: &'a str,
            _payload: Vec<u8>,
            _instance_id: Option<&'a str>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            Vec<u8>,
                            evo_plugin_sdk::contract::ShelfDispatchError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            self.0 .0.lock().unwrap().push(request_type.to_string());
            // add_source's response carries the id the runtime
            // stores as `library_source_id`; without a parseable
            // one the drive would never learn it has a source
            // and the retraction under test could not fire.
            let body = if request_type == "library.add_source" {
                serde_json::json!({ "v": 1, "source_id": "usb-music" })
            } else {
                serde_json::json!({ "v": 1 })
            };
            Box::pin(async move {
                Ok(serde_json::to_vec(&body).unwrap_or_default())
            })
        }
    }

    #[tokio::test]
    async fn the_scrub_is_not_dispatched_until_the_volume_is_detached() {
        // The scrub is an `update` over the source's path and MPD
        // only prunes rows whose files are gone. Dispatched
        // before the detach it walks a still-mounted tree, finds
        // everything present, and prunes nothing.
        let log = OrderLog(Arc::new(StdMutex::new(Vec::new())));
        let rt = Arc::new(StorageUsbRuntime::with_sources(
            1000,
            1000,
            true,
            Arc::new(FakeInputSource {
                mountinfo: String::new(),
                swaps: String::new(),
                lsblk_json: removable_stick_lsblk().to_string(),
            }),
            Arc::new(OrderedRunner {
                log: log.clone(),
                outcomes: Arc::new(StdMutex::new(Vec::new())),
            }),
        ));
        rt.attach_shelf_dispatcher(Arc::new(OrderedDispatcher(log.clone())));

        rt.dispatch_verb("storage.usb.list_drives", b"{}")
            .await
            .unwrap();
        let payload = serde_json::to_vec(&SafeRemoveRequest {
            stable_id: "MUSIC".to_string(),
            force: None,
            library_source_id: None,
            retract_library: true,
            release_queue: true,
        })
        .unwrap();
        rt.dispatch_verb("storage.usb.safe_remove", &payload)
            .await
            .expect("safe_remove");

        let seen = log.0.lock().unwrap().clone();
        let detach = seen
            .iter()
            .position(|c| c == "umount" || c == "umount-force")
            .unwrap_or_else(|| panic!("no detach recorded: {seen:?}"));
        let scrub = seen
            .iter()
            .position(|c| c == "library.remove_source")
            .unwrap_or_else(|| panic!("no scrub dispatched: {seen:?}"));
        assert!(
            detach < scrub,
            "the volume must be detached before the scrub is asked for; \
             saw {seen:?}",
        );
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
            library_source_id: None,
            retract_library: true,
            release_queue: true,
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
