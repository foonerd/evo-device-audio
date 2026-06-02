//! MPD `audio_output` fragment renderer + atomic writer.
//!
//! Translates a framework-negotiated
//! [`WriteEndpoint`](evo_plugin_sdk::contract::audio_routing::WriteEndpoint)
//! into the MPD configuration block MPD picks up on restart, and
//! writes it atomically to a configurable fragment path.
//!
//! The rendered block carries:
//!
//! - `device` — the substrate path the framework selected (e.g.
//!   `hw:2,0` for direct DAC, `hw:Loopback,1,0` for the ALSA
//!   loopback substrate composition.alsa drives).
//! - `format` — MPD's `<rate>:<bits>:<channels>` form, derived
//!   from [`AudioFormat`](evo_plugin_sdk::audio::AudioFormat).
//! - `mixer_type` — one of `"hardware"`, `"software"`, or `"none"`
//!   per the operator's selected [`MixerConfig`]. Hardware mode
//!   additionally emits the `mixer_device` + `mixer_control`
//!   lines that name the ALSA mixer the operator wants MPD to
//!   drive; software + none modes omit those lines (MPD 0.24+
//!   rejects them outside hardware mode).
//!
//! ## Audiophile-grade three-mode model
//!
//! Hardware mode: MPD drives the DAC's hardware mixer control
//! directly; the PCM stream stays bit-perfect; the analog volume
//! changes at the DAC. Requires the card to expose an ALSA mixer
//! control. This is the audiophile-correct mode when the
//! hardware supports it.
//!
//! Software mode: MPD applies a digital gain stage internally
//! before writing to ALSA. Compatible with every card. NOT bit-
//! perfect at non-100% gain because the gain stage rescales
//! samples. The framework's topology scorer surfaces this in
//! the topology projection so operators see when bit-perfect
//! is lost.
//!
//! None mode: MPD does not interpret volume calls. Downstream
//! device (preamp / AVR / line-out + analog volume on the DAC
//! face) handles gain. The PCM stream is bit-perfect; volume
//! control is outside MPD's surface.
//!
//! Only [`EndpointKind::AlsaPcm`] is rendered. Source-plugin
//! topologies whose `WriteEndpoint` is a non-ALSA substrate
//! (NamedPipe / SharedMemory / JackPort) are not in scope for
//! this build — the worker logs and remains in the previous
//! fragment state rather than render an MPD block that MPD
//! would reject.

use std::io;
use std::path::Path;

use evo_plugin_sdk::audio::{AudioFormat, PcmCodec};
use evo_plugin_sdk::contract::audio_routing::{EndpointKind, WriteEndpoint};

/// Three-mode mixer selection projected into the MPD
/// `audio_output` block. Mirrors `playback.options::MixerType`
/// at the rendering boundary; the renderer owns the per-mode
/// MPD syntax (hardware vs software vs none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MixerConfig {
    /// MPD drives the DAC's ALSA mixer control directly.
    /// Renders `mixer_type "hardware"` + `mixer_device` +
    /// `mixer_control` lines. PCM stream stays bit-perfect.
    Hardware {
        /// ALSA mixer device the operator selected. Matches
        /// MPD's `mixer_device` line; typically `"hw:<card>"`
        /// where `<card>` is the kernel-stable card name.
        mixer_device: String,
        /// ALSA mixer control name. Matches MPD's
        /// `mixer_control` line; typical values include
        /// `"Master"`, `"PCM"`, or DAC-specific control names
        /// visible via `amixer scontrols`.
        mixer_control: String,
    },
    /// MPD applies a digital gain stage before writing to
    /// ALSA. Renders `mixer_type "software"` only. Compatible
    /// with every card; not bit-perfect at non-100% gain.
    Software,
    /// MPD does not interpret volume calls. Renders
    /// `mixer_type "none"` only. PCM stream is bit-perfect;
    /// volume control is the downstream device's concern
    /// (preamp / AVR / DAC analog volume).
    None,
}

impl MixerConfig {
    /// Wire-string ("hardware" / "software" / "none") used by
    /// MPD's config parser. Idempotent with
    /// `playback.options::MixerType::as_wire_str` so the
    /// settings projection and the rendered fragment agree.
    fn mpd_mixer_type_str(&self) -> &'static str {
        match self {
            Self::Hardware { .. } => "hardware",
            Self::Software => "software",
            Self::None => "none",
        }
    }
}

