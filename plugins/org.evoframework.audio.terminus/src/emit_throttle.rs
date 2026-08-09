// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Wall-clock emit throttle for the spectrum-frame subject.
//!
//! Pure decision logic extracted from the ALSA capture loop so
//! it is unit-testable without the ALSA-substrate feature (the
//! capture module itself is feature-gated because it links
//! `alsa-sys`; this helper is not).
//!
//! ## Why an ideal-target scheme
//!
//! The capture loop wakes at the ALSA hop rate (~47 Hz on the
//! canonical 48 kHz / 1024-point chain — 21.3 ms per hop) and
//! must emit to the spectrum-frame subject at the operator's
//! demand target (typical 30 Hz — 33.3 ms per emit gap). The
//! two rates are not multiples of each other, so a naïve
//! "elapsed since last emit >= min_gap" scheme rounds every
//! decision down to a hop boundary and drops one extra hop
//! whenever the previous emit happened at a boundary past the
//! target — measured rate lands at ALSA_hz / 2 (~23.5 Hz for
//! the canonical chain) instead of the requested 30 Hz.
//!
//! The scheme this module implements instead tracks the
//! **ideal** wall-clock target for the next emit and advances
//! it by `min_gap` per emit, regardless of when the actual
//! emit fired. The long-run rate averages to `1/min_gap`
//! (i.e., `rate_hz_target`) even though individual gaps
//! alternate one-hop / two-hops as the target grid slips
//! across hop boundaries.
//!
//! ## Catch-up snap
//!
//! If the capture loop was blocked (transport gate closed,
//! demand shape change forcing an analyser rebuild) and the
//! scheduled `next_emit_at` is now far in the past, we snap
//! forward to `now + min_gap` instead of replaying a burst of
//! catch-up emits. The threshold is one `min_gap` of slack —
//! a single hop late is still on-schedule, more than one full
//! gap late is a re-entry after a real pause.

use std::time::{Duration, Instant};

/// Wall-clock emit governor state.
///
/// One instance per capture-loop lifetime. `should_emit` is
/// called once per successful FFT compute; it returns `true`
/// when the caller should push to the spectrum subject and
/// advances the internal schedule as a side effect.
#[derive(Debug, Default)]
pub struct EmitThrottle {
    /// Ideal wall-clock target for the NEXT emit. `None` on
    /// first invocation — the throttle emits immediately and
    /// arms the schedule.
    next_emit_at: Option<Instant>,
}

impl EmitThrottle {
    // The production caller (capture.rs) is feature-gated to
    // `alsa-substrate`; on the default feature set the throttle
    // is exercised only via the test module below. The
    // `dead_code` suppression tracks the same pattern already
    // in use for `spectrum_subject::emit_frame`.
    #[cfg_attr(not(feature = "alsa-substrate"), allow(dead_code))]
    pub fn new() -> Self {
        Self { next_emit_at: None }
    }

    /// Return `true` if the caller should emit at `now`,
    /// advancing the internal schedule as a side effect.
    /// `min_gap` is the desired wall-clock spacing between
    /// emits (typically `Duration::from_millis(1000 /
    /// rate_hz_target)`).
    #[cfg_attr(not(feature = "alsa-substrate"), allow(dead_code))]
    pub fn should_emit(&mut self, now: Instant, min_gap: Duration) -> bool {
        if let Some(scheduled) = self.next_emit_at {
            if now < scheduled {
                return false;
            }
            // On-schedule: advance the ideal target by exactly
            // one `min_gap`. If we're more than one full gap
            // behind (transport-gate re-entry), snap forward to
            // `now + min_gap` to avoid a burst.
            self.next_emit_at =
                Some(if now.duration_since(scheduled) < min_gap {
                    scheduled + min_gap
                } else {
                    now + min_gap
                });
        } else {
            // First emit — arm the schedule.
            self.next_emit_at = Some(now + min_gap);
        }
        true
    }

    /// Reset the throttle to first-emit state — the next
    /// `should_emit` call returns `true` and re-arms. Used when
    /// the demand shape changes and the caller re-enters the
    /// inner loop.
    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.next_emit_at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn first_call_always_emits_and_arms_schedule() {
        let mut t = EmitThrottle::new();
        let now = Instant::now();
        assert!(t.should_emit(now, ms(33)));
        // Same instant, second call: schedule was armed to
        // now + 33, so we should refuse until then.
        assert!(!t.should_emit(now, ms(33)));
    }

    #[test]
    fn refuses_before_scheduled_target() {
        let mut t = EmitThrottle::new();
        let t0 = Instant::now();
        assert!(t.should_emit(t0, ms(33)));
        // Query at every ALSA hop up to but not past the target.
        assert!(!t.should_emit(t0 + ms(21), ms(33)));
        assert!(!t.should_emit(t0 + ms(32), ms(33)));
    }

