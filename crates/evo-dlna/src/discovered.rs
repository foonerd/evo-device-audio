// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Sidecar file bridging source.dlna discovery → playback.mpd SourceRegistry.
//!
//! OOP ShelfRequestDispatcher is not yet wired; this file is the
//! interim sync surface. Path default:
//! `/var/lib/evo/state/org.evoframework.source.dlna/discovered.json`
//! overridable via `EVO_DLNA_DISCOVERED_PATH`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::DlnaError;

/// Wire version for the discovered-servers file.
pub const DISCOVERED_VERSION: u32 = 1;

/// Default absolute path for the discovered-servers sidecar.
pub fn default_discovered_path() -> PathBuf {
    std::env::var_os("EVO_DLNA_DISCOVERED_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(
                "/var/lib/evo/state/org.evoframework.source.dlna/discovered.json",
            )
        })
}

/// On-disk envelope.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscoveredFile {
    /// Format version.
    pub v: u32,
    /// Servers last seen by SSDP + device parse.
    pub servers: Vec<DiscoveredServer>,
}

/// One discovered MediaServer row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscoveredServer {
    /// UPnP UDN (uuid without prefix).
    pub service_id: String,
    /// Friendly name.
    pub friendly_name: String,
    /// ContentDirectory control URL.
    pub control_url: String,
    /// Base URL.
    pub base_url: String,
    /// Device description LOCATION.
    pub location: String,
    /// Epoch ms of last successful discovery sighting.
    pub last_seen_ms: u64,
}

/// Read the sidecar; missing file → empty list.
pub fn read_discovered(path: &Path) -> Result<DiscoveredFile, DlnaError> {
    if !path.exists() {
        return Ok(DiscoveredFile {
            v: DISCOVERED_VERSION,
            servers: Vec::new(),
        });
    }
    let bytes = std::fs::read(path)?;
    let file: DiscoveredFile = serde_json::from_slice(&bytes)
        .map_err(|e| DlnaError::Parse(e.to_string()))?;
    Ok(file)
}

/// Atomic write (tmp + rename).
pub fn write_discovered(
    path: &Path,
    file: &DiscoveredFile,
) -> Result<(), DlnaError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(file)
        .map_err(|e| DlnaError::Parse(e.to_string()))?;
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("discovered.json");
        let file = DiscoveredFile {
            v: 1,
            servers: vec![DiscoveredServer {
                service_id: "abc".into(),
                friendly_name: "Jellyfin".into(),
                control_url: "http://x/c".into(),
                base_url: "http://x".into(),
                location: "http://x/d.xml".into(),
                last_seen_ms: 1,
            }],
        };
        write_discovered(&path, &file).unwrap();
        let got = read_discovered(&path).unwrap();
        assert_eq!(got, file);
    }
}
