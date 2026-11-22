// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! # org-evoframework-system-info
//!
//! Stocks the `system.info` shelf with two reads.
//!
//! - `get_system_info { v: 1 }` answers `{ v: 1, hostname, os,
//!   kernel, architecture, model, cpu: { model, cores },
//!   memory_total_bytes, uptime_s }` from the host:
//!   - `hostname`: `/proc/sys/kernel/hostname`, trimmed;
//!   - `os`: `PRETTY_NAME` in `/etc/os-release`, quotes removed;
//!   - `kernel`: `/proc/sys/kernel/osrelease`, trimmed;
//!   - `architecture`: the machine name `uname(2)` reports;
//!   - `model`: `/proc/device-tree/model` when it has text,
//!     otherwise `sys_vendor` and `product_name` under
//!     `/sys/class/dmi/id/` joined by one space;
//!   - `cpu.model`: the first `model name` in `/proc/cpuinfo`;
//!   - `cpu.cores`: the number of `processor` lines there;
//!   - `memory_total_bytes`: `MemTotal` in `/proc/meminfo`, kB to
//!     bytes;
//!   - `uptime_s`: the whole seconds of the first field of
//!     `/proc/uptime`.
//! - `get_system_version { v: 1 }` answers `{ v: 1, release,
//!   build, architecture, built_at_utc }`. `build` is the
//!   workspace version this crate was compiled from. `release`,
//!   `architecture` and `built_at_utc` are the keys of those names
//!   in the installer's record, `/var/lib/evo/installed.toml`.
//!   `release` is read from no other key and no other source.
//!
//! A source that is missing or unreadable answers null for its
//! field, and the verb still answers. Neither verb writes
//! anything. The plugin publishes no subject.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::manual_async_fn)]

use std::future::Future;
use std::path::{Path, PathBuf};

use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities,
};
use evo_plugin_sdk::Manifest;
use serde::{Deserialize, Serialize};

/// Embedded manifest source.
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");

/// Plugin name (reverse-DNS); same as manifest and tests.
pub const PLUGIN_NAME: &str = "org.evoframework.system.info";

/// Read the host.
pub const VERB_INFO: &str = "get_system_info";

/// Read the installed release.
pub const VERB_VERSION: &str = "get_system_version";

/// The installer's record of the installed release, relative to
/// the host root.
pub const INSTALLED_RECORD: &str = "var/lib/evo/installed.toml";

/// The workspace version this crate was compiled from.
const BUILD: &str = env!("CARGO_PKG_VERSION");

/// Wire payload version.
const PAYLOAD_VERSION: u32 = 1;

/// Parse the embedded [`Manifest`].
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("org-evoframework-system-info: embedded manifest must parse")
}

fn plugin_crate_version() -> semver::Version {
    semver::Version::parse(BUILD).expect("CARGO_PKG_VERSION is valid semver")
}

/// What `get_system_info` answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SystemInfo {
    /// Host name.
    pub hostname: Option<String>,
    /// Operating system's pretty name.
    pub os: Option<String>,
    /// Kernel release.
    pub kernel: Option<String>,
    /// Machine name.
    pub architecture: Option<String>,
    /// Board or product model.
    pub model: Option<String>,
    /// Processor.
    pub cpu: Cpu,
    /// Total memory in bytes.
    pub memory_total_bytes: Option<u64>,
    /// Whole seconds since boot.
    pub uptime_s: Option<u64>,
}

/// The processor part of [`SystemInfo`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Cpu {
    /// The first `model name` in `/proc/cpuinfo`.
    pub model: Option<String>,
    /// The number of `processor` lines in `/proc/cpuinfo`.
    pub cores: Option<u32>,
}

/// What `get_system_version` answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SystemVersion {
    /// The installed release, from the installer's record.
    pub release: Option<String>,
    /// The workspace version this crate was compiled from.
    pub build: String,
    /// The bundle's architecture, from the installer's record.
    pub architecture: Option<String>,
    /// When the bundle was built, from the installer's record.
    pub built_at_utc: Option<String>,
}

/// Where the facts are read from: a root directory (`/` on a
/// device) and the machine name.
#[derive(Debug, Clone)]
pub struct Host {
    root: PathBuf,
    machine: fn() -> Option<String>,
}

