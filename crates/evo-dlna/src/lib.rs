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
pub use ssdp::{discover_media_servers, SsdpHit};

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
