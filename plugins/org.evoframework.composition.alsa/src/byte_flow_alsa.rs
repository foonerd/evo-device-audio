//! ALSA PCM substrate for the byte-flow worker.
//!
//! Compiled only when the `alsa-substrate` Cargo feature is
//! enabled — typically only for cross-builds targeting reference target
//! / Debian Trixie / ALSA-having hardware. On the dev environment
//! the feature stays off and this module is excluded from
//! the build entirely; libasound headers are not required
//! on the host.
//!
//! ## Substrate semantics
//!
//! The composition stage receives an
//! [`EndpointKind::AlsaPcm`](evo_plugin_sdk::contract::audio_routing::EndpointKind)
//! pair from the framework: the framework has wired both
//! input and output to ALSA pcm names (`hw:Loopback,0,0` /
//! `hw:Loopback,1,0` for the canonical loopback chain;
//! `hw:0,0` / `hw:1,0` for direct device pairs). This
//! substrate opens both pcms (input as Capture, output as
//! Playback), configures hardware parameters from the
//! negotiated [`AudioFormat`](evo_plugin_sdk::audio::AudioFormat),
//! starts both, and pumps frames input → output until the
//! cancel signal fires or an unrecoverable substrate error
//! surfaces.
//!
//! ## Async / blocking bridging
//!
//! The `alsa` crate exposes a synchronous, blocking API.
//! This module bridges to the worker's tokio-async lifecycle
//! by spawning the blocking substrate loop on
//! `tokio::task::spawn_blocking` and translating the cancel
//! [`Notify`] signal into an `AtomicBool` the blocking
//! thread polls between every PCM wait.
//!
//! ALSA pcms are configured non-blocking with a ~50 ms
//! timeout on `pcm.wait()`; on every iteration the loop
//! observes the cancel flag, then either pumps available
//! frames or loops back. This bounds shutdown latency to
//! ~50 ms regardless of audio data flow rate.
//!
//! ## Format negotiation
//!
//! The negotiated [`AudioFormat`] from the framework's
//! reconciliation engine arrives as a typed
//! `AudioFormat::Pcm { codec, rate_hz, channels }` triple.
//! This module translates each [`PcmCodec`] variant to the
//! matching ALSA `Format` constant and configures hwparams
//! against it; mismatches against device capability surface
//! as a structured `OpenFailed` error carrying the ALSA
//! error message. The framework MUST NOT have selected a
//! format the delivery target's hardware refused at probe
//! time, so a hwparams refusal indicates a topology /
//! reconciliation drift the operator needs surfaced.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use evo_plugin_sdk::audio::{AudioFormat, PcmCodec};
use evo_plugin_sdk::contract::audio_routing::CompositionEndpoints;
use tokio::sync::Notify;

use crate::byte_flow::ByteFlowError;

/// Async wrapper around the blocking ALSA pump loop. Spawns
/// the loop on `spawn_blocking`; mirrors the cancel signal
/// onto an `AtomicBool` the loop polls; awaits completion.
///
/// Returns `Ok(())` on graceful exit (cancel observed or
/// input EOF), `Err` on substrate failure.
pub(crate) async fn run_alsa_pcm(
    endpoints: CompositionEndpoints,
    cancel: Arc<Notify>,
) -> Result<(), ByteFlowError> {
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let cancel_flag_for_signal = Arc::clone(&cancel_flag);

    // Background task: when the upstream cancel Notify
    // fires, set the atomic flag the blocking thread polls.
    // The observer task is aborted when the blocking task
    // returns, regardless of whether cancel fired or not.
    let observer = tokio::spawn(async move {
        cancel.notified().await;
        cancel_flag_for_signal.store(true, Ordering::Release);
    });

    let blocking_result = tokio::task::spawn_blocking(move || {
        run_alsa_pcm_blocking(&endpoints, cancel_flag)
    })
    .await;

    observer.abort();

    match blocking_result {
        Ok(r) => r,
        Err(join_err) => Err(ByteFlowError::OpenFailed(format!(
            "alsa substrate task panicked: {join_err}"
        ))),
    }
}

