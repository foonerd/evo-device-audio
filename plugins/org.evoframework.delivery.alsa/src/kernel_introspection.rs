// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Kernel runtime introspection — Layer 1 of the four-layer
//! audio-card identification pipeline (per the kernel-introspection-authoritative discipline).
//!
//! Reads `/proc/asound/cards`, `/proc/asound/cardN/codec#N`,
//! `lspci`, and `lsusb` to produce a typed [`CardIdentity`] per
//! kernel-enumerated card. This is the authoritative source for
//! card identification; downstream layers (capability classifier,
//! catalogue overlay) consume the structured shape without
//! re-parsing the kernel surfaces.
//!
//! The module is pure-data — read-only against the kernel-provided
//! procfs interfaces, no side effects, no kernel writes. Permission-
//! denied reads surface as typed [`IntrospectionError::PermissionDenied`]
//! variants; the caller decides whether to retry under privileged
//! context or fall back to a partial identity.

use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// One audio card the kernel has enumerated. Identification source-
/// of-truth across the four-layer pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardIdentity {
    /// ALSA card index (`N` in `hw:N,M`). Stable across the boot;
    /// may change across reboots.
    pub card_idx: u32,
    /// Short identifier from `/proc/asound/cards` — the token in
    /// brackets on the card line (e.g. `PCH`, `vc4hdmi0`, `DAC`).
    pub short_id: String,
    /// Long-form description from `/proc/asound/cards` (e.g.
    /// `HDA Intel PCH at 0x98b20000 irq 149`). Operator-readable
    /// fallback label when no codec / catalogue identification
    /// resolves.
    pub long_name: String,
    /// Kernel driver string (e.g. `HDA-Intel`, `vc4-hdmi`,
    /// `Loopback`, `I-Sabre_Q2M_DAC`). The discriminator for
    /// [`CardKind`] resolution.
    pub driver: String,
    /// Structural kind — drives Layer 2 classification.
    pub kind: CardKind,
    /// Codecs the kernel registered against this card. Empty for
    /// non-HDA cards (I²S DACs, Loopback, etc.). Multiple entries
    /// for HDA cards with separate analog + HDMI codecs.
    pub codecs: Vec<CodecIdentity>,
    /// AC97 codec the kernel registered against this card, when
    /// applicable. `Some` only for [`CardKind::Ac97`] cards; the
    /// kernel exposes the codec chip name on the first line of
    /// `/proc/asound/cardN/codec97#0/ac97#0-0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ac97_codec: Option<Ac97Codec>,
    /// USB DAC identity (vendor + product IDs) the kernel
    /// registered against this card, when applicable. `Some`
    /// only for [`CardKind::Usb`] cards; the kernel exposes the
    /// vendor:product pair in `/proc/asound/cardN/usbid`
    /// (e.g. `152a:8762`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usb_dac: Option<UsbDac>,
}

/// Structural classification — the discriminator downstream layers
/// branch on. The kernel driver string maps to a `CardKind`; the
/// mapping is the only place the framework interprets kernel-side
/// driver naming.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CardKind {
    /// Intel HD-Audio family — `HDA-Intel`, `HDA-NVidia`,
    /// `HDA-AMD`, etc. Codec list non-empty.
    Hda,
    /// HDMI audio output via the GPU's display block — e.g.
    /// `vc4-hdmi` on Raspberry Pi, `HDA-Intel` HDMI codec siblings
    /// (the codec lives on the HDA controller but the
    /// classification surfaces as HDMI). Codec list MAY be empty
    /// (vc4-hdmi exposes ALSA PCM without a separate codec node).
    Hdmi,
    /// I²S DAC reached via SoC GPIO — Raspberry Pi HATs, Tinkerboard
    /// I²S boards, etc. No codec file; identification falls to the
    /// catalogue's overlay-keyed path.
    I2s,
    /// USB Audio Class device — DAC, interface, headset. The card
    /// driver string is `USB-Audio`.
    Usb,
    /// Bluetooth A2DP / SCO sink — driver string `bluez`-derived.
    Bluetooth,
    /// AC97 audio family — Intel ICH (snd_intel8x0), VIA 82xx
    /// (snd_via82xx), ALi 5451 (snd_ali5451), ATI IXP
    /// (snd_atiixp), Cirrus AC97, and other pre-HDA PCI audio
    /// chipsets. The codec (e.g. `Analog Devices AD1980`,
    /// `Realtek ALC650`, `Sigmatel STAC9750`) is exposed under
    /// `/proc/asound/cardN/codec97#N/ac97#0-0` — distinct from
    /// HDA's `codec#N` files in path + format. Operator-facing
    /// classification is [`OutputClass::Analog`] (AC97 is an
    /// analog-output bus by design). Detection is presence-
    /// driven: any card with a `codec97#N` directory in its
    /// procfs node classifies AC97 regardless of the driver
    /// string variant.
    Ac97,
    /// ALSA Loopback — framework-internal pipeline card; never an
    /// operator-facing output.
    Loopback,
    /// Driver the framework does not yet classify. Surfaces with
    /// the driver string for diagnostic; downstream layers treat
    /// as [`OutputClass::Unknown`].
    Unknown,
}

/// One USB DAC identity. USB audio devices expose their vendor +
/// product IDs through the `/proc/asound/cardN/usbid` file —
/// e.g. `152a:8762` for a Topping E50. The vendor + product pair
/// is the catalogue's USB DAC lookup key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbDac {
    /// USB vendor id (16-bit, e.g. `0x152a` for Topping).
    pub vendor_id: u16,
    /// USB product id (16-bit, e.g. `0x8762`).
    pub product_id: u16,
}

/// One AC97 codec on an AC97-class card. AC97's kernel surface
/// (`/proc/asound/cardN/codec97#N/ac97#0-0`) uses a structurally
/// distinct shape from HDA's `codec#N` files — different path,
/// different content format. The first line of the AC97 codec
/// file carries the chip name as the kernel resolves it from the
/// AC97 vendor/device-id registers (e.g. `Analog Devices AD1980`,
/// `Realtek ALC650`, `Sigmatel STAC9750`); that name is the
/// authoritative identification field for AC97 chips.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ac97Codec {
    /// Chip name as the kernel reports it on the first line of
    /// `/proc/asound/cardN/codec97#N/ac97#0-0` (after the
    /// `B-A/N:` bus/address/codec-number prefix). The most
    /// operator-meaningful identity field for AC97 cards.
    pub chip_name: String,
}

