// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Result caches for the artist-artwork cascade.
//!
//! The cascade has three cost centres that dominate its
//! wall-clock and social budget:
//!
//! 1. MusicBrainz reconciliation. Every cold request hits
//!    `/ws/2/artist?query=<name>` + `/ws/2/artist/<mbid>
//!    ?inc=url-rels` — two requests through a 1-req/sec
//!    limiter. Repeated calls for the same artist across
//!    browse sessions dominate the cascade's latency.
//! 2. Non-Deezer provider fetches. TheAudioDB / fanart.tv /
//!    volumio_meta. Each is one HTTPS round through a shared
//!    rate limiter; results are stable per artist and safe
//!    to memoise.
//! 3. Deezer. Live-fetch by artist id. Stable id is cached
//!    (via the MB reconcile URL-rels); the image URL itself
//!    is deliberately NOT memoised — Deezer's ToS treats the
//!    response body as a live-fetch surface and the plugin
//!    honours that structurally by refusing to `Serialize`
//!    the response type.
//!
//! Two caches, keyed on the same fold-key
//! ([`evo_device_audio_shared::artist_name::artist_fold_key`]):
//!
//! - [`ReconcileCache`] — memoises the MB reconcile outcome
//!   (positive hit with the full `ArtistLookup` or a negative
//!   miss with reason). Positive TTL is long (7 days: MB
//!   records rarely change); negative TTL is short (6 hours:
//!   an operator tag correction should propagate the same
//!   day). Never eternal-negative-caches — every miss expires
//!   and re-tries.
//! - [`ProviderResultCache`] — memoises the non-Deezer
//!   provider `SourceEntry` outcomes (both hits and misses).
//!   TTL is medium (24 hours: URLs are stable, but a provider
//!   restore-from-outage should propagate within a day).
//!
//! Both caches are LRU-capped at [`CACHE_CAPACITY`] entries so
//! a library with an unbounded artist set cannot exhaust the
//! plugin's memory; the oldest entry evicts on insert. Both
//! are cleared when the plugin unloads (drop of the owning
//! `Arc<ArtworkCaches>`).

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use evo_online_providers::musicbrainz::ArtistLookup;
use lru::LruCache;

use crate::provider_index::ProviderIndex;
use crate::reconcile_index::ReconcileIndex;

/// Working-set cap for both caches. A library with fewer than
/// ~5000 unique fold-keys stays warm indefinitely; larger
/// libraries evict the oldest entries as new artists browse.
const CACHE_CAPACITY: usize = 5000;

/// TTL for a successful MB reconcile. MusicBrainz artist
/// records change on the order of months, so a week is well
/// inside the safe window; the primary side-effect on
/// eviction is a re-fetch of the URL-rels.
const RECONCILE_HIT_TTL: Duration = Duration::from_secs(60 * 60 * 24 * 7);

/// TTL for a negative MB reconcile. Short so an operator tag
/// correction (or a MusicBrainz submission) surfaces within
/// hours rather than days. Never eternal — the "no MB match"
/// verdict is always time-bounded.
const RECONCILE_MISS_TTL: Duration = Duration::from_secs(60 * 60 * 6);

/// TTL for a non-Deezer provider `SourceEntry`. Long enough to
/// carry a browse session cold-start (repeat visits within a
/// day are all warm) but short enough to re-fetch a provider
/// that just restored an image after an outage.
const PROVIDER_RESULT_TTL: Duration = Duration::from_secs(60 * 60 * 24);

