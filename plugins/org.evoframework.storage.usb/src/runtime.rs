// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! `storage.usb` runtime — dispatch surface for the five shelf
//! verbs.
//!
//! This module hosts [`StorageUsbRuntime`], the singleton the
//! plugin's [`Plugin::load`](crate::StorageUsbPlugin) constructs
//! and drives. The runtime owns:
//!
//! - The path constants for the wrapper + mount root + state
//!   directory.
//! - The verb dispatcher: parses each request's payload,
//!   invokes the internal handler, and serialises the response.
//! - The `list_drives` handler, which invokes the classifier
//!   against live procfs + lsblk inputs and returns the six-way
//!   [`ClassifiedPartition`] set as the `list_drives` response.
//!
//! Every mutating verb (`mount` / `safe_remove` /
//! `repair_filesystem` / `rename`) currently returns
//! [`VerbDispatchError::NotImplemented`] with a stable error
//! class so consumers see a deterministic response shape while
//! Steps 3-5 land the implementations in place. The wrapper's
//! argv shape stays stable across those steps so bootstrap +
//! sudoers grant do not churn.

use crate::classifier::{classify, ClassifiedPartition, ClassifierError};

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::process::Command;

/// Path to the narrow root-only wrapper installed by
/// `bootstrap.sh` Step 1g. Every mount / umount / fsck / eject
/// invocation dispatches through this binary; the plugin does
/// NOT hold raw sudo grants on the underlying tools.
pub const USB_WRAPPER_PATH: &str = "/usr/local/bin/evo-usb-mount";

/// Mount root under which every media USB volume mounts. Owned
/// by the distribution installer per the four-primitive
/// install/reset contract; the plugin never creates it and
/// never falls back to another location.
pub const USB_MOUNT_ROOT: &str = "/var/lib/evo/music/USB";

/// Verb list for this shelf. Kept in this crate so the
/// [`crate::StorageUsbPlugin::describe`] impl and the
/// manifest.request_types stay aligned by construction (a test
/// asserts the two lists match).
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

/// The runtime singleton. Constructed once in
/// [`crate::StorageUsbPlugin::load`] and held for the plugin
/// instance lifetime. Cheap-clone (`Arc<Self>` at the plugin
/// site).
pub struct StorageUsbRuntime {
    /// Effective steward uid (interpolated into mount options).
    /// Resolved at plugin load via `/proc/self/status`.
    service_uid: u32,
    /// Effective steward gid.
    service_gid: u32,
    /// Whether the runtime needs to prefix wrapper invocations
    /// with `sudo -n`. `true` when the plugin runs as a non-
    /// root steward; `false` when the plugin runs as root
    /// (only in dev-loop test scenarios).
    needs_sudo: bool,
    /// Injectable command runner. In production points at
    /// [`RealCommandRunner`]; tests supply a fake to assert
    /// argv without spawning processes.
    #[allow(dead_code)]
    command_runner: Arc<dyn CommandRunner>,
    /// Injectable input source for the classifier. In
    /// production points at [`ProcfsAndLsblkSource`]; tests
    /// supply a fake with fixture strings.
    input_source: Arc<dyn ClassifierInputSource>,
}

impl StorageUsbRuntime {
    /// New runtime with production input/command sources. The
    /// steward uid/gid MUST be resolved by the caller before
    /// construction (the plugin's `load` handler does this via
    /// [`detect_service_uid_gid`]).
    pub fn new(service_uid: u32, service_gid: u32, needs_sudo: bool) -> Self {
        Self {
            service_uid,
            service_gid,
            needs_sudo,
            command_runner: Arc::new(RealCommandRunner),
            input_source: Arc::new(ProcfsAndLsblkSource),
        }
    }

