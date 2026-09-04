// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Persistent (scheme, value, size) → content_hash sidecar
//! index for the framework's artwork resolve endpoint.
//!
//! Complements [`evo_runtime_http::asset_cache::FilesystemAssetCache`]:
//! the AssetCache stores bytes by content hash; this index
//! stores a small mapping row that makes those bytes reachable
//! by an operator-facing key. Without this index, the
//! endpoint's positive-hit path can only be memoised in the
//! coalescer's 30 s TTL — after that TTL, or after a restart,
//! every browse tile forces a fresh cascade even for artwork
//! whose bytes are still present in the AssetCache. On a
//! 100 k-track library that turns browse into an O(library)
//! per-tile tag-walk.
//!
//! Layout (mirrors [`FilesystemAssetCache`]):
//!
//! - `<root>/artwork-resolve-index/<first-2-chars-of-key-hash>/<full-key-hash>`
//! - File content: 64-lowercase-hex content hash + newline.
//!
//! `key_hash = SHA-256(scheme || 0x1F || value || 0x1F || size)`.
//! The unit separator avoids collisions between distinct
//! `(scheme, value, size)` tuples that would otherwise
//! concatenate to the same byte sequence.
//!
//! ## Freshness contract
//!
//! An index entry is a claim that "if you resolve
//! `(scheme, value, size)` the answer will be this hash." It
//! DOES NOT prove the bytes still live in the AssetCache — an
//! LRU quota eviction or an operator-invoked delete can leave
//! the index pointing at a hash whose bytes are gone. Callers
//! that need byte-serving robustness should treat the 404 on
//! `/api/v1/audio/artwork/<hash>` as an implicit
//! `?refresh=1` — the framework's endpoint layer surfaces the
//! 404 verbatim so the operator UI can retry with refresh.
//!
//! The index write happens AFTER the AssetCache put succeeds,
//! so a positive index entry always corresponds to bytes that
//! WERE stored at some point.
//!
//! ## Invalidation
//!
//! The endpoint's `?refresh=1` gesture calls [`Self::forget`]
//! alongside the negative memo, coalescer memo, and
//! `AssetCache::delete`. See
//! [`evo_runtime_http::artwork_cascade::ArtworkCascade::forget`]
//! for the fan-out.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Persistent artwork-resolve positive index.
///
/// Cheap to `Arc`-share across the endpoint's request-handler
/// tasks. All operations are async and fault-tolerant: an
/// unreachable filesystem returns `None` on lookup / `Err` on
/// write, never a panic. The endpoint layer treats every
/// failure mode as "index miss, run the cascade" — the
/// operator's browse still works, only its acceleration is
/// suppressed for the duration of the fault.
#[derive(Debug, Clone)]
pub struct ArtworkResolveIndex {
    root: PathBuf,
}

/// A resolve-index row: the memoised content hash plus the
/// cascade-logic version that produced it.
///
/// The version is what makes a corrected cascade reach the
/// operator. Without it the index is a permanent positive: a
/// hash chosen by superseded selection rules keeps short-
/// circuiting every browse, so a fix landed in the plugin is
/// invisible on the glass until someone issues an out-of-band
/// refresh. With it, a row written under older rules is refused
/// on read and the cascade re-runs once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexHit {
    /// Memoised content hash, 64 lowercase hex.
    pub hash: String,
    /// Cascade-logic version in force when the row was written.
    /// Rows predating versioning read back as
    /// [`LEGACY_ENTRY_VERSION`].
    pub version: u32,
}

/// Version reported for rows written before the index carried a
/// version at all (a bare content hash on disk).
///
/// Zero rather than "unknown" on purpose: every real version is
/// greater, so legacy rows compare as stale and are re-resolved
/// once. That is the one-shot invalidation of the pre-versioning
/// index, applied lazily per row on first read instead of as a
/// big-bang delete.
pub const LEGACY_ENTRY_VERSION: u32 = 0;

