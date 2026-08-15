// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Six-way role classifier for USB-transport block partitions.
//!
//! # Model
//!
//! Every USB-transport block partition is classified into one of
//! six roles at classify time. The classification is deterministic
//! from three inputs — `/proc/self/mountinfo`, `/proc/swaps`, and
//! the JSON output of `lsblk -J -o …` — with no hidden I/O. The
//! runtime shells out to `lsblk` and reads the two procfs paths,
//! then hands the strings to [`classify`]. The pure-function shape
//! makes the taxonomy fixture-testable: every rule in
//! `USB-STORAGE.md` §1 has a synthetic input triple under
//! `tests/fixtures/<name>/{mountinfo,swaps,lsblk.json,expected.json}`
//! and the fixture runner in `tests/classifier.rs` asserts the
//! output matches byte-for-byte.
//!
//! # Roles
//!
//! - [`PartitionRole::SystemRoot`] — backs `/`.
//! - [`PartitionRole::SystemBoot`] — backs `/boot` or
//!   `/boot/firmware` (Pi convention for the FAT boot partition).
//! - [`PartitionRole::SystemEfi`] — backs `/boot/efi` (or the ESP
//!   marked by GPT partition-type UUID when present).
//! - [`PartitionRole::SystemSwap`] — listed in `/proc/swaps`.
//! - [`PartitionRole::SystemAdjacent`] — sibling partition on the
//!   same parent disk as any `System*` partition, but not itself
//!   backing a live system mount.
//! - [`PartitionRole::Removable`] — everything else.
//!
//! # Boot-drive invariant
//!
//! The classifier NEVER omits a USB-transport partition from
//! its output — a USB-booted device sees its own root / boot /
//! EFI partitions in the inventory with `role = system-*` and a
//! [`MountPolicy::RefusedSystemLive`] gate. Consumers (UI) filter
//! by role for display; the plugin's mount verb refuses at the
//! runtime layer with a stable error class. Sibling data
//! partitions on the boot drive are visible with
//! [`PartitionRole::SystemAdjacent`] and
//! [`MountPolicy::OptInRequired`] — mountable + repairable, but
//! only after the operator names an alias and confirms the policy.
//!
//! # Non-goals
//!
//! - This module does NOT execute `lsblk`, read `/proc/*`, or
//!   perform any I/O. The runtime is the sole caller and is
//!   responsible for reading the three inputs and passing the
//!   raw strings.
//! - This module does NOT compute stable-ids or mount paths. The
//!   [`stable_id`](crate::stable_id) module (Step 3) consumes the
//!   [`ClassifiedPartition`] output to derive the friendly id.

use serde::{Deserialize, Serialize};

/// Six-way role of a USB-transport block partition.
///
/// See the module-level docs for the precise semantics of each
/// role and the boot-drive invariant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PartitionRole {
    /// Backs `/`.
    SystemRoot,
    /// Backs `/boot` or `/boot/firmware`.
    SystemBoot,
    /// Backs `/boot/efi`.
    SystemEfi,
    /// Listed in `/proc/swaps`.
    SystemSwap,
    /// Sibling of a `System*` partition on the same parent disk.
    SystemAdjacent,
    /// Everything else (the normal USB-media path).
    Removable,
}

impl PartitionRole {
    /// True when the role represents a partition currently
    /// backing a live system mount (root / boot / EFI / swap).
    /// The runtime uses this to refuse mount + live-fsck.
    pub fn is_system_live(self) -> bool {
        matches!(
            self,
            PartitionRole::SystemRoot
                | PartitionRole::SystemBoot
                | PartitionRole::SystemEfi
                | PartitionRole::SystemSwap
        )
    }

    /// Default [`MountPolicy`] for this role. See
    /// [`MountPolicy`] for the operator-facing semantics.
    pub fn default_policy(self) -> MountPolicy {
        match self {
            PartitionRole::SystemRoot
            | PartitionRole::SystemBoot
            | PartitionRole::SystemEfi
            | PartitionRole::SystemSwap => MountPolicy::RefusedSystemLive,
            PartitionRole::SystemAdjacent => MountPolicy::OptInRequired,
            PartitionRole::Removable => MountPolicy::Auto,
        }
    }
}

/// Mount policy the runtime applies before building the mount
/// argv. Emitted on every [`ClassifiedPartition`] so consumers
/// can render honestly (boot drive visible with reduced
/// affordances; adjacent data partitions gated behind opt-in).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MountPolicy {
    /// Auto-mount on plug / coldplug. Default for
    /// [`PartitionRole::Removable`].
    Auto,
    /// Refuse mount until the operator sets an alias with
    /// `mount_policy = "opt-in"` in `aliases.toml`. Default for
    /// [`PartitionRole::SystemAdjacent`].
    OptInRequired,
    /// Refuse mount unconditionally. Fixed for the four
    /// live-system roles — non-negotiable.
    RefusedSystemLive,
}

