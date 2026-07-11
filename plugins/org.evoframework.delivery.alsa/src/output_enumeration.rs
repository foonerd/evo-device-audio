// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
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

use crate::alsa_cards::{AlsaCardCatalog, CardEntry};
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
    // Layer 1: read kernel-supplied card identities. The kernel is
    // the authoritative source for card identification.
    // If the read fails (no /proc/asound, permission denied), we
    // still proceed with an empty identity set — Layer 2 falls
    // back to OutputClass::Unknown for every row, which is the
    // explicit failure semantics. The introspection error is
    // logged but not propagated as a hard error: enumeration
    // still produces hw:N,M rows operators can route audio to,
    // just without rich classification.
    let identities =
        match crate::kernel_introspection::introspect_all_cards().await {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "kernel introspection failed; output_class \
                     classification falls back to Unknown for every \
                     row this enumeration pass — operator UI may show \
                     'Other' for cards the kernel knows but we could \
                     not parse"
                );
                Vec::new()
            }
        };
    Ok(resolve(&stdout, catalog, active_dac_config, &identities))
}

/// Pure resolution function — splits `aplay -l` output into
/// `(card_idx, device_idx, card_name)` rows and joins each
/// against the catalog. Pulled out of `enumerate_outputs` so
/// tests can pass synthetic stdout + identity fixtures without
/// spawning processes or accessing `/proc/asound`.
pub fn resolve(
    stdout: &str,
    catalog: &AlsaCardCatalog,
    active_dac_config: Option<&ActiveDacConfig>,
    identities: &[crate::kernel_introspection::CardIdentity],
) -> Vec<ResolvedAlsaOutput> {
    let rows = parse_aplay_l_lowercase(stdout);
    rows.into_iter()
        .map(|row| {
            let short_id = row.short_id.clone();
            let identity =
                identities.iter().find(|i| i.card_idx == row.card_idx);
            let mut resolved = resolve_row(row, identity, catalog);
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
    // Output class: set from the hardware.audio-config catalogue's
    // declared `interface` value when the active-DAC join
    // identifies this row. The catalogue is the authoritative
    // source — every DAC row declares its bus topology per the
    // schema's `interface-declared-per-dac` acceptance contract,
    // and the loader rejects rows omitting it. The declared value
    // overrides any prior derivation: a card whose kernel name
    // contains no class-marker keyword cannot be classified by
    // string heuristics, yet the active-DAC join carries the
    // catalogue's exact answer. When the active-DAC join does not
    // match this row (different card), the prior derivation stands
    // and the keyword path remains authoritative for outputs the
    // catalogue does not know about (HDMI, USB, integrated codecs).
    if let Some(iface) = active.interface.as_deref() {
        if let Some(declared) = parse_output_class(iface) {
            resolved.output_class = declared;
        }
    }
}

/// Parse the catalogue's declared `interface` string into the
/// finer-grained [`OutputClass`]. Returns `None` when the string
/// is not a recognised variant — caller leaves the resolved
/// row's existing `output_class` untouched in that case (logged
/// upstream by the subject deserialiser when it surfaces).
fn parse_output_class(raw: &str) -> Option<OutputClass> {
    match raw {
        "i2s" => Some(OutputClass::I2s),
        "usb" => Some(OutputClass::Usb),
        "spdif" => Some(OutputClass::Spdif),
        "hdmi" => Some(OutputClass::Hdmi),
        "analog" => Some(OutputClass::Analog),
        "bluetooth" => Some(OutputClass::Bluetooth),
        _ => None,
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
    identity: Option<&crate::kernel_introspection::CardIdentity>,
    catalog: &AlsaCardCatalog,
) -> ResolvedAlsaOutput {
    let alsa_id = format!("hw:{},{}", row.card_idx, row.device_idx);
    // Layer 2: kernel-supplied classification is authoritative per
    // the kernel-introspection-authoritative discipline. When kernel
    // introspection produced an identity for
    // this card index, derive output_class from it. When it didn't
    // (read failed, card_idx mismatch, hardware unplugged between
    // enumeration passes), output_class is Unknown — the explicit
    // failure semantic that surfaces the gap rather than silently
    // inferring from card-name string heuristics.
    let kernel_output_class = identity
        .map(crate::kernel_introspection::classify_from_kernel)
        .unwrap_or(OutputClass::Unknown);
    match catalog.lookup(&row.card_name) {
        Some(CardEntry::Single(single)) => {
            let label = single.pretty_name.clone();
            ResolvedAlsaOutput {
                card_idx: row.card_idx,
                device_idx: row.device_idx,
                card_name: row.card_name,
                alsa_id,
                label,
                output_class: kernel_output_class,
                default_mixer_control: single.default_mixer.clone(),
                catalog_provenance: CatalogProvenance::Curated,
                hidden: false,
                ignore_generic_mixer: false,
            }
        }
        Some(CardEntry::MultiDevice(multi)) => {
            if let Some(dev) = multi.devices.get(&row.device_idx) {
                let label = dev.pretty_name.clone();
                ResolvedAlsaOutput {
                    card_idx: row.card_idx,
                    device_idx: row.device_idx,
                    card_name: row.card_name,
                    alsa_id,
                    label,
                    output_class: kernel_output_class,
                    default_mixer_control: dev.default_mixer.clone(),
                    catalog_provenance: CatalogProvenance::Curated,
                    hidden: dev.hidden,
                    ignore_generic_mixer: dev.ignore_generic_mixer,
                }
            } else {
                // Card matched but subdevice index isn't in the
                // catalog's per-subdevice map. Partial-match:
                // kernel classification still applies (card-level);
                // label applies the same precedence the fully-
                // unmapped branch uses — codec overlay → kernel
                // chip name → kernel long_name → card_name. This
                // keeps catalogue widening (codec overlay rows
                // added under `[[hda_codecs]]`) effective for
                // subdevices whose per-subdevice label isn't in
                // the per-card-name multi-device table.
                let label = resolve_unmapped_label(identity, &row, catalog);
                ResolvedAlsaOutput {
                    card_idx: row.card_idx,
                    device_idx: row.device_idx,
                    card_name: row.card_name.clone(),
                    alsa_id,
                    label,
                    output_class: kernel_output_class,
                    default_mixer_control: None,
                    catalog_provenance: CatalogProvenance::Unmapped,
                    hidden: false,
                    ignore_generic_mixer: false,
                }
            }
        }
        None => {
            // Unmapped against alsa-cards.toml's per-card-name
            // table. Label precedence for HDA cards (the common
            // x86 / amd64 case):
            //   1. Catalogue codec overlay (`[[hda_codecs]]`) hit
            //      via the kernel-supplied codec vendor_id — the
            //      operator-friendly chip family brand label.
            //   2. Kernel-supplied codec chip name (e.g.
            //      `Realtek ALC233`) — already operator-readable,
            //      just not curated.
            //   3. Kernel-supplied card long_name (e.g.
            //      `HDA Intel PCH at 0x98b20000 irq 149`) — the
            //      controller identity; meaningful but verbose.
            //   4. aplay-parsed card_name — last resort.
            //
            // For non-HDA unmapped cards, the codec-overlay layer
            // is skipped (no codecs); the precedence collapses to
            // 3 → 4 (kernel long_name → aplay card_name).
            let label = resolve_unmapped_label(identity, &row, catalog);
            ResolvedAlsaOutput {
                card_idx: row.card_idx,
                device_idx: row.device_idx,
                card_name: row.card_name.clone(),
                alsa_id,
                label,
                output_class: kernel_output_class,
                default_mixer_control: None,
                catalog_provenance: CatalogProvenance::Unmapped,
                hidden: false,
                ignore_generic_mixer: false,
            }
        }
    }
}

/// Resolve the operator-facing label for an unmapped card,
/// applying the precedence documented above the call site:
/// codec overlay → kernel chip name → kernel long_name → aplay
/// card_name. Catalogue codec lookup is consulted for HDA and
/// AC97 codecs alike — the kernel surfaces the codec identity
/// differently (HDA: `codec#N` with `Vendor Id:` u32; AC97:
/// `codec97#N/ac97#0-0` with chip-name string), so the
/// catalogue carries two parallel overlay sections
/// (`[[hda_codecs]]` keyed by vendor_id, `[[ac97_codecs]]`
/// keyed by chip_name). Both are consulted; whichever the
/// kernel-reported card identity carries is the active path.
fn resolve_unmapped_label(
    identity: Option<&crate::kernel_introspection::CardIdentity>,
    row: &AplayCardDevice,
    catalog: &AlsaCardCatalog,
) -> String {
    if let Some(id) = identity {
        // HDA codec overlay path: walk the codec list and return
        // the first catalogue brand match. Prefer the analog
        // codec (typically address 0) — it's the operator-
        // meaningful chip on a card; HDMI codec sibling labels
        // are less useful for primary-output identification.
        for codec in &id.codecs {
            if let Some(entry) = catalog.lookup_codec(codec.vendor_id) {
                return entry.pretty_name.clone();
            }
        }
        // AC97 codec overlay path: when the kernel registered an
        // AC97 codec (CardKind::Ac97), look up its chip name
        // against the catalogue's AC97 overlay. The kernel name
        // is already operator-readable; the overlay rebrands
        // where the catalogue carries a preferred form.
        if let Some(ac97) = id.ac97_codec.as_ref() {
            if let Some(entry) = catalog.lookup_ac97_codec(&ac97.chip_name) {
                return entry.pretty_name.clone();
            }
            // Catalogue miss: fall back to the kernel chip name
            // before reaching for long_name.
            if !ac97.chip_name.is_empty() {
                return ac97.chip_name.clone();
            }
        }
        // USB DAC overlay path: when the kernel registered a USB
        // audio device (CardKind::Usb), look up its
        // vendor:product against the catalogue's USB overlay.
        // The kernel's long_name typically carries the USB
        // device's bDescriptor name (e.g. `Topping E50 at usb-...`)
        // which is already meaningful; the overlay rebrands to a
        // clean operator-facing label (`Topping E50`) without
        // the bus-attachment suffix.
        if let Some(usb) = id.usb_dac.as_ref() {
            if let Some(entry) =
                catalog.lookup_usb_dac(usb.vendor_id, usb.product_id)
            {
                return entry.pretty_name.clone();
            }
        }
        // No HDA-codec catalogue hit. Use the first HDA codec's
        // kernel-supplied chip name if any — already
        // operator-readable.
        if let Some(first_codec) = id.codecs.first() {
            if !first_codec.chip_name.is_empty() {
                return first_codec.chip_name.clone();
            }
        }
        // No codec at all (I²S DAC, Loopback, vc4-hdmi).
        // Kernel long_name carries the controller identity.
        if !id.long_name.is_empty() {
            return id.long_name.clone();
        }
    }
    row.card_name.clone()
}

// NOTE: `derive_output_class` retired with the kernel-introspection-
// authoritative discipline. The old path string-matched keywords
// against kernel-reported card names ("usb", "hdmi", "spdif",
// "analog", "headphone", ...) to classify the row's OutputClass.
// Kernel runtime introspection (`kernel_introspection::classify_from_kernel`)
// is the single canonical classification path now; the catalog
// supplies presentation only. Reintroducing keyword classification
// at the join site violates the discipline's invariant.

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

    fn synthetic_identity(
        card_idx: u32,
        kind: crate::kernel_introspection::CardKind,
    ) -> crate::kernel_introspection::CardIdentity {
        crate::kernel_introspection::CardIdentity {
            card_idx,
            short_id: "X".into(),
            long_name: "X".into(),
            driver: "X".into(),
            kind,
            codecs: Vec::new(),
            ac97_codec: None,
            usb_dac: None,
        }
    }

    #[test]
    fn resolve_single_device_curated_row() {
        let cat = fixture_catalog();
        let raw = "card 0: ALSA [bcm2835 ALSA], device 0: bcm2835 ALSA [bcm2835 ALSA]\n";
        // Kernel introspection identifies card 0 as HDMI (this
        // fixture's bcm2835 ALSA row is HDMI-class). Under
        // the kernel-introspection-authoritative discipline the
        // OutputClass derives from this CardIdentity,
        // not from keyword inference over the card-name string.
        let identities = vec![synthetic_identity(
            0,
            crate::kernel_introspection::CardKind::Hdmi,
        )];
        let outputs = resolve(raw, &cat, None, &identities);
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
    fn resolve_i2s_dac_row_marked_i2s_via_kernel_identity() {
        let cat = fixture_catalog();
        let raw = "card 1: sndrpihifiberry [snd_rpi_hifiberry_dacplus], device 0: HiFiBerry DAC+ HiFi pcm5122-hifi-0 [HiFiBerry DAC+ HiFi pcm5122-hifi-0]\n";
        // Kernel introspection identifies card 1 as I²S DAC
        // (driver name matches the I²S heuristic + no codec file
        // — the path Layer 1 classifies as I²S). The previous
        // path relied on the alsa-cards.toml row's TypeHint::I2s;
        // that's now ignored in favour of kernel-derived identity.
        let identities = vec![synthetic_identity(
            1,
            crate::kernel_introspection::CardKind::I2s,
        )];
        let outputs = resolve(raw, &cat, None, &identities);
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].label, "HiFiBerry DAC Plus");
        assert_eq!(outputs[0].output_class, OutputClass::I2s);
        assert!(outputs[0].default_mixer_control.is_none());
    }

    #[test]
    fn resolve_multi_device_card_all_subdevices_share_kernel_classification() {
        // Structural pin: a single kernel card has
        // ONE classification. Subdevice labels can differ per
        // device (operator-facing nomenclature), but the
        // OutputClass is a property of the card, derived from
        // its kernel-supplied identity. The previous keyword-scan
        // path classified each subdevice independently by label
        // keyword (a label containing "hdmi" classified as Hdmi,
        // "spdif" as Spdif, etc.) — that was a parallel truth
        // path the kernel was already authoritative on.
        let cat = fixture_catalog();
        let raw = "card 0: link [atm7059_link], device 0: jack [jack]\ncard 0: link [atm7059_link], device 1: hdmi [hdmi]\ncard 0: link [atm7059_link], device 2: spdif [spdif]\n";
        // Kernel classifies card 0 as Hda (analog primary,
        // representative SoC integrated audio).
        let identities = vec![synthetic_identity(
            0,
            crate::kernel_introspection::CardKind::Hda,
        )];
        let outputs = resolve(raw, &cat, None, &identities);
        assert_eq!(outputs.len(), 3);
        // Labels still differ per subdevice (catalogue
        // multi-device map supplies them).
        assert_eq!(outputs[0].label, "Cheapo Audio Jack");
        assert_eq!(outputs[0].default_mixer_control.as_deref(), Some("DAC PA"));
        assert_eq!(outputs[1].label, "HDMI Audio Out");
        assert_eq!(outputs[2].label, "Cheapo S/PDIF");
        // OutputClass is uniform across the card's subdevices,
        // derived from the kernel CardIdentity.
        for out in &outputs {
            assert_eq!(out.output_class, OutputClass::Analog);
        }
    }

    #[test]
    fn resolve_unmapped_card_falls_back_to_raw_name() {
        let cat = fixture_catalog();
        let raw = "card 7: WeirdHat [SomeUnknownDac], device 0: foo [bar]\n";
        let outputs = resolve(raw, &cat, None, &[]);
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].label, "SomeUnknownDac");
        assert_eq!(outputs[0].catalog_provenance, CatalogProvenance::Unmapped);
        // No keyword match → Unknown.
        assert_eq!(outputs[0].output_class, OutputClass::Unknown);
    }

    // NOTE: the `derive_output_class_keyword_inference_covers_each_class`
    // test retired with the `derive_output_class` function it
    // covered. The classification surface moved to
    // `kernel_introspection::classify_from_kernel`, which is
    // exercised by `kernel_introspection::tests::classify_*` —
    // each test pins one CardKind → OutputClass arm explicitly.
    // Reintroducing keyword-derivation tests at this layer
    // re-introduces the retired path.

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
            interface: Some("i2s".into()),
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
        let outputs = resolve(raw, &cat, Some(&active), &[]);
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
        let outputs = resolve(raw, &cat, Some(&active), &[]);
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
        let outputs = resolve(raw, &cat, Some(&active), &[]);
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
            interface: None,
            boot_config_path: String::new(),
        };
        let outputs = resolve(raw, &cat, Some(&active), &[]);
        let out = &outputs[0];
        assert_eq!(out.label, "Foo");
        assert!(out.default_mixer_control.is_none());
    }

    #[test]
    fn declared_interface_sets_output_class_on_unmapped_row() {
        // Regression pin. The hardware.audio-config catalogue
        // declares each DAC row's bus topology via the `interface`
        // field; downstream the active_config subject carries the
        // resolved row's declared value to delivery.alsa as
        // `ActiveDacConfig.interface`. The delivery-side
        // `enrich_from_active_dac_config` MUST set the resolved
        // row's output_class from that declared value when the
        // active-DAC join identifies this row, even when the
        // alsa-cards catalogue does NOT carry the kernel card
        // name verbatim. Without this declarative path, unmapped-
        // card rows fall through `derive_output_class`'s keyword
        // scan — and a card whose kernel name contains no class-
        // marker keyword is classified as Unknown and renders as
        // "Other" in the operator UI even though the framework
        // knows the precise DAC and its bus topology.
        let cat = fixture_catalog();
        // Kernel card name with no class-marker keyword. The
        // alsa-cards catalogue fixture does not contain this name
        // (Unmapped). active_dac is provided with
        // `interface = "i2s"` declared.
        let raw = "card 3: SynthDAC [SynthDAC], device 0: foo [bar]\n";
        let active = ActiveDacConfig {
            overlay: "synth-overlay".into(),
            catalogue_id: Some("synth-dac".into()),
            display_name: Some("Synthetic DAC".into()),
            alsacard_hint: Some("SynthDAC".into()),
            mixer_hint: Some("Digital".into()),
            interface: Some("i2s".into()),
            boot_config_path: "/boot/firmware/config.txt".into(),
        };
        let outputs = resolve(raw, &cat, Some(&active), &[]);
        assert_eq!(outputs.len(), 1);
        let out = &outputs[0];
        assert_eq!(
            out.output_class,
            OutputClass::I2s,
            "declared catalogue interface MUST set output_class \
             on an unmapped row; the operator UI relies on this \
             classification to render Destination chips",
        );
        // The active-DAC label enrichment still applies — the
        // declarative interface fix doesn't regress label
        // promotion.
        assert_eq!(out.label, "Synthetic DAC");
        assert_eq!(out.default_mixer_control.as_deref(), Some("Digital"));
    }

    #[test]
    fn declared_interface_overrides_keyword_derivation() {
        // Symmetric pin: even when the catalogue row carries a
        // type_hint that would otherwise derive a non-I2S class,
        // an active-DAC declared interface for the SAME card wins.
        // The catalogue is the authoritative classification source
        // when present.
        let cat = fixture_catalog();
        // Use a card the fixture catalogue maps to I2s via its
        // type_hint; declare the active DAC as spdif. The result
        // must be spdif (the declared value), not i2s (the
        // catalogue type_hint).
        let raw = "card 0: sndrpihifiberry [snd_rpi_hifiberry_dacplus], device 0: x [y]\n";
        let active = ActiveDacConfig {
            overlay: "hifiberry-digi".into(),
            catalogue_id: Some("hifiberry-digi".into()),
            display_name: Some("HiFiBerry Digi".into()),
            alsacard_hint: Some("sndrpihifiberry".into()),
            mixer_hint: None,
            interface: Some("spdif".into()),
            boot_config_path: "/boot/firmware/config.txt".into(),
        };
        let outputs = resolve(raw, &cat, Some(&active), &[]);
        let out = &outputs[0];
        assert_eq!(
            out.output_class,
            OutputClass::Spdif,
            "active-DAC declared interface MUST override the \
             alsa-cards catalogue's coarser type_hint — the \
             hardware.audio-config catalogue is the authoritative \
             source for the active row's classification",
        );
    }

    #[test]
    fn unknown_active_interface_string_leaves_kernel_class_intact() {
        // Hardening pin: when the active-DAC subject carries an
        // unrecognised interface string (forward-compat with a
        // newer catalogue declaring a value this build does not
        // know), the kernel-derived OutputClass stands — the row
        // does NOT collapse to Unknown. Forward-compat: a vendor
        // adding `interface = "spdif_optical"` later does not
        // silently unclassify every existing row's output_class.
        let cat = fixture_catalog();
        let raw = "card 0: sndrpihifiberry [snd_rpi_hifiberry_dacplus], device 0: x [y]\n";
        // Kernel introspection identifies card 0 as I²S (Layer 1+2
        // classification baseline).
        let identities = vec![synthetic_identity(
            0,
            crate::kernel_introspection::CardKind::I2s,
        )];
        let active = ActiveDacConfig {
            overlay: "hifiberry-dacplus".into(),
            catalogue_id: Some("hifiberry-dacplus".into()),
            display_name: Some("HiFiBerry DAC+".into()),
            alsacard_hint: Some("sndrpihifiberry".into()),
            mixer_hint: Some("Digital".into()),
            interface: Some("spdif_optical".into()),
            boot_config_path: "/boot/firmware/config.txt".into(),
        };
        let outputs = resolve(raw, &cat, Some(&active), &identities);
        let out = &outputs[0];
        // Kernel-derived OutputClass = I2s; the unrecognised
        // active_dac.interface leaves it alone.
        assert_eq!(out.output_class, OutputClass::I2s);
    }

    #[test]
    fn x86_class_hda_card_uncatalogued_classifies_as_analog_not_unknown() {
        // FAILING-CASE REPRODUCTION pin for the kernel-introspection
        // discipline. Before the kernel-introspection layer landed,
        // an HDA card unknown to alsa-cards.toml's per-card-name
        // table fell through to OutputClass::Unknown — the UI
        // rendered "Other" for a perfectly usable card.
        //
        // After: kernel reports CardKind::Hda; classifier returns
        // OutputClass::Analog. This test uses fixture_catalog
        // (synthetic, no codec overlay) so the label precedence
        // collapses to the kernel-supplied long_name — the next
        // test exercises the codec-overlay branch against the
        // shipped catalogue.
        let cat = fixture_catalog();
        let raw = "card 1: PCH [HDA Intel PCH], device 0: ALC233 Analog [ALC233 Analog]\n";
        let identities = vec![crate::kernel_introspection::CardIdentity {
            card_idx: 1,
            short_id: "PCH".into(),
            long_name: "HDA Intel PCH at 0x98b20000 irq 149".into(),
            driver: "HDA-Intel".into(),
            kind: crate::kernel_introspection::CardKind::Hda,
            codecs: vec![crate::kernel_introspection::CodecIdentity {
                address: 0,
                chip_name: "Realtek ALC233".into(),
                vendor_id: 0x10ec0235,
                subsystem_id: 0x80862074,
                is_hdmi: false,
            }],
            ac97_codec: None,
            usb_dac: None,
        }];
        let outputs = resolve(raw, &cat, None, &identities);
        assert_eq!(outputs.len(), 1);
        let out = &outputs[0];
        assert_eq!(
            out.output_class,
            OutputClass::Analog,
            "kernel CardKind::Hda MUST classify as Analog under \
             the kernel-introspection-authoritative discipline — \
             the prior keyword-scan path returned Unknown and the \
             UI rendered 'Other' for this card",
        );
        // Label falls back to the kernel-supplied chip name (this
        // catalogue has no codec overlay for 0x10ec0235; the
        // fixture's per-card-name table doesn't carry PCH either).
        // Precedence: codec overlay (miss) → kernel chip_name (hit).
        // Synthetic fixture has no codec overlay for 0x10ec0235;
        // precedence resolves to the kernel-supplied chip name.
        assert_eq!(out.label, "Realtek ALC233");
        assert_eq!(out.catalog_provenance, CatalogProvenance::Unmapped);
    }

    #[test]
    fn shipped_catalog_codec_overlay_relabels_hda_card_with_brand_name() {
        // STAGE 2 REGRESSION PIN: against the SHIPPED catalogue
        // (not the synthetic fixture), an HDA card whose kernel
        // card_name is NOT in the per-card-name table AND whose
        // codec vendor_id IS in the `[[hda_codecs]]` overlay
        // surfaces with the catalogue's brand-name label. This is
        // the operator-facing value the codec-overlay widening
        // delivers for hosts the per-card-name catalogue does not
        // cover.
        //
        // Note: when both tables match (per-card-name AND codec
        // overlay), per-card-name wins — the existing subdevice
        // pretty_names ("Analog Out", "HDMI") carry operator-
        // meaningful subdevice-level information that codec-id-
        // keyed branding cannot. The two layers are complementary.
        let cat = AlsaCardCatalog::load_embedded().expect("shipped catalogue");
        let raw = "card 1: FutureAudio [FutureAudio Custom Mainboard], device 0: ALC233 Analog [ALC233 Analog]\n";
        let identities = vec![crate::kernel_introspection::CardIdentity {
            card_idx: 1,
            short_id: "FutureAudio".into(),
            long_name: "FutureAudio Custom Mainboard at 0xabcdef".into(),
            driver: "HDA-Intel".into(),
            kind: crate::kernel_introspection::CardKind::Hda,
            codecs: vec![crate::kernel_introspection::CodecIdentity {
                address: 0,
                chip_name: "Realtek ALC233".into(),
                vendor_id: 0x10ec0235,
                subsystem_id: 0x12345678,
                is_hdmi: false,
            }],
            ac97_codec: None,
            usb_dac: None,
        }];
        let outputs = resolve(raw, &cat, None, &identities);
        assert_eq!(outputs.len(), 1);
        let out = &outputs[0];
        // OutputClass remains Analog (Stage 1 invariant unchanged).
        assert_eq!(out.output_class, OutputClass::Analog);
        // Stage 2 win: label = catalogue overlay brand name from
        // the [[hda_codecs]] table — precedence applies because
        // the per-card-name table does NOT carry "FutureAudio
        // Custom Mainboard".
        assert_eq!(out.label, "Realtek ALC233");
    }

    #[test]
    fn shipped_catalog_codec_overlay_miss_falls_back_to_kernel_chip_name() {
        // Precedence pin: when the codec's vendor_id is NOT in the
        // overlay (Stage 2's initial dataset is intentionally
        // bounded — operator may encounter codecs not yet
        // catalogued), the label falls back to the kernel-supplied
        // chip_name. The kernel name is already operator-readable;
        // catalogue widening grows label quality without breaking
        // anything.
        let cat = AlsaCardCatalog::load_embedded().expect("shipped catalogue");
        let raw = "card 5: Future [Mystery Audio], device 0: foo [bar]\n";
        let identities = vec![crate::kernel_introspection::CardIdentity {
            card_idx: 5,
            short_id: "Future".into(),
            long_name: "Mystery Audio at 0xdeadbeef".into(),
            driver: "HDA-Intel".into(),
            kind: crate::kernel_introspection::CardKind::Hda,
            codecs: vec![crate::kernel_introspection::CodecIdentity {
                address: 0,
                chip_name: "FutureCorp FC9999".into(),
                vendor_id: 0xdeadbeef, // not in any catalogue overlay
                subsystem_id: 0x00000000,
                is_hdmi: false,
            }],
            ac97_codec: None,
            usb_dac: None,
        }];
        let outputs = resolve(raw, &cat, None, &identities);
        let out = &outputs[0];
        // Precedence: codec overlay (miss) → kernel chip_name (hit).
        assert_eq!(out.label, "FutureCorp FC9999");
        // Classification unchanged — Stage 1 path intact.
        assert_eq!(out.output_class, OutputClass::Analog);
    }

    #[test]
    fn shipped_catalog_ac97_codec_overlay_relabels_with_brand_name() {
        // Failing-case reproduction for the AC97 family. An
        // uncatalogued AC97 controller (kernel card_name not in
        // the per-card-name table) whose codec chip_name IS in
        // the [[ac97_codecs]] overlay surfaces with the
        // catalogue's brand label — the operator-facing value
        // the AC97 overlay delivers. Before the AC97 codec parse
        // + overlay path landed, the label was the kernel
        // long_name (the controller identity) — useful but
        // verbose; the catalogue-rebranded chip name is the
        // operator-meaningful identity.
        let cat = AlsaCardCatalog::load_embedded().expect("shipped catalogue");
        let raw = "card 1: SynthCard [Synth AC97 Audio], device 0: foo [bar]\n";
        let identities = vec![crate::kernel_introspection::CardIdentity {
            card_idx: 1,
            short_id: "SynthCard".into(),
            long_name: "Synth AC97 Audio Controller".into(),
            driver: "synth-ac97".into(),
            kind: crate::kernel_introspection::CardKind::Ac97,
            codecs: Vec::new(),
            ac97_codec: Some(crate::kernel_introspection::Ac97Codec {
                chip_name: "Analog Devices AD1980".into(),
            }),
            usb_dac: None,
        }];
        let outputs = resolve(raw, &cat, None, &identities);
        let out = &outputs[0];
        assert_eq!(out.output_class, OutputClass::Analog);
        // Catalogue keys by kernel chip_name; pretty_name is the
        // regen-generated vendor-prefixed form ("Analog Devices
        // AD1980"). The hand-curated "SoundMAX" sub-brand was
        // retired when the regen example replaced hand-curated
        // codec rows with kernel-source-derived rows.
        assert_eq!(out.label, "Analog Devices AD1980");
    }

    #[test]
    fn shipped_catalog_ac97_codec_overlay_miss_falls_back_to_kernel_chip_name()
    {
        // Precedence pin: catalogue miss on AC97 codec → label
        // falls back to the kernel-supplied chip name (already
        // operator-readable). Catalogue widening adds rebrand
        // polish without breaking anything.
        let cat = AlsaCardCatalog::load_embedded().expect("shipped catalogue");
        let raw = "card 1: SynthCard [Synth AC97 Audio], device 0: foo [bar]\n";
        let identities = vec![crate::kernel_introspection::CardIdentity {
            card_idx: 1,
            short_id: "SynthCard".into(),
            long_name: "Synth AC97 Audio Controller".into(),
            driver: "synth-ac97".into(),
            kind: crate::kernel_introspection::CardKind::Ac97,
            codecs: Vec::new(),
            ac97_codec: Some(crate::kernel_introspection::Ac97Codec {
                chip_name: "FutureCorp FC-AC97".into(), // not catalogued
            }),
            usb_dac: None,
        }];
        let outputs = resolve(raw, &cat, None, &identities);
        let out = &outputs[0];
        assert_eq!(out.output_class, OutputClass::Analog);
        // Precedence: catalogue overlay (miss) → kernel chip
        // name (hit).
        assert_eq!(out.label, "FutureCorp FC-AC97");
    }

    #[test]
    fn shipped_catalog_usb_dac_overlay_relabels_with_brand_name() {
        // USB DAC label precedence: when the kernel registered a
        // USB audio device (CardKind::Usb) and the catalogue's
        // [[usb_dacs]] table carries a matching vendor:product
        // entry, the label resolves to the catalogue brand
        // (e.g. "Topping E50") rather than the kernel-supplied
        // long_name (which typically includes a bus-attachment
        // suffix like "Topping E50 at usb-0000:00:14.0-1").
        let cat = AlsaCardCatalog::load_embedded().expect("shipped catalogue");
        let raw = "card 2: USBDAC [Topping E50 at usb-0000:00:14.0-1], device 0: foo [bar]\n";
        let identities = vec![crate::kernel_introspection::CardIdentity {
            card_idx: 2,
            short_id: "USBDAC".into(),
            long_name: "Topping E50 at usb-0000:00:14.0-1, high speed".into(),
            driver: "USB-Audio".into(),
            kind: crate::kernel_introspection::CardKind::Usb,
            codecs: Vec::new(),
            ac97_codec: None,
            usb_dac: Some(crate::kernel_introspection::UsbDac {
                vendor_id: 0x152a,  // Topping
                product_id: 0x8762, // E50
            }),
        }];
        let outputs = resolve(raw, &cat, None, &identities);
        let out = &outputs[0];
        assert_eq!(out.output_class, OutputClass::Usb);
        assert_eq!(out.label, "Topping E50");
    }

    #[test]
    fn shipped_catalog_usb_dac_overlay_miss_falls_back_to_kernel_long_name() {
        // USB DAC precedence pin: catalogue miss → kernel
        // long_name. Operator still sees a usable label.
        let cat = AlsaCardCatalog::load_embedded().expect("shipped catalogue");
        let raw = "card 2: USBDAC [Synthetic USB Audio], device 0: foo [bar]\n";
        let identities = vec![crate::kernel_introspection::CardIdentity {
            card_idx: 2,
            short_id: "USBDAC".into(),
            long_name: "Synthetic USB DAC at usb-bus-1".into(),
            driver: "USB-Audio".into(),
            kind: crate::kernel_introspection::CardKind::Usb,
            codecs: Vec::new(),
            ac97_codec: None,
            usb_dac: Some(crate::kernel_introspection::UsbDac {
                vendor_id: 0xdead,
                product_id: 0xbeef, // not in any catalogue overlay
            }),
        }];
        let outputs = resolve(raw, &cat, None, &identities);
        let out = &outputs[0];
        assert_eq!(out.output_class, OutputClass::Usb);
        // Falls back to kernel long_name (catalogue miss + no
        // codec chip-name surface for USB).
        assert_eq!(out.label, "Synthetic USB DAC at usb-bus-1");
    }

    #[test]
    fn shipped_catalog_codec_overlay_skipped_when_no_codecs() {
        // I²S DAC: kernel introspection produces a CardIdentity
        // with empty codec list. Codec overlay path is skipped;
        // precedence collapses to kernel long_name → aplay
        // card_name. Catalogue widening does NOT regress the I²S
        // path.
        let cat = AlsaCardCatalog::load_embedded().expect("shipped catalogue");
        let raw = "card 3: SynthDAC [Synth I2S DAC], device 0: foo [bar]\n";
        let identities = vec![crate::kernel_introspection::CardIdentity {
            card_idx: 3,
            short_id: "SynthDAC".into(),
            long_name: "Synth I2S DAC".into(),
            driver: "synth_i2s_dac".into(),
            kind: crate::kernel_introspection::CardKind::I2s,
            codecs: Vec::new(), // No codec — I²S DAC
            ac97_codec: None,
            usb_dac: None,
        }];
        let outputs = resolve(raw, &cat, None, &identities);
        let out = &outputs[0];
        assert_eq!(out.label, "Synth I2S DAC");
        assert_eq!(out.output_class, OutputClass::I2s);
    }
}
