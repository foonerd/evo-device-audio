// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Universal-floor polling event source.
//!
//! The simplest possible [`LinkEventSource`]
//! implementation: sleep for `interval_ms`, return a
//! `LinkEvent::PeriodicTick`. Independent of any kernel or
//! userspace event surface; works on every platform that
//! can host a tokio runtime.
//!
//! The polling source is mandatory in every shipping
//! configuration wherever it is implementable — it is the
//! universal correctness floor that guarantees the
//! supervisor keeps making progress when typed sources are
//! demoted or have not been built into the binary.

use super::{
    LinkEvent, LinkEventSource, LinkSourceCapabilities, LinkSourceError,
};
use std::time::Duration;
use tokio::sync::Notify;

/// Default cadence the polling source ticks at when not
/// overridden. Per the connectivity-check redesign declares that the polling source is a
/// cold-start / event-source-quiescent fallback, not a
/// steady-state probe loop. The default 60 s floor matches
/// the cold-start window; the supervisor stretches this to
/// `adaptive_tick::DEFAULT_TICK_MAX` (5 min) once admitted
/// event sources are healthy.
pub const DEFAULT_POLLING_INTERVAL_MS: u64 = 60_000;

/// Hard floor enforced at construction. Operators may
/// shorten the polling cadence but not below 5 000 ms —
/// faster polling is a foot-gun on SBC-class devices.
pub const POLLING_INTERVAL_MIN_MS: u64 = 5_000;

/// Periodic-tick event source.
pub struct PollingEventSource {
    interval_ms: u64,
}

impl PollingEventSource {
    /// Construct with the supplied tick interval in
    /// milliseconds. Values below [`POLLING_INTERVAL_MIN_MS`]
    /// are clamped to the floor.
    pub fn new(interval_ms: u64) -> Self {
        Self {
            interval_ms: interval_ms.max(POLLING_INTERVAL_MIN_MS),
        }
    }

    /// Construct with the substrate's default cadence.
    pub fn with_default_interval() -> Self {
        Self::new(DEFAULT_POLLING_INTERVAL_MS)
    }

    /// The effective interval after clamping.
    pub fn interval_ms(&self) -> u64 {
        self.interval_ms
    }
}

impl Default for PollingEventSource {
    fn default() -> Self {
        Self::with_default_interval()
    }
}

#[async_trait::async_trait]
impl LinkEventSource for PollingEventSource {
    fn name(&self) -> &'static str {
        "polling"
    }

    fn capabilities(&self) -> LinkSourceCapabilities {
        LinkSourceCapabilities::polling()
    }

    async fn next_event(&mut self, shutdown: &Notify) -> Option<LinkEvent> {
        tokio::select! {
            _ = shutdown.notified() => None,
            _ = tokio::time::sleep(Duration::from_millis(self.interval_ms)) => {
                Some(LinkEvent::PeriodicTick)
            }
        }
    }

    async fn health_probe(&self) -> Result<(), LinkSourceError> {
        // The polling source has no external dependency
        // beyond the tokio runtime. It is always healthy by
        // construction.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn new_clamps_interval_to_floor() {
        let s = PollingEventSource::new(100);
        assert_eq!(s.interval_ms(), POLLING_INTERVAL_MIN_MS);
    }

    #[test]
    fn new_accepts_value_at_floor() {
        let s = PollingEventSource::new(POLLING_INTERVAL_MIN_MS);
        assert_eq!(s.interval_ms(), POLLING_INTERVAL_MIN_MS);
    }

    #[test]
    fn new_preserves_value_above_floor() {
        let s = PollingEventSource::new(60_000);
        assert_eq!(s.interval_ms(), 60_000);
    }

    #[test]
    fn default_uses_substrate_default_cadence() {
        let s = PollingEventSource::default();
        assert_eq!(s.interval_ms(), DEFAULT_POLLING_INTERVAL_MS);
    }

    #[test]
    fn name_is_polling() {
        let s = PollingEventSource::default();
        assert_eq!(LinkEventSource::name(&s), "polling");
    }

    #[test]
    fn capabilities_cover_every_field() {
        let s = PollingEventSource::default();
        let c = s.capabilities();
        assert!(c.observes_carrier);
        assert!(c.observes_address);
        assert!(c.observes_wifi_association);
        assert!(c.observes_connectivity_verdict);
    }

    #[tokio::test]
    async fn next_event_produces_periodic_tick() {
        let mut s = PollingEventSource::new(POLLING_INTERVAL_MIN_MS);
        // Override the interval for test speed via the
        // public API; values < floor clamp so this still
        // ticks at the floor.
        s.interval_ms = 20;
        let shutdown = Arc::new(Notify::new());
        let event = s.next_event(&shutdown).await;
        assert_eq!(event, Some(LinkEvent::PeriodicTick));
    }

    #[tokio::test]
    async fn next_event_returns_none_on_shutdown() {
        let mut s = PollingEventSource::new(POLLING_INTERVAL_MIN_MS);
        s.interval_ms = 60_000; // long enough that the
                                // shutdown wins the race.
        let shutdown = Arc::new(Notify::new());
        let shutdown_clone = Arc::clone(&shutdown);
        let task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            shutdown_clone.notify_waiters();
        });
        let event = s.next_event(&shutdown).await;
        assert_eq!(event, None);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn health_probe_always_succeeds() {
        let s = PollingEventSource::default();
        assert!(s.health_probe().await.is_ok());
    }
}
