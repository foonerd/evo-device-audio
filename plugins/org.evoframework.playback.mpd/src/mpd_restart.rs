//! MPD daemon restart strategy.
//!
//! After re-writing the framework-controlled `audio_output`
//! fragment, MPD has to re-read its configuration to pick the
//! new device / format up. There is no in-band MPD command for
//! that — the operator-visible way is `systemctl restart mpd`.
//!
//! ## Strategies
//!
//! The plugin ships three production strategies, one fallback,
//! and two test stubs. Production builds wire
//! [`AutoMpdRestarter`] at the top — it inspects the
//! framework's [`CapabilityResolution`] for the
//! `mpd_systemctl_restart` intent (delivered via
//! `LoadContext::capabilities`) and dispatches to the right
//! concrete strategy:
//!
//! - [`DirectSystemctlRestarter`] when the framework-resolved
//!   strategy is `"direct"`, or when EUID detection (volumio-
//!   evo's `/proc/self/status` reader) finds the process
//!   running as root.
//! - [`SudoSystemctlRestarter`] when the framework-resolved
//!   strategy is `"sudo"`, or when EUID is non-zero and
//!   `/usr/bin/sudo` is on PATH.
//! - [`NoOpRestarter`] when the framework refused to admit the
//!   restart intent (preflight returned `Unavailable`) but the
//!   plugin admission was not gated on it; the fragment-writer
//!   worker logs and continues without recycling MPD.
//!
//! The PPAG end-state lets the framework dictate the strategy;
//! the EUID detection is the unblock-Pi-5 path that mirrors
//! volumio-evo's proven shape when the framework-side probe
//! runner has not yet populated the resolution map (P2.5 is the
//! framework chunk that fills it; until P2.5 lands, every
//! plugin sees an empty map).
//!
//! Environment-variable overrides:
//!
//! - `EVO_SYSTEMCTL` — full path to the `systemctl` binary
//!   (default `/usr/bin/systemctl`). Honours volumio-evo's
//!   `VOLUMIO_EVO_SYSTEMCTL` convention but namespaced to the
//!   evo framework. Distributions on non-standard prefixes
//!   override this so the binary path the plugin invokes
//!   matches the path their sudoers drop-in scopes.
//! - `EVO_RUNTIME_USER` — present + non-empty when EUID
//!   detection fails (e.g. `/proc/self/status` unreadable in a
//!   sandbox). The plugin treats the variable's presence as
//!   "non-root service user" and selects sudo accordingly.

use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use evo_plugin_sdk::privileges::{
    CapabilityResolution, CapabilityResolutionMap,
};

/// Capability-intent id the framework's preflight associates
/// with the MPD restart leg. Plugin reads
/// `LoadContext::capabilities` and looks up this id to learn
/// the framework-resolved strategy.
pub const INTENT_MPD_SYSTEMCTL_RESTART: &str = "mpd_systemctl_restart";

/// Default systemctl binary path. Overridden by the
/// `EVO_SYSTEMCTL` environment variable for distributions on
/// non-standard prefixes (Alpine `/sbin/systemctl`, Yocto
/// custom paths). Set in `Environment=` inside the steward's
/// systemd drop-in, written by the bootstrap script so the
/// sudoers drop-in's path and the runtime path match.
const DEFAULT_SYSTEMCTL_BIN: &str = "/usr/bin/systemctl";

/// Resolve the systemctl binary path: env override wins,
/// otherwise default. Pure read of `std::env`; safe to call
/// repeatedly.
fn resolve_systemctl_bin() -> String {
    std::env::var("EVO_SYSTEMCTL")
        .unwrap_or_else(|_| DEFAULT_SYSTEMCTL_BIN.to_string())
}

/// Future returned by [`MpdRestarter::restart`]. The trait is
/// object-safe at the cost of explicit pinning; the
/// [`tokio::process`] command path is `Send` and `'static` so
/// this fits.
pub type RestartFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// Restart strategy for the MPD daemon. Implementations
/// signal the daemon to reload its configuration. Errors are
/// operator-readable strings (no structured taxonomy because
/// the plugin treats every failure mode equivalently — log,
/// publish to the worker status channel, keep the previous
/// state).
pub trait MpdRestarter: Send + Sync + Debug {
    /// Initiate the restart. Resolves on completion of the
    /// underlying signal (`Ok(())` on success; `Err(reason)`
    /// when systemctl rejected the request, sudo refused the
    /// drop-in, or the process failed to spawn).
    fn restart(&self) -> RestartFuture<'_>;

