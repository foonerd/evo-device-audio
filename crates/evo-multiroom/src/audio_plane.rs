// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Network audio plane for multi-room groups.
//!
//! TCP-based control + data channel between source-host and
//! group receivers, plus the network-coordinated heartbeat
//! and NTP-lite sync protocol that resolve split-brain and
//! feed [`evo::clock_sync::ClockSyncRuntime::record_sync_sample`].
//!
//! ## Protocol
//!
//! Length-prefixed JSON envelope (4-byte big-endian frame
//! length followed by serde-tagged
//! [`AudioPlaneMessage`]). Matches the framing convention
//! the steward's client API already uses on its Unix socket;
//! a TCP variant of that frame format runs here.
//!
//! On connection, both sides exchange [`AudioPlaneMessage::Hello`]
//! carrying the canonical device id, framework version, and
//! the set of group ids the local node believes that node
//! is the elected source-host for. The receiver of a Hello
//! cross-checks against its own election state — disagreement
//! is the [`evo::happenings::Happening::SplitBrainDetected`]
//! signal, deterministically resolved by the same lowest-
//! canonical-id-wins rule the local-view election uses.
//!
//! Periodic [`AudioPlaneMessage::Heartbeat`] frames flow in
//! both directions on every active connection. Missing
//! heartbeats past the configured timeout window mark the
//! peer offline and tear the connection down; the local
//! source-host election runtime re-evaluates against the
//! new view.
//!
//! Receivers run a periodic NTP-lite sync against their
//! source-host. Each sync exchange is one
//! [`AudioPlaneMessage::SyncProbe`] / `SyncResponse` round-
//! trip. The receiver computes the half-round-trip offset
//! and writes it through to the clock-sync runtime via
//! `record_sync_sample`.
//!
//! [`AudioPlaneMessage::AudioFrame`] envelopes the typed
//! audio frame the source-host fans out to receivers. The
//! payload bytes are codec-opaque at this layer; the
//! source-host encodes per the active topology's chain and
//! the receiver decodes through its own delivery chain.
//!
//! ## Current substrate scope
//!
//! This sub-primitive lands the wire substrate: framing,
//! connection lifecycle, heartbeat, sync protocol, split-
//! brain detection, operator observability. Subsequent
//! sub-primitives layer on top:
//!
//! - UDP transport + FEC + adaptive buffering + jitter
//!   compensation are transport-layer optimisations that
//!   ride observed-in-the-field traffic patterns; the TCP
//!   substrate here is the v1 baseline.
//! - The receiver-side wiring of `AudioFrame` payloads into
//!   the local audio delivery chain rides the audio
//!   delivery plugin's load context — the framework's
//!   transport substrate accepts and counts frames; the
//!   plugin claims them when one admits.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, Mutex as AsyncMutex};
use tokio::task::JoinHandle;

use evo_primitives::DeviceId;

// Response shape of `list_audio_plane_connections`, a wire op the
// steward owns and keeps registered with or without a plane. The
// plane fills it; it does not define it.
use evo::server::{
    ConnectionDirection, PeerConnectionInfo, PeerConnectionState,
};

use evo::clock_sync::ClockSyncRuntime;
use evo::discovery::DiscoveryRuntime;
use evo::groups::GroupStore;
use evo::happenings::{Happening, HappeningBus};

/// Maximum frame body length the protocol accepts. 1 MiB is
/// large enough for any realistic audio frame plus envelope;
/// rejecting larger frames protects the runtime from a
/// peer mis-announcing a length and forcing a multi-MB
/// allocation.
const MAX_FRAME_BYTES: usize = 1_048_576;

/// Wire-protocol message exchanged between peers on the
/// audio-plane TCP channel. Tagged via serde's `type` field
/// so unknown variants surface as decode errors rather than
/// silent drops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AudioPlaneMessage {
    /// Initial handshake. Carries the sender's canonical
    /// device id, framework version string, and the set of
    /// group ids the sender believes itself to be the
    /// elected source-host for.
    Hello {
        /// Sender's canonical UUIDv4 device id.
        device_id: String,
        /// Sender's framework version string.
        framework_version: String,
        /// Group ids the sender believes itself to be source-
        /// host for. Empty when the sender is purely a
        /// receiver.
        claimed_source_host_groups: Vec<String>,
    },
    /// Periodic liveness signal. Sent in both directions.
    Heartbeat {
        /// Sender's canonical device id (echoed for cross-
        /// check; should match the connection's recorded
        /// remote id).
        device_id: String,
        /// Sender's local monotonic millisecond clock at the
        /// time the heartbeat was sent.
        monotonic_ms: u64,
        /// Group ids the sender currently considers active
        /// for this connection.
        groups: Vec<String>,
    },
    /// Receiver-initiated sync probe. The sender records its
    /// local monotonic clock as `t1_ms` immediately before
    /// transmission.
    SyncProbe {
        /// Group this sync sample applies to.
        group_id: String,
        /// Receiver's local monotonic ms at probe send.
        t1_ms: u64,
    },
    /// Source-host's response to a sync probe. Carries an
    /// echo of the receiver's `t1_ms` plus the source-host's
    /// local monotonic ms at the moment the probe was
    /// received.
    SyncResponse {
        /// Group the sample applies to (echo of probe).
        group_id: String,
        /// Receiver's `t1_ms` (echo).
        t1_ms: u64,
        /// Source-host's local monotonic ms when probe was
        /// received.
        t2_ms: u64,
    },
    /// Audio data frame emitted by the source-host. Codec-
    /// opaque payload — the source-host encodes per the
    /// active topology's chain and the receiver decodes
    /// through its own delivery chain. The current substrate
    /// ships the envelope; receiver-side render integration
    /// rides a later sub-primitive when an audio delivery
    /// plugin admits.
    AudioFrame {
        /// Group this frame is fanning out to.
        group_id: String,
        /// Monotonically-increasing per-group sequence so
        /// receivers detect drops and out-of-order arrival.
        sequence: u64,
        /// Source-host's local monotonic ms at which the
        /// frame should be played by every receiver — the
        /// scheduling target the synchronised-playback
        /// alignment is built on.
        presentation_time_ms: u64,
        /// Codec discriminator (e.g. `"pcm_s16_le"`,
        /// `"pcm_s24_le"`, `"opus"`).
        codec: String,
        /// Sample rate in Hz.
        rate_hz: u32,
        /// Channel count.
        channels: u16,
        /// URL-safe base64-encoded codec payload bytes.
        /// Base64 keeps the JSON envelope printable and
        /// debuggable; the cost is a ~33% overhead.
        ///
        /// **Retirement criterion (per audit Finding F8):** the
        /// JSON+base64 frame envelope plus per-frame `flush()`
        /// is acceptable v1 substrate at current per-rig load
        /// (3 nodes × 50 Hz × ~50 KB/frame ≈ 7.5 MB/s peak,
        /// well within the steward's TCP stack). It is NOT
        /// acceptable at the engineering-excellence bar for the
        /// target hardware-tier range (ESP32 → Epyc) or for the
        /// MULTIROOM apex showcase (10+ receivers per group).
        /// This envelope retires before any of these — whichever
        /// is first: (a) a scale-validation setup with six or more
        /// receivers in a single group ships; (b) an ESP32-class
        /// follower joins as a full receiver-role plugin (the
        /// JSON parser alone exceeds MCU-class memory budget);
        /// (c) v0.2.x cycle opens.
        /// Replacement shape: length-prefixed binary framing
        /// (CBOR or bare-bytes envelope), payload sent raw
        /// rather than base64, write-batching across the
        /// frame burst rather than per-frame flush.
        payload_b64: String,
    },
    /// Receiver-side back-report of per-frame audible-time
    /// trace fields. Sent by a receiver-role plugin to the
    /// source-host of the named group via the SDK's
    /// `AudioPlaneHandle::report_frame_trace` after each
    /// frame it rendered. The source-host's framework
    /// runtime decodes this message + broadcasts the
    /// derived [`FrameTraceReport`] to subscribed source-
    /// role plugins for aggregation into the published
    /// `audio.multiroom.frame_trace` subject.
    FrameTraceReport {
        /// Group the reported frame belonged to.
        group_id: String,
        /// Per-group sequence the source-host assigned.
        sequence: u64,
        /// Receiver-local monotonic ns at which the
        /// receiver's audio-plane decoded the AudioFrame
        /// envelope.
        wire_recv_ns: u64,
        /// Receiver-local monotonic ns at which the
        /// receiver plugin dequeued the frame from its
        /// render scheduler.
        scheduler_dequeue_ns: u64,
        /// Receiver-local monotonic ns at which the
        /// receiver-side `io.writei(&samples)` returned.
        writei_return_ns: u64,
    },
    /// Graceful disconnect. Sender announces it is leaving
    /// the connection; receiver tears the connection down
    /// without waiting for the heartbeat timeout.
    Goodbye {
        /// Sender's canonical device id.
        device_id: String,
    },
    /// Domain witness chain entry being broadcast to a
    /// peer. Receivers verify the signature, apply if
    /// `prev_hash` matches the local chain head, and
    /// reply via a `DomainWitnessRequest` if the chain
    /// has drifted.
    ///
    /// Carries one witness per message — the receiver's
    /// runtime layers handle ordering and de-duplication
    /// by content-hash.
    DomainWitnessAnnounce {
        /// The signed witness being delivered.
        witness: evo_witness::DomainWitness,
    },
    /// Request from a peer that has fallen behind the
    /// sender's chain head. The peer supplies its current
    /// chain head; the recipient responds with the tail
    /// (entries with `prev_hash` chaining forward from
    /// `from_hash_b64`).
    DomainWitnessRequest {
        /// Caller's current chain head.
        from_hash_b64: String,
    },
    /// Response to a `DomainWitnessRequest`. Carries the
    /// chain tail in order; the receiver applies each
    /// witness via its runtime layer.
    DomainWitnessResponse {
        /// Chain tail starting after the requester's head.
        witnesses: Vec<evo_witness::DomainWitness>,
    },
    /// Out-of-chain presence observation forwarded by a
    /// chain-aware relay. Carries the relay's observation
    /// of a peer's runtime presence state on a network the
    /// receiver does not directly observe.
    ///
    /// Presence is not chain content (it is runtime state,
    /// not durable audit material), so this variant rides
    /// a dedicated control-channel variant rather than the
    /// witness-chain message family.
    CrossNetworkPresenceObservation {
        /// Device id the relay is observing.
        peer_id: String,
        /// Network on which the observation was made.
        network_id: String,
        /// Observed presence state at the time of report.
        state: String,
        /// Wall-clock milliseconds at which the relay
        /// observed the state.
        observed_at_ms: u64,
    },
}

