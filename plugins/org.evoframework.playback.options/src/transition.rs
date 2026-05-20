//! Mixer-mode transition state machine + injected executor +
//! lifecycle-happening emitter.
//!
//! Operator-actuating mixer-type gestures (`options.set_mixer_type`)
//! drive an orchestrated state machine that honours the
//! mixer-transition invariants contract:
//!
//! - **I1 Loudness continuity**: the prior authority's
//!   effective level is read AND used to seed the new
//!   authority before unmute.
//! - **I2 No blast risk**: unmute is the LAST step, reachable
//!   only after a successful set-and-verify.
//! - **I3 Single authority**: the state machine is held under
//!   a per-plugin lock; concurrent gestures wait.
//! - **I4 Deterministic carry-over**: the carried level is an
//!   explicit `u8` value plumbed through every step.
//! - **I5 Rollback-safe**: every step's failure routes to the
//!   rollback chain; a rollback that itself fails escalates
//!   to a terminal `failed` state with the chain left muted.
//! - **I6 Operator truth**: the `applied` happening carries
//!   the post-transition effective level so UI displays the
//!   new authority's value, not the pre-transition cached
//!   slider position.
//!
//! The state machine is pure: it advances phase-by-phase by
//! calling injected step functions (the
//! [`TransitionExecutor`] trait). For unit tests, the
//! [`CapturingTransitionHappener`] records lifecycle
//! emissions in-memory; the
//! [`StubTransitionExecutor`] implements deterministic step
//! outcomes. For the runtime (P1), a `LiveTransitionExecutor`
//! will coordinate delivery.alsa + playback.mpd via the
//! framework's cross-plugin reactive substrate.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::MixerType;

/// Outcome of a transition. Returned by [`run_transition`];
/// mutually exclusive across the three terminal variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionOutcome {
    /// Transition completed all eight steps successfully.
    Applied {
        /// Post-transition effective level (the verify step's
        /// readback). The `applied` happening carries this
        /// value so UI updates without re-querying.
        effective_level: u8,
    },
    /// A step failed; rollback succeeded; the chain is back
    /// at the prior valid state.
    RolledBack {
        /// Phase name at which the failure occurred.
        at_phase: &'static str,
        /// Operator-readable reason.
        reason: String,
    },
    /// A step failed AND the rollback chain itself failed;
    /// the chain is in a terminal muted state. The operator
    /// must intervene (manual chain reset).
    Failed {
        /// Phase name at which the original failure occurred.
        at_phase: &'static str,
        /// Operator-readable reason (concatenation of the
        /// original failure + the rollback failure).
        reason: String,
    },
    /// Source and target mixer-types match; no transition
    /// needed. The state machine is short-circuited; no
    /// lifecycle happenings emit.
    NoOp,
}

/// Eight-step executor trait. Concrete implementations carry
/// the actual side effects (cross-plugin coordination with
/// delivery.alsa + playback.mpd). The state machine is
/// independent of which implementation runs — runtime,
/// stub, or capture.
///
/// Every method returns `Result<T, String>`; the `Err`
/// payload is the operator-readable diagnostic that flows
/// into the `rolled_back` / `failed` lifecycle happening.
pub trait TransitionExecutor: Send + Sync {
    /// Step 1 — read the prior authority's effective level
    /// in 0..=100 percent units. For `MixerType::None`
    /// (i.e. transitioning OUT of no-control) the
    /// implementation returns a documented default (e.g. 0).
    fn read_carried_level<'a>(
        &'a self,
        from: MixerType,
    ) -> Pin<Box<dyn Future<Output = Result<u8, String>> + Send + 'a>>;

    /// Step 2 — pre-mute the chain. After this returns Ok,
    /// no audio reaches the output until step 7 (unmute) runs.
    fn pre_mute<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

    /// Step 3 — set the new authority's value to
    /// `carried_level`. For `MixerType::None` the operation
    /// is a documented no-op (no authority to set).
    fn set_new_authority<'a>(
        &'a self,
        to: MixerType,
        carried_level: u8,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

    /// Step 4 — switch the playback chain's fragment to the
    /// new authority (delivery.alsa drop-in + playback.mpd
    /// fragment).
    fn switch_fragment<'a>(
        &'a self,
        to: MixerType,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

    /// Step 5 — restart the playback chain so the new
    /// fragment is in effect (MPD restart, typically).
    fn restart_playback<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

    /// Step 6 — verify the effective level matches the
    /// expected `carried_level` within the per-device-profile
    /// tolerance. Returns the actually-observed effective
    /// level for inclusion in the `applied` lifecycle
    /// happening.
    fn verify_effective_level<'a>(
        &'a self,
        expected: u8,
    ) -> Pin<Box<dyn Future<Output = Result<u8, String>> + Send + 'a>>;

    /// Step 7 — unmute the chain. Reachable only after a
    /// successful verify. After this returns Ok the
    /// transition is observable at the output.
    fn unmute<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

    /// Rollback hook — revert the chain to the prior valid
    /// state (prior authority's level restored, prior
    /// fragment restored, mute released). Called when any of
    /// the seven steps above fails. Returns Ok on successful
    /// rollback (chain back to `from`); Err if the rollback
    /// itself failed (chain in terminal muted state).
    fn rollback<'a>(
        &'a self,
        from: MixerType,
        at_phase: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

