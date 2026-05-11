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

/// Capability-intent id the framework's preflight associates
/// with the fragment-write leg. The fragment-writer worker
/// observes this resolution via `LoadContext::capabilities` —
/// when the framework reports the path is not writable, the
/// worker publishes `FragmentWorkerStatus::Failed` instead of
/// emitting writes that would fail at runtime.
pub const INTENT_MPD_FRAGMENT_WRITE: &str = "mpd_fragment_write";

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
pub(crate) fn process_needs_sudo() -> bool {
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
/// The composite delegates to one of the leaf strategies. The
/// inner strategy is held behind a [`Mutex`] so the plugin's
/// capabilities-watch reactor can swap it on PPAG resolution
/// updates without re-admission (hot-tightening, Phase B).
#[derive(Debug)]
pub struct AutoMpdRestarter {
    inner: std::sync::Mutex<Arc<dyn MpdRestarter>>,
    /// Operator-readable rationale for the chosen strategy,
    /// surfaced in the plugin's load-time log so operators see
    /// why the framework selected sudo vs direct (or
    /// fell-back to no-op). Behind the same mutex shape as
    /// [`Self::inner`] so a re-resolve atomically updates
    /// both.
    rationale: std::sync::Mutex<String>,
}

impl AutoMpdRestarter {
    /// Resolve a strategy from the framework's resolution map.
    /// Public entry the plugin calls at load time with the
    /// `LoadContext::capabilities` map.
    pub fn resolve(map: &CapabilityResolutionMap) -> Self {
        let resolution = map.get(INTENT_MPD_SYSTEMCTL_RESTART);
        let (inner, rationale) =
            resolve_inner_and_rationale(resolution, process_needs_sudo);
        Self {
            inner: std::sync::Mutex::new(inner),
            rationale: std::sync::Mutex::new(rationale),
        }
    }

    /// Re-resolve from a new capability resolution map and
    /// swap the inner strategy + rationale in place. Called by
    /// the plugin's capabilities-watch reactor when the
    /// framework's re-probe loop publishes a change. Lock
    /// scope is tiny (one Arc swap + one String swap); the
    /// next `restart()` observes the new strategy.
    pub fn re_resolve(&self, map: &CapabilityResolutionMap) {
        let resolution = map.get(INTENT_MPD_SYSTEMCTL_RESTART);
        let (new_inner, new_rationale) =
            resolve_inner_and_rationale(resolution, process_needs_sudo);
        *self
            .inner
            .lock()
            .expect("AutoMpdRestarter inner mutex poisoned") = new_inner;
        *self
            .rationale
            .lock()
            .expect("AutoMpdRestarter rationale mutex poisoned") =
            new_rationale;
    }

    /// Resolution path isolated from the EUID-detection
    /// syscall so unit tests can inject a deterministic answer.
    /// `needs_sudo_fn` is consulted only when the framework
    /// resolution does not carry a strategy hint.
    #[cfg(test)]
    fn resolve_with_eject(
        resolution: Option<&CapabilityResolution>,
        needs_sudo_fn: fn() -> bool,
    ) -> Self {
        let (inner, rationale) =
            resolve_inner_and_rationale(resolution, needs_sudo_fn);
        Self {
            inner: std::sync::Mutex::new(inner),
            rationale: std::sync::Mutex::new(rationale),
        }
    }

    /// Operator-readable rationale for the chosen strategy.
    /// The plugin logs this at INFO once at load time so the
    /// journal carries an audit trail of the resolution. Each
    /// call returns a fresh clone — the mutex scope stays
    /// tight and the caller never holds a reference into the
    /// lock.
    pub fn rationale(&self) -> String {
        self.rationale
            .lock()
            .expect("AutoMpdRestarter rationale mutex poisoned")
            .clone()
    }

    /// Current strategy name. Forwards to the inner leaf
    /// strategy's `strategy_name()`, dropping the lock before
    /// returning.
    pub fn current_strategy_name(&self) -> &'static str {
        self.inner
            .lock()
            .expect("AutoMpdRestarter inner mutex poisoned")
            .strategy_name()
    }
}

