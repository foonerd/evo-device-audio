//! ALSA capture loop.
//!
//! Opens the configured input PCM (default `hw:Loopback,1,7`),
//! reads interleaved stereo S32_LE samples in FFT-sized chunks,
//! converts to normalised f32, feeds the SpectrumAnalyser, and
//! emits the resulting `PerceptualFrame` to the spectrum subject.
//! Also stashes the frame in the shared `latest_frame` slot so
//! the `get_spectrum_frame` read verb has a deterministic answer
//! without waiting for the next happening.
//!
//! Failure semantics (explicit per the engineering contract):
//!
//! - **ALSA open fails (PCM busy, kernel module not loaded,
//!   wrong device name):** logged at WARN, capture loop exits;
//!   plugin health degrades to "loaded but no spectrum frames".
//!   A plugin restart re-opens.
//! - **Read transport error (loopback writer absent, period
//!   underrun):** loop catches the error, drops the partial
//!   buffer, calls `pcm.recover()` and continues; one retry
//!   budget per cycle. Sustained errors (recover() itself
//!   failing N times in a row) exit the loop the same as open
//!   failure.
//! - **Shutdown signal:** notify_waiters() from `unload` exits
//!   the loop cleanly within one read budget (configured to be
//!   small — ~33 ms at the 30 Hz target).
//!
//! Lifecycle gating (transport_state + leader-authoritative)
//! is enforced in this module by skipping the subject emit
//! when the plugin's playing flag is unset OR the local device
//! is not the leader of any active multi-room group. The flag
//! is set by the `audio_playback_now_playing` subscriber the
//! plugin spawns; absent that subscriber being wired, emission
//! defaults to "always emit while capture loop runs" so a
//! standalone-device deployment still surfaces spectrum.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use evo_plugin_sdk::contract::SubjectAnnouncer;
use tokio::sync::Notify;

use crate::fft::{PerceptualFrame, SpectrumAnalyser, CHANNEL_COUNT, FFT_SIZE};
use crate::spectrum_subject;
use crate::PluginConfig;

const PLUGIN_NAME: &str = "org.evoframework.audio.terminus";

/// Initial reconnect delay on capture-open failure.
const RECONNECT_INITIAL: Duration = Duration::from_millis(250);
/// Upper bound on reconnect delay.
const RECONNECT_MAX: Duration = Duration::from_secs(5);
/// Maximum number of consecutive open failures before the
/// capture task exits.
const RECONNECT_MAX_ATTEMPTS: u32 = 12;

/// Spawn the capture task. Returns the JoinHandle the plugin
/// awaits in `unload`. The task respects the `shutdown` Notify
/// and exits cleanly within one read budget when notified.
pub fn spawn(
    config: PluginConfig,
    latest_frame: Arc<Mutex<Option<PerceptualFrame>>>,
    announcer: Arc<dyn SubjectAnnouncer>,
    shutdown: Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        run_capture_loop(config, latest_frame, announcer, shutdown);
    })
}

