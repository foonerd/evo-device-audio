// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Operator-options → asound.conf drop-in renderer.
//!
//! The reference distribution's `/etc/asound.conf` template
//! defines a baseline `pcm.evo` chain: `plug → hw:CARD=DAC,DEV=0`.
//! Operator-tunable settings (mixer type, output device, sample
//! rate target) live in the `audio.options.settings` subject the
//! `org.evoframework.playback.options` plugin publishes. This
//! module computes the canonical asound.conf body that overrides
//! the baseline when one or more settings deviate from the
//! defaults the bootstrap-installed asound.conf encodes.
//!
//! ## Drop-in mechanism
//!
//! The framework's bootstrap script installs the baseline
//! asound.conf with a trailing `<</etc/asound.d/evo-options.conf>>`
//! include directive (ALSA's standard config-file inclusion).
//! ALSA reads drop-ins LAST, so any name defined here overrides
//! the base. This module renders the drop-in body; the plugin's
//! observer task atomically writes it on every operator settings
//! change. ALSA re-reads `/etc/asound.conf` (and every included
//! drop-in) on each PCM open, so the running playback chain
//! picks up the new pipeline on the next play / pause-resume
//! cycle. No daemon-level recycle is required.
//!
//! ## Why a renderer (not direct edits to /etc/asound.conf)
//!
//! Two writers for one file is the "parallel truth" anti-pattern
//! the engineering bar refuses. The baseline asound.conf is
//! distribution-owned (bootstrap.sh writes it once at install
//! time); the drop-in is plugin-owned (the delivery.alsa plugin
//! writes it at runtime on every operator change). Each surface
//! has exactly one writer. The drop-in includes the canonical
//! `pcm.evo` definition with the operator-selected settings; the
//! baseline retains the no-options-set default so an absent
//! drop-in leaves audio working at the bootstrap-installed
//! pipeline.
//!
//! ## Render coverage
//!
//! The current release covers three operator settings:
//!
//! - `mixer_type` — `none` / `software` / `hardware`. Hardware
//!   binds a specific ALSA mixer control via the `softvol`
//!   plugin or direct `hw` control attachment; software inserts
//!   ALSA's `softvol` between `plug` and `hw`; none keeps the
//!   chain unmixed (volume is fixed at the source).
//! - `output_device` — the ALSA card descriptor the chain
//!   terminates at. Empty / unset defaults to `hw:CARD=DAC,DEV=0`
//!   (the bootstrap baseline). Concrete operator values are
//!   typed strings of the form `hw:CARD=<NAME>,DEV=<N>` or
//!   `hw:<INDEX>,<N>` — passed through verbatim into the chain.
//! - `resampling` — when present and `enabled = true`, inserts
//!   a `plug` rate-conversion step at the operator-supplied
//!   target rate; when absent or `enabled = false`, no
//!   conversion node is inserted (ALSA's default `plug`
//!   negotiates per-stream).
//!
//! DoP (DSD-over-PCM) lives at the source / playback warden
//! layer (mpd.conf `dop "yes"` / equivalent); the delivery
//! chain does not transform DoP-packed PCM, so the renderer
//! ignores the `dop` field. Volume-normalisation lives at the
//! playback warden (per-track / album LUFS); the renderer
//! ignores `volume_normalization` for the same reason.
//!
//! ## Testing strategy
//!
//! Pure-function renderer (no I/O). Per-variation snapshot tests
//! pin the exact output; an integration test in `lib.rs` wires
//! the observer body to call this renderer + an atomic-write
//! helper against a tempdir-backed path and asserts the file
//! contents match the renderer's output byte-for-byte.

use std::fmt::Write as _;
use std::io;
use std::path::Path;

