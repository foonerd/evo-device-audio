// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! The worker's state under `/var/lib/evo-updater`: the channel, the last
//! status and inventory, the history, the record of what it applied, and
//! the lock. Everything is written by rename, so a reader never sees half
//! a file. The plugin reads these files and writes none of them.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{io_err, Res, UpdError};
use crate::grammar::Channel;
use crate::timeutil;

/// Write `bytes` to `path` by rename with `mode`.
pub fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Res<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let mut f = fs::File::create(&tmp)
        .map_err(|e| io_err("write_failed", "create", e))?;
    f.write_all(bytes)
        .map_err(|e| io_err("write_failed", "write", e))?;
    f.sync_all()
        .map_err(|e| io_err("write_failed", "sync", e))?;
    drop(f);
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))
        .map_err(|e| io_err("write_failed", "chmod", e))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        io_err("write_failed", "rename", e)
    })
}

/// What the worker recorded when it applied a plugin.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct AppliedEntry {
    /// Version applied.
    pub version: String,
    /// SHA-256 of the bundle tarball applied.
    pub sha256: String,
    /// When.
    pub applied_at_utc: String,
}

/// What the worker recorded when it applied the host files.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct AppliedHost {
    /// Release applied.
    pub release: String,
    /// SHA-256 of the carrier tarball applied.
    pub sha256: String,
    /// When.
    pub applied_at_utc: String,
}

/// `applied.toml`.
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct Applied {
    /// Plugins applied by this worker.
    #[serde(default)]
    pub plugins: BTreeMap<String, AppliedEntry>,
    /// The host files last applied.
    #[serde(default)]
    pub host: Option<AppliedHost>,
}

/// The state directory.
#[derive(Debug, Clone)]
pub struct State {
    /// `/var/lib/evo-updater`.
    pub dir: PathBuf,
}

/// Drop guard for the single-run lock.
pub struct LockGuard(PathBuf);

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

impl State {
    /// A state handle.
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    /// Scratch directory for staging and fetches.
    pub fn stage_dir(&self) -> PathBuf {
        self.dir.join("stage")
    }

    /// Create the scratch directory if needed.
    pub fn ensure_stage(&self) -> Res<()> {
        fs::create_dir_all(self.stage_dir())
            .map_err(|e| io_err("state_unwritable", "stage dir", e))
    }

    /// The recorded channel; `production` until one is set.
    pub fn channel(&self) -> Channel {
        fs::read_to_string(self.dir.join("channel"))
            .ok()
            .and_then(|s| Channel::parse(s.trim()))
            .unwrap_or(Channel::Production)
    }

    /// Record a channel.
    pub fn set_channel(&self, c: Channel) -> Res<()> {
        atomic_write(
            &self.dir.join("channel"),
            format!("{}\n", c.name()).as_bytes(),
            0o644,
        )
    }

    /// Write `status.json`.
    pub fn write_status(&self, v: &Value) -> Res<()> {
        atomic_write(
            &self.dir.join("status.json"),
            v.to_string().as_bytes(),
            0o644,
        )
    }

    /// Write `inventory.json`.
    pub fn write_inventory(&self, v: &Value) -> Res<()> {
        atomic_write(
            &self.dir.join("inventory.json"),
            v.to_string().as_bytes(),
            0o644,
        )
    }

    /// Append one JSON line to `history.jsonl`, keeping the last 200.
    pub fn history(&self, mut entry: Value) {
        entry["at_utc"] = json!(timeutil::format_utc(timeutil::now_epoch()));
        let path = self.dir.join("history.jsonl");
        let mut lines: Vec<String> = fs::read_to_string(&path)
            .map(|s| s.lines().map(str::to_string).collect())
            .unwrap_or_default();
        lines.push(entry.to_string());
        if lines.len() > 200 {
            lines.drain(..lines.len() - 200);
        }
        let _ = atomic_write(
            &path,
            format!("{}\n", lines.join("\n")).as_bytes(),
            0o644,
        );
    }

    /// Read `applied.toml`; empty when absent or unreadable.
    pub fn applied(&self) -> Applied {
        fs::read_to_string(self.dir.join("applied.toml"))
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Write `applied.toml`.
    pub fn write_applied(&self, a: &Applied) -> Res<()> {
        let text = toml::to_string(a)
            .map_err(|e| UpdError::new("write_failed", e.to_string()))?;
        atomic_write(&self.dir.join("applied.toml"), text.as_bytes(), 0o644)
    }

    /// Is a worker running? True if the lock names a live process.
    pub fn busy(&self) -> bool {
        fs::read_to_string(self.dir.join("lock"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .is_some_and(|pid| Path::new(&format!("/proc/{pid}")).exists())
    }

    /// Take the single-run lock, or `busy`. A lock naming a dead process
    /// is replaced.
    pub fn lock(&self) -> Res<LockGuard> {
        let path = self.dir.join("lock");
        if self.busy()
            && fs::read_to_string(&path)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                != Some(std::process::id())
        {
            return Err(UpdError::new("busy", "another update is running"));
        }
        fs::write(&path, format!("{}\n", std::process::id()))
            .map_err(|e| io_err("state_unwritable", "lock", e))?;
        Ok(LockGuard(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_channel_defaults_to_production_and_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let s = State::new(d.path());
        assert_eq!(s.channel(), Channel::Production);
        s.set_channel(Channel::Alpha).unwrap();
        assert_eq!(s.channel(), Channel::Alpha);
        fs::write(d.path().join("channel"), "garbage\n").unwrap();
        assert_eq!(
            s.channel(),
            Channel::Production,
            "an unknown value never selects a channel"
        );
    }

    #[test]
    fn applied_records_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let s = State::new(d.path());
        let mut a = Applied::default();
        a.plugins.insert(
            "org.evoframework.x".into(),
            AppliedEntry {
                version: "1.0".into(),
                sha256: "ab".into(),
                applied_at_utc: "t".into(),
            },
        );
        s.write_applied(&a).unwrap();
        assert_eq!(s.applied().plugins["org.evoframework.x"].sha256, "ab");
    }

    #[test]
    fn a_second_run_is_busy_and_a_dead_lock_is_replaced() {
        let d = tempfile::tempdir().unwrap();
        let s = State::new(d.path());
        fs::write(d.path().join("lock"), "4294967\n").unwrap();
        let g = s.lock().expect("a lock naming a dead pid is replaced");
        drop(g);
        assert!(
            !d.path().join("lock").exists(),
            "the guard removes the lock"
        );
        fs::write(d.path().join("lock"), "1\n").unwrap();
        assert_eq!(
            s.lock().err().map(|e| e.code),
            Some("busy"),
            "pid 1 is alive"
        );
    }

    #[test]
    fn history_keeps_the_last_200() {
        let d = tempfile::tempdir().unwrap();
        let s = State::new(d.path());
        for i in 0..205 {
            s.history(json!({"n": i}));
        }
        let text = fs::read_to_string(d.path().join("history.jsonl")).unwrap();
        assert_eq!(text.lines().count(), 200);
        assert!(text.lines().last().unwrap().contains("\"n\":204"));
    }
}
