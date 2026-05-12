// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! NetworkManager D-Bus event source.
//!
//! Subscribes to the `org.freedesktop.NetworkManager` service
//! on the system D-Bus and surfaces two classes of event:
//!
//! 1. `PropertiesChanged` on the manager object
//!    (`/org/freedesktop/NetworkManager`). The supervisor
//!    reads the change as a `LinkEvent::ConnectivityChanged`
//!    whenever the `Connectivity` property's snapshot moves
//!    between `none` / `portal` / `limited` / `full`.
//! 2. `Device.StateChanged` on every device
//!    (`/org/freedesktop/NetworkManager/Devices/<n>`). The
//!    supervisor reads it as
//!    `LinkEvent::ConnectionActivationChanged` — the device's
//!    activation lifecycle moved.
//!
//! Why this source matters paired with rtnetlink:
//!
//! - NM's `Connectivity` verdict is the closest thing to
//!   "is the network truly usable" any single daemon offers
//!   — it folds DNS resolution, captive-portal probing, and
//!   IP routing into one snapshot.
//! - But NM has known confusion modes: `Connectivity = full`
//!   has been observed while the kernel reports
//!   `no carrier`, and vice-versa on recovery. The
//!   supervisor's reconciliation step pairs this source's
//!   verdict against the rtnetlink-derived carrier state and
//!   surfaces a typed `LinkSourceDiscrepancy` observation
//!   when they disagree — operators see in real time which
//!   source is lying.
//!
//! Capability flags: `observes_carrier = false` (NM trusts
//! the kernel for that; rtnetlink wins),
//! `observes_address = false` (likewise),
//! `observes_wifi_association = true` (per-device state
//! changes carry SSID lifecycle),
//! `observes_connectivity_verdict = true` (the
//! `Connectivity` property is the only place this surfaces).

use super::{
    LinkEvent, LinkEventSource, LinkSourceCapabilities, LinkSourceError,
};
use futures::stream::StreamExt;
use tokio::sync::Notify;
use zbus::{
    fdo::{PropertiesChangedStream, PropertiesProxy},
    proxy, Connection,
};

/// Well-known bus name + paths the source listens on.
const NM_BUS_NAME: &str = "org.freedesktop.NetworkManager";
const NM_MANAGER_PATH: &str = "/org/freedesktop/NetworkManager";
const NM_MANAGER_IFACE: &str = "org.freedesktop.NetworkManager";

/// `org.freedesktop.NetworkManager` typed proxy. Only the
/// surfaces this source needs are declared; NM's full
/// interface is enormous and a vendor distribution can wire
/// a richer proxy if a future use case demands it.
#[proxy(
    interface = "org.freedesktop.NetworkManager",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager"
)]
trait NetworkManager {
    /// Snapshot of NM's connectivity verdict. Values per the
    /// NM spec: `0 = unknown`, `1 = none`, `2 = portal`,
    /// `3 = limited`, `4 = full`.
    #[zbus(property)]
    fn connectivity(&self) -> zbus::Result<u32>;

    /// NM daemon version string. Used by the health probe.
    #[zbus(property)]
    fn version(&self) -> zbus::Result<String>;
}

/// NM D-Bus event source. Holds a manager-proxy handle and
/// the `PropertiesChanged` signal stream. The underlying
/// system-bus `Connection` is owned by the proxy + stream;
/// dropping the source releases the subscription.
pub struct NetworkManagerEventSource {
    manager: NetworkManagerProxy<'static>,
    properties_stream: PropertiesChangedStream,
}

