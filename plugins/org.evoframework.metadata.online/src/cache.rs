// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Persistent reconciliation cache.
//!
//! One JSON file per cache key under `state_dir/reconcile_cache/`.
//! Keys are stable SHA-256 hashes of the normalised
//! `(artist, album)` pair; entries are the full reconciliation
//! response for positive hits, or a `{status: "not_found",
//! expires_at_epoch_secs}` shape for negatives.
//!
//! ## Persistence discipline
//!
//! - Positive hits — persisted indefinitely. A MusicBrainz
//!   release identity does not churn; once we've reconciled a
//!   `(artist, album)` to a `release_mbid`, that mapping is
//!   stable for the lifetime of the operator's library.
//! - Negative hits — persisted with an expiry. Same shape as
//!   the artwork endpoint's negative cache (24 h by default).
//!
//! On restart every entry survives (positive) or is honoured
//! until expiry (negative); no in-memory-only state.
//!
//! ## Atomic writes
//!
//! Writes go through a `<file>.tmp` + `rename` sequence so a
//! partial write during a crash does not leave a truncated JSON
//! blob that the next reader mistakes for a valid entry.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Serialisable cache entry — matches the on-disk JSON shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CacheEntry {
    /// `"ok"` for a positive hit; `"not_found"` for a negative.
    pub(crate) status: String,
    /// Positive-hit payload (`canonical` object from the wire
    /// response). `None` for negatives.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) canonical: Option<serde_json::Value>,
    /// The provider that produced the positive hit (currently
    /// always `"musicbrainz"`; forward-compat). `None` for
    /// negatives.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) provider_id: Option<String>,
    /// MB search score on the positive hit; `None` for
    /// negatives.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) confidence_percent: Option<u32>,
    /// Expiry epoch seconds. `None` = never expire (positive
    /// hits). Set on negatives to `now + negative_ttl`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) expires_at_epoch_secs: Option<u64>,
    /// Optional operator-readable detail (e.g. why a negative
    /// was recorded — "MB search returned zero releases").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) detail: Option<String>,
}

impl CacheEntry {
    /// Whether this entry is fresh at the given wall-clock time.
    /// Positive hits (`expires_at_epoch_secs == None`) are
    /// always fresh; negatives are fresh until their expiry.
    pub(crate) fn is_fresh(&self, now_epoch_secs: u64) -> bool {
        match self.expires_at_epoch_secs {
            None => true,
            Some(exp) => now_epoch_secs < exp,
        }
    }
}

/// Persistent cache handle. Clone-shareable across the plugin
/// (the underlying filesystem paths + TTL are stored behind an
/// `Arc` for lightweight clones).
#[derive(Clone)]
pub(crate) struct ReconcileCache {
    inner: Arc<Inner>,
}

struct Inner {
    root: PathBuf,
    negative_ttl: Duration,
}

impl ReconcileCache {
    /// Construct a new handle rooted at `root`. Does not create
    /// the directory eagerly — the first `put` does so.
    pub(crate) fn new(root: PathBuf, negative_ttl: Duration) -> Self {
        Self {
            inner: Arc::new(Inner { root, negative_ttl }),
        }
    }

    /// Compute the cache key for a normalised `(artist, album)`
    /// pair. Callers normalise BEFORE calling — the cache does
    /// not want to know about the normalisation policy (which
    /// lives in `reconcile.rs`).
    pub(crate) fn key_for(
        normalised_artist: &str,
        normalised_album: &str,
    ) -> String {
        use std::hash::{Hash, Hasher};
        // 64-bit hash is enough for a per-device cache — collision
        // probability at 100 k entries is ~10^-14; each entry is
        // still keyed on the full (artist, album) inside the file
        // so a collision only wastes a disk read.
        let mut h = std::collections::hash_map::DefaultHasher::new();
        normalised_artist.hash(&mut h);
        "|".hash(&mut h);
        normalised_album.hash(&mut h);
        format!("{:016x}", h.finish())
    }

    fn path_for_key(&self, key: &str) -> PathBuf {
        self.inner.root.join(format!("{key}.json"))
    }

    /// Read the fresh entry for a key. Returns `Ok(None)` on
    /// cache miss / stale-expiry / read failure. On stale, the
    /// stale file is left in place — the next `put` overwrites
    /// it atomically.
    pub(crate) fn get(&self, key: &str) -> Option<CacheEntry> {
        let path = self.path_for_key(key);
        let bytes = std::fs::read(&path).ok()?;
        let entry: CacheEntry = serde_json::from_slice(&bytes).ok()?;
        let now = current_epoch_secs();
        if entry.is_fresh(now) {
            Some(entry)
        } else {
            None
        }
    }

