// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Privilege-strategy abstraction for `nmcli` invocations.
//!
//! The network plugin's full privileged surface — connection
//! profile mutation, Wi-Fi radio control, captive-portal probes
//! — funnels through `nmcli`. Under root the plugin can exec
//! `nmcli` directly; under a non-root service identity the plugin
//! must dispatch via `sudo -n nmcli ...` against a narrow
//! NOPASSWD drop-in shipped by the distribution's bootstrap. This
//! module isolates the choice behind one trait so call sites stay
//! strategy-agnostic.
//!
//! ## Strategy selection
//!
//! [`AutoNmcliDispatcher::resolve`] consumes
//! [`LoadContext::capabilities`] at plugin load time. The
//! framework's Privilege Preflight Admission Gate (PPAG) probes
//! the host's nmcli reachability against the calling identity and
//! stamps the resolution on the map under
//! [`INTENT_NMCLI_INVOCATION`]; the composite reads that map,
//! installs the matching leaf strategy, and records the
//! operator-readable rationale for the load-time log line.
//!
//! Fallback (when the map is empty — legacy admission paths that
//! have not been wired to the runner yet): `/proc/self/status`
//! EUID detection. Root → direct; non-root → sudo. Identical
//! mechanism to the playback.mpd plugin's
//! `AutoMpdRestarter::process_needs_sudo`, intentionally so that
//! the two consumers of PPAG inside this distribution observe the
//! same EUID floor when the runner is silent.
//!
//! [`LoadContext::capabilities`]: evo_plugin_sdk::contract::LoadContext::capabilities

use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use std::process::Output;
use std::sync::Arc;
use std::time::Duration;

use evo_plugin_sdk::contract::PluginError;
use evo_plugin_sdk::privileges::{
    CapabilityResolution, CapabilityResolutionMap,
};
use tokio::process::Command;

/// Capability-intent id the framework's preflight runner stamps
/// onto [`LoadContext::capabilities`] for nmcli reachability. The
/// plugin looks this id up at load time to learn the framework-
/// resolved strategy.
///
/// [`LoadContext::capabilities`]:
/// evo_plugin_sdk::contract::LoadContext::capabilities
pub const INTENT_NMCLI_INVOCATION: &str = "nmcli_invocation";

/// Future returned by [`NmcliDispatcher::dispatch`]. The trait is
/// object-safe at the cost of explicit pinning; the
/// [`tokio::process`] command path is `Send` and `'static` so
/// this fits.
pub type DispatchFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Output, PluginError>> + Send + 'a>>;

/// Dispatch strategy for one nmcli invocation. Implementations
/// decide whether to exec the supplied binary directly or via
/// `sudo -n`; the caller passes the binary path + args + timeout
/// at every call so tests can swap the binary to a mock script
/// without rebuilding the dispatcher.
pub trait NmcliDispatcher: Send + Sync + Debug {
    /// Dispatch `bin args...`. `bin` is the resolved nmcli
    /// binary path (`PluginConfig.nmcli_path`); the dispatcher
    /// chooses the invocation shape.
    fn dispatch<'a>(
        &'a self,
        bin: &'a str,
        args: &'a [&'a str],
        timeout: Duration,
    ) -> DispatchFuture<'a>;

    /// Strategy name surfaced in load-time logs ("direct" or
    /// "sudo"). The composite [`AutoNmcliDispatcher`] forwards
    /// to its inner strategy.
    fn strategy_name(&self) -> &'static str;
}

/// Direct exec of `nmcli ...`. Used when the process is running
/// as root (EUID == 0) or when the framework's preflight has
/// confirmed that the calling identity can run nmcli without
/// escalation.
#[derive(Debug, Default)]
pub struct DirectNmcliDispatcher;

impl DirectNmcliDispatcher {
    /// Construct a fresh dispatcher. Stateless.
    pub fn new() -> Self {
        Self
    }
}

impl NmcliDispatcher for DirectNmcliDispatcher {
    fn dispatch<'a>(
        &'a self,
        bin: &'a str,
        args: &'a [&'a str],
        timeout: Duration,
    ) -> DispatchFuture<'a> {
        Box::pin(async move {
            let mut cmd = Command::new(bin);
            cmd.args(args);
            run_with_timeout(cmd, timeout, "nmcli direct").await
        })
    }

    fn strategy_name(&self) -> &'static str {
        "direct"
    }
}

/// `sudo -n nmcli ...` exec. Used when the process is running
/// as a non-root service identity. Relies on the distribution's
/// bootstrap shipping a sudoers drop-in that whitelists the
/// nmcli binary for the service user; absence surfaces as a
/// sudo refusal the caller sees as a `PluginError::Transient`.
#[derive(Debug, Default)]
pub struct SudoNmcliDispatcher;

