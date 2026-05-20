//! Volumio JSON → evo-native catalog importer.
//!
//! Build-time / developer-runnable conversion from the frozen
//! Volumio sources under `data/import/` to the runtime-load-bearing
//! evo-native `data/evo-catalog.toml`. The runtime never invokes
//! this module — it lives here so the `regen_evo_catalog` example
//! produces deterministic output AND so a regression-guard test
//! pins the importer's output byte-equal against the checked-in
//! golden catalog.
//!
//! The Volumio shape is not a runtime contract: this module is
//! the only place evo-device-audio understands Volumio's
//! `dacs.json` + `dac_dsp.json` shapes.

use std::collections::HashMap;

use serde::{Deserialize, Deserializer};

use crate::evo_catalog::{BoardProfile, DacEntry, EvoCatalog};

/// Generator timestamp used for every importer run. Constant so the
/// importer's output is byte-equal-deterministic — the checked-in
/// golden catalog matches every developer's locally-generated output
/// exactly. Bump when the importer logic changes meaningfully so
/// the golden + the test re-sync.
const IMPORTER_GENERATED_AT: &str = "2026-05-20T00:00:00Z";

const IMPORT_SOURCE_DACS: &str = "data/import/volumio-dacs.json";
const IMPORT_SOURCE_DAC_DSP: &str = "data/import/volumio-dac-dsp.json";

/// Failure variants returned by [`import_volumio_to_evo_catalog`].
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    /// `volumio-dacs.json` failed to parse as the expected Volumio
    /// shape (devices array of {name, data}).
    #[error("invalid volumio-dacs.json: {0}")]
    InvalidVolumioDacs(String),
    /// `volumio-dac-dsp.json` failed to parse as the expected Volumio
    /// shape (cards array of {name, dsp_options}).
    #[error("invalid volumio-dac-dsp.json: {0}")]
    InvalidVolumioDacDsp(String),
}

/// Convert the Volumio source JSONs to the evo-native catalog.
///
/// Pure function — no IO, no global state. Deterministic given the
/// inputs: re-running with identical bytes produces byte-identical
/// output (after `toml::to_string_pretty` serialisation). The
/// `regen_evo_catalog` example invokes this once with the embedded
/// import sources; the regression-guard test in this module's
/// [`tests`] section asserts the output matches the checked-in
/// `data/evo-catalog.toml` byte-equal.
pub fn import_volumio_to_evo_catalog(
    volumio_dacs_json: &str,
    volumio_dac_dsp_json: &str,
) -> Result<EvoCatalog, ImportError> {
    let v_dacs: VolumioDacsFile = serde_json::from_str(volumio_dacs_json)
        .map_err(|e| ImportError::InvalidVolumioDacs(e.to_string()))?;
    let v_dsp: VolumioDacDspFile =
        serde_json::from_str(volumio_dac_dsp_json)
            .map_err(|e| ImportError::InvalidVolumioDacDsp(e.to_string()))?;

    let dsp_lookup: HashMap<String, Vec<String>> = v_dsp
        .cards
        .into_iter()
        .map(|c| (c.name.to_ascii_lowercase(), c.dsp_options))
        .collect();

    let boards: Vec<BoardProfile> = v_dacs
        .devices
        .into_iter()
        .map(|device| {
            let provider = pick_provider(&device.name);
            let dacs: Vec<DacEntry> = device
                .data
                .into_iter()
                .map(|row| convert_row(row, &dsp_lookup))
                .collect();
            BoardProfile {
                name: device.name,
                provider,
                dacs,
            }
        })
        .collect();

    Ok(EvoCatalog {
        schema_version: 1,
        generated_at: IMPORTER_GENERATED_AT.to_string(),
        imported_from: vec![
            IMPORT_SOURCE_DACS.to_string(),
            IMPORT_SOURCE_DAC_DSP.to_string(),
        ],
        boards,
    })
}

/// Map a Volumio board-class name to the evo-native provider key.
/// Pi → `pi` (PiProvider with dtoverlay management);
/// Tinkerboard → `rockchip` (RockchipProvider against
/// `/boot/hw_intf.conf`); every other class → `noop` until a
/// concrete provider lands on the rig.
fn pick_provider(board_name: &str) -> String {
    match board_name {
        "Raspberry PI" => "pi".to_string(),
        "Tinkerboard" => "rockchip".to_string(),
        _ => "noop".to_string(),
    }
}

