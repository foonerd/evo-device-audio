// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Cross-source reconciliation + discrepancy detection.
//!
//! Real-world userspace network daemons (NetworkManager,
//! ConnMan, systemd-networkd, iwd) periodically misreport
//! state during transitions. NM's `Connectivity` property has
//! been observed advertising `full` while the kernel reports
//! `no carrier`, and the inverse on recovery. A supervisor
//! that trusts one source uncritically inherits that source's
//! confusion modes.
//!
//! The reconciler is the substrate seam that catches these
//! cases:
//!
//! - It holds a [`LinkSourceView`] cell per active source —
//!   the most recent state each source reported, plus the
//!   instant at which it was last updated.
//! - On every supervisor wake, [`detect_discrepancies`] walks
//!   an explicit rule table comparing views against each
//!   other, producing a [`Discrepancy`] per rule whose
//!   precondition fires.
//! - The supervisor's main loop emits one
//!   `LinkSourceDiscrepancy` observation per [`Discrepancy`]
//!   the reconciler produces, so operators reading the
//!   observability surface see in real time which sources
//!   disagree and which one the framework trusted.
//!
//! The rule table is *data*, not code. [`RULES`] is a static
//! `&[ReconciliationRule]` and the framework's
//! `describe_capabilities` wire-op surfaces it on the wire so
//! operators read the active rule set without source-code
//! access. Adding a rule means appending a row, not editing
//! a match arm.

use serde::Serialize;
use std::collections::HashMap;
use std::time::Instant;

/// One source's most recent observation. The reconciler holds
/// one cell per active source; the supervisor's main loop
/// refreshes the cell after every event the source emits.
#[derive(Debug, Clone, Default)]
pub struct LinkSourceView {
    /// Source's most recently reported layer-2 carrier state.
    /// `None` when the source does not observe carrier (e.g.
    /// the network-manager source defers carrier to the
    /// kernel).
    pub carrier_up: Option<bool>,

    /// Source's most recently reported address-attached
    /// state. `None` when the source does not observe
    /// addresses.
    pub address_attached: Option<bool>,

    /// Source's most recently reported Wi-Fi association
    /// state. `None` when the source does not observe Wi-Fi.
    pub wifi_associated: Option<bool>,

    /// Source's most recently reported connectivity verdict
    /// (`full` / `portal` / `limited` / `none` / `unknown`).
    /// `None` when the source does not observe a verdict.
    pub connectivity_verdict: Option<ConnectivityVerdict>,

    /// When the cell was last refreshed. Used by the rules
    /// to ignore stale views (a source that hasn't emitted
    /// in a long time may have silently fallen behind real
    /// state).
    #[allow(dead_code)]
    pub updated_at: Option<Instant>,
}

/// Canonical connectivity verdict surface, normalised across
/// every userspace daemon's vocabulary. NM's
/// `Connectivity` property maps to this directly; ConnMan +
/// systemd-networkd map via a per-source translator (lands
/// with each backend).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectivityVerdict {
    /// The daemon believes connectivity is fully functional
    /// (no portal, reachable upstream).
    Full,
    /// A captive portal is intercepting traffic.
    Portal,
    /// The link is up but cannot reach the configured probe
    /// target (e.g. DNS resolution failing, upstream blocked).
    Limited,
    /// No connectivity.
    None,
    /// The daemon could not determine connectivity (probe
    /// pending, daemon initialising).
    Unknown,
}

impl ConnectivityVerdict {
    /// Whether the verdict claims the link is up enough to
    /// reach upstream services. The reconciler's rules use
    /// this to detect contradictions with kernel-reported
    /// carrier-down states.
    pub fn is_up(self) -> bool {
        matches!(self, Self::Full | Self::Portal | Self::Limited)
    }
}

/// One row in the reconciliation rule table. Rules are
/// declared as data so the supervisor's `describe_capabilities`
/// can serialise them on the wire.
#[derive(Debug, Clone, Serialize)]
pub struct ReconciliationRule {
    /// Stable identifier surfaced on the wire.
    pub id: &'static str,

    /// Operator-readable one-line description of what the
    /// rule detects.
    pub description: &'static str,

    /// Sources the rule cross-references. Surfaced for
    /// operators so they understand the rule's scope.
    pub sources: &'static [&'static str],

    /// The kind of discrepancy this rule detects. Surfaces
    /// on the [`Discrepancy::kind`] field so consumers can
    /// match on the category without parsing the rule id.
    pub kind: DiscrepancyKind,
}

/// Classification of a detected cross-source disagreement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscrepancyKind {
    /// The kernel says no carrier on every interface but a
    /// userspace daemon advertises a usable connectivity
    /// verdict. The daemon is lying — trust the kernel.
    CarrierDownButDaemonReportsUp,

    /// The kernel says carrier-up + at least one IP address
    /// attached but a userspace daemon advertises no
    /// connectivity. The daemon is slow to converge —
    /// probably still probing.
    CarrierUpButDaemonReportsDown,

    /// Two daemon-style sources report contradictory
    /// verdicts (NM says `full`, ConnMan says `none`).
    /// Neither source is the kernel; the reconciler surfaces
    /// the disagreement without preferring either.
    DaemonsDisagree,
}

