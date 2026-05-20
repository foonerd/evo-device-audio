//! Curated DSP control pool — types + loader.
//!
//! The pool is the middle layer of the three-layer DSP capability
//! resolver: per-DAC catalog `dsp_options[]` list → curated pool
//! entry (this module) → live `amixer cget` introspection. Pool
//! entries carry the operator-facing metadata the runtime cannot
//! recover from amixer alone — human label, recommended default,
//! apply-semantics, description.
//!
//! Unknown controls (declared in catalog but not present in this
//! pool) surface in the runtime `dsp_capabilities` subject with the
//! raw mixer-control name as the human label and
//! `unbound_pool_entry = true`. They are never silently dropped.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Top-level pool shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DspControlPool {
    /// Pool schema version. Pinned at 1 for this release line.
    pub schema_version: u32,
    /// Per-control schema entries.
    #[serde(default)]
    pub controls: Vec<DspControlEntry>,
}

impl DspControlPool {
    /// Lookup a control entry by its exact ALSA mixer-control
    /// name. Case-sensitive: the ALSA mixer's namespace is
    /// case-sensitive, and the catalog `dsp_options[]` carries
    /// the verbatim mixer-control name.
    pub fn lookup<'a>(
        &'a self,
        alsa_control_name: &str,
    ) -> Option<&'a DspControlEntry> {
        self.controls.iter().find(|c| c.name == alsa_control_name)
    }

    /// Build a lookup map keyed by control name. Convenience for
    /// callers that need many lookups against the same pool
    /// instance (e.g. the resolver iterating a DAC's
    /// `dsp_options[]`).
    pub fn build_lookup(&self) -> HashMap<&str, &DspControlEntry> {
        self.controls.iter().map(|c| (c.name.as_str(), c)).collect()
    }
}

/// One curated control entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DspControlEntry {
    /// Exact ALSA mixer-control name. Case-sensitive. This is
    /// the lookup key joining the catalog's `dsp_options[]` to
    /// the pool entry; mismatches surface as `unbound_pool_entry`
    /// at the resolver layer.
    pub name: String,
    /// Operator-readable label (e.g. `"FIR Filter (anti-alias)"`).
    /// UI surfaces this in the DSP-controls form; falls back to
    /// the raw `name` when the pool lookup misses.
    pub human_label: String,
    /// Type discriminator the UI uses to pick the appropriate
    /// widget. See [`ControlType`].
    #[serde(rename = "type")]
    pub control_type: ControlType,
    /// For `enum` controls: the union of values the pool author
    /// knows about. Runtime amixer introspection narrows this to
    /// the values the bound card actually exposes. Empty for
    /// non-enum types.
    #[serde(default)]
    pub known_values: Vec<String>,
    /// Recommended default value, encoded as a JSON value. Allows
    /// strings (for enums), numbers (for integers + db_scale),
    /// and booleans uniformly. Surfaced by the UI as the "reset
    /// to recommended" affordance.
    #[serde(default)]
    pub recommended_default: serde_json::Value,
    /// Apply-semantics: whether the control takes effect on the
    /// next ALSA frame (`hot_apply`) or whether the steward must
    /// restart the audio chain (`requires_restart`).
    pub apply_semantics: ApplySemantics,
    /// Operator-facing description. Surfaced as the tooltip /
    /// help-text in the UI form.
    #[serde(default)]
    pub description: String,
    /// Source of the curated entry. `volumio:dac_dsp.json` for
    /// entries derived from the Volumio import; `evo:hat-survey-*`
    /// for entries curated from contemporary HAT documentation;
    /// `alsa:hardware-mixer-baseline` for the canonical hardware-
    /// mixer controls (Master / Digital / PCM).
    #[serde(default)]
    pub provenance_seed: String,
}

/// Control-type discriminator the UI uses to render the right widget.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ControlType {
    /// Enumerated value from a finite set. UI renders as dropdown.
    Enum,
    /// Integer value, optionally constrained by the runtime
    /// amixer `cget` range. UI renders as number input + slider.
    Integer,
    /// Boolean on/off. UI renders as toggle switch.
    Boolean,
    /// Integer encoded as a dB scale (typically the ALSA mixer
    /// volume controls). UI renders as a vertical slider with
    /// dB-scaled tick marks.
    DbScale,
}

/// Apply-semantics discriminator.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApplySemantics {
    /// Change takes effect on the next ALSA frame. No restart
    /// required. The vast majority of controls; safe for live
    /// operator gestures.
    HotApply,
    /// Change takes effect only after a restart of the audio
    /// chain. UI must surface this as an explicit "restart now"
    /// affordance after the gesture; the operator confirms
    /// before the change is actuated.
    RequiresRestart,
}

/// Failure variants returned by [`parse_dsp_control_pool`].
#[derive(Debug, thiserror::Error)]
pub enum DspControlPoolParseError {
    /// The supplied bytes did not parse as TOML.
    #[error("invalid TOML: {0}")]
    InvalidToml(String),
}