/// Configuration for the [`AudioPlaneRuntime`].
#[derive(Debug, Clone)]
pub struct AudioPlaneConfig {
    /// Whether the runtime is enabled. When `false`,
    /// [`AudioPlaneRuntime::start`] is a no-op (no listener
    /// bound, no outbound connections opened).
    pub enabled: bool,
    /// TCP port the listener binds. Should match the value
    /// advertised in mDNS-SD discovery's TXT record
    /// (`[multiroom] control_port`).
    pub control_port: u16,
    /// Cadence of the periodic housekeeping tick
    /// (ensure-outbound-connections, idle-reap, sync-probe
    /// dispatch). Default 1 second — the cadence itself is
    /// cheap; the work it gates is bounded.
    pub sweep_interval: Duration,
    /// Total-channel-silence window after which an idle
    /// connection is reaped. The housekeeping loop drops
    /// connections whose `last_channel_activity_ms` is
    /// staler than this threshold. Sync-probe responses
    /// (and any other inbound traffic) reset the silence
    /// clock, so connections persist indefinitely while
    /// either audio frames or sync probes are flowing.
    /// Default 120 seconds — well beyond any
    /// operator-immediacy window; only genuinely dead
    /// connections (peer crashed, network partition, full
    /// kernel-stack outage) cross it.
    pub idle_reap_threshold: Duration,
    /// Cadence of receiver-initiated sync probes. Sync
    /// probes are the clock-domain primitive that drives
    /// each receiver's sample-rate-skew PLL; they keep the
    /// PLL warm during transport-state transitions
    /// (pause / resume) so audio resumes bit-perfect
    /// without a re-convergence settle window. Default
    /// 5 seconds.
    pub sync_probe_interval: Duration,
}

impl Default for AudioPlaneConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            control_port: 7331,
            sweep_interval: Duration::from_secs(1),
            idle_reap_threshold: Duration::from_secs(120),
            sync_probe_interval: Duration::from_secs(5),
        }
    }
}

/// Errors raised by [`AudioPlaneRuntime`].
#[derive(Debug, thiserror::Error)]
pub enum AudioPlaneError {
    /// Underlying I/O error.
    #[error("audio-plane I/O: {0}")]
    Io(#[from] std::io::Error),
    /// JSON encode / decode error.
    #[error("audio-plane wire format: {0}")]
    Serde(#[from] serde_json::Error),
    /// Frame longer than [`MAX_FRAME_BYTES`].
    #[error("audio-plane frame too large: {0} bytes")]
    FrameTooLarge(usize),
    /// Invariant violated by the peer (zero-length frame,
    /// malformed Hello, etc.).
    #[error("audio-plane protocol violation: {0}")]
    Protocol(String),
}

/// Persistence-light network audio-plane runtime. Owns the
/// TCP listener, the outbound-connector, the periodic sweep
/// (ensure-connections + heartbeat send + sync probe), and
/// the per-peer connection state.
pub struct AudioPlaneRuntime {
    config: AudioPlaneConfig,
    happenings: Arc<HappeningBus>,
    discovery: Arc<DiscoveryRuntime>,
    election: evo_primitives::SharedElectionState,
    clock_sync: Arc<ClockSyncRuntime>,
    group_store: Arc<GroupStore>,
    local_device_id: DeviceId,
    /// Broadcast channel that re-publishes every received
    /// [`AudioPlaneMessage::AudioFrame`] to subscribed plugins.
    /// Capacity caps how many frames a slow consumer can fall
    /// behind before lagging (and dropping). Receiver-side
    /// render plugins subscribe via
    /// [`AudioPlaneRuntime::subscribe_audio_frames`] and consume
    /// the resulting stream into their local audio delivery
    /// chain. The framework's transport substrate continues to
    /// count frames irrespective of subscriber presence so the
    /// `frames_received` metric is unaffected by plugin lifecycle.
    audio_frame_tx: broadcast::Sender<AudioFrameReceived>,
    /// Broadcast channel for per-recipient source-side frame
    /// send observations. Each emission carries
    /// `(receiver_device_id, group_id, sequence, wire_send_ns)`
    /// captured at the moment the runtime queued the frame onto
    /// the per-peer write channel. Source-role plugins
    /// aggregating the published `audio.multiroom.frame_trace`
    /// subject subscribe via
    /// [`AudioPlaneRuntime::subscribe_frame_send_events`].
    frame_send_event_tx: broadcast::Sender<FrameSendEvent>,
    /// Broadcast channel for receiver back-reports of per-
    /// frame audible-time trace fields. Wire form is
    /// [`AudioPlaneMessage::FrameTraceReport`]; this channel
    /// is the framework-internal broadcast that the SDK's
    /// [`AudioPlaneHandle::subscribe_frame_trace_reports`]
    /// surfaces to source-role plugins for aggregation.
    frame_trace_report_tx: broadcast::Sender<FrameTraceReport>,
    /// Broadcast channel for inbound domain-witness events.
    /// The [`crate::domain_witness::DomainWitnessRuntime`]
    /// subscribes via
    /// [`Self::subscribe_domain_witness_inbound`] and
    /// dispatches each variant.
    domain_witness_inbound_tx: broadcast::Sender<DomainWitnessInbound>,
    /// Broadcast channel for inbound cross-network presence
    /// observations forwarded by chain-aware relays. The
    /// presence correlator subscribes via
    /// [`Self::subscribe_cross_network_presence`].
    cross_network_presence_tx: broadcast::Sender<CrossNetworkPresenceReport>,
    /// Runtime-wide monotonic epoch. All `wire_send_ns` /
    /// `wire_recv_ns` / back-reported timestamp fields the
    /// audio-plane emits are nanoseconds since this Instant,
    /// captured by [`Self::monotonic_ns`]. Establishing one
    /// epoch per runtime means every same-node timestamp
    /// pair on the audio-plane is directly subtractable
    /// without per-call epoch reconciliation. Cross-node
    /// reconciliation rides the existing NTP-lite sync
    /// probe (consumers receive a `clock_offset_ns` value
    /// alongside each trace record).
    start_instant: std::time::Instant,
    inner: AsyncMutex<Inner>,
}

/// Re-export the SDK's audio-frame envelope so the framework-
/// internal broadcast carries the same shape the plugin
/// consumer side sees. Keeps the type aligned across the SDK
/// boundary without an adapter step (same pattern as
/// `SubjectStateUpdate` re-export from the subjects substrate).
pub use evo_plugin_sdk::contract::{
    AudioFrameReceived, FrameSendEvent, FrameTraceReport,
    ReceiverFrameTraceReport,
};

/// One inbound domain-witness event surfaced on the
/// [`AudioPlaneRuntime::subscribe_domain_witness_inbound`]
/// broadcast channel. The [`crate::domain_witness::
/// DomainWitnessRuntime`] consumes the stream, dispatching
/// each variant to the appropriate runtime method.
///
/// The `Witness` variant is the common case (one witness
/// per event) and the `TailResponse` variant can carry a
/// chain delta of many entries. The size differential is
/// inherent to the protocol and intentional — wrapping the
/// payloads in `Box` would force a heap allocation on
/// every single-witness announce just to satisfy a lint.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainWitnessInbound {
    /// A peer announced a fresh signed chain entry.
    /// Runtime calls `receive_remote_witness`.
    Witness {
        /// Device id of the peer that delivered the
        /// announce (may be the originator or a relay).
        from_peer_id: String,
        /// The signed chain entry being delivered.
        witness: evo_witness::DomainWitness,
    },
    /// A peer that has fallen behind asks for the chain
    /// tail. Runtime calls `tail_after(from_hash)` and the
    /// audio-plane sends a `DomainWitnessResponse` back.
    TailRequest {
        /// Device id of the peer requesting the tail.
        from_peer_id: String,
        /// The peer's current chain head — entries with
        /// `prev_hash` chaining forward from this hash are
        /// the requested tail.
        from_hash_b64: String,
    },
    /// A peer responded to a tail request with the
    /// requested entries in order. Runtime calls
    /// `apply_response_batch`.
    TailResponse {
        /// Device id of the peer that responded.
        from_peer_id: String,
        /// Chain tail in order.
        witnesses: Vec<evo_witness::DomainWitness>,
    },
}

/// One cross-network presence observation surfaced on the
/// [`AudioPlaneRuntime::subscribe_cross_network_presence`]
/// broadcast channel. The presence correlator merges these
/// observations into its local map so the operator UI sees
/// peers on remote VLANs / sites at the same five-state
/// resolution as local peers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossNetworkPresenceReport {
    /// Relay device id that produced the observation.
    pub relay_device_id: String,
    /// Device id being observed.
    pub peer_id: String,
    /// Network on which the relay observed the peer.
    pub network_id: String,
    /// Observed presence state at report time (typically
    /// `"live"` / `"quiet"` / `"stalled"` / `"absent"` —
    /// the relay's local presence-correlator output).
    pub state: String,
    /// Wall-clock milliseconds at the relay at observation
    /// time.
    pub observed_at_ms: u64,
}

