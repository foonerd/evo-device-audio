// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! What survives a restart: the operator's switch, the control port, and the
//! pairing password.
//!
//! The switch and the port are one small JSON file in the plugin's state
//! directory. The port persists so that a sender that remembered the device
//! finds it at the same address after a restart. The password is a separate
//! file in the credentials directory, written with owner-only permission, and
//! is never part of anything that is published, logged or returned.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const STATE_FILE: &str = "receiver.json";
const PASSWORD_FILE: &str = "password";

/// What the state file holds.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize,
)]
pub struct Persisted {
    /// Operator switch. Defaults to off.
    #[serde(default)]
    pub enabled: bool,
    /// TCP control port; `0` until the first start picks one.
    #[serde(default)]
    pub port: u16,
}

/// State load/save failures.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// A file or directory could not be read or written.
    #[error("receiver state {path}: {source}")]
    Io {
        /// The path involved.
        path: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },
}

fn io_error(path: &Path) -> impl FnOnce(std::io::Error) -> StateError + '_ {
    move |source| StateError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Load the switch and port. A missing or damaged file means disabled: a
/// receiver must never come up on by accident.
pub fn load(dir: &Path) -> Persisted {
    let path = dir.join(STATE_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            tracing::warn!(
                path = %path.display(),
                "receiver state unreadable; starting disabled"
            );
            Persisted::default()
        }),
        Err(_) => Persisted::default(),
    }
}

/// Write the switch and port atomically (temporary file, then rename).
pub fn save(dir: &Path, state: &Persisted) -> Result<(), StateError> {
    std::fs::create_dir_all(dir).map_err(io_error(dir))?;
    let path = dir.join(STATE_FILE);
    let tmp = dir.join(format!("{STATE_FILE}.tmp"));
    let bytes = serde_json::to_vec_pretty(state).unwrap_or_default();
    std::fs::write(&tmp, bytes).map_err(io_error(&tmp))?;
    std::fs::rename(&tmp, &path).map_err(io_error(&path))
}

/// The stored pairing password, if one is set. An unreadable or empty file
/// is no password.
pub fn load_password(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(PASSWORD_FILE)).ok()?;
    let text = text.trim_end_matches(['\n', '\r']).to_string();
    (!text.is_empty()).then_some(text)
}

/// Store the pairing password with owner-only permission, or remove it when
/// `None`.
pub fn store_password(
    dir: &Path,
    password: Option<&str>,
) -> Result<(), StateError> {
    let path = dir.join(PASSWORD_FILE);
    let Some(password) = password else {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_error(&path)(e)),
        };
    };
    std::fs::create_dir_all(dir).map_err(io_error(dir))?;
    let tmp = dir.join(format!("{PASSWORD_FILE}.tmp"));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp).map_err(io_error(&tmp))?;
    file.write_all(password.as_bytes())
        .map_err(io_error(&tmp))?;
    file.sync_all().map_err(io_error(&tmp))?;
    std::fs::rename(&tmp, &path).map_err(io_error(&path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_fresh_directory_is_disabled_with_no_port() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(load(d.path()), Persisted::default());
        assert!(!load(d.path()).enabled, "off until an operator enables it");
    }

    #[test]
    fn a_state_file_that_lacks_the_switch_is_disabled() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(STATE_FILE), br#"{"port": 7000}"#)
            .unwrap();
        let p = load(d.path());
        assert!(!p.enabled, "a missing switch must never read as on");
        assert_eq!(p.port, 7000);
        std::fs::write(d.path().join(STATE_FILE), b"{}").unwrap();
        assert!(!load(d.path()).enabled);
    }

    #[test]
    fn a_saved_switch_and_port_survive() {
        let d = tempfile::tempdir().unwrap();
        let p = Persisted {
            enabled: true,
            port: 7000,
        };
        save(d.path(), &p).unwrap();
        assert_eq!(load(d.path()), p);
    }

    #[test]
    fn a_damaged_state_file_comes_back_disabled() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(STATE_FILE), b"{not json").unwrap();
        assert_eq!(load(d.path()), Persisted::default());
    }

    #[test]
    fn a_password_is_stored_owner_only_and_read_back() {
        let d = tempfile::tempdir().unwrap();
        store_password(d.path(), Some("sekrit-1")).unwrap();
        assert_eq!(load_password(d.path()).as_deref(), Some("sekrit-1"));
        let mode = std::fs::metadata(d.path().join(PASSWORD_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "owner read and write only");
    }

    #[test]
    fn clearing_the_password_removes_it_and_is_idempotent() {
        let d = tempfile::tempdir().unwrap();
        store_password(d.path(), Some("sekrit-1")).unwrap();
        store_password(d.path(), None).unwrap();
        assert_eq!(load_password(d.path()), None);
        store_password(d.path(), None).unwrap();
    }

    #[test]
    fn an_empty_password_file_is_no_password() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(PASSWORD_FILE), b"\n").unwrap();
        assert_eq!(load_password(d.path()), None);
    }
}