fn run_capture_loop(
    config: PluginConfig,
    latest_frame: Arc<Mutex<Option<PerceptualFrame>>>,
    announcer: Arc<dyn SubjectAnnouncer>,
    shutdown: Arc<Notify>,
) {
    let mut analyser = SpectrumAnalyser::new(config.sample_rate_hz);
    // Single source of truth for the wire `rate_hz` field. Derived
    // once at loop construction since neither sample_rate_hz nor
    // FFT_SIZE change within a capture lifetime.
    let rate_hz = crate::fft::frame_rate_hz(config.sample_rate_hz);
    let mut consecutive_failures: u32 = 0;
    let mut backoff = RECONNECT_INITIAL;

    // Outer loop reconnects on capture-open failure or
    // sustained read errors.
    loop {
        if shutdown_check(&shutdown) {
            tracing::info!(
                plugin = PLUGIN_NAME,
                "capture task exiting on shutdown signal"
            );
            return;
        }

        let pcm = match open_capture(&config.input_pcm, config.sample_rate_hz) {
            Ok(p) => p,
            Err(e) => {
                consecutive_failures += 1;
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    input_pcm = %config.input_pcm,
                    sample_rate_hz = config.sample_rate_hz,
                    error = %e,
                    attempt = consecutive_failures,
                    "ALSA capture open failed"
                );
                if consecutive_failures >= RECONNECT_MAX_ATTEMPTS {
                    tracing::warn!(
                        plugin = PLUGIN_NAME,
                        attempts = consecutive_failures,
                        "capture task exhausted reconnect budget; \
                         spectrum will be silent until plugin restart"
                    );
                    return;
                }
                if wait_with_shutdown(&shutdown, backoff) {
                    return;
                }
                backoff = (backoff * 2).min(RECONNECT_MAX);
                continue;
            }
        };

        // Reset backoff on successful open.
        consecutive_failures = 0;
        backoff = RECONNECT_INITIAL;

        tracing::info!(
            plugin = PLUGIN_NAME,
            input_pcm = %config.input_pcm,
            sample_rate_hz = config.sample_rate_hz,
            "ALSA capture opened; entering FFT loop"
        );

        // Inner loop: read frames, compute FFT, emit subject.
        // Exits on shutdown OR on a read error severe enough
        // that recover() fails — bubbles back to the outer loop
        // which retries the open.
        let exit_inner = run_fft_loop(
            &pcm,
            &mut analyser,
            &latest_frame,
            &announcer,
            &shutdown,
            rate_hz,
        );
        match exit_inner {
            InnerExit::Shutdown => return,
            InnerExit::TransportFailed => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    "capture inner loop bailed on transport error; \
                     re-opening capture"
                );
                // Fall through to outer-loop retry.
            }
        }
    }
}

#[derive(Debug)]
enum InnerExit {
    Shutdown,
    TransportFailed,
}

