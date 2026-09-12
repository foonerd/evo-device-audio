// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! SSDP discovery for UPnP MediaServer devices.
//!
//! Two entry points:
//!
//! - [`discover_media_servers`] — active M-SEARCH pass. Sends two
//!   M-SEARCH frames back-to-back (`ST: ssdp:all` +
//!   `ST: upnp:rootdevice`) and collects every unicast response's
//!   LOCATION. Broader than a MediaServer-typed M-SEARCH: real
//!   MediaServers on real LANs frequently only reply to
//!   `ssdp:all` or only announce, so a MediaServer-typed
//!   M-SEARCH systematically misses a substantial fraction of
//!   the servers on the wire. Downstream filtering (MediaServer vs
//!   MediaRenderer vs Basic:1 vs Cast target etc.) happens via
//!   [`crate::device::fetch_media_server`], which GETs each
//!   LOCATION's description XML and refuses devices that lack a
//!   `ContentDirectory:1` service.
//!
//! - [`spawn_notify_listener`] — passive NOTIFY listener. Binds
//!   `239.255.255.250:1900`, joins the multicast group, and
//!   forwards every incoming SSDP frame's LOCATION-carrying hit
//!   through an mpsc channel. Catches servers that announce
//!   between M-SEARCH sweeps and servers that don't respond to
//!   M-SEARCH but do NOTIFY. Best-effort — bind failure (port
//!   in use by a co-hosted SSDP responder) returns an error and
//!   the caller degrades to M-SEARCH-only discovery.
//!
//! The response-parser [`parse_ssdp_response`] is agnostic to
//! frame kind — both `HTTP/1.1 200 OK` M-SEARCH responses and
//! `NOTIFY * HTTP/1.1` announces carry the same
//! `LOCATION` / `USN` / `SERVER` headers, so one parser handles
//! both.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket as StdUdpSocket};
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::DlnaError;

const SSDP_MCAST_ADDR: &str = "239.255.255.250:1900";
const SSDP_MCAST_IP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);

/// Broad SSDP search target — every SSDP responder answers this.
const ST_SSDP_ALL: &str = "ssdp:all";

/// Root-device search target — some SSDP responders (older
/// implementations, some appliance NAS units) only answer this
/// one. Sending both `ssdp:all` and `upnp:rootdevice` in a
/// back-to-back pair is the standard control-point pattern and
/// what upmpdcli / gupnp-based tooling use as the belt-and-
/// braces default.
const ST_ROOT_DEVICE: &str = "upnp:rootdevice";

/// One SSDP response locating a device description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsdpHit {
    /// Absolute URL of the device description XML.
    pub location: String,
    /// `USN` header when present.
    pub usn: Option<String>,
    /// `SERVER` header when present.
    pub server: Option<String>,
}

/// Run an SSDP M-SEARCH and collect unique `LOCATION` hits.
///
/// Sends two M-SEARCH frames back-to-back (`ssdp:all` +
/// `upnp:rootdevice`) then listens for unicast replies for
/// `window`. Deduplicates by LOCATION URL. Downstream
/// [`crate::device::fetch_media_server`] classifies each hit's
/// device-description XML and drops non-MediaServer devices.
pub async fn discover_media_servers(
    window: Duration,
) -> Result<Vec<SsdpHit>, DlnaError> {
    let sock = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| DlnaError::Ssdp(e.to_string()))?;
    sock.set_broadcast(true)
        .map_err(|e| DlnaError::Ssdp(e.to_string()))?;

    let dest: SocketAddr = SSDP_MCAST_ADDR
        .parse()
        .map_err(|e| DlnaError::Ssdp(format!("bad SSDP addr: {e}")))?;

    for st in [ST_SSDP_ALL, ST_ROOT_DEVICE] {
        let msg = format!(
            "M-SEARCH * HTTP/1.1\r\n\
             HOST: 239.255.255.250:1900\r\n\
             MAN: \"ssdp:discover\"\r\n\
             MX: 2\r\n\
             ST: {st}\r\n\
             \r\n"
        );
        sock.send_to(msg.as_bytes(), dest)
            .await
            .map_err(|e| DlnaError::Ssdp(e.to_string()))?;
    }

    let deadline = Instant::now() + window;
    let mut by_location: HashMap<String, SsdpHit> = HashMap::new();
    let mut buf = [0u8; 4096];

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match timeout(remaining, sock.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) => {
                if let Some(hit) = parse_ssdp_response(&buf[..n]) {
                    by_location.entry(hit.location.clone()).or_insert(hit);
                }
            }
            Ok(Err(e)) => return Err(DlnaError::Ssdp(e.to_string())),
            Err(_) => break, // timeout → window elapsed
        }
    }

    let mut out: Vec<SsdpHit> = by_location.into_values().collect();
    out.sort_by(|a, b| a.location.cmp(&b.location));
    Ok(out)
}

