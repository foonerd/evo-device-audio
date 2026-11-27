// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Where decoded audio goes: the reserved Loopback substream.
//!
//! The library hands the sink interleaved `i16` PCM at the rate the
//! blocking `write` allows. This sink writes it to the playback end of a
//! substream the receiver reserves for itself (`hw:Loopback,0,4`); the
//! player plays the capture end of the same substream as a queue item.
//! Nothing here opens `pcm.evo`, which the output pipeline holds for good.
//!
//! A write never blocks without end: it waits in short slices and looks
//! at the stop signal between them, so ending a session always joins.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use evo_airplay::{AudioSink, SinkError};
use tokio::runtime::Handle;
use tokio::sync::mpsc::UnboundedSender;

use crate::player::Player;

/// The reserved playback substream: Loopback card, device 0, substream 4.
pub const DEVICE: &str = "hw:Loopback,0,4";

/// How long one wait for room in the device lasts before the stop signal
/// is looked at again.
pub const WAIT_SLICE: Duration = Duration::from_millis(100);

/// How long ending the capture may take when a sink is dropped.
const END_BUDGET: Duration = Duration::from_secs(3);

/// What a device write can say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PcmError {
    /// The device has no room right now; wait and try again.
    Full,
    /// The device ran dry and has been made ready again; try again.
    Underrun,
    /// The device failed for good.
    Failed(String),
}

/// A playback device, as the sink needs it.
pub trait Pcm: Send + 'static {
    /// Write as many of `samples` as fit without blocking; the number of
    /// samples taken is returned. A whole number of frames is taken.
    fn write(&mut self, samples: &[i16]) -> Result<usize, PcmError>;
    /// Wait up to `slice` for room. A device that ran dry while waited on
    /// is made ready again and this returns `Ok`; only a device that is
    /// really gone fails.
    fn wait(&mut self, slice: Duration) -> Result<(), PcmError>;
    /// Throw away whatever is queued and make the device ready again.
    fn flush(&mut self) -> Result<(), PcmError>;
}

/// Opens the device for a stream.
pub trait PcmOpener: Send + Sync + 'static {
    /// Open the playback device at `rate` Hz and `channels`, signed
    /// 16-bit. The stream is refused if this fails.
    fn open(&self, rate: u32, channels: u8) -> Result<Box<dyn Pcm>, SinkError>;
}

/// Something that happened to a sink that the runtime must act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// A stream was refused; the code says why (`loopback_unavailable`,
    /// `playback_refused`).
    Refused(&'static str),
    /// The device failed under a running stream.
    WriteFailed(String),
}

/// Ends the capture when the sink goes away, whatever the route: the
/// session ended, the receiver was disabled, the plugin stopped. The
/// capture removal is asked for again by the runtime; asking twice is
/// harmless, and a capture left in the queue plays silence for good.
pub struct EndCapture {
    handle: Handle,
    player: Arc<dyn Player>,
}

impl EndCapture {
    /// The guard for one stream.
    pub fn new(handle: Handle, player: Arc<dyn Player>) -> Self {
        Self { handle, player }
    }
}

impl Drop for EndCapture {
    fn drop(&mut self) {
        let player = self.player.clone();
        let end = async move {
            if let Err(e) =
                tokio::time::timeout(END_BUDGET, player.end_capture()).await
            {
                tracing::warn!(error = %e, "the capture was not ended in time");
            }
        };
        if Handle::try_current().is_ok() {
            // On a runtime thread: never block it.
            self.handle.spawn(end);
        } else {
            self.handle.block_on(end);
        }
    }
}

/// The sink the receiver gives the library.
pub struct LoopbackSink {
    pcm: Box<dyn Pcm>,
    stop: Arc<AtomicBool>,
    notes: UnboundedSender<Note>,
    failed: bool,
    _end: Option<EndCapture>,
}

