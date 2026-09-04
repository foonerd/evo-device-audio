// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Artwork resolve-endpoint coalescer + short-TTL hash memo.
//!
//! The `GET /api/v1/audio/artwork?scheme=&value=&size=` endpoint
//! dispatches to the `artwork.providers` shelf on every hit.
//! Without coalescing, N concurrent same-key requests (a
//! browser rendering 3000 track thumbs) fan out to N plugin
//! dispatches, each running the same disk-read + tag-parse +
//! hash work. Under sequential back-channel processing
//! (framework-side `forward_plugin_request` awaits inline) the
//! N dispatches serialise into a growing queue behind the
//! plugin's own 64-slot dispatch semaphore; the semaphore
//! saturates and every subsequent artwork request stalls.
//!
//! This coalescer sits between the resolve endpoint and the
//! dispatcher, keyed on the canonical `(scheme, value, size)`
//! tuple. Two collapse layers:
//!
//! - **Short-TTL memo** ([`RESOLVE_MEMO_TTL`], 30 s): every
//!   successful or negative resolve is cached under its key
//!   for the TTL window. Warm-cache hits return the stored
//!   `Result<String, String>` (hash or error) directly — zero
//!   plugin dispatch, zero back-channel serialisation.
//! - **Single-flight inflight** (bounded by
//!   [`INFLIGHT_WAIT_DEADLINE`], 30 s): when the memo misses,
//!   the first caller for a key becomes the fetcher; every
//!   concurrent same-key caller subscribes to the fetcher's
//!   `broadcast::Sender` and awaits the outcome. Cancellation
//!   safety is enforced by an RAII `InflightGuard` — if the
//!   fetcher's outer future is dropped mid-dispatch, the
//!   guard removes the inflight entry synchronously, waiters
//!   see the sender close, and the next caller becomes the
//!   fresh fetcher. Never an infinite hang.
//!
//! Two together: N concurrent cold-cache calls for one key
//! result in exactly one plugin dispatch; N warm calls result
//! in zero. Cross-key work is naturally isolated — one hung
//! key does not affect another key's memo or inflight slot.
//!
//! ## Failure memoization
//!
//! Both success (`Ok(hash)`) and failure (`Err(reason)`)
//! outcomes are memoized under the same TTL. This prevents a
//! browse burst from re-hammering the provider for a track
//! whose art genuinely does not resolve (missing tag, no
//! sidecar, no online provider match). Operator edits (adds
//! a cover file, fixes a tag) surface within the TTL window;
//! for zero-latency refresh the operator supplies a distinct
//! size or triggers a plugin-side invalidation (out of scope
//! for this coalescer).
//!
//! ## Bounded waiter deadline
//!
//! Waiters awaiting a fetcher that never publishes return
//! [`CoalesceError::WaitDeadlineElapsed`] — structured,
//! matchable, discriminated from `FetcherError`. The endpoint
//! translates the deadline error to `503 Service Unavailable`
//! with `Retry-After` (see the endpoint-side handler); the
//! coalescer itself is transport-agnostic.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::broadcast;

/// TTL of the resolved-hash memo. A key's outcome is served
/// from the memo for this long after a successful (or failed)
/// resolve. Longer = more warm-cache benefit during a browse
/// session; shorter = faster pickup of operator tag edits.
/// 30 s is the neutral default: 6x the resolve redirect's
/// browser `Cache-Control: max-age=5` (so most browse re-hits
/// during a session take the memo path) while keeping edit
/// staleness under half a minute.
pub const RESOLVE_MEMO_TTL: Duration = Duration::from_secs(30);

/// Bounded wait deadline for the inflight sleeper arm — a
/// caller awaiting an in-flight fetcher gives up after this
/// window and returns [`CoalesceError::WaitDeadlineElapsed`].
/// Matches the framework `FilesystemAssetCache`'s deadline so
/// operators see one consistent artwork-resolve latency ceiling.
pub const INFLIGHT_WAIT_DEADLINE: Duration = Duration::from_secs(30);

