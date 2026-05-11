// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Runtime supervisor for the network plane.
//!
//! The supervisor is a background loop that runs alongside the
//! plugin's request-handling. It watches link state, reaches
//! upstream connectivity probes, detects captive portals, and on
//! full-loss raises an open critical-recovery hotspot so an
//! operator can recover the device without physical access.
//! When the previously-configured STA's BSS comes back into
//! range, the supervisor tears the hotspot down and re-applies
//! the operator's intent.
//!
//! Loop shape:
//!
//! 1. Tick every `interval_ms` (default 10 s; configurable via
//!    `EVO_NETWORK_SUPERVISOR_INTERVAL_MS`).
//! 2. Compose a [`SupervisorObservations`] snapshot from the
//!    NetworkManager connectivity surface + a `curl` reachability
//!    probe (RFC 8910 / HTTP 204 style).
//! 3. Drive the [`SupervisorState`] state machine; publish the
//!    new [`SupervisorView`] on a `tokio::sync::watch` channel so
//!    wire-op handlers and reactive subscribers read consistent
//!    state.
//! 4. On `Offline` persisting longer than `critical_grace_ms`,
//!    trigger critical-recovery action (caller-supplied). On
//!    return-from-`Offline`, trigger the STA-restore action.
//!
//! All I/O is routed through the [`PrivilegedExec`] dispatchers
//! the plugin holds; the supervisor never spawns commands
//! directly. The probe / recovery actions are passed in as boxed
//! futures so unit tests can substitute deterministic fakes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;

/// Reachability state surfaced to operators + reactive
/// subscribers. The state machine derives this from
/// [`SupervisorObservations`] every tick.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum ReachabilityState {
    /// No probe outcome yet (supervisor just started).
    #[default]
    Unknown,
    /// Gateway reachable AND connectivity probe returned HTTP
    /// 204 (no redirect) — full internet reachability.
    Online,
    /// Gateway reachable AND connectivity probe redirected to a
    /// portal — operator authentication required.
    Portal,
    /// Gateway reachable AND connectivity probe failed or
    /// returned a non-204 non-redirect (intranet-only / DNS
    /// flake / upstream issue) — local services work, internet
    /// does not.
    Limited,
    /// No usable uplink: STA not associated AND no Ethernet
    /// carrier, OR every probe failed to reach the gateway.
    Offline,
}

impl ReachabilityState {
    /// `true` when the link is healthy enough to drop a
    /// critical-recovery hotspot.
    pub fn is_serviceable(&self) -> bool {
        matches!(self, ReachabilityState::Online | ReachabilityState::Limited)
    }
}

/// Compact diagnostic snapshot the supervisor records on every
/// tick. Drives the state machine and surfaces on the wire-op
/// status verb so operators can debug without enabling trace
/// logging.
#[derive(Debug, Clone, Serialize, Default)]
pub struct SupervisorObservations {
    /// `nmcli general connectivity` result (`full` / `limited` /
    /// `portal` / `none` / `unknown`).
    pub nm_connectivity: Option<String>,
    /// HTTP status code from the connectivity probe (typically
    /// 204 when online, 200/301/302 on captive-portal redirect,
    /// `None` on probe failure).
    pub probe_http_code: Option<u16>,
    /// Effective URL the connectivity probe ended at — when this
    /// differs from the probe target, a captive portal is
    /// likely intercepting traffic.
    pub probe_effective_url: Option<String>,
    /// `true` when an Ethernet device reports a non-zero carrier
    /// state. Lets the state machine distinguish "Wi-Fi down,
    /// Ethernet up" from "everything down".
    pub ethernet_carrier_up: bool,
    /// `true` when at least one Wi-Fi device reports `connected`
    /// in the iw link probe.
    pub wifi_associated: bool,
}

/// Captive-portal info recorded when the supervisor detects a
/// redirect. Surfaces via the reactive subject so a UI can prompt
/// the operator to authenticate.
#[derive(Debug, Clone, Serialize, Default)]
pub struct PortalInfo {
    /// URL the connectivity probe was redirected to.
    pub portal_url: String,
    /// First instant at which the portal was observed.
    #[serde(skip)]
    pub since: Option<Instant>,
}

