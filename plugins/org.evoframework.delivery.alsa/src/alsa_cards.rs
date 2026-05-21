#![allow(missing_docs)]

// Static ALSA card-name catalog.
//
// Maps the raw card name `aplay -l` prints (the bracketed token,
// e.g. `bcm2835 ALSA` or `snd_rpi_hifiberry_dacplus`) to an
// operator-friendly label, a type hint (i2s vs integrated), and
// the per-card default ALSA mixer control. Multi-device cards
// (one card with multiple subdevices addressable by `hw:N,M`)
// carry per-subdevice labels.
//
// The catalog data ships in `data/alsa-cards.toml` and is baked
// into the plugin binary at compile time via `include_str!`.
// Operator-supplied additions are not part of this surface; new
// rows ship in framework releases.

use serde::Deserialize;
use std::collections::HashMap;
use thiserror::Error;

/// Embedded catalog source. Updated by framework releases.
const CATALOG_TOML: &str = include_str!("../data/alsa-cards.toml");

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("alsa-cards.toml parse failed: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("alsa-cards.toml schema_version {found} unsupported (expected 1)")]
    SchemaVersion { found: u32 },
}

/// Top-level catalog payload as it appears on disk.
#[derive(Debug, Clone, Deserialize)]
struct CatalogFile {
    schema_version: u32,
    cards: Vec<RawCardRow>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawCardRow {
    /// Raw ALSA card name as `aplay -l` prints it.
    name: String,
    /// Operator-friendly label for single-device cards.
    #[serde(default)]
    pretty_name: Option<String>,
    /// Default ALSA mixer control name. Empty / absent = no
    /// default; the resolver may probe `amixer -c <card>`.
    #[serde(default)]
    default_mixer: Option<String>,
    /// `"i2s"` or `"integrated"`. Absent on a few volumio-era
    /// rows (multidevice variants where the discriminator lives
    /// on the per-device row instead).
    #[serde(default)]
    type_hint: Option<String>,
    /// Per-subdevice rows for multi-device cards. Absent for
    /// single-device cards.
    #[serde(default)]
    devices: Vec<RawDeviceRow>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawDeviceRow {
    /// `aplay -l` device number (the M in `hw:N,M`).
    index: u32,
    /// Operator-friendly label for this subdevice.
    pretty_name: String,
    /// Default ALSA mixer control name for this subdevice.
    #[serde(default)]
    default_mixer: Option<String>,
    /// Volumio's `ignore` flag — UI hides the entry.
    #[serde(default)]
    hidden: bool,
    /// Volumio's `ignoreGenmixer` flag — operator should not be
    /// offered the generic-mixer fallback even if amixer reports
    /// controls.
    #[serde(default)]
    ignore_generic_mixer: bool,
}

/// Resolved catalog entry returned to consumers. Either a
/// single-device row (one card → one label) or a multi-device
/// row (one card → many labels keyed by subdevice index).
#[derive(Debug, Clone)]
pub enum CardEntry {
    /// Single-device card.
    Single(SingleCard),
    /// Multi-device card (e.g. HDMI with multiple outputs).
    MultiDevice(MultiDeviceCard),
}

#[derive(Debug, Clone)]
pub struct SingleCard {
    pub pretty_name: String,
    pub default_mixer: Option<String>,
    pub type_hint: TypeHint,
}

#[derive(Debug, Clone)]
pub struct MultiDeviceCard {
    pub type_hint: TypeHint,
    pub devices: HashMap<u32, DeviceEntry>,
}

#[derive(Debug, Clone)]
pub struct DeviceEntry {
    pub pretty_name: String,
    pub default_mixer: Option<String>,
    pub hidden: bool,
    pub ignore_generic_mixer: bool,
}

/// Coarse ALSA card classification carried in the catalog. The
/// finer-grained `OutputClass` (HDMI / Analog / SPDIF / USB /
/// Bluetooth) is derived at runtime from `pretty_name` and card
/// name keywords; the catalog only ships the coarse i2s /
/// integrated discriminator the predecessor JSON shape carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeHint {
    /// I2S DAC HAT (Raspberry Pi-style add-on board).
    I2s,
    /// Integrated audio (HDMI, headphones, analog jack, USB,
    /// onboard SPDIF). Sub-classification derives at runtime.
    Integrated,
    /// Absent in source row.
    Unspecified,
}

impl TypeHint {
    fn from_str(raw: &str) -> Self {
        match raw.to_ascii_lowercase().as_str() {
            "i2s" => Self::I2s,
            "integrated" => Self::Integrated,
            _ => Self::Unspecified,
        }
    }
}

/// Loaded catalog indexed by raw ALSA card name.
#[derive(Debug, Clone)]
pub struct AlsaCardCatalog {
    by_name: HashMap<String, CardEntry>,
}

impl AlsaCardCatalog {
    /// Load the baked-in catalog. Returns an error if the
    /// embedded TOML fails to parse or carries an unsupported
    /// schema_version; both are admission-time failures because
    /// they indicate a framework-release shipping defect.
    pub fn load_embedded() -> Result<Self, CatalogError> {
        Self::from_toml_str(CATALOG_TOML)
    }