/// A resolved-hash outcome for a `(scheme, value, size)` key.
///
/// The coalescer memoizes both success (`Ok((content_hash, provider_id))`)
/// and failure (`Err(reason)`) shapes so a burst against
/// unresolvable art does not re-hammer the provider.
///
/// The success arm carries the provider identifier alongside the
/// content hash so a memo hit can surface the same
/// `X-Artwork-Provider` value the fetcher originally produced —
/// without which a warm-cache render would drop provenance and
/// the operator UI could not distinguish "cover came from local
/// sidecar" from "cover came from Cover Art Archive" for the
/// same album across repeat visits.
///
/// `provider_id` is `Option<String>` because pre-provenance
/// plugin builds omit the field entirely; the coalescer must
/// tolerate that gracefully rather than panic on a missing
/// value.
pub type ResolveOutcome = Result<(String, Option<String>), String>;

/// Structured error the coalescer surfaces at the boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoalesceError {
    /// The waiter's bounded deadline
    /// ([`INFLIGHT_WAIT_DEADLINE`]) elapsed without the
    /// in-flight fetcher publishing an outcome. The caller
    /// decides whether to retry, degrade, or surface a
    /// timeout upstream.
    WaitDeadlineElapsed {
        /// Canonical scheme (e.g. `"mpd-album"`).
        scheme: String,
        /// Scheme-specific value that timed out.
        value: String,
        /// Canonical size (e.g. `"medium"`).
        size: String,
        /// Milliseconds the waiter blocked before giving up.
        waited_ms: u64,
    },
    /// The fetcher itself returned an error; propagated to
    /// waiters verbatim and memoized under the failure TTL.
    FetcherError {
        /// Canonical scheme.
        scheme: String,
        /// Scheme-specific value.
        value: String,
        /// Canonical size.
        size: String,
        /// Reason the fetcher returned; memoized so a burst
        /// against the same failing key does not re-hammer
        /// upstream.
        reason: String,
    },
}

impl std::fmt::Display for CoalesceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WaitDeadlineElapsed {
                scheme,
                value,
                size,
                waited_ms,
            } => write!(
                f,
                "artwork resolve coalescer wait deadline elapsed for \
                 (scheme={scheme}, value={value}, size={size}) after \
                 {waited_ms} ms"
            ),
            Self::FetcherError {
                scheme,
                value,
                size,
                reason,
            } => write!(
                f,
                "artwork resolve fetcher failed for (scheme={scheme}, \
                 value={value}, size={size}): {reason}"
            ),
        }
    }
}

impl std::error::Error for CoalesceError {}

type Key = (String, String, String);

/// Memoized outcome plus its expiry.
#[derive(Debug, Clone)]
struct MemoEntry {
    outcome: ResolveOutcome,
    expires_at: Instant,
}

impl MemoEntry {
    fn is_fresh(&self, now: Instant) -> bool {
        now < self.expires_at
    }
}

/// RAII guard that removes an inflight-map entry on drop.
///
/// Cancellation-safe by construction: whether the fetcher
/// returns, is cancelled, or panics, the guard's `Drop` runs
/// and clears the inflight slot. If `outcome` is set, waiters
/// are woken with the answer; if unset (cancel / panic mid-
/// dispatch), waiters see the sender close and re-enter the
/// coalescer — where they either hit a fresh memo (if a
/// concurrent fetcher landed the answer) or become the next
/// fetcher.
///
/// Mirrors the pattern from
/// `evo::asset_cache::FilesystemAssetCache::get_or_fetch`;
/// intentionally kept as a private type here (the endpoint-
/// side coalescer is its own concern, not a general primitive
/// re-export).
struct InflightGuard {
    inflight: Arc<Mutex<HashMap<Key, broadcast::Sender<ResolveOutcome>>>>,
    key: Key,
    outcome: Option<ResolveOutcome>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        let mut map = match self.inflight.lock() {
            Ok(m) => m,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(tx) = map.remove(&self.key) {
            if let Some(outcome) = self.outcome.take() {
                let _ = tx.send(outcome);
            }
            // Otherwise the sender drops; subscribed waiters
            // get `RecvError::Closed` and re-enter the
            // coalescer.
        }
    }
}

/// Coalescer type. Holds the memo + inflight maps behind
/// interior mutability; safe to share via `Arc` across the
/// endpoint's request-handling tasks.
#[derive(Debug)]
pub struct ArtworkResolveCoalescer {
    memo: Arc<Mutex<HashMap<Key, MemoEntry>>>,
    inflight: Arc<Mutex<HashMap<Key, broadcast::Sender<ResolveOutcome>>>>,
    memo_ttl: Duration,
    wait_deadline: Duration,
}

