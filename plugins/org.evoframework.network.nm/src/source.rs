// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Link event source abstraction.
//!
//! The supervisor consumes one or more [`LinkEventSource`]
//! implementations concurrently. Each source emits a
//! [`LinkEvent`] when something it can observe changes —
//! for the polling source that's a periodic-tick wake;
//! for rtnetlink it's a kernel link-state notification;
//! for NetworkManager D-Bus it's a `PropertiesChanged` or
//! `Device.StateChanged` signal.
//!
//! The event is a *trigger*, not a data source. The
//! supervisor's `compose_observations` callback runs
//! after every wake to produce the authoritative
//! [`crate::supervisor::SupervisorObservations`] snapshot
//! from the privileged-exec dispatcher. This keeps the
//! state machine (`classify_reachability` + `step`)
//! independent of which source woke the loop —
//! observations are data, events are timing.

use std::fmt;
use tokio::sync::Notify;

/// One trigger from an event source. The supervisor wakes
/// on receipt and runs its `compose_observations` callback;
/// the kind here informs observability and adaptive-cadence
/// decisions but does NOT shape the state machine's input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkEvent {
    /// A periodic safety tick fired. Emitted by the
    /// polling source on its configured cadence.
    PeriodicTick,

    /// A network interface's carrier or address state
    /// changed. The source name carries the originating
    /// surface (`"rtnetlink"`, `"network-manager"`, etc.).
    InterfaceStateChanged {
        /// Name of the interface that changed (e.g.
        /// `"wlan0"`), when the source could identify it.
        interface: Option<String>,
    },

    /// The userspace daemon's connectivity verdict
    /// changed (full / portal / limited / none). Emitted by
    /// daemon-tracking sources like NetworkManager.
    ConnectivityChanged,

    /// Wi-Fi association came up or went down. Emitted by
    /// sources that surface association state distinctly.
    WifiAssociationChanged {
        /// `true` when the new state is associated.
        associated: bool,
    },

    /// A connection's activation lifecycle advanced (NM's
    /// `Connection.Active.StateChanged`, or equivalent).
    ConnectionActivationChanged,
}

impl LinkEvent {
    /// Short stable identifier for observability.
    pub fn kind(&self) -> &'static str {
        match self {
            LinkEvent::PeriodicTick => "periodic_tick",
            LinkEvent::InterfaceStateChanged { .. } => {
                "interface_state_changed"
            }
            LinkEvent::ConnectivityChanged => "connectivity_changed",
            LinkEvent::WifiAssociationChanged { .. } => {
                "wifi_association_changed"
            }
            LinkEvent::ConnectionActivationChanged => {
                "connection_activation_changed"
            }
        }
    }
}

/// Capabilities advertised by a source — what shape of
/// signal it can produce. Lets the supervisor reason about
/// coverage gaps when sources are demoted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LinkSourceCapabilities {
    /// `true` when the source observes layer-2 carrier
    /// state (Ethernet link up/down).
    pub observes_carrier: bool,
    /// `true` when the source observes IP address
    /// attachment/removal.
    pub observes_address: bool,
    /// `true` when the source observes Wi-Fi association.
    pub observes_wifi_association: bool,
    /// `true` when the source observes a userspace
    /// connectivity verdict (full / portal / limited /
    /// none).
    pub observes_connectivity_verdict: bool,
}

impl LinkSourceCapabilities {
    /// Capabilities of the polling source: it produces a
    /// fresh observation every tick regardless of what
    /// changed, so it covers every observation field by
    /// composition (rather than by observation).
    pub const fn polling() -> Self {
        Self {
            observes_carrier: true,
            observes_address: true,
            observes_wifi_association: true,
            observes_connectivity_verdict: true,
        }
    }
}

/// Structured error returned by a source's health probe.
/// Surfaces in `LinkSourceDemoted` observations.
#[derive(Debug, thiserror::Error)]
pub enum LinkSourceError {
    /// The source's underlying connection was lost (D-Bus
    /// disconnect, netlink socket closed, etc.).
    #[error("source disconnected: {0}")]
    Disconnected(String),