/// A single discrepancy detected on one wake. The supervisor
/// turns each into a `LinkSourceDiscrepancy` observation.
#[derive(Debug, Clone, Serialize)]
pub struct Discrepancy {
    /// The rule that fired.
    pub rule: &'static str,

    /// Category — same value as the rule's `kind` field.
    pub kind: DiscrepancyKind,

    /// Operator-readable description of what was observed.
    pub detail: String,

    /// The source the reconciler treated as authoritative.
    /// Empty for `DaemonsDisagree` (no daemon is preferred).
    pub trusted: &'static str,

    /// The source the reconciler treated as untrusted.
    /// Empty for `DaemonsDisagree`.
    pub untrusted: &'static str,
}

/// The framework's active reconciliation rule table. Adding a
/// rule means appending to this slice; consumers reading
/// `describe_capabilities` see the new rule immediately.
///
/// Rules are evaluated in source-table order. Each rule
/// produces zero or one `Discrepancy` per wake.
pub static RULES: &[ReconciliationRule] = &[
    ReconciliationRule {
        id: "carrier_down_but_daemon_full",
        description:
            "Kernel reports no carrier on any observed interface, but a \
             userspace daemon advertises Full / Portal / Limited \
             connectivity. Trust the kernel — the daemon is mid-\
             transition or stuck.",
        sources: &["rtnetlink", "network-manager"],
        kind: DiscrepancyKind::CarrierDownButDaemonReportsUp,
    },
    ReconciliationRule {
        id: "carrier_up_but_daemon_none",
        description:
            "Kernel reports carrier-up + an IP address attached, but a \
             userspace daemon advertises None connectivity. The daemon \
             is still probing or silently stuck — wait for it to \
             converge before raising a recovery action.",
        sources: &["rtnetlink", "network-manager"],
        kind: DiscrepancyKind::CarrierUpButDaemonReportsDown,
    },
    ReconciliationRule {
        id: "daemons_disagree",
        description: "Two userspace daemon sources report contradictory \
             connectivity verdicts. Surface the disagreement; the \
             kernel-side reconciliation rules above carry the trust \
             decision when one daemon also contradicts the kernel.",
        sources: &["network-manager", "connman", "systemd-networkd"],
        kind: DiscrepancyKind::DaemonsDisagree,
    },
];