/// Failure modes of [`render_audio_output_fragment`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FragmentError {
    /// Endpoint substrate kind cannot be expressed as an MPD
    /// `audio_output` block (NamedPipe / SharedMemory / JackPort
    /// have substrate-specific MPD wiring outside this build's
    /// scope).
    #[error("MPD audio_output fragment supports only AlsaPcm; got {0:?}")]
    UnsupportedKind(EndpointKind),
    /// DSD format on an ALSA endpoint. MPD's `dsd` format token
    /// is valid in principle; this build's `audio.playback`
    /// shape v1 does not declare DSD output, so refuse loudly
    /// rather than render a contract MPD might honour but the
    /// shape forbids.
    #[error(
        "MPD audio_output fragment does not render DSD output in this build"
    )]
    DsdNotSupported,
    /// Encoded-passthrough format on an ALSA endpoint. Same
    /// rationale as [`Self::DsdNotSupported`].
    #[error(
        "MPD audio_output fragment does not render encoded-passthrough output \
         in this build"
    )]
    EncodedPassthroughNotSupported,
    /// A numeric `mixer_device` (`hw:3`) was supplied but the
    /// referenced card index does not exist in `/proc/asound/cards`.
    /// Numeric indices are brittle (they reorder across reboots,
    /// USB-DAC hotplugs, kernel driver-load order); we resolve
    /// them at fragment-render time to the kernel-stable
    /// `hw:CARD=<name>` form and surface a clear error when the
    /// resolution fails rather than silently shipping a
    /// fragment that targets a different card than the operator
    /// intended.
    #[error(
        "mixer_device '{requested}' references ALSA card index {index} \
         which is not present in /proc/asound/cards (operator config \
         must use 'hw:CARD=<name>' for stability — numeric indices \
         reorder across reboots)"
    )]
    MixerDeviceCardIndexNotFound {
        /// The original mixer_device string the operator supplied.
        requested: String,
        /// The numeric card index parsed out of the request.
        index: u32,
    },
}

/// Parse a card NAME (the bracketed kernel-stable identifier)
/// out of `/proc/asound/cards` for the supplied numeric index.
/// Pure: no I/O, takes the file contents as input. Returns
/// `None` when the index is not found.
///
/// `/proc/asound/cards` lines look like:
///
/// ```text
///  3 [DAC            ]: I-Sabre_Q2M_DAC - I-Sabre Q2M DAC
///                       I-Sabre Q2M DAC
/// ```
///
/// We extract `DAC` (trimmed) for index `3`. Continuation
/// lines (the indented description) are skipped.
fn parse_card_name_from_proc(cards_text: &str, index: u32) -> Option<String> {
    for line in cards_text.lines() {
        let trimmed = line.trim_start();
        // Index lines start with a digit; continuation lines
        // start with non-digit (whitespace + description).
        if !trimmed.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            continue;
        }
        // Split off the leading index.
        let mut parts = trimmed.splitn(2, |c: char| c.is_whitespace());
        let idx_str = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("");
        let parsed_idx: u32 = match idx_str.parse() {
            Ok(n) => n,
            Err(_) => continue,
        };
        if parsed_idx != index {
            continue;
        }
        // The card name is inside brackets after the index:
        // `[DAC            ]: ...`.
        let after_bracket_open = rest.strip_prefix('[')?;
        let close_idx = after_bracket_open.find(']')?;
        let name = after_bracket_open[..close_idx].trim();
        return Some(name.to_string());
    }
    None
}

/// Normalise a `mixer_device` string from the operator-supplied
/// form (commonly numeric, e.g. `hw:3`) to the kernel-stable
/// named form (`hw:CARD=DAC`). Pass-through for inputs that
/// already use the named form or that don't match the
/// `hw:<digits>` shape (operator's custom values are honoured
/// verbatim).
///
/// `cards_text` is the contents of `/proc/asound/cards`; supply
/// it explicitly so the function stays pure + trivially
/// testable. The caller is the one site that performs the I/O
/// read at fragment-render time.
///
/// Numeric `hw:N` is THE landmine: ALSA assigns card indices in
/// probe order; USB-DAC hotplug, kernel driver-load reorder, or
/// a kernel update can shift the index without warning. A
/// fragment that hardcodes `hw:3` ends up binding MPD's mixer
/// to a different card than the operator intended — silently,
/// at boot, with no error surfaced. The named form survives
/// every reorder because the kernel-stable name is the
/// authoritative identifier.
pub fn normalize_mixer_device(
    mixer_device: &str,
    cards_text: &str,
) -> Result<String, FragmentError> {
    // Already-named: `hw:CARD=<name>` — pass through.
    if mixer_device.starts_with("hw:CARD=") {
        return Ok(mixer_device.to_string());
    }
    // Numeric form: `hw:N` or `hw:N,M` (latter rare for
    // mixer_device but possible). Match and resolve N.
    let Some(rest) = mixer_device.strip_prefix("hw:") else {
        // Not an `hw:` form at all — operator custom value;
        // pass through.
        return Ok(mixer_device.to_string());
    };
    // Split on `,` to handle the optional device subscript.
    // Resolve N (the card index portion) and re-attach
    // anything after the comma.
    let (idx_str, suffix) = match rest.find(',') {
        Some(pos) => (&rest[..pos], &rest[pos..]),
        None => (rest, ""),
    };
    let Ok(index) = idx_str.parse::<u32>() else {
        // Not a numeric index — pass through (e.g. operator
        // wrote `hw:Loopback` directly).
        return Ok(mixer_device.to_string());
    };
    let name =
        parse_card_name_from_proc(cards_text, index).ok_or_else(|| {
            FragmentError::MixerDeviceCardIndexNotFound {
                requested: mixer_device.to_string(),
                index,
            }
        })?;
    Ok(format!("hw:CARD={name}{suffix}"))
}

