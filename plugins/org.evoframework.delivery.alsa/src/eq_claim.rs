// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Which zone's EQ is heard on the one pipe.
//!
//! The delivery plugin publishes one subject,
//! `evo.audio.zones` / `eq_claim`. The claim is true only while
//! one physical output is the chain terminus — the output
//! `delivery.play_output` treats as playing — and exactly one
//! zone with `engaged` true holds a logical output that points
//! at that terminus. A silent terminus does not claim. A zone
//! with `engaged` false does not claim. Two engaged zones on the
//! same terminus do not claim: the composition plugin then uses
//! the options EQ, as it does when the subject is absent.
//!
//! This module decides; `lib.rs` publishes. Nothing here renders
//! anything: `eq_only` in composition.alsa stays the one cascade
//! and chooses its bands from this claim.

use serde_json::{json, Value};

use crate::logical_outputs::LogicalOutput;
use crate::zones::{EqBand, Zone};

/// Payload version of the claim subject.
pub(crate) const EQ_CLAIM_VERSION: u32 = 1;

/// The claim as published.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum EqClaim {
    /// No zone's EQ is heard; the options EQ is.
    None,
    /// This zone's EQ is heard, engaged, with these bands.
    Claimed {
        /// The one engaged zone on the terminus.
        zone_id: String,
        /// Its ten bands.
        bands: Vec<EqBand>,
    },
}

impl EqClaim {
    /// The wire shape: `{ v: 1, claimed: false }` or
    /// `{ v: 1, claimed: true, zone_id, engaged: true, bands }`.
    pub(crate) fn to_wire(&self) -> Value {
        match self {
            EqClaim::None => json!({
                "v": EQ_CLAIM_VERSION,
                "claimed": false,
            }),
            EqClaim::Claimed { zone_id, bands } => json!({
                "v": EQ_CLAIM_VERSION,
                "claimed": true,
                "zone_id": zone_id,
                "engaged": true,
                "bands": bands,
            }),
        }
    }
}

/// The claim for a terminus, given the logical outputs and the
/// zones. `terminus` is the `hw:N,M` id the chain ends at, or
/// `None` when it is silent.
pub(crate) fn compute_claim(
    terminus: Option<&str>,
    logical: &[LogicalOutput],
    zones: &[Zone],
) -> EqClaim {
    let Some(terminus) = terminus else {
        return EqClaim::None;
    };
    // The logical outputs that point at the terminus.
    let on_terminus: Vec<&str> = logical
        .iter()
        .filter(|l| l.output_id == terminus)
        .map(|l| l.id.as_str())
        .collect();
    if on_terminus.is_empty() {
        return EqClaim::None;
    }
    // The engaged zones that hold one of them.
    let mut claimants = zones.iter().filter(|z| {
        z.eq.engaged
            && z.logical_output_ids
                .iter()
                .any(|id| on_terminus.contains(&id.as_str()))
    });
    match (claimants.next(), claimants.next()) {
        (Some(zone), None) => EqClaim::Claimed {
            zone_id: zone.id.clone(),
            bands: zone.eq.bands.clone(),
        },
        // No engaged zone, or more than one: the options EQ.
        _ => EqClaim::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zones::ZoneEq;

    fn logical(id: &str, output_id: &str) -> LogicalOutput {
        LogicalOutput {
            id: id.to_string(),
            name: id.to_string(),
            output_id: output_id.to_string(),
        }
    }

    fn zone(id: &str, ids: &[&str], engaged: bool, gain0: f32) -> Zone {
        let mut eq = ZoneEq {
            engaged,
            ..ZoneEq::default()
        };
        eq.bands[0].gain_db = gain0;
        Zone {
            id: id.to_string(),
            name: id.to_string(),
            logical_output_ids: ids.iter().map(|s| s.to_string()).collect(),
            eq,
        }
    }

    fn fixtures() -> (Vec<LogicalOutput>, Vec<Zone>) {
        (
            vec![
                logical("logical:1", "hw:0,0"),
                logical("logical:2", "hw:1,0"),
                logical("logical:3", "hw:0,0"),
            ],
            vec![
                zone("zone:1", &["logical:1"], true, 3.0),
                zone("zone:2", &["logical:2"], true, -3.0),
            ],
        )
    }

    /// One engaged zone on the terminus claims with its bands;
    /// the terminus decides which.
    #[test]
    fn one_engaged_zone_on_the_terminus_claims() {
        let (l, z) = fixtures();
        match compute_claim(Some("hw:0,0"), &l, &z) {
            EqClaim::Claimed { zone_id, bands } => {
                assert_eq!(zone_id, "zone:1");
                assert_eq!(bands.len(), 10);
                assert_eq!(bands[0].gain_db, 3.0);
            }
            other => panic!("{other:?}"),
        }
        match compute_claim(Some("hw:1,0"), &l, &z) {
            EqClaim::Claimed { zone_id, bands } => {
                assert_eq!(zone_id, "zone:2");
                assert_eq!(bands[0].gain_db, -3.0);
            }
            other => panic!("{other:?}"),
        }
    }

    /// A silent terminus, a terminus no logical output points
    /// at, and a terminus whose zone is not engaged do not claim.
    #[test]
    fn silent_unnamed_and_unengaged_do_not_claim() {
        let (l, mut z) = fixtures();
        assert_eq!(compute_claim(None, &l, &z), EqClaim::None);
        assert_eq!(compute_claim(Some("hw:2,0"), &l, &z), EqClaim::None);
        z[0].eq.engaged = false;
        assert_eq!(compute_claim(Some("hw:0,0"), &l, &z), EqClaim::None);
        let no_zones: Vec<Zone> = Vec::new();
        assert_eq!(compute_claim(Some("hw:0,0"), &l, &no_zones), EqClaim::None);
    }

    /// Two engaged zones on one terminus do not claim; disengage
    /// one and the other claims.
    #[test]
    fn two_engaged_zones_on_one_terminus_do_not_claim() {
        let (l, mut z) = fixtures();
        z.push(zone("zone:3", &["logical:3"], true, 6.0));
        assert_eq!(compute_claim(Some("hw:0,0"), &l, &z), EqClaim::None);
        z[0].eq.engaged = false;
        match compute_claim(Some("hw:0,0"), &l, &z) {
            EqClaim::Claimed { zone_id, .. } => assert_eq!(zone_id, "zone:3"),
            other => panic!("{other:?}"),
        }
        // An engaged zone on another terminus does not count.
        z[0].eq.engaged = true;
        z[1].eq.engaged = true;
        assert_eq!(
            compute_claim(Some("hw:1,0"), &l, &z).to_wire()["zone_id"],
            "zone:2"
        );
    }

    /// The wire shapes are the two the composition plugin reads.
    #[test]
    fn the_wire_shape_is_the_two_forms() {
        assert_eq!(
            EqClaim::None.to_wire(),
            json!({ "v": 1, "claimed": false })
        );
        let (l, z) = fixtures();
        let w = compute_claim(Some("hw:0,0"), &l, &z).to_wire();
        assert_eq!(w["v"], 1);
        assert_eq!(w["claimed"], true);
        assert_eq!(w["zone_id"], "zone:1");
        assert_eq!(w["engaged"], true);
        assert_eq!(w["bands"].as_array().unwrap().len(), 10);
        assert_eq!(
            w["bands"][0],
            json!({ "freq_hz": 1000, "gain_db": 3.0, "q": 1.0 })
        );
    }
}