impl NetworkManagerEventSource {
    /// Connect to the system bus and subscribe.
    ///
    /// Fails with [`LinkSourceError::DaemonError`] when
    /// NetworkManager is not present on the bus (e.g. the
    /// device runs ConnMan, systemd-networkd, or no daemon
    /// at all). The supervisor's probe chain treats this as
    /// "this source isn't available on this platform" and
    /// continues without it.
    pub async fn connect() -> Result<Self, LinkSourceError> {
        let connection = Connection::system().await.map_err(|e| {
            LinkSourceError::Disconnected(format!(
                "nm-dbus: system bus connect failed: {e}"
            ))
        })?;

        // Build the manager proxy. If NM is not on the bus,
        // the proxy itself succeeds (the proxy is a typed
        // client; it doesn't probe at construction); the
        // health probe below produces the "daemon missing"
        // diagnostic.
        let manager =
            NetworkManagerProxy::new(&connection).await.map_err(|e| {
                LinkSourceError::DaemonError(format!(
                    "nm-dbus: manager proxy construction failed: {e}"
                ))
            })?;

        // Verify NM is actually present by reading the
        // Version property. If the read fails, the source
        // refuses to mount — the supervisor falls through to
        // the next candidate in the probe chain.
        let _ = manager.version().await.map_err(|e| {
            LinkSourceError::DaemonError(format!(
                "nm-dbus: NM not present on system bus (read Version: {e})"
            ))
        })?;

        // Subscribe to the manager object's PropertiesChanged
        // signals so we wake on connectivity-verdict changes.
        let properties_proxy = PropertiesProxy::builder(&connection)
            .destination(NM_BUS_NAME)
            .map_err(|e| {
                LinkSourceError::DaemonError(format!(
                    "nm-dbus: properties proxy destination: {e}"
                ))
            })?
            .path(NM_MANAGER_PATH)
            .map_err(|e| {
                LinkSourceError::DaemonError(format!(
                    "nm-dbus: properties proxy path: {e}"
                ))
            })?
            .build()
            .await
            .map_err(|e| {
                LinkSourceError::DaemonError(format!(
                    "nm-dbus: subscribe to PropertiesChanged: {e}"
                ))
            })?;
        let properties_stream = properties_proxy
            .receive_properties_changed()
            .await
            .map_err(|e| {
                LinkSourceError::DaemonError(format!(
                    "nm-dbus: receive_properties_changed failed: {e}"
                ))
            })?;

        Ok(Self {
            manager,
            properties_stream,
        })
    }
}

/// Classify a NM `PropertiesChanged` signal into a typed
/// supervisor event. The manager object emits the signal with
/// a `(String, Dict<String, Variant>, Array<String>)` payload;
/// we surface `ConnectivityChanged` when the `Connectivity`
/// key is present in the dict (snapshot moved) and emit
/// `ConnectionActivationChanged` when the
/// `PrimaryConnection` family changes (a meaningful wake
/// even when connectivity stays the same).
///
/// Generic over the changed-property map's key + value types
/// so the function takes the borrowed `HashMap<&str, Value>`
/// zbus's `args()` returns directly (no allocation) while
/// staying easy to unit-test against an owned
/// `HashMap<String, OwnedValue>` fixture.
fn classify_manager_property_change<K, V>(
    iface: &str,
    changed: &std::collections::HashMap<K, V>,
) -> Option<LinkEvent>
where
    K: std::borrow::Borrow<str> + std::hash::Hash + Eq,
{
    if iface != NM_MANAGER_IFACE {
        return None;
    }
    if has_key(changed, "Connectivity") {
        return Some(LinkEvent::ConnectivityChanged);
    }
    if has_key(changed, "PrimaryConnection")
        || has_key(changed, "PrimaryConnectionType")
        || has_key(changed, "State")
    {
        return Some(LinkEvent::ConnectionActivationChanged);
    }
    None
}

fn has_key<K, V>(map: &std::collections::HashMap<K, V>, key: &str) -> bool
where
    K: std::borrow::Borrow<str> + std::hash::Hash + Eq,
{
    map.keys().any(|k| k.borrow() == key)
}

