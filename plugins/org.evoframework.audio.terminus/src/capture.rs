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
//! three `watch::Receiver`s before opening PCM:
//!
//! - `TransportGate` (from the `now_playing_subscriber`):
//!   `Playing` opens this half of the gate; anything else
//!   closes it.
//! - `SpectrumDemand` (from the `demand::Store`):
//!   `demand.enabled == true` opens this half; `false` closes.
//! - `LocalRole` (from the `local_role_subscriber`): `Source`
//!   and `Auto` open this half; `Receiver` closes it
//!   (followers of an active multi-room group MUST NOT
//!   publish a parallel spectrum subject — the source-host's
//!   spectrum is the authoritative wavefront).
//!
//! All three halves MUST be open for the outer loop to open a
//! PCM handle. Any transition from active (all-open) to parked
//! (any-closed) publishes ONE final envelope on the spectrum
//! subject at the current demand's shape with `at_ms = 0`
//! before the PCM handle drops — so subscribers observing the
//! stream see a wire-visible signal of the transition and can
//! distinguish deliberate quiet from a transport-layer drop
//! or a producer wedge.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use evo_plugin_sdk::contract::SubjectAnnouncer;
use tokio::sync::{watch, Notify};

use crate::demand::SpectrumDemand;
use crate::emit_throttle::EmitThrottle;
use crate::fft::{
    PerceptualFrame, SpectrumAnalyser, FFT_WINDOW, HOP_SIZE, INPUT_CHANNELS,
};
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
    interest: watch::Receiver<u32>,
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
            interest,
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
    interest: watch::Receiver<u32>,
) {
    // Analyser is (re)built lazily on entry to the inner FFT
    // loop from the demand's current bins / channels /
    // frequency_scale. A demand change on any of those exits
    // the inner loop and the outer loop rebuilds the analyser
    // with the new shape.
    let mut analyser: Option<SpectrumAnalyser> = None;
    let mut analyser_bins: u32 = 0;
    let mut analyser_channels: u32 = 0;
    let mut analyser_scale: crate::demand::FrequencyScale =
        crate::demand::FrequencyScale::default();
    // Note: the wire `rate_hz` field is populated from
    // `current_demand.rate_hz_target` at emit time (F2C —
    // wall-clock throttle target), not from the ALSA hop rate.
    // The compute cadence (ALSA hop) is derivable from
    // `fft::frame_rate_hz(config.sample_rate_hz)` if a
    // subscriber ever needs it — nothing on the wire currently
    // carries it.
    let mut consecutive_failures: u32 = 0;
    let mut backoff = RECONNECT_INITIAL;
    // Parked-state wire-visibility discipline (F3): the outer
    // loop must emit one final envelope on the spectrum subject
    // when transitioning FROM active TO parked, before releasing
    // the ALSA PCM handle. Without the transition envelope,
    // subscribers cannot distinguish "producer parked
    // deliberately" from "transport-layer drop" from
    // "mid-processing quiet". Track a `was_running` flag that
    // flips true after a successful PCM open + inner-loop entry
    // and back to false after the parked envelope is published,
    // so we emit the transition envelope exactly once per
    // active → parked transition and never at boot.
    let mut was_running = false;

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

        // Outer-loop gate. THREE conditions must hold for the
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
        // 3. LocalRole::should_emit — the local device is
        //    Source or Auto (not Receiver). Followers of an
        //    active multi-room group do not publish a parallel
        //    spectrum subject.
        //
        // When any half closes, the capture task holds no
        // ALSA handle — no read, no FFT compute, no reconnect
        // cycle, no spin against an idle snd-aloop. The task
        // sleeps on all three watch::Receivers until whichever
        // half is closed reopens.
        //
        // Rig-verifiable via `lsof -p <pid>`: the ALSA capture
        // device disappears from the FD list.
        //
        // Parked-state wire visibility (F3): when the gate
        // closes AFTER a running session (was_running == true),
        // publish one final envelope on the spectrum subject at
        // the current demand's shape with at_ms = 0 BEFORE
        // waiting for the gate to reopen. Subscribers observing
        // the stream see the parked envelope arrive and know
        // the silence that follows is deliberate production-
        // side quiet, not a transient drop or a wedge.
        let transport_open = transport_gate.borrow().should_emit();
        let demand_open = demand.borrow().enabled;
        let role_open = local_role.borrow().should_emit();
        let interest_open = *interest.borrow() > 0;
        if !(transport_open && demand_open && role_open && interest_open) {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                transport_open,
                demand_open,
                role_open,
                interest_open,
                interest_count = *interest.borrow(),
                was_running,
                "capture-gate closed; task idle until all halves reopen"
            );
            if was_running {
                emit_parked_envelope(&announcer, *demand.borrow());
                was_running = false;
            }
            if wait_for_capture_gate_or_shutdown(
                &mut transport_gate.clone(),
                &mut demand.clone(),
                &mut local_role.clone(),
                &mut interest.clone(),
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
        // Mark active so the next outer-loop iteration that
        // finds the gate closed knows to publish the parked-
        // envelope transition signal before waiting. Cleared
        // by the gate-close branch after emitting.
        was_running = true;

        tracing::info!(
            plugin = PLUGIN_NAME,
            input_pcm = %config.input_pcm,
            sample_rate_hz = config.sample_rate_hz,
            "ALSA capture opened; entering FFT loop"
        );

        // (Re)build the analyser to match the current demand's
        // bins + channels + frequency_scale. First open after a
        // fresh plugin admit constructs from the disabled-
        // default (or the last enabled-then-disabled shape). A
        // demand change that arrives while the inner loop is
        // running exits via `InnerExit::DemandShapeChanged` and
        // the outer loop rebuilds here with the new shape.
        let current_demand = *demand.borrow();
        if analyser.is_none()
            || analyser_bins != current_demand.bins
            || analyser_channels != current_demand.channels
            || analyser_scale != current_demand.frequency_scale
        {
            analyser = Some(SpectrumAnalyser::new(
                config.sample_rate_hz,
                current_demand.bins as usize,
                current_demand.channels as usize,
                current_demand.frequency_scale,
            ));
            analyser_bins = current_demand.bins;
            analyser_channels = current_demand.channels;
            analyser_scale = current_demand.frequency_scale;
            // Republish the empty-frame envelope at the new
            // shape so consumers see the shape change on the
            // wire immediately — the first live frame will
            // carry the same shape as this seed. The wire
            // `rate_hz` field carries the demand's governed
            // emit target (not the ALSA hop rate) so
            // subscribers drive their render loop off the
            // true wire cadence.
            let seed_addr = crate::spectrum_subject::render_empty_frame(
                current_demand.rate_hz_target,
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
                            .update_state_volatile(addressing, seed_addr)
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
            &transport_gate,
            &local_role,
            &demand,
            &interest,
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
    transport_gate: &watch::Receiver<TransportGate>,
    local_role: &watch::Receiver<LocalRole>,
    demand: &watch::Receiver<SpectrumDemand>,
    interest: &watch::Receiver<u32>,
) -> InnerExit {
    // S32_LE interleaved stereo hop: HOP_SIZE frames per channel
    // per ALSA read. Analysis uses an overlap ring of FFT_WINDOW
    // frames (hop ≠ window) so frequency resolution improves
    // without slowing the compute cadence.
    let hop_samples: usize = HOP_SIZE * INPUT_CHANNELS;
    let window_samples: usize = FFT_WINDOW * INPUT_CHANNELS;
    let mut raw_buf = vec![0i32; hop_samples];
    let mut hop_f32 = vec![0.0f32; hop_samples];
    let mut ring = vec![0.0f32; window_samples];
    let mut ring_frames: usize = 0;

    // Snapshot the analyser shape at inner-loop entry. A demand
    // change to bins, channels, or frequency_scale triggers
    // `DemandShapeChanged` exit and the outer loop rebuilds the
    // analyser + re-enters.
    let entry_bins = analyser.bins() as u32;
    let entry_channels = analyser.channels() as u32;
    let entry_scale = analyser.frequency_scale();

    // F2C — wall-clock emit throttle. The inner FFT compute
    // runs at ALSA hop rate (~47 Hz at 48 kHz / hop 1024) once
    // the overlap ring is warm; the wire emit is throttled
    // independently to `demand.rate_hz_target` (typical 30).
    // Compute keeps running for peak-hold + onset detection
    // continuity; only the announcer.update_state call is gated.
    // `get_spectrum_frame` still returns the latest frame
    // regardless of throttle — the read verb serves the shared
    // latest_frame slot.
    //
    // Scheme details + regression-guarding tests live in the
    // `emit_throttle` module.
    let mut throttle = EmitThrottle::new();

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
                if frames_read < HOP_SIZE {
                    // Partial hop; drop and retry. Analysis
                    // advances only on full hops.
                    continue;
                }
                // Convert i32 [-INT32_MAX, INT32_MAX] -> f32
                // [-1, 1]. The S32_LE samples occupy the full
                // 32-bit range; divide by max-int32 as f32.
                let scale = 1.0f32 / (i32::MAX as f32);
                for i in 0..hop_samples {
                    hop_f32[i] = raw_buf[i] as f32 * scale;
                }
                // Overlap ring: slide by one hop, append the new
                // hop at the end. Warm up until FFT_WINDOW frames
                // of audio have been seen before the first FFT.
                if ring_frames >= FFT_WINDOW {
                    ring.copy_within(hop_samples..window_samples, 0);
                    ring[window_samples - hop_samples..]
                        .copy_from_slice(&hop_f32);
                } else {
                    let start = ring_frames * INPUT_CHANNELS;
                    let end = start + hop_samples;
                    if end <= window_samples {
                        ring[start..end].copy_from_slice(&hop_f32);
                    }
                    ring_frames = (ring_frames + HOP_SIZE).min(FFT_WINDOW);
                    if ring_frames < FFT_WINDOW {
                        continue;
                    }
                }
                let mut frame = analyser.process_frame(&ring);
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
                // Demand shape change (bins, channels, or
                // frequency_scale) mid-play. Bail out so the
                // outer loop rebuilds the analyser at the new
                // shape + republishes the seed envelope.
                if current_demand.bins != entry_bins
                    || current_demand.channels != entry_channels
                    || current_demand.frequency_scale != entry_scale
                {
                    tracing::info!(
                        plugin = PLUGIN_NAME,
                        entry_bins,
                        entry_channels,
                        entry_scale = entry_scale.as_str(),
                        new_bins = current_demand.bins,
                        new_channels = current_demand.channels,
                        new_scale = current_demand.frequency_scale.as_str(),
                        "demand shape changed mid-capture; bailing inner loop \
                         for analyser rebuild"
                    );
                    return InnerExit::DemandShapeChanged;
                }
                // Role half of the CaptureGate. When the local
                // device becomes a Receiver (follower of an
                // active multi-room group), bail out of the
                // inner loop so the outer loop emits the
                // parked envelope + releases the PCM handle.
                // Followers of an active group must not publish
                // a parallel spectrum subject — the group's
                // source-host emits the authoritative wavefront.
                let role_open = local_role.borrow().should_emit();
                if !role_open {
                    tracing::info!(
                        plugin = PLUGIN_NAME,
                        "local role closed mid-capture (became Receiver); \
                         releasing PCM handle and idling outer loop"
                    );
                    return InnerExit::TransportFailed;
                }
                // Interest half of the CaptureGate. When the
                // subscriber count for the spectrum subject
                // reaches zero (last WS consumer disconnected,
                // kiosk killed, etc.), bail out so the outer
                // loop emits the parked envelope + releases
                // the PCM handle. Produce-iff-consumed
                // substrate: no consumer means no compute.
                if *interest.borrow() == 0 {
                    tracing::info!(
                        plugin = PLUGIN_NAME,
                        "subscription interest reached zero mid-capture; \
                         releasing PCM handle and idling outer loop"
                    );
                    return InnerExit::TransportFailed;
                }
                // F2C emit throttle. The FFT compute above
                // updates `latest_frame` and refreshes peak-hold
                // + onset history on every ALSA hop; the wire
                // emit fires only at the demand's governed rate.
                let target_hz = current_demand.rate_hz_target.max(1);
                let min_gap =
                    std::time::Duration::from_millis(1_000 / target_hz as u64);
                if !throttle.should_emit(std::time::Instant::now(), min_gap) {
                    continue;
                }
                let announcer = Arc::clone(announcer);
                let wire_rate_hz = target_hz;
                tokio_handle.spawn(async move {
                    spectrum_subject::emit_frame(
                        &announcer,
                        &frame_clone,
                        wire_rate_hz,
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
        // Period follows the hop (not the analysis window) so
        // ALSA delivers ~47 Hz ticks at 48 kHz while the
        // overlap ring accumulates FFT_WINDOW samples.
        hwp.set_period_size(HOP_SIZE as alsa::pcm::Frames, ValueOr::Nearest)?;
        hwp.set_buffer_size_near((HOP_SIZE * 4) as alsa::pcm::Frames)?;
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
    local_role: &mut watch::Receiver<LocalRole>,
    interest: &mut watch::Receiver<u32>,
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
            let role_open = local_role.borrow_and_update().should_emit();
            let interest_open = *interest.borrow_and_update() > 0;
            if transport_open && demand_open && role_open && interest_open {
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
                changed = local_role.changed() => {
                    if changed.is_err() {
                        return true;
                    }
                }
                changed = interest.changed() => {
                    if changed.is_err() {
                        return true;
                    }
                }
            }
        }
    })
}

/// Publish one final envelope on the spectrum subject at the
/// current demand's shape with `at_ms = 0` — the "parked"
/// sentinel matching the existing empty-envelope semantics
/// used for a fresh-connect on a quiet transport. Called from
/// the outer loop's gate-close branch on every transition from
/// active to parked, before the ALSA PCM handle drops.
///
/// Runs synchronously against the current tokio runtime handle
/// so the parked envelope is on the wire before the caller
/// proceeds to release resources; if no runtime is present
/// (test path), the emit is skipped — the outer loop path
/// where this is called always has a live runtime in
/// production because it was reached from a `tokio::spawn_blocking`
/// context wired to the framework's runtime handle.
fn emit_parked_envelope(
    announcer: &Arc<dyn SubjectAnnouncer>,
    current_demand: SpectrumDemand,
) {
    use evo_plugin_sdk::contract::ExternalAddressing;
    let parked = crate::spectrum_subject::render_empty_frame(
        current_demand.rate_hz_target,
        current_demand.bins,
        current_demand.channels,
    );
    let addressing = ExternalAddressing::new(
        crate::spectrum_subject::SPECTRUM_SUBJECT_ADDRESSING_SCHEME,
        crate::spectrum_subject::SPECTRUM_SUBJECT_ADDRESSING_VALUE,
    );
    let announcer_clone = Arc::clone(announcer);
    let handle = match tokio::runtime::Handle::try_current() {
        Ok(h) => h,
        Err(_) => {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                "no tokio runtime available for parked-envelope emit; skipping \
                 (test path — production always has a runtime here)"
            );
            return;
        }
    };
    // Block on the emit so the transition envelope is on the
    // wire before the caller drops the PCM handle. The write is
    // a single frame at the current shape; latency is dominated
    // by the framework's WS-out queue and is bounded well below
    // any user-perceptible window.
    handle.block_on(async move {
        if let Err(e) = announcer_clone
            .update_state_volatile(addressing, parked)
            .await
        {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "parked-envelope emit failed; subscribers may not see the \
                 transition signal for this park cycle"
            );
        }
    });
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
