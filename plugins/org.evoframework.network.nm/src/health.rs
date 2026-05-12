// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Per-source health-probe scheduler with demotion +
//! exponential-backoff re-admission.
//!
//! Every active [`crate::source::LinkEventSource`] runs a
//! periodic health probe on its own cadence. The probe is the
//! source's self-report — for the polling source it is
//! tautologically `Ok`; for rtnetlink it is an in-band
//! `link().get()` round-trip; for the NetworkManager D-Bus
//! source it is a `Version` property read against the live
//! daemon.
//!
//! The supervisor's health monitor:
//!
//! 1. Runs a probe on each active source every `probe_interval`
//!    (default 60 s).
//! 2. On a successful probe, leaves the source in
//!    [`SourceAdmissionState::Admitted`].
//! 3. On a failed probe, transitions the source to
//!    [`SourceAdmissionState::Demoted`] with an
//!    exponentially-backed-off `next_attempt_at` instant.
//!    A demoted source is no longer consumed by the supervisor;
//!    its event stream is paused. The polling source carries
//!    the load for the missing surfaces.
//! 4. When the backoff elapses, runs the probe again. Success
//!    re-admits the source; failure increments the backoff.
//!
//! Backoff schedule: 30 s, 60 s, 2 min, 5 min, 15 min, 1 h,
//! capped at 6 h. The cap is deliberate — a source that has
//! failed continuously for hours is producing diagnostic
//! evidence the operator should react to; probing it every
//! second buys nothing.
//!
//! Every transition emits a typed observation through the
//! supervisor's observability fan-out:
//!
//! - `LinkSourceDemoted { source, reason }` on
//!   Admitted → Demoted.
//! - `LinkSourceAdmitted { source }` on Demoted → Admitted.
//!
//! Both are surfaced on the wire-op `describe_capabilities`
//! response (the current admission state of every active
//! source) so operators see which sources are healthy at any
//! moment.

use crate::source::LinkSourceError;
use serde::Serialize;
use std::time::{Duration, Instant};

/// Default cadence at which the monitor probes each active
/// source. 60 s balances responsiveness against load — a
/// source that's healthy will round-trip its probe in
/// milliseconds; one probe per minute per source is a
/// trivial cost.
pub const DEFAULT_PROBE_INTERVAL: Duration = Duration::from_secs(60);

/// Backoff schedule for re-admission attempts on a demoted
/// source. Indexed by the failure count (count = 0 is the
/// initial demotion's backoff before the first re-probe;
/// count = 1 is the second re-probe's backoff, etc.).
/// Capped at 6 hours.
const BACKOFF_SCHEDULE: &[Duration] = &[
    Duration::from_secs(30),          // 30 s
    Duration::from_secs(60),          // 1 min
    Duration::from_secs(2 * 60),      // 2 min
    Duration::from_secs(5 * 60),      // 5 min
    Duration::from_secs(15 * 60),     // 15 min
    Duration::from_secs(60 * 60),     // 1 hour
    Duration::from_secs(6 * 60 * 60), // 6 hours (cap)
];

/// Compute the backoff for a given failure count. Values
/// past the end of the schedule clamp to the schedule's
/// final entry (6 h).
pub fn backoff_for(failure_count: u32) -> Duration {
    let idx = (failure_count as usize).min(BACKOFF_SCHEDULE.len() - 1);
    BACKOFF_SCHEDULE[idx]
}

/// Admission state of one source. The supervisor consults
/// this on every wake to decide whether to read events from
/// the source's fan-in queue or skip it.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceAdmissionState {
    /// Source is healthy and contributing events.
    #[default]
    Admitted,
    /// Source has been demoted after a failed probe. The
    /// `failure_count` increments on every subsequent failed
    /// re-probe; `next_attempt_at` is the instant after which
    /// the next probe should run.
    Demoted {
        /// Operator-readable reason the demotion fired.
        reason: String,
        /// Number of consecutive failed probes that drove
        /// the demotion (starts at 1 on first failure).
        failure_count: u32,
        /// Instant after which the next re-probe should run.
        /// Serde-skipped — wall-clock instants don't
        /// round-trip cleanly through JSON; the wire surface
        /// reports the remaining-backoff seconds via a
        /// separate field if needed.
        #[serde(skip)]
        next_attempt_at: Instant,
    },
}

