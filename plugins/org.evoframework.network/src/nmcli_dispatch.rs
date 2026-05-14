// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Privilege-strategy abstraction for the network plugin's
//! external-binary invocations.
//!
//! The network plane reaches the host through four privileged
//! tools: `nmcli` (NetworkManager state), `iw` (PHY capability
//! detection + virtual AP vif lifecycle), `rfkill` (hardware /
//! software radio block toggle), and `curl` (connectivity +
//! captive-portal probes). Under root the plugin can exec these
//! directly; under a non-root service identity it must dispatch
//! via `sudo -n <bin> ...` against narrow NOPASSWD drop-ins
//! shipped by the distribution's bootstrap. This module isolates
//! the choice behind one trait so call sites stay strategy-
//! agnostic.
//!
//! ## Strategy selection
//!
//! [`AutoPrivilegedExec::resolve`] consumes
//! [`LoadContext::capabilities`] at plugin load time. The
//! framework's Privilege Preflight Admission Gate (PPAG) probes
//! the host's reachability for each capability intent against the
//! calling identity and stamps the resolution on the map under
//! the appropriate intent id; the composite reads that map,
//! installs the matching leaf strategy, and records the
//! operator-readable rationale for the load-time log line.
//!
//! Fallback (when the map is empty — legacy admission paths that
//! have not been wired to the runner yet): `/proc/self/status`
//! EUID detection. Root → direct; non-root → sudo. Identical
//! mechanism to the `playback.mpd` plugin's
//! `AutoMpdRestarter::process_needs_sudo`, intentionally so that
//! the PPAG consumers inside this distribution observe the same
//! EUID floor when the runner is silent.
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

/// Capability-intent id for `nmcli` reachability under the
/// framework's PPAG runner.
pub const INTENT_NMCLI_INVOCATION: &str = "nmcli_invocation";

/// Capability-intent id for `iw` reachability — used by the
/// `wifi_phy` helpers (PHY capability detection + virtual AP
/// vif lifecycle).
pub const INTENT_IW_INVOCATION: &str = "iw_invocation";

/// Capability-intent id for `rfkill` reachability — used to
/// toggle hardware / software radio block from the runtime
/// supervisor + flight-mode plumbing.
pub const INTENT_RFKILL_INVOCATION: &str = "rfkill_invocation";

// `INTENT_CURL_INVOCATION` retired per the connectivity-check redesign: routine
// connectivity probes are read-only GETs that need no elevation
// and run via direct exec. A future captive-portal
// credential-submission flow will declare its own
// `captive_portal_credential_submit` capability intent at that
// time, with the elevation rationale recorded in the manifest.

/// Future returned by [`PrivilegedExec::dispatch`]. The trait is
/// object-safe at the cost of explicit pinning; the
/// [`tokio::process`] command path is `Send` and `'static` so
/// this fits.
pub type DispatchFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Output, PluginError>> + Send + 'a>>;

/// Dispatch strategy for one privileged binary invocation.
/// Implementations decide whether to exec the supplied binary
/// directly or via `sudo -n`; the caller passes the binary path,
/// args, and timeout at every call so tests can swap the binary
/// to a mock script without rebuilding the dispatcher.
pub trait PrivilegedExec: Send + Sync + Debug {
    /// Dispatch `bin args...`. `bin` is the resolved binary path
    /// (e.g. `PluginConfig.nmcli_path` / `iw_path` / `rfkill_path`
    /// / `curl_path`); the dispatcher chooses the invocation
    /// shape.
    fn dispatch<'a>(
        &'a self,
        bin: &'a str,
        args: &'a [&'a str],
        timeout: Duration,
    ) -> DispatchFuture<'a>;

    /// Strategy name surfaced in load-time logs ("direct" or
    /// "sudo"). The composite [`AutoPrivilegedExec`] forwards
    /// to its inner strategy.
    fn strategy_name(&self) -> &'static str;
}

/// Back-compat type alias — older call sites in this plugin and
/// in tests reference `NmcliDispatcher`; the renamed trait keeps
/// the same surface shape, so the alias works without change.
pub type NmcliDispatcher = dyn PrivilegedExec;

/// Direct exec of the supplied binary. Used when the process is
/// running as root (EUID == 0) or when the framework's preflight
/// has confirmed that the calling identity can run the binary
/// without escalation.
#[derive(Debug, Default)]
pub struct DirectExec;