/// Path the renderer reads to resolve numeric card indices.
/// Constant so tests can identify it; production callers always
/// read this exact file. Exposed at module scope rather than
/// embedded as a literal so the dependency is grep-discoverable.
pub const PROC_ASOUND_CARDS_PATH: &str = "/proc/asound/cards";

/// Canonical ALSA alias the audio-terminus plugin captures
/// from. Matches the `pcm.evo_terminus_tap` definition in
/// `dist/alsa/asound.conf` (snd-aloop subdev 7 playback side).
/// The terminus plugin opens the paired capture side at
/// `hw:Loopback,1,7` to read the same frames MPD writes here.
const TERMINUS_OUTPUT_DEVICE: &str = "evo_terminus_tap";

/// Wire format of the terminus loopback contract, imported
/// from the shared crate that is the single source of truth
/// for the rate / bit-depth / channel-count contract between
/// MPD's terminus audio_output and the audio.terminus plugin's
/// capture loop. snd-aloop's playback + capture halves MUST
/// agree on the format — a mismatch causes the capture-side
/// open to fail and the visualiser to go silent.
use evo_device_audio_shared::terminus_loopback::TERMINUS_LOOPBACK_MPD_FORMAT;

/// Render the MPD fragment carrying the audio_output blocks
/// the playback chain needs.
///
/// Emits TWO blocks:
///
/// 1. **listening output** — targets the supplied
///    [`WriteEndpoint`] with the supplied [`MixerConfig`]. This
///    is the operator-audible path; the mixer_type honours
///    whatever the operator chose (hardware / software / none).
///
/// 2. **terminus output** — fixed device
///    `pcm.evo_terminus_tap`, fixed format
///    `[TERMINUS_LOOPBACK_MPD_FORMAT]`, mixer_type `none`. The terminus
///    output is always full-scale source audio — never
///    attenuated by MPD's mixer — so the audio-terminus
///    plugin's tap captures pre-fader signal regardless of the
///    listening output's mixer mode.
///
/// Splitting the tap at MPD's output stage (rather than via an
/// ALSA multi-slave tee downstream of MPD) keeps the tap
/// pre-fader on every rig class: hardware-mixer DAC,
/// software-mixer USB DAC, Bluetooth output, multi-room
/// receiver. The listening output stays operator-configured;
/// the terminus output is wire-shape-contracted with the
/// terminus plugin.
///
/// Failure semantics: MPD's audio_output blocks are
/// independent. A terminus-output open failure (snd-aloop
/// kernel module unavailable, Loopback subdev 7 in use) does
/// not disrupt the listening output — local audio keeps
/// reaching the DAC regardless of terminus health (floor
/// invariant preserved by construction).
pub fn render_audio_output_fragment(
    ep: &WriteEndpoint,
    mixer: &MixerConfig,
) -> Result<String, FragmentError> {
    if ep.kind != EndpointKind::AlsaPcm {
        return Err(FragmentError::UnsupportedKind(ep.kind));
    }
    let format_str = render_format_string(&ep.format)?;
    let device = ep.path.to_string_lossy();
    let mixer_block = render_mixer_block(mixer);
    // Local Unix-domain control socket. The
    // `emit_test_tone` course-correct verb opens a dedicated
    // MPD connection here to dispatch `add file://...` +
    // `play` — MPD's security model refuses local-file loads
    // over TCP but allows them on the Unix socket. The
    // supervisor's main connection stays on its configured
    // endpoint; this socket carries the test-tone path only.
    // MPD treats `bind_to_address` directives additively, so
    // this layers the Unix socket alongside the main
    // /etc/mpd.conf's TCP / localhost binds.
    Ok(format!(
        "bind_to_address \"/run/mpd/socket\"\n\n\
         audio_output {{\n    \
         type            \"alsa\"\n    \
         name            \"evo-device-audio\"\n    \
         device          \"{device}\"\n    \
         format          \"{format_str}\"\n\
         {mixer_block}\
         }}\n\
         \n\
         audio_output {{\n    \
         type            \"alsa\"\n    \
         name            \"evo-audio-terminus-tap\"\n    \
         device          \"{TERMINUS_OUTPUT_DEVICE}\"\n    \
         format          \"{TERMINUS_LOOPBACK_MPD_FORMAT}\"\n    \
         mixer_type      \"none\"\n\
         }}\n"
    ))
}

