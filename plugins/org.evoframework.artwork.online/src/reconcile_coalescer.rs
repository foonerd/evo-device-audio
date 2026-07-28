// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Single-flight coalescer for the artist-artwork cascade.
//!
//! Under browse fan-out (~40 visible artist tiles), the UI can
//! fire the resolve verb concurrently for many distinct
//! artists — and, more damagingly, MANY CALLS for the SAME
//! artist when a fold-key collision or a duplicate mount
//! surfaces the same tile twice. MusicBrainz enforces a
//! 1 req/sec ceiling per client; without coalescing, N
//! concurrent same-fold callers each queue behind the shared
//! MB rate limiter and serialise into an N-second wall-clock
//! wave, plus each fires its own provider fan-out.
//!
//! This coalescer sits between [`query_artist_artwork`] and
//! the actual cascade work. Keyed on the fold-key produced by
//! [`evo_device_audio_shared::artist_name::artist_fold_key`],
//! it ensures the cascade runs AT MOST ONCE per key at any
//! moment; every concurrent caller subscribes to the in-flight
//! computation and receives the same outcome. When the first
//! caller finishes, the outcome broadcasts to every waiter and
//! the coalescer entry drops.
//!
//! ## Discipline
//!
//! Coalescing is orthogonal to caching. The cache in
//! `artwork_caches` memoises Found/Absent under TTL; the
//! coalescer memoises the in-flight future across concurrent
//! callers within one call cycle. Together:
//!
//! 1. A warm cache hit returns instantly (no coalesce needed).
//! 2. A cold cache miss with one caller runs the cascade
//!    once, caches per policy, returns.
//! 3. A cold cache miss with N concurrent callers coalesces
//!    to one cascade run; the shared outcome then writes to
//!    the cache per policy, so subsequent calls hit warm.
//! 4. Cancellation-safe: if the first caller's future is
//!    dropped mid-flight, the in-flight guard removes the
//!    entry so the next caller becomes the fresh fetcher.
//!
//! ## Cancellation safety (RAII)
//!
//! The `InflightGuard` removes the coalescer entry on drop —
//! whether the fetcher completed, panicked, or was cancelled
//! by the executor. Without this, a cancelled fetcher would
//! leave a broadcast sender with no publisher, and waiters
//! would either block indefinitely or receive a `RecvError`.
//! The guard's drop combined with the waiters' bounded
//! deadline turns cancellation into "next caller becomes the
//! fresh fetcher," not deadlock.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::broadcast;

/// Bounded deadline a waiter blocks for its in-flight
/// fetcher's outcome. Longer than MB's 1 req/sec ceiling +
/// a full provider fan-out under normal conditions; short
/// enough that a wedged fetcher does not stall the browse
/// forever. On elapse the waiter falls through to a fresh
/// cascade run (which then coalesces the next wave).
pub(crate) const INFLIGHT_WAIT_DEADLINE: Duration = Duration::from_secs(30);

/// Single-flight coalescer for the artist-artwork cascade.
///
/// Keyed on fold-key. `Arc`-safe: one instance lives on the
/// plugin state and every request handler clones a reference.
pub(crate) struct ReconcileCoalescer<Out: Clone + Send + 'static> {
    inflight: Arc<Mutex<HashMap<String, broadcast::Sender<Out>>>>,
}