impl std::fmt::Debug for AudioPlaneRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioPlaneRuntime")
            .field("local_device_id", &self.local_device_id)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct Inner {
    listener_task: Option<JoinHandle<()>>,
    sweep_task: Option<JoinHandle<()>>,
    /// Per-peer connection state, keyed on remote device id
    /// once the handshake completes. Connections in
    /// `Handshaking` state are keyed on their socket-addr
    /// string until Hello arrives.
    connections: HashMap<String, ConnectionEntry>,
    /// Per-peer in-flight dial tokens. Recorded BEFORE the
    /// spawned dial task starts; removed when the dial task
    /// completes (either via `connections` insertion on a
    /// successful handshake or by the spawned task's cleanup
    /// after failure). The housekeeping sweep consults both
    /// this and `connections` to decide whether to dial a
    /// peer — preventing the race where a slow Hello on the
    /// prior dial leaves `connections` empty long enough for
    /// the next 1s sweep to fire a duplicate dial. Without
    /// this guard, two concurrent dials produce two TCP
    /// connections to the same peer; whichever completes its
    /// Hello second triggers the dedup-on-handshake path
    /// (line 1252), aborting the prior — and that abort
    /// closes the prior TCP stream, surfacing as
    /// `audio-plane: inbound connection failed` (error:
    /// `early eof`) on the peer side. Concurrent unidirectional
    /// dials to the same peer ARE always a race; this set is
    /// the canonical serialisation.
    in_flight_dials: std::collections::HashSet<String>,
    /// Per-peer outbound dial backoff state. Prevents 1s
    /// housekeeping cadence from hammering unreachable peers.
    dial_retry: HashMap<String, DialRetryState>,
    /// Sticky peer endpoint cache learned from prior
    /// successful connections. Used as fallback when discovery
    /// has a transient blind spot so reconnect can still
    /// proceed.
    cached_peer_addresses: HashMap<String, Vec<String>>,
    /// Pending sync probes per (peer_device_id, group_id) →
    /// receiver's `t1_ms` and a receiver-side `t3_ms` set
    /// at probe send. Cleared on response or timeout.
    pending_probes: HashMap<(String, String), PendingProbe>,
}

struct ConnectionEntry {
    info: PeerConnectionInfo,
    write_tx: mpsc::UnboundedSender<AudioPlaneMessage>,
    read_task: JoinHandle<()>,
    write_task: JoinHandle<()>,
}

#[derive(Debug, Clone, Copy)]
struct PendingProbe {
    t1_ms: u64,
    /// Receiver's monotonic ms at probe send (==t1_ms when
    /// using a single clock; kept distinct for readability).
    sent_at_ms: u64,
}

#[derive(Debug, Clone, Copy)]
struct DialRetryState {
    consecutive_failures: u32,
    next_retry_at_ms: u64,
}

impl AudioPlaneRuntime {
    /// Construct a runtime. Network operations do not begin
    /// until [`Self::start`] is called.
    pub fn new(
        config: AudioPlaneConfig,
        happenings: Arc<HappeningBus>,
        discovery: Arc<DiscoveryRuntime>,
        election: evo_primitives::SharedElectionState,
        clock_sync: Arc<ClockSyncRuntime>,
        group_store: Arc<GroupStore>,
        local_device_id: DeviceId,
    ) -> Self {
        // Broadcast channel capacity sized at one second of
        // 48 kHz / 20 ms frames (50 frames/sec) with a 4x slack
        // for slow consumers. Subscribers that fall behind by
        // more than 200 frames see Lagged errors and resync.
        let (audio_frame_tx, _) = broadcast::channel(200);
        // Same sizing rationale as audio_frame_tx — one second
        // of 50 fps frames with 4x slack for slow aggregators.
        let (frame_send_event_tx, _) = broadcast::channel(200);
        // Back-reports are 1-per-frame-per-receiver. Sizing
        // matches the send-event channel.
        let (frame_trace_report_tx, _) = broadcast::channel(200);
        // Domain-witness inbound events are low-rate
        // (operator gestures + reconciliation tails);
        // 128 slots accommodates a burst of catch-up
        // entries without lagging slow consumers.
        let (domain_witness_inbound_tx, _) = broadcast::channel(128);
        // Cross-network presence reports run at the relay's
        // forward cadence (~1 Hz per observed peer per
        // network); 128 slots covers a venue-scale cluster.
        let (cross_network_presence_tx, _) = broadcast::channel(128);
        Self {
            config,
            happenings,
            discovery,
            election,
            clock_sync,
            group_store,
            local_device_id,
            audio_frame_tx,
            frame_send_event_tx,
            frame_trace_report_tx,
            domain_witness_inbound_tx,
            cross_network_presence_tx,
            start_instant: std::time::Instant::now(),
            inner: AsyncMutex::new(Inner::default()),
        }
    }

    /// Subscribe to the inbound stream of domain-witness
    /// events delivered by peers over the audio-plane
    /// control channel. The
    /// [`crate::domain_witness::DomainWitnessRuntime`]
    /// consumes the stream; consumers that fall behind by
    /// more than the channel capacity see `Lagged` errors
    /// and re-subscribe.
    pub fn subscribe_domain_witness_inbound(
        &self,
    ) -> broadcast::Receiver<DomainWitnessInbound> {
        self.domain_witness_inbound_tx.subscribe()
    }

    /// Subscribe to the inbound stream of cross-network
    /// presence observations forwarded by chain-aware
    /// relays.
    pub fn subscribe_cross_network_presence(
        &self,
    ) -> broadcast::Receiver<CrossNetworkPresenceReport> {
        self.cross_network_presence_tx.subscribe()
    }

    /// Send one message to a specific connected peer.
    /// Returns `true` if the peer is currently connected
    /// and the message was queued on the per-peer write
    /// channel; `false` otherwise. Lookup is keyed on
    /// the remote device id recorded at handshake.
    ///
    /// Used by:
    /// - the witness-broadcaster trait impl to deliver
    ///   `DomainWitnessAnnounce` to one peer at a time;
    /// - the chain-requester trait impl to deliver
    ///   `DomainWitnessRequest` to the sender of a stale
    ///   witness;
    /// - the relay runtime to deliver
    ///   `CrossNetworkPresenceObservation` to peers on
    ///   the destination network.
    pub async fn send_to_peer(
        &self,
        peer_id: &str,
        message: AudioPlaneMessage,
    ) -> bool {
        let g = self.inner.lock().await;
        if let Some(entry) = g.connections.get(peer_id) {
            entry.write_tx.send(message).is_ok()
        } else {
            false
        }
    }

    /// Broadcast a message to every currently-connected
    /// peer. Used by the witness-broadcaster trait impl
    /// when announcing a freshly-signed chain entry.
    ///
    /// Returns the number of peers the message was queued
    /// onto. Connections whose write channel is closed
    /// (peer disconnected concurrently) are skipped.
    pub async fn broadcast_to_peers(
        &self,
        message: AudioPlaneMessage,
    ) -> usize {
        let g = self.inner.lock().await;
        let mut delivered = 0usize;
        for entry in g.connections.values() {
            if entry.write_tx.send(message.clone()).is_ok() {
                delivered += 1;
            }
        }
        delivered
    }

    /// Compute the runtime-wide monotonic ns timestamp used in
    /// every audible-time trace field this runtime emits. The
    /// epoch is the runtime's construction `Instant`; the
    /// returned `u64` is the elapsed nanoseconds since then.
    /// Same-node timestamps captured via this helper are
    /// directly subtractable. Cross-node reconciliation rides
    /// the audio-plane sync probe (consumers carry a
    /// `clock_offset_ns` per record).
    pub fn monotonic_ns(&self) -> u64 {
        self.start_instant.elapsed().as_nanos() as u64
    }

    /// The local device's canonical id as a borrowed `str`.
    /// Stable for the process lifetime; matches the
    /// `from_device_id` / `source_device_id` /
    /// `receiver_device_id` fields the audio-plane stamps on
    /// every message it routes. Exposed via the SDK's
    /// `AudioPlaneHandle::local_device_id` so plugins can
    /// address per-device subjects (e.g. the per-device card
    /// envelope subject) and recognise their own frames in
    /// self-loopback paths.
    pub fn local_device_id(&self) -> &str {
        self.local_device_id.0.as_ref()
    }

    /// Subscribe to the broadcast stream of every received
    /// audio frame. Returns a [`broadcast::Receiver`] that
    /// yields one [`AudioFrameReceived`] per arrived frame
    /// across every source-host peer; the receiver-side
    /// render plugin spawns a task that consumes this stream
    /// into its local audio delivery chain.
    ///
    /// The broadcast is fire-and-forget from the runtime's
    /// perspective: a subscriber that never reads its
    /// receiver simply sees `Lagged` errors and rejoins at
    /// the live frame on the next recv. Slow consumers do
    /// not block the runtime's accept / write tasks.
    ///
    /// Frames are broadcast AFTER the receive-side counter
    /// increments and AFTER the wire-form base64 payload is
    /// decoded once; every subscriber observes the same
    /// `Vec<u8>` payload without duplicating the decode.
    pub fn subscribe_audio_frames(
        &self,
    ) -> broadcast::Receiver<AudioFrameReceived> {
        self.audio_frame_tx.subscribe()
    }