/// Parse the curated DSP control pool from its TOML representation.
pub fn parse_dsp_control_pool(
    toml_str: &str,
) -> Result<DspControlPool, DspControlPoolParseError> {
    toml::from_str(toml_str)
        .map_err(|e| DspControlPoolParseError::InvalidToml(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evo_catalog::parse_evo_catalog;
    use std::collections::HashSet;

    const EMBEDDED_POOL: &str = include_str!("../data/dsp-control-pool.toml");
    const EMBEDDED_CATALOG: &str = include_str!("../data/evo-catalog.toml");

    #[test]
    fn embedded_pool_parses() {
        let pool = parse_dsp_control_pool(EMBEDDED_POOL)
            .expect("embedded pool parses");
        assert_eq!(pool.schema_version, 1);
        assert!(
            pool.controls.len() >= 25,
            "expected the shipped pool to carry at least ~25 controls; found {}",
            pool.controls.len()
        );
    }

    #[test]
    fn pool_lookup_resolves_known_control() {
        let pool = parse_dsp_control_pool(EMBEDDED_POOL).expect("parse");
        let entry = pool
            .lookup("DSP Program")
            .expect("DSP Program is a load-bearing entry");
        assert_eq!(entry.human_label, "DSP Program");
        assert!(matches!(entry.control_type, ControlType::Enum));
        assert!(matches!(entry.apply_semantics, ApplySemantics::HotApply));
        assert!(entry.known_values.contains(&"None".to_string()));
    }

    #[test]
    fn pool_lookup_misses_on_unknown_control() {
        let pool = parse_dsp_control_pool(EMBEDDED_POOL).expect("parse");
        assert!(pool.lookup("Nonexistent Control").is_none());
    }

    #[test]
    fn pool_lookup_is_case_sensitive() {
        // ALSA mixer-control namespace is case-sensitive; the pool
        // matches verbatim. "dsp program" (lowercase) is a
        // different control name from "DSP Program".
        let pool = parse_dsp_control_pool(EMBEDDED_POOL).expect("parse");
        assert!(pool.lookup("dsp program").is_none());
        assert!(pool.lookup("DSP Program").is_some());
    }

    #[test]
    fn every_catalog_dsp_option_resolves_in_pool() {
        // Unknown controls (declared in catalog but absent from
        // pool) surface as `unbound_pool_entry` at the resolver
        // layer — they are not silent failures. This test pins
        // that EVERY DSP option
        // currently referenced by ANY catalog DAC has a matching
        // pool entry; new catalog entries that introduce a control
        // not in the pool fail this gate and force the pool author
        // to add the entry.
        let catalog =
            parse_evo_catalog(EMBEDDED_CATALOG).expect("catalog parse");
        let pool = parse_dsp_control_pool(EMBEDDED_POOL).expect("pool parse");
        let pool_names: HashSet<&str> =
            pool.controls.iter().map(|c| c.name.as_str()).collect();

        let mut unresolved: Vec<String> = Vec::new();
        for board in &catalog.boards {
            for dac in &board.dacs {
                for option in &dac.dsp_options {
                    if !pool_names.contains(option.as_str()) {
                        unresolved.push(format!(
                            "{}#{}: {}",
                            board.name, dac.id, option
                        ));
                    }
                }
            }
        }
        assert!(
            unresolved.is_empty(),
            "catalog declares DSP options not present in dsp-control-pool.toml; \
             add curated entries or accept the unbound_pool_entry surface: {unresolved:?}"
        );
    }

    #[test]
    fn build_lookup_returns_all_controls() {
        let pool = parse_dsp_control_pool(EMBEDDED_POOL).expect("parse");
        let lookup = pool.build_lookup();
        assert_eq!(lookup.len(), pool.controls.len());
        assert!(lookup.contains_key("DSP Program"));
        assert!(lookup.contains_key("Master"));
    }

    #[test]
    fn malformed_toml_surfaces_invalid_variant() {
        let err = parse_dsp_control_pool("not [[[ valid TOML").unwrap_err();
        assert!(matches!(err, DspControlPoolParseError::InvalidToml(_)));
    }

    #[test]
    fn control_type_round_trip() {
        for ct in [
            ControlType::Enum,
            ControlType::Integer,
            ControlType::Boolean,
            ControlType::DbScale,
        ] {
            let s = serde_json::to_string(&ct).expect("serialize");
            let parsed: ControlType =
                serde_json::from_str(&s).expect("deserialize");
            assert_eq!(parsed, ct);
        }
    }

    #[test]
    fn apply_semantics_round_trip() {
        for sem in [ApplySemantics::HotApply, ApplySemantics::RequiresRestart] {
            let s = serde_json::to_string(&sem).expect("serialize");
            let parsed: ApplySemantics =
                serde_json::from_str(&s).expect("deserialize");
            assert_eq!(parsed, sem);
        }
    }

    #[test]
    fn requires_restart_controls_documented() {
        // Pin that the pool carries at least one requires_restart
        // control so the UI's apply-semantics surface is exercised
        // by real data. The I2S Format / Bit Clock Polarity entries
        // currently fill this role; if removed, replace with
        // another genuine restart-required control.
        let pool = parse_dsp_control_pool(EMBEDDED_POOL).expect("parse");
        assert!(
            pool.controls.iter().any(|c| matches!(
                c.apply_semantics,
                ApplySemantics::RequiresRestart
            )),
            "pool should expose at least one requires_restart control"
        );
    }
}
