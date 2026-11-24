// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! # org-evoframework-receiver-dlna
//!
//! The device as a UPnP/DLNA MediaRenderer. The plugin is the renderer:
//! it advertises over SSDP, serves the device and service descriptions,
//! and owns the control and event endpoints. There is no external
//! renderer daemon, and nothing here speaks to the player except through
//! the `playback.mpd` plugin's verbs (the bridge lands in a later slice).
//!
//! Default **disabled**: no socket is open until an operator enables it.
//! Enabling and disabling are write verbs under `network_admin`; reading
//! the state needs no scope. The state is published on the
//! `audio_receiver_dlna` subject.
//!
//! Orthogonal to the `source.dlna` client: that plugin browses and plays
//! from MediaServers, this one is a MediaRenderer, and the two share no
//! state. The renderer exposes no ContentDirectory, so the client's
//! MediaServer filter ignores it.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::manual_async_fn)]

use std::future::Future;
use std::sync::Arc;

use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities,
};
use evo_plugin_sdk::Manifest;

pub mod description;
pub mod http;
pub mod lan;
pub mod runtime;
pub mod ssdp;
pub mod state;

use runtime::{is_verb, RendererRuntime, RuntimeConfig, VerbError, VERBS};

/// Embedded manifest source.
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");

/// Plugin name (reverse-DNS); same as the manifest.
pub const PLUGIN_NAME: &str = "org.evoframework.receiver.dlna";

/// Parse the embedded [`Manifest`].
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("org-evoframework-receiver-dlna: embedded manifest must parse")
}

fn plugin_crate_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is valid semver")
}

/// The plugin singleton.
pub struct ReceiverDlnaPlugin {
    runtime: Option<Arc<RendererRuntime>>,
}

impl ReceiverDlnaPlugin {
    /// New instance; call [`Plugin::load`] before handling requests.
    pub fn new() -> Self {
        Self { runtime: None }
    }
}

impl Default for ReceiverDlnaPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for ReceiverDlnaPlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: PLUGIN_NAME.to_string(),
                    version: plugin_crate_version(),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: VERBS
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                    accepts_custody: false,
                    flags: Default::default(),
                    course_correct_verbs: Vec::new(),
                },
                build_info: BuildInfo {
                    plugin_build: env!("CARGO_PKG_VERSION").to_string(),
                    sdk_version: evo_plugin_sdk::VERSION.to_string(),
                    rustc_version: None,
                    built_at: None,
                },
            }
        }
    }

    fn load<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
        async move {
            std::fs::create_dir_all(&ctx.state_dir).map_err(|e| {
                PluginError::Permanent(format!(
                    "create state_dir {}: {e}",
                    ctx.state_dir.display()
                ))
            })?;
            let rt = Arc::new(
                RendererRuntime::open(
                    &ctx.state_dir,
                    RuntimeConfig::production(),
                )
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "receiver.dlna runtime open failed: {e}"
                    ))
                })?,
            );
            if let Err(e) = rt
                .attach_subject_publisher(Arc::clone(&ctx.subject_announcer))
                .await
            {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "receiver.dlna subject publisher attach failed; the \
                     state is still served by get_state"
                );
            }
            // A persisted "on" brings the listeners back. A bind failure
            // is reported as a degraded state, not a failed load.
            rt.reconcile_on_load().await;
            self.runtime = Some(rt);
            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            if let Some(rt) = self.runtime.take() {
                rt.shutdown().await;
            }
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            if self.runtime.is_some() {
                HealthReport::healthy()
            } else {
                HealthReport::unhealthy("receiver.dlna plugin not loaded")
            }
        }
    }
}

impl Respondent for ReceiverDlnaPlugin {
    fn handle_request<'a>(
        &'a self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            let Some(rt) = self.runtime.as_ref() else {
                return Err(PluginError::Permanent(
                    "receiver.dlna plugin not loaded".to_string(),
                ));
            };
            if req.is_past_deadline() {
                return Err(PluginError::Transient(
                    "request deadline already expired".to_string(),
                ));
            }
            let verb = req.request_type.as_str();
            if !is_verb(verb) {
                return Err(PluginError::Permanent(format!(
                    "receiver.dlna: unknown verb {verb:?}"
                )));
            }
            match rt.dispatch_verb(verb).await {
                Ok(bytes) => Ok(Response::for_request(req, bytes)),
                Err(VerbError::Unknown(v)) => Err(PluginError::Permanent(
                    format!("receiver.dlna: unknown verb {v:?}"),
                )),
                Err(e) => Err(PluginError::Transient(e.to_string())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_plugin_sdk::manifest::VerbCapability;

    #[test]
    fn both_manifests_declare_the_same_verbs_and_gate_writes_at_write_rank() {
        let oop = Manifest::from_toml(include_str!("../manifest.oop.toml"))
            .expect("manifest.oop.toml parses");
        for m in [manifest(), oop] {
            let resp = m.capabilities.respondent.expect("respondent");
            for verb in VERBS {
                assert!(
                    resp.request_types.iter().any(|v| v == verb),
                    "{verb} is declared"
                );
            }
            assert!(matches!(
                resp.verb_capabilities.get("receiver.dlna.get_state"),
                Some(VerbCapability::None)
            ));
            for verb in ["receiver.dlna.enable", "receiver.dlna.disable"] {
                assert!(
                    matches!(
                        resp.verb_capabilities.get(verb),
                        Some(VerbCapability::Write { scope }) if scope == "network_admin"
                    ),
                    "{verb}: {:?}",
                    resp.verb_capabilities.get(verb)
                );
            }
        }
    }

    #[test]
    fn the_renderer_is_shipped_disabled_and_declares_no_outbound_network() {
        let m = manifest();
        assert!(!m.prerequisites.outbound_network);
    }
}