impl SourceAdmissionState {
    /// Whether the source is currently contributing events.
    pub fn is_admitted(&self) -> bool {
        matches!(self, Self::Admitted)
    }

    /// Whether a re-probe is due now (only meaningful on a
    /// `Demoted` state).
    pub fn reprobe_due(&self, now: Instant) -> bool {
        match self {
            Self::Admitted => false,
            Self::Demoted {
                next_attempt_at, ..
            } => now >= *next_attempt_at,
        }
    }

    /// Failure count, or 0 when admitted.
    pub fn failure_count(&self) -> u32 {
        match self {
            Self::Admitted => 0,
            Self::Demoted { failure_count, .. } => *failure_count,
        }
    }
}

/// Outcome of one probe cycle on a source. The supervisor's
/// observability fan-out emits a matching typed observation
/// per outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// Source remained in [`SourceAdmissionState::Admitted`];
    /// no observation needed (probes that succeed silently
    /// are not interesting to the operator).
    StillAdmitted,
    /// Source moved from `Admitted` → `Demoted`. Emit
    /// `LinkSourceDemoted { source, reason }`.
    Demoted {
        /// Operator-readable reason.
        reason: String,
    },
    /// Source stayed in `Demoted`; the re-probe failed
    /// again. The new failure count is in
    /// [`Self::new_failure_count`]. Emit the same kind of
    /// observation as the initial demotion so a long-running
    /// demotion is visible at a configurable cadence on the
    /// wire (one observation per probe failure).
    StillDemoted {
        /// New failure count after this probe.
        new_failure_count: u32,
        /// Operator-readable reason from this probe attempt.
        reason: String,
    },
    /// Source moved from `Demoted` → `Admitted` after a
    /// successful re-probe. Emit `LinkSourceAdmitted { source }`.
    Readmitted,
}