/// Pure resolver returning the leaf strategy + rationale for a
/// given resolution. Shared between `resolve`, `resolve_with_eject`
/// (test path), and `re_resolve` so a single source of truth
/// drives the policy.
fn resolve_inner_and_rationale(
    resolution: Option<&CapabilityResolution>,
    needs_sudo_fn: fn() -> bool,
) -> (Arc<dyn MpdRestarter>, String) {
    match resolution {
        Some(CapabilityResolution::Available {
            strategy: Some(s),
            evidence,
        }) => match s.as_str() {
            "direct" => (
                Arc::new(DirectSystemctlRestarter::new()),
                format!("framework-resolved strategy=direct ({evidence})"),
            ),
            "sudo" => (
                Arc::new(SudoSystemctlRestarter::new()),
                format!("framework-resolved strategy=sudo ({evidence})"),
            ),
            other => fallback_inner_and_rationale(
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
        }) => fallback_inner_and_rationale(
            needs_sudo_fn,
            format!(
                "framework resolution Available without strategy hint \
                 ({evidence}); falling back to EUID detection"
            ),
        ),
        Some(CapabilityResolution::Unavailable { reason, remedy }) => (
            Arc::new(NoOpProductionRestarter::new(
                reason.clone(),
                remedy.clone(),
            )),
            format!(
                "framework refused mpd_systemctl_restart: {reason}; \
                 restart leg disabled (remedy: {remedy})"
            ),
        ),
        Some(CapabilityResolution::Degraded {
            fallback_strategy,
            reason,
        }) => {
            if fallback_strategy == "no_op" {
                (
                    Arc::new(NoOpProductionRestarter::new(
                        reason.clone(),
                        "no remedy declared".to_string(),
                    )),
                    format!(
                        "framework degraded mpd_systemctl_restart to \
                         no_op fallback: {reason}"
                    ),
                )
            } else {
                fallback_inner_and_rationale(
                    needs_sudo_fn,
                    format!(
                        "framework degraded with fallback_strategy=\
                         {fallback_strategy:?} not recognised; \
                         falling back to EUID detection ({reason})"
                    ),
                )
            }
        }
        Some(CapabilityResolution::NotProbed { reason }) => {
            fallback_inner_and_rationale(
                needs_sudo_fn,
                format!(
                    "framework did not probe mpd_systemctl_restart \
                         ({reason}); falling back to EUID detection"
                ),
            )
        }
        None => fallback_inner_and_rationale(
            needs_sudo_fn,
            "framework resolution map did not contain \
             mpd_systemctl_restart; falling back to EUID detection"
                .to_string(),
        ),
    }
}

fn fallback_inner_and_rationale(
    needs_sudo_fn: fn() -> bool,
    rationale_prefix: String,
) -> (Arc<dyn MpdRestarter>, String) {
    if needs_sudo_fn() {
        (
            Arc::new(SudoSystemctlRestarter::new()),
            format!("{rationale_prefix}; EUID detection selected sudo"),
        )
    } else {
        (
            Arc::new(DirectSystemctlRestarter::new()),
            format!("{rationale_prefix}; EUID detection selected direct"),
        )
    }
}

