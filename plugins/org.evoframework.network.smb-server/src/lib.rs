// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! # org-evoframework-network-smb-server
//!
//! Framework-reference plugin that stocks the SMB-server verb
//! subset on the `networking.shares` shelf. Owns the persisted
//! SMB-server configuration (enabled toggle + `server min
//! protocol` selector + operator-defined `extra_shares` + SMB
//! users), renders `smb.conf` from that state, validates via
//! `testparm -s`, installs over `/etc/samba/smb.conf`, restarts
//! `smbd`, and manages SMB users via `evo-smb-user-sync`
//! (nologin NSS + Samba passdb). Publishes the reactive
//! `system_smb_server` subject on every apply / user_add /
//! user_revoke transition.
//!
//! Sibling plugin `org.evoframework.network.shares` stocks the
//! share-management + discovery verb subset on the same shelf;
//! the framework's multi-occupant router partitions verbs
//! across the two plugins.
//!
//! ## Share inventory (normative)
//!
//! Stock music shares, delivery shares (`evo-plugins-stage`,
//! `Uploads`), allow/deny prefixes, forbidden LAN names, and
//! sole-ownership of `/etc/samba/smb.conf` are pinned in
//! [`docs/SAMBA-SHARES.md`](../docs/SAMBA-SHARES.md), which
//! carries the audio-distribution binding. Do not re-derive
//! the share list from comments or chat.
//!
//! ## Path allowlist
//!
//! Every `extra_share` whose `path` does not begin with a
//! prefix in the runtime's allowlist is refused with a
//! structured [`runtime::RefusedSetting`] surfaced on the
//! [`runtime::ApplyReport`] response — the operator UI renders
//! the refusal inline against the row rather than as a generic
//! form-level error. Default prefixes MUST match
//! `docs/SAMBA-SHARES.md` (evo music plane + uploads + plugin
//! stage — not classic Volumio `/data` / `/mnt` paths).
//!
//! ## Sudoers grant
//!
//! testparm + `evo-smb-user-sync` + `systemctl restart smbd`
//! are invoked under the narrow `EVO_SAMBA_SERVER_CONFIG` +
//! `EVO_SAMBA_SERVER_USERS` + `EVO_SAMBA_SERVER_RESTART`
//! Cmnd_Aliases shipped in
//! `dist/sudoers.d/evo-samba-server.in`. Bootstrap installs
//! the wrapper to `/usr/local/bin/evo-smb-user-sync` and the
//! sudoers drop-in at mode 0440 root:root.

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

pub mod runtime;

use runtime::{
    is_smb_server_verb, SambaServerRuntime, VerbDispatchError, SMB_SERVER_VERBS,
};

/// Embedded manifest source.
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");

/// Plugin name (reverse-DNS); same as manifest and tests.
pub const PLUGIN_NAME: &str = "org.evoframework.network.smb-server";

/// Parse the embedded [`Manifest`].
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML).expect(
        "org-evoframework-network-smb-server: embedded manifest must parse",
    )
}

fn plugin_crate_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is valid semver")
}

/// The plugin singleton. Holds the internal runtime + the
/// load-state flag the request handler consults.
pub struct SmbServerPlugin {
    loaded: bool,
    runtime: Option<Arc<SambaServerRuntime>>,
}

impl SmbServerPlugin {
    /// New instance; call [`Plugin::load`] before handling requests.
    pub fn new() -> Self {
        Self {
            loaded: false,
            runtime: None,
        }
    }
}

impl Default for SmbServerPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for SmbServerPlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: PLUGIN_NAME.to_string(),
                    version: plugin_crate_version(),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: SMB_SERVER_VERBS
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
            tracing::info!(
                plugin = PLUGIN_NAME,
                state_dir = %ctx.state_dir.display(),
                "network.smb-server plugin load"
            );

            std::fs::create_dir_all(&ctx.state_dir).map_err(|e| {
                PluginError::Permanent(format!(
                    "create state_dir {}: {e}",
                    ctx.state_dir.display()
                ))
            })?;

            let rt = Arc::new(
                SambaServerRuntime::open(&ctx.state_dir).map_err(|e| {
                    PluginError::Permanent(format!(
                        "network.smb-server runtime open failed: {e}"
                    ))
                })?,
            );

            // Attach the plugin's subject-announcer handle so
            // the runtime republishes the singleton on every
            // apply / user_add / user_revoke transition.
            let announcer = Arc::clone(&ctx.subject_announcer);
            if let Err(e) = rt.attach_subject_publisher(announcer).await {
                // LOGGING.md §2: warn (recoverable — dispatch_verb
                // still executes handle-method calls; consumers
                // see the initial empty subject envelope only
                // until the next restart retries the attach path).
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "network.smb-server subject publisher attach failed; \
                     runtime continues without republishing until next \
                     restart"
                );
            }

            self.runtime = Some(rt);
            self.loaded = true;
            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            self.runtime = None;
            self.loaded = false;
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            if self.loaded && self.runtime.is_some() {
                HealthReport::healthy()
            } else {
                HealthReport::unhealthy("network.smb-server plugin not loaded")
            }
        }
    }
}

impl Respondent for SmbServerPlugin {
    fn handle_request<'a>(
        &'a self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            let Some(rt) = self.runtime.as_ref() else {
                return Err(PluginError::Permanent(
                    "network.smb-server plugin not loaded".to_string(),
                ));
            };
            if req.is_past_deadline() {
                return Err(PluginError::Transient(
                    "request deadline already expired".to_string(),
                ));
            }
            let verb = req.request_type.as_str();
            if !is_smb_server_verb(verb) {
                return Err(PluginError::Permanent(format!(
                    "network.smb-server: unknown verb {verb:?}"
                )));
            }
            tracing::debug!(
                plugin = PLUGIN_NAME,
                verb,
                cid = req.correlation_id,
                scope = req.principal_scope.as_deref().unwrap_or("<none>"),
                has_step_up = req.has_step_up,
                "network.smb-server: dispatcher-authorised verb"
            );
            match rt.dispatch_verb(verb, &req.payload).await {
                Ok(bytes) => Ok(Response::for_request(req, bytes)),
                Err(e) => Err(verb_error_to_plugin_error(e)),
            }
        }
    }
}

fn verb_error_to_plugin_error(e: VerbDispatchError) -> PluginError {
    match e {
        VerbDispatchError::UnknownRequestType { .. }
        | VerbDispatchError::PayloadDecode { .. } => {
            PluginError::Permanent(e.to_string())
        }
        VerbDispatchError::Apply(_) => PluginError::Transient(e.to_string()),
        VerbDispatchError::ResponseSerialise { .. } => {
            PluginError::Permanent(e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses() {
        let _ = manifest();
    }

    #[test]
    fn manifest_declares_every_runtime_verb() {
        let m = manifest();
        let resp = m
            .capabilities
            .respondent
            .as_ref()
            .expect("respondent capabilities present");
        for v in SMB_SERVER_VERBS {
            assert!(
                resp.request_types.iter().any(|s| s == v),
                "manifest request_types missing {v:?}"
            );
        }
        assert_eq!(resp.request_types.len(), SMB_SERVER_VERBS.len());
    }

    #[test]
    fn manifest_target_names_the_networking_shares_shelf() {
        let m = manifest();
        assert_eq!(m.target.shelf, "networking.shares");
        assert_eq!(m.target.shape, 1);
    }
}
