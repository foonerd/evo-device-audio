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

use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::AudioProtocolSettings;
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
///
/// MPD's `idle` protocol blocks the connection until a
/// subsystem change is observed. The `idle()` API takes a
/// budget so tests can drive deterministic short-timeout
/// paths; in production on a quiet MPD, idle should block
/// for as long as MPD is willing to hold the connection
/// open (typically indefinitely). Sending another `idle`
/// command on a connection still in idle state is a
/// protocol violation that MPD closes the connection on,
/// so a short client-side timeout that re-issues without
/// first sending `noidle` is wrong; the budget therefore
/// has to be effectively-forever so the timeout path is
/// never reached in steady state. 1 day picks a value
/// large enough that healthy MPD never times out
/// client-side, while still bounded for testability /
/// catastrophic-stuck-state recovery. Real connection
/// failures (TCP close, protocol error) surface as
/// `MpdError::Transport` / `Protocol` and trigger the
/// actor's reconnect path.
const IDLE_BUDGET: Duration = Duration::from_secs(86_400);
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
        SupervisorCommandSender {
            command_tx: self.command_tx.clone(),
        }
        .command(cmd)
        .await
    }

    /// Return a cloneable command-side view of this handle.
    /// The returned [`SupervisorCommandSender`] can dispatch
    /// commands to the same supervisor but cannot initiate
    /// shutdown — its lifecycle is decoupled from the
    /// owning `SupervisorHandle`. Spawned tasks that need
    /// to dispatch playback commands (e.g. the
    /// mixer-transition envelope subscriber) clone one of
    /// these at custody-acceptance time + drop it at
    /// custody-release.
    pub(crate) fn command_sender(&self) -> SupervisorCommandSender {
        SupervisorCommandSender {
            command_tx: self.command_tx.clone(),
        }
    }

    /// Read the live playback-state report. See
    /// [`SupervisorCommandSender::query_state`] for semantics.
    pub(crate) async fn query_state(
        &self,
    ) -> Result<PlaybackStateReport, PlaybackError> {
        SupervisorCommandSender {
            command_tx: self.command_tx.clone(),
        }
        .query_state()
        .await
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

/// Cloneable command-side view of a [`SupervisorHandle`].
///
/// Holds only the `command_tx` half; can dispatch commands
/// but cannot initiate shutdown. Spawned tasks that need to
/// dispatch playback commands to the active custody hold
/// one of these via a plugin-owned watch / mutex cell so
/// they can react to custody lifecycle transitions without
/// owning the supervisor itself.
#[derive(Clone)]
pub(crate) struct SupervisorCommandSender {
    command_tx: mpsc::Sender<SupervisorMessage>,
}

impl SupervisorCommandSender {
    /// Dispatch a command. Semantically identical to
    /// [`SupervisorHandle::command`] — they share the
    /// underlying mpsc + oneshot machinery. Returns once
    /// the supervisor has either executed the command,
    /// surfaced an ACK, reached the reconnection limit, or
    /// shut down.
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

    /// Read the live playback-state report. Drives a fresh
    /// MPD `status` + `currentsong` round-trip in the
    /// supervisor task and returns the resulting
    /// [`PlaybackStateReport`] including the supervisor's
    /// task-local `muted` flag. Used by the warden's
    /// `get_now_playing` read verb to satisfy first-render
    /// requests from freshly-connected UI clients.
    pub(crate) async fn query_state(
        &self,
    ) -> Result<PlaybackStateReport, PlaybackError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(SupervisorMessage::QueryState { reply: reply_tx })
            .await
            .map_err(|_| PlaybackError::Shutdown)?;
        match reply_rx.await {
            Ok(result) => result,
            Err(_) => Err(PlaybackError::Shutdown),
        }
    }

    /// Cycle MPD output 0 (disable + enable on the MPD wire
    /// protocol). The asound-watcher task dispatches this
    /// whenever `/etc/asound.d/` composition changes so MPD
    /// drops and reopens its `snd_pcm_t` handle, re-resolving
    /// `pcm.evo` against the post-change drop-in stack. The
    /// supervisor's command connection serialises this with
    /// every other command; ordinary playback verbs queued
    /// before / after a cycle remain correctly ordered.
    pub(crate) async fn cycle_output(&self) -> Result<(), PlaybackError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.command_tx
            .send(SupervisorMessage::CycleOutput { reply: reply_tx })
            .await
            .map_err(|_| PlaybackError::Shutdown)?;
        match reply_rx.await {
            Ok(result) => result,
            Err(_) => Err(PlaybackError::Shutdown),
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
    audio_protocol_settings_rx: watch::Receiver<AudioProtocolSettings>,
    music_directory: Option<std::path::PathBuf>,
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

    // Apply the operator's MPD-protocol settings to the freshly
    // opened command connection before the initial report. The
    // settings are persisted across MPD restarts within a single
    // server session but reset to defaults when MPD itself
    // restarts; applying on every supervisor spawn restores them
    // deterministically. Best-effort: if MPD refuses the verb
    // (e.g. older server without `crossfade` support — unlikely
    // on a modern build), the apply is logged and skipped, the
    // session still proceeds.
    let initial_protocol_settings = *audio_protocol_settings_rx.borrow();
    apply_audio_protocol_settings(&mut cmd_conn, initial_protocol_settings)
        .await;

    // Initial report: failure here means MPD is unusable, so bail
    // before spawning anything. The same query populates
    // `file_tracker.emitted_file` so the supervisor task starts
    // with an accurate "what has been announced already" state;
    // a subsequent idle wake on the same song will not
    // re-announce it. `file_tracker.probed_file` is the
    // session's file-side source-format probe cache: a non-empty
    // value means "we already probed THIS file; another emit
    // cycle on the same file is zero I/O." A track transition
    // replaces both values AND drives an `update_source_format`
    // so the `stream_format` envelope's `source` field never
    // carries the prior track's shape into the new track.
    let mut file_tracker = FileEmissionTracker::default();
    // Sessions start unmuted with a 50% pre-mute fallback. A
    // first `set_mute(true)` will capture the live MPD volume
    // before silencing; `set_mute(false)` ahead of any prior
    // mute restores to this fallback rather than to an
    // operator-confusing 0.
    let initial_muted: bool = false;
    let initial_pre_mute_volume: u8 = 50;
    emit_initial_report(
        &mut cmd_conn,
        &custody_handle,
        reporter.as_ref(),
        &subject_emitter,
        &mut file_tracker,
        music_directory.as_deref(),
        initial_muted,
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
        file_tracker,
        music_directory,
        muted: initial_muted,
        pre_mute_volume: initial_pre_mute_volume,
    };
    let task_handle = tokio::spawn(task_state.run(
        command_rx,
        shutdown_rx,
        idle_rx,
        audio_protocol_settings_rx,
    ));

    Ok(SupervisorHandle {
        command_tx,
        shutdown_tx: Some(shutdown_tx),
        task_handle: Some(task_handle),
    })
}

/// Apply the operator's MPD-protocol settings to a live command
/// connection. Both verbs are best-effort — an ACK from MPD is
/// surfaced as a warning and the session continues; this matches
/// the engineering bar's "no silent failure" rule (the warning
/// is observable) without making MPD-version-skew a session-
/// abort condition.
async fn apply_audio_protocol_settings(
    cmd_conn: &mut MpdConnection,
    settings: AudioProtocolSettings,
) {
    if let Err(e) = cmd_conn.set_crossfade(settings.crossfade_seconds).await {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            crossfade_seconds = settings.crossfade_seconds,
            error = %e,
            "set_crossfade rejected; mpd's protocol-layer crossfade \
             not applied for this session"
        );
    }
    // `gapless = true` engages MPD `single 0` (continue through
    // the queue); `gapless = false` engages `single 1` (stop
    // after each track).
    let single = !settings.gapless;
    if let Err(e) = cmd_conn.set_single(single).await {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            single,
            error = %e,
            "set_single rejected; mpd's single-mode not applied \
             for this session"
        );
    }
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
    /// One-shot query for the current playback state report.
    /// Fired by the warden's `get_now_playing` read verb. The
    /// supervisor builds a fresh `PlaybackStateReport` from a
    /// live MPD `status` + `currentsong` round-trip (so the
    /// caller observes the live state, not a cached snapshot)
    /// and replies through the oneshot. The current `muted`
    /// state from the supervisor's task-local mute tracking is
    /// folded in as the report's `muted` field.
    QueryState {
        reply: oneshot::Sender<Result<PlaybackStateReport, PlaybackError>>,
    },
    /// One-shot directive to cycle MPD output 0 (disable +
    /// enable on the MPD wire protocol) so MPD drops and
    /// reopens its `snd_pcm_t` handle. Fired by the asound
    /// watcher when `/etc/asound.d/` composition changes
    /// (multi-room drop-in install / remove, operator-options
    /// rewrite). The cycle causes the next `snd_pcm_open` to
    /// re-resolve `pcm.evo` against the post-change drop-in
    /// stack without a `systemctl restart mpd`.
    CycleOutput {
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
/// Per-session emission and probe tracker.
///
/// Bundles the two `Option<String>` cross-call states that the
/// emit functions read and update in lockstep: which MPD `file:`
/// last triggered a `now_playing` envelope, and which `file:` the
/// source-format probe last ran against. Pairing them here
/// retires arg-count complexity at every call site and keeps the
/// rule that the two values move together within one supervisor
/// session.
#[derive(Default)]
struct FileEmissionTracker {
    /// Last MPD `file:` value that triggered a `now_playing`
    /// envelope emission. Gates duplicate-track suppression so
    /// the emitter does not flood the subject with the same
    /// envelope on every status poll.
    emitted_file: Option<String>,
    /// Last MPD `file:` value the source-format probe ran
    /// against. Held so the `stream_format` envelope's `source`
    /// field stays coherent across track transitions inside one
    /// supervisor session.
    probed_file: Option<String>,
}

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
    /// Per-session emission and probe tracker; bundled so the
    /// emit functions take one cross-call state handle instead
    /// of two correlated `Option<String>` fields that are
    /// always read and updated together.
    file_tracker: FileEmissionTracker,
    /// MPD's music_directory at supervisor-spawn time. Used to
    /// resolve the absolute filesystem path of the current
    /// song for the head-bytes probe. `None` when the
    /// plugin's load context could not resolve mpd.conf's
    /// music_directory directive; the probe path then
    /// publishes None on every track change, which clears the
    /// source field rather than carrying stale data forward.
    music_directory: Option<std::path::PathBuf>,
    /// Operator-toggled mute state. MPD has no native mute
    /// primitive — mute is synthesised as `setvol 0` with the
    /// pre-mute volume captured for restore on unmute. Defaults to
    /// false (not muted) on session start.
    muted: bool,
    /// Captured volume to restore on `set_mute(false)`. Updated
    /// every time the warden issues `set_mute(true)`: the actor
    /// reads MPD's current volume via `status()` before sending
    /// `setvol 0`. Defaults to 50 (the safe-fallback used when no
    /// pre-mute value was captured, e.g. unmute called on a
    /// session that started muted).
    pre_mute_volume: u8,
}