/// One codec on an HDA card. Multiple per card (analog + HDMI codec
/// siblings share an HDA controller). For non-HDA cards the codec
/// list is empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodecIdentity {
    /// Codec address on the HDA bus (typically 0, 1, 2, 3; 0 is
    /// usually the analog codec, higher addresses HDMI / digital
    /// siblings).
    pub address: u8,
    /// Chip name as the kernel reports it on the `Codec:` line
    /// (e.g. `Realtek ALC233`, `Intel Kabylake HDMI`). The most
    /// operator-meaningful identity field.
    pub chip_name: String,
    /// 32-bit codec vendor + device id. High 16 bits are vendor
    /// (e.g. `0x10ec` = Realtek, `0x8086` = Intel); low 16 bits
    /// are chip variant. The catalogue's HDA lookup key.
    pub vendor_id: u32,
    /// 32-bit subsystem identifier — usually the OEM board id
    /// the codec is mounted on (e.g. `0x80862074` on Cannon
    /// Point-LP). Distinguishes per-OEM tuning of the same codec.
    pub subsystem_id: u32,
    /// Whether this codec drives an HDMI output node. Determined
    /// by chip name containing `HDMI` (Intel / NVIDIA / AMD HDMI
    /// codecs all expose this in their reported chip name). When
    /// true and the card's [`CardKind`] is [`Hda`], the card's
    /// classification widens to [`Hdmi`] for THIS codec's PCM
    /// devices.
    pub is_hdmi: bool,
}

/// Error variants from kernel introspection. Every variant carries
/// the underlying diagnostic verbatim so downstream surfaces (the
/// operator-facing UI, the audit log) preserve the debug detail.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum IntrospectionError {
    /// `/proc/asound/cards` (or a per-card file) could not be read.
    /// Operator-readable diagnostic carries the underlying io
    /// failure.
    #[error("filesystem read failed: {0}")]
    FilesystemRead(String),
    /// A required field was missing from the kernel-surfaced data.
    /// Indicates a kernel-API drift or a malformed virtual file;
    /// the operator-facing surface should expose the diagnostic
    /// so the runtime drift is visible.
    #[error("parse failed: {0}")]
    Parse(String),
    /// The reader lacks permission to access a privileged file
    /// (typically `/proc/asound/cardN/codec#N` under hardened
    /// distributions). The caller may retry under elevated
    /// context, or fall back to a partial identity.
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// No card with the requested index. The card was unplugged
    /// between enumeration and per-card read, or the caller
    /// passed an out-of-range index.
    #[error("card not present: {0}")]
    CardNotPresent(String),
}

/// Read `/proc/asound/cards` and return one [`CardIdentity`] per
/// kernel-enumerated card. Codec lists are populated by per-card
/// reads of `/proc/asound/cardN/codec#N` files.
///
/// The function is async because it shells out to filesystem reads;
/// in production it uses `tokio::fs`. In tests, the
/// [`introspect_from_proc`] variant accepts a synthetic
/// `/proc/asound`-shaped directory so the parser exercises against
/// fixture data without privileged reads.
pub async fn introspect_all_cards(
) -> Result<Vec<CardIdentity>, IntrospectionError> {
    introspect_from_proc(Path::new("/proc/asound")).await
}

/// Read a `/proc/asound`-shaped directory and parse one
/// [`CardIdentity`] per card. Exposed for tests against synthetic
/// fixtures.
pub async fn introspect_from_proc(
    proc_asound: &Path,
) -> Result<Vec<CardIdentity>, IntrospectionError> {
    let cards_path = proc_asound.join("cards");
    let cards_text =
        tokio::fs::read_to_string(&cards_path).await.map_err(|e| {
            match e.kind() {
                std::io::ErrorKind::PermissionDenied => {
                    IntrospectionError::PermissionDenied(format!(
                        "cannot read {}: {}",
                        cards_path.display(),
                        e
                    ))
                }
                std::io::ErrorKind::NotFound => {
                    IntrospectionError::CardNotPresent(format!(
                        "no /proc/asound on this host: {}",
                        e
                    ))
                }
                _ => IntrospectionError::FilesystemRead(format!(
                    "reading {}: {}",
                    cards_path.display(),
                    e
                )),
            }
        })?;

    let rows = parse_cards_file(&cards_text)?;
    let mut identities = Vec::with_capacity(rows.len());
    for row in rows {
        let procfs = read_codecs_for_card(proc_asound, row.card_idx).await?;
        let kind = classify_kind(
            &row.driver,
            &procfs.hda_codecs,
            procfs.has_ac97_codec,
        );
        identities.push(CardIdentity {
            card_idx: row.card_idx,
            short_id: row.short_id,
            long_name: row.long_name,
            driver: row.driver,
            kind,
            codecs: procfs.hda_codecs,
            ac97_codec: procfs.ac97_codec,
            usb_dac: procfs.usb_dac,
        });
    }
    Ok(identities)
}

/// Parsed line from `/proc/asound/cards`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CardsFileRow {
    card_idx: u32,
    short_id: String,
    driver: String,
    long_name: String,
}

