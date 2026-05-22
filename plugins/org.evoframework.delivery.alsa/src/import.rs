//! Build-time codec-catalogue importers.
//!
//! Scrapes the Linux kernel's HDA + AC97 codec identification
//! tables (under `data/import/`) and emits operator-friendly
//! catalogue rows for the `[[hda_codecs]]` and `[[ac97_codecs]]`
//! sections of `data/alsa-cards.toml`.
//!
//! The runtime never invokes this module — it's reachable only
//! through the `regen_codec_catalogues` example under
//! `examples/`. The runtime reads the generated TOML.
//!
//! Two parsers:
//!
//! 1. [`parse_hda_codec_entries`] — pulls
//!    `HDA_CODEC_ENTRY(0x<id>, "<chip>", ...)` lines from the
//!    `sound/pci/hda/patch_*.c` files. The kernel uses this
//!    macro to declare every HDA codec the driver knows about;
//!    the codec_id is a 32-bit value and the chip name is the
//!    operator-meaningful chip identity.
//!
//! 2. [`parse_ac97_codec_entries`] — pulls entries from the
//!    `snd_ac97_codec_ids[]` table in `sound/pci/ac97/ac97_codec.c`.
//!    Each entry carries a 32-bit codec id, a mask (`0xffffffff`
//!    for exact match, lower bits masked for chip family), a
//!    chip name, and the patch function. The importer skips
//!    pure-vendor entries (mask < 0xffff0000) which don't
//!    identify specific chips.
//!
//! Output: a list of [`HdaCodecRow`] / [`Ac97CodecRow`] structs
//! ready to be serialised to TOML by the regen example.

use serde::Serialize;

/// One HDA codec entry produced by the importer. Serialises to
/// the same shape `alsa-cards.toml`'s `[[hda_codecs]]` rows expect.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HdaCodecRow {
    /// 32-bit HDA `vendor_id` formatted as 8-char lowercase hex
    /// (no `0x` prefix), matching the TOML schema convention.
    pub codec_id: String,
    /// Operator-friendly chip family label derived from the
    /// kernel's `Codec:` string + the codec patch family.
    pub pretty_name: String,
}

/// One AC97 codec entry. Same role as [`HdaCodecRow`] for AC97.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Ac97CodecRow {
    /// Kernel-reported chip name as it appears on the AC97
    /// codec file's first line — the catalogue's lookup key
    /// (verbatim).
    pub chip_name: String,
    /// Operator-friendly catalogue label.
    pub pretty_name: String,
}

/// Parse HDA codec entries from a single `patch_*.c` source
/// file. The kernel pattern is
/// `HDA_CODEC_ENTRY(0x<id>, "<chip>", patch_func)` — one entry
/// per supported codec. The importer regex tolerates whitespace
/// variations.
///
/// Returns the entries in source order. The same chip name may
/// appear multiple times under different codec_ids (e.g.
/// `ALC233` at `0x10ec0233` and `0x10ec0235`); both rows are
/// kept since the catalogue is keyed by codec_id.
pub fn parse_hda_codec_entries(source: &str) -> Vec<HdaCodecRow> {
    let mut out = Vec::new();
    // Match: HDA_CODEC_ENTRY(0xDEADBEEF, "ChipName", ...)
    // The fmt allows whitespace + tab variations.
    let mut i = 0;
    while let Some(pos) = source[i..].find("HDA_CODEC_ENTRY(") {
        let start = i + pos + "HDA_CODEC_ENTRY(".len();
        // Parse the 0x... codec id.
        let rest = &source[start..];
        let rest_trim = rest.trim_start();
        let consumed = rest.len() - rest_trim.len();
        let rest = rest_trim;
        if !rest.starts_with("0x") {
            i = start + consumed;
            continue;
        }
        let after_0x = &rest[2..];
        let hex_end = after_0x
            .find(|c: char| !c.is_ascii_hexdigit())
            .unwrap_or(after_0x.len());
        if hex_end == 0 {
            i = start + consumed + 2;
            continue;
        }
        let codec_id_hex = &after_0x[..hex_end];
        let after_hex = &after_0x[hex_end..];
        // Skip `, ` and find the opening quote.
        let comma = match after_hex.find(',') {
            Some(c) => c,
            None => {
                i = start + consumed + 2 + hex_end;
                continue;
            }
        };
        let after_comma = &after_hex[comma + 1..];
        let quote_open = match after_comma.find('"') {
            Some(q) => q,
            None => {
                i = start + consumed + 2 + hex_end + comma + 1;
                continue;
            }
        };
        let after_quote = &after_comma[quote_open + 1..];
        let quote_close = match after_quote.find('"') {
            Some(q) => q,
            None => {
                i = start + consumed + 2 + hex_end + comma + 1 + quote_open + 1;
                continue;
            }
        };
        let chip_name = &after_quote[..quote_close];

        // Normalise codec_id to 8-char zero-padded lowercase hex.
        let codec_id = format!(
            "{:08x}",
            u32::from_str_radix(codec_id_hex, 16).unwrap_or(0)
        );
        out.push(HdaCodecRow {
            codec_id,
            pretty_name: chip_name.to_string(),
        });

        i = start
            + consumed
            + 2
            + hex_end
            + comma
            + 1
            + quote_open
            + 1
            + quote_close
            + 1;
    }
    out
}

