// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! The receiver's lifecycle.
//!
//! Disabled is the default and means nothing is open and nothing is
//! registered: no listener, no accepted connection, no mDNS entry. Enabling
//! binds the control port first, then registers the device with the local
//! mDNS daemon pointing at it, then serves. A bind that fails leaves the
//! receiver enabled but `degraded` with a reason code; so does an mDNS daemon
//! that is absent, in which case the listener is closed again (nothing is
//! left half open) and the registration is retried. Disabling withdraws the
//! registration, ends the running session and closes every socket.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime};

use evo_airplay::{
    AudioSink, Event, Receiver, SessionControl, SessionGate, SinkError,
    SinkFactory,
};
use evo_plugin_sdk::contract::{
    ExternalAddressing, SubjectAnnouncement, SubjectAnnouncer,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::conduct::Conduct;
use crate::lan::LanGate;
use crate::mdns::{Advertiser, Registration, ServiceRecords};
use crate::player::{Player, Tags};
use crate::sink::{EndCapture, LoopbackSink, Note, PcmOpener};
use crate::state::{self, Persisted};

/// Subject type of the receiver's state.
pub const SUBJECT_TYPE: &str = "audio_receiver_airplay";
/// Addressing scheme of the singleton.
pub const SUBJECT_SCHEME: &str = "evo.audio.receiver.airplay";
/// Addressing value of the singleton.
pub const SUBJECT_VALUE: &str = "local";

/// The verbs this plugin answers.
pub const VERBS: [&str; 3] = [
    "receiver.airplay.get_state",
    "receiver.airplay.enable",
    "receiver.airplay.disable",
];

/// The shortest and longest pairing password accepted, in characters.
pub const PASSWORD_LEN: std::ops::RangeInclusive<usize> = 1..=128;

/// Whether `verb` is one of ours.
pub fn is_verb(verb: &str) -> bool {
    VERBS.contains(&verb)
}

/// The singleton's addressing.
pub fn addressing() -> ExternalAddressing {
    ExternalAddressing {
        scheme: SUBJECT_SCHEME.to_string(),
        value: SUBJECT_VALUE.to_string(),
    }
}

/// Where the receiver stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Off. Nothing is open and nothing is registered.
    Disabled,
    /// Enabled and starting.
    Starting,
    /// Enabled, listening and registered; no sender is streaming.
    Advertising,
    /// A sender is streaming.
    Session {
        /// The sender's address.
        peer: String,
        /// When the stream began, in milliseconds since the epoch.
        since_ms: u64,
    },
    /// Enabled but something could not be opened; the code says what.
    Degraded(String),
}

impl Status {
    /// The wire word.
    pub fn word(&self) -> &'static str {
        match self {
            Status::Disabled => "disabled",
            Status::Starting => "starting",
            Status::Advertising => "advertising",
            Status::Session { .. } => "session",
            Status::Degraded(_) => "degraded",
        }
    }
}

/// A running stream, as shown to the operator.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SessionInfo {
    /// The sender's address.
    pub peer: String,
    /// When the stream began, in milliseconds since the epoch.
    pub since_ms: u64,
}

/// The envelope the verbs return and the subject carries. It never holds the
/// password; it says only whether there is one.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Envelope {
    /// Wire version.
    pub v: u32,
    /// `disabled` | `starting` | `advertising` | `session` | `degraded`.
    pub state: &'static str,
    /// Reason code when `degraded`, else absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The operator switch.
    pub enabled: bool,
    /// Name senders show.
    pub friendly_name: String,
    /// TCP control port; `0` until the first start.
    pub port: u16,
    /// `true` when there is no password, so any device on the local network
    /// may pair and stream.
    pub open: bool,
    /// The running stream, while there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionInfo>,
    /// Milliseconds since the Unix epoch at the last change.
    pub last_update_at_ms: u64,
}

/// What the receiver plays through: the player it asks, and the device its
/// audio is written to.
#[derive(Clone)]
pub struct Audio {
    player: Arc<dyn Player>,
    pcm: Arc<dyn PcmOpener>,
}

impl Audio {
    /// The player and the device opener.
    pub fn new(player: Arc<dyn Player>, pcm: Arc<dyn PcmOpener>) -> Self {
        Self { player, pcm }
    }
}

/// How the runtime binds and retries.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// The control port asked for the first time (`0` asks for any).
    pub preferred_port: u16,
    /// Fixed friendly name; `None` reads the host name.
    pub friendly_name: Option<String>,
    /// How long to wait before retrying a registration that failed.
    pub retry_every: Duration,
    /// How often a running stream checks that the player still has its
    /// capture.
    pub watch_every: Duration,
}

impl RuntimeConfig {
    /// Production wiring.
    pub fn production() -> Self {
        Self {
            preferred_port: 7000,
            friendly_name: None,
            retry_every: Duration::from_secs(15),
            watch_every: Duration::from_secs(1),
        }
    }
}

struct Running {
    serve: JoinHandle<()>,
    events: JoinHandle<()>,
    control: SessionControl,
    registration: Option<Box<dyn Registration>>,
}

struct Inner {
    persisted: Persisted,
    password: Option<String>,
    status: Status,
    running: Option<Running>,
    retry: Option<JoinHandle<()>>,
    /// The running stream's work with the player.
    session: Option<Conduct>,
    /// Why the receiver is degraded once the running stream is over.
    after_session: Option<&'static str>,
}

/// The runtime behind the verbs.
pub struct ReceiverRuntime {
    state_dir: PathBuf,
    credentials_dir: PathBuf,
    cfg: RuntimeConfig,
    advertiser: Arc<dyn Advertiser>,
    audio: Audio,
    inner: Mutex<Inner>,
    publisher: StdMutex<Option<Arc<dyn SubjectAnnouncer>>>,
}

