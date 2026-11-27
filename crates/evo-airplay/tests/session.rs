// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Whole-session behaviour against the in-process server, driven by the
//! synthetic sender: a stream is decoded, a second sender is refused, a sink
//! that cannot be created refuses the stream, a stream the receiver cannot
//! play is refused, the host can end a session, and a stuck sink never stalls
//! the runtime nor releases the gate before it is closed.
//!
//! The fixture `tests/data/tone_lr.frames` is three seconds of 440 Hz (left)
//! and 880 Hz (right) as raw AAC-LC frames, each prefixed by its big-endian
//! `u32` length. It was made with
//! `ffmpeg -f lavfi -i sine=frequency=440:duration=3 -f lavfi -i
//! sine=frequency=880:duration=3 -filter_complex amerge=inputs=2 -c:a aac
//! -profile:a aac_low -ar 44100 -f adts`, with each ADTS header removed.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use evo_airplay::sender::{channel_hz, parse_frames, Sender, StreamPorts};
use evo_airplay::server::{serve, Context};
use evo_airplay::{
    Admission, AudioSink, Config, Event, Identity, PairingLimiter,
    SessionControl, SessionGate, SingleSession, SinkError, SinkFactory,
};
use tokio::net::TcpListener;
use tokio::sync::mpsc::UnboundedReceiver;

const FRAMES: &[u8] = include_bytes!("data/tone_lr.frames");
const KEY: [u8; 32] = [7u8; 32];
const THREE_SECONDS: usize = 44_100 * 2 * 3;

/// A gate that counts and can be told to drop every peer.
struct CountingGate {
    inner: SingleSession,
    admits: AtomicUsize,
    releases: AtomicUsize,
    drop_peers: bool,
}

impl CountingGate {
    fn new(drop_peers: bool) -> Arc<CountingGate> {
        Arc::new(CountingGate {
            inner: SingleSession::new(),
            admits: AtomicUsize::new(0),
            releases: AtomicUsize::new(0),
            drop_peers,
        })
    }
    fn admits(&self) -> usize {
        self.admits.load(Ordering::SeqCst)
    }
    fn releases(&self) -> usize {
        self.releases.load(Ordering::SeqCst)
    }
}

impl SessionGate for CountingGate {
    fn accept_peer(&self, _peer: IpAddr) -> bool {
        !self.drop_peers
    }
    fn admit(&self, peer: IpAddr) -> Admission {
        let answer = self.inner.admit(peer);
        if answer == Admission::Allow {
            self.admits.fetch_add(1, Ordering::SeqCst);
        }
        answer
    }
    fn released(&self, peer: IpAddr) {
        self.inner.released(peer);
        self.releases.fetch_add(1, Ordering::SeqCst);
    }
}

/// A sink that keeps what it is given.
struct CaptureSink(Arc<Mutex<Vec<i16>>>);

impl AudioSink for CaptureSink {
    fn write(&mut self, pcm: &[i16]) {
        self.0.lock().expect("pcm lock").extend_from_slice(pcm);
    }
    fn flush(&mut self) {}
}

/// A sink whose write blocks until its stop signal is raised.
struct BlockedSink {
    stop: Arc<AtomicBool>,
    entered: Arc<AtomicBool>,
}