    /// New runtime with a caller-supplied input source. Used by
    /// the runtime-level fixture tests that replay canned
    /// procfs + lsblk strings.
    #[allow(dead_code)]
    pub fn with_input_source(
        service_uid: u32,
        service_gid: u32,
        needs_sudo: bool,
        input_source: Arc<dyn ClassifierInputSource>,
    ) -> Self {
        Self {
            service_uid,
            service_gid,
            needs_sudo,
            command_runner: Arc::new(RealCommandRunner),
            input_source,
        }
    }

    /// Getter: the resolved steward uid. Used by the plugin's
    /// `describe` info + acceptance evidence.
    pub fn service_uid(&self) -> u32 {
        self.service_uid
    }

    /// Getter: the resolved steward gid.
    pub fn service_gid(&self) -> u32 {
        self.service_gid
    }

    /// Getter: whether wrapper invocations get a `sudo -n` prefix.
    pub fn needs_sudo(&self) -> bool {
        self.needs_sudo
    }

    /// Dispatch a decoded request. The plugin's Respondent
    /// impl consults [`is_storage_usb_verb`] first, then hands
    /// the verb + raw payload here. Returns raw response bytes
    /// (JSON) or a structured [`VerbDispatchError`] the caller
    /// converts to `PluginError`.
    pub async fn dispatch_verb(
        &self,
        verb: &str,
        _payload: &[u8],
    ) -> Result<Vec<u8>, VerbDispatchError> {
        match verb {
            "storage.usb.list_drives" => self.handle_list_drives().await,
            "storage.usb.mount"
            | "storage.usb.safe_remove"
            | "storage.usb.repair_filesystem"
            | "storage.usb.rename" => Err(VerbDispatchError::NotImplemented {
                verb: verb.to_string(),
            }),
            other => Err(VerbDispatchError::UnknownRequestType {
                verb: other.to_string(),
            }),
        }
    }

    /// `storage.usb.list_drives` handler: reads live procfs +
    /// lsblk inputs, invokes the classifier, serialises the
    /// six-way [`ClassifiedPartition`] set as the response
    /// envelope.
    async fn handle_list_drives(&self) -> Result<Vec<u8>, VerbDispatchError> {
        let inputs = self
            .input_source
            .read_inputs()
            .await
            .map_err(|e| VerbDispatchError::InputSource(e.to_string()))?;
        let classified =
            classify(&inputs.mountinfo, &inputs.swaps, &inputs.lsblk_json)
                .map_err(VerbDispatchError::Classify)?;
        let last_update_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let envelope = ListDrivesEnvelope {
            v: 1,
            drives: classified
                .iter()
                .map(DriveRecord::from_classified)
                .collect(),
            last_update_at_ms,
        };
        serde_json::to_vec(&envelope)
            .map_err(|e| VerbDispatchError::ResponseSerialise(e.to_string()))
    }
}

// -------- Wire-shape records --------

/// Envelope returned by `storage.usb.list_drives`. Mirrors the
/// `storage_usb_drives` subject payload shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListDrivesEnvelope {
    /// Envelope shape version.
    pub v: u32,
    /// One record per classified USB-transport partition.
    pub drives: Vec<DriveRecord>,
    /// Wall-clock ms of the enumeration.
    pub last_update_at_ms: i64,
}