impl LoopbackSink {
    /// A sink over `pcm`. `end` ends the capture when the sink is dropped.
    pub fn new(
        pcm: Box<dyn Pcm>,
        notes: UnboundedSender<Note>,
        end: Option<EndCapture>,
    ) -> Self {
        Self {
            pcm,
            stop: Arc::new(AtomicBool::new(false)),
            notes,
            failed: false,
            _end: end,
        }
    }

    fn fail(&mut self, why: String) {
        self.failed = true;
        tracing::warn!(error = %why, "the Loopback write failed; ending the session");
        let _ = self.notes.send(Note::WriteFailed(why));
    }
}

impl AudioSink for LoopbackSink {
    fn write(&mut self, pcm: &[i16]) {
        let mut rest = pcm;
        while !rest.is_empty() {
            if self.failed || self.stop.load(Ordering::Acquire) {
                return;
            }
            match self.pcm.write(rest) {
                Ok(0) | Err(PcmError::Full) => {
                    if let Err(PcmError::Failed(why)) =
                        self.pcm.wait(WAIT_SLICE)
                    {
                        self.fail(why);
                        return;
                    }
                }
                Ok(n) => rest = &rest[n.min(rest.len())..],
                Err(PcmError::Underrun) => {}
                Err(PcmError::Failed(why)) => {
                    self.fail(why);
                    return;
                }
            }
        }
    }

    fn flush(&mut self) {
        if let Err(PcmError::Failed(why)) = self.pcm.flush() {
            self.fail(why);
        }
    }

    fn stop_signal(&self) -> Option<Arc<AtomicBool>> {
        Some(self.stop.clone())
    }
}

/// The opener a build without ALSA has: every stream is refused, so the
/// receiver is never advertised as playing what it cannot.
pub struct NoAlsa;

impl PcmOpener for NoAlsa {
    fn open(&self, _: u32, _: u8) -> Result<Box<dyn Pcm>, SinkError> {
        Err(SinkError::new("this build has no ALSA support"))
    }
}

/// The opener the running plugin uses.
pub fn production_opener() -> Arc<dyn PcmOpener> {
    #[cfg(feature = "alsa-substrate")]
    {
        Arc::new(alsa_device::LoopbackOpener)
    }
    #[cfg(not(feature = "alsa-substrate"))]
    {
        Arc::new(NoAlsa)
    }
}

#[cfg(feature = "alsa-substrate")]
mod alsa_device {
    use super::{Pcm, PcmError, PcmOpener, DEVICE};
    use alsa::pcm::{Access, Format, HwParams, State, PCM};
    use alsa::{Direction, ValueOr};
    use evo_airplay::SinkError;
    use std::time::Duration;

    /// Frames per period and in the buffer: about 23 ms and 186 ms at
    /// 44.1 kHz. Small enough to add little delay, large enough to ride
    /// out scheduling jitter.
    const PERIOD_FRAMES: alsa::pcm::Frames = 1024;
    const BUFFER_FRAMES: alsa::pcm::Frames = 8192;

    pub(super) struct LoopbackOpener;

    impl PcmOpener for LoopbackOpener {
        fn open(
            &self,
            rate: u32,
            channels: u8,
        ) -> Result<Box<dyn Pcm>, SinkError> {
            let pcm = PCM::new(DEVICE, Direction::Playback, true)
                .map_err(|e| SinkError::new(format!("open {DEVICE}: {e}")))?;
            {
                let hw = HwParams::any(&pcm)
                    .map_err(|e| SinkError::new(format!("{DEVICE}: {e}")))?;
                let set = |what: &str, r: alsa::Result<()>| {
                    r.map_err(|e| {
                        SinkError::new(format!("{DEVICE}: {what}: {e}"))
                    })
                };
                set("channels", hw.set_channels(u32::from(channels)))?;
                set("rate", hw.set_rate(rate, ValueOr::Nearest))?;
                set("format", hw.set_format(Format::s16()))?;
                set("access", hw.set_access(Access::RWInterleaved))?;
                set(
                    "period",
                    hw.set_period_size_near(PERIOD_FRAMES, ValueOr::Nearest)
                        .map(|_| ()),
                )?;
                set(
                    "buffer",
                    hw.set_buffer_size_near(BUFFER_FRAMES).map(|_| ()),
                )?;
                set("hw_params", pcm.hw_params(&hw))?;
                let got = hw.get_rate().unwrap_or(0);
                if got != rate {
                    return Err(SinkError::new(format!(
                        "{DEVICE} runs at {got} Hz, not {rate} Hz"
                    )));
                }
            }
            Ok(Box::new(AlsaPcm {
                pcm,
                channels: usize::from(channels),
            }))
        }
    }