impl Default for ArtworkResolveCoalescer {
    fn default() -> Self {
        Self::new()
    }
}

impl ArtworkResolveCoalescer {
    /// Construct a fresh coalescer with the module's default
    /// TTL + wait deadline.
    pub fn new() -> Self {
        Self::with_tunables(RESOLVE_MEMO_TTL, INFLIGHT_WAIT_DEADLINE)
    }

    /// Construct with explicit TTL + wait deadline. Primarily
    /// for tests that want to exercise expiry / timeout paths
    /// without waiting the production deadline.
    pub fn with_tunables(memo_ttl: Duration, wait_deadline: Duration) -> Self {
        Self {
            memo: Arc::new(Mutex::new(HashMap::new())),
            inflight: Arc::new(Mutex::new(HashMap::new())),
            memo_ttl,
            wait_deadline,
        }
    }

    /// Evict any memoised outcome for `(scheme, value, size)`.
    ///
    /// The operator escape hatch: `?refresh=1` on the resolve
    /// endpoint calls the cascade's `forget()` which in turn
    /// calls this, so a fresh resolve re-runs the plugin
    /// dispatch chain rather than re-hydrating the previous
    /// outcome from the coalescer's memo. Without this hook a
    /// refresh that evicted the AssetCache entry would race
    /// with the memo — the resolve endpoint would 302 to the
    /// same content hash whose bytes we just evicted, and the
    /// hash endpoint would 404 until `memo_ttl` elapsed.
    pub fn forget(&self, scheme: &str, value: &str, size: &str) {
        let key: Key =
            (scheme.to_string(), value.to_string(), size.to_string());
        let mut memo = self.memo.lock().expect("memo poisoned");
        memo.remove(&key);
    }

    /// Drop every memoised outcome.
    ///
    /// Paired with the operator's clear-all gesture: a positive
    /// memo that outlives the bytes it names would 302 callers
    /// at a hash that no longer resolves.
    pub fn clear(&self) {
        let mut memo = self.memo.lock().expect("memo poisoned");
        memo.clear();
    }