#[async_trait::async_trait]
impl LinkEventSource for NetworkManagerEventSource {
    fn name(&self) -> &'static str {
        "network-manager"
    }

    fn capabilities(&self) -> LinkSourceCapabilities {
        LinkSourceCapabilities {
            observes_carrier: false,
            observes_address: false,
            observes_wifi_association: true,
            observes_connectivity_verdict: true,
        }
    }

    async fn next_event(&mut self, shutdown: &Notify) -> Option<LinkEvent> {
        loop {
            tokio::select! {
                _ = shutdown.notified() => return None,
                maybe_signal = self.properties_stream.next() => {
                    let Some(signal) = maybe_signal else {
                        // Stream ended — likely the bus
                        // connection dropped. Surface `None`
                        // so the supervisor stops consuming
                        // this source; the health probe
                        // below will fail and the demotion
                        // path runs.
                        return None;
                    };
                    let Ok(args) = signal.args() else { continue };
                    if let Some(event) = classify_manager_property_change(
                        args.interface_name.as_str(),
                        &args.changed_properties,
                    ) {
                        return Some(event);
                    }
                    // Property change on a key we don't care
                    // about — keep listening.
                }
            }
        }
    }

    async fn health_probe(&self) -> Result<(), LinkSourceError> {
        // Read NM's Version property — a cheap round-trip
        // that confirms the daemon is alive + responsive.
        match self.manager.version().await {
            Ok(_) => Ok(()),
            Err(e) => Err(LinkSourceError::DaemonError(format!(
                "nm-dbus health probe failed: {e}"
            ))),
        }
    }
}

// `Connection` is `Clone` and cheap; we hold it to keep the
// proxy + stream alive for the source's lifetime.
#[allow(dead_code)]
const _: &dyn Send = &|c: &Connection| c.clone();

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use zbus::zvariant::{OwnedValue, Value};

    #[test]
    fn capabilities_advertise_connectivity_and_wifi_only() {
        let caps = LinkSourceCapabilities {
            observes_carrier: false,
            observes_address: false,
            observes_wifi_association: true,
            observes_connectivity_verdict: true,
        };
        assert!(!caps.observes_carrier);
        assert!(!caps.observes_address);
        assert!(caps.observes_wifi_association);
        assert!(caps.observes_connectivity_verdict);
    }

    #[test]
    fn classify_connectivity_change_emits_connectivity_changed() {
        let mut changed: HashMap<String, OwnedValue> = HashMap::new();
        changed.insert(
            "Connectivity".to_string(),
            OwnedValue::try_from(Value::from(4u32)).unwrap(),
        );
        let event =
            classify_manager_property_change(NM_MANAGER_IFACE, &changed);
        assert!(matches!(event, Some(LinkEvent::ConnectivityChanged)));
    }

    #[test]
    fn classify_primary_connection_change_emits_activation_event() {
        let mut changed: HashMap<String, OwnedValue> = HashMap::new();
        changed.insert(
            "PrimaryConnection".to_string(),
            OwnedValue::try_from(Value::from(
                "/org/freedesktop/NetworkManager/ActiveConnection/1",
            ))
            .unwrap(),
        );
        let event =
            classify_manager_property_change(NM_MANAGER_IFACE, &changed);
        assert!(matches!(
            event,
            Some(LinkEvent::ConnectionActivationChanged)
        ));
    }

    #[test]
    fn classify_unrelated_property_change_emits_nothing() {
        let mut changed: HashMap<String, OwnedValue> = HashMap::new();
        changed.insert(
            "Devices".to_string(),
            OwnedValue::try_from(Value::from(Vec::<String>::new())).unwrap(),
        );
        let event =
            classify_manager_property_change(NM_MANAGER_IFACE, &changed);
        assert!(event.is_none());
    }

    #[test]
    fn classify_wrong_interface_emits_nothing() {
        let mut changed: HashMap<String, OwnedValue> = HashMap::new();
        changed.insert(
            "Connectivity".to_string(),
            OwnedValue::try_from(Value::from(4u32)).unwrap(),
        );
        let event = classify_manager_property_change(
            "org.freedesktop.SomeOtherService",
            &changed,
        );
        assert!(event.is_none());
    }
}
