//! Shared test fixtures for the `playback_supervisor` module and
//! its consumers.
//!
//! Kept `#[cfg(test)]` so it is only compiled during test builds
//! and does not inflate the release binary. Visibility is
//! `pub(crate)` so `lib.rs` tests can import these fixtures in
//! addition to `actor.rs` tests; keeping one copy of the mock
//! avoids drift between the integration tests for the supervisor
//! itself and the integration tests for the warden that wraps it.

#![cfg(test)]

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use evo_plugin_sdk::contract::{
    CustodyHandle, CustodyStateReporter, ExternalAddressing, HealthStatus,
    RelationAnnouncer, RelationAssertion, RelationRetraction, ReportError,
    SubjectAnnouncement, SubjectAnnouncer,
};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::mpd::{ConnectTimeouts, MpdEndpoint};
use crate::playback_supervisor::SubjectEmitter;

// ----- timeouts and handles -----

/// Short timeouts suitable for tests. Generous enough to tolerate
/// a loaded CI machine; tight enough that a test against an
/// unresponsive mock fails in well under a second.
pub(crate) fn short_timeouts() -> ConnectTimeouts {
    ConnectTimeouts {
        connect: Duration::from_millis(500),
        welcome: Duration::from_millis(500),
        command: Duration::from_millis(500),
    }
}

/// A deterministic [`CustodyHandle`] for tests that do not care
/// about handle identity.
pub(crate) fn test_custody_handle() -> CustodyHandle {
    CustodyHandle::new("custody-test")
}

// ----- capturing reporter -----

/// Reporter that records every `report()` invocation. Used by
/// both `actor.rs` and `lib.rs` tests to assert on initial and
/// follow-up state reports.
#[derive(Default)]
pub(crate) struct CapturingReporter {
    reports: Mutex<Vec<(CustodyHandle, Vec<u8>, HealthStatus)>>,
    count: AtomicUsize,
}

impl CapturingReporter {
    pub(crate) fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    /// Full record of the most recent report, if any.
    pub(crate) fn last(
        &self,
    ) -> Option<(CustodyHandle, Vec<u8>, HealthStatus)> {
        self.reports.lock().unwrap().last().cloned()
    }

    /// Convenience: the payload of the most recent report.
    pub(crate) fn last_payload(&self) -> Option<Vec<u8>> {
        self.last().map(|(_, p, _)| p)
    }
}

impl CustodyStateReporter for CapturingReporter {
    fn report<'a>(
        &'a self,
        handle: &'a CustodyHandle,
        payload: Vec<u8>,
        health: HealthStatus,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        let handle = handle.clone();
        Box::pin(async move {
            self.reports.lock().unwrap().push((handle, payload, health));
            self.count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

// ----- capturing subject / relation announcers -----

/// Policy for what a capturing announcer returns from `announce`
/// and `assert`. Tests select a policy to simulate success or
/// steward-side rejection.
///
/// Today only `Ok` and `Err(Invalid)` are reachable from tests;
/// the full [`ReportError`] taxonomy (rate-limited, shutting-down,
/// deregistered) is not exercised here because no Phase 3.4 test
/// needs to distinguish those outcomes. A later phase that does
/// need them can extend `ReturnError` with named variants; the
/// current shape is the minimum that compiles cleanly without
/// dead-code warnings.
#[derive(Debug, Clone)]
enum CaptureReturn {
    Ok,
    Err(ReturnError),
}

/// Parameterised representation of the errors the capturing
/// announcers can return. [`ReportError`] itself is
/// `#[non_exhaustive]`, so tests construct instances through this
/// enum rather than matching on the SDK type. Only the variant
/// the current tests use is represented; extend when a new test
/// actually needs a different outcome.
#[derive(Debug, Clone)]
enum ReturnError {
    Invalid(String),
}

impl ReturnError {
    fn to_report_error(&self) -> ReportError {
        match self {
            Self::Invalid(s) => ReportError::Invalid(s.clone()),
        }
    }
}

/// [`SubjectAnnouncer`] double that records every `announce` and
/// `retract` call for test assertion.
///
/// By default every call returns `Ok(())`. Use
/// [`Self::failing_with_invalid`] to configure all `announce`
/// calls to fail with `ReportError::Invalid`.
pub(crate) struct CapturingSubjectAnnouncer {
    announced: Mutex<Vec<SubjectAnnouncement>>,
    retracted: Mutex<Vec<(ExternalAddressing, Option<String>)>>,
    count: AtomicUsize,
    announce_return: Mutex<CaptureReturn>,
}

impl Default for CapturingSubjectAnnouncer {
    fn default() -> Self {
        Self {
            announced: Mutex::new(Vec::new()),
            retracted: Mutex::new(Vec::new()),
            count: AtomicUsize::new(0),
            announce_return: Mutex::new(CaptureReturn::Ok),
        }
    }
}

impl CapturingSubjectAnnouncer {
    /// Construct a capturing announcer whose `announce` calls
    /// always return `ReportError::Invalid`. `retract` is
    /// unaffected and still returns `Ok`.
    pub(crate) fn failing_with_invalid() -> Self {
        Self {
            announce_return: Mutex::new(CaptureReturn::Err(
                ReturnError::Invalid("test-configured failure".into()),
            )),
            ..Self::default()
        }
    }

    pub(crate) fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    /// The Nth recorded announcement (zero-indexed), if any.
    pub(crate) fn at(&self, idx: usize) -> Option<SubjectAnnouncement> {
        self.announced.lock().unwrap().get(idx).cloned()
    }
}

impl SubjectAnnouncer for CapturingSubjectAnnouncer {
    fn announce<'a>(
        &'a self,
        announcement: SubjectAnnouncement,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        Box::pin(async move {
            let ret = self.announce_return.lock().unwrap().clone();
            self.announced.lock().unwrap().push(announcement);
            self.count.fetch_add(1, Ordering::SeqCst);
            match ret {
                CaptureReturn::Ok => Ok(()),
                CaptureReturn::Err(e) => Err(e.to_report_error()),
            }
        })
    }

    fn retract<'a>(
        &'a self,
        addressing: ExternalAddressing,
        reason: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        Box::pin(async move {
            self.retracted.lock().unwrap().push((addressing, reason));
            Ok(())
        })
    }
}

/// [`RelationAnnouncer`] double that records every `assert` and
/// `retract` call for test assertion.
pub(crate) struct CapturingRelationAnnouncer {
    asserted: Mutex<Vec<RelationAssertion>>,
    retracted: Mutex<Vec<RelationRetraction>>,
    count: AtomicUsize,
    assert_return: Mutex<CaptureReturn>,
}

impl Default for CapturingRelationAnnouncer {
    fn default() -> Self {
        Self {
            asserted: Mutex::new(Vec::new()),
            retracted: Mutex::new(Vec::new()),
            count: AtomicUsize::new(0),
            assert_return: Mutex::new(CaptureReturn::Ok),
        }
    }
}

impl CapturingRelationAnnouncer {
    /// Construct a capturing announcer whose `assert` calls
    /// always return `ReportError::Invalid`.
    pub(crate) fn failing_with_invalid() -> Self {
        Self {
            assert_return: Mutex::new(CaptureReturn::Err(
                ReturnError::Invalid("test-configured failure".into()),
            )),
            ..Self::default()
        }
    }

