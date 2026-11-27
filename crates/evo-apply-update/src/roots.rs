// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Where the worker reads and writes, and the check that those places are
//! root-owned all the way up. The service user owns
//! `/etc/evo`, `/var/lib/evo` and, on drifted boxes, `/opt/evo`; the
//! worker takes nothing from under them.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::error::{Res, UpdError};

/// The worker's paths and the identity it requires of them.
#[derive(Debug, Clone)]
pub struct Roots {
    /// `/etc/evo-updater`: `updater.toml`, `allow.toml`, `trust.d/`.
    pub etc_dir: PathBuf,
    /// `/var/lib/evo-updater`: results and scratch.
    pub state_dir: PathBuf,
    /// `/opt/evo/plugins`.
    pub plugins_dir: PathBuf,
    /// `/opt/evo/rollback`.
    pub rollback_dir: PathBuf,
    /// `/usr/local/bin`.
    pub bin_dir: PathBuf,
    /// `/etc/sudoers.d`.
    pub sudoers_dir: PathBuf,
    /// `/var/lib/evo/installed.toml`.
    pub installed_record: PathBuf,
    /// `/run/evo/plugins`: where an admitted plugin's socket appears.
    pub socket_dir: PathBuf,
    /// The uid every checked path must belong to (0 in production).
    pub owner_uid: u32,
    /// Ancestors are checked up to, not including, this path.
    pub chain_stop: PathBuf,
}

impl Roots {
    /// The production layout.
    pub fn system() -> Self {
        Self {
            etc_dir: "/etc/evo-updater".into(),
            state_dir: "/var/lib/evo-updater".into(),
            plugins_dir: "/opt/evo/plugins".into(),
            rollback_dir: "/opt/evo/rollback".into(),
            bin_dir: "/usr/local/bin".into(),
            sudoers_dir: "/etc/sudoers.d".into(),
            installed_record: "/var/lib/evo/installed.toml".into(),
            socket_dir: "/run/evo/plugins".into(),
            owner_uid: 0,
            chain_stop: "/".into(),
        }
    }

    /// `updater.toml`.
    pub fn config_path(&self) -> PathBuf {
        self.etc_dir.join("updater.toml")
    }
    /// `allow.toml`.
    pub fn allow_path(&self) -> PathBuf {
        self.etc_dir.join("allow.toml")
    }
    /// `trust.d`.
    pub fn trust_dir(&self) -> PathBuf {
        self.etc_dir.join("trust.d")
    }

    /// Require `path` and every ancestor below `chain_stop` to be a real
    /// directory (not a symlink) owned by `owner_uid` and not writable by
    /// group or other.
    pub fn check_chain(&self, path: &Path) -> Res<()> {
        let mut p = path.to_path_buf();
        while p != self.chain_stop {
            let meta = fs::symlink_metadata(&p).map_err(|e| {
                UpdError::new("root_missing", format!("{}: {e}", p.display()))
            })?;
            if meta.file_type().is_symlink() {
                return Err(UpdError::new(
                    "root_is_symlink",
                    format!("{} is a symlink", p.display()),
                ));
            }
            if !meta.is_dir() {
                return Err(UpdError::new(
                    "root_not_directory",
                    format!("{} is not a directory", p.display()),
                ));
            }
            if meta.uid() != self.owner_uid {
                return Err(UpdError::new(
                    "root_not_owned",
                    format!(
                        "{} is not owned by uid {}",
                        p.display(),
                        self.owner_uid
                    ),
                ));
            }
            if meta.mode() & 0o022 != 0 {
                return Err(UpdError::new(
                    "root_writable",
                    format!("{} is writable by group or other", p.display()),
                ));
            }
            match p.parent() {
                Some(parent) => p = parent.to_path_buf(),
                None => break,
            }
        }
        Ok(())
    }

    /// The roots a check needs.
    pub fn check_for_read(&self) -> Res<()> {
        self.check_chain(&self.etc_dir)?;
        self.check_chain(&self.state_dir)
    }

    /// The roots an apply needs.
    pub fn check_for_write(&self) -> Res<()> {
        self.check_for_read()?;
        self.check_chain(&self.plugins_dir)?;
        self.check_chain(&self.rollback_dir)?;
        self.check_chain(&self.bin_dir)?;
        self.check_chain(&self.sudoers_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn roots_in(base: &Path) -> Roots {
        let uid = fs::metadata(base).unwrap().uid();
        let mut r = Roots::system();
        r.etc_dir = base.join("etc");
        r.state_dir = base.join("state");
        r.plugins_dir = base.join("plugins");
        r.rollback_dir = base.join("rollback");
        r.bin_dir = base.join("bin");
        r.sudoers_dir = base.join("sudoers.d");
        r.owner_uid = uid;
        r.chain_stop = base.to_path_buf();
        for d in [
            &r.etc_dir,
            &r.state_dir,
            &r.plugins_dir,
            &r.rollback_dir,
            &r.bin_dir,
            &r.sudoers_dir,
        ] {
            fs::create_dir_all(d).unwrap();
            fs::set_permissions(d, fs::Permissions::from_mode(0o755)).unwrap();
        }
        r
    }

    #[test]
    fn a_clean_chain_passes() {
        let d = tempfile::tempdir().unwrap();
        roots_in(d.path()).check_for_write().unwrap();
    }

    #[test]
    fn a_root_the_wrong_uid_owns_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let mut r = roots_in(d.path());
        r.owner_uid += 1;
        assert_eq!(r.check_for_read().unwrap_err().code, "root_not_owned");
    }

    #[test]
    fn a_group_writable_root_or_ancestor_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let r = roots_in(d.path());
        fs::set_permissions(&r.plugins_dir, fs::Permissions::from_mode(0o775))
            .unwrap();
        assert_eq!(r.check_for_write().unwrap_err().code, "root_writable");
        fs::set_permissions(&r.plugins_dir, fs::Permissions::from_mode(0o755))
            .unwrap();
        let nested = d.path().join("opt/evo/plugins");
        fs::create_dir_all(&nested).unwrap();
        fs::set_permissions(
            d.path().join("opt/evo"),
            fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        let mut r2 = r.clone();
        r2.plugins_dir = nested;
        assert_eq!(
            r2.check_chain(&r2.plugins_dir).unwrap_err().code,
            "root_writable"
        );
    }

    #[test]
    fn a_symlinked_root_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let mut r = roots_in(d.path());
        let real = d.path().join("elsewhere");
        fs::create_dir(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();
        let link = d.path().join("link");
        symlink(&real, &link).unwrap();
        r.plugins_dir = link;
        assert_eq!(
            r.check_chain(&r.plugins_dir).unwrap_err().code,
            "root_is_symlink"
        );
    }

    #[test]
    fn a_missing_root_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let mut r = roots_in(d.path());
        r.rollback_dir = d.path().join("nope");
        assert_eq!(r.check_for_write().unwrap_err().code, "root_missing");
    }
}
