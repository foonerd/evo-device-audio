// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! # org-evoframework-system-updater
//!
//! Stocks the `system.updater` shelf: the surface of the distribution's
//! in-app updater. The plugin fetches, verifies and installs
//! nothing. Every verb that starts work makes one call to the root request
//! wrapper (`sudo -n /usr/local/bin/evo-apply-update ...`), which parses a
//! closed grammar and starts a transient unit; the rest read what the
//! helper recorded in `/var/lib/evo-updater`, which is root-owned.
//!
//! Verbs, each `v: 1`:
//!
//! - `updater.check`: start a check of the recorded channel's registry.
//!   Answers `{ v, accepted, op }` or `{ v, accepted: false, error }`.
//! - `updater.apply { component, version }`: start an apply of one
//!   offered component (a plugin, or `evo-host-files`).
//! - `updater.channel.set { channel }`: `alpha`, `test` or `production`.
//! - `updater.status`: `{ v, channel, state, op?, component?, version?,
//!   started_at_utc?, finished_at_utc?, error?, summary?, inventory }`.
//!   `state` is `idle` before any run, `starting` from an accepted
//!   request until the helper records a newer one, then the helper's own
//!   (`checked`, `applying`, `applied`, `refused`, `rolled_back`,
//!   `channel_set`). `inventory` is the last check or null.
//! - `updater.history { limit? }`: `{ v, entries }` newest first,
//!   at most 50.
//! - `updater.channel.get`: `{ v, channel }`, `production` until set.
//!
//! The three that start work are writes at `system_admin`; the three
//! reads are gated at scope `system`. A missing or unreadable state file
//! answers an empty field and the verb still answers. The plugin writes
//! no file and publishes no subject.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::future::Future;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

use evo_apply_update::grammar::{valid_component, valid_version, Channel};
use evo_apply_update::timeutil;
use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities,
};
use evo_plugin_sdk::Manifest;
use serde::Deserialize;
use serde_json::{json, Value};

/// Embedded manifest source.
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");

/// Plugin name (reverse-DNS); same as manifest and tests.
pub const PLUGIN_NAME: &str = "org.evoframework.system.updater";

/// Start a check.
pub const VERB_CHECK: &str = "updater.check";
/// Read the updater's state.
pub const VERB_STATUS: &str = "updater.status";
/// Start an apply.
pub const VERB_APPLY: &str = "updater.apply";
/// Read the apply history.
pub const VERB_HISTORY: &str = "updater.history";
/// Read the channel.
pub const VERB_CHANNEL_GET: &str = "updater.channel.get";
/// Record a channel.
pub const VERB_CHANNEL_SET: &str = "updater.channel.set";

/// The helper's root-owned state directory.
pub const STATE_DIR: &str = "/var/lib/evo-updater";

/// The root request wrapper, the one sudo target.
pub const WRAPPER: &str = "/usr/local/bin/evo-apply-update";

/// The workspace-independent crate version, also the plugin version.
const BUILD: &str = env!("CARGO_PKG_VERSION");

/// Wire payload version.
const PAYLOAD_VERSION: u32 = 1;

/// Most history entries one call returns.
const HISTORY_MAX: usize = 50;

/// Default history entries.
const HISTORY_DEFAULT: usize = 20;

/// Parse the embedded [`Manifest`].
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("org-evoframework-system-updater: embedded manifest must parse")
}

fn plugin_crate_version() -> semver::Version {
    semver::Version::parse(BUILD).expect("CARGO_PKG_VERSION is valid semver")
}

/// Runs the helper's request wrapper. Faked in tests.
pub trait Runner: Send + Sync {
    /// Run the wrapper with `args` and return what it printed on stdout,
    /// or the reason it could not be run.
    fn run(&self, args: &[String]) -> Result<String, String>;
}

/// The real runner: `sudo -n -- /usr/local/bin/evo-apply-update <args>`.
pub struct SudoRunner;

impl Runner for SudoRunner {
    fn run(&self, args: &[String]) -> Result<String, String> {
        let out = Command::new("/usr/bin/sudo")
            .env_clear()
            .args(["-n", "--", WRAPPER])
            .args(args)
            .output()
            .map_err(|e| format!("sudo could not be run: {e}"))?;
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if stdout.is_empty() {
            let err = String::from_utf8_lossy(&out.stderr);
            let first = err.lines().next().unwrap_or("no output").trim();
            return Err(first.to_string());
        }
        Ok(stdout)
    }
}

/// What the plugin last asked the helper for, so a status that is older
/// than the request reads as `starting` rather than as the run before.
#[derive(Debug, Clone)]
struct LastRequest {
    at: u64,
    op: &'static str,
    component: Option<String>,
    version: Option<String>,
}