impl DirectExec {
    /// Construct a fresh dispatcher. Stateless.
    pub fn new() -> Self {
        Self
    }
}

/// Back-compat alias for the renamed [`DirectExec`] type.
pub type DirectNmcliDispatcher = DirectExec;

impl PrivilegedExec for DirectExec {
    fn dispatch<'a>(
        &'a self,
        bin: &'a str,
        args: &'a [&'a str],
        timeout: Duration,
    ) -> DispatchFuture<'a> {
        Box::pin(async move {
            let mut cmd = Command::new(bin);
            cmd.args(args);
            run_with_timeout(cmd, timeout, "direct").await
        })
    }

    fn strategy_name(&self) -> &'static str {
        "direct"
    }
}

/// `sudo -n <bin> ...` exec. Used when the process is running as
/// a non-root service identity. Relies on the distribution's
/// bootstrap shipping a sudoers drop-in that whitelists the
/// binary for the service user; absence surfaces as a sudo
/// refusal the caller sees as a [`PluginError::Transient`].
#[derive(Debug, Default)]
pub struct SudoExec;

impl SudoExec {
    /// Construct a fresh dispatcher. Stateless.
    pub fn new() -> Self {
        Self
    }
}

/// Back-compat alias for the renamed [`SudoExec`] type.
pub type SudoNmcliDispatcher = SudoExec;

impl PrivilegedExec for SudoExec {
    fn dispatch<'a>(
        &'a self,
        bin: &'a str,
        args: &'a [&'a str],
        timeout: Duration,
    ) -> DispatchFuture<'a> {
        Box::pin(async move {
            let mut cmd = Command::new("sudo");
            cmd.arg("-n").arg(bin).args(args);
            run_with_timeout(cmd, timeout, "sudo -n").await
        })
    }

    fn strategy_name(&self) -> &'static str {
        "sudo"
    }
}

/// Production dispatch composite. Reads the framework's
/// [`CapabilityResolution`] for the intent id supplied at
/// construction time and installs the matching leaf strategy.
/// Falls back to EUID detection when the map is silent.
pub struct AutoPrivilegedExec {
    /// Capability-intent id this dispatcher tracks (e.g.
    /// `nmcli_invocation`, `iw_invocation`). Lifetime is
    /// `'static` so the dispatcher can be passed around without
    /// borrow-checker concerns.
    intent_id: &'static str,
    /// Inner leaf strategy. Held behind a `Mutex` so the
    /// plugin's capabilities-watch reactor (PPAG hot-tightening)
    /// can swap it on framework re-probe publications.
    inner: std::sync::Mutex<Arc<dyn PrivilegedExec>>,
    /// Operator-readable rationale for the current strategy,
    /// kept in lockstep with [`Self::inner`] under the same
    /// mutex discipline.
    rationale: std::sync::Mutex<String>,
}

/// Back-compat alias for the renamed [`AutoPrivilegedExec`].
pub type AutoNmcliDispatcher = AutoPrivilegedExec;

impl Debug for AutoPrivilegedExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutoPrivilegedExec")
            .field("intent_id", &self.intent_id)
            .field("strategy", &self.current_strategy_name())
            .field("rationale", &self.rationale())
            .finish()
    }
}

impl AutoPrivilegedExec {
    /// Resolve a strategy from the framework's resolution map.
    /// Plugin entry: pass the intent id + `&ctx.capabilities` at
    /// load time.
    pub fn resolve(
        intent_id: &'static str,
        map: &CapabilityResolutionMap,
    ) -> Self {
        let resolution = map.get(intent_id);
        let (inner, rationale) = resolve_inner_and_rationale(
            intent_id,
            resolution,
            process_needs_sudo,
        );
        Self {
            intent_id,
            inner: std::sync::Mutex::new(inner),
            rationale: std::sync::Mutex::new(rationale),
        }
    }

    /// Re-resolve from a new capability resolution map and swap
    /// the inner strategy + rationale in place. Called by the
    /// plugin's capabilities-watch reactor when the framework's
    /// re-probe loop publishes a change.
    pub fn re_resolve(&self, map: &CapabilityResolutionMap) {
        let resolution = map.get(self.intent_id);
        let (new_inner, new_rationale) = resolve_inner_and_rationale(
            self.intent_id,
            resolution,
            process_needs_sudo,
        );
        *self
            .inner
            .lock()
            .expect("AutoPrivilegedExec inner mutex poisoned") = new_inner;
        *self
            .rationale
            .lock()
            .expect("AutoPrivilegedExec rationale mutex poisoned") =
            new_rationale;
    }

