//! Playback supervisor actor.
//!
//! Long-lived orchestrator that holds two [`MpdConnection`]s for
//! the duration of a custody: one dedicated to command dispatch,
//! one dedicated to the MPD idle subprotocol. Two connections are
//! required because MPD blocks the connection while an idle call
//! is pending, so running idle and commands on the same socket is
//! impossible.
//!
//! # Architecture
//!
//! Two tokio tasks communicating via channels:
//!
//! - **Main supervisor task** ([`SupervisorTask::run`]): owns
//!   `command_connection`. Receives [`SupervisorMessage`] values
//!   from an `mpsc::Receiver`, dispatches them against the command
//!   connection, emits state reports through the reporter. Handles
//!   shutdown on a `oneshot::Receiver`. Reconnects with bounded
//!   exponential backoff when the command connection fails.
//! - **Idle task** ([`idle_task`]): owns `idle_connection`. Loops
//!   on [`MpdConnection::idle`] against `[Player, Mixer, Options,
//!   Playlist]` with a 30s per-call budget. Sends `IdleEvent`
//!   values to the main supervisor via a second `mpsc::Sender`.
//!   Reconnects with the same backoff when idle fails.
//!
//! Separation by task rather than a single `select!` avoids the
//! borrow-conflict and cancellation hazard: a `select!` arm that
//! called `idle(&mut self, ...)` would hold `&mut conn` across an
//! await for up to 30s, blocking the other arms from using the
//! same connection even if they wanted a different one.
//!
//! # Failure classification
//!
//! - [`MpdError::Ack`]: command-level rejection, connection stays
//!   healthy. Not retried; surfaced as [`PlaybackError::Ack`].
//! - [`MpdError::Transport`] / [`MpdError::Timeout`]: connection
//!   is suspect. Triggers reconnection with backoff; the command
//!   is retried exactly once after a successful reconnect.
//! - [`MpdError::Protocol`] / [`MpdError::Config`]: non-retryable.
//!   Surfaced as [`PlaybackError::Protocol`].
//!
//! # State reports
//!
//! Emitted at three trigger points:
//! 1. Initial report during [`spawn`] (synchronous; failure here
//!    aborts spawn).
//! 2. After every successful command (best-effort; failure warns
//!    but does not break the supervisor).
//! 3. After every non-empty idle event (best-effort).
//!
//! Each emission is a fresh `status` + `currentsong` on the
//! command connection, projected to `PlaybackStateReport`,
//! serialised to TOML, sent via the reporter.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use evo_plugin_sdk::contract::{
    CustodyHandle, CustodyStateReporter, HealthStatus,
};

use crate::mpd::{
    ConnectTimeouts, IdleSubsystem, MpdConnection, MpdEndpoint, MpdError,
    MpdSong,
};
use crate::PLUGIN_NAME;

use super::command::{PlaybackCommand, PlaybackError};
use super::report::PlaybackStateReport;
use super::subject_emitter::SubjectEmitter;

// ----- tuning constants -----

/// Initial delay before the first reconnect attempt.
const RECONNECT_INITIAL: Duration = Duration::from_millis(100);
/// Upper bound on the delay between reconnect attempts.
const RECONNECT_MAX: Duration = Duration::from_secs(10);
/// Maximum number of reconnect attempts before reporting
/// exhausted.
const RECONNECT_MAX_ATTEMPTS: u32 = 10;
/// Budget per [`MpdConnection::idle`] call on the idle task.
const IDLE_BUDGET: Duration = Duration::from_secs(30);
/// Subsystems the idle task subscribes to. Covers everything that
/// affects the fields reported in `PlaybackStateReport`.
const IDLE_SUBSYSTEMS: &[IdleSubsystem] = &[
    IdleSubsystem::Player,
    IdleSubsystem::Mixer,
    IdleSubsystem::Options,
    IdleSubsystem::Playlist,
];
/// Bounded capacity for the external-command channel. Values
/// smaller than ~8 would risk blocking the warden's
/// `course_correct`; larger than ~64 buys nothing for a human-
/// driven UI.
const COMMAND_CHANNEL_CAPACITY: usize = 32;
/// Bounded capacity for the idle-event channel. MPD idle events
/// arrive sparsely (seconds apart at most), so a small capacity
/// suffices.
const IDLE_CHANNEL_CAPACITY: usize = 8;

// ----- public-within-crate surface -----