/// One classified USB-transport partition.
///
/// The runtime returns a `Vec<ClassifiedPartition>` from the
/// classifier and feeds it to the stable-id derivation (Step 3),
/// the mount lifecycle (Steps 3-5), and the `list_drives`
/// response serialiser.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassifiedPartition {
    /// Partition device node (e.g. `/dev/sda1`,
    /// `/dev/nvme0n1p2`, `/dev/mmcblk0p1`).
    pub device_node: String,
    /// Parent disk device node (e.g. `/dev/sda` for `/dev/sda1`).
    pub parent_disk: String,
    /// 1-based partition index on the parent disk.
    pub partition_index: u32,
    /// Total mountable partitions on the parent disk (drives
    /// the `-p<N>` stable-id suffix rule).
    pub partition_count: u32,
    /// Filesystem label if present (udev `ID_FS_LABEL`).
    pub label: Option<String>,
    /// Filesystem UUID if present.
    pub uuid: Option<String>,
    /// GPT partition UUID if present (alias-key component).
    pub partuuid: Option<String>,
    /// Udev `ID_VENDOR` (e.g. `SanDisk`, `WD`, `Samsung`).
    pub vendor: Option<String>,
    /// Udev `ID_MODEL` (e.g. `Cruzer-Blade`, `Elements-25A2`).
    pub model: Option<String>,
    /// Udev `ID_SERIAL_SHORT` (alias-key component).
    pub serial_short: Option<String>,
    /// Filesystem family string as reported by udev / blkid
    /// (`vfat`, `exfat`, `ntfs`, `ext2`, `ext3`, `ext4`, or a
    /// value not in the support matrix which surfaces as
    /// `class: unsupported` in the drive listing).
    pub fs_type: String,
    /// Byte size from `lsblk` (parsed from the JSON `SIZE`
    /// integer field). Drives the >2 TiB FAT32 refuse.
    pub size_bytes: u64,
    /// Six-way role from the classifier.
    pub role: PartitionRole,
    /// Default mount policy for the role. Overridable per-alias
    /// on `system-adjacent` at rename time; fixed on system-live
    /// and removable.
    pub mount_policy: MountPolicy,
    /// Current kernel mount point if the partition is already
    /// mounted (system mounts, or an operator-initiated mount
    /// before the plugin loaded). `None` when the kernel has no
    /// mount for this partition.
    pub current_mount: Option<String>,
}

/// Errors returned by [`classify`].
///
/// The classifier is otherwise infallible on well-formed
/// procfs + lsblk inputs; malformed inputs surface as one of
/// these variants with a message the runtime logs and maps to
/// `PluginError::Permanent`.
#[derive(Debug, thiserror::Error)]
pub enum ClassifierError {
    /// The lsblk JSON did not deserialise into the expected
    /// nested-disk-and-partition shape. Message carries the
    /// serde error text.
    #[error("lsblk JSON parse failed: {0}")]
    LsblkJson(String),
    /// A mountinfo line did not carry the minimum required
    /// fields (device major:minor + mount point). Message
    /// carries the offending line for operator diagnostics.
    #[error("mountinfo line malformed: {0}")]
    Mountinfo(String),
}

