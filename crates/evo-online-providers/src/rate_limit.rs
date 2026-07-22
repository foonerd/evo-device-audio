// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Per-provider token bucket rate limiter.
//!
//! MusicBrainz's API policy is 1 request per second per client
//! (measured by IP + User-Agent). Two plugins hitting the API
//! independently would bust that under any browse burst — a
//! shared limiter, threaded through both callers, is the
//! discipline that keeps the distribution's outbound cadence
//! honest.
//!
//! This is a strict per-second limiter with capacity = 1: no
//! burst compensation, no leaky-bucket smoothing. A caller
//! waits until the previous token has been consumed and the
//! next second's token has arrived, then proceeds. Under N
//! concurrent callers, calls serialise; total wall-clock scales
//! linearly with N × (1 s / rate).
//!
//! ## Fairness
//!
//! The mutex is `tokio::sync::Mutex` (fair FIFO on newer tokio
//! releases). Concurrent callers are serviced in arrival order.
//!
//! ## Cancellation
//!
//! `acquire` is cancellation-safe: dropping the returned future
//! before it resolves releases nothing (no token was granted).
//! The rate limiter carries no per-caller state — a dropped
//! wait costs nothing and does not slow the next caller.

use std::time::{Duration, Instant};

use tokio::sync::Mutex;

/// Single-provider token bucket.
///
/// Constructed with a `min_interval` between grants. `acquire`
/// blocks until the interval has elapsed since the previous
/// grant, then records the current instant as the new "last
/// granted" time and returns.
///
/// The most common configuration for MusicBrainz is
/// `Duration::from_secs(1)`. Callers wrap every outbound
/// `musicbrainz.org` request in `.acquire().await` before
/// dispatching.
#[derive(Debug)]
pub struct RateLimiter {
    inner: Mutex<Inner>,
    min_interval: Duration,
}

#[derive(Debug)]
struct Inner {
    last_granted: Option<Instant>,
}

impl RateLimiter {
    /// Construct a new limiter with the given minimum gap
    /// between grants. `min_interval == 0` yields an
    /// unlimited (pass-through) limiter — useful in tests that
    /// exercise the wiring without spending real seconds.
    pub fn new(min_interval: Duration) -> Self {
        Self {
            inner: Mutex::new(Inner { last_granted: None }),
            min_interval,
        }
    }

    /// Await the next available slot, then return.
    ///
    /// Under contention, callers serialise; the wall-clock cost
    /// of N concurrent acquires is `(N - 1) * min_interval` +
    /// any overhead. On a cold limiter (no prior grant), the
    /// first caller returns immediately.
    pub async fn acquire(&self) {
        // Compute the deadline BEFORE releasing the lock so a
        // second caller waiting behind us can start their own
        // deadline calculation the moment we drop the guard.
        let mut inner = self.inner.lock().await;
        let now = Instant::now();
        if let Some(last) = inner.last_granted {
            let elapsed = now.saturating_duration_since(last);
            if elapsed < self.min_interval {
                let wait = self.min_interval - elapsed;
                tokio::time::sleep(wait).await;
            }
        }
        inner.last_granted = Some(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn zero_interval_never_blocks() {
        // Pass-through mode — no wall-clock cost even under
        // repeated acquires.
        let lim = RateLimiter::new(Duration::from_nanos(0));
        let started = Instant::now();
        for _ in 0..10 {
            lim.acquire().await;
        }
        assert!(started.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn interval_bounds_grant_cadence() {
        // 50ms interval → three acquires take ~100ms (first
        // one is immediate; second waits 50ms; third waits
        // another 50ms).
        let lim = RateLimiter::new(Duration::from_millis(50));
        let started = Instant::now();
        lim.acquire().await;
        lim.acquire().await;
        lim.acquire().await;
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(90),
            "three sequential acquires at 50ms interval must \
             take at least 90ms — got {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(200),
            "three sequential acquires at 50ms interval must \
             not exceed 200ms — got {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn concurrent_acquires_serialise() {
        // Two concurrent tasks under a 40ms interval must
        // complete in at least 40ms wall-clock — the second one
        // waits for the first's slot.
        let lim =
            std::sync::Arc::new(RateLimiter::new(Duration::from_millis(40)));
        let started = Instant::now();
        let l1 = std::sync::Arc::clone(&lim);
        let l2 = std::sync::Arc::clone(&lim);
        let h1 = tokio::spawn(async move { l1.acquire().await });
        let h2 = tokio::spawn(async move { l2.acquire().await });
        let _ = h1.await;
        let _ = h2.await;
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(35),
            "concurrent acquires must serialise — got {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn late_reuse_does_not_carry_credit() {
        // Cold limiter — acquire, wait 100ms (interval is
        // 50ms), acquire again. Second acquire should be
        // immediate (the interval has already elapsed).
        // Regression: an earlier draft added the elapsed
        // time to the grant deadline as "credit"; that broke
        // the strict-per-second contract for MB.
        let lim = RateLimiter::new(Duration::from_millis(50));
        lim.acquire().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let started = Instant::now();
        lim.acquire().await;
        assert!(
            started.elapsed() < Duration::from_millis(10),
            "second acquire after elapsed interval must be immediate"
        );
    }
}