/// The cache pair owned by the plugin. Cheap to `Arc`-share
/// across the request-handler and the reactor task.
///
/// Two orthogonal tiers layered on the same fold-key:
///
/// - The in-memory `Mutex<LruCache>` pair. Fast, TTL-bound,
///   size-capped. Absorbs same-session repeat browse. Contains
///   BOTH positives (Hit / Ok) and short-lived negatives
///   (Miss / stale-reason). Dies at process restart.
/// - The persistent `reconcile_index` / `provider_index`
///   sidecars, optional (present iff constructed with
///   [`ArtworkCaches::with_state_dir`]). Positive-only, no
///   expiry. Survives restart. Invalidated by the operator's
///   `artwork.online.clear_cache` verb and the framework's
///   `?refresh=1`.
///
/// Read path: LRU first, then sidecar. Sidecar hit hydrates
/// the LRU as a `Hit` so subsequent same-session reads are
/// LRU-fast; no MusicBrainz call runs on the way in.
///
/// Write path (Hit / Ok): LRU first (fast for same-session
/// repeat), then sidecar (survives restart). Both writes fire;
/// a sidecar write failure logs but does not block the return
/// path — the plugin degrades to in-memory-only for the affected
/// key rather than refusing to answer.
pub(crate) struct ArtworkCaches {
    pub(crate) reconcile: Mutex<LruCache<String, ReconcileEntry>>,
    pub(crate) provider: Mutex<LruCache<String, ProviderEntry>>,
    reconcile_index: Option<Arc<ReconcileIndex>>,
    provider_index: Option<Arc<ProviderIndex>>,
}

impl ArtworkCaches {
    pub(crate) fn new() -> Self {
        Self::build(None)
    }

    /// Construct with a persistent sidecar rooted at the
    /// plugin's `state_dir`. Called by `load()` once the
    /// framework has handed the plugin its per-plugin state
    /// directory; a positive identity persisted here survives
    /// restart AND a MusicBrainz outage AND cold boot with MB
    /// unreachable — the four durability properties this cache
    /// commits to.
    pub(crate) fn with_state_dir(state_dir: PathBuf) -> Self {
        Self::build(Some(state_dir))
    }

    fn build(state_dir: Option<PathBuf>) -> Self {
        let cap = NonZeroUsize::new(CACHE_CAPACITY)
            .expect("CACHE_CAPACITY is non-zero");
        let (reconcile_index, provider_index) = match state_dir {
            Some(dir) => (
                Some(Arc::new(ReconcileIndex::new(dir.clone()))),
                Some(Arc::new(ProviderIndex::new(dir))),
            ),
            None => (None, None),
        };
        Self {
            reconcile: Mutex::new(LruCache::new(cap)),
            provider: Mutex::new(LruCache::new(cap)),
            reconcile_index,
            provider_index,
        }
    }

    /// Look up a fresh reconcile entry for the fold-key. Stale
    /// entries are removed from the LRU (so the next miss falls
    /// through to a live reconcile without leaving expired
    /// state in the cache).
    ///
    /// Two-tier read: consults the in-memory LRU first (fast),
    /// then the persistent [`ReconcileIndex`] sidecar (survives
    /// restart). A sidecar hit is hydrated back into the LRU
    /// under a fresh `Hit` TTL so subsequent same-session reads
    /// stay LRU-fast — and, critically, so a persisted identity
    /// makes MusicBrainz consultation "at most once per artist,
    /// ever" (the P1 invariant): subsequent reconciles skip the
    /// MB round trip entirely.
    pub(crate) async fn get_reconcile(
        &self,
        fold_key: &str,
    ) -> Option<ReconcileEntry> {
        {
            let mut lru =
                self.reconcile.lock().expect("reconcile lock poisoned");
            if let Some(entry) = lru.get(fold_key).cloned() {
                let expiry = match &entry {
                    ReconcileEntry::Hit { expires_at, .. }
                    | ReconcileEntry::Miss { expires_at, .. } => *expires_at,
                };
                if expiry > Instant::now() {
                    return Some(entry);
                }
                lru.pop(fold_key);
            }
        }
        let sidecar = self.reconcile_index.as_ref()?;
        let lookup = sidecar.get(fold_key).await?;
        let hydrated = ReconcileEntry::Hit {
            lookup: Box::new(lookup),
            expires_at: Instant::now() + RECONCILE_HIT_TTL,
        };
        let mut lru = self.reconcile.lock().expect("reconcile lock poisoned");
        lru.put(fold_key.to_string(), hydrated.clone());
        Some(hydrated)
    }