/// Parse the two-line-per-card shape `/proc/asound/cards` emits.
///
/// Format (per card):
///
/// ```text
///  N [short_id     ]: driver - card_long_name
///                       extended_long_name (optional second line)
/// ```
///
/// Example:
///
/// ```text
///  1 [PCH            ]: HDA-Intel - HDA Intel PCH
///                       HDA Intel PCH at 0x98b20000 irq 149
/// ```
fn parse_cards_file(
    text: &str,
) -> Result<Vec<CardsFileRow>, IntrospectionError> {
    let mut rows = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(header) = lines.next() {
        let trimmed = header.trim_start();
        // Header line begins with the card index (a decimal
        // followed by " [short_id  ]:"); subsequent lines indent
        // with whitespace and continue the long name. Skip blanks.
        if trimmed.is_empty() {
            continue;
        }
        // Robust: header lines start with a digit at any leading-
        // whitespace position.
        let first_char = trimmed.chars().next().unwrap_or(' ');
        if !first_char.is_ascii_digit() {
            // Not a header — orphan continuation line (shouldn't
            // happen in well-formed kernel output); skip.
            continue;
        }
        // Split on the first ']' to capture the short id +
        // everything after.
        let bracket_open = match trimmed.find('[') {
            Some(i) => i,
            None => {
                return Err(IntrospectionError::Parse(format!(
                    "cards header missing '[': {trimmed}"
                )));
            }
        };
        let bracket_close = match trimmed.find(']') {
            Some(i) => i,
            None => {
                return Err(IntrospectionError::Parse(format!(
                    "cards header missing ']': {trimmed}"
                )));
            }
        };
        if bracket_close <= bracket_open {
            return Err(IntrospectionError::Parse(format!(
                "cards header malformed brackets: {trimmed}"
            )));
        }
        let card_idx_str = trimmed[..bracket_open].trim();
        let card_idx: u32 = card_idx_str.parse().map_err(|_| {
            IntrospectionError::Parse(format!(
                "cards header non-numeric index: {card_idx_str}"
            ))
        })?;
        let short_id =
            trimmed[bracket_open + 1..bracket_close].trim().to_string();
        let after_bracket = trimmed[bracket_close + 1..].trim();
        // After ']' the form is `: driver - card_name`.
        let after_colon = match after_bracket.strip_prefix(':') {
            Some(s) => s.trim(),
            None => after_bracket,
        };
        let (driver, card_name_first) = match after_colon.split_once(" - ") {
            Some((d, n)) => (d.trim().to_string(), n.trim().to_string()),
            None => (after_colon.to_string(), String::new()),
        };
        // Continuation line — at most one, whitespace-indented.
        let long_name = if let Some(next) = lines.peek() {
            let next_trim = next.trim_start();
            if next_trim
                .chars()
                .next()
                .map(|c| !c.is_ascii_digit())
                .unwrap_or(true)
                && !next_trim.is_empty()
            {
                let consumed = lines.next().unwrap();
                let extended = consumed.trim().to_string();
                if extended.is_empty() {
                    card_name_first
                } else if card_name_first.is_empty() {
                    extended
                } else {
                    // Prefer the extended form if it provides more
                    // detail; the kernel typically prints the
                    // verbose form on line 2.
                    extended
                }
            } else {
                card_name_first
            }
        } else {
            card_name_first
        };
        rows.push(CardsFileRow {
            card_idx,
            short_id,
            driver,
            long_name,
        });
    }
    Ok(rows)
}

/// Combined readout of `/proc/asound/cardN`'s codec surfaces:
/// HDA codecs (`codec#N` files), AC97 codec (parsed from
/// `codec97#0/ac97#0-0` when present), and the AC97-presence
/// flag (any `codec97#N` directory under the card). The two
/// kernel surfaces are structurally distinct (different naming,
/// different content format) but enumerated in the same
/// directory scan pass — one read trip per card.
struct CardCodecProcfs {
    hda_codecs: Vec<CodecIdentity>,
    ac97_codec: Option<Ac97Codec>,
    has_ac97_codec: bool,
    usb_dac: Option<UsbDac>,
}

/// Scan `/proc/asound/cardN` for codec surfaces. HDA codecs
/// (`codec#N` files) parse into [`CodecIdentity`]; AC97 codecs
/// (`codec97#N` directories) surface BOTH as a presence flag —
/// for [`CardKind::Ac97`] classification — AND as a parsed
/// [`Ac97Codec`] carrying the chip name from
/// `codec97#0/ac97#0-0`.
///
/// Returns empty HDA codec list + `has_ac97_codec=false` for
/// cards with no codec surfaces (I²S DACs, Loopback, vc4-hdmi).
async fn read_codecs_for_card(
    proc_asound: &Path,
    card_idx: u32,
) -> Result<CardCodecProcfs, IntrospectionError> {
    let card_dir = proc_asound.join(format!("card{card_idx}"));
    if !card_dir.exists() {
        return Ok(CardCodecProcfs {
            hda_codecs: Vec::new(),
            ac97_codec: None,
            has_ac97_codec: false,
            usb_dac: None,
        });
    }
    let mut entries = match tokio::fs::read_dir(&card_dir).await {
        Ok(e) => e,
        Err(e) => {
            // Card directory unreadable — surface as parse error
            // (it should exist if /proc/asound/cards listed it).
            return Err(IntrospectionError::FilesystemRead(format!(
                "reading {}: {e}",
                card_dir.display()
            )));
        }
    };
    let mut hda_codec_files = Vec::new();
    let mut ac97_codec_dirs: Vec<(u8, std::path::PathBuf)> = Vec::new();
    let mut usbid_path: Option<std::path::PathBuf> = None;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let name_str = match name.to_str() {
            Some(s) => s,
            None => continue,
        };
        // HDA codec: file named `codec#0`, `codec#2`, etc.
        if let Some(rest) = name_str.strip_prefix("codec#") {
            if let Ok(addr) = rest.parse::<u8>() {
                hda_codec_files.push((addr, entry.path()));
                continue;
            }
        }
        // AC97 codec: directory named `codec97#0`, etc. The
        // chipset-driver string variants (ICH / VIA82xx / ALi /
        // ATIIXP / Cirrus / ...) all expose this same dir
        // shape — presence-detection is robust across the entire
        // AC97 driver family without enumerating chipset names.
        if let Some(rest) = name_str.strip_prefix("codec97#") {
            if let Ok(addr) = rest.parse::<u8>() {
                ac97_codec_dirs.push((addr, entry.path()));
            }
        }
        // USB DAC: file `usbid` carries `vendor:product` hex
        // (e.g. `152a:8762` for a Topping E50). Presence
        // identifies the card as USB-attached; the parsed
        // vendor + product IDs become the catalogue lookup key.
        if name_str == "usbid" {
            usbid_path = Some(entry.path());
        }
    }
    hda_codec_files.sort_by_key(|(addr, _)| *addr);
    ac97_codec_dirs.sort_by_key(|(addr, _)| *addr);

    let mut hda_codecs = Vec::with_capacity(hda_codec_files.len());
    for (address, path) in hda_codec_files {
        let text =
            match tokio::fs::read_to_string(&path).await {
                Ok(t) => t,
                Err(e) => match e.kind() {
                    std::io::ErrorKind::PermissionDenied => {
                        return Err(IntrospectionError::PermissionDenied(
                            format!("cannot read {}: {}", path.display(), e),
                        ));
                    }
                    _ => {
                        return Err(IntrospectionError::FilesystemRead(
                            format!("reading {}: {}", path.display(), e),
                        ));
                    }
                },
            };
        hda_codecs.push(parse_codec_file(address, &text)?);
    }

    // Parse the first AC97 codec's identity. AC97 cards typically
    // expose a single codec; multiple codecs are theoretically
    // possible but unusual on consumer hardware. This scan reads
    // the primary AC97 codec at codec97#0 — the codec on
    // address 0 of the AC-link bus, which is the analog-output
    // codec on every AC97 card the framework targets.
    let has_ac97_codec = !ac97_codec_dirs.is_empty();
    let ac97_codec = if let Some((_, codec_dir)) = ac97_codec_dirs.first() {
        // The chip-name file inside the codec97#N directory
        // follows the `ac97#<bus>-<addr>` convention. Most cards
        // expose `ac97#0-0` for the primary codec. Read it.
        let inner = codec_dir.join("ac97#0-0");
        if inner.exists() {
            match tokio::fs::read_to_string(&inner).await {
                Ok(text) => parse_ac97_codec_file(&text),
                Err(e) => match e.kind() {
                    std::io::ErrorKind::PermissionDenied => {
                        return Err(IntrospectionError::PermissionDenied(
                            format!("cannot read {}: {}", inner.display(), e),
                        ));
                    }
                    _ => {
                        return Err(IntrospectionError::FilesystemRead(
                            format!("reading {}: {}", inner.display(), e),
                        ));
                    }
                },
            }
        } else {
            // Codec97 directory present but the conventional
            // ac97#0-0 file is absent. Card is structurally AC97
            // (presence flag stays true so classification still
            // works) but identity isn't parseable from this
            // surface — caller falls back to long_name.
            None
        }
    } else {
        None
    };

    // USB DAC identification. `/proc/asound/cardN/usbid`
    // contains `vendor:product` hex (e.g. `152a:8762`). Read +
    // parse if present; structural failure (malformed body)
    // surfaces as None rather than a hard error, so a stray
    // kernel quirk doesn't fail enumeration of all cards.
    let usb_dac =
        if let Some(path) = usbid_path {
            match tokio::fs::read_to_string(&path).await {
                Ok(text) => parse_usbid_file(&text),
                Err(e) => match e.kind() {
                    std::io::ErrorKind::PermissionDenied => {
                        return Err(IntrospectionError::PermissionDenied(
                            format!("cannot read {}: {}", path.display(), e),
                        ));
                    }
                    _ => None,
                },
            }
        } else {
            None
        };

    Ok(CardCodecProcfs {
        hda_codecs,
        ac97_codec,
        has_ac97_codec,
        usb_dac,
    })
}