    /// Resolution path isolated from the EUID-detection syscall
    /// so unit tests can inject a deterministic answer.
    /// `needs_sudo_fn` is consulted only when the framework
    /// resolution does not carry a strategy hint.
    #[cfg(test)]
    fn resolve_with_eject(
        intent_id: &'static str,
        resolution: Option<&CapabilityResolution>,
        needs_sudo_fn: fn() -> bool,
    ) -> Self {
        let (inner, rationale) =
            resolve_inner_and_rationale(intent_id, resolution, needs_sudo_fn);
        Self {
            intent_id,
            inner: std::sync::Mutex::new(inner),
            rationale: std::sync::Mutex::new(rationale),
        }
    }

    /// Intent id this dispatcher tracks. Useful for log lines
    /// and diagnostics so operators can tell which capability
    /// resolution a dispatcher reflects.
    pub fn intent_id(&self) -> &'static str {
        self.intent_id
    }

    /// Operator-readable rationale for the current strategy.
    /// Cloned out under the mutex so callers never hold a
    /// reference into the lock.
    pub fn rationale(&self) -> String {
        self.rationale
            .lock()
            .expect("AutoPrivilegedExec rationale mutex poisoned")
            .clone()
    }

    /// Current leaf strategy name. Forwards to the inner
    /// strategy's `strategy_name()`, dropping the lock before
    /// returning the `'static` string.
    pub fn current_strategy_name(&self) -> &'static str {
        self.inner
            .lock()
            .expect("AutoPrivilegedExec inner mutex poisoned")
            .strategy_name()
    }
}

/// Pure resolver returning the leaf strategy + rationale for a
/// given resolution. Shared between `resolve`, `resolve_with_eject`
/// (test path), and `re_resolve` so the policy lives in one place.
/// `intent_id` flows into the rationale strings so operators can
/// see which intent a refusal / fallback relates to.
fn resolve_inner_and_rationale(
    intent_id: &'static str,
    resolution: Option<&CapabilityResolution>,
    needs_sudo_fn: fn() -> bool,
) -> (Arc<dyn PrivilegedExec>, String) {
    match resolution {
        Some(CapabilityResolution::Available {
            strategy: Some(s),
            evidence,
        }) => match s.as_str() {
            "direct" => (
                Arc::new(DirectExec::new()),
                format!(
                    "framework-resolved {intent_id} strategy=direct \
                     ({evidence})"
                ),
            ),
            "sudo" => (
                Arc::new(SudoExec::new()),
                format!(
                    "framework-resolved {intent_id} strategy=sudo \
                     ({evidence})"
                ),
            ),
            other => fallback_inner_and_rationale(
                needs_sudo_fn,
                format!(
                    "framework-resolved {intent_id} strategy={other:?} \
                     not recognised by this build; falling back to EUID \
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
                "framework resolution for {intent_id} Available without \
                 strategy hint ({evidence}); falling back to EUID detection"
            ),
        ),
        Some(CapabilityResolution::Unavailable { reason, remedy }) => (
            Arc::new(DirectExec::new()),
            format!(
                "framework refused {intent_id}: {reason}; \
                 attempting direct exec anyway (remedy: {remedy})"
            ),
        ),
        Some(CapabilityResolution::Degraded {
            fallback_strategy,
            reason,
        }) => match fallback_strategy.as_str() {
            "direct" => (
                Arc::new(DirectExec::new()),
                format!(
                    "framework degraded {intent_id} to direct fallback: \
                     {reason}"
                ),
            ),
            "sudo" => (
                Arc::new(SudoExec::new()),
                format!(
                    "framework degraded {intent_id} to sudo fallback: \
                     {reason}"
                ),
            ),
            other => fallback_inner_and_rationale(
                needs_sudo_fn,
                format!(
                    "framework degraded {intent_id} with unrecognised \
                     fallback={other:?}: {reason}; falling back to EUID \
                     detection"
                ),
            ),
        },
        Some(CapabilityResolution::NotProbed { reason }) => {
            fallback_inner_and_rationale(
                needs_sudo_fn,
                format!(
                    "framework did not probe {intent_id} ({reason}); \
                     falling back to EUID detection"
                ),
            )
        }
        None => fallback_inner_and_rationale(
            needs_sudo_fn,
            format!(
                "framework resolution map did not contain {intent_id}; \
                 falling back to EUID detection"
            ),
        ),
    }
}

fn fallback_inner_and_rationale(
    needs_sudo_fn: fn() -> bool,
    reason_prefix: String,
) -> (Arc<dyn PrivilegedExec>, String) {
    let needs_sudo = needs_sudo_fn();
    let (inner, strategy): (Arc<dyn PrivilegedExec>, &'static str) =
        if needs_sudo {
            (Arc::new(SudoExec::new()), "sudo")
        } else {
            (Arc::new(DirectExec::new()), "direct")
        };
    (
        inner,
        format!("{reason_prefix}; EUID detection selected {strategy}"),
    )
}

impl PrivilegedExec for AutoPrivilegedExec {
    fn dispatch<'a>(
        &'a self,
        bin: &'a str,
        args: &'a [&'a str],
        timeout: Duration,
    ) -> DispatchFuture<'a> {
        // Clone the inner Arc under the lock, drop the lock,
        // then run the dispatch through the clone. The async
        // block doesn't borrow from `self` once `inner` is
        // captured, so a parallel `re_resolve` never races
        // with an in-flight invocation.
        let inner = self
            .inner
            .lock()
            .expect("AutoPrivilegedExec inner mutex poisoned")
            .clone();
        Box::pin(async move { inner.dispatch(bin, args, timeout).await })
    }

    fn strategy_name(&self) -> &'static str {
        self.current_strategy_name()
    }
}

