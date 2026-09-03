// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Persistent non-Deezer provider-result sidecar for the
//! artist-artwork cascade.
//!
//! Companion to the in-memory
//! [`crate::artwork_caches::ArtworkCaches::provider`] LRU: this
//! module stores the `Vec<SourceEntry>` (as opaque
//! `serde_json::Value` per source, same shape the in-memory
//! cache holds) on disk so a restart or a provider outage
//! after cold boot does not blank an artist tile whose
//! provider results were already fetched at some point in
//! the past.
//!
//! Layout (mirrors [`crate::reconcile_index::ReconcileIndex`]):
//!
//! - `<state_dir>/provider-index/<sha256(fold_key)[0:2]>/<sha256(fold_key)>`
//! - File body: a JSON envelope `{"v", "sources", "covered"}`.
//!
//! `covered` names the providers the snapshot durably speaks
//! for. A provider listed there but absent from `sources`
//! answered with a definitive absence; one missing from both was
//! never asked. Files written before the envelope existed are a
//! bare `Vec<serde_json::Value>` and are still read, with
//! coverage derived from the entries present.
//!
//! ## Freshness contract
//!
//! **No expiry.** Hero images effectively never change; the
//! symmetric decision with `reconcile-index` keeps operator
//! mental model simple (one gesture invalidates everything for
//! an artist). Invalidation is operator-triggered only:
//! `artwork.online.clear_cache` (per-artist or global) and the
//! endpoint's `?refresh=1`.
//!
//! Staleness-on-error is the safety net: if a persisted
//! provider URL 404s at fetch time, the AssetCache propagates
//! the 404 and the operator UI can refresh. With the P0
//! framework-side resolve index in place, hero bytes are
//! content-addressed and independent of provider URL freshness
//! for the common case.
//!
//! ## Positive-only
//!
//! Only Ok cascades write here — the same `sources` list the
//! in-memory cache holds. An empty `sources` list is a valid
//! entry (meaning "we tried every provider and none returned
//! content") and DOES persist; it prevents a browse burst from
//! re-hammering exhausted providers after restart.
//!
//! Deezer entries are excluded upstream (in the caller) because
//! the plugin's live-fetch invariant forbids caching Deezer
//! image URLs. This module is not the enforcement point — it
//! stores whatever the caller passes.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Persistent provider-result sidecar.
#[derive(Debug, Clone)]
pub(crate) struct ProviderIndex {
    root: PathBuf,
}

/// On-disk snapshot format version. Bumped only when the shape
/// changes incompatibly; readers fall back to the legacy
/// bare-array form when the envelope does not parse.
const SNAPSHOT_VERSION: u32 = 1;

/// Envelope written to each sidecar file.
///
/// The predecessor format was a bare JSON array of source
/// entries, which could not express which providers a snapshot
/// spoke for. Both are read; only this one is written.
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredSnapshot {
    #[serde(rename = "v")]
    version: u32,
    sources: Vec<serde_json::Value>,
    covered: Vec<String>,
}

impl ProviderIndex {
    /// Construct a sidecar rooted under
    /// `<state_dir>/provider-index/`. The directory is created
    /// lazily on first write.
    pub(crate) fn new(state_root: PathBuf) -> Self {
        Self {
            root: state_root.join("provider-index"),
        }
    }

    /// Read the persisted snapshot for the fold-key: the stored
    /// `sources` plus the provider ids the snapshot durably
    /// covers. Returns `None` when no entry is stored, the stored
    /// file is unreadable, or its content fails to deserialise.
    /// Never surfaces I/O errors.
    ///
    /// Reads both the current envelope and the legacy bare-array
    /// form written before coverage was recorded. A legacy file
    /// only knows which providers returned content, so coverage
    /// is derived from the entries present: providers that were
    /// durably absent read back as uncovered and get re-tried
    /// once, which costs one wave per artist and then settles.
    /// That is strictly better than the alternative reading,
    /// which would invent a durable absence nobody observed.
    pub(crate) async fn get(
        &self,
        fold_key: &str,
    ) -> Option<(Vec<serde_json::Value>, Vec<String>)> {
        let path = self.path_for(&Self::key_hash(fold_key));
        let raw = tokio::fs::read(&path).await.ok()?;
        if let Ok(env) = serde_json::from_slice::<StoredSnapshot>(&raw) {
            return Some((env.sources, env.covered));
        }
        let sources =
            serde_json::from_slice::<Vec<serde_json::Value>>(&raw).ok()?;
        let covered = sources
            .iter()
            .filter_map(|s| s.get("provider_id")?.as_str().map(str::to_string))
            .collect();
        Some((sources, covered))
    }

