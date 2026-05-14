// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Published `networking.link.connectivity` subject.
//!
//! Per the connectivity-check redesign the network plugin
//! publishes a queryable subject carrying the current
//! connectivity verdict. Consumers (UI, online-metadata plugins,
//! share-mount controllers, operator wire-ops) read this subject
//! through the framework's subject-state addressing instead of
//! grepping `Happening::PluginEvent` instances or parsing
//! journal lines.
//!
//! The subject is updated on rtnetlink carrier / route changes,
//! NetworkManager D-Bus Connectivity / PrimaryConnection
//! signals, on-demand `refresh_connectivity` wire-op
//! invocations, or the polling source's adaptive-tick fallback
//! (cold-start only). Updates are driven from
//! [`crate::supervisor::SupervisorView`] state-machine
//! transitions; the publish path is best-effort (a failed
//! announce / update logs at warn but never panics).

use serde::Serialize;
use std::net::IpAddr;
use std::time::SystemTime;

use crate::supervisor::{ReachabilityState, SupervisorView};

/// External-addressing scheme for the connectivity subject.
/// Consumers resolve the canonical id by querying the framework's
/// subject querier with `(scheme = CONNECTIVITY_SCHEME, value =
/// CONNECTIVITY_VALUE)`.
pub const CONNECTIVITY_SCHEME: &str = "evo.networking.link";

/// External-addressing value for the connectivity subject.
pub const CONNECTIVITY_VALUE: &str = "connectivity";

/// Subject type registered with the framework. Matches the
/// `[[subjects]]` declaration in
/// `evo-catalogue-schemas/schemas/org.evoframework/networking/link.v1.toml`.
pub const CONNECTIVITY_SUBJECT_TYPE: &str = "network_connectivity_state";

/// Origin of the latest connectivity-state update.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConnectivitySource {
    /// Source is rtnetlink (kernel link-state events).
    Rtnetlink,
    /// Source is the NetworkManager D-Bus event surface.
    Nm,
    /// Source is the cold-start polling fallback.
    Polling,
    /// Source is an on-demand `network.refresh_connectivity`
    /// wire-op invocation.
    OnDemand,
    /// The subject has been announced but no event-driven
    /// update has fired yet — the initial-announce state.
    #[default]
    Boot,
}

/// Snapshot of the device's current connectivity verdict
/// published on the `networking.link.connectivity` subject.
///
/// The shape mirrors the schema declaration in the
/// catalogue-schemas repo. Some fields (`ip_address`,
/// `default_gateway`, `dns_resolves`) are reported as `None` /
/// `false` until the supervisor's observation surface gains the
/// matching probe paths (rtnetlink RTM_GETROUTE for the
/// gateway, getifaddrs for addresses, getaddrinfo against a
/// fixed canary hostname for DNS). The current ADR-locked fields
/// are filled in from the supervisor's existing observation
/// payload.
#[derive(Debug, Clone, Serialize)]
pub struct NetworkConnectivityState {
    /// At least one non-loopback interface is administratively
    /// up. Derived from the same uplink-presence check the
    /// supervisor uses to classify reachability.
    pub interface_up: bool,
    /// At least one non-loopback interface has link carrier.
    pub carrier: bool,
    /// Preferred routable address (first non-link-local on the
    /// primary interface). `None` until rtnetlink address read
    /// lands as a follow-up observation in the supervisor.
    pub ip_address: Option<IpAddr>,
    /// Current default-route next hop. `None` until rtnetlink
    /// route read lands as a follow-up observation.
    pub default_gateway: Option<IpAddr>,
    /// Last DNS resolution attempt succeeded. `false` until a
    /// dedicated DNS-resolve probe lands.
    pub dns_resolves: bool,
    /// Last probe verdict. `None` when `probe_kind = off` and
    /// no probe has been requested.
    pub internet_reachable: Option<bool>,
    /// Timestamp of the last probe attempt regardless of verdict.
    pub last_probe_at: Option<SystemTime>,
    /// Timestamp of the last subject-state update.
    pub last_change_at: SystemTime,
    /// Input that triggered the last subject update.
    pub source: ConnectivitySource,
}

impl Default for NetworkConnectivityState {
    fn default() -> Self {
        Self {
            interface_up: false,
            carrier: false,
            ip_address: None,
            default_gateway: None,
            dns_resolves: false,
            internet_reachable: None,
            last_probe_at: None,
            last_change_at: SystemTime::now(),
            source: ConnectivitySource::default(),
        }
    }
}

impl NetworkConnectivityState {
    /// Build a [`NetworkConnectivityState`] from the supervisor's
    /// current view. The `source` field is supplied by the
    /// caller because the supervisor's view is the same shape
    /// regardless of which input drove the latest update.
    pub fn from_supervisor_view(
        view: &SupervisorView,
        source: ConnectivitySource,
    ) -> Self {
        let obs = &view.last_observations;
        let has_uplink = obs.ethernet_carrier_up || obs.wifi_associated;
        let internet_reachable = match obs.probe_http_code {
            Some(204) => Some(true),
            // Anything else (None, redirect, 5xx) is best
            // expressed as `None` rather than a false negative:
            // a `None` `probe_http_code` may mean the probe is
            // off, not that the device is offline.
            Some(_) | None => match view.reachability {
                ReachabilityState::Online => Some(true),
                ReachabilityState::Limited
                | ReachabilityState::Portal
                | ReachabilityState::Offline => Some(false),
                ReachabilityState::Unknown => None,
            },
        };
        let last_probe_at = obs.probe_http_code.map(|_| SystemTime::now());
        Self {
            interface_up: has_uplink,
            carrier: has_uplink,
            // ip_address / default_gateway / dns_resolves: see
            // struct-doc note; awaiting follow-up observation
            // surfaces.
            ip_address: None,
            default_gateway: None,
            dns_resolves: false,
            internet_reachable,
            last_probe_at,
            last_change_at: SystemTime::now(),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::SupervisorObservations;

    #[test]
    fn from_supervisor_view_online() {
        let view = SupervisorView {
            reachability: ReachabilityState::Online,
            last_observations: SupervisorObservations {
                ethernet_carrier_up: true,
                wifi_associated: false,
                probe_http_code: Some(204),
                ..Default::default()
            },
            ..Default::default()
        };
        let s =
            NetworkConnectivityState::from_supervisor_view(&view, ConnectivitySource::Rtnetlink);
        assert!(s.carrier);
        assert!(s.interface_up);
        assert_eq!(s.internet_reachable, Some(true));
        assert_eq!(s.source, ConnectivitySource::Rtnetlink);
    }

    #[test]
    fn from_supervisor_view_offline() {
        let view = SupervisorView {
            reachability: ReachabilityState::Offline,
            last_observations: SupervisorObservations::default(),
            ..Default::default()
        };
        let s =
            NetworkConnectivityState::from_supervisor_view(&view, ConnectivitySource::Polling);
        assert!(!s.carrier);
        assert_eq!(s.internet_reachable, Some(false));
    }

    #[test]
    fn from_supervisor_view_unknown_at_boot() {
        let view = SupervisorView::default();
        let s = NetworkConnectivityState::from_supervisor_view(
            &view,
            ConnectivitySource::Boot,
        );
        assert!(!s.interface_up);
        assert_eq!(s.internet_reachable, None);
        assert_eq!(s.source, ConnectivitySource::Boot);
    }
}
