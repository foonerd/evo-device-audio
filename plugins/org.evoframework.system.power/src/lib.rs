//! # org-evoframework-system-power
//!
//! Framework-reserved power-management plugin. Stocks the
//! `system.power` shelf with two operator-gestured verbs:
//!
//! - `reboot_device` — request an orderly host reboot via
//!   `sudo /usr/bin/systemctl reboot`.
//! - `power_off_device` — request an orderly host power-off via
//!   `sudo /usr/bin/systemctl poweroff`.
//!
//! Both verbs are gated at the framework dispatcher's per-verb
//! capability gate (step_up:system_admin) — the principal must
//! hold the system_admin write scope AND an active step-up auth
//! session before the request reaches this plugin's
//! `handle_request`. A request that does not satisfy the gate
//! refuses with `permission_denied` at the dispatcher boundary
//! and this plugin never sees the call. The plugin does NOT
//! re-check the principal; the framework's gate is the
//! authoritative check.
//!
//! ## What this plugin does
//!
//! On a successful gate, the plugin shells out to `systemctl`
//! via the narrow `EVO_SYSTEM_POWER` sudoers grant
//! (`dist/sudoers.d/evo-system-power`). `systemctl` is a client
//! that signals PID 1 over the system D-Bus; the call itself
//! does not write through the filesystem, so the steward's
//! `ProtectSystem=strict` mount-namespace policy does not bite
//! (no systemd drop-in widening `ReadWritePaths` is needed). The
//! plugin returns to the framework as soon as `systemctl` exits;
//! PID 1 performs the actual shutdown asynchronously, so by the
//! time the operator's wire response arrives the framework
//! itself is likely shutting down.
//!
//! ## What this plugin does NOT do
//!
//! - Schedule a delayed shutdown. The verbs are immediate. If
//!   operator policy needs a grace window, a warden surface
//!   layered on top of this verb set can express it.
//! - Negotiate with other plugins. A reboot is a host-level
//!   event; framework shutdown hooks fire as PID 1 sends SIGTERM
//!   to the steward.
//! - Provide a power state machine. A broader idle / sleep /
//!   wake state machine layers on top of the verb surface this
//!   plugin delivers.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::manual_async_fn)]

use std::future::Future;

use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities,
};
use evo_plugin_sdk::Manifest;

/// Embedded manifest source.
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");

/// Plugin name (reverse-DNS); same as manifest and tests.
pub const PLUGIN_NAME: &str = "org.evoframework.system.power";

/// The verb name for `reboot_device`. Match the manifest entry
/// and the dispatch arm below; a single source of truth.
pub const VERB_REBOOT_DEVICE: &str = "reboot_device";

/// The verb name for `power_off_device`.
pub const VERB_POWER_OFF_DEVICE: &str = "power_off_device";

/// Default path to `systemctl` on Linux distributions targeted
/// by this reference plugin. The value can be overridden via the
/// `systemctl_path` plugin config key (`/etc/evo/plugins.d/
/// org.evoframework.system.power.toml`) for non-standard hosts;
/// vendor distributions that route reboot through a different
/// binary ship their own plugin under their namespace and do not
/// override this path.
const DEFAULT_SYSTEMCTL_PATH: &str = "/usr/bin/systemctl";

/// Default path to `sudo`. Override via the `sudo_path` config
/// key if the host's sudo lives at a non-standard path.
const DEFAULT_SUDO_PATH: &str = "/usr/bin/sudo";

/// Parse the embedded [`Manifest`].
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("org-evoframework-system-power: embedded manifest must parse")
}

fn plugin_crate_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is valid semver")
}

/// Resolved plugin configuration. Populated at load time from
/// the operator-supplied `LoadContext.config` table; absent keys
/// fall back to the documented defaults.
#[derive(Debug, Clone)]
struct PluginConfig {
    systemctl_path: String,
    sudo_path: String,
}

impl PluginConfig {
    fn defaults() -> Self {
        Self {
            systemctl_path: DEFAULT_SYSTEMCTL_PATH.to_string(),
            sudo_path: DEFAULT_SUDO_PATH.to_string(),
        }
    }

    fn from_load_context(ctx: &LoadContext) -> Self {
        let mut out = Self::defaults();
        if let Some(toml::Value::String(s)) = ctx.config.get("systemctl_path") {
            out.systemctl_path = s.clone();
        }
        if let Some(toml::Value::String(s)) = ctx.config.get("sudo_path") {
            out.sudo_path = s.clone();
        }
        out
    }
}

/// The plugin singleton. Holds the resolved per-host paths and
/// a load-state flag the request handler consults.
pub struct SystemPowerPlugin {
    loaded: bool,
    config: PluginConfig,
}

