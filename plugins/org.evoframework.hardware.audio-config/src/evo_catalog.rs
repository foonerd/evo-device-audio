//! Evo-native audio-hardware catalog — types + parser.
//!
//! The runtime catalog source-of-truth is `data/evo-catalog.toml`,
//! a build-time-generated artefact derived from the frozen Volumio
//! sources under `data/import/`. The runtime never parses Volumio
//! JSON shape; this module is the only catalog parser in the plugin
//! after ADR-0132 §Decision 1.
//!
//! Compared to Volumio's `dacs.json` row, the evo-native [`DacEntry`]
//! adds three load-bearing fields:
//!
//! * [`advanced_settings_enabled`] — per-DAC gate the UI honours
//!   for the modder workflow and advanced-DSP controls. Defaults
//!   to `true` in the reference (showcase) distribution; vendor
//!   distributions override either per-DAC or via the
//!   distribution-tier config flag.
//! * [`dsp_options`] — the per-DAC list of ALSA mixer-control names
//!   the DSP capability resolver surfaces. Joined with the curated
//!   pool + live `amixer cget` introspection in three-layer merge.
//! * [`provenance`] — the source-of-truth origin string (e.g.
//!   `volumio:dacs.json#hifiberry-dacplus`). The user-overlay
//!   catalog from the modder workflow uses a different scheme
//!   (e.g. `modder:operator-overlay-2026-05-20`).
//!
//! The Volumio-shape field names (`alsacard` / `alsanum` / `mixer`
//! / `needsreboot`) are translated to evo-native names at import
//! time (`alsa_card_hint` / `alsa_num_hint` / `in_card_mixer` /
//! `needs_reboot_on_apply`) so the runtime surface speaks the
//! framework's vocabulary.

use serde::{Deserialize, Serialize};

/// Top-level evo-native catalog.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvoCatalog {
    /// Catalog schema version. The plugin admits only the version
    /// it was built against; future versions land as a new shape
    /// alongside the existing one (foot-locked schema discipline).
    pub schema_version: u32,
    /// ISO-8601 timestamp recorded when the importer generated
    /// this catalog. Empty when the catalog was authored by hand
    /// (modder overlays).
    #[serde(default)]
    pub generated_at: String,
    /// Source files the importer read to produce this catalog.
    /// Empty when authored by hand. Used for provenance + the
    /// regression-guard test in `import.rs`.
    #[serde(default)]
    pub imported_from: Vec<String>,
    /// Per-board profiles. Each profile carries its own DAC list.
    #[serde(default)]
    pub boards: Vec<BoardProfile>,
}

impl EvoCatalog {
    /// Lookup helper — return the DAC list for the resolved board
    /// profile (e.g. `Raspberry PI`). Empty Vec if the profile
    /// does not appear in this catalog.
    pub fn dac_list_for_profile(&self, profile: &str) -> Vec<DacEntry> {
        self.boards
            .iter()
            .find(|b| b.name == profile)
            .map(|b| b.dacs.clone())
            .unwrap_or_default()
    }

    /// Lookup helper — find one DAC entry by id within a profile.
    /// Returns None when the profile or the id does not match.
    pub fn find_dac<'a>(
        &'a self,
        profile: &str,
        dac_id: &str,
    ) -> Option<&'a DacEntry> {
        let board = self.boards.iter().find(|b| b.name == profile)?;
        board.dacs.iter().find(|d| d.id == dac_id)
    }
}

/// One board profile from `boards[]`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BoardProfile {
    /// Board class name (e.g. `"Raspberry PI"`). Matches the
    /// resolved profile string the provider returns.
    pub name: String,
    /// Provider key the plugin uses to select the concrete
    /// [`HardwareAudioProvider`] implementation. Values: `"pi"`
    /// (PiProvider with dtoverlay management), `"noop"`
    /// (NoopProvider returning NotApplicable on writes). Future
    /// board classes add new variants.
    ///
    /// [`HardwareAudioProvider`]: crate::provider::HardwareAudioProvider
    pub provider: String,
    /// Per-DAC entries for this profile.
    #[serde(default)]
    pub dacs: Vec<DacEntry>,
}