/// On-disk envelope. Written for every new row; the bare-hash
/// predecessor form is still read.
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredEntry {
    #[serde(rename = "v")]
    version: u32,
    hash: String,
}

impl ArtworkResolveIndex {
    /// Construct an index rooted under `<state_root>/artwork-resolve-index/`.
    /// The directory is created lazily on first write.
    pub fn new(state_root: PathBuf) -> Self {
        Self {
            root: state_root.join("artwork-resolve-index"),
        }
    }

    /// Read the content hash memoised for
    /// `(scheme, value, size)`. Returns `None` when no entry
    /// is stored, when the stored file is unreadable, or when
    /// its content fails the 64-lowercase-hex sanity check.
    /// Never surfaces I/O errors — a broken index is a
    /// deliberate miss so the caller falls through to the
    /// cascade.
    pub async fn get(
        &self,
        scheme: &str,
        value: &str,
        size: &str,
    ) -> Option<IndexHit> {
        let path = self.path_for(&Self::key_hash(scheme, value, size));
        let raw = tokio::fs::read_to_string(&path).await.ok()?;
        if let Ok(entry) = serde_json::from_str::<StoredEntry>(&raw) {
            if !is_valid_content_hash(&entry.hash) {
                return None;
            }
            return Some(IndexHit {
                hash: entry.hash,
                version: entry.version,
            });
        }
        // Pre-versioning form: the file body is the bare hash.
        let trimmed = raw.trim();
        if !is_valid_content_hash(trimmed) {
            return None;
        }
        Some(IndexHit {
            hash: trimmed.to_string(),
            version: LEGACY_ENTRY_VERSION,
        })
    }

    /// Fallback lookup: return the content hash for the first
    /// size (in the ["original","large","medium","small"] size
    /// taxonomy, minus `preferred_size` which is checked first)
    /// that has a stored entry for `(scheme, value)`.
    ///
    /// Returns `Some((hash, size_actually_served))` — the caller
    /// can surface `size_actually_served` on the response
    /// provenance so the operator UI knows the bytes came from a
    /// different size than the one requested (the bytes are the
    /// same content; the UI's `<img>` scaling handles the size
    /// mismatch, so operator-visible outcome is "art rendered"
    /// instead of "no art, network storm").
    ///
    /// Rationale: the operator's browse view frequently asks for
    /// one size (`small` for the tile) while a prior browse
    /// action or a prior enrichment resolved bytes at a different
    /// size (`medium` for a detail view, `original` for a full-
    /// screen view). Exact-key-only lookup misses these bytes
    /// and forces a re-storm through the provider cascade every
    /// paint. The fallback returns whatever we already have.
    pub async fn get_any_size(
        &self,
        scheme: &str,
        value: &str,
        preferred_size: &str,
    ) -> Option<(IndexHit, String)> {
        // Preferred size first — this preserves the exact-match
        // fast path when the caller's size is already stored.
        if let Some(hit) = self.get(scheme, value, preferred_size).await {
            return Some((hit, preferred_size.to_string()));
        }
        let want = size_rank(preferred_size);
        for candidate in SIZE_FALLBACK_ORDER {
            if *candidate == preferred_size {
                continue;
            }
            // Downscale only, never up.
            //
            // The order below prefers larger sizes, but preferring
            // is not refusing: with only `small` stored, a `large`
            // request took it and served a thumbnail stretched to
            // fill the tile. Worse, the fallback returns without
            // storing anything, so the requested size never
            // resolved and every later request repeated the
            // substitution — permanently. Two rigs running the
            // same code served the same subject at a 9x size
            // difference, decided by nothing but which size
            // happened to be asked for first.
            //
            // A candidate smaller than the request is refused, so
            // the caller falls through to a real resolve that
            // stores the size actually wanted. That costs one
            // cascade, once, instead of a permanently degraded
            // image.
            match (want, size_rank(candidate)) {
                (Some(w), Some(c)) if c <= w => {}
                // Unknown size on either side: no ordering can be
                // established, so no substitution is made.
                _ => continue,
            }
            if let Some(hit) = self.get(scheme, value, candidate).await {
                return Some((hit, (*candidate).to_string()));
            }
        }
        None
    }

