// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Priority-aware admission for the artwork resolve endpoint.
//!
//! Two buckets, one gate. Together they enforce two invariants:
//!
//! 1. **Browse cannot starve now-playing.** Now-playing artwork
//!    resolves (`mpd-path` scheme by contract with the playback
//!    subject emitter) hold a small reserved permit pool that
//!    browse-side calls cannot touch. A 3000-track browse burst
//!    that fully saturates the shared bucket still leaves the
//!    now-playing bucket open — the hero surface renders
//!    without waiting.
//! 2. **Runaway backpressure surfaces as a structured 503.** A
//!    saturated shared bucket returns
//!    [`AdmissionError::DeadlineElapsed`] within the configured
//!    deadline, never blocks indefinitely. The endpoint
//!    translates the error to `503 Service Unavailable` with
//!    `Retry-After` — the operator UI retries without
//!    penalising other artwork traffic.
//!
//! ## Buckets + acquisition semantics
//!
//! - **Now-playing bucket** ([`DEFAULT_NOW_PLAYING_PERMITS`],
//!   currently 4): reserved for callers whose scheme signals
//!   now-playing intent. Today the contract is `mpd-path` →
//!   now-playing (per the playback subject emitter). This
//!   maps future schemes (`airplay-now-playing`,
//!   `stream-now-playing`) via [`is_now_playing_scheme`].
//! - **Shared / browse bucket**
//!   ([`DEFAULT_BROWSE_PERMITS`], currently 24): any scheme.
//!   Now-playing schemes fall back to this bucket when the
//!   reserved pool is exhausted — a burst of concurrent
//!   now-playing changes still gets through, just without
//!   priority.
//!
//! Acquisition order for a now-playing scheme:
//!   1. Try_acquire the now-playing bucket (non-blocking); if
//!      a permit is immediately available, return it.
//!   2. Otherwise, acquire the browse bucket with the
//!      configured deadline.
//!
//! Acquisition order for a browse (or unknown) scheme:
//!   1. Acquire the browse bucket with the configured
//!      deadline.
//!
//! ## Positioning
//!
//! Admission is applied INSIDE the resolve endpoint's fetcher
//! closure (the closure passed to
//! [`crate::artwork_resolve_coalescer::ArtworkResolveCoalescer::resolve_or_coalesce`]).
//! Same-key waiters entering the coalescer's sleeper arm do
//! NOT hit admission — they share the fetcher's permit
//! outcome. This keeps the effective concurrent-dispatch cap
//! at `min(N distinct keys, permits available)` rather than
//! `min(N callers, permits available)` — the latter would
//! starve on any N-parallel same-key burst.
//!
//! ## Deadlines
//!
//! The admission deadline is intentionally shorter than the
//! coalescer's wait deadline
//! ([`crate::artwork_resolve_coalescer::INFLIGHT_WAIT_DEADLINE`],
//! 30 s). Under saturation the fetcher fails at the admission
//! deadline; the coalescer publishes the error; waiters see
//! the failure and either retry (503-driven UI retry) or fall
//! into their own admission attempt.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Reserved permits for now-playing artwork resolves. Small
/// enough that browse cannot starve the hero surface; large
/// enough that a rapid succession of now-playing changes (a
/// user skipping through tracks) does not queue.
pub const DEFAULT_NOW_PLAYING_PERMITS: usize = 4;

/// Shared / browse permits — the pool every non-now-playing
/// scheme uses, and the fallback pool for now-playing schemes
/// once the reserved bucket is exhausted. 24 is generous
/// enough for a browse-scale burst against Layer A's
/// mpd-album collapsed keys (most libraries have < 24
/// distinct albums on a browse page) while still enforcing
/// an upstream cap so a runaway concurrent burst does not
/// spawn N plugin dispatches simultaneously.
pub const DEFAULT_BROWSE_PERMITS: usize = 24;

/// Default admission-acquisition deadline. Callers whose
/// bucket is saturated for this long surface as `503
/// Service Unavailable` — well short of the coalescer's
/// [`crate::artwork_resolve_coalescer::INFLIGHT_WAIT_DEADLINE`]
/// so the deadline surfaces at the admission layer, where
/// the operator UI can retry with structured Retry-After
/// backoff.
pub const DEFAULT_ADMISSION_DEADLINE: Duration = Duration::from_secs(5);

/// Structured admission error. Discriminated so the endpoint
/// can translate cleanly to HTTP + observability metrics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionError {
    /// The caller's bucket was saturated for the configured
    /// deadline. The endpoint surfaces this as 503 + Retry-
    /// After; observability records the scheme + bucket +
    /// wait time for tuning.
    DeadlineElapsed {
        /// Scheme the caller supplied (canonical or otherwise).
        scheme: String,
        /// Which bucket the caller ultimately waited on
        /// (`"now-playing"` or `"browse"`).
        bucket: &'static str,
        /// Milliseconds the caller blocked before giving up.
        waited_ms: u64,
    },
    /// Semaphore was closed (steward tearing down). Rare;
    /// surfaces as 503 without Retry-After.
    ClosedRuntime,
}