impl SystemPowerPlugin {
    /// New instance; call [`Plugin::load`] before handling requests.
    pub fn new() -> Self {
        Self {
            loaded: false,
            config: PluginConfig::defaults(),
        }
    }
}

impl Default for SystemPowerPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for SystemPowerPlugin {
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
                        VERB_REBOOT_DEVICE.to_string(),
                        VERB_POWER_OFF_DEVICE.to_string(),
                    ],
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
            tracing::info!(plugin = PLUGIN_NAME, "system.power plugin load");
            self.config = PluginConfig::from_load_context(ctx);
            tracing::info!(
                plugin = PLUGIN_NAME,
                systemctl_path = %self.config.systemctl_path,
                sudo_path = %self.config.sudo_path,
                "system.power plugin: resolved invocation paths"
            );
            self.loaded = true;
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
                HealthReport::unhealthy("system.power plugin not loaded")
            }
        }
    }
}

impl Respondent for SystemPowerPlugin {
    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "system.power plugin not loaded".to_string(),
                ));
            }
            if req.is_past_deadline() {
                return Err(PluginError::Transient(
                    "request deadline already expired".to_string(),
                ));
            }
            // Structured log of the elevation the framework
            // dispatcher verified before forwarding this request.
            // The plugin does NOT re-check the principal; this
            // log line is the audit trail of the framework's
            // verdict.
            tracing::info!(
                plugin = PLUGIN_NAME,
                verb = req.request_type.as_str(),
                cid = req.correlation_id,
                scope = req.principal_scope.as_deref().unwrap_or("<none>"),
                has_step_up = req.has_step_up,
                "system.power: dispatcher-authorised verb"
            );
            match req.request_type.as_str() {
                VERB_REBOOT_DEVICE => {
                    self.invoke_systemctl("reboot").await?;
                    Ok(Response::for_request(req, Vec::new()))
                }
                VERB_POWER_OFF_DEVICE => {
                    self.invoke_systemctl("poweroff").await?;
                    Ok(Response::for_request(req, Vec::new()))
                }
                other => Err(PluginError::Permanent(format!(
                    "system.power: unknown verb {other:?}"
                ))),
            }
        }
    }
}

impl SystemPowerPlugin {
    async fn invoke_systemctl(
        &self,
        verb: &'static str,
    ) -> Result<(), PluginError> {
        // The sudoers grant ships under the narrow
        // EVO_SYSTEM_POWER Cmnd_Aliases (one per verb) so the
        // invocation MUST pass the verb as a single argv element
        // exactly matching the alias entry. Tokio's `arg(verb)`
        // does this — no shell interpolation involved.
        let output = tokio::process::Command::new(&self.config.sudo_path)
            .arg("-n")
            .arg(&self.config.systemctl_path)
            .arg(verb)
            .output()
            .await
            .map_err(|e| {
                PluginError::Transient(format!(
                    "system.power: failed to spawn sudo: {e}"
                ))
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(PluginError::Transient(format!(
                "system.power: sudo {} systemctl {} failed (status={}): \
                 {}",
                self.config.sudo_path,
                verb,
                output.status,
                stderr.trim()
            )));
        }
        tracing::warn!(
            plugin = PLUGIN_NAME,
            verb,
            "system.power: systemctl issued; PID 1 is shutting down"
        );
        Ok(())
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
    fn manifest_declares_verb_capabilities_for_both_verbs() {
        let m = manifest();
        let resp = m
            .capabilities
            .respondent
            .as_ref()
            .expect("respondent capabilities present");
        assert_eq!(resp.request_types.len(), 2);
        assert!(resp.request_types.iter().any(|s| s == VERB_REBOOT_DEVICE));
        assert!(resp
            .request_types
            .iter()
            .any(|s| s == VERB_POWER_OFF_DEVICE));
        // Both verbs MUST declare step_up:system_admin so the
        // framework dispatcher gates them before the request
        // reaches the plugin.
        for verb in [VERB_REBOOT_DEVICE, VERB_POWER_OFF_DEVICE] {
            let cap = resp.verb_capabilities.get(verb).unwrap_or_else(|| {
                panic!("manifest must declare verb_capabilities[{verb:?}]")
            });
            match cap {
                evo_plugin_sdk::manifest::VerbCapability::StepUp { scope } => {
                    assert_eq!(scope, "system_admin");
                }
                other => panic!(
                    "verb_capabilities[{verb:?}] must be StepUp; got {other:?}"
                ),
            }
        }
    }

    #[test]
    fn plugin_config_defaults_match_canonical_paths() {
        let cfg = PluginConfig::defaults();
        assert_eq!(cfg.systemctl_path, DEFAULT_SYSTEMCTL_PATH);
        assert_eq!(cfg.sudo_path, DEFAULT_SUDO_PATH);
    }
}