    /// Record a positive reconcile. Writes to LRU (fast,
    /// TTL-bound) AND to the persistent sidecar (no expiry, so a
    /// restart or MB outage does not blank the tile). Called
    /// from BOTH the `Found` path (URL-rels complete) AND the
    /// `FoundPartial` path (search-confident, URL-rels
    /// transient) so an MBID that has been proven at least once
    /// survives every restart — even when the URL-rels sub-step
    /// blipped on the same call.
    pub(crate) async fn put_reconcile_hit(
        &self,
        fold_key: String,
        lookup: ArtistLookup,
    ) {
        {
            let mut lru =
                self.reconcile.lock().expect("reconcile lock poisoned");
            lru.put(
                fold_key.clone(),
                ReconcileEntry::Hit {
                    lookup: Box::new(lookup.clone()),
                    expires_at: Instant::now() + RECONCILE_HIT_TTL,
                },
            );
        }
        if let Some(sidecar) = &self.reconcile_index {
            if let Err(e) = sidecar.put(&fold_key, &lookup).await {
                tracing::warn!(
                    plugin = crate::PLUGIN_NAME,
                    fold_key = %fold_key,
                    error = %e,
                    "artwork online reconcile-index put failed; \
                     persistent identity write skipped for this artist"
                );
            }
        }
    }

    /// Record a definitive-absence reconcile. In-memory ONLY,
    /// under the short 6 h Miss TTL. Not persisted: the corpus
    /// changes underneath (MB adds artists; the operator
    /// retags), and a durable negative would block newly-
    /// available data until manual refresh — the classic
    /// negative-cache anti-pattern (DNS / HTTP / resolver design
    /// all use short-TTL negatives, never durable ones). A
    /// restart correctly re-tries the same day.
    pub(crate) fn put_reconcile_miss(
        &self,
        fold_key: String,
        reason: MissReason,
    ) {
        let mut lru = self.reconcile.lock().expect("reconcile lock poisoned");
        lru.put(
            fold_key,
            ReconcileEntry::Miss {
                reason,
                expires_at: Instant::now() + RECONCILE_MISS_TTL,
            },
        );
    }

    /// Look up a fresh provider-result entry.
    ///
    /// Two-tier read symmetric with [`Self::get_reconcile`]:
    /// LRU first, then the persistent [`ProviderIndex`]
    /// sidecar. A sidecar hit hydrates the LRU under a fresh
    /// TTL. Deezer entries are excluded upstream (in the caller)
    /// per the live-fetch invariant; the sidecar stores whatever
    /// the caller passed.
    pub(crate) async fn get_provider(
        &self,
        fold_key: &str,
    ) -> Option<ProviderEntry> {
        {
            let mut lru = self.provider.lock().expect("provider lock poisoned");
            if let Some(entry) = lru.get(fold_key).cloned() {
                if entry.expires_at > Instant::now() {
                    return Some(entry);
                }
                lru.pop(fold_key);
            }
        }
        let sidecar = self.provider_index.as_ref()?;
        let sources = sidecar.get(fold_key).await?;
        let hydrated = ProviderEntry::new(sources);
        let mut lru = self.provider.lock().expect("provider lock poisoned");
        lru.put(fold_key.to_string(), hydrated.clone());
        Some(hydrated)
    }