fn convert_row(
    row: VolumioDacRow,
    dsp_lookup: &HashMap<String, Vec<String>>,
) -> DacEntry {
    let dsp_options = dsp_lookup
        .get(&row.name.to_ascii_lowercase())
        .cloned()
        .unwrap_or_default();
    let alsa_num_hint: u8 = row.alsanum.parse().unwrap_or(0);
    let needs_reboot_on_apply = matches!(
        row.needsreboot.to_ascii_lowercase().as_str(),
        "yes" | "true" | "1"
    );
    let companion_modules = if row.modules.is_empty() {
        Vec::new()
    } else {
        row.modules
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };
    let provenance = format!("volumio:dacs.json#{}", row.id);
    DacEntry {
        id: row.id,
        display_name: row.name,
        overlay: row.overlay,
        alsa_card_hint: row.alsacard,
        alsa_num_hint,
        in_card_mixer: row.mixer,
        companion_modules,
        init_script: row.script,
        eeprom_names: row.eeprom_name,
        i2c_address: row.i2c_address,
        needs_reboot_on_apply,
        advanced_settings_enabled: true,
        dsp_options,
        provenance,
    }
}

// =============================================================
// Volumio-shape deserialisation (private to this module)
// =============================================================

#[derive(Debug, Deserialize)]
struct VolumioDacsFile {
    devices: Vec<VolumioDevice>,
}

#[derive(Debug, Deserialize)]
struct VolumioDevice {
    name: String,
    data: Vec<VolumioDacRow>,
}

#[derive(Debug, Deserialize)]
struct VolumioDacRow {
    id: String,
    name: String,
    #[serde(default)]
    overlay: String,
    #[serde(default)]
    alsanum: String,
    #[serde(default)]
    alsacard: String,
    #[serde(default)]
    mixer: String,
    #[serde(default, deserialize_with = "deserialize_modules_loose")]
    modules: String,
    #[serde(default)]
    script: String,
    #[serde(default, deserialize_with = "deserialize_eeprom_loose")]
    eeprom_name: Vec<String>,
    /// Volumio's source file mis-spells the field as `i2c_adress`
    /// in one row (hifiberry-dac2hd). Accept both spellings on the
    /// way in; emit canonical evo-native `i2c_address`.
    #[serde(default, alias = "i2c_adress")]
    i2c_address: String,
    #[serde(default)]
    needsreboot: String,
}

#[derive(Debug, Deserialize)]
struct VolumioDacDspFile {
    cards: Vec<VolumioDacDspCard>,
}

#[derive(Debug, Deserialize)]
struct VolumioDacDspCard {
    name: String,
    #[serde(default)]
    dsp_options: Vec<String>,
}

fn deserialize_modules_loose<'de, D>(
    deserializer: D,
) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(deserializer)?;
    Ok(match v {
        serde_json::Value::String(s) => s,
        serde_json::Value::Array(a) => a
            .into_iter()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .collect::<Vec<_>>()
            .join(","),
        serde_json::Value::Null => String::new(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Object(_) => String::new(),
    })
}