    #[test]
    fn emits_at_first_hop_at_or_past_target() {
        let mut t = EmitThrottle::new();
        let t0 = Instant::now();
        assert!(t.should_emit(t0, ms(33)));
        // Third hop lands at 63.9 ms; we emit at 42 (the second
        // hop past 33).
        assert!(!t.should_emit(t0 + ms(21), ms(33)));
        assert!(t.should_emit(t0 + ms(42), ms(33)));
    }

    #[test]
    fn long_run_rate_matches_target_on_21ms_hops_at_30hz() {
        // The canonical failure case this module fixes.
        // Simulate 4 seconds of ~47 Hz ALSA hops (21 ms) with
        // a 30 Hz emit target (33 ms gap). Naïve "elapsed >=
        // min_gap" gives 23.5 Hz (every other hop). The
        // ideal-target scheme MUST give ~30 Hz.
        let mut t = EmitThrottle::new();
        let t0 = Instant::now();
        let hop = 21;
        let target_ms = 33;
        let window_ms: u64 = 4_000;
        let mut emits = 0usize;
        let mut n = 0u64;
        loop {
            let elapsed = n * hop;
            if elapsed > window_ms {
                break;
            }
            if t.should_emit(t0 + ms(elapsed), ms(target_ms)) {
                emits += 1;
            }
            n += 1;
        }
        // 4 seconds at 30 Hz = 120 emits. Ideal-target scheme
        // yields 118–121 depending on where the last hop lands.
        assert!(
            (115..=125).contains(&emits),
            "expected ~120 emits in 4s at 30 Hz target on 21ms hops, \
             got {emits} (naïve scheme would give ~90)"
        );
    }

    #[test]
    fn long_run_rate_matches_target_on_1ms_hops_at_60hz() {
        // High-precision case: 1 ms hop resolution, 60 Hz
        // target (16 ms gap). Long-run rate should be exact.
        let mut t = EmitThrottle::new();
        let t0 = Instant::now();
        let mut emits = 0usize;
        for n in 0..1_000u64 {
            if t.should_emit(t0 + ms(n), ms(16)) {
                emits += 1;
            }
        }
        // 1000 ms / 16 ms/emit ≈ 62 emits (integer-arithmetic
        // gap is 16 ms so 1000/16 = 62.5, first-emit accounts
        // for the extra).
        assert!(
            (58..=65).contains(&emits),
            "expected ~62 emits in 1s at 60 Hz target (16ms gap), got {emits}"
        );
    }

    #[test]
    fn catch_up_snap_after_long_idle_does_not_burst() {
        // Emit at t=0, then a long idle (transport gate closed
        // for 500 ms — capture loop was skipping emits). When
        // the loop re-enters at t=500, we should emit once, NOT
        // 500/33 ≈ 15 times.
        let mut t = EmitThrottle::new();
        let t0 = Instant::now();
        assert!(t.should_emit(t0, ms(33)));
        // Re-entry at t+500 ms.
        assert!(t.should_emit(t0 + ms(500), ms(33)));
        // Immediately after re-entry, next emit should be at
        // 500 + 33 = 533 ms. Anything before is refused.
        assert!(!t.should_emit(t0 + ms(510), ms(33)));
        assert!(!t.should_emit(t0 + ms(530), ms(33)));
        assert!(t.should_emit(t0 + ms(535), ms(33)));
    }

    #[test]
    fn reset_re_arms_first_emit() {
        let mut t = EmitThrottle::new();
        let t0 = Instant::now();
        assert!(t.should_emit(t0, ms(33)));
        assert!(!t.should_emit(t0 + ms(5), ms(33)));
        t.reset();
        // After reset, next call emits regardless of prior schedule.
        assert!(t.should_emit(t0 + ms(6), ms(33)));
    }

    #[test]
    fn min_gap_change_takes_effect_next_call() {
        // Demand target changes from 30 Hz (33 ms) to 60 Hz
        // (16 ms) between calls. The throttle should adapt on
        // the next decision — the scheduled target advances by
        // the new min_gap.
        let mut t = EmitThrottle::new();
        let t0 = Instant::now();
        assert!(t.should_emit(t0, ms(33)));
        // At t+20 with 16 ms gap, we should emit (scheduled
        // target was t+33 under the old gap; we're now past
        // t+16 under the new gap — the scheduled target is
        // still t+33 so we refuse).
        assert!(!t.should_emit(t0 + ms(20), ms(16)));
        // At t+34 under the new 16 ms gap, we're past the
        // scheduled t+33 target, so emit — next schedule is
        // scheduled+min_gap = 33+16 = 49.
        assert!(t.should_emit(t0 + ms(34), ms(16)));
        assert!(!t.should_emit(t0 + ms(48), ms(16)));
        assert!(t.should_emit(t0 + ms(50), ms(16)));
    }
}