impl MpdRestarter for AutoMpdRestarter {
    fn restart(&self) -> RestartFuture<'_> {
        // Clone the inner Arc under the lock, drop the lock,
        // and call through the clone. The await scope sees no
        // mutex guard so a parallel `re_resolve` does not block
        // and the future's send semantics stay intact.
        let inner = self
            .inner
            .lock()
            .expect("AutoMpdRestarter inner mutex poisoned")
            .clone();
        Box::pin(async move { inner.restart().await })
    }

    fn strategy_name(&self) -> &'static str {
        self.current_strategy_name()
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

    // ===== AutoMpdRestarter::re_resolve =====
    // Hot-tightening: when the framework's re-probe loop
    // publishes an updated resolution map, the composite
    // should swap its inner strategy in place without
    // requiring re-construction.

    fn map_with_strategy(
        intent: &str,
        strategy: &str,
    ) -> CapabilityResolutionMap {
        let mut map = CapabilityResolutionMap::new();
        map.insert(
            intent.to_string(),
            CapabilityResolution::Available {
                evidence: format!("test fixture: {strategy}"),
                strategy: Some(strategy.into()),
            },
        );
        map
    }

    fn map_with_unavailable(intent: &str) -> CapabilityResolutionMap {
        let mut map = CapabilityResolutionMap::new();
        map.insert(
            intent.to_string(),
            CapabilityResolution::Unavailable {
                reason: "test: drop-in removed".into(),
                remedy: "reinstall the drop-in".into(),
            },
        );
        map
    }

    #[test]
    fn re_resolve_swaps_sudo_to_direct() {
        let initial = map_with_strategy(INTENT_MPD_SYSTEMCTL_RESTART, "sudo");
        let auto = AutoMpdRestarter::resolve(&initial);
        assert_eq!(auto.current_strategy_name(), "sudo");
        let updated = map_with_strategy(INTENT_MPD_SYSTEMCTL_RESTART, "direct");
        auto.re_resolve(&updated);
        assert_eq!(auto.current_strategy_name(), "direct");
        assert!(auto.rationale().contains("direct"));
    }

    #[test]
    fn re_resolve_swaps_direct_to_sudo() {
        let initial = map_with_strategy(INTENT_MPD_SYSTEMCTL_RESTART, "direct");
        let auto = AutoMpdRestarter::resolve(&initial);
        assert_eq!(auto.current_strategy_name(), "direct");
        let updated = map_with_strategy(INTENT_MPD_SYSTEMCTL_RESTART, "sudo");
        auto.re_resolve(&updated);
        assert_eq!(auto.current_strategy_name(), "sudo");
        assert!(auto.rationale().contains("sudo"));
    }

    #[test]
    fn re_resolve_to_unavailable_installs_no_op_with_remedy() {
        let initial = map_with_strategy(INTENT_MPD_SYSTEMCTL_RESTART, "sudo");
        let auto = AutoMpdRestarter::resolve(&initial);
        let updated = map_with_unavailable(INTENT_MPD_SYSTEMCTL_RESTART);
        auto.re_resolve(&updated);
        // No-op variant identifies itself as "no_op_disabled".
        assert_eq!(auto.current_strategy_name(), "no_op_disabled");
        assert!(auto.rationale().contains("framework refused"));
        assert!(auto.rationale().contains("reinstall the drop-in"));
    }

    #[test]
    fn re_resolve_concurrent_with_restart_does_not_deadlock() {
        // Smoke check: the Mutex<inner> swap is short enough
        // that overlapping a restart future construction with
        // a re_resolve never deadlocks. We construct the auto
        // restarter, take an async future from restart(),
        // re_resolve mid-flight, and verify the future still
        // completes against the cloned inner.
        let initial = map_with_strategy(INTENT_MPD_SYSTEMCTL_RESTART, "sudo");
        let auto = AutoMpdRestarter::resolve(&initial);
        // Construct future before re-resolve.
        let fut = MpdRestarter::restart(&auto);
        // Swap the inner under us.
        let updated = map_with_unavailable(INTENT_MPD_SYSTEMCTL_RESTART);
        auto.re_resolve(&updated);
        // The captured future used the previous inner; drop
        // it without awaiting to avoid the real sudo call.
        // Just make sure nothing panicked.
        drop(fut);
        assert_eq!(auto.current_strategy_name(), "no_op_disabled");
    }
}
