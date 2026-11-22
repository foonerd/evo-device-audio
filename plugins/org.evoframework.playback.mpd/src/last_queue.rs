// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! The last queue: where it is kept.
//!
//! A clear throws the operator's queue away, and a reboot can.
//! Before either, the queue is written here — the rows in order,
//! and where the player was in them — so `queue.restore_last_queue`
//! can put it back.
//!
//! One file under the plugin's state directory, written whole and
//! renamed into place, so a crash mid-write leaves the previous
//! snapshot rather than half of a new one. An empty queue is never
//! written over a kept one: clearing an already-empty queue, or
//! shutting down with one, must not cost the operator the queue
//! they could still restore.
//!
//! This is the plugin's own store. MPD keeps its own state file
//! and restores its queue on its own restart; that is MPD's
//! business and covers neither a clear nor a queue lost between
//! MPD and the device. This covers both, and is the one the verb
//! reads.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::PLUGIN_NAME;

/// File-format version of the snapshot.
pub(crate) const LAST_QUEUE_VERSION: u32 = 1;

/// Why the snapshot was taken. Recorded so an operator reading
/// the file, or a later report, can tell a clear from a shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SnapshotReason {
    /// The operator cleared the queue.
    Clear,
    /// The plugin was unloading.
    Shutdown,
}

/// The queue as it stood, and where the player was in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LastQueue {
    /// File-format version.
    pub(crate) v: u32,
    /// Seconds since the Unix epoch when the snapshot was taken.
    pub(crate) taken_at: u64,
    /// Why it was taken.
    pub(crate) reason: SnapshotReason,
    /// MPD URIs in queue order. What `add` accepts, verbatim.
    pub(crate) uris: Vec<String>,
    /// The playing position, when something was playing or
    /// paused. `None` when the player was stopped.
    pub(crate) current: Option<u32>,
}

/// Where the snapshot lives. `None` is a store that keeps
/// nothing — the shape the queue tests use when the row under
/// test is not this one.
#[derive(Debug, Clone, Default)]
pub(crate) struct LastQueueStore {
    path: Option<PathBuf>,
}

/// What went wrong reading or writing the store.
#[derive(Debug, thiserror::Error)]
pub(crate) enum StoreError {
    #[error("last queue: {reason}")]
    Io { reason: String },
}

impl LastQueueStore {
    /// A store at this path.
    pub(crate) fn at(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    /// Whether this store keeps anything at all.
    pub(crate) fn is_enabled(&self) -> bool {
        self.path.is_some()
    }

    /// Write the snapshot whole, then rename it into place.
    pub(crate) async fn save(
        &self,
        last: &LastQueue,
    ) -> Result<(), StoreError> {
        let Some(path) = self.path.as_ref() else {
            return Ok(());
        };
        let io = |what: String| StoreError::Io { reason: what };
        let body = toml::to_string_pretty(last)
            .map_err(|e| io(format!("serialise: {e}")))?;
        let parent = path
            .parent()
            .ok_or_else(|| io(format!("{path:?} has no parent")))?;
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| io(format!("mkdir {parent:?}: {e}")))?;
        let staging = parent.join(format!(
            ".{}.tmp",
            path.file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| "last-queue.toml".to_string())
        ));
        tokio::fs::write(&staging, body)
            .await
            .map_err(|e| io(format!("write {staging:?}: {e}")))?;
        tokio::fs::rename(&staging, path)
            .await
            .map_err(|e| io(format!("rename {staging:?} -> {path:?}: {e}")))?;
        Ok(())
    }

    /// The kept snapshot, or `None` when there is none. A file
    /// that cannot be read or parsed is reported and treated as
    /// none: the verb then says there is nothing to restore,
    /// rather than restoring half a queue.
    pub(crate) async fn load(&self) -> Option<LastQueue> {
        let path = self.path.as_ref()?;
        let body = match tokio::fs::read_to_string(path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    path = %path.display(),
                    error = %e,
                    "last queue unreadable; nothing to restore"
                );
                return None;
            }
        };
        match toml::from_str::<LastQueue>(&body) {
            Ok(last) if last.v == LAST_QUEUE_VERSION => Some(last),
            Ok(last) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    path = %path.display(),
                    version = last.v,
                    "last queue is from another format version; nothing \
                     to restore"
                );
                None
            }
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    path = %path.display(),
                    error = %e,
                    "last queue does not parse; nothing to restore"
                );
                None
            }
        }
    }
}

/// Now, in seconds since the Unix epoch. Zero if the clock is
/// before the epoch, which is not a reason to refuse a snapshot.
pub(crate) fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(reason: SnapshotReason) -> LastQueue {
        LastQueue {
            v: LAST_QUEUE_VERSION,
            taken_at: 1_700_000_000,
            reason,
            uris: vec![
                "INTERNAL/a/1.flac".to_string(),
                "http://example.invalid/stream".to_string(),
                "USB/Stick/b/2.flac".to_string(),
            ],
            current: Some(1),
        }
    }

    /// What is saved is what is loaded: every row, in order,
    /// with the position and the reason.
    #[tokio::test]
    async fn the_snapshot_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = LastQueueStore::at(dir.path().join("last-queue.toml"));
        assert!(store.load().await.is_none(), "nothing kept yet");
        let last = sample(SnapshotReason::Clear);
        store.save(&last).await.unwrap();
        assert_eq!(store.load().await, Some(last));
    }

    /// The file is renamed into place: no staging file is left
    /// behind, and the directory is created on the way.
    #[tokio::test]
    async fn the_write_is_whole_and_leaves_no_staging_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deeper").join("last-queue.toml");
        let store = LastQueueStore::at(path.clone());
        store.save(&sample(SnapshotReason::Shutdown)).await.unwrap();
        assert!(path.is_file());
        assert!(
            !path.with_file_name(".last-queue.toml.tmp").exists(),
            "the staging file must be renamed away"
        );
    }

    /// A file that does not parse, or is from another version,
    /// is nothing to restore — not a panic and not half a queue.
    #[tokio::test]
    async fn a_bad_file_is_nothing_to_restore() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("last-queue.toml");
        let store = LastQueueStore::at(path.clone());
        std::fs::write(&path, "this is not toml = [").unwrap();
        assert!(store.load().await.is_none());
        std::fs::write(
            &path,
            "v = 99\ntaken_at = 1\nreason = \"clear\"\nuris = []\n",
        )
        .unwrap();
        assert!(store.load().await.is_none(), "another version");
    }

    /// A disabled store keeps nothing and refuses nothing.
    #[tokio::test]
    async fn a_disabled_store_keeps_nothing() {
        let store = LastQueueStore::default();
        assert!(!store.is_enabled());
        store.save(&sample(SnapshotReason::Clear)).await.unwrap();
        assert!(store.load().await.is_none());
    }
}
