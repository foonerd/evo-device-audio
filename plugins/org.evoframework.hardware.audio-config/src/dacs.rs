//! DAC catalogue — types + loader.
//!
//! The catalogue is a packaged JSON file (`data/dacs.json`, embedded
//! at build time, also installed at `/usr/share/evo-device-audio/dacs.json`
//! by the bootstrap script for OOP-shipped plugins to read at admission).
//! Each top-level device profile (e.g. `Raspberry PI`) holds a list of
//! per-DAC rows; rows carry the overlay token, ALSA card short id,
//! mixer-control hint, optional EEPROM-id list, and a needsreboot flag.
//!
//! The plugin loads the catalogue once at admission, holds it in
//! memory for the steward's lifetime, and re-loads on operator-
//! triggered refresh. Per-row filtering against the resolved board
//! profile is the [`dac_list_for_profile`] surface; lookup by id is
//! [`find_dac`].

use serde::{Deserialize, Deserializer};

/// The top-level dacs.json shape: a `devices` array, each with a
/// hardware profile name (e.g. "Raspberry PI") and a per-profile
/// list of DAC entries.
#[derive(Debug, Clone, Deserialize)]
pub struct DacsFile {
    /// Top-level list of hardware profiles. Each profile carries its
    /// own per-DAC list keyed by the host's board class name.
    pub devices: Vec<DacsHardware>,
}

/// One hardware profile from `devices[]`. The profile name keys the
/// catalogue against the host's board class.
#[derive(Debug, Clone, Deserialize)]
pub struct DacsHardware {
    /// Board class name (e.g. `"Raspberry PI"`, `"Sparky"`,
    /// `"Tinkerboard"`). Matches the resolved profile string the
    /// provider returns.
    pub name: String,
    /// Per-DAC entries for this profile.
    pub data: Vec<DacEntry>,
}

/// One DAC entry from `devices[].data[]`. Optional fields default to
/// the empty string when absent; the catalogue is hand-curated and
/// permissive — required fields are `id`, `name`, and (for boards
/// with a managed dtoverlay block) `overlay`.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct DacEntry {
    /// Catalogue id (e.g. `hifiberry-dacplus`). The operator's
    /// `select_dac` gesture references entries by this id.
    pub id: String,
    /// Human-readable display name (e.g. `"HiFiBerry DAC Plus"`).
    pub name: String,
    /// The `dtoverlay=` token to write into the managed block. Empty
    /// for module-only DACs (Sparky catalogue rows), which v1 does
    /// not support on `apply`.
    #[serde(default)]
    pub overlay: String,
    /// Legacy ALSA card-number hint. Not used directly — downstream
    /// resolves the real card index via `alsacard`.
    #[serde(default)]
    pub alsanum: String,
    /// Short ALSA id from `/proc/asound/cards` brackets (e.g.
    /// `sndrpihifiberry`). Used by downstream plugins to resolve the
    /// real card index at runtime — `alsanum` alone is a legacy
    /// hint and varies across Pi models.
    #[serde(default)]
    pub alsacard: String,
    /// Mixer control hint (e.g. `Digital`, `Master`, `PCM`). Empty
    /// string means the board has no in-DAC mixer; the operator's
    /// volume must be applied in software (MPD software-mixer) or
    /// not at all (downstream device handles it).
    #[serde(default)]
    pub mixer: String,
    /// Optional companion kernel modules to load. Stock dacs.json
    /// emits either an empty string or a JSON array of module
    /// names; both forms parse via [`deserialize_modules_loose`].
    #[serde(default, deserialize_with = "deserialize_modules_loose")]
    pub modules: String,
    /// Companion init script (relative to a packaged scripts
    /// directory). The plugin does NOT execute the script in v1 —
    /// it surfaces the value for downstream wardens that may.
    #[serde(default)]
    pub script: String,
    /// Optional ID EEPROM string(s) matched against the HAT EEPROM
    /// for auto-detect. Stock dacs.json uses either a single string
    /// or a JSON array; both parse via [`deserialize_eeprom_loose`].
    #[serde(default, deserialize_with = "deserialize_eeprom_loose")]
    pub eeprom_name: Vec<String>,
    /// I2C address hint (hex string like `4d`). Operators using
    /// auto-detect can probe this address with `i2cdetect` once the
    /// i2c-dev module is loaded.
    #[serde(default, alias = "i2c_address", alias = "i2c_adress")]
    pub i2c_address: String,
    /// `"yes"` / `"true"` / `"1"` — the catalogue's per-row
    /// needs-reboot flag. The plugin treats any non-empty truthy
    /// value as true; absent or empty as false.
    #[serde(default)]
    pub needsreboot: String,
}