impl std::fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DeadlineElapsed {
                scheme,
                bucket,
                waited_ms,
            } => write!(
                f,
                "artwork admission deadline elapsed on {bucket} bucket for \
                 scheme={scheme} after {waited_ms} ms — retry"
            ),
            Self::ClosedRuntime => {
                write!(f, "artwork admission runtime closed")
            }
        }
    }
}

impl std::error::Error for AdmissionError {}

/// RAII wrapper around an acquired semaphore permit.
///
/// Held for the duration of the plugin dispatch inside the
/// coalescer's fetcher closure. On drop the permit returns to
/// its bucket. The endpoint never inspects the permit — it
/// exists only to keep the count honest.
#[derive(Debug)]
pub struct AdmissionGuard {
    _permit: OwnedSemaphorePermit,
}

/// Two-bucket admission gate. Cheap to construct via
/// [`Admission::new()`]; wrap in `Arc` for sharing across the
/// endpoint's request-handler tasks.
#[derive(Debug)]
pub struct Admission {
    now_playing: Arc<Semaphore>,
    browse: Arc<Semaphore>,
    deadline: Duration,
}

impl Default for Admission {
    fn default() -> Self {
        Self::new()
    }
}

impl Admission {
    /// Construct with the module defaults (4 now-playing, 24
    /// browse, 5 s deadline).
    pub fn new() -> Self {
        Self::with_tunables(
            DEFAULT_NOW_PLAYING_PERMITS,
            DEFAULT_BROWSE_PERMITS,
            DEFAULT_ADMISSION_DEADLINE,
        )
    }

    /// Construct with explicit tunables. Primarily for tests
    /// exercising saturation without waiting production
    /// deadlines.
    pub fn with_tunables(
        now_playing_permits: usize,
        browse_permits: usize,
        deadline: Duration,
    ) -> Self {
        Self {
            now_playing: Arc::new(Semaphore::new(now_playing_permits)),
            browse: Arc::new(Semaphore::new(browse_permits)),
            deadline,
        }
    }

    /// Acquire an admission permit for the supplied scheme.
    ///
    /// Now-playing schemes try the reserved pool first
    /// (non-blocking); on failure, fall back to the browse
    /// pool with the configured deadline. Non-now-playing
    /// schemes go directly to the browse pool with the
    /// deadline.
    ///
    /// The returned guard must be held for the duration of
    /// the plugin dispatch; drop releases the permit back to
    /// its bucket.
    pub async fn admit(
        &self,
        scheme: &str,
    ) -> Result<AdmissionGuard, AdmissionError> {
        let started = Instant::now();
        if is_now_playing_scheme(scheme) {
            // Try the reserved bucket first, non-blocking.
            if let Ok(permit) =
                Arc::clone(&self.now_playing).try_acquire_owned()
            {
                return Ok(AdmissionGuard { _permit: permit });
            }
            // Fall back to shared bucket with deadline.
            return self.acquire_browse(scheme, started).await;
        }
        // Non-now-playing goes directly to the browse bucket.
        self.acquire_browse(scheme, started).await
    }

    async fn acquire_browse(
        &self,
        scheme: &str,
        started: Instant,
    ) -> Result<AdmissionGuard, AdmissionError> {
        let acquire = Arc::clone(&self.browse).acquire_owned();
        match tokio::time::timeout(self.deadline, acquire).await {
            Ok(Ok(permit)) => Ok(AdmissionGuard { _permit: permit }),
            Ok(Err(_closed)) => Err(AdmissionError::ClosedRuntime),
            Err(_) => Err(AdmissionError::DeadlineElapsed {
                scheme: scheme.to_string(),
                bucket: "browse",
                waited_ms: started.elapsed().as_millis() as u64,
            }),
        }
    }

    /// Test-only introspection: available permits on the
    /// now-playing bucket.
    #[cfg(test)]
    pub(crate) fn now_playing_available(&self) -> usize {
        self.now_playing.available_permits()
    }

    /// Test-only introspection: available permits on the
    /// browse bucket.
    #[cfg(test)]
    pub(crate) fn browse_available(&self) -> usize {
        self.browse.available_permits()
    }
}

