// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Inbound-pump task: bridges the audio-plane's
//! `DomainWitnessInbound` broadcast channel into the
//! domain-witness runtime.
//!
//! The audio-plane decodes inbound chain messages
//! (`DomainWitnessAnnounce`, `DomainWitnessRequest`,
//! `DomainWitnessResponse`) into `DomainWitnessInbound`
//! events on its broadcast channel. Without a consumer
//! task draining that channel into the runtime, the
//! decoded events fall on the floor. This module's
//! `InboundPump` spawns a long-running task that:
//!
//! - On `Witness` event: applies the witness via
//!   `DomainWitnessRuntime::receive_remote_witness`. On
//!   stale-chain error, the runtime's chain-requester
//!   hook fires automatically to ask the sender for the
//!   tail.
//! - On `TailRequest` event: composes the chain tail
//!   starting after `from_hash_b64` and sends a
//!   `DomainWitnessResponse` back to the requesting peer.
//! - On `TailResponse` event: applies each witness in
//!   order via `apply_response_batch`.
//!
//! Drop-the-task semantics: when the audio-plane channel
//! reports `Lagged`, the pump continues (the next
//! announce will resync state). When the channel closes
//! (shutdown), the pump exits.

use std::sync::Arc;

use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

use crate::audio_plane::{
    AudioPlaneMessage, AudioPlaneRuntime, DomainWitnessInbound,
};
use evo::domain_witness::runtime::DomainWitnessRuntime;

/// Long-running task draining domain-witness events from
/// the audio-plane into the chain runtime.
pub struct InboundPump {
    handle: JoinHandle<()>,
}

impl InboundPump {
    /// Spawn the pump task. Returns immediately; the task
    /// runs until the audio-plane channel closes.
    pub fn spawn(
        audio_plane: Arc<AudioPlaneRuntime>,
        witness_runtime: Arc<DomainWitnessRuntime>,
    ) -> Self {
        let mut receiver = audio_plane.subscribe_domain_witness_inbound();
        let handle = tokio::spawn(async move {
            loop {
                match receiver.recv().await {
                    Ok(DomainWitnessInbound::Witness {
                        from_peer_id,
                        witness,
                    }) => {
                        if let Err(e) = witness_runtime
                            .receive_remote_witness(witness, &from_peer_id)
                            .await
                        {
                            tracing::debug!(
                                peer = %from_peer_id,
                                error = %e,
                                "domain witness inbound pump: apply failed"
                            );
                        }
                    }
                    Ok(DomainWitnessInbound::TailRequest {
                        from_peer_id,
                        from_hash_b64,
                    }) => {
                        let tail = witness_runtime.tail_after(&from_hash_b64);
                        if !tail.is_empty() {
                            let msg =
                                AudioPlaneMessage::DomainWitnessResponse {
                                    witnesses: tail,
                                };
                            let _ = audio_plane
                                .send_to_peer(&from_peer_id, msg)
                                .await;
                        }
                    }
                    Ok(DomainWitnessInbound::TailResponse {
                        from_peer_id,
                        witnesses,
                    }) => {
                        if let Err(e) = witness_runtime
                            .apply_response_batch(witnesses, &from_peer_id)
                            .await
                        {
                            tracing::debug!(
                                peer = %from_peer_id,
                                error = %e,
                                "domain witness inbound pump: tail-response \
                                 apply failed"
                            );
                        }
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        tracing::warn!(
                            skipped = skipped,
                            "domain witness inbound pump: lagged, resync via \
                             next announce"
                        );
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        });
        Self { handle }
    }

    /// Abort the pump task. Idempotent.
    pub fn shutdown(&self) {
        self.handle.abort();
    }
}