/// Operator-tunable settings extracted from the
/// `audio.options.settings` subject. The shape mirrors the
/// `org.evoframework.playback.options` plugin's persisted
/// Settings struct on the wire (per the subject schema declared
/// in `evo-catalogue-schemas/org.evoframework/audio/options.v1.toml`).
///
/// All fields are `Option`-typed so an incomplete subject state
/// (operator has only set some fields) renders the partial
/// pipeline correctly: unset fields fall through to the
/// bootstrap-installed baseline behaviour.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OptionsSettings {
    /// Mixer type the chain renders with. `None` means "operator
    /// has not chosen yet"; the renderer falls through to the
    /// bootstrap default (no mixer insertion).
    pub mixer_type: Option<MixerType>,
    /// ALSA mixer control name the hardware mixer attaches to.
    /// Used only when `mixer_type = Some(Hardware)`. Empty /
    /// unset with `mixer_type = Some(Hardware)` triggers the
    /// degrade-to-software path (per the audit's operator-
    /// visible safety net).
    pub mixer_control: Option<String>,
    /// Output device ALSA descriptor the chain terminates at.
    /// `None` means "use bootstrap default"; concrete values
    /// are passed through verbatim.
    pub output_device: Option<String>,
    /// Resampling settings. `None` means "no rate conversion
    /// node inserted"; `Some` inserts a `plug` rate-conversion
    /// step at the supplied target rate when `enabled = true`.
    pub resampling: Option<Resampling>,
}

/// Mixer-type discriminator. Mirrors the
/// `playback.options.set_mixer_type` request's argument
/// vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixerType {
    /// No mixer; chain is `plug → hw:...`. Volume control is
    /// the source's responsibility (e.g. mpd's mixer).
    None,
    /// Software mixer; chain is `plug → softvol → hw:...`.
    /// ALSA's `softvol` plugin attenuates digitally; bit-perfect
    /// playback requires the operator to set the softvol level
    /// to 100% (or pick MixerType::None).
    Software,
    /// Hardware mixer; chain is `plug → hw:...` and operator
    /// volume gestures bind to a specific ALSA mixer control on
    /// the same card. Pure bit-perfect path when the mixer
    /// control passes through (no digital attenuation).
    Hardware,
}

/// Resampling configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resampling {
    /// `true` to insert an explicit `plug` rate-conversion node
    /// targeting `target_rate_hz`; `false` to omit the
    /// conversion entirely.
    pub enabled: bool,
    /// Target sample rate the rate-conversion node converts to.
    /// Common values: 44100, 48000, 88200, 96000, 176400,
    /// 192000. The renderer does not validate against host
    /// hardware support — operator-visible probe surface in
    /// `delivery.probe_hardware` is the validation path.
    pub target_rate_hz: u32,
}

/// Card descriptor the renderer falls through to when
/// `OptionsSettings.output_device` is unset. Matches the
/// reference bootstrap default.
pub const DEFAULT_OUTPUT_DEVICE: &str = "hw:CARD=DAC,DEV=0";