    pub(crate) fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    /// The Nth recorded assertion (zero-indexed), if any.
    pub(crate) fn at(&self, idx: usize) -> Option<RelationAssertion> {
        self.asserted.lock().unwrap().get(idx).cloned()
    }
}

impl RelationAnnouncer for CapturingRelationAnnouncer {
    fn assert<'a>(
        &'a self,
        assertion: RelationAssertion,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        Box::pin(async move {
            let ret = self.assert_return.lock().unwrap().clone();
            self.asserted.lock().unwrap().push(assertion);
            self.count.fetch_add(1, Ordering::SeqCst);
            match ret {
                CaptureReturn::Ok => Ok(()),
                CaptureReturn::Err(e) => Err(e.to_report_error()),
            }
        })
    }

    fn retract<'a>(
        &'a self,
        retraction: RelationRetraction,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        Box::pin(async move {
            self.retracted.lock().unwrap().push(retraction);
            Ok(())
        })
    }
}

/// Build a [`SubjectEmitter`] backed by capturing announcers.
/// Returns the Arcs to the capturing objects so the test can
/// inspect them after calling the emitter; the emitter itself
/// holds its own Arcs (via clones) and can be passed to the
/// supervisor or the warden.
pub(crate) fn capturing_emitter() -> (
    Arc<CapturingSubjectAnnouncer>,
    Arc<CapturingRelationAnnouncer>,
    SubjectEmitter,
) {
    let subjects = Arc::new(CapturingSubjectAnnouncer::default());
    let relations = Arc::new(CapturingRelationAnnouncer::default());
    let emitter = SubjectEmitter::new(
        subjects.clone() as Arc<dyn SubjectAnnouncer>,
        relations.clone() as Arc<dyn RelationAnnouncer>,
    );
    (subjects, relations, emitter)
}

// ----- TCP mock MPD -----

/// Behaviour for a single connection accepted by the mock.
///
/// Variants describe what the mock does after sending its welcome
/// banner. Each connection consumes one variant in the order the
/// mock was configured; extras are dropped.
#[derive(Clone)]
pub(crate) enum ConnBehaviour {
    /// Generic "MPD is working" handler:
    /// - `status`     => `state: stop\nOK\n`
    /// - `currentsong`=> `OK\n` (empty current song)
    /// - `idle`       => hold without response
    /// - anything else=> `OK\n`
    Standard,
    /// Like [`Standard`] but `status` reports a `play` state and
    /// `currentsong` returns a populated response (`file`,
    /// `Title`, `Artist`, `Album`). Used by Phase 3.4 subject-
    /// emission tests that need a real song to trigger the
    /// emitter.
    ///
    /// [`Standard`]: Self::Standard
    StandardWithSong {
        file: String,
        title: String,
        artist: String,
        album: String,
    },
    /// Same as [`Standard`] but the Nth command (1-indexed) is
    /// met with an ACK reply instead of OK.
    ///
    /// [`Standard`]: Self::Standard
    AckOnNth {
        nth: usize,
        code: u32,
        message: String,
    },
    /// Same as [`Standard`] but the Nth command (1-indexed)
    /// causes the connection to close without replying.
    ///
    /// [`Standard`]: Self::Standard
    CloseOnNth { nth: usize },
    /// Welcome then silence. Useful for idle-side connection
    /// slots when the test does not exercise idle.
    HoldAfterWelcome,
    /// Welcome, then respond to the first `idle` command with
    /// `changed: player\nOK\n`, then hold.
    IdleOnceThenHold,
}

