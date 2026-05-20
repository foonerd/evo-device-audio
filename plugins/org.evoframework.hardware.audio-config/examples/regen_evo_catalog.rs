//! Regenerate `data/evo-catalog.toml` from the frozen Volumio
//! sources under `data/import/`. Developer-runnable binary; the
//! runtime never invokes this.
//!
//! Usage:
//!
//! ```text
//! cargo run --example regen_evo_catalog \
//!     --package org-evoframework-hardware-audio-config \
//!     > plugins/org.evoframework.hardware.audio-config/data/evo-catalog.toml
//! ```
//!
//! Re-run after changing either Volumio source file OR the
//! importer logic in `src/import.rs`; commit the regenerated
//! `data/evo-catalog.toml`. The `importer_output_matches_checked_in_golden_byte_equal`
//! test in `src/import.rs` gates regression on the next CI run.

#![forbid(unsafe_code)]

use org_evoframework_hardware_audio_config::import::import_volumio_to_evo_catalog;

/// Volumio import source — frozen provenance, included into the
/// example at build time. Not exposed on the plugin's public
/// surface: the runtime never sees the Volumio shape (ADR-0132
/// §Invariant). Re-running this example with the same inputs
/// produces byte-identical output.
const VOLUMIO_DACS_JSON: &str =
    include_str!("../data/import/volumio-dacs.json");
const VOLUMIO_DAC_DSP_JSON: &str =
    include_str!("../data/import/volumio-dac-dsp.json");

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let catalog =
        import_volumio_to_evo_catalog(VOLUMIO_DACS_JSON, VOLUMIO_DAC_DSP_JSON)?;
    let toml_out = toml::to_string_pretty(&catalog)?;
    print!("{toml_out}");
    Ok(())
}
