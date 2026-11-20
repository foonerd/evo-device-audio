// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Background artwork enrichment.
//!
//! A paint that misses the local tier is answered at once and
//! asks, here, for the network tier to be tried later. Nothing in
//! this module is on the operator's clock.
//!
//! - **Bounded.** At most [`EnrichmentTunables::capacity`] targets
//!   are queued or running. A request beyond that is refused
//!   ([`EnrichRequest::Full`]) and the paint is unaffected; the
//!   target is asked for again by a later paint once its pending
//!   memo lapses.
//! - **Coalesced.** A target already queued or running is not
//!   queued twice, however many tiles ask for it.
//! - **Off the paint clock.** Jobs run on their own tasks through
//!   [`ArtworkCascade::enrich`]; concurrency is capped separately
//!   so a large queue cannot flood the providers.
//! - **Retried with backoff** on a transient outcome (upstream
//!   unavailable, admission or coalescer deadline). Retrying
//!   writes nothing: until a job settles, the pending memo stands.
//!   When the schedule is exhausted the target is memoised
//!   transient for the existing five-minute window, so a revisit
//!   does not restart the cycle.
//! - **Announced.** A landed positive is announced by the cascade
//!   itself (`artwork_resolved`); this module adds no second
//!   signal.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use evo_runtime_http::principal::Principal;
use tokio::sync::{mpsc, Semaphore};

use crate::artwork_cascade::{ArtworkCascade, CascadeOutcome};

/// `(scheme, value, size)` — the same key every artwork cache uses.
pub type EnrichKey = (String, String, String);

/// Provenance on a 404 whose enrichment is queued or running.
pub const PROVENANCE_BACKGROUND_ENRICHING: &str = "background_enriching";

/// What a request for enrichment came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnrichRequest {
    /// Newly queued.
    Queued,
    /// Already queued or running; nothing added.
    Coalesced,
    /// Refused: the bound is reached.
    Full,
}

/// Where a paint miss sends its request for enrichment.
pub trait EnrichmentSink: Send + Sync {
    /// Ask for the full cascade to be run for `key`, off the paint.
    fn request(&self, principal: &Principal, key: &EnrichKey) -> EnrichRequest;
}

/// Knobs. Defaults suit a single-room device.
#[derive(Debug, Clone)]
pub struct EnrichmentTunables {
    /// Most targets queued or running at once.
    pub capacity: usize,
    /// Most jobs inside the cascade at once.
    pub concurrency: usize,
    /// Wait before each retry. The number of entries is the
    /// number of retries; the first attempt is immediate.
    pub backoff: Vec<Duration>,
}

impl Default for EnrichmentTunables {
    fn default() -> Self {
        Self {
            capacity: 512,
            concurrency: 2,
            backoff: vec![
                Duration::from_secs(5),
                Duration::from_secs(30),
                Duration::from_secs(120),
            ],
        }
    }
}

struct Job {
    principal: Principal,
    key: EnrichKey,
}

/// The sink half: held by the cascade.
pub struct EnrichmentQueue {
    tx: mpsc::Sender<Job>,
    inflight: Arc<Mutex<HashSet<EnrichKey>>>,
    capacity: usize,
}

impl EnrichmentQueue {
    /// Whether `key` is queued or running.
    pub fn is_pending(&self, key: &EnrichKey) -> bool {
        lock(&self.inflight).contains(key)
    }

    /// Targets queued or running.
    pub fn pending_len(&self) -> usize {
        lock(&self.inflight).len()
    }
}

impl EnrichmentSink for EnrichmentQueue {
    fn request(&self, principal: &Principal, key: &EnrichKey) -> EnrichRequest {
        let mut set = lock(&self.inflight);
        if set.contains(key) {
            return EnrichRequest::Coalesced;
        }
        if set.len() >= self.capacity {
            return EnrichRequest::Full;
        }
        set.insert(key.clone());
        let job = Job {
            principal: principal.clone(),
            key: key.clone(),
        };
        if self.tx.try_send(job).is_err() {
            set.remove(key);
            return EnrichRequest::Full;
        }
        EnrichRequest::Queued
    }
}

