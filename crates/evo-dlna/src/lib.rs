// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! UPnP / DLNA MediaServer client primitives.
//!
//! Protocol knowledge for SSDP discovery, device description
//! parse, and paged ContentDirectory `Browse` lives here — not
//! in evo-core and not inlined into `playback.mpd` sources.
//!
//! # Paging (mandatory)
//!
//! ContentDirectory calls always carry bounded `StartingIndex` +
//! `RequestedCount`. Defaults are `50` per request and the hard cap
//! is `100`; large MediaServer libraries would time out on
//! unbounded SOAP calls.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod browse;
mod device;
mod didl;
mod discovered;
mod ssdp;

pub use browse::{
    browse_page, BrowsePage, BrowseParams, DLNA_PAGE_DEFAULT,
    DLNA_PAGE_HARD_CAP,
};
pub use device::{fetch_media_server, MediaServer};
pub use didl::{
    parse_didl, pick_stream_uri, DidlContainer, DidlItem, DidlObject,
};
pub use discovered::{
    default_discovered_path, read_discovered, write_discovered, DiscoveredFile,
    DiscoveredServer, DISCOVERED_VERSION,
};
pub use ssdp::{discover_media_servers, spawn_notify_listener, SsdpHit};

/// Crate error surface.
#[derive(Debug, thiserror::Error)]
pub enum DlnaError {
    /// UDP / SSDP I/O failure.
    #[error("ssdp: {0}")]
    Ssdp(String),
    /// HTTP transport failure (device desc or SOAP).
    #[error("http: {0}")]
    Http(String),
    /// XML / DIDL parse failure.
    #[error("parse: {0}")]
    Parse(String),
    /// ContentDirectory SOAP fault or non-success.
    #[error("soap: {0}")]
    Soap(String),
    /// I/O for the discovered-servers sidecar.
    #[error("io: {0}")]
    Io(String),
}

impl From<std::io::Error> for DlnaError {
    fn from(e: std::io::Error) -> Self {
        DlnaError::Io(e.to_string())
    }
}

impl DlnaError {
    /// Classify whether this error represents a transient
    /// network-plane state — the interface is up but the
    /// route table or IGMP membership isn't ready yet, so
    /// send-to-multicast returns EHOSTUNREACH / ENETUNREACH.
    /// Common during the first ~5 s after boot when the
    /// steward starts before `network-online.target` is
    /// reached, and self-resolves as soon as the kernel's
    /// routing / IGMP state catches up.
    ///
    /// Callers use this classification to keep transient
    /// startup-race conditions out of the operator-facing
    /// WARN stream: transient errors log at INFO with an
    /// explicit "will retry on cadence" message; only
    /// persistent errors (or errors that survive the
    /// startup window) escalate to WARN.
    pub fn is_transient_network(&self) -> bool {
        match self {
            DlnaError::Ssdp(msg) | DlnaError::Http(msg) => {
                msg.contains("Network is unreachable")
                    || msg.contains("Network unreachable")
                    || msg.contains("Host is unreachable")
                    || msg.contains("os error 101") // EHOSTUNREACH / ENETUNREACH
                    || msg.contains("os error 113") // EHOSTUNREACH on some libcs
            }
            _ => false,
        }
    }
}

impl From<reqwest::Error> for DlnaError {
    fn from(e: reqwest::Error) -> Self {
        DlnaError::Http(e.to_string())
    }
}

impl From<url::ParseError> for DlnaError {
    fn from(e: url::ParseError) -> Self {
        DlnaError::Parse(e.to_string())
    }
}

#[cfg(test)]
mod error_classification_tests {
    use super::*;

    #[test]
    fn ssdp_network_unreachable_classifies_transient() {
        // The exact kernel error string SSDP sendto returns
        // when routing / IGMP membership isn't ready (typical
        // during the first few seconds after boot when the
        // steward starts before network-online.target).
        let e = DlnaError::Ssdp(
            "Network is unreachable (os error 101)".to_string(),
        );
        assert!(e.is_transient_network());
    }

    #[test]
    fn ssdp_host_unreachable_classifies_transient() {
        let e = DlnaError::Ssdp("Host is unreachable".to_string());
        assert!(e.is_transient_network());
    }

    #[test]
    fn ssdp_os_error_101_alone_classifies_transient() {
        let e = DlnaError::Ssdp("sendto: os error 101".to_string());
        assert!(e.is_transient_network());
    }

    #[test]
    fn http_network_unreachable_classifies_transient() {
        // Same transient class over HTTP (e.g. the follow-up
        // GET on a MediaServer's device description URL
        // firing before the LAN peer's route is set up).
        let e = DlnaError::Http("Network unreachable".to_string());
        assert!(e.is_transient_network());
    }

    #[test]
    fn parse_error_classifies_non_transient() {
        // Parse errors are structural — retrying without
        // caller action is pointless. Not a transient class.
        let e = DlnaError::Parse("unexpected xml element".to_string());
        assert!(!e.is_transient_network());
    }

    #[test]
    fn soap_fault_classifies_non_transient() {
        // SOAP-layer faults are peer-side application errors.
        // Not transient network-plane state.
        let e = DlnaError::Soap("401 Unauthorized".to_string());
        assert!(!e.is_transient_network());
    }

    #[test]
    fn io_error_classifies_non_transient() {
        // File-system I/O on the discovered-sidecar has no
        // network-plane semantics.
        let e = DlnaError::Io("no such file".to_string());
        assert!(!e.is_transient_network());
    }

    #[test]
    fn ssdp_dns_failure_classifies_non_transient() {
        // A DNS-lookup failure or SOAP-side error string that
        // does not mention "unreachable" is not the transient
        // startup-race class we want to suppress WARN for.
        let e = DlnaError::Ssdp("dns lookup failed for foo.local".to_string());
        assert!(!e.is_transient_network());
    }
}