impl DacEntry {
    /// Per-row needs-reboot flag, normalised to bool. Empty /
    /// missing → false; `"yes"` / `"true"` / `"1"` → true; anything
    /// else → false. The plugin's overall `pending_reboot` subject
    /// flips to true on every dtoverlay write regardless of this
    /// flag (the kernel will not load the new overlay without a
    /// reboot); the flag captures whether the DAC author wants the
    /// UI to surface a reboot prompt.
    pub fn needs_reboot(&self) -> bool {
        matches!(
            self.needsreboot.to_ascii_lowercase().as_str(),
            "yes" | "true" | "1"
        )
    }
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

/// Parse the embedded dacs.json bytes (or any equivalent string) into
/// a [`DacsFile`]. Returns the underlying serde error on malformed
/// JSON so callers can surface a structured diagnostic.
pub fn parse_dacs(raw: &str) -> Result<DacsFile, serde_json::Error> {
    serde_json::from_str(raw)
}

/// Return the per-row catalogue for a given board profile. Empty Vec
/// if the profile does not appear in the file (the calling plugin
/// surfaces this as "no catalogue for this board class").
pub fn dac_list_for_profile(dacs: &DacsFile, profile: &str) -> Vec<DacEntry> {
    dacs.devices
        .iter()
        .find(|d| d.name == profile)
        .map(|d| d.data.clone())
        .unwrap_or_default()
}

/// Lookup a single DAC entry by its catalogue id within a profile.
pub fn find_dac<'a>(
    dacs: &'a DacsFile,
    profile: &str,
    dac_id: &str,
) -> Option<&'a DacEntry> {
    let hw = dacs.devices.iter().find(|d| d.name == profile)?;
    hw.data.iter().find(|e| e.id == dac_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMBEDDED_DACS: &str = include_str!("../data/dacs.json");

    #[test]
    fn embedded_dacs_json_parses() {
        let parsed =
            parse_dacs(EMBEDDED_DACS).expect("embedded dacs.json parses");
        assert!(
            parsed
                .devices
                .iter()
                .any(|d| d.name == "Raspberry PI" && !d.data.is_empty()),
            "expected non-empty Raspberry PI profile"
        );
    }

    #[test]
    fn raspberry_pi_profile_has_known_entries() {
        let parsed = parse_dacs(EMBEDDED_DACS).expect("parse");
        let pi = dac_list_for_profile(&parsed, "Raspberry PI");
        assert!(!pi.is_empty(), "Raspberry PI profile non-empty");
        assert!(
            pi.iter().any(|e| e.id == "audiophonics-es9028q2m-dac"),
            "expected Audiophonics I-Sabre ES9028Q2M in catalogue"
        );
        assert!(
            pi.iter().any(|e| e.id == "hifiberry-dacplus"),
            "expected HiFiBerry DAC Plus in catalogue"
        );
    }

    #[test]
    fn find_dac_returns_expected_entry() {
        let parsed = parse_dacs(EMBEDDED_DACS).expect("parse");
        let entry = find_dac(&parsed, "Raspberry PI", "hifiberry-dacplus")
            .expect("entry resolves");
        assert_eq!(entry.name, "HiFiBerry DAC Plus");
        assert_eq!(entry.overlay, "hifiberry-dacplus");
        assert_eq!(entry.alsacard, "sndrpihifiberry");
        assert!(entry.needs_reboot());
    }

    #[test]
    fn find_dac_returns_none_for_unknown_id() {
        let parsed = parse_dacs(EMBEDDED_DACS).expect("parse");
        assert!(find_dac(&parsed, "Raspberry PI", "nonexistent-dac").is_none());
    }

    #[test]
    fn unknown_profile_returns_empty_list() {
        let parsed = parse_dacs(EMBEDDED_DACS).expect("parse");
        let unknown = dac_list_for_profile(&parsed, "GenericX86");
        assert!(unknown.is_empty());
    }

    #[test]
    fn modules_loose_deserialises_array_form() {
        let raw = r#"{"devices":[{"name":"Sparky","data":[{"id":"x","name":"Y","overlay":"o","modules":["snd-a","snd-b"]}]}]}"#;
        let parsed = parse_dacs(raw).expect("parse loose modules array");
        assert_eq!(parsed.devices[0].data[0].modules, "snd-a,snd-b");
    }

    #[test]
    fn eeprom_name_loose_deserialises_array_and_string() {
        let raw_array = r#"{"devices":[{"name":"X","data":[{"id":"a","name":"A","eeprom_name":["E1","E2"]}]}]}"#;
        let parsed_a = parse_dacs(raw_array).expect("parse");
        assert_eq!(parsed_a.devices[0].data[0].eeprom_name, vec!["E1", "E2"]);

        let raw_str = r#"{"devices":[{"name":"X","data":[{"id":"a","name":"A","eeprom_name":"Solo"}]}]}"#;
        let parsed_s = parse_dacs(raw_str).expect("parse");
        assert_eq!(parsed_s.devices[0].data[0].eeprom_name, vec!["Solo"]);
    }

    #[test]
    fn needs_reboot_truthy_variants() {
        let mut e = DacEntry {
            id: "x".into(),
            name: "Y".into(),
            overlay: String::new(),
            alsanum: String::new(),
            alsacard: String::new(),
            mixer: String::new(),
            modules: String::new(),
            script: String::new(),
            eeprom_name: Vec::new(),
            i2c_address: String::new(),
            needsreboot: "yes".into(),
        };
        assert!(e.needs_reboot());
        e.needsreboot = "TRUE".into();
        assert!(e.needs_reboot());
        e.needsreboot = "1".into();
        assert!(e.needs_reboot());
        e.needsreboot = "no".into();
        assert!(!e.needs_reboot());
        e.needsreboot = String::new();
        assert!(!e.needs_reboot());
    }
}
