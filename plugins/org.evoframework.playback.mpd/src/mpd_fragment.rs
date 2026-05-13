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
}

/// Render an MPD `audio_output` configuration block targeting
/// the supplied [`WriteEndpoint`] with the supplied
/// [`MixerConfig`].
///
/// The output is a complete MPD config fragment terminated by a
/// trailing newline; concatenation into a larger file is the
/// caller's concern.
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
    Ok(format!(
        "audio_output {{\n    \
         type            \"alsa\"\n    \
         name            \"evo-device-audio\"\n    \
         device          \"{device}\"\n    \
         format          \"{format_str}\"\n\
         {mixer_block}\
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
}