/// Render the asound.conf drop-in body that overrides the
/// baseline `pcm.evo` chain with operator-selected settings.
///
/// Returns a complete, valid asound.conf fragment terminating
/// in a single trailing newline. Callers atomic-write this to
/// the drop-in path; ALSA picks it up on the next PCM open.
///
/// Determinism guarantees: same input renders byte-identical
/// output across calls. The function has no internal state and
/// no environment dependencies.
pub fn render_drop_in(settings: &OptionsSettings) -> String {
    let mut out = String::new();
    out.push_str(
        "# Auto-generated by org.evoframework.delivery.alsa\n\
         # at runtime from the audio.options.settings subject\n\
         # operator state. DO NOT EDIT BY HAND — the file is\n\
         # rewritten on every operator gesture against the\n\
         # playback.options plugin's settings surface.\n\n",
    );

    let output_device = settings
        .output_device
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_OUTPUT_DEVICE);

    // Resampling node (if requested): `plug` with the operator-
    // selected target rate. The node is named `pcm.evo_rate` and
    // chains into the mixer / hardware terminus.
    let resampling_node = match &settings.resampling {
        Some(r) if r.enabled => Some(r.target_rate_hz),
        _ => None,
    };

    // Mixer node naming + insertion. The renderer emits one of
    // three shapes:
    //
    //   - None    → pcm.evo = plug { slave = hw_terminus }
    //   - Software → pcm.evo = plug { slave = softvol → hw_terminus }
    //   - Hardware → pcm.evo = plug { slave = hw_terminus };
    //               operator-visible volume control attaches at
    //               the card's hardware mixer via the
    //               `ctl.evo` chain (already defined in the
    //               baseline asound.conf).
    let mixer_kind =
        match (settings.mixer_type, settings.mixer_control.as_ref()) {
            (Some(MixerType::Hardware), Some(ctrl)) if !ctrl.is_empty() => {
                EffectiveMixer::Hardware(ctrl.as_str())
            }
            (Some(MixerType::Hardware), _) => {
                // Hardware requested but no control supplied —
                // degrade-to-software safety net. The renderer
                // surfaces this as Software here; the plugin's
                // `parse_mixer_config_from_settings_state` path
                // emits the operator-visible WARN on the same
                // condition.
                EffectiveMixer::Software
            }
            (Some(MixerType::Software), _) => EffectiveMixer::Software,
            (Some(MixerType::None) | None, _) => EffectiveMixer::None,
        };

    // Hardware terminus — the named card the chain delivers to.
    writeln!(out, "pcm.evo_terminus {{").unwrap();
    writeln!(out, "    type hw").unwrap();
    writeln!(out, "    card \"{}\"", parse_card_name(output_device)).unwrap();
    writeln!(out, "    device {}", parse_card_device(output_device)).unwrap();
    writeln!(out, "}}").unwrap();
    out.push('\n');

    // Optional resampling node inserted between the front-end
    // `pcm.evo` and either the mixer or the terminus.
    if let Some(rate) = resampling_node {
        writeln!(out, "pcm.evo_rate {{").unwrap();
        writeln!(out, "    type plug").unwrap();
        writeln!(out, "    slave {{").unwrap();
        writeln!(out, "        pcm \"evo_terminus\"").unwrap();
        writeln!(out, "        rate {rate}").unwrap();
        writeln!(out, "    }}").unwrap();
        writeln!(out, "}}").unwrap();
        out.push('\n');
    }

    // Mixer node when software-mixing.
    if matches!(mixer_kind, EffectiveMixer::Software) {
        writeln!(out, "pcm.evo_mixer {{").unwrap();
        writeln!(out, "    type softvol").unwrap();
        writeln!(
            out,
            "    slave.pcm \"{}\"",
            if resampling_node.is_some() {
                "evo_rate"
            } else {
                "evo_terminus"
            }
        )
        .unwrap();
        writeln!(out, "    control.name \"Evo Master\"").unwrap();
        writeln!(
            out,
            "    control.card \"{}\"",
            parse_card_name(output_device)
        )
        .unwrap();
        writeln!(out, "}}").unwrap();
        out.push('\n');
    }

    // Front-end `pcm.evo` chain — what MPD / source plugins
    // write into. `plug` for automatic format conversion.
    let slave_name = match (mixer_kind, resampling_node.is_some()) {
        (EffectiveMixer::Software, _) => "evo_mixer",
        (_, true) => "evo_rate",
        _ => "evo_terminus",
    };
    writeln!(out, "pcm.evo {{").unwrap();
    writeln!(out, "    type plug").unwrap();
    writeln!(out, "    slave.pcm \"{slave_name}\"").unwrap();
    writeln!(out, "    hint {{").unwrap();
    writeln!(out, "        show on").unwrap();
    writeln!(
        out,
        "        description \"evo modular pipeline (options-rendered)\""
    )
    .unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out, "}}").unwrap();
    out.push('\n');

    // `ctl.evo` — operator-visible control surface. For hardware
    // mixers, binds to the named control on the card; for
    // software / none, binds to the card itself (volume is the
    // softvol node's control, or none).
    writeln!(out, "ctl.evo {{").unwrap();
    writeln!(out, "    type hw").unwrap();
    writeln!(out, "    card \"{}\"", parse_card_name(output_device)).unwrap();
    if let EffectiveMixer::Hardware(ctrl) = mixer_kind {
        // ALSA's `ctl` type does not take a control name —
        // operator gestures pick the named control at runtime
        // via amixer. Render the chosen control name as a
        // comment-attached hint so operators / diagnostic
        // tools can read it without parsing the binding the
        // playback warden (mpd) holds.
        writeln!(out, "    # hardware-mixer control: \"{ctrl}\"").unwrap();
    }
    writeln!(out, "}}").unwrap();

    out
}

