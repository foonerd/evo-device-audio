// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! The worker's dependencies, gathered once so every operation takes one
//! argument and a test can fake the network and the host.

use std::collections::BTreeMap;
use std::fs;

use crate::docs::{Allow, Config};
use crate::error::{Res, UpdError};
use crate::fetch::Fetch;
use crate::host::Host;
use crate::roots::Roots;
use crate::state::State;
use crate::trust::TrustStore;

/// Everything an operation needs.
pub struct Ctx<'a> {
    /// Paths and the identity they must have.
    pub roots: &'a Roots,
    /// The state directory.
    pub state: &'a State,
    /// `updater.toml`.
    pub cfg: &'a Config,
    /// Trust roots.
    pub trust: &'a TrustStore,
    /// `allow.toml`.
    pub allow: &'a Allow,
    /// Network.
    pub fetch: &'a dyn Fetch,
    /// Host operations.
    pub host: &'a dyn Host,
    /// Now, epoch seconds.
    pub now: u64,
    /// The host's target triple.
    pub triple: &'a str,
    /// The host's short architecture name for the installed record.
    pub arch: &'a str,
}

/// The target triple the registry is keyed by.
pub fn host_triple() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "aarch64-unknown-linux-gnu",
        "x86_64" => "x86_64-unknown-linux-gnu",
        _ => "armv7-unknown-linux-gnueabihf",
    }
}

/// The architecture name the installed record carries.
pub fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        _ => "armv7",
    }
}

/// Installed plugins: directory name to the version in its manifest.
pub fn installed_plugins(roots: &Roots) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Ok(rd) = fs::read_dir(&roots.plugins_dir) else {
        return out;
    };
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if let Some(v) =
            manifest_version(&ent.path().join("manifest.toml"), &name)
        {
            out.insert(name, v);
        }
    }
    out
}

/// `[plugin] version` of a manifest whose `[plugin] name` is `name`.
pub fn manifest_version(path: &std::path::Path, name: &str) -> Option<String> {
    let t: toml::Table = fs::read_to_string(path).ok()?.parse().ok()?;
    let p = t.get("plugin")?.as_table()?;
    (p.get("name")?.as_str()? == name).then(|| {
        p.get("version")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    })?
}

/// The installed record's `release`, if there is a record with one.
pub fn installed_release(roots: &Roots) -> Option<String> {
    let t: toml::Table = fs::read_to_string(&roots.installed_record)
        .ok()?
        .parse()
        .ok()?;
    t.get("release")?.as_str().map(str::to_string)
}

/// Join a base URL and a plane-relative path.
pub fn plane_url(cfg: &Config, rel: &str) -> String {
    format!("{}/{}", cfg.base_url, rel)
}

/// A URL from a signed document must sit under the configured base.
pub fn under_base(cfg: &Config, url: &str) -> Res<()> {
    if url.starts_with(&format!("{}/", cfg.base_url)) {
        Ok(())
    } else {
        Err(UpdError::new(
            "url_outside_base",
            "document URL is not under the configured base",
        ))
    }
}
