// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Persistent MB reconcile Hit sidecar for the artist-artwork
//! cascade.
//!
//! Companion to the in-memory
//! [`crate::artwork_caches::ArtworkCaches::reconcile`] LRU: this
//! module stores the reconciled `ArtistLookup` on disk so a
//! restart, or a MusicBrainz outage after cold boot, does not
//! blank an artist tile whose identity was already proven at
//! some point in the past.
//!
//! Layout (mirrors the framework's artwork-resolve-index):
//!
//! - `<state_dir>/reconcile-index/<sha256(fold_key)[0:2]>/<sha256(fold_key)>`
//! - File body: JSON-serialised
//!   [`evo_online_providers::musicbrainz::ArtistLookup`].
//!
//! ## Freshness contract
//!
//! **No expiry.** An MBID is a permanent stable identifier — the
//! entire point of MusicBrainz. Picard / beets / Roon all treat
//! a resolved MBID as permanent and never re-resolve identity
//! on a timer. Invalidation runs on operator gestures only —
//! `artwork.online.clear_cache` (per-artist or global) and the
//! endpoint's `?refresh=1` — never on a clock.
//!
//! ## Positive-only
//!
//! Only the `Found` and `FoundPartial` cases persist here.
//! `Absent` (MB confirmed no-match) stays in-memory under the 6 h
//! Miss TTL so a same-day tag correction does not have to wait
//! for a refresh gesture; a restart correctly flushes it and re-
//! tries. `Unavailable` (MB reachable-but-transient) never
//! writes anywhere — this is the OPEN-1 rule.
//!
//! ## Merge redirects
//!
//! MusicBrainz emits an HTTP 301 to the survivor entity when
//! two artist MBIDs merge. `reqwest` follows redirects by
//! default, so a refresh gesture on a merged MBID naturally
//! returns the survivor's payload, which then overwrites this
//! sidecar under the operator's fold_key — no bespoke merge
//! machinery in this module.

use std::path::{Path, PathBuf};

use evo_online_providers::musicbrainz::ArtistLookup;
use sha2::{Digest, Sha256};

/// Persistent MB reconcile Hit sidecar.
///
/// Cheap to `Arc`-share across the request handler and any
/// background reactor tasks. All operations are async and
/// fault-tolerant: an unreachable filesystem returns `None` on
/// lookup / `Err` on write, never a panic. Callers treat every
/// failure as "sidecar miss, run the reconcile" — the plugin
/// still works, only its persistent acceleration is suppressed
/// for the duration of the fault.
#[derive(Debug, Clone)]
pub(crate) struct ReconcileIndex {
    root: PathBuf,
}

impl ReconcileIndex {
    /// Construct a sidecar rooted under
    /// `<state_dir>/reconcile-index/`. The directory is created
    /// lazily on first write.
    pub(crate) fn new(state_root: PathBuf) -> Self {
        Self {
            root: state_root.join("reconcile-index"),
        }
    }

    /// Read the reconciled `ArtistLookup` memoised for the
    /// fold-key. Returns `None` when no entry is stored, the
    /// stored file is unreadable, or its content fails to
    /// deserialise. Never surfaces I/O errors — a broken entry
    /// is a deliberate miss so the caller falls through to a
    /// fresh reconcile.
    pub(crate) async fn get(&self, fold_key: &str) -> Option<ArtistLookup> {
        let path = self.path_for(&Self::key_hash(fold_key));
        let raw = tokio::fs::read(&path).await.ok()?;
        serde_json::from_slice::<ArtistLookup>(&raw).ok()
    }

