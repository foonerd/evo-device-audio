//! Byte-flow worker substrate.
//!
//! The composition plugin's reactor (in the parent module)
//! publishes endpoint snapshots onto a watch channel. This
//! module's worker consumes those snapshots and runs the
//! actual byte-flow loop: open the OS-native primitives the
//! framework has selected for the chain stage, and pump
//! audio frames from input to output.
//!
//! The substrate selection is per-endpoint-kind:
//!
//! - `EndpointKind::NamedPipe` — opens both endpoints as
//!   filesystem FIFOs via [`tokio::fs::OpenOptions`] and
//!   pumps bytes between them. Hermetic, testable on any
//!   Unix box; the production path for chains the framework
//!   wires through named-pipe substrate.
//! - `EndpointKind::AlsaPcm` — opens both endpoints as ALSA
//!   capture/playback PCMs via libasound (the `alsa` crate),
//!   configures hwparams from the negotiated `AudioFormat`,
//!   pumps frames input → output with cancel-aware
//!   non-blocking PCM waits. Compiled only when the
//!   `alsa-substrate` Cargo feature is enabled — typically
//!   for cross-builds targeting reference target / Debian Trixie /
//!   ALSA-having hardware. The dev-rig native build runs
//!   without the feature and reports AlsaPcm as
//!   `UnsupportedKind`.
//! - `EndpointKind::SharedMemory` / `JackPort` — reported as
//!   `UnsupportedKind`. Shared-memory and JACK-port
//!   substrates are vendor-distribution territory; the
//!   reference plugin in this build does not implement
//!   them.
//!
//! When the input and output endpoints declare different
//! substrate kinds, the worker reports `MixedSubstrate` —
//! the passthrough mode this build supports requires
//! homogeneous substrate. (Future modes that perform
//! transformation may bridge substrates; that's a per-mode
//! choice, not a passthrough invariant.)

use std::sync::Arc;

use evo_plugin_sdk::contract::audio_routing::{
    CompositionEndpoints, EndpointKind,
};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;

/// Errors raised by the byte-flow substrate. Surfaced via
/// the worker status channel so observability surfaces
/// (chunk-D test harness, future operator UI) can render
/// the failure mode without losing structure.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ByteFlowError {
    /// The endpoints declared a substrate kind this build
    /// does not implement. Carries the kind for
    /// diagnostics. Surfaced as a structured operator-
    /// readable state, not a panic — the framework should
    /// not have selected an unsupported substrate, so a
    /// failure here indicates topology / vendor-build
    /// drift the operator needs.
    #[error("substrate kind not implemented in this build: {0:?}")]
    UnsupportedKind(EndpointKind),

    /// Input and output endpoints declared different
    /// substrate kinds; passthrough mode requires
    /// homogeneous substrate. Future composition modes
    /// (resampler, DSD-to-PCM converter) may bridge
    /// substrates; passthrough cannot.
    #[error(
        "input/output substrate kinds differ for passthrough \
         mode: input={input:?} output={output:?}"
    )]
    MixedSubstrate {
        /// Input endpoint substrate kind.
        input: EndpointKind,
        /// Output endpoint substrate kind.
        output: EndpointKind,
    },

    /// Failure opening the substrate primitive at the
    /// path/identifier the framework configured. The
    /// inner string carries the `std::io::Error`'s message
    /// alongside which endpoint failed.
    #[error("substrate open failed: {0}")]
    OpenFailed(String),

    /// Read from the input substrate failed mid-flow. The
    /// worker terminates the current substrate and waits
    /// for the next route change to retry.
    #[error("substrate read failed: {0}")]
    ReadFailed(String),

    /// Write to the output substrate failed mid-flow.
    /// Same recovery semantics as
    /// [`Self::ReadFailed`].
    #[error("substrate write failed: {0}")]
    WriteFailed(String),
}

/// Frame buffer size for the byte-flow loop. Tuned to keep
/// the syscall rate reasonable across sub-millisecond
/// audio formats (192 kHz stereo s24le ≈ 1.15 MB/s) without
/// making latency a concern (4 KiB ≈ 5 ms at the same
/// rate). Production builds may tune this against the
/// negotiated format's `buffer_frames` hint; the constant
/// here is a sensible default.
const PUMP_BUFFER_BYTES: usize = 4096;

/// Run the byte-flow substrate appropriate to the
/// endpoint pair. Returns `Ok(())` on graceful exit
/// (cancel signalled, or input EOF), `Err` on any
/// substrate-side failure. The caller (the worker)
/// reports the outcome to the status channel and waits
/// for the next snapshot or shutdown.
pub async fn run_substrate(
    endpoints: &CompositionEndpoints,
    cancel: Arc<Notify>,
) -> Result<(), ByteFlowError> {
    if endpoints.input.kind != endpoints.output.kind {
        return Err(ByteFlowError::MixedSubstrate {
            input: endpoints.input.kind,
            output: endpoints.output.kind,
        });
    }
    match endpoints.input.kind {
        EndpointKind::NamedPipe => {
            run_named_pipe(endpoints.clone(), cancel).await
        }
        #[cfg(feature = "alsa-substrate")]
        EndpointKind::AlsaPcm => {
            crate::byte_flow_alsa::run_alsa_pcm(endpoints.clone(), cancel).await
        }
        #[cfg(not(feature = "alsa-substrate"))]
        EndpointKind::AlsaPcm => {
            Err(ByteFlowError::UnsupportedKind(EndpointKind::AlsaPcm))
        }
        kind @ (EndpointKind::SharedMemory | EndpointKind::JackPort) => {
            Err(ByteFlowError::UnsupportedKind(kind))
        }
    }
}

/// Named-pipe substrate: open both endpoints as FIFOs at
/// their configured paths and pump bytes from input to
/// output until cancel fires or input reaches EOF.
async fn run_named_pipe(
    endpoints: CompositionEndpoints,
    cancel: Arc<Notify>,
) -> Result<(), ByteFlowError> {
    let mut input = tokio::fs::OpenOptions::new()
        .read(true)
        .open(&endpoints.input.path)
        .await
        .map_err(|e| {
            ByteFlowError::OpenFailed(format!(
                "input fifo {}: {e}",
                endpoints.input.path.display()
            ))
        })?;
    let mut output = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&endpoints.output.path)
        .await
        .map_err(|e| {
            ByteFlowError::OpenFailed(format!(
                "output fifo {}: {e}",
                endpoints.output.path.display()
            ))
        })?;

    let mut buf = vec![0u8; PUMP_BUFFER_BYTES];
    loop {
        tokio::select! {
            biased;
            _ = cancel.notified() => return Ok(()),
            result = input.read(&mut buf) => {
                match result {
                    Ok(0) => return Ok(()),
                    Ok(n) => {
                        output
                            .write_all(&buf[..n])
                            .await
                            .map_err(|e| {
                                ByteFlowError::WriteFailed(e.to_string())
                            })?;
                    }
                    Err(e) => {
                        return Err(ByteFlowError::ReadFailed(e.to_string()));
                    }
                }
            }
        }
    }
}