    /// Resolve (or coalesce) the outcome for a key.
    ///
    /// Fast path — memo hit: returns the stored outcome
    /// directly.
    ///
    /// Slow path — memo miss:
    ///
    /// - If a fetcher is already in-flight for this key, the
    ///   caller subscribes to the fetcher's broadcast and
    ///   awaits with a bounded deadline.
    /// - Otherwise the caller becomes the fetcher: runs
    ///   `fetcher()`, stores the outcome in the memo,
    ///   publishes to waiters, drops the RAII guard.
    ///
    /// `fetcher` is invoked at most once per key per
    /// (memo-miss × inflight-empty) event. It returns a
    /// [`ResolveOutcome`] — the wire-level outcome. The
    /// coalescer wraps the outcome into
    /// [`CoalesceError::FetcherError`] on the failure arm for
    /// callers that want a discriminated error type.
    pub async fn resolve_or_coalesce<F, Fut>(
        &self,
        scheme: &str,
        value: &str,
        size: &str,
        fetcher: F,
    ) -> Result<(String, Option<String>), CoalesceError>
    where
        F: FnOnce() -> Fut + Send,
        Fut: std::future::Future<Output = ResolveOutcome> + Send,
    {
        let key: Key =
            (scheme.to_string(), value.to_string(), size.to_string());

        // Fast-path memo check.
        {
            let mut memo = self.memo.lock().expect("memo poisoned");
            let now = Instant::now();
            if let Some(entry) = memo.get(&key) {
                if entry.is_fresh(now) {
                    return match entry.outcome.clone() {
                        Ok(pair) => Ok(pair),
                        Err(reason) => Err(CoalesceError::FetcherError {
                            scheme: key.0.clone(),
                            value: key.1.clone(),
                            size: key.2.clone(),
                            reason,
                        }),
                    };
                } else {
                    memo.remove(&key);
                }
            }
        }

        // Miss path: fetcher / sleeper arm.
        let sleeper_rx = {
            let mut inflight = self.inflight.lock().expect("inflight poisoned");
            if let Some(tx) = inflight.get(&key) {
                Some(tx.subscribe())
            } else {
                let (tx, _rx) = broadcast::channel(8);
                inflight.insert(key.clone(), tx);
                None
            }
        };

        if let Some(mut rx) = sleeper_rx {
            let started = Instant::now();
            let recv_result =
                tokio::time::timeout(self.wait_deadline, rx.recv()).await;
            return match recv_result {
                Ok(Ok(Ok(pair))) => Ok(pair),
                Ok(Ok(Err(reason))) => Err(CoalesceError::FetcherError {
                    scheme: key.0,
                    value: key.1,
                    size: key.2,
                    reason,
                }),
                Ok(Err(_)) => {
                    // Sender closed without publishing —
                    // fetcher was cancelled. Re-enter (become
                    // fresh fetcher or hit a newly-populated
                    // memo).
                    Box::pin(
                        self.resolve_or_coalesce(
                            &key.0, &key.1, &key.2, fetcher,
                        ),
                    )
                    .await
                }
                Err(_) => Err(CoalesceError::WaitDeadlineElapsed {
                    scheme: key.0,
                    value: key.1,
                    size: key.2,
                    waited_ms: started.elapsed().as_millis() as u64,
                }),
            };
        }

        // We are the fetcher. Construct the RAII guard.
        let mut guard = InflightGuard {
            inflight: Arc::clone(&self.inflight),
            key: key.clone(),
            outcome: None,
        };

        let outcome = fetcher().await;
        guard.outcome = Some(outcome.clone());

        // Memoize both success and failure under the same TTL
        // — a browse burst against unresolvable art must not
        // re-hammer the provider.
        {
            let mut memo = self.memo.lock().expect("memo poisoned");
            memo.insert(
                key.clone(),
                MemoEntry {
                    outcome: outcome.clone(),
                    expires_at: Instant::now() + self.memo_ttl,
                },
            );
        }
        drop(guard);

        match outcome {
            Ok(pair) => Ok(pair),
            Err(reason) => Err(CoalesceError::FetcherError {
                scheme: key.0,
                value: key.1,
                size: key.2,
                reason,
            }),
        }
    }

    /// Test-only introspection: current inflight-map size.
    #[cfg(test)]
    pub(crate) fn inflight_len(&self) -> usize {
        self.inflight.lock().expect("inflight poisoned").len()
    }

    /// Test-only introspection: current memo size (fresh +
    /// expired; expired entries are lazily evicted on next
    /// `resolve_or_coalesce` for that key).
    #[cfg(test)]
    pub(crate) fn memo_len(&self) -> usize {
        self.memo.lock().expect("memo poisoned").len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Sub-second tunables for tests that exercise the memo
    /// expiry or the waiter deadline without waiting 30 s.
    fn cache_with_short_tunables() -> Arc<ArtworkResolveCoalescer> {
        Arc::new(ArtworkResolveCoalescer::with_tunables(
            Duration::from_millis(200),
            Duration::from_millis(400),
        ))
    }

    #[tokio::test]
    async fn memo_hit_short_circuits_upstream() {
        // First call invokes the fetcher and memoizes; second
        // call within the TTL window MUST NOT invoke the
        // fetcher.
        let coalescer = Arc::new(ArtworkResolveCoalescer::new());
        let call_count = Arc::new(AtomicUsize::new(0));

        let ci = Arc::clone(&call_count);
        let first = coalescer
            .resolve_or_coalesce(
                "mpd-album",
                "Beatles|Revolver",
                "medium",
                || async move {
                    ci.fetch_add(1, Ordering::SeqCst);
                    Ok(("abc".to_string(), None))
                },
            )
            .await
            .unwrap();
        assert_eq!(first, ("abc".to_string(), None));
        assert_eq!(call_count.load(Ordering::SeqCst), 1);

        let ci = Arc::clone(&call_count);
        let second = coalescer
            .resolve_or_coalesce(
                "mpd-album",
                "Beatles|Revolver",
                "medium",
                || async move {
                    ci.fetch_add(1, Ordering::SeqCst);
                    Ok(("SHOULD_NOT_BE_RETURNED".to_string(), None))
                },
            )
            .await
            .unwrap();
        assert_eq!(
            second,
            ("abc".to_string(), None),
            "second call must return the memoized hash, not \
             invoke the fetcher"
        );
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "second call must NOT invoke the fetcher"
        );
    }

