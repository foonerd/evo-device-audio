// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! # org-evoframework-network-shares
//!
//! Framework-reference plugin that stocks the share-management +
//! discovery verb subset on the `networking.shares` shelf. Owns
//! the persisted share record set, the CIFS dialect probe, NFS
//! mount, LAN discovery via `avahi-browse` + `smbclient`, mount
//! lifecycle (boot-mount + periodic remount + 5-min discovery
//! cadence), and the reactive subjects the operator UI's shares
//! surface consumes:
//!
//! - `system_network_shares_configured` (singleton)
//! - `system_network_shares_discovered` (singleton)
//! - `network_share_state` (per-share instance)
//!
//! Sibling plugin `org.evoframework.network.smb-server` stocks
//! the SMB-server verb subset on the same shelf; the framework's
//! multi-occupant router partitions verbs across the two.
//!
//! ## What this plugin does
//!
//! On [`Plugin::load`] the plugin opens (or creates)
//! `<state_dir>/network_shares.toml`, attaches its
//! `LoadContext.subject_announcer` handle to the internal
//! [`runtime::NetworkSharesRuntime`], and spawns three
//! background tasks:
//!
//! 1. `boot_mount_all` — walks every persisted share and
//!    attempts an initial mount. Runs as a detached task so
//!    slow CIFS probe ladders on unreachable NAS never gate
//!    plugin readiness.
//! 2. Remount task — retries any share currently in `Failed`
//!    or `Unmounted` on a 5-min cadence.
//! 3. Discovery task — refreshes the LAN NAS inventory via
//!    `avahi-browse` + per-host `smbclient` on a 5-min cadence.
//!
//! On [`Respondent::handle_request`] the plugin routes the
//! `request_type` through [`runtime::NetworkSharesRuntime::dispatch_verb`]
//! which handles every declared verb (add / edit / remove /
//! mount / unmount / discovery.refresh + the three read verbs
//! list_configured / discovery.list / get_state).
//!
//! ## Sudoers grant
//!
//! Mount + umount + avahi-browse + smbclient are invoked under
//! the narrow `EVO_NETWORK_SHARES_MOUNT` +
//! `EVO_NETWORK_SHARES_DISCOVERY` Cmnd_Aliases shipped in
//! `dist/sudoers.d/evo-network-shares.in`. The distribution's
//! bootstrap script renders + installs the drop-in at mode
//! 0440 root:root.

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
    is_network_shares_verb, spawn_discovery_task, spawn_remount_task,
    NetworkSharesRuntime, VerbDispatchError, DEFAULT_DISCOVERY_CADENCE_MS,
    DEFAULT_REMOUNT_CADENCE_MS, NETWORK_SHARES_VERBS,
};

/// Embedded manifest source.
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");

/// Plugin name (reverse-DNS); same as manifest and tests.
pub const PLUGIN_NAME: &str = "org.evoframework.network.shares";

/// Parse the embedded [`Manifest`].
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("org-evoframework-network-shares: embedded manifest must parse")
}

fn plugin_crate_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is valid semver")
}

/// The plugin singleton. Holds the internal runtime + the
/// background-task join handles so shutdown can abort them.
pub struct NetworkSharesPlugin {
    loaded: bool,
    runtime: Option<Arc<NetworkSharesRuntime>>,
    remount_task: Option<tokio::task::JoinHandle<()>>,
    discovery_task: Option<tokio::task::JoinHandle<()>>,
    boot_mount_task: Option<tokio::task::JoinHandle<()>>,
}

impl NetworkSharesPlugin {
    /// New instance; call [`Plugin::load`] before handling requests.
    pub fn new() -> Self {
        Self {
            loaded: false,
            runtime: None,
            remount_task: None,
            discovery_task: None,
            boot_mount_task: None,
        }
    }
}