/// Returns true when the scheme signals now-playing intent.
///
/// Today the playback subject emitter emits `mpd-path` for
/// the now-playing artwork URL (per-track fidelity for the
/// hero surface); every other emitter (library, queue,
/// favourites, playlists) uses `mpd-album` via the
/// stable-cover-identity helper. Future now-playing schemes
/// (`airplay-now-playing`, `stream-now-playing`) extend the
/// match here without touching the acquisition logic.
pub fn is_now_playing_scheme(scheme: &str) -> bool {
    matches!(scheme, "mpd-path")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn now_playing_uses_reserved_bucket_first() {
        let a = Admission::new();
        let g = a.admit("mpd-path").await.unwrap();
        assert_eq!(
            a.now_playing_available(),
            DEFAULT_NOW_PLAYING_PERMITS - 1,
            "now-playing scheme MUST consume from the reserved bucket \
             first"
        );
        assert_eq!(
            a.browse_available(),
            DEFAULT_BROWSE_PERMITS,
            "browse bucket must be untouched"
        );
        drop(g);
    }

    #[tokio::test]
    async fn browse_scheme_goes_to_browse_bucket() {
        let a = Admission::new();
        let g = a.admit("mpd-album").await.unwrap();
        assert_eq!(
            a.now_playing_available(),
            DEFAULT_NOW_PLAYING_PERMITS,
            "browse scheme MUST NOT touch the now-playing reserved bucket"
        );
        assert_eq!(a.browse_available(), DEFAULT_BROWSE_PERMITS - 1);
        drop(g);
    }

    #[tokio::test]
    async fn saturated_browse_returns_structured_deadline_error() {
        // Sub-second deadline so the test can prove the
        // structured error surfaces bounded.
        let a = Arc::new(Admission::with_tunables(
            1,
            1,
            Duration::from_millis(200),
        ));
        // Consume the only browse permit; hold it.
        let held = a.admit("mpd-album").await.unwrap();
        // Second browse call MUST return DeadlineElapsed
        // within the deadline.
        let started = Instant::now();
        let err = a.admit("mpd-album").await.unwrap_err();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "deadline error must fire near 200 ms, not near hang; got {elapsed:?}"
        );
        match err {
            AdmissionError::DeadlineElapsed {
                scheme,
                bucket,
                waited_ms,
            } => {
                assert_eq!(scheme, "mpd-album");
                assert_eq!(bucket, "browse");
                assert!(
                    waited_ms >= 100,
                    "waited_ms {waited_ms} must reflect the deadline"
                );
            }
            other => panic!("expected DeadlineElapsed, got {other:?}"),
        }
        drop(held);
    }

    #[tokio::test]
    async fn now_playing_falls_back_to_browse_when_reserved_saturated() {
        // 1 reserved + 1 shared; consume reserved.
        let a = Arc::new(Admission::with_tunables(
            1,
            1,
            Duration::from_millis(200),
        ));
        let reserved = a.admit("mpd-path").await.unwrap();
        assert_eq!(a.now_playing_available(), 0);
        assert_eq!(a.browse_available(), 1);
        // Next now-playing acquires from the shared bucket
        // (fallback path).
        let shared = a.admit("mpd-path").await.unwrap();
        assert_eq!(a.now_playing_available(), 0);
        assert_eq!(a.browse_available(), 0);
        drop(reserved);
        drop(shared);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn browse_burst_does_not_starve_now_playing_reserved() {
        // Named regression from the memo: browse burst MUST
        // NOT starve now-playing. Saturate the browse bucket
        // fully; the reserved bucket MUST remain servable.
        let a =
            Arc::new(Admission::with_tunables(4, 4, Duration::from_secs(2)));
        let mut held = Vec::new();
        for _ in 0..4 {
            held.push(a.admit("mpd-album").await.unwrap());
        }
        assert_eq!(a.browse_available(), 0);
        // Now-playing MUST still be servable within a tight
        // ceiling — well under the deadline.
        let started = Instant::now();
        let g = tokio::time::timeout(
            Duration::from_millis(200),
            a.admit("mpd-path"),
        )
        .await
        .expect("now-playing MUST succeed even under browse saturation")
        .expect("now-playing MUST NOT error");
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "now-playing under a saturated browse must acquire near-instantly \
             via the reserved bucket; took {:?}",
            started.elapsed()
        );
        drop(g);
        for h in held {
            drop(h);
        }
    }

    #[tokio::test]
    async fn permit_release_on_drop_restores_bucket() {
        let a = Admission::new();
        assert_eq!(a.browse_available(), DEFAULT_BROWSE_PERMITS);
        {
            let _g = a.admit("mpd-album").await.unwrap();
            assert_eq!(a.browse_available(), DEFAULT_BROWSE_PERMITS - 1);
        }
        // Give the semaphore a scheduler yield to reflect
        // the drop.
        tokio::task::yield_now().await;
        assert_eq!(
            a.browse_available(),
            DEFAULT_BROWSE_PERMITS,
            "dropped guard must return its permit to the bucket"
        );
    }

    #[tokio::test]
    async fn scheme_classifier_recognises_mpd_path_as_now_playing() {
        assert!(is_now_playing_scheme("mpd-path"));
        assert!(!is_now_playing_scheme("mpd-album"));
        assert!(!is_now_playing_scheme(""));
        assert!(!is_now_playing_scheme("unknown"));
    }
}