/// A verb failed.
#[derive(Debug, thiserror::Error)]
pub enum VerbError {
    /// The verb is not one of ours.
    #[error("unknown request type {0:?}")]
    Unknown(String),
    /// The payload was refused. The text never contains the payload.
    #[error("{0}")]
    Invalid(String),
    /// A persisted value could not be written.
    #[error("{0}")]
    State(#[from] state::StateError),
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The host name, used as the friendly name.
pub fn host_friendly_name() -> String {
    let host = std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "evo".to_string());
    format!("{host} (evo)")
}

/// Check a password a sender will type. The reason never contains it.
pub fn validate_password(password: &str) -> Result<(), VerbError> {
    let len = password.chars().count();
    if !PASSWORD_LEN.contains(&len) || password.chars().any(char::is_control) {
        return Err(VerbError::Invalid(format!(
            "the password must be {} to {} printable characters",
            PASSWORD_LEN.start(),
            PASSWORD_LEN.end()
        )));
    }
    Ok(())
}

#[derive(Deserialize)]
struct EnablePayload {
    #[serde(default)]
    v: Option<u32>,
    #[serde(default)]
    password: Option<String>,
}

/// Read the optional password out of an `enable` payload. Nothing the
/// payload says is repeated in an error.
fn parse_enable(payload: &[u8]) -> Result<Option<String>, VerbError> {
    if payload.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let parsed: EnablePayload =
        serde_json::from_slice(payload).map_err(|_| {
            VerbError::Invalid(
                "the payload is not a valid enable request".into(),
            )
        })?;
    if parsed.v.is_some_and(|v| v != 1) {
        return Err(VerbError::Invalid("unsupported payload version".into()));
    }
    if let Some(password) = &parsed.password {
        validate_password(password)?;
    }
    Ok(parsed.password)
}

/// The gate every receiver is built with: local-network peers only, and one
/// stream at a time.
fn receiver_gate() -> Arc<dyn SessionGate> {
    Arc::new(LanGate::new())
}

impl ReceiverRuntime {
    /// Open the runtime. Nothing is bound; call
    /// [`ReceiverRuntime::reconcile_on_load`] to honour a persisted switch.
    pub fn open(
        state_dir: &Path,
        credentials_dir: &Path,
        cfg: RuntimeConfig,
        advertiser: Arc<dyn Advertiser>,
        audio: Audio,
    ) -> Self {
        let persisted = state::load(state_dir);
        let password = persisted
            .enabled
            .then(|| state::load_password(credentials_dir))
            .flatten();
        Self {
            state_dir: state_dir.to_path_buf(),
            credentials_dir: credentials_dir.to_path_buf(),
            cfg,
            advertiser,
            audio,
            inner: Mutex::new(Inner {
                persisted,
                password,
                status: Status::Disabled,
                running: None,
                retry: None,
                session: None,
                after_session: None,
            }),
            publisher: StdMutex::new(None),
        }
    }

    fn friendly_name(&self) -> String {
        self.cfg
            .friendly_name
            .clone()
            .unwrap_or_else(host_friendly_name)
    }

    fn envelope_of(&self, g: &Inner) -> Envelope {
        Envelope {
            v: 1,
            state: g.status.word(),
            reason: match &g.status {
                Status::Degraded(r) => Some(r.clone()),
                _ => None,
            },
            enabled: g.persisted.enabled,
            friendly_name: self.friendly_name(),
            port: g.persisted.port,
            open: g.password.is_none(),
            session: match &g.status {
                Status::Session { peer, since_ms } => Some(SessionInfo {
                    peer: peer.clone(),
                    since_ms: *since_ms,
                }),
                _ => None,
            },
            last_update_at_ms: now_ms(),
        }
    }

    /// The current envelope.
    pub async fn get_state(&self) -> Envelope {
        let g = self.inner.lock().await;
        self.envelope_of(&g)
    }

    /// Attach the subject announcer and publish the current state.
    pub async fn attach_subject_publisher(
        &self,
        announcer: Arc<dyn SubjectAnnouncer>,
    ) -> Result<(), evo_plugin_sdk::contract::ReportError> {
        let env = self.get_state().await;
        announcer
            .announce(SubjectAnnouncement {
                subject_type: SUBJECT_TYPE.to_string(),
                addressings: vec![addressing()],
                claims: Vec::new(),
                state: serde_json::to_value(&env)
                    .unwrap_or_else(|_| serde_json::json!({})),
                announced_at: SystemTime::now(),
            })
            .await?;
        *self.publisher.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(announcer);
        Ok(())
    }

    async fn publish(&self, env: &Envelope) {
        let announcer = self
            .publisher
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(announcer) = announcer else { return };
        let state =
            serde_json::to_value(env).unwrap_or_else(|_| serde_json::json!({}));
        if let Err(e) = announcer.update_state(addressing(), state).await {
            tracing::debug!(error = %e, "audio_receiver_airplay republish failed");
        }
    }

    /// Bring the receiver up when the persisted switch says on.
    pub async fn reconcile_on_load(self: &Arc<Self>) {
        let on = self.inner.lock().await.persisted.enabled;
        if on {
            let _ = self.start_inner(false).await;
        }
    }

    async fn start(self: &Arc<Self>) -> Envelope {
        self.start_inner(false).await
    }