fn deserialize_eeprom_loose<'de, D>(
    deserializer: D,
) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(deserializer)?;
    Ok(match v {
        serde_json::Value::String(s) if !s.is_empty() => vec![s],
        serde_json::Value::String(_) => Vec::new(),
        serde_json::Value::Array(a) => a
            .into_iter()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const VOLUMIO_DACS_JSON: &str =
        include_str!("../data/import/volumio-dacs.json");
    const VOLUMIO_DAC_DSP_JSON: &str =
        include_str!("../data/import/volumio-dac-dsp.json");
    const EMBEDDED_GOLDEN: &str = include_str!("../data/evo-catalog.toml");

    #[test]
    fn importer_output_matches_checked_in_golden_byte_equal() {
        // The regression-guard contract: re-running the importer
        // against the frozen Volumio sources
        // produces byte-identical output to the checked-in golden.
        // Any drift in the importer logic OR the source JSONs OR
        // the catalog struct field order surfaces here; the
        // developer regenerates via `cargo run --example
        // regen_evo_catalog --package
        // org-evoframework-hardware-audio-config > data/evo-catalog.toml`.
        let catalog = import_volumio_to_evo_catalog(
            VOLUMIO_DACS_JSON,
            VOLUMIO_DAC_DSP_JSON,
        )
        .expect("import");
        let regenerated =
            toml::to_string_pretty(&catalog).expect("serialise to TOML");
        assert_eq!(
            regenerated, EMBEDDED_GOLDEN,
            "checked-in data/evo-catalog.toml drift detected; \
             regenerate via cargo run --example regen_evo_catalog"
        );
    }

    #[test]
    fn dsp_lookup_join_is_case_insensitive() {
        // Volumio dac_dsp.json uses "Hifiberry DAC Plus" (lowercase
        // 'f'); dacs.json uses "HiFiBerry DAC Plus". The importer
        // must join case-insensitively or the HiFiBerry DAC Plus
        // row loses its DSP options. Pin the contract.
        let catalog = import_volumio_to_evo_catalog(
            VOLUMIO_DACS_JSON,
            VOLUMIO_DAC_DSP_JSON,
        )
        .expect("import");
        let entry = catalog
            .find_dac("Raspberry PI", "hifiberry-dacplus")
            .expect("hifiberry-dacplus entry");
        assert_eq!(
            entry.dsp_options,
            vec!["DSP Program", "Clock Missing Period"]
        );
    }

    #[test]
    fn provider_picked_per_board_class() {
        let catalog = import_volumio_to_evo_catalog(
            VOLUMIO_DACS_JSON,
            VOLUMIO_DAC_DSP_JSON,
        )
        .expect("import");
        let pi = catalog
            .boards
            .iter()
            .find(|b| b.name == "Raspberry PI")
            .expect("Pi board");
        assert_eq!(pi.provider, "pi");
        let tinker = catalog
            .boards
            .iter()
            .find(|b| b.name == "Tinkerboard")
            .expect("Tinkerboard board");
        assert_eq!(tinker.provider, "rockchip");
        // Every other class (Odroid C1+, Sparky) still resolves
        // to noop until a concrete provider lands on the rig.
        for board in &catalog.boards {
            if board.name != "Raspberry PI" && board.name != "Tinkerboard" {
                assert_eq!(
                    board.provider, "noop",
                    "board {} should default to noop provider until concrete impl lands",
                    board.name
                );
            }
        }
    }

    #[test]
    fn malformed_dacs_json_surfaces_structured_error() {
        let err = import_volumio_to_evo_catalog(
            "this is not valid json",
            VOLUMIO_DAC_DSP_JSON,
        )
        .unwrap_err();
        assert!(matches!(err, ImportError::InvalidVolumioDacs(_)));
    }

    #[test]
    fn malformed_dac_dsp_json_surfaces_structured_error() {
        let err =
            import_volumio_to_evo_catalog(VOLUMIO_DACS_JSON, "not valid json")
                .unwrap_err();
        assert!(matches!(err, ImportError::InvalidVolumioDacDsp(_)));
    }

    #[test]
    fn needs_reboot_truthy_variants_parse() {
        // The Volumio source uses "yes" / occasionally "no";
        // exercise the loose-parse contract.
        let json = r#"{"devices":[{"name":"X","data":[
            {"id":"a","name":"A","needsreboot":"yes"},
            {"id":"b","name":"B","needsreboot":"YES"},
            {"id":"c","name":"C","needsreboot":"no"},
            {"id":"d","name":"D","needsreboot":""}
        ]}]}"#;
        let cat = import_volumio_to_evo_catalog(json, "{\"cards\":[]}")
            .expect("import");
        let board = &cat.boards[0];
        assert!(board.dacs[0].needs_reboot_on_apply);
        assert!(board.dacs[1].needs_reboot_on_apply);
        assert!(!board.dacs[2].needs_reboot_on_apply);
        assert!(!board.dacs[3].needs_reboot_on_apply);
    }

    #[test]
    fn advanced_settings_default_true_for_showcase() {
        // Showcase distribution default. Vendor distributions
        // override at the distribution-tier config layer (lands
        // with the modder workflow in P2).
        let catalog = import_volumio_to_evo_catalog(
            VOLUMIO_DACS_JSON,
            VOLUMIO_DAC_DSP_JSON,
        )
        .expect("import");
        for board in &catalog.boards {
            for dac in &board.dacs {
                assert!(
                    dac.advanced_settings_enabled,
                    "{} should default to advanced_settings_enabled=true",
                    dac.id
                );
            }
        }
    }
}
