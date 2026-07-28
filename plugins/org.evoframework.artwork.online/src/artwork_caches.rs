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
use std::sync::Mutex;
use std::time::{Duration, Instant};

use evo_online_providers::musicbrainz::ArtistLookup;
use lru::LruCache;

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
pub(crate) struct ArtworkCaches {
    pub(crate) reconcile: Mutex<LruCache<String, ReconcileEntry>>,
    pub(crate) provider: Mutex<LruCache<String, ProviderEntry>>,
}

impl ArtworkCaches {
    pub(crate) fn new() -> Self {
        let cap = NonZeroUsize::new(CACHE_CAPACITY)
            .expect("CACHE_CAPACITY is non-zero");
        Self {
            reconcile: Mutex::new(LruCache::new(cap)),
            provider: Mutex::new(LruCache::new(cap)),
        }
    }

    /// Look up a fresh reconcile entry for the fold-key. Stale
    /// entries are removed from the LRU (so the next miss falls
    /// through to a live reconcile without leaving expired
    /// state in the cache).
    pub(crate) fn get_reconcile(
        &self,
        fold_key: &str,
    ) -> Option<ReconcileEntry> {
        let mut lru = self.reconcile.lock().expect("reconcile lock poisoned");
        let entry = lru.get(fold_key)?.clone();
        let now = Instant::now();
        let expiry = match &entry {
            ReconcileEntry::Hit { expires_at, .. }
            | ReconcileEntry::Miss { expires_at, .. } => *expires_at,
        };
        if expiry > now {
            Some(entry)
        } else {
            lru.pop(fold_key);
            None
        }
    }

    pub(crate) fn put_reconcile_hit(
        &self,
        fold_key: String,
        lookup: ArtistLookup,
    ) {
        let mut lru = self.reconcile.lock().expect("reconcile lock poisoned");
        lru.put(
            fold_key,
            ReconcileEntry::Hit {
                lookup: Box::new(lookup),
                expires_at: Instant::now() + RECONCILE_HIT_TTL,
            },
        );
    }

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
    pub(crate) fn get_provider(&self, fold_key: &str) -> Option<ProviderEntry> {
        let mut lru = self.provider.lock().expect("provider lock poisoned");
        let entry = lru.get(fold_key)?.clone();
        if entry.expires_at > Instant::now() {
            Some(entry)
        } else {
            lru.pop(fold_key);
            None
        }
    }

    pub(crate) fn put_provider(&self, fold_key: String, entry: ProviderEntry) {
        let mut lru = self.provider.lock().expect("provider lock poisoned");
        lru.put(fold_key, entry);
    }

    /// Drop every entry from both LRUs. Returns
    /// `(reconcile_entries_dropped, provider_entries_dropped)`
    /// so the caller can surface the counts to the operator.
    /// Called by the plugin's `artwork.online.clear_cache`
    /// verb; also used at unload via a fresh replacement.
    pub(crate) fn drop_all(&self) -> (usize, usize) {
        let reconcile_dropped = {
            let mut lru =
                self.reconcile.lock().expect("reconcile lock poisoned");
            let n = lru.len();
            lru.clear();
            n
        };
        let provider_dropped = {
            let mut lru = self.provider.lock().expect("provider lock poisoned");
            let n = lru.len();
            lru.clear();
            n
        };
        (reconcile_dropped, provider_dropped)
    }

    /// Drop one entry from both LRUs by fold-key. Returns
    /// `(reconcile_dropped_bool, provider_dropped_bool)`
    /// coerced to `(0|1, 0|1)` so the caller can surface a
    /// numeric drop count identical in shape to `drop_all`.
    /// A miss on either LRU is not an error — an operator may
    /// legitimately target an artist that was never resolved.
    pub(crate) fn drop_one(&self, fold_key: &str) -> (usize, usize) {
        let reconcile_dropped = {
            let mut lru =
                self.reconcile.lock().expect("reconcile lock poisoned");
            lru.pop(fold_key).is_some() as usize
        };
        let provider_dropped = {
            let mut lru = self.provider.lock().expect("provider lock poisoned");
            lru.pop(fold_key).is_some() as usize
        };
        (reconcile_dropped, provider_dropped)
    }