    async fn start_inner(self: &Arc<Self>, from_retry: bool) -> Envelope {
        let mut g = self.inner.lock().await;
        if g.running.is_some() {
            return self.envelope_of(&g);
        }
        // A pending retry is superseded by this start; the retry task itself
        // is already running and simply finishes.
        match g.retry.take() {
            Some(pending) if !from_retry => pending.abort(),
            _ => {}
        }
        g.status = Status::Starting;
        let name = self.friendly_name();
        let password = g.password.clone();
        let wanted = if g.persisted.port != 0 {
            g.persisted.port
        } else {
            self.cfg.preferred_port
        };

        let build = |port: u16| {
            let mut builder = Receiver::builder()
                .name(name.clone())
                .port(port)
                .identity_path(self.state_dir.join("airplay-identity"))
                .gate(receiver_gate());
            if let Some(password) = &password {
                builder = builder.password(password.clone());
            }
            builder.build()
        };
        let bind = |port: u16| async move {
            let receiver = build(port)?;
            receiver.bind().await
        };
        let bound = match bind(wanted).await {
            Ok(b) => b,
            Err(e) if wanted != 0 => {
                tracing::warn!(port = wanted, error = %e, "the control port is unavailable; choosing another");
                match bind(0).await {
                    Ok(b) => b,
                    Err(e) => {
                        return self
                            .degrade(&mut g, "port_bind_failed", &e.to_string())
                            .await;
                    }
                }
            }
            Err(e) => {
                return self
                    .degrade(&mut g, "port_bind_failed", &e.to_string())
                    .await;
            }
        };

        let port = bound.port();
        let receiver = bound.receiver();
        let records = ServiceRecords {
            airplay_name: name.clone(),
            raop_name: receiver.raop_instance_name(),
            port,
            airplay_txt: receiver.txt_records(),
            raop_txt: receiver.raop_txt_records(),
        };
        let control = receiver.session_control();
        let registration = match self.advertiser.register(&records).await {
            Ok(r) => r,
            Err(e) => {
                // Nothing may be left half open: the listener goes with it.
                drop(bound);
                let env = self
                    .degrade(&mut g, "avahi_unavailable", &e.to_string())
                    .await;
                self.schedule_retry(&mut g);
                return env;
            }
        };

        if port != g.persisted.port {
            g.persisted.port = port;
            if let Err(e) = state::save(&self.state_dir, &g.persisted) {
                tracing::warn!(error = %e, "receiver port not persisted; it will change on restart");
            }
        }
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let (notes_tx, mut notes_rx) = tokio::sync::mpsc::unbounded_channel();
        let factory = self.sink_factory(notes_tx);
        let serve = tokio::spawn(async move {
            if let Err(e) =
                bound.run(move |r, c| factory(r, c), events_tx).await
            {
                tracing::warn!(error = %e, "the AirPlay listener stopped");
            }
        });
        let rt = Arc::clone(self);
        let events = tokio::spawn(async move {
            loop {
                tokio::select! {
                    event = events_rx.recv() => match event {
                        Some(event) => rt.on_event(event).await,
                        None => break,
                    },
                    note = notes_rx.recv() => match note {
                        Some(note) => rt.on_note(note).await,
                        None => break,
                    },
                }
            }
        });
        g.running = Some(Running {
            serve,
            events,
            control,
            registration: Some(registration),
        });
        g.status = Status::Advertising;
        let env = self.envelope_of(&g);
        drop(g);
        self.publish(&env).await;
        env
    }

    fn schedule_retry(self: &Arc<Self>, g: &mut Inner) {
        let rt = Arc::clone(self);
        let every = self.cfg.retry_every;
        g.retry = Some(tokio::spawn(async move {
            tokio::time::sleep(every).await;
            let still_wanted = {
                let g = rt.inner.lock().await;
                g.persisted.enabled && g.running.is_none()
            };
            if still_wanted {
                let _ = rt.start_inner(true).await;
            }
        }));
    }

    /// The sink the library gets for each stream. It is created before the
    /// sender is told yes, so everything that can refuse does so here: the
    /// device must open, and the player must take the capture. A stream is
    /// never accepted and then silenced.
    fn sink_factory(&self, notes: UnboundedSender<Note>) -> SinkFactory {
        let audio = self.audio.clone();
        let handle = tokio::runtime::Handle::current();
        Arc::new(move |rate, channels| {
            if rate != 44_100 || channels != 2 {
                return Err(SinkError::new(format!(
                    "the capture is 44.1 kHz stereo, not {rate} Hz and {channels} channels"
                )));
            }
            let pcm = match audio.pcm.open(rate, channels) {
                Ok(pcm) => pcm,
                Err(e) => {
                    let _ = notes.send(Note::Refused("loopback_unavailable"));
                    return Err(e);
                }
            };
            let asked = handle.block_on(tokio::time::timeout(
                crate::player::CALL_BUDGET,
                audio.player.play_capture(&Tags::default()),
            ));
            match asked {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    drop(pcm);
                    let _ = notes.send(Note::Refused("playback_refused"));
                    return Err(SinkError::new(format!(
                        "the player did not take the capture: {e}"
                    )));
                }
                Err(_) => {
                    drop(pcm);
                    let _ = notes.send(Note::Refused("playback_refused"));
                    return Err(SinkError::new(
                        "the player did not answer in time",
                    ));
                }
            }
            let end = EndCapture::new(handle.clone(), audio.player.clone());
            let sink: Box<dyn AudioSink> =
                Box::new(LoopbackSink::new(pcm, notes.clone(), Some(end)));
            Ok(sink)
        })
    }

    async fn on_note(&self, note: Note) {
        let env = {
            let mut g = self.inner.lock().await;
            if g.running.is_none() {
                return;
            }
            match note {
                Note::Refused(reason) => {
                    // No stream is running: say why the last one was refused.
                    if !matches!(g.status, Status::Session { .. }) {
                        g.status = Status::Degraded(reason.to_string());
                    }
                }
                Note::WriteFailed(why) => {
                    tracing::warn!(error = %why, "ending the stream: the device failed");
                    g.after_session = Some("loopback_unavailable");
                    if let Some(running) = g.running.as_ref() {
                        running.control.end_session();
                    }
                    return;
                }
            }
            self.envelope_of(&g)
        };
        self.publish(&env).await;
    }

