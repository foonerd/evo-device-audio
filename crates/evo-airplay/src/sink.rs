// Copyright (c) 2026 Stefan Arentz; modifications (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0 AND MIT
//! The seam between the AirPlay session and the host's audio output.
//!
//! The library owns network → PCM (pairing, decrypt, decode, the queue and
//! its pause/flush gating); the host owns PCM → speaker. A host provides an
//! [`AudioSink`] and the library's playback thread feeds it interleaved
//! `i16` samples that should actually play — audio is withheld while paused
//! and pre-seek audio is dropped before it ever reaches the sink.

use std::fmt;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Where decoded audio goes. Implemented by the host (the receiver binary's
/// sink is ALSA), called from a dedicated library-managed playback thread.
pub trait AudioSink: Send + 'static {
    /// Play interleaved `i16` PCM. `write` may block — blocking is the pacing
    /// mechanism: the sink drains at the hardware's rate, the playback thread
    /// waits on it, and the TCP reader backpressures on the queue behind it.
    fn write(&mut self, pcm: &[i16]);

    /// Seek/skip (or pause): immediately drop anything the sink has queued or
    /// buffered of its own (hardware buffers, prebuffer cushions). The
    /// library has already dropped its queued PCM when this is called.
    fn flush(&mut self);

    /// A flag the library sets when the session is over and the playback
    /// thread is about to be joined. A sink whose [`write`](Self::write) can
    /// block for as long as its device stays idle (a capture that nobody is
    /// reading, for example) returns the flag it polls between bounded waits,
    /// so the join always completes. A sink that always returns promptly
    /// leaves the default, `None`.
    fn stop_signal(&self) -> Option<Arc<AtomicBool>> {
        None
    }
}

/// Why a sink could not be created. The session is refused with the reason
/// in the log; the sender is told the service is unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkError(String);

impl SinkError {
    /// A sink failure with a human-readable reason.
    pub fn new(reason: impl Into<String>) -> SinkError {
        SinkError(reason.into())
    }
}

impl fmt::Display for SinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SinkError {}

/// Creates the sink for a stream, invoked at `SETUP` phase 2 with the
/// negotiated sample rate and channel count — once per stream. An error
/// refuses the session: the library never accepts a stream it cannot play.
pub type SinkFactory =
    Arc<dyn Fn(u32, u8) -> Result<Box<dyn AudioSink>, SinkError> + Send + Sync>;
