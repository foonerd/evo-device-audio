#![allow(missing_docs)]

// Runtime ALSA output enumeration.
//
// Runs `aplay -l` to enumerate the ALSA cards + subdevices the
// host kernel currently exposes. Joins each row against the
// shipped alsa-cards catalog by raw card name → attaches an
// operator-friendly label, a derived output class (HDMI /
// Analog / SPDIF / USB / Bluetooth / I2S / Unknown), and the
// default ALSA mixer control. Returns a list of resolved
// outputs suitable for publication on
// `evo.audio.delivery:outputs`.
//
// `aplay -l` (lowercase) is intentionally distinct from
// `aplay -L` (capital) used elsewhere in this plugin for PCM
// device-name probing. Lowercase prints card-level identity;
// uppercase prints the full PCM space (including soft-pcm
// aliases like `default`, `plughw:...`). The two surfaces are
// disjoint by design.

use serde::{Deserialize, Serialize};
use std::process::Stdio;
use thiserror::Error;
use tokio::process::Command;

use crate::alsa_cards::{AlsaCardCatalog, CardEntry, TypeHint};

#[derive(Debug, Error)]
pub enum OutputEnumerationError {
    #[error(
        "aplay -l unavailable on this host (binary missing or non-executable)"
    )]
    AplayUnavailable,
    #[error("aplay -l returned non-zero exit code {code}: {stderr}")]
    AplayFailed { code: i32, stderr: String },
}

/// Resolved output row: one `(card_idx, device_idx)` pair joined
/// against the catalog. Serialisable on the wire (this is the
/// payload shape `evo.audio.delivery:outputs` publishes).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedAlsaOutput {
    /// ALSA card index (`N` in `hw:N,M`).
    pub card_idx: u32,
    /// ALSA device index (`M` in `hw:N,M`).
    pub device_idx: u32,
    /// Raw ALSA card name as the kernel reports it (the
    /// bracketed token from `aplay -l`).
    pub card_name: String,
    /// Canonical `hw:N,M` identifier.
    pub alsa_id: String,
    /// Operator-friendly label resolved against the catalog,
    /// falling back to the raw card name when no catalog row
    /// matches.
    pub label: String,
    /// Derived output classification.
    pub output_class: OutputClass,
    /// Default ALSA mixer control name, if the catalog declares
    /// one for this row.
    pub default_mixer_control: Option<String>,
    /// Catalog provenance — `Curated` when a row matched,
    /// `Unmapped` when the kernel exposed a card the catalog
    /// doesn't carry.
    pub catalog_provenance: CatalogProvenance,
    /// Whether the catalog row marks this subdevice hidden
    /// (volumio's `ignore` flag); the publisher leaves it in
    /// the list, UI may choose to filter.
    pub hidden: bool,
    /// Whether the catalog row marks this subdevice as
    /// generic-mixer-ignorable (volumio's `ignoreGenmixer`).
    pub ignore_generic_mixer: bool,
}

/// Coarse classification used by UI to render an icon /
/// section. Derived at runtime from the catalog `type_hint`
/// plus keyword matches against the card name and pretty label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputClass {
    /// I2S DAC HAT (Pi-style add-on board, MCU-tier integrated
    /// boards that present as I2S).
    I2s,
    /// HDMI audio output (TV passthrough, AVR, monitor speaker).
    Hdmi,
    /// USB audio device (USB DAC, USB audio interface).
    Usb,
    /// Analog jack / headphone / line-out / built-in speaker.
    Analog,
    /// S/PDIF (TOSLINK / coax) digital output.
    Spdif,
    /// Bluetooth audio profile (A2DP sink).
    Bluetooth,
    /// Catalog declared a type the resolver doesn't recognise,
    /// or no catalog row matched and keyword inference produced
    /// no answer.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogProvenance {
    /// Catalog row matched the runtime card name; label, type
    /// hint, default mixer all carry through.
    Curated,
    /// Kernel exposed a card the catalog doesn't carry. Label
    /// falls back to the raw card name; output_class derives
    /// from keyword inference only.
    Unmapped,
}

/// `aplay -l` row pre-resolution: card index + device index +
/// raw card name as the kernel printed it. Intermediate only;
/// consumers see `ResolvedAlsaOutput`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AplayCardDevice {
    card_idx: u32,
    device_idx: u32,
    card_name: String,
}

/// Enumerate outputs visible to the host kernel + resolve
/// against the catalog. The catalog is provided by the caller
/// so tests can inject a synthetic catalog.
pub async fn enumerate_outputs(
    catalog: &AlsaCardCatalog,
) -> Result<Vec<ResolvedAlsaOutput>, OutputEnumerationError> {
    let stdout = run_aplay_l_lowercase().await?;
    Ok(resolve(&stdout, catalog))
}