impl Host {
    /// The device this process runs on.
    pub fn local() -> Self {
        Self {
            root: PathBuf::from("/"),
            machine: uname_machine,
        }
    }

    /// A host rooted elsewhere, with the machine name given.
    pub fn at(
        root: impl Into<PathBuf>,
        machine: fn() -> Option<String>,
    ) -> Self {
        Self {
            root: root.into(),
            machine,
        }
    }

    /// The file's text, or `None` when it is missing or unreadable.
    fn read(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(rel)).ok()
    }

    /// The trimmed text of a one-value file, or `None`.
    fn value(&self, rel: &str) -> Option<String> {
        self.read(rel).as_deref().and_then(text)
    }

    /// Read the host.
    pub fn info(&self) -> SystemInfo {
        let cpuinfo = self.read("proc/cpuinfo");
        SystemInfo {
            hostname: self.value("proc/sys/kernel/hostname"),
            os: self.read("etc/os-release").as_deref().and_then(pretty_name),
            kernel: self.value("proc/sys/kernel/osrelease"),
            architecture: (self.machine)().as_deref().and_then(text),
            model: self.model(),
            cpu: Cpu {
                model: cpuinfo.as_deref().and_then(cpu_model),
                cores: cpuinfo.as_deref().and_then(cpu_cores),
            },
            memory_total_bytes: self
                .read("proc/meminfo")
                .as_deref()
                .and_then(mem_total_bytes),
            uptime_s: self.read("proc/uptime").as_deref().and_then(uptime_s),
        }
    }

    /// The device-tree model when it has text; otherwise the DMI
    /// vendor and product joined by one space; otherwise `None`.
    fn model(&self) -> Option<String> {
        if let Some(model) = self.value("proc/device-tree/model") {
            return Some(model);
        }
        let parts: Vec<String> = [
            "sys/class/dmi/id/sys_vendor",
            "sys/class/dmi/id/product_name",
        ]
        .iter()
        .filter_map(|rel| self.value(rel))
        .collect();
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" "))
        }
    }

    /// Read the installed release.
    pub fn version(&self) -> SystemVersion {
        let record = self
            .read(INSTALLED_RECORD)
            .and_then(|body| body.parse::<toml::Table>().ok());
        let key = |name: &str| {
            record
                .as_ref()
                .and_then(|t| t.get(name))
                .and_then(toml::Value::as_str)
                .and_then(text)
        };
        SystemVersion {
            release: key("release"),
            build: BUILD.to_string(),
            architecture: key("architecture"),
            built_at_utc: key("built_at_utc"),
        }
    }

    /// The root the facts are read from.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// The machine name `uname(2)` reports.
fn uname_machine() -> Option<String> {
    let uts = nix::sys::utsname::uname().ok()?;
    uts.machine().to_str().map(str::to_string)
}

/// Text with surrounding whitespace and NULs removed, or `None`
/// when nothing is left.
fn text(raw: &str) -> Option<String> {
    let t = raw.trim_matches(|c: char| c.is_whitespace() || c == '\0');
    (!t.is_empty()).then(|| t.to_string())
}

/// `PRETTY_NAME` in an os-release file, its quotes removed.
fn pretty_name(os_release: &str) -> Option<String> {
    let value = os_release
        .lines()
        .find_map(|l| l.trim().strip_prefix("PRETTY_NAME="))?
        .trim();
    let unquoted = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
        .unwrap_or(value);
    text(unquoted)
}

/// The value of each `key : value` line in a procfs file whose key
/// is `key`.
fn values<'a>(body: &'a str, key: &'a str) -> impl Iterator<Item = &'a str> {
    body.lines().filter_map(move |l| {
        let (k, v) = l.split_once(':')?;
        (k.trim() == key).then_some(v)
    })
}

/// The first `model name` in `/proc/cpuinfo`.
fn cpu_model(cpuinfo: &str) -> Option<String> {
    values(cpuinfo, "model name").next().and_then(text)
}

/// The number of `processor` lines in `/proc/cpuinfo`.
fn cpu_cores(cpuinfo: &str) -> Option<u32> {
    let n = values(cpuinfo, "processor").count();
    u32::try_from(n).ok().filter(|n| *n > 0)
}