    #[tokio::test]
    async fn memo_stale_re_dispatches_fresh_fetcher() {
        // With a sub-second TTL, sleep past expiry, then
        // verify the fetcher runs again.
        let coalescer = cache_with_short_tunables();
        let call_count = Arc::new(AtomicUsize::new(0));

        let ci = Arc::clone(&call_count);
        let first = coalescer
            .resolve_or_coalesce("mpd-album", "A|B", "medium", || async move {
                ci.fetch_add(1, Ordering::SeqCst);
                Ok(("first".to_string(), None))
            })
            .await
            .unwrap();
        assert_eq!(first, ("first".to_string(), None));

        tokio::time::sleep(Duration::from_millis(300)).await;

        let ci = Arc::clone(&call_count);
        let second = coalescer
            .resolve_or_coalesce("mpd-album", "A|B", "medium", || async move {
                ci.fetch_add(1, Ordering::SeqCst);
                Ok(("second".to_string(), None))
            })
            .await
            .unwrap();
        assert_eq!(
            second,
            ("second".to_string(), None),
            "stale memo must be evicted; fresh fetcher must run"
        );
        assert_eq!(call_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn resolves_same_key_across_concurrent_callers_via_single_upstream() {
        // 20 concurrent same-key callers must collapse to ONE
        // fetcher invocation. Named regression from the memo:
        // "N concurrent cold fetches of one key, single
        // upstream".
        let coalescer = Arc::new(ArtworkResolveCoalescer::new());
        let call_count = Arc::new(AtomicUsize::new(0));
        let release_fetcher = Arc::new(tokio::sync::Notify::new());
        let release_for_task = Arc::clone(&release_fetcher);

        let mut handles = Vec::new();
        for _ in 0..20 {
            let c = Arc::clone(&coalescer);
            let cc = Arc::clone(&call_count);
            let rel = Arc::clone(&release_for_task);
            handles.push(tokio::spawn(async move {
                c.resolve_or_coalesce(
                    "mpd-album",
                    "Same|Same",
                    "medium",
                    move || async move {
                        cc.fetch_add(1, Ordering::SeqCst);
                        // Let all peers subscribe before the
                        // fetcher completes.
                        rel.notified().await;
                        Ok(("one-hash".to_string(), None))
                    },
                )
                .await
            }));
        }

        // Give the peers a moment to arrive at the subscribe
        // point.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Release the (single) fetcher.
        release_fetcher.notify_one();

        // All 20 must resolve Ok with the same hash.
        for handle in handles {
            let outcome = tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .expect("handle must complete within bound")
                .expect("task must not panic")
                .expect("resolve must succeed");
            assert_eq!(outcome, ("one-hash".to_string(), None));
        }

        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "N=20 concurrent same-key callers MUST collapse to \
             exactly one fetcher invocation"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cross_key_calls_do_not_serialise() {
        // Key K blocked forever must not block key K'.
        // Isolation regression from the memo verbatim.
        let coalescer = Arc::new(ArtworkResolveCoalescer::new());
        let never_release = Arc::new(tokio::sync::Notify::new());
        let rel_clone = Arc::clone(&never_release);

        let c = Arc::clone(&coalescer);
        let blocked_task = tokio::spawn(async move {
            c.resolve_or_coalesce(
                "mpd-album",
                "Blocked|Album",
                "medium",
                move || async move {
                    rel_clone.notified().await;
                    unreachable!("never released in this test")
                },
            )
            .await
        });

        // Give the blocked fetcher time to install its
        // inflight entry.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // A call on a DIFFERENT key must complete.
        let other = tokio::time::timeout(
            Duration::from_secs(2),
            coalescer.resolve_or_coalesce(
                "mpd-album",
                "Other|Album",
                "medium",
                || async { Ok(("other-hash".to_string(), None)) },
            ),
        )
        .await
        .expect("cross-key call MUST complete within bound")
        .expect("cross-key call MUST succeed — jam isolation");
        assert_eq!(other, ("other-hash".to_string(), None));

        blocked_task.abort();
        let _ = blocked_task.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn waiter_bounded_deadline_returns_structured_error() {
        // A waiter awaiting a permanently-hung fetcher must
        // return WaitDeadlineElapsed within the wait deadline,
        // never an infinite hang.
        let coalescer = cache_with_short_tunables();
        let never_release = Arc::new(tokio::sync::Notify::new());
        let rel_clone = Arc::clone(&never_release);

        let c = Arc::clone(&coalescer);
        let jam = tokio::spawn(async move {
            c.resolve_or_coalesce(
                "mpd-album",
                "Jam|Album",
                "medium",
                move || async move {
                    rel_clone.notified().await;
                    unreachable!("never released")
                },
            )
            .await
        });

        // Wait for the fetcher to install the inflight entry.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Second caller enters the sleeper arm. Under the
        // 400 ms wait deadline the caller must surface
        // WaitDeadlineElapsed within a generous outer ceiling.
        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            coalescer.resolve_or_coalesce(
                "mpd-album",
                "Jam|Album",
                "medium",
                || async { Ok(("SHOULD_NOT_RUN".to_string(), None)) },
            ),
        )
        .await
        .expect(
            "waiter MUST return within the outer ceiling — infinite-hang \
             regression",
        );

        match outcome {
            Err(CoalesceError::WaitDeadlineElapsed {
                scheme,
                value,
                size,
                waited_ms,
            }) => {
                assert_eq!(scheme, "mpd-album");
                assert_eq!(value, "Jam|Album");
                assert_eq!(size, "medium");
                assert!(
                    waited_ms >= 200,
                    "waited_ms {waited_ms} should reflect the wait deadline"
                );
            }
            other => panic!("expected WaitDeadlineElapsed, got {other:?}"),
        }

        jam.abort();
        let _ = jam.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancellation_of_fetcher_frees_the_slot() {
        // First caller enters as fetcher; its outer future is
        // dropped mid-flight; second caller for the same key
        // MUST complete via a fresh fetcher within bound.
        let coalescer = Arc::new(ArtworkResolveCoalescer::new());
        let never_release = Arc::new(tokio::sync::Notify::new());
        let rel_clone = Arc::clone(&never_release);

        let c = Arc::clone(&coalescer);
        let first = tokio::spawn(async move {
            c.resolve_or_coalesce(
                "mpd-album",
                "Cancel|Album",
                "medium",
                move || async move {
                    rel_clone.notified().await;
                    unreachable!()
                },
            )
            .await
        });

        // Wait for the inflight entry to be installed.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(coalescer.inflight_len(), 1);

        first.abort();
        let _ = first.await;
        tokio::task::yield_now().await;

        assert_eq!(
            coalescer.inflight_len(),
            0,
            "cancelled fetcher must free the inflight slot via RAII \
             guard drop"
        );

        // Second caller for the SAME key must complete with a
        // fresh fetcher.
        let second = tokio::time::timeout(
            Duration::from_secs(5),
            coalescer.resolve_or_coalesce(
                "mpd-album",
                "Cancel|Album",
                "medium",
                || async { Ok(("fresh-hash".to_string(), None)) },
            ),
        )
        .await
        .expect("second call must complete within bound")
        .expect("second call must succeed");
        assert_eq!(second, ("fresh-hash".to_string(), None));
    }

    #[tokio::test]
    async fn failure_memoized_under_same_ttl() {
        // A resolve failure (e.g. missing art) is memoized so
        // a burst against the same unresolvable key does not
        // re-hammer the provider.
        let coalescer = Arc::new(ArtworkResolveCoalescer::new());
        let call_count = Arc::new(AtomicUsize::new(0));

        for _ in 0..3 {
            let cc = Arc::clone(&call_count);
            let outcome = coalescer
                .resolve_or_coalesce(
                    "mpd-album",
                    "Missing|Album",
                    "medium",
                    move || async move {
                        cc.fetch_add(1, Ordering::SeqCst);
                        Err("not_found".to_string())
                    },
                )
                .await;
            match outcome {
                Err(CoalesceError::FetcherError { reason, .. }) => {
                    assert_eq!(reason, "not_found");
                }
                other => panic!("expected FetcherError, got {other:?}"),
            }
        }

        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "failure outcome MUST be memoized — 3 sequential same-key \
             calls invoke the fetcher exactly once"
        );
        assert_eq!(
            coalescer.memo_len(),
            1,
            "one memoized entry for the failing key"
        );
    }
}