/// One drive record as returned by `list_drives` + carried on
/// the `storage_usb_drives` subject. Field shapes match the
/// schema at `evo-catalogue-schemas/schemas/org.evoframework/storage/usb.v1.toml`.
///
/// Fields absent in this Step-2 skeleton (`stable_id`,
/// `display_name`, `id_source`, `mount_root`,
/// `library_source_id`, `alias_set`, `last_transition_at_ms`)
/// land in Steps 3-6 as the stable-id derivation, mount
/// lifecycle, cross-plugin library dispatch, and alias
/// persistence come online. The wire-envelope shape carries
/// them as `Option`-typed today so the schema is stable across
/// the roll-in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriveRecord {
    /// Stable id (§3 derivation ladder). Populated from
    /// Step 3 onward; `None` today.
    pub stable_id: Option<String>,
    /// Display token (stable-id sans partition suffix).
    /// Populated from Step 3 onward.
    pub display_name: Option<String>,
    /// Which rule in the derivation ladder produced the id.
    /// Populated from Step 3 onward.
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
    /// GPT PARTUUID (alias-key component).
    pub partuuid: Option<String>,
    /// Udev vendor.
    pub vendor: Option<String>,
    /// Udev model.
    pub model: Option<String>,
    /// Udev serial short (alias-key component).
    pub serial_short: Option<String>,
    /// Filesystem family (`vfat` / `exfat` / `ntfs` / `ext4` / …).
    pub fs_type: String,
    /// Byte size from `blockdev --getsize64` (via lsblk `-b`).
    pub size_bytes: u64,
    /// Six-way role. Non-null.
    pub role: String,
    /// Mount policy. Non-null.
    pub mount_policy: String,
    /// Current kernel mount point (if any).
    pub mount_root: Option<String>,
    /// `library.add_source` result when mounted. Populated from
    /// Step 3 onward.
    pub library_source_id: Option<String>,
    /// True when the drive has an operator-set alias.
    /// Populated from Step 6.
    pub alias_set: bool,
    /// Wall-clock ms of the last state change. Populated from
    /// Step 3 onward; `None` today (the record is
    /// enumeration-derived, not transition-derived, in Step 2).
    pub last_transition_at_ms: Option<i64>,
}

impl DriveRecord {
    /// Build a wire record from a [`ClassifiedPartition`]. The
    /// alias / stable-id / mount-lifecycle fields stay `None`
    /// in Step 2; Steps 3-6 populate them as the pipeline
    /// stages come online.
    pub fn from_classified(p: &ClassifiedPartition) -> Self {
        Self {
            stable_id: None,
            display_name: None,
            id_source: None,
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
            role: role_wire_string(p.role),
            mount_policy: mount_policy_wire_string(p.mount_policy),
            mount_root: p.current_mount.clone(),
            library_source_id: None,
            alias_set: false,
            last_transition_at_ms: None,
        }
    }
}