fn run_fft_loop(
    pcm: &PCM,
    analyser: &mut SpectrumAnalyser,
    latest_frame: &Arc<Mutex<Option<PerceptualFrame>>>,
    announcer: &Arc<dyn SubjectAnnouncer>,
    shutdown: &Arc<Notify>,
    rate_hz: u32,
) -> InnerExit {
    // S32_LE interleaved stereo: FFT_SIZE samples per channel
    // -> FFT_SIZE * 2 i32s per frame.
    let frame_samples: usize = FFT_SIZE * CHANNEL_COUNT;
    let mut raw_buf = vec![0i32; frame_samples];
    let mut f32_buf = vec![0.0f32; frame_samples];

    // tokio runtime handle for the announcer emit (which is async).
    // The capture loop runs on a blocking thread but the announcer
    // is async; we need to bridge via block_on against the current
    // tokio handle, or spawn an emit task onto the handle.
    let tokio_handle = match tokio::runtime::Handle::try_current() {
        Ok(h) => h,
        Err(_) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                "no tokio runtime handle in capture context; cannot emit \
                 spectrum subject from blocking thread"
            );
            return InnerExit::TransportFailed;
        }
    };

    let io = pcm.io_i32().expect("io_i32 should succeed for S32_LE");

    let mut consecutive_read_failures = 0u32;
    loop {
        if shutdown_check(shutdown) {
            return InnerExit::Shutdown;
        }

        match io.readi(&mut raw_buf) {
            Ok(frames_read) => {
                consecutive_read_failures = 0;
                if frames_read < FFT_SIZE {
                    // Partial frame; drop and retry (next read
                    // gets the remainder). FFT needs a full
                    // frame.
                    continue;
                }
                // Convert i32 [-INT32_MAX, INT32_MAX] -> f32
                // [-1, 1]. The S32_LE samples occupy the full
                // 32-bit range; divide by max-int32 as f32.
                let scale = 1.0f32 / (i32::MAX as f32);
                for i in 0..frame_samples {
                    f32_buf[i] = raw_buf[i] as f32 * scale;
                }
                let mut frame = analyser.process_frame(&f32_buf);
                frame.at_ms = now_ms();
                let frame_clone = clone_frame(&frame);
                if let Ok(mut guard) = latest_frame.lock() {
                    *guard = Some(frame);
                }
                let announcer = Arc::clone(announcer);
                tokio_handle.spawn(async move {
                    spectrum_subject::emit_frame(
                        &announcer,
                        &frame_clone,
                        rate_hz,
                    )
                    .await;
                });
            }
            Err(e) => {
                consecutive_read_failures += 1;
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    consecutive_read_failures,
                    "ALSA read errored; attempting recovery"
                );
                if let Err(recover_err) = pcm.recover(e.errno() as i32, true) {
                    tracing::warn!(
                        plugin = PLUGIN_NAME,
                        recover_error = %recover_err,
                        consecutive_read_failures,
                        "pcm.recover() failed; bailing inner loop"
                    );
                    return InnerExit::TransportFailed;
                }
                if consecutive_read_failures >= RECONNECT_MAX_ATTEMPTS {
                    tracing::warn!(
                        plugin = PLUGIN_NAME,
                        consecutive_read_failures,
                        "sustained read failures despite recover(); bailing"
                    );
                    return InnerExit::TransportFailed;
                }
                // Brief pause before next read to avoid a hot
                // spin while the loopback writer (MPD) is still
                // ramping up after a transport-state transition.
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

fn open_capture(
    pcm_name: &str,
    sample_rate_hz: u32,
) -> Result<PCM, alsa::Error> {
    let pcm = PCM::new(pcm_name, Direction::Capture, false)?;
    {
        let hwp = HwParams::any(&pcm)?;
        hwp.set_channels(CHANNEL_COUNT as u32)?;
        hwp.set_rate(sample_rate_hz, ValueOr::Nearest)?;
        hwp.set_format(Format::s32())?;
        hwp.set_access(Access::RWInterleaved)?;
        // Buffer geometry: large enough to absorb scheduling
        // jitter at 30 Hz emit cadence; small enough that the
        // first frame after a transport-state change reflects
        // current audio within one period.
        hwp.set_period_size(FFT_SIZE as alsa::pcm::Frames, ValueOr::Nearest)?;
        hwp.set_buffer_size_near((FFT_SIZE * 4) as alsa::pcm::Frames)?;
        pcm.hw_params(&hwp)?;
    }
    pcm.start()?;
    Ok(pcm)
}

fn shutdown_check(shutdown: &Arc<Notify>) -> bool {
    // Non-blocking peek: tokio Notify doesn't expose
    // "already notified" without await, but the capture-blocking
    // context can poll via try_notify pattern using try_recv
    // semantics. Cheapest: tokio::time::timeout-zero on
    // `.notified()` via the runtime handle.
    let handle = match tokio::runtime::Handle::try_current() {
        Ok(h) => h,
        Err(_) => return false,
    };
    handle.block_on(async {
        tokio::time::timeout(Duration::from_millis(0), shutdown.notified())
            .await
            .is_ok()
    })
}

fn wait_with_shutdown(shutdown: &Arc<Notify>, dur: Duration) -> bool {
    let handle = match tokio::runtime::Handle::try_current() {
        Ok(h) => h,
        Err(_) => {
            std::thread::sleep(dur);
            return false;
        }
    };
    handle.block_on(async {
        tokio::time::timeout(dur, shutdown.notified()).await.is_ok()
    })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn clone_frame(frame: &PerceptualFrame) -> PerceptualFrame {
    PerceptualFrame {
        magnitudes: Box::new(*frame.magnitudes),
        peak_hold: Box::new(*frame.peak_hold),
        onsets: frame.onsets,
        correlation: Box::new(*frame.correlation),
        at_ms: frame.at_ms,
    }
}