impl SudoNmcliDispatcher {
    /// Construct a fresh dispatcher. Stateless.
    pub fn new() -> Self {
        Self
    }
}

impl NmcliDispatcher for SudoNmcliDispatcher {
    fn dispatch<'a>(
        &'a self,
        bin: &'a str,
        args: &'a [&'a str],
        timeout: Duration,
    ) -> DispatchFuture<'a> {
        Box::pin(async move {
            let mut cmd = Command::new("sudo");
            cmd.arg("-n").arg(bin).args(args);
            run_with_timeout(cmd, timeout, "sudo -n nmcli").await
        })
    }

    fn strategy_name(&self) -> &'static str {
        "sudo"
    }
}

/// Production dispatch composite. Reads the framework's
/// [`CapabilityResolution`] for [`INTENT_NMCLI_INVOCATION`] at
/// construction time and installs the matching leaf strategy.
/// Falls back to EUID detection when the map is silent.
pub struct AutoNmcliDispatcher {
    inner: Arc<dyn NmcliDispatcher>,
    rationale: String,
}

impl Debug for AutoNmcliDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutoNmcliDispatcher")
            .field("strategy", &self.inner.strategy_name())
            .field("rationale", &self.rationale)
            .finish()
    }
}

impl AutoNmcliDispatcher {
    /// Resolve a strategy from the framework's resolution map.
    /// Plugin entry: pass `&ctx.capabilities` at load time.
    pub fn resolve(map: &CapabilityResolutionMap) -> Self {
        let resolution = map.get(INTENT_NMCLI_INVOCATION);
        Self::resolve_with_eject(resolution, process_needs_sudo)
    }

    /// Resolution path isolated from the EUID-detection syscall
    /// so unit tests can inject a deterministic answer.
    /// `needs_sudo_fn` is consulted only when the framework
    /// resolution does not carry a strategy hint.
    fn resolve_with_eject(
        resolution: Option<&CapabilityResolution>,
        needs_sudo_fn: fn() -> bool,
    ) -> Self {
        match resolution {
            Some(CapabilityResolution::Available {
                strategy: Some(s),
                evidence,
            }) => match s.as_str() {
                "direct" => Self {
                    inner: Arc::new(DirectNmcliDispatcher::new()),
                    rationale: format!(
                        "framework-resolved strategy=direct ({evidence})"
                    ),
                },
                "sudo" => Self {
                    inner: Arc::new(SudoNmcliDispatcher::new()),
                    rationale: format!(
                        "framework-resolved strategy=sudo ({evidence})"
                    ),
                },
                other => Self::fallback(
                    needs_sudo_fn,
                    format!(
                        "framework-resolved strategy={other:?} not \
                         recognised by this build; falling back to EUID \
                         detection"
                    ),
                ),
            },
            Some(CapabilityResolution::Available {
                strategy: None,
                evidence,
            }) => Self::fallback(
                needs_sudo_fn,
                format!(
                    "framework resolution Available without strategy hint \
                     ({evidence}); falling back to EUID detection"
                ),
            ),
            Some(CapabilityResolution::Unavailable { reason, remedy }) => {
                // Nmcli unreachable through any privilege path the
                // framework probed. The plugin still attempts direct
                // exec — under sandboxed test or hardened-host
                // configurations the leg may still work despite the
                // preflight refusal; the rationale carries the
                // remedy so operators see the actionable next step.
                Self {
                    inner: Arc::new(DirectNmcliDispatcher::new()),
                    rationale: format!(
                        "framework refused nmcli_invocation: {reason}; \
                         attempting direct exec anyway (remedy: {remedy})"
                    ),
                }
            }
            Some(CapabilityResolution::Degraded {
                fallback_strategy,
                reason,
            }) => match fallback_strategy.as_str() {
                "direct" => Self {
                    inner: Arc::new(DirectNmcliDispatcher::new()),
                    rationale: format!(
                        "framework degraded nmcli_invocation to \
                         direct fallback: {reason}"
                    ),
                },
                "sudo" => Self {
                    inner: Arc::new(SudoNmcliDispatcher::new()),
                    rationale: format!(
                        "framework degraded nmcli_invocation to \
                         sudo fallback: {reason}"
                    ),
                },
                other => Self::fallback(
                    needs_sudo_fn,
                    format!(
                        "framework degraded nmcli_invocation with \
                         unrecognised fallback={other:?}: {reason}; \
                         falling back to EUID detection"
                    ),
                ),
            },
            Some(CapabilityResolution::NotProbed { reason }) => Self::fallback(
                needs_sudo_fn,
                format!(
                    "framework did not probe nmcli_invocation \
                         ({reason}); falling back to EUID detection"
                ),
            ),
            None => Self::fallback(
                needs_sudo_fn,
                "framework resolution map did not contain \
                 nmcli_invocation; falling back to EUID detection"
                    .to_string(),
            ),
        }
    }

