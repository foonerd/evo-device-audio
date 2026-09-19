// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Shared test fixtures for the `playback_supervisor` module and
//! its consumers.
//!
//! Kept `#[cfg(test)]` so it is only compiled during test builds
//! and does not inflate the release binary. Visibility is
//! `pub(crate)` so `lib.rs` tests can import these fixtures in
//! addition to `actor.rs` tests; keeping one copy of the mock
//! avoids drift between the integration tests for the supervisor
//! itself and the integration tests for the warden that wraps it.
//!
//! The `#[cfg(test)]` gate lives on the `mod test_mock;` declaration
//! in `playback_supervisor.rs`; no inner attribute needed here.

use std::collections::BTreeMap;
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
/// deregistered) is not exercised here. A consumer that does
/// need the distinction can extend `ReturnError` with named
/// variants; the current shape is the minimum that compiles
/// cleanly without dead-code warnings.
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
    state_updates: Mutex<Vec<(ExternalAddressing, serde_json::Value)>>,
    count: AtomicUsize,
    announce_return: Mutex<CaptureReturn>,
}

impl Default for CapturingSubjectAnnouncer {
    fn default() -> Self {
        Self {
            announced: Mutex::new(Vec::new()),
            retracted: Mutex::new(Vec::new()),
            state_updates: Mutex::new(Vec::new()),
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

    /// Total recorded `update_state` invocations.
    pub(crate) fn state_update_count(&self) -> usize {
        self.state_updates.lock().unwrap().len()
    }

    /// The Nth recorded `update_state` invocation (zero-indexed).
    pub(crate) fn state_update_at(
        &self,
        idx: usize,
    ) -> Option<(ExternalAddressing, serde_json::Value)> {
        self.state_updates.lock().unwrap().get(idx).cloned()
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

    fn update_state<'a>(
        &'a self,
        addressing: ExternalAddressing,
        state: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
    {
        Box::pin(async move {
            self.state_updates.lock().unwrap().push((addressing, state));
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
    /// `Title`, `Artist`, `Album`). Used by subject-emission
    /// tests that need a real song to trigger the emitter.
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
    /// A library that records every command it is sent.
    ///
    /// `listallinfo <path>` answers with `files`, so a caller
    /// that expands a directory sees its tracks; `status`
    /// reports a playing queue so a position can be computed;
    /// `addid` answers with an id. The recorded command lines
    /// let a test assert what was actually asked of MPD —
    /// whether a directory or its files reached the queue.
    RecordingLibrary {
        commands: Arc<Mutex<Vec<String>>>,
        files: Vec<String>,
    },
    /// A database that prunes only when the queued `update` job
    /// finishes.
    ///
    /// `find` answers with `internal` + `usb` until the job has
    /// run, then with `internal` alone. `status` carries
    /// `updating_db` for `in_flight_polls` reads after the
    /// `update` command, then clears — so a caller that re-counts
    /// on the update ACK sees the unpruned set and one that waits
    /// sees the pruned one.
    PrunesAfterUpdate {
        internal: Vec<String>,
        usb: Vec<String>,
        in_flight_polls: usize,
    },
    /// Like [`StandardWithSong`] but the current song changes
    /// after the first `currentsong` read: the player moved on
    /// while nobody was listening. Lets a test distinguish a
    /// fresh read from a replayed envelope.
    ///
    /// [`StandardWithSong`]: Self::StandardWithSong
    SongChangesAfterFirstRead { first: String, second: String },
    /// A live operator queue that can actually be mutated.
    ///
    /// `playlistinfo` lists what is in it with `Pos:` and `Id:`,
    /// `addid` inserts (refusing a position past the end with
    /// MPD's Bad song index ACK), `add` appends, `clear` empties,
    /// `deleteid` takes an entry out, `play <pos>` selects and
    /// starts, `stop` stops, `currentsong` answers with the
    /// selected entry, and `status` names the current song by
    /// position. A `command_list_begin` … `command_list_end`
    /// batch applies those writes and answers with one `OK` at
    /// the end — the same wire shape MPD uses for atomic
    /// replace. Every command line is recorded, so a test can
    /// assert both what survived and in which order the work
    /// was done.
    ///
    /// `items` is `(songid, mpd-relative path)` in queue order;
    /// `playing` is the position MPD reports as current, or
    /// `None` for a stopped player.
    LiveQueue {
        commands: Arc<Mutex<Vec<String>>>,
        items: Vec<(u32, String)>,
        playing: Option<u32>,
    },
    /// MPD's stored-playlist namespace, plus enough of the
    /// library to seed a new playlist from.
    ///
    /// `listplaylists` indexes it; `playlistadd` creates the
    /// playlist when it does not exist and appends when it does,
    /// as MPD's own does; `playlistdelete` and `playlistclear`
    /// mutate it; `lsinfo` walks `library`; and a
    /// `command_list_ok_begin` block of `listplaylist` answers
    /// the index's batched count query. `playlistinfo` and
    /// `status` report `queue`, which nothing here may move.
    ///
    /// `playlists` is shared with the test so it can read what
    /// MPD ended up holding. `library` maps an lsinfo path to
    /// its `(subdirectories, files)`; the root is the empty
    /// string.
    StoredPlaylists {
        commands: Arc<Mutex<Vec<String>>>,
        playlists: Arc<Mutex<BTreeMap<String, Vec<String>>>>,
        library: Vec<(String, Vec<String>, Vec<String>)>,
        queue: Vec<String>,
    },
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
        ConnBehaviour::RecordingLibrary {
            ref commands,
            ref files,
        } => {
            let listing = {
                let mut out = String::new();
                for f in files {
                    out.push_str(&format!("file: {f}\n"));
                }
                out.push_str("OK\n");
                out
            };
            let mut next_id = 100u32;
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                commands.lock().unwrap().push(line.trim_end().to_string());
                if line.starts_with("status") {
                    let _ = w
                        .write_all(
                            b"state: play\nsong: 0\nplaylistlength: 1\nOK\n",
                        )
                        .await;
                } else if line.starts_with("listallinfo") {
                    let _ = w.write_all(listing.as_bytes()).await;
                } else if line.starts_with("addid") {
                    let _ = w
                        .write_all(format!("Id: {next_id}\nOK\n").as_bytes())
                        .await;
                    next_id += 1;
                } else if line.starts_with("idle") {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    return;
                } else {
                    let _ = w.write_all(b"OK\n").await;
                }
                let _ = w.flush().await;
            }
        }
        ConnBehaviour::PrunesAfterUpdate {
            ref internal,
            ref usb,
            in_flight_polls,
        } => {
            let files = |v: &[String]| {
                let mut out = String::new();
                for f in v {
                    out.push_str(&format!("file: {f}\n"));
                }
                out.push_str("OK\n");
                out
            };
            let unpruned = {
                let mut all = internal.clone();
                all.extend(usb.iter().cloned());
                files(&all)
            };
            let pruned = files(internal);
            let mut update_seen = false;
            let mut polls_after_update = 0usize;
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                let job_done =
                    update_seen && polls_after_update >= in_flight_polls;
                if line.starts_with("update") {
                    update_seen = true;
                    polls_after_update = 0;
                    let _ = w.write_all(b"updating_db: 1\nOK\n").await;
                } else if line.starts_with("status") {
                    if update_seen && polls_after_update < in_flight_polls {
                        polls_after_update += 1;
                        let _ = w
                            .write_all(b"state: stop\nupdating_db: 1\nOK\n")
                            .await;
                    } else {
                        let _ = w.write_all(b"state: stop\nOK\n").await;
                    }
                } else if line.starts_with("find") {
                    let body = if job_done { &pruned } else { &unpruned };
                    let _ = w.write_all(body.as_bytes()).await;
                } else if line.starts_with("idle") {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    return;
                } else {
                    let _ = w.write_all(b"OK\n").await;
                }
                let _ = w.flush().await;
            }
        }
        ConnBehaviour::LiveQueue {
            ref commands,
            ref items,
            playing,
        } => {
            let mut queue = items.clone();
            let mut current = playing;
            let mut state = if playing.is_some() { "play" } else { "stop" };
            let mut next_id =
                queue.iter().map(|(id, _)| *id).max().unwrap_or(99) + 1;
            let mut in_list = false;
            let mut stored: std::collections::HashMap<String, Vec<String>> =
                std::collections::HashMap::new();
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                let cmd = line.trim_end().to_string();
                commands.lock().unwrap().push(cmd.clone());
                if cmd == "command_list_begin" {
                    in_list = true;
                    continue;
                } else if cmd == "command_list_end" {
                    in_list = false;
                    let _ = w.write_all(b"OK\n").await;
                } else if cmd.starts_with("playlistinfo") {
                    let mut out = String::new();
                    for (pos, (id, path)) in queue.iter().enumerate() {
                        out.push_str(&format!(
                            "file: {path}\nPos: {pos}\nId: {id}\n"
                        ));
                    }
                    out.push_str("OK\n");
                    let _ = w.write_all(out.as_bytes()).await;
                } else if cmd.starts_with("status") {
                    let mut out = format!(
                        "state: {state}\nplaylistlength: {}\n",
                        queue.len()
                    );
                    if let Some(pos) = current {
                        out.push_str(&format!("song: {pos}\n"));
                    }
                    out.push_str("OK\n");
                    let _ = w.write_all(out.as_bytes()).await;
                } else if cmd.starts_with("deleteid") {
                    let id = cmd
                        .split_whitespace()
                        .nth(1)
                        .and_then(|t| t.trim_matches('"').parse::<u32>().ok());
                    if let Some(id) = id {
                        queue.retain(|(qid, _)| *qid != id);
                    }
                    if current.is_some_and(|p| p as usize >= queue.len()) {
                        current = None;
                    }
                    let _ = w.write_all(b"OK\n").await;
                } else if cmd.starts_with("stop") {
                    state = "stop";
                    let _ = w.write_all(b"OK\n").await;
                } else if cmd.starts_with("addid") {
                    // `addid "<uri>" ["<pos>"]`. MPD refuses a
                    // position past the end with a Bad song
                    // index ACK — the mock refuses it the same
                    // way, so a test sees the operator's
                    // refusal rather than a silent success.
                    let mut args = cmd.split('"').filter(|s| {
                        !s.trim().is_empty() && !s.starts_with("addid")
                    });
                    let uri = args.next().unwrap_or_default().to_string();
                    let pos =
                        args.next().and_then(|t| t.trim().parse::<u32>().ok());
                    let at = pos.unwrap_or(queue.len() as u32) as usize;
                    if at > queue.len() {
                        let _ = w
                            .write_all(b"ACK [2@0] {addid} Bad song index\n")
                            .await;
                    } else {
                        queue.insert(at, (next_id, uri));
                        let _ = w
                            .write_all(
                                format!("Id: {next_id}\nOK\n").as_bytes(),
                            )
                            .await;
                        next_id += 1;
                    }
                } else if cmd.starts_with("listplaylists") {
                    let mut out = String::new();
                    for name in stored.keys() {
                        out.push_str(&format!("playlist: {name}\n"));
                    }
                    out.push_str("OK\n");
                    let _ = w.write_all(out.as_bytes()).await;
                } else if cmd.starts_with("listplaylistinfo") {
                    let name =
                        cmd.split('"').nth(1).unwrap_or_default().to_string();
                    let mut out = String::new();
                    if let Some(entries) = stored.get(&name) {
                        for (pos, path) in entries.iter().enumerate() {
                            out.push_str(&format!(
                                "file: {path}\nPos: {pos}\n"
                            ));
                        }
                    }
                    out.push_str("OK\n");
                    let _ = w.write_all(out.as_bytes()).await;
                } else if cmd.starts_with("playlistadd") {
                    let mut args = cmd.split('"').filter(|s| {
                        !s.trim().is_empty() && !s.starts_with("playlistadd")
                    });
                    let name = args.next().unwrap_or_default().to_string();
                    let uri = args.next().unwrap_or_default().to_string();
                    if !name.is_empty() && !uri.is_empty() {
                        stored.entry(name).or_default().push(uri);
                    }
                    let _ = w.write_all(b"OK\n").await;
                } else if cmd.starts_with("playlistdelete") {
                    let mut args = cmd.split('"').filter(|s| {
                        !s.trim().is_empty() && !s.starts_with("playlistdelete")
                    });
                    let name = args.next().unwrap_or_default().to_string();
                    let pos = args
                        .next()
                        .and_then(|t| t.trim().parse::<usize>().ok());
                    if let (Some(entries), Some(pos)) =
                        (stored.get_mut(&name), pos)
                    {
                        if pos < entries.len() {
                            entries.remove(pos);
                        }
                    }
                    let _ = w.write_all(b"OK\n").await;
                } else if cmd.starts_with("playlistclear") {
                    let name =
                        cmd.split('"').nth(1).unwrap_or_default().to_string();
                    stored.remove(&name);
                    let _ = w.write_all(b"OK\n").await;
                } else if cmd.starts_with("count") {
                    // This mock models a library that matches
                    // nothing: every `count` answers zero, the
                    // way MPD answers a filter with no hits. A
                    // test that needs a real match count wants
                    // its own behaviour rather than this one.
                    let _ = w.write_all(b"songs: 0\nplaytime: 0\nOK\n").await;
                } else if cmd.starts_with("currentsong") {
                    // Ordered after `playlistinfo` and before
                    // `play`: MPD's queue verbs share prefixes
                    // and the first match wins.
                    let out =
                        match current.and_then(|pos| queue.get(pos as usize)) {
                            Some((_, path)) => format!(
                                "file: {path}\nTitle: T\nArtist: A\nAlbum: X\n\
                             Time: 180\nduration: 180.000\nOK\n"
                            ),
                            None => "OK\n".to_string(),
                        };
                    let _ = w.write_all(out.as_bytes()).await;
                } else if cmd.starts_with("play") {
                    let pos = cmd
                        .split_whitespace()
                        .nth(1)
                        .and_then(|t| t.trim_matches('"').parse::<u32>().ok());
                    if let Some(p) = pos {
                        current = Some(p);
                    }
                    state = "play";
                    if !in_list {
                        let _ = w.write_all(b"OK\n").await;
                    }
                } else if cmd == "clear" {
                    queue.clear();
                    current = None;
                    if !in_list {
                        let _ = w.write_all(b"OK\n").await;
                    }
                } else if cmd.starts_with("add ") {
                    let uri =
                        cmd.split('"').nth(1).unwrap_or_default().to_string();
                    if !uri.is_empty() {
                        queue.push((next_id, uri));
                        next_id += 1;
                    }
                    if !in_list {
                        let _ = w.write_all(b"OK\n").await;
                    }
                } else if cmd.starts_with("idle") {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    return;
                } else if !in_list {
                    let _ = w.write_all(b"OK\n").await;
                }
                let _ = w.flush().await;
            }
        }
        ConnBehaviour::StoredPlaylists {
            ref commands,
            ref playlists,
            ref library,
            ref queue,
        } => {
            // `cmd "arg one" "arg two"` — MPD quotes every
            // argument, so the odd-indexed splits are the args.
            fn args(cmd: &str) -> Vec<String> {
                cmd.split('"')
                    .skip(1)
                    .step_by(2)
                    .map(str::to_string)
                    .collect()
            }
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                let cmd = line.trim_end().to_string();
                commands.lock().unwrap().push(cmd.clone());

                // A command list is the one shape that reads more
                // input before it can answer, so it is collected
                // first and answered with the rest.
                let mut block: Vec<String> = Vec::new();
                if cmd.starts_with("command_list")
                    && !cmd.starts_with("command_list_end")
                {
                    loop {
                        line.clear();
                        match reader.read_line(&mut line).await {
                            Ok(0) | Err(_) => return,
                            Ok(_) => {}
                        }
                        let c = line.trim_end().to_string();
                        commands.lock().unwrap().push(c.clone());
                        if c.starts_with("command_list_end") {
                            break;
                        }
                        block.push(c);
                    }
                }

                // Every arm computes its reply with the lock held
                // only inside this block: a guard alive across the
                // write below would make this future non-Send.
                let reply = {
                    let mut held = playlists.lock().unwrap();
                    if cmd.starts_with("command_list") {
                        // `command_list_ok_begin` separates each
                        // command's reply with `list_OK`; plain
                        // `command_list_begin` answers once at
                        // the end. Both apply their writes.
                        let separated = cmd.starts_with("command_list_ok");
                        let mut out = String::new();
                        for c in &block {
                            let a = args(c);
                            if c.starts_with("listplaylist") {
                                let name =
                                    a.first().cloned().unwrap_or_default();
                                if let Some(entries) = held.get(&name) {
                                    for e in entries {
                                        out.push_str(&format!("file: {e}\n"));
                                    }
                                }
                            } else if c.starts_with("playlistadd")
                                && a.len() >= 2
                            {
                                held.entry(a[0].clone())
                                    .or_default()
                                    .push(a[1].clone());
                            }
                            if separated {
                                out.push_str("list_OK\n");
                            }
                        }
                        out.push_str("OK\n");
                        out
                    } else if cmd.starts_with("count") {
                        // Models a library that matches nothing,
                        // the way MPD answers a filter with no
                        // hits. A test needing a real count wants
                        // its own behaviour.
                        "songs: 0\nplaytime: 0\nOK\n".to_string()
                    } else if cmd.starts_with("listplaylists") {
                        let mut out = String::new();
                        for name in held.keys() {
                            out.push_str(&format!("playlist: {name}\n"));
                        }
                        out.push_str("OK\n");
                        out
                    } else if cmd.starts_with("lsinfo") {
                        let path =
                            args(&cmd).first().cloned().unwrap_or_default();
                        let mut out = String::new();
                        if let Some((_, subdirs, files)) =
                            library.iter().find(|(p, _, _)| *p == path)
                        {
                            for d in subdirs {
                                out.push_str(&format!("directory: {d}\n"));
                            }
                            for f in files {
                                out.push_str(&format!("file: {f}\n"));
                            }
                        }
                        out.push_str("OK\n");
                        out
                    } else if cmd.starts_with("playlistadd") {
                        let a = args(&cmd);
                        if a.len() >= 2 {
                            // MPD creates NAME.m3u when it is
                            // absent; that creation is the whole
                            // point of the seed.
                            held.entry(a[0].clone())
                                .or_default()
                                .push(a[1].clone());
                        }
                        "OK\n".to_string()
                    } else if cmd.starts_with("playlistdelete") {
                        let a = args(&cmd);
                        let pos =
                            a.get(1).and_then(|p| p.parse::<usize>().ok());
                        let entries = a.first().and_then(|n| held.get_mut(n));
                        match (entries, pos) {
                            (Some(entries), Some(p)) if p < entries.len() => {
                                entries.remove(p);
                                "OK\n".to_string()
                            }
                            _ => "ACK [2@0] {playlistdelete} Bad song index\n"
                                .to_string(),
                        }
                    } else if cmd.starts_with("playlistclear") {
                        if let Some(name) = args(&cmd).first() {
                            if let Some(entries) = held.get_mut(name) {
                                entries.clear();
                            }
                        }
                        "OK\n".to_string()
                    } else if cmd.starts_with("save") {
                        // MPD's `save` ACKs 56 on a name that
                        // already exists; otherwise it writes
                        // the current queue under that name.
                        match args(&cmd).first() {
                            Some(name) if held.contains_key(name) => {
                                "ACK [56@0] {save} Playlist already exists\n"
                                    .to_string()
                            }
                            Some(name) => {
                                held.insert(name.clone(), queue.clone());
                                "OK\n".to_string()
                            }
                            None => "OK\n".to_string(),
                        }
                    } else if cmd.starts_with("rm") {
                        // And `rm` ACKs 50 on a name that is not
                        // there, which the save-as path swallows
                        // on purpose.
                        match args(&cmd).first() {
                            Some(name) if held.remove(name).is_some() => {
                                "OK\n".to_string()
                            }
                            _ => {
                                "ACK [50@0] {rm} No such playlist\n".to_string()
                            }
                        }
                    } else if cmd.starts_with("playlistinfo") {
                        let mut out = String::new();
                        for (pos, path) in queue.iter().enumerate() {
                            out.push_str(&format!(
                                "file: {path}\nPos: {pos}\nId: {}\n",
                                pos + 1
                            ));
                        }
                        out.push_str("OK\n");
                        out
                    } else if cmd.starts_with("status") {
                        let mut out = format!(
                            "state: play\nplaylistlength: {}\n",
                            queue.len()
                        );
                        if !queue.is_empty() {
                            out.push_str("song: 0\n");
                        }
                        out.push_str("OK\n");
                        out
                    } else if cmd.starts_with("idle") {
                        String::new()
                    } else {
                        "OK\n".to_string()
                    }
                };

                if cmd.starts_with("idle") {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    return;
                }
                let _ = w.write_all(reply.as_bytes()).await;
                let _ = w.flush().await;
            }
        }
        ConnBehaviour::SongChangesAfterFirstRead {
            ref first,
            ref second,
        } => {
            let song = |file: &str| {
                format!(
                    "file: {file}\nTitle: T\nArtist: A\nAlbum: X\n\
                     Time: 180\nduration: 180.000\nOK\n"
                )
            };
            let first_resp = song(first);
            let second_resp = song(second);
            let status_resp =
                b"state: play\nsong: 0\nelapsed: 1.000\nduration: 180.000\nvolume: 50\nOK\n";
            let mut reads = 0usize;
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
                    reads += 1;
                    let body = if reads <= 1 {
                        &first_resp
                    } else {
                        &second_resp
                    };
                    let _ = w.write_all(body.as_bytes()).await;
                } else if line.starts_with("idle") {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    return;
                } else {
                    let _ = w.write_all(b"OK\n").await;
                }
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
