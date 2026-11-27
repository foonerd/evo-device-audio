// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! # org-evoframework-receiver-airplay
//!
//! The device as an AirPlay 2 audio receiver. The plugin owns the receiver,
//! built on the `evo-airplay` library: it binds the control port, registers
//! the device with the local mDNS daemon, answers local-network peers only,
//! admits one stream at a time and holds the pairing password. There is no
//! external AirPlay daemon, and nothing here speaks to the player except
//! through the `playback.mpd` plugin's verbs.
//!
//! Default **disabled**: no socket is open and nothing is registered until
//! an operator enables it. Enabling and disabling are write verbs under
//! `network_admin`; reading the state needs no scope. The state is published
//! on the `audio_receiver_airplay` subject. The password is never part of the
//! state, the subject, a log line or an error.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::future::Future;
use std::sync::Arc;

use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities,
};
use evo_plugin_sdk::Manifest;

pub mod lan;
pub mod mdns;
pub mod runtime;
pub mod state;

use mdns::AvahiAdvertiser;
use runtime::{is_verb, ReceiverRuntime, RuntimeConfig, VerbError, VERBS};

/// Embedded manifest source.
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");

/// Plugin name (reverse-DNS); same as the manifest.
pub const PLUGIN_NAME: &str = "org.evoframework.receiver.airplay";

/// Parse the embedded [`Manifest`].
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML).unwrap_or_else(|e| {
        panic!("org-evoframework-receiver-airplay: embedded manifest: {e}")
    })
}

fn plugin_crate_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .unwrap_or_else(|_| semver::Version::new(0, 0, 0))
}

/// The plugin singleton.
pub struct ReceiverAirplayPlugin {
    runtime: Option<Arc<ReceiverRuntime>>,
}

impl ReceiverAirplayPlugin {
    /// New instance; call [`Plugin::load`] before handling requests.
    pub fn new() -> Self {
        Self { runtime: None }
    }
}

impl Default for ReceiverAirplayPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for ReceiverAirplayPlugin {
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
            for dir in [&ctx.state_dir, &ctx.credentials_dir] {
                std::fs::create_dir_all(dir).map_err(|e| {
                    PluginError::Permanent(format!(
                        "create {}: {e}",
                        dir.display()
                    ))
                })?;
            }
            let rt = Arc::new(ReceiverRuntime::open(
                &ctx.state_dir,
                &ctx.credentials_dir,
                RuntimeConfig::production(),
                Arc::new(AvahiAdvertiser),
            ));
            if let Err(e) = rt
                .attach_subject_publisher(Arc::clone(&ctx.subject_announcer))
                .await
            {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "receiver.airplay subject publisher attach failed; the \
                     state is still served by get_state"
                );
            }
            // A persisted "on" brings the receiver back. A failure is shown
            // as a degraded state, not a failed load.
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
                HealthReport::unhealthy("receiver.airplay plugin not loaded")
            }
        }
    }
}

impl Respondent for ReceiverAirplayPlugin {
    fn handle_request<'a>(
        &'a self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            let Some(rt) = self.runtime.as_ref() else {
                return Err(PluginError::Permanent(
                    "receiver.airplay plugin not loaded".to_string(),
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
                    "receiver.airplay: unknown verb {verb:?}"
                )));
            }
            match rt.dispatch_verb(verb, &req.payload).await {
                Ok(bytes) => Ok(Response::for_request(req, bytes)),
                Err(VerbError::Unknown(v)) => Err(PluginError::Permanent(
                    format!("receiver.airplay: unknown verb {v:?}"),
                )),
                Err(VerbError::Invalid(why)) => Err(PluginError::Permanent(
                    format!("receiver.airplay: {why}"),
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
                resp.verb_capabilities.get("receiver.airplay.get_state"),
                Some(VerbCapability::None)
            ));
            for verb in ["receiver.airplay.enable", "receiver.airplay.disable"]
            {
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
    fn the_receiver_is_shipped_disabled_and_declares_no_outbound_network() {
        let m = manifest();
        assert!(!m.prerequisites.outbound_network);
    }
}