/// Public view of the supervisor's current state. Returned to
/// wire-op callers + emitted on the reactive subject every time
/// the state advances.
#[derive(Debug, Clone, Serialize, Default)]
pub struct SupervisorView {
    /// Current reachability classification.
    pub reachability: ReachabilityState,
    /// Most recent observations snapshot.
    pub last_observations: SupervisorObservations,
    /// Set when [`Self::reachability`] is `Portal`.
    pub portal: Option<PortalInfo>,
    /// `true` when the supervisor has raised the critical-
    /// recovery hotspot in response to a sustained `Offline`
    /// state.
    pub critical_recovery_active: bool,
    /// Number of consecutive ticks the supervisor has observed
    /// the current reachability state. Used to gate the
    /// critical-recovery trigger.
    pub state_ticks: u32,
}

/// Configuration for the supervisor task. Each field is
/// overridable via `EVO_NETWORK_SUPERVISOR_*` env vars.
#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    /// Tick period in milliseconds. Default 10 000 ms; min 1000.
    pub interval_ms: u64,
    /// How long the supervisor must observe `Offline` before
    /// firing the critical-recovery action. Default 30 000 ms.
    pub critical_grace_ms: u64,
    /// How long the supervisor must observe `Online` / `Limited`
    /// after a critical-recovery before firing the STA-restore
    /// action. Default 15 000 ms.
    pub restore_grace_ms: u64,
    /// Default URL for the connectivity probe. RFC-8910-style
    /// HTTP 204 endpoint. Default
    /// `http://connectivitycheck.gstatic.com/generate_204`.
    pub probe_url: String,
}

impl SupervisorConfig {
    /// Load env-overridable defaults.
    pub fn from_env() -> Self {
        let interval_ms = std::env::var("EVO_NETWORK_SUPERVISOR_INTERVAL_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|v| *v >= 1000)
            .unwrap_or(10_000);
        let critical_grace_ms =
            std::env::var("EVO_NETWORK_SUPERVISOR_CRITICAL_GRACE_MS")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|v| *v >= 1000)
                .unwrap_or(30_000);
        let restore_grace_ms =
            std::env::var("EVO_NETWORK_SUPERVISOR_RESTORE_GRACE_MS")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|v| *v >= 1000)
                .unwrap_or(15_000);
        let probe_url = std::env::var("EVO_NETWORK_SUPERVISOR_PROBE_URL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| {
                "http://connectivitycheck.gstatic.com/generate_204".to_string()
            });
        Self {
            interval_ms,
            critical_grace_ms,
            restore_grace_ms,
            probe_url,
        }
    }
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self::from_env()
    }
}

/// Pure classifier — takes one observations snapshot, returns
/// the reachability classification. Extracted as a free function
/// so unit tests exercise the rules without a running task.
pub fn classify_reachability(
    obs: &SupervisorObservations,
) -> ReachabilityState {
    // No uplink at all: nothing to classify but offline.
    let has_uplink = obs.ethernet_carrier_up || obs.wifi_associated;
    if !has_uplink {
        return ReachabilityState::Offline;
    }
    // NM's own verdict takes priority when it is concrete.
    if let Some(c) = obs.nm_connectivity.as_deref() {
        match c {
            "full" => return ReachabilityState::Online,
            "portal" => return ReachabilityState::Portal,
            "limited" => return ReachabilityState::Limited,
            "none" => return ReachabilityState::Offline,
            _ => {}
        }
    }
    // Curl-probe classification: 204 is online; any redirect
    // away from the probe URL implies a captive portal; other
    // codes mean limited connectivity.
    let probe_changed = matches!(
        (obs.probe_effective_url.as_deref(), obs.probe_http_code),
        (Some(_), Some(c)) if c != 204
    );
    if probe_changed {
        return ReachabilityState::Portal;
    }
    match obs.probe_http_code {
        Some(204) => ReachabilityState::Online,
        Some(_) => ReachabilityState::Limited,
        None => ReachabilityState::Limited,
    }
}

