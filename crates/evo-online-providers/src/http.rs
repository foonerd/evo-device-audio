// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Canonical HTTPS client factory for the audio distribution's
//! online-metadata plugins.
//!
//! One [`reqwest::Client`] per plugin load, shared across every
//! outbound call the plugin makes. Connection pool + DNS cache
//! reuse across the provider cascade — no per-call TLS
//! handshake, no per-call DNS lookup.

use std::time::Duration;

use reqwest::Client;

/// Build the shared HTTPS client with the distribution's
/// canonical posture: bounded timeout, redirect ceiling, rustls
/// backend (cross-compile clean, no system OpenSSL dependency).
///
/// Callers pass their per-plugin `timeout` — the value from the
/// operator's plugin config so overall latency stays under the
/// framework's coalescer wait deadline.
///
/// Redirect ceiling is 5, matching artwork.online's original
/// posture: Cover Art Archive returns a 307 chain from
/// `coverartarchive.org/release/{mbid}/front` to a MetaBrainz
/// image CDN; keeping the ceiling generous lets that chain
/// terminate without a spurious refusal, while still bounding
/// pathological redirect loops.
pub fn build_http_client(timeout: Duration) -> Client {
    Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        // Build failures here are framework-level configuration
        // errors (TLS init, threadpool spawn); refusing to admit
        // is the right shape. Panic here surfaces to the
        // plugin's load() return via a caught panic in the SDK.
        .expect("reqwest client builder")
}