/// Extract an [`OptionsSettings`] from an
/// `audio.options.settings` subject-state payload. The payload
/// is the typed Settings struct the playback.options plugin
/// publishes, serialised through serde_json — the shape mirrors
/// `evo-catalogue-schemas/org.evoframework/audio/options.v1.toml`.
///
/// Missing / unparseable fields render as `None` rather than
/// returning a parse error: the renderer falls through to the
/// bootstrap baseline for each unset field, and operator-visible
/// configuration is an additive surface rather than a
/// validate-or-refuse contract. Plugins emitting malformed state
/// are surfaced through the framework's separate state-validation
/// path (the publisher contract), not through this consumer.
pub fn extract_options_settings_from_state(
    state: &serde_json::Value,
) -> OptionsSettings {
    let mixer_type = state
        .get("mixer_type")
        .and_then(|v| v.as_str())
        .map(str::to_ascii_lowercase)
        .and_then(|s| match s.as_str() {
            "none" => Some(MixerType::None),
            "software" => Some(MixerType::Software),
            "hardware" => Some(MixerType::Hardware),
            _ => None,
        });

    let mixer_control = state
        .get("mixer_control")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let output_device = state
        .get("output_device")
        .and_then(|v| {
            // Accept both the simple-string form (legacy /
            // future) and the typed structured form the
            // playback.options Settings carries via the
            // OutputDevice variant. The structured form
            // serialises as an object with a `pcm` field
            // naming the ALSA descriptor.
            v.as_str().map(str::to_string).or_else(|| {
                v.get("pcm").and_then(|p| p.as_str()).map(str::to_string)
            })
        })
        .filter(|s| !s.is_empty());

    // Resampling shape mirrors the `set_resampling` request
    // arg: `{ "enabled": bool, "target_rate_hz": u32 }`. The
    // subject state may carry an absent field (operator never
    // touched it) or an object with the two fields.
    let resampling = state.get("resampling").and_then(|r| {
        let enabled = r.get("enabled").and_then(|v| v.as_bool())?;
        let target_rate_hz =
            r.get("target_rate_hz").and_then(|v| v.as_u64())? as u32;
        Some(Resampling {
            enabled,
            target_rate_hz,
        })
    });

    OptionsSettings {
        mixer_type,
        mixer_control,
        output_device,
        resampling,
    }
}

/// Atomically write `body` to `path` via the tempfile-rename
/// pattern. The temp file is created in the same parent
/// directory as `path` so `rename(2)` is a same-filesystem
/// atomic operation. On success the operator-visible drop-in
/// transitions from old contents to new contents in one
/// kernel-level step; partial writes cannot leak through.
///
/// Fails with `io::Error` when the parent directory is missing,
/// not writable by the steward's service user, or the rename
/// fails. The caller logs the failure at warn level (recoverable;
/// the previous drop-in remains on disk).
pub async fn atomic_write_drop_in(path: &Path, body: &str) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::other(format!(
            "drop-in path {} has no parent directory",
            path.display()
        ))
    })?;
    // The temp filename carries the canonical drop-in basename
    // + a `.tmp` suffix so any interrupted write that survives
    // the rename window is discoverable + cleanable through
    // ordinary `ls`.
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::other(format!(
            "drop-in path {} has no file name",
            path.display()
        ))
    })?;
    let mut tmp_name = file_name.to_os_string();
    tmp_name.push(".tmp");
    let tmp_path = parent.join(&tmp_name);

    // Write the temp file. tokio::fs::write replaces if the
    // file exists; this gives us idempotent retry on interrupted
    // prior writes.
    tokio::fs::write(&tmp_path, body.as_bytes()).await?;
    // Rename is atomic on POSIX same-filesystem.
    tokio::fs::rename(&tmp_path, path).await?;
    Ok(())
}

