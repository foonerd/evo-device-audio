// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
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
//! Lifecycle gating is enforced in this module by checking
//! two `watch::Receiver`s before each emit:
//!
//! - `TransportGate` (from the `now_playing_subscriber`):
//!   `Playing` opens this half of the gate; anything else
//!   closes it.
//! - `LocalRole` (from the `local_role_subscriber`): `Source`
//!   and `Auto` open this half; `Receiver` closes it
//!   (followers of an active multi-room group MUST NOT
//!   publish a parallel spectrum subject — the source-host's
//!   spectrum is the authoritative wavefront).
//!
//! Both halves MUST be open for `spectrum_subject::emit_frame`
//! to fire. The capture loop continues running regardless of
//! gate state (a resume picks up at the next tick without
//! spawn latency); only the subject emit is skipped when
//! either half is closed.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use evo_plugin_sdk::contract::SubjectAnnouncer;
use tokio::sync::{watch, Notify};

use crate::demand::SpectrumDemand;
use crate::fft::{PerceptualFrame, SpectrumAnalyser, FFT_SIZE, INPUT_CHANNELS};
use crate::local_role::LocalRole;
use crate::read_fail_class::{classify_read_failure, ReadFailClass};
use crate::spectrum_subject;
use crate::transport_gate::TransportGate;
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
// Signature carries five collaborators plus config + latest-frame
// slot + shutdown notifier. Bundling into a struct would move the
// plumbing one hop away without changing the number of moving parts
// the reader has to follow.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    config: PluginConfig,
    latest_frame: Arc<Mutex<Option<PerceptualFrame>>>,
    announcer: Arc<dyn SubjectAnnouncer>,
    shutdown: Arc<Notify>,
    transport_gate: watch::Receiver<TransportGate>,
    local_role: watch::Receiver<LocalRole>,
    demand: watch::Receiver<SpectrumDemand>,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        run_capture_loop(
            config,
            latest_frame,
            announcer,
            shutdown,
            transport_gate,
            local_role,
            demand,
        );
    })
}

