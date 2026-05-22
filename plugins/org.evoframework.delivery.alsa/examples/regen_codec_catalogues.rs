//! Regen example — rewrites the `[[hda_codecs]]` and
//! `[[ac97_codecs]]` sections of `data/alsa-cards.toml` from
//! the kernel source files checked into `data/import/`.
//!
//! Usage:
//!
//! ```bash
//! cd plugins/org.evoframework.delivery.alsa
//! cargo run --example regen_codec_catalogues
//! ```
//!
//! Deterministic — same input → byte-identical output. The
//! per-card-name `[[cards]]` section is preserved unchanged.
//!
//! After running, commit the regenerated `data/alsa-cards.toml`
//! alongside any source-file updates under `data/import/`. The
//! regression-guard test pins the importer output against the
//! checked-in file.

use std::fs;
use std::path::{Path, PathBuf};

use org_evoframework_delivery_alsa::import::{
    parse_ac97_codec_entries, parse_hda_codec_entries,
    render_ac97_codecs_section, render_hda_codecs_section, Ac97CodecRow,
    HdaCodecRow,
};

/// HDA patch_*.c source files to scrape, in deterministic
/// alphabetical order so the output is reproducible.
const HDA_SOURCES: &[&str] = &[
    "patch_analog.c",
    "patch_ca0132.c",
    "patch_cirrus.c",
    "patch_cmedia.c",
    "patch_conexant.c",
    "patch_cs8409.c",
    "patch_hdmi.c",
    "patch_realtek.c",
    "patch_si3054.c",
    "patch_sigmatel.c",
    "patch_via.c",
];

const AC97_SOURCE: &str = "ac97_codec.c";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let plugin_root = locate_plugin_root()?;
    let import_dir = plugin_root.join("data").join("import");
    let toml_path = plugin_root.join("data").join("alsa-cards.toml");

    println!("plugin_root: {}", plugin_root.display());
    println!("import_dir:  {}", import_dir.display());
    println!("toml_path:   {}", toml_path.display());
    println!();

    // ---- HDA codecs ----------------------------------------
    let mut hda_rows: Vec<HdaCodecRow> = Vec::new();
    for src_name in HDA_SOURCES {
        let p = import_dir.join(src_name);
        let text = fs::read_to_string(&p)
            .map_err(|e| format!("reading {}: {e}", p.display()))?;
        let rows = parse_hda_codec_entries(&text);
        println!("  {} → {} HDA codec entries", src_name, rows.len());
        hda_rows.extend(rows);
    }
    println!("HDA codec rows total: {}", hda_rows.len());

    // Deduplicate by codec_id, keeping the FIRST seen
    // pretty_name. The kernel source order is deterministic, so
    // this is reproducible. Same chip_name appearing under
    // multiple codec_ids is kept (one row per id).
    let mut seen = std::collections::HashSet::new();
    hda_rows.retain(|r| seen.insert(r.codec_id.clone()));
    println!("HDA codec rows after dedup-by-id: {}", hda_rows.len());

    // Decorate with vendor prefix where the chip name doesn't
    // carry it (kernel chip names are bare like "ALC233" — add
    // the vendor brand from the codec_id's high 16 bits).
    for row in &mut hda_rows {
        row.pretty_name =
            decorate_hda_pretty_name(&row.codec_id, &row.pretty_name);
    }

    // ---- AC97 codecs ---------------------------------------
    let ac97_path = import_dir.join(AC97_SOURCE);
    let ac97_text = fs::read_to_string(&ac97_path)
        .map_err(|e| format!("reading {}: {e}", ac97_path.display()))?;
    let ac97_rows: Vec<Ac97CodecRow> = parse_ac97_codec_entries(&ac97_text);
    println!("AC97 codec rows: {}", ac97_rows.len());

    // ---- Render TOML sections ------------------------------
    let hda_section = render_hda_codecs_section(&hda_rows);
    let ac97_section = render_ac97_codecs_section(&ac97_rows);

    // ---- Rewrite alsa-cards.toml ---------------------------
    // Strategy: read the existing file, replace everything from
    // the first `[[hda_codecs]]` marker onwards with the
    // freshly-rendered sections. The per-card-name `[[cards]]`
    // section above that marker is preserved byte-for-byte.
    let existing = fs::read_to_string(&toml_path)
        .map_err(|e| format!("reading {}: {e}", toml_path.display()))?;
    let pre_codec_text = split_at_codec_section_boundary(&existing);

    let mut new_text = String::with_capacity(existing.len() + 8192);
    new_text.push_str(&pre_codec_text);
    if !new_text.ends_with("\n\n") {
        if !new_text.ends_with('\n') {
            new_text.push('\n');
        }
        new_text.push('\n');
    }
    new_text.push_str(HDA_HEADER_BANNER);
    new_text.push_str(&hda_section);
    new_text.push_str("\n\n");
    new_text.push_str(AC97_HEADER_BANNER);
    new_text.push_str(&ac97_section);
    new_text.push('\n');

    fs::write(&toml_path, &new_text)
        .map_err(|e| format!("writing {}: {e}", toml_path.display()))?;
    println!();
    println!("wrote {} ({} bytes)", toml_path.display(), new_text.len());
    Ok(())
}