/// Emit the four lifecycle happenings: `started` / `applied`
/// / `rolled_back` / `failed`. Concrete implementation wraps
/// the SDK's `HappeningEmitter`; test implementations
/// capture emissions in-memory for assertion.
pub trait TransitionHappener: Send + Sync {
    /// Emit `audio.mixer_transition.started`.
    fn emit_started<'a>(
        &'a self,
        from: MixerType,
        to: MixerType,
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    /// Emit `audio.mixer_transition.applied`.
    fn emit_applied<'a>(
        &'a self,
        from: MixerType,
        to: MixerType,
        carried_level: u8,
        effective_level: u8,
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    /// Emit `audio.mixer_transition.rolled_back`.
    fn emit_rolled_back<'a>(
        &'a self,
        from: MixerType,
        to: MixerType,
        at_phase: &'static str,
        reason: String,
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    /// Emit `audio.mixer_transition.failed`.
    fn emit_failed<'a>(
        &'a self,
        from: MixerType,
        to: MixerType,
        at_phase: &'static str,
        reason: String,
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

/// Run the eight-step mixer-mode transition state machine.
/// Pure orchestration: no state of its own — all side
/// effects route through the injected `executor`;
/// observability routes through the injected `happener`.
///
/// Steps (per the mixer-transition invariants contract):
///
/// 1. Read carried level from prior authority.
/// 2. Pre-mute the chain.
/// 3. Set new authority to carried level.
/// 4. Switch fragment.
/// 5. Restart playback.
/// 6. Verify effective level.
/// 7. Unmute.
/// 8. (Implicit) Emit `applied`.
///
/// Any step's failure routes to the rollback chain; any
/// rollback failure escalates to a terminal `failed` state.
pub async fn run_transition(
    from: MixerType,
    to: MixerType,
    executor: Arc<dyn TransitionExecutor>,
    happener: Arc<dyn TransitionHappener>,
) -> TransitionOutcome {
    if from == to {
        return TransitionOutcome::NoOp;
    }

    let at_ms = now_ms();
    happener.emit_started(from, to, at_ms).await;

    // Step 1 — read carried level.
    let carried_level = match executor.read_carried_level(from).await {
        Ok(v) => v,
        Err(e) => {
            return rollback_or_fail(
                from,
                to,
                "read_carried_level",
                e,
                executor,
                happener,
            )
            .await;
        }
    };

    // Step 2 — pre-mute.
    if let Err(e) = executor.pre_mute().await {
        return rollback_or_fail(from, to, "pre_mute", e, executor, happener)
            .await;
    }

    // Step 3 — set new authority to carried level.
    if let Err(e) = executor.set_new_authority(to, carried_level).await {
        return rollback_or_fail(
            from,
            to,
            "set_new_authority",
            e,
            executor,
            happener,
        )
        .await;
    }

    // Step 4 — switch fragment.
    if let Err(e) = executor.switch_fragment(to).await {
        return rollback_or_fail(
            from,
            to,
            "switch_fragment",
            e,
            executor,
            happener,
        )
        .await;
    }

    // Step 5 — restart playback.
    if let Err(e) = executor.restart_playback().await {
        return rollback_or_fail(
            from,
            to,
            "restart_playback",
            e,
            executor,
            happener,
        )
        .await;
    }

    // Step 6 — verify effective level.
    let effective_level =
        match executor.verify_effective_level(carried_level).await {
            Ok(v) => v,
            Err(e) => {
                return rollback_or_fail(
                    from,
                    to,
                    "verify_effective_level",
                    e,
                    executor,
                    happener,
                )
                .await;
            }
        };

    // Step 7 — unmute.
    if let Err(e) = executor.unmute().await {
        return rollback_or_fail(from, to, "unmute", e, executor, happener)
            .await;
    }

    // Step 8 — emit applied; transition complete.
    happener
        .emit_applied(from, to, carried_level, effective_level, now_ms())
        .await;
    TransitionOutcome::Applied { effective_level }
}

/// Run the rollback chain; emit `rolled_back` on success, or
/// `failed` if the rollback itself failed.
async fn rollback_or_fail(
    from: MixerType,
    to: MixerType,
    at_phase: &'static str,
    reason: String,
    executor: Arc<dyn TransitionExecutor>,
    happener: Arc<dyn TransitionHappener>,
) -> TransitionOutcome {
    match executor.rollback(from, at_phase).await {
        Ok(()) => {
            happener
                .emit_rolled_back(from, to, at_phase, reason.clone(), now_ms())
                .await;
            TransitionOutcome::RolledBack { at_phase, reason }
        }
        Err(rollback_err) => {
            let combined = format!("{reason}; rollback failed: {rollback_err}");
            happener
                .emit_failed(from, to, at_phase, combined.clone(), now_ms())
                .await;
            TransitionOutcome::Failed {
                at_phase,
                reason: combined,
            }
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
pub mod test_support {
    //! In-memory executor + happener for state-machine tests.

    use super::*;
    use std::sync::Mutex;

    /// Deterministic executor: every step succeeds with the
    /// supplied carried level + effective level. Mutating
    /// scenarios use [`StubTransitionExecutor::with_step_failure`]
    /// to inject a failure at a named phase, optionally with
    /// rollback success or rollback failure.
    pub struct StubTransitionExecutor {
        /// Carried level returned by step 1.
        pub carried_level: u8,
        /// Effective level returned by step 6.
        pub effective_level: u8,
        /// Phase to fail at, if any. Match by string.
        pub fail_at: Option<&'static str>,
        /// Whether rollback succeeds (true) or fails (false).
        /// Only matters when `fail_at` is Some.
        pub rollback_succeeds: bool,
        /// Per-step invocation order, captured for test
        /// assertions about the sequence and unmute being
        /// last.
        pub steps_called: Mutex<Vec<&'static str>>,
    }

    impl StubTransitionExecutor {
        /// Construct an all-steps-succeed executor.
        pub fn happy(carried_level: u8, effective_level: u8) -> Self {
            Self {
                carried_level,
                effective_level,
                fail_at: None,
                rollback_succeeds: true,
                steps_called: Mutex::new(Vec::new()),
            }
        }

        /// Construct an executor that fails at the named
        /// phase. `rollback_succeeds = true` exercises the
        /// rolled_back path; `false` exercises the failed
        /// path.
        pub fn with_step_failure(
            fail_at: &'static str,
            rollback_succeeds: bool,
        ) -> Self {
            Self {
                carried_level: 50,
                effective_level: 50,
                fail_at: Some(fail_at),
                rollback_succeeds,
                steps_called: Mutex::new(Vec::new()),
            }
        }

        /// Return the step-invocation order captured during
        /// the run.
        pub fn steps(&self) -> Vec<&'static str> {
            self.steps_called.lock().unwrap().clone()
        }

        fn record(&self, step: &'static str) {
            self.steps_called.lock().unwrap().push(step);
        }

        fn maybe_fail(&self, step: &'static str) -> Result<(), String> {
            if self.fail_at == Some(step) {
                Err(format!("stub-failure-at-{step}"))
            } else {
                Ok(())
            }
        }
    }

    impl TransitionExecutor for StubTransitionExecutor {
        fn read_carried_level<'a>(
            &'a self,
            _from: MixerType,
        ) -> Pin<Box<dyn Future<Output = Result<u8, String>> + Send + 'a>>
        {
            Box::pin(async move {
                self.record("read_carried_level");
                self.maybe_fail("read_carried_level")?;
                Ok(self.carried_level)
            })
        }

        fn pre_mute<'a>(
            &'a self,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>
        {
            Box::pin(async move {
                self.record("pre_mute");
                self.maybe_fail("pre_mute")
            })
        }

        fn set_new_authority<'a>(
            &'a self,
            _to: MixerType,
            _carried_level: u8,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>
        {
            Box::pin(async move {
                self.record("set_new_authority");
                self.maybe_fail("set_new_authority")
            })
        }

        fn switch_fragment<'a>(
            &'a self,
            _to: MixerType,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>
        {
            Box::pin(async move {
                self.record("switch_fragment");
                self.maybe_fail("switch_fragment")
            })
        }

        fn restart_playback<'a>(
            &'a self,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>
        {
            Box::pin(async move {
                self.record("restart_playback");
                self.maybe_fail("restart_playback")
            })
        }

        fn verify_effective_level<'a>(
            &'a self,
            _expected: u8,
        ) -> Pin<Box<dyn Future<Output = Result<u8, String>> + Send + 'a>>
        {
            Box::pin(async move {
                self.record("verify_effective_level");
                self.maybe_fail("verify_effective_level")?;
                Ok(self.effective_level)
            })
        }

        fn unmute<'a>(
            &'a self,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>
        {
            Box::pin(async move {
                self.record("unmute");
                self.maybe_fail("unmute")
            })
        }

        fn rollback<'a>(
            &'a self,
            _from: MixerType,
            at_phase: &'static str,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>
        {
            Box::pin(async move {
                self.record("rollback");
                if self.rollback_succeeds {
                    Ok(())
                } else {
                    Err(format!("stub-rollback-failure-from-{at_phase}"))
                }
            })
        }
    }

    /// Lifecycle happening emission for assertion. Captures
    /// each emitted event in order.
    #[derive(Default)]
    pub struct CapturingTransitionHappener {
        events: Mutex<Vec<CapturedEvent>>,
    }

    /// One lifecycle happening captured by
    /// [`CapturingTransitionHappener`] in tests. Mirrors the
    /// four `audio.mixer_transition.*` happenings the
    /// orchestrator emits at runtime.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum CapturedEvent {
        /// `audio.mixer_transition.started`.
        Started {
            /// Prior authority.
            from: MixerType,
            /// Target authority.
            to: MixerType,
        },
        /// `audio.mixer_transition.applied`.
        Applied {
            /// Prior authority.
            from: MixerType,
            /// Target authority.
            to: MixerType,
            /// Carried level (step 1 readback).
            carried_level: u8,
            /// Effective level (step 6 readback).
            effective_level: u8,
        },
        /// `audio.mixer_transition.rolled_back`.
        RolledBack {
            /// Prior authority.
            from: MixerType,
            /// Target authority (never reached).
            to: MixerType,
            /// Phase at which the original failure occurred.
            at_phase: &'static str,
            /// Operator-readable reason.
            reason: String,
        },
        /// `audio.mixer_transition.failed`.
        Failed {
            /// Prior authority.
            from: MixerType,
            /// Target authority (never reached; chain muted).
            to: MixerType,
            /// Phase at which the original failure occurred.
            at_phase: &'static str,
            /// Operator-readable reason.
            reason: String,
        },
    }