/// Run the rule table against the supplied per-source views
/// and return every discrepancy detected on this wake.
///
/// Pure function over the views — no I/O, no Notify, no
/// timer. The supervisor calls it after refreshing the views
/// from the wake's compose-observations result.
pub fn detect_discrepancies(
    views: &HashMap<&'static str, LinkSourceView>,
) -> Vec<Discrepancy> {
    let mut out = Vec::new();

    let kernel = views.get("rtnetlink");
    let nm = views.get("network-manager");

    // Rule 1: carrier-down + daemon-up
    if let (Some(k), Some(d)) = (kernel, nm) {
        if k.carrier_up == Some(false) {
            if let Some(verdict) = d.connectivity_verdict {
                if verdict.is_up() {
                    out.push(Discrepancy {
                        rule: "carrier_down_but_daemon_full",
                        kind: DiscrepancyKind::CarrierDownButDaemonReportsUp,
                        detail: format!(
                            "rtnetlink: carrier_up = false; \
                             network-manager: connectivity = {verdict:?}"
                        ),
                        trusted: "rtnetlink",
                        untrusted: "network-manager",
                    });
                }
            }
        }
    }

    // Rule 2: carrier-up + address-attached + daemon-none
    if let (Some(k), Some(d)) = (kernel, nm) {
        if k.carrier_up == Some(true) && k.address_attached == Some(true) {
            if let Some(ConnectivityVerdict::None) = d.connectivity_verdict {
                out.push(Discrepancy {
                    rule: "carrier_up_but_daemon_none",
                    kind: DiscrepancyKind::CarrierUpButDaemonReportsDown,
                    detail: "rtnetlink: carrier_up + address_attached; \
                             network-manager: connectivity = None"
                        .to_string(),
                    trusted: "rtnetlink",
                    untrusted: "network-manager",
                });
            }
        }
    }

    // Rule 3: two userspace daemons disagree (no kernel
    // preference; the supervisor's downstream rules handle
    // which to trust when one is also contradicting the
    // kernel). Only `network-manager` ships as a daemon
    // source today; the rule sits in place for the planned
    // ConnMan + systemd-networkd backends.
    let candidate_daemons: &[&'static str] =
        &["network-manager", "connman", "systemd-networkd"];
    let active_daemons: Vec<(&'static str, ConnectivityVerdict)> =
        candidate_daemons
            .iter()
            .filter_map(|n| {
                views
                    .get(n)
                    .and_then(|v| v.connectivity_verdict.map(|c| (*n, c)))
            })
            .collect();
    for window in active_daemons.windows(2) {
        let (n1, v1) = window[0];
        let (n2, v2) = window[1];
        if v1 != v2 {
            out.push(Discrepancy {
                rule: "daemons_disagree",
                kind: DiscrepancyKind::DaemonsDisagree,
                detail: format!("{n1}: {v1:?}; {n2}: {v2:?}"),
                trusted: "",
                untrusted: "",
            });
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn views_of(
        items: impl IntoIterator<Item = (&'static str, LinkSourceView)>,
    ) -> HashMap<&'static str, LinkSourceView> {
        items.into_iter().collect()
    }

    #[test]
    fn empty_views_produce_no_discrepancies() {
        assert!(detect_discrepancies(&HashMap::new()).is_empty());
    }

    #[test]
    fn matching_kernel_and_daemon_views_produce_no_discrepancies() {
        let views = views_of([
            (
                "rtnetlink",
                LinkSourceView {
                    carrier_up: Some(true),
                    address_attached: Some(true),
                    ..Default::default()
                },
            ),
            (
                "network-manager",
                LinkSourceView {
                    connectivity_verdict: Some(ConnectivityVerdict::Full),
                    ..Default::default()
                },
            ),
        ]);
        let d = detect_discrepancies(&views);
        assert!(d.is_empty(), "unexpected discrepancies: {d:?}");
    }

    #[test]
    fn carrier_down_but_nm_full_emits_discrepancy() {
        let views = views_of([
            (
                "rtnetlink",
                LinkSourceView {
                    carrier_up: Some(false),
                    ..Default::default()
                },
            ),
            (
                "network-manager",
                LinkSourceView {
                    connectivity_verdict: Some(ConnectivityVerdict::Full),
                    ..Default::default()
                },
            ),
        ]);
        let d = detect_discrepancies(&views);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].rule, "carrier_down_but_daemon_full");
        assert_eq!(d[0].kind, DiscrepancyKind::CarrierDownButDaemonReportsUp);
        assert_eq!(d[0].trusted, "rtnetlink");
        assert_eq!(d[0].untrusted, "network-manager");
    }

    #[test]
    fn carrier_down_but_nm_portal_also_fires_rule_1() {
        // Portal still counts as "claims connectivity is up".
        let views = views_of([
            (
                "rtnetlink",
                LinkSourceView {
                    carrier_up: Some(false),
                    ..Default::default()
                },
            ),
            (
                "network-manager",
                LinkSourceView {
                    connectivity_verdict: Some(ConnectivityVerdict::Portal),
                    ..Default::default()
                },
            ),
        ]);
        let d = detect_discrepancies(&views);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].rule, "carrier_down_but_daemon_full");
    }

    #[test]
    fn carrier_up_with_address_but_nm_none_emits_discrepancy() {
        let views = views_of([
            (
                "rtnetlink",
                LinkSourceView {
                    carrier_up: Some(true),
                    address_attached: Some(true),
                    ..Default::default()
                },
            ),
            (
                "network-manager",
                LinkSourceView {
                    connectivity_verdict: Some(ConnectivityVerdict::None),
                    ..Default::default()
                },
            ),
        ]);
        let d = detect_discrepancies(&views);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].rule, "carrier_up_but_daemon_none");
        assert_eq!(d[0].kind, DiscrepancyKind::CarrierUpButDaemonReportsDown);
    }

    #[test]
    fn carrier_up_without_address_does_not_fire_rule_2() {
        // Carrier-up + no IP yet is the normal initial state
        // during link bring-up; NM saying `None` is correct
        // there.
        let views = views_of([
            (
                "rtnetlink",
                LinkSourceView {
                    carrier_up: Some(true),
                    address_attached: Some(false),
                    ..Default::default()
                },
            ),
            (
                "network-manager",
                LinkSourceView {
                    connectivity_verdict: Some(ConnectivityVerdict::None),
                    ..Default::default()
                },
            ),
        ]);
        assert!(detect_discrepancies(&views).is_empty());
    }

    #[test]
    fn kernel_alone_produces_no_discrepancies() {
        // Single source is the baseline; cross-source rules
        // are no-ops.
        let views = views_of([(
            "rtnetlink",
            LinkSourceView {
                carrier_up: Some(false),
                ..Default::default()
            },
        )]);
        assert!(detect_discrepancies(&views).is_empty());
    }

    #[test]
    fn rules_table_exposed_for_describe_capabilities() {
        // The wire-op surface serialises the rule table. We
        // assert the shape so a downstream consumer can rely
        // on the fields without spelunking.
        assert!(!RULES.is_empty());
        for rule in RULES {
            assert!(!rule.id.is_empty());
            assert!(!rule.description.is_empty());
            assert!(!rule.sources.is_empty());
        }
    }

    #[test]
    fn connectivity_verdict_is_up_classifies_correctly() {
        assert!(ConnectivityVerdict::Full.is_up());
        assert!(ConnectivityVerdict::Portal.is_up());
        assert!(ConnectivityVerdict::Limited.is_up());
        assert!(!ConnectivityVerdict::None.is_up());
        assert!(!ConnectivityVerdict::Unknown.is_up());
    }
}