/// Bind a loopback listener and serve incoming connections with
/// the supplied behaviours, in order. Extra connections beyond
/// the end of `behaviours` are dropped on accept.
///
/// Returns the endpoint to hand to the supervisor (or the warden)
/// plus the listener task's `JoinHandle`. Dropping the handle
/// does not close the listener; the listener lives until the
/// tokio runtime shuts down.
pub(crate) async fn spawn_mock_mpd(
    behaviours: Vec<ConnBehaviour>,
) -> (MpdEndpoint, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let endpoint =
        MpdEndpoint::tcp(addr.ip().to_string(), addr.port()).unwrap();
    let task = tokio::spawn(async move {
        let mut iter = behaviours.into_iter();
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => return,
            };
            match iter.next() {
                Some(b) => {
                    tokio::spawn(serve_connection(stream, b));
                }
                None => {
                    drop(stream);
                }
            }
        }
    });
    (endpoint, task)
}

/// The "nothing responds" variant: binds a listener but never
/// sends a welcome. Useful for tests that need the supervisor's
/// connect / welcome path to fail. Connections accepted are held
/// open silently until the listener drops.
pub(crate) async fn spawn_unresponsive_mock() -> (MpdEndpoint, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let endpoint =
        MpdEndpoint::tcp(addr.ip().to_string(), addr.port()).unwrap();
    let task = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tokio::spawn(async move {
                        // Hold but do not write the welcome; the
                        // supervisor's welcome timeout fires.
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        drop(stream);
                    });
                }
                Err(_) => return,
            }
        }
    });
    (endpoint, task)
}

async fn serve_connection(mut stream: TcpStream, b: ConnBehaviour) {
    let (r, mut w) = stream.split();
    let mut reader = BufReader::new(r);

    // Welcome first, unconditionally.
    if w.write_all(b"OK MPD 0.23.5\n").await.is_err() {
        return;
    }
    if w.flush().await.is_err() {
        return;
    }

    match b {
        ConnBehaviour::HoldAfterWelcome => {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
        ConnBehaviour::IdleOnceThenHold => {
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                if line.starts_with("idle") {
                    let _ = w.write_all(b"changed: player\nOK\n").await;
                    let _ = w.flush().await;
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    return;
                }
                let _ = w.write_all(b"OK\n").await;
                let _ = w.flush().await;
            }
        }
        ConnBehaviour::StandardWithSong {
            ref file,
            ref title,
            ref artist,
            ref album,
        } => {
            let currentsong_resp = format!(
                "file: {}\nTitle: {}\nArtist: {}\nAlbum: {}\nTime: 180\nduration: 180.000\nOK\n",
                file, title, artist, album
            );
            let status_resp =
                b"state: play\nsong: 0\nelapsed: 1.000\nduration: 180.000\nvolume: 50\nOK\n";
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                if line.starts_with("status") {
                    let _ = w.write_all(status_resp).await;
                } else if line.starts_with("currentsong") {
                    let _ = w.write_all(currentsong_resp.as_bytes()).await;
                } else if line.starts_with("idle") {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    return;
                } else {
                    let _ = w.write_all(b"OK\n").await;
                }
                let _ = w.flush().await;
            }
        }
        ConnBehaviour::Standard
        | ConnBehaviour::AckOnNth { .. }
        | ConnBehaviour::CloseOnNth { .. } => {
            let mut seq: usize = 0;
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                seq += 1;

                if let ConnBehaviour::AckOnNth {
                    nth,
                    code,
                    ref message,
                } = b
                {
                    if seq == nth {
                        let cmd_name =
                            line.split_whitespace().next().unwrap_or("");
                        let ack = format!(
                            "ACK [{}@0] {{{}}} {}\n",
                            code, cmd_name, message
                        );
                        let _ = w.write_all(ack.as_bytes()).await;
                        let _ = w.flush().await;
                        continue;
                    }
                }
                if let ConnBehaviour::CloseOnNth { nth } = b {
                    if seq == nth {
                        return;
                    }
                }

                if line.starts_with("status") {
                    let _ = w.write_all(b"state: stop\nOK\n").await;
                } else if line.starts_with("currentsong") {
                    let _ = w.write_all(b"OK\n").await;
                } else if line.starts_with("idle") {
                    // Hold forever on idle; no response.
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    return;
                } else {
                    let _ = w.write_all(b"OK\n").await;
                }
                let _ = w.flush().await;
            }
        }
    }
}