impl Default for NetworkSharesPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for NetworkSharesPlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: PLUGIN_NAME.to_string(),
                    version: plugin_crate_version(),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: NETWORK_SHARES_VERBS
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
                "network.shares plugin load"
            );

            // Ensure the state directory exists before the
            // runtime tries to open its TOML file.
            std::fs::create_dir_all(&ctx.state_dir).map_err(|e| {
                PluginError::Permanent(format!(
                    "create state_dir {}: {e}",
                    ctx.state_dir.display()
                ))
            })?;

            // Ensure the mount root exists before any verb tries
            // to mount into it. Absent, `mount.cifs` /
            // `mount.nfs` fail with `os error 2` (ENOENT) even
            // when the helper binary is installed and the NAS
            // is reachable. Idempotent create_dir_all is the
            // correct posture on writable filesystems: a
            // downstream distribution's installer may have
            // already provisioned the directory with its own
            // ownership + mode; this call is a belt-and-braces
            // guarantee against handover onto a host where no
            // installer ran.
            //
            // On hosts where `/mnt` is a read-only filesystem
            // (immutable OS images, some appliance targets),
            // the create call fails with `EROFS`. That is NOT
            // a plugin-level admission fault — the distribution
            // is responsible for provisioning the mount root
            // into the image at build time. Log the condition
            // as a warning naming the failure; the mount verbs
            // themselves surface the actual missing-directory
            // error at invocation with a clearer per-verb
            // failure_mode.
            // EROFS = 30 on Linux (read-only filesystem). Match
            // on raw_os_error rather than `ErrorKind::ReadOnlyFilesystem`
            // because the latter is unstable at the time of
            // writing; the numeric constant is stable ABI on
            // every Linux target the framework supports.
            const EROFS: i32 = 30;
            match std::fs::create_dir_all(crate::runtime::NAS_MOUNT_ROOT) {
                Ok(()) => {}
                Err(e) if e.raw_os_error() == Some(EROFS) => {
                    tracing::warn!(
                        plugin = PLUGIN_NAME,
                        mount_root = crate::runtime::NAS_MOUNT_ROOT,
                        error = %e,
                        "mount root cannot be auto-created on this host \
                         (filesystem is read-only); provisioning must \
                         land it at image build time; mount verbs will \
                         refuse with the missing-directory error"
                    );
                }
                Err(e) => {
                    return Err(PluginError::Permanent(format!(
                        "create mount root {}: {e}",
                        crate::runtime::NAS_MOUNT_ROOT
                    )));
                }
            }

            // Open the runtime against the plugin's state_dir.
            // The builder path lets us swap the executor +
            // credentials fetcher for tests; the plugin's load
            // path uses the production defaults (subprocess
            // executor + no credential fetcher — vault access
            // for password shares lands in a follow-on ship).
            let rt = Arc::new(
                NetworkSharesRuntime::open(&ctx.state_dir).map_err(|e| {
                    PluginError::Permanent(format!(
                        "network.shares runtime open failed: {e}"
                    ))
                })?,
            );

            // Attach the plugin's subject-announcer handle so
            // the runtime republishes the three subjects on
            // every CRUD / mount / discovery transition.
            let announcer = Arc::clone(&ctx.subject_announcer);
            if let Err(e) = rt.attach_subject_publisher(announcer).await {
                // LOGGING.md §2: warn (recoverable — dispatch_verb
                // still executes handle-method calls; consumers
                // see the initial empty subject envelope only
                // until the next restart retries the attach path).
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "network.shares subject publisher attach failed; \
                     runtime continues without republishing until next \
                     restart"
                );
            }

            // Boot-mount fires as a detached task so slow probe
            // ladders on unreachable NAS never gate plugin
            // readiness. Per-share state transitions publish on
            // the subject substrate as they complete.
            let boot_rt = Arc::clone(&rt);
            self.boot_mount_task = Some(tokio::spawn(async move {
                let report = boot_rt.boot_mount_all().await;
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    mounted = report.success_count(),
                    failed = report.failure_count(),
                    total = report.outcomes.len(),
                    "network.shares boot-mount sweep complete"
                );
            }));

            self.remount_task = Some(spawn_remount_task(
                Arc::clone(&rt),
                std::time::Duration::from_millis(DEFAULT_REMOUNT_CADENCE_MS),
            ));
            self.discovery_task = Some(spawn_discovery_task(
                Arc::clone(&rt),
                std::time::Duration::from_millis(DEFAULT_DISCOVERY_CADENCE_MS),
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
            // Abort background tasks so a reload cycle does not
            // leave two remount + discovery loops racing on the
            // same state file.
            if let Some(h) = self.boot_mount_task.take() {
                h.abort();
            }
            if let Some(h) = self.remount_task.take() {
                h.abort();
            }
            if let Some(h) = self.discovery_task.take() {
                h.abort();
            }
            // Drop the runtime so its background tasks (which
            // hold Weak references) observe drop and exit.
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
                HealthReport::unhealthy("network.shares plugin not loaded")
            }
        }
    }
}

impl Respondent for NetworkSharesPlugin {
    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            let Some(rt) = self.runtime.as_ref() else {
                return Err(PluginError::Permanent(
                    "network.shares plugin not loaded".to_string(),
                ));
            };
            if req.is_past_deadline() {
                return Err(PluginError::Transient(
                    "request deadline already expired".to_string(),
                ));
            }
            let verb = req.request_type.as_str();
            if !is_network_shares_verb(verb) {
                return Err(PluginError::Permanent(format!(
                    "network.shares: unknown verb {verb:?}"
                )));
            }
            tracing::debug!(
                plugin = PLUGIN_NAME,
                verb,
                cid = req.correlation_id,
                scope = req.principal_scope.as_deref().unwrap_or("<none>"),
                has_step_up = req.has_step_up,
                "network.shares: dispatcher-authorised verb"
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
        | VerbDispatchError::PayloadDecode { .. }
        | VerbDispatchError::Persistence(_) => {
            PluginError::Permanent(e.to_string())
        }
        VerbDispatchError::Mount(_) => PluginError::Transient(e.to_string()),
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
        for v in NETWORK_SHARES_VERBS {
            assert!(
                resp.request_types.iter().any(|s| s == v),
                "manifest request_types missing {v:?}"
            );
        }
        assert_eq!(resp.request_types.len(), NETWORK_SHARES_VERBS.len());
    }

    #[test]
    fn manifest_target_names_the_networking_shares_shelf() {
        let m = manifest();
        assert_eq!(m.target.shelf, "networking.shares");
        assert_eq!(m.target.shape, 1);
    }
}