    /// Persist `sources` and its coverage for the fold-key.
    /// Overwrites any existing entry atomically. An empty
    /// `sources` list is a legitimate "every provider tried and
    /// none had content" signal and IS persisted — provided
    /// `covered` names the providers that reported it.
    pub(crate) async fn put(
        &self,
        fold_key: &str,
        sources: &[serde_json::Value],
        covered: &[String],
    ) -> Result<(), std::io::Error> {
        if fold_key.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "provider-index refuses empty fold_key",
            ));
        }
        let bytes = serde_json::to_vec(&StoredSnapshot {
            version: SNAPSHOT_VERSION,
            sources: sources.to_vec(),
            covered: covered.to_vec(),
        })
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let key = Self::key_hash(fold_key);
        let path = self.path_for(&key);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut tmp = path.clone();
        tmp.as_mut_os_string().push(".tmp");
        tokio::fs::write(&tmp, &bytes).await?;
        tokio::fs::rename(&tmp, &path).await
    }

    /// Evict the fold-key's entry. Returns `Ok(true)` when a
    /// file was removed, `Ok(false)` when there was nothing
    /// there, `Err(_)` only on I/O faults other than absence.
    pub(crate) async fn forget(
        &self,
        fold_key: &str,
    ) -> Result<bool, std::io::Error> {
        let path = self.path_for(&Self::key_hash(fold_key));
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Evict by pre-hashed key (used with
    /// [`crate::reconcile_index::ReconcileIndex::find_key_hash_by_mbid`]
    /// so the MBID-clear path can also drop this sidecar's
    /// entry without re-hashing the plaintext fold_key).
    pub(crate) async fn forget_by_key_hash(
        &self,
        key_hash: &str,
    ) -> Result<bool, std::io::Error> {
        let path = self.path_for(key_hash);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Purge every persisted entry. Returns the count of files
    /// removed. Called by the global `clear_cache` (no target)
    /// gesture alongside the LRU flush.
    pub(crate) async fn drop_all(&self) -> Result<usize, std::io::Error> {
        let Ok(mut outer) = tokio::fs::read_dir(&self.root).await else {
            return Ok(0);
        };
        let mut removed = 0usize;
        while let Ok(Some(shard)) = outer.next_entry().await {
            let shard_path = shard.path();
            if !shard_path.is_dir() {
                continue;
            }
            let Ok(mut inner) = tokio::fs::read_dir(&shard_path).await else {
                continue;
            };
            while let Ok(Some(entry)) = inner.next_entry().await {
                let entry_path = entry.path();
                if tokio::fs::remove_file(&entry_path).await.is_ok() {
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    fn key_hash(fold_key: &str) -> String {
        let digest = Sha256::digest(fold_key.as_bytes());
        let mut hex = String::with_capacity(64);
        for byte in digest {
            hex.push_str(&format!("{byte:02x}"));
        }
        hex
    }

    fn path_for(&self, key_hash: &str) -> PathBuf {
        let shard: &str = &key_hash[..2];
        self.root.join(Path::new(shard)).join(key_hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn source(provider: &str) -> serde_json::Value {
        serde_json::json!({"provider_id": provider, "payload": {"image_url": "u"}})
    }

    #[tokio::test]
    async fn put_then_get_round_trips() {
        let dir = TempDir::new().unwrap();
        let idx = ProviderIndex::new(dir.path().to_path_buf());
        let sources = vec![source("theaudiodb"), source("fanart")];
        let covered = vec!["theaudiodb".to_string(), "fanart".to_string()];
        idx.put("abba", &sources, &covered).await.unwrap();
        assert_eq!(idx.get("abba").await.unwrap(), (sources, covered));
    }

    /// A snapshot may cover a provider that contributed no
    /// content — that is a recorded absence, and it must survive
    /// the disk round trip as coverage rather than being inferred
    /// away. Losing it would send the cascade back to the network
    /// for an answer it already has.
    #[tokio::test]
    async fn coverage_without_content_round_trips() {
        let dir = TempDir::new().unwrap();
        let idx = ProviderIndex::new(dir.path().to_path_buf());
        let sources = vec![source("theaudiodb")];
        let covered = vec!["theaudiodb".to_string(), "discogs".to_string()];
        idx.put("abba", &sources, &covered).await.unwrap();
        let (got_sources, got_covered) = idx.get("abba").await.unwrap();
        assert_eq!(got_sources, sources);
        assert_eq!(got_covered, covered);
    }

    /// Sidecar files written before coverage was recorded are a
    /// bare JSON array. They must still load, with coverage
    /// inferred from the entries actually present — never widened
    /// to providers the old file never spoke for.
    #[tokio::test]
    async fn legacy_bare_array_file_is_read_with_derived_coverage() {
        let dir = TempDir::new().unwrap();
        let idx = ProviderIndex::new(dir.path().to_path_buf());
        let key = ProviderIndex::key_hash("abba");
        let shard = dir.path().join("provider-index").join(&key[..2]);
        tokio::fs::create_dir_all(&shard).await.unwrap();
        let legacy = serde_json::to_vec(&vec![source("theaudiodb")]).unwrap();
        tokio::fs::write(shard.join(&key), legacy).await.unwrap();
        let (sources, covered) = idx.get("abba").await.unwrap();
        assert_eq!(sources, vec![source("theaudiodb")]);
        assert_eq!(covered, vec!["theaudiodb".to_string()]);
    }

    #[tokio::test]
    async fn put_empty_sources_persists_and_reads_back_empty() {
        let dir = TempDir::new().unwrap();
        let idx = ProviderIndex::new(dir.path().to_path_buf());
        let covered = vec!["theaudiodb".to_string()];
        idx.put("abba", &[], &covered).await.unwrap();
        assert_eq!(
            idx.get("abba").await.unwrap(),
            (Vec::<serde_json::Value>::new(), covered)
        );
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let dir = TempDir::new().unwrap();
        let idx = ProviderIndex::new(dir.path().to_path_buf());
        assert!(idx.get("nope").await.is_none());
    }

    #[tokio::test]
    async fn put_overwrites_existing() {
        let dir = TempDir::new().unwrap();
        let idx = ProviderIndex::new(dir.path().to_path_buf());
        idx.put("abba", &[source("a")], &["a".to_string()])
            .await
            .unwrap();
        idx.put("abba", &[source("b")], &["b".to_string()])
            .await
            .unwrap();
        assert_eq!(
            idx.get("abba").await.unwrap(),
            (vec![source("b")], vec!["b".to_string()])
        );
    }

    #[tokio::test]
    async fn forget_removes_entry() {
        let dir = TempDir::new().unwrap();
        let idx = ProviderIndex::new(dir.path().to_path_buf());
        idx.put("abba", &[source("a")], &["a".to_string()])
            .await
            .unwrap();
        assert!(idx.forget("abba").await.unwrap());
        assert!(idx.get("abba").await.is_none());
    }

    #[tokio::test]
    async fn forget_of_absent_returns_false() {
        let dir = TempDir::new().unwrap();
        let idx = ProviderIndex::new(dir.path().to_path_buf());
        assert!(!idx.forget("never").await.unwrap());
    }

    #[tokio::test]
    async fn put_refuses_empty_fold_key() {
        let dir = TempDir::new().unwrap();
        let idx = ProviderIndex::new(dir.path().to_path_buf());
        assert!(idx
            .put("", &[source("a")], &["a".to_string()])
            .await
            .is_err());
    }

    #[tokio::test]
    async fn get_ignores_malformed_file() {
        let dir = TempDir::new().unwrap();
        let idx = ProviderIndex::new(dir.path().to_path_buf());
        let key = ProviderIndex::key_hash("garbled");
        let shard = dir.path().join("provider-index").join(&key[..2]);
        tokio::fs::create_dir_all(&shard).await.unwrap();
        tokio::fs::write(shard.join(&key), b"{not json")
            .await
            .unwrap();
        assert!(idx.get("garbled").await.is_none());
    }

    #[tokio::test]
    async fn drop_all_purges_every_entry() {
        let dir = TempDir::new().unwrap();
        let idx = ProviderIndex::new(dir.path().to_path_buf());
        for i in 0..5 {
            idx.put(
                &format!("a-{i}"),
                &[source(&format!("p-{i}"))],
                &[format!("p-{i}")],
            )
            .await
            .unwrap();
        }
        assert_eq!(idx.drop_all().await.unwrap(), 5);
        for i in 0..5 {
            assert!(idx.get(&format!("a-{i}")).await.is_none());
        }
    }
}