/// The updater surface.
pub struct UpdaterPlugin {
    loaded: bool,
    state_dir: PathBuf,
    runner: Arc<dyn Runner>,
    clock: fn() -> u64,
    last: Arc<Mutex<Option<LastRequest>>>,
}

impl UpdaterPlugin {
    /// New instance on the device: the real state directory and sudo.
    pub fn new() -> Self {
        Self::with(
            PathBuf::from(STATE_DIR),
            Arc::new(SudoRunner),
            timeutil::now_epoch,
        )
    }

    /// New instance over a state directory, a runner and a clock.
    pub fn with(
        state_dir: PathBuf,
        runner: Arc<dyn Runner>,
        clock: fn() -> u64,
    ) -> Self {
        Self {
            loaded: false,
            state_dir,
            runner,
            clock,
            last: Arc::new(Mutex::new(None)),
        }
    }

    fn read_json(&self, name: &str) -> Option<Value> {
        let text = std::fs::read_to_string(self.state_dir.join(name)).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn channel(&self) -> &'static str {
        std::fs::read_to_string(self.state_dir.join("channel"))
            .ok()
            .and_then(|s| Channel::parse(s.trim()))
            .unwrap_or(Channel::Production)
            .name()
    }

    fn status(&self) -> Value {
        let mut out = json!({
            "v": PAYLOAD_VERSION,
            "channel": self.channel(),
            "state": "idle",
            "inventory": self.read_json("inventory.json").unwrap_or(Value::Null),
        });
        let recorded = self.read_json("status.json");
        if let Some(Value::Object(m)) = &recorded {
            for key in [
                "state",
                "op",
                "component",
                "version",
                "started_at_utc",
                "finished_at_utc",
                "error",
                "summary",
            ] {
                if let Some(v) = m.get(key) {
                    out[key] = v.clone();
                }
            }
        }
        let last = self.last.lock().ok().and_then(|g| g.clone());
        if let Some(req) = last {
            let started = recorded
                .as_ref()
                .and_then(|r| r.get("started_at_utc"))
                .and_then(Value::as_str)
                .and_then(timeutil::parse_utc);
            if started.is_none_or(|s| s < req.at) {
                for key in
                    ["finished_at_utc", "error", "summary", "started_at_utc"]
                {
                    if let Some(m) = out.as_object_mut() {
                        m.remove(key);
                    }
                }
                out["state"] = json!("starting");
                out["op"] = json!(req.op);
                out["component"] = json!(req.component);
                out["version"] = json!(req.version);
            }
        }
        out
    }

    fn history(&self, limit: usize) -> Vec<Value> {
        let text =
            std::fs::read_to_string(self.state_dir.join("history.jsonl"))
                .unwrap_or_default();
        let mut entries: Vec<Value> = text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        entries.reverse();
        entries.truncate(limit);
        entries
    }

    /// Ask the helper to start work. The answer is the helper's own
    /// JSON, or a structured refusal when it could not be asked.
    async fn ask(
        &self,
        args: Vec<String>,
        op: &'static str,
        component: Option<String>,
        version: Option<String>,
    ) -> Value {
        let runner = Arc::clone(&self.runner);
        let result = tokio::task::spawn_blocking(move || runner.run(&args))
            .await
            .map_err(|e| format!("the request task failed: {e}"))
            .and_then(|r| r);
        let refusal = |detail: String| {
            json!({"v": PAYLOAD_VERSION, "accepted": false,
                   "error": {"code": "helper_unavailable", "detail": detail}})
        };
        let line = match result {
            Ok(s) => s,
            Err(e) => return refusal(e),
        };
        let parsed: Option<Value> = line
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .and_then(|l| serde_json::from_str(l).ok());
        match parsed {
            Some(v)
                if v.get("accepted").and_then(Value::as_bool) == Some(true) =>
            {
                if let Ok(mut g) = self.last.lock() {
                    *g = Some(LastRequest {
                        at: (self.clock)(),
                        op,
                        component,
                        version,
                    });
                }
                json!({"v": PAYLOAD_VERSION, "accepted": true, "op": op})
            }
            Some(v)
                if v.get("accepted").and_then(Value::as_bool)
                    == Some(false) =>
            {
                let mut e = v.get("error").cloned().unwrap_or(Value::Null);
                if !e.is_object() {
                    e = json!({"code": "helper_refused", "detail": "no reason given"});
                }
                json!({"v": PAYLOAD_VERSION, "accepted": false, "error": e})
            }
            _ => refusal("the helper's answer was not understood".to_string()),
        }
    }
}