    struct AlsaPcm {
        pcm: PCM,
        channels: usize,
    }

    impl AlsaPcm {
        /// Put a device error right (an underrun or a suspend are routine on
        /// a loaded machine); `Underrun` says it was, `Failed` says it was not.
        fn recovered(&self, e: alsa::Error) -> PcmError {
            match self.pcm.try_recover(e, true) {
                Ok(()) => PcmError::Underrun,
                Err(e) => PcmError::Failed(e.to_string()),
            }
        }
    }

    impl Pcm for AlsaPcm {
        fn write(&mut self, samples: &[i16]) -> Result<usize, PcmError> {
            let whole = samples.len() - samples.len() % self.channels;
            if whole == 0 {
                return Ok(samples.len());
            }
            let io = self
                .pcm
                .io_i16()
                .map_err(|e| PcmError::Failed(e.to_string()))?;
            match io.writei(&samples[..whole]) {
                Ok(frames) => Ok(frames * self.channels),
                Err(e) if e.errno() == EAGAIN => Err(PcmError::Full),
                Err(e) => Err(self.recovered(e)),
            }
        }

        fn wait(&mut self, slice: Duration) -> Result<(), PcmError> {
            let ms = u32::try_from(slice.as_millis()).unwrap_or(u32::MAX);
            match self.pcm.wait(Some(ms)) {
                Ok(_) => Ok(()),
                // The device can run dry (or be suspended) while it is
                // waited on, not only while it is written: that costs a
                // moment of silence, not the stream.
                Err(e) => match self.recovered(e) {
                    PcmError::Underrun => Ok(()),
                    failed => Err(failed),
                },
            }
        }

        fn flush(&mut self) -> Result<(), PcmError> {
            if self.pcm.state() == State::Running {
                self.pcm
                    .drop()
                    .map_err(|e| PcmError::Failed(e.to_string()))?;
            }
            self.pcm
                .prepare()
                .map_err(|e| PcmError::Failed(e.to_string()))
        }
    }

    /// `EAGAIN` on Linux: the device is non-blocking and has no room.
    const EAGAIN: i32 = 11;
}