/// Read the calling process's effective UID. Returns `None` on
/// platforms where the lookup fails (non-Linux dev hosts where
/// `/proc/self/status` is absent; sandboxes that mask `/proc`).
/// Implemented via `/proc/self/status` parsing so the plugin
/// does not pull `libc` / `nix` for one syscall — mirrors the
/// `playback.mpd` plugin's shape so the PPAG consumers share
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

/// Returns `true` when the process needs `sudo -n` to reach a
/// privileged binary. Linux: `/proc/self/status` EUID parse.
/// Other platforms: `EVO_RUNTIME_USER` env-var set by the
/// distribution's bootstrap when the steward runs as a non-root
/// user.
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
        let auto = AutoPrivilegedExec::resolve_with_eject(
            INTENT_NMCLI_INVOCATION,
            Some(&resolution),
            always_root,
        );
        assert_eq!(auto.strategy_name(), "sudo");
        assert!(auto
            .rationale()
            .contains("framework-resolved nmcli_invocation strategy=sudo"));
    }

    #[test]
    fn resolve_with_available_strategy_direct() {
        let resolution = CapabilityResolution::Available {
            evidence: "binary present: /usr/bin/nmcli".into(),
            strategy: Some("direct".into()),
        };
        let auto = AutoPrivilegedExec::resolve_with_eject(
            INTENT_NMCLI_INVOCATION,
            Some(&resolution),
            always_non_root,
        );
        assert_eq!(auto.strategy_name(), "direct");
        assert!(auto
            .rationale()
            .contains("framework-resolved nmcli_invocation strategy=direct"));
    }

    #[test]
    fn resolve_with_available_no_strategy_falls_back_to_euid_root() {
        let resolution = CapabilityResolution::Available {
            evidence: "present".into(),
            strategy: None,
        };
        let auto = AutoPrivilegedExec::resolve_with_eject(
            INTENT_NMCLI_INVOCATION,
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
        let auto = AutoPrivilegedExec::resolve_with_eject(
            INTENT_NMCLI_INVOCATION,
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
        let auto = AutoPrivilegedExec::resolve_with_eject(
            INTENT_NMCLI_INVOCATION,
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
        let auto = AutoPrivilegedExec::resolve_with_eject(
            INTENT_NMCLI_INVOCATION,
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
        let auto = AutoPrivilegedExec::resolve_with_eject(
            INTENT_NMCLI_INVOCATION,
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
        let auto = AutoPrivilegedExec::resolve_with_eject(
            INTENT_NMCLI_INVOCATION,
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
        let auto = AutoPrivilegedExec::resolve_with_eject(
            INTENT_NMCLI_INVOCATION,
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
        let auto = AutoPrivilegedExec::resolve_with_eject(
            INTENT_NMCLI_INVOCATION,
            Some(&resolution),
            always_non_root,
        );
        assert_eq!(auto.strategy_name(), "sudo");
        assert!(auto.rationale().contains("did not probe"));
    }

    #[test]
    fn resolve_with_no_resolution_falls_back_to_euid() {
        let auto = AutoPrivilegedExec::resolve_with_eject(
            INTENT_NMCLI_INVOCATION,
            None,
            always_root,
        );
        assert_eq!(auto.strategy_name(), "direct");
        assert!(auto
            .rationale()
            .contains("did not contain nmcli_invocation"));
    }

    #[test]
    fn resolve_from_empty_map_uses_euid_fallback() {
        let map = CapabilityResolutionMap::new();
        let auto = AutoPrivilegedExec::resolve(INTENT_NMCLI_INVOCATION, &map);
        assert!(matches!(auto.strategy_name(), "direct" | "sudo"));
        assert!(auto.rationale().contains("EUID detection"));
    }

    #[test]
    fn resolve_records_intent_id_for_diagnostics() {
        let map = CapabilityResolutionMap::new();
        let iw = AutoPrivilegedExec::resolve(INTENT_IW_INVOCATION, &map);
        assert_eq!(iw.intent_id(), "iw_invocation");
        assert!(iw.rationale().contains("iw_invocation"));

        let rfkill =
            AutoPrivilegedExec::resolve(INTENT_RFKILL_INVOCATION, &map);
        assert_eq!(rfkill.intent_id(), "rfkill_invocation");
    }

    #[tokio::test]
    async fn direct_dispatcher_invokes_binary_directly() {
        let d = DirectExec::new();
        let out = d
            .dispatch("/bin/true", &[], Duration::from_millis(2000))
            .await
            .expect("direct dispatch should succeed against /bin/true");
        assert!(out.status.success());
    }

    #[tokio::test]
    async fn dispatcher_timeout_surfaces_transient_error() {
        let d = DirectExec::new();
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

    fn map_with_strategy_hint(
        intent_id: &str,
        strategy: &str,
    ) -> CapabilityResolutionMap {
        let mut map = CapabilityResolutionMap::new();
        map.insert(
            intent_id.to_string(),
            CapabilityResolution::Available {
                evidence: format!("test fixture: {strategy}"),
                strategy: Some(strategy.into()),
            },
        );
        map
    }

    #[test]
    fn re_resolve_swaps_sudo_to_direct() {
        let auto = AutoPrivilegedExec::resolve(
            INTENT_NMCLI_INVOCATION,
            &map_with_strategy_hint(INTENT_NMCLI_INVOCATION, "sudo"),
        );
        assert_eq!(auto.current_strategy_name(), "sudo");
        auto.re_resolve(&map_with_strategy_hint(
            INTENT_NMCLI_INVOCATION,
            "direct",
        ));
        assert_eq!(auto.current_strategy_name(), "direct");
        assert!(auto.rationale().contains("direct"));
    }

    #[test]
    fn re_resolve_swaps_direct_to_sudo() {
        let auto = AutoPrivilegedExec::resolve(
            INTENT_NMCLI_INVOCATION,
            &map_with_strategy_hint(INTENT_NMCLI_INVOCATION, "direct"),
        );
        assert_eq!(auto.current_strategy_name(), "direct");
        auto.re_resolve(&map_with_strategy_hint(
            INTENT_NMCLI_INVOCATION,
            "sudo",
        ));
        assert_eq!(auto.current_strategy_name(), "sudo");
        assert!(auto.rationale().contains("sudo"));
    }

    #[test]
    fn re_resolve_to_unavailable_keeps_direct_with_remedy() {
        let auto = AutoPrivilegedExec::resolve(
            INTENT_NMCLI_INVOCATION,
            &map_with_strategy_hint(INTENT_NMCLI_INVOCATION, "sudo"),
        );
        let mut updated = CapabilityResolutionMap::new();
        updated.insert(
            INTENT_NMCLI_INVOCATION.to_string(),
            CapabilityResolution::Unavailable {
                reason: "test: drop-in removed".into(),
                remedy: "reinstall sudoers".into(),
            },
        );
        auto.re_resolve(&updated);
        // network.nm policy on Unavailable: keep direct
        // dispatcher (hardened hosts may still allow direct
        // exec); carry the remedy in rationale.
        assert_eq!(auto.current_strategy_name(), "direct");
        assert!(auto.rationale().contains("framework refused"));
        assert!(auto.rationale().contains("reinstall sudoers"));
    }
}