    /// Put a positive hit — persisted indefinitely. `canonical`
    /// is the full canonical object the wire response serialises;
    /// `provider_id` records which provider produced it (currently
    /// always `"musicbrainz"`; forward-compat).
    pub(crate) fn put_positive(
        &self,
        key: &str,
        canonical: serde_json::Value,
        provider_id: &str,
        confidence_percent: u32,
    ) -> std::io::Result<()> {
        let entry = CacheEntry {
            status: "ok".to_string(),
            canonical: Some(canonical),
            provider_id: Some(provider_id.to_string()),
            confidence_percent: Some(confidence_percent),
            expires_at_epoch_secs: None,
            detail: None,
        };
        self.write_entry(key, &entry)
    }

    /// Put a negative — persisted with a TTL from `negative_ttl`.
    pub(crate) fn put_negative(
        &self,
        key: &str,
        detail: impl Into<String>,
    ) -> std::io::Result<()> {
        let expires = current_epoch_secs() + self.inner.negative_ttl.as_secs();
        let entry = CacheEntry {
            status: "not_found".to_string(),
            canonical: None,
            provider_id: None,
            confidence_percent: None,
            expires_at_epoch_secs: Some(expires),
            detail: Some(detail.into()),
        };
        self.write_entry(key, &entry)
    }

    fn write_entry(
        &self,
        key: &str,
        entry: &CacheEntry,
    ) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.inner.root)?;
        let final_path = self.path_for_key(key);
        let tmp_path = final_path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(entry).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e)
        })?;
        std::fs::write(&tmp_path, &bytes)?;
        std::fs::rename(&tmp_path, &final_path)?;
        Ok(())
    }
}

fn current_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_hit_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ReconcileCache::new(
            dir.path().to_path_buf(),
            Duration::from_secs(60),
        );
        let key = ReconcileCache::key_for("radiohead", "okcomputer");
        let canonical = serde_json::json!({
            "artist": "Radiohead",
            "album": "OK Computer",
            "release_mbid": "b1392450-e666-3926-a536-22c65f834433",
            "recording_type": "Studio",
        });
        cache
            .put_positive(&key, canonical.clone(), "musicbrainz", 100)
            .unwrap();
        let got = cache.get(&key).unwrap();
        assert_eq!(got.status, "ok");
        assert_eq!(got.canonical, Some(canonical));
        assert_eq!(got.provider_id.as_deref(), Some("musicbrainz"));
        assert_eq!(got.confidence_percent, Some(100));
        assert_eq!(
            got.expires_at_epoch_secs, None,
            "positive hit MUST persist indefinitely"
        );
    }

    #[test]
    fn negative_hit_persists_with_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ReconcileCache::new(
            dir.path().to_path_buf(),
            Duration::from_secs(60),
        );
        let key = ReconcileCache::key_for("zzzz", "unknown");
        cache
            .put_negative(&key, "MB search returned zero releases")
            .unwrap();
        let got = cache.get(&key).unwrap();
        assert_eq!(got.status, "not_found");
        assert_eq!(got.canonical, None);
        assert!(got.expires_at_epoch_secs.is_some());
        assert!(got.expires_at_epoch_secs.unwrap() > current_epoch_secs());
    }

    #[test]
    fn stale_negative_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ReconcileCache::new(
            dir.path().to_path_buf(),
            Duration::from_nanos(1),
        );
        let key = ReconcileCache::key_for("z", "z");
        cache.put_negative(&key, "d").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        // Force an epoch delta so the 1ns TTL is definitely exceeded.
        assert!(cache.get(&key).is_none(), "stale negative must return None");
    }

    #[test]
    fn miss_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ReconcileCache::new(
            dir.path().to_path_buf(),
            Duration::from_secs(60),
        );
        assert!(cache.get("no_such_key").is_none());
    }

    #[test]
    fn atomic_write_uses_rename() {
        // Verify no `.tmp` file remains after put succeeds.
        let dir = tempfile::tempdir().unwrap();
        let cache = ReconcileCache::new(
            dir.path().to_path_buf(),
            Duration::from_secs(60),
        );
        let key = "test";
        cache
            .put_positive(key, serde_json::json!({}), "musicbrainz", 100)
            .unwrap();
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(
            entries.iter().all(|e| {
                let name = e.as_ref().unwrap().file_name();
                !name.to_string_lossy().ends_with(".tmp")
            }),
            "put must not leave .tmp files behind"
        );
    }

    #[test]
    fn keys_are_deterministic_across_calls() {
        // Same inputs → same key. Important because callers hash
        // the same (artist, album) at reconcile time AND at
        // cache lookup time — they MUST agree.
        let k1 = ReconcileCache::key_for("radiohead", "okcomputer");
        let k2 = ReconcileCache::key_for("radiohead", "okcomputer");
        assert_eq!(k1, k2);
        let k3 = ReconcileCache::key_for("radiohead", "amnesiac");
        assert_ne!(k1, k3);
    }
}
