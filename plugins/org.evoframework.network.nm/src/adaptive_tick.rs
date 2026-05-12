// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Adaptive safety-tick interval computed from observed event
//! cadence + source-health state.
//!
//! The polling source's tick interval is *not* a configuration
//! constant. It is recomputed before every wake from the
//! supervisor's recent observations of:
//!
//! - How many event sources are currently admitted (healthy +
//!   contributing).
//! - How long it has been since the last event from any
//!   source.
//! - The supervisor's current safety tick (so the new value
//!   moves in a direction informed by the prior one — no
//!   discontinuous jumps).
//!
//! Behaviour at the two extremes:
//!
//! - **Steady state, multi-source healthy coverage.** The
//!   supervisor is receiving frequent events from typed
//!   sources; the polling source is a backstop, not a primary.
//!   The safety tick stretches toward [`DEFAULT_TICK_MAX`]
//!   (5 minutes) — polling fires rarely, the CPU stays cool,
//!   the observatory stops looking busy when nothing is
//!   happening.
//!
//! - **Degraded coverage or extended event silence.** Some
//!   sources have been demoted, or no events have arrived
//!   within the expected window. The safety tick shrinks
//!   toward [`DEFAULT_TICK_MIN`] (10 seconds) so polling
//!   compensates for the missing typed coverage.
//!
//! Hard floor: 5 000 ms. The floor is non-negotiable — going
//! faster is a foot-gun on SBC-class devices and is refused at
//! config-validate time. Operators who genuinely need faster
//! cadence install another typed source rather than ratcheting
//! polling.
//!
//! The computation is pure — wall-clock is passed in, no I/O,
//! no Notify. The supervisor calls it once per wake to decide
//! the next [`Duration`] for its polling source's sleep.

use std::time::Duration;

/// Hard lower bound on the safety tick. Below this the
/// supervisor exhibits the runaway-polling behaviour the
/// design exists to eliminate.
pub const TICK_HARD_FLOOR: Duration = Duration::from_millis(5_000);

/// Default minimum safety tick. The supervisor shrinks toward
/// this value when typed-source coverage is degraded.
pub const DEFAULT_TICK_MIN: Duration = Duration::from_secs(10);

/// Default maximum safety tick. The supervisor stretches
/// toward this value when typed-source coverage is healthy +
/// recent events confirm it.
pub const DEFAULT_TICK_MAX: Duration = Duration::from_secs(5 * 60);

/// How long without any event the supervisor classifies as
/// "silent past the expected cadence". When the silence
/// exceeds this, the tick shrinks regardless of source count
/// — even fully-admitted sources might be silently broken.
pub const DEFAULT_SILENCE_THRESHOLD: Duration = Duration::from_secs(120);

/// Configuration knobs for the adaptive tick calculator. The
/// supervisor builds this once at boot from its operator-
/// supplied config; subsequent ticks consult it without
/// re-reading the config.
#[derive(Debug, Clone)]
pub struct AdaptiveTickConfig {
    /// Floor the computed tick clamps to. Validated to be
    /// `>= TICK_HARD_FLOOR` at construction; the framework
    /// refuses lower values.
    pub tick_min: Duration,
    /// Ceiling the computed tick clamps to.
    pub tick_max: Duration,
    /// Silence threshold past which the tick shrinks
    /// regardless of source-admission state.
    pub silence_threshold: Duration,
}

impl Default for AdaptiveTickConfig {
    fn default() -> Self {
        Self {
            tick_min: DEFAULT_TICK_MIN,
            tick_max: DEFAULT_TICK_MAX,
            silence_threshold: DEFAULT_SILENCE_THRESHOLD,
        }
    }
}

