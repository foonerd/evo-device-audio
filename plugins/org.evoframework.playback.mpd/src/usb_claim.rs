// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! What a `local_usb` library source has to be.
//!
//! `library.add_source` reaches this plugin from two directions that
//! look the same on arrival: `storage.usb` registering a stick it has
//! just mounted, and any consumer holding a LAN connection. The
//! request names no caller, so the verb cannot be reserved for the
//! first. What it can do is refuse to take the claim on trust.
//!
//! A `local_usb` source is real only if:
//!
//! - its mount path is a direct child of `<music_directory>/USB`, the
//!   one place `storage.usb` mounts sticks, with no `.` or `..`;
//! - that path is, right now, a mount point on this host;
//! - what is mounted there is a block device (`/dev/...`), and it is
//!   the device the claim names.
//!
//! A path nothing is mounted on, a directory somewhere else, a share
//! mounted under the USB root, or a device other than the one named
//! is refused, and nothing is registered. A genuine attach passes
//! because `storage.usb` mounts first and registers second.

use std::path::{Component, Path, PathBuf};

/// Directory under `music_directory` where USB volumes are mounted.
pub(crate) const USB_MOUNT_SUBDIR: &str = "USB";

/// Why a `local_usb` claim was refused.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum UsbClaimError {
    /// The host's mount table could not be read, so nothing can be
    /// confirmed. Refused, not assumed.
    MountTableUnreadable(String),
    /// The mount path is not an absolute, normal path.
    PathNotNormal(String),
    /// The mount path is not directly under the USB mount root.
    OutsideUsbRoot(String),
    /// Nothing is mounted at the mount path.
    NotMounted(String),
    /// What is mounted there is not a block device.
    NotABlockDevice(String),
    /// The device named in the claim is not the one mounted there.
    DeviceMismatch { claimed: String, mounted: String },
}

impl std::fmt::Display for UsbClaimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MountTableUnreadable(e) => {
                write!(f, "local_usb claim: mount table unreadable: {e}")
            }
            Self::PathNotNormal(p) => write!(
                f,
                "local_usb claim: {p:?} is not a normal absolute path"
            ),
            Self::OutsideUsbRoot(p) => write!(
                f,
                "local_usb claim: {p:?} is not directly under the USB mount root"
            ),
            Self::NotMounted(p) => {
                write!(f, "local_usb claim: nothing is mounted at {p:?}")
            }
            Self::NotABlockDevice(s) => write!(
                f,
                "local_usb claim: the mount is {s:?}, not a block device"
            ),
            Self::DeviceMismatch { claimed, mounted } => write!(
                f,
                "local_usb claim: names {claimed:?} but {mounted:?} is mounted there"
            ),
        }
    }
}

/// The host facts a claim is checked against.
pub(crate) trait HostMounts: Sync {
    /// Contents of the mount table, in `/proc/self/mountinfo` format.
    fn mountinfo(&self) -> std::io::Result<String>;
    /// The path a device name resolves to, symlinks followed.
    fn canonical(&self, path: &str) -> Option<PathBuf>;
}

/// The real host.
pub(crate) struct SystemMounts;

impl HostMounts for SystemMounts {
    fn mountinfo(&self) -> std::io::Result<String> {
        std::fs::read_to_string("/proc/self/mountinfo")
    }
    fn canonical(&self, path: &str) -> Option<PathBuf> {
        std::fs::canonicalize(path).ok()
    }
}