/// `MemTotal` in `/proc/meminfo`, from kB to bytes.
fn mem_total_bytes(meminfo: &str) -> Option<u64> {
    let mut fields = values(meminfo, "MemTotal").next()?.split_whitespace();
    let kb: u64 = fields.next()?.parse().ok()?;
    if fields.next() != Some("kB") {
        return None;
    }
    kb.checked_mul(1024)
}

/// The whole seconds of the first field of `/proc/uptime`.
fn uptime_s(uptime: &str) -> Option<u64> {
    let first = uptime.split_whitespace().next()?;
    let whole = first.split_once('.').map_or(first, |(w, _)| w);
    whole.parse().ok()
}

/// The plugin singleton.
pub struct SystemInfoPlugin {
    loaded: bool,
    host: Host,
}

impl SystemInfoPlugin {
    /// New instance reading the local host; call [`Plugin::load`]
    /// before handling requests.
    pub fn new() -> Self {
        Self::reading(Host::local())
    }

    /// New instance reading the given host.
    pub fn reading(host: Host) -> Self {
        Self {
            loaded: false,
            host,
        }
    }
}

impl Default for SystemInfoPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for SystemInfoPlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: PLUGIN_NAME.to_string(),
                    version: plugin_crate_version(),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: vec![
                        VERB_INFO.to_string(),
                        VERB_VERSION.to_string(),
                    ],
                    accepts_custody: false,
                    flags: Default::default(),
                    course_correct_verbs: Vec::new(),
                },
                build_info: BuildInfo {
                    plugin_build: BUILD.to_string(),
                    sdk_version: evo_plugin_sdk::VERSION.to_string(),
                    rustc_version: None,
                    built_at: None,
                },
            }
        }
    }

    fn load<'a>(
        &'a mut self,
        _ctx: &'a LoadContext,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
        async move {
            self.loaded = true;
            tracing::info!(
                plugin = PLUGIN_NAME,
                root = %self.host.root().display(),
                "system.info plugin loaded"
            );
            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            self.loaded = false;
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            if self.loaded {
                HealthReport::healthy()
            } else {
                HealthReport::unhealthy("system.info plugin not loaded")
            }
        }
    }
}

impl Respondent for SystemInfoPlugin {
    fn handle_request<'a>(
        &'a self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "system.info plugin not loaded".to_string(),
                ));
            }
            if req.is_past_deadline() {
                return Err(PluginError::Transient(
                    "request deadline already expired".to_string(),
                ));
            }
            match req.request_type.as_str() {
                VERB_INFO => {
                    check_version(req)?;
                    encode(
                        req,
                        &InfoResponse {
                            v: PAYLOAD_VERSION,
                            info: self.host.info(),
                        },
                    )
                }
                VERB_VERSION => {
                    check_version(req)?;
                    encode(
                        req,
                        &VersionResponse {
                            v: PAYLOAD_VERSION,
                            version: self.host.version(),
                        },
                    )
                }
                other => Err(PluginError::Permanent(format!(
                    "system.info: unknown verb {other:?}"
                ))),
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct EmptyPayload {
    #[serde(default = "default_payload_version")]
    v: u32,
}

fn default_payload_version() -> u32 {
    PAYLOAD_VERSION
}

fn check_version(req: &Request) -> Result<(), PluginError> {
    let parsed: EmptyPayload =
        serde_json::from_slice(&req.payload).map_err(|e| {
            PluginError::Permanent(format!(
                "{:?} payload is not valid JSON for the expected shape: {e}",
                req.request_type
            ))
        })?;
    if parsed.v != PAYLOAD_VERSION {
        return Err(PluginError::Permanent(format!(
            "{:?} payload version {} unsupported; expected {}",
            req.request_type, parsed.v, PAYLOAD_VERSION
        )));
    }
    Ok(())
}

fn encode<T: Serialize>(
    req: &Request,
    payload: &T,
) -> Result<Response, PluginError> {
    let body = serde_json::to_vec(payload).map_err(|e| {
        PluginError::Permanent(format!(
            "{:?} response encode failed: {e}",
            req.request_type
        ))
    })?;
    Ok(Response::for_request(req, body))
}

#[derive(Debug, Serialize)]
struct InfoResponse {
    v: u32,
    #[serde(flatten)]
    info: SystemInfo,
}

#[derive(Debug, Serialize)]
struct VersionResponse {
    v: u32,
    #[serde(flatten)]
    version: SystemVersion,
}

#[cfg(test)]
mod tests;
