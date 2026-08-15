// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Operator-alias persistence for USB drives.
//!
//! The alias store maps `(vendor, model, serial_short, partuuid)`
//! tuples to friendly operator names. On every classifier output,
//! the runtime looks up each partition's identity tuple in this
//! store; a hit becomes rule 0 in the stable-id derivation ladder
//! (see [`crate::stable_id`]). Aliases survive replug because the
//! identity tuple travels with the physical drive, not the port.
//!
//! # Read/write split
//!
//! Step 3 (this module) ships the **read** path: parse
//! `aliases.toml`, look up by identity tuple, return the alias.
//! Step 6 lands the **write** path — the `storage.usb.rename`
//! verb — which mutates + persists the file atomically. The
//! shape of the on-disk record is fixed here so Step 6's writer
//! is compatible with Step 3's reader by construction.
//!
//! # File format
//!
//! `<state_dir>/state/aliases.toml`, mode 0600, owned by the
//! steward service user. TOML shape:
//!
//! ```toml
//! schema_version = 1
//!
//! [[alias]]
//! vendor        = "SanDisk"
//! model         = "Cruzer-Blade"
//! serial_short  = "4C530"
//! partuuid      = "a1b2c3d4-01"
//! alias         = "My-Vinyl-Rip"
//! set_at_ms     = 1786100000000
//! ```
//!
//! # Match rules
//!
//! - **Exact tuple match** (all four fields present + equal) →
//!   alias applies.
//! - **Partial tuple match** — when the classifier record lacks
//!   `partuuid` (MBR-partitioned stick), the reader falls back to
//!   `(vendor, model, serial_short, partition_index)` and matches
//!   on that. Documented in the alias-set flow so operators know
//!   MBR aliases are a bit coarser.
//! - **No match** → `None`; derivation falls through to rule 1.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Current on-disk schema version. Bumps land alongside a
/// migration path in [`AliasStore::load`].
pub const ALIAS_SCHEMA_VERSION: u32 = 1;

/// File name under `<state_dir>/state/`.
pub const ALIAS_FILE_NAME: &str = "aliases.toml";

/// On-disk root.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AliasFile {
    /// Schema version marker.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// One entry per operator-renamed drive.
    #[serde(default)]
    pub alias: Vec<AliasEntry>,
}

fn default_schema_version() -> u32 {
    ALIAS_SCHEMA_VERSION
}

/// One persisted `(identity → alias)` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AliasEntry {
    /// Udev `ID_VENDOR` at the time the operator set the alias.
    #[serde(default)]
    pub vendor: Option<String>,
    /// Udev `ID_MODEL`.
    #[serde(default)]
    pub model: Option<String>,
    /// Udev `ID_SERIAL_SHORT` — required identity component.
    pub serial_short: String,
    /// GPT PARTUUID (present for GPT drives; absent on MBR).
    #[serde(default)]
    pub partuuid: Option<String>,
    /// MBR fallback identity: partition index within the parent
    /// disk. Written by Step 6's rename verb when `partuuid` is
    /// absent so an MBR drive still gets a stable identity key.
    #[serde(default)]
    pub partition_index: Option<u32>,
    /// The operator-set friendly name. Already sanitised per
    /// [`crate::stable_id::sanitise`] at write time.
    pub alias: String,
    /// Wall-clock ms at set time. Purely informational.
    #[serde(default)]
    pub set_at_ms: Option<i64>,
}

/// In-memory alias store. Cheap to clone (all fields are
/// `Vec<AliasEntry>`); the runtime holds one per plugin instance
/// and re-loads on `storage.usb.rename` (Step 6).
#[derive(Debug, Clone)]
pub struct AliasStore {
    path: PathBuf,
    entries: Vec<AliasEntry>,
}