    /// The source initialised but no events have arrived
    /// within an expected window. Distinct from
    /// `Disconnected` so the supervisor can choose to
    /// degrade rather than demote.
    #[error("source silent past expected cadence")]
    Silent,

    /// The source's dependent userspace daemon reported
    /// an internal error (e.g. NM in `unmanaged` state).
    #[error("source daemon reported error: {0}")]
    DaemonError(String),

    /// Catch-all for source-specific failures.
    #[error("source failure: {0}")]
    Other(String),
}

/// A pluggable surface that wakes the supervisor when
/// network-relevant state changes. Implementations:
///
/// - [`polling::PollingEventSource`] — universal correctness
///   floor; periodic-tick wake. Mandatory wherever it is
///   implementable (every platform that can host a shell).
/// - `rtnetlink::RtnetlinkEventSource` — Linux kernel
///   netlink socket subscriber; layer-2 + layer-3 events.
///   (Planned; not in this chunk.)
/// - `network_manager::NetworkManagerEventSource` — D-Bus
///   subscriber for `org.freedesktop.NetworkManager`
///   PropertiesChanged + Device.StateChanged signals.
///   (Planned; not in this chunk.)
///
/// Future implementations (ConnMan, systemd-networkd, iwd,
/// BSD devd, macOS SystemConfiguration, ESP-IDF) plug into
/// this same trait without re-shaping the supervisor.
#[async_trait::async_trait]
pub trait LinkEventSource: Send + Sync {
    /// Stable identifier surfaced in observations + the
    /// reconciliation rule table.
    fn name(&self) -> &'static str;

    /// What this source can observe.
    fn capabilities(&self) -> LinkSourceCapabilities;

    /// Wait for the next event from this source or for the
    /// supplied shutdown notifier to fire. Returns `None`
    /// on shutdown; the supervisor stops consuming the
    /// source.
    async fn next_event(&mut self, shutdown: &Notify) -> Option<LinkEvent>;

    /// Probe the source's health. Returns `Ok(())` when the
    /// source believes it can continue producing events;
    /// any `Err` triggers the supervisor's demotion path
    /// (the source stops being consumed, with exponentially-
    /// backed-off re-admission attempts later).
    ///
    /// Default implementation: always healthy. Sources
    /// that depend on a userspace daemon override this to
    /// probe the daemon directly.
    async fn health_probe(&self) -> Result<(), LinkSourceError> {
        Ok(())
    }
}

impl fmt::Debug for dyn LinkEventSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinkEventSource")
            .field("name", &self.name())
            .field("capabilities", &self.capabilities())
            .finish()
    }
}

pub mod polling;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_event_kind_strings_are_stable() {
        assert_eq!(LinkEvent::PeriodicTick.kind(), "periodic_tick");
        assert_eq!(
            LinkEvent::InterfaceStateChanged { interface: None }.kind(),
            "interface_state_changed",
        );
        assert_eq!(
            LinkEvent::ConnectivityChanged.kind(),
            "connectivity_changed",
        );
        assert_eq!(
            LinkEvent::WifiAssociationChanged { associated: true }.kind(),
            "wifi_association_changed",
        );
        assert_eq!(
            LinkEvent::ConnectionActivationChanged.kind(),
            "connection_activation_changed",
        );
    }

    #[test]
    fn polling_capabilities_cover_every_field() {
        let c = LinkSourceCapabilities::polling();
        assert!(c.observes_carrier);
        assert!(c.observes_address);
        assert!(c.observes_wifi_association);
        assert!(c.observes_connectivity_verdict);
    }

    #[test]
    fn default_capabilities_are_empty() {
        let c = LinkSourceCapabilities::default();
        assert!(!c.observes_carrier);
        assert!(!c.observes_address);
        assert!(!c.observes_wifi_association);
        assert!(!c.observes_connectivity_verdict);
    }
}