/// Classify every USB-transport block partition reachable from
/// the supplied inputs. Returns one [`ClassifiedPartition`] per
/// USB-transport partition, in the order they appear in the
/// lsblk JSON (which is `/sys/class/block` order — stable across
/// boots on the same rig with the same drives).
///
/// The three inputs must be:
///
/// - `mountinfo` — verbatim contents of `/proc/self/mountinfo`
///   (or a synthetic fixture with the same format).
/// - `swaps` — verbatim contents of `/proc/swaps` (or synthetic).
/// - `lsblk_json` — the JSON output of
///   `lsblk -J -o NAME,PKNAME,MOUNTPOINT,TRAN,TYPE,UUID,LABEL,FSTYPE,PARTUUID,VENDOR,MODEL,SERIAL,SIZE`.
///
/// The runtime is responsible for producing all three inputs
/// (the runtime shells out to `lsblk` and reads the two procfs
/// paths); this function is I/O-free and pure.
pub fn classify(
    mountinfo: &str,
    swaps: &str,
    lsblk_json: &str,
) -> Result<Vec<ClassifiedPartition>, ClassifierError> {
    let mounts = parse_mountinfo(mountinfo)?;
    let swap_devices = parse_swaps(swaps);
    let devices = parse_lsblk(lsblk_json)?;

    // Two-pass classification. Pass 1 assigns system-live roles
    // to USB-transport partitions that back a live system mount
    // or swap. Pass 2 marks siblings on the same parent disk as
    // SystemAdjacent, then Removable for everything else.

    let usb_partitions: Vec<UsbPartition> = collect_usb_partitions(&devices);

    // Set of parent disks whose sub-partitions include at least
    // one system-live role. Feeds the SystemAdjacent gate.
    let mut system_parent_disks: std::collections::BTreeSet<String> =
        Default::default();

    // First pass: system-live role assignment.
    let mut roles: Vec<(usize, PartitionRole, Option<String>)> = Vec::new();
    for (idx, part) in usb_partitions.iter().enumerate() {
        let mountpoints_for_dev: Vec<&str> = mounts
            .iter()
            .filter(|m| m.device_node == part.device_node)
            .map(|m| m.mountpoint.as_str())
            .collect();

        let is_swap = swap_devices.contains(&part.device_node);

        let system_role = if is_swap {
            Some(PartitionRole::SystemSwap)
        } else {
            classify_system_mount(&mountpoints_for_dev)
        };

        if let Some(role) = system_role {
            let current_mount =
                mountpoints_for_dev.first().map(|s| s.to_string());
            roles.push((idx, role, current_mount));
            system_parent_disks.insert(part.parent_disk.clone());
        } else {
            // Placeholder — resolved in the second pass.
            roles.push((
                idx,
                PartitionRole::Removable,
                mountpoints_for_dev.first().map(|s| s.to_string()),
            ));
        }
    }

    // Second pass: SystemAdjacent + Removable.
    for (idx, role, _) in roles.iter_mut() {
        if *role != PartitionRole::Removable {
            continue;
        }
        let part = &usb_partitions[*idx];
        if system_parent_disks.contains(&part.parent_disk) {
            *role = PartitionRole::SystemAdjacent;
        }
    }

    // Compose the classified output.
    let mut out: Vec<ClassifiedPartition> =
        Vec::with_capacity(usb_partitions.len());
    for (idx, part) in usb_partitions.iter().enumerate() {
        let (_, role, current_mount) = &roles[idx];
        out.push(ClassifiedPartition {
            device_node: part.device_node.clone(),
            parent_disk: part.parent_disk.clone(),
            partition_index: part.partition_index,
            partition_count: part.partition_count,
            label: part.label.clone(),
            uuid: part.uuid.clone(),
            partuuid: part.partuuid.clone(),
            vendor: part.vendor.clone(),
            model: part.model.clone(),
            serial_short: part.serial_short.clone(),
            fs_type: part.fs_type.clone(),
            size_bytes: part.size_bytes,
            role: *role,
            mount_policy: role.default_policy(),
            current_mount: current_mount.clone(),
        });
    }

    Ok(out)
}

// -------- Mountinfo parsing --------

#[derive(Debug, Clone)]
struct MountEntry {
    /// `/dev/…` device node resolved from the mountinfo
    /// major:minor pair by looking it up in `/sys`. When the
    /// mountinfo line's field 10 (the mount source) carries a
    /// `/dev/…` path directly, we use that; otherwise the
    /// major:minor field is left for the caller to resolve
    /// (unused in the current classifier — we match on the
    /// mount source path directly).
    device_node: String,
    mountpoint: String,
}

fn parse_mountinfo(text: &str) -> Result<Vec<MountEntry>, ClassifierError> {
    // /proc/self/mountinfo format (per proc(5)):
    //
    //   mount-id  parent-id  major:minor  root  mount-point  \
    //     mount-options   optional-fields   -   fs-type  \
    //     mount-source   super-options
    //
    // Field indices (0-based):
    //   0: mount-id
    //   1: parent-id
    //   2: major:minor
    //   3: root (subdir of the fs)
    //   4: mount-point
    //   5: mount-options
    //   6..: optional-fields terminated by a lone "-" separator
    //   after "-": fs-type, mount-source, super-options
    //
    // We only need mount-point (field 4) and mount-source (first
    // token after the "-" separator).
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let toks: Vec<&str> = line.split_whitespace().collect();
        if toks.len() < 7 {
            return Err(ClassifierError::Mountinfo(line.to_string()));
        }
        let mountpoint = decode_mountinfo_field(toks[4]);
        // Find the "-" separator that ends the optional-fields
        // section. The next token is fs-type; the one after
        // that is mount-source.
        let sep_pos = toks.iter().position(|t| *t == "-");
        let source = match sep_pos {
            Some(p) if p + 2 < toks.len() => toks[p + 2],
            _ => {
                return Err(ClassifierError::Mountinfo(line.to_string()));
            }
        };
        out.push(MountEntry {
            device_node: decode_mountinfo_field(source),
            mountpoint,
        });
    }
    Ok(out)
}