impl SupervisorTask {
    /// The supervisor task body. Consumes `self` (the run is the
    /// whole life of the task) and the four channel receivers,
    /// and loops until shutdown or one of the channels closes.
    /// The watch receiver carries operator-toggled MPD-protocol
    /// settings (crossfade + single mode); on change, the task
    /// applies them to the command connection through the same
    /// canonical dispatch path that handles steward-issued
    /// commands.
    async fn run(
        mut self,
        mut command_rx: mpsc::Receiver<SupervisorMessage>,
        mut shutdown_rx: oneshot::Receiver<()>,
        mut idle_rx: mpsc::Receiver<IdleEvent>,
        mut audio_protocol_settings_rx: watch::Receiver<AudioProtocolSettings>,
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
                                &mut self.muted,
                                &mut self.pre_mute_volume,
                            ).await;
                            let ok = result.is_ok();
                            let _ = reply.send(result);
                            if ok {
                                emit_best_effort_report(
                                    &mut self.cmd_conn,
                                    &self.custody_handle,
                                    self.reporter.as_ref(),
                                    &self.subject_emitter,
                                    &mut self.file_tracker,
                                    self.music_directory.as_deref(),
                                    self.muted,
                                ).await;
                            }
                        }
                        Some(SupervisorMessage::CycleOutput { reply }) => {
                            // Disable + enable MPD output 0 on
                            // the command connection. MPD's
                            // disableoutput drops the underlying
                            // snd_pcm_t; enableoutput reopens it
                            // by re-running snd_pcm_open against
                            // the alias `evo`, which ALSA
                            // resolves through whatever
                            // composition currently lives under
                            // /etc/asound.d/. This is the
                            // canonical primitive for adapting
                            // MPD to a runtime asound change
                            // (multi-room drop-in install /
                            // remove, operator-options rewrite)
                            // without a systemd bounce.
                            // handle_cycle_output wraps the
                            // disable/enable pair in the same
                            // try-then-reconnect-then-retry
                            // contract handle_command uses, so a
                            // wedged cmd_conn at the moment the
                            // asound watcher fires recovers via
                            // reconnect rather than synthesising
                            // an exhaustion error.
                            let result = handle_cycle_output(
                                &mut self.cmd_conn,
                                &self.endpoint,
                                self.timeouts,
                            ).await;
                            let _ = reply.send(result);
                        }
                        Some(SupervisorMessage::QueryState { reply }) => {
                            // Live MPD status + currentsong round-trip;
                            // build a PlaybackStateReport carrying the
                            // supervisor's task-local mute flag.
                            // handle_query wraps the round-trip in
                            // the same try-then-reconnect-then-retry
                            // contract handle_command uses, so a
                            // first-call wedged cmd_conn recovers via
                            // reconnect — the fresh-custody first-render
                            // contract for get_now_playing depends on
                            // this. No emit_best_effort_report
                            // side-channel: a read MUST NOT publish to
                            // the now_playing subject (otherwise every
                            // read fires a delta the consumer already
                            // has via the read response, doubling
                            // traffic without adding signal).
                            let result = handle_query(
                                &mut self.cmd_conn,
                                &self.endpoint,
                                self.timeouts,
                                self.muted,
                            ).await;
                            let _ = reply.send(result);
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
                                &mut self.file_tracker,
                                self.music_directory.as_deref(),
                                self.muted,
                            ).await;
                        }
                    }
                }
                changed = audio_protocol_settings_rx.changed() => {
                    match changed {
                        Ok(()) => {
                            let settings = *audio_protocol_settings_rx.borrow();
                            tracing::debug!(
                                plugin = PLUGIN_NAME,
                                handle = %self.custody_handle.id,
                                crossfade_seconds = settings.crossfade_seconds,
                                gapless = settings.gapless,
                                "operator changed mpd-protocol settings; \
                                 applying to live session"
                            );
                            apply_audio_protocol_settings(
                                &mut self.cmd_conn,
                                settings,
                            ).await;
                        }
                        Err(_) => {
                            tracing::debug!(
                                plugin = PLUGIN_NAME,
                                handle = %self.custody_handle.id,
                                "audio_protocol_settings watch sender dropped; \
                                 stopping settings-following arm"
                            );
                            // The watch sender on the plugin side has
                            // dropped, which only happens when the
                            // plugin itself is being torn down. The
                            // supervisor will receive shutdown via the
                            // dedicated channel shortly; stay in the
                            // select loop on the other arms.
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
    muted: &mut bool,
    pre_mute_volume: &mut u8,
) -> Result<(), PlaybackError> {
    // First attempt on the current connection.
    match dispatch_command(cmd.clone(), cmd_conn, muted, pre_mute_volume).await
    {
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

    // Reconnect with backoff; bail with a real attempt count on
    // exhaustion (NOT the synthetic-10 path classify_command_error
    // produces on a single transport error).
    reconnect_cmd_conn(cmd_conn, endpoint, timeouts).await?;

    // Retry the command once on the fresh connection.
    match dispatch_command(cmd, cmd_conn, muted, pre_mute_volume).await {
        Ok(()) => Ok(()),
        Err(e) => Err(classify_command_error(e)),
    }
}

/// Read live playback state (status + currentsong) with the same
/// try-then-reconnect-then-retry contract `handle_command` uses
/// for write verbs. The QueryState message path goes through this
/// so a wedged `cmd_conn` (fresh-spawn race, prior MPD downtime
/// that left the connection in transport-error state) recovers
/// the same way a write verb's first attempt recovers. Without
/// this wrap, the first transport error short-circuits to
/// `classify_command_error -> ConnectionExhausted { attempts: 10 }`
/// — a synthetic exhaustion that never tried the real reconnect
/// machinery, the symptom the operator-UI first-render-read kept
/// hitting on fresh custodies.
async fn handle_query(
    cmd_conn: &mut MpdConnection,
    endpoint: &MpdEndpoint,
    timeouts: ConnectTimeouts,
    muted: bool,
) -> Result<PlaybackStateReport, PlaybackError> {
    // First attempt on the current connection.
    match do_query(cmd_conn, muted).await {
        Ok(report) => return Ok(report),
        Err(e) if !error_calls_for_reconnect(&e) => {
            return Err(classify_command_error(e));
        }
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "query hit transient error; reconnecting"
            );
        }
    }

    reconnect_cmd_conn(cmd_conn, endpoint, timeouts).await?;

    // Retry the query once on the fresh connection.
    do_query(cmd_conn, muted)
        .await
        .map_err(classify_command_error)
}

async fn do_query(
    cmd_conn: &mut MpdConnection,
    muted: bool,
) -> Result<PlaybackStateReport, MpdError> {
    let status = cmd_conn.status().await?;
    let song = cmd_conn.current_song().await?;
    Ok(PlaybackStateReport::from_mpd(status, song, muted))
}

/// Cycle MPD output 0 (`disableoutput` + `enableoutput`) with the
/// same try-then-reconnect-then-retry contract. The asound
/// watcher dispatches CycleOutput on every `/etc/asound.d/`
/// composition change; a wedged `cmd_conn` at that moment must
/// recover via reconnect rather than synthesise an exhaustion
/// error and leave MPD on a stale handle.
async fn handle_cycle_output(
    cmd_conn: &mut MpdConnection,
    endpoint: &MpdEndpoint,
    timeouts: ConnectTimeouts,
) -> Result<(), PlaybackError> {
    match do_cycle_output(cmd_conn).await {
        Ok(()) => return Ok(()),
        Err(e) if !error_calls_for_reconnect(&e) => {
            return Err(classify_command_error(e));
        }
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "output-cycle hit transient error; reconnecting"
            );
        }
    }

    reconnect_cmd_conn(cmd_conn, endpoint, timeouts).await?;

    do_cycle_output(cmd_conn)
        .await
        .map_err(classify_command_error)
}