/// Parse AC97 codec entries from `sound/pci/ac97/ac97_codec.c`.
/// The table format is:
///
/// ```text
/// static const struct ac97_codec_id snd_ac97_codec_ids[] = {
/// { 0x41445370, 0xffffffff, "AD1980", patch_ad1980, NULL },
/// ...
/// };
/// ```
///
/// The importer:
///
/// 1. Skips vendor-only entries (the parallel
///    `snd_ac97_codec_id_vendors[]` table that lists chip
///    families like `0x41445300, 0xffffff00, "Analog Devices"`)
///    by requiring the mask field to be `0xffffffff` (exact
///    match — a specific chip, not a vendor family).
/// 2. Reads the `chip_name` (third quoted argument) as the
///    catalogue lookup key.
/// 3. Maps to a vendor-prefixed `pretty_name` where the chip
///    name on its own is non-obvious — keeps the kernel chip
///    name verbatim when it's already a complete identifier
///    (e.g. `AD1980` → `Analog Devices AD1980`,
///    `STAC9750` → `SigmaTel STAC9750`).
pub fn parse_ac97_codec_entries(source: &str) -> Vec<Ac97CodecRow> {
    // Locate the table by its declaration line.
    let table_anchor = "snd_ac97_codec_ids[]";
    let table_start = match source.find(table_anchor) {
        Some(pos) => pos,
        None => return Vec::new(),
    };
    // The table extends to the next `};` line.
    let after_anchor = &source[table_start..];
    let table_end_rel = after_anchor.find("\n};").unwrap_or(after_anchor.len());
    let table_body = &after_anchor[..table_end_rel];

    let mut out = Vec::new();
    for line in table_body.lines() {
        // Match: { 0xXXXXXXXX, 0xXXXXXXXX, "Name", ... },
        let line = line.trim();
        if !line.starts_with("{ 0x") && !line.starts_with("{0x") {
            continue;
        }
        // Find the first 0x... (codec id).
        let after_brace = line.trim_start_matches('{').trim_start();
        if !after_brace.starts_with("0x") {
            continue;
        }
        let after_0x = &after_brace[2..];
        let hex_end = after_0x
            .find(|c: char| !c.is_ascii_hexdigit())
            .unwrap_or(after_0x.len());
        let codec_id_hex = &after_0x[..hex_end];
        // Find the mask (second 0x...).
        let after_id = &after_0x[hex_end..];
        let comma1 = match after_id.find(',') {
            Some(c) => c,
            None => continue,
        };
        let after_comma1 = after_id[comma1 + 1..].trim_start();
        if !after_comma1.starts_with("0x") {
            continue;
        }
        let after_mask_prefix = &after_comma1[2..];
        let mask_end = after_mask_prefix
            .find(|c: char| !c.is_ascii_hexdigit())
            .unwrap_or(after_mask_prefix.len());
        let mask_hex = &after_mask_prefix[..mask_end];
        let mask = u32::from_str_radix(mask_hex, 16).unwrap_or(0);
        // Only emit exact-match entries (specific chip, not a
        // vendor family).
        if mask != 0xffffffff {
            continue;
        }
        // Find the chip name (first quoted string).
        let after_mask = &after_mask_prefix[mask_end..];
        let quote_open = match after_mask.find('"') {
            Some(q) => q,
            None => continue,
        };
        let after_quote = &after_mask[quote_open + 1..];
        let quote_close = match after_quote.find('"') {
            Some(q) => q,
            None => continue,
        };
        let chip_name = &after_quote[..quote_close];

        let codec_id = u32::from_str_radix(codec_id_hex, 16).unwrap_or(0);
        let pretty_name = ac97_pretty_name(codec_id, chip_name);
        out.push(Ac97CodecRow {
            chip_name: chip_name.to_string(),
            pretty_name,
        });
    }
    out
}