#[allow(clippy::too_many_arguments)]
fn run_capture_loop(
    config: PluginConfig,
    latest_frame: Arc<Mutex<Option<PerceptualFrame>>>,
    announcer: Arc<dyn SubjectAnnouncer>,
    shutdown: Arc<Notify>,
    transport_gate: watch::Receiver<TransportGate>,
    local_role: watch::Receiver<LocalRole>,
    demand: watch::Receiver<SpectrumDemand>,
) {
    // Analyser is (re)built lazily on entry to the inner FFT
    // loop from the demand's current bins/channels. A demand
    // change mid-play breaks out of the inner loop and the
    // outer loop rebuilds the analyser with the new shape.
    let mut analyser: Option<SpectrumAnalyser> = None;
    let mut analyser_bins: u32 = 0;
    let mut analyser_channels: u32 = 0;
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

        // Outer-loop gate. TWO conditions must hold for the
        // capture task to open PCM + run the FFT loop:
        //
        // 1. TransportGate::should_emit (i.e. Playing) — MPD
        //    or another source is actively producing audio on
        //    the loopback tap.
        // 2. demand.enabled — the operator has enabled the
        //    visualiser through the settings surface. The
        //    demand subject is the framework's single
        //    production-truth field; evo-ui-runtime derives it
        //    from `ui.visualizer.enabled` on every settings
        //    patch (F1-A apply bridge).
        //
        // When either half closes, the capture task holds no
        // ALSA handle — no read, no FFT compute, no reconnect
        // cycle, no spin against an idle snd-aloop. The task
        // sleeps on both watch::Receivers until whichever half
        // is closed reopens.
        //
        // This is the operator-metric-that-matters — an
        // `enabled=false` toggle releases PCM identically to
        // a `NotPlaying` transport transition. Rig-verifiable
        // via `lsof -p <pid>`: the ALSA capture device
        // disappears from the FD list.
        //
        // The inner-emit gate (in `run_fft_loop`) remains as
        // the second line of defence for Receiver-role skip.
        let transport_open = transport_gate.borrow().should_emit();
        let demand_open = demand.borrow().enabled;
        if !(transport_open && demand_open) {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                transport_open,
                demand_open,
                "capture-gate closed; task idle until either half reopens"
            );
            if wait_for_capture_gate_or_shutdown(
                &mut transport_gate.clone(),
                &mut demand.clone(),
                &shutdown,
            ) {
                return;
            }
            // Gate just opened; reset reconnect backoff so
            // the first open attempt under fresh playback
            // is unthrottled.
            consecutive_failures = 0;
            backoff = RECONNECT_INITIAL;
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

        // (Re)build the analyser to match the current demand's
        // bins + channels. First open after a fresh plugin
        // admit constructs from the disabled-default (or the
        // last enabled-then-disabled shape). A demand change
        // that arrives while the inner loop is running exits
        // via `InnerExit::DemandShapeChanged` and the outer
        // loop rebuilds here with the new shape.
        let current_demand = *demand.borrow();
        if analyser.is_none()
            || analyser_bins != current_demand.bins
            || analyser_channels != current_demand.channels
        {
            analyser = Some(SpectrumAnalyser::new(
                config.sample_rate_hz,
                current_demand.bins as usize,
                current_demand.channels as usize,
            ));
            analyser_bins = current_demand.bins;
            analyser_channels = current_demand.channels;
            // Republish the empty-frame envelope at the new
            // shape so consumers see the shape change on the
            // wire immediately — the first live frame will
            // carry the same shape as this seed.
            let seed_addr = crate::spectrum_subject::render_empty_frame(
                rate_hz,
                current_demand.bins,
                current_demand.channels,
            );
            use evo_plugin_sdk::contract::ExternalAddressing;
            let addressing = ExternalAddressing::new(
                crate::spectrum_subject::SPECTRUM_SUBJECT_ADDRESSING_SCHEME,
                crate::spectrum_subject::SPECTRUM_SUBJECT_ADDRESSING_VALUE,
            );
            let announcer_clone = Arc::clone(&announcer);
            match tokio::runtime::Handle::try_current() {
                Ok(h) => {
                    h.spawn(async move {
                        let _ = announcer_clone
                            .update_state(addressing, seed_addr)
                            .await;
                    });
                }
                Err(_) => {
                    // No tokio runtime — the async announce
                    // would fail in `run_fft_loop` too. Skip
                    // the seed publish; the first live frame
                    // still carries the new shape.
                }
            }
        }

        // Inner loop: read frames, compute FFT, emit subject.
        // Exits on shutdown OR on a read error severe enough
        // that recover() fails — bubbles back to the outer loop
        // which retries the open. Also exits on demand.enabled
        // → false so the outer loop re-checks the gate + parks
        // (dropping the PCM handle), OR on a demand shape
        // change so the outer loop rebuilds the analyser.
        let exit_inner = run_fft_loop(
            &pcm,
            analyser.as_mut().expect("analyser constructed above"),
            &latest_frame,
            &announcer,
            &shutdown,
            rate_hz,
            &transport_gate,
            &local_role,
            &demand,
        );
        match exit_inner {
            InnerExit::Shutdown => return,
            InnerExit::TransportFailed => {
                // Transport-failure exits are the expected signal
                // that the steward (and therefore the ALSA capture
                // chain the terminus taps) has restarted. Outer
                // loop re-opens deterministically; a warn-class
                // emit on every steward-restart cycle is journal
                // noise, not a fault. Debug-class is the correct
                // level: observable when the operator enables
                // debug logging, silent in normal operation.
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    "capture inner loop bailed on transport error; \
                     re-opening capture"
                );
                // Fall through to outer-loop retry.
            }
            InnerExit::DemandShapeChanged => {
                // Operator changed `bins` or `channels` on the
                // demand subject mid-play. Rebuild the analyser
                // at the new shape (outer-loop head does this
                // when the current demand differs from
                // `analyser_bins` / `analyser_channels`) and
                // re-open PCM. Info-class because the operator
                // gesture is worth journal-visible; the
                // subsequent PCM re-open is normal + expected.
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    "capture inner loop bailed on demand shape change; \
                     rebuilding analyser + re-opening capture"
                );
            }
        }
    }
}

#[derive(Debug)]
enum InnerExit {
    Shutdown,
    TransportFailed,
    DemandShapeChanged,
}