/// Decision the supervisor reaches after applying the state
/// machine to one fresh observation. The plugin's task body
/// invokes the matching action callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorDecision {
    /// State advanced but no recovery action is needed.
    NoAction,
    /// Trigger critical-recovery hotspot. Caller invokes its
    /// recovery callback.
    RaiseCriticalRecovery,
    /// Tear the recovery hotspot down + re-apply the operator's
    /// intent. Caller invokes its restore callback.
    RestoreSta,
}

/// Apply the state-machine rules: update the view from the new
/// observations + last view, and decide whether to raise a
/// critical-recovery hotspot or restore STA.
pub fn step(
    prev: &SupervisorView,
    obs: SupervisorObservations,
    config: &SupervisorConfig,
) -> (SupervisorView, SupervisorDecision) {
    let reachability = classify_reachability(&obs);
    let state_ticks = if reachability == prev.reachability {
        prev.state_ticks.saturating_add(1)
    } else {
        1
    };
    let mut portal = prev.portal.clone();
    if reachability == ReachabilityState::Portal {
        let url = obs.probe_effective_url.clone().unwrap_or_default();
        portal = Some(PortalInfo {
            portal_url: url,
            since: portal.and_then(|p| p.since).or(Some(Instant::now())),
        });
    } else {
        portal = None;
    }

    // Critical-recovery trigger: Offline persisting longer than
    // the grace window AND no recovery already active.
    let ticks_per_grace = ticks_to_cover(state_ticks, &reachability, config);
    let mut decision = SupervisorDecision::NoAction;
    let mut critical = prev.critical_recovery_active;
    if !critical
        && reachability == ReachabilityState::Offline
        && ticks_per_grace.offline_ms >= config.critical_grace_ms
    {
        decision = SupervisorDecision::RaiseCriticalRecovery;
        critical = true;
    } else if critical
        && reachability.is_serviceable()
        && ticks_per_grace.serviceable_ms >= config.restore_grace_ms
    {
        decision = SupervisorDecision::RestoreSta;
        critical = false;
    }

    let view = SupervisorView {
        reachability,
        last_observations: obs,
        portal,
        critical_recovery_active: critical,
        state_ticks,
    };
    (view, decision)
}

/// Grace-window accounting helper. Returns the cumulative time
/// the supervisor has spent in the current reachability class so
/// the state machine can compare against the configured grace
/// windows without needing wall-clock readings inside the
/// classifier.
struct GraceTimings {
    offline_ms: u64,
    serviceable_ms: u64,
}

fn ticks_to_cover(
    state_ticks: u32,
    state: &ReachabilityState,
    config: &SupervisorConfig,
) -> GraceTimings {
    let elapsed = u64::from(state_ticks).saturating_mul(config.interval_ms);
    GraceTimings {
        offline_ms: if matches!(state, ReachabilityState::Offline) {
            elapsed
        } else {
            0
        },
        serviceable_ms: if state.is_serviceable() { elapsed } else { 0 },
    }
}

/// Action closures the supervisor task drives every tick. The
/// shapes are `Arc<dyn Fn>` returning boxed futures so the task
/// stays `Send + 'static` and the plugin's `Plugin::load` can
/// capture Arc-cloned dispatcher state at construction time
/// without sharing the plugin itself across the task boundary.
type AsyncProbe = Arc<
    dyn Fn() -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = SupervisorObservations> + Send,
            >,
        > + Send
        + Sync,
>;
type AsyncRecovery = Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

/// Bundle of action callbacks the supervisor task invokes.
#[derive(Clone)]
pub struct SupervisorActions {
    /// Build one observations snapshot. Called every tick.
    pub probe: AsyncProbe,
    /// Raise the critical-recovery hotspot. Called once when
    /// `SupervisorDecision::RaiseCriticalRecovery` fires.
    pub raise_critical_recovery: AsyncRecovery,
    /// Tear the recovery hotspot down + re-apply the operator's
    /// intent. Called once when `SupervisorDecision::RestoreSta`
    /// fires.
    pub restore_sta: AsyncRecovery,
}