/// Compose the operator-friendly HDA codec label by prefixing
/// the kernel chip name with the vendor brand derived from the
/// codec_id's high 16 bits (the HDA vendor id). Kernel chip
/// names are typically bare (`ALC233`, `CX20751`) — the
/// catalogue presents them with vendor branding so the
/// operator UI can render "Realtek ALC233" rather than just
/// "ALC233".
fn decorate_hda_pretty_name(codec_id_hex: &str, chip_name: &str) -> String {
    let codec_id = u32::from_str_radix(codec_id_hex, 16).unwrap_or(0);
    let vendor = (codec_id >> 16) as u16;
    let vendor_label = match vendor {
        0x10ec => "Realtek",
        0x14f1 => "Conexant",
        0x1013 => "Cirrus Logic",
        0x111d => "IDT",
        0x8384 => "SigmaTel",
        0x1102 => "Creative",
        0x1057 => "Motorola",
        0x10de => "NVIDIA",
        0x1002 => "AMD/ATI",
        0x8086 => "Intel",
        0x434d => "C-Media",
        0x1106 => "VIA",
        0x11d4 => "Analog Devices",
        0x4321 => "Si3054",
        0x1aec => "Wolfson",
        0x6803 => "Conexant",
        _ => "",
    };
    if vendor_label.is_empty() || chip_name.starts_with(vendor_label) {
        return chip_name.to_string();
    }
    format!("{vendor_label} {chip_name}")
}

/// Find the boundary in the existing alsa-cards.toml where the
/// codec sections begin. Everything before the boundary is the
/// per-card-name `[[cards]]` section (operator-curated) which
/// the regen example MUST preserve unchanged. The boundary is
/// the `# ====` banner introducing the HDA codec overlay (the
/// regen-marker comment), and failing that, the first
/// `[[hda_codecs]]` line.
fn split_at_codec_section_boundary(text: &str) -> String {
    // Preferred: cut at the banner the previous regen run wrote.
    if let Some(pos) = text.find("# REGEN MARKER — codec overlays") {
        return text[..pos].to_string();
    }
    // Fallback: cut at the first hand-written banner line.
    if let Some(pos) = text.find("# HDA codec overlay") {
        // Walk back to the start of the comment-block (line of `#`s
        // above it).
        let head = &text[..pos];
        // Find the last `# ==` separator above the banner.
        if let Some(sep_pos) = head.rfind("# ====") {
            return text[..sep_pos].to_string();
        }
        return text[..pos].to_string();
    }
    // Final fallback: cut at the first `[[hda_codecs]]` row.
    if let Some(pos) = text.find("[[hda_codecs]]") {
        return text[..pos].to_string();
    }
    // No codec section yet — append after the existing text.
    text.to_string()
}

const HDA_HEADER_BANNER: &str =
    "# REGEN MARKER — codec overlays below are auto-generated by\n\
# `cargo run --example regen_codec_catalogues`. Do not hand-edit;\n\
# the next regen will overwrite. The per-card-name `[[cards]]`\n\
# section above this marker is hand-curated and preserved.\n\
# ============================================================\n\
# HDA codec overlay — operator-friendly chip family labels.\n\
# Scraped from sound/pci/hda/patch_*.c at kernel v6.6.\n\
# ============================================================\n\n";

const AC97_HEADER_BANNER: &str =
    "# ============================================================\n\
# AC97 codec overlay — operator-friendly chip family labels.\n\
# Scraped from sound/pci/ac97/ac97_codec.c at kernel v6.6.\n\
# ============================================================\n\n";

/// Locate the plugin root directory regardless of where cargo
/// invoked the example. The example might be run with cwd =
/// workspace root, plugin root, or anywhere with `--manifest-path`.
fn locate_plugin_root() -> Result<PathBuf, Box<dyn std::error::Error>> {
    // CARGO_MANIFEST_DIR is the directory containing the
    // Cargo.toml for the package the example belongs to —
    // always the plugin root.
    let dir = std::env::var("CARGO_MANIFEST_DIR")
        .map_err(|_| "CARGO_MANIFEST_DIR not set")?;
    let p = PathBuf::from(dir);
    if !p.join("data").join("import").is_dir() {
        return Err(format!(
            "expected data/import/ under {} but it's missing",
            p.display()
        )
        .into());
    }
    Ok(p)
}

#[allow(dead_code)]
fn _unused() -> &'static Path {
    Path::new("")
}