    /// Store the content hash for `(scheme, value, size)`.
    /// Overwrites any existing entry atomically via a
    /// tempfile + rename. Refuses to write when the supplied
    /// content hash fails the shape check — the endpoint
    /// caller has already validated it, but the guard makes
    /// this module defensively self-consistent.
    pub async fn put(
        &self,
        scheme: &str,
        value: &str,
        size: &str,
        content_hash: &str,
        version: u32,
    ) -> Result<(), std::io::Error> {
        if !is_valid_content_hash(content_hash) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "content hash must be 64 lowercase-hex chars",
            ));
        }
        let body = serde_json::to_vec(&StoredEntry {
            version,
            hash: content_hash.to_string(),
        })
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let key = Self::key_hash(scheme, value, size);
        let path = self.path_for(&key);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut tmp = path.clone();
        tmp.as_mut_os_string().push(".tmp");
        tokio::fs::write(&tmp, &body).await?;
        tokio::fs::rename(&tmp, &path).await
    }

    /// Evict the index entry for `(scheme, value, size)`.
    /// Returns `Ok(true)` when an entry was removed;
    /// `Ok(false)` when there was no entry (delete-of-absent
    /// is not an error, matching the AssetCache contract).
    pub async fn forget(
        &self,
        scheme: &str,
        value: &str,
        size: &str,
    ) -> Result<bool, std::io::Error> {
        let path = self.path_for(&Self::key_hash(scheme, value, size));
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Remove every entry, returning the content hashes the
    /// removed entries pointed at.
    ///
    /// The caller needs those hashes: this index owns the
    /// `(scheme, value, size) → hash` mapping, but the bytes
    /// live in the asset cache, and clearing the mapping without
    /// evicting the bytes would leave the store growing with
    /// content nothing can reach any more.
    ///
    /// Order matters for the same reason it does in the
    /// per-target path: the entry is removed before its hash is
    /// reported, so a caller that dies midway leaves bytes
    /// without an index (recoverable — the next resolve
    /// overwrites) rather than an index without bytes (a
    /// persistent mapping to nothing).
    ///
    /// Hashes are de-duplicated: many targets legitimately share
    /// one hash — every size variant that transcoded to identical
    /// bytes, and every artist whose providers returned the same
    /// placeholder — and the caller would otherwise attempt the
    /// same delete repeatedly.
    ///
    /// I/O faults on individual entries are skipped rather than
    /// aborting the sweep: a purge that stops at the first
    /// unreadable shard would leave the operator with a
    /// half-cleared store and no way to finish.
    pub async fn drop_all(&self) -> Result<Vec<String>, std::io::Error> {
        let mut hashes = std::collections::BTreeSet::new();
        let Ok(mut shards) = tokio::fs::read_dir(&self.root).await else {
            // No index directory means nothing was ever stored.
            return Ok(Vec::new());
        };
        while let Ok(Some(shard)) = shards.next_entry().await {
            if !shard.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let Ok(mut entries) = tokio::fs::read_dir(shard.path()).await
            else {
                continue;
            };
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                let hash = tokio::fs::read_to_string(&path)
                    .await
                    .ok()
                    .map(|raw| {
                        // Both the versioned envelope and the
                        // pre-versioning bare-hash form.
                        serde_json::from_str::<StoredEntry>(&raw)
                            .map(|e| e.hash)
                            .unwrap_or_else(|_| raw.trim().to_string())
                    })
                    .filter(|h| is_valid_content_hash(h));
                if tokio::fs::remove_file(&path).await.is_err() {
                    continue;
                }
                if let Some(hash) = hash {
                    hashes.insert(hash);
                }
            }
        }
        Ok(hashes.into_iter().collect())
    }

    fn path_for(&self, key_hash: &str) -> PathBuf {
        let shard = &key_hash[0..2];
        self.root.join(shard).join(key_hash)
    }

    fn key_hash(scheme: &str, value: &str, size: &str) -> String {
        // Unit separator (0x1F) between fields prevents
        // collision between distinct tuples that concatenate
        // to the same byte sequence (e.g. scheme="a", value="b|c"
        // vs scheme="a|b", value="c").
        let mut hasher = Sha256::new();
        hasher.update(scheme.as_bytes());
        hasher.update([0x1F]);
        hasher.update(value.as_bytes());
        hasher.update([0x1F]);
        hasher.update(size.as_bytes());
        let digest = hasher.finalize();
        let mut out = String::with_capacity(64);
        for byte in digest {
            use std::fmt::Write;
            let _ = write!(out, "{byte:02x}");
        }
        out
    }
}