/// Handle the warden retains for the life of a custody. Dropping
/// it is equivalent to calling [`SupervisorHandle::shutdown`]: the
/// `command_tx` half drops, the supervisor's `recv` returns
/// `None`, the run loop exits. Explicit `shutdown()` is preferred
/// so the caller can await completion.
pub(crate) struct SupervisorHandle {
    command_tx: mpsc::Sender<SupervisorMessage>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    task_handle: Option<JoinHandle<()>>,
}

impl SupervisorHandle {
    /// Dispatch a command. Returns once the supervisor has either
    /// executed the command, surfaced an ACK, reached the
    /// reconnection limit, or shut down.
    pub(crate) async fn command(
        &self,
        cmd: PlaybackCommand,
    ) -> Result<(), PlaybackError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(SupervisorMessage::Command {
                cmd,
                reply: reply_tx,
            })
            .await
            .map_err(|_| PlaybackError::Shutdown)?;
        match reply_rx.await {
            Ok(result) => result,
            Err(_) => Err(PlaybackError::Shutdown),
        }
    }

    /// Signal shutdown and wait for the supervisor's task to
    /// finish. Idempotent: calling a second time is a no-op.
    pub(crate) async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.task_handle.take() {
            let _ = h.await;
        }
    }
}

/// Open both connections, emit the initial state report, spawn
/// both tasks, return the handle.
///
/// Either connection failing to open, or the initial report
/// failing to be produced, aborts the whole spawn: no tasks are
/// spawned, no resources leak. The caller's `take_custody` impl
/// propagates the error.
///
/// Subject emission (track + album + `album_of` relation) is
/// piggy-backed on the initial state report: if MPD reports a
/// current song at spawn time, the [`SubjectEmitter`] is invoked
/// before the first custody-state report is acknowledged. This
/// gives the album-art respondent something to walk from the
/// moment playback becomes active. Subject-emission failures are
/// logged but not propagated (the state report is authoritative
/// for spawn success).
pub(crate) async fn spawn(
    endpoint: MpdEndpoint,
    timeouts: ConnectTimeouts,
    custody_handle: CustodyHandle,
    reporter: Arc<dyn CustodyStateReporter>,
    subject_emitter: SubjectEmitter,
) -> Result<SupervisorHandle, PlaybackError> {
    tracing::info!(
        plugin = PLUGIN_NAME,
        handle = %custody_handle.id,
        endpoint = %endpoint,
        "spawning playback supervisor"
    );

    let mut cmd_conn =
        MpdConnection::connect_with_timeouts(endpoint.clone(), timeouts)
            .await
            .map_err(classify_connect_error)?;
    let idle_conn =
        MpdConnection::connect_with_timeouts(endpoint.clone(), timeouts)
            .await
            .map_err(classify_connect_error)?;

    // Initial report: failure here means MPD is unusable, so bail
    // before spawning anything. The same query populates
    // `last_emitted_file` so the supervisor task starts with an
    // accurate "what has been announced already" state; a
    // subsequent idle wake on the same song will not re-announce
    // it.
    let mut last_emitted_file: Option<String> = None;
    emit_initial_report(
        &mut cmd_conn,
        &custody_handle,
        reporter.as_ref(),
        &subject_emitter,
        &mut last_emitted_file,
    )
    .await?;

    let (command_tx, command_rx) =
        mpsc::channel::<SupervisorMessage>(COMMAND_CHANNEL_CAPACITY);
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let (idle_tx, idle_rx) = mpsc::channel::<IdleEvent>(IDLE_CHANNEL_CAPACITY);

    let idle_endpoint = endpoint.clone();
    tokio::spawn(idle_task(idle_conn, idle_endpoint, timeouts, idle_tx));

    // Bundle the per-task state into a struct and hand it to the
    // supervisor task. The struct holds everything the run loop
    // needs across iterations (connection, endpoint for reconnect,
    // timeouts, custody handle, reporter, subject emitter,
    // last-emitted-file gate); the channels stay as independent
    // arguments to the run method because they have shorter
    // lifetimes tied to the task body.
    let task_state = SupervisorTask {
        cmd_conn,
        endpoint,
        timeouts,
        custody_handle,
        reporter,
        subject_emitter,
        last_emitted_file,
    };
    let task_handle =
        tokio::spawn(task_state.run(command_rx, shutdown_rx, idle_rx));

    Ok(SupervisorHandle {
        command_tx,
        shutdown_tx: Some(shutdown_tx),
        task_handle: Some(task_handle),
    })
}

// ----- internal types -----