impl AliasStore {
    /// Load the alias store from `<state_dir>/state/aliases.toml`.
    /// Returns an empty store when the file does not exist yet
    /// (fresh install with no operator renames). Parse errors on
    /// a non-empty file surface as an error so the runtime can
    /// log + fall back to an empty store rather than panicking.
    pub fn load(state_dir: &Path) -> Result<Self, AliasStoreError> {
        let path = state_dir.join("state").join(ALIAS_FILE_NAME);
        if !path.exists() {
            return Ok(Self {
                path,
                entries: Vec::new(),
            });
        }
        let text = std::fs::read_to_string(&path).map_err(|e| {
            AliasStoreError::Io {
                path: path.clone(),
                source: e,
            }
        })?;
        if text.trim().is_empty() {
            return Ok(Self {
                path,
                entries: Vec::new(),
            });
        }
        let file: AliasFile =
            toml::from_str(&text).map_err(|e| AliasStoreError::Parse {
                path: path.clone(),
                message: e.to_string(),
            })?;
        if file.schema_version != ALIAS_SCHEMA_VERSION {
            return Err(AliasStoreError::SchemaVersion {
                path,
                found: file.schema_version,
                expected: ALIAS_SCHEMA_VERSION,
            });
        }
        Ok(Self {
            path,
            entries: file.alias,
        })
    }

    /// Empty in-memory store — for tests + the fresh-load
    /// fallback path when the on-disk file is missing.
    pub fn empty(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join("state").join(ALIAS_FILE_NAME),
            entries: Vec::new(),
        }
    }

    /// On-disk path this store maps to. Step 6's writer uses
    /// this for the atomic tmp+rename cycle.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Return all persisted entries. Cheap clone; caller
    /// typically iterates or filters.
    pub fn entries(&self) -> &[AliasEntry] {
        &self.entries
    }

    /// Look up an alias by identity tuple. Returns `Some(alias)`
    /// on exact match or partition-index-fallback match; `None`
    /// otherwise. See module docs for match rules.
    pub fn lookup(&self, id: &AliasLookup<'_>) -> Option<&str> {
        // Exact match first — all four fields present + equal.
        for e in &self.entries {
            if e.serial_short == id.serial_short
                && opt_eq(e.vendor.as_deref(), id.vendor)
                && opt_eq(e.model.as_deref(), id.model)
                && opt_eq(e.partuuid.as_deref(), id.partuuid)
            {
                return Some(e.alias.as_str());
            }
        }
        // Partition-index fallback when partuuid is absent on
        // the caller (MBR-partitioned stick) — match on
        // (vendor, model, serial_short, partition_index) if the
        // entry has partition_index stored.
        if id.partuuid.is_none() {
            for e in &self.entries {
                if e.partuuid.is_none()
                    && e.serial_short == id.serial_short
                    && opt_eq(e.vendor.as_deref(), id.vendor)
                    && opt_eq(e.model.as_deref(), id.model)
                    && e.partition_index == Some(id.partition_index)
                {
                    return Some(e.alias.as_str());
                }
            }
        }
        None
    }
}

fn opt_eq(a: Option<&str>, b: Option<&str>) -> bool {
    a == b
}

impl AliasStore {
    /// Set-or-update the alias entry for the identity tuple.
    /// Replaces an existing entry that matches on the same
    /// tuple; appends otherwise. Does NOT persist — caller
    /// invokes [`Self::save`] separately (the runtime does
    /// this after successful validation + collision check).
    #[allow(clippy::too_many_arguments)]
    pub fn set_alias(
        &mut self,
        vendor: Option<&str>,
        model: Option<&str>,
        serial_short: &str,
        partuuid: Option<&str>,
        partition_index: u32,
        alias: &str,
        set_at_ms: i64,
    ) {
        // Try to update in place first.
        for entry in self.entries.iter_mut() {
            if entry.serial_short == serial_short
                && opt_eq(entry.vendor.as_deref(), vendor)
                && opt_eq(entry.model.as_deref(), model)
                && opt_eq(entry.partuuid.as_deref(), partuuid)
            {
                entry.alias = alias.to_string();
                entry.partition_index =
                    partuuid.is_none().then_some(partition_index);
                entry.set_at_ms = Some(set_at_ms);
                return;
            }
        }
        // Append a new entry.
        self.entries.push(AliasEntry {
            vendor: vendor.map(str::to_string),
            model: model.map(str::to_string),
            serial_short: serial_short.to_string(),
            partuuid: partuuid.map(str::to_string),
            partition_index: partuuid.is_none().then_some(partition_index),
            alias: alias.to_string(),
            set_at_ms: Some(set_at_ms),
        });
    }