    /// Record a provider-result entry. Writes to LRU + sidecar.
    /// An empty `sources` list is a valid entry meaning "every
    /// non-Deezer provider was tried and none returned content";
    /// persisting it prevents a browse burst from re-hammering
    /// exhausted providers after restart.
    pub(crate) async fn put_provider(
        &self,
        fold_key: String,
        entry: ProviderEntry,
    ) {
        {
            let mut lru = self.provider.lock().expect("provider lock poisoned");
            lru.put(fold_key.clone(), entry.clone());
        }
        if let Some(sidecar) = &self.provider_index {
            if let Err(e) = sidecar.put(&fold_key, &entry.sources).await {
                tracing::warn!(
                    plugin = crate::PLUGIN_NAME,
                    fold_key = %fold_key,
                    error = %e,
                    "artwork online provider-index put failed; \
                     persistent provider-result write skipped for this artist"
                );
            }
        }
    }

    /// Drop every entry from both tiers. Returns
    /// `(reconcile_entries_dropped, provider_entries_dropped)`
    /// aggregated across the LRU and the persistent sidecar;
    /// the same numeric-shape the operator UI has always
    /// consumed. Called by the plugin's
    /// `artwork.online.clear_cache` verb (global scope) and at
    /// unload via a fresh replacement.
    pub(crate) async fn drop_all(&self) -> (usize, usize) {
        let mut reconcile_dropped = {
            let mut lru =
                self.reconcile.lock().expect("reconcile lock poisoned");
            let n = lru.len();
            lru.clear();
            n
        };
        let mut provider_dropped = {
            let mut lru = self.provider.lock().expect("provider lock poisoned");
            let n = lru.len();
            lru.clear();
            n
        };
        if let Some(sidecar) = &self.reconcile_index {
            match sidecar.drop_all().await {
                Ok(n) => reconcile_dropped += n,
                Err(e) => tracing::warn!(
                    plugin = crate::PLUGIN_NAME,
                    error = %e,
                    "artwork online reconcile-index drop_all partial failure"
                ),
            }
        }
        if let Some(sidecar) = &self.provider_index {
            match sidecar.drop_all().await {
                Ok(n) => provider_dropped += n,
                Err(e) => tracing::warn!(
                    plugin = crate::PLUGIN_NAME,
                    error = %e,
                    "artwork online provider-index drop_all partial failure"
                ),
            }
        }
        (reconcile_dropped, provider_dropped)
    }

    /// Drop one entry from both tiers by fold-key. Returns
    /// `(reconcile_dropped_count, provider_dropped_count)` where
    /// each count is `0`, `1`, or `2` — 1 for LRU-only hit or
    /// sidecar-only hit, 2 for both. A miss on both is not an
    /// error — an operator may legitimately target an artist
    /// that was never resolved.
    pub(crate) async fn drop_one(&self, fold_key: &str) -> (usize, usize) {
        let mut reconcile_dropped = {
            let mut lru =
                self.reconcile.lock().expect("reconcile lock poisoned");
            lru.pop(fold_key).is_some() as usize
        };
        let mut provider_dropped = {
            let mut lru = self.provider.lock().expect("provider lock poisoned");
            lru.pop(fold_key).is_some() as usize
        };
        if let Some(sidecar) = &self.reconcile_index {
            match sidecar.forget(fold_key).await {
                Ok(true) => reconcile_dropped += 1,
                Ok(false) => {}
                Err(e) => tracing::warn!(
                    plugin = crate::PLUGIN_NAME,
                    fold_key = %fold_key,
                    error = %e,
                    "artwork online reconcile-index forget failed"
                ),
            }
        }
        if let Some(sidecar) = &self.provider_index {
            match sidecar.forget(fold_key).await {
                Ok(true) => provider_dropped += 1,
                Ok(false) => {}
                Err(e) => tracing::warn!(
                    plugin = crate::PLUGIN_NAME,
                    fold_key = %fold_key,
                    error = %e,
                    "artwork online provider-index forget failed"
                ),
            }
        }
        (reconcile_dropped, provider_dropped)
    }