/// Parse the `/proc/asound/cardN/usbid` file's `vendor:product`
/// shape (e.g. `152a:8762\n`). Returns `None` on malformed
/// input.
fn parse_usbid_file(text: &str) -> Option<UsbDac> {
    let trimmed = text.trim();
    let (vendor_str, product_str) = trimmed.split_once(':')?;
    let vendor_id = u16::from_str_radix(vendor_str.trim(), 16).ok()?;
    let product_id = u16::from_str_radix(product_str.trim(), 16).ok()?;
    Some(UsbDac {
        vendor_id,
        product_id,
    })
}

/// Parse the kernel's AC97 codec identity from a
/// `/proc/asound/cardN/codec97#N/ac97#0-0` file. The kernel emits
/// the chip name on the first line as `B-A/N: <chip_name>` where
/// `B-A/N` is the bus / address / codec-number prefix (e.g.
/// `0-0/0: Analog Devices AD1980`). Returns `None` on a
/// malformed first line; the caller surfaces the gap via the
/// label-fallback chain rather than failing the introspection.
fn parse_ac97_codec_file(text: &str) -> Option<Ac97Codec> {
    let first = text.lines().next()?;
    // Split on the first colon — anything before is the prefix,
    // anything after is the chip name.
    let (_prefix, chip_part) = first.split_once(':')?;
    let chip_name = chip_part.trim().to_string();
    if chip_name.is_empty() {
        return None;
    }
    Some(Ac97Codec { chip_name })
}