/// Internal enum the renderer consults after collapsing the
/// hardware-without-control degrade case to Software.
#[derive(Debug, Clone, Copy)]
enum EffectiveMixer<'a> {
    None,
    Software,
    Hardware(&'a str),
}

/// Extract the card name from an `hw:CARD=NAME,DEV=N` or
/// `hw:N,M` descriptor. Returns the verbatim card token; the
/// renderer quotes it as a string in the asound.conf output.
fn parse_card_name(descriptor: &str) -> &str {
    if let Some(rest) = descriptor.strip_prefix("hw:CARD=") {
        // hw:CARD=DAC,DEV=0 → DAC
        rest.split(',').next().unwrap_or(rest)
    } else if let Some(rest) = descriptor.strip_prefix("hw:") {
        // hw:2,0 → 2
        rest.split(',').next().unwrap_or(rest)
    } else {
        descriptor
    }
}

/// Extract the device index from a descriptor; defaults to `0`
/// when not present (matches alsa convention).
fn parse_card_device(descriptor: &str) -> u32 {
    let after_card = if let Some(rest) = descriptor.strip_prefix("hw:CARD=") {
        rest
    } else if let Some(rest) = descriptor.strip_prefix("hw:") {
        rest
    } else {
        return 0;
    };
    after_card
        .split(',')
        .nth(1)
        .and_then(|s| {
            s.strip_prefix("DEV=")
                .unwrap_or(s)
                .trim()
                .parse::<u32>()
                .ok()
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_render_baseline_chain() {
        let s = OptionsSettings::default();
        let out = render_drop_in(&s);
        assert!(out.contains("pcm.evo {"));
        assert!(out.contains("type plug"));
        assert!(out.contains("slave.pcm \"evo_terminus\""));
        assert!(out.contains("card \"DAC\""));
        assert!(out.contains("device 0"));
        // No mixer, no resampling.
        assert!(!out.contains("type softvol"));
        assert!(!out.contains("evo_rate"));
        assert!(!out.contains("evo_mixer"));
    }

    #[test]
    fn software_mixer_inserts_softvol_node() {
        let s = OptionsSettings {
            mixer_type: Some(MixerType::Software),
            ..Default::default()
        };
        let out = render_drop_in(&s);
        assert!(out.contains("pcm.evo_mixer {"));
        assert!(out.contains("type softvol"));
        assert!(out.contains("slave.pcm \"evo_terminus\""));
        assert!(out.contains("control.name \"Evo Master\""));
        // Front-end chains through the mixer.
        let evo_block = out
            .split("pcm.evo {")
            .nth(1)
            .expect("pcm.evo block present");
        assert!(evo_block.contains("slave.pcm \"evo_mixer\""));
    }

    #[test]
    fn hardware_mixer_with_control_skips_softvol() {
        let s = OptionsSettings {
            mixer_type: Some(MixerType::Hardware),
            mixer_control: Some("Digital".to_string()),
            ..Default::default()
        };
        let out = render_drop_in(&s);
        assert!(!out.contains("type softvol"));
        assert!(!out.contains("evo_mixer"));
        // Hardware-mixer control surfaces as a comment hint on ctl.evo.
        assert!(out.contains("# hardware-mixer control: \"Digital\""));
        let evo_block = out
            .split("pcm.evo {")
            .nth(1)
            .expect("pcm.evo block present");
        assert!(evo_block.contains("slave.pcm \"evo_terminus\""));
    }

    #[test]
    fn hardware_without_control_degrades_to_software() {
        let s = OptionsSettings {
            mixer_type: Some(MixerType::Hardware),
            mixer_control: None,
            ..Default::default()
        };
        let out = render_drop_in(&s);
        // Degrade safety net: hardware without control falls back
        // to software-mixer rendering.
        assert!(out.contains("type softvol"));
        assert!(out.contains("pcm.evo_mixer"));
    }

    #[test]
    fn resampling_inserts_rate_node_between_front_end_and_terminus() {
        let s = OptionsSettings {
            resampling: Some(Resampling {
                enabled: true,
                target_rate_hz: 96000,
            }),
            ..Default::default()
        };
        let out = render_drop_in(&s);
        assert!(out.contains("pcm.evo_rate {"));
        assert!(out.contains("rate 96000"));
        let rate_block = out
            .split("pcm.evo_rate {")
            .nth(1)
            .expect("pcm.evo_rate block present");
        assert!(rate_block.contains("pcm \"evo_terminus\""));
        let evo_block = out
            .split("pcm.evo {")
            .nth(1)
            .expect("pcm.evo block present");
        assert!(evo_block.contains("slave.pcm \"evo_rate\""));
    }

    #[test]
    fn resampling_with_software_mixer_chains_rate_then_mixer() {
        let s = OptionsSettings {
            mixer_type: Some(MixerType::Software),
            resampling: Some(Resampling {
                enabled: true,
                target_rate_hz: 192000,
            }),
            ..Default::default()
        };
        let out = render_drop_in(&s);
        assert!(out.contains("rate 192000"));
        assert!(out.contains("pcm.evo_mixer"));
        // Mixer chains through the rate node.
        let mixer_block = out
            .split("pcm.evo_mixer {")
            .nth(1)
            .expect("pcm.evo_mixer block present");
        assert!(mixer_block.contains("slave.pcm \"evo_rate\""));
        let evo_block = out
            .split("pcm.evo {")
            .nth(1)
            .expect("pcm.evo block present");
        assert!(evo_block.contains("slave.pcm \"evo_mixer\""));
    }

    #[test]
    fn explicit_output_device_overrides_default() {
        let s = OptionsSettings {
            output_device: Some("hw:CARD=USBDAC,DEV=0".to_string()),
            ..Default::default()
        };
        let out = render_drop_in(&s);
        assert!(out.contains("card \"USBDAC\""));
        assert!(!out.contains("card \"DAC\""));
    }

    #[test]
    fn output_device_with_explicit_dev_index_renders_correctly() {
        let s = OptionsSettings {
            output_device: Some("hw:CARD=MultiDevice,DEV=2".to_string()),
            ..Default::default()
        };
        let out = render_drop_in(&s);
        assert!(out.contains("card \"MultiDevice\""));
        assert!(out.contains("device 2"));
    }

    #[test]
    fn empty_output_device_falls_through_to_default() {
        let s = OptionsSettings {
            output_device: Some(String::new()),
            ..Default::default()
        };
        let out = render_drop_in(&s);
        assert!(out.contains("card \"DAC\""));
    }

    #[test]
    fn render_is_deterministic() {
        let s = OptionsSettings {
            mixer_type: Some(MixerType::Software),
            resampling: Some(Resampling {
                enabled: true,
                target_rate_hz: 96000,
            }),
            output_device: Some("hw:CARD=DAC,DEV=0".to_string()),
            ..Default::default()
        };
        let first = render_drop_in(&s);
        let second = render_drop_in(&s);
        assert_eq!(
            first, second,
            "renderer must produce byte-identical output for identical input"
        );
    }

    #[test]
    fn render_ends_with_single_trailing_newline() {
        let s = OptionsSettings::default();
        let out = render_drop_in(&s);
        assert!(out.ends_with('\n'));
        assert!(!out.ends_with("\n\n"));
    }

    #[test]
    fn render_carries_do_not_edit_header_comment() {
        let s = OptionsSettings::default();
        let out = render_drop_in(&s);
        assert!(out.contains("# Auto-generated"));
        assert!(out.contains("DO NOT EDIT BY HAND"));
        assert!(out.contains("org.evoframework.delivery.alsa"));
    }

    // ---- extractor coverage ----

    #[test]
    fn extractor_returns_default_for_empty_state() {
        let s = extract_options_settings_from_state(&serde_json::json!({}));
        assert_eq!(s, OptionsSettings::default());
    }

    #[test]
    fn extractor_parses_software_mixer_type() {
        let s = extract_options_settings_from_state(
            &serde_json::json!({"mixer_type": "software"}),
        );
        assert_eq!(s.mixer_type, Some(MixerType::Software));
    }

    #[test]
    fn extractor_parses_hardware_mixer_with_control() {
        let s = extract_options_settings_from_state(
            &serde_json::json!({"mixer_type": "hardware", "mixer_control": "Digital"}),
        );
        assert_eq!(s.mixer_type, Some(MixerType::Hardware));
        assert_eq!(s.mixer_control.as_deref(), Some("Digital"));
    }

    #[test]
    fn extractor_accepts_simple_string_output_device() {
        let s = extract_options_settings_from_state(&serde_json::json!({
            "output_device": "hw:CARD=USBDAC,DEV=0"
        }));
        assert_eq!(s.output_device.as_deref(), Some("hw:CARD=USBDAC,DEV=0"));
    }

    #[test]
    fn extractor_accepts_structured_output_device_via_pcm_field() {
        let s = extract_options_settings_from_state(&serde_json::json!({
            "output_device": {"pcm": "hw:CARD=DAC,DEV=0"}
        }));
        assert_eq!(s.output_device.as_deref(), Some("hw:CARD=DAC,DEV=0"));
    }

    #[test]
    fn extractor_parses_resampling_node() {
        let s = extract_options_settings_from_state(&serde_json::json!({
            "resampling": {"enabled": true, "target_rate_hz": 96000}
        }));
        assert_eq!(
            s.resampling,
            Some(Resampling {
                enabled: true,
                target_rate_hz: 96000
            })
        );
    }

    #[test]
    fn extractor_ignores_unknown_mixer_type_string() {
        let s = extract_options_settings_from_state(&serde_json::json!({
            "mixer_type": "magic"
        }));
        assert_eq!(s.mixer_type, None);
    }

    #[test]
    fn extractor_full_round_trip_renders_expected_pipeline() {
        let s = extract_options_settings_from_state(&serde_json::json!({
            "mixer_type": "software",
            "output_device": "hw:CARD=DAC,DEV=0",
            "resampling": {"enabled": true, "target_rate_hz": 192000}
        }));
        let out = render_drop_in(&s);
        assert!(out.contains("type softvol"));
        assert!(out.contains("rate 192000"));
        assert!(out.contains("card \"DAC\""));
    }

    // ---- atomic-write coverage ----

    #[tokio::test]
    async fn atomic_write_creates_file_with_expected_contents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("evo-options.conf");
        let body = "pcm.evo_test { type plug }\n";
        atomic_write_drop_in(&path, body).await.expect("write");
        let read_back =
            tokio::fs::read_to_string(&path).await.expect("read back");
        assert_eq!(read_back, body);
    }

    #[tokio::test]
    async fn atomic_write_replaces_existing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("evo-options.conf");
        tokio::fs::write(&path, "old contents").await.expect("seed");
        atomic_write_drop_in(&path, "new contents")
            .await
            .expect("rewrite");
        let read_back =
            tokio::fs::read_to_string(&path).await.expect("read back");
        assert_eq!(read_back, "new contents");
    }

    #[tokio::test]
    async fn atomic_write_leaves_no_stale_tmp_after_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("evo-options.conf");
        atomic_write_drop_in(&path, "x").await.expect("write");
        let tmp = path.with_file_name("evo-options.conf.tmp");
        assert!(!tmp.exists(), "tmp file must not survive the rename");
    }

    #[tokio::test]
    async fn atomic_write_refuses_path_without_parent() {
        let err = atomic_write_drop_in(Path::new("/"), "body")
            .await
            .expect_err("root path has no parent");
        let _ = err; // exact ErrorKind varies by OS; surface-shape only.
    }
}