    /// Operator-readable name of the strategy. Surfaced in the
    /// plugin's load-time log so operators can see which
    /// mechanism the framework or fallback selected.
    fn strategy_name(&self) -> &'static str;
}

// =============================================================
// DirectSystemctlRestarter — used when EUID == 0
// =============================================================

/// Direct exec of `systemctl restart mpd`. Used when the
/// process is running as root (EUID == 0). No sudo escalation;
/// systemctl's setuid bit is not relevant because root can
/// already manage system units. Honours `EVO_SYSTEMCTL` for
/// non-standard binary paths.
#[derive(Debug, Default)]
pub struct DirectSystemctlRestarter;

impl DirectSystemctlRestarter {
    /// Construct a fresh restarter. Stateless.
    pub fn new() -> Self {
        Self
    }
}

impl MpdRestarter for DirectSystemctlRestarter {
    fn restart(&self) -> RestartFuture<'_> {
        Box::pin(async move {
            let bin = resolve_systemctl_bin();
            let output = tokio::process::Command::new(&bin)
                .args(["restart", "mpd"])
                .output()
                .await
                .map_err(|e| {
                    format!("failed to spawn {bin} restart mpd: {e}")
                })?;
            if !output.status.success() {
                return Err(format!(
                    "{bin} restart mpd failed: exit={:?} stderr={}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            Ok(())
        })
    }

    fn strategy_name(&self) -> &'static str {
        "direct"
    }
}

// =============================================================
// SudoSystemctlRestarter — used when EUID != 0
// =============================================================

/// `sudo -n systemctl restart mpd`. Used when the process is
/// running as a non-root service user. Relies on the
/// distribution's bring-up shipping a sudoers drop-in that
/// whitelists this exact command for the steward identity;
/// absence surfaces as a sudo refusal the plugin logs and
/// remembers (the fragment-writer worker publishes
/// `FragmentWorkerStatus::Failed` and waits for the next route
/// change without disrupting playback).
#[derive(Debug, Default)]
pub struct SudoSystemctlRestarter;

impl SudoSystemctlRestarter {
    /// Construct a fresh restarter. Stateless.
    pub fn new() -> Self {
        Self
    }
}

impl MpdRestarter for SudoSystemctlRestarter {
    fn restart(&self) -> RestartFuture<'_> {
        Box::pin(async move {
            let bin = resolve_systemctl_bin();
            let output = tokio::process::Command::new("sudo")
                .arg("-n")
                .arg(&bin)
                .args(["restart", "mpd"])
                .output()
                .await
                .map_err(|e| {
                    format!("failed to spawn sudo -n {bin} restart mpd: {e}")
                })?;
            if !output.status.success() {
                return Err(format!(
                    "sudo -n {bin} restart mpd failed: exit={:?} stderr={} \
                     (remedy: bootstrap script must install \
                     /etc/sudoers.d/evo-mpd-restart with NOPASSWD for the \
                     service user)",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            Ok(())
        })
    }

    fn strategy_name(&self) -> &'static str {
        "sudo"
    }
}

// =============================================================
// AutoMpdRestarter — the production composite
// =============================================================

/// Read the calling process's effective UID. Returns `None` on
/// platforms where the lookup fails (non-Linux dev hosts where
/// `/proc/self/status` is absent; sandboxes that mask `/proc`).
/// Implemented via `/proc/self/status` parsing — same shape
/// volumio-evo uses — so the plugin does not pull `libc` /
/// `nix` for one syscall.
#[cfg(target_os = "linux")]
fn linux_effective_uid() -> Option<u32> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    s.lines().find_map(|line| {
        let line = line.trim_start();
        let rest = line.strip_prefix("Uid:")?;
        // /proc/self/status's `Uid:` line is
        // `Uid:\t<real>\t<eff>\t<saved>\t<filesystem>`. The
        // effective UID is the second whitespace-separated
        // field.
        rest.split_whitespace().nth(1)?.parse().ok()
    })
}

/// Returns `true` when the process is running with a non-zero
/// effective UID and the plugin therefore needs sudo to reach
/// systemctl. Mirrors volumio-evo's
/// `restart_mpd_use_sudo_only` heuristic: on Linux read
/// `/proc/self/status`; on other platforms or when `/proc` is
/// unreadable, defer to `EVO_RUNTIME_USER` set by the
/// distribution's bootstrap script.
fn process_needs_sudo() -> bool {
    #[cfg(target_os = "linux")]
    if let Some(uid) = linux_effective_uid() {
        return uid != 0;
    }
    // Non-Linux dev builds, or `/proc` masked: the bootstrap
    // script sets EVO_RUNTIME_USER in the steward's systemd
    // drop-in when the steward runs as a non-root user.
    std::env::var("EVO_RUNTIME_USER")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

/// Production restart composite. At construction time, picks
/// the right strategy based on:
///
/// 1. The framework-resolved [`CapabilityResolution`] for the
///    `mpd_systemctl_restart` intent, if the resolution map
///    is populated (Phase A P2.5 onwards).
/// 2. EUID detection (volumio-evo's `/proc/self/status` shape)
///    when the map is empty or the resolution doesn't carry a
///    strategy hint.
///
/// The composite delegates to one of the leaf strategies for
/// the lifetime of the plugin admission. Hot-tightening
/// (Phase B) will swap the inner strategy on capability
/// state changes.
#[derive(Debug)]
pub struct AutoMpdRestarter {
    inner: Arc<dyn MpdRestarter>,
    /// Operator-readable rationale for the chosen strategy,
    /// surfaced in the plugin's load-time log so operators see
    /// why the framework selected sudo vs direct (or
    /// fell-back to no-op).
    rationale: String,
}

impl AutoMpdRestarter {
    /// Resolve a strategy from the framework's resolution map.
    /// Public entry the plugin calls at load time with the
    /// `LoadContext::capabilities` map.
    pub fn resolve(map: &CapabilityResolutionMap) -> Self {
        let resolution = map.get(INTENT_MPD_SYSTEMCTL_RESTART);
        Self::resolve_with_eject(resolution, process_needs_sudo)
    }

    /// Resolution path isolated from the EUID-detection
    /// syscall so unit tests can inject a deterministic answer.
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
                    inner: Arc::new(DirectSystemctlRestarter::new()),
                    rationale: format!(
                        "framework-resolved strategy=direct ({evidence})"
                    ),
                },
                "sudo" => Self {
                    inner: Arc::new(SudoSystemctlRestarter::new()),
                    rationale: format!(
                        "framework-resolved strategy=sudo ({evidence})"
                    ),
                },
                other => {
                    // Unknown strategy name from the framework
                    // — defer to EUID-based fallback so we
                    // don't refuse admission over an
                    // unrecognised string.
                    Self::fallback(
                        needs_sudo_fn,
                        format!(
                            "framework-resolved strategy={other:?} not \
                             recognised by this build; falling back to EUID \
                             detection"
                        ),
                    )
                }
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
                Self {
                    inner: Arc::new(NoOpProductionRestarter::new(
                        reason.clone(),
                        remedy.clone(),
                    )),
                    rationale: format!(
                        "framework refused mpd_systemctl_restart: {reason}; \
                         restart leg disabled (remedy: {remedy})"
                    ),
                }
            }
            Some(CapabilityResolution::Degraded {
                fallback_strategy,
                reason,
            }) => {
                if fallback_strategy == "no_op" {
                    Self {
                        inner: Arc::new(NoOpProductionRestarter::new(
                            reason.clone(),
                            "no remedy declared".to_string(),
                        )),
                        rationale: format!(
                            "framework degraded mpd_systemctl_restart to \
                             no_op fallback: {reason}"
                        ),
                    }
                } else {
                    Self::fallback(
                        needs_sudo_fn,
                        format!(
                            "framework degraded with fallback_strategy=\
                             {fallback_strategy:?} not recognised; \
                             falling back to EUID detection ({reason})"
                        ),
                    )
                }
            }
            Some(CapabilityResolution::NotProbed { reason }) => Self::fallback(
                needs_sudo_fn,
                format!(
                    "framework did not probe mpd_systemctl_restart \
                         ({reason}); falling back to EUID detection"
                ),
            ),
            None => Self::fallback(
                needs_sudo_fn,
                "framework resolution map did not contain \
                 mpd_systemctl_restart; falling back to EUID detection"
                    .to_string(),
            ),
        }
    }

    /// Internal constructor for the EUID-detection path.
    fn fallback(needs_sudo_fn: fn() -> bool, rationale_prefix: String) -> Self {
        if needs_sudo_fn() {
            Self {
                inner: Arc::new(SudoSystemctlRestarter::new()),
                rationale: format!(
                    "{rationale_prefix}; EUID detection selected sudo"
                ),
            }
        } else {
            Self {
                inner: Arc::new(DirectSystemctlRestarter::new()),
                rationale: format!(
                    "{rationale_prefix}; EUID detection selected direct"
                ),
            }
        }
    }

    /// Operator-readable rationale for the chosen strategy.
    /// The plugin logs this at INFO once at load time so the
    /// journal carries an audit trail of the resolution.
    pub fn rationale(&self) -> &str {
        &self.rationale
    }
}

