// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! SSDP M-SEARCH for MediaServer:1.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::time::timeout;

use crate::DlnaError;

const SSDP_ADDR: &str = "239.255.255.250:1900";
const MEDIA_SERVER_ST: &str = "urn:schemas-upnp-org:device:MediaServer:1";

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
pub async fn discover_media_servers(
    window: Duration,
) -> Result<Vec<SsdpHit>, DlnaError> {
    let sock = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| DlnaError::Ssdp(e.to_string()))?;
    sock.set_broadcast(true)
        .map_err(|e| DlnaError::Ssdp(e.to_string()))?;

    let msg = format!(
        "M-SEARCH * HTTP/1.1\r\n\
         HOST: 239.255.255.250:1900\r\n\
         MAN: \"ssdp:discover\"\r\n\
         MX: 2\r\n\
         ST: {MEDIA_SERVER_ST}\r\n\
         \r\n"
    );
    let dest: SocketAddr = SSDP_ADDR
        .parse()
        .map_err(|e| DlnaError::Ssdp(format!("bad SSDP addr: {e}")))?;
    sock.send_to(msg.as_bytes(), dest)
        .await
        .map_err(|e| DlnaError::Ssdp(e.to_string()))?;

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
}
