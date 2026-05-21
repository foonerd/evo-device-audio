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
use crate::ActiveDacConfig;

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
    /// The short identifier — the token between `card N:` and the
    /// opening `[` on the aplay -l line. Matches what the
    /// hardware.audio-config plugin's DAC catalogue records as
    /// `alsa_card_hint`. Used for active-DAC enrichment matching.
    /// Empty when the line lacks a short id (defensive fallback).
    short_id: String,
    /// The long form — the bracketed token immediately following
    /// the short id. Matches what the `alsa-cards.toml` catalogue
    /// records under `name`. Used for catalog-row lookup and as
    /// the operator-facing fallback label when no catalog row
    /// matches.
    card_name: String,
}

/// Enumerate outputs visible to the host kernel + resolve
/// against the catalog. The catalog is provided by the caller
/// so tests can inject a synthetic catalog. The optional active
/// DAC config (sourced from the cached
/// `evo.hardware.audio:active_config` subject) enriches rows
/// whose kernel card name matches the active DAC's
/// `alsacard_hint` AND whose catalog row lacks a label or a
/// default mixer control — closing the gap for DAC HATs whose
/// kernel card name is generic (e.g. `DAC` shared by several
/// distinct DACs) and whose curated metadata lives in the
/// hardware.audio-config plugin's DAC catalogue rather than
/// this plugin's `alsa-cards.toml`.
pub async fn enumerate_outputs(
    catalog: &AlsaCardCatalog,
    active_dac_config: Option<&ActiveDacConfig>,
) -> Result<Vec<ResolvedAlsaOutput>, OutputEnumerationError> {
    let stdout = run_aplay_l_lowercase().await?;
    Ok(resolve(&stdout, catalog, active_dac_config))
}

/// Pure resolution function — splits `aplay -l` output into
/// `(card_idx, device_idx, card_name)` rows and joins each
/// against the catalog. Pulled out of `enumerate_outputs` so
/// tests can pass synthetic stdout without spawning processes.
pub fn resolve(
    stdout: &str,
    catalog: &AlsaCardCatalog,
    active_dac_config: Option<&ActiveDacConfig>,
) -> Vec<ResolvedAlsaOutput> {
    let rows = parse_aplay_l_lowercase(stdout);
    rows.into_iter()
        .map(|row| {
            let short_id = row.short_id.clone();
            let mut resolved = resolve_row(row, catalog);
            enrich_from_active_dac_config(
                &mut resolved,
                &short_id,
                active_dac_config,
            );
            resolved
        })
        .collect()
}