/// Decode the octal escapes (`\040` for a space) mountinfo uses.
fn unescape(field: &str) -> String {
    let b = field.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let escaped = b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..=i + 3].iter().all(|c| (b'0'..=b'7').contains(c));
        if escaped {
            out.push(
                (b[i + 1] - b'0') * 64
                    + (b[i + 2] - b'0') * 8
                    + (b[i + 3] - b'0'),
            );
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `(fstype, source)` of what is visible at `mount_point`. When a
/// path is mounted more than once, the last entry is the one in view.
fn mounted_at(mountinfo: &str, mount_point: &str) -> Option<(String, String)> {
    let mut found = None;
    for line in mountinfo.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        if left.split_whitespace().nth(4).map(unescape).as_deref()
            != Some(mount_point)
        {
            continue;
        }
        let mut r = right.split_whitespace();
        if let Some(fstype) = r.next() {
            let source = r.next().map(unescape).unwrap_or_default();
            found = Some((fstype.to_string(), source));
        }
    }
    found
}

/// Confirm a `local_usb` claim against the host. See the module
/// documentation for what is checked.
pub(crate) fn verify_usb_claim(
    music_directory: &Path,
    mount_path: &Path,
    device_node: &str,
    host: &dyn HostMounts,
) -> Result<(), UsbClaimError> {
    let shown = mount_path.display().to_string();
    let normal = mount_path.is_absolute()
        && mount_path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)));
    if !normal {
        return Err(UsbClaimError::PathNotNormal(shown));
    }
    let root = music_directory.join(USB_MOUNT_SUBDIR);
    if mount_path.parent() != Some(root.as_path())
        || mount_path.file_name().is_none()
    {
        return Err(UsbClaimError::OutsideUsbRoot(shown));
    }
    let table = host
        .mountinfo()
        .map_err(|e| UsbClaimError::MountTableUnreadable(e.to_string()))?;
    let Some((_fstype, source)) = mounted_at(&table, &shown) else {
        return Err(UsbClaimError::NotMounted(shown));
    };
    if !source.starts_with("/dev/") {
        return Err(UsbClaimError::NotABlockDevice(source));
    }
    let mounted = host
        .canonical(&source)
        .unwrap_or_else(|| PathBuf::from(&source));
    let claimed = host.canonical(device_node);
    if claimed.as_deref() != Some(mounted.as_path()) {
        return Err(UsbClaimError::DeviceMismatch {
            claimed: device_node.to_string(),
            mounted: mounted.display().to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const MUSIC: &str = "/var/lib/evo/music";
    const STICK: &str = "/var/lib/evo/music/USB/Audio";

    struct Host {
        table: Result<String, String>,
        links: HashMap<String, PathBuf>,
    }

    impl Host {
        fn with_stick() -> Self {
            Self {
                table: Ok(format!(
                    "36 25 8:1 / {STICK} rw,relatime shared:5 - vfat /dev/sda1 rw\n\
                     40 25 0:40 / /var/lib/evo/music/NAS/share ro - cifs //nas/x rw\n\
                     41 25 0:41 / /var/lib/evo/music/USB/Fake rw - tmpfs tmpfs rw\n\
                     42 25 179:1 / /boot rw - vfat /dev/mmcblk0p1 rw\n"
                )),
                links: HashMap::from([
                    (
                        "/dev/disk/by-uuid/ABCD-1234".to_string(),
                        PathBuf::from("/dev/sda1"),
                    ),
                    ("/dev/sda1".to_string(), PathBuf::from("/dev/sda1")),
                    ("/dev/sdb1".to_string(), PathBuf::from("/dev/sdb1")),
                    (
                        "/dev/mmcblk0p1".to_string(),
                        PathBuf::from("/dev/mmcblk0p1"),
                    ),
                ]),
            }
        }
    }

    impl HostMounts for Host {
        fn mountinfo(&self) -> std::io::Result<String> {
            self.table
                .clone()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
        }
        fn canonical(&self, path: &str) -> Option<PathBuf> {
            self.links.get(path).cloned()
        }
    }

    fn check(path: &str, dev: &str, host: &Host) -> Result<(), UsbClaimError> {
        verify_usb_claim(Path::new(MUSIC), Path::new(path), dev, host)
    }

    /// What `storage.usb` sends after it mounts a stick: accepted.
    #[test]
    fn a_genuinely_mounted_stick_is_accepted() {
        let h = Host::with_stick();
        assert_eq!(check(STICK, "/dev/disk/by-uuid/ABCD-1234", &h), Ok(()));
        assert_eq!(check(STICK, "/dev/sda1", &h), Ok(()));
    }

    /// The hole: an arbitrary path claimed as USB.
    #[test]
    fn an_arbitrary_path_is_not_a_usb_source() {
        let h = Host::with_stick();
        for p in [
            "/etc",
            "/var/lib/evo/music/INTERNAL",
            "/var/lib/evo/music/USB",
            "/var/lib/evo/music/USB/Audio/sub",
            "/var/lib/evo/music/USB/../NAS/share",
            "/var/lib/evo/music/USB/./Audio",
            "relative/USB/Audio",
        ] {
            assert!(check(p, "/dev/sda1", &h).is_err(), "{p} must be refused");
        }
    }

    /// A real block device, mounted somewhere that is not the USB
    /// root, is not a USB source: the box's own boot partition cannot
    /// be registered as a stick.
    #[test]
    fn a_block_device_mounted_outside_the_usb_root_is_refused() {
        let h = Host::with_stick();
        assert!(matches!(
            check("/boot", "/dev/mmcblk0p1", &h),
            Err(UsbClaimError::OutsideUsbRoot(_))
        ));
    }

    #[test]
    fn a_path_nothing_is_mounted_on_is_refused() {
        let h = Host::with_stick();
        assert_eq!(
            check("/var/lib/evo/music/USB/Ghost", "/dev/sda1", &h),
            Err(UsbClaimError::NotMounted(
                "/var/lib/evo/music/USB/Ghost".into()
            ))
        );
    }

    #[test]
    fn a_non_block_mount_under_the_usb_root_is_refused() {
        let h = Host::with_stick();
        assert!(matches!(
            check("/var/lib/evo/music/USB/Fake", "/dev/sda1", &h),
            Err(UsbClaimError::NotABlockDevice(_))
        ));
    }

    #[test]
    fn a_different_device_than_the_one_mounted_is_refused() {
        let h = Host::with_stick();
        assert!(matches!(
            check(STICK, "/dev/sdb1", &h),
            Err(UsbClaimError::DeviceMismatch { .. })
        ));
        assert!(matches!(
            check(STICK, "/dev/nonexistent", &h),
            Err(UsbClaimError::DeviceMismatch { .. })
        ));
    }

    /// Nothing can be confirmed without the table: refuse.
    #[test]
    fn an_unreadable_mount_table_refuses() {
        let mut h = Host::with_stick();
        h.table = Err("denied".into());
        assert!(matches!(
            check(STICK, "/dev/sda1", &h),
            Err(UsbClaimError::MountTableUnreadable(_))
        ));
    }

    /// mountinfo escapes a space as `\040`; a stick named with one
    /// still matches its own path.
    #[test]
    fn escaped_mount_points_are_decoded() {
        let mut h = Host::with_stick();
        h.table = Ok(
            "36 25 8:1 / /var/lib/evo/music/USB/My\\040Stick rw - vfat /dev/sda1 rw\n"
                .into(),
        );
        assert_eq!(
            check("/var/lib/evo/music/USB/My Stick", "/dev/sda1", &h),
            Ok(())
        );
    }

    /// The visible mount is the last one at that point.
    #[test]
    fn the_last_mount_at_a_path_is_the_one_in_view() {
        let mut h = Host::with_stick();
        h.table = Ok(format!(
            "1 0 8:1 / {STICK} rw - vfat /dev/sda1 rw\n2 0 0:9 / {STICK} rw - tmpfs tmpfs rw\n"
        ));
        assert!(matches!(
            check(STICK, "/dev/sda1", &h),
            Err(UsbClaimError::NotABlockDevice(_))
        ));
    }
}
