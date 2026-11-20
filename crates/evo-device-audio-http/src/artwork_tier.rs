// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Which artwork verbs reach the network, and who may call them.
//!
//! The `artwork.providers` shelf is co-occupied by a local plugin
//! (reads files already on the device) and an online plugin
//! (fetches from third-party services). The paint of a browse
//! tile is on the operator's clock; a third-party service is not.
//! Dispatching the second from the first is what turned a
//! fifty-tile view into a forty-seven-second stall.
//!
//! This module is the single statement of the boundary:
//!
//! - [`NETWORK_TIER_REQUEST_TYPES`] is the **set** of request
//!   types served by a plugin that declares `outbound_network`.
//!   It is a set, not one provider's name: a verb a future plugin
//!   adds to the shelf is a member the day that plugin declares
//!   the capability, and the drift pin in this module's tests
//!   fails until it is listed.
//! - [`CascadeMode`] says whose clock a cascade run is on. A
//!   paint-triggered run is refused every member of the set; an
//!   enrichment run is not.
//!
//! The marker lives here, not in a plugin manifest: the manifest
//! schema is the framework's and is closed to unknown keys. The
//! manifest's existing `outbound_network` flag is the source of
//! truth, and the pin ties this set to it.

/// Request types on the `artwork.providers` shelf that reach a
/// third-party service (or clear the cache of the plugin that
/// does). Never dispatched from a paint.
pub const NETWORK_TIER_REQUEST_TYPES: &[&str] = &[
    "artwork.resolve_online",
    "artwork.resolve_artist_artwork",
    "artwork.resolve_artist_online",
    "artwork.online.clear_cache",
];

/// Whether `request_type` belongs to the network tier.
pub fn is_network_tier(request_type: &str) -> bool {
    NETWORK_TIER_REQUEST_TYPES.contains(&request_type)
}

/// Whose clock a cascade run is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CascadeMode {
    /// Triggered by a paint. Local tier only; the network tier is
    /// refused and deferred to enrichment.
    Paint,
    /// Triggered by the background enrichment path. Runs the full
    /// local-then-network cascade.
    Enrich,
}

impl CascadeMode {
    /// Whether a run in this mode may dispatch `request_type`.
    pub fn permits(self, request_type: &str) -> bool {
        match self {
            CascadeMode::Enrich => true,
            CascadeMode::Paint => !is_network_tier(request_type),
        }
    }
}

/// Fetcher-error prefix for "local tier exhausted, network tier
/// deferred". Classified to a fast NotFound that carries
/// [`PROVENANCE_LOCAL_EXHAUSTED_PENDING_ENRICH`].
pub const PENDING_ENRICH_PREFIX: &str = "artwork enrichment pending:";

/// `x-artwork-provider` value on a 404 served because the local
/// tier is exhausted and the network tier has not been asked.
pub const PROVENANCE_LOCAL_EXHAUSTED_PENDING_ENRICH: &str =
    "local_exhausted_pending_enrich";

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::Path;

    /// Every `artwork.providers` request type of a plugin that
    /// declares `outbound_network = true`, across every plugin in
    /// the workspace, read from the shipped manifests.
    fn manifest_network_request_types() -> (BTreeSet<String>, BTreeSet<String>)
    {
        let plugins =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
        let mut network = BTreeSet::new();
        let mut local = BTreeSet::new();
        for entry in std::fs::read_dir(&plugins).expect("plugins dir") {
            let manifest = entry.unwrap().path().join("manifest.toml");
            let Ok(text) = std::fs::read_to_string(&manifest) else {
                continue;
            };
            let doc: toml::Value = toml::from_str(&text)
                .unwrap_or_else(|e| panic!("{}: {e}", manifest.display()));
            let outbound = doc
                .get("prerequisites")
                .and_then(|p| p.get("outbound_network"))
                .and_then(toml::Value::as_bool)
                .unwrap_or(false);
            let Some(stockings) =
                doc.get("stockings").and_then(toml::Value::as_array)
            else {
                continue;
            };
            for s in stockings {
                if s.get("shelf").and_then(toml::Value::as_str)
                    != Some("artwork.providers")
                {
                    continue;
                }
                let types = s
                    .get("request_types")
                    .and_then(toml::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(toml::Value::as_str)
                    .map(String::from);
                if outbound {
                    network.extend(types);
                } else {
                    local.extend(types);
                }
            }
        }
        (network, local)
    }

    /// The marker set is the manifests' own claim, not a second
    /// opinion. A verb added to an `outbound_network` plugin on
    /// the artwork shelf that is not listed would be dispatchable
    /// from a paint; a local verb listed here would be refused
    /// from one. Either fails here, at build, not on the glass.
    #[test]
    fn the_network_set_matches_the_manifests() {
        let (network, local) = manifest_network_request_types();
        assert!(
            !network.is_empty(),
            "no outbound_network plugin found on artwork.providers: the \
             pin is reading nothing"
        );
        let marked: BTreeSet<String> = NETWORK_TIER_REQUEST_TYPES
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            marked, network,
            "NETWORK_TIER_REQUEST_TYPES must equal the artwork.providers \
             request types of every outbound_network plugin"
        );
        for rt in &local {
            assert!(!is_network_tier(rt), "local verb {rt} is marked network");
        }
    }

    #[test]
    fn a_paint_is_refused_every_network_verb() {
        for rt in NETWORK_TIER_REQUEST_TYPES {
            assert!(!CascadeMode::Paint.permits(rt), "{rt}");
            assert!(CascadeMode::Enrich.permits(rt), "{rt}");
        }
        assert!(CascadeMode::Paint.permits("artwork.resolve"));
    }
}