/// Spawn a passive NOTIFY listener bound to
/// `239.255.255.250:1900`. Returns an mpsc receiver that yields
/// one [`SsdpHit`] per LOCATION-carrying inbound SSDP frame
/// (both `NOTIFY * HTTP/1.1` announces and any other SSDP
/// traffic multicast on the group). The spawned task runs until
/// the receiver is dropped.
///
/// Bind + multicast join happen synchronously in this call so
/// the caller can surface a WARN and fall back to M-SEARCH-only
/// discovery if the port is unavailable (e.g. co-hosted
/// SSDP responder like DSM already bound `:1900` on the same
/// host — rare on evo devices, common on NAS appliances).
///
/// Downstream classification (MediaServer vs MediaRenderer vs
/// Basic:1 vs Cast target etc.) happens via
/// [`crate::device::fetch_media_server`], mirroring the
/// [`discover_media_servers`] pipeline. This function only
/// widens the hit funnel; it does not itself decide what
/// counts as a MediaServer.
pub fn spawn_notify_listener(
) -> Result<mpsc::UnboundedReceiver<SsdpHit>, DlnaError> {
    // Bind + multicast join via std::net::UdpSocket so we can
    // set nonblocking BEFORE handing the fd to tokio. Tokio's
    // UdpSocket::from_std requires a nonblocking socket.
    let std_sock =
        StdUdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 1900))
            .map_err(|e| {
                DlnaError::Ssdp(format!("bind :1900 for NOTIFY listener: {e}"))
            })?;
    std_sock
        .join_multicast_v4(&SSDP_MCAST_IP, &Ipv4Addr::UNSPECIFIED)
        .map_err(|e| DlnaError::Ssdp(format!("join_multicast_v4: {e}")))?;
    std_sock
        .set_nonblocking(true)
        .map_err(|e| DlnaError::Ssdp(format!("set_nonblocking: {e}")))?;
    let sock = UdpSocket::from_std(std_sock)
        .map_err(|e| DlnaError::Ssdp(format!("tokio from_std: {e}")))?;

    let (tx, rx) = mpsc::unbounded_channel::<SsdpHit>();
    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        loop {
            match sock.recv_from(&mut buf).await {
                Ok((n, _)) => {
                    if let Some(hit) = parse_ssdp_response(&buf[..n]) {
                        // Channel closed = receiver dropped =
                        // caller no longer wants us. Exit.
                        if tx.send(hit).is_err() {
                            return;
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!(
                        error = %e,
                        "SSDP NOTIFY listener recv_from errored; retrying"
                    );
                }
            }
        }
    });
    Ok(rx)
}

fn parse_ssdp_response(bytes: &[u8]) -> Option<SsdpHit> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut location = None;
    let mut usn = None;
    let mut server = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line
            .strip_prefix("LOCATION:")
            .or_else(|| line.strip_prefix("Location:"))
            .or_else(|| line.strip_prefix("location:"))
        {
            location = Some(rest.trim().to_string());
        } else if let Some(rest) = line
            .strip_prefix("USN:")
            .or_else(|| line.strip_prefix("Usn:"))
        {
            usn = Some(rest.trim().to_string());
        } else if let Some(rest) = line
            .strip_prefix("SERVER:")
            .or_else(|| line.strip_prefix("Server:"))
        {
            server = Some(rest.trim().to_string());
        }
    }
    location.map(|location| SsdpHit {
        location,
        usn,
        server,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ssdp_extracts_location() {
        let raw = b"HTTP/1.1 200 OK\r\n\
LOCATION: http://192.0.2.10:8096/dlna/123/description.xml\r\n\
USN: uuid:abc::urn:schemas-upnp-org:device:MediaServer:1\r\n\
SERVER: Jellyfin\r\n\
\r\n";
        let hit = parse_ssdp_response(raw).expect("hit");
        assert_eq!(
            hit.location,
            "http://192.0.2.10:8096/dlna/123/description.xml"
        );
        assert!(hit.usn.unwrap().contains("uuid:abc"));
        assert_eq!(hit.server.as_deref(), Some("Jellyfin"));
    }

    /// `NOTIFY` frames from passive multicast listen carry the
    /// same `LOCATION` / `USN` / `SERVER` headers as M-SEARCH
    /// responses. One parser handles both — the NOTIFY listener
    /// feeds through this same code path.
    #[test]
    fn parse_ssdp_extracts_location_from_notify_frame() {
        let raw = b"NOTIFY * HTTP/1.1\r\n\
HOST: 239.255.255.250:1900\r\n\
CACHE-CONTROL: max-age=1800\r\n\
LOCATION: http://192.0.2.42:8200/rootDesc.xml\r\n\
NT: urn:schemas-upnp-org:device:MediaServer:1\r\n\
NTS: ssdp:alive\r\n\
SERVER: Linux/5.15 UPnP/1.0 MiniDLNA/1.3.0\r\n\
USN: uuid:def::urn:schemas-upnp-org:device:MediaServer:1\r\n\
\r\n";
        let hit = parse_ssdp_response(raw).expect("hit");
        assert_eq!(hit.location, "http://192.0.2.42:8200/rootDesc.xml");
        assert!(hit.usn.unwrap().contains("uuid:def"));
        assert_eq!(
            hit.server.as_deref(),
            Some("Linux/5.15 UPnP/1.0 MiniDLNA/1.3.0")
        );
    }

    /// A frame carrying no `LOCATION` header (some `M-SEARCH`
    /// requests other clients send are also multicast on 1900
    /// and reach the passive listener) parses to `None` and is
    /// dropped by the caller.
    #[test]
    fn parse_ssdp_returns_none_when_no_location() {
        let raw = b"M-SEARCH * HTTP/1.1\r\n\
HOST: 239.255.255.250:1900\r\n\
MAN: \"ssdp:discover\"\r\n\
MX: 2\r\n\
ST: urn:schemas-upnp-org:device:MediaServer:1\r\n\
\r\n";
        assert!(parse_ssdp_response(raw).is_none());
    }
}