    async fn on_event(&self, event: Event) {
        let mut ended = None;
        let env = {
            let mut g = self.inner.lock().await;
            if g.running.is_none() {
                return;
            }
            match event {
                Event::SessionStarted { peer, .. } => {
                    if let Some(control) =
                        g.running.as_ref().map(|r| r.control.clone())
                    {
                        g.session = Some(Conduct::start(
                            self.audio.player.clone(),
                            control,
                            self.cfg.watch_every,
                        ));
                    }
                    g.after_session = None;
                    g.status = Status::Session {
                        peer: peer.to_canonical().to_string(),
                        since_ms: now_ms(),
                    };
                }
                Event::Metadata {
                    title,
                    artist,
                    album,
                } => {
                    if let Some(conduct) = g.session.as_ref() {
                        conduct
                            .tags(Tags::from_metadata(
                                title.as_deref(),
                                artist.as_deref(),
                                album.as_deref(),
                            ))
                            .await;
                    }
                    return;
                }
                Event::Volume { db } => {
                    if let Some(conduct) = g.session.as_ref() {
                        conduct.volume(db);
                    }
                    return;
                }
                Event::SessionEnded => {
                    ended = g.session.take();
                    if matches!(g.status, Status::Session { .. }) {
                        g.status = match g.after_session.take() {
                            Some(reason) => Status::Degraded(reason.into()),
                            None => Status::Advertising,
                        };
                    }
                }
                _ => return,
            }
            self.envelope_of(&g)
        };
        // The capture is out of the player before the session is reported
        // over, or it would play silence.
        if let Some(conduct) = ended {
            conduct.finish().await;
        }
        self.publish(&env).await;
    }

    async fn degrade(
        &self,
        g: &mut tokio::sync::MutexGuard<'_, Inner>,
        reason: &str,
        detail: &str,
    ) -> Envelope {
        tracing::warn!(reason, detail, "receiver enabled but degraded");
        g.status = Status::Degraded(reason.to_string());
        let env = self.envelope_of(g);
        self.publish(&env).await;
        env
    }

    /// Withdraw the registration, end the session and close every socket. The
    /// persisted switch is left as it is, so a restart brings the receiver
    /// back.
    pub async fn shutdown(&self) {
        self.stop().await;
    }

    async fn stop(&self) {
        let mut g = self.inner.lock().await;
        if let Some(pending) = g.retry.take() {
            pending.abort();
        }
        if let Some(mut running) = g.running.take() {
            // Out of sight of the network first, then out of existence.
            if let Some(registration) = running.registration.take() {
                registration.withdraw().await;
            }
            running.control.end_session();
            running.events.abort();
            running.serve.abort();
            let _ = running.serve.await;
        }
        // A stream that was running leaves the player's queue before the
        // receiver reports itself off.
        if let Some(conduct) = g.session.take() {
            conduct.finish().await;
        }
        g.after_session = None;
        g.status = Status::Disabled;
    }

    /// Turn the receiver on and persist the switch. A `password` is stored,
    /// owner-only, and required of senders; with none, any stored password is
    /// cleared and the receiver is open. Enabling a running receiver with a
    /// different password restarts it, which ends any session.
    pub async fn enable(
        self: &Arc<Self>,
        password: Option<String>,
    ) -> Result<Envelope, VerbError> {
        if let Some(password) = &password {
            validate_password(password)?;
        }
        let restart = {
            let mut g = self.inner.lock().await;
            let changed = g.password != password;
            if changed {
                state::store_password(
                    &self.credentials_dir,
                    password.as_deref(),
                )?;
                g.password = password;
            }
            if !g.persisted.enabled {
                g.persisted.enabled = true;
                state::save(&self.state_dir, &g.persisted)?;
            }
            changed && g.running.is_some()
        };
        if restart {
            self.stop().await;
        }
        Ok(self.start().await)
    }

    /// Turn the receiver off, close every socket, forget the password and
    /// persist the switch.
    pub async fn disable(&self) -> Result<Envelope, VerbError> {
        self.stop().await;
        let env = {
            let mut g = self.inner.lock().await;
            if g.password.take().is_some() {
                state::store_password(&self.credentials_dir, None)?;
            }
            if g.persisted.enabled {
                g.persisted.enabled = false;
                state::save(&self.state_dir, &g.persisted)?;
            }
            self.envelope_of(&g)
        };
        self.publish(&env).await;
        Ok(env)
    }

    /// Answer one verb with its payload.
    pub async fn dispatch_verb(
        self: &Arc<Self>,
        verb: &str,
        payload: &[u8],
    ) -> Result<Vec<u8>, VerbError> {
        let env = match verb {
            "receiver.airplay.get_state" => self.get_state().await,
            "receiver.airplay.enable" => {
                self.enable(parse_enable(payload)?).await?
            }
            "receiver.airplay.disable" => self.disable().await?,
            other => return Err(VerbError::Unknown(other.to_string())),
        };
        Ok(
            serde_json::to_vec(&serde_json::json!({ "v": 1, "state": env }))
                .unwrap_or_default(),
        )
    }
}