/// Render the mixer-related portion of an audio_output block.
/// Hardware mode emits three lines (mixer_type plus mixer_device
/// plus mixer_control); software and none modes emit one line
/// (mixer_type only). MPD 0.24+ rejects the mixer_device and
/// mixer_control lines outside hardware mode so the omission
/// is required, not aesthetic.
fn render_mixer_block(mixer: &MixerConfig) -> String {
    let mixer_type_str = mixer.mpd_mixer_type_str();
    match mixer {
        MixerConfig::Hardware {
            mixer_device,
            mixer_control,
        } => format!(
            "    mixer_type      \"{mixer_type_str}\"\n    \
             mixer_device    \"{mixer_device}\"\n    \
             mixer_control   \"{mixer_control}\"\n"
        ),
        MixerConfig::Software | MixerConfig::None => {
            format!("    mixer_type      \"{mixer_type_str}\"\n")
        }
    }
}

/// Render an [`AudioFormat`] into MPD's `<rate>:<bits>:<channels>`
/// audio-output format string.
///
/// MPD's `format` line accepts `<rate>:<bits>:<channels>` where
/// `bits` is the integer bit-depth (`16`, `24`, `32`) for fixed
/// PCM or the literal `f` for IEEE 754 floating-point PCM. See
/// MPD upstream's `mpd.conf` documentation.
fn render_format_string(fmt: &AudioFormat) -> Result<String, FragmentError> {
    match fmt {
        AudioFormat::Pcm {
            codec,
            rate_hz,
            channels,
        } => {
            let bits = match codec {
                PcmCodec::PcmS16Le => "16",
                PcmCodec::PcmS24Le => "24",
                PcmCodec::PcmS32Le => "32",
                PcmCodec::PcmF32 => "f",
            };
            Ok(format!("{rate_hz}:{bits}:{channels}"))
        }
        AudioFormat::Dsd { .. } => Err(FragmentError::DsdNotSupported),
        AudioFormat::EncodedPassthrough { .. } => {
            Err(FragmentError::EncodedPassthroughNotSupported)
        }
    }
}

