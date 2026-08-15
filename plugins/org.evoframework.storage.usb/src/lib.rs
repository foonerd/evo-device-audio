// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! # org-evoframework-storage-usb
//!
//! Framework-reference plugin that stocks the `storage.usb`
//! shelf. Owns udev-driven USB block-device enumeration,
//! six-way role classification (system-root / system-boot /
//! system-efi / system-swap / system-adjacent / removable),
//! stable-id derivation, mount lifecycle at
//! `/var/lib/evo/music/USB/<stable-id>/`, filesystem repair,
//! safe-remove, operator-alias persistence, and the reactive
//! `storage_usb_drives` subject the operator UI's Sources page
//! consumes.
//!
//! Step-2 skeleton: this crate lands the plugin admission
//! surface, the pure-function classifier with fixture tests,
//! the FS support matrix, and a working `storage.usb.list_drives`
//! verb that returns the live classifier output. The four
//! mutating verbs (`mount` / `safe_remove` /
//! `repair_filesystem` / `rename`) return a stable
//! not-implemented response until Steps 3-5 wire them in place;
//! the wrapper's argv shape stays stable across the roll-in so
//! bootstrap + sudoers grant do not churn.
//!
//! # Trust boundary
//!
//! Every mount / umount / fsck / eject invocation dispatches
//! through the narrow root-only wrapper at
//! `/usr/local/bin/evo-usb-mount`. The plugin does NOT hold raw
//! sudo grants on the underlying tools; the wrapper's argv
//! allowlist (path allowlist for mount targets + block-device
//! allowlist for source arguments) is the last-mile runtime
//! enforcement. The bootstrap installs the wrapper + sudoers
//! drop-in at install time (Step 1g).

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

pub mod classifier;
pub mod fs_matrix;
pub mod runtime;

use runtime::{
    detect_service_uid_gid, is_storage_usb_verb, StorageUsbRuntime,
    VerbDispatchError, STORAGE_USB_VERBS,
};

/// Embedded manifest source.
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");

/// Plugin name (reverse-DNS); same as manifest and tests.
pub const PLUGIN_NAME: &str = "org.evoframework.storage.usb";

/// Parse the embedded [`Manifest`].
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("org-evoframework-storage-usb: embedded manifest must parse")
}

fn plugin_crate_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is valid semver")
}

/// The plugin singleton. Holds the internal runtime + the
/// load-state flag the request handler consults.
pub struct StorageUsbPlugin {
    loaded: bool,
    runtime: Option<Arc<StorageUsbRuntime>>,
}

impl StorageUsbPlugin {
    /// New instance; call [`Plugin::load`] before handling requests.
    pub fn new() -> Self {
        Self {
            loaded: false,
            runtime: None,
        }
    }
}

impl Default for StorageUsbPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for StorageUsbPlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: PLUGIN_NAME.to_string(),
                    version: plugin_crate_version(),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: STORAGE_USB_VERBS
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
                "storage.usb plugin load"
            );

            // Ensure the state directory exists. The bootstrap
            // installer creates the top-level plugin state root
            // + credentials sub-dir at Step 1g with mode 0700
            // SERVICE_USER; the plugin itself creates the
            // `<state_dir>/` path passed by the LoadContext,
            // which is a per-instance sub-dir under the top-
            // level plugin root.
            std::fs::create_dir_all(&ctx.state_dir).map_err(|e| {
                PluginError::Permanent(format!(
                    "create state_dir {}: {e}",
                    ctx.state_dir.display()
                ))
            })?;

            // Resolve the steward's effective uid/gid for the
            // FS-matrix mount options. `detect_service_uid_gid`
            // reads /proc/self/status (this crate forbids
            // unsafe_code so we cannot call libc::geteuid
            // directly).
            let (service_uid, service_gid) =
                detect_service_uid_gid().map_err(|e| {
                    PluginError::Permanent(format!(
                        "storage.usb: failed to resolve steward uid/gid: {e}"
                    ))
                })?;
            // Root plugins do not need `sudo -n` wrapping; non-
            // root plugins need the narrow NOPASSWD drop-in the
            // distribution ships at `dist/sudoers.d/evo-storage-usb.in`.
            let needs_sudo = service_uid != 0;
            tracing::info!(
                plugin = PLUGIN_NAME,
                service_uid,
                service_gid,
                needs_sudo,
                "storage.usb: steward identity resolved"
            );

            let rt = Arc::new(StorageUsbRuntime::new(
                service_uid,
                service_gid,
                needs_sudo,
            ));

            self.runtime = Some(rt);
            self.loaded = true;
            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            // No background tasks to abort in Step 2. The
            // hotplug monitor + coldplug reconcile land in
            // Step 3 and get corresponding abort handling
            // here.
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
                HealthReport::unhealthy("storage.usb plugin not loaded")
            }
        }
    }
}

impl Respondent for StorageUsbPlugin {
    fn handle_request<'a>(
        &'a self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            let Some(rt) = self.runtime.as_ref() else {
                return Err(PluginError::Permanent(
                    "storage.usb plugin not loaded".to_string(),
                ));
            };
            if req.is_past_deadline() {
                return Err(PluginError::Transient(
                    "request deadline already expired".to_string(),
                ));
            }
            let verb = req.request_type.as_str();
            if !is_storage_usb_verb(verb) {
                return Err(PluginError::Permanent(format!(
                    "storage.usb: unknown verb {verb:?}"
                )));
            }
            tracing::debug!(
                plugin = PLUGIN_NAME,
                verb,
                cid = req.correlation_id,
                scope = req.principal_scope.as_deref().unwrap_or("<none>"),
                has_step_up = req.has_step_up,
                "storage.usb: dispatcher-authorised verb"
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
        | VerbDispatchError::ResponseSerialise { .. } => {
            PluginError::Permanent(e.to_string())
        }
        VerbDispatchError::NotImplemented { .. } => {
            PluginError::Permanent(e.to_string())
        }
        VerbDispatchError::Classify(_) | VerbDispatchError::InputSource(_) => {
            PluginError::Transient(e.to_string())
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
        for v in STORAGE_USB_VERBS {
            assert!(
                resp.request_types.iter().any(|s| s == v),
                "manifest request_types missing {v:?}"
            );
        }
        assert_eq!(resp.request_types.len(), STORAGE_USB_VERBS.len());
    }

    #[test]
    fn manifest_target_names_the_storage_usb_shelf() {
        let m = manifest();
        assert_eq!(m.target.shelf, "storage.usb");
        assert_eq!(m.target.shape, 1);
    }
}