/// Errors raised at [`AdaptiveTickConfig::validate`] time.
#[derive(Debug, thiserror::Error)]
pub enum AdaptiveTickConfigError {
    /// `tick_min` was below the hard floor.
    #[error(
        "adaptive tick: tick_min = {got_ms} ms is below the {floor_ms} ms \
         hard floor; install another typed source rather than ratcheting \
         polling cadence"
    )]
    BelowHardFloor {
        /// The rejected value.
        got_ms: u128,
        /// The hard floor.
        floor_ms: u128,
    },
    /// `tick_max` was not greater than `tick_min`.
    #[error(
        "adaptive tick: tick_max = {max_ms} ms must be greater than \
         tick_min = {min_ms} ms"
    )]
    MaxNotGreaterThanMin {
        /// The minimum.
        min_ms: u128,
        /// The maximum (must exceed `min_ms`).
        max_ms: u128,
    },
}

impl AdaptiveTickConfig {
    /// Validate the config. Called once at supervisor boot;
    /// failure refuses the boot rather than silently clamping.
    pub fn validate(&self) -> Result<(), AdaptiveTickConfigError> {
        if self.tick_min < TICK_HARD_FLOOR {
            return Err(AdaptiveTickConfigError::BelowHardFloor {
                got_ms: self.tick_min.as_millis(),
                floor_ms: TICK_HARD_FLOOR.as_millis(),
            });
        }
        if self.tick_max <= self.tick_min {
            return Err(AdaptiveTickConfigError::MaxNotGreaterThanMin {
                min_ms: self.tick_min.as_millis(),
                max_ms: self.tick_max.as_millis(),
            });
        }
        Ok(())
    }
}

/// Inputs to the per-wake tick computation. All fields are
/// observable from the supervisor's existing state cells —
/// no new persistence is required.
#[derive(Debug, Clone, Copy)]
pub struct AdaptiveTickInput {
    /// Number of sources currently mounted (regardless of
    /// admission state).
    pub active_source_count: usize,
    /// Number of sources whose admission state is currently
    /// `Admitted`. Always `<= active_source_count`.
    pub healthy_source_count: usize,
    /// Time elapsed since the supervisor last received any
    /// event from any source. `None` when no event has been
    /// received yet (boot path).
    pub time_since_last_event: Option<Duration>,
}