async fn do_cycle_output(cmd_conn: &mut MpdConnection) -> Result<(), MpdError> {
    cmd_conn.disable_output(0).await?;
    cmd_conn.enable_output(0).await
}

/// Reconnect-loop-with-backoff helper. Replaces `*cmd_conn` with
/// a freshly-opened connection on success; returns
/// `PlaybackError::ConnectionExhausted` with the REAL attempt
/// count when backoff exhausts. Shared by every code path that
/// recovers from a wedged command connection
/// (`handle_command`, `handle_query`, `handle_cycle_output`).
async fn reconnect_cmd_conn(
    cmd_conn: &mut MpdConnection,
    endpoint: &MpdEndpoint,
    timeouts: ConnectTimeouts,
) -> Result<(), PlaybackError> {
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
                return Ok(());
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
}

async fn dispatch_command(
    cmd: PlaybackCommand,
    cmd_conn: &mut MpdConnection,
    muted: &mut bool,
    pre_mute_volume: &mut u8,
) -> Result<(), MpdError> {
    match cmd {
        PlaybackCommand::Play => cmd_conn.play().await,
        PlaybackCommand::PlayPosition(p) => cmd_conn.play_position(p).await,
        PlaybackCommand::Pause(p) => cmd_conn.pause(p).await,
        PlaybackCommand::Stop => cmd_conn.stop().await,
        PlaybackCommand::Next => cmd_conn.next().await,
        PlaybackCommand::Previous => cmd_conn.previous().await,
        PlaybackCommand::Seek(d) => cmd_conn.seek(d).await,
        PlaybackCommand::SeekRelative(delta_ms) => {
            cmd_conn.seek_relative(delta_ms).await
        }
        PlaybackCommand::SetVolume(v) => {
            // Explicit non-zero volume clears mute (operator
            // moved the slider; that is an unmute). Volume 0
            // is treated as "operator chose silence" — distinct
            // from set_mute(true); leaves mute state untouched
            // so set_mute(false) still restores the captured
            // pre-mute volume rather than the zero the operator
            // just set.
            if v > 0 {
                *muted = false;
            }
            cmd_conn.set_volume(v).await
        }
        PlaybackCommand::SetMute(true) => {
            // Capture current MPD volume before muting so
            // set_mute(false) restores. MPD reports volume = -1
            // (mapped to None) when no mixer is configured; in
            // that case we keep the existing pre_mute_volume
            // (defaults to 50) so unmute still produces audible
            // output. Already-muted is idempotent — re-issuing
            // set_mute(true) over an already-zero volume keeps
            // the previously captured pre-mute value.
            if !*muted {
                let status = cmd_conn.status().await?;
                if let Some(current) = status.volume {
                    if current > 0 {
                        *pre_mute_volume = current;
                    }
                }
            }
            *muted = true;
            cmd_conn.set_volume(0).await
        }
        PlaybackCommand::SetMute(false) => {
            // Restore the captured pre-mute volume. Defaults to
            // 50 when no value was captured (e.g. unmute from a
            // session that started muted). Volume clamping is
            // handled by `MpdConnection::set_volume`.
            *muted = false;
            cmd_conn.set_volume(*pre_mute_volume).await
        }
        PlaybackCommand::SetRepeat(enabled) => {
            cmd_conn.set_repeat(enabled).await
        }
        PlaybackCommand::SetShuffle(enabled) => {
            // Operator-facing name `shuffle` maps to MPD's
            // `random` mode primitive.
            cmd_conn.set_random(enabled).await
        }
        PlaybackCommand::SetSingle(enabled) => {
            cmd_conn.set_single(enabled).await
        }
        PlaybackCommand::SetConsume(enabled) => {
            cmd_conn.set_consume(enabled).await
        }
        PlaybackCommand::LoadAndPlay(path) => {
            // Replace queue with the single supplied path
            // and start playback. The three commands run on
            // the same connection sequentially; if any
            // step ACKs, the error short-circuits and the
            // queue is left in an intermediate state — the
            // operator-readable diagnostic carries the
            // failing step's MPD error.
            cmd_conn.clear().await?;
            cmd_conn.add(&path).await?;
            cmd_conn.play().await
        }
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
    file_tracker: &mut FileEmissionTracker,
    music_directory: Option<&std::path::Path>,
    muted: bool,
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
    let report = PlaybackStateReport::from_mpd(status, song, muted);
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
    maybe_emit_subjects(
        &song_for_emitter,
        subject_emitter,
        &mut file_tracker.emitted_file,
    )
    .await;
    let source_codec = song_for_emitter
        .as_ref()
        .and_then(|s| s.codec_name.as_deref());
    subject_emitter.update_source_codec(source_codec).await;
    // Drive the source-format probe at the same call site as
    // update_source_codec so both halves of the source-side
    // envelope shape stay coherent on every track change. A
    // prior Some(format) must be replaced by either the new
    // track's parser result or `None`; the helper handles both
    // and the publish path republishes the merged envelope.
    super::ambient_observer::maybe_probe_source_format(
        song_for_emitter.as_ref(),
        music_directory,
        &mut file_tracker.probed_file,
        subject_emitter,
    )
    .await;
    subject_emitter.update_now_playing(&report).await;
    Ok(())
}

async fn emit_best_effort_report(
    cmd_conn: &mut MpdConnection,
    custody_handle: &CustodyHandle,
    reporter: &dyn CustodyStateReporter,
    subject_emitter: &SubjectEmitter,
    file_tracker: &mut FileEmissionTracker,
    music_directory: Option<&std::path::Path>,
    muted: bool,
) {
    let status = match cmd_conn.status().await {
        Ok(s) => s,
        Err(e) => {
            // Transient transport errors (broken pipe after MPD
            // restart, read timeout) are expected during the
            // recovery window — the next command-path call
            // triggers the proper reconnect via
            // error_calls_for_reconnect. Logging at WARN would
            // pollute the journal every time MPD restarts (which
            // the framework triggers on every route change).
            // Demote to DEBUG for those; keep WARN for
            // protocol-level surprises.
            if error_calls_for_reconnect(&e) {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    handle = %custody_handle.id,
                    error = %e,
                    "state report: status query failed transiently; \
                     next command will reconnect"
                );
            } else {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    handle = %custody_handle.id,
                    error = %e,
                    "state report: status query failed"
                );
            }
            return;
        }
    };
    let song = match cmd_conn.current_song().await {
        Ok(s) => s,
        Err(e) => {
            if error_calls_for_reconnect(&e) {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    handle = %custody_handle.id,
                    error = %e,
                    "state report: currentsong query failed \
                     transiently; next command will reconnect"
                );
            } else {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    handle = %custody_handle.id,
                    error = %e,
                    "state report: currentsong query failed"
                );
            }
            return;
        }
    };
    let song_for_emitter = song.clone();
    let report = PlaybackStateReport::from_mpd(status, song, muted);
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
    maybe_emit_subjects(
        &song_for_emitter,
        subject_emitter,
        &mut file_tracker.emitted_file,
    )
    .await;
    let source_codec = song_for_emitter
        .as_ref()
        .and_then(|s| s.codec_name.as_deref());
    subject_emitter.update_source_codec(source_codec).await;
    // Same probe-and-publish call as emit_initial_report — the
    // best-effort cycle MUST drive update_source_format on
    // every track change for the stream_format envelope's
    // source field to stay coherent.
    super::ambient_observer::maybe_probe_source_format(
        song_for_emitter.as_ref(),
        music_directory,
        &mut file_tracker.probed_file,
        subject_emitter,
    )
    .await;
    subject_emitter.update_now_playing(&report).await;
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

    /// Test helper: default-valued audio_protocol_settings_rx for
    /// spawn sites that don't exercise the operator-toggle path.
    /// Returns a receiver bound to a sender that lives in a leaked
    /// channel — fine for tests, the sender ownership doesn't
    /// matter because the receiver only reads the default value.
    fn null_protocol_settings_rx() -> watch::Receiver<AudioProtocolSettings> {
        let (tx, rx) =
            watch::channel(AudioProtocolSettings::audiophile_default());
        // Leak the sender so the receiver stays usable. Tests
        // don't poll for sender-drop here.
        Box::leak(Box::new(tx));
        rx
    }

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
            null_protocol_settings_rx(),
            None,
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
            null_protocol_settings_rx(),
            None,
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
        // Command-conn: 1 = crossfade (apply_audio_protocol_settings),
        //               2 = single    (apply_audio_protocol_settings),
        //               3 = status    (initial report),
        //               4 = currentsong (initial report),
        //               5 = play -> ACK.
        let (endpoint, _mock) = spawn_mock_mpd(vec![
            ConnBehaviour::AckOnNth {
                nth: 5,
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
            null_protocol_settings_rx(),
            None,
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
        // First command-conn:  1 = crossfade (apply_audio_protocol_settings),
        //                      2 = single    (apply_audio_protocol_settings),
        //                      3 = status    (initial report),
        //                      4 = currentsong (initial report),
        //                      5 = play -> close connection.
        // Second command-conn (reconnect): Standard -> OK on play.
        // Idle conn: hold.
        let (endpoint, _mock) = spawn_mock_mpd(vec![
            ConnBehaviour::CloseOnNth { nth: 5 },
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
            null_protocol_settings_rx(),
            None,
        )
        .await
        .unwrap();

        // play fails the first time (conn closes), the supervisor
        // reconnects, retries on the new connection, succeeds.
        handle.command(PlaybackCommand::Play).await.unwrap();

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn query_state_reconnects_after_transient_drop() {
        // Regression: get_now_playing's first-render contract
        // depends on the QueryState message path recovering from
        // a wedged cmd_conn via reconnect. Before the
        // handle_query refactor the QueryState branch called
        // cmd_conn.status() directly; a single transport error
        // short-circuited through classify_command_error to
        // ConnectionExhausted{attempts: RECONNECT_MAX_ATTEMPTS}
        // synthetically, never trying the reconnect machinery.
        //
        // Connection budget mirrors
        // command_reconnects_after_transient_drop:
        //   First command-conn:
        //     1 = crossfade (apply_audio_protocol_settings),
        //     2 = single    (apply_audio_protocol_settings),
        //     3 = status    (initial report),
        //     4 = currentsong (initial report),
        //     5 = status (query_state first attempt) -> close.
        //   Second command-conn (reconnect): Standard -> OK on
        //     status + currentsong retry.
        //   Idle conn: hold.
        let (endpoint, _mock) = spawn_mock_mpd(vec![
            ConnBehaviour::CloseOnNth { nth: 5 },
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
            null_protocol_settings_rx(),
            None,
        )
        .await
        .unwrap();

        // First query_state call hits the close-on-nth, the
        // supervisor reconnects, retries on the fresh connection,
        // returns a real PlaybackStateReport — the very first
        // interaction on a fresh custody succeeds.
        let report = handle.query_state().await.unwrap();
        // Standard mock's status returns "state: stop" so the
        // report's state is Stopped; the test asserts the
        // round-trip COMPLETED rather than a specific song
        // (the mock has no current song).
        assert_eq!(report.state, crate::mpd::PlayState::Stopped);

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
            null_protocol_settings_rx(),
            None,
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
            null_protocol_settings_rx(),
            None,
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

    // ----- subject-emission integration tests -----

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
            null_protocol_settings_rx(),
            None,
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
            null_protocol_settings_rx(),
            None,
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
            null_protocol_settings_rx(),
            None,
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
            null_protocol_settings_rx(),
            None,
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