/// Handle on the running supervisor task. The plugin stores one
/// of these per loaded instance; `Plugin::unload` calls
/// `shutdown` and awaits the task.
pub struct SupervisorTask {
    /// Tokio task running the probe loop.
    pub task: JoinHandle<()>,
    /// Shutdown signal — `notify_one()` to stop the loop.
    pub shutdown: Arc<Notify>,
    /// Watch sender publishing the live view. Wire-op handlers
    /// borrow the receiver to render `network.nm.supervisor.status`
    /// without polling the task.
    pub view: watch::Sender<SupervisorView>,
}

impl SupervisorTask {
    /// Stop the supervisor gracefully. Idempotent.
    pub async fn shutdown(self) {
        self.shutdown.notify_one();
        let _ = self.task.await;
    }
}

/// Spawn the supervisor's probe loop. Returns a handle the
/// plugin owns and shuts down at unload time. The task ticks
/// every `config.interval_ms` until `shutdown.notify_one()` is
/// called or the watch receiver count drops to zero (sender
/// would still be live; the loop guards against panic in the
/// observed action callbacks by catching `JoinError` upstream).
pub fn spawn(
    config: SupervisorConfig,
    actions: SupervisorActions,
    plugin_log_tag: &'static str,
) -> SupervisorTask {
    let (view_tx, _view_rx_initial) = watch::channel(SupervisorView::default());
    let shutdown = Arc::new(Notify::new());
    let task_shutdown = Arc::clone(&shutdown);
    let task_view = view_tx.clone();
    let task = tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(Duration::from_millis(config.interval_ms));
        // First tick fires immediately; subsequent ticks honour
        // the configured cadence. Skip if a previous tick took
        // longer than the interval (no burst-catchup).
        interval
            .set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let obs = (actions.probe)().await;
                    let prev = task_view.borrow().clone();
                    let (next, decision) = step(&prev, obs, &config);
                    // Best-effort publish — receivers dropping
                    // is fine; the task carries on.
                    let _ = task_view.send(next.clone());
                    match decision {
                        SupervisorDecision::NoAction => {}
                        SupervisorDecision::RaiseCriticalRecovery => {
                            tracing::warn!(
                                plugin = plugin_log_tag,
                                reachability = ?next.reachability,
                                state_ticks = next.state_ticks,
                                "supervisor: raising critical-recovery hotspot"
                            );
                            (actions.raise_critical_recovery)().await;
                        }
                        SupervisorDecision::RestoreSta => {
                            tracing::info!(
                                plugin = plugin_log_tag,
                                reachability = ?next.reachability,
                                state_ticks = next.state_ticks,
                                "supervisor: restoring STA after recovery"
                            );
                            (actions.restore_sta)().await;
                        }
                    }
                }
                _ = task_shutdown.notified() => {
                    tracing::debug!(
                        plugin = plugin_log_tag,
                        "supervisor: shutdown signal received"
                    );
                    break;
                }
            }
        }
    });
    SupervisorTask {
        task,
        shutdown,
        view: view_tx,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs_online() -> SupervisorObservations {
        SupervisorObservations {
            nm_connectivity: Some("full".to_string()),
            probe_http_code: Some(204),
            probe_effective_url: None,
            ethernet_carrier_up: false,
            wifi_associated: true,
        }
    }

    fn obs_offline() -> SupervisorObservations {
        SupervisorObservations {
            nm_connectivity: Some("none".to_string()),
            probe_http_code: None,
            probe_effective_url: None,
            ethernet_carrier_up: false,
            wifi_associated: false,
        }
    }

    fn obs_portal() -> SupervisorObservations {
        SupervisorObservations {
            nm_connectivity: Some("portal".to_string()),
            probe_http_code: Some(302),
            probe_effective_url: Some("http://hotel.portal/login".to_string()),
            ethernet_carrier_up: false,
            wifi_associated: true,
        }
    }

    #[test]
    fn classify_online_when_nm_says_full() {
        assert_eq!(
            classify_reachability(&obs_online()),
            ReachabilityState::Online
        );
    }

    #[test]
    fn classify_offline_when_no_uplink() {
        assert_eq!(
            classify_reachability(&obs_offline()),
            ReachabilityState::Offline
        );
    }

    #[test]
    fn classify_portal_when_redirect_observed() {
        assert_eq!(
            classify_reachability(&obs_portal()),
            ReachabilityState::Portal
        );
    }

    #[test]
    fn classify_limited_when_no_204_no_redirect() {
        let obs = SupervisorObservations {
            nm_connectivity: None,
            probe_http_code: Some(500),
            probe_effective_url: None,
            ethernet_carrier_up: true,
            wifi_associated: false,
        };
        assert_eq!(classify_reachability(&obs), ReachabilityState::Limited);
    }

    #[test]
    fn classify_uses_curl_when_nm_unknown() {
        let obs = SupervisorObservations {
            nm_connectivity: Some("unknown".to_string()),
            probe_http_code: Some(204),
            probe_effective_url: None,
            ethernet_carrier_up: false,
            wifi_associated: true,
        };
        assert_eq!(classify_reachability(&obs), ReachabilityState::Online);
    }

    #[test]
    fn step_advances_ticks_on_steady_state() {
        let config = SupervisorConfig {
            interval_ms: 1000,
            ..SupervisorConfig::default()
        };
        let prev = SupervisorView {
            reachability: ReachabilityState::Online,
            state_ticks: 5,
            ..Default::default()
        };
        let (view, dec) = step(&prev, obs_online(), &config);
        assert_eq!(view.reachability, ReachabilityState::Online);
        assert_eq!(view.state_ticks, 6);
        assert_eq!(dec, SupervisorDecision::NoAction);
    }

    #[test]
    fn step_raises_critical_recovery_after_offline_grace() {
        let config = SupervisorConfig {
            interval_ms: 1000,
            critical_grace_ms: 3000,
            ..SupervisorConfig::default()
        };
        // Build a starting view that already saw two offline
        // ticks (so the third tick crosses the 3s grace).
        let prev = SupervisorView {
            reachability: ReachabilityState::Offline,
            state_ticks: 2,
            critical_recovery_active: false,
            ..Default::default()
        };
        let (view, dec) = step(&prev, obs_offline(), &config);
        assert_eq!(view.reachability, ReachabilityState::Offline);
        assert_eq!(view.state_ticks, 3);
        assert!(view.critical_recovery_active);
        assert_eq!(dec, SupervisorDecision::RaiseCriticalRecovery);
    }

    #[test]
    fn step_restores_sta_after_serviceable_grace() {
        let config = SupervisorConfig {
            interval_ms: 1000,
            restore_grace_ms: 3000,
            ..SupervisorConfig::default()
        };
        let prev = SupervisorView {
            reachability: ReachabilityState::Online,
            state_ticks: 2,
            critical_recovery_active: true,
            ..Default::default()
        };
        let (view, dec) = step(&prev, obs_online(), &config);
        assert_eq!(view.state_ticks, 3);
        assert!(!view.critical_recovery_active);
        assert_eq!(dec, SupervisorDecision::RestoreSta);
    }

    #[test]
    fn step_records_portal_url_when_classified_as_portal() {
        let config = SupervisorConfig::default();
        let prev = SupervisorView::default();
        let (view, _) = step(&prev, obs_portal(), &config);
        assert_eq!(view.reachability, ReachabilityState::Portal);
        let portal = view.portal.expect("portal info recorded");
        assert_eq!(portal.portal_url, "http://hotel.portal/login");
    }

    #[test]
    fn step_clears_portal_when_state_leaves_portal() {
        let config = SupervisorConfig::default();
        let prev = SupervisorView {
            reachability: ReachabilityState::Portal,
            portal: Some(PortalInfo {
                portal_url: "http://old/login".into(),
                since: Some(Instant::now()),
            }),
            ..Default::default()
        };
        let (view, _) = step(&prev, obs_online(), &config);
        assert_eq!(view.reachability, ReachabilityState::Online);
        assert!(view.portal.is_none());
    }
}