fn role_wire_string(r: crate::classifier::PartitionRole) -> String {
    use crate::classifier::PartitionRole;
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

fn mount_policy_wire_string(p: crate::classifier::MountPolicy) -> String {
    use crate::classifier::MountPolicy;
    match p {
        MountPolicy::Auto => "auto",
        MountPolicy::OptInRequired => "opt-in-required",
        MountPolicy::RefusedSystemLive => "refused-system-live",
    }
    .to_string()
}

// -------- Verb-dispatch error taxonomy --------

/// Errors returned by [`StorageUsbRuntime::dispatch_verb`].
/// The plugin's Respondent impl maps each to a `PluginError`
/// variant.
#[derive(Debug, thiserror::Error)]
pub enum VerbDispatchError {
    /// Verb string is not one of [`STORAGE_USB_VERBS`]. Maps
    /// to `PluginError::Permanent` — no plugin restart will
    /// help. This should never fire in production because the
    /// dispatcher already partitions verbs by manifest; kept
    /// as a defence-in-depth check.
    #[error("storage.usb: unknown verb {verb:?}")]
    UnknownRequestType {
        /// The unrecognised verb.
        verb: String,
    },
    /// Classifier failed on the current procfs + lsblk input.
    /// Maps to `PluginError::Transient` — a retry may succeed
    /// if the underlying tool becomes reachable.
    #[error("storage.usb: classifier failed: {0}")]
    Classify(#[from] ClassifierError),
    /// The input source (procfs read or `lsblk` invocation)
    /// failed. Maps to `PluginError::Transient`.
    #[error("storage.usb: input source failed: {0}")]
    InputSource(String),
    /// Response serialisation failed. Should never fire on
    /// well-formed output; maps to `PluginError::Permanent`.
    #[error("storage.usb: response serialise failed: {0}")]
    ResponseSerialise(String),
    /// A verb is declared in [`STORAGE_USB_VERBS`] but its
    /// implementation lands in a later step. Steps 3-5 remove
    /// this variant one verb at a time.
    #[error("storage.usb: verb {verb:?} not implemented yet")]
    NotImplemented {
        /// The declared-but-unwired verb.
        verb: String,
    },
}

// -------- Input source (classifier upstream) --------

/// Triple of classifier inputs read from the host: mountinfo
/// text + swaps text + lsblk JSON.
pub struct ClassifierInputs {
    /// Verbatim `/proc/self/mountinfo` text.
    pub mountinfo: String,
    /// Verbatim `/proc/swaps` text.
    pub swaps: String,
    /// `lsblk -J -o …` JSON output.
    pub lsblk_json: String,
}

/// Abstraction over the classifier input path. The production
/// impl reads `/proc/*` + spawns `lsblk`; tests supply a fake
/// with fixture strings.
#[async_trait::async_trait]
pub trait ClassifierInputSource: Send + Sync {
    /// Read the current classifier inputs.
    async fn read_inputs(&self) -> anyhow::Result<ClassifierInputs>;
}

/// Production input source: reads `/proc/self/mountinfo` +
/// `/proc/swaps` and spawns `lsblk`.
pub struct ProcfsAndLsblkSource;

#[async_trait::async_trait]
impl ClassifierInputSource for ProcfsAndLsblkSource {
    async fn read_inputs(&self) -> anyhow::Result<ClassifierInputs> {
        let mountinfo =
            tokio::fs::read_to_string("/proc/self/mountinfo").await?;
        let swaps = tokio::fs::read_to_string("/proc/swaps")
            .await
            .unwrap_or_default();
        // -b forces byte SIZE (integer, not human-readable).
        // -J emits JSON. The wide -o field list gives us
        // everything the classifier consults in one shot.
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

// -------- Command runner (mount / umount / fsck / eject) --------

/// Result of one command invocation.
#[derive(Debug, Clone)]
pub struct CommandOutcome {
    /// Process exit status (raw code, or -1 if terminated by signal).
    pub status: i32,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
}

/// Abstraction over the wrapper subprocess dispatch. The
/// production impl spawns the real wrapper; tests supply a
/// fake that records argv and returns a scripted
/// [`CommandOutcome`]. Reserved for Steps 3-5 wiring — every
/// current verb either does not call the wrapper (list_drives)
/// or returns NotImplemented (mount/etc.).
#[async_trait::async_trait]
pub trait CommandRunner: Send + Sync {
    /// Run the wrapper with the supplied argv. `needs_sudo`
    /// controls whether the invocation is prefixed with
    /// `sudo -n`.
    async fn run_wrapper(
        &self,
        needs_sudo: bool,
        argv: &[String],
    ) -> anyhow::Result<CommandOutcome>;
}

/// Production command runner: spawns the wrapper via `Command`.
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

// -------- Steward-uid resolver --------

/// Resolve the plugin's effective uid + gid from
/// `/proc/self/status`. The runtime interpolates these into
/// the FS-matrix mount options. Called by the plugin's `load`
/// handler.
pub fn detect_service_uid_gid() -> anyhow::Result<(u32, u32)> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    let mut uid: Option<u32> = None;
    let mut gid: Option<u32> = None;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            // Real Effective SavedSet Filesystem
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

    #[test]
    fn verb_recognition() {
        for v in STORAGE_USB_VERBS {
            assert!(is_storage_usb_verb(v));
        }
        assert!(!is_storage_usb_verb("storage.usb.bogus"));
        assert!(!is_storage_usb_verb("network.share.add"));
    }

    #[test]
    fn verb_list_matches_manifest() {
        // The manifest declares the same verbs. If either drifts,
        // the plugin's admission-time verb partition rejects.
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
    async fn list_drives_returns_classified_envelope() {
        let lsblk = r#"{
          "blockdevices": [
            {"name":"sda","type":"disk","tran":"usb","vendor":"SanDisk","model":"Cruzer","serial":"4C530",
             "size":32000000000,
             "children":[
               {"name":"sda1","type":"part","fstype":"vfat","label":"MUSIC","size":32000000000,"partuuid":"aaaa-01"}
             ]}
          ]
        }"#;
        let rt = StorageUsbRuntime::with_input_source(
            1000,
            1000,
            true,
            Arc::new(FakeInputSource {
                mountinfo: String::new(),
                swaps: String::new(),
                lsblk_json: lsblk.to_string(),
            }),
        );
        let bytes = rt
            .dispatch_verb("storage.usb.list_drives", b"{}")
            .await
            .expect("dispatch");
        let env: ListDrivesEnvelope =
            serde_json::from_slice(&bytes).expect("json");
        assert_eq!(env.v, 1);
        assert_eq!(env.drives.len(), 1);
        assert_eq!(env.drives[0].device_node, "/dev/sda1");
        assert_eq!(env.drives[0].role, "removable");
        assert_eq!(env.drives[0].mount_policy, "auto");
        assert_eq!(env.drives[0].fs_type, "vfat");
        assert_eq!(env.drives[0].label.as_deref(), Some("MUSIC"));
        // Step-2 skeleton: stable-id-derivation fields empty.
        assert!(env.drives[0].stable_id.is_none());
        assert!(env.drives[0].id_source.is_none());
        assert!(!env.drives[0].alias_set);
    }

    #[tokio::test]
    async fn mutating_verbs_return_not_implemented() {
        let rt = StorageUsbRuntime::with_input_source(
            1000,
            1000,
            true,
            Arc::new(FakeInputSource {
                mountinfo: String::new(),
                swaps: String::new(),
                lsblk_json: r#"{"blockdevices":[]}"#.to_string(),
            }),
        );
        for verb in [
            "storage.usb.mount",
            "storage.usb.safe_remove",
            "storage.usb.repair_filesystem",
            "storage.usb.rename",
        ] {
            let err = rt.dispatch_verb(verb, b"{}").await.unwrap_err();
            match err {
                VerbDispatchError::NotImplemented { verb: v } => {
                    assert_eq!(v, verb);
                }
                other => panic!("expected NotImplemented, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn unknown_verb_returns_unknown_request_type() {
        let rt = StorageUsbRuntime::with_input_source(
            1000,
            1000,
            true,
            Arc::new(FakeInputSource {
                mountinfo: String::new(),
                swaps: String::new(),
                lsblk_json: r#"{"blockdevices":[]}"#.to_string(),
            }),
        );
        let err = rt
            .dispatch_verb("storage.usb.bogus", b"{}")
            .await
            .unwrap_err();
        matches!(err, VerbDispatchError::UnknownRequestType { .. });
    }

    #[test]
    fn role_wire_strings_match_schema() {
        use crate::classifier::PartitionRole;
        assert_eq!(role_wire_string(PartitionRole::SystemRoot), "system-root");
        assert_eq!(role_wire_string(PartitionRole::SystemBoot), "system-boot");
        assert_eq!(role_wire_string(PartitionRole::SystemEfi), "system-efi");
        assert_eq!(role_wire_string(PartitionRole::SystemSwap), "system-swap");
        assert_eq!(
            role_wire_string(PartitionRole::SystemAdjacent),
            "system-adjacent"
        );
        assert_eq!(role_wire_string(PartitionRole::Removable), "removable");
    }

    #[test]
    fn mount_policy_wire_strings_match_schema() {
        use crate::classifier::MountPolicy;
        assert_eq!(mount_policy_wire_string(MountPolicy::Auto), "auto");
        assert_eq!(
            mount_policy_wire_string(MountPolicy::OptInRequired),
            "opt-in-required"
        );
        assert_eq!(
            mount_policy_wire_string(MountPolicy::RefusedSystemLive),
            "refused-system-live"
        );
    }
}