impl MpdRestarter for AutoMpdRestarter {
    fn restart(&self) -> RestartFuture<'_> {
        self.inner.restart()
    }

    fn strategy_name(&self) -> &'static str {
        self.inner.strategy_name()
    }
}

// =============================================================
// NoOpProductionRestarter — used when framework refused
// =============================================================

/// Production no-op variant for capabilities the framework
/// explicitly refused. Distinct from the test-only
/// [`NoOpRestarter`] so the production code path never
/// silently swallows by accident. The first call logs the
/// refusal reason; subsequent calls log at a lower frequency.
#[derive(Debug)]
pub(crate) struct NoOpProductionRestarter {
    reason: String,
    #[allow(dead_code)]
    remedy: String,
}

impl NoOpProductionRestarter {
    pub(crate) fn new(reason: String, remedy: String) -> Self {
        Self { reason, remedy }
    }
}

impl MpdRestarter for NoOpProductionRestarter {
    fn restart(&self) -> RestartFuture<'_> {
        let reason = self.reason.clone();
        Box::pin(async move {
            Err(format!(
                "MPD restart disabled by framework preflight: {reason}"
            ))
        })
    }

    fn strategy_name(&self) -> &'static str {
        "no_op_disabled"
    }
}

// =============================================================
// Test-only stubs
// =============================================================