/// The runner half: owns the receiver until [`Self::spawn`].
pub struct EnrichmentRunner {
    rx: mpsc::Receiver<Job>,
    inflight: Arc<Mutex<HashSet<EnrichKey>>>,
    tunables: EnrichmentTunables,
}

/// Build the two halves. The cascade takes the queue; the runner
/// is started against that cascade afterwards, which is why the
/// two are separate: the worker needs the cascade and the cascade
/// needs somewhere to send.
pub fn channel(
    tunables: EnrichmentTunables,
) -> (Arc<EnrichmentQueue>, EnrichmentRunner) {
    let capacity = tunables.capacity.max(1);
    let (tx, rx) = mpsc::channel(capacity);
    let inflight = Arc::new(Mutex::new(HashSet::new()));
    (
        Arc::new(EnrichmentQueue {
            tx,
            inflight: Arc::clone(&inflight),
            capacity,
        }),
        EnrichmentRunner {
            rx,
            inflight,
            tunables,
        },
    )
}

impl EnrichmentRunner {
    /// Start the worker. It holds the cascade weakly and ends when
    /// the cascade is dropped.
    pub fn spawn(
        mut self,
        cascade: Weak<ArtworkCascade>,
    ) -> tokio::task::JoinHandle<()> {
        let gate = Arc::new(Semaphore::new(self.tunables.concurrency.max(1)));
        let backoff = Arc::new(self.tunables.backoff.clone());
        tokio::spawn(async move {
            while let Some(job) = self.rx.recv().await {
                let cascade = cascade.clone();
                let gate = Arc::clone(&gate);
                let backoff = Arc::clone(&backoff);
                let inflight = Arc::clone(&self.inflight);
                tokio::spawn(async move {
                    run_job(&cascade, &gate, &backoff, &job).await;
                    lock(&inflight).remove(&job.key);
                });
            }
        })
    }
}