    /// Reverse-lookup: given an MBID, return the fold-key
    /// under which a `Hit` is stored. Scans the LRU first
    /// (fast, bounded by [`CACHE_CAPACITY`]), then the
    /// persistent reconcile sidecar (bounded by disk entries).
    /// Used by the targeted-clear verb when the caller
    /// identified an artist by MBID.
    ///
    /// When the LRU carries a matching Hit, returns the RAW
    /// fold-key (invertible to plaintext because the LRU stores
    /// it plaintext). When only the sidecar has a match, returns
    /// the hashed key hex-string prefixed with `sha256:` — the
    /// caller cannot look this up in the LRU, but can pass it
    /// verbatim to [`Self::drop_one_by_sidecar_key_hash`] to
    /// evict.
    pub(crate) async fn find_reconcile_fold_key_by_mbid(
        &self,
        mbid: &str,
    ) -> Option<String> {
        {
            let lru = self.reconcile.lock().expect("reconcile lock poisoned");
            for (key, entry) in lru.iter() {
                if let ReconcileEntry::Hit { lookup, .. } = entry {
                    if lookup.artist_mbid.as_str() == mbid {
                        return Some(key.clone());
                    }
                }
            }
        }
        let sidecar = self.reconcile_index.as_ref()?;
        let key_hash = sidecar.find_key_hash_by_mbid(mbid).await?;
        Some(format!("sha256:{key_hash}"))
    }

    /// Companion to [`Self::find_reconcile_fold_key_by_mbid`]
    /// for the sidecar-only case: the LRU had no matching
    /// plaintext fold-key but the persistent index did. Takes
    /// the `sha256:<hex>` form returned in that case and evicts
    /// the entry from both sidecars.
    pub(crate) async fn drop_one_by_sidecar_key_hash(
        &self,
        prefixed_key_hash: &str,
    ) -> (usize, usize) {
        let Some(key_hash) = prefixed_key_hash.strip_prefix("sha256:") else {
            return (0, 0);
        };
        let mut reconcile_dropped = 0usize;
        let mut provider_dropped = 0usize;
        if let Some(sidecar) = &self.reconcile_index {
            if let Ok(true) = sidecar.forget_by_key_hash(key_hash).await {
                reconcile_dropped += 1;
            }
        }
        if let Some(sidecar) = &self.provider_index {
            if let Ok(true) = sidecar.forget_by_key_hash(key_hash).await {
                provider_dropped += 1;
            }
        }
        (reconcile_dropped, provider_dropped)
    }
}

impl Default for ArtworkCaches {
    fn default() -> Self {
        Self::new()
    }
}

/// Cached MB reconcile outcome for one fold-key.
///
/// `Hit` boxes the `ArtistLookup` because it is significantly
/// larger than `Miss` — heap-allocating the payload keeps every
/// LRU slot the same size regardless of variant, so 5000
/// `Miss` entries do not each carry an unused `ArtistLookup`-
/// sized hole.
#[derive(Debug, Clone)]
pub(crate) enum ReconcileEntry {
    /// MB search returned a hit at ≥90% confidence; the URL-
    /// rels lookup succeeded (or fabricated bare) and produced
    /// this `ArtistLookup`.
    Hit {
        lookup: Box<ArtistLookup>,
        expires_at: Instant,
    },
    /// MB search returned no hit at confidence, or the
    /// reconcile client is not configured. Short TTL so a
    /// later tag correction surfaces. `reason` is retained
    /// for diagnostic tracing at insert time; the getter does
    /// not consume it in the current code path but the value
    /// stays part of the cache row so a future observer /
    /// stats surface can render it without a cache invalidation.
    Miss {
        #[allow(dead_code)]
        reason: MissReason,
        expires_at: Instant,
    },
}