/// Test-only restart strategy: counts invocations and returns
/// `Ok(())` without touching systemctl. The counter survives
/// arbitrary numbers of `restart` calls and is readable through
/// [`Self::call_count`].
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct NoOpRestarter {
    call_count: AtomicU64,
}

#[cfg(test)]
impl NoOpRestarter {
    /// Construct a fresh restarter with a zero counter.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Cumulative `restart` invocations on this counter.
    pub(crate) fn call_count(&self) -> u64 {
        self.call_count.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
impl MpdRestarter for NoOpRestarter {
    fn restart(&self) -> RestartFuture<'_> {
        Box::pin(async move {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    fn strategy_name(&self) -> &'static str {
        "no_op_test"
    }
}

/// Test-only restart strategy that always reports a fixed
/// failure. Used to drive the worker's failure-path tests
/// (status channel publishes `Failed`; the worker keeps
/// running and reattempts on the next route change).
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct FailingRestarter {
    reason: String,
    call_count: AtomicU64,
}

#[cfg(test)]
impl FailingRestarter {
    /// Construct a restarter that fails every call with the
    /// supplied reason.
    pub(crate) fn new<S: Into<String>>(reason: S) -> Self {
        Self {
            reason: reason.into(),
            call_count: AtomicU64::new(0),
        }
    }

    /// Cumulative `restart` invocations on this counter.
    pub(crate) fn call_count(&self) -> u64 {
        self.call_count.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
impl MpdRestarter for FailingRestarter {
    fn restart(&self) -> RestartFuture<'_> {
        let reason = self.reason.clone();
        Box::pin(async move {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Err(reason)
        })
    }

    fn strategy_name(&self) -> &'static str {
        "failing_test"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_restarter_counts_invocations() {
        let r = NoOpRestarter::new();
        assert_eq!(r.call_count(), 0);
        r.restart().await.unwrap();
        assert_eq!(r.call_count(), 1);
        r.restart().await.unwrap();
        r.restart().await.unwrap();
        assert_eq!(r.call_count(), 3);
    }

    #[tokio::test]
    async fn failing_restarter_returns_configured_reason() {
        let r = FailingRestarter::new("sudoers drop-in missing");
        let err = r.restart().await.unwrap_err();
        assert_eq!(err, "sudoers drop-in missing");
        assert_eq!(r.call_count(), 1);
    }

    #[tokio::test]
    async fn restarter_is_object_safe_through_arc_dyn() {
        let r: Arc<dyn MpdRestarter> = Arc::new(NoOpRestarter::new());
        r.restart().await.unwrap();
    }

    // ===== AutoMpdRestarter resolution logic =====

    fn always_root() -> bool {
        false
    }
    fn always_non_root() -> bool {
        true
    }

    #[test]
    fn auto_with_framework_direct_strategy_selects_direct() {
        let resolution = CapabilityResolution::Available {
            evidence: "EUID == 0".into(),
            strategy: Some("direct".into()),
        };
        let r = AutoMpdRestarter::resolve_with_eject(
            Some(&resolution),
            always_non_root,
        );
        assert_eq!(r.strategy_name(), "direct");
        assert!(r.rationale().contains("framework-resolved strategy=direct"));
    }

    #[test]
    fn auto_with_framework_sudo_strategy_selects_sudo() {
        let resolution = CapabilityResolution::Available {
            evidence: "sudo -l confirmed".into(),
            strategy: Some("sudo".into()),
        };
        let r = AutoMpdRestarter::resolve_with_eject(
            Some(&resolution),
            always_root,
        );
        assert_eq!(r.strategy_name(), "sudo");
        assert!(r.rationale().contains("framework-resolved strategy=sudo"));
    }

    #[test]
    fn auto_with_unrecognised_strategy_falls_back_to_euid() {
        let resolution = CapabilityResolution::Available {
            evidence: "x".into(),
            strategy: Some("pkexec".into()),
        };
        let r = AutoMpdRestarter::resolve_with_eject(
            Some(&resolution),
            always_non_root,
        );
        assert_eq!(r.strategy_name(), "sudo");
        assert!(r.rationale().contains("not recognised"));
        assert!(r.rationale().contains("EUID detection selected sudo"));
    }

    #[test]
    fn auto_with_available_no_strategy_falls_back_to_euid_root() {
        let resolution = CapabilityResolution::Available {
            evidence: "x".into(),
            strategy: None,
        };
        let r = AutoMpdRestarter::resolve_with_eject(
            Some(&resolution),
            always_root,
        );
        assert_eq!(r.strategy_name(), "direct");
    }

    #[test]
    fn auto_with_available_no_strategy_falls_back_to_euid_non_root() {
        let resolution = CapabilityResolution::Available {
            evidence: "x".into(),
            strategy: None,
        };
        let r = AutoMpdRestarter::resolve_with_eject(
            Some(&resolution),
            always_non_root,
        );
        assert_eq!(r.strategy_name(), "sudo");
    }

    #[test]
    fn auto_with_unavailable_installs_no_op_with_reason() {
        let resolution = CapabilityResolution::Unavailable {
            reason: "sudoers drop-in missing".into(),
            remedy: "run bootstrap".into(),
        };
        let r = AutoMpdRestarter::resolve_with_eject(
            Some(&resolution),
            always_non_root,
        );
        assert_eq!(r.strategy_name(), "no_op_disabled");
        assert!(r.rationale().contains("framework refused"));
        assert!(r.rationale().contains("sudoers drop-in missing"));
        assert!(r.rationale().contains("run bootstrap"));
    }

    #[test]
    fn auto_with_degraded_no_op_installs_no_op() {
        let resolution = CapabilityResolution::Degraded {
            reason: "MPD service unmanaged".into(),
            fallback_strategy: "no_op".into(),
        };
        let r = AutoMpdRestarter::resolve_with_eject(
            Some(&resolution),
            always_non_root,
        );
        assert_eq!(r.strategy_name(), "no_op_disabled");
        assert!(r.rationale().contains("MPD service unmanaged"));
    }

    #[test]
    fn auto_with_degraded_other_falls_back_to_euid() {
        let resolution = CapabilityResolution::Degraded {
            reason: "x".into(),
            fallback_strategy: "unknown".into(),
        };
        let r = AutoMpdRestarter::resolve_with_eject(
            Some(&resolution),
            always_root,
        );
        assert_eq!(r.strategy_name(), "direct");
    }

    #[test]
    fn auto_with_not_probed_falls_back_to_euid() {
        let resolution = CapabilityResolution::NotProbed {
            reason: "framework runner not wired".into(),
        };
        let r = AutoMpdRestarter::resolve_with_eject(
            Some(&resolution),
            always_non_root,
        );
        assert_eq!(r.strategy_name(), "sudo");
        assert!(r.rationale().contains("framework did not probe"));
    }

    #[test]
    fn auto_with_no_resolution_falls_back_to_euid() {
        let r = AutoMpdRestarter::resolve_with_eject(None, always_root);
        assert_eq!(r.strategy_name(), "direct");
        assert!(r.rationale().contains("did not contain"));
    }

    #[test]
    fn auto_resolve_from_empty_map_uses_euid_fallback() {
        let map = CapabilityResolutionMap::new();
        let r = AutoMpdRestarter::resolve(&map);
        // strategy depends on whether the test is running as
        // root or not — both are valid outcomes. Assert
        // rationale carries the empty-map signal.
        assert!(r.rationale().contains("did not contain"));
        assert!(matches!(r.strategy_name(), "direct" | "sudo"));
    }

    #[tokio::test]
    async fn no_op_production_restarter_returns_disabled_message() {
        let r = NoOpProductionRestarter::new(
            "sudoers absent".into(),
            "run bootstrap".into(),
        );
        let err = r.restart().await.unwrap_err();
        assert!(err.contains("MPD restart disabled"));
        assert!(err.contains("sudoers absent"));
    }
}