/// Messages the main supervisor task consumes on its command
/// channel. Extending the enum (e.g. for health-probe queries) is
/// a source-only change; the channel signature is
/// `mpsc::Sender<SupervisorMessage>`.
enum SupervisorMessage {
    Command {
        cmd: PlaybackCommand,
        reply: oneshot::Sender<Result<(), PlaybackError>>,
    },
}

/// Events the idle task sends to the main supervisor.
enum IdleEvent {
    /// One or more subsystems changed. The supervisor emits a
    /// fresh state report in response.
    Changed(Vec<IdleSubsystem>),
    /// The idle task exhausted its reconnect attempts and has
    /// terminated. No further events will arrive on this channel.
    /// The supervisor logs and continues running command-only.
    Exhausted,
}

/// Exponential backoff state, per reconnection sequence.
///
/// `next_delay` doubles the delay each call up to [`RECONNECT_MAX`],
/// returning `None` after [`RECONNECT_MAX_ATTEMPTS`] have been
/// consumed.
struct BackoffState {
    attempt: u32,
    max_attempts: u32,
    initial: Duration,
    max: Duration,
}

impl BackoffState {
    fn new() -> Self {
        Self {
            attempt: 0,
            max_attempts: RECONNECT_MAX_ATTEMPTS,
            initial: RECONNECT_INITIAL,
            max: RECONNECT_MAX,
        }
    }

    fn next_delay(&mut self) -> Option<Duration> {
        if self.attempt >= self.max_attempts {
            return None;
        }
        let multiplier = 1u32 << self.attempt.min(16);
        let raw = self.initial.saturating_mul(multiplier);
        let delay = if raw > self.max { self.max } else { raw };
        self.attempt += 1;
        Some(delay)
    }

    fn attempts_used(&self) -> u32 {
        self.attempt
    }
}

// ----- main supervisor task -----

/// Per-task state for the main supervisor loop.
///
/// Bundles every piece of state the run loop carries across
/// iterations. Reduces what would otherwise be a ten-parameter
/// free function to a single `self` plus the three channel
/// receivers; the channel receivers stay as run-method
/// arguments because their lifetime is strictly bounded by the
/// task body, whereas every field here has to survive every
/// iteration of the select loop.
///
/// Constructed by [`spawn`] after the initial state report lands
/// successfully; the struct is moved into [`tokio::spawn`] as
/// part of the returned future.
struct SupervisorTask {
    cmd_conn: MpdConnection,
    endpoint: MpdEndpoint,
    timeouts: ConnectTimeouts,
    custody_handle: CustodyHandle,
    reporter: Arc<dyn CustodyStateReporter>,
    subject_emitter: SubjectEmitter,
    last_emitted_file: Option<String>,
}

impl SupervisorTask {
    /// The supervisor task body. Consumes `self` (the run is the
    /// whole life of the task) and the three channel receivers,
    /// and loops until shutdown or one of the channels closes.
    async fn run(
        mut self,
        mut command_rx: mpsc::Receiver<SupervisorMessage>,
        mut shutdown_rx: oneshot::Receiver<()>,
        mut idle_rx: mpsc::Receiver<IdleEvent>,
    ) {
        tracing::info!(
            plugin = PLUGIN_NAME,
            handle = %self.custody_handle.id,
            "playback supervisor task started"
        );

        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown_rx => {
                    tracing::info!(
                        plugin = PLUGIN_NAME,
                        handle = %self.custody_handle.id,
                        "supervisor received shutdown signal"
                    );
                    return;
                }
                msg = command_rx.recv() => {
                    match msg {
                        None => {
                            tracing::info!(
                                plugin = PLUGIN_NAME,
                                handle = %self.custody_handle.id,
                                "command channel closed; supervisor exiting"
                            );
                            return;
                        }
                        Some(SupervisorMessage::Command { cmd, reply }) => {
                            let result = handle_command(
                                cmd,
                                &mut self.cmd_conn,
                                &self.endpoint,
                                self.timeouts,
                            ).await;
                            let ok = result.is_ok();
                            let _ = reply.send(result);
                            if ok {
                                emit_best_effort_report(
                                    &mut self.cmd_conn,
                                    &self.custody_handle,
                                    self.reporter.as_ref(),
                                    &self.subject_emitter,
                                    &mut self.last_emitted_file,
                                ).await;
                            }
                        }
                    }
                }
                evt = idle_rx.recv() => {
                    match evt {
                        None | Some(IdleEvent::Exhausted) => {
                            tracing::warn!(
                                plugin = PLUGIN_NAME,
                                handle = %self.custody_handle.id,
                                "idle task terminated; continuing command-only"
                            );
                        }
                        Some(IdleEvent::Changed(changed)) => {
                            tracing::debug!(
                                plugin = PLUGIN_NAME,
                                handle = %self.custody_handle.id,
                                changed_count = changed.len(),
                                "idle wake"
                            );
                            emit_best_effort_report(
                                &mut self.cmd_conn,
                                &self.custody_handle,
                                self.reporter.as_ref(),
                                &self.subject_emitter,
                                &mut self.last_emitted_file,
                            ).await;
                        }
                    }
                }
            }
        }
    }
}

