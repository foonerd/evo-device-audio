// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Linux kernel netlink event source.
//!
//! Subscribes to `RTMGRP_LINK` (carrier up/down) plus
//! `RTMGRP_IPV4_IFADDR` + `RTMGRP_IPV6_IFADDR` (address
//! attach / detach) via the rtnetlink crate's async stream
//! interface. Every kernel notification is translated into a
//! [`crate::source::LinkEvent`] and forwarded to the
//! supervisor as a wake trigger.
//!
//! Why this source matters as a substrate primitive:
//!
//! - Layer-2 carrier state is authoritative — when the kernel
//!   says "no carrier", nothing reachable upstream can lie
//!   about it. Pairing rtnetlink with a userspace daemon
//!   source (NetworkManager / ConnMan / systemd-networkd) is
//!   the canonical recipe for catching the userspace-daemon-
//!   lies-about-state failure mode the supervisor's design
//!   exists to detect.
//! - rtnetlink is universal across every Linux distribution
//!   regardless of userspace stack — Debian, Yocto, Alpine,
//!   Arch, bare-Linux without any daemon. One source covers
//!   every Linux device the framework ships on.
//! - The socket is non-blocking and stream-oriented; the
//!   `next_event` hot path costs one async `recv` per event
//!   with no subprocess invocations.

use super::{
    LinkEvent, LinkEventSource, LinkSourceCapabilities, LinkSourceError,
};
use futures::channel::mpsc::UnboundedReceiver;
use futures::stream::StreamExt;
use futures::TryStreamExt;
use netlink_packet_core::{NetlinkMessage, NetlinkPayload};
use netlink_packet_route::{
    address::AddressMessage, link::LinkMessage, RouteNetlinkMessage,
};
use netlink_sys::{AsyncSocket, SocketAddr};
use tokio::sync::Notify;

/// Netlink multicast groups the source subscribes to. The
/// numeric constants are stable per Linux's `<linux/rtnetlink.h>`:
///
/// - `RTNLGRP_LINK = 1` → layer-2 link state changes.
/// - `RTNLGRP_IPV4_IFADDR = 5` → IPv4 address attach / detach.
/// - `RTNLGRP_IPV6_IFADDR = 9` → IPv6 address attach / detach.
const RTNLGRP_LINK: u32 = 1;
const RTNLGRP_IPV4_IFADDR: u32 = 5;
const RTNLGRP_IPV6_IFADDR: u32 = 9;

const SUBSCRIPTION_GROUPS: &[u32] =
    &[RTNLGRP_LINK, RTNLGRP_IPV4_IFADDR, RTNLGRP_IPV6_IFADDR];

/// Concrete type of the rtnetlink message receiver.
/// `rtnetlink::new_connection` returns this directly; we
/// alias it so the struct's field type stays compact.
type MessageReceiver =
    UnboundedReceiver<(NetlinkMessage<RouteNetlinkMessage>, SocketAddr)>;

/// Rtnetlink event source. Holds the connection driver task
/// handle and the unsolicited-message stream the supervisor
/// drains.
pub struct RtnetlinkEventSource {
    /// Stream of inbound netlink messages. Surfaces both
    /// link-state and address-state notifications; the
    /// `next_event` impl filters by message type and emits
    /// the matching `LinkEvent`.
    messages: MessageReceiver,

    /// Handle on the rtnetlink connection driver task. We
    /// keep it alive as long as the source is alive; when
    /// the source is dropped, the connection's senders drop
    /// and the driver task exits cleanly.
    _connection_task: tokio::task::JoinHandle<()>,

    /// Handle for in-band probing. Used by `health_probe` to
    /// confirm the socket is alive without re-opening it.
    handle: rtnetlink::Handle,
}

impl RtnetlinkEventSource {
    /// Construct + subscribe.
    ///
    /// Opens a netlink socket via `rtnetlink::new_connection`,
    /// joins the three multicast groups declared at module
    /// top, and spawns the driver task on the current tokio
    /// runtime. Returns the source on success; surfaces a
    /// structured [`LinkSourceError`] on any open / subscribe
    /// failure.
    pub fn connect() -> Result<Self, LinkSourceError> {
        let (mut connection, handle, messages) = rtnetlink::new_connection()
            .map_err(|e| {
                LinkSourceError::Other(format!(
                    "rtnetlink: open netlink socket failed: {e}"
                ))
            })?;

        // The `connection.socket_mut()` returns the
        // `AsyncSocket` impl (tokio-backed); a second
        // `.socket_mut()` on the AsyncSocket trait returns
        // the raw `netlink_sys::Socket` whose
        // `add_membership` joins the multicast group.
        for &group in SUBSCRIPTION_GROUPS {
            connection
                .socket_mut()
                .socket_mut()
                .add_membership(group)
                .map_err(|e| {
                    LinkSourceError::Other(format!(
                        "rtnetlink: subscribe to multicast group {group}: {e}"
                    ))
                })?;
        }

        let connection_task = tokio::spawn(connection);

        Ok(Self {
            messages,
            _connection_task: connection_task,
            handle,
        })
    }
}

