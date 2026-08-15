// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Filesystem support matrix.
//!
//! The support matrix is the sole source of truth for the four
//! things the mount + repair pipeline needs per FS family:
//!
//! - Mount options string (interpolated with the steward's
//!   effective uid/gid at build time).
//! - Volume-size cap (currently only FAT32 at 2 TiB).
//! - Dirty-detection invocation.
//! - Repair-tool invocation.
//!
//! The runtime consults [`FsFamily::from_lsblk`] on every
//! [`crate::classifier::ClassifiedPartition`] to decide whether
//! the drive is mountable at all. Values in the matrix mirror
//! `USB-STORAGE.md` §2; changes to the matrix land in this file
//! and the doc in the same commit per §13 change discipline.

/// Filesystem family recognised by the plugin.
///
/// A value not in this enum surfaces as
/// [`FsFamily::Unsupported`], which the runtime maps to
/// `class: "unsupported"` in the drive listing (no actionable
/// verbs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsFamily {
    /// FAT16 / FAT32.
    Vfat,
    /// exFAT.
    Exfat,
    /// NTFS.
    Ntfs,
    /// ext2 / ext3 / ext4.
    Ext,
    /// Any other filesystem — reported unsupported.
    Unsupported,
}

impl FsFamily {
    /// Map a raw lsblk `FSTYPE` string to a family enum. Empty
    /// string / unknown → [`FsFamily::Unsupported`].
    pub fn from_lsblk(fstype: &str) -> Self {
        match fstype {
            "vfat" => FsFamily::Vfat,
            "exfat" => FsFamily::Exfat,
            "ntfs" | "ntfs3" => FsFamily::Ntfs,
            "ext2" | "ext3" | "ext4" => FsFamily::Ext,
            _ => FsFamily::Unsupported,
        }
    }

    /// Volume-size cap for this family, in bytes. FAT32 is the
    /// only family with a plugin-enforced cap (2 TiB); others
    /// mount at whatever the kernel reports. `None` means
    /// uncapped.
    pub fn size_cap_bytes(self) -> Option<u64> {
        match self {
            FsFamily::Vfat => Some(2u64 * 1024 * 1024 * 1024 * 1024),
            _ => None,
        }
    }

    /// Return the mount option string for this family with the
    /// steward's effective uid/gid interpolated. The runtime
    /// resolves uid/gid at plugin load via
    /// `detect_service_user_from_procfs` (same pattern the
    /// smb-server plugin uses).
    pub fn mount_options(self, uid: u32, gid: u32) -> String {
        match self {
            FsFamily::Vfat => format!(
                "noatime,dmask=0000,fmask=0000,iocharset=utf8,uid={uid},gid={gid}"
            ),
            FsFamily::Exfat => format!(
                "noatime,dmask=0000,fmask=0000,iocharset=utf8,uid={uid},gid={gid}"
            ),
            FsFamily::Ntfs => format!(
                "noatime,dmask=0000,fmask=0000,uid={uid},gid={gid},windows_names,big_writes"
            ),
            FsFamily::Ext => "noatime".to_string(),
            FsFamily::Unsupported => String::new(),
        }
    }

    /// Filesystem-repair tool binary name. Not used directly by
    /// the runtime — the narrow root-only wrapper dispatches
    /// based on the `fs-type` argv the runtime supplies and
    /// invokes the matching binary internally. Exposed here so
    /// the runtime can log the tool it EXPECTS the wrapper to
    /// invoke for diagnostics.
    pub fn repair_tool(self) -> Option<&'static str> {
        match self {
            FsFamily::Vfat => Some("fsck.vfat"),
            FsFamily::Exfat => Some("fsck.exfat"),
            FsFamily::Ntfs => Some("ntfsfix"),
            FsFamily::Ext => Some("e2fsck"),
            FsFamily::Unsupported => None,
        }
    }

    /// Stable string form for the wrapper argv (`fs-type`
    /// positional). Kept distinct from the debug representation
    /// so a rename here doesn't silently break argv shape.
    pub fn wrapper_fs_arg(self) -> Option<&'static str> {
        match self {
            FsFamily::Vfat => Some("vfat"),
            FsFamily::Exfat => Some("exfat"),
            FsFamily::Ntfs => Some("ntfs"),
            FsFamily::Ext => Some("ext4"),
            FsFamily::Unsupported => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_lsblk_fstype_to_family() {
        assert_eq!(FsFamily::from_lsblk("vfat"), FsFamily::Vfat);
        assert_eq!(FsFamily::from_lsblk("exfat"), FsFamily::Exfat);
        assert_eq!(FsFamily::from_lsblk("ntfs"), FsFamily::Ntfs);
        assert_eq!(FsFamily::from_lsblk("ntfs3"), FsFamily::Ntfs);
        assert_eq!(FsFamily::from_lsblk("ext2"), FsFamily::Ext);
        assert_eq!(FsFamily::from_lsblk("ext3"), FsFamily::Ext);
        assert_eq!(FsFamily::from_lsblk("ext4"), FsFamily::Ext);
        assert_eq!(FsFamily::from_lsblk(""), FsFamily::Unsupported);
        assert_eq!(FsFamily::from_lsblk("btrfs"), FsFamily::Unsupported);
    }

    #[test]
    fn vfat_size_cap_is_2tib() {
        assert_eq!(
            FsFamily::Vfat.size_cap_bytes(),
            Some(2u64 * 1024 * 1024 * 1024 * 1024)
        );
    }

    #[test]
    fn non_vfat_families_uncapped() {
        assert!(FsFamily::Exfat.size_cap_bytes().is_none());
        assert!(FsFamily::Ntfs.size_cap_bytes().is_none());
        assert!(FsFamily::Ext.size_cap_bytes().is_none());
    }

    #[test]
    fn mount_options_interpolate_uid_gid() {
        let opts = FsFamily::Vfat.mount_options(1000, 1000);
        assert!(opts.contains("uid=1000"));
        assert!(opts.contains("gid=1000"));
        assert!(opts.contains("iocharset=utf8"));
    }

    #[test]
    fn ntfs_mount_options_include_windows_names() {
        let opts = FsFamily::Ntfs.mount_options(1000, 1000);
        assert!(opts.contains("windows_names"));
        assert!(opts.contains("big_writes"));
    }

    #[test]
    fn ext_mount_options_are_minimal() {
        assert_eq!(FsFamily::Ext.mount_options(1000, 1000), "noatime");
    }

    #[test]
    fn repair_tool_matrix() {
        assert_eq!(FsFamily::Vfat.repair_tool(), Some("fsck.vfat"));
        assert_eq!(FsFamily::Exfat.repair_tool(), Some("fsck.exfat"));
        assert_eq!(FsFamily::Ntfs.repair_tool(), Some("ntfsfix"));
        assert_eq!(FsFamily::Ext.repair_tool(), Some("e2fsck"));
        assert!(FsFamily::Unsupported.repair_tool().is_none());
    }

    #[test]
    fn wrapper_fs_arg_is_stable() {
        // The wrapper argv strings are pinned. Renaming these is
        // a wire-shape change and requires a wrapper argv-shape
        // version bump (`evo-usb-mount --version` from "1" to
        // "2" and a coordinated bootstrap update).
        assert_eq!(FsFamily::Vfat.wrapper_fs_arg(), Some("vfat"));
        assert_eq!(FsFamily::Exfat.wrapper_fs_arg(), Some("exfat"));
        assert_eq!(FsFamily::Ntfs.wrapper_fs_arg(), Some("ntfs"));
        assert_eq!(FsFamily::Ext.wrapper_fs_arg(), Some("ext4"));
    }
}