/// One DAC entry. Evo-native shape — field names translated from
/// the Volumio source at import time so the runtime surface speaks
/// the framework's vocabulary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DacEntry {
    /// Catalog id (e.g. `hifiberry-dacplus`). Operator gestures
    /// reference entries by this id.
    pub id: String,
    /// Operator-readable display name (e.g. `"HiFiBerry DAC Plus"`).
    pub display_name: String,
    /// The `dtoverlay=` token written to the managed boot-config
    /// block. Empty for module-only DACs (Sparky catalog rows),
    /// which v1 does not support on `apply`.
    #[serde(default)]
    pub overlay: String,
    /// Short ALSA card identifier hint (e.g. `sndrpihifiberry`).
    /// Downstream plugins (delivery.alsa, playback.mpd) bind
    /// against this hint rather than re-running `aplay -L`.
    #[serde(default)]
    pub alsa_card_hint: String,
    /// Legacy ALSA card-number hint (Pi: 2, Tinkerboard: 0, …).
    /// Not load-bearing; surfaced for diagnostic output.
    #[serde(default)]
    pub alsa_num_hint: u8,
    /// In-DAC mixer-control hint (e.g. `Digital`, `Master`).
    /// Empty when the DAC has no in-card mixer; the operator's
    /// volume must then be applied in software or downstream.
    #[serde(default)]
    pub in_card_mixer: String,
    /// Companion kernel modules to load via modules-load.d.
    /// Empty Vec when no companion module is needed.
    #[serde(default)]
    pub companion_modules: Vec<String>,
    /// Companion init script filename (relative to a packaged
    /// scripts directory). The plugin does NOT execute the
    /// script in v1; the field is surfaced for downstream wardens
    /// that may.
    #[serde(default)]
    pub init_script: String,
    /// HAT ID-EEPROM product string(s) matched for auto-detect.
    /// Empty Vec when the DAC has no EEPROM identifier.
    #[serde(default)]
    pub eeprom_names: Vec<String>,
    /// I2C address hint (hex string, no prefix; e.g. `"4d"`).
    /// Empty when the DAC has no I2C bus presence.
    #[serde(default)]
    pub i2c_address: String,
    /// Whether the dtoverlay change requires a reboot to take
    /// effect. The plugin's overall `pending_reboot` subject
    /// flips on every write regardless; this flag drives the UI
    /// reboot-prompt affordance for THIS specific DAC selection.
    #[serde(default)]
    pub needs_reboot_on_apply: bool,
    /// Whether the operator (and the modder workflow) can apply
    /// advanced-settings gestures against this DAC. Showcase
    /// reference distribution default: `true`. Vendor distributions
    /// override at the distribution-tier config layer; per-DAC
    /// operator opt-in remains.
    #[serde(default = "default_advanced_settings_enabled")]
    pub advanced_settings_enabled: bool,
    /// ALSA mixer-control names the DSP capability resolver
    /// surfaces for this DAC. Joined with the curated control
    /// pool + live `amixer cget` introspection. Empty Vec means
    /// the DAC has no surfaceable DSP controls beyond
    /// [`in_card_mixer`].
    #[serde(default)]
    pub dsp_options: Vec<String>,
    /// Source-of-truth provenance string. Volumio-imported rows
    /// carry `volumio:dacs.json#<id>`; modder-overlay rows carry
    /// `modder:<overlay-name>`; hand-authored rows carry
    /// `hand-authored`. Surfaced for diagnostic output.
    #[serde(default)]
    pub provenance: String,
}

fn default_advanced_settings_enabled() -> bool {
    true
}

/// Failure variants returned by [`parse_evo_catalog`].
#[derive(Debug, thiserror::Error)]
pub enum EvoCatalogParseError {
    /// The supplied bytes did not parse as TOML.
    #[error("invalid TOML: {0}")]
    InvalidToml(String),
}