    impl CapturingTransitionHappener {
        /// Return the events captured during the run, in
        /// emission order.
        pub fn events(&self) -> Vec<CapturedEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    impl TransitionHappener for CapturingTransitionHappener {
        fn emit_started<'a>(
            &'a self,
            from: MixerType,
            to: MixerType,
            _at_ms: u64,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
            Box::pin(async move {
                self.events
                    .lock()
                    .unwrap()
                    .push(CapturedEvent::Started { from, to });
            })
        }

        fn emit_applied<'a>(
            &'a self,
            from: MixerType,
            to: MixerType,
            carried_level: u8,
            effective_level: u8,
            _at_ms: u64,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
            Box::pin(async move {
                self.events.lock().unwrap().push(CapturedEvent::Applied {
                    from,
                    to,
                    carried_level,
                    effective_level,
                });
            })
        }

        fn emit_rolled_back<'a>(
            &'a self,
            from: MixerType,
            to: MixerType,
            at_phase: &'static str,
            reason: String,
            _at_ms: u64,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
            Box::pin(async move {
                self.events.lock().unwrap().push(CapturedEvent::RolledBack {
                    from,
                    to,
                    at_phase,
                    reason,
                });
            })
        }

        fn emit_failed<'a>(
            &'a self,
            from: MixerType,
            to: MixerType,
            at_phase: &'static str,
            reason: String,
            _at_ms: u64,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
            Box::pin(async move {
                self.events.lock().unwrap().push(CapturedEvent::Failed {
                    from,
                    to,
                    at_phase,
                    reason,
                });
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    async fn run(
        from: MixerType,
        to: MixerType,
        executor: Arc<StubTransitionExecutor>,
    ) -> (TransitionOutcome, Arc<CapturingTransitionHappener>) {
        let happener: Arc<CapturingTransitionHappener> =
            Arc::new(CapturingTransitionHappener::default());
        let happener_dyn: Arc<dyn TransitionHappener> =
            happener.clone() as Arc<dyn TransitionHappener>;
        let outcome = run_transition(
            from,
            to,
            executor.clone() as Arc<dyn TransitionExecutor>,
            happener_dyn,
        )
        .await;
        (outcome, happener)
    }

    #[tokio::test]
    async fn happy_path_emits_started_then_applied_in_order() {
        let exec = Arc::new(StubTransitionExecutor::happy(50, 50));
        let (outcome, happener) =
            run(MixerType::Software, MixerType::Hardware, exec.clone()).await;
        assert!(matches!(
            outcome,
            TransitionOutcome::Applied {
                effective_level: 50
            }
        ));
        let events = happener.events();
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(matches!(events[0], CapturedEvent::Started { .. }));
        assert!(matches!(events[1], CapturedEvent::Applied { .. }));
    }

    #[tokio::test]
    async fn happy_path_runs_steps_in_invariant_order() {
        // I2 (no blast): unmute is the LAST step. Verify the
        // step sequence captures pre_mute BEFORE
        // set_new_authority and unmute AFTER
        // verify_effective_level.
        let exec = Arc::new(StubTransitionExecutor::happy(50, 50));
        let _ =
            run(MixerType::Software, MixerType::Hardware, exec.clone()).await;
        let steps = exec.steps();
        assert_eq!(
            steps,
            vec![
                "read_carried_level",
                "pre_mute",
                "set_new_authority",
                "switch_fragment",
                "restart_playback",
                "verify_effective_level",
                "unmute",
            ],
            "step order must honour the mixer-transition invariants"
        );
    }

    #[tokio::test]
    async fn no_op_when_from_equals_to_emits_nothing() {
        let exec = Arc::new(StubTransitionExecutor::happy(0, 0));
        let (outcome, happener) =
            run(MixerType::Software, MixerType::Software, exec.clone()).await;
        assert!(matches!(outcome, TransitionOutcome::NoOp));
        assert!(happener.events().is_empty());
        assert!(exec.steps().is_empty(), "no executor steps for no-op");
    }

    #[tokio::test]
    async fn failure_at_pre_mute_rolls_back_and_unmute_is_unreachable() {
        // I2 (no blast): unmute MUST NOT run when an earlier
        // step failed. Pre-mute failure exercises the
        // earliest rollback path.
        let exec = Arc::new(StubTransitionExecutor::with_step_failure(
            "pre_mute", true,
        ));
        let (outcome, happener) =
            run(MixerType::Software, MixerType::Hardware, exec.clone()).await;
        match outcome {
            TransitionOutcome::RolledBack { at_phase, .. } => {
                assert_eq!(at_phase, "pre_mute");
            }
            other => panic!("expected RolledBack, got {other:?}"),
        }
        let steps = exec.steps();
        assert!(steps.contains(&"pre_mute"));
        assert!(steps.contains(&"rollback"));
        assert!(
            !steps.contains(&"unmute"),
            "unmute is unreachable when an earlier step failed: {steps:?}"
        );
        // I5: rolled_back lifecycle happening fires; failed does NOT.
        let evs = happener.events();
        assert!(evs
            .iter()
            .any(|e| matches!(e, CapturedEvent::RolledBack { .. })));
        assert!(!evs
            .iter()
            .any(|e| matches!(e, CapturedEvent::Failed { .. })));
    }

    #[tokio::test]
    async fn failure_at_verify_does_not_unmute_and_rolls_back() {
        // Even when six steps have already succeeded, a
        // verify failure must NOT unmute.
        let exec = Arc::new(StubTransitionExecutor::with_step_failure(
            "verify_effective_level",
            true,
        ));
        let (outcome, _) =
            run(MixerType::Software, MixerType::Hardware, exec.clone()).await;
        assert!(matches!(
            outcome,
            TransitionOutcome::RolledBack {
                at_phase: "verify_effective_level",
                ..
            }
        ));
        assert!(
            !exec.steps().contains(&"unmute"),
            "verify failure must not be followed by unmute"
        );
    }

    #[tokio::test]
    async fn rollback_failure_escalates_to_failed_terminal_state() {
        // I5: if rollback itself fails, the transition
        // terminates in `failed` and the chain is left
        // muted (the rollback's incomplete return path is
        // documented in the contract).
        let exec = Arc::new(StubTransitionExecutor::with_step_failure(
            "set_new_authority",
            false,
        ));
        let (outcome, happener) =
            run(MixerType::Software, MixerType::Hardware, exec.clone()).await;
        match outcome {
            TransitionOutcome::Failed { at_phase, reason } => {
                assert_eq!(at_phase, "set_new_authority");
                assert!(reason.contains("rollback failed"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        // The `failed` lifecycle happening fires; rolled_back
        // does NOT.
        let evs = happener.events();
        assert!(evs
            .iter()
            .any(|e| matches!(e, CapturedEvent::Failed { .. })));
        assert!(!evs
            .iter()
            .any(|e| matches!(e, CapturedEvent::RolledBack { .. })));
    }

    #[tokio::test]
    async fn applied_happening_carries_post_transition_effective_level() {
        // I6 (operator truth): the `applied` happening's
        // payload includes the effective level so the UI's
        // display tracks the new authority's value, not the
        // pre-transition slider.
        let exec = Arc::new(StubTransitionExecutor::happy(50, 47));
        let (_, happener) =
            run(MixerType::Software, MixerType::Hardware, exec.clone()).await;
        let evs = happener.events();
        let applied = evs
            .iter()
            .find_map(|e| {
                if let CapturedEvent::Applied {
                    carried_level,
                    effective_level,
                    ..
                } = e
                {
                    Some((*carried_level, *effective_level))
                } else {
                    None
                }
            })
            .expect("Applied event present");
        assert_eq!(applied, (50, 47));
    }

    #[tokio::test]
    async fn lifecycle_events_are_mutually_exclusive_per_transition() {
        // The contract pins applied xor rolled_back xor
        // failed per `started`. Three runs (happy / rollback
        // / failed) each emit exactly ONE terminal event.
        for (fail_at, rb_ok, expect_kind) in [
            (None, true, "applied"),
            (Some("pre_mute"), true, "rolled_back"),
            (Some("pre_mute"), false, "failed"),
        ] {
            let exec = Arc::new(match fail_at {
                None => StubTransitionExecutor::happy(50, 50),
                Some(p) => StubTransitionExecutor::with_step_failure(p, rb_ok),
            });
            let (_, happener) =
                run(MixerType::Software, MixerType::Hardware, exec).await;
            let evs = happener.events();
            let terminals = evs
                .iter()
                .filter(|e| {
                    matches!(
                        e,
                        CapturedEvent::Applied { .. }
                            | CapturedEvent::RolledBack { .. }
                            | CapturedEvent::Failed { .. }
                    )
                })
                .count();
            assert_eq!(
                terminals, 1,
                "exactly one terminal lifecycle event per transition ({expect_kind}); got {evs:?}"
            );
        }
    }
}
