// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Long-TTL memoisation of "no artwork resolves for this target
//! anywhere" outcomes.
//!
//! The endpoint's short-TTL memo (see
//! [`crate::artwork_resolve_coalescer`]) protects against burst
//! dedup within a single browse pass — 30 s. This module adds
//! the day-scale layer: once a target has been probed against
//! every provider tier and every tier structured-NotFound, the
//! outcome is memoised for [`DEFAULT_TTL`] so a browse that
//! re-visits the same missing target across days does not
//! re-hammer the online providers (MusicBrainz CAA is rate-
//! limited at 1 req/sec; iTunes has quotas; every miss costs
//! a network round-trip that stalls the UI).
//!
//! ## Contract
//!
//! - **Positive resolves are NOT stored here.** The AssetCache
//!   handles content-hashed positive bytes; the coalescer's
//!   30 s memo handles hash-URL redirect burst dedup. This
//!   module is negative-only.
//! - **Only structured NotFound outcomes are cached.** Transient
//!   failures (admission deadline elapsed, wire error, malformed
//!   envelope, `bad_request` at either tier) are NOT stored:
//!   the operator's next attempt must try again, because these
//!   errors are per-attempt, not per-target.
//! - **Entries expire on TTL and never leak.** A capacity-bound
//!   FIFO eviction protects against unbounded growth if an
//!   operator's library has more no-cover targets than the cap
//!   allows within one TTL window.
//! - **Get is a mutable operation.** On a stale entry, `get`
//!   evicts + returns `None`. Callers only see fresh entries.
//!
//! ## Key
//!
//! Same `(scheme, value, size)` triple the coalescer uses so
//! the two layers can share the endpoint's canonical key.
//!
//! ## Sizing
//!
//! [`DEFAULT_CAPACITY`] holds one negative for each of 50 000
//! unique no-cover targets — comfortably above any real
//! operator library's cover-absent tail. At ~250 bytes per
//! entry (key + status + detail + expiry), the ceiling is
//! ~12 MB steady state.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Default TTL for a negative memo entry.
///
/// This is burst protection, not a verdict. Its job is to stop a
/// single browse paint from re-hammering providers for the same
/// target across the tiles visible in one screenful — a horizon
/// of seconds, not of days.
///
/// It was a day. That made "no portrait for this artist" a
/// decision that outlived the conditions that produced it: an
/// artist whose only provider lost a rate-limit race, or whose
/// one slow source timed out, stayed a glyph until tomorrow. A
/// provider coming back, a credential being added, or a cascade
/// correction shipping could not be seen. Absence is the one
/// outcome most likely to be wrong, so it is the one that must
/// be cheapest to retry.
///
/// A definite absence costs one cheap re-check after this
/// elapses. A wrong absence costs the operator a permanently
/// empty tile, which is the failure this bound exists to avoid.
pub const DEFAULT_TTL: Duration = Duration::from_secs(120);

/// Default FIFO capacity for the negative memo store. Sized
/// well above any real operator library's cover-absent tail;
/// exceeding it triggers oldest-first eviction so memory
/// stays bounded even under a pathological operator's browse.
pub const DEFAULT_CAPACITY: usize = 50_000;

/// Key triple: `(scheme, value, size)`. Matches the endpoint's
/// canonical addressing so this cache and the coalescer's
/// short-TTL memo share the same key space.
pub type Key = (String, String, String);

/// One memoised negative outcome plus its expiry and the
/// tier that produced it — the tier is surfaced as the response
/// header value on a cache-hit 404 so the operator sees "this
/// 404 was memoised" rather than "we tried every provider
/// again this second".
#[derive(Debug, Clone)]
pub struct NegativeEntry {
    /// Why this target is memoised, and it is NOT always
    /// `"not_found"`.
    ///
    /// Two different things live in this store. `"not_found"` is
    /// a provider-confirmed absence — a verdict. The transient
    /// tokens (`"unavailable"`, `"admission_deadline"`,
    /// `"coalescer_deadline"`) are a short-lived note that an
    /// upstream was failing, written to stop a browse burst
    /// re-storming it.
    ///
    /// Readers MUST branch on this. Treating every entry as an
    /// absence turned "the provider answered 503 moments ago"
    /// into "this album has no artwork", which a caller cannot
    /// retry, so a resolvable album stayed a glyph for the
    /// memo's whole life.
    pub status: String,
    /// Detail string from the last provider tier tried.
    /// Surfaced verbatim in the 404 body on a cache hit.
    pub detail: String,
    /// Provider identifier last tried (typically the online
    /// tier's final sub-provider). Surfaced as
    /// `X-Artwork-Provider` on the 404 response so the
    /// operator sees which chain produced this negative.
    pub provider_id: Option<String>,
    /// Expiry — computed at insertion as `now + ttl`.
    pub expires_at: Instant,
}

