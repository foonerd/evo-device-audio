// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Structured per-case report.
//!
//! Every case the harness drives emits one [`CaseResult`] as
//! a JSON line on stdout. A CI gate consumes the stream and
//! decides pass/fail per case. The harness's process exit
//! code is 0 iff every case is `Pass`.

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Pass,
    Fail { reason: String },
    Skip { reason: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct CaseResult {
    /// Stable case id; survives reordering.
    pub id: String,
    /// Shelf the case exercises (`audio.queue`, `audio.playlist`,
    /// `audio.favourites`, `audio.library`, or `cross_cutting`
    /// for scenarios that span shelves).
    pub shelf: String,
    /// Human-readable label.
    pub label: String,
    /// Per-case outcome.
    #[serde(flatten)]
    pub outcome: Outcome,
    /// Optional per-case context — observed state, diffed
    /// values, captured envelopes. Included on every case so
    /// the CI consumer can render a richer audit trail than
    /// pass/fail alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Value>,
}

impl CaseResult {
    pub fn pass(
        id: impl Into<String>,
        shelf: impl Into<String>,
        label: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            shelf: shelf.into(),
            label: label.into(),
            outcome: Outcome::Pass,
            context: None,
        }
    }

    pub fn pass_with(
        id: impl Into<String>,
        shelf: impl Into<String>,
        label: impl Into<String>,
        context: Value,
    ) -> Self {
        Self {
            id: id.into(),
            shelf: shelf.into(),
            label: label.into(),
            outcome: Outcome::Pass,
            context: Some(context),
        }
    }

    pub fn fail(
        id: impl Into<String>,
        shelf: impl Into<String>,
        label: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            shelf: shelf.into(),
            label: label.into(),
            outcome: Outcome::Fail {
                reason: reason.into(),
            },
            context: None,
        }
    }

    pub fn fail_with(
        id: impl Into<String>,
        shelf: impl Into<String>,
        label: impl Into<String>,
        reason: impl Into<String>,
        context: Value,
    ) -> Self {
        Self {
            id: id.into(),
            shelf: shelf.into(),
            label: label.into(),
            outcome: Outcome::Fail {
                reason: reason.into(),
            },
            context: Some(context),
        }
    }

    pub fn skip(
        id: impl Into<String>,
        shelf: impl Into<String>,
        label: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            shelf: shelf.into(),
            label: label.into(),
            outcome: Outcome::Skip {
                reason: reason.into(),
            },
            context: None,
        }
    }

    pub fn is_pass(&self) -> bool {
        matches!(self.outcome, Outcome::Pass)
    }

    pub fn is_fail(&self) -> bool {
        matches!(self.outcome, Outcome::Fail { .. })
    }

    /// Emit this case as a JSON line on stdout.
    pub fn emit(&self) {
        match serde_json::to_string(self) {
            Ok(line) => println!("{line}"),
            Err(e) => eprintln!(
                "{{\"id\":\"{}\",\"outcome\":\"fail\",\"reason\":\"serialise: {e}\"}}",
                self.id
            ),
        }
    }
}

/// Final summary line emitted after every case has run.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
}

impl Summary {
    pub fn from(cases: &[CaseResult]) -> Self {
        let total = cases.len();
        let passed = cases.iter().filter(|c| c.is_pass()).count();
        let failed = cases.iter().filter(|c| c.is_fail()).count();
        let skipped = total - passed - failed;
        Self {
            total,
            passed,
            failed,
            skipped,
        }
    }

    pub fn emit(&self) {
        match serde_json::to_string(self) {
            Ok(line) => println!("{{\"summary\":{line}}}"),
            Err(_) => println!(
                "{{\"summary\":{{\"total\":{},\"passed\":{},\"failed\":{},\"skipped\":{}}}}}",
                self.total, self.passed, self.failed, self.skipped
            ),
        }
    }

    pub fn all_passed(&self) -> bool {
        self.failed == 0 && self.skipped == 0
    }
}