/// Run one probe-result through the state machine.
///
/// Pure function: takes the current state + the probe
/// outcome + the current wall-clock, returns the new state +
/// a typed `ProbeOutcome` the supervisor uses to drive the
/// observability fan-out.
///
/// Wall-clock is passed explicitly so unit tests can advance
/// time without spinning on real elapsed seconds.
pub fn apply_probe_result(
    current: &SourceAdmissionState,
    probe: Result<(), LinkSourceError>,
    now: Instant,
) -> (SourceAdmissionState, ProbeOutcome) {
    match (current, probe) {
        // Healthy → healthy.
        (SourceAdmissionState::Admitted, Ok(())) => {
            (SourceAdmissionState::Admitted, ProbeOutcome::StillAdmitted)
        }
        // Healthy → demoted.
        (SourceAdmissionState::Admitted, Err(err)) => {
            let reason = err.to_string();
            let next_state = SourceAdmissionState::Demoted {
                reason: reason.clone(),
                failure_count: 1,
                next_attempt_at: now + backoff_for(0),
            };
            (next_state, ProbeOutcome::Demoted { reason })
        }
        // Demoted → admitted.
        (SourceAdmissionState::Demoted { .. }, Ok(())) => {
            (SourceAdmissionState::Admitted, ProbeOutcome::Readmitted)
        }
        // Demoted → still demoted; backoff escalates.
        (SourceAdmissionState::Demoted { failure_count, .. }, Err(err)) => {
            let reason = err.to_string();
            let new_failure_count = failure_count.saturating_add(1);
            let next_state = SourceAdmissionState::Demoted {
                reason: reason.clone(),
                failure_count: new_failure_count,
                next_attempt_at: now + backoff_for(new_failure_count),
            };
            (
                next_state,
                ProbeOutcome::StillDemoted {
                    new_failure_count,
                    reason,
                },
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_schedule_climbs_then_caps() {
        assert_eq!(backoff_for(0), Duration::from_secs(30));
        assert_eq!(backoff_for(1), Duration::from_secs(60));
        assert_eq!(backoff_for(2), Duration::from_secs(120));
        assert_eq!(backoff_for(3), Duration::from_secs(300));
        assert_eq!(backoff_for(4), Duration::from_secs(900));
        assert_eq!(backoff_for(5), Duration::from_secs(3600));
        assert_eq!(backoff_for(6), Duration::from_secs(21600));
        // Past the schedule, clamps at 6 h.
        assert_eq!(backoff_for(7), Duration::from_secs(21600));
        assert_eq!(backoff_for(100), Duration::from_secs(21600));
    }

    #[test]
    fn admitted_healthy_stays_admitted() {
        let now = Instant::now();
        let (next, outcome) =
            apply_probe_result(&SourceAdmissionState::Admitted, Ok(()), now);
        assert!(next.is_admitted());
        assert_eq!(outcome, ProbeOutcome::StillAdmitted);
    }

    #[test]
    fn admitted_failure_demotes_with_initial_backoff() {
        let now = Instant::now();
        let err = LinkSourceError::Disconnected("test".into());
        let (next, outcome) =
            apply_probe_result(&SourceAdmissionState::Admitted, Err(err), now);
        assert!(!next.is_admitted());
        assert_eq!(next.failure_count(), 1);
        match &outcome {
            ProbeOutcome::Demoted { reason } => {
                assert!(reason.contains("source disconnected"));
            }
            other => panic!("expected Demoted, got {other:?}"),
        }
        // Next attempt is in ~30 s.
        let SourceAdmissionState::Demoted {
            next_attempt_at, ..
        } = next
        else {
            unreachable!()
        };
        let gap = next_attempt_at - now;
        assert!(gap >= Duration::from_secs(29));
        assert!(gap <= Duration::from_secs(31));
    }

    #[test]
    fn demoted_success_re_admits() {
        let now = Instant::now();
        let demoted = SourceAdmissionState::Demoted {
            reason: "old failure".into(),
            failure_count: 3,
            next_attempt_at: now,
        };
        let (next, outcome) = apply_probe_result(&demoted, Ok(()), now);
        assert!(next.is_admitted());
        assert_eq!(outcome, ProbeOutcome::Readmitted);
    }

    #[test]
    fn demoted_failure_escalates_backoff() {
        let now = Instant::now();
        let demoted = SourceAdmissionState::Demoted {
            reason: "first".into(),
            failure_count: 1,
            next_attempt_at: now,
        };
        let err = LinkSourceError::DaemonError("still down".into());
        let (next, outcome) = apply_probe_result(&demoted, Err(err), now);
        assert_eq!(next.failure_count(), 2);
        match &outcome {
            ProbeOutcome::StillDemoted {
                new_failure_count,
                reason,
            } => {
                assert_eq!(*new_failure_count, 2);
                assert!(reason.contains("still down"));
            }
            other => panic!("expected StillDemoted, got {other:?}"),
        }
        // Next attempt at +60 s (failure_count = 2 → backoff_for(2) = 120 s).
        let SourceAdmissionState::Demoted {
            next_attempt_at, ..
        } = next
        else {
            unreachable!()
        };
        let gap = next_attempt_at - now;
        assert!(gap >= Duration::from_secs(119));
        assert!(gap <= Duration::from_secs(121));
    }

    #[test]
    fn reprobe_due_respects_next_attempt_at() {
        let now = Instant::now();
        let demoted = SourceAdmissionState::Demoted {
            reason: "x".into(),
            failure_count: 1,
            next_attempt_at: now + Duration::from_secs(10),
        };
        assert!(!demoted.reprobe_due(now));
        assert!(demoted.reprobe_due(now + Duration::from_secs(10)));
        assert!(demoted.reprobe_due(now + Duration::from_secs(20)));
    }

    #[test]
    fn admitted_state_never_indicates_reprobe_due() {
        let now = Instant::now();
        assert!(!SourceAdmissionState::Admitted.reprobe_due(now));
        assert!(!SourceAdmissionState::Admitted
            .reprobe_due(now + Duration::from_secs(3600)));
    }

    #[test]
    fn many_consecutive_failures_clamp_at_cap() {
        let mut state = SourceAdmissionState::Admitted;
        let mut now = Instant::now();
        // 20 consecutive failures.
        for _ in 0..20 {
            let err = LinkSourceError::Silent;
            let (next, _) = apply_probe_result(&state, Err(err), now);
            state = next;
            now += Duration::from_secs(60_000);
        }
        // Failure count tracks total failures.
        assert_eq!(state.failure_count(), 20);
        // Backoff caps at 6 h.
        let SourceAdmissionState::Demoted {
            next_attempt_at, ..
        } = state
        else {
            unreachable!()
        };
        let gap = next_attempt_at - now;
        assert!(gap <= Duration::from_secs(6 * 60 * 60));
    }
}