/// A scripted device for the sink's tests.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// What the fake device did, shared with the test.
    #[derive(Default)]
    pub struct Device {
        /// Samples the device took, in order.
        pub taken: Vec<i16>,
        /// How many samples the device has room for before it is full;
        /// `None` is unlimited.
        pub room: Option<usize>,
        /// Answers to give before behaving, front first.
        pub script: VecDeque<PcmError>,
        /// Answers `wait` gives before behaving, front first.
        pub wait_script: VecDeque<PcmError>,
        /// How many times the sink waited.
        pub waits: usize,
        /// How many times the device was flushed.
        pub flushes: usize,
    }

    pub struct FakePcm(pub Arc<Mutex<Device>>);

    impl Pcm for FakePcm {
        fn write(&mut self, samples: &[i16]) -> Result<usize, PcmError> {
            let mut d = self.0.lock().unwrap();
            if let Some(e) = d.script.pop_front() {
                return Err(e);
            }
            let free = d.room.unwrap_or(usize::MAX);
            if free == 0 {
                return Err(PcmError::Full);
            }
            let n = samples.len().min(free);
            d.taken.extend_from_slice(&samples[..n]);
            if let Some(r) = d.room.as_mut() {
                *r -= n;
            }
            Ok(n)
        }

        fn wait(&mut self, _: Duration) -> Result<(), PcmError> {
            let mut d = self.0.lock().unwrap();
            d.waits += 1;
            let scripted = d.wait_script.pop_front();
            drop(d);
            std::thread::sleep(Duration::from_millis(2));
            match scripted {
                Some(e) => Err(e),
                None => Ok(()),
            }
        }

        fn flush(&mut self) -> Result<(), PcmError> {
            let mut d = self.0.lock().unwrap();
            d.flushes += 1;
            d.taken.clear();
            Ok(())
        }
    }

    pub fn device() -> Arc<Mutex<Device>> {
        Arc::new(Mutex::new(Device::default()))
    }

    /// Opens [`FakePcm`]s over one shared [`Device`], counts how many are
    /// open, and can be told that the device cannot be opened.
    #[derive(Default)]
    pub struct FakeOpener {
        pub device: Arc<Mutex<Device>>,
        pub fail: Mutex<Option<String>>,
        pub opened: std::sync::atomic::AtomicUsize,
        pub live: Arc<std::sync::atomic::AtomicUsize>,
    }

    struct Counted(FakePcm, Arc<std::sync::atomic::AtomicUsize>);

    impl Pcm for Counted {
        fn write(&mut self, samples: &[i16]) -> Result<usize, PcmError> {
            self.0.write(samples)
        }
        fn wait(&mut self, slice: Duration) -> Result<(), PcmError> {
            self.0.wait(slice)
        }
        fn flush(&mut self) -> Result<(), PcmError> {
            self.0.flush()
        }
    }

    impl Drop for Counted {
        fn drop(&mut self) {
            self.1.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl PcmOpener for FakeOpener {
        fn open(&self, _: u32, _: u8) -> Result<Box<dyn Pcm>, SinkError> {
            if let Some(why) = self.fail.lock().unwrap().clone() {
                return Err(SinkError::new(why));
            }
            self.opened.fetch_add(1, Ordering::SeqCst);
            self.live.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Counted(
                FakePcm(self.device.clone()),
                self.live.clone(),
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;
    use crate::player::fake::{Call, FakePlayer};

    fn sink(
        d: &Arc<std::sync::Mutex<Device>>,
    ) -> (LoopbackSink, tokio::sync::mpsc::UnboundedReceiver<Note>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            LoopbackSink::new(Box::new(FakePcm(d.clone())), tx, None),
            rx,
        )
    }

    #[test]
    fn every_sample_reaches_the_device_in_order() {
        let d = device();
        let (mut s, _rx) = sink(&d);
        let audio: Vec<i16> = (0..5000).map(|n| (n % 3000) as i16).collect();
        s.write(&audio);
        assert_eq!(d.lock().unwrap().taken, audio);
    }

    #[test]
    fn a_full_device_is_waited_on_not_spun_on_and_nothing_is_lost() {
        let d = device();
        d.lock().unwrap().room = Some(10);
        let (mut s, _rx) = sink(&d);
        let audio: Vec<i16> = (0..40).collect();
        let feeder = {
            let d = d.clone();
            std::thread::spawn(move || {
                for _ in 0..4 {
                    std::thread::sleep(Duration::from_millis(15));
                    d.lock().unwrap().room = Some(10);
                }
            })
        };
        s.write(&audio);
        feeder.join().unwrap();
        let got = d.lock().unwrap();
        assert_eq!(got.taken, audio, "all of it, in order");
        assert!(got.waits > 0, "it waited for room");
    }

    #[test]
    fn a_device_that_never_has_room_is_left_when_the_session_stops() {
        let d = device();
        d.lock().unwrap().room = Some(0);
        let (mut s, _rx) = sink(&d);
        let stop = s.stop_signal().expect("a sink that can block has a signal");
        let audio = vec![1i16; 4096];
        let t = std::thread::spawn(move || s.write(&audio));
        std::thread::sleep(Duration::from_millis(50));
        assert!(!t.is_finished(), "it is waiting for room");
        stop.store(true, Ordering::Release);
        for _ in 0..200 {
            if t.is_finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(t.is_finished(), "stopping must free a blocked write");
    }

    #[test]
    fn an_underrun_is_retried_and_loses_nothing() {
        let d = device();
        d.lock().unwrap().script.push_back(PcmError::Underrun);
        let (mut s, _rx) = sink(&d);
        let audio: Vec<i16> = (0..100).collect();
        s.write(&audio);
        assert_eq!(d.lock().unwrap().taken, audio);
    }

    #[test]
    fn a_device_that_runs_dry_while_waited_on_does_not_end_the_stream() {
        let d = device();
        {
            let mut dev = d.lock().unwrap();
            dev.room = Some(0);
            dev.wait_script.push_back(PcmError::Underrun);
            dev.wait_script.push_back(PcmError::Underrun);
        }
        let (mut s, mut rx) = sink(&d);
        let opener = {
            let d = d.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(30));
                d.lock().unwrap().room = None;
            })
        };
        let audio: Vec<i16> = (0..64).collect();
        s.write(&audio);
        opener.join().unwrap();
        assert_eq!(d.lock().unwrap().taken, audio, "nothing lost");
        assert!(rx.try_recv().is_err(), "an underrun is not a failure");
    }

    #[test]
    fn a_device_that_fails_while_waited_on_is_reported() {
        let d = device();
        {
            let mut dev = d.lock().unwrap();
            dev.room = Some(0);
            dev.wait_script.push_back(PcmError::Failed("gone".into()));
        }
        let (mut s, mut rx) = sink(&d);
        s.write(&[1, 2, 3, 4]);
        assert_eq!(rx.try_recv(), Ok(Note::WriteFailed("gone".into())));
    }

    #[test]
    fn a_device_that_fails_is_reported_once_and_then_ignored() {
        let d = device();
        d.lock()
            .unwrap()
            .script
            .push_back(PcmError::Failed("gone".into()));
        let (mut s, mut rx) = sink(&d);
        s.write(&[1, 2, 3, 4]);
        s.write(&[5, 6, 7, 8]);
        assert_eq!(rx.try_recv(), Ok(Note::WriteFailed("gone".into())));
        assert!(rx.try_recv().is_err(), "reported once");
        assert!(d.lock().unwrap().taken.is_empty(), "nothing written after");
    }

    #[test]
    fn flush_drops_what_the_device_holds() {
        let d = device();
        let (mut s, _rx) = sink(&d);
        s.write(&[1, 2, 3, 4]);
        s.flush();
        let got = d.lock().unwrap();
        assert_eq!(got.flushes, 1);
        assert!(got.taken.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_the_sink_ends_the_capture_from_any_thread() {
        let player = FakePlayer::new();
        let d = device();
        // Dropped on a plain thread, as the playback thread does.
        {
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let sink = LoopbackSink::new(
                Box::new(FakePcm(d.clone())),
                tx,
                Some(EndCapture::new(Handle::current(), player.clone())),
            );
            std::thread::spawn(move || drop(sink)).join().unwrap();
        }
        assert_eq!(player.count(|c| *c == Call::End), 1);
        // Dropped on a runtime thread, which must not block or panic.
        {
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let sink = LoopbackSink::new(
                Box::new(FakePcm(d.clone())),
                tx,
                Some(EndCapture::new(Handle::current(), player.clone())),
            );
            drop(sink);
        }
        for _ in 0..100 {
            if player.count(|c| *c == Call::End) == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(player.count(|c| *c == Call::End), 2);
    }

    #[test]
    fn a_build_without_alsa_refuses_every_stream() {
        let r = NoAlsa.open(44_100, 2);
        assert!(r.is_err());
    }
}