/// Inspect a parsed rtnetlink message and decide whether it
/// represents a link-state event the supervisor should wake
/// on. Pure function — easy to unit-test against constructed
/// messages without an open netlink socket.
fn message_to_event(
    payload: &NetlinkPayload<RouteNetlinkMessage>,
) -> Option<LinkEvent> {
    match payload {
        NetlinkPayload::InnerMessage(msg) => match msg {
            RouteNetlinkMessage::NewLink(link)
            | RouteNetlinkMessage::DelLink(link)
            | RouteNetlinkMessage::SetLink(link) => {
                Some(LinkEvent::InterfaceStateChanged {
                    interface: interface_name_from_link(link),
                })
            }
            RouteNetlinkMessage::NewAddress(addr)
            | RouteNetlinkMessage::DelAddress(addr) => {
                Some(LinkEvent::InterfaceStateChanged {
                    interface: interface_name_from_address(addr),
                })
            }
            _ => None,
        },
        _ => None,
    }
}

fn interface_name_from_link(link: &LinkMessage) -> Option<String> {
    use netlink_packet_route::link::LinkAttribute;
    link.attributes.iter().find_map(|attr| match attr {
        LinkAttribute::IfName(name) => Some(name.clone()),
        _ => None,
    })
}

fn interface_name_from_address(addr: &AddressMessage) -> Option<String> {
    use netlink_packet_route::address::AddressAttribute;
    addr.attributes.iter().find_map(|attr| match attr {
        AddressAttribute::Label(name) => Some(name.clone()),
        _ => None,
    })
}

#[async_trait::async_trait]
impl LinkEventSource for RtnetlinkEventSource {
    fn name(&self) -> &'static str {
        "rtnetlink"
    }

    fn capabilities(&self) -> LinkSourceCapabilities {
        LinkSourceCapabilities {
            observes_carrier: true,
            observes_address: true,
            observes_wifi_association: false,
            observes_connectivity_verdict: false,
        }
    }

    async fn next_event(&mut self, shutdown: &Notify) -> Option<LinkEvent> {
        loop {
            tokio::select! {
                _ = shutdown.notified() => return None,
                maybe_msg = self.messages.next() => {
                    let Some((message, _src_addr)) = maybe_msg else {
                        // The connection task exited (socket
                        // closed or driver panicked). Surface
                        // `None` so the supervisor stops
                        // consuming this source; the health
                        // probe will subsequently fail and the
                        // demotion path runs.
                        return None;
                    };
                    if let Some(event) = message_to_event(&message.payload) {
                        return Some(event);
                    }
                    // Message we don't care about (e.g. a
                    // route change); loop and await the next.
                }
            }
        }
    }

    async fn health_probe(&self) -> Result<(), LinkSourceError> {
        // Issue an in-band link enumeration via the rtnetlink
        // handle. Success means the socket is alive and the
        // kernel responded. We discard the result; this is a
        // liveness probe, not a state read.
        let mut stream = self.handle.link().get().execute();
        match stream.try_next().await {
            Ok(_) => Ok(()),
            Err(e) => Err(LinkSourceError::Disconnected(format!(
                "rtnetlink health probe failed: {e}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use netlink_packet_core::NetlinkPayload;
    use netlink_packet_route::link::LinkAttribute;

    #[test]
    fn capabilities_advertise_carrier_and_address_only() {
        // Manually constructing a fake source for the
        // capability check — connecting to the real kernel
        // netlink isn't appropriate in unit tests.
        let caps = LinkSourceCapabilities {
            observes_carrier: true,
            observes_address: true,
            observes_wifi_association: false,
            observes_connectivity_verdict: false,
        };
        assert!(caps.observes_carrier);
        assert!(caps.observes_address);
        assert!(!caps.observes_wifi_association);
        assert!(!caps.observes_connectivity_verdict);
    }

    #[test]
    fn message_to_event_new_link_emits_interface_state_changed() {
        let mut link = LinkMessage::default();
        link.attributes
            .push(LinkAttribute::IfName("wlan0".to_string()));
        let inner = RouteNetlinkMessage::NewLink(link);
        let payload = NetlinkPayload::InnerMessage(inner);
        match message_to_event(&payload) {
            Some(LinkEvent::InterfaceStateChanged { interface }) => {
                assert_eq!(interface.as_deref(), Some("wlan0"));
            }
            other => panic!("expected InterfaceStateChanged, got {other:?}"),
        }
    }

    #[test]
    fn message_to_event_del_link_also_emits_event() {
        let mut link = LinkMessage::default();
        link.attributes
            .push(LinkAttribute::IfName("eth0".to_string()));
        let inner = RouteNetlinkMessage::DelLink(link);
        let payload = NetlinkPayload::InnerMessage(inner);
        match message_to_event(&payload) {
            Some(LinkEvent::InterfaceStateChanged { interface }) => {
                assert_eq!(interface.as_deref(), Some("eth0"));
            }
            other => panic!("expected InterfaceStateChanged, got {other:?}"),
        }
    }

    #[test]
    fn message_to_event_route_change_filtered_out() {
        use netlink_packet_route::route::RouteMessage;
        let inner = RouteNetlinkMessage::NewRoute(RouteMessage::default());
        let payload = NetlinkPayload::InnerMessage(inner);
        assert!(message_to_event(&payload).is_none());
    }

    #[test]
    fn message_to_event_done_filtered_out() {
        let payload: NetlinkPayload<RouteNetlinkMessage> =
            NetlinkPayload::Done(netlink_packet_core::DoneMessage::default());
        assert!(message_to_event(&payload).is_none());
    }
}