    /// Reverse-lookup: given an MBID, return the fold-key
    /// under which the reconcile LRU stored the `Hit`, or
    /// `None` if no `Hit` in the LRU references that MBID.
    /// Linear scan over the reconcile LRU (bounded by
    /// [`CACHE_CAPACITY`]). Used by the targeted-clear verb
    /// when the caller identified an artist by MBID rather
    /// than by raw name.
    pub(crate) fn find_reconcile_fold_key_by_mbid(
        &self,
        mbid: &str,
    ) -> Option<String> {
        let lru = self.reconcile.lock().expect("reconcile lock poisoned");
        for (key, entry) in lru.iter() {
            if let ReconcileEntry::Hit { lookup, .. } = entry {
                if lookup.artist_mbid.as_str() == mbid {
                    return Some(key.clone());
                }
            }
        }
        None
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

    #[test]
    fn reconcile_hit_round_trips() {
        let caches = ArtworkCaches::new();
        caches.put_reconcile_hit("abba".into(), bare_lookup("abba-mbid"));
        let entry = caches
            .get_reconcile("abba")
            .expect("hit entry must be present");
        match entry {
            ReconcileEntry::Hit { lookup, .. } => {
                assert_eq!(lookup.artist_mbid, "abba-mbid");
            }
            _ => panic!("expected Hit"),
        }
    }

    #[test]
    fn reconcile_miss_round_trips() {
        let caches = ArtworkCaches::new();
        caches
            .put_reconcile_miss("nobody".into(), MissReason::NoConfidentMatch);
        match caches.get_reconcile("nobody") {
            Some(ReconcileEntry::Miss { reason, .. }) => {
                assert_eq!(reason, MissReason::NoConfidentMatch);
            }
            _ => panic!("expected Miss"),
        }
    }

    #[test]
    fn reconcile_expired_entry_is_dropped() {
        let caches = ArtworkCaches::new();
        // Insert an already-expired entry by reaching into the
        // LRU directly. Simulates a natural expiration.
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
        // Get should treat the expired entry as absent and
        // remove it from the LRU.
        assert!(caches.get_reconcile("stale").is_none());
        assert!(!caches
            .reconcile
            .lock()
            .unwrap()
            .contains(&"stale".to_string()));
    }

    #[test]
    fn provider_entry_round_trips() {
        let caches = ArtworkCaches::new();
        let sources = vec![serde_json::json!({"provider_id": "theaudiodb"})];
        caches.put_provider("abba".into(), ProviderEntry::new(sources.clone()));
        let entry =
            caches.get_provider("abba").expect("provider entry present");
        assert_eq!(entry.sources, sources);
    }

    #[test]
    fn provider_expired_entry_is_dropped() {
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
        assert!(caches.get_provider("stale").is_none());
        assert!(!caches
            .provider
            .lock()
            .unwrap()
            .contains(&"stale".to_string()));
    }

    #[test]
    fn lru_evicts_oldest_when_capacity_reached() {
        let caches = ArtworkCaches::new();
        // The capacity is 5000; simulate with lower-scoped test
        // by asserting the LRU behaves LRU-shaped.
        for i in 0..(CACHE_CAPACITY + 10) {
            caches.put_reconcile_hit(
                format!("artist-{i}"),
                bare_lookup(&format!("mbid-{i}")),
            );
        }
        // The oldest entries should have evicted.
        assert!(caches.get_reconcile("artist-0").is_none());
        assert!(caches.get_reconcile("artist-9").is_none());
        // The newest ones remain.
        assert!(caches
            .get_reconcile(&format!("artist-{}", CACHE_CAPACITY + 9))
            .is_some());
    }

    #[test]
    fn drop_one_removes_only_the_named_fold_key() {
        let caches = ArtworkCaches::new();
        caches.put_reconcile_hit("abba".into(), bare_lookup("abba-mbid"));
        caches.put_reconcile_hit("adele".into(), bare_lookup("adele-mbid"));
        caches.put_provider(
            "abba".into(),
            ProviderEntry::new(vec![serde_json::json!({"src":"a"})]),
        );
        caches.put_provider(
            "adele".into(),
            ProviderEntry::new(vec![serde_json::json!({"src":"b"})]),
        );
        // Precondition: both keys resolve.
        assert!(caches.get_reconcile("abba").is_some());
        assert!(caches.get_reconcile("adele").is_some());
        assert!(caches.get_provider("abba").is_some());
        assert!(caches.get_provider("adele").is_some());
        // Drop only "abba".
        let (r, p) = caches.drop_one("abba");
        assert_eq!(r, 1);
        assert_eq!(p, 1);
        // "abba" gone from both LRUs, "adele" untouched.
        assert!(caches.get_reconcile("abba").is_none());
        assert!(caches.get_reconcile("adele").is_some());
        assert!(caches.get_provider("abba").is_none());
        assert!(caches.get_provider("adele").is_some());
    }

    #[test]
    fn drop_one_reports_zero_on_missing_fold_key() {
        let caches = ArtworkCaches::new();
        let (r, p) = caches.drop_one("never-cached");
        assert_eq!(r, 0);
        assert_eq!(p, 0);
    }

    #[test]
    fn find_reconcile_fold_key_by_mbid_reverse_lookup() {
        let caches = ArtworkCaches::new();
        caches.put_reconcile_hit("abba".into(), bare_lookup("abba-mbid-x"));
        caches.put_reconcile_hit("adele".into(), bare_lookup("adele-mbid-y"));
        // Miss (never stored) also present so we prove we skip
        // non-`Hit` entries.
        caches
            .put_reconcile_miss("someone".into(), MissReason::NoConfidentMatch);

        assert_eq!(
            caches.find_reconcile_fold_key_by_mbid("abba-mbid-x"),
            Some("abba".into())
        );
        assert_eq!(
            caches.find_reconcile_fold_key_by_mbid("adele-mbid-y"),
            Some("adele".into())
        );
        assert_eq!(
            caches.find_reconcile_fold_key_by_mbid("does-not-exist"),
            None
        );
    }
}