    /// Remove the alias entry matching the identity tuple.
    /// No-op when no entry matches. Does NOT persist — caller
    /// invokes [`Self::save`] separately.
    pub fn clear_alias(
        &mut self,
        vendor: Option<&str>,
        model: Option<&str>,
        serial_short: &str,
        partuuid: Option<&str>,
    ) {
        self.entries.retain(|entry| {
            !(entry.serial_short == serial_short
                && opt_eq(entry.vendor.as_deref(), vendor)
                && opt_eq(entry.model.as_deref(), model)
                && opt_eq(entry.partuuid.as_deref(), partuuid))
        });
    }

    /// Atomic write of the store to
    /// `<state_dir>/state/aliases.toml`. Uses tmp+rename so a
    /// mid-write crash never leaves a truncated file that would
    /// fail to parse at next boot. Sets mode 0600 owned by the
    /// current process user (steward at runtime, andrew in
    /// unit tests).
    ///
    /// Errors surface as [`AliasStoreError::Io`] with the
    /// path + underlying io error so the runtime can log and
    /// surface an operator-visible error class.
    pub fn save(&self) -> Result<(), AliasStoreError> {
        use std::io::Write;
        let file = AliasFile {
            schema_version: ALIAS_SCHEMA_VERSION,
            alias: self.entries.clone(),
        };
        let text = toml::to_string_pretty(&file).map_err(|e| {
            AliasStoreError::Parse {
                path: self.path.clone(),
                message: e.to_string(),
            }
        })?;
        let parent = self.path.parent().ok_or_else(|| AliasStoreError::Io {
            path: self.path.clone(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "alias store path has no parent",
            ),
        })?;
        std::fs::create_dir_all(parent).map_err(|e| AliasStoreError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
        // Tmp path sibling to the target so rename(2) is atomic
        // (same filesystem). Suffix with pid to avoid collisions
        // if multiple runtimes ever share a state_dir (they
        // don't — the plugin is singleton — but discipline).
        let tmp = self
            .path
            .with_extension(format!("toml.tmp.{}", std::process::id()));
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(|e| AliasStoreError::Io {
                    path: tmp.clone(),
                    source: e,
                })?;
            f.write_all(text.as_bytes())
                .map_err(|e| AliasStoreError::Io {
                    path: tmp.clone(),
                    source: e,
                })?;
            f.sync_all().map_err(|e| AliasStoreError::Io {
                path: tmp.clone(),
                source: e,
            })?;
        }
        std::fs::rename(&tmp, &self.path).map_err(|e| AliasStoreError::Io {
            path: self.path.clone(),
            source: e,
        })?;
        Ok(())
    }
}

// The `mode(0o600)` call above needs OpenOptionsExt in scope.
use std::os::unix::fs::OpenOptionsExt;

/// Query record for [`AliasStore::lookup`].
#[derive(Debug, Clone)]
pub struct AliasLookup<'a> {
    /// Udev `ID_VENDOR`.
    pub vendor: Option<&'a str>,
    /// Udev `ID_MODEL`.
    pub model: Option<&'a str>,
    /// Udev `ID_SERIAL_SHORT` — required.
    pub serial_short: &'a str,
    /// GPT PARTUUID (drives the exact vs fallback branch).
    pub partuuid: Option<&'a str>,
    /// Partition index within parent disk (used only when
    /// `partuuid` is absent).
    pub partition_index: u32,
}

