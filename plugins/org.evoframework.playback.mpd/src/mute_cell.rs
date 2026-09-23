// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! The operator mute flag lives here, not in MPD.
//!
//! Three now_playing publishers used to guess `false`:
//! the custody supervisor, the ambient idle observer, and
//! the queue shelf after a transport verb. A skip or a
//! tap then painted the hero surface unmuted while the
//! speaker was still silent. One cell, three readers,
//! the supervisor writes on `set_mute`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Shared mute flag for every now_playing publisher.
#[derive(Clone, Debug)]
pub(crate) struct MuteCell {
    muted: Arc<AtomicBool>,
}

impl MuteCell {
    /// Sessions start unmuted.
    pub(crate) fn new() -> Self {
        Self {
            muted: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The operator's current mute intent.
    pub(crate) fn is_muted(&self) -> bool {
        self.muted.load(Ordering::SeqCst)
    }

    /// Record the operator's mute intent. Only `set_mute`
    /// writes this.
    pub(crate) fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::SeqCst);
    }
}