async fn handle_command(
    cmd: PlaybackCommand,
    cmd_conn: &mut MpdConnection,
    endpoint: &MpdEndpoint,
    timeouts: ConnectTimeouts,
) -> Result<(), PlaybackError> {
    // First attempt on the current connection.
    match dispatch_command(cmd.clone(), cmd_conn).await {
        Ok(()) => return Ok(()),
        Err(e) if !error_calls_for_reconnect(&e) => {
            return Err(classify_command_error(e));
        }
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "command hit transient error; reconnecting"
            );
        }
    }

    // Reconnect loop with backoff.
    let mut backoff = BackoffState::new();
    loop {
        let delay = match backoff.next_delay() {
            Some(d) => d,
            None => {
                return Err(PlaybackError::ConnectionExhausted {
                    attempts: backoff.attempts_used(),
                });
            }
        };
        tokio::time::sleep(delay).await;

        match MpdConnection::connect_with_timeouts(endpoint.clone(), timeouts)
            .await
        {
            Ok(new_conn) => {
                *cmd_conn = new_conn;
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    attempts = backoff.attempts_used(),
                    "command connection re-established"
                );
                break;
            }
            Err(e) if error_calls_for_reconnect(&e) => {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    attempt = backoff.attempts_used(),
                    "reconnect attempt failed"
                );
                continue;
            }
            Err(e) => {
                return Err(classify_command_error(e));
            }
        }
    }

    // Retry the command once on the fresh connection.
    match dispatch_command(cmd, cmd_conn).await {
        Ok(()) => Ok(()),
        Err(e) => Err(classify_command_error(e)),
    }
}

async fn dispatch_command(
    cmd: PlaybackCommand,
    cmd_conn: &mut MpdConnection,
) -> Result<(), MpdError> {
    match cmd {
        PlaybackCommand::Play => cmd_conn.play().await,
        PlaybackCommand::PlayPosition(p) => cmd_conn.play_position(p).await,
        PlaybackCommand::Pause(p) => cmd_conn.pause(p).await,
        PlaybackCommand::Stop => cmd_conn.stop().await,
        PlaybackCommand::Next => cmd_conn.next().await,
        PlaybackCommand::Previous => cmd_conn.previous().await,
        PlaybackCommand::Seek(d) => cmd_conn.seek(d).await,
        PlaybackCommand::SetVolume(v) => cmd_conn.set_volume(v).await,
    }
}

fn error_calls_for_reconnect(e: &MpdError) -> bool {
    matches!(e, MpdError::Transport(_) | MpdError::Timeout { .. })
}

fn classify_connect_error(e: MpdError) -> PlaybackError {
    match e {
        MpdError::Transport(_) | MpdError::Timeout { .. } => {
            PlaybackError::ConnectionExhausted { attempts: 1 }
        }
        MpdError::Protocol(_) | MpdError::Config(_) => {
            PlaybackError::Protocol(format!("{}", e))
        }
        MpdError::Ack { code, message, .. } => {
            PlaybackError::Ack { code, message }
        }
    }
}

fn classify_command_error(e: MpdError) -> PlaybackError {
    match e {
        MpdError::Ack { code, message, .. } => {
            PlaybackError::Ack { code, message }
        }
        MpdError::Transport(_) | MpdError::Timeout { .. } => {
            PlaybackError::ConnectionExhausted {
                attempts: RECONNECT_MAX_ATTEMPTS,
            }
        }
        MpdError::Protocol(_) | MpdError::Config(_) => {
            PlaybackError::Protocol(format!("{}", e))
        }
    }
}

// ----- state report emission -----