/// Write `content` to `path` atomically: stage in a sibling
/// `.tmp` file in the same directory, fsync, then rename onto
/// the target. Readers (i.e. MPD on restart) see either the
/// previous contents or the new contents — never a torn write.
///
/// Returns the underlying [`io::Error`] on any step. Failure
/// leaves the target file at its previous contents and may
/// leave the staging file behind for operator inspection.
pub async fn atomic_write_fragment(
    path: &Path,
    content: &str,
) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("fragment path {path:?} has no parent directory"),
        )
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("fragment path {path:?} has no file name"),
        )
    })?;
    let staging = parent.join(format!(".{}.tmp", file_name.to_string_lossy()));

    tokio::fs::write(&staging, content).await?;

    // Open the staging file again to fsync. Drop the handle
    // before rename so no descriptor holds the file open
    // across the rename (kernels tolerate it, but releasing
    // matches the conventional atomic-write recipe and lets
    // file-system tracing tools attribute the rename
    // cleanly).
    {
        let f = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&staging)
            .await?;
        f.sync_all().await?;
    }

    tokio::fs::rename(&staging, path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    fn pcm_endpoint(
        path: &str,
        codec: PcmCodec,
        rate_hz: u32,
        channels: u8,
    ) -> WriteEndpoint {
        WriteEndpoint {
            kind: EndpointKind::AlsaPcm,
            path: PathBuf::from(path),
            format: AudioFormat::Pcm {
                codec,
                rate_hz,
                channels,
            },
            buffer_frames: 1024,
        }
    }

    #[test]
    fn render_pcm_s16_44100_stereo() {
        let ep = pcm_endpoint("hw:2,0", PcmCodec::PcmS16Le, 44_100, 2);
        let out =
            render_audio_output_fragment(&ep, &MixerConfig::Software).unwrap();
        assert!(out.contains("type            \"alsa\""));
        assert!(out.contains("device          \"hw:2,0\""));
        assert!(out.contains("format          \"44100:16:2\""));
        assert!(out.contains("mixer_type      \"software\""));
        // Ends with a single trailing newline after the closing
        // brace so concatenation with neighbouring fragments
        // is well-formed.
        assert!(out.ends_with("}\n"));
    }

    /// The fragment emits a `bind_to_address` directive for the
    /// local Unix-domain socket so the `emit_test_tone` verb
    /// can dispatch `add file://...` + `play` against a
    /// connection MPD trusts for local-file loads. Every
    /// admitted topology lands this binding (it lives in the
    /// per-topology fragment the warden writes).
    #[test]
    fn render_includes_unix_socket_bind_to_address() {
        let ep = pcm_endpoint("hw:0,0", PcmCodec::PcmS16Le, 44_100, 2);
        let out =
            render_audio_output_fragment(&ep, &MixerConfig::Software).unwrap();
        assert!(
            out.contains("bind_to_address \"/run/mpd/socket\""),
            "fragment must declare the Unix-socket bind so the \
             test-tone verb has a local connection to dispatch \
             file:// loads on; got:\n{out}"
        );
    }

    #[test]
    fn render_pcm_s24_192000_stereo() {
        let ep = pcm_endpoint("hw:2,0", PcmCodec::PcmS24Le, 192_000, 2);
        let out =
            render_audio_output_fragment(&ep, &MixerConfig::Software).unwrap();
        assert!(out.contains("format          \"192000:24:2\""));
    }

    #[test]
    fn render_pcm_s32_96000_stereo() {
        let ep = pcm_endpoint("hw:2,0", PcmCodec::PcmS32Le, 96_000, 2);
        let out =
            render_audio_output_fragment(&ep, &MixerConfig::Software).unwrap();
        assert!(out.contains("format          \"96000:32:2\""));
    }

    #[test]
    fn render_pcm_f32_uses_f_marker() {
        let ep = pcm_endpoint("hw:2,0", PcmCodec::PcmF32, 48_000, 2);
        let out =
            render_audio_output_fragment(&ep, &MixerConfig::Software).unwrap();
        assert!(out.contains("format          \"48000:f:2\""));
    }

    #[test]
    fn render_pcm_s16_mono() {
        let ep = pcm_endpoint("evo", PcmCodec::PcmS16Le, 44_100, 1);
        let out =
            render_audio_output_fragment(&ep, &MixerConfig::Software).unwrap();
        assert!(out.contains("format          \"44100:16:1\""));
    }

    #[test]
    fn render_pcm_s24_5_1_surround() {
        // 5.1 = 6 channels. The renderer passes the channel
        // count through verbatim; MPD's format-line parser
        // accepts any 1..=255 channel count.
        let ep = pcm_endpoint("evo", PcmCodec::PcmS24Le, 96_000, 6);
        let out =
            render_audio_output_fragment(&ep, &MixerConfig::Software).unwrap();
        assert!(out.contains("format          \"96000:24:6\""));
    }

    #[test]
    fn render_pcm_s32_high_rate_352800() {
        // DSD64-equivalent PCM sample rate. Some DACs accept
        // PCM at this rate; the renderer must pass it through
        // verbatim.
        let ep = pcm_endpoint("evo", PcmCodec::PcmS32Le, 352_800, 2);
        let out =
            render_audio_output_fragment(&ep, &MixerConfig::Software).unwrap();
        assert!(out.contains("format          \"352800:32:2\""));
    }

    #[test]
    fn render_pcm_s32_ultra_high_rate_384000() {
        // Studio / DXD rate. Common audiophile high end.
        let ep = pcm_endpoint("evo", PcmCodec::PcmS32Le, 384_000, 2);
        let out =
            render_audio_output_fragment(&ep, &MixerConfig::Software).unwrap();
        assert!(out.contains("format          \"384000:32:2\""));
    }

    #[test]
    fn render_pcm_f32_at_non_44_1_rate() {
        // PcmF32 maps to MPD's `f` marker; rate is independent
        // of the bit-depth marker.
        let ep = pcm_endpoint("evo", PcmCodec::PcmF32, 192_000, 2);
        let out =
            render_audio_output_fragment(&ep, &MixerConfig::Software).unwrap();
        assert!(out.contains("format          \"192000:f:2\""));
    }

    #[test]
    fn render_alsa_loopback_path() {
        let ep = pcm_endpoint("hw:Loopback,1,0", PcmCodec::PcmS24Le, 48_000, 2);
        let out =
            render_audio_output_fragment(&ep, &MixerConfig::Software).unwrap();
        assert!(out.contains("device          \"hw:Loopback,1,0\""));
    }

    #[test]
    fn render_refuses_named_pipe_kind() {
        let ep = WriteEndpoint {
            kind: EndpointKind::NamedPipe,
            path: PathBuf::from("/tmp/evo.fifo"),
            format: AudioFormat::Pcm {
                codec: PcmCodec::PcmS16Le,
                rate_hz: 44_100,
                channels: 2,
            },
            buffer_frames: 1024,
        };
        let err = render_audio_output_fragment(&ep, &MixerConfig::Software)
            .unwrap_err();
        match err {
            FragmentError::UnsupportedKind(kind) => {
                assert_eq!(kind, EndpointKind::NamedPipe);
            }
            other => panic!("expected UnsupportedKind, got {other:?}"),
        }
    }

    #[test]
    fn render_refuses_shared_memory_kind() {
        let ep = WriteEndpoint {
            kind: EndpointKind::SharedMemory,
            path: PathBuf::from("/dev/shm/evo"),
            format: AudioFormat::Pcm {
                codec: PcmCodec::PcmS16Le,
                rate_hz: 44_100,
                channels: 2,
            },
            buffer_frames: 1024,
        };
        let err = render_audio_output_fragment(&ep, &MixerConfig::Software)
            .unwrap_err();
        assert!(matches!(err, FragmentError::UnsupportedKind(_)));
    }

    #[test]
    fn render_refuses_jack_port_kind() {
        let ep = WriteEndpoint {
            kind: EndpointKind::JackPort,
            path: PathBuf::from("system:playback_1"),
            format: AudioFormat::Pcm {
                codec: PcmCodec::PcmS16Le,
                rate_hz: 44_100,
                channels: 2,
            },
            buffer_frames: 1024,
        };
        let err = render_audio_output_fragment(&ep, &MixerConfig::Software)
            .unwrap_err();
        assert!(matches!(err, FragmentError::UnsupportedKind(_)));
    }

    #[test]
    fn render_refuses_dsd_format() {
        use evo_plugin_sdk::audio::{DsdRate, DsdTransport};
        let ep = WriteEndpoint {
            kind: EndpointKind::AlsaPcm,
            path: PathBuf::from("hw:2,0"),
            format: AudioFormat::Dsd {
                rate: DsdRate::Dsd64,
                transport: DsdTransport::Dop,
                channels: 2,
            },
            buffer_frames: 1024,
        };
        let err = render_audio_output_fragment(&ep, &MixerConfig::Software)
            .unwrap_err();
        assert!(matches!(err, FragmentError::DsdNotSupported));
    }

    #[test]
    fn render_refuses_encoded_passthrough() {
        let ep = WriteEndpoint {
            kind: EndpointKind::AlsaPcm,
            path: PathBuf::from("hw:2,0"),
            format: AudioFormat::EncodedPassthrough {
                codec: "dts".to_string(),
                rate_hz: 48_000,
                channels: 6,
                bitrate_kbps: None,
            },
            buffer_frames: 1024,
        };
        let err = render_audio_output_fragment(&ep, &MixerConfig::Software)
            .unwrap_err();
        assert!(matches!(err, FragmentError::EncodedPassthroughNotSupported));
    }

    #[tokio::test]
    async fn atomic_write_creates_file_with_content() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("mpd.conf");
        let body = "audio_output { type \"alsa\" }\n";
        atomic_write_fragment(&target, body).await.unwrap();
        let read = tokio::fs::read_to_string(&target).await.unwrap();
        assert_eq!(read, body);
    }

    #[tokio::test]
    async fn atomic_write_replaces_existing_content() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("mpd.conf");
        tokio::fs::write(&target, "old content\n").await.unwrap();
        atomic_write_fragment(&target, "new content\n")
            .await
            .unwrap();
        let read = tokio::fs::read_to_string(&target).await.unwrap();
        assert_eq!(read, "new content\n");
    }

    #[tokio::test]
    async fn atomic_write_leaves_no_staging_file_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("mpd.conf");
        atomic_write_fragment(&target, "x\n").await.unwrap();
        let staging = dir.path().join(".mpd.conf.tmp");
        assert!(
            !staging.exists(),
            "atomic_write_fragment must remove its staging file on success"
        );
    }

    #[tokio::test]
    async fn atomic_write_rejects_path_with_no_parent() {
        // "/" has no parent.
        let err = atomic_write_fragment(Path::new("/"), "x")
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    // ------- Audio-terminus tap output ------------------------
    //
    // The fragment emits TWO audio_output blocks: a listening
    // output (operator-configurable mixer) + a terminus output
    // (mixer_type "none", always full-scale source audio). The
    // terminus output makes the spectrum tap pre-fader on every
    // rig class without an ALSA-level workaround.

    fn count_audio_output_blocks(fragment: &str) -> usize {
        fragment.matches("audio_output {").count()
    }

    #[test]
    fn render_emits_two_audio_output_blocks() {
        let ep = pcm_endpoint("hw:2,0", PcmCodec::PcmS16Le, 44_100, 2);
        let out =
            render_audio_output_fragment(&ep, &MixerConfig::Software).unwrap();
        assert_eq!(
            count_audio_output_blocks(&out),
            2,
            "fragment must emit listening output + terminus output; got:\n{out}"
        );
    }

    #[test]
    fn terminus_block_carries_fixed_device_format_and_none_mixer() {
        let ep = pcm_endpoint("hw:2,0", PcmCodec::PcmS24Le, 96_000, 2);
        let out =
            render_audio_output_fragment(&ep, &MixerConfig::Software).unwrap();
        assert!(
            out.contains("name            \"evo-audio-terminus-tap\""),
            "terminus block missing canonical name; got:\n{out}"
        );
        assert!(
            out.contains("device          \"evo_terminus_tap\""),
            "terminus block missing canonical device; got:\n{out}"
        );
        assert!(
            out.contains("format          \"48000:32:2\""),
            "terminus block must carry the wire-shape contract \
             format (48000:32:2); got:\n{out}"
        );
        assert!(
            out.contains("mixer_type      \"none\""),
            "terminus block must use mixer_type \"none\" so the \
             tap is pre-fader regardless of operator volume; got:\n{out}"
        );
    }

    #[test]
    fn terminus_block_present_under_hardware_mixer() {
        // Listening output uses Hardware mixer (DAC drives gain
        // downstream); terminus output MUST still be present
        // with mixer_type "none" so the wire contract is
        // independent of the listening mixer mode.
        let ep = pcm_endpoint("hw:0,0", PcmCodec::PcmS16Le, 44_100, 2);
        let mixer = MixerConfig::Hardware {
            mixer_device: "hw:0".to_string(),
            mixer_control: "Digital".to_string(),
        };
        let out = render_audio_output_fragment(&ep, &mixer).unwrap();
        assert_eq!(count_audio_output_blocks(&out), 2);
        assert!(out.contains("mixer_type      \"hardware\""));
        assert!(out.contains("mixer_type      \"none\""));
        assert!(out.contains("device          \"evo_terminus_tap\""));
    }

    #[test]
    fn terminus_block_present_under_none_mixer() {
        // Listening output uses None mixer (downstream preamp /
        // AVR owns gain); terminus output MUST still be present
        // with mixer_type "none" — the terminus's "none" is
        // mechanically identical but the contract is
        // independent of what the listening output declared.
        let ep = pcm_endpoint("hw:0,0", PcmCodec::PcmS16Le, 44_100, 2);
        let out =
            render_audio_output_fragment(&ep, &MixerConfig::None).unwrap();
        assert_eq!(count_audio_output_blocks(&out), 2);
        // Both blocks carry mixer_type "none"; the count must
        // be 2 (one per block).
        assert_eq!(
            out.matches("mixer_type      \"none\"").count(),
            2,
            "both listening + terminus carry mixer_type none; got:\n{out}"
        );
        assert!(out.contains("device          \"evo_terminus_tap\""));
    }

    #[test]
    fn terminus_block_present_under_software_mixer() {
        // The defect-of-record: SW-mixer rigs attenuate samples
        // before writing to ALSA. The terminus block with
        // mixer_type "none" is the fix — MPD writes full-scale
        // source to the terminus output regardless of the
        // listening output's software gain.
        let ep = pcm_endpoint("hw:0,0", PcmCodec::PcmS16Le, 44_100, 2);
        let out =
            render_audio_output_fragment(&ep, &MixerConfig::Software).unwrap();
        assert_eq!(count_audio_output_blocks(&out), 2);
        assert!(out.contains("mixer_type      \"software\""));
        assert!(out.contains("mixer_type      \"none\""));
        assert!(out.contains("device          \"evo_terminus_tap\""));
    }

    // ------- mixer_device card-index normalisation ------------
    //
    // Numeric `hw:N` indices are brittle (USB-DAC hotplug or
    // kernel driver-load reorder shifts them silently). The
    // renderer resolves them to the kernel-stable
    // `hw:CARD=<name>` form via /proc/asound/cards before
    // shipping to MPD.

    /// Sample /proc/asound/cards content modelled after a
    /// Raspberry Pi 5 with an I-Sabre Q2M USB DAC plugged in
    /// (cards 0/1: vc4-hdmi; card 2: snd-aloop; card 3: DAC).
    const CARDS_PI5_DAC: &str = "\
 0 [vc4hdmi0       ]: vc4-hdmi - vc4-hdmi-0
                      vc4-hdmi-0
 1 [vc4hdmi1       ]: vc4-hdmi - vc4-hdmi-1
                      vc4-hdmi-1
 2 [Loopback       ]: Loopback - Loopback
                      Loopback 1
 3 [DAC            ]: I-Sabre_Q2M_DAC - I-Sabre Q2M DAC
                      I-Sabre Q2M DAC
";

    /// Same hardware after a hypothetical reorder (DAC at index
    /// 0; vc4hdmi shifted up). The named form survives this;
    /// the numeric form would now address the wrong card.
    const CARDS_PI5_DAC_REORDERED: &str = "\
 0 [DAC            ]: I-Sabre_Q2M_DAC - I-Sabre Q2M DAC
                      I-Sabre Q2M DAC
 1 [vc4hdmi0       ]: vc4-hdmi - vc4-hdmi-0
                      vc4-hdmi-0
 2 [vc4hdmi1       ]: vc4-hdmi - vc4-hdmi-1
                      vc4-hdmi-1
 3 [Loopback       ]: Loopback - Loopback
                      Loopback 1
";

    #[test]
    fn parse_card_name_extracts_bracketed_name_at_index() {
        assert_eq!(
            parse_card_name_from_proc(CARDS_PI5_DAC, 0),
            Some("vc4hdmi0".to_string())
        );
        assert_eq!(
            parse_card_name_from_proc(CARDS_PI5_DAC, 3),
            Some("DAC".to_string())
        );
        assert_eq!(parse_card_name_from_proc(CARDS_PI5_DAC, 99), None);
    }

    #[test]
    fn parse_card_name_skips_continuation_lines() {
        // Continuation lines start with whitespace + description.
        // The parser must not mistake them for index lines.
        // CARDS_PI5_DAC has them; verifying we still hit the
        // right index.
        assert_eq!(
            parse_card_name_from_proc(CARDS_PI5_DAC, 2),
            Some("Loopback".to_string())
        );
    }

    #[test]
    fn normalize_passes_through_already_named_form() {
        assert_eq!(
            normalize_mixer_device("hw:CARD=DAC", CARDS_PI5_DAC).unwrap(),
            "hw:CARD=DAC"
        );
    }

    #[test]
    fn normalize_resolves_numeric_to_named_form() {
        // The defect-of-record on the Pi 5 rig: persisted
        // mixer_device = "hw:3" addresses index 3 today but
        // would address something else after a card reorder.
        // The normaliser converts to the stable form.
        assert_eq!(
            normalize_mixer_device("hw:3", CARDS_PI5_DAC).unwrap(),
            "hw:CARD=DAC"
        );
    }

    #[test]
    fn normalize_survives_card_reorder() {
        // The point of the fix: after a reorder, the same
        // normaliser call returns the DAC's CARD= form
        // regardless of what index it ended up at. (In a real
        // deployment the persisted value would have been
        // normalised at the last load before the reorder, so
        // the input to this test mirrors what the operator
        // would set after first realising the index changed —
        // hw:0 to address the DAC at its new position.)
        assert_eq!(
            normalize_mixer_device("hw:0", CARDS_PI5_DAC_REORDERED).unwrap(),
            "hw:CARD=DAC"
        );
    }

    #[test]
    fn normalize_preserves_device_subscript() {
        // mixer_device occasionally carries a device subscript
        // (`hw:3,0`). Normalisation rewrites only the card
        // portion; the `,0` suffix is preserved verbatim.
        assert_eq!(
            normalize_mixer_device("hw:3,0", CARDS_PI5_DAC).unwrap(),
            "hw:CARD=DAC,0"
        );
    }

    #[test]
    fn normalize_passes_through_non_hw_forms() {
        // Operator might have set a custom value like a JACK
        // port or a software-mixer placeholder. The normaliser
        // does not interfere.
        assert_eq!(
            normalize_mixer_device("default", CARDS_PI5_DAC).unwrap(),
            "default"
        );
        assert_eq!(
            normalize_mixer_device("plug:dmix", CARDS_PI5_DAC).unwrap(),
            "plug:dmix"
        );
    }

    #[test]
    fn normalize_passes_through_hw_with_non_numeric_card() {
        // `hw:Loopback` is already a card name (not numeric).
        // The current strip_prefix("hw:") path parses Loopback
        // as a non-numeric token and passes through.
        assert_eq!(
            normalize_mixer_device("hw:Loopback", CARDS_PI5_DAC).unwrap(),
            "hw:Loopback"
        );
    }

    #[test]
    fn normalize_errors_on_unknown_numeric_index() {
        let err = normalize_mixer_device("hw:99", CARDS_PI5_DAC).unwrap_err();
        match err {
            FragmentError::MixerDeviceCardIndexNotFound {
                requested,
                index,
            } => {
                assert_eq!(requested, "hw:99");
                assert_eq!(index, 99);
            }
            other => {
                panic!("expected MixerDeviceCardIndexNotFound, got {other:?}")
            }
        }
    }
}