/// mountinfo field 4 (mount-point) and field 10 (source) escape
/// space / tab / newline / backslash as octal `\NNN`. Decode
/// them back to raw bytes so path matching works.
fn decode_mountinfo_field(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let d1 = bytes[i + 1];
            let d2 = bytes[i + 2];
            let d3 = bytes[i + 3];
            if d1.is_ascii_digit() && d2.is_ascii_digit() && d3.is_ascii_digit()
            {
                let val = ((d1 - b'0') << 6) | ((d2 - b'0') << 3) | (d3 - b'0');
                out.push(val as char);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

// -------- /proc/swaps parsing --------

fn parse_swaps(text: &str) -> std::collections::BTreeSet<String> {
    // /proc/swaps format:
    //
    //   Filename   Type   Size   Used   Priority
    //   /dev/sda3  partition  1048572  0  -2
    //   ...
    //
    // Header line + data lines. We only need column 0 for
    // partition-type entries; file-backed swap is not a USB
    // block device.
    let mut out = std::collections::BTreeSet::new();
    for (i, line) in text.lines().enumerate() {
        if i == 0 || line.trim().is_empty() {
            continue;
        }
        let toks: Vec<&str> = line.split_whitespace().collect();
        if toks.len() < 2 {
            continue;
        }
        if toks[1] != "partition" {
            continue;
        }
        out.insert(toks[0].to_string());
    }
    out
}

/// Return the system role a USB partition plays if any of its
/// mount points is a system mount. Returns None if none of the
/// mount points map to a system mount.
fn classify_system_mount(mountpoints: &[&str]) -> Option<PartitionRole> {
    // Order matters: EFI is a subpath of /boot on some layouts
    // (`/boot/efi`), so check EFI before /boot. Similarly /boot
    // exact-match before its subpath variants.
    for mp in mountpoints {
        if *mp == "/" {
            return Some(PartitionRole::SystemRoot);
        }
    }
    for mp in mountpoints {
        if *mp == "/boot/efi" {
            return Some(PartitionRole::SystemEfi);
        }
    }
    for mp in mountpoints {
        if *mp == "/boot" || *mp == "/boot/firmware" {
            return Some(PartitionRole::SystemBoot);
        }
    }
    None
}

// -------- lsblk JSON parsing --------

#[derive(Debug, Deserialize)]
struct LsblkRoot {
    blockdevices: Vec<LsblkNode>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct LsblkNode {
    name: String,
    // Kept for future coldplug mount-truth reconcile (Step 3).
    #[serde(default)]
    pkname: Option<String>,
    // Kept for future coldplug mount-truth reconcile (Step 3).
    #[serde(default)]
    mountpoint: Option<String>,
    #[serde(default)]
    tran: Option<String>,
    #[serde(default, rename = "type")]
    node_type: Option<String>,
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    fstype: Option<String>,
    #[serde(default)]
    partuuid: Option<String>,
    #[serde(default)]
    vendor: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    serial: Option<String>,
    /// lsblk `-J` emits SIZE as either a JSON integer (bytes)
    /// or a JSON string (human-readable — e.g. `"32G"`) depending
    /// on whether `-b` is in effect. The runtime always passes
    /// `-b` so SIZE is bytes as an integer. We accept both to
    /// keep fixtures forgiving.
    #[serde(default)]
    size: Option<serde_json::Value>,
    #[serde(default)]
    children: Vec<LsblkNode>,
}

/// Flattened USB-transport partition record built from lsblk +
/// its parent disk metadata (VENDOR / MODEL / SERIAL / TRAN).
#[derive(Debug, Clone)]
struct UsbPartition {
    device_node: String,
    parent_disk: String,
    partition_index: u32,
    partition_count: u32,
    label: Option<String>,
    uuid: Option<String>,
    partuuid: Option<String>,
    vendor: Option<String>,
    model: Option<String>,
    serial_short: Option<String>,
    fs_type: String,
    size_bytes: u64,
}

fn parse_lsblk(text: &str) -> Result<LsblkRoot, ClassifierError> {
    serde_json::from_str::<LsblkRoot>(text)
        .map_err(|e| ClassifierError::LsblkJson(e.to_string()))
}

fn collect_usb_partitions(root: &LsblkRoot) -> Vec<UsbPartition> {
    let mut out: Vec<UsbPartition> = Vec::new();
    for disk in &root.blockdevices {
        // A disk is USB when its TRAN is "usb". Nested disk
        // (e.g. dm-crypt over USB) is out of scope for the P0
        // media-source path.
        let is_usb = disk.tran.as_deref() == Some("usb");
        if !is_usb {
            continue;
        }
        let vendor = disk
            .vendor
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let model = disk
            .model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let serial_short = disk
            .serial
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let disk_node = format!("/dev/{}", disk.name);

        let mountable_partitions: Vec<&LsblkNode> = disk
            .children
            .iter()
            .filter(|c| c.node_type.as_deref() == Some("part"))
            .collect();
        let partition_count = mountable_partitions.len() as u32;

        for (i, child) in mountable_partitions.iter().enumerate() {
            let device_node = format!("/dev/{}", child.name);
            let partition_index =
                parse_partition_index(&child.name).unwrap_or((i + 1) as u32);
            let size_bytes =
                child.size.as_ref().and_then(size_to_bytes).unwrap_or(0);
            out.push(UsbPartition {
                device_node,
                parent_disk: disk_node.clone(),
                partition_index,
                partition_count,
                label: child.label.clone(),
                uuid: child.uuid.clone(),
                partuuid: child.partuuid.clone(),
                vendor: vendor.clone(),
                model: model.clone(),
                serial_short: serial_short.clone(),
                fs_type: child.fstype.clone().unwrap_or_default(),
                size_bytes,
            });
        }
    }
    out
}

/// Extract the partition-index integer from a partition device
/// name (`sda1` → 1, `nvme0n1p2` → 2, `mmcblk0p3` → 3).
fn parse_partition_index(name: &str) -> Option<u32> {
    // Walk from the end, collecting trailing digits.
    let bytes = name.as_bytes();
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1].is_ascii_digit() {
        end -= 1;
    }
    if end == bytes.len() {
        return None;
    }
    name[end..].parse().ok()
}

/// lsblk with `-b` emits SIZE as an integer JSON value. Without
/// `-b` it emits a human-readable string (`"32G"`, `"1.8T"`).
/// The runtime always passes `-b`; the string branch keeps
/// fixtures forgiving.
fn size_to_bytes(v: &serde_json::Value) -> Option<u64> {
    if let Some(n) = v.as_u64() {
        return Some(n);
    }
    if let Some(s) = v.as_str() {
        return parse_human_size(s);
    }
    None
}

/// Human-readable size parser (`"32G"` → 32 * 1024^3). Accepts
/// SI + IEC suffixes. Returns None on unparseable input.
fn parse_human_size(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num_str, suffix) = split_num_suffix(s);
    let num: f64 = num_str.parse().ok()?;
    let mult: u64 = match suffix {
        "" | "B" => 1,
        "K" | "KB" | "KiB" => 1024,
        "M" | "MB" | "MiB" => 1024 * 1024,
        "G" | "GB" | "GiB" => 1024 * 1024 * 1024,
        "T" | "TB" | "TiB" => 1024u64 * 1024 * 1024 * 1024,
        "P" | "PB" | "PiB" => 1024u64 * 1024 * 1024 * 1024 * 1024,
        _ => return None,
    };
    Some((num * (mult as f64)) as u64)
}

fn split_num_suffix(s: &str) -> (&str, &str) {
    let mut boundary = s.len();
    for (i, c) in s.char_indices() {
        if !(c.is_ascii_digit() || c == '.' || c == '-') {
            boundary = i;
            break;
        }
    }
    (&s[..boundary], s[boundary..].trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_index_extraction() {
        assert_eq!(parse_partition_index("sda1"), Some(1));
        assert_eq!(parse_partition_index("sda10"), Some(10));
        assert_eq!(parse_partition_index("nvme0n1p2"), Some(2));
        assert_eq!(parse_partition_index("mmcblk0p3"), Some(3));
        assert_eq!(parse_partition_index("sda"), None);
    }

    #[test]
    fn human_size_parses_common_forms() {
        assert_eq!(parse_human_size("1024"), Some(1024));
        assert_eq!(parse_human_size("1K"), Some(1024));
        assert_eq!(parse_human_size("1M"), Some(1024 * 1024));
        assert_eq!(parse_human_size("32G"), Some(32u64 * 1024 * 1024 * 1024));
        assert_eq!(
            parse_human_size("1.8T"),
            Some((1.8 * (1024u64 * 1024 * 1024 * 1024) as f64) as u64)
        );
        assert!(parse_human_size("bogus").is_none());
    }

    #[test]
    fn mountinfo_parse_extracts_source_and_target() {
        let mi = "\
36 35 0:29 / /proc rw,relatime shared:14 - proc proc rw
27 22 8:1 / /boot/efi rw,relatime - vfat /dev/sda1 rw,fmask=0022
28 22 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw,discard
";
        let out = parse_mountinfo(mi).expect("parses");
        assert_eq!(out.len(), 3);
        assert_eq!(out[1].device_node, "/dev/sda1");
        assert_eq!(out[1].mountpoint, "/boot/efi");
        assert_eq!(out[2].device_node, "/dev/sda2");
        assert_eq!(out[2].mountpoint, "/");
    }

    #[test]
    fn mountinfo_decodes_octal_escapes() {
        // A space in the mount path is escaped as \040.
        let mi = "\
40 35 8:1 / /mnt/my\\040music rw,relatime - vfat /dev/sda1 rw
";
        let out = parse_mountinfo(mi).expect("parses");
        assert_eq!(out[0].mountpoint, "/mnt/my music");
    }

    #[test]
    fn swaps_extracts_partition_devices() {
        let sw = "\
Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority
/dev/sda3\t\t\t\tpartition\t1048572\t\t0\t\t-2
/swapfile\t\t\t\tfile\t\t2097148\t\t0\t\t-3
";
        let out = parse_swaps(sw);
        assert!(out.contains("/dev/sda3"));
        assert!(!out.contains("/swapfile"));
    }

    #[test]
    fn role_default_policy_matrix() {
        assert_eq!(
            PartitionRole::SystemRoot.default_policy(),
            MountPolicy::RefusedSystemLive
        );
        assert_eq!(
            PartitionRole::SystemBoot.default_policy(),
            MountPolicy::RefusedSystemLive
        );
        assert_eq!(
            PartitionRole::SystemEfi.default_policy(),
            MountPolicy::RefusedSystemLive
        );
        assert_eq!(
            PartitionRole::SystemSwap.default_policy(),
            MountPolicy::RefusedSystemLive
        );
        assert_eq!(
            PartitionRole::SystemAdjacent.default_policy(),
            MountPolicy::OptInRequired
        );
        assert_eq!(
            PartitionRole::Removable.default_policy(),
            MountPolicy::Auto
        );
    }

    #[test]
    fn role_is_system_live_matrix() {
        assert!(PartitionRole::SystemRoot.is_system_live());
        assert!(PartitionRole::SystemBoot.is_system_live());
        assert!(PartitionRole::SystemEfi.is_system_live());
        assert!(PartitionRole::SystemSwap.is_system_live());
        assert!(!PartitionRole::SystemAdjacent.is_system_live());
        assert!(!PartitionRole::Removable.is_system_live());
    }

    // --- Fixture-style scenario tests ---
    //
    // Each scenario builds mountinfo + swaps + lsblk inline and
    // asserts the classifier output. Full-file fixtures under
    // tests/fixtures/ carry the same scenarios in on-disk form
    // for the wire-boundary regression harness (Step 3).

    fn nvme_boot_lsblk_with_usb_stick() -> &'static str {
        r#"{
          "blockdevices": [
            {"name":"nvme0n1","type":"disk","tran":"nvme","size":512000000000,
             "children":[
               {"name":"nvme0n1p1","type":"part","fstype":"vfat","size":536870912,"partuuid":"aaaa-01"},
               {"name":"nvme0n1p2","type":"part","fstype":"ext4","size":511000000000,"partuuid":"aaaa-02"}
             ]},
            {"name":"sda","type":"disk","tran":"usb","vendor":"SanDisk","model":"Cruzer Blade","serial":"4C530",
             "size":32000000000,
             "children":[
               {"name":"sda1","type":"part","fstype":"vfat","label":"MUSIC","size":32000000000,"partuuid":"bbbb-01"}
             ]}
          ]
        }"#
    }

    #[test]
    fn fixture_nvme_boot_usb_stick() {
        let mi = "\
28 22 259:2 / / rw - ext4 /dev/nvme0n1p2 rw
27 22 259:1 / /boot/efi rw - vfat /dev/nvme0n1p1 rw
";
        let out = classify(
            mi,
            "Filename Type Size Used Priority\n",
            nvme_boot_lsblk_with_usb_stick(),
        )
        .expect("classifies");
        // Only the USB stick appears in the inventory (nvme is
        // not USB-transport). The stick is Removable — nothing
        // on nvme touches it, and its own parent disk has no
        // system-role partitions.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].device_node, "/dev/sda1");
        assert_eq!(out[0].role, PartitionRole::Removable);
        assert_eq!(out[0].mount_policy, MountPolicy::Auto);
        assert_eq!(out[0].vendor.as_deref(), Some("SanDisk"));
        assert_eq!(out[0].model.as_deref(), Some("Cruzer Blade"));
        assert_eq!(out[0].fs_type, "vfat");
        assert_eq!(out[0].label.as_deref(), Some("MUSIC"));
    }

    fn usb_boot_lsblk() -> &'static str {
        // Boot USB SSD at /dev/sda (root on sda2, EFI on sda1);
        // a second USB stick at /dev/sdb (data on sdb1).
        r#"{
          "blockdevices": [
            {"name":"sda","type":"disk","tran":"usb","vendor":"Samsung","model":"T7 Portable","serial":"S6P5",
             "size":1000000000000,
             "children":[
               {"name":"sda1","type":"part","fstype":"vfat","label":"EFI","size":536870912,"partuuid":"cccc-01"},
               {"name":"sda2","type":"part","fstype":"ext4","label":"root","size":999000000000,"partuuid":"cccc-02"}
             ]},
            {"name":"sdb","type":"disk","tran":"usb","vendor":"SanDisk","model":"Cruzer Blade","serial":"4C530",
             "size":32000000000,
             "children":[
               {"name":"sdb1","type":"part","fstype":"vfat","label":"MUSIC","size":32000000000,"partuuid":"dddd-01"}
             ]}
          ]
        }"#
    }

    #[test]
    fn fixture_usb_boot_second_usb_stick() {
        // Root on sda2 (USB SSD), EFI on sda1, sdb1 is a second
        // USB stick for music. Expected:
        //   sda1 → SystemEfi (RefusedSystemLive)
        //   sda2 → SystemRoot (RefusedSystemLive)
        //   sdb1 → Removable (Auto)
        let mi = "\
27 22 8:1 / /boot/efi rw - vfat /dev/sda1 rw
28 22 8:2 / / rw - ext4 /dev/sda2 rw
";
        let out = classify(
            mi,
            "Filename Type Size Used Priority\n",
            usb_boot_lsblk(),
        )
        .expect("classifies");
        assert_eq!(out.len(), 3);

        let sda1 = out.iter().find(|p| p.device_node == "/dev/sda1").unwrap();
        assert_eq!(sda1.role, PartitionRole::SystemEfi);
        assert_eq!(sda1.mount_policy, MountPolicy::RefusedSystemLive);
        assert_eq!(sda1.current_mount.as_deref(), Some("/boot/efi"));

        let sda2 = out.iter().find(|p| p.device_node == "/dev/sda2").unwrap();
        assert_eq!(sda2.role, PartitionRole::SystemRoot);
        assert_eq!(sda2.mount_policy, MountPolicy::RefusedSystemLive);
        assert_eq!(sda2.current_mount.as_deref(), Some("/"));

        let sdb1 = out.iter().find(|p| p.device_node == "/dev/sdb1").unwrap();
        assert_eq!(sdb1.role, PartitionRole::Removable);
        assert_eq!(sdb1.mount_policy, MountPolicy::Auto);
    }

    #[test]
    fn fixture_usb_boot_with_sibling_data_partition() {
        // Same boot USB SSD at /dev/sda, but now sda has THREE
        // partitions: sda1 EFI, sda2 root, sda3 an unmounted
        // ext4 data partition the operator wants as a music
        // source. Expected:
        //   sda1 → SystemEfi
        //   sda2 → SystemRoot
        //   sda3 → SystemAdjacent (OptInRequired)
        let lsblk = r#"{
          "blockdevices": [
            {"name":"sda","type":"disk","tran":"usb","vendor":"Samsung","model":"T7 Portable","serial":"S6P5",
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
        let out = classify(mi, "Filename Type Size Used Priority\n", lsblk)
            .expect("classifies");
        assert_eq!(out.len(), 3);

        let sda3 = out.iter().find(|p| p.device_node == "/dev/sda3").unwrap();
        assert_eq!(sda3.role, PartitionRole::SystemAdjacent);
        assert_eq!(sda3.mount_policy, MountPolicy::OptInRequired);
        assert!(sda3.current_mount.is_none());
        assert_eq!(sda3.label.as_deref(), Some("DATA"));
    }

    #[test]
    fn fixture_usb_boot_with_swap_partition() {
        // Boot USB with sda1=EFI sda2=root sda3=swap sda4=data.
        // Expected: sda3 → SystemSwap; sda4 → SystemAdjacent.
        let lsblk = r#"{
          "blockdevices": [
            {"name":"sda","type":"disk","tran":"usb","vendor":"Samsung","model":"T7","serial":"S6P5",
             "size":1000000000000,
             "children":[
               {"name":"sda1","type":"part","fstype":"vfat","label":"EFI","size":536870912,"partuuid":"eeee-01"},
               {"name":"sda2","type":"part","fstype":"ext4","label":"root","size":400000000000,"partuuid":"eeee-02"},
               {"name":"sda3","type":"part","fstype":"swap","size":8000000000,"partuuid":"eeee-03"},
               {"name":"sda4","type":"part","fstype":"ext4","label":"DATA","size":591000000000,"partuuid":"eeee-04"}
             ]}
          ]
        }"#;
        let mi = "\
27 22 8:1 / /boot/efi rw - vfat /dev/sda1 rw
28 22 8:2 / / rw - ext4 /dev/sda2 rw
";
        let swaps = "\
Filename\t\tType\tSize\tUsed\tPriority
/dev/sda3\t\tpartition\t7812500\t0\t-2
";
        let out = classify(mi, swaps, lsblk).expect("classifies");
        assert_eq!(out.len(), 4);

        let sda3 = out.iter().find(|p| p.device_node == "/dev/sda3").unwrap();
        assert_eq!(sda3.role, PartitionRole::SystemSwap);
        assert_eq!(sda3.mount_policy, MountPolicy::RefusedSystemLive);

        let sda4 = out.iter().find(|p| p.device_node == "/dev/sda4").unwrap();
        assert_eq!(sda4.role, PartitionRole::SystemAdjacent);
    }

    #[test]
    fn fixture_pi5_boot_firmware_convention() {
        // Pi 5 booting from USB SSD with /boot/firmware
        // instead of /boot/efi (Debian raspi convention).
        let lsblk = r#"{
          "blockdevices": [
            {"name":"sda","type":"disk","tran":"usb","vendor":"Samsung","model":"T7","serial":"S6P5",
             "size":1000000000000,
             "children":[
               {"name":"sda1","type":"part","fstype":"vfat","label":"BOOT","size":536870912,"partuuid":"ffff-01"},
               {"name":"sda2","type":"part","fstype":"ext4","label":"root","size":999000000000,"partuuid":"ffff-02"}
             ]}
          ]
        }"#;
        let mi = "\
27 22 8:1 / /boot/firmware rw - vfat /dev/sda1 rw
28 22 8:2 / / rw - ext4 /dev/sda2 rw
";
        let out = classify(mi, "Filename Type Size Used Priority\n", lsblk)
            .expect("classifies");
        let sda1 = out.iter().find(|p| p.device_node == "/dev/sda1").unwrap();
        assert_eq!(sda1.role, PartitionRole::SystemBoot);
        let sda2 = out.iter().find(|p| p.device_node == "/dev/sda2").unwrap();
        assert_eq!(sda2.role, PartitionRole::SystemRoot);
    }

    #[test]
    fn fixture_vm_virtio_boot_usb_passthrough() {
        // VM: root on /dev/vda (virtio, not USB) — invisible to
        // classifier. A USB stick passed through as /dev/sda.
        // Expected: only sda1 in the output, Removable.
        let lsblk = r#"{
          "blockdevices": [
            {"name":"vda","type":"disk","tran":"virtio","size":21474836480,
             "children":[
               {"name":"vda1","type":"part","fstype":"ext4","label":"root","size":21000000000,"partuuid":"9999-01"}
             ]},
            {"name":"sda","type":"disk","tran":"usb","vendor":"SanDisk","model":"Cruzer","serial":"4C530",
             "size":32000000000,
             "children":[
               {"name":"sda1","type":"part","fstype":"vfat","label":"MUSIC","size":32000000000,"partuuid":"aaaa-01"}
             ]}
          ]
        }"#;
        let mi = "\
28 22 254:1 / / rw - ext4 /dev/vda1 rw
";
        let out = classify(mi, "Filename Type Size Used Priority\n", lsblk)
            .expect("classifies");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].device_node, "/dev/sda1");
        assert_eq!(out[0].role, PartitionRole::Removable);
    }

    #[test]
    fn fixture_relabelled_boot_partition_still_system() {
        // Rootfs partition on USB SSD relabelled "USB_MUSIC" —
        // still SystemRoot because the classifier uses mountinfo
        // (parent-of-mount rule), not the label.
        let lsblk = r#"{
          "blockdevices": [
            {"name":"sda","type":"disk","tran":"usb","vendor":"Samsung","model":"T7","serial":"S6P5",
             "size":1000000000000,
             "children":[
               {"name":"sda2","type":"part","fstype":"ext4","label":"USB_MUSIC","size":999000000000,"partuuid":"bbbb-02"}
             ]}
          ]
        }"#;
        let mi = "\
28 22 8:2 / / rw - ext4 /dev/sda2 rw
";
        let out = classify(mi, "Filename Type Size Used Priority\n", lsblk)
            .expect("classifies");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, PartitionRole::SystemRoot);
        assert_eq!(out[0].label.as_deref(), Some("USB_MUSIC"));
    }

    #[test]
    fn fixture_two_identical_media_sticks_yield_two_removable() {
        // Two SanDisk Cruzer Blade sticks plugged simultaneously
        // (same vendor + model, different serial). Both classify
        // as Removable; enumeration + stable-id deconfliction is
        // Step 3's responsibility, not the classifier's.
        let lsblk = r#"{
          "blockdevices": [
            {"name":"sda","type":"disk","tran":"usb","vendor":"SanDisk","model":"Cruzer Blade","serial":"4C530",
             "size":32000000000,
             "children":[
               {"name":"sda1","type":"part","fstype":"vfat","label":"MUSIC","size":32000000000,"partuuid":"aaaa-01"}
             ]},
            {"name":"sdb","type":"disk","tran":"usb","vendor":"SanDisk","model":"Cruzer Blade","serial":"7B221",
             "size":32000000000,
             "children":[
               {"name":"sdb1","type":"part","fstype":"vfat","label":"MUSIC","size":32000000000,"partuuid":"aaaa-02"}
             ]}
          ]
        }"#;
        let out = classify("", "", lsblk).expect("classifies");
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|p| p.role == PartitionRole::Removable));
        assert!(out.iter().all(|p| p.mount_policy == MountPolicy::Auto));
    }

    #[test]
    fn fixture_empty_lsblk_yields_empty_output() {
        let out =
            classify("", "", r#"{"blockdevices":[]}"#).expect("classifies");
        assert!(out.is_empty());
    }

    #[test]
    fn malformed_lsblk_yields_error() {
        let err = classify("", "", "{not json").unwrap_err();
        matches!(err, ClassifierError::LsblkJson(_));
    }

    #[test]
    fn malformed_mountinfo_yields_error() {
        let err = classify(
            "bogus line without enough tokens",
            "",
            r#"{"blockdevices":[]}"#,
        )
        .unwrap_err();
        matches!(err, ClassifierError::Mountinfo(_));
    }
}