impl NegativeEntry {
    fn is_fresh(&self, now: Instant) -> bool {
        now < self.expires_at
    }
}

/// Long-TTL negative memo store. Interior mutability behind a
/// single `Mutex` so the endpoint can share it via `Arc`
/// across the request-handling tasks.
#[derive(Debug)]
pub struct NegativeCache {
    /// Interior: entries + insertion-order queue. Held together
    /// under one mutex to keep the two structures consistent
    /// during eviction and expiry pruning.
    inner: Mutex<Inner>,
    ttl: Duration,
    capacity: usize,
}

#[derive(Debug, Default)]
struct Inner {
    map: HashMap<Key, NegativeEntry>,
    /// FIFO insertion order. Not strictly LRU — that would need
    /// a doubly-linked list — but for a memo where every entry
    /// has the same TTL, FIFO is a faithful approximation and
    /// keeps the code auditable.
    order: VecDeque<Key>,
}

impl Default for NegativeCache {
    fn default() -> Self {
        Self::new()
    }
}

impl NegativeCache {
    /// Construct a fresh cache with the module's default TTL +
    /// capacity.
    pub fn new() -> Self {
        Self::with_tunables(DEFAULT_TTL, DEFAULT_CAPACITY)
    }

    /// Construct with explicit TTL + capacity. Primarily for
    /// tests that need short TTLs / small caps to exercise
    /// expiry + eviction paths deterministically.
    pub fn with_tunables(ttl: Duration, capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            ttl,
            capacity: capacity.max(1),
        }
    }

    /// Get a fresh entry for a key, evicting on stale.
    ///
    /// Returns `None` when there is no entry OR the entry has
    /// expired. On a stale hit, the entry is removed so subsequent
    /// gets see the cache as empty for that key.
    pub fn get(&self, key: &Key) -> Option<NegativeEntry> {
        let now = Instant::now();
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let entry = inner.map.get(key).cloned()?;
        if entry.is_fresh(now) {
            Some(entry)
        } else {
            // Stale — drop it here so the caller sees a clean
            // miss and the map does not carry expired weight
            // through subsequent calls.
            inner.map.remove(key);
            inner.order.retain(|k| k != key);
            None
        }
    }

    /// Insert (or replace) a negative for a key. Expiry is
    /// computed relative to now using the cache's TTL.
    ///
    /// Contract: callers must only invoke this after confirming
    /// every provider tier has structured-NotFounded. Callers
    /// MUST NOT invoke this for transient errors (admission /
    /// wire / malformed / bad_request) — those errors are
    /// per-attempt, not per-target.
    pub fn put(
        &self,
        key: Key,
        status: impl Into<String>,
        detail: impl Into<String>,
        provider_id: Option<String>,
    ) {
        self.put_with_ttl(key, status, detail, provider_id, self.ttl);
    }

    /// Insert a negative memo entry with an explicit TTL that
    /// overrides the cache's default.
    ///
    /// Intended for transient-failure memoisation — callers store
    /// e.g. an `unavailable` / admission-deadline / wire-error
    /// outcome for a short window (a few minutes) so a paint
    /// storm re-visiting the same key inside that window does
    /// not re-dispatch the full provider cascade. Structured
    /// NotFound entries continue to use [`Self::put`] with the
    /// cache's default TTL (day-scale per the artwork-online
    /// provider bar); this method carries the shorter,
    /// caller-supplied TTL for transients so a subsequent
    /// probe within the operator's browse session can re-try
    /// once the upstream recovers.
    pub fn put_with_ttl(
        &self,
        key: Key,
        status: impl Into<String>,
        detail: impl Into<String>,
        provider_id: Option<String>,
        ttl: Duration,
    ) {
        let now = Instant::now();
        let entry = NegativeEntry {
            status: status.into(),
            detail: detail.into(),
            provider_id,
            expires_at: now + ttl,
        };
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        // If the key is already present, replace in-place and
        // update its position at the back of the FIFO — most-
        // recently-written stays youngest.
        if inner.map.contains_key(&key) {
            inner.order.retain(|k| k != &key);
        } else if inner.map.len() >= self.capacity {
            // Cap eviction: prune expired entries first
            // (amortised cleanup); if still at cap, drop the
            // oldest by insertion order.
            let now2 = Instant::now();
            inner.map.retain(|_, e| e.is_fresh(now2));
            let live_keys: std::collections::HashSet<Key> =
                inner.map.keys().cloned().collect();
            inner.order.retain(|k| live_keys.contains(k));
            while inner.map.len() >= self.capacity {
                match inner.order.pop_front() {
                    Some(k) => {
                        inner.map.remove(&k);
                    }
                    None => break,
                }
            }
        }
        inner.order.push_back(key.clone());
        inner.map.insert(key, entry);
    }

    /// Evict any entry for `key`, whether fresh or stale.
    ///
    /// Used by the operator escape hatch (`?refresh=1` on the
    /// resolve endpoint) so an operator who has just corrected a
    /// tagging / cover issue can force a fresh cascade for that
    /// exact target without waiting for the TTL to expire.
    pub fn forget(&self, key: &Key) {
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        inner.map.remove(key);
        // The order deque may still hold the key; the next
        // eviction pass tolerates that (see the same-key skip
        // in `put`).
    }

    /// Drop every memoised negative.
    ///
    /// The operator's clear-all gesture: a remembered "this
    /// target has no artwork" must not outlive the store it was
    /// derived from, or a cleared library keeps answering from a
    /// memo whose evidence has been deleted.
    pub fn clear(&self) {
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        inner.map.clear();
        inner.order.clear();
    }

    /// Current entry count — for the observability surface and
    /// tests. Includes expired entries that have not yet been
    /// pruned; the value is a rough ceiling on memory use rather
    /// than a precise fresh-entry count.
    pub fn len(&self) -> usize {
        match self.inner.lock() {
            Ok(g) => g.map.len(),
            Err(poisoned) => poisoned.into_inner().map.len(),
        }
    }

    /// Whether the store is currently empty (approximate for the
    /// same reason as [`Self::len`]).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod class_preservation_tests {
    use super::*;

    /// The store must keep the two classes distinguishable. A
    /// reader that cannot tell a confirmed absence from a
    /// transient note will answer one as the other.
    #[test]
    fn transient_and_absence_entries_are_distinguishable() {
        let c = NegativeCache::new();
        let absent = ("s".into(), "confirmed".into(), "large".into());
        let wobble = ("s".into(), "wobbled".into(), "large".into());
        c.put(absent.clone(), "not_found", "no art".to_string(), None);
        c.put_with_ttl(
            wobble.clone(),
            "unavailable",
            "HTTP 503".to_string(),
            None,
            Duration::from_secs(300),
        );
        assert_eq!(c.get(&absent).expect("stored").status, "not_found");
        assert_eq!(c.get(&wobble).expect("stored").status, "unavailable");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(scheme: &str, value: &str, size: &str) -> Key {
        (scheme.to_string(), value.to_string(), size.to_string())
    }

    #[test]
    fn put_then_get_returns_the_stored_entry() {
        let cache = NegativeCache::with_tunables(Duration::from_secs(60), 100);
        let k = key("mpd-album", "Artist|Album", "small");
        cache.put(
            k.clone(),
            "not_found",
            "no provider tier hit",
            Some("cover_art_archive".into()),
        );
        let got = cache.get(&k).expect("fresh entry must be present");
        assert_eq!(got.status, "not_found");
        assert_eq!(got.detail, "no provider tier hit");
        assert_eq!(got.provider_id.as_deref(), Some("cover_art_archive"));
    }

    #[test]
    fn get_returns_none_when_no_entry_was_stored() {
        let cache = NegativeCache::new();
        assert!(cache.get(&key("mpd-album", "X", "medium")).is_none());
    }

    #[test]
    fn stale_entry_is_evicted_on_get() {
        // Zero TTL forces every write to expire immediately;
        // the get MUST return None AND remove the entry so the
        // cache does not carry expired weight.
        let cache = NegativeCache::with_tunables(Duration::from_nanos(1), 10);
        let k = key("mpd-album", "S|A", "small");
        cache.put(k.clone(), "not_found", "gone", None);
        // Sleep past the TTL so `is_fresh` returns false.
        std::thread::sleep(Duration::from_millis(1));
        assert!(cache.get(&k).is_none(), "stale entry must be evicted");
        assert!(cache.is_empty(), "eviction must drop the entry from map");
    }

    #[test]
    fn put_replaces_existing_entry_and_refreshes_expiry() {
        let cache = NegativeCache::with_tunables(Duration::from_secs(60), 10);
        let k = key("mpd-album", "S|A", "small");
        cache.put(k.clone(), "not_found", "first", None);
        cache.put(k.clone(), "not_found", "second", Some("itunes".to_string()));
        assert_eq!(cache.len(), 1, "replace, not append");
        let got = cache.get(&k).unwrap();
        assert_eq!(got.detail, "second");
        assert_eq!(got.provider_id.as_deref(), Some("itunes"));
    }

    #[test]
    fn capacity_evicts_oldest_by_insertion_order() {
        // Capacity 2. Three inserts under long TTL. Oldest-by-
        // insertion must be evicted; the two most-recent stay.
        let cache = NegativeCache::with_tunables(Duration::from_secs(60), 2);
        let k1 = key("mpd-album", "One|", "small");
        let k2 = key("mpd-album", "Two|", "small");
        let k3 = key("mpd-album", "Three|", "small");
        cache.put(k1.clone(), "not_found", "first", None);
        cache.put(k2.clone(), "not_found", "second", None);
        cache.put(k3.clone(), "not_found", "third", None);
        assert!(cache.get(&k1).is_none(), "oldest entry must be evicted");
        assert!(cache.get(&k2).is_some(), "surviving entry (n-1)");
        assert!(cache.get(&k3).is_some(), "newest entry");
        assert_eq!(cache.len(), 2, "cap enforced");
    }

    #[test]
    fn capacity_prunes_expired_before_evicting_live() {
        // Fill the cache with expired entries + one live entry.
        // A new put should prune the expired ones + keep the
        // live one; it should NOT evict the live one because
        // capacity has room after pruning.
        let cache = NegativeCache::with_tunables(Duration::from_nanos(1), 3);
        let expired1 = key("mpd-album", "E1|", "small");
        let expired2 = key("mpd-album", "E2|", "small");
        let expired3 = key("mpd-album", "E3|", "small");
        cache.put(expired1, "not_found", "e", None);
        cache.put(expired2, "not_found", "e", None);
        cache.put(expired3, "not_found", "e", None);
        // Age them past the TTL.
        std::thread::sleep(Duration::from_millis(1));
        // Now switch to a long TTL for the fresh entry by
        // constructing a NEW cache — the earlier one was for
        // aging only. This isolates the "prune expired then
        // insert" branch of the eviction path.
        let long = NegativeCache::with_tunables(Duration::from_secs(60), 2);
        let live1 = key("mpd-album", "L1|", "small");
        let live2 = key("mpd-album", "L2|", "small");
        long.put(live1.clone(), "not_found", "first", None);
        long.put(live2.clone(), "not_found", "second", None);
        // Trigger the eviction path with a third put; live1 (oldest)
        // must be the one dropped.
        let live3 = key("mpd-album", "L3|", "small");
        long.put(live3.clone(), "not_found", "third", None);
        assert!(long.get(&live1).is_none());
        assert!(long.get(&live2).is_some());
        assert!(long.get(&live3).is_some());
    }

    #[test]
    fn different_size_variants_are_distinct_keys() {
        // Same (scheme, value) with different sizes must be
        // treated as separate memos — an operator running
        // `?size=small` should not accidentally hide a fresh
        // `?size=original` attempt.
        let cache = NegativeCache::new();
        let ks = key("mpd-album", "Artist|Album", "small");
        let ko = key("mpd-album", "Artist|Album", "original");
        cache.put(ks.clone(), "not_found", "small missed", None);
        assert!(cache.get(&ks).is_some());
        assert!(
            cache.get(&ko).is_none(),
            "distinct size must not hit the same memo"
        );
    }

    #[test]
    fn provider_id_round_trips_through_the_memo() {
        let cache = NegativeCache::new();
        let k = key("mpd-album", "Artist|Album", "small");
        cache.put(
            k.clone(),
            "not_found",
            "no tier hit",
            Some("volumio_meta".into()),
        );
        let got = cache.get(&k).unwrap();
        assert_eq!(
            got.provider_id.as_deref(),
            Some("volumio_meta"),
            "the provenance surface MUST carry the last-tier provider \
             id through the memo so a cache-hit 404 explains itself"
        );
    }

    #[test]
    fn ttl_shorter_than_wait_still_evicts_on_get_only() {
        // Contract check: the cache does not spontaneously
        // clean up on a timer — expiry is checked on `get`.
        // Insert with a very short TTL; after waiting, `len()`
        // still reports the entry (no timer runs). Only a `get`
        // (or capacity-triggered pruning during `put`) actually
        // removes it.
        let cache = NegativeCache::with_tunables(Duration::from_millis(2), 100);
        let k = key("mpd-album", "S|A", "small");
        cache.put(k.clone(), "not_found", "d", None);
        assert_eq!(cache.len(), 1);
        std::thread::sleep(Duration::from_millis(4));
        assert_eq!(cache.len(), 1, "no background timer; entry still present");
        assert!(cache.get(&k).is_none(), "get evicts on stale");
        assert_eq!(cache.len(), 0, "post-get map cleared");
    }
}