/// Why the reconcile missed. Only definitive-absence reasons
/// are cacheable — transient upstream failures (rate-limit,
/// 5xx, transport) surface as `Unavailable` on the wire and
/// write NOTHING to this cache (see `ReconcileOutcome`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MissReason {
    /// The MB search returned no hits, or the top hit was
    /// below the confidence threshold. Definitive at MB's
    /// catalogue; safe to memoise under the miss TTL.
    NoConfidentMatch,
}

/// Cached per-provider result for one fold-key.
///
/// `sources` carries one `SourceEntry` per non-Deezer
/// provider that responded to the cascade this cycle — an
/// empty vector represents "all non-Deezer providers were
/// tried and none returned content". Deezer is deliberately
/// excluded (its live-fetch invariant would break if the URL
/// crossed this cache).
#[derive(Debug, Clone)]
pub(crate) struct ProviderEntry {
    pub(crate) sources: Vec<serde_json::Value>,
    pub(crate) expires_at: Instant,
}

impl ProviderEntry {
    pub(crate) fn new(sources: Vec<serde_json::Value>) -> Self {
        Self {
            sources,
            expires_at: Instant::now() + PROVIDER_RESULT_TTL,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_online_providers::musicbrainz::ArtistLookup;

    fn bare_lookup(mbid: &str) -> ArtistLookup {
        ArtistLookup {
            artist_mbid: mbid.to_string(),
            canonical_name: "".to_string(),
            artist_type: None,
            life_span_begin: None,
            life_span_end: None,
            country: None,
            wikipedia_url: None,
            wikidata_url: None,
            official_homepage_url: None,
            deezer_artist_url: None,
        }
    }

    #[tokio::test]
    async fn reconcile_hit_round_trips() {
        let caches = ArtworkCaches::new();
        caches
            .put_reconcile_hit("abba".into(), bare_lookup("abba-mbid"))
            .await;
        let entry = caches
            .get_reconcile("abba")
            .await
            .expect("hit entry must be present");
        match entry {
            ReconcileEntry::Hit { lookup, .. } => {
                assert_eq!(lookup.artist_mbid, "abba-mbid");
            }
            _ => panic!("expected Hit"),
        }
    }

    #[tokio::test]
    async fn reconcile_miss_round_trips() {
        let caches = ArtworkCaches::new();
        caches
            .put_reconcile_miss("nobody".into(), MissReason::NoConfidentMatch);
        match caches.get_reconcile("nobody").await {
            Some(ReconcileEntry::Miss { reason, .. }) => {
                assert_eq!(reason, MissReason::NoConfidentMatch);
            }
            _ => panic!("expected Miss"),
        }
    }

    #[tokio::test]
    async fn reconcile_expired_entry_is_dropped() {
        let caches = ArtworkCaches::new();
        // Insert an already-expired entry by reaching into the
        // LRU directly. Simulates a natural expiration. No
        // sidecar wired in ::new() so the get falls through to
        // None once the LRU expires the entry.
        {
            let mut lru = caches.reconcile.lock().unwrap();
            lru.put(
                "stale".to_string(),
                ReconcileEntry::Hit {
                    lookup: Box::new(bare_lookup("stale")),
                    expires_at: Instant::now() - Duration::from_secs(1),
                },
            );
        }
        assert!(caches.get_reconcile("stale").await.is_none());
        assert!(!caches
            .reconcile
            .lock()
            .unwrap()
            .contains(&"stale".to_string()));
    }

    #[tokio::test]
    async fn provider_entry_round_trips() {
        let caches = ArtworkCaches::new();
        let sources = vec![serde_json::json!({"provider_id": "theaudiodb"})];
        caches
            .put_provider("abba".into(), ProviderEntry::new(sources.clone()))
            .await;
        let entry = caches
            .get_provider("abba")
            .await
            .expect("provider entry present");
        assert_eq!(entry.sources, sources);
    }

    #[tokio::test]
    async fn provider_expired_entry_is_dropped() {
        let caches = ArtworkCaches::new();
        {
            let mut lru = caches.provider.lock().unwrap();
            lru.put(
                "stale".to_string(),
                ProviderEntry {
                    sources: vec![],
                    expires_at: Instant::now() - Duration::from_secs(1),
                },
            );
        }
        assert!(caches.get_provider("stale").await.is_none());
        assert!(!caches
            .provider
            .lock()
            .unwrap()
            .contains(&"stale".to_string()));
    }

    #[tokio::test]
    async fn lru_evicts_oldest_when_capacity_reached() {
        let caches = ArtworkCaches::new();
        // No sidecar wired: the LRU cap of 5000 is the ONLY
        // bound. With sidecar wired the LRU still bounds itself
        // but a fell-out entry can be re-hydrated from disk;
        // that path is exercised in the state-dir tests below.
        for i in 0..(CACHE_CAPACITY + 10) {
            caches
                .put_reconcile_hit(
                    format!("artist-{i}"),
                    bare_lookup(&format!("mbid-{i}")),
                )
                .await;
        }
        assert!(caches.get_reconcile("artist-0").await.is_none());
        assert!(caches.get_reconcile("artist-9").await.is_none());
        assert!(caches
            .get_reconcile(&format!("artist-{}", CACHE_CAPACITY + 9))
            .await
            .is_some());
    }

    #[tokio::test]
    async fn drop_one_removes_only_the_named_fold_key() {
        let caches = ArtworkCaches::new();
        caches
            .put_reconcile_hit("abba".into(), bare_lookup("abba-mbid"))
            .await;
        caches
            .put_reconcile_hit("adele".into(), bare_lookup("adele-mbid"))
            .await;
        caches
            .put_provider(
                "abba".into(),
                ProviderEntry::new(vec![serde_json::json!({"src":"a"})]),
            )
            .await;
        caches
            .put_provider(
                "adele".into(),
                ProviderEntry::new(vec![serde_json::json!({"src":"b"})]),
            )
            .await;
        assert!(caches.get_reconcile("abba").await.is_some());
        assert!(caches.get_reconcile("adele").await.is_some());
        assert!(caches.get_provider("abba").await.is_some());
        assert!(caches.get_provider("adele").await.is_some());
        let (r, p) = caches.drop_one("abba").await;
        assert_eq!(r, 1);
        assert_eq!(p, 1);
        assert!(caches.get_reconcile("abba").await.is_none());
        assert!(caches.get_reconcile("adele").await.is_some());
        assert!(caches.get_provider("abba").await.is_none());
        assert!(caches.get_provider("adele").await.is_some());
    }

    #[tokio::test]
    async fn drop_one_reports_zero_on_missing_fold_key() {
        let caches = ArtworkCaches::new();
        let (r, p) = caches.drop_one("never-cached").await;
        assert_eq!(r, 0);
        assert_eq!(p, 0);
    }

    #[tokio::test]
    async fn find_reconcile_fold_key_by_mbid_reverse_lookup() {
        let caches = ArtworkCaches::new();
        caches
            .put_reconcile_hit("abba".into(), bare_lookup("abba-mbid-x"))
            .await;
        caches
            .put_reconcile_hit("adele".into(), bare_lookup("adele-mbid-y"))
            .await;
        caches
            .put_reconcile_miss("someone".into(), MissReason::NoConfidentMatch);

        assert_eq!(
            caches.find_reconcile_fold_key_by_mbid("abba-mbid-x").await,
            Some("abba".into())
        );
        assert_eq!(
            caches.find_reconcile_fold_key_by_mbid("adele-mbid-y").await,
            Some("adele".into())
        );
        assert_eq!(
            caches
                .find_reconcile_fold_key_by_mbid("does-not-exist")
                .await,
            None
        );
    }

    // -----------------------------------------------------------
    // Persistent-sidecar tests (with_state_dir)
    // -----------------------------------------------------------

    #[tokio::test]
    async fn reconcile_hit_persists_across_lru_flush() {
        let dir = tempfile::TempDir::new().unwrap();
        let caches = ArtworkCaches::with_state_dir(dir.path().to_path_buf());
        caches
            .put_reconcile_hit("abba".into(), bare_lookup("abba-mbid"))
            .await;
        // Flush the LRU to simulate restart (fresh process, LRU
        // empty). Sidecar should re-hydrate on get.
        caches.reconcile.lock().unwrap().clear();
        let entry = caches
            .get_reconcile("abba")
            .await
            .expect("sidecar must re-hydrate LRU on read");
        match entry {
            ReconcileEntry::Hit { lookup, .. } => {
                assert_eq!(lookup.artist_mbid, "abba-mbid");
            }
            _ => panic!("expected Hit"),
        }
        // LRU is now warm again.
        assert!(caches
            .reconcile
            .lock()
            .unwrap()
            .contains(&"abba".to_string()));
    }

    #[tokio::test]
    async fn reconcile_miss_does_not_persist() {
        let dir = tempfile::TempDir::new().unwrap();
        let caches = ArtworkCaches::with_state_dir(dir.path().to_path_buf());
        caches
            .put_reconcile_miss("nobody".into(), MissReason::NoConfidentMatch);
        // Flush LRU — a Miss must NOT survive restart.
        caches.reconcile.lock().unwrap().clear();
        assert!(caches.get_reconcile("nobody").await.is_none());
    }

    #[tokio::test]
    async fn provider_entry_persists_across_lru_flush() {
        let dir = tempfile::TempDir::new().unwrap();
        let caches = ArtworkCaches::with_state_dir(dir.path().to_path_buf());
        let sources = vec![serde_json::json!({"provider_id": "theaudiodb"})];
        caches
            .put_provider("abba".into(), ProviderEntry::new(sources.clone()))
            .await;
        caches.provider.lock().unwrap().clear();
        let entry = caches
            .get_provider("abba")
            .await
            .expect("sidecar must re-hydrate LRU on read");
        assert_eq!(entry.sources, sources);
    }

    #[tokio::test]
    async fn drop_one_purges_both_lru_and_sidecar() {
        let dir = tempfile::TempDir::new().unwrap();
        let caches = ArtworkCaches::with_state_dir(dir.path().to_path_buf());
        caches
            .put_reconcile_hit("abba".into(), bare_lookup("abba-mbid"))
            .await;
        caches
            .put_provider(
                "abba".into(),
                ProviderEntry::new(vec![serde_json::json!({"src": "a"})]),
            )
            .await;
        // Both dropped: LRU + sidecar = 2 each.
        let (r, p) = caches.drop_one("abba").await;
        assert_eq!(r, 2);
        assert_eq!(p, 2);
        // Flush LRU proves sidecar is really gone.
        caches.reconcile.lock().unwrap().clear();
        caches.provider.lock().unwrap().clear();
        assert!(caches.get_reconcile("abba").await.is_none());
        assert!(caches.get_provider("abba").await.is_none());
    }

    #[tokio::test]
    async fn find_reconcile_fold_key_by_mbid_falls_through_to_sidecar() {
        let dir = tempfile::TempDir::new().unwrap();
        let caches = ArtworkCaches::with_state_dir(dir.path().to_path_buf());
        caches
            .put_reconcile_hit("abba".into(), bare_lookup("abba-mbid"))
            .await;
        // Flush the LRU to prove sidecar path fires.
        caches.reconcile.lock().unwrap().clear();
        let recovered = caches
            .find_reconcile_fold_key_by_mbid("abba-mbid")
            .await
            .expect("sidecar reverse lookup must find the fold_key hash");
        assert!(recovered.starts_with("sha256:"));
        // And the sidecar-key-hash drop must evict.
        let (r, _) = caches.drop_one_by_sidecar_key_hash(&recovered).await;
        assert_eq!(r, 1);
    }
}