/// Compute the next safety-tick duration.
///
/// Pure function over the supplied inputs + config. Always
/// returns a value within `[config.tick_min, config.tick_max]`;
/// the result is clamped at both ends regardless of the
/// scoring logic's output.
///
/// Scoring (informal):
///
/// - At boot (`time_since_last_event = None`): return
///   `tick_min`. The supervisor needs to compose observations
///   as quickly as possible until typed events confirm
///   coverage.
/// - Every source admitted AND the last event was recent
///   (within the silence threshold): stretch toward
///   `tick_max`. Polling fades into the background.
/// - Some sources demoted: scale linearly between
///   `tick_min` and `tick_max` based on the healthy ratio.
///   No healthy sources at all → `tick_min`. All sources
///   healthy → `tick_max`.
/// - Silence past the threshold: shrink to `tick_min`
///   regardless of admission state. Even fully-admitted
///   sources might be silently broken.
pub fn next_tick(
    input: AdaptiveTickInput,
    config: &AdaptiveTickConfig,
) -> Duration {
    // Boot path — no events yet.
    let Some(silence) = input.time_since_last_event else {
        return config.tick_min;
    };

    // Silence past the threshold — shrink to floor.
    if silence > config.silence_threshold {
        return config.tick_min;
    }

    // No sources at all → fall back to min.
    if input.active_source_count == 0 {
        return config.tick_min;
    }

    // Healthy ratio drives the linear interpolation.
    let healthy_ratio = (input.healthy_source_count as f64)
        / (input.active_source_count as f64);
    let span_ms = config.tick_max.as_millis() - config.tick_min.as_millis();
    let computed_ms = config.tick_min.as_millis()
        + ((span_ms as f64) * healthy_ratio) as u128;
    let clamped_ms = computed_ms
        .min(config.tick_max.as_millis())
        .max(config.tick_min.as_millis());
    Duration::from_millis(clamped_ms as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AdaptiveTickConfig {
        AdaptiveTickConfig::default()
    }

    #[test]
    fn boot_path_returns_tick_min() {
        let input = AdaptiveTickInput {
            active_source_count: 0,
            healthy_source_count: 0,
            time_since_last_event: None,
        };
        assert_eq!(next_tick(input, &cfg()), DEFAULT_TICK_MIN);
    }

    #[test]
    fn no_active_sources_returns_tick_min() {
        let input = AdaptiveTickInput {
            active_source_count: 0,
            healthy_source_count: 0,
            time_since_last_event: Some(Duration::from_secs(1)),
        };
        assert_eq!(next_tick(input, &cfg()), DEFAULT_TICK_MIN);
    }

    #[test]
    fn all_healthy_recent_events_stretch_to_tick_max() {
        let input = AdaptiveTickInput {
            active_source_count: 3,
            healthy_source_count: 3,
            time_since_last_event: Some(Duration::from_secs(10)),
        };
        assert_eq!(next_tick(input, &cfg()), DEFAULT_TICK_MAX);
    }

    #[test]
    fn half_demoted_interpolates_to_mid() {
        let input = AdaptiveTickInput {
            active_source_count: 4,
            healthy_source_count: 2,
            time_since_last_event: Some(Duration::from_secs(5)),
        };
        let tick = next_tick(input, &cfg());
        let expected_ms =
            (DEFAULT_TICK_MIN.as_millis() + DEFAULT_TICK_MAX.as_millis()) / 2;
        let got_ms = tick.as_millis();
        // Allow 1 ms slop from integer rounding.
        assert!(
            got_ms.abs_diff(expected_ms) <= 1,
            "expected ~{expected_ms} ms, got {got_ms} ms"
        );
    }

    #[test]
    fn fully_demoted_returns_tick_min() {
        let input = AdaptiveTickInput {
            active_source_count: 3,
            healthy_source_count: 0,
            time_since_last_event: Some(Duration::from_secs(5)),
        };
        assert_eq!(next_tick(input, &cfg()), DEFAULT_TICK_MIN);
    }

    #[test]
    fn silence_past_threshold_shrinks_regardless_of_health() {
        let input = AdaptiveTickInput {
            active_source_count: 3,
            healthy_source_count: 3,
            time_since_last_event: Some(Duration::from_secs(300)),
        };
        assert_eq!(next_tick(input, &cfg()), DEFAULT_TICK_MIN);
    }

    #[test]
    fn config_validate_rejects_below_hard_floor() {
        let c = AdaptiveTickConfig {
            tick_min: Duration::from_millis(1_000),
            tick_max: Duration::from_secs(60),
            silence_threshold: Duration::from_secs(120),
        };
        match c.validate().unwrap_err() {
            AdaptiveTickConfigError::BelowHardFloor { got_ms, floor_ms } => {
                assert_eq!(got_ms, 1_000);
                assert_eq!(floor_ms, 5_000);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn config_validate_rejects_max_not_greater_than_min() {
        let c = AdaptiveTickConfig {
            tick_min: Duration::from_secs(60),
            tick_max: Duration::from_secs(60),
            silence_threshold: Duration::from_secs(120),
        };
        assert!(matches!(
            c.validate(),
            Err(AdaptiveTickConfigError::MaxNotGreaterThanMin { .. })
        ));
    }

    #[test]
    fn config_validate_accepts_default() {
        AdaptiveTickConfig::default()
            .validate()
            .expect("default valid");
    }

    #[test]
    fn output_always_clamped_to_config_range() {
        let c = cfg();
        for active in 0..=5 {
            for healthy in 0..=active {
                for silence_s in [0u64, 30, 60, 119, 121, 1_000] {
                    let input = AdaptiveTickInput {
                        active_source_count: active,
                        healthy_source_count: healthy,
                        time_since_last_event: Some(Duration::from_secs(
                            silence_s,
                        )),
                    };
                    let tick = next_tick(input, &c);
                    assert!(
                        tick >= c.tick_min,
                        "tick {tick:?} below min {:?}",
                        c.tick_min
                    );
                    assert!(
                        tick <= c.tick_max,
                        "tick {tick:?} above max {:?}",
                        c.tick_max
                    );
                }
            }
        }
    }
}