    /// Parse a TOML payload into an indexed catalog. Exposed for
    /// tests against synthetic inputs; production callers use
    /// `load_embedded`.
    pub fn from_toml_str(raw: &str) -> Result<Self, CatalogError> {
        let parsed: CatalogFile = toml::from_str(raw)?;
        if parsed.schema_version != 1 {
            return Err(CatalogError::SchemaVersion {
                found: parsed.schema_version,
            });
        }
        let mut by_name: HashMap<String, CardEntry> = HashMap::new();
        for row in parsed.cards {
            let type_hint = row
                .type_hint
                .as_deref()
                .map(TypeHint::from_str)
                .unwrap_or(TypeHint::Unspecified);
            let entry = if row.devices.is_empty() {
                CardEntry::Single(SingleCard {
                    pretty_name: row
                        .pretty_name
                        .clone()
                        .unwrap_or_else(|| row.name.clone()),
                    default_mixer: row.default_mixer.filter(|s| !s.is_empty()),
                    type_hint,
                })
            } else {
                let mut devices: HashMap<u32, DeviceEntry> = HashMap::new();
                for dev in row.devices {
                    devices.entry(dev.index).or_insert(DeviceEntry {
                        pretty_name: dev.pretty_name,
                        default_mixer: dev
                            .default_mixer
                            .filter(|s| !s.is_empty()),
                        hidden: dev.hidden,
                        ignore_generic_mixer: dev.ignore_generic_mixer,
                    });
                }
                CardEntry::MultiDevice(MultiDeviceCard { type_hint, devices })
            };
            by_name.insert(row.name, entry);
        }
        Ok(Self { by_name })
    }

    /// Look up by raw ALSA card name.
    pub fn lookup(&self, card_name: &str) -> Option<&CardEntry> {
        self.by_name.get(card_name)
    }

    /// Number of rows in the loaded catalog. Mostly for
    /// admission-time logging.
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// Empty-catalog predicate (e.g. parsed but no rows).
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalog_loads_and_has_rows() {
        let cat =
            AlsaCardCatalog::load_embedded().expect("embedded catalog parses");
        assert!(!cat.is_empty(), "embedded catalog has rows");
        // Spot-checks against well-known volumio entries.
        let bcm = cat.lookup("bcm2835 ALSA").expect("bcm2835 ALSA present");
        match bcm {
            CardEntry::Single(s) => {
                assert_eq!(s.pretty_name, "HDMI Out");
                assert_eq!(s.default_mixer.as_deref(), Some("PCM"));
                assert_eq!(s.type_hint, TypeHint::Integrated);
            }
            _ => panic!("bcm2835 ALSA should be single-device"),
        }
        let hifiberry = cat
            .lookup("snd_rpi_hifiberry_dacplus")
            .expect("hifiberry dac+ present");
        match hifiberry {
            CardEntry::Single(s) => {
                assert_eq!(s.pretty_name, "HiFiBerry DAC Plus");
                assert!(s.default_mixer.is_none());
                assert_eq!(s.type_hint, TypeHint::I2s);
            }
            _ => panic!("hifiberry dac+ should be single-device"),
        }
    }

    #[test]
    fn embedded_catalog_resolves_multi_device_cards() {
        let cat = AlsaCardCatalog::load_embedded().unwrap();
        let entry = cat
            .lookup("HDA Intel HDMI")
            .expect("HDA Intel HDMI present");
        match entry {
            CardEntry::MultiDevice(m) => {
                assert_eq!(m.type_hint, TypeHint::Integrated);
                assert!(
                    m.devices.contains_key(&3),
                    "HDMI 0 (subdev 3) present"
                );
                assert!(
                    m.devices.contains_key(&10),
                    "HDMI 4 (subdev 10) present"
                );
                assert_eq!(m.devices.get(&3).unwrap().pretty_name, "HDMI 0");
            }
            _ => panic!("HDA Intel HDMI should be multi-device"),
        }
    }

    #[test]
    fn from_toml_str_refuses_unsupported_schema_version() {
        let raw = "schema_version = 99\ncards = []\n";
        let err = AlsaCardCatalog::from_toml_str(raw).unwrap_err();
        assert!(matches!(err, CatalogError::SchemaVersion { found: 99 }));
    }

    #[test]
    fn from_toml_str_parse_error_surfaces() {
        let raw = "schema_version = \"not-an-integer\"\n";
        let err = AlsaCardCatalog::from_toml_str(raw).unwrap_err();
        assert!(matches!(err, CatalogError::Parse(_)));
    }

    #[test]
    fn empty_default_mixer_normalises_to_none() {
        let raw = r#"
schema_version = 1
[[cards]]
name = "test-card"
pretty_name = "Test"
default_mixer = ""
type_hint = "integrated"
"#;
        let cat = AlsaCardCatalog::from_toml_str(raw).unwrap();
        match cat.lookup("test-card").unwrap() {
            CardEntry::Single(s) => {
                assert!(s.default_mixer.is_none(), "empty mixer → None");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn multi_device_hidden_and_ignore_generic_mixer_round_trip() {
        let raw = r#"
schema_version = 1
[[cards]]
name = "AML-M8AUDIO"
type_hint = "integrated"
[[cards.devices]]
index = 0
pretty_name = "I2S"
hidden = true
[[cards.devices]]
index = 1
pretty_name = "SPDIF"
ignore_generic_mixer = true
"#;
        let cat = AlsaCardCatalog::from_toml_str(raw).unwrap();
        match cat.lookup("AML-M8AUDIO").unwrap() {
            CardEntry::MultiDevice(m) => {
                assert!(m.devices.get(&0).unwrap().hidden);
                assert!(!m.devices.get(&0).unwrap().ignore_generic_mixer);
                assert!(!m.devices.get(&1).unwrap().hidden);
                assert!(m.devices.get(&1).unwrap().ignore_generic_mixer);
            }
            _ => unreachable!(),
        }
    }
}