async fn run_job(
    cascade: &Weak<ArtworkCascade>,
    gate: &Semaphore,
    backoff: &[Duration],
    job: &Job,
) {
    let (scheme, value, size) = &job.key;
    let mut attempt = 0usize;
    loop {
        let Some(c) = cascade.upgrade() else {
            return;
        };
        let outcome = {
            // The slot is held for the attempt only, never across
            // a backoff wait: a target sleeping out an outage must
            // not starve the others.
            let Ok(_slot) = gate.acquire().await else {
                return;
            };
            c.enrich(&job.principal, scheme, value, size).await
        };
        let detail = match &outcome {
            CascadeOutcome::Resolved { .. } => {
                tracing::info!(
                    scheme = %scheme, value = %value, size = %size,
                    attempt, "artwork enrichment (background): resolved"
                );
                return;
            }
            CascadeOutcome::NotFound { .. }
            | CascadeOutcome::BadRequest { .. } => {
                tracing::info!(
                    scheme = %scheme, value = %value, size = %size,
                    attempt, "artwork enrichment (background): settled \
                     without art"
                );
                return;
            }
            CascadeOutcome::Transient { detail }
            | CascadeOutcome::AdmissionDeadline { detail }
            | CascadeOutcome::CoalescerDeadline { detail } => detail.clone(),
        };
        let Some(wait) = backoff.get(attempt).copied() else {
            tracing::warn!(
                scheme = %scheme, value = %value, size = %size,
                attempts = attempt + 1, detail = %detail,
                "artwork enrichment (background): gave up; memoised \
                 transient"
            );
            c.memoise_enrichment_gave_up(scheme, value, size, &detail);
            return;
        };
        tracing::info!(
            scheme = %scheme, value = %value, size = %size, attempt,
            wait_ms = wait.as_millis() as u64,
            "artwork enrichment (background): transient, backing off"
        );
        drop(c);
        tokio::time::sleep(wait).await;
        attempt += 1;
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_projection_core::WireOpId;
    use evo_runtime_http::dispatcher::{DispatchError, Dispatcher};
    use serde_json::{json, Value};
    use std::collections::VecDeque;
    use tokio::sync::Notify;

    const HASH: &str =
        "5555555555555555555555555555555555555555555555555555555555555555";

    /// Local always misses with an identity. Online answers from a
    /// script (the last entry repeats) and can be held on a gate.
    struct Script {
        online: Mutex<VecDeque<Value>>,
        online_calls: Mutex<usize>,
        gate: Option<Arc<Notify>>,
    }

    impl Script {
        fn new(online: Vec<Value>, gate: Option<Arc<Notify>>) -> Arc<Self> {
            Arc::new(Self {
                online: Mutex::new(online.into()),
                online_calls: Mutex::new(0),
                gate,
            })
        }
        fn online_calls(&self) -> usize {
            *lock(&self.online_calls)
        }
    }

    #[async_trait::async_trait]
    impl Dispatcher for Script {
        async fn dispatch(
            &self,
            _op: &WireOpId,
            payload: Value,
            _p: &Principal,
        ) -> Result<Value, DispatchError> {
            use base64::Engine;
            let rt = payload["request_type"].as_str().unwrap();
            let body = if crate::artwork_tier::is_network_tier(rt) {
                *lock(&self.online_calls) += 1;
                if let Some(g) = &self.gate {
                    g.notified().await;
                }
                let mut q = lock(&self.online);
                if q.len() > 1 {
                    q.pop_front().unwrap()
                } else {
                    q.front().cloned().unwrap()
                }
            } else {
                json!({
                    "status": "not_found",
                    "provider_id": "local_sidecar",
                    "identity": {"artist": "Adele", "album": "21"},
                })
            };
            Ok(json!({
                "payload_b64": base64::engine::general_purpose::STANDARD
                    .encode(body.to_string().as_bytes()),
            }))
        }
    }

    fn hit() -> Value {
        json!({"status": "ok", "content_hash": HASH, "provider_id": "itunes"})
    }
    fn down() -> Value {
        json!({"status": "unavailable", "detail": "503"})
    }
    fn principal() -> Principal {
        Principal::new("t", evo_auth_bearer::CapabilitySet::default())
    }
    fn key(n: &str) -> EnrichKey {
        ("mpd-path".into(), n.into(), "small".into())
    }
    fn fast(capacity: usize, retries: usize) -> EnrichmentTunables {
        EnrichmentTunables {
            capacity,
            concurrency: 2,
            backoff: vec![Duration::from_millis(5); retries],
        }
    }

    async fn settled(q: &EnrichmentQueue) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while q.pending_len() != 0 {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("enrichment did not settle");
    }

    fn wired(
        d: Arc<Script>,
        t: EnrichmentTunables,
    ) -> (Arc<ArtworkCascade>, Arc<EnrichmentQueue>, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let index =
            Arc::new(crate::ArtworkResolveIndex::new(dir.path().to_path_buf()));
        let (queue, runner) = channel(t);
        let cascade = ArtworkCascade::new(d, None, Some(index))
            .with_enrichment(Arc::clone(&queue));
        runner.spawn(Arc::downgrade(&cascade));
        (cascade, queue, dir)
    }

    /// Same target asked for repeatedly is one job.
    #[tokio::test]
    async fn a_target_is_never_queued_twice() {
        let (queue, _runner) = channel(fast(8, 0));
        let p = principal();
        assert_eq!(queue.request(&p, &key("a")), EnrichRequest::Queued);
        for _ in 0..4 {
            assert_eq!(queue.request(&p, &key("a")), EnrichRequest::Coalesced);
        }
        assert_eq!(queue.pending_len(), 1);
    }

    /// The queue is bounded; overflow is refused, not buffered.
    #[tokio::test]
    async fn the_queue_is_bounded() {
        let (queue, _runner) = channel(fast(2, 0));
        let p = principal();
        assert_eq!(queue.request(&p, &key("a")), EnrichRequest::Queued);
        assert_eq!(queue.request(&p, &key("b")), EnrichRequest::Queued);
        assert_eq!(queue.request(&p, &key("c")), EnrichRequest::Full);
        assert_eq!(queue.pending_len(), 2);
    }

    /// The paint answers before the network tier does, says why,
    /// and the art lands afterwards — announced, and served from
    /// the index on the next paint. RED if the paint awaits the
    /// job.
    #[tokio::test]
    async fn the_paint_returns_before_the_job_and_the_art_lands_after() {
        let gate = Arc::new(Notify::new());
        let d = Script::new(vec![hit()], Some(Arc::clone(&gate)));
        let dir = tempfile::TempDir::new().unwrap();
        let index =
            Arc::new(crate::ArtworkResolveIndex::new(dir.path().to_path_buf()));
        let (queue, runner) = channel(fast(8, 0));
        let cascade = ArtworkCascade::new(d.clone(), None, Some(index))
            .with_enrichment(Arc::clone(&queue));
        runner.spawn(Arc::downgrade(&cascade));
        let p = principal();

        let first = tokio::time::timeout(
            Duration::from_secs(1),
            cascade.resolve(&p, "mpd-path", "a/b.flac", "small"),
        )
        .await
        .expect("the paint waited on the network tier");
        match first {
            CascadeOutcome::NotFound { provider_id, .. } => assert_eq!(
                provider_id.as_deref(),
                Some(PROVENANCE_BACKGROUND_ENRICHING)
            ),
            other => panic!("{other:?}"),
        }
        // The job is in flight and held on the gate.
        assert_eq!(queue.pending_len(), 1);
        // A revisit meanwhile coalesces and still answers fast.
        let again = cascade.resolve(&p, "mpd-path", "a/b.flac", "small").await;
        assert!(matches!(again, CascadeOutcome::NotFound { .. }));
        assert_eq!(queue.pending_len(), 1);

        gate.notify_one();
        settled(&queue).await;
        assert_eq!(d.online_calls(), 1, "one job for the whole burst");
        match cascade.resolve(&p, "mpd-path", "a/b.flac", "small").await {
            CascadeOutcome::Resolved { content_hash, .. } => {
                assert_eq!(content_hash, HASH)
            }
            other => panic!("art did not land: {other:?}"),
        }
    }

    /// A transient failure is retried, and the art lands when the
    /// upstream recovers. Retrying writes no cache.
    #[tokio::test]
    async fn a_transient_failure_is_retried_until_it_lands() {
        let d = Script::new(vec![down(), down(), hit()], None);
        let (cascade, queue, _dir) = wired(d.clone(), fast(8, 3));
        let p = principal();
        cascade.resolve(&p, "mpd-path", "a/b.flac", "small").await;
        settled(&queue).await;
        assert_eq!(d.online_calls(), 3);
        assert!(matches!(
            cascade.resolve(&p, "mpd-path", "a/b.flac", "small").await,
            CascadeOutcome::Resolved { .. }
        ));
    }

    /// When the schedule runs out the target is memoised transient:
    /// a revisit answers from memory and does not restart the
    /// cycle against a failing upstream.
    #[tokio::test]
    async fn an_exhausted_schedule_is_memoised_not_restarted() {
        let d = Script::new(vec![down()], None);
        let (cascade, queue, _dir) = wired(d.clone(), fast(8, 2));
        let p = principal();
        cascade.resolve(&p, "mpd-path", "a/b.flac", "small").await;
        settled(&queue).await;
        assert_eq!(d.online_calls(), 3, "first attempt plus two retries");
        let again = cascade.resolve(&p, "mpd-path", "a/b.flac", "small").await;
        assert!(
            matches!(again, CascadeOutcome::Transient { .. }),
            "{again:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(queue.pending_len(), 0, "no new job on the revisit");
        assert_eq!(d.online_calls(), 3);
    }

    /// A confirmed absence settles the job without retry and
    /// leaves the verdict, not the pending memo.
    #[tokio::test]
    async fn a_confirmed_absence_is_not_retried() {
        let d = Script::new(
            vec![json!({"status": "not_found", "provider_id": "itunes"})],
            None,
        );
        let (cascade, queue, _dir) = wired(d.clone(), fast(8, 3));
        let p = principal();
        cascade.resolve(&p, "mpd-path", "a/b.flac", "small").await;
        settled(&queue).await;
        assert_eq!(d.online_calls(), 1);
        match cascade.resolve(&p, "mpd-path", "a/b.flac", "small").await {
            CascadeOutcome::NotFound { provider_id, .. } => {
                assert_eq!(provider_id.as_deref(), Some("itunes"))
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(queue.pending_len(), 0);
    }
}