    /// Start the runtime: bind the TCP listener, spawn the
    /// accept loop, and spawn the periodic sweep task. No-op
    /// when [`AudioPlaneConfig::enabled`] is `false`.
    pub async fn start(self: &Arc<Self>) -> Result<(), AudioPlaneError> {
        if !self.config.enabled {
            return Ok(());
        }

        let bind_addr = format!("0.0.0.0:{}", self.config.control_port);
        let listener = TcpListener::bind(&bind_addr).await?;

        {
            let mut g = self.inner.lock().await;
            if g.listener_task.is_some() {
                return Ok(());
            }
            let runtime = Arc::clone(self);
            g.listener_task = Some(tokio::spawn(async move {
                runtime.accept_loop(listener).await;
            }));

            let runtime = Arc::clone(self);
            g.sweep_task = Some(tokio::spawn(async move {
                runtime.housekeeping_loop().await;
            }));
        }
        Ok(())
    }

    /// Shut the runtime down. Idempotent.
    pub async fn shutdown(&self) {
        let mut g = self.inner.lock().await;
        if let Some(t) = g.listener_task.take() {
            t.abort();
        }
        if let Some(t) = g.sweep_task.take() {
            t.abort();
        }
        let device_id = self.local_device_id.0.clone();
        let goodbye = AudioPlaneMessage::Goodbye {
            device_id: device_id.clone(),
        };
        for (_, entry) in g.connections.drain() {
            let _ = entry.write_tx.send(goodbye.clone());
            entry.read_task.abort();
            entry.write_task.abort();
        }
    }

    /// Operator-visible snapshot of every active peer
    /// connection.
    pub async fn list_connections(&self) -> Vec<PeerConnectionInfo> {
        let g = self.inner.lock().await;
        let mut rows: Vec<PeerConnectionInfo> =
            g.connections.values().map(|e| e.info.clone()).collect();
        rows.sort_by(|a, b| a.remote_device_id.cmp(&b.remote_device_id));
        rows
    }

    /// Close every outbound peer connection. Inbound
    /// connections (peers that dialed us) are preserved —
    /// they belong to the remote source-host and survive
    /// local role changes. Source-role plugins call this on
    /// role-demotion teardown so the next engagement does
    /// not inherit stale outbound connections dialed by the
    /// abandoned role.
    ///
    /// Sends a `Goodbye` on each closed connection so the
    /// remote peer drops its inbound entry cleanly rather
    /// than waiting for read-EOF.
    pub async fn close_outbound_connections(&self) {
        let device_id = self.local_device_id.0.clone();
        let goodbye = AudioPlaneMessage::Goodbye {
            device_id: device_id.clone(),
        };
        let closed_ids: Vec<String> = {
            let mut g = self.inner.lock().await;
            let outbound_ids: Vec<String> = g
                .connections
                .iter()
                .filter_map(|(id, entry)| {
                    if entry.info.direction == ConnectionDirection::Outbound {
                        Some(id.clone())
                    } else {
                        None
                    }
                })
                .collect();
            for id in &outbound_ids {
                if let Some(entry) = g.connections.remove(id) {
                    let _ = entry.write_tx.send(goodbye.clone());
                    entry.read_task.abort();
                    entry.write_task.abort();
                }
            }
            outbound_ids
        };
        for id in &closed_ids {
            self.emit(Happening::PeerDisconnected {
                device_id: id.clone(),
                at: std::time::SystemTime::now(),
            })
            .await;
        }
    }

    /// Heartbeat timeout — the window after which a connection
    /// is considered dead if no inbound heartbeat has arrived.
    /// Operator projections (the `is_session_connected` field
    /// on `DomainMemberEntry`) MUST consult this accessor when
    /// classifying session-level liveness so the projection
    /// and the connection-drop logic never disagree.
    pub fn idle_reap_threshold(&self) -> Duration {
        self.config.idle_reap_threshold
    }

    /// TCP port the audio-plane listener binds. Read-only.
    /// Wire-op handlers (e.g. `bootstrap_domain`) consult this
    /// when composing the founder admit witness's endpoints
    /// list — the chain entry advertises every reachable
    /// IPv4 interface paired with this port.
    pub fn control_port(&self) -> u16 {
        self.config.control_port
    }

    /// Test-only: open an outbound TCP connection to `addr` and
    /// run the handshake inline. Mirrors the dial path the
    /// periodic sweep runs against operator-declared peer
    /// addresses; exposed so unit tests can wire two runtimes
    /// over loopback without standing up the discovery
    /// runtime + election state the production sweep relies
    /// on.
    #[cfg(test)]
    pub async fn dial_for_test(
        self: Arc<Self>,
        addr: std::net::SocketAddr,
    ) -> Result<(), AudioPlaneError> {
        let stream = TcpStream::connect(addr).await?;
        self.run_connection(
            stream,
            addr.to_string(),
            ConnectionDirection::Outbound,
        )
        .await
    }

    /// Open an outbound TCP connection to `addr` and run the
    /// handshake inline. Operator-facing seam exposed via the
    /// `audio_plane_dial` wire op so an operator (or admin
    /// tool) can manually establish a peer connection when
    /// auto-discovery does not surface a dialable address
    /// (IPv6-link-local-without-scope-id edge case, NAT/VLAN
    /// boundaries, opaque-network shapes the runtime cannot
    /// auto-resolve).
    ///
    /// The connection is registered in the runtime's
    /// connection map on successful handshake and participates
    /// in heartbeat / sync probe / audio frame fan-out the
    /// same as auto-discovered connections.
    pub async fn dial_peer(
        self: Arc<Self>,
        addr: std::net::SocketAddr,
    ) -> Result<(), AudioPlaneError> {
        let stream = TcpStream::connect(addr).await?;
        self.run_connection(
            stream,
            addr.to_string(),
            ConnectionDirection::Outbound,
        )
        .await
    }