#[allow(clippy::too_many_arguments)]
fn run_fft_loop(
    pcm: &PCM,
    analyser: &mut SpectrumAnalyser,
    latest_frame: &Arc<Mutex<Option<PerceptualFrame>>>,
    announcer: &Arc<dyn SubjectAnnouncer>,
    shutdown: &Arc<Notify>,
    rate_hz: u32,
    transport_gate: &watch::Receiver<TransportGate>,
    local_role: &watch::Receiver<LocalRole>,
    demand: &watch::Receiver<SpectrumDemand>,
) -> InnerExit {
    // S32_LE interleaved stereo: FFT_SIZE samples per channel
    // -> FFT_SIZE * INPUT_CHANNELS i32s per frame.
    let frame_samples: usize = FFT_SIZE * INPUT_CHANNELS;
    let mut raw_buf = vec![0i32; frame_samples];
    let mut f32_buf = vec![0.0f32; frame_samples];

    // Snapshot the analyser shape at inner-loop entry. A demand
    // change to bins or channels triggers `DemandShapeChanged`
    // exit and the outer loop rebuilds the analyser + re-enters.
    let entry_bins = analyser.bins() as u32;
    let entry_channels = analyser.channels() as u32;

    // F2C — wall-clock emit throttle. The inner FFT compute
    // runs at ALSA hop rate (~47 Hz at 48 kHz / 1024-point);
    // the wire emit is throttled independently to
    // `demand.rate_hz_target` (typical 30). The compute keeps
    // running for peak-hold + onset detection continuity; only
    // the announcer.update_state call is gated. `get_spectrum_frame`
    // still returns the latest frame regardless of throttle
    // — the read verb serves the shared latest_frame slot.
    let mut last_emit_at: Option<std::time::Instant> = None;

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
    let mut frames_processed_this_session: u64 = 0;
    loop {
        if shutdown_check(shutdown) {
            return InnerExit::Shutdown;
        }

        match io.readi(&mut raw_buf) {
            Ok(frames_read) => {
                consecutive_read_failures = 0;
                frames_processed_this_session =
                    frames_processed_this_session.saturating_add(1);
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
                // Combined gate. The transport half, when it
                // closes, breaks the inner loop and releases
                // the PCM handle so the outer loop can sleep on
                // the gate. The role half (Receiver-without-
                // source) is a per-emit skip (FFT continues so a
                // role flip picks up at the next tick).
                let transport_open = transport_gate.borrow().should_emit();
                if !transport_open {
                    tracing::info!(
                        plugin = PLUGIN_NAME,
                        "transport-state closed mid-capture; releasing PCM \
                         handle and idling outer loop"
                    );
                    return InnerExit::TransportFailed;
                }
                // Demand half of the CaptureGate. When the
                // operator disables the visualiser mid-play,
                // bail out of the inner loop so the outer loop
                // re-checks the gate + parks (the PCM drops
                // when we exit this scope). Same class as the
                // transport half above — release the ALSA
                // handle rather than spin an idle read cycle.
                let current_demand = *demand.borrow();
                if !current_demand.enabled {
                    tracing::info!(
                        plugin = PLUGIN_NAME,
                        "demand.enabled closed mid-capture; releasing PCM \
                         handle and idling outer loop"
                    );
                    return InnerExit::TransportFailed;
                }
                // Demand shape change (bins or channels) mid-play.
                // Bail out so the outer loop rebuilds the analyser
                // at the new shape + republishes the seed envelope.
                if current_demand.bins != entry_bins
                    || current_demand.channels != entry_channels
                {
                    tracing::info!(
                        plugin = PLUGIN_NAME,
                        entry_bins,
                        entry_channels,
                        new_bins = current_demand.bins,
                        new_channels = current_demand.channels,
                        "demand shape changed mid-capture; bailing inner loop \
                         for analyser rebuild"
                    );
                    return InnerExit::DemandShapeChanged;
                }
                let role_open = local_role.borrow().should_emit();
                if !role_open {
                    continue;
                }
                // F2C emit throttle. The FFT compute above
                // updates `latest_frame` and refreshes peak-hold
                // + onset history on every ALSA hop; the wire
                // emit fires only when the wall-clock elapsed
                // since the last emit exceeds
                // `1000 / demand.rate_hz_target` ms. Decouples
                // wire cadence from compute cadence so a fast
                // ALSA chain doesn't flood the happenings bus
                // and a slow one still emits at the operator's
                // requested cadence when frames are available.
                let target_hz = current_demand.rate_hz_target.max(1);
                let min_gap =
                    std::time::Duration::from_millis(1_000 / target_hz as u64);
                let now = std::time::Instant::now();
                if let Some(prev) = last_emit_at {
                    if now.duration_since(prev) < min_gap {
                        continue;
                    }
                }
                last_emit_at = Some(now);
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
                    // LOGGING.md §2 classification via
                    // `classify_read_failure` (single source of
                    // truth for this plugin's read-fault log level).
                    match classify_read_failure(
                        frames_processed_this_session,
                        transport_gate.borrow().should_emit(),
                    ) {
                        ReadFailClass::Warn => tracing::warn!(
                            plugin = PLUGIN_NAME,
                            recover_error = %recover_err,
                            consecutive_read_failures,
                            frames_processed_this_session,
                            "pcm.recover() failed mid-playback; bailing inner loop"
                        ),
                        ReadFailClass::Info => tracing::info!(
                            plugin = PLUGIN_NAME,
                            recover_error = %recover_err,
                            consecutive_read_failures,
                            frames_processed_this_session,
                            "pcm.recover() failed after transport transitioned to NotPlaying; \
                             expected loopback-writer lifecycle"
                        ),
                        ReadFailClass::Debug => tracing::debug!(
                            plugin = PLUGIN_NAME,
                            recover_error = %recover_err,
                            consecutive_read_failures,
                            "pcm.recover() failed with no frames captured this session; \
                             expected cycling pattern on hardware-less targets"
                        ),
                    }
                    return InnerExit::TransportFailed;
                }
                if consecutive_read_failures >= RECONNECT_MAX_ATTEMPTS {
                    match classify_read_failure(
                        frames_processed_this_session,
                        transport_gate.borrow().should_emit(),
                    ) {
                        ReadFailClass::Warn => tracing::warn!(
                            plugin = PLUGIN_NAME,
                            consecutive_read_failures,
                            frames_processed_this_session,
                            "sustained read failures despite recover() mid-playback; bailing"
                        ),
                        ReadFailClass::Info => tracing::info!(
                            plugin = PLUGIN_NAME,
                            consecutive_read_failures,
                            frames_processed_this_session,
                            "sustained read failures after transport transitioned to NotPlaying; \
                             expected loopback-writer lifecycle"
                        ),
                        ReadFailClass::Debug => tracing::debug!(
                            plugin = PLUGIN_NAME,
                            consecutive_read_failures,
                            "sustained read failures with no frames captured this session; bailing"
                        ),
                    }
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
        hwp.set_channels(INPUT_CHANNELS as u32)?;
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

/// Wait for BOTH the transport gate to open AND the demand
/// gate to open — whichever transitions first wakes the block.
/// Returns `true` on shutdown, `false` when both halves are
/// open (the outer loop should proceed to open PCM).
///
/// Same class as [`wait_for_gate_or_shutdown`] but with two
/// watch receivers instead of one; kept as a distinct helper
/// because the tokio::select! shape doesn't compose neatly
/// into the single-receiver signature without generics that
/// would obscure the read-and-check on each half.
fn wait_for_capture_gate_or_shutdown(
    transport_gate: &mut watch::Receiver<TransportGate>,
    demand: &mut watch::Receiver<SpectrumDemand>,
    shutdown: &Arc<Notify>,
) -> bool {
    let handle = match tokio::runtime::Handle::try_current() {
        Ok(h) => h,
        Err(_) => return false,
    };
    handle.block_on(async {
        loop {
            let transport_open =
                transport_gate.borrow_and_update().should_emit();
            let demand_open = demand.borrow_and_update().enabled;
            if transport_open && demand_open {
                return false;
            }
            tokio::select! {
                _ = shutdown.notified() => return true,
                changed = transport_gate.changed() => {
                    if changed.is_err() {
                        return true;
                    }
                }
                changed = demand.changed() => {
                    if changed.is_err() {
                        return true;
                    }
                }
            }
        }
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

// `PerceptualFrame` now derives `Clone` on its variable-shape
// Vec-backed fields, so the previous bespoke `clone_frame`
// helper is unused. `frame.clone()` at the call site is the
// direct replacement.
fn clone_frame(frame: &PerceptualFrame) -> PerceptualFrame {
    frame.clone()
}