impl<Out: Clone + Send + 'static> ReconcileCoalescer<Out> {
    pub(crate) fn new() -> Self {
        Self {
            inflight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Run `fetcher` under the coalescer.
    ///
    /// - If no other caller is in flight for `key`, this call
    ///   becomes the fetcher: runs `fetcher()`, publishes the
    ///   outcome to any subscribers, drops the RAII guard,
    ///   returns the outcome.
    /// - If another caller is in flight, this call subscribes
    ///   to that fetcher's broadcast and awaits the outcome
    ///   (with [`INFLIGHT_WAIT_DEADLINE`]). If the wait
    ///   elapses or the sender closes without publishing (the
    ///   fetcher was cancelled), the caller retries by
    ///   becoming the fresh fetcher.
    pub(crate) async fn run<F, Fut>(&self, key: String, fetcher: F) -> Out
    where
        F: FnOnce() -> Fut + Send,
        Fut: std::future::Future<Output = Out> + Send,
    {
        // Fast path — is a fetcher in flight?
        let existing = {
            let inflight =
                self.inflight.lock().expect("inflight lock poisoned");
            inflight.get(&key).map(broadcast::Sender::subscribe)
        };
        if let Some(mut rx) = existing {
            match tokio::time::timeout(INFLIGHT_WAIT_DEADLINE, rx.recv()).await
            {
                Ok(Ok(outcome)) => return outcome,
                _ => {
                    // Sender dropped without publishing (fetcher
                    // cancelled), or the deadline elapsed.
                    // Re-enter — become the fresh fetcher OR
                    // subscribe to a new inflight entry that
                    // appeared meanwhile.
                    return Box::pin(self.run(key, fetcher)).await;
                }
            }
        }

        // Slow path — install ourselves as the fetcher.
        let (tx, _rx) = broadcast::channel(16);
        // Race guard: another caller may have installed a
        // sender in the window between the fast-path check
        // and here. Check-and-insert under the lock; if we
        // find an existing sender, subscribe outside the
        // lock so we never hold the MutexGuard across an
        // await point (Send-bound requirement for
        // tokio::spawn'd futures).
        let race_rx = {
            let mut inflight =
                self.inflight.lock().expect("inflight lock poisoned");
            if let Some(existing_tx) = inflight.get(&key) {
                Some(existing_tx.subscribe())
            } else {
                inflight.insert(key.clone(), tx.clone());
                None
            }
        };
        if let Some(mut rx) = race_rx {
            return match tokio::time::timeout(INFLIGHT_WAIT_DEADLINE, rx.recv())
                .await
            {
                Ok(Ok(outcome)) => outcome,
                _ => Box::pin(self.run(key, fetcher)).await,
            };
        }

        // RAII guard: whatever happens to the fetcher's
        // future (completion, panic, cancellation), the
        // coalescer entry is removed on drop.
        let _guard = InflightGuard {
            inflight: Arc::clone(&self.inflight),
            key: key.clone(),
        };

        let outcome = fetcher().await;
        // Broadcast to every waiter. Send errors mean no
        // subscribers, which is fine — the fetcher's own
        // return path carries the outcome.
        let _ = tx.send(outcome.clone());
        outcome
    }
}

impl<Out: Clone + Send + 'static> Default for ReconcileCoalescer<Out> {
    fn default() -> Self {
        Self::new()
    }
}

struct InflightGuard<Out: Clone + Send + 'static> {
    inflight: Arc<Mutex<HashMap<String, broadcast::Sender<Out>>>>,
    key: String,
}

impl<Out: Clone + Send + 'static> Drop for InflightGuard<Out> {
    fn drop(&mut self) {
        if let Ok(mut m) = self.inflight.lock() {
            m.remove(&self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn coalesces_concurrent_same_key_to_one_fetcher() {
        let c: Arc<ReconcileCoalescer<String>> =
            Arc::new(ReconcileCoalescer::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let c = Arc::clone(&c);
            let calls = Arc::clone(&calls);
            handles.push(tokio::spawn(async move {
                c.run("abba".into(), || {
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        "abba-outcome".to_string()
                    }
                })
                .await
            }));
        }
        for h in handles {
            assert_eq!(h.await.unwrap(), "abba-outcome");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn distinct_keys_do_not_coalesce() {
        let c: Arc<ReconcileCoalescer<String>> =
            Arc::new(ReconcileCoalescer::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let c1 = Arc::clone(&c);
        let calls1 = Arc::clone(&calls);
        let h1 = tokio::spawn(async move {
            c1.run("abba".into(), || {
                let calls = Arc::clone(&calls1);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    "abba".to_string()
                }
            })
            .await
        });
        let c2 = Arc::clone(&c);
        let calls2 = Arc::clone(&calls);
        let h2 = tokio::spawn(async move {
            c2.run("adele".into(), || {
                let calls = Arc::clone(&calls2);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    "adele".to_string()
                }
            })
            .await
        });
        assert_eq!(h1.await.unwrap(), "abba");
        assert_eq!(h2.await.unwrap(), "adele");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn subsequent_call_after_completion_re_runs() {
        // The coalescer is single-flight, not caching. After
        // an outcome is published, the entry is removed; the
        // next call for the same key becomes a fresh fetcher.
        let c: ReconcileCoalescer<String> = ReconcileCoalescer::new();
        let n1 = c.run("k".into(), || async { "first".to_string() }).await;
        let n2 = c.run("k".into(), || async { "second".to_string() }).await;
        assert_eq!(n1, "first");
        assert_eq!(n2, "second");
    }
}