/// Size taxonomy the endpoint accepts. Iterated by
/// [`ArtworkResolveIndex::get_any_size`] as the fallback probe
/// order when a caller's exact-size key misses. Ordered from
/// largest to smallest so `original` bytes are preferred over
/// `small` when either would satisfy the operator (the browser
/// downscales fine; upscaling from small looks worse than
/// downscaling from original).
const SIZE_FALLBACK_ORDER: &[&str] = &["original", "large", "medium", "small"];

/// Position of `size` in [`SIZE_FALLBACK_ORDER`] — 0 is the
/// largest. `None` for a size outside the taxonomy, which makes
/// it ineligible for substitution in either direction.
fn size_rank(size: &str) -> Option<usize> {
    SIZE_FALLBACK_ORDER.iter().position(|s| *s == size)
}

fn is_valid_content_hash(s: &str) -> bool {
    s.len() == 64
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// Path helper exposed for tests + operator diagnostics.
///
/// Not part of the resolve hot path — the framework
/// endpoint only ever calls [`ArtworkResolveIndex::get`] /
/// [`put`] / [`forget`].
#[doc(hidden)]
pub fn path_root(state_root: &Path) -> PathBuf {
    state_root.join("artwork-resolve-index")
}

#[cfg(test)]
mod size_fallback_direction_tests {
    use super::*;

    const HASH: &str =
        "4444444444444444444444444444444444444444444444444444444444444444";

    /// A larger stored size satisfies a smaller request: the
    /// browser downscales and nothing is lost.
    #[tokio::test]
    async fn a_larger_stored_size_serves_a_smaller_request() {
        let dir = tempfile::TempDir::new().unwrap();
        let idx = ArtworkResolveIndex::new(dir.path().to_path_buf());
        idx.put("artist-name", "a", "large", HASH, 1).await.unwrap();
        let (hit, served) =
            idx.get_any_size("artist-name", "a", "small").await.unwrap();
        assert_eq!(served, "large");
        assert_eq!(hit.hash, HASH);
    }

    /// A smaller stored size must NOT satisfy a larger request.
    ///
    /// Serving it stretched a thumbnail across the tile, and
    /// because the fallback path stores nothing, the requested
    /// size never resolved and the substitution repeated for
    /// every later request. Refusing here sends the caller to a
    /// real resolve that stores the size actually wanted.
    #[tokio::test]
    async fn a_smaller_stored_size_does_not_serve_a_larger_request() {
        let dir = tempfile::TempDir::new().unwrap();
        let idx = ArtworkResolveIndex::new(dir.path().to_path_buf());
        idx.put("artist-name", "a", "small", HASH, 1).await.unwrap();
        assert!(
            idx.get_any_size("artist-name", "a", "large")
                .await
                .is_none(),
            "an upscale substitution must be refused"
        );
        assert!(idx
            .get_any_size("artist-name", "a", "original")
            .await
            .is_none());
    }

    /// The exact-size fast path is unaffected by the direction
    /// rule — a stored size always satisfies its own request.
    #[tokio::test]
    async fn an_exact_size_hit_still_wins() {
        let dir = tempfile::TempDir::new().unwrap();
        let idx = ArtworkResolveIndex::new(dir.path().to_path_buf());
        idx.put("artist-name", "a", "small", HASH, 1).await.unwrap();
        let (hit, served) =
            idx.get_any_size("artist-name", "a", "small").await.unwrap();
        assert_eq!(served, "small");
        assert_eq!(hit.hash, HASH);
    }

    /// Largest-first preference is preserved among candidates
    /// that are all big enough.
    #[tokio::test]
    async fn the_largest_eligible_candidate_wins() {
        let dir = tempfile::TempDir::new().unwrap();
        let idx = ArtworkResolveIndex::new(dir.path().to_path_buf());
        idx.put("artist-name", "a", "medium", HASH, 1)
            .await
            .unwrap();
        idx.put("artist-name", "a", "original", HASH, 1)
            .await
            .unwrap();
        let (_, served) =
            idx.get_any_size("artist-name", "a", "small").await.unwrap();
        assert_eq!(served, "original", "largest eligible should win");
    }

    /// A size outside the taxonomy establishes no ordering, so no
    /// substitution is made in either direction.
    #[tokio::test]
    async fn an_unknown_size_is_never_substituted() {
        let dir = tempfile::TempDir::new().unwrap();
        let idx = ArtworkResolveIndex::new(dir.path().to_path_buf());
        idx.put("artist-name", "a", "large", HASH, 1).await.unwrap();
        assert!(idx
            .get_any_size("artist-name", "a", "gigantic")
            .await
            .is_none());
    }
}

#[cfg(test)]
mod version_tests {
    use super::*;

    const HASH: &str =
        "1111111111111111111111111111111111111111111111111111111111111111";

    #[tokio::test]
    async fn a_written_row_reports_the_version_it_was_written_with() {
        let dir = tempfile::TempDir::new().unwrap();
        let idx = ArtworkResolveIndex::new(dir.path().to_path_buf());
        idx.put("artist-name", "a", "large", HASH, 7).await.unwrap();
        let hit = idx.get("artist-name", "a", "large").await.unwrap();
        assert_eq!(hit.hash, HASH);
        assert_eq!(hit.version, 7);
    }

    /// The one-shot invalidation of the pre-versioning index.
    /// A bare-hash file is still readable — the bytes it names
    /// are real — but it reports version 0, which no current
    /// version equals, so the caller re-resolves it once.
    #[tokio::test]
    async fn a_pre_versioning_row_reads_back_as_legacy() {
        let dir = tempfile::TempDir::new().unwrap();
        let idx = ArtworkResolveIndex::new(dir.path().to_path_buf());
        let key = ArtworkResolveIndex::key_hash("artist-name", "a", "large");
        let shard = dir.path().join("artwork-resolve-index").join(&key[..2]);
        tokio::fs::create_dir_all(&shard).await.unwrap();
        tokio::fs::write(shard.join(&key), format!("{HASH}\n"))
            .await
            .unwrap();
        let hit = idx.get("artist-name", "a", "large").await.unwrap();
        assert_eq!(hit.hash, HASH, "legacy bytes are still named");
        assert_eq!(hit.version, LEGACY_ENTRY_VERSION);
        assert_ne!(
            hit.version, 1,
            "a legacy row must never satisfy a current-version check"
        );
    }

    /// The size-fallback arm carries the version too — a stale
    /// row must not sneak back in through the any-size path.
    #[tokio::test]
    async fn any_size_fallback_carries_the_version() {
        let dir = tempfile::TempDir::new().unwrap();
        let idx = ArtworkResolveIndex::new(dir.path().to_path_buf());
        idx.put("artist-name", "a", "original", HASH, 3)
            .await
            .unwrap();
        let (hit, served) =
            idx.get_any_size("artist-name", "a", "small").await.unwrap();
        assert_eq!(served, "original");
        assert_eq!(hit.version, 3);
    }

    /// A corrupt envelope is a miss, not a panic and not a
    /// half-read row.
    #[tokio::test]
    async fn a_malformed_row_is_a_miss() {
        let dir = tempfile::TempDir::new().unwrap();
        let idx = ArtworkResolveIndex::new(dir.path().to_path_buf());
        let key = ArtworkResolveIndex::key_hash("artist-name", "a", "large");
        let shard = dir.path().join("artwork-resolve-index").join(&key[..2]);
        tokio::fs::create_dir_all(&shard).await.unwrap();
        tokio::fs::write(shard.join(&key), b"{not json and not a hash")
            .await
            .unwrap();
        assert!(idx.get("artist-name", "a", "large").await.is_none());
    }

    /// drop_all must recover hashes from both on-disk forms, or
    /// a purge would orphan the bytes of every legacy row.
    #[tokio::test]
    async fn drop_all_recovers_hashes_from_both_forms() {
        let dir = tempfile::TempDir::new().unwrap();
        let idx = ArtworkResolveIndex::new(dir.path().to_path_buf());
        idx.put("artist-name", "new", "large", HASH, 1)
            .await
            .unwrap();
        let legacy_hash =
            "2222222222222222222222222222222222222222222222222222222222222222";
        let key = ArtworkResolveIndex::key_hash("artist-name", "old", "large");
        let shard = dir.path().join("artwork-resolve-index").join(&key[..2]);
        tokio::fs::create_dir_all(&shard).await.unwrap();
        tokio::fs::write(shard.join(&key), format!("{legacy_hash}\n"))
            .await
            .unwrap();
        let mut got = idx.drop_all().await.unwrap();
        got.sort();
        assert_eq!(got, vec![HASH.to_string(), legacy_hash.to_string()]);
    }
}

#[cfg(test)]
mod drop_all_tests {
    use super::*;

    fn h(n: u8) -> String {
        format!("{n:02x}").repeat(32)
    }

    #[tokio::test]
    async fn drop_all_removes_entries_and_reports_their_hashes() {
        let dir = tempfile::TempDir::new().unwrap();
        let idx = ArtworkResolveIndex::new(dir.path().to_path_buf());
        idx.put("artist-name", "a", "large", &h(1), 1)
            .await
            .unwrap();
        idx.put("artist-name", "b", "large", &h(2), 1)
            .await
            .unwrap();
        let mut got = idx.drop_all().await.unwrap();
        got.sort();
        assert_eq!(got, vec![h(1), h(2)]);
        // Entries are gone, so a subsequent sweep finds nothing.
        assert!(idx.drop_all().await.unwrap().is_empty());
        assert!(idx.get("artist-name", "a", "large").await.is_none());
    }

    /// Many targets legitimately share one hash — every size
    /// variant that transcoded to identical bytes, every artist
    /// whose providers returned the same placeholder. The caller
    /// deletes bytes by hash, so a duplicate would be a wasted
    /// second delete of something already gone.
    #[tokio::test]
    async fn drop_all_deduplicates_shared_hashes() {
        let dir = tempfile::TempDir::new().unwrap();
        let idx = ArtworkResolveIndex::new(dir.path().to_path_buf());
        idx.put("artist-name", "a", "large", &h(7), 1)
            .await
            .unwrap();
        idx.put("artist-name", "a", "small", &h(7), 1)
            .await
            .unwrap();
        idx.put("artist-name", "b", "large", &h(7), 1)
            .await
            .unwrap();
        assert_eq!(idx.drop_all().await.unwrap(), vec![h(7)]);
    }

    /// A store that was never written to is not an error — the
    /// operator clearing an empty cache gets a clean zero, not a
    /// failure they have to interpret.
    #[tokio::test]
    async fn drop_all_on_absent_index_is_empty_not_an_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let idx = ArtworkResolveIndex::new(dir.path().to_path_buf());
        assert!(idx.drop_all().await.unwrap().is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn scratch_index() -> (tempfile::TempDir, ArtworkResolveIndex) {
        let tmp = tempfile::tempdir().unwrap();
        let idx = ArtworkResolveIndex::new(tmp.path().to_path_buf());
        (tmp, idx)
    }

    const HASH_A: &str =
        "0000000000000000000000000000000000000000000000000000000000000001";
    const HASH_B: &str =
        "0000000000000000000000000000000000000000000000000000000000000002";

    #[tokio::test]
    async fn put_then_get_round_trip() {
        let (_tmp, idx) = scratch_index().await;
        idx.put("mpd-album", "Artist|Album", "medium", HASH_A, 1)
            .await
            .unwrap();
        let out = idx.get("mpd-album", "Artist|Album", "medium").await;
        assert_eq!(out.map(|h| h.hash).as_deref(), Some(HASH_A));
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let (_tmp, idx) = scratch_index().await;
        assert!(idx
            .get("mpd-album", "Never|Cached", "medium")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn put_overwrites_existing() {
        let (_tmp, idx) = scratch_index().await;
        idx.put("mpd-album", "Artist|Album", "medium", HASH_A, 1)
            .await
            .unwrap();
        idx.put("mpd-album", "Artist|Album", "medium", HASH_B, 1)
            .await
            .unwrap();
        assert_eq!(
            idx.get("mpd-album", "Artist|Album", "medium")
                .await
                .map(|h| h.hash)
                .as_deref(),
            Some(HASH_B)
        );
    }

    #[tokio::test]
    async fn forget_removes_entry() {
        let (_tmp, idx) = scratch_index().await;
        idx.put("mpd-album", "Artist|Album", "medium", HASH_A, 1)
            .await
            .unwrap();
        let existed = idx
            .forget("mpd-album", "Artist|Album", "medium")
            .await
            .unwrap();
        assert!(existed);
        assert!(idx
            .get("mpd-album", "Artist|Album", "medium")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn forget_of_absent_returns_false() {
        let (_tmp, idx) = scratch_index().await;
        let existed = idx
            .forget("mpd-album", "Never|Cached", "medium")
            .await
            .unwrap();
        assert!(!existed);
    }

    #[tokio::test]
    async fn put_refuses_invalid_content_hash() {
        let (_tmp, idx) = scratch_index().await;
        let bad = idx
            .put("mpd-album", "Artist|Album", "medium", "not-a-hash", 1)
            .await;
        assert!(bad.is_err());
    }

    #[tokio::test]
    async fn keys_do_not_collide_across_delimiters() {
        // Regression: scheme || value collisions.
        // Without the 0x1F separator the two calls would hash
        // to the same key ("a" + "|b" == "a|" + "b").
        let (_tmp, idx) = scratch_index().await;
        idx.put("a", "|b", "medium", HASH_A, 1).await.unwrap();
        idx.put("a|", "b", "medium", HASH_B, 1).await.unwrap();
        assert_eq!(
            idx.get("a", "|b", "medium")
                .await
                .map(|h| h.hash)
                .as_deref(),
            Some(HASH_A)
        );
        assert_eq!(
            idx.get("a|", "b", "medium")
                .await
                .map(|h| h.hash)
                .as_deref(),
            Some(HASH_B)
        );
    }

    #[tokio::test]
    async fn get_ignores_malformed_file_content() {
        let (tmp, idx) = scratch_index().await;
        // Write a hand-crafted broken entry into the index
        // path scheme.
        let key = ArtworkResolveIndex::key_hash(
            "mpd-album",
            "Artist|Album",
            "medium",
        );
        let path = tmp
            .path()
            .join("artwork-resolve-index")
            .join(&key[0..2])
            .join(&key);
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&path, "not-a-hash\n").await.unwrap();
        // The get path defensively refuses the entry.
        assert!(idx
            .get("mpd-album", "Artist|Album", "medium")
            .await
            .is_none());
    }
}