    /// Persist the reconciled `ArtistLookup` for the fold-key.
    /// Overwrites any existing entry atomically via a
    /// tempfile + rename. Called by the cache layer on the Ok
    /// path of the cascade — both `Found` (complete URL-rels)
    /// and `FoundPartial` (URL-rels transient but MBID confidently
    /// nailed by the search step). `FoundPartial` entries carry
    /// an empty `deezer_artist_url`; providers that key on MBID
    /// (fanart, TheAudioDB) still fire on retrieval; Deezer
    /// noops until an operator refresh upgrades the entry.
    pub(crate) async fn put(
        &self,
        fold_key: &str,
        lookup: &ArtistLookup,
    ) -> Result<(), std::io::Error> {
        if fold_key.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "reconcile-index refuses empty fold_key",
            ));
        }
        if lookup.artist_mbid.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "reconcile-index refuses lookup with empty artist_mbid",
            ));
        }
        let bytes = serde_json::to_vec(lookup).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e)
        })?;
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

    /// Reverse-lookup: sweep the sidecar dir and return the
    /// fold-key hash whose stored `ArtistLookup.artist_mbid`
    /// matches. O(N) over persisted entries; used by the
    /// operator's targeted `artwork.online.clear_cache` verb
    /// when the caller identified the artist by MBID rather
    /// than by raw name. Returns the RAW fold_key that produced
    /// this entry only if we can recover it — since we hash on
    /// write, we cannot invert; instead, this returns the
    /// filesystem key hash so the caller can call
    /// [`Self::forget_by_key_hash`] to evict without needing
    /// the plaintext fold_key.
    pub(crate) async fn find_key_hash_by_mbid(
        &self,
        mbid: &str,
    ) -> Option<String> {
        let mut outer = tokio::fs::read_dir(&self.root).await.ok()?;
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
                if entry_path.extension().is_some() {
                    continue;
                }
                let Ok(bytes) = tokio::fs::read(&entry_path).await else {
                    continue;
                };
                let Ok(lookup) = serde_json::from_slice::<ArtistLookup>(&bytes)
                else {
                    continue;
                };
                if lookup.artist_mbid == mbid {
                    let file_name = entry_path.file_name()?;
                    return Some(file_name.to_string_lossy().to_string());
                }
            }
        }
        None
    }

    /// Evict by the key hash returned by
    /// [`Self::find_key_hash_by_mbid`]. Same semantics as
    /// [`Self::forget`] but takes the hex-string key rather
    /// than the plaintext fold_key.
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

    fn bare_lookup(mbid: &str) -> ArtistLookup {
        ArtistLookup {
            artist_mbid: mbid.to_string(),
            canonical_name: format!("canonical {mbid}"),
            artist_type: Some("Person".into()),
            life_span_begin: None,
            life_span_end: None,
            country: None,
            wikipedia_url: None,
            wikidata_url: None,
            official_homepage_url: None,
            deezer_artist_url: Some(format!(
                "https://www.deezer.com/artist/{mbid}"
            )),
        }
    }

    #[tokio::test]
    async fn put_then_get_round_trips() {
        let dir = TempDir::new().unwrap();
        let idx = ReconcileIndex::new(dir.path().to_path_buf());
        let lookup = bare_lookup("abba-mbid-x");
        idx.put("abba", &lookup).await.unwrap();
        let recovered = idx.get("abba").await.expect("hit");
        assert_eq!(recovered, lookup);
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let dir = TempDir::new().unwrap();
        let idx = ReconcileIndex::new(dir.path().to_path_buf());
        assert!(idx.get("never-stored").await.is_none());
    }

    #[tokio::test]
    async fn put_overwrites_existing() {
        let dir = TempDir::new().unwrap();
        let idx = ReconcileIndex::new(dir.path().to_path_buf());
        idx.put("abba", &bare_lookup("m1")).await.unwrap();
        idx.put("abba", &bare_lookup("m2")).await.unwrap();
        assert_eq!(idx.get("abba").await.unwrap().artist_mbid, "m2");
    }

    #[tokio::test]
    async fn forget_removes_entry() {
        let dir = TempDir::new().unwrap();
        let idx = ReconcileIndex::new(dir.path().to_path_buf());
        idx.put("abba", &bare_lookup("m")).await.unwrap();
        assert!(idx.forget("abba").await.unwrap());
        assert!(idx.get("abba").await.is_none());
    }

    #[tokio::test]
    async fn forget_of_absent_returns_false() {
        let dir = TempDir::new().unwrap();
        let idx = ReconcileIndex::new(dir.path().to_path_buf());
        assert!(!idx.forget("never").await.unwrap());
    }

    #[tokio::test]
    async fn put_refuses_empty_fold_key() {
        let dir = TempDir::new().unwrap();
        let idx = ReconcileIndex::new(dir.path().to_path_buf());
        assert!(idx.put("", &bare_lookup("m")).await.is_err());
    }

    #[tokio::test]
    async fn put_refuses_empty_mbid() {
        let dir = TempDir::new().unwrap();
        let idx = ReconcileIndex::new(dir.path().to_path_buf());
        let mut bad = bare_lookup("real");
        bad.artist_mbid = "".into();
        assert!(idx.put("abba", &bad).await.is_err());
    }

    #[tokio::test]
    async fn get_ignores_malformed_file() {
        let dir = TempDir::new().unwrap();
        let idx = ReconcileIndex::new(dir.path().to_path_buf());
        let key = ReconcileIndex::key_hash("garbled");
        let shard = dir.path().join("reconcile-index").join(&key[..2]);
        tokio::fs::create_dir_all(&shard).await.unwrap();
        tokio::fs::write(shard.join(&key), b"{not json")
            .await
            .unwrap();
        assert!(idx.get("garbled").await.is_none());
    }

    #[tokio::test]
    async fn find_key_hash_by_mbid_returns_hit() {
        let dir = TempDir::new().unwrap();
        let idx = ReconcileIndex::new(dir.path().to_path_buf());
        idx.put("abba", &bare_lookup("abba-mbid-x")).await.unwrap();
        idx.put("adele", &bare_lookup("adele-mbid-y"))
            .await
            .unwrap();
        let hit = idx.find_key_hash_by_mbid("adele-mbid-y").await.unwrap();
        assert_eq!(hit, ReconcileIndex::key_hash("adele"));
    }

    #[tokio::test]
    async fn find_key_hash_by_mbid_returns_none_on_miss() {
        let dir = TempDir::new().unwrap();
        let idx = ReconcileIndex::new(dir.path().to_path_buf());
        idx.put("abba", &bare_lookup("abba-mbid")).await.unwrap();
        assert!(idx.find_key_hash_by_mbid("nope").await.is_none());
    }

    #[tokio::test]
    async fn forget_by_key_hash_removes_the_shard() {
        let dir = TempDir::new().unwrap();
        let idx = ReconcileIndex::new(dir.path().to_path_buf());
        idx.put("abba", &bare_lookup("m")).await.unwrap();
        let kh = ReconcileIndex::key_hash("abba");
        assert!(idx.forget_by_key_hash(&kh).await.unwrap());
        assert!(idx.get("abba").await.is_none());
    }

    #[tokio::test]
    async fn drop_all_purges_every_entry() {
        let dir = TempDir::new().unwrap();
        let idx = ReconcileIndex::new(dir.path().to_path_buf());
        for i in 0..5 {
            idx.put(&format!("artist-{i}"), &bare_lookup(&format!("m-{i}")))
                .await
                .unwrap();
        }
        let removed = idx.drop_all().await.unwrap();
        assert_eq!(removed, 5);
        for i in 0..5 {
            assert!(idx.get(&format!("artist-{i}")).await.is_none());
        }
    }
}