    fn fallback(needs_sudo_fn: fn() -> bool, reason_prefix: String) -> Self {
        let needs_sudo = needs_sudo_fn();
        let (inner, strategy): (Arc<dyn NmcliDispatcher>, &'static str) =
            if needs_sudo {
                (Arc::new(SudoNmcliDispatcher::new()), "sudo")
            } else {
                (Arc::new(DirectNmcliDispatcher::new()), "direct")
            };
        Self {
            inner,
            rationale: format!(
                "{reason_prefix}; EUID detection selected {strategy}"
            ),
        }
    }

    /// Operator-readable rationale: shown in the load-time log
    /// line so the journal explains which strategy the
    /// dispatcher installed and why.
    pub fn rationale(&self) -> &str {
        &self.rationale
    }
}

impl NmcliDispatcher for AutoNmcliDispatcher {
    fn dispatch<'a>(
        &'a self,
        bin: &'a str,
        args: &'a [&'a str],
        timeout: Duration,
    ) -> DispatchFuture<'a> {
        self.inner.dispatch(bin, args, timeout)
    }

    fn strategy_name(&self) -> &'static str {
        self.inner.strategy_name()
    }
}

/// Read the calling process's effective UID. Returns `None` on
/// platforms where the lookup fails (non-Linux dev hosts where
/// `/proc/self/status` is absent; sandboxes that mask `/proc`).
/// Implemented via `/proc/self/status` parsing so the plugin
/// does not pull `libc` / `nix` for one syscall — mirrors the
/// playback.mpd plugin's shape so the two PPAG consumers share
/// the same EUID floor.
#[cfg(target_os = "linux")]
fn linux_effective_uid() -> Option<u32> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    s.lines().find_map(|line| {
        let line = line.trim_start();
        let rest = line.strip_prefix("Uid:")?;
        rest.split_whitespace().nth(1)?.parse().ok()
    })
}