impl AudioSink for BlockedSink {
    fn write(&mut self, _pcm: &[i16]) {
        self.entered.store(true, Ordering::SeqCst);
        while !self.stop.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn flush(&mut self) {}
    fn stop_signal(&self) -> Option<Arc<AtomicBool>> {
        Some(self.stop.clone())
    }
}

/// A sink whose write blocks until the test lets it go, ignoring any stop
/// signal: the worst case for the session's end.
struct StubbornSink {
    entered: Arc<AtomicBool>,
    go: Arc<(Mutex<bool>, Condvar)>,
}

impl AudioSink for StubbornSink {
    fn write(&mut self, _pcm: &[i16]) {
        self.entered.store(true, Ordering::SeqCst);
        let (lock, cv) = &*self.go;
        let mut open = lock.lock().expect("go lock");
        while !*open {
            open = cv.wait(open).expect("go wait");
        }
    }
    fn flush(&mut self) {}
}

struct Harness {
    addr: SocketAddr,
    events: UnboundedReceiver<Event>,
    control: SessionControl,
    gate: Arc<CountingGate>,
}

async fn start(factory: SinkFactory, gate: Arc<CountingGate>) -> Harness {
    start_custom(factory, gate, None, PairingLimiter::default()).await
}

async fn start_custom(
    factory: SinkFactory,
    gate: Arc<CountingGate>,
    password: Option<&str>,
    limiter: PairingLimiter,
) -> Harness {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (events_tx, events) = tokio::sync::mpsc::unbounded_channel();
    let control = SessionControl::new();
    let context = Arc::new(Context {
        config: Config {
            name: "Test Room".into(),
            port: addr.port(),
            mac: [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
            model: "EvoAudio,1".into(),
            source_version: "366.0".into(),
            features: 0x0001_8340_405F_CA00,
            status_flags: 0x4,
            password: password.map(str::to_string),
        },
        identity: Identity::generate(),
        sink_factory: factory,
        events: events_tx,
        gate: gate.clone(),
        control: control.clone(),
        limiter,
    });
    tokio::spawn(serve(listener, context));
    Harness {
        addr,
        events,
        control,
        gate,
    }
}

fn capture() -> (SinkFactory, Arc<Mutex<Vec<i16>>>) {
    let pcm = Arc::new(Mutex::new(Vec::new()));
    let sink = pcm.clone();
    let factory: SinkFactory =
        Arc::new(move |_, _| Ok(Box::new(CaptureSink(sink.clone()))));
    (factory, pcm)
}

/// Pair, run the handshakes and set the stream up, returning the sender and
/// the answer to the stream's SETUP.
async fn begin(
    addr: SocketAddr,
    stream_type: u64,
) -> (Sender, evo_airplay::sender::Reply, Option<StreamPorts>) {
    let mut sender = Sender::connect(addr, "3939").await.expect("pair");
    sender.fp_setup().await.expect("fp-setup");
    assert!(sender.setup_event().await.expect("setup 1").is_ok());
    let (reply, ports) = sender
        .setup_stream(stream_type, &KEY)
        .await
        .expect("setup 2");
    (sender, reply, ports)
}

async fn send_all(sender: &Sender, ports: StreamPorts) {
    let mut data = sender.open_data(ports, &KEY).await.expect("data");
    for frame in parse_frames(FRAMES) {
        data.send_frame(frame).await.expect("send");
    }
}

async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..400 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

async fn wait_event(
    rx: &mut UnboundedReceiver<Event>,
    want: impl Fn(&Event) -> bool,
) -> Event {
    let wait = async {
        loop {
            match rx.recv().await {
                Some(event) if want(&event) => return event,
                Some(_) => {}
                None => panic!("event channel closed"),
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .expect("timed out waiting for an event")
}

#[tokio::test]
async fn a_stream_is_decoded_and_its_events_arrive() {
    let (factory, pcm) = capture();
    let mut h = start(factory, CountingGate::new(false)).await;
    let (mut sender, reply, ports) = begin(h.addr, 103).await;
    assert!(reply.is_ok(), "SETUP: {}", reply.status);
    let ports = ports.expect("ports");
    assert!(sender.record().await.expect("record").is_ok());
    sender.set_volume(-12.5).await.expect("volume");
    sender
        .set_metadata("Title", "Artist", "Album")
        .await
        .expect("meta");
    send_all(&sender, ports).await;
    sender.set_rate(1, 1_000_000).await.expect("rate");

    eventually("three seconds of audio", || {
        pcm.lock().expect("pcm").len() >= THREE_SECONDS - 4_096
    })
    .await;
    let heard = pcm.lock().expect("pcm").clone();
    let left = channel_hz(&heard, 0, 44_100);
    let right = channel_hz(&heard, 1, 44_100);
    assert!(
        (left - 440.0).abs() < 440.0 * 0.02,
        "left channel {left} Hz"
    );
    assert!(
        (right - 880.0).abs() < 880.0 * 0.02,
        "right channel {right} Hz"
    );

    wait_event(&mut h.events, |e| matches!(e, Event::SessionStarted { .. }))
        .await;
    wait_event(
        &mut h.events,
        |e| matches!(e, Event::Volume { db } if (*db + 12.5).abs() < 0.01),
    )
    .await;
    wait_event(
        &mut h.events,
        |e| matches!(e, Event::Metadata { title: Some(t), .. } if t == "Title"),
    )
    .await;

    assert!(sender.teardown().await.expect("teardown").is_ok());
    wait_event(&mut h.events, |e| matches!(e, Event::SessionEnded)).await;
    drop(sender);
    eventually("the gate is released", || h.gate.releases() == 1).await;
    assert_eq!(h.gate.admits(), 1);
}

#[tokio::test]
async fn a_pause_holds_the_audio_and_resume_plays_all_of_it() {
    let (factory, pcm) = capture();
    let h = start(factory, CountingGate::new(false)).await;
    let (mut sender, _, ports) = begin(h.addr, 103).await;
    let ports = ports.expect("ports");
    sender.record().await.expect("record");
    sender.set_rate(0, 1_000_000).await.expect("pause");
    send_all(&sender, ports).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        pcm.lock().expect("pcm").len(),
        0,
        "audio was played while paused"
    );
    sender.set_rate(1, 1_000_000).await.expect("resume");
    eventually("all of the held audio", || {
        pcm.lock().expect("pcm").len() >= THREE_SECONDS - 4_096
    })
    .await;
}

#[tokio::test]
async fn a_second_sender_is_refused_while_one_streams_and_admitted_after() {
    let (factory, _pcm) = capture();
    let h = start(factory, CountingGate::new(false)).await;
    let (mut first, reply, _) = begin(h.addr, 103).await;
    assert!(reply.is_ok());

    let (_second, refused, ports) = begin(h.addr, 103).await;
    assert_eq!(refused.code(), 453, "{}", refused.status);
    assert!(ports.is_none());
    assert_eq!(h.gate.admits(), 1, "the refusal took no slot");

    first.teardown().await.expect("teardown");
    drop(first);
    eventually("the first session's slot is free", || {
        h.gate.releases() == 1
    })
    .await;
    let (_third, reply, _) = begin(h.addr, 103).await;
    assert!(
        reply.is_ok(),
        "a sender is admitted once the first has ended"
    );
}

#[tokio::test]
async fn a_sink_that_cannot_be_created_refuses_the_stream() {
    let factory: SinkFactory =
        Arc::new(|_, _| Err(SinkError::new("the device is busy")));
    let mut h = start(factory, CountingGate::new(false)).await;
    let (_sender, reply, ports) = begin(h.addr, 103).await;
    assert_eq!(reply.code(), 503, "{}", reply.status);
    assert!(ports.is_none());
    eventually("the admitted slot is returned", || {
        h.gate.admits() == 1 && h.gate.releases() == 1
    })
    .await;
    while let Ok(event) = h.events.try_recv() {
        assert!(
            !matches!(event, Event::SessionStarted { .. }),
            "a refused stream must never be announced"
        );
    }
}

#[tokio::test]
async fn a_stream_type_the_receiver_cannot_play_is_refused_not_silenced() {
    let (factory, _pcm) = capture();
    let h = start(factory, CountingGate::new(false)).await;
    let (_sender, reply, ports) = begin(h.addr, 96).await;
    assert_eq!(reply.code(), 400, "{}", reply.status);
    assert!(ports.is_none());
    assert_eq!(
        h.gate.admits(),
        0,
        "the gate is not consulted for a stream that cannot play"
    );
}

#[tokio::test]
async fn the_host_can_end_the_running_session() {
    let (factory, _pcm) = capture();
    let mut h = start(factory, CountingGate::new(false)).await;
    let (mut sender, reply, _) = begin(h.addr, 103).await;
    assert!(reply.is_ok());
    assert!(sender.record().await.expect("record").is_ok());
    wait_event(&mut h.events, |e| matches!(e, Event::SessionStarted { .. }))
        .await;

    assert!(h.control.end_session(), "a session was running");
    wait_event(&mut h.events, |e| matches!(e, Event::SessionEnded)).await;
    let mut closed = false;
    for _ in 0..200 {
        if sender.is_closed().await {
            closed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(closed, "the control connection was not closed");
    eventually("the gate is released", || h.gate.releases() == 1).await;
    assert!(!h.control.end_session(), "nothing left to end");
}

#[tokio::test]
async fn a_peer_the_gate_rejects_gets_no_reply() {
    let (factory, _pcm) = capture();
    let h = start(factory, CountingGate::new(true)).await;
    let outcome = Sender::connect(h.addr, "3939").await;
    assert!(
        outcome.is_err(),
        "a rejected peer must not complete pairing"
    );
    assert_eq!(h.gate.admits(), 0);
}

#[tokio::test]
async fn a_sink_blocked_in_write_is_released_by_its_stop_signal() {
    let entered = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let (e, s) = (entered.clone(), stop.clone());
    let factory: SinkFactory = Arc::new(move |_, _| {
        Ok(Box::new(BlockedSink {
            stop: s.clone(),
            entered: e.clone(),
        }))
    });
    let h = start(factory, CountingGate::new(false)).await;
    let (sender, _, ports) = begin(h.addr, 103).await;
    send_all(&sender, ports.expect("ports")).await;
    eventually("the sink is blocked in write", || {
        entered.load(Ordering::SeqCst)
    })
    .await;

    assert!(h.control.end_session());
    eventually("the blocked sink is stopped and the gate released", || {
        h.gate.releases() == 1
    })
    .await;
    assert!(stop.load(Ordering::SeqCst), "the stop signal was raised");
}

#[tokio::test]
async fn a_sink_that_ignores_stop_cannot_stall_the_runtime_or_free_the_gate_early(
) {
    let entered = Arc::new(AtomicBool::new(false));
    let go = Arc::new((Mutex::new(false), Condvar::new()));
    let (e, g) = (entered.clone(), go.clone());
    let factory: SinkFactory = Arc::new(move |_, _| {
        Ok(Box::new(StubbornSink {
            entered: e.clone(),
            go: g.clone(),
        }))
    });
    let h = start(factory, CountingGate::new(false)).await;
    let (sender, _, ports) = begin(h.addr, 103).await;
    send_all(&sender, ports.expect("ports")).await;
    eventually("the sink is stuck in write", || {
        entered.load(Ordering::SeqCst)
    })
    .await;

    assert!(h.control.end_session());
    // This test runs on a single-threaded runtime. If ending the session
    // joined the stuck playback thread on it, nothing below would ever run.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        h.gate.releases(),
        0,
        "the gate must not be released while the sink is still open"
    );

    let (lock, cv) = &*go;
    *lock.lock().expect("go lock") = true;
    cv.notify_all();
    eventually("the gate is released once the sink returns", || {
        h.gate.releases() == 1
    })
    .await;
}

#[tokio::test]
async fn a_peer_that_keeps_guessing_is_told_to_back_off_then_may_try_again() {
    let (factory, _pcm) = capture();
    let h = start_custom(
        factory,
        CountingGate::new(false),
        Some("sekrit1"),
        PairingLimiter::new(3, Duration::from_millis(600)),
    )
    .await;
    for _ in 0..3 {
        let refused = Sender::connect(h.addr, "wrong").await.err();
        let refused = refused.expect("a wrong password is refused");
        assert!(
            refused.to_string().contains("Some(2)"),
            "authentication error: {refused}"
        );
    }
    // The right password is now turned away: this peer must wait.
    let backed_off = Sender::connect(h.addr, "sekrit1")
        .await
        .err()
        .expect("locked out");
    assert!(
        backed_off.to_string().contains("Some(3)"),
        "backoff error: {backed_off}"
    );
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        Sender::connect(h.addr, "sekrit1").await.is_ok(),
        "the right password pairs once the wait is over"
    );
}

#[tokio::test]
async fn a_success_resets_the_failure_count() {
    let (factory, _pcm) = capture();
    let h = start_custom(
        factory,
        CountingGate::new(false),
        Some("sekrit1"),
        PairingLimiter::new(3, Duration::from_secs(30)),
    )
    .await;
    for _ in 0..2 {
        assert!(Sender::connect(h.addr, "wrong").await.is_err());
    }
    assert!(Sender::connect(h.addr, "sekrit1").await.is_ok());
    for _ in 0..2 {
        assert!(Sender::connect(h.addr, "wrong").await.is_err());
    }
    assert!(
        Sender::connect(h.addr, "sekrit1").await.is_ok(),
        "two failures after a success are not a lockout"
    );
}

#[tokio::test]
async fn hostile_connections_do_not_disturb_a_running_session() {
    use rand::{RngCore, SeedableRng};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let (factory, pcm) = capture();
    let mut h = start(factory, CountingGate::new(false)).await;
    let (mut sender, reply, ports) = begin(h.addr, 103).await;
    assert!(reply.is_ok());
    let ports = ports.expect("ports");
    sender.record().await.expect("record");

    let mut rng = rand::rngs::StdRng::seed_from_u64(11);
    let mut noise = vec![0u8; 4096];
    rng.fill_bytes(&mut noise);
    let mut long_head = b"GET /info HTTP/1.1\r\n".to_vec();
    for _ in 0..4000 {
        long_head.extend_from_slice(b"X-Pad: padpadpadpad\r\n");
    }
    let attacks: Vec<Vec<u8>> = vec![
        noise,
        long_head,
        b"POST /pair-setup RTSP/1.0\r\nContent-Length: 99999999\r\n\r\n".to_vec(),
        b"POST /pair-setup RTSP/1.0\r\nContent-Length: 4\r\n\r\n\xff\xff\xff\xff"
            .to_vec(),
        b"POST /pair-setup RTSP/1.0\r\nContent-Length: -1\r\n\r\n".to_vec(),
        b"\x00\x01\x02".to_vec(),
        b"SETUP rtsp://x/1 RTSP/1.0\r\nContent-Length: 0\r\n\r\n".to_vec(),
        b"\r\n\r\n\r\n\r\n".to_vec(),
    ];
    for attack in attacks {
        let mut hostile = TcpStream::connect(h.addr).await.expect("connect");
        let _ = hostile.write_all(&attack).await;
        let _ = hostile.shutdown().await;
        let mut sink = Vec::new();
        let _ = tokio::time::timeout(
            Duration::from_secs(3),
            hostile.read_to_end(&mut sink),
        )
        .await
        .expect("the receiver must close or answer, not hang");
    }

    // The session that was playing all along still works.
    assert!(sender.info().await.expect("info").is_ok());
    send_all(&sender, ports).await;
    sender.set_rate(1, 1_000_000).await.expect("rate");
    eventually("the stream still plays", || {
        pcm.lock().expect("pcm").len() >= THREE_SECONDS - 4_096
    })
    .await;
    wait_event(&mut h.events, |e| matches!(e, Event::SessionStarted { .. }))
        .await;
}