    /// Send an audio frame to every receiver of the supplied
    /// group. Public seam the audio data plane will call when
    /// a real delivery plugin admits and the source-host
    /// integration lands.
    pub async fn fan_out_audio_frame(
        &self,
        group_id: &str,
        frame: AudioFrameSeed,
    ) {
        // Look up which receivers this frame should reach:
        // every member of the group EXCEPT the local node
        // for per-peer wire send (we are the source-host and
        // do not TCP-send to ourselves), but INCLUDING the
        // local node in the audible-time trace + the
        // audio_frame_tx broadcast — the multi-room renderer
        // design (one-renderer-pipeline) treats the source-
        // host's local DAC as a Receiver of its own output,
        // so source frames flow through the same broadcast
        // channel remote frames flow through; the source
        // plugin's receiver task picks them up and renders
        // locally through the same scheduler the remote
        // receivers use. This eliminates the dual-pipeline
        // (`evo-direct` MPD output bypassing the multi-room
        // scheduler) that previously made source-local
        // audible-time inconsistent with remote-receiver
        // audible-time.
        let group = match self.group_store.get(group_id).await {
            Ok(Some(g)) => g,
            _ => return,
        };
        let local_id = self.local_device_id.0.clone();
        let local_is_member = group.members.iter().any(|m| m == &local_id);
        let remote_recipients: Vec<String> = group
            .members
            .iter()
            .filter(|m| m.as_str() != local_id.as_str())
            .cloned()
            .collect();

        let msg = AudioPlaneMessage::AudioFrame {
            group_id: group_id.to_string(),
            sequence: frame.sequence,
            presentation_time_ms: frame.presentation_time_ms,
            codec: frame.codec.clone(),
            rate_hz: frame.rate_hz,
            channels: frame.channels,
            payload_b64: frame.payload_b64.clone(),
        };

        // Per-peer wire send for remote recipients.
        let g = self.inner.lock().await;
        for recipient_id in remote_recipients {
            if let Some(entry) = g.connections.get(&recipient_id) {
                if entry.write_tx.send(msg.clone()).is_ok() {
                    // Audible-time trace stage 5a: the
                    // moment the runtime queued the frame
                    // onto this recipient's per-peer write
                    // channel. Source-role plugins
                    // aggregating the
                    // `audio.multiroom.frame_trace` subject
                    // observe this via
                    // subscribe_frame_send_events.
                    let event = FrameSendEvent {
                        receiver_device_id: recipient_id.clone(),
                        group_id: group_id.to_string(),
                        sequence: frame.sequence,
                        wire_send_ns: self.monotonic_ns(),
                    };
                    let _ = self.frame_send_event_tx.send(event);
                }
            }
        }
        drop(g);

        // Self-loopback: when the local node is itself a
        // member of the group, broadcast the frame on the
        // audio_frame_tx channel with from_device_id =
        // local_id. The source plugin's receiver task
        // (admitted in the same plugin instance) picks the
        // frame up and renders it through the same scheduler
        // remote receivers use, closing the
        // one-renderer-pipeline invariant: source-local DAC
        // and every remote receiver share the same render
        // path. Emits a FrameSendEvent for the local
        // recipient too so the audible-time trace
        // aggregator's per-recipient bookkeeping covers the
        // local-DAC render alongside the remote receivers.
        if local_is_member {
            use base64::engine::general_purpose::STANDARD;
            use base64::Engine as _;
            match STANDARD.decode(&frame.payload_b64) {
                Ok(payload) => {
                    let wire_recv_ns = self.monotonic_ns();
                    let frame_received = AudioFrameReceived {
                        from_device_id: local_id.clone(),
                        group_id: group_id.to_string(),
                        sequence: frame.sequence,
                        presentation_time_ms: frame.presentation_time_ms,
                        codec: frame.codec,
                        rate_hz: frame.rate_hz,
                        channels: frame.channels,
                        payload,
                        wire_recv_ns,
                    };
                    let _ = self.audio_frame_tx.send(frame_received);
                    let event = FrameSendEvent {
                        receiver_device_id: local_id,
                        group_id: group_id.to_string(),
                        sequence: frame.sequence,
                        // Wire-send-ns for the self-loopback
                        // matches the wire-recv-ns we just
                        // captured because the loopback has
                        // no TCP transit; the scheduler
                        // queue delta still shows up later
                        // in the trace record.
                        wire_send_ns: wire_recv_ns,
                    };
                    let _ = self.frame_send_event_tx.send(event);
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "audio-plane: self-loopback decode failed; \
                         source-local DAC render skipped this frame"
                    );
                }
            }
        }
    }

    /// Subscribe to the broadcast stream of per-recipient
    /// frame-send observations the runtime emits as it queues
    /// frames onto each peer's write channel. Source-role
    /// plugins aggregating the `audio.multiroom.frame_trace`
    /// subject subscribe via this method to capture the
    /// wire-send stage of the audible-time trace.
    pub fn subscribe_frame_send_events(
        &self,
    ) -> broadcast::Receiver<FrameSendEvent> {
        self.frame_send_event_tx.subscribe()
    }

    /// Subscribe to the broadcast stream of receiver back-
    /// reports for the audible-time trace. Source-role plugins
    /// aggregating the published subject subscribe via this
    /// method to receive each receiver's per-frame stage
    /// observations (wire_recv_ns / scheduler_dequeue_ns /
    /// writei_return_ns).
    pub fn subscribe_frame_trace_reports(
        &self,
    ) -> broadcast::Receiver<FrameTraceReport> {
        self.frame_trace_report_tx.subscribe()
    }

    /// Route a receiver back-report to the source-host peer of
    /// the named group. Receiver-role plugins call this once
    /// per frame they render via the SDK's
    /// [`AudioPlaneHandle::report_frame_trace`]; the framework
    /// looks up the source-host's connection from
    /// `local_device_id`-keyed group membership + per-peer
    /// connection state and queues the corresponding
    /// [`AudioPlaneMessage::FrameTraceReport`] onto that
    /// peer's write channel.
    ///
    /// Best-effort: a peer that has disconnected between the
    /// frame's arrival and the back-report send results in
    /// the message being silently dropped at the per-peer
    /// lookup. The next frame's report goes through if the
    /// connection re-establishes.
    pub async fn route_frame_trace_report(
        &self,
        report: ReceiverFrameTraceReport,
    ) {
        // Self-loopback: when the report's source is the
        // local node (one-renderer-pipeline shape — the
        // source plugin's receiver task back-reports its
        // own render's stages 5b / 6 / 7), broadcast
        // directly on the frame_trace_report_tx channel
        // instead of routing over TCP. The source plugin's
        // aggregator subscribed to that channel sees the
        // back-report identically to a remote receiver's
        // back-report; the per-(sequence, receiver_id)
        // completion logic does not care whether the
        // receiver_id is local or remote.
        if report.source_device_id == self.local_device_id.0.as_ref() {
            let local_report = FrameTraceReport {
                from_device_id: self.local_device_id.0.clone(),
                group_id: report.group_id,
                sequence: report.sequence,
                wire_recv_ns: report.wire_recv_ns,
                scheduler_dequeue_ns: report.scheduler_dequeue_ns,
                writei_return_ns: report.writei_return_ns,
            };
            let _ = self.frame_trace_report_tx.send(local_report);
            return;
        }
        let msg = AudioPlaneMessage::FrameTraceReport {
            group_id: report.group_id,
            sequence: report.sequence,
            wire_recv_ns: report.wire_recv_ns,
            scheduler_dequeue_ns: report.scheduler_dequeue_ns,
            writei_return_ns: report.writei_return_ns,
        };
        let g = self.inner.lock().await;
        if let Some(entry) = g.connections.get(&report.source_device_id) {
            let _ = entry.write_tx.send(msg);
        }
    }

    async fn accept_loop(self: Arc<Self>, listener: TcpListener) {
        loop {
            match listener.accept().await {
                Ok((stream, addr)) => {
                    let runtime = Arc::clone(&self);
                    tokio::spawn(async move {
                        if let Err(e) =
                            runtime.handle_inbound(stream, addr).await
                        {
                            tracing::warn!(
                                error = %e,
                                peer = %addr,
                                "audio-plane: inbound connection failed"
                            );
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "audio-plane: accept failed"
                    );
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    async fn handle_inbound(
        self: Arc<Self>,
        stream: TcpStream,
        addr: SocketAddr,
    ) -> Result<(), AudioPlaneError> {
        self.run_connection(
            stream,
            addr.to_string(),
            ConnectionDirection::Inbound,
        )
        .await
    }

    async fn run_connection(
        self: Arc<Self>,
        stream: TcpStream,
        remote_address: String,
        direction: ConnectionDirection,
    ) -> Result<(), AudioPlaneError> {
        let (mut read_half, mut write_half) = stream.into_split();

        // Send our Hello first — inline to the socket, NOT via
        // the write-pump task that starts further down. Both
        // peers run this symmetric handshake: each writes its
        // Hello immediately, then awaits the peer's. If we
        // queued the Hello into the mpsc channel (the way
        // subsequent messages flow), the write task that drains
        // it does not exist yet — it spawns only after the
        // read below succeeds. Both peers would then deadlock,
        // each holding its Hello in an in-process queue while
        // waiting for the peer's frame that never arrives.
        let local_hello = self.build_local_hello().await;
        write_message(&mut write_half, &local_hello).await?;

        // Read peer's Hello to identify them. Bound it so a
        // mute peer cannot pin a connection slot forever.
        let hello = tokio::time::timeout(
            Duration::from_secs(5),
            read_message(&mut read_half),
        )
        .await
        .map_err(|_| AudioPlaneError::Protocol("hello timeout".into()))??;
        let (remote_id, remote_version, claimed_source_groups) = match hello {
            AudioPlaneMessage::Hello {
                device_id,
                framework_version,
                claimed_source_host_groups,
            } => (device_id, framework_version, claimed_source_host_groups),
            other => {
                return Err(AudioPlaneError::Protocol(format!(
                    "expected Hello, got {other:?}"
                )));
            }
        };

        if remote_id == self.local_device_id.0 {
            return Err(AudioPlaneError::Protocol(
                "peer announced our own device id".into(),
            ));
        }

        // Cross-check claimed source-host groups against our
        // local election state — disagreement is split-brain.
        self.check_split_brain(&remote_id, &claimed_source_groups)
            .await;

        let now = now_ms();
        let info = PeerConnectionInfo {
            remote_device_id: remote_id.clone(),
            remote_address,
            direction,
            state: PeerConnectionState::Connected,
            framework_version: remote_version,
            claimed_source_host_groups: claimed_source_groups,
            last_channel_activity_ms: now,
            last_sync_offset_ms: None,
            last_sync_at_ms: 0,
            connected_at_ms: now,
            frames_received: 0,
        };

        // Open the post-hello write queue. Subsequent messages
        // (heartbeat, sync, goodbye, etc.) flow into `write_tx`;
        // the spawned `write_task` below drains it and writes to
        // the socket. The Hello is NOT routed through this
        // channel — see the inline `write_message` call at the
        // top of `run_connection`.
        let (write_tx, mut write_rx) =
            mpsc::unbounded_channel::<AudioPlaneMessage>();

        // Spawn read + write tasks. Use Arc-shared inner so
        // both can mutate connection state.
        let runtime_for_read = Arc::clone(&self);
        let remote_for_read = remote_id.clone();
        let read_task = tokio::spawn(async move {
            loop {
                match read_message(&mut read_half).await {
                    Ok(msg) => {
                        runtime_for_read
                            .observe_message(&remote_for_read, msg)
                            .await;
                    }
                    Err(e) => {
                        tracing::debug!(
                            error = %e,
                            peer = %remote_for_read,
                            "audio-plane: read loop ended"
                        );
                        break;
                    }
                }
            }
            runtime_for_read.mark_disconnected(&remote_for_read).await;
        });

        let remote_for_write = remote_id.clone();
        let write_task = tokio::spawn(async move {
            while let Some(msg) = write_rx.recv().await {
                match write_message(&mut write_half, &msg).await {
                    Ok(()) => {}
                    Err(e) => {
                        tracing::debug!(
                            error = %e,
                            peer = %remote_for_write,
                            "audio-plane: write loop ended"
                        );
                        break;
                    }
                }
            }
        });

        let entry = ConnectionEntry {
            info: info.clone(),
            write_tx,
            read_task,
            write_task,
        };
        let already_present;
        {
            let mut g = self.inner.lock().await;
            already_present = g.connections.contains_key(&remote_id);
            // If a prior connection exists for this peer
            // (race between accept + outbound dial on both
            // sides), drop the prior in favour of the new
            // one — both sides will deterministically pick
            // the same survivor on next sweep.
            if let Some(prev) = g.connections.remove(&remote_id) {
                prev.read_task.abort();
                prev.write_task.abort();
            }
            g.connections.insert(remote_id.clone(), entry);
            // Cache only addresses we actively dialed. Inbound
            // `remote_address` carries the peer socket's source
            // port (ephemeral), which is not a durable control
            // endpoint and must not be used as a redial target.
            if direction == ConnectionDirection::Outbound {
                g.cached_peer_addresses
                    .entry(remote_id.clone())
                    .and_modify(|addrs| {
                        if !addrs.iter().any(|a| a == &info.remote_address) {
                            addrs.push(info.remote_address.clone());
                            if addrs.len() > 8 {
                                let drop_n = addrs.len().saturating_sub(8);
                                addrs.drain(0..drop_n);
                            }
                        }
                    })
                    .or_insert_with(|| vec![info.remote_address.clone()]);
            }
            g.dial_retry.remove(&remote_id);
        }
        if !already_present {
            self.emit(Happening::PeerConnected {
                device_id: remote_id.clone(),
                direction: connection_direction_str(direction).to_string(),
                at: std::time::SystemTime::now(),
            })
            .await;
        }
        Ok(())
    }

    async fn build_local_hello(&self) -> AudioPlaneMessage {
        let claimed: Vec<String> = self
            .election
            .current()
            .list_source_hosts()
            .await
            .into_iter()
            .filter_map(|(group_id, host_opt)| {
                if host_opt.as_deref() == Some(self.local_device_id.0.as_str())
                {
                    Some(group_id)
                } else {
                    None
                }
            })
            .collect();
        AudioPlaneMessage::Hello {
            device_id: self.local_device_id.0.clone(),
            framework_version: env!("CARGO_PKG_VERSION").to_string(),
            claimed_source_host_groups: claimed,
        }
    }

    async fn check_split_brain(
        &self,
        remote_id: &str,
        remote_claimed: &[String],
    ) {
        for group_id in remote_claimed {
            let local_view =
                self.election.current().source_host_for(group_id).await;
            match local_view {
                Some(ref id) if id == remote_id => {
                    // Agreement; no split brain.
                }
                Some(other) => {
                    self.emit(Happening::SplitBrainDetected {
                        group_id: group_id.clone(),
                        local_view_source_host: Some(other.clone()),
                        peer_view_source_host: remote_id.to_string(),
                        resolution_source_host: pick_lower(&other, remote_id),
                        at: std::time::SystemTime::now(),
                    })
                    .await;
                }
                None => {
                    self.emit(Happening::SplitBrainDetected {
                        group_id: group_id.clone(),
                        local_view_source_host: None,
                        peer_view_source_host: remote_id.to_string(),
                        resolution_source_host: remote_id.to_string(),
                        at: std::time::SystemTime::now(),
                    })
                    .await;
                }
            }
        }
    }

    async fn observe_message(
        self: &Arc<Self>,
        peer_id: &str,
        msg: AudioPlaneMessage,
    ) {
        // Every inbound message resets the channel-silence
        // clock. Connection housekeeping reaps connections
        // whose total channel silence crosses
        // `idle_reap_threshold`; source-host election reads
        // this field as the flow-derived liveness signal for
        // peer-pairs in active flow. Updated up-front so
        // every variant's dispatch path benefits without
        // per-arm boilerplate.
        {
            let mut g = self.inner.lock().await;
            if let Some(entry) = g.connections.get_mut(peer_id) {
                entry.info.last_channel_activity_ms = now_ms();
            }
        }
        match msg {
            AudioPlaneMessage::Heartbeat { groups, .. } => {
                // Heartbeat is retained in the wire format
                // for backwards compatibility with peers that
                // still send it; the framework no longer
                // dispatches periodic Heartbeats outbound
                // (the clock-domain SyncProbe is the
                // always-on primitive, the activity update
                // above covers liveness, and the
                // idle-reap threshold is the housekeeping
                // signal). On receipt we still record any
                // claimed source-host group list the
                // sender attaches, so older peers continue
                // to surface their group claims correctly.
                let mut g = self.inner.lock().await;
                if let Some(entry) = g.connections.get_mut(peer_id) {
                    entry.info.claimed_source_host_groups = groups;
                }
            }
            AudioPlaneMessage::SyncProbe { group_id, t1_ms } => {
                let response = AudioPlaneMessage::SyncResponse {
                    group_id,
                    t1_ms,
                    t2_ms: now_ms(),
                };
                let g = self.inner.lock().await;
                if let Some(entry) = g.connections.get(peer_id) {
                    let _ = entry.write_tx.send(response);
                }
            }
            AudioPlaneMessage::SyncResponse {
                group_id,
                t1_ms,
                t2_ms,
            } => {
                self.handle_sync_response(peer_id, &group_id, t1_ms, t2_ms)
                    .await;
            }
            AudioPlaneMessage::AudioFrame {
                group_id,
                sequence,
                presentation_time_ms,
                codec,
                rate_hz,
                channels,
                payload_b64,
            } => {
                {
                    let mut g = self.inner.lock().await;
                    if let Some(entry) = g.connections.get_mut(peer_id) {
                        entry.info.frames_received =
                            entry.info.frames_received.saturating_add(1);
                    }
                }
                // Decode the base64 envelope once and broadcast
                // the raw bytes to every subscribed plugin.
                // base64::engine::general_purpose::URL_SAFE_NO_PAD
                // matches the encoding side used by
                // fan_out_audio_frame's caller (the source-host
                // plugin) so the round-trip is bit-exact.
                use base64::engine::general_purpose::STANDARD;
                use base64::Engine as _;
                match STANDARD.decode(&payload_b64) {
                    Ok(payload) => {
                        // Capture the audible-time trace
                        // wire_recv_ns stage at the exact
                        // moment the framework finished
                        // decoding the envelope. Receivers
                        // echo this value back to the source-
                        // host via the FrameTraceReport
                        // message for source-side aggregation.
                        let wire_recv_ns = self.monotonic_ns();
                        let frame = AudioFrameReceived {
                            from_device_id: peer_id.to_string(),
                            group_id,
                            sequence,
                            presentation_time_ms,
                            codec,
                            rate_hz,
                            channels,
                            payload,
                            wire_recv_ns,
                        };
                        // send() ignores the count of receivers
                        // and the Err(SendError) when no
                        // subscribers exist — the runtime keeps
                        // counting frames even when no plugin
                        // listens (substrate stays correct).
                        let _ = self.audio_frame_tx.send(frame);
                    }
                    Err(e) => {
                        tracing::warn!(
                            peer = %peer_id,
                            error = %e,
                            "audio-plane: rejected AudioFrame with malformed base64 payload"
                        );
                    }
                }
            }
            AudioPlaneMessage::FrameTraceReport {
                group_id,
                sequence,
                wire_recv_ns,
                scheduler_dequeue_ns,
                writei_return_ns,
            } => {
                // Receiver back-report of per-frame audible-
                // time stages. Wrap into the SDK-facing
                // FrameTraceReport shape and broadcast so
                // source-role plugins subscribed via
                // subscribe_frame_trace_reports observe it.
                let report = FrameTraceReport {
                    from_device_id: peer_id.to_string(),
                    group_id,
                    sequence,
                    wire_recv_ns,
                    scheduler_dequeue_ns,
                    writei_return_ns,
                };
                let _ = self.frame_trace_report_tx.send(report);
            }
            AudioPlaneMessage::Goodbye { .. } => {
                self.mark_disconnected(peer_id).await;
            }
            AudioPlaneMessage::Hello { .. } => {
                // A second Hello after handshake is a
                // protocol violation; ignore.
                tracing::debug!(
                    peer = %peer_id,
                    "audio-plane: stray Hello received post-handshake"
                );
            }
            AudioPlaneMessage::DomainWitnessAnnounce { witness } => {
                let _ = self.domain_witness_inbound_tx.send(
                    DomainWitnessInbound::Witness {
                        from_peer_id: peer_id.to_string(),
                        witness,
                    },
                );
            }
            AudioPlaneMessage::DomainWitnessRequest { from_hash_b64 } => {
                let _ = self.domain_witness_inbound_tx.send(
                    DomainWitnessInbound::TailRequest {
                        from_peer_id: peer_id.to_string(),
                        from_hash_b64,
                    },
                );
            }
            AudioPlaneMessage::DomainWitnessResponse { witnesses } => {
                let _ = self.domain_witness_inbound_tx.send(
                    DomainWitnessInbound::TailResponse {
                        from_peer_id: peer_id.to_string(),
                        witnesses,
                    },
                );
            }
            AudioPlaneMessage::CrossNetworkPresenceObservation {
                peer_id: observed_peer_id,
                network_id,
                state,
                observed_at_ms,
            } => {
                let _ = self.cross_network_presence_tx.send(
                    CrossNetworkPresenceReport {
                        relay_device_id: peer_id.to_string(),
                        peer_id: observed_peer_id,
                        network_id,
                        state,
                        observed_at_ms,
                    },
                );
            }
        }
    }

    async fn handle_sync_response(
        &self,
        peer_id: &str,
        group_id: &str,
        t1_ms: u64,
        t2_ms: u64,
    ) {
        let t4_ms = now_ms();
        let pending = {
            let mut g = self.inner.lock().await;
            g.pending_probes
                .remove(&(peer_id.to_string(), group_id.to_string()))
        };
        let Some(pending) = pending else {
            return;
        };
        if pending.t1_ms != t1_ms {
            // Echo mismatch; ignore.
            return;
        }
        let rtt = t4_ms.saturating_sub(pending.sent_at_ms);
        let half_rtt = (rtt / 2) as i64;
        // offset = source-host clock - local clock. Estimated
        // as `t2 - (t1 + half_rtt)`.
        let offset_ms = t2_ms as i64 - (pending.t1_ms as i64 + half_rtt);
        let uncertainty_ms = half_rtt as u32;
        self.clock_sync
            .record_sync_sample(
                group_id,
                peer_id,
                offset_ms,
                uncertainty_ms,
                t4_ms,
            )
            .await;
        // Update the connection-info offset surface.
        let mut g = self.inner.lock().await;
        if let Some(entry) = g.connections.get_mut(peer_id) {
            entry.info.last_sync_offset_ms = Some(offset_ms);
            entry.info.last_sync_at_ms = t4_ms;
        }
    }

    async fn mark_disconnected(&self, peer_id: &str) {
        let removed = {
            let mut g = self.inner.lock().await;
            g.connections.remove(peer_id)
        };
        if let Some(entry) = removed {
            entry.read_task.abort();
            entry.write_task.abort();
            self.emit(Happening::PeerDisconnected {
                device_id: peer_id.to_string(),
                at: std::time::SystemTime::now(),
            })
            .await;
        }
    }

    /// Housekeeping loop. Replaces the prior periodic-
    /// heartbeat sweep with two narrowly-scoped concerns:
    /// (a) reap connections that have crossed the
    /// total-channel-silence threshold, (b) ensure outbound
    /// connections to group-co-members exist, and (c)
    /// dispatch sync probes on the established cadence. The
    /// sync-probe step is intentionally retained — it is the
    /// clock-domain primitive that drives each receiver's
    /// sample-rate-skew PLL; pausing it would cost
    /// bit-perfect audio on transport-state resume, so the
    /// probes run at a steady cadence regardless of whether
    /// audio frames are currently in flight. No outbound
    /// heartbeats are dispatched; the channel-activity
    /// timestamp the housekeeping reaper inspects is updated
    /// by every inbound message (sync responses, audio
    /// frames, hello, goodbye, legacy heartbeats from older
    /// peers) so a connection that has any inbound traffic
    /// never reaches the reap threshold.
    async fn housekeeping_loop(self: Arc<Self>) {
        let mut last_sync_probe = 0u64;
        loop {
            tokio::time::sleep(self.config.sweep_interval).await;
            let now = now_ms();
            self.reap_idle_connections(now).await;
            self.ensure_outbound_connections().await;
            if now.saturating_sub(last_sync_probe)
                >= self.config.sync_probe_interval.as_millis() as u64
            {
                self.send_sync_probes(now).await;
                last_sync_probe = now;
            }
        }
    }

    /// Reap connections whose total channel silence has
    /// crossed `idle_reap_threshold`. The threshold is sized
    /// far above any operator-immediacy window — only
    /// genuinely dead connections (peer crashed, network
    /// partition, kernel-stack outage on the other side)
    /// cross it. Sync-probe responses and any other inbound
    /// traffic reset the silence clock; an idle paused
    /// group whose receiver still responds to sync probes
    /// keeps its connection indefinitely.
    async fn reap_idle_connections(&self, now: u64) {
        let threshold_ms = self.config.idle_reap_threshold.as_millis() as u64;
        let stale: Vec<String> = {
            let g = self.inner.lock().await;
            g.connections
                .values()
                .filter(|e| {
                    now.saturating_sub(e.info.last_channel_activity_ms)
                        > threshold_ms
                })
                .map(|e| e.info.remote_device_id.clone())
                .collect()
        };
        for id in stale {
            tracing::info!(
                peer = %id,
                "audio-plane: idle-reap threshold exceeded — tearing down"
            );
            self.mark_disconnected(&id).await;
        }
    }

    async fn ensure_outbound_connections(self: &Arc<Self>) {
        let now = now_ms();
        let local_id = self.local_device_id.0.clone();
        let elections = self.election.current().list_source_hosts().await;
        let peers = self.discovery.list_peers().await;
        let peer_addr_map: HashMap<String, Vec<String>> = peers
            .iter()
            .map(|p| (p.device_id.clone(), p.addresses.clone()))
            .collect();
        let cached_addr_map = {
            let g = self.inner.lock().await;
            g.cached_peer_addresses.clone()
        };

        // Unidirectional dial: the source-host is the only
        // active dialer; receivers stay passive listeners.
        // Eliminates the duplicate-connect race where both
        // peers race to open a control channel for the same
        // (source-host, receiver) pair — the audio-plane
        // connection lifecycle then has a single owner per
        // pair and no dedup-replace thrash.
        //
        // For each election where local is the source-host,
        // dial every other member of the group. Idempotent:
        // `already_present` below filters peers already
        // connected. Discovery-driven retry path: when a
        // receiver's address surfaces only after the source-
        // host's explicit `dial_peer` (operator-config one-
        // shot at plugin load) has run, this loop catches it.
        let mut targets: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        for (group_id, host_opt) in &elections {
            let Some(host) = host_opt.clone() else {
                continue;
            };
            if host != local_id {
                continue;
            }
            let group = match self.group_store.get(group_id).await {
                Ok(Some(g)) => g,
                _ => continue,
            };
            for member in group.members {
                if member == local_id {
                    continue;
                }
                let mut addresses =
                    peer_addr_map.get(&member).cloned().unwrap_or_default();
                if let Some(cached) = cached_addr_map.get(&member) {
                    for addr in cached {
                        if !addresses.iter().any(|a| a == addr) {
                            addresses.push(addr.clone());
                        }
                    }
                }
                if addresses.is_empty() {
                    continue;
                }
                targets
                    .entry(member)
                    .and_modify(|existing| {
                        for addr in &addresses {
                            if !existing.iter().any(|a| a == addr) {
                                existing.push(addr.clone());
                            }
                        }
                    })
                    .or_insert(addresses);
            }
        }

        for (peer_id, target_addrs) in targets {
            // Atomic check-and-claim: lock once, skip if the
            // peer is already connected OR if a dial is
            // already in flight. The second clause closes the
            // race where the prior sweep's dial has spawned
            // but its `run_connection` has not yet inserted
            // into `connections` (Hello exchange takes time);
            // without it, the second sweep dials again, both
            // dials complete handshake to the peer, the
            // dedup-on-handshake path aborts the prior, and
            // the aborted TCP stream surfaces as
            // `audio-plane: inbound connection failed` on
            // the peer.
            {
                let mut g = self.inner.lock().await;
                if let Some(retry) = g.dial_retry.get(&peer_id) {
                    if now < retry.next_retry_at_ms {
                        continue;
                    }
                }
                if g.connections.contains_key(&peer_id)
                    || g.in_flight_dials.contains(&peer_id)
                {
                    continue;
                }
                g.in_flight_dials.insert(peer_id.clone());
            }
            let runtime = Arc::clone(self);
            let target_addrs_for_task = target_addrs.clone();
            let peer_id_for_cleanup = peer_id.clone();
            tokio::spawn(async move {
                let cleanup_runtime = Arc::clone(&runtime);
                let mut connected = false;
                for target_addr in &target_addrs_for_task {
                    match TcpStream::connect(target_addr).await {
                        Ok(stream) => {
                            if let Err(e) = Arc::clone(&runtime)
                                .run_connection(
                                    stream,
                                    target_addr.clone(),
                                    ConnectionDirection::Outbound,
                                )
                                .await
                            {
                                // LOGGING.md §2: warn (recoverable
                                // anomaly — this endpoint failed but
                                // the dial loop continues with the
                                // next candidate; operator should
                                // review if every endpoint fails).
                                tracing::warn!(
                                    error = %e,
                                    addr = %target_addr,
                                    "audio-plane: outbound dial failed"
                                );
                                continue;
                            }
                            connected = true;
                            break;
                        }
                        Err(e) => {
                            tracing::debug!(
                                error = %e,
                                addr = %target_addr,
                                "audio-plane: TCP connect failed"
                            );
                        }
                    }
                }
                if !connected {
                    let mut g = cleanup_runtime.inner.lock().await;
                    let failures = g
                        .dial_retry
                        .get(&peer_id_for_cleanup)
                        .map(|r| r.consecutive_failures)
                        .unwrap_or(0)
                        .saturating_add(1);
                    let capped = failures.min(6);
                    let backoff_ms = 1_000u64.saturating_mul(1u64 << capped);
                    let jitter_ms = ((now_ms()
                        ^ (peer_id_for_cleanup.len() as u64 * 7919))
                        % 250)
                        + 50;
                    g.dial_retry.insert(
                        peer_id_for_cleanup.clone(),
                        DialRetryState {
                            consecutive_failures: failures,
                            next_retry_at_ms: now_ms().saturating_add(
                                backoff_ms.saturating_add(jitter_ms),
                            ),
                        },
                    );
                }
                // Release the in-flight-dial token regardless
                // of whether the handshake succeeded. On
                // success the peer is in `connections`; on
                // failure the next sweep retries.
                let mut g = cleanup_runtime.inner.lock().await;
                g.in_flight_dials.remove(&peer_id_for_cleanup);
            });
        }
    }

    async fn send_sync_probes(&self, now: u64) {
        // Identify groups the local node is a receiver for
        // (source-host is a remote peer). Send a probe per
        // such group on the connection to that peer.
        let elections = self.election.current().list_source_hosts().await;
        let local_id = self.local_device_id.0.clone();
        let g = self.inner.lock().await;
        for (group_id, host_opt) in &elections {
            let Some(host) = host_opt.clone() else {
                continue;
            };
            if host == local_id {
                continue;
            }
            let Some(entry) = g.connections.get(&host) else {
                continue;
            };
            let probe = AudioPlaneMessage::SyncProbe {
                group_id: group_id.clone(),
                t1_ms: now,
            };
            let _ = entry.write_tx.send(probe);
        }
        drop(g);
        // Record pending probes after releasing the read
        // lock so we don't reacquire while iterating.
        let elections = self.election.current().list_source_hosts().await;
        let mut g = self.inner.lock().await;
        for (group_id, host_opt) in &elections {
            let Some(host) = host_opt.clone() else {
                continue;
            };
            if host == local_id {
                continue;
            }
            if !g.connections.contains_key(&host) {
                continue;
            }
            g.pending_probes.insert(
                (host, group_id.clone()),
                PendingProbe {
                    t1_ms: now,
                    sent_at_ms: now,
                },
            );
        }
    }

    async fn emit(&self, h: Happening) {
        if let Err(e) = self.happenings.emit_durable(h).await {
            tracing::warn!(
                error = %e,
                "audio-plane: emit happening failed"
            );
        }
    }
}

/// Source-host-side input to [`AudioPlaneRuntime::fan_out_audio_frame`].
#[derive(Debug, Clone)]
pub struct AudioFrameSeed {
    /// Per-group monotonically-increasing sequence number.
    pub sequence: u64,
    /// Source-host's monotonic ms at which the frame should
    /// be played.
    pub presentation_time_ms: u64,
    /// Codec discriminator.
    pub codec: String,
    /// Sample rate.
    pub rate_hz: u32,
    /// Channel count.
    pub channels: u16,
    /// Base64-encoded codec payload bytes.
    pub payload_b64: String,
}

fn connection_direction_str(d: ConnectionDirection) -> &'static str {
    match d {
        ConnectionDirection::Inbound => "inbound",
        ConnectionDirection::Outbound => "outbound",
    }
}

fn pick_lower(a: &str, b: &str) -> String {
    if a <= b {
        a.to_string()
    } else {
        b.to_string()
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

async fn read_message<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> Result<AudioPlaneMessage, AudioPlaneError> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 {
        return Err(AudioPlaneError::Protocol("zero-length frame".into()));
    }
    if len > MAX_FRAME_BYTES {
        return Err(AudioPlaneError::FrameTooLarge(len));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    let msg: AudioPlaneMessage = serde_json::from_slice(&body)?;
    Ok(msg)
}

async fn write_message<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    msg: &AudioPlaneMessage,
) -> Result<(), AudioPlaneError> {
    let body = serde_json::to_vec(msg)?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(AudioPlaneError::FrameTooLarge(body.len()));
    }
    let len = (body.len() as u32).to_be_bytes();
    writer.write_all(&len).await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

// ---------------------------------------------------------------
// Steward-facing face.
//
// The steward keeps two wire ops that report on the plane, and
// group topology reads the same connection snapshot. It reaches
// them through `evo::AudioPlaneControl` rather than through this
// concrete type, so a distribution without a plane still compiles
// and still answers those ops with their not-configured degrade.
// ---------------------------------------------------------------

impl evo::AudioPlaneControl for AudioPlaneRuntime {
    fn list_connections<'a>(
        &'a self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Vec<PeerConnectionInfo>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move { AudioPlaneRuntime::list_connections(self).await })
    }

    fn broadcast_domain_witness_request<'a>(
        &'a self,
        from_hash_b64: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>
    {
        Box::pin(async move {
            self.broadcast_to_peers(AudioPlaneMessage::DomainWitnessRequest {
                from_hash_b64,
            })
            .await;
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_lower_orders_lexicographically() {
        assert_eq!(pick_lower("aaa", "bbb"), "aaa");
        assert_eq!(pick_lower("zzz", "aaa"), "aaa");
        assert_eq!(pick_lower("xyz", "xyz"), "xyz");
    }

    #[test]
    fn message_hello_round_trips() {
        let msg = AudioPlaneMessage::Hello {
            device_id: "abc".into(),
            framework_version: "0.1.13".into(),
            claimed_source_host_groups: vec!["g1".into(), "g2".into()],
        };
        let s = serde_json::to_string(&msg).unwrap();
        let back: AudioPlaneMessage = serde_json::from_str(&s).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn message_sync_probe_round_trips() {
        let msg = AudioPlaneMessage::SyncProbe {
            group_id: "g1".into(),
            t1_ms: 1234,
        };
        let s = serde_json::to_string(&msg).unwrap();
        let back: AudioPlaneMessage = serde_json::from_str(&s).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn message_audio_frame_round_trips() {
        let msg = AudioPlaneMessage::AudioFrame {
            group_id: "g1".into(),
            sequence: 42,
            presentation_time_ms: 1_000_000,
            codec: "pcm_s16_le".into(),
            rate_hz: 44_100,
            channels: 2,
            payload_b64: "SGVsbG8gd29ybGQ".into(),
        };
        let s = serde_json::to_string(&msg).unwrap();
        let back: AudioPlaneMessage = serde_json::from_str(&s).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn audio_plane_config_default_enabled() {
        let c = AudioPlaneConfig::default();
        assert!(c.enabled);
        assert_eq!(c.control_port, 7331);
        assert_eq!(c.sweep_interval, Duration::from_secs(1));
        assert_eq!(c.idle_reap_threshold, Duration::from_secs(120));
        assert_eq!(c.sync_probe_interval, Duration::from_secs(5));
    }

    #[tokio::test]
    async fn read_write_round_trip() {
        let (mut a, mut b) = tokio::io::duplex(8192);
        let msg = AudioPlaneMessage::Heartbeat {
            device_id: "x".into(),
            monotonic_ms: 100,
            groups: vec!["g1".into()],
        };
        write_message(&mut a, &msg).await.unwrap();
        let back = read_message(&mut b).await.unwrap();
        assert_eq!(back, msg);
    }

    /// Two `AudioPlaneRuntime` instances, both bound on real
    /// loopback TCP sockets, must complete the symmetric Hello
    /// handshake within the configured 5-second window. The
    /// pre-fix implementation queued the local Hello into an
    /// mpsc channel before the write-pump task was spawned;
    /// both peers deadlocked waiting for each other's Hello
    /// because neither's frame ever reached the wire. This
    /// test wires two runtimes against each other so the
    /// regression is caught at compile-and-test time rather
    /// than only on a real two-node deployment.
    #[tokio::test]
    async fn two_runtimes_handshake_and_connect_over_loopback() {
        use evo::clock_sync::{ClockSyncConfig, ClockSyncRuntime};
        use evo::discovery::{DiscoveryConfig, DiscoveryRuntime};
        use evo::groups::GroupStore;
        use evo::happenings::HappeningBus;
        use evo::persistence::{MemoryPersistenceStore, PersistenceStore};
        use std::time::Duration;

        async fn build_runtime(
            local_id: &str,
            control_port: u16,
        ) -> Arc<AudioPlaneRuntime> {
            let persistence: Arc<dyn PersistenceStore> =
                Arc::new(MemoryPersistenceStore::default());
            let bus = Arc::new(HappeningBus::with_capacity(256));
            let groups = Arc::new(GroupStore::new(
                Arc::clone(&persistence),
                Arc::clone(&bus),
            ));
            let discovery = Arc::new(DiscoveryRuntime::new(
                Arc::clone(&persistence),
                Arc::clone(&bus),
                DiscoveryConfig {
                    enabled: false,
                    ..Default::default()
                },
            ));
            let clock_sync = Arc::new(ClockSyncRuntime::new(
                Arc::clone(&bus),
                Arc::clone(&groups),
                DeviceId(local_id.to_string()),
                ClockSyncConfig::default(),
            ));
            // No-op election state: the handshake test exercises
            // the audio-plane transport path, not source-host
            // election semantics. A `NoElection` shared handle
            // satisfies the runtime's `SharedElectionState`
            // dependency without pulling the multi-room crate
            // (which would create a dev-dep cycle that compiles
            // `evo` twice).
            let shared_election = evo_primitives::SharedElectionState::no_op();
            Arc::new(AudioPlaneRuntime::new(
                AudioPlaneConfig {
                    enabled: true,
                    control_port,
                    ..Default::default()
                },
                bus,
                discovery,
                shared_election,
                clock_sync,
                groups,
                DeviceId(local_id.to_string()),
            ))
        }

        // Bind both runtimes on operator-arbitrary ports above
        // the default 7331 so the test never collides with a
        // running steward on the dev box.
        let alice = build_runtime("alice", 27331).await;
        let bob = build_runtime("bob", 27332).await;
        alice.start().await.expect("alice.start");
        bob.start().await.expect("bob.start");

        // Alice dials Bob. Either dial direction exercises the
        // same `run_connection` code path on both sides.
        let bob_addr: std::net::SocketAddr = "127.0.0.1:27332".parse().unwrap();
        let alice_for_dial = Arc::clone(&alice);
        tokio::spawn(async move {
            // `dial_for_test` runs the handshake inline + then
            // returns once the read loop ends. Spawn it so this
            // test body can observe the pair entering Connected
            // state through `list_connections` rather than
            // waiting for the dial to return.
            let _ = alice_for_dial.dial_for_test(bob_addr).await;
        });

        // Both peers must enter Connected state within ~1 s.
        // Pre-fix: both timed out at 5 s with "hello timeout".
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let alice_sees_bob =
                alice.list_connections().await.iter().any(|p| {
                    p.remote_device_id == "bob"
                        && p.state == PeerConnectionState::Connected
                });
            let bob_sees_alice = bob.list_connections().await.iter().any(|p| {
                p.remote_device_id == "alice"
                    && p.state == PeerConnectionState::Connected
            });
            if alice_sees_bob && bob_sees_alice {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!(
                    "handshake did not complete: alice_sees_bob={} \
                     bob_sees_alice={}",
                    alice_sees_bob, bob_sees_alice,
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn read_rejects_oversize_frame() {
        let (mut a, mut b) = tokio::io::duplex(8192);
        let len_bytes = (2_000_000_u32).to_be_bytes();
        a.write_all(&len_bytes).await.unwrap();
        let err = read_message(&mut b).await.unwrap_err();
        assert!(matches!(err, AudioPlaneError::FrameTooLarge(_)));
    }
}