/// Errors from [`AliasStore::load`].
#[derive(Debug, thiserror::Error)]
pub enum AliasStoreError {
    /// I/O error reading the file.
    #[error("read {path}: {source}")]
    Io {
        /// File path attempted.
        path: PathBuf,
        /// Underlying io error.
        #[source]
        source: std::io::Error,
    },
    /// TOML parse error.
    #[error("parse {path}: {message}")]
    Parse {
        /// File path attempted.
        path: PathBuf,
        /// Serde error message.
        message: String,
    },
    /// On-disk schema version does not match this build.
    #[error("schema mismatch at {path}: found {found}, expected {expected}")]
    SchemaVersion {
        /// File path.
        path: PathBuf,
        /// Version read from disk.
        found: u32,
        /// Version this build supports.
        expected: u32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_state_dir_with_aliases(content: &str) -> TempDir {
        let td = TempDir::new().unwrap();
        let state = td.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join(ALIAS_FILE_NAME), content).unwrap();
        td
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let td = TempDir::new().unwrap();
        let store = AliasStore::load(td.path()).unwrap();
        assert!(store.entries().is_empty());
    }

    #[test]
    fn load_empty_file_returns_empty() {
        let td = make_state_dir_with_aliases("");
        let store = AliasStore::load(td.path()).unwrap();
        assert!(store.entries().is_empty());
    }

    #[test]
    fn load_parses_one_entry() {
        let td = make_state_dir_with_aliases(
            r#"
schema_version = 1

[[alias]]
vendor        = "SanDisk"
model         = "Cruzer-Blade"
serial_short  = "4C530"
partuuid      = "a1b2c3d4-01"
alias         = "My-Vinyl-Rip"
set_at_ms     = 1786100000000
"#,
        );
        let store = AliasStore::load(td.path()).unwrap();
        assert_eq!(store.entries().len(), 1);
        assert_eq!(store.entries()[0].alias, "My-Vinyl-Rip");
    }

    #[test]
    fn lookup_exact_tuple_match() {
        let td = make_state_dir_with_aliases(
            r#"
schema_version = 1

[[alias]]
vendor        = "SanDisk"
model         = "Cruzer-Blade"
serial_short  = "4C530"
partuuid      = "a1b2c3d4-01"
alias         = "My-Vinyl-Rip"
"#,
        );
        let store = AliasStore::load(td.path()).unwrap();
        let hit = store.lookup(&AliasLookup {
            vendor: Some("SanDisk"),
            model: Some("Cruzer-Blade"),
            serial_short: "4C530",
            partuuid: Some("a1b2c3d4-01"),
            partition_index: 1,
        });
        assert_eq!(hit, Some("My-Vinyl-Rip"));
    }

    #[test]
    fn lookup_no_match_on_different_serial() {
        let td = make_state_dir_with_aliases(
            r#"
schema_version = 1

[[alias]]
vendor        = "SanDisk"
model         = "Cruzer-Blade"
serial_short  = "4C530"
partuuid      = "a1b2c3d4-01"
alias         = "My-Vinyl-Rip"
"#,
        );
        let store = AliasStore::load(td.path()).unwrap();
        let miss = store.lookup(&AliasLookup {
            vendor: Some("SanDisk"),
            model: Some("Cruzer-Blade"),
            serial_short: "7B221",
            partuuid: Some("a1b2c3d4-01"),
            partition_index: 1,
        });
        assert!(miss.is_none());
    }

    #[test]
    fn lookup_partition_index_fallback_when_no_partuuid() {
        let td = make_state_dir_with_aliases(
            r#"
schema_version = 1

[[alias]]
vendor           = "SanDisk"
model            = "Cruzer"
serial_short     = "4C530"
partition_index  = 2
alias            = "MBR-Stick-p2"
"#,
        );
        let store = AliasStore::load(td.path()).unwrap();
        let hit = store.lookup(&AliasLookup {
            vendor: Some("SanDisk"),
            model: Some("Cruzer"),
            serial_short: "4C530",
            partuuid: None,
            partition_index: 2,
        });
        assert_eq!(hit, Some("MBR-Stick-p2"));
    }

    #[test]
    fn lookup_schema_mismatch_returns_error() {
        let td = make_state_dir_with_aliases(
            r#"
schema_version = 99

[[alias]]
serial_short = "4C530"
alias        = "Test"
"#,
        );
        let err = AliasStore::load(td.path()).unwrap_err();
        matches!(err, AliasStoreError::SchemaVersion { .. });
    }
}