/// Parse the evo-native catalog from its TOML representation.
///
/// Returns the structured catalog on success; the only failure
/// mode is malformed TOML ([`EvoCatalogParseError::InvalidToml`]).
/// Missing optional fields default per the serde-default
/// declarations on the struct fields; required fields
/// (`schema_version`, board `name` + `provider`, DAC `id` +
/// `display_name`) refuse via the same variant if absent (the
/// TOML deserialiser surfaces the missing-field diagnostic in
/// its error string).
pub fn parse_evo_catalog(
    toml_str: &str,
) -> Result<EvoCatalog, EvoCatalogParseError> {
    toml::from_str(toml_str)
        .map_err(|e| EvoCatalogParseError::InvalidToml(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMBEDDED_CATALOG: &str = include_str!("../data/evo-catalog.toml");

    #[test]
    fn embedded_catalog_parses() {
        let cat = parse_evo_catalog(EMBEDDED_CATALOG)
            .expect("embedded catalog parses");
        assert_eq!(cat.schema_version, 1);
        assert!(
            !cat.boards.is_empty(),
            "at least one board profile expected"
        );
    }

    #[test]
    fn raspberry_pi_profile_has_known_entries() {
        let cat = parse_evo_catalog(EMBEDDED_CATALOG).expect("parse");
        let pi = cat.dac_list_for_profile("Raspberry PI");
        assert!(!pi.is_empty(), "Raspberry PI profile non-empty");
        assert!(
            pi.iter().any(|d| d.id == "audiophonics-es9028q2m-dac"),
            "expected Audiophonics I-Sabre ES9028Q2M in catalog"
        );
        assert!(
            pi.iter().any(|d| d.id == "hifiberry-dacplus"),
            "expected HiFiBerry DAC Plus in catalog"
        );
    }

    #[test]
    fn find_dac_returns_expected_entry() {
        let cat = parse_evo_catalog(EMBEDDED_CATALOG).expect("parse");
        let entry = cat
            .find_dac("Raspberry PI", "hifiberry-dacplus")
            .expect("entry resolves");
        assert_eq!(entry.display_name, "HiFiBerry DAC Plus");
        assert_eq!(entry.overlay, "hifiberry-dacplus");
        assert_eq!(entry.alsa_card_hint, "sndrpihifiberry");
        assert!(entry.needs_reboot_on_apply);
        assert!(entry.advanced_settings_enabled);
    }

    #[test]
    fn find_dac_returns_none_for_unknown_id() {
        let cat = parse_evo_catalog(EMBEDDED_CATALOG).expect("parse");
        assert!(cat.find_dac("Raspberry PI", "nonexistent-dac").is_none());
    }

    #[test]
    fn unknown_profile_returns_empty_list() {
        let cat = parse_evo_catalog(EMBEDDED_CATALOG).expect("parse");
        let unknown = cat.dac_list_for_profile("GenericX86");
        assert!(unknown.is_empty());
    }

    #[test]
    fn malformed_toml_surfaces_invalid_variant() {
        let err =
            parse_evo_catalog("this is not = valid TOML [[[").unwrap_err();
        assert!(matches!(err, EvoCatalogParseError::InvalidToml(_)));
    }

    #[test]
    fn known_dsp_options_attached_to_matching_dacs() {
        // After the importer joins dac_dsp.json into the catalog,
        // HiFiBerry DAC Plus carries the two controls Volumio
        // surfaces ("DSP Program", "Clock Missing Period"). This
        // pins the join contract: case-insensitive on display name.
        let cat = parse_evo_catalog(EMBEDDED_CATALOG).expect("parse");
        let entry = cat
            .find_dac("Raspberry PI", "hifiberry-dacplus")
            .expect("entry");
        assert!(entry.dsp_options.contains(&"DSP Program".to_string()));
        assert!(entry
            .dsp_options
            .contains(&"Clock Missing Period".to_string()));
    }

    #[test]
    fn provenance_string_carries_origin() {
        let cat = parse_evo_catalog(EMBEDDED_CATALOG).expect("parse");
        let entry = cat
            .find_dac("Raspberry PI", "hifiberry-dacplus")
            .expect("entry");
        assert_eq!(entry.provenance, "volumio:dacs.json#hifiberry-dacplus");
    }
}