impl Default for UpdaterPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for UpdaterPlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: PLUGIN_NAME.to_string(),
                    version: plugin_crate_version(),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: [
                        VERB_CHECK,
                        VERB_STATUS,
                        VERB_APPLY,
                        VERB_HISTORY,
                        VERB_CHANNEL_GET,
                        VERB_CHANNEL_SET,
                    ]
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect(),
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
                state_dir = %self.state_dir.display(),
                "system.updater plugin loaded"
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
                HealthReport::unhealthy("system.updater plugin not loaded")
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct Versioned {
    #[serde(default = "default_payload_version")]
    v: u32,
}

#[derive(Debug, Deserialize)]
struct ApplyPayload {
    #[serde(default = "default_payload_version")]
    v: u32,
    component: String,
    version: String,
}

#[derive(Debug, Deserialize)]
struct ChannelPayload {
    #[serde(default = "default_payload_version")]
    v: u32,
    channel: String,
}

#[derive(Debug, Deserialize)]
struct HistoryPayload {
    #[serde(default = "default_payload_version")]
    v: u32,
    #[serde(default)]
    limit: Option<u32>,
}

fn default_payload_version() -> u32 {
    PAYLOAD_VERSION
}

fn parse<'a, T: Deserialize<'a>>(req: &'a Request) -> Result<T, PluginError> {
    serde_json::from_slice(&req.payload).map_err(|e| {
        PluginError::Permanent(format!(
            "{:?} payload is not valid JSON for the expected shape: {e}",
            req.request_type
        ))
    })
}

fn check_v(req: &Request, v: u32) -> Result<(), PluginError> {
    if v == PAYLOAD_VERSION {
        Ok(())
    } else {
        Err(PluginError::Permanent(format!(
            "{:?} payload version {v} unsupported; expected {PAYLOAD_VERSION}",
            req.request_type
        )))
    }
}

fn soft_refusal(code: &str, detail: &str) -> Value {
    json!({"v": PAYLOAD_VERSION, "accepted": false,
           "error": {"code": code, "detail": detail}})
}

fn encode(req: &Request, payload: &Value) -> Result<Response, PluginError> {
    let body = serde_json::to_vec(payload).map_err(|e| {
        PluginError::Permanent(format!(
            "{:?} response encode failed: {e}",
            req.request_type
        ))
    })?;
    Ok(Response::for_request(req, body))
}

impl Respondent for UpdaterPlugin {
    fn handle_request<'a>(
        &'a self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "system.updater plugin not loaded".to_string(),
                ));
            }
            if req.is_past_deadline() {
                return Err(PluginError::Transient(
                    "request deadline already expired".to_string(),
                ));
            }
            let answer = match req.request_type.as_str() {
                VERB_CHECK => {
                    check_v(req, parse::<Versioned>(req)?.v)?;
                    self.ask(vec!["check".into()], "check", None, None).await
                }
                VERB_APPLY => {
                    let p: ApplyPayload = parse(req)?;
                    check_v(req, p.v)?;
                    if !valid_component(&p.component) {
                        soft_refusal("bad_component", "component not accepted")
                    } else if !valid_version(&p.version) {
                        soft_refusal("bad_version", "version not accepted")
                    } else {
                        self.ask(
                            vec![
                                "apply".into(),
                                p.component.clone(),
                                p.version.clone(),
                            ],
                            "apply",
                            Some(p.component),
                            Some(p.version),
                        )
                        .await
                    }
                }
                VERB_CHANNEL_SET => {
                    let p: ChannelPayload = parse(req)?;
                    check_v(req, p.v)?;
                    match Channel::parse(&p.channel) {
                        Some(c) => {
                            self.ask(
                                vec!["channel".into(), c.name().into()],
                                "channel",
                                None,
                                None,
                            )
                            .await
                        }
                        None => {
                            soft_refusal("bad_channel", "channel not accepted")
                        }
                    }
                }
                VERB_STATUS => {
                    check_v(req, parse::<Versioned>(req)?.v)?;
                    self.status()
                }
                VERB_HISTORY => {
                    let p: HistoryPayload = parse(req)?;
                    check_v(req, p.v)?;
                    let n = p
                        .limit
                        .map_or(HISTORY_DEFAULT, |l| l as usize)
                        .clamp(1, HISTORY_MAX);
                    json!({"v": PAYLOAD_VERSION, "entries": self.history(n)})
                }
                VERB_CHANNEL_GET => {
                    check_v(req, parse::<Versioned>(req)?.v)?;
                    json!({"v": PAYLOAD_VERSION, "channel": self.channel()})
                }
                other => {
                    return Err(PluginError::Permanent(format!(
                        "system.updater: unknown verb {other:?}"
                    )))
                }
            };
            encode(req, &answer)
        }
    }
}

#[cfg(test)]
mod tests;
