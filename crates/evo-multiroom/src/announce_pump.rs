// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Announce-pump task: consumes UDP-broadcast announce
//! observations and triggers chain reconciliation when a
//! peer's chain head differs from local.
//!
//! Without this pump, peers exchange head hashes on the
//! UDP carrier but never reconcile when they diverge — the
//! announce delivers awareness, not entries. This pump
//! closes the loop: when an inbound announce reports a
//! head different from local, dial the originator's
//! audio-plane endpoint and request the chain tail via a
//! `DomainWitnessRequest`. The audio-plane connection +
//! the inbound pump then carry the tail back to the local
//! runtime.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast::error::RecvError;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

use crate::audio_plane::{AudioPlaneMessage, AudioPlaneRuntime};
use evo::domain_witness::announce::MultiCarrierAnnounceRuntime;
use evo::domain_witness::runtime::DomainWitnessRuntime;

/// Long-running task draining announce observations into
/// chain-reconciliation requests.
pub struct AnnouncePump {
    handle: JoinHandle<()>,
}

impl AnnouncePump {
    /// Spawn the pump task. Returns immediately; the task
    /// runs until the announce channel closes.
    pub fn spawn(
        audio_plane: Arc<AudioPlaneRuntime>,
        announce_runtime: Arc<MultiCarrierAnnounceRuntime>,
        witness_runtime: Arc<DomainWitnessRuntime>,
    ) -> Self {
        let mut receiver = announce_runtime.subscribe_announce_inbound();
        // Track which addresses we've already dialled this
        // session so the pump doesn't re-dial on every
        // 1Hz announce. Dial happens once; subsequent
        // divergence triggers a request over the existing
        // connection.
        let dialled: Arc<AsyncMutex<HashSet<std::net::SocketAddr>>> =
            Arc::new(AsyncMutex::new(HashSet::new()));
        let handle = tokio::spawn(async move {
            loop {
                match receiver.recv().await {
                    Ok(observation) => {
                        // Compare announced head to local. If
                        // identical, nothing to do.
                        let local_head = witness_runtime.chain_head_b64();
                        if observation.envelope.chain_head_b64 == local_head {
                            continue;
                        }
                        // Compose the originator's audio-plane
                        // socket address. The announce envelope
                        // endpoints carry the port; the
                        // source_addr carries the resolved IP.
                        let port = observation
                            .envelope
                            .endpoints
                            .first()
                            .map(|e| e.port)
                            .unwrap_or(7331);
                        let addr = std::net::SocketAddr::new(
                            observation.source_addr.ip(),
                            port,
                        );
                        // Dial once per peer address; subsequent
                        // divergences ride the existing
                        // connection.
                        let needs_dial = {
                            let mut g = dialled.lock().await;
                            if g.contains(&addr) {
                                false
                            } else {
                                g.insert(addr);
                                true
                            }
                        };
                        if needs_dial {
                            // dial_peer blocks for the
                            // connection's lifetime. Spawn it
                            // so the pump can continue.
                            let dial_runtime = Arc::clone(&audio_plane);
                            let dialled_handle = Arc::clone(&dialled);
                            tokio::spawn(async move {
                                if let Err(e) =
                                    dial_runtime.dial_peer(addr).await
                                {
                                    tracing::debug!(
                                        error = %e,
                                        addr = %addr,
                                        "announce pump: dial returned"
                                    );
                                    // Connection ended; allow
                                    // re-dial on next divergence.
                                    dialled_handle.lock().await.remove(&addr);
                                }
                            });
                            // Give the handshake a moment to
                            // complete before sending the
                            // request.
                            tokio::time::sleep(Duration::from_millis(500))
                                .await;
                        }
                        // Send a tail request. Receivers reply
                        // with whatever tail they hold from our
                        // current local head; mismatched-hash
                        // responses arrive as orphans and are
                        // safely ignored by the chain runtime's
                        // prev_hash check.
                        let request = AudioPlaneMessage::DomainWitnessRequest {
                            from_hash_b64: local_head,
                        };
                        let delivered =
                            audio_plane.broadcast_to_peers(request).await;
                        if delivered == 0 {
                            tracing::debug!(
                                addr = %addr,
                                "announce pump: no audio-plane peers \
                                 connected yet; tail request did not flow"
                            );
                        }
                    }
                    Err(RecvError::Lagged(_)) => continue,
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