/// Best-effort enrichment from the cached active DAC config.
/// Operates on a row whose catalog resolution may have left
/// the label as the raw card name (Unmapped) or the default
/// mixer control empty (catalog row without `default_mixer`).
/// Single-owner discipline: this function never overrides a
/// label or mixer the alsa-cards catalog supplied — the catalog
/// is the shipped-reference source for generic cards; the
/// active DAC config is the operator-selected DAC's metadata
/// owned by the hardware.audio-config plugin and is the
/// authoritative source ONLY when the kernel-exposed card
/// name matches the active DAC's hint.
fn enrich_from_active_dac_config(
    resolved: &mut ResolvedAlsaOutput,
    short_id: &str,
    active: Option<&ActiveDacConfig>,
) {
    let Some(active) = active else {
        return;
    };
    let Some(active_hint) = active.alsacard_hint.as_deref() else {
        return;
    };
    // The hardware.audio-config DAC catalogue records the short
    // identifier as `alsa_card_hint`; the alsa-cards.toml records
    // either short_id or the bracketed long form depending on
    // historical inheritance from the upstream cards.json. Match
    // against both to remain resilient to the convention drift.
    if active_hint != short_id && active_hint != resolved.card_name {
        return;
    }
    // Default mixer control: only fill when the catalog left
    // it empty. The catalog's per-card value (when present)
    // remains authoritative — different subdevices of the same
    // card may carry different mixer hints the catalog
    // distinguishes.
    if resolved.default_mixer_control.is_none() {
        if let Some(mixer) = active.mixer_hint.as_ref() {
            if !mixer.is_empty() {
                resolved.default_mixer_control = Some(mixer.clone());
            }
        }
    }
    // Label: only fill when the catalog left the row Unmapped.
    // A Curated row carries the catalog's label which the
    // operator-facing UI relies on for stable nomenclature
    // (the active DAC's display name may differ from the
    // catalog's preferred form).
    if resolved.catalog_provenance == CatalogProvenance::Unmapped {
        if let Some(display) = active.display_name.as_ref() {
            if !display.is_empty() {
                resolved.label = display.clone();
            }
        }
    }
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
    // Short id is the token between the colon and the opening
    // bracket. Card name (long form) is the bracketed token.
    let after_idx = after_idx.trim_start();
    let bracket_start = after_idx.find('[')?;
    let short_id = after_idx[..bracket_start].trim().to_string();
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
        short_id,
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
        let outputs = resolve(raw, &cat, None);
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
        let outputs = resolve(raw, &cat, None);
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].label, "HiFiBerry DAC Plus");
        assert_eq!(outputs[0].output_class, OutputClass::I2s);
        assert!(outputs[0].default_mixer_control.is_none());
    }

    #[test]
    fn resolve_multi_device_card_per_subdevice_label() {
        let cat = fixture_catalog();
        let raw = "card 0: link [atm7059_link], device 0: jack [jack]\ncard 0: link [atm7059_link], device 1: hdmi [hdmi]\ncard 0: link [atm7059_link], device 2: spdif [spdif]\n";
        let outputs = resolve(raw, &cat, None);
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
        let outputs = resolve(raw, &cat, None);
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

    // ----- active-DAC enrichment tests -----
    //
    // Regression coverage for the architectural fix: DAC HATs
    // whose kernel-reported card name is generic (e.g. `DAC`
    // shared by Audiophonics I-Sabre, ALLO Mini Boss, and other
    // boards) carry their operator-facing label + mixer-control
    // on the hardware.audio-config plugin's DAC catalogue, not
    // on this plugin's `alsa-cards.toml`. The enumerator
    // consults the cached active-DAC subject and enriches rows
    // whose kernel card name matches the active DAC's
    // `alsacard_hint` AND whose catalog-resolved row lacks a
    // label or mixer.

    fn active_dac_for(
        alsacard_hint: &str,
        display_name: &str,
        mixer_hint: &str,
    ) -> ActiveDacConfig {
        ActiveDacConfig {
            overlay: "irrelevant".into(),
            catalogue_id: None,
            display_name: Some(display_name.into()),
            alsacard_hint: Some(alsacard_hint.into()),
            mixer_hint: Some(mixer_hint.into()),
            boot_config_path: "/boot/config.txt".into(),
        }
    }

    #[test]
    fn active_dac_enriches_unmapped_row_with_label_and_mixer() {
        // I-Sabre Q2M scenario: card_name = "DAC", catalog has
        // no row, active DAC config carries the friendly label
        // and the Digital mixer. After enrichment the row has
        // label = "I-Sabre Q2M" and default_mixer_control =
        // Some("Digital") — the UI's Hardware mixer affordance
        // becomes reachable.
        let cat = fixture_catalog();
        let raw = "card 3: DAC [I-Sabre Q2M DAC], device 0: I-Sabre [foo]\n";
        let active =
            active_dac_for("DAC", "Audiophonics I-Sabre Q2M", "Digital");
        let outputs = resolve(raw, &cat, Some(&active));
        assert_eq!(outputs.len(), 1);
        let out = &outputs[0];
        // card_name is the bracketed long form (matches the catalog
        // key); the active-DAC enrichment matches on the short id
        // "DAC".
        assert_eq!(out.card_name, "I-Sabre Q2M DAC");
        assert_eq!(out.label, "Audiophonics I-Sabre Q2M");
        assert_eq!(out.default_mixer_control.as_deref(), Some("Digital"));
    }

    #[test]
    fn active_dac_does_not_match_when_alsacard_hint_differs() {
        // Row card_name = "DAC", active DAC hint = "BossDAC".
        // No match → row stays Unmapped / unenriched.
        let cat = fixture_catalog();
        let raw = "card 3: DAC [Foo], device 0: Foo [bar]\n";
        let active = active_dac_for("BossDAC", "Allo BOSS", "Digital");
        let outputs = resolve(raw, &cat, Some(&active));
        assert_eq!(outputs.len(), 1);
        let out = &outputs[0];
        assert_eq!(out.label, "Foo");
        assert!(out.default_mixer_control.is_none());
        assert_eq!(out.catalog_provenance, CatalogProvenance::Unmapped);
    }

    #[test]
    fn active_dac_does_not_override_catalog_label_or_mixer() {
        // Catalog has snd_rpi_hifiberry_dacplus with no
        // default_mixer (i2s type hint). Active DAC config
        // also points at hint = "snd_rpi_hifiberry_dacplus"
        // with display_name = "Should Not Override" and a
        // mixer hint of "Digital". The catalog's label takes
        // precedence (Curated provenance is not overridden);
        // the absent default_mixer_control gets filled from
        // the active DAC's mixer_hint.
        let cat = fixture_catalog();
        let raw = "card 1: sndrpihifiberry [snd_rpi_hifiberry_dacplus], device 0: HiFiBerry DAC+ HiFi pcm5122-hifi-0 [HiFiBerry DAC+ HiFi pcm5122-hifi-0]\n";
        let active = active_dac_for(
            "snd_rpi_hifiberry_dacplus",
            "Should Not Override",
            "Digital",
        );
        let outputs = resolve(raw, &cat, Some(&active));
        assert_eq!(outputs.len(), 1);
        let out = &outputs[0];
        // Catalog label held (Curated provenance untouched).
        assert_eq!(out.label, "HiFiBerry DAC Plus");
        assert_eq!(out.catalog_provenance, CatalogProvenance::Curated);
        // Missing mixer filled from active DAC's hint.
        assert_eq!(out.default_mixer_control.as_deref(), Some("Digital"));
    }

    #[test]
    fn active_dac_with_empty_strings_does_not_enrich() {
        // ActiveConfig::unset (operator cleared the DAC, or
        // the catalogue lookup failed): display_name/mixer_hint
        // are None or empty. Enrichment must not write empty
        // values over the catalog fallback.
        let cat = fixture_catalog();
        let raw = "card 3: DAC [Foo], device 0: Foo [bar]\n";
        let active = ActiveDacConfig {
            overlay: String::new(),
            catalogue_id: None,
            display_name: None,
            alsacard_hint: Some("DAC".into()),
            mixer_hint: None,
            boot_config_path: String::new(),
        };
        let outputs = resolve(raw, &cat, Some(&active));
        let out = &outputs[0];
        assert_eq!(out.label, "Foo");
        assert!(out.default_mixer_control.is_none());
    }
}
