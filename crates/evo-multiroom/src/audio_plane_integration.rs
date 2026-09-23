// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Bridge between [`evo::domain_witness::
//! DomainWitnessRuntime`] and [`crate::audio_plane::
//! AudioPlaneRuntime`].
//!
//! The chain runtime is wire-agnostic — it calls into the
//! `WitnessBroadcaster` and `ChainRequester` traits without
//! knowing what transport delivers the bytes. This module
//! provides the production binding: a thin wrapper around
//! `Arc<AudioPlaneRuntime>` that satisfies both traits by
//! composing the appropriate [`crate::audio_plane::
//! AudioPlaneMessage`] variant and dispatching via the
//! audio-plane's peer-write channels.
//!
//! Trait methods are synchronous (the runtime's append
//! path is on the hot operator-gesture path and cannot
//! block on a peer ack); this impl spawns a Tokio task
//! per call to bridge into the async send. The audio-plane
//! retains best-effort semantics — a queued message either
//! lands on the peer's wire or is silently dropped when
//! the peer disconnects mid-flight, which is consistent
//! with the broader "chain is the durable record;
//! delivery is best-effort across carriers" substrate
//! invariant.

use std::sync::Arc;

use evo_witness::DomainWitness;

use crate::audio_plane::{AudioPlaneMessage, AudioPlaneRuntime};
use evo::domain_witness::runtime::{ChainRequester, WitnessBroadcaster};

/// Production [`WitnessBroadcaster`] + [`ChainRequester`]
/// impl backed by the audio-plane control channel.
///
/// One instance per runtime; bind via
/// [`evo::domain_witness::DomainWitnessRuntime::
/// with_broadcaster`] and
/// [`evo::domain_witness::DomainWitnessRuntime::
/// with_requester`] at boot time.
pub struct AudioPlaneWitnessBroadcaster {
    runtime: Arc<AudioPlaneRuntime>,
}

impl AudioPlaneWitnessBroadcaster {
    /// Construct a bridge around the supplied audio-plane
    /// runtime.
    pub fn new(runtime: Arc<AudioPlaneRuntime>) -> Self {
        Self { runtime }
    }
}

impl WitnessBroadcaster for AudioPlaneWitnessBroadcaster {
    fn broadcast_witness(&self, witness: &DomainWitness) {
        let witness = witness.clone();
        let runtime = Arc::clone(&self.runtime);
        tokio::spawn(async move {
            let msg = AudioPlaneMessage::DomainWitnessAnnounce { witness };
            runtime.broadcast_to_peers(msg).await;
        });
    }
}

impl ChainRequester for AudioPlaneWitnessBroadcaster {
    fn request_tail_from_peer(&self, peer_id: &str, from_hash: &str) {
        let peer_id = peer_id.to_string();
        let from_hash = from_hash.to_string();
        let runtime = Arc::clone(&self.runtime);
        tokio::spawn(async move {
            let msg = AudioPlaneMessage::DomainWitnessRequest {
                from_hash_b64: from_hash,
            };
            runtime.send_to_peer(&peer_id, msg).await;
        });
    }
}