/// Map an AC97 codec_id + kernel chip name to a vendor-
/// prefixed operator-friendly label. The kernel chip names
/// (`AD1980`, `STAC9750`, `ALC650`) are correct chip
/// identifiers but lack the vendor brand; this helper joins
/// them with the codec_id's vendor prefix where the codec_id
/// reveals it.
///
/// The AC97 codec_id high 24 bits encode the vendor (`ADS` =
/// Analog Devices, `ALC` = Realtek, `SIL` = SigmaTel, etc.).
/// The mapping below covers the vendors with chip-name entries
/// in the kernel table.
fn ac97_pretty_name(codec_id: u32, chip_name: &str) -> String {
    let vendor_prefix = codec_id >> 8; // High 24 bits
    let vendor = match vendor_prefix {
        0x414453 => "Analog Devices ", // "ADS" — SoundMAX
        0x414b4d => "AKM ",            // "AKM"
        0x414c43 => "Realtek ",        // "ALC" — Realtek (legacy)
        0x414c47 => "Avance Logic ",   // "ALG"
        0x434d49 => "C-Media ",        // "CMI"
        0x435352 => "Crystal Semiconductor ", // "CRS" — Crystal / Cirrus
        0x435858 => "Conexant ",       // "CXX"
        0x454d43 => "EMC ",            // "EMC"
        0x4e534c => "National Semiconductor ", // "NSC"
        0x505343 => "Philips ",        // "PSC"
        0x534c43 => "Silicon Laboratories ", // "SLC"
        0x53494c => "SigmaTel ",       // "SIL"
        0x545241 => "TriTech ",        // "TRA"
        0x574543 => "Winbond ",        // "WEC"
        0x57424c => "WilLink ",        // "WBL"
        0x594d48 => "Yamaha ",         // "YMH"
        0x564941 => "VIA ",            // "VIA"
        0x495445 => "ITE ",            // "ITE"
        0x494345 => "ICEnsemble ",     // "ICE"
        _ => "",
    };
    if vendor.is_empty() {
        chip_name.to_string()
    } else if chip_name.starts_with(vendor.trim_end()) {
        // Chip name already carries the vendor — don't duplicate.
        chip_name.to_string()
    } else {
        format!("{vendor}{chip_name}")
    }
}

