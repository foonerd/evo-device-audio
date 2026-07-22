// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Persistent enrichment cache — shared shape for the piece 6
//! bio / album-notes / lyrics verbs.
//!
//! Mirrors the piece 3 [`crate::cache::ReconcileCache`] discipline
//! (one JSON file per key under a caller-supplied root; atomic
//! `.tmp` + rename; positive-indefinite + negative-TTL) but the
//! payload field is generic: entries carry a `serde_json::Value`
//! called `payload` rather than the reconciliation-specific
//! `canonical` object. Each verb creates its own cache instance
//! rooted at a distinct subdirectory (`bio_cache`,
//! `album_notes_cache`, `lyrics_cache`) so entries never collide
//! across shapes.
//!
//! Positive hits persist indefinitely because bios / album notes
//! / lyrics rarely churn once cataloguers agree on them.
//! Negative hits carry a TTL so a track without lyrics on
//! LRCLIB today is retried after a day (in case someone
//! uploaded them since).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Serialisable cache entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EnrichmentEntry {
    /// `"ok"` for positive hit; `"not_found"` for negative.
    pub(crate) status: String,
    /// Provider that produced the hit (`"lastfm"`,
    /// `"musicbrainz_annotation"`, `"lrclib"`). `None` on
    /// negatives that came from a chain-exhaustion path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) provider_id: Option<String>,
    /// The payload the wire response carries. Verb-specific
    /// shape: bio verb stores `{bio, source_url, ...}`; notes
    /// verb stores `{notes, source_url, ...}`; lyrics verb
    /// stores `{lyrics, synced, source_url, ...}`. `None` on
    /// negatives.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) payload: Option<serde_json::Value>,
    /// Expiry epoch seconds. `None` = never expire (positive).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) expires_at_epoch_secs: Option<u64>,
    /// Operator-readable detail on negatives (e.g. "LRCLIB
    /// returned no lyrics for this track").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) detail: Option<String>,
}

impl EnrichmentEntry {
    pub(crate) fn is_fresh(&self, now_epoch_secs: u64) -> bool {
        match self.expires_at_epoch_secs {
            None => true,
            Some(exp) => now_epoch_secs < exp,
        }
    }
}

/// Persistent handle. Clone-shareable across the plugin.
#[derive(Clone)]
pub(crate) struct EnrichmentCache {
    inner: Arc<Inner>,
}

struct Inner {
    root: PathBuf,
    negative_ttl: Duration,
}

impl EnrichmentCache {
    pub(crate) fn new(root: PathBuf, negative_ttl: Duration) -> Self {
        Self {
            inner: Arc::new(Inner { root, negative_ttl }),
        }
    }

    /// Compute the cache key for a namespaced list of
    /// normalised inputs. Callers pass the inputs already
    /// normalised (lower-case, whitespace-collapsed); the cache
    /// hashes them into a stable filename token.
    pub(crate) fn key_for(components: &[&str]) -> String {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for (i, c) in components.iter().enumerate() {
            if i > 0 {
                "|".hash(&mut h);
            }
            c.hash(&mut h);
        }
        format!("{:016x}", h.finish())
    }

    fn path_for_key(&self, key: &str) -> PathBuf {
        self.inner.root.join(format!("{key}.json"))
    }

    pub(crate) fn get(&self, key: &str) -> Option<EnrichmentEntry> {
        let bytes = std::fs::read(self.path_for_key(key)).ok()?;
        let entry: EnrichmentEntry = serde_json::from_slice(&bytes).ok()?;
        let now = current_epoch_secs();
        if entry.is_fresh(now) {
            Some(entry)
        } else {
            None
        }
    }

    pub(crate) fn put_positive(
        &self,
        key: &str,
        payload: serde_json::Value,
        provider_id: &str,
    ) -> std::io::Result<()> {
        let entry = EnrichmentEntry {
            status: "ok".to_string(),
            provider_id: Some(provider_id.to_string()),
            payload: Some(payload),
            expires_at_epoch_secs: None,
            detail: None,
        };
        self.write_entry(key, &entry)
    }

    pub(crate) fn put_negative(
        &self,
        key: &str,
        detail: impl Into<String>,
    ) -> std::io::Result<()> {
        let expires = current_epoch_secs() + self.inner.negative_ttl.as_secs();
        let entry = EnrichmentEntry {
            status: "not_found".to_string(),
            provider_id: None,
            payload: None,
            expires_at_epoch_secs: Some(expires),
            detail: Some(detail.into()),
        };
        self.write_entry(key, &entry)
    }

    fn write_entry(
        &self,
        key: &str,
        entry: &EnrichmentEntry,
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
        let cache = EnrichmentCache::new(
            dir.path().to_path_buf(),
            Duration::from_secs(60),
        );
        let key = EnrichmentCache::key_for(&["radiohead"]);
        let payload = serde_json::json!({
            "bio": "Radiohead are an English rock band...",
            "source_url": "https://last.fm/artist/Radiohead",
        });
        cache.put_positive(&key, payload.clone(), "lastfm").unwrap();
        let got = cache.get(&key).unwrap();
        assert_eq!(got.status, "ok");
        assert_eq!(got.payload, Some(payload));
        assert_eq!(got.provider_id.as_deref(), Some("lastfm"));
        assert_eq!(got.expires_at_epoch_secs, None);
    }

    #[test]
    fn negative_hit_persists_with_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let cache = EnrichmentCache::new(
            dir.path().to_path_buf(),
            Duration::from_secs(60),
        );
        let key = EnrichmentCache::key_for(&["unknown", "artist"]);
        cache.put_negative(&key, "no bio anywhere").unwrap();
        let got = cache.get(&key).unwrap();
        assert_eq!(got.status, "not_found");
        assert_eq!(got.payload, None);
        assert!(got.expires_at_epoch_secs.is_some());
    }

    #[test]
    fn stale_negative_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let cache = EnrichmentCache::new(
            dir.path().to_path_buf(),
            Duration::from_nanos(1),
        );
        let key = EnrichmentCache::key_for(&["x"]);
        cache.put_negative(&key, "d").unwrap();
        std::thread::sleep(Duration::from_millis(10));
        assert!(cache.get(&key).is_none());
    }

    #[test]
    fn multi_component_keys_are_distinct() {
        let k1 = EnrichmentCache::key_for(&["radiohead", "ok computer"]);
        let k2 = EnrichmentCache::key_for(&["radiohead", "kid a"]);
        let k3 = EnrichmentCache::key_for(&["radiohead"]);
        assert_ne!(k1, k2);
        assert_ne!(k1, k3);
        assert_ne!(k2, k3);
    }

    #[test]
    fn atomic_write_leaves_no_tmp_behind() {
        let dir = tempfile::tempdir().unwrap();
        let cache = EnrichmentCache::new(
            dir.path().to_path_buf(),
            Duration::from_secs(60),
        );
        cache
            .put_positive("k", serde_json::json!({}), "lastfm")
            .unwrap();
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(entries.iter().all(|e| !e
            .as_ref()
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")));
    }
}
