// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Just a Nerd

//! The one error type: a stable machine code and a human detail.

use std::fmt;

/// A refusal or failure. `code` is stable and is what the surface reads;
/// `detail` is for the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdError {
    /// Stable snake-case reason, such as `signature_invalid`.
    pub code: &'static str,
    /// Human-readable detail. Never holds a secret.
    pub detail: String,
}

impl UpdError {
    /// Build an error from a code and a detail.
    pub fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for UpdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for UpdError {}

/// Shorthand result.
pub type Res<T> = Result<T, UpdError>;

/// Map an I/O error to an [`UpdError`] with the given code.
pub fn io_err(code: &'static str, what: &str, e: std::io::Error) -> UpdError {
    UpdError::new(code, format!("{what}: {e}"))
}