/// Pure resolution function — splits `aplay -l` output into
/// `(card_idx, device_idx, card_name)` rows and joins each
/// against the catalog. Pulled out of `enumerate_outputs` so
/// tests can pass synthetic stdout without spawning processes.
pub fn resolve(
    stdout: &str,
    catalog: &AlsaCardCatalog,
) -> Vec<ResolvedAlsaOutput> {
    let rows = parse_aplay_l_lowercase(stdout);
    rows.into_iter()
        .map(|row| resolve_row(row, catalog))
        .collect()
}

async fn run_aplay_l_lowercase() -> Result<String, OutputEnumerationError> {
    let output = Command::new("aplay")
        .arg("-l")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|_| OutputEnumerationError::AplayUnavailable)?;
    if !output.status.success() {
        return Err(OutputEnumerationError::AplayFailed {
            code: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parse `aplay -l` (lowercase) output. Each line of the form
///
///   card 0: ALSA [bcm2835 ALSA], device 0: bcm2835 ALSA [...]
///
/// yields one `(card_idx, device_idx, card_name)` row. The
/// indented continuation lines (`Subdevices:`, `Subdevice #N:`)
/// are skipped.
fn parse_aplay_l_lowercase(stdout: &str) -> Vec<AplayCardDevice> {
    let mut rows = Vec::new();
    for line in stdout.lines() {
        let line = line.trim_start();
        if !line.starts_with("card ") {
            continue;
        }
        let Some(row) = parse_card_line(line) else {
            continue;
        };
        rows.push(row);
    }
    rows
}

/// Parse one `card N: SHORT [CARD NAME], device M: SHORT2 [...]`
/// line into a structured row. Returns `None` if the shape
/// doesn't match (the line is silently skipped).
fn parse_card_line(line: &str) -> Option<AplayCardDevice> {
    // Card index between `card ` and `:`.
    let rest = line.strip_prefix("card ")?;
    let (idx_str, after_idx) = rest.split_once(':')?;
    let card_idx: u32 = idx_str.trim().parse().ok()?;
    // Card name is the bracketed token immediately following
    // the short id.
    let after_idx = after_idx.trim_start();
    let bracket_start = after_idx.find('[')?;
    let after_bracket = &after_idx[bracket_start + 1..];
    let bracket_end = after_bracket.find(']')?;
    let card_name = after_bracket[..bracket_end].to_string();
    // Device portion follows the first comma. Find `device M:`
    // and parse the index.
    let after_card_bracket = &after_bracket[bracket_end + 1..];
    let device_marker = after_card_bracket.find("device ")?;
    let after_device = &after_card_bracket[device_marker + "device ".len()..];
    let (device_idx_str, _) = after_device.split_once(':')?;
    let device_idx: u32 = device_idx_str.trim().parse().ok()?;
    Some(AplayCardDevice {
        card_idx,
        device_idx,
        card_name,
    })
}

fn resolve_row(
    row: AplayCardDevice,
    catalog: &AlsaCardCatalog,
) -> ResolvedAlsaOutput {
    let alsa_id = format!("hw:{},{}", row.card_idx, row.device_idx);
    match catalog.lookup(&row.card_name) {
        Some(CardEntry::Single(single)) => {
            let label = single.pretty_name.clone();
            let output_class = derive_output_class(
                &row.card_name,
                &label,
                Some(single.type_hint),
            );
            ResolvedAlsaOutput {
                card_idx: row.card_idx,
                device_idx: row.device_idx,
                card_name: row.card_name,
                alsa_id,
                label,
                output_class,
                default_mixer_control: single.default_mixer.clone(),
                catalog_provenance: CatalogProvenance::Curated,
                hidden: false,
                ignore_generic_mixer: false,
            }
        }
        Some(CardEntry::MultiDevice(multi)) => {
            if let Some(dev) = multi.devices.get(&row.device_idx) {
                let label = dev.pretty_name.clone();
                let output_class = derive_output_class(
                    &row.card_name,
                    &label,
                    Some(multi.type_hint),
                );
                ResolvedAlsaOutput {
                    card_idx: row.card_idx,
                    device_idx: row.device_idx,
                    card_name: row.card_name,
                    alsa_id,
                    label,
                    output_class,
                    default_mixer_control: dev.default_mixer.clone(),
                    catalog_provenance: CatalogProvenance::Curated,
                    hidden: dev.hidden,
                    ignore_generic_mixer: dev.ignore_generic_mixer,
                }
            } else {
                // Card matched but subdevice index isn't in the
                // catalog. Treat as partial-match: keep the type
                // hint, fall back the label to the raw name.
                let output_class = derive_output_class(
                    &row.card_name,
                    &row.card_name,
                    Some(multi.type_hint),
                );
                ResolvedAlsaOutput {
                    card_idx: row.card_idx,
                    device_idx: row.device_idx,
                    card_name: row.card_name.clone(),
                    alsa_id,
                    label: row.card_name,
                    output_class,
                    default_mixer_control: None,
                    catalog_provenance: CatalogProvenance::Unmapped,
                    hidden: false,
                    ignore_generic_mixer: false,
                }
            }
        }
        None => {
            let output_class =
                derive_output_class(&row.card_name, &row.card_name, None);
            ResolvedAlsaOutput {
                card_idx: row.card_idx,
                device_idx: row.device_idx,
                card_name: row.card_name.clone(),
                alsa_id,
                label: row.card_name,
                output_class,
                default_mixer_control: None,
                catalog_provenance: CatalogProvenance::Unmapped,
                hidden: false,
                ignore_generic_mixer: false,
            }
        }
    }
}

/// Derive the finer-grained `OutputClass` from the type hint
/// the catalog ships plus keyword matches against the card name
/// and label. The order matters — Bluetooth and USB checks come
/// first so a card name containing "USB" doesn't fall into the
/// HDMI bucket via the label.
fn derive_output_class(
    card_name: &str,
    label: &str,
    hint: Option<TypeHint>,
) -> OutputClass {
    let card_lc = card_name.to_ascii_lowercase();
    let label_lc = label.to_ascii_lowercase();
    let either =
        |needle: &str| card_lc.contains(needle) || label_lc.contains(needle);

    if either("bluez") || either("bluetooth") {
        return OutputClass::Bluetooth;
    }
    if either("usb") {
        return OutputClass::Usb;
    }
    if either("hdmi") {
        return OutputClass::Hdmi;
    }
    if either("spdif") || either("s/pdif") || either("toslink") {
        return OutputClass::Spdif;
    }
    // The catalog type hint runs before generic Analog inference
    // because the volumio cards.json marks I2S DAC HATs with
    // `type = "i2S"` and they should be reported as I2S even
    // when the label happens to contain a generic word.
    if let Some(TypeHint::I2s) = hint {
        return OutputClass::I2s;
    }
    if either("analog")
        || either("headphone")
        || either("line out")
        || either("audiocodec")
        || either("onboard audio")
        || either("audio jack")
        || either("speaker")
    {
        return OutputClass::Analog;
    }
    OutputClass::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_catalog() -> AlsaCardCatalog {
        let raw = r#"
schema_version = 1

[[cards]]
name = "bcm2835 ALSA"
pretty_name = "HDMI Out"
default_mixer = "PCM"
type_hint = "integrated"

[[cards]]
name = "snd_rpi_hifiberry_dacplus"
pretty_name = "HiFiBerry DAC Plus"
type_hint = "i2s"

[[cards]]
name = "atm7059_link"
type_hint = "integrated"
[[cards.devices]]
index = 0
pretty_name = "Cheapo Audio Jack"
default_mixer = "DAC PA"
[[cards.devices]]
index = 1
pretty_name = "HDMI Audio Out"
[[cards.devices]]
index = 2
pretty_name = "Cheapo S/PDIF"
"#;
        AlsaCardCatalog::from_toml_str(raw).unwrap()
    }

    #[test]
    fn parse_aplay_l_lowercase_extracts_card_and_device() {
        let raw = "**** List of PLAYBACK Hardware Devices ****
card 0: ALSA [bcm2835 ALSA], device 0: bcm2835 ALSA [bcm2835 ALSA]
  Subdevices: 7/7
  Subdevice #0: subdevice #0
card 1: sndrpihifiberry [snd_rpi_hifiberry_dacplus], device 0: HiFiBerry DAC+ HiFi pcm5122-hifi-0 [HiFiBerry DAC+ HiFi pcm5122-hifi-0]
  Subdevices: 1/1
";
        let rows = parse_aplay_l_lowercase(raw);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].card_idx, 0);
        assert_eq!(rows[0].device_idx, 0);
        assert_eq!(rows[0].card_name, "bcm2835 ALSA");
        assert_eq!(rows[1].card_idx, 1);
        assert_eq!(rows[1].card_name, "snd_rpi_hifiberry_dacplus");
    }

    #[test]
    fn resolve_single_device_curated_row() {
        let cat = fixture_catalog();
        let raw = "card 0: ALSA [bcm2835 ALSA], device 0: bcm2835 ALSA [bcm2835 ALSA]\n";
        let outputs = resolve(raw, &cat);
        assert_eq!(outputs.len(), 1);
        let out = &outputs[0];
        assert_eq!(out.card_idx, 0);
        assert_eq!(out.device_idx, 0);
        assert_eq!(out.alsa_id, "hw:0,0");
        assert_eq!(out.label, "HDMI Out");
        assert_eq!(out.output_class, OutputClass::Hdmi);
        assert_eq!(out.default_mixer_control.as_deref(), Some("PCM"));
        assert_eq!(out.catalog_provenance, CatalogProvenance::Curated);
        assert!(!out.hidden);
        assert!(!out.ignore_generic_mixer);
    }

    #[test]
    fn resolve_i2s_dac_row_marked_i2s_via_type_hint() {
        let cat = fixture_catalog();
        let raw = "card 1: sndrpihifiberry [snd_rpi_hifiberry_dacplus], device 0: HiFiBerry DAC+ HiFi pcm5122-hifi-0 [HiFiBerry DAC+ HiFi pcm5122-hifi-0]\n";
        let outputs = resolve(raw, &cat);
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].label, "HiFiBerry DAC Plus");
        assert_eq!(outputs[0].output_class, OutputClass::I2s);
        assert!(outputs[0].default_mixer_control.is_none());
    }

    #[test]
    fn resolve_multi_device_card_per_subdevice_label() {
        let cat = fixture_catalog();
        let raw = "card 0: link [atm7059_link], device 0: jack [jack]\ncard 0: link [atm7059_link], device 1: hdmi [hdmi]\ncard 0: link [atm7059_link], device 2: spdif [spdif]\n";
        let outputs = resolve(raw, &cat);
        assert_eq!(outputs.len(), 3);
        assert_eq!(outputs[0].label, "Cheapo Audio Jack");
        assert_eq!(outputs[0].output_class, OutputClass::Analog);
        assert_eq!(outputs[0].default_mixer_control.as_deref(), Some("DAC PA"));
        assert_eq!(outputs[1].label, "HDMI Audio Out");
        assert_eq!(outputs[1].output_class, OutputClass::Hdmi);
        assert_eq!(outputs[2].label, "Cheapo S/PDIF");
        assert_eq!(outputs[2].output_class, OutputClass::Spdif);
    }

    #[test]
    fn resolve_unmapped_card_falls_back_to_raw_name() {
        let cat = fixture_catalog();
        let raw = "card 7: WeirdHat [SomeUnknownDac], device 0: foo [bar]\n";
        let outputs = resolve(raw, &cat);
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].label, "SomeUnknownDac");
        assert_eq!(outputs[0].catalog_provenance, CatalogProvenance::Unmapped);
        // No keyword match → Unknown.
        assert_eq!(outputs[0].output_class, OutputClass::Unknown);
    }

    #[test]
    fn derive_output_class_keyword_inference_covers_each_class() {
        assert_eq!(
            derive_output_class("bcm2835 HDMI 1", "HDMI Out", None),
            OutputClass::Hdmi
        );
        assert_eq!(
            derive_output_class("USB Audio", "USB DAC", None),
            OutputClass::Usb
        );
        assert_eq!(
            derive_output_class("sndspdif", "TOSLINK (S/PDIF)", None),
            OutputClass::Spdif
        );
        assert_eq!(
            derive_output_class("bluez_card.AA:BB:CC", "Bluetooth Sink", None),
            OutputClass::Bluetooth
        );
        assert_eq!(
            derive_output_class("audiocodec", "Analog Audio Out", None),
            OutputClass::Analog
        );
        assert_eq!(
            derive_output_class(
                "HiFiBerry DAC",
                "HiFiBerry DAC",
                Some(TypeHint::I2s)
            ),
            OutputClass::I2s
        );
        assert_eq!(
            derive_output_class(
                "weird-card-no-keyword",
                "label-no-keyword",
                None
            ),
            OutputClass::Unknown
        );
    }

    #[test]
    fn parse_skips_continuation_lines() {
        let raw = "**** Header ****
card 0: ALSA [bcm2835 ALSA], device 0: bcm2835 ALSA [bcm2835 ALSA]
  Subdevices: 7/7
  Subdevice #0: subdevice #0
  Subdevice #1: subdevice #1
";
        let rows = parse_aplay_l_lowercase(raw);
        assert_eq!(rows.len(), 1);
    }
}