async fn emit_initial_report(
    cmd_conn: &mut MpdConnection,
    custody_handle: &CustodyHandle,
    reporter: &dyn CustodyStateReporter,
    subject_emitter: &SubjectEmitter,
    last_emitted_file: &mut Option<String>,
) -> Result<(), PlaybackError> {
    let status = cmd_conn.status().await.map_err(classify_command_error)?;
    let song = cmd_conn
        .current_song()
        .await
        .map_err(classify_command_error)?;
    // Clone before handing to the report projection so the
    // emitter can read the same song. MpdSong is cheap to clone
    // (a small fixed set of Option<String> plus a short String).
    let song_for_emitter = song.clone();
    let report = PlaybackStateReport::from_mpd(status, song);
    let payload = report.serialise().into_bytes();
    if let Err(e) = reporter
        .report(custody_handle, payload, HealthStatus::Healthy)
        .await
    {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            handle = %custody_handle.id,
            error = %e,
            "initial state report delivery failed; spawn proceeds anyway"
        );
    }
    maybe_emit_subjects(&song_for_emitter, subject_emitter, last_emitted_file)
        .await;
    Ok(())
}

async fn emit_best_effort_report(
    cmd_conn: &mut MpdConnection,
    custody_handle: &CustodyHandle,
    reporter: &dyn CustodyStateReporter,
    subject_emitter: &SubjectEmitter,
    last_emitted_file: &mut Option<String>,
) {
    let status = match cmd_conn.status().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                handle = %custody_handle.id,
                error = %e,
                "state report: status query failed"
            );
            return;
        }
    };
    let song = match cmd_conn.current_song().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                handle = %custody_handle.id,
                error = %e,
                "state report: currentsong query failed"
            );
            return;
        }
    };
    let song_for_emitter = song.clone();
    let report = PlaybackStateReport::from_mpd(status, song);
    let payload = report.serialise().into_bytes();
    if let Err(e) = reporter
        .report(custody_handle, payload, HealthStatus::Healthy)
        .await
    {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            handle = %custody_handle.id,
            error = %e,
            "state report delivery failed"
        );
    }
    maybe_emit_subjects(&song_for_emitter, subject_emitter, last_emitted_file)
        .await;
}

/// Invoke the [`SubjectEmitter`] for a song if (and only if) its
/// `file_path` differs from what was last emitted. A `None` song
/// (MPD reported no current song) is a no-op. The first call
/// with a given file path always emits; a subsequent call with
/// the same path is a no-op.
///
/// Rationale: subject/relation announcements are stable on
/// repeat, but they are not free; idle wakes can fire for mixer
/// and options changes that do not imply a song change, and
/// command dispatches re-emit a report each time. Gating on the
/// song URI keeps the steward's registry traffic proportional to
/// real song changes.
async fn maybe_emit_subjects(
    song: &Option<MpdSong>,
    emitter: &SubjectEmitter,
    last_emitted_file: &mut Option<String>,
) {
    let Some(song) = song.as_ref() else {
        return;
    };
    if song.file_path.is_empty() {
        return;
    }
    if last_emitted_file.as_deref() == Some(song.file_path.as_str()) {
        return;
    }
    emitter.emit_song(song).await;
    *last_emitted_file = Some(song.file_path.clone());
}

// ----- idle task -----

async fn idle_task(
    mut idle_conn: MpdConnection,
    endpoint: MpdEndpoint,
    timeouts: ConnectTimeouts,
    tx: mpsc::Sender<IdleEvent>,
) {
    tracing::info!(plugin = PLUGIN_NAME, "idle task started");
    loop {
        match idle_conn.idle(IDLE_SUBSYSTEMS, IDLE_BUDGET).await {
            Ok(changed) if changed.is_empty() => {
                // No-change OK (e.g. from a noidle from elsewhere).
                // Re-enter idle; no event to emit.
                continue;
            }
            Ok(changed) => {
                if tx.send(IdleEvent::Changed(changed)).await.is_err() {
                    tracing::info!(
                        plugin = PLUGIN_NAME,
                        "idle task: event receiver dropped, exiting"
                    );
                    return;
                }
            }
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "idle failed; will reconnect"
                );
                let mut backoff = BackoffState::new();
                let reconnected = loop {
                    let delay = match backoff.next_delay() {
                        Some(d) => d,
                        None => break None,
                    };
                    tokio::time::sleep(delay).await;
                    match MpdConnection::connect_with_timeouts(
                        endpoint.clone(),
                        timeouts,
                    )
                    .await
                    {
                        Ok(c) => break Some(c),
                        Err(err) => {
                            tracing::debug!(
                                plugin = PLUGIN_NAME,
                                error = %err,
                                attempt = backoff.attempts_used(),
                                "idle reconnect attempt failed"
                            );
                            continue;
                        }
                    }
                };
                match reconnected {
                    Some(c) => {
                        idle_conn = c;
                        tracing::info!(
                            plugin = PLUGIN_NAME,
                            "idle connection re-established"
                        );
                    }
                    None => {
                        let _ = tx.send(IdleEvent::Exhausted).await;
                        tracing::warn!(
                            plugin = PLUGIN_NAME,
                            "idle task exhausted reconnect attempts; exiting"
                        );
                        return;
                    }
                }
            }
        }
    }
}

