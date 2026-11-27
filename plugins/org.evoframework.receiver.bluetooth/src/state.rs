// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! What survives a restart: the operator's switch, and nothing else.
//!
//! The pairing window does not survive a restart, on purpose: a plugin that
//! comes back up never comes back with the adapter open. The paired devices
//! and their keys are the host stack's, not ours.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const STATE_FILE: &str = "receiver.json";

/// What the state file holds.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize,
)]
pub struct Persisted {
    /// Operator switch. Defaults to off.
    #[serde(default)]
    pub enabled: bool,
    /// The stack's own discoverable and pairable timeouts, while a pairing
    /// window has them changed. If the plugin dies mid-window the next start
    /// puts them back; without this the window's length would stay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_timeouts: Option<(u32, u32)>,
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

/// Load the switch. A missing or damaged file means disabled: a receiver must
/// never come up on by accident.
pub fn load(dir: &Path) -> Persisted {
    let path = dir.join(STATE_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            tracing::warn!(path = %path.display(), "receiver state unreadable; starting disabled");
            Persisted::default()
        }),
        Err(_) => Persisted::default(),
    }
}

/// Write the switch atomically (temporary file, then rename).
pub fn save(dir: &Path, state: &Persisted) -> Result<(), StateError> {
    std::fs::create_dir_all(dir).map_err(io_error(dir))?;
    let path = dir.join(STATE_FILE);
    let tmp = dir.join(format!("{STATE_FILE}.tmp"));
    let bytes = serde_json::to_vec_pretty(state).unwrap_or_default();
    std::fs::write(&tmp, bytes).map_err(io_error(&tmp))?;
    std::fs::rename(&tmp, &path).map_err(io_error(&path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_stored_means_disabled() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(load(d.path()), Persisted::default());
    }

    #[test]
    fn the_switch_round_trips() {
        let d = tempfile::tempdir().unwrap();
        save(
            d.path(),
            &Persisted {
                enabled: true,
                restore_timeouts: Some((180, 0)),
            },
        )
        .unwrap();
        assert!(load(d.path()).enabled);
        assert_eq!(load(d.path()).restore_timeouts, Some((180, 0)));
        save(d.path(), &Persisted::default()).unwrap();
        assert!(!load(d.path()).enabled);
        assert_eq!(load(d.path()).restore_timeouts, None);
    }

    #[test]
    fn a_damaged_file_means_disabled() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(STATE_FILE), b"{ not json").unwrap();
        assert!(!load(d.path()).enabled);
        std::fs::write(d.path().join(STATE_FILE), b"\xff\xfe").unwrap();
        assert!(!load(d.path()).enabled);
    }
}