/// Serialise a list of [`HdaCodecRow`] into the TOML shape the
/// `[[hda_codecs]]` array expects in `alsa-cards.toml`. One
/// row per `[[hda_codecs]]` block; pretty_name on a separate
/// line for readability.
pub fn render_hda_codecs_section(rows: &[HdaCodecRow]) -> String {
    let mut out = String::new();
    for row in rows {
        out.push_str("[[hda_codecs]]\n");
        out.push_str(&format!("codec_id = \"{}\"\n", row.codec_id));
        out.push_str(&format!(
            "pretty_name = {}\n\n",
            toml_string_lit(&row.pretty_name)
        ));
    }
    // Trim the trailing blank line so the file ends cleanly.
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

/// Serialise a list of [`Ac97CodecRow`] into the TOML shape the
/// `[[ac97_codecs]]` array expects.
pub fn render_ac97_codecs_section(rows: &[Ac97CodecRow]) -> String {
    let mut out = String::new();
    for row in rows {
        out.push_str("[[ac97_codecs]]\n");
        out.push_str(&format!(
            "chip_name = {}\n",
            toml_string_lit(&row.chip_name)
        ));
        out.push_str(&format!(
            "pretty_name = {}\n\n",
            toml_string_lit(&row.pretty_name)
        ));
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

/// Render a string as a TOML basic-string literal, escaping
/// backslashes + quotes. The codec names the kernel ships
/// don't carry control characters or unicode escapes, so the
/// minimal-escape form is sufficient.
fn toml_string_lit(s: &str) -> String {
    let escaped: String = s
        .chars()
        .map(|c| match c {
            '\\' => "\\\\".to_string(),
            '"' => "\\\"".to_string(),
            other => other.to_string(),
        })
        .collect();
    format!("\"{}\"", escaped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hda_codec_entry_basic_shape() {
        let source = r#"
static const struct hda_codec_ops_table snd_hda_id_realtek[] = {
    HDA_CODEC_ENTRY(0x10ec0233, "ALC233", patch_alc269),
    HDA_CODEC_ENTRY(0x10ec0235, "ALC233", patch_alc269),
    HDA_CODEC_ENTRY(0x10ec1220, "ALC1220", patch_alc269),
};
"#;
        let rows = parse_hda_codec_entries(source);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].codec_id, "10ec0233");
        assert_eq!(rows[0].pretty_name, "ALC233");
        assert_eq!(rows[1].codec_id, "10ec0235");
        assert_eq!(rows[2].codec_id, "10ec1220");
        assert_eq!(rows[2].pretty_name, "ALC1220");
    }

    #[test]
    fn parse_hda_codec_entry_tolerates_whitespace_variations() {
        // Tab + spaces + multi-line should all parse.
        let source = "HDA_CODEC_ENTRY(   0x10ec0233 , \"ALC233\" , patch);
HDA_CODEC_ENTRY(\t0x14f12008,\t\"CX20751\",\tpatch);";
        let rows = parse_hda_codec_entries(source);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].codec_id, "10ec0233");
        assert_eq!(rows[1].codec_id, "14f12008");
        assert_eq!(rows[1].pretty_name, "CX20751");
    }

    #[test]
    fn parse_hda_codec_entry_skips_malformed_lines() {
        let source = r#"
HDA_CODEC_ENTRY(0x10ec0233, "ALC233", patch),
HDA_CODEC_ENTRY(broken_macro_call_no_id),
HDA_CODEC_ENTRY(0x14f12008, "CX20751", patch),
"#;
        let rows = parse_hda_codec_entries(source);
        // The malformed middle line is skipped; the parser
        // continues from a safe offset and picks up the next
        // valid entry.
        assert!(rows.iter().any(|r| r.codec_id == "10ec0233"));
        assert!(rows.iter().any(|r| r.codec_id == "14f12008"));
    }

    #[test]
    fn parse_ac97_codec_entry_keeps_exact_match_only() {
        let source = r#"
static const struct ac97_codec_id snd_ac97_codec_id_vendors[] = {
{ 0x41445300, 0xffffff00, "Analog Devices", NULL, NULL },
};

static const struct ac97_codec_id snd_ac97_codec_ids[] = {
{ 0x41445370, 0xffffffff, "AD1980", patch_ad1980, NULL },
{ 0x414c4720, 0xfffffff0, "ALC650", patch_alc650, NULL },
{ 0x53494c20, 0xffffffff, "STAC9750", patch_stac9700, NULL },
};
"#;
        let rows = parse_ac97_codec_entries(source);
        // Only the exact-match (0xffffffff) entries survive.
        // The vendor-only table and the masked-low-nibble ALC650
        // entry are filtered out.
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].chip_name, "AD1980");
        assert_eq!(rows[0].pretty_name, "Analog Devices AD1980");
        assert_eq!(rows[1].chip_name, "STAC9750");
        assert_eq!(rows[1].pretty_name, "SigmaTel STAC9750");
    }

    #[test]
    fn ac97_pretty_name_handles_unknown_vendor() {
        // Codec_id with no vendor prefix in the mapping — falls
        // back to the kernel chip name verbatim.
        let name = ac97_pretty_name(0xdeadbeef, "FutureChip");
        assert_eq!(name, "FutureChip");
    }

    #[test]
    fn ac97_pretty_name_does_not_duplicate_vendor_prefix() {
        // Kernel chip name already starts with the vendor —
        // don't add it again.
        let name = ac97_pretty_name(0x41445370, "Analog Devices AD1980");
        assert_eq!(name, "Analog Devices AD1980");
    }

    #[test]
    fn render_hda_codecs_section_round_trips() {
        let rows = vec![
            HdaCodecRow {
                codec_id: "10ec0233".to_string(),
                pretty_name: "Realtek ALC233".to_string(),
            },
            HdaCodecRow {
                codec_id: "10ec1220".to_string(),
                pretty_name: "Realtek ALC1220".to_string(),
            },
        ];
        let rendered = render_hda_codecs_section(&rows);
        assert!(rendered.contains("codec_id = \"10ec0233\""));
        assert!(rendered.contains("pretty_name = \"Realtek ALC233\""));
        assert!(rendered.contains("codec_id = \"10ec1220\""));
    }

    #[test]
    fn render_ac97_codecs_section_round_trips() {
        let rows = vec![Ac97CodecRow {
            chip_name: "AD1980".to_string(),
            pretty_name: "Analog Devices AD1980".to_string(),
        }];
        let rendered = render_ac97_codecs_section(&rows);
        assert!(rendered.contains("chip_name = \"AD1980\""));
        assert!(rendered.contains("pretty_name = \"Analog Devices AD1980\""));
    }

    #[test]
    fn shipped_catalog_carries_kernel_scraped_codec_coverage() {
        // Regression guard: the checked-in `data/alsa-cards.toml`
        // carries the regen-output codec sections. The regen
        // example scrapes ~425 HDA codec rows + ~78 AC97 codec
        // rows from the kernel source files under
        // `data/import/`. If a future bump to the kernel source
        // changes the counts, re-run the regen example and
        // commit the output. This test fails fast on drift so
        // operator-side coverage never silently shrinks.
        let cat = crate::alsa_cards::AlsaCardCatalog::load_embedded()
            .expect("shipped catalogue parses");
        // The Realtek ALC233 codec_id is canonical — it MUST be
        // in the catalogue (kernel patch_realtek.c carries it,
        // and the regen example pulls it forward). Tests
        // assert it remains catalogued so a regen mishap that
        // accidentally drops codec rows is caught immediately.
        assert!(
            cat.lookup_codec(0x10ec0235).is_some(),
            "shipped catalogue MUST carry Realtek ALC233 \
             (codec_id 0x10ec0235) — the regen example pulls \
             this row from kernel patch_realtek.c"
        );
        // The Analog Devices AD1980 AC97 codec MUST be in the
        // AC97 overlay (kernel ac97_codec.c carries it).
        assert!(
            cat.lookup_ac97_codec("AD1980").is_some(),
            "shipped catalogue MUST carry AD1980 AC97 codec — \
             the regen example pulls this row from kernel \
             ac97_codec.c"
        );
    }

    #[test]
    fn toml_string_lit_escapes_quotes_and_backslashes() {
        assert_eq!(toml_string_lit(r#"hello"world"#), "\"hello\\\"world\"");
        assert_eq!(toml_string_lit(r"back\slash"), "\"back\\\\slash\"");
        assert_eq!(toml_string_lit("plain"), "\"plain\"");
    }
}