// ----- tests -----

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use super::super::test_mock::{
        capturing_emitter, short_timeouts, spawn_mock_mpd, test_custody_handle,
        CapturingReporter, ConnBehaviour,
    };

    // ----- backoff unit tests -----

    #[test]
    fn backoff_delays_double_up_to_cap() {
        let mut b = BackoffState::new();
        assert_eq!(b.next_delay(), Some(Duration::from_millis(100)));
        assert_eq!(b.next_delay(), Some(Duration::from_millis(200)));
        assert_eq!(b.next_delay(), Some(Duration::from_millis(400)));
        assert_eq!(b.next_delay(), Some(Duration::from_millis(800)));
        assert_eq!(b.next_delay(), Some(Duration::from_millis(1600)));
        assert_eq!(b.next_delay(), Some(Duration::from_millis(3200)));
        assert_eq!(b.next_delay(), Some(Duration::from_millis(6400)));
        // Next raw would be 12800ms; capped to 10000ms.
        assert_eq!(b.next_delay(), Some(RECONNECT_MAX));
        assert_eq!(b.next_delay(), Some(RECONNECT_MAX));
        assert_eq!(b.next_delay(), Some(RECONNECT_MAX));
    }

    #[test]
    fn backoff_returns_none_after_max_attempts() {
        let mut b = BackoffState::new();
        for _ in 0..RECONNECT_MAX_ATTEMPTS {
            assert!(b.next_delay().is_some());
        }
        assert_eq!(b.next_delay(), None);
        assert_eq!(b.attempts_used(), RECONNECT_MAX_ATTEMPTS);
    }

    // ----- integration tests -----

    #[tokio::test]
    async fn spawn_succeeds_and_emits_initial_report() {
        let (endpoint, _mock) = spawn_mock_mpd(vec![
            ConnBehaviour::Standard,
            ConnBehaviour::HoldAfterWelcome,
        ])
        .await;

        let reporter = Arc::new(CapturingReporter::default());
        let reporter_dyn: Arc<dyn CustodyStateReporter> = reporter.clone();

        let handle = spawn(
            endpoint,
            short_timeouts(),
            test_custody_handle(),
            reporter_dyn,
            SubjectEmitter::null(),
        )
        .await
        .unwrap();

        assert_eq!(reporter.count(), 1);
        let payload = reporter.last_payload().unwrap();
        let text = String::from_utf8(payload).unwrap();
        assert!(
            text.contains("state = \"stopped\""),
            "expected stopped state in report: {text:?}"
        );

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn command_dispatch_returns_ok_and_emits_followup_report() {
        let (endpoint, _mock) = spawn_mock_mpd(vec![
            ConnBehaviour::Standard,
            ConnBehaviour::HoldAfterWelcome,
        ])
        .await;

        let reporter = Arc::new(CapturingReporter::default());
        let reporter_dyn: Arc<dyn CustodyStateReporter> = reporter.clone();

        let handle = spawn(
            endpoint,
            short_timeouts(),
            test_custody_handle(),
            reporter_dyn,
            SubjectEmitter::null(),
        )
        .await
        .unwrap();

        // Initial report is already in.
        assert_eq!(reporter.count(), 1);

        handle.command(PlaybackCommand::Play).await.unwrap();

        // After the command, wait briefly for the follow-up
        // report to land.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            reporter.count(),
            2,
            "expected initial + post-command report, got {}",
            reporter.count()
        );

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn command_ack_returns_playback_error_ack() {
        // Command-conn: 1 = status (initial report),
        //               2 = currentsong (initial report),
        //               3 = play -> ACK.
        let (endpoint, _mock) = spawn_mock_mpd(vec![
            ConnBehaviour::AckOnNth {
                nth: 3,
                code: 2,
                message: "Bad song index".to_string(),
            },
            ConnBehaviour::HoldAfterWelcome,
        ])
        .await;

        let reporter = Arc::new(CapturingReporter::default());
        let reporter_dyn: Arc<dyn CustodyStateReporter> = reporter.clone();

        let handle = spawn(
            endpoint,
            short_timeouts(),
            test_custody_handle(),
            reporter_dyn,
            SubjectEmitter::null(),
        )
        .await
        .unwrap();

        let err = handle.command(PlaybackCommand::Play).await.unwrap_err();
        match err {
            PlaybackError::Ack { code, message } => {
                assert_eq!(code, 2);
                assert_eq!(message, "Bad song index");
            }
            other => panic!("expected Ack, got {other:?}"),
        }

        // ACK does not kill the supervisor; shutdown still works.
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn command_reconnects_after_transient_drop() {
        // First command-conn: initial status (seq 1),
        //                     initial currentsong (seq 2),
        //                     play (seq 3) -> close connection.
        // Second command-conn (reconnect): Standard -> OK on play.
        // Idle conn: hold.
        let (endpoint, _mock) = spawn_mock_mpd(vec![
            ConnBehaviour::CloseOnNth { nth: 3 },
            ConnBehaviour::HoldAfterWelcome,
            ConnBehaviour::Standard,
        ])
        .await;

        let reporter = Arc::new(CapturingReporter::default());
        let reporter_dyn: Arc<dyn CustodyStateReporter> = reporter.clone();

        let handle = spawn(
            endpoint,
            short_timeouts(),
            test_custody_handle(),
            reporter_dyn,
            SubjectEmitter::null(),
        )
        .await
        .unwrap();

        // play fails the first time (conn closes), the supervisor
        // reconnects, retries on the new connection, succeeds.
        handle.command(PlaybackCommand::Play).await.unwrap();

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_completes_promptly() {
        let (endpoint, _mock) = spawn_mock_mpd(vec![
            ConnBehaviour::Standard,
            ConnBehaviour::HoldAfterWelcome,
        ])
        .await;

        let reporter = Arc::new(CapturingReporter::default());
        let reporter_dyn: Arc<dyn CustodyStateReporter> = reporter.clone();

        let handle = spawn(
            endpoint,
            short_timeouts(),
            test_custody_handle(),
            reporter_dyn,
            SubjectEmitter::null(),
        )
        .await
        .unwrap();

        let start = std::time::Instant::now();
        handle.shutdown().await;
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "shutdown took too long: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn idle_event_triggers_extra_state_report() {
        let (endpoint, _mock) = spawn_mock_mpd(vec![
            ConnBehaviour::Standard,
            ConnBehaviour::IdleOnceThenHold,
        ])
        .await;

        let reporter = Arc::new(CapturingReporter::default());
        let reporter_dyn: Arc<dyn CustodyStateReporter> = reporter.clone();

        let handle = spawn(
            endpoint,
            short_timeouts(),
            test_custody_handle(),
            reporter_dyn,
            SubjectEmitter::null(),
        )
        .await
        .unwrap();

        // The mock's idle connection responds with a single
        // `changed: player` event; the supervisor should emit a
        // follow-up report in response.
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert!(
            reporter.count() >= 2,
            "expected >= 2 reports (initial + idle-triggered), got {}",
            reporter.count()
        );

        handle.shutdown().await;
    }

    // ----- Phase 3.4: subject-emission integration tests -----

    #[tokio::test]
    async fn spawn_with_playing_song_emits_track_album_and_relation() {
        let (endpoint, _mock) = spawn_mock_mpd(vec![
            ConnBehaviour::StandardWithSong {
                file: "library/pf/thewall/01.flac".to_string(),
                title: "In the Flesh?".to_string(),
                artist: "Pink Floyd".to_string(),
                album: "The Wall".to_string(),
            },
            ConnBehaviour::HoldAfterWelcome,
        ])
        .await;

        let reporter = Arc::new(CapturingReporter::default());
        let reporter_dyn: Arc<dyn CustodyStateReporter> = reporter.clone();
        let (subjects, relations, emitter) = capturing_emitter();

        let handle = spawn(
            endpoint,
            short_timeouts(),
            test_custody_handle(),
            reporter_dyn,
            emitter,
        )
        .await
        .unwrap();

        assert_eq!(
            subjects.count(),
            2,
            "expected track + album announcements at spawn, got {}",
            subjects.count()
        );
        assert_eq!(
            relations.count(),
            1,
            "expected 1 album_of assertion at spawn"
        );

        let track = subjects.at(0).unwrap();
        assert_eq!(track.subject_type, "track");
        assert_eq!(track.addressings[0].scheme, "mpd-path");
        assert_eq!(track.addressings[0].value, "library/pf/thewall/01.flac");

        let album = subjects.at(1).unwrap();
        assert_eq!(album.subject_type, "album");
        assert_eq!(album.addressings[0].scheme, "mpd-album");
        assert_eq!(album.addressings[0].value, "Pink Floyd|The Wall");

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn spawn_with_empty_currentsong_emits_no_subjects() {
        // Standard mock returns empty `OK\n` for currentsong;
        // the supervisor should not invoke the emitter at all.
        let (endpoint, _mock) = spawn_mock_mpd(vec![
            ConnBehaviour::Standard,
            ConnBehaviour::HoldAfterWelcome,
        ])
        .await;

        let reporter = Arc::new(CapturingReporter::default());
        let reporter_dyn: Arc<dyn CustodyStateReporter> = reporter.clone();
        let (subjects, relations, emitter) = capturing_emitter();

        let handle = spawn(
            endpoint,
            short_timeouts(),
            test_custody_handle(),
            reporter_dyn,
            emitter,
        )
        .await
        .unwrap();

        // Initial state report still happens; subjects do not.
        assert_eq!(reporter.count(), 1);
        assert_eq!(subjects.count(), 0);
        assert_eq!(relations.count(), 0);

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn idle_event_on_same_song_does_not_reemit_subjects() {
        // cmd_conn = StandardWithSong so every currentsong
        // query returns the same populated song.
        // idle_conn = IdleOnceThenHold so the first idle call
        // receives `changed: player`, triggering a follow-up
        // state report and subject-emission gate.
        let (endpoint, _mock) = spawn_mock_mpd(vec![
            ConnBehaviour::StandardWithSong {
                file: "a.flac".to_string(),
                title: "Track One".to_string(),
                artist: "Artist".to_string(),
                album: "Album".to_string(),
            },
            ConnBehaviour::IdleOnceThenHold,
        ])
        .await;

        let reporter = Arc::new(CapturingReporter::default());
        let reporter_dyn: Arc<dyn CustodyStateReporter> = reporter.clone();
        let (subjects, relations, emitter) = capturing_emitter();

        let handle = spawn(
            endpoint,
            short_timeouts(),
            test_custody_handle(),
            reporter_dyn,
            emitter,
        )
        .await
        .unwrap();

        // Wait for the idle event + follow-up report to land.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Two state reports (initial + idle-triggered), but
        // only the initial emission because the song URI has
        // not changed since the first emit.
        assert!(
            reporter.count() >= 2,
            "expected >= 2 reports, got {}",
            reporter.count()
        );
        assert_eq!(
            subjects.count(),
            2,
            "expected only initial track + album (2), got {}",
            subjects.count()
        );
        assert_eq!(
            relations.count(),
            1,
            "expected only initial album_of (1), got {}",
            relations.count()
        );

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn command_on_same_song_does_not_reemit_subjects() {
        // cmd_conn = StandardWithSong so every currentsong
        // query returns the same populated song. Issuing a
        // command triggers a follow-up state report but not
        // a follow-up subject emission.
        let (endpoint, _mock) = spawn_mock_mpd(vec![
            ConnBehaviour::StandardWithSong {
                file: "a.flac".to_string(),
                title: "T".to_string(),
                artist: "A".to_string(),
                album: "B".to_string(),
            },
            ConnBehaviour::HoldAfterWelcome,
        ])
        .await;

        let reporter = Arc::new(CapturingReporter::default());
        let reporter_dyn: Arc<dyn CustodyStateReporter> = reporter.clone();
        let (subjects, relations, emitter) = capturing_emitter();

        let handle = spawn(
            endpoint,
            short_timeouts(),
            test_custody_handle(),
            reporter_dyn,
            emitter,
        )
        .await
        .unwrap();

        // Initial emission already happened.
        assert_eq!(subjects.count(), 2);
        assert_eq!(relations.count(), 1);

        handle.command(PlaybackCommand::Play).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // State reports: initial + post-command = 2. Subjects:
        // unchanged, because the song did not change.
        assert_eq!(reporter.count(), 2);
        assert_eq!(subjects.count(), 2);
        assert_eq!(relations.count(), 1);

        handle.shutdown().await;
    }
}