/// The address a peer string was made from, for tests and the log.
pub fn canonical(addr: IpAddr) -> String {
    addr.to_canonical().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mdns::fake::FakeAdvertiser;
    use evo_airplay::sender::Sender;
    use evo_plugin_sdk::contract::ReportError;
    use std::net::SocketAddr;
    use std::sync::atomic::Ordering;
    use std::sync::Mutex as TestMutex;

    #[derive(Default)]
    struct CapturingAnnouncer {
        announces: TestMutex<Vec<SubjectAnnouncement>>,
        updates: TestMutex<Vec<serde_json::Value>>,
    }

    impl std::fmt::Debug for CapturingAnnouncer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("CapturingAnnouncer").finish_non_exhaustive()
        }
    }

    type Report<'a> = std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<(), ReportError>>
                + Send
                + 'a,
        >,
    >;

    impl SubjectAnnouncer for CapturingAnnouncer {
        fn announce(&self, a: SubjectAnnouncement) -> Report<'_> {
            Box::pin(async move {
                self.announces.lock().unwrap().push(a);
                Ok(())
            })
        }
        fn retract(
            &self,
            _addressing: ExternalAddressing,
            _reason: Option<String>,
        ) -> Report<'_> {
            Box::pin(async move { Ok(()) })
        }
        fn update_state(
            &self,
            _addressing: ExternalAddressing,
            state: serde_json::Value,
        ) -> Report<'_> {
            Box::pin(async move {
                self.updates.lock().unwrap().push(state);
                Ok(())
            })
        }
    }

    struct Rig {
        rt: Arc<ReceiverRuntime>,
        adv: Arc<FakeAdvertiser>,
        player: Arc<crate::player::fake::FakePlayer>,
        opener: Arc<crate::sink::fake::FakeOpener>,
        state: tempfile::TempDir,
        creds: tempfile::TempDir,
    }

    fn cfg(preferred_port: u16) -> RuntimeConfig {
        RuntimeConfig {
            preferred_port,
            friendly_name: Some("Test receiver".into()),
            retry_every: Duration::from_millis(150),
            watch_every: Duration::from_millis(30),
        }
    }

    fn rig_with(preferred_port: u16) -> Rig {
        let state = tempfile::tempdir().unwrap();
        let creds = tempfile::tempdir().unwrap();
        let adv = Arc::new(FakeAdvertiser::default());
        let player = crate::player::fake::FakePlayer::new();
        let opener = Arc::new(crate::sink::fake::FakeOpener::default());
        let rt = Arc::new(ReceiverRuntime::open(
            state.path(),
            creds.path(),
            cfg(preferred_port),
            adv.clone(),
            Audio::new(player.clone(), opener.clone()),
        ));
        Rig {
            rt,
            adv,
            player,
            opener,
            state,
            creds,
        }
    }

    fn rig() -> Rig {
        rig_with(0)
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn live(r: &Rig) -> usize {
        r.adv.live.load(Ordering::SeqCst)
    }

    async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
        for _ in 0..300 {
            if check() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for: {what}");
    }

    #[tokio::test]
    async fn a_new_receiver_is_disabled_and_opens_nothing() {
        let r = rig();
        r.rt.reconcile_on_load().await;
        let e = r.rt.get_state().await;
        assert_eq!(e.state, "disabled");
        assert!(!e.enabled);
        assert_eq!(live(&r), 0, "nothing is registered");
        assert!(r.adv.published.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn enabling_registers_both_services_and_listens() {
        let r = rig();
        let e = r.rt.enable(None).await.unwrap();
        assert_eq!(e.state, "advertising");
        assert!(e.enabled && e.open && e.port != 0 && e.session.is_none());
        assert_eq!(live(&r), 1);
        let published = r.adv.published.lock().unwrap().clone();
        assert_eq!(published.len(), 1);
        let rec = &published[0];
        assert_eq!(rec.port, e.port);
        assert_eq!(rec.airplay_name, "Test receiver");
        assert!(rec.raop_name.ends_with("@Test receiver"));
        assert!(rec.airplay_txt.iter().any(|t| t.starts_with("deviceid=")));
        assert!(rec.raop_txt.iter().any(|t| t == "et=0,4"));
        assert!(!rec.raop_txt.iter().any(|t| t.starts_with("pw=")));
        // The listener is real and an open receiver pairs with the fixed code.
        assert!(Sender::connect(addr(e.port), "3939").await.is_ok());
    }

    const FRAMES: &[u8] =
        include_bytes!("../../../crates/evo-airplay/tests/data/tone_lr.frames");
    const KEY: [u8; 32] = [7u8; 32];

    /// Pair and set a stream up; the sender and the answer to SETUP.
    async fn begin(
        port: u16,
    ) -> (
        Sender,
        evo_airplay::sender::Reply,
        Option<evo_airplay::sender::StreamPorts>,
    ) {
        let mut s = Sender::connect(addr(port), "3939").await.unwrap();
        s.fp_setup().await.unwrap();
        assert!(s.setup_event().await.unwrap().is_ok());
        let (reply, ports) = s.setup_stream(103, &KEY).await.unwrap();
        (s, reply, ports)
    }

    fn taken(r: &Rig) -> usize {
        r.opener.device.lock().unwrap().taken.len()
    }

    #[tokio::test]
    async fn a_stream_plays_through_the_device_and_the_capture_is_taken_out_after(
    ) {
        use crate::player::fake::Call;
        let r = rig();
        let e = r.rt.enable(None).await.unwrap();
        let (mut s, reply, ports) = begin(e.port).await;
        assert_eq!(reply.code(), 200, "{}", reply.status);
        let ports = ports.expect("ports");
        // The player took the capture before the sender was told yes.
        assert_eq!(
            r.player.calls().first(),
            Some(&Call::Play(Tags::default())),
            "{:?}",
            r.player.calls()
        );
        assert_eq!(r.opener.live.load(Ordering::SeqCst), 1);
        eventually("the session is shown", || {
            r.rt.inner
                .try_lock()
                .is_ok_and(|g| matches!(g.status, Status::Session { .. }))
        })
        .await;

        let mut data = s.open_data(ports, &KEY).await.unwrap();
        for frame in evo_airplay::sender::parse_frames(FRAMES).iter().take(40) {
            data.send_frame(frame).await.unwrap();
        }
        s.set_metadata("A Song", "A Band", "A Disc").await.unwrap();
        s.set_volume(-15.0).await.unwrap();
        eventually("audio reaches the device", || taken(&r) > 20_000).await;
        eventually("the title and volume reach the player", || {
            r.player.calls().contains(&Call::Tags(Tags::from_metadata(
                Some("A Song"),
                Some("A Band"),
                Some("A Disc"),
            ))) && r.player.calls().contains(&Call::Volume(50))
        })
        .await;

        s.teardown().await.unwrap();
        eventually("the session is over", || {
            r.rt.inner
                .try_lock()
                .is_ok_and(|g| g.status == Status::Advertising)
        })
        .await;
        assert!(
            r.player.calls().contains(&Call::End),
            "the capture was ended"
        );
        eventually("the device is closed", || {
            r.opener.live.load(Ordering::SeqCst) == 0
        })
        .await;
    }

    #[tokio::test]
    async fn the_capture_is_out_of_the_player_before_the_session_is_reported_over(
    ) {
        use crate::player::fake::Call;
        let r = rig();
        let e = r.rt.enable(None).await.unwrap();
        let (mut s, _, _) = begin(e.port).await;
        s.teardown().await.unwrap();
        eventually("over", || {
            r.rt.inner
                .try_lock()
                .is_ok_and(|g| g.status == Status::Advertising)
        })
        .await;
        // By the time the state said advertising, End had been asked.
        assert!(r.player.calls().contains(&Call::End));
    }

    #[tokio::test]
    async fn a_player_that_will_not_take_the_capture_refuses_the_stream() {
        let r = rig();
        *r.player.play_error.lock().unwrap() =
            Some(crate::player::PlayerError::Idle);
        let e = r.rt.enable(None).await.unwrap();
        let (_s, reply, ports) = begin(e.port).await;
        assert_eq!(reply.code(), 503, "{}", reply.status);
        assert!(ports.is_none());
        eventually("the device is closed again", || {
            r.opener.live.load(Ordering::SeqCst) == 0
        })
        .await;
        eventually("the reason is shown", || {
            r.rt.inner.try_lock().is_ok_and(|g| {
                g.status == Status::Degraded("playback_refused".into())
            })
        })
        .await;
        assert_eq!(taken(&r), 0, "nothing was played");
    }

    #[tokio::test]
    async fn a_device_that_cannot_be_opened_refuses_the_stream_without_asking_the_player(
    ) {
        use crate::player::fake::Call;
        let r = rig();
        *r.opener.fail.lock().unwrap() = Some("busy".into());
        let e = r.rt.enable(None).await.unwrap();
        let (_s, reply, _) = begin(e.port).await;
        assert_eq!(reply.code(), 503, "{}", reply.status);
        assert_eq!(r.player.count(|c| matches!(c, Call::Play(_))), 0);
        eventually("the reason is shown", || {
            r.rt.inner.try_lock().is_ok_and(|g| {
                g.status == Status::Degraded("loopback_unavailable".into())
            })
        })
        .await;
    }

    #[tokio::test]
    async fn a_good_stream_after_a_refused_one_clears_the_reason() {
        let r = rig();
        *r.opener.fail.lock().unwrap() = Some("busy".into());
        let e = r.rt.enable(None).await.unwrap();
        let (_s, reply, _) = begin(e.port).await;
        assert_eq!(reply.code(), 503);
        *r.opener.fail.lock().unwrap() = None;
        let (mut s2, reply, _) = begin(e.port).await;
        assert_eq!(reply.code(), 200, "{}", reply.status);
        eventually("a session", || {
            r.rt.inner
                .try_lock()
                .is_ok_and(|g| matches!(g.status, Status::Session { .. }))
        })
        .await;
        s2.teardown().await.unwrap();
        eventually("advertising again", || {
            r.rt.inner
                .try_lock()
                .is_ok_and(|g| g.status == Status::Advertising)
        })
        .await;
    }

    #[tokio::test]
    async fn a_capture_the_player_took_away_ends_the_stream() {
        use crate::player::fake::Call;
        let r = rig();
        let e = r.rt.enable(None).await.unwrap();
        let (mut s, _, _) = begin(e.port).await;
        eventually("a session", || {
            r.rt.inner
                .try_lock()
                .is_ok_and(|g| matches!(g.status, Status::Session { .. }))
        })
        .await;
        // The operator pressed stop: the player took the capture out.
        *r.player.tags_error.lock().unwrap() =
            Some(crate::player::PlayerError::Absent);
        eventually("the receiver ended the stream", || {
            r.rt.inner
                .try_lock()
                .is_ok_and(|g| g.status == Status::Advertising)
        })
        .await;
        assert!(s.is_closed().await, "the sender's connection is closed");
        assert!(r.player.calls().contains(&Call::End));
        eventually("the device is closed", || {
            r.opener.live.load(Ordering::SeqCst) == 0
        })
        .await;
    }

    #[tokio::test]
    async fn a_device_that_fails_under_a_stream_ends_it_and_says_why() {
        let r = rig();
        r.opener
            .device
            .lock()
            .unwrap()
            .script
            .push_back(crate::sink::PcmError::Failed("gone".into()));
        let e = r.rt.enable(None).await.unwrap();
        let (s, _, ports) = begin(e.port).await;
        let mut data = s.open_data(ports.unwrap(), &KEY).await.unwrap();
        for frame in evo_airplay::sender::parse_frames(FRAMES).iter().take(10) {
            let _ = data.send_frame(frame).await;
        }
        eventually("the reason is shown once it is over", || {
            r.rt.inner.try_lock().is_ok_and(|g| {
                g.status == Status::Degraded("loopback_unavailable".into())
            })
        })
        .await;
    }

    #[tokio::test]
    async fn disabling_mid_stream_takes_the_capture_out_before_it_returns() {
        use crate::player::fake::Call;
        let r = rig();
        let e = r.rt.enable(None).await.unwrap();
        let (_s, reply, _) = begin(e.port).await;
        assert_eq!(reply.code(), 200);
        eventually("a session", || {
            r.rt.inner
                .try_lock()
                .is_ok_and(|g| matches!(g.status, Status::Session { .. }))
        })
        .await;
        let off = r.rt.disable().await.unwrap();
        assert_eq!(off.state, "disabled");
        assert!(
            r.player.calls().contains(&Call::End),
            "ended by the time disable returned"
        );
        eventually("the device is closed", || {
            r.opener.live.load(Ordering::SeqCst) == 0
        })
        .await;
    }

    #[tokio::test]
    async fn a_second_sender_is_refused_and_the_stream_is_undisturbed() {
        let r = rig();
        let e = r.rt.enable(None).await.unwrap();
        let (_first, reply, _) = begin(e.port).await;
        assert_eq!(reply.code(), 200);
        let (_second, refused, ports) = begin(e.port).await;
        assert_eq!(refused.code(), 453, "{}", refused.status);
        assert!(ports.is_none());
        assert_eq!(
            r.opener.opened.load(Ordering::SeqCst),
            1,
            "one device open"
        );
    }

    #[tokio::test]
    async fn a_password_is_required_when_set_and_is_never_shown() {
        let r = rig();
        let e = r.rt.enable(Some("sekrit-1".into())).await.unwrap();
        assert!(!e.open);
        assert!(Sender::connect(addr(e.port), "3939").await.is_err());
        assert!(Sender::connect(addr(e.port), "sekrit-1").await.is_ok());

        let json = String::from_utf8(
            r.rt.dispatch_verb("receiver.airplay.get_state", b"")
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(!json.contains("sekrit"), "the state shows the password");
        assert!(json.contains("\"open\":false"));
        let rec = r.adv.published.lock().unwrap()[0].clone();
        assert!(rec.raop_txt.iter().any(|t| t == "pw=true"));
        assert!(
            !format!("{rec:?}").contains("sekrit"),
            "the advertisement carries the password"
        );
        assert_eq!(
            state::load_password(r.creds.path()).as_deref(),
            Some("sekrit-1")
        );
    }

    #[tokio::test]
    async fn disabling_withdraws_ends_sessions_and_closes_every_socket() {
        let r = rig();
        let e = r.rt.enable(Some("sekrit-1".into())).await.unwrap();
        let mut sender =
            Sender::connect(addr(e.port), "sekrit-1").await.unwrap();
        let mut idle =
            tokio::net::TcpStream::connect(addr(e.port)).await.unwrap();

        let d = r.rt.disable().await.unwrap();
        assert_eq!(d.state, "disabled");
        assert!(!d.enabled);
        assert_eq!(live(&r), 0, "the registration is withdrawn");
        assert!(
            tokio::net::TcpStream::connect(addr(e.port)).await.is_err(),
            "the listener is closed"
        );
        let mut closed = false;
        for _ in 0..100 {
            if sender.is_closed().await {
                closed = true;
                break;
            }
        }
        assert!(closed, "the paired connection is closed");
        use tokio::io::AsyncReadExt;
        let mut byte = [0u8; 1];
        let read =
            tokio::time::timeout(Duration::from_secs(3), idle.read(&mut byte))
                .await
                .expect("the idle connection must close");
        assert!(matches!(read, Ok(0) | Err(_)));
        assert!(
            !state::load(r.state.path()).enabled,
            "the switch persists off"
        );
        assert_eq!(
            state::load_password(r.creds.path()),
            None,
            "a disabled receiver keeps no password"
        );
    }

    #[tokio::test]
    async fn enabling_twice_changes_nothing() {
        let r = rig();
        let a = r.rt.enable(None).await.unwrap();
        let b = r.rt.enable(None).await.unwrap();
        assert_eq!(a.port, b.port);
        assert_eq!(r.adv.published.lock().unwrap().len(), 1);
        assert_eq!(live(&r), 1);
    }

    #[tokio::test]
    async fn a_new_password_restarts_the_receiver_and_ends_the_session() {
        let r = rig();
        let e = r.rt.enable(None).await.unwrap();
        let mut old = Sender::connect(addr(e.port), "3939").await.unwrap();
        let e2 = r.rt.enable(Some("newpass-9".into())).await.unwrap();
        assert!(!e2.open);
        let mut closed = false;
        for _ in 0..100 {
            if old.is_closed().await {
                closed = true;
                break;
            }
        }
        assert!(closed, "the old connection was ended by the restart");
        assert!(Sender::connect(addr(e2.port), "newpass-9").await.is_ok());
        assert_eq!(live(&r), 1, "one registration, the new one");
        assert_eq!(r.adv.published.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn enabling_without_a_password_clears_the_stored_one() {
        let r = rig();
        r.rt.enable(Some("sekrit-1".into())).await.unwrap();
        let e = r.rt.enable(None).await.unwrap();
        assert!(e.open);
        assert_eq!(state::load_password(r.creds.path()), None);
        assert!(Sender::connect(addr(e.port), "3939").await.is_ok());
    }

    #[tokio::test]
    async fn a_missing_mdns_daemon_degrades_closes_the_listener_and_retries() {
        // A port known to be free, so the closing of the listener is visible.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let r = rig_with(port);
        r.adv.fail.store(true, Ordering::SeqCst);
        let e = r.rt.enable(None).await.unwrap();
        assert_eq!(e.state, "degraded");
        assert_eq!(e.reason.as_deref(), Some("avahi_unavailable"));
        assert!(e.enabled, "the operator's switch stays on");
        assert_eq!(live(&r), 0);
        assert!(
            std::net::TcpListener::bind(("0.0.0.0", port)).is_ok(),
            "nothing is left listening"
        );

        // The daemon comes back; the retry registers the device.
        r.adv.fail.store(false, Ordering::SeqCst);
        eventually("the retry registers the device", || {
            r.adv.live.load(Ordering::SeqCst) == 1
        })
        .await;
        assert_eq!(r.rt.get_state().await.state, "advertising");
    }

    #[tokio::test]
    async fn a_busy_port_falls_back_to_another_and_is_not_an_error() {
        let squatter = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let busy = squatter.local_addr().unwrap().port();
        let r = rig_with(busy);
        let e = r.rt.enable(None).await.unwrap();
        assert_eq!(e.state, "advertising");
        assert_ne!(e.port, busy);
        assert_ne!(e.port, 0);
    }

    #[tokio::test]
    async fn the_switch_the_port_and_the_password_survive_a_restart() {
        let r = rig();
        let e = r.rt.enable(Some("sekrit-1".into())).await.unwrap();
        r.rt.shutdown().await;
        assert_eq!(live(&r), 0, "shutdown withdraws");

        let adv = Arc::new(FakeAdvertiser::default());
        let again = Arc::new(ReceiverRuntime::open(
            r.state.path(),
            r.creds.path(),
            cfg(0),
            adv.clone(),
            Audio::new(r.player.clone(), r.opener.clone()),
        ));
        again.reconcile_on_load().await;
        let e2 = again.get_state().await;
        assert_eq!(e2.state, "advertising");
        assert!(!e2.open, "the password came back");
        assert_eq!(
            e2.port, e.port,
            "the same port, so remembered senders find it"
        );
        assert!(Sender::connect(addr(e2.port), "sekrit-1").await.is_ok());
    }

    #[tokio::test]
    async fn a_disabled_receiver_stays_disabled_after_a_restart() {
        let r = rig();
        r.rt.enable(None).await.unwrap();
        r.rt.disable().await.unwrap();
        let adv = Arc::new(FakeAdvertiser::default());
        let again = Arc::new(ReceiverRuntime::open(
            r.state.path(),
            r.creds.path(),
            cfg(0),
            adv.clone(),
            Audio::new(r.player.clone(), r.opener.clone()),
        ));
        again.reconcile_on_load().await;
        assert_eq!(again.get_state().await.state, "disabled");
        assert_eq!(adv.live.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn session_events_move_the_state_and_it_is_published() {
        let r = rig();
        let announcer = Arc::new(CapturingAnnouncer::default());
        r.rt.attach_subject_publisher(announcer.clone())
            .await
            .unwrap();
        assert_eq!(announcer.announces.lock().unwrap().len(), 1);
        r.rt.enable(None).await.unwrap();

        r.rt.on_event(Event::SessionStarted {
            rate: 44_100,
            channels: 2,
            peer: "::ffff:192.168.1.10".parse().unwrap(),
        })
        .await;
        let e = r.rt.get_state().await;
        assert_eq!(e.state, "session");
        assert_eq!(e.session.as_ref().unwrap().peer, "192.168.1.10");

        r.rt.on_event(Event::SessionEnded).await;
        assert_eq!(r.rt.get_state().await.state, "advertising");

        let updates = announcer.updates.lock().unwrap().clone();
        let words: Vec<&str> = updates
            .iter()
            .filter_map(|u| u.get("state").and_then(|s| s.as_str()))
            .collect();
        assert_eq!(words, vec!["advertising", "session", "advertising"]);
        assert!(
            !updates.iter().any(|u| u.to_string().contains("sekrit")),
            "the subject never carries a password"
        );
    }

    #[tokio::test]
    async fn the_verbs_answer_and_refuse_payloads_without_echoing_them() {
        let r = rig();
        let get =
            r.rt.dispatch_verb("receiver.airplay.get_state", b"")
                .await
                .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&get).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["state"]["state"], "disabled");

        let secret = "pw\u{7}sekrit-bell";
        let too_long = "x".repeat(129);
        for bad in [
            serde_json::json!({"v": 1, "password": secret}).to_string(),
            serde_json::json!({"v": 1, "password": ""}).to_string(),
            serde_json::json!({"v": 1, "password": too_long}).to_string(),
            serde_json::json!({"v": 2, "password": "fine-pass"}).to_string(),
            "not json at all".to_string(),
            // Cut off mid-way, so it will not parse, with a password in it.
            "{\"v\":1,\"password\":\"sekrit-truncated\"".to_string(),
            serde_json::json!({"password": 12345}).to_string(),
        ] {
            let err =
                r.rt.dispatch_verb("receiver.airplay.enable", bad.as_bytes())
                    .await
                    .expect_err(&bad);
            assert!(matches!(err, VerbError::Invalid(_)), "{bad}: {err}");
            let text = err.to_string();
            assert!(
                !text.contains("sekrit") && !text.contains("fine-pass"),
                "an error repeated the payload: {text}"
            );
        }
        assert_eq!(
            r.rt.get_state().await.state,
            "disabled",
            "a refusal changes nothing"
        );

        let ok =
            r.rt.dispatch_verb(
                "receiver.airplay.enable",
                br#"{"v":1,"password":"fine-pass"}"#,
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&ok).unwrap();
        assert_eq!(v["state"]["open"], false);
        assert!(matches!(
            r.rt.dispatch_verb("receiver.airplay.nope", b"").await,
            Err(VerbError::Unknown(_))
        ));
    }

    #[test]
    fn every_receiver_is_built_with_the_local_network_gate() {
        let gate = receiver_gate();
        assert!(!gate.accept_peer("8.8.8.8".parse().unwrap()));
        assert!(!gate.accept_peer("2001:db8::1".parse().unwrap()));
        assert!(gate.accept_peer("192.168.1.10".parse().unwrap()));
        assert!(gate.accept_peer("::ffff:10.0.0.7".parse().unwrap()));
    }
}