/// Parse one `/proc/asound/cardN/codec#N` file. The format is
/// well-defined by the kernel — the first ~6 lines carry the
/// identification fields the framework reads. Field name → value
/// is colon-delimited; values are textual (e.g. `Realtek ALC233`)
/// or hex (`0x10ec0235`).
fn parse_codec_file(
    address: u8,
    text: &str,
) -> Result<CodecIdentity, IntrospectionError> {
    let mut chip_name = String::new();
    let mut vendor_id: Option<u32> = None;
    let mut subsystem_id: Option<u32> = None;
    for line in text.lines().take(20) {
        if let Some(rest) = line.strip_prefix("Codec:") {
            chip_name = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("Vendor Id:") {
            vendor_id = parse_hex_field(rest);
        } else if let Some(rest) = line.strip_prefix("Subsystem Id:") {
            subsystem_id = parse_hex_field(rest);
        }
        if !chip_name.is_empty()
            && vendor_id.is_some()
            && subsystem_id.is_some()
        {
            break;
        }
    }
    let vendor_id = vendor_id.ok_or_else(|| {
        IntrospectionError::Parse(format!(
            "codec file missing 'Vendor Id:' field (addr {address})"
        ))
    })?;
    let subsystem_id = subsystem_id.ok_or_else(|| {
        IntrospectionError::Parse(format!(
            "codec file missing 'Subsystem Id:' field (addr {address})"
        ))
    })?;
    if chip_name.is_empty() {
        return Err(IntrospectionError::Parse(format!(
            "codec file missing 'Codec:' field (addr {address})"
        )));
    }
    let is_hdmi = chip_name.to_ascii_lowercase().contains("hdmi");
    Ok(CodecIdentity {
        address,
        chip_name,
        vendor_id,
        subsystem_id,
        is_hdmi,
    })
}

/// Parse a hex field value like ` 0x10ec0235` (whitespace +
/// optional `0x` prefix). Returns None on parse failure so the
/// caller surfaces it as a structured error with field context.
fn parse_hex_field(raw: &str) -> Option<u32> {
    let trimmed = raw.trim();
    let cleaned = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    u32::from_str_radix(cleaned, 16).ok()
}

/// Classify a card's structural [`CardKind`] from its kernel
/// driver string, codec set, and AC97 presence flag. The mapping
/// is the ONLY place the framework interprets kernel-side driver
/// naming; downstream layers branch on [`CardKind`], not the
/// driver string.
fn classify_kind(
    driver: &str,
    codecs: &[CodecIdentity],
    has_ac97_codec: bool,
) -> CardKind {
    let d = driver.to_ascii_lowercase();
    if d == "loopback" {
        return CardKind::Loopback;
    }
    // AC97 presence is detected via the `codec97#N` directory
    // in the card's procfs node — a kernel-surface fact common
    // to every AC97 driver variant (snd_intel8x0 / snd_via82xx /
    // snd_ali5451 / snd_atiixp / etc.). Presence-driven
    // detection is robust across chipset names + kernel-driver-
    // string variants without enumerating every driver tag the
    // AC97 family has shipped over the years.
    if has_ac97_codec {
        return CardKind::Ac97;
    }
    if d.starts_with("hda-") {
        // HDA controller. If EVERY codec on it is HDMI (e.g.
        // discrete-GPU HDMI audio with no analog codec), classify
        // as HDMI; otherwise HDA (mixed / analog-primary).
        if !codecs.is_empty() && codecs.iter().all(|c| c.is_hdmi) {
            return CardKind::Hdmi;
        }
        return CardKind::Hda;
    }
    if d.starts_with("vc4-hdmi") || d == "snd-hdmi" || d.contains("hdmi") {
        return CardKind::Hdmi;
    }
    if d == "usb-audio" || d.starts_with("snd-usb-") {
        return CardKind::Usb;
    }
    if d.starts_with("bluez") || d.contains("bluetooth") {
        return CardKind::Bluetooth;
    }
    // Heuristic for I²S DACs reached via SoC GPIO: driver name
    // contains "dac" / "i2s" / known I²S codec markers. The
    // framework's catalogue overlay (Layer 3) resolves the precise
    // DAC via `alsa_card_hint` lookup; this discriminator only
    // separates I²S from genuinely unknown drivers. Conservative:
    // if a card has zero codecs AND the driver matches a known I²S
    // marker, classify as I²S; otherwise Unknown.
    if codecs.is_empty()
        && (d.contains("dac")
            || d.contains("i2s")
            || d.contains("sabre")
            || d.contains("hifiberry")
            || d.contains("allo")
            || d.contains("iqaudio"))
    {
        return CardKind::I2s;
    }
    CardKind::Unknown
}

/// Classify a [`CardIdentity`] into the operator-facing
/// [`OutputClass`] consumed by the UI's Destination chips and
/// downstream classification surface. Pure function — Layer 2 of
/// the kernel-introspection pipeline. Deterministic over `CardIdentity`; no
/// side effects, no fallback inference at the join site.
///
/// HDA analog cards classify as `Analog` (HDA is its own bus, not
/// I²S; the operator-facing chip family is the analog-codec class
/// regardless of whether the underlying hardware is on the
/// motherboard or a discrete card). HDA controllers whose every
/// codec is HDMI (discrete-GPU audio paths) are upstream-
/// classified as [`CardKind::Hdmi`] by [`classify_kind`], so they
/// surface here as `Hdmi`.
///
/// Loopback cards surface as `Unknown` so the existing
/// `visibleOutputs` UI-side filter (which drops `cardName ==
/// "Loopback"`) continues to hide them. Reclassifying Loopback
/// into a positive bucket would risk operator-facing display of
/// a framework-internal pipeline card.
pub fn classify_from_kernel(
    identity: &CardIdentity,
) -> crate::output_enumeration::OutputClass {
    use crate::output_enumeration::OutputClass;
    match identity.kind {
        CardKind::Hda => OutputClass::Analog,
        CardKind::Hdmi => OutputClass::Hdmi,
        CardKind::I2s => OutputClass::I2s,
        CardKind::Usb => OutputClass::Usb,
        CardKind::Bluetooth => OutputClass::Bluetooth,
        CardKind::Ac97 => OutputClass::Analog,
        CardKind::Loopback => OutputClass::Unknown,
        CardKind::Unknown => OutputClass::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_enumeration::OutputClass;
    use tempfile::TempDir;

    fn identity_with_kind(kind: CardKind) -> CardIdentity {
        CardIdentity {
            card_idx: 1,
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
    fn classify_hda_returns_analog() {
        assert_eq!(
            classify_from_kernel(&identity_with_kind(CardKind::Hda)),
            OutputClass::Analog
        );
    }

    #[test]
    fn classify_hdmi_returns_hdmi() {
        assert_eq!(
            classify_from_kernel(&identity_with_kind(CardKind::Hdmi)),
            OutputClass::Hdmi
        );
    }

    #[test]
    fn classify_i2s_returns_i2s() {
        assert_eq!(
            classify_from_kernel(&identity_with_kind(CardKind::I2s)),
            OutputClass::I2s
        );
    }

    #[test]
    fn classify_usb_returns_usb() {
        assert_eq!(
            classify_from_kernel(&identity_with_kind(CardKind::Usb)),
            OutputClass::Usb
        );
    }

    #[test]
    fn classify_bluetooth_returns_bluetooth() {
        assert_eq!(
            classify_from_kernel(&identity_with_kind(CardKind::Bluetooth)),
            OutputClass::Bluetooth
        );
    }

    #[test]
    fn classify_loopback_returns_unknown_so_ui_filter_hides_it() {
        // Loopback is a framework-internal pipeline card. The
        // delivery surface filters it out at the visibleOutputs
        // pass; classifying as Unknown lets that filter continue
        // to operate. Any positive classification here would
        // risk operator-facing display.
        assert_eq!(
            classify_from_kernel(&identity_with_kind(CardKind::Loopback)),
            OutputClass::Unknown
        );
    }

    #[test]
    fn classify_unknown_returns_unknown_explicit_not_silent() {
        // Engineering-bar dim A: deterministic + explicit failure
        // handling. An unrecognised CardKind surfaces as Unknown
        // through the explicit match arm — no silent inference,
        // no keyword fallback (which retired with the
        // kernel-introspection-authoritative discipline).
        assert_eq!(
            classify_from_kernel(&identity_with_kind(CardKind::Unknown)),
            OutputClass::Unknown
        );
    }

    fn write_synthetic_proc_asound(
        cards_text: &str,
        per_card_codecs: &[(u32, &str)],
    ) -> TempDir {
        let tmp = TempDir::new().expect("tmpdir");
        let root = tmp.path();
        std::fs::write(root.join("cards"), cards_text).expect("write cards");
        for (card_idx, codec_text) in per_card_codecs {
            let card_dir = root.join(format!("card{card_idx}"));
            std::fs::create_dir_all(&card_dir).expect("mkdir card");
            std::fs::write(card_dir.join("codec#0"), codec_text)
                .expect("write codec");
        }
        tmp
    }

    /// Synthetic fixture writer for AC97 cards — defaults to a
    /// stub `ac97#0-0` body that does NOT carry a parseable chip
    /// name (mirrors the codec97-dir-present-but-identity-file-
    /// missing case the introspector handles defensively).
    /// Tests needing a real chip name use
    /// [`write_synthetic_ac97_proc_asound_named`].
    fn write_synthetic_ac97_proc_asound(
        cards_text: &str,
        per_card_has_ac97: &[u32],
    ) -> TempDir {
        let tmp = TempDir::new().expect("tmpdir");
        let root = tmp.path();
        std::fs::write(root.join("cards"), cards_text).expect("write cards");
        for card_idx in per_card_has_ac97 {
            let codec_dir =
                root.join(format!("card{card_idx}")).join("codec97#0");
            std::fs::create_dir_all(&codec_dir).expect("mkdir codec97");
            // Stub body — parser rejects (no colon on first line)
            // → ac97_codec field stays None. Card still
            // classifies as Ac97 via directory presence.
            std::fs::write(codec_dir.join("ac97#0-0"), "stub ac97 codec")
                .expect("write ac97 stub");
        }
        tmp
    }

    /// Synthetic fixture writer for AC97 cards with a parseable
    /// chip-name file. The `ac97#0-0` body follows the kernel
    /// shape (`B-A/N: <chip_name>` on first line).
    fn write_synthetic_ac97_proc_asound_named(
        cards_text: &str,
        per_card_chip_name: &[(u32, &str)],
    ) -> TempDir {
        let tmp = TempDir::new().expect("tmpdir");
        let root = tmp.path();
        std::fs::write(root.join("cards"), cards_text).expect("write cards");
        for (card_idx, chip_name) in per_card_chip_name {
            let codec_dir =
                root.join(format!("card{card_idx}")).join("codec97#0");
            std::fs::create_dir_all(&codec_dir).expect("mkdir codec97");
            let body =
                format!("0-0/0: {chip_name}\n\nPCI Subsys Vendor: 0x0000\n");
            std::fs::write(codec_dir.join("ac97#0-0"), body)
                .expect("write ac97 codec");
        }
        tmp
    }

    #[tokio::test]
    async fn hda_card_with_analog_codec_classifies_as_hda() {
        // Representative HDA card: Intel controller + Realtek
        // ALC233 codec. The kernel driver is `HDA-Intel`; the
        // chip name `Realtek ALC233` does not contain "HDMI";
        // expected classification: CardKind::Hda.
        let cards = " 1 [PCH            ]: HDA-Intel - HDA Intel PCH\n                      HDA Intel PCH at 0x98b20000 irq 149\n";
        let codec = "Codec: Realtek ALC233\nAddress: 0\nAFG Function Id: 0x1 (unsol 1)\nVendor Id: 0x10ec0235\nSubsystem Id: 0x80862074\nRevision Id: 0x100002\n";
        let tmp = write_synthetic_proc_asound(cards, &[(1, codec)]);

        let identities =
            introspect_from_proc(tmp.path()).await.expect("introspect");
        assert_eq!(identities.len(), 1);
        let c = &identities[0];
        assert_eq!(c.card_idx, 1);
        assert_eq!(c.short_id, "PCH");
        assert_eq!(c.driver, "HDA-Intel");
        assert_eq!(c.kind, CardKind::Hda);
        assert_eq!(c.codecs.len(), 1);
        assert_eq!(c.codecs[0].chip_name, "Realtek ALC233");
        assert_eq!(c.codecs[0].vendor_id, 0x10ec0235);
        assert_eq!(c.codecs[0].subsystem_id, 0x80862074);
        assert!(!c.codecs[0].is_hdmi);
    }

    #[tokio::test]
    async fn hda_card_with_only_hdmi_codecs_classifies_as_hdmi() {
        // Discrete-GPU HDMI audio: HDA controller but every codec
        // is an HDMI variant. Classification widens to HDMI.
        let cards = " 1 [HDMI           ]: HDA-Intel - HDA Intel Kabylake\n                      HDA Intel Kabylake HDMI\n";
        let codec = "Codec: Intel Kabylake HDMI\nAddress: 0\nVendor Id: 0x8086280b\nSubsystem Id: 0x80860101\nRevision Id: 0x100000\n";
        let tmp = write_synthetic_proc_asound(cards, &[(1, codec)]);

        let identities =
            introspect_from_proc(tmp.path()).await.expect("introspect");
        assert_eq!(identities.len(), 1);
        assert_eq!(identities[0].kind, CardKind::Hdmi);
        assert!(identities[0].codecs[0].is_hdmi);
    }

    #[tokio::test]
    async fn loopback_card_classifies_as_loopback() {
        let cards =
            " 0 [Loopback       ]: Loopback - Loopback\n                      Loopback 1\n";
        let tmp = write_synthetic_proc_asound(cards, &[]);
        let identities =
            introspect_from_proc(tmp.path()).await.expect("introspect");
        assert_eq!(identities.len(), 1);
        assert_eq!(identities[0].kind, CardKind::Loopback);
        assert!(identities[0].codecs.is_empty());
    }

    #[tokio::test]
    async fn i2s_card_with_no_codec_classifies_as_i2s() {
        // Pi 5 I-Sabre DAC: kernel driver `I-Sabre_Q2M_DAC`, no
        // codec file (I²S DAC, SoC-attached). Classification falls
        // to I²S via the conservative driver-name heuristic
        // (matches `sabre`).
        let cards = " 3 [DAC            ]: I-Sabre_Q2M_DAC - I-Sabre Q2M DAC\n                      I-Sabre Q2M DAC\n";
        let tmp = write_synthetic_proc_asound(cards, &[]);
        let identities =
            introspect_from_proc(tmp.path()).await.expect("introspect");
        assert_eq!(identities.len(), 1);
        assert_eq!(identities[0].kind, CardKind::I2s);
        assert!(identities[0].codecs.is_empty());
    }

    #[tokio::test]
    async fn vc4_hdmi_classifies_as_hdmi() {
        let cards =
            " 0 [vc4hdmi0       ]: vc4-hdmi - vc4-hdmi-0\n                      vc4-hdmi-0\n";
        let tmp = write_synthetic_proc_asound(cards, &[]);
        let identities =
            introspect_from_proc(tmp.path()).await.expect("introspect");
        assert_eq!(identities[0].kind, CardKind::Hdmi);
    }

    #[tokio::test]
    async fn usb_audio_classifies_as_usb() {
        let cards = " 2 [Topping        ]: USB-Audio - Topping E50\n                      Topping E50 at usb-0000:00:14.0-1, high speed\n";
        let tmp = write_synthetic_proc_asound(cards, &[]);
        let identities =
            introspect_from_proc(tmp.path()).await.expect("introspect");
        assert_eq!(identities[0].kind, CardKind::Usb);
    }

    #[tokio::test]
    async fn unknown_driver_classifies_as_unknown_not_silently_misclassified() {
        // A driver the framework doesn't recognise + no codecs
        // surfaces as Unknown, NOT silently misclassified. This
        // is the failure-mode-explicit invariant from
        // engineering-bar dim A.
        let cards =
            " 5 [Mystery        ]: weirdo-future-driver - Mystery Card\n";
        let tmp = write_synthetic_proc_asound(cards, &[]);
        let identities =
            introspect_from_proc(tmp.path()).await.expect("introspect");
        assert_eq!(identities[0].kind, CardKind::Unknown);
        assert_eq!(identities[0].driver, "weirdo-future-driver");
    }

    #[tokio::test]
    async fn multiple_cards_parse_independently_with_correct_codec_assignment()
    {
        // Multi-card host: HDA-Intel + Loopback + Pi-style I²S
        // DAC. Each card classifies independently; the HDA card's
        // codec list does NOT leak into the I²S card's identity.
        let cards = " 0 [Loopback       ]: Loopback - Loopback\n 1 [PCH            ]: HDA-Intel - HDA Intel PCH\n                      HDA Intel PCH at 0x98b20000 irq 149\n 2 [DAC            ]: I-Sabre_Q2M_DAC - I-Sabre Q2M DAC\n";
        let codec = "Codec: Realtek ALC233\nAddress: 0\nVendor Id: 0x10ec0235\nSubsystem Id: 0x80862074\nRevision Id: 0x100002\n";
        let tmp = write_synthetic_proc_asound(cards, &[(1, codec)]);

        let identities =
            introspect_from_proc(tmp.path()).await.expect("introspect");
        assert_eq!(identities.len(), 3);
        assert_eq!(identities[0].kind, CardKind::Loopback);
        assert_eq!(identities[0].codecs.len(), 0);
        assert_eq!(identities[1].kind, CardKind::Hda);
        assert_eq!(identities[1].codecs.len(), 1);
        assert_eq!(identities[1].codecs[0].chip_name, "Realtek ALC233");
        assert_eq!(identities[2].kind, CardKind::I2s);
        assert_eq!(identities[2].codecs.len(), 0);
    }

    #[tokio::test]
    async fn codec_file_with_missing_vendor_id_surfaces_parse_error() {
        // Explicit failure semantics: a malformed codec file
        // surfaces a Parse error variant carrying the field-name
        // diagnostic. Operator-readable; not silent.
        let cards = " 1 [PCH            ]: HDA-Intel - HDA Intel PCH\n";
        let codec = "Codec: SomeChip\nAddress: 0\nRevision Id: 0x100002\n"; // no Vendor Id
        let tmp = write_synthetic_proc_asound(cards, &[(1, codec)]);

        let err = introspect_from_proc(tmp.path()).await.unwrap_err();
        match err {
            IntrospectionError::Parse(msg) => {
                assert!(
                    msg.contains("Vendor Id"),
                    "diagnostic must name the missing field: {msg}"
                );
            }
            other => panic!("expected Parse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_proc_asound_produces_empty_identity_list() {
        let tmp = write_synthetic_proc_asound("", &[]);
        let identities =
            introspect_from_proc(tmp.path()).await.expect("introspect");
        assert!(identities.is_empty());
    }

    #[tokio::test]
    async fn ac97_card_with_codec97_dir_classifies_as_ac97() {
        // FAILING-CASE REPRODUCTION pin: the Intel 82801AA-ICH +
        // Analog Devices AD1980 AC97 codec scenario. Kernel driver
        // string is `ICH`, codec file path is
        // `codec97#0/ac97#0-0` (directory shape, not HDA's
        // `codec#N` file shape). Before AC97 detection landed,
        // this card fell through classify_kind to CardKind::
        // Unknown → OutputClass::Unknown → UI "Other".
        //
        // After: the codec97#N directory presence triggers
        // CardKind::Ac97 classification regardless of the driver
        // string (kernel-surface-driven detection, robust across
        // chipset variants).
        let cards = " 1 [I82801AAICH    ]: ICH - Intel 82801AA-ICH\n                      Intel 82801AA-ICH with AD1980 at irq 21\n";
        let tmp = write_synthetic_ac97_proc_asound(cards, &[1]);
        let identities =
            introspect_from_proc(tmp.path()).await.expect("introspect");
        assert_eq!(identities.len(), 1);
        let c = &identities[0];
        assert_eq!(c.card_idx, 1);
        assert_eq!(c.driver, "ICH");
        assert_eq!(c.kind, CardKind::Ac97);
        // HDA codec list is empty — AC97 codecs use a distinct
        // procfs surface; populating CodecIdentity from
        // codec97#N/ac97#0-0 is a future extension.
        assert!(c.codecs.is_empty());
    }

    #[tokio::test]
    async fn ac97_detection_robust_across_driver_string_variants() {
        // Kernel-surface-driven detection: the codec97#N directory
        // presence triggers Ac97 classification regardless of
        // what the driver string says. Test fixtures cover three
        // common AC97 chipset families with different driver
        // strings — all classify as Ac97 because each has a
        // codec97#0 directory.
        let cards = " 0 [VIA8233]: VIA8233 - VIA 8233 Audio\n 1 [ALI5451]: ALI5451 - ALi M5451 PCI\n 2 [ATIIXP]: ATIIXP - ATI IXP\n";
        let tmp = write_synthetic_ac97_proc_asound(cards, &[0, 1, 2]);
        let identities =
            introspect_from_proc(tmp.path()).await.expect("introspect");
        assert_eq!(identities.len(), 3);
        for c in &identities {
            assert_eq!(
                c.kind,
                CardKind::Ac97,
                "AC97 detection MUST be driver-string-agnostic; \
                 driver={} should still classify as Ac97 via \
                 codec97 directory presence",
                c.driver
            );
        }
    }

    #[test]
    fn classify_ac97_returns_analog() {
        // Layer 2 arm: CardKind::Ac97 → OutputClass::Analog. AC97
        // is an analog-output bus by design; the codec feeds the
        // line/headphone jack. Operator-facing class is the same
        // as HDA analog.
        assert_eq!(
            classify_from_kernel(&CardIdentity {
                card_idx: 1,
                short_id: "synthcard".into(),
                long_name: "Synthetic AC97 audio controller".into(),
                driver: "synth-ac97".into(),
                kind: CardKind::Ac97,
                codecs: Vec::new(),
                ac97_codec: None,
                usb_dac: None,
            }),
            crate::output_enumeration::OutputClass::Analog
        );
    }

    #[tokio::test]
    async fn ac97_codec_file_parsed_into_chip_name() {
        // Failing-case extension: codec97 dir presence triggers
        // Ac97 classification (already covered), AND the
        // ac97#0-0 file's first line parses into the ac97_codec
        // field carrying the chip name. The kernel surfaces the
        // chip name authoritatively; the parser extracts it from
        // the `B-A/N: <chip_name>` format.
        let cards = " 1 [synthcard]: synth-ac97 - Synthetic Audio Controller\n";
        let tmp = write_synthetic_ac97_proc_asound_named(
            cards,
            &[(1, "Analog Devices AD1980")],
        );
        let identities =
            introspect_from_proc(tmp.path()).await.expect("introspect");
        assert_eq!(identities.len(), 1);
        let c = &identities[0];
        assert_eq!(c.kind, CardKind::Ac97);
        let codec = c
            .ac97_codec
            .as_ref()
            .expect("ac97_codec parsed from ac97#0-0");
        assert_eq!(codec.chip_name, "Analog Devices AD1980");
    }

    #[tokio::test]
    async fn ac97_codec_file_missing_leaves_codec_none_but_classification_intact(
    ) {
        // Defensive: codec97 directory present but the ac97#0-0
        // file is absent (or the body has no parseable first
        // line). Classification still works via dir-presence;
        // ac97_codec stays None and the caller falls back to
        // long_name in the label-precedence chain.
        let cards = " 1 [synthcard]: synth-ac97 - Synthetic Audio Controller\n";
        let tmp = write_synthetic_ac97_proc_asound(cards, &[1]);
        let identities =
            introspect_from_proc(tmp.path()).await.expect("introspect");
        let c = &identities[0];
        assert_eq!(c.kind, CardKind::Ac97);
        // Stub body has no colon on first line → parser rejects
        // → ac97_codec stays None.
        assert!(c.ac97_codec.is_none());
    }

    #[test]
    fn parse_ac97_codec_file_extracts_chip_name() {
        // Direct parser test for the AC97 codec file format. The
        // kernel emits `B-A/N: <chip_name>` on the first line —
        // any of the kernel's known prefix shapes (`0-0/0:`,
        // `0-1/1:`, etc.) parses to the same shape.
        let text =
            "0-0/0: Analog Devices AD1980\n\nPCI Subsys Vendor: 0x1028\n";
        let codec = parse_ac97_codec_file(text)
            .expect("first line parses into Ac97Codec");
        assert_eq!(codec.chip_name, "Analog Devices AD1980");
    }

    #[test]
    fn parse_usbid_file_extracts_vendor_product() {
        // The kernel writes `vendor:product` hex with a trailing
        // newline. Verify the parser handles whitespace.
        let codec =
            parse_usbid_file("152a:8762\n").expect("well-formed usbid parses");
        assert_eq!(codec.vendor_id, 0x152a);
        assert_eq!(codec.product_id, 0x8762);
    }

    #[test]
    fn parse_usbid_file_rejects_malformed_input() {
        assert!(parse_usbid_file("not a usbid").is_none());
        assert!(parse_usbid_file("").is_none());
        assert!(parse_usbid_file("152a").is_none()); // no colon
        assert!(parse_usbid_file("xx:yy").is_none()); // non-hex
    }

    #[tokio::test]
    async fn usb_audio_card_parses_usbid_into_usb_dac_field() {
        // Failing-case reproduction for USB DACs: the kernel
        // writes vendor:product to /proc/asound/cardN/usbid. The
        // introspector must read it + populate CardIdentity.usb_dac
        // so the catalogue overlay can rebrand the device.
        let cards = " 2 [USBDAC]: USB-Audio - Synth USB DAC\n";
        let tmp = TempDir::new().expect("tmpdir");
        let root = tmp.path();
        std::fs::write(root.join("cards"), cards).expect("write cards");
        let card_dir = root.join("card2");
        std::fs::create_dir_all(&card_dir).expect("mkdir");
        std::fs::write(card_dir.join("usbid"), "152a:8762\n")
            .expect("write usbid");

        let identities = introspect_from_proc(root).await.expect("introspect");
        assert_eq!(identities.len(), 1);
        let c = &identities[0];
        assert_eq!(c.kind, CardKind::Usb);
        let usb = c.usb_dac.as_ref().expect("usb_dac parsed");
        assert_eq!(usb.vendor_id, 0x152a);
        assert_eq!(usb.product_id, 0x8762);
    }

    #[test]
    fn parse_ac97_codec_file_rejects_malformed_first_line() {
        // First line without a colon — defensive rejection.
        assert!(parse_ac97_codec_file("not a codec header\n").is_none());
        assert!(parse_ac97_codec_file("").is_none());
        // Colon present but empty chip name — rejected.
        assert!(parse_ac97_codec_file("0-0/0:    \n").is_none());
    }
}