/// Returns `true` when the process needs `sudo -n` to reach
/// nmcli. Linux: `/proc/self/status` EUID parse. Other platforms:
/// `EVO_RUNTIME_USER` env-var set by the distribution's
/// bootstrap when the steward runs as a non-root user.
pub(crate) fn process_needs_sudo() -> bool {
    #[cfg(target_os = "linux")]
    if let Some(uid) = linux_effective_uid() {
        return uid != 0;
    }
    std::env::var("EVO_RUNTIME_USER")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

async fn run_with_timeout(
    mut cmd: Command,
    timeout: Duration,
    label: &str,
) -> Result<Output, PluginError> {
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(v) => v.map_err(|e| {
            PluginError::Transient(format!("spawn {label} failed: {e}"))
        }),
        Err(_) => Err(PluginError::Transient(format!(
            "{label} timed out after {}ms",
            timeout.as_millis()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn always_root() -> bool {
        false
    }

    fn always_non_root() -> bool {
        true
    }

    #[test]
    fn resolve_with_available_strategy_sudo() {
        let resolution = CapabilityResolution::Available {
            evidence: "sudo -l -n permits: /usr/bin/nmcli".into(),
            strategy: Some("sudo".into()),
        };
        let auto = AutoNmcliDispatcher::resolve_with_eject(
            Some(&resolution),
            always_root,
        );
        assert_eq!(auto.strategy_name(), "sudo");
        assert!(auto
            .rationale()
            .contains("framework-resolved strategy=sudo"));
    }

    #[test]
    fn resolve_with_available_strategy_direct() {
        let resolution = CapabilityResolution::Available {
            evidence: "binary present: /usr/bin/nmcli".into(),
            strategy: Some("direct".into()),
        };
        let auto = AutoNmcliDispatcher::resolve_with_eject(
            Some(&resolution),
            always_non_root,
        );
        assert_eq!(auto.strategy_name(), "direct");
        assert!(auto
            .rationale()
            .contains("framework-resolved strategy=direct"));
    }

    #[test]
    fn resolve_with_available_no_strategy_falls_back_to_euid_root() {
        let resolution = CapabilityResolution::Available {
            evidence: "present".into(),
            strategy: None,
        };
        let auto = AutoNmcliDispatcher::resolve_with_eject(
            Some(&resolution),
            always_root,
        );
        assert_eq!(auto.strategy_name(), "direct");
        assert!(auto.rationale().contains("EUID detection selected direct"));
    }

    #[test]
    fn resolve_with_available_no_strategy_falls_back_to_euid_non_root() {
        let resolution = CapabilityResolution::Available {
            evidence: "present".into(),
            strategy: None,
        };
        let auto = AutoNmcliDispatcher::resolve_with_eject(
            Some(&resolution),
            always_non_root,
        );
        assert_eq!(auto.strategy_name(), "sudo");
        assert!(auto.rationale().contains("EUID detection selected sudo"));
    }

    #[test]
    fn resolve_with_unrecognised_strategy_falls_back_to_euid() {
        let resolution = CapabilityResolution::Available {
            evidence: "huh".into(),
            strategy: Some("weird".into()),
        };
        let auto = AutoNmcliDispatcher::resolve_with_eject(
            Some(&resolution),
            always_root,
        );
        assert_eq!(auto.strategy_name(), "direct");
        assert!(auto.rationale().contains("not recognised by this build"));
    }

    #[test]
    fn resolve_with_unavailable_keeps_direct_with_remedy() {
        let resolution = CapabilityResolution::Unavailable {
            reason: "nmcli not on PATH".into(),
            remedy: "install network-manager".into(),
        };
        let auto = AutoNmcliDispatcher::resolve_with_eject(
            Some(&resolution),
            always_non_root,
        );
        assert_eq!(auto.strategy_name(), "direct");
        assert!(auto.rationale().contains("framework refused"));
        assert!(auto.rationale().contains("install network-manager"));
    }

    #[test]
    fn resolve_with_degraded_direct() {
        let resolution = CapabilityResolution::Degraded {
            fallback_strategy: "direct".into(),
            reason: "sudo refused but binary present".into(),
        };
        let auto = AutoNmcliDispatcher::resolve_with_eject(
            Some(&resolution),
            always_non_root,
        );
        assert_eq!(auto.strategy_name(), "direct");
        assert!(auto.rationale().contains("framework degraded"));
    }

    #[test]
    fn resolve_with_degraded_sudo() {
        let resolution = CapabilityResolution::Degraded {
            fallback_strategy: "sudo".into(),
            reason: "primary failed".into(),
        };
        let auto = AutoNmcliDispatcher::resolve_with_eject(
            Some(&resolution),
            always_root,
        );
        assert_eq!(auto.strategy_name(), "sudo");
    }

    #[test]
    fn resolve_with_degraded_unknown_falls_back_to_euid() {
        let resolution = CapabilityResolution::Degraded {
            fallback_strategy: "exotic".into(),
            reason: "weird".into(),
        };
        let auto = AutoNmcliDispatcher::resolve_with_eject(
            Some(&resolution),
            always_root,
        );
        assert_eq!(auto.strategy_name(), "direct");
        assert!(auto.rationale().contains("unrecognised fallback"));
    }

    #[test]
    fn resolve_with_not_probed_falls_back_to_euid() {
        let resolution = CapabilityResolution::NotProbed {
            reason: "probe skipped".into(),
        };
        let auto = AutoNmcliDispatcher::resolve_with_eject(
            Some(&resolution),
            always_non_root,
        );
        assert_eq!(auto.strategy_name(), "sudo");
        assert!(auto.rationale().contains("did not probe"));
    }

    #[test]
    fn resolve_with_no_resolution_falls_back_to_euid() {
        let auto = AutoNmcliDispatcher::resolve_with_eject(None, always_root);
        assert_eq!(auto.strategy_name(), "direct");
        assert!(auto
            .rationale()
            .contains("did not contain nmcli_invocation"));
    }

    #[test]
    fn resolve_from_empty_map_uses_euid_fallback() {
        let map = CapabilityResolutionMap::new();
        let auto = AutoNmcliDispatcher::resolve(&map);
        // Strategy depends on the host's actual EUID. Just
        // verify the resolution succeeded and the rationale
        // mentions EUID detection.
        assert!(matches!(auto.strategy_name(), "direct" | "sudo"));
        assert!(auto.rationale().contains("EUID detection"));
    }

    #[tokio::test]
    async fn direct_dispatcher_invokes_binary_directly() {
        // Use `/bin/true` as a deterministic stand-in for the
        // nmcli binary path. Direct dispatch should exec it and
        // observe success.
        let d = DirectNmcliDispatcher::new();
        let out = d
            .dispatch("/bin/true", &[], Duration::from_millis(2000))
            .await
            .expect("direct dispatch should succeed against /bin/true");
        assert!(out.status.success());
    }

    #[tokio::test]
    async fn dispatcher_timeout_surfaces_transient_error() {
        let d = DirectNmcliDispatcher::new();
        let out = d
            .dispatch("/bin/sleep", &["10"], Duration::from_millis(50))
            .await;
        match out {
            Err(PluginError::Transient(msg)) => {
                assert!(msg.contains("timed out"));
            }
            other => panic!("expected Transient timeout, got {other:?}"),
        }
    }
}