/// Tunable: how many milliseconds the blocking pump waits on
/// the input pcm before checking the cancel flag. Bounds
/// shutdown latency. 50 ms is conservative — well below
/// human-perceptible audio dropout while keeping the cancel
/// poll rate reasonable.
const PCM_WAIT_TIMEOUT_MS: u32 = 50;

fn run_alsa_pcm_blocking(
    endpoints: &CompositionEndpoints,
    cancel_flag: Arc<AtomicBool>,
) -> Result<(), ByteFlowError> {
    let pcm_format = match &endpoints.input.format {
        AudioFormat::Pcm { codec, .. } => alsa_format_for_codec(*codec)?,
        AudioFormat::Dsd { .. } => {
            return Err(ByteFlowError::OpenFailed(
                "AlsaPcm substrate received an AudioFormat::Dsd; \
                 the framework's reconciliation engine should have \
                 selected a DSD-capable substrate, not AlsaPcm"
                    .to_string(),
            ));
        }
        AudioFormat::EncodedPassthrough { codec, .. } => {
            return Err(ByteFlowError::OpenFailed(format!(
                "AlsaPcm substrate received an EncodedPassthrough \
                 format with codec {codec:?}; encoded-bitstream \
                 chains should not route through the PCM substrate"
            )));
        }
    };

    let (rate_hz, channels) = match &endpoints.input.format {
        AudioFormat::Pcm {
            rate_hz, channels, ..
        } => (*rate_hz, *channels),
        _ => unreachable!("checked above"),
    };

    let input_path = pcm_path(&endpoints.input.path)?;
    let output_path = pcm_path(&endpoints.output.path)?;

    let input_pcm =
        PCM::new(input_path, Direction::Capture, true).map_err(|e| {
            ByteFlowError::OpenFailed(format!(
                "alsa capture open ({input_path}): {e}"
            ))
        })?;
    let output_pcm =
        PCM::new(output_path, Direction::Playback, true).map_err(|e| {
            ByteFlowError::OpenFailed(format!(
                "alsa playback open ({output_path}): {e}"
            ))
        })?;

    apply_hw_params(&input_pcm, "capture", pcm_format, rate_hz, channels)?;
    apply_hw_params(&output_pcm, "playback", pcm_format, rate_hz, channels)?;

    input_pcm.start().map_err(|e| {
        ByteFlowError::OpenFailed(format!("alsa input start: {e}"))
    })?;
    output_pcm.start().map_err(|e| {
        ByteFlowError::OpenFailed(format!("alsa output start: {e}"))
    })?;

    let frame_size_bytes = bytes_per_frame(pcm_format, channels)?;
    let frames_per_pump = 1024usize;
    let mut buf = vec![0u8; frames_per_pump * frame_size_bytes];

    let input_io = input_pcm.io_bytes();
    let output_io = output_pcm.io_bytes();

    loop {
        if cancel_flag.load(Ordering::Acquire) {
            return Ok(());
        }

        // wait() returns true when data is available, false
        // on timeout. Errors here are typically xrun / suspend
        // / unplug and surface as substrate failures.
        match input_pcm.wait(Some(PCM_WAIT_TIMEOUT_MS)) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => {
                return Err(ByteFlowError::ReadFailed(format!(
                    "alsa input wait: {e}"
                )));
            }
        }

        let frames_read = match input_io.readi(&mut buf) {
            Ok(n) => n,
            Err(e) if e.errno() == nix::errno::Errno::EAGAIN as i32 => {
                continue;
            }
            Err(e) => {
                return Err(ByteFlowError::ReadFailed(format!(
                    "alsa input readi: {e}"
                )));
            }
        };
        if frames_read == 0 {
            continue;
        }

        let bytes_to_write = frames_read * frame_size_bytes;
        let mut written_frames = 0usize;
        while written_frames < frames_read {
            let frames_remaining = frames_read - written_frames;
            let byte_offset = written_frames * frame_size_bytes;
            let byte_slice = &buf[byte_offset..bytes_to_write];

            match output_io.writei(byte_slice) {
                Ok(n) => {
                    written_frames += n;
                }
                Err(e) if e.errno() == nix::errno::Errno::EAGAIN as i32 => {
                    if cancel_flag.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    if let Err(wait_err) =
                        output_pcm.wait(Some(PCM_WAIT_TIMEOUT_MS))
                    {
                        return Err(ByteFlowError::WriteFailed(format!(
                            "alsa output wait: {wait_err}"
                        )));
                    }
                }
                Err(e) => {
                    return Err(ByteFlowError::WriteFailed(format!(
                        "alsa output writei (frames={frames_remaining}): {e}"
                    )));
                }
            }
        }
    }
}

