// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Persisted renderer identity and switch.
//!
//! One small JSON file in the plugin's state directory: whether the
//! operator enabled the renderer, the device UUID the network knows it
//! by, and the TCP port the description is served on. The UUID and the
//! port persist so a controller that remembered the device finds it at
//! the same address after a restart.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "renderer.json";

/// What survives a restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Persisted {
    /// Operator switch. Defaults to off.
    #[serde(default)]
    pub enabled: bool,
    /// Device UUID, minted once.
    pub uuid: String,
    /// TCP port for the description and control endpoints; `0` until the
    /// first start picks one.
    #[serde(default)]
    pub port: u16,
}

impl Persisted {
    /// A fresh identity, disabled.
    pub fn fresh() -> Self {
        Self {
            enabled: false,
            uuid: uuid::Uuid::new_v4().to_string(),
            port: 0,
        }
    }
}

/// State load/save failures.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// The state directory or file could not be read or written.
    #[error("renderer state {path}: {source}")]
    Io {
        /// The file involved.
        path: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },
}

fn file(dir: &Path) -> PathBuf {
    dir.join(FILE_NAME)
}

/// Load the state, creating and saving a fresh one when none exists or the
/// file is unreadable JSON (a damaged file must not stop the plugin; the
/// device simply gets a new identity and comes back disabled).
pub fn load_or_create(dir: &Path) -> Result<Persisted, StateError> {
    let path = file(dir);
    match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<Persisted>(&bytes) {
            Ok(p) if uuid::Uuid::parse_str(&p.uuid).is_ok() => Ok(p),
            _ => {
                tracing::warn!(
                    path = %path.display(),
                    "renderer state unreadable; minting a new identity, disabled"
                );
                let p = Persisted::fresh();
                save(dir, &p)?;
                Ok(p)
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let p = Persisted::fresh();
            save(dir, &p)?;
            Ok(p)
        }
        Err(source) => Err(StateError::Io { path, source }),
    }
}

/// Write the state atomically (temp file, then rename).
pub fn save(dir: &Path, p: &Persisted) -> Result<(), StateError> {
    let path = file(dir);
    let tmp = dir.join(format!("{FILE_NAME}.tmp"));
    let bytes = serde_json::to_vec_pretty(p).expect("state serialises");
    std::fs::create_dir_all(dir).map_err(|source| StateError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    std::fs::write(&tmp, bytes).map_err(|source| StateError::Io {
        path: tmp.clone(),
        source,
    })?;
    std::fs::rename(&tmp, &path)
        .map_err(|source| StateError::Io { path, source })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_load_is_disabled_with_a_stable_identity() {
        let d = tempfile::tempdir().unwrap();
        let a = load_or_create(d.path()).unwrap();
        assert!(
            !a.enabled,
            "the renderer is off until an operator enables it"
        );
        assert_eq!(a.port, 0);
        let b = load_or_create(d.path()).unwrap();
        assert_eq!(a.uuid, b.uuid, "the identity survives a reload");
    }

    #[test]
    fn a_saved_switch_and_port_survive() {
        let d = tempfile::tempdir().unwrap();
        let mut p = load_or_create(d.path()).unwrap();
        p.enabled = true;
        p.port = 49494;
        save(d.path(), &p).unwrap();
        assert_eq!(load_or_create(d.path()).unwrap(), p);
    }

    #[test]
    fn a_damaged_file_comes_back_disabled_with_a_new_identity() {
        let d = tempfile::tempdir().unwrap();
        let first = load_or_create(d.path()).unwrap();
        std::fs::write(d.path().join(FILE_NAME), b"{not json").unwrap();
        let again = load_or_create(d.path()).unwrap();
        assert!(!again.enabled);
        assert_ne!(first.uuid, again.uuid);
    }
}