fn pcm_path(path: &std::path::Path) -> Result<&str, ByteFlowError> {
    path.to_str().ok_or_else(|| {
        ByteFlowError::OpenFailed(format!(
            "alsa pcm path is not valid UTF-8: {path:?}"
        ))
    })
}

fn apply_hw_params(
    pcm: &PCM,
    role: &str,
    format: Format,
    rate_hz: u32,
    channels: u8,
) -> Result<(), ByteFlowError> {
    let hwp = HwParams::any(pcm).map_err(|e| {
        ByteFlowError::OpenFailed(format!("hwparams init ({role}): {e}"))
    })?;
    hwp.set_access(Access::RWInterleaved).map_err(|e| {
        ByteFlowError::OpenFailed(format!(
            "hwparams set_access RWInterleaved ({role}): {e}"
        ))
    })?;
    hwp.set_format(format).map_err(|e| {
        ByteFlowError::OpenFailed(format!(
            "hwparams set_format {format:?} ({role}): {e}"
        ))
    })?;
    hwp.set_channels(channels as u32).map_err(|e| {
        ByteFlowError::OpenFailed(format!(
            "hwparams set_channels {channels} ({role}): {e}"
        ))
    })?;
    hwp.set_rate(rate_hz, ValueOr::Nearest).map_err(|e| {
        ByteFlowError::OpenFailed(format!(
            "hwparams set_rate {rate_hz}Hz ({role}): {e}"
        ))
    })?;
    pcm.hw_params(&hwp).map_err(|e| {
        ByteFlowError::OpenFailed(format!("hw_params apply ({role}): {e}"))
    })?;
    Ok(())
}

fn alsa_format_for_codec(codec: PcmCodec) -> Result<Format, ByteFlowError> {
    Ok(match codec {
        PcmCodec::PcmS16Le => Format::s16(),
        PcmCodec::PcmS24Le => Format::s24(),
        PcmCodec::PcmS32Le => Format::s32(),
        PcmCodec::PcmF32 => Format::float(),
    })
}

fn bytes_per_frame(
    format: Format,
    channels: u8,
) -> Result<usize, ByteFlowError> {
    // Sample byte sizes for ALSA's interleaved formats. s24
    // is packed into a 32-bit container ("S24_LE" =
    // signed 24-bit aligned in 32-bit) so its frame size
    // matches s32 — matches alsa-lib's behaviour and
    // matches what the framework's reconciliation engine
    // negotiates when both source and delivery declare the
    // 24-bit codec.
    let sample_bytes = match format {
        f if f == Format::s16() => 2,
        f if f == Format::s24() => 4,
        f if f == Format::s32() => 4,
        f if f == Format::float() => 4,
        other => {
            return Err(ByteFlowError::OpenFailed(format!(
                "no bytes-per-sample mapping for ALSA format {other:?}"
            )));
        }
    };
    Ok(sample_bytes * channels as usize)
}
