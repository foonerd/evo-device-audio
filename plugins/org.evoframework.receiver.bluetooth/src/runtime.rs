// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! The receiver's lifecycle.
//!
//! Disabled is the default and means nothing is registered with the host
//! stack and the adapter is neither discoverable nor pairable. Enabling
//! connects to the stack, powers the radio if it was off, makes sure no
//! pairing window is left open, then registers the sink endpoint and the
//! pairing agent. A stack that is absent, an adapter that is missing or
//! blocked, or a registration that is refused leaves the receiver enabled but
//! `degraded` with a reason code, and it tries again.
//!
//! Visibility is closed by default. Enabling alone lets already-paired
//! devices connect and nothing more: a stranger cannot find the device. Only
//! an explicit, bounded **pairing window** makes the adapter discoverable and
//! pairable, and it closes on timeout, on the first pairing, on request and on
//! disable.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::{Duration, SystemTime};

use evo_plugin_sdk::contract::{
    ExternalAddressing, SubjectAnnouncement, SubjectAnnouncer,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::bluez::{AdapterInfo, Bluez, BtError, Connector, DeviceInfo, Event};
use crate::policy::{is_address, PairingGate};
use crate::state::{self, Persisted};

/// Subject type of the receiver's state.
pub const SUBJECT_TYPE: &str = "audio_receiver_bluetooth";
/// Addressing scheme of the singleton.
pub const SUBJECT_SCHEME: &str = "evo.audio.receiver.bluetooth";
/// Addressing value of the singleton.
pub const SUBJECT_VALUE: &str = "local";

/// The verbs this plugin answers.
pub const VERBS: [&str; 6] = [
    "receiver.bluetooth.get_state",
    "receiver.bluetooth.enable",
    "receiver.bluetooth.disable",
    "receiver.bluetooth.start_pairing",
    "receiver.bluetooth.stop_pairing",
    "receiver.bluetooth.forget_device",
];

/// The pairing window when the caller names none, in seconds.
pub const WINDOW_DEFAULT: u32 = 120;
/// The longest pairing window, in seconds.
pub const WINDOW_MAX: u32 = 300;

/// Degraded reasons.
pub mod reason {
    /// The stack is not running or cannot be reached.
    pub const UNAVAILABLE: &str = "bluetooth_unavailable";
    /// There is no adapter.
    pub const NO_ADAPTER: &str = "no_adapter";
    /// The radio is blocked and will not power on.
    pub const BLOCKED: &str = "adapter_blocked";
    /// The stack refused the endpoint or the agent.
    pub const ENDPOINT_REFUSED: &str = "endpoint_refused";
}

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

/// Knobs that differ between production and a test.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// How long to wait before trying again after a failure.
    pub retry_every: Duration,
}

impl RuntimeConfig {
    /// Production settings.
    pub fn production() -> Self {
        Self {
            retry_every: Duration::from_secs(15),
        }
    }
}

/// Where the receiver stands, before the window and the devices are looked at.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Base {
    Disabled,
    Starting,
    Running,
    Degraded(String),
}

/// The radio, as shown to the operator.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AdapterBrief {
    /// Whether there is an adapter.
    pub present: bool,
    /// Whether the radio is on.
    pub powered: bool,
    /// The adapter's address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
}

/// An open pairing window, as shown to the operator.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PairingInfo {
    /// When it closes, in milliseconds since the epoch.
    pub until_ms: u64,
}

/// The envelope the verbs return and the subject carries. It never holds a
/// link key; the host stack keeps those.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Envelope {
    /// Wire version.
    pub v: u32,
    /// `disabled` | `starting` | `ready` | `pairing` | `connected` | `degraded`.
    pub state: &'static str,
    /// Reason code when `degraded`, else absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The operator switch.
    pub enabled: bool,
    /// The name a phone shows.
    pub friendly_name: String,
    /// The radio.
    pub adapter: AdapterBrief,
    /// The pairing window, while it is open.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pairing: Option<PairingInfo>,
    /// The devices the stack knows (empty while disabled or degraded).
    pub devices: Vec<DeviceInfo>,
    /// Milliseconds since the Unix epoch at the last change.
    pub last_update_at_ms: u64,
}

struct Window {
    until_ms: u64,
    /// The stack's own timeouts before the window, put back when it closes.
    prev: (u32, u32),
    timer: JoinHandle<()>,
    epoch: u64,
}

struct Inner {
    persisted: Persisted,
    base: Base,
    bluez: Option<Arc<dyn Bluez>>,
    adapter: Option<AdapterInfo>,
    devices: Vec<DeviceInfo>,
    window: Option<Window>,
    /// The radio was off and this plugin switched it on.
    powered_by_us: bool,
    retry: Option<JoinHandle<()>>,
    events: Option<JoinHandle<()>>,
    /// Bumped every start and stop, so a late event is ignored.
    epoch: u64,
    /// Bumped every time a window opens, so a late timer is ignored.
    window_seq: u64,
}

/// The runtime behind the verbs.
pub struct BluetoothRuntime {
    state_dir: PathBuf,
    cfg: RuntimeConfig,
    connector: Arc<dyn Connector>,
    gate: Arc<PairingGate>,
    inner: Mutex<Inner>,
    publisher: StdMutex<Option<Arc<dyn SubjectAnnouncer>>>,
}

/// A verb failed.
#[derive(Debug, thiserror::Error)]
pub enum VerbError {
    /// The verb is not one of ours.
    #[error("unknown request type {0:?}")]
    Unknown(String),
    /// The payload or the state was refused. The text never contains the
    /// payload.
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

/// The host name, shown when the stack has no adapter name to give.
pub fn host_friendly_name() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "evo".to_string())
}

fn reason_of(e: &BtError) -> &'static str {
    match e {
        BtError::NoAdapter => reason::NO_ADAPTER,
        BtError::Blocked => reason::BLOCKED,
        BtError::Refused(_) => reason::ENDPOINT_REFUSED,
        BtError::Unavailable(_) | BtError::NotFound => reason::UNAVAILABLE,
    }
}

#[derive(Deserialize, Default)]
struct PairingPayload {
    #[serde(default)]
    v: Option<u32>,
    #[serde(default)]
    seconds: Option<u32>,
}

#[derive(Deserialize)]
struct ForgetPayload {
    #[serde(default)]
    v: Option<u32>,
    address: String,
}

fn check_version(v: Option<u32>) -> Result<(), VerbError> {
    if v.is_some_and(|v| v != 1) {
        return Err(VerbError::Invalid("unsupported payload version".into()));
    }
    Ok(())
}

/// Read the optional window length out of a `start_pairing` payload. Nothing
/// the payload says is repeated in an error.
fn parse_pairing(payload: &[u8]) -> Result<u32, VerbError> {
    let parsed: PairingPayload = if payload.iter().all(u8::is_ascii_whitespace)
    {
        PairingPayload::default()
    } else {
        serde_json::from_slice(payload).map_err(|_| {
            VerbError::Invalid(
                "the payload is not a valid start_pairing request".into(),
            )
        })?
    };
    check_version(parsed.v)?;
    let seconds = parsed.seconds.unwrap_or(WINDOW_DEFAULT);
    if !(1..=WINDOW_MAX).contains(&seconds) {
        return Err(VerbError::Invalid(format!(
            "the pairing window must be 1 to {WINDOW_MAX} seconds"
        )));
    }
    Ok(seconds)
}

fn parse_forget(payload: &[u8]) -> Result<String, VerbError> {
    let parsed: ForgetPayload =
        serde_json::from_slice(payload).map_err(|_| {
            VerbError::Invalid(
                "the payload is not a valid forget_device request".into(),
            )
        })?;
    check_version(parsed.v)?;
    if !is_address(&parsed.address) {
        return Err(VerbError::Invalid(
            "the address is not a Bluetooth address".into(),
        ));
    }
    Ok(parsed.address.to_ascii_uppercase())
}

impl BluetoothRuntime {
    /// Open the runtime. Nothing is registered; call
    /// [`BluetoothRuntime::reconcile_on_load`] to honour a persisted switch.
    pub fn open(
        state_dir: &Path,
        cfg: RuntimeConfig,
        connector: Arc<dyn Connector>,
    ) -> Self {
        Self {
            state_dir: state_dir.to_path_buf(),
            cfg,
            connector,
            gate: Arc::new(PairingGate::new()),
            inner: Mutex::new(Inner {
                persisted: state::load(state_dir),
                base: Base::Disabled,
                bluez: None,
                adapter: None,
                devices: Vec::new(),
                window: None,
                powered_by_us: false,
                retry: None,
                events: None,
                epoch: 0,
                window_seq: 0,
            }),
            publisher: StdMutex::new(None),
        }
    }

    fn envelope_of(&self, g: &Inner) -> Envelope {
        let running = g.base == Base::Running;
        let word = match &g.base {
            Base::Disabled => "disabled",
            Base::Starting => "starting",
            Base::Degraded(_) => "degraded",
            Base::Running if g.window.is_some() => "pairing",
            Base::Running if g.devices.iter().any(|d| d.connected) => {
                "connected"
            }
            Base::Running => "ready",
        };
        Envelope {
            v: 1,
            state: word,
            reason: match &g.base {
                Base::Degraded(r) => Some(r.clone()),
                _ => None,
            },
            enabled: g.persisted.enabled,
            friendly_name: g
                .adapter
                .as_ref()
                .map(|a| a.alias.clone())
                .filter(|a| !a.is_empty())
                .unwrap_or_else(host_friendly_name),
            adapter: AdapterBrief {
                present: g.adapter.is_some(),
                powered: g.adapter.as_ref().is_some_and(|a| a.powered),
                address: g.adapter.as_ref().map(|a| a.address.clone()),
            },
            pairing: g.window.as_ref().map(|w| PairingInfo {
                until_ms: w.until_ms,
            }),
            devices: if running {
                g.devices.clone()
            } else {
                Vec::new()
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
            tracing::debug!(error = %e, "audio_receiver_bluetooth republish failed");
        }
    }

    /// Bring the receiver up when the persisted switch says on.
    pub async fn reconcile_on_load(self: &Arc<Self>) {
        let on = self.inner.lock().await.persisted.enabled;
        if on {
            let _ = self.start(false).await;
        }
    }

    /// Start (or try again). Returns the state after the attempt.
    async fn start(self: &Arc<Self>, from_retry: bool) -> Envelope {
        let env = {
            let mut g = self.inner.lock().await;
            if !g.persisted.enabled || g.base == Base::Running {
                return self.envelope_of(&g);
            }
            if from_retry && !matches!(g.base, Base::Degraded(_)) {
                return self.envelope_of(&g);
            }
            if let Some(pending) = g.retry.take() {
                pending.abort();
            }
            g.base = Base::Starting;
            match self.bring_up(&mut g).await {
                Ok(()) => g.base = Base::Running,
                Err(why) => {
                    g.base = Base::Degraded(why.to_string());
                    self.schedule_retry(&mut g);
                }
            }
            self.envelope_of(&g)
        };
        self.publish(&env).await;
        env
    }

    /// Connect, make the adapter ready and closed, and register. On failure
    /// nothing is left registered and the adapter is not left visible.
    async fn bring_up(
        self: &Arc<Self>,
        g: &mut Inner,
    ) -> Result<(), &'static str> {
        let bluez =
            self.connector.connect().await.map_err(|e| reason_of(&e))?;
        let mut adapter = bluez.adapter().await.map_err(|e| reason_of(&e))?;
        if !adapter.powered {
            bluez.set_powered(true).await.map_err(|e| reason_of(&e))?;
            adapter.powered = true;
            g.powered_by_us = true;
        }
        // Closed by default, every start: a window left open by an earlier run
        // (or by anything else) does not survive.
        self.gate.close();
        if adapter.discoverable {
            bluez
                .set_discoverable(false)
                .await
                .map_err(|e| reason_of(&e))?;
            adapter.discoverable = false;
        }
        if adapter.pairable {
            bluez.set_pairable(false).await.map_err(|e| reason_of(&e))?;
            adapter.pairable = false;
        }
        // A window that died with the plugin left the stack's own timeouts at
        // the window's length: put them back.
        if let Some((d, p)) = g.persisted.restore_timeouts {
            if bluez.set_timeouts(d, p).await.is_ok() {
                adapter.discoverable_timeout = d;
                adapter.pairable_timeout = p;
                g.persisted.restore_timeouts = None;
                let _ = state::save(&self.state_dir, &g.persisted);
            }
        }
        g.epoch += 1;
        let events = bluez.events();
        // The endpoint only. The agent is registered for a pairing window and
        // for nothing else: the stack makes the adapter pairable whenever a
        // default agent exists.
        if let Err(e) = bluez.register_endpoint().await {
            bluez.unregister_endpoint().await;
            return Err(reason_of(&e));
        }
        g.devices = bluez.devices().await.unwrap_or_default();
        g.adapter = Some(adapter);
        g.events = Some(self.spawn_events(events, g.epoch));
        g.bluez = Some(bluez);
        Ok(())
    }

    fn schedule_retry(self: &Arc<Self>, g: &mut Inner) {
        if let Some(old) = g.retry.take() {
            old.abort();
        }
        let weak: Weak<Self> = Arc::downgrade(self);
        let after = self.cfg.retry_every;
        g.retry = Some(tokio::spawn(async move {
            tokio::time::sleep(after).await;
            if let Some(rt) = weak.upgrade() {
                let _ = rt.start(true).await;
            }
        }));
    }

    fn spawn_events(
        self: &Arc<Self>,
        mut rx: tokio::sync::broadcast::Receiver<Event>,
        epoch: u64,
    ) -> JoinHandle<()> {
        let weak: Weak<Self> = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let event = match rx.recv().await {
                    Ok(e) => e,
                    Err(RecvError::Lagged(_)) => Event::Changed,
                    Err(RecvError::Closed) => break,
                };
                let Some(rt) = weak.upgrade() else { break };
                rt.on_event(epoch, event).await;
            }
        })
    }

    async fn on_event(self: &Arc<Self>, epoch: u64, event: Event) {
        let env = {
            let mut g = self.inner.lock().await;
            if g.epoch != epoch || g.base != Base::Running {
                return;
            }
            match event {
                Event::Changed => self.refresh(&mut g).await,
                Event::Paired(address) => {
                    if g.window.is_some() {
                        // Trusted, so the stack authorises the device's audio
                        // later with no agent (there is none outside the
                        // window). The first pairing then closes the window.
                        if let Some(bluez) = g.bluez.clone() {
                            if let Err(e) = bluez.trust(&address).await {
                                tracing::warn!(error = %e, "could not mark a newly paired device trusted");
                            }
                        }
                        self.end_window(&mut g, true).await;
                    }
                    self.refresh(&mut g).await;
                }
                Event::Gone => {
                    // The stack's registrations went with it, and so did any
                    // window it held. Nothing here can be unregistered.
                    self.gate.close();
                    if let Some(w) = g.window.take() {
                        w.timer.abort();
                    }
                    g.bluez = None;
                    g.adapter = None;
                    g.devices.clear();
                    g.powered_by_us = false;
                    g.base = Base::Degraded(reason::UNAVAILABLE.to_string());
                    self.schedule_retry(&mut g);
                }
                Event::Back => {}
            }
            self.envelope_of(&g)
        };
        self.publish(&env).await;
    }

    /// Read the adapter and the devices again.
    async fn refresh(&self, g: &mut Inner) {
        let Some(bluez) = g.bluez.clone() else { return };
        if let Ok(a) = bluez.adapter().await {
            g.adapter = Some(a);
        }
        // Outside a window the adapter is closed. If something else opened it
        // (another agent, another tool), the plugin closes it again: while the
        // receiver is enabled the plugin owns the adapter's visibility.
        if g.window.is_none() {
            if let Some(a) = g.adapter.clone() {
                if a.pairable || a.discoverable {
                    tracing::warn!(
                        pairable = a.pairable,
                        discoverable = a.discoverable,
                        "the adapter was opened outside a pairing window; closing it"
                    );
                    let _ = bluez.set_discoverable(false).await;
                    let _ = bluez.set_pairable(false).await;
                    if let Ok(a) = bluez.adapter().await {
                        g.adapter = Some(a);
                    }
                }
            }
        }
        if let Ok(d) = bluez.devices().await {
            g.devices = d;
        }
    }

    /// Close the pairing window: the agent refuses from this moment, then the
    /// adapter stops being discoverable and pairable and gets back the
    /// stack's own timeouts.
    async fn end_window(&self, g: &mut Inner, abort_timer: bool) {
        self.gate.close();
        let Some(window) = g.window.take() else {
            return;
        };
        if abort_timer {
            window.timer.abort();
        }
        if let Some(bluez) = g.bluez.clone() {
            // Agent first: with none the stack makes the adapter unpairable
            // by itself, and the explicit calls below make sure.
            bluez.unregister_agent().await;
            let _ = bluez.set_discoverable(false).await;
            let _ = bluez.set_pairable(false).await;
            if bluez
                .set_timeouts(window.prev.0, window.prev.1)
                .await
                .is_ok()
                && g.persisted.restore_timeouts.take().is_some()
            {
                let _ = state::save(&self.state_dir, &g.persisted);
            }
            if let Ok(a) = bluez.adapter().await {
                g.adapter = Some(a);
            }
        }
    }

    async fn on_window_timeout(self: &Arc<Self>, epoch: u64) {
        let env = {
            let mut g = self.inner.lock().await;
            if g.window.as_ref().map(|w| w.epoch) != Some(epoch) {
                return;
            }
            // This is the timer itself: dropping its handle, not aborting it.
            self.end_window(&mut g, false).await;
            self.envelope_of(&g)
        };
        self.publish(&env).await;
    }

    /// Open the pairing window for `seconds`. Opening it again restarts it.
    pub async fn start_pairing(
        self: &Arc<Self>,
        seconds: u32,
    ) -> Result<Envelope, VerbError> {
        let env = {
            let mut g = self.inner.lock().await;
            if g.base != Base::Running {
                return Err(VerbError::Invalid(
                    "the Bluetooth receiver is not running; enable it first"
                        .into(),
                ));
            }
            let bluez = g.bluez.clone().ok_or_else(|| {
                VerbError::Invalid(
                    "the Bluetooth receiver is not running; enable it first"
                        .into(),
                )
            })?;
            // The stack's own timeouts, to put back. A window being restarted
            // keeps the ones from before the first.
            let prev = match g.window.take() {
                Some(w) => {
                    w.timer.abort();
                    w.prev
                }
                None => match bluez.adapter().await {
                    Ok(a) => (a.discoverable_timeout, a.pairable_timeout),
                    Err(_) => (180, 0),
                },
            };
            let refuse = |e: BtError| {
                VerbError::Invalid(format!("the Bluetooth stack refused: {e}"))
            };
            // Remember the stack's own timeouts on disk before touching them,
            // so a plugin that dies mid-window can put them back.
            if g.persisted.restore_timeouts.is_none() {
                g.persisted.restore_timeouts = Some(prev);
                state::save(&self.state_dir, &g.persisted)?;
            }
            // The agent is part of the window: registering it is what makes
            // the stack pairable, and it goes with the window.
            let opened = async {
                bluez.set_timeouts(seconds, seconds).await?;
                bluez.register_agent(self.gate.clone()).await?;
                bluez.set_pairable(true).await?;
                bluez.set_discoverable(true).await
            }
            .await;
            if let Err(e) = opened {
                // Never leave half a window.
                self.gate.close();
                bluez.unregister_agent().await;
                let _ = bluez.set_discoverable(false).await;
                let _ = bluez.set_pairable(false).await;
                if bluez.set_timeouts(prev.0, prev.1).await.is_ok() {
                    g.persisted.restore_timeouts = None;
                    let _ = state::save(&self.state_dir, &g.persisted);
                }
                return Err(refuse(e));
            }
            self.gate.open();
            g.window_seq += 1;
            let epoch = g.window_seq;
            let weak: Weak<Self> = Arc::downgrade(self);
            let timer = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(u64::from(seconds)))
                    .await;
                if let Some(rt) = weak.upgrade() {
                    rt.on_window_timeout(epoch).await;
                }
            });
            let until_ms = now_ms() + u64::from(seconds) * 1000;
            g.window = Some(Window {
                until_ms,
                prev,
                timer,
                epoch,
            });
            self.refresh(&mut g).await;
            self.envelope_of(&g)
        };
        self.publish(&env).await;
        Ok(env)
    }

    /// Close the pairing window now. Safe when none is open.
    pub async fn stop_pairing(&self) -> Result<Envelope, VerbError> {
        let env = {
            let mut g = self.inner.lock().await;
            self.end_window(&mut g, true).await;
            self.envelope_of(&g)
        };
        self.publish(&env).await;
        Ok(env)
    }

    /// Forget a device: disconnect it and drop its pairing.
    pub async fn forget_device(
        &self,
        address: &str,
    ) -> Result<Envelope, VerbError> {
        let env = {
            let mut g = self.inner.lock().await;
            let running = g.base == Base::Running;
            let bluez = g.bluez.clone();
            let (true, Some(bluez)) = (running, bluez) else {
                return Err(VerbError::Invalid(
                    "the Bluetooth receiver is not running; enable it first"
                        .into(),
                ));
            };
            if !g
                .devices
                .iter()
                .any(|d| d.address.eq_ignore_ascii_case(address))
            {
                return Err(VerbError::Invalid("no such device".into()));
            }
            bluez.remove_device(address).await.map_err(|e| {
                VerbError::Invalid(format!("the Bluetooth stack refused: {e}"))
            })?;
            self.refresh(&mut g).await;
            self.envelope_of(&g)
        };
        self.publish(&env).await;
        Ok(env)
    }

    /// Withdraw the registrations and close the adapter. The persisted switch
    /// is left as it is, so a restart brings the receiver back.
    pub async fn shutdown(&self) {
        let mut g = self.inner.lock().await;
        self.stop_locked(&mut g).await;
    }

    async fn stop_locked(&self, g: &mut Inner) {
        if let Some(pending) = g.retry.take() {
            pending.abort();
        }
        if let Some(events) = g.events.take() {
            events.abort();
        }
        g.epoch += 1;
        self.end_window(g, true).await;
        if let Some(bluez) = g.bluez.take() {
            bluez.unregister_endpoint().await;
            // Leave the radio as it was found.
            if g.powered_by_us {
                let _ = bluez.set_powered(false).await;
            }
        }
        g.powered_by_us = false;
        g.devices.clear();
        g.adapter = None;
        self.gate.close();
        g.base = Base::Disabled;
    }

    /// Turn the receiver on and persist the switch.
    pub async fn enable(self: &Arc<Self>) -> Result<Envelope, VerbError> {
        {
            let mut g = self.inner.lock().await;
            if !g.persisted.enabled {
                g.persisted.enabled = true;
                state::save(&self.state_dir, &g.persisted)?;
            }
        }
        Ok(self.start(false).await)
    }

    /// Turn the receiver off: close any window, withdraw both registrations,
    /// leave the adapter as found, and persist the switch. Pairings are kept.
    pub async fn disable(&self) -> Result<Envelope, VerbError> {
        let env = {
            let mut g = self.inner.lock().await;
            self.stop_locked(&mut g).await;
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
            "receiver.bluetooth.get_state" => self.get_state().await,
            "receiver.bluetooth.enable" => self.enable().await?,
            "receiver.bluetooth.disable" => self.disable().await?,
            "receiver.bluetooth.start_pairing" => {
                self.start_pairing(parse_pairing(payload)?).await?
            }
            "receiver.bluetooth.stop_pairing" => self.stop_pairing().await?,
            "receiver.bluetooth.forget_device" => {
                self.forget_device(&parse_forget(payload)?).await?
            }
            other => return Err(VerbError::Unknown(other.to_string())),
        };
        Ok(
            serde_json::to_vec(&serde_json::json!({ "v": 1, "state": env }))
                .unwrap_or_default(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bluez::fake::{adapter, FakeBluez, FakeConnector};
    use evo_plugin_sdk::contract::ReportError;
    use std::sync::Mutex as TestMutex;

    #[derive(Default)]
    struct Capturing {
        announces: TestMutex<Vec<SubjectAnnouncement>>,
        updates: TestMutex<Vec<serde_json::Value>>,
    }

    impl std::fmt::Debug for Capturing {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Capturing").finish_non_exhaustive()
        }
    }

    type Report<'a> = std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<(), ReportError>>
                + Send
                + 'a,
        >,
    >;

    impl SubjectAnnouncer for Capturing {
        fn announce(&self, a: SubjectAnnouncement) -> Report<'_> {
            Box::pin(async move {
                self.announces.lock().unwrap().push(a);
                Ok(())
            })
        }
        fn retract(
            &self,
            _a: ExternalAddressing,
            _r: Option<String>,
        ) -> Report<'_> {
            Box::pin(async move { Ok(()) })
        }
        fn update_state(
            &self,
            _a: ExternalAddressing,
            state: serde_json::Value,
        ) -> Report<'_> {
            Box::pin(async move {
                self.updates.lock().unwrap().push(state);
                Ok(())
            })
        }
    }

    struct Rig {
        rt: Arc<BluetoothRuntime>,
        bt: Arc<FakeBluez>,
        conn: Arc<FakeConnector>,
        ann: Arc<Capturing>,
        dir: tempfile::TempDir,
    }

    fn cfg() -> RuntimeConfig {
        RuntimeConfig {
            retry_every: Duration::from_millis(500),
        }
    }

    async fn rig_over(bt: Arc<FakeBluez>, dir: tempfile::TempDir) -> Rig {
        let conn = FakeConnector::to(&bt);
        let rt =
            Arc::new(BluetoothRuntime::open(dir.path(), cfg(), conn.clone()));
        let ann = Arc::new(Capturing::default());
        rt.attach_subject_publisher(ann.clone()).await.unwrap();
        Rig {
            rt,
            bt,
            conn,
            ann,
            dir,
        }
    }

    async fn rig() -> Rig {
        rig_over(
            FakeBluez::new(Some(adapter(true))),
            tempfile::tempdir().unwrap(),
        )
        .await
    }

    async fn settle() {
        for _ in 0..8 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn word(e: &Envelope) -> &'static str {
        e.state
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_receiver_is_disabled_and_touches_nothing() {
        let r = rig().await;
        let e = r.rt.get_state().await;
        assert_eq!(word(&e), "disabled");
        assert!(!e.enabled);
        assert!(e.devices.is_empty() && e.pairing.is_none());
        assert!(r.bt.calls().is_empty());
        assert_eq!(
            r.conn.connects.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    #[tokio::test(start_paused = true)]
    async fn enabling_registers_and_does_not_make_the_device_visible() {
        let r = rig().await;
        let e = r.rt.enable().await.unwrap();
        assert_eq!(word(&e), "ready");
        assert!(e.enabled && e.adapter.present && e.adapter.powered);
        assert_eq!(e.friendly_name, "testspeaker");
        let calls = r.bt.calls();
        assert!(
            calls.contains(&"register_endpoint".to_string()),
            "{calls:?}"
        );
        assert!(
            !calls.contains(&"register_agent".to_string()),
            "no agent outside a pairing window: the stack makes the adapter pairable for one"
        );
        assert!(r.bt.st.lock().unwrap().gate.is_none());
        assert!(
            !calls
                .iter()
                .any(|c| c == "discoverable=true" || c == "pairable=true"),
            "enabling alone must not open the adapter: {calls:?}"
        );
        let a = r.bt.adapter_now();
        assert!(!a.discoverable && !a.pairable);
    }

    #[tokio::test(start_paused = true)]
    async fn a_window_left_open_is_closed_at_every_start() {
        let mut a = adapter(true);
        a.discoverable = true;
        a.pairable = true;
        let r = rig_over(FakeBluez::new(Some(a)), tempfile::tempdir().unwrap())
            .await;
        r.rt.enable().await.unwrap();
        let a = r.bt.adapter_now();
        assert!(
            !a.discoverable && !a.pairable,
            "the adapter must come up closed"
        );
        assert!(r.bt.pair("AA:BB:CC:DD:EE:01", "intruder").is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_radio_that_was_off_is_powered_on_and_put_back_off() {
        let r = rig_over(
            FakeBluez::new(Some(adapter(false))),
            tempfile::tempdir().unwrap(),
        )
        .await;
        let e = r.rt.enable().await.unwrap();
        assert_eq!(word(&e), "ready");
        assert!(r.bt.adapter_now().powered);
        r.rt.disable().await.unwrap();
        assert!(!r.bt.adapter_now().powered, "left as found");
    }

    #[tokio::test(start_paused = true)]
    async fn a_radio_that_was_on_is_left_on() {
        let r = rig().await;
        r.rt.enable().await.unwrap();
        r.rt.disable().await.unwrap();
        assert!(r.bt.adapter_now().powered);
        assert!(!r.bt.calls().iter().any(|c| c == "powered=false"));
    }

    #[tokio::test(start_paused = true)]
    async fn no_adapter_is_degraded_and_found_when_it_arrives() {
        let r =
            rig_over(FakeBluez::new(None), tempfile::tempdir().unwrap()).await;
        let e = r.rt.enable().await.unwrap();
        assert_eq!(word(&e), "degraded");
        assert_eq!(e.reason.as_deref(), Some(reason::NO_ADAPTER));
        assert!(!e.adapter.present);
        assert!(!r.bt.calls().contains(&"register_endpoint".to_string()));
        r.bt.st.lock().unwrap().adapter = Some(adapter(true));
        tokio::time::sleep(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(word(&r.rt.get_state().await), "ready");
    }

    #[tokio::test(start_paused = true)]
    async fn a_stack_that_is_not_there_is_degraded_and_found_when_it_is() {
        let r = rig().await;
        r.conn.set(Err(BtError::Unavailable("no bus".into())));
        let e = r.rt.enable().await.unwrap();
        assert_eq!(e.reason.as_deref(), Some(reason::UNAVAILABLE));
        r.conn.set(Ok(r.bt.clone()));
        tokio::time::sleep(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(word(&r.rt.get_state().await), "ready");
        assert!(r.bt.st.lock().unwrap().endpoint);
    }

    #[tokio::test(start_paused = true)]
    async fn a_blocked_radio_is_reported_and_never_unblocked_by_us() {
        let r = rig_over(
            FakeBluez::new(Some(adapter(false))),
            tempfile::tempdir().unwrap(),
        )
        .await;
        r.bt.st.lock().unwrap().blocked = true;
        let e = r.rt.enable().await.unwrap();
        assert_eq!(e.reason.as_deref(), Some(reason::BLOCKED));
        assert!(!e.adapter.powered);
        assert!(!r.bt.st.lock().unwrap().endpoint);
        r.bt.st.lock().unwrap().blocked = false;
        tokio::time::sleep(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(word(&r.rt.get_state().await), "ready");
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_endpoint_leaves_nothing_registered_and_the_adapter_closed(
    ) {
        let r = rig().await;
        r.bt.st.lock().unwrap().refuse_register = true;
        let e = r.rt.enable().await.unwrap();
        assert_eq!(e.reason.as_deref(), Some(reason::ENDPOINT_REFUSED));
        let s = r.bt.st.lock().unwrap();
        assert!(!s.endpoint && s.gate.is_none());
        assert!(!s.adapter.as_ref().unwrap().pairable);
    }

    #[tokio::test(start_paused = true)]
    async fn the_pairing_window_opens_the_adapter_and_the_agent() {
        let r = rig().await;
        r.rt.enable().await.unwrap();
        let before = now_ms();
        let e = r.rt.start_pairing(120).await.unwrap();
        assert_eq!(word(&e), "pairing");
        let until = e.pairing.as_ref().unwrap().until_ms;
        assert!(
            until >= before + 119_000 && until <= before + 121_000,
            "{until} vs {before}"
        );
        let a = r.bt.adapter_now();
        assert!(a.discoverable && a.pairable);
        assert_eq!((a.discoverable_timeout, a.pairable_timeout), (120, 120));
        let gate = r.bt.st.lock().unwrap().gate.clone().unwrap();
        assert!(gate.is_open());
        r.bt.pair("AA:BB:CC:DD:EE:01", "phone")
            .expect("inside the window");
    }

    #[tokio::test(start_paused = true)]
    async fn the_window_closes_on_request_and_puts_the_stacks_timeouts_back() {
        let r = rig().await;
        r.rt.enable().await.unwrap();
        r.rt.start_pairing(60).await.unwrap();
        let e = r.rt.stop_pairing().await.unwrap();
        assert_eq!(word(&e), "ready");
        let a = r.bt.adapter_now();
        assert!(!a.discoverable && !a.pairable);
        assert_eq!((a.discoverable_timeout, a.pairable_timeout), (180, 0));
        assert!(
            r.bt.st.lock().unwrap().gate.is_none(),
            "the agent goes with the window"
        );
        assert_eq!(state::load(r.dir.path()).restore_timeouts, None);
        // closing again changes nothing
        assert_eq!(word(&r.rt.stop_pairing().await.unwrap()), "ready");
    }

    #[tokio::test(start_paused = true)]
    async fn the_window_closes_by_itself() {
        let r = rig().await;
        r.rt.enable().await.unwrap();
        r.rt.start_pairing(30).await.unwrap();
        tokio::time::sleep(Duration::from_secs(29)).await;
        assert_eq!(word(&r.rt.get_state().await), "pairing");
        tokio::time::sleep(Duration::from_secs(2)).await;
        settle().await;
        let e = r.rt.get_state().await;
        assert_eq!(word(&e), "ready");
        assert!(e.pairing.is_none());
        let a = r.bt.adapter_now();
        assert!(!a.discoverable && !a.pairable);
        assert_eq!((a.discoverable_timeout, a.pairable_timeout), (180, 0));
        assert!(r.bt.pair("AA:BB:CC:DD:EE:02", "late").is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_pairing_closes_the_window() {
        let r = rig().await;
        r.rt.enable().await.unwrap();
        r.rt.start_pairing(120).await.unwrap();
        r.bt.pair("AA:BB:CC:DD:EE:01", "phone").unwrap();
        settle().await;
        let e = r.rt.get_state().await;
        assert_eq!(word(&e), "ready");
        assert!(e.pairing.is_none());
        assert_eq!(e.devices.len(), 1);
        assert!(e.devices[0].paired);
        let a = r.bt.adapter_now();
        assert!(!a.discoverable && !a.pairable);
        // a second device now finds the door shut
        assert!(r.bt.pair("AA:BB:CC:DD:EE:02", "second").is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn outside_the_window_nothing_pairs_there_is_no_agent_and_the_adapter_is_shut(
    ) {
        let r = rig().await;
        r.rt.enable().await.unwrap();
        assert!(
            r.bt.st.lock().unwrap().gate.is_none(),
            "no agent: the stack would make the adapter pairable for it"
        );
        assert_eq!(
            r.bt.pair("AA:BB:CC:DD:EE:01", "stranger"),
            Err("the adapter is not pairable")
        );
        assert!(r.rt.get_state().await.devices.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn an_agent_registered_by_something_else_does_not_leave_the_adapter_open(
    ) {
        let r = rig().await;
        r.rt.enable().await.unwrap();
        // another tool registers its own agent: the stack makes the adapter
        // pairable, and tells us something changed
        r.bt.st.lock().unwrap().adapter.as_mut().unwrap().pairable = true;
        r.bt.st
            .lock()
            .unwrap()
            .adapter
            .as_mut()
            .unwrap()
            .discoverable = true;
        r.bt.send(Event::Changed);
        settle().await;
        let a = r.bt.adapter_now();
        assert!(
            !a.pairable && !a.discoverable,
            "the plugin owns the adapter's visibility while enabled"
        );
        assert_eq!(
            r.bt.pair("AA:BB:CC:DD:EE:01", "stranger"),
            Err("the adapter is not pairable")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_agent_exists_only_while_the_window_is_open() {
        let r = rig().await;
        r.rt.enable().await.unwrap();
        assert!(r.bt.st.lock().unwrap().gate.is_none());
        r.rt.start_pairing(60).await.unwrap();
        assert!(r.bt.st.lock().unwrap().gate.is_some());
        assert!(r.bt.adapter_now().pairable);
        r.rt.stop_pairing().await.unwrap();
        assert!(r.bt.st.lock().unwrap().gate.is_none());
        assert!(!r.bt.adapter_now().pairable);
        // and it goes with the timer, with the first pairing, and with disable
        r.rt.start_pairing(5).await.unwrap();
        tokio::time::sleep(Duration::from_secs(6)).await;
        settle().await;
        assert!(r.bt.st.lock().unwrap().gate.is_none(), "timeout");
        r.rt.start_pairing(60).await.unwrap();
        r.bt.pair("AA:BB:CC:DD:EE:01", "phone").unwrap();
        settle().await;
        assert!(r.bt.st.lock().unwrap().gate.is_none(), "first pairing");
        r.rt.start_pairing(60).await.unwrap();
        r.rt.disable().await.unwrap();
        assert!(r.bt.st.lock().unwrap().gate.is_none(), "disable");
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_agent_opens_nothing_and_puts_everything_back() {
        let r = rig().await;
        r.rt.enable().await.unwrap();
        r.bt.st.lock().unwrap().refuse_agent = true;
        let e = r.rt.start_pairing(60).await.unwrap_err();
        assert!(e.to_string().contains("refused"), "{e}");
        let a = r.bt.adapter_now();
        assert!(!a.discoverable && !a.pairable);
        assert_eq!((a.discoverable_timeout, a.pairable_timeout), (180, 0));
        assert_eq!(state::load(r.dir.path()).restore_timeouts, None);
        assert_eq!(word(&r.rt.get_state().await), "ready");
    }

    #[tokio::test(start_paused = true)]
    async fn a_window_that_fails_half_way_is_taken_down_again() {
        let r = rig().await;
        r.rt.enable().await.unwrap();
        // the agent goes in (and the stack makes the adapter pairable), then
        // the next step fails
        r.bt.st.lock().unwrap().refuse_discoverable = true;
        assert!(r.rt.start_pairing(60).await.is_err());
        {
            let s = r.bt.st.lock().unwrap();
            assert!(s.gate.is_none(), "the agent must not be left in");
            let a = s.adapter.as_ref().unwrap();
            assert!(!a.pairable && !a.discoverable, "nothing left open");
            assert_eq!((a.discoverable_timeout, a.pairable_timeout), (180, 0));
        }
        assert_eq!(state::load(r.dir.path()).restore_timeouts, None);
        assert_eq!(word(&r.rt.get_state().await), "ready");
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_pairing_trusts_the_device_and_a_pairing_outside_a_window_does_not(
    ) {
        let r = rig().await;
        r.rt.enable().await.unwrap();
        r.rt.start_pairing(60).await.unwrap();
        r.bt.pair("AA:BB:CC:DD:EE:01", "phone").unwrap();
        settle().await;
        assert_eq!(r.bt.st.lock().unwrap().trusted, ["AA:BB:CC:DD:EE:01"]);
        // something pairs a device with no window open (not through us)
        r.bt.add_paired("AA:BB:CC:DD:EE:02", "other");
        r.bt.send(Event::Paired("AA:BB:CC:DD:EE:02".into()));
        settle().await;
        assert_eq!(
            r.bt.st.lock().unwrap().trusted,
            ["AA:BB:CC:DD:EE:01"],
            "only devices paired inside our window are trusted"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_window_that_died_with_the_plugin_has_its_timeouts_put_back_at_the_next_start(
    ) {
        let r = rig().await;
        r.rt.enable().await.unwrap();
        r.rt.start_pairing(120).await.unwrap();
        assert_eq!(
            state::load(r.dir.path()).restore_timeouts,
            Some((180, 0)),
            "kept on disk while the window has them changed"
        );
        let a = r.bt.adapter_now();
        assert_eq!((a.discoverable_timeout, a.pairable_timeout), (120, 120));
        // the plugin dies: its connection goes, and with it its agent
        r.bt.st.lock().unwrap().gate = None;
        let (bt, dir) = (r.bt, r.dir);
        let r2 = rig_over(bt.clone(), dir).await;
        r2.rt.reconcile_on_load().await;
        let a = bt.adapter_now();
        assert_eq!(
            (a.discoverable_timeout, a.pairable_timeout),
            (180, 0),
            "the stack's own timeouts, not the window's"
        );
        assert!(!a.pairable && !a.discoverable);
        assert_eq!(state::load(r2.dir.path()).restore_timeouts, None);
    }

    #[tokio::test(start_paused = true)]
    async fn opening_the_window_again_restarts_it_and_keeps_the_original_timeouts(
    ) {
        let r = rig().await;
        r.rt.enable().await.unwrap();
        r.rt.start_pairing(30).await.unwrap();
        tokio::time::sleep(Duration::from_secs(20)).await;
        r.rt.start_pairing(60).await.unwrap();
        tokio::time::sleep(Duration::from_secs(20)).await;
        assert_eq!(
            word(&r.rt.get_state().await),
            "pairing",
            "the first timer must not close the second window"
        );
        tokio::time::sleep(Duration::from_secs(41)).await;
        settle().await;
        assert_eq!(word(&r.rt.get_state().await), "ready");
        let a = r.bt.adapter_now();
        assert_eq!(
            (a.discoverable_timeout, a.pairable_timeout),
            (180, 0),
            "not (60, 60)"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_timer_from_an_earlier_window_cannot_close_a_later_one() {
        let r = rig().await;
        r.rt.enable().await.unwrap();
        r.rt.start_pairing(120).await.unwrap(); // window 1
        r.rt.start_pairing(120).await.unwrap(); // window 2 replaces it
                                                // a timer that was already past its abort when window 2 opened
        r.rt.on_window_timeout(1).await;
        assert_eq!(word(&r.rt.get_state().await), "pairing");
        assert!(r.bt.adapter_now().pairable);
        r.rt.on_window_timeout(2).await;
        assert_eq!(word(&r.rt.get_state().await), "ready");
    }

    #[tokio::test(start_paused = true)]
    async fn a_window_can_only_be_opened_on_a_running_receiver() {
        let r = rig().await;
        let e = r.rt.start_pairing(60).await.unwrap_err();
        assert!(e.to_string().contains("not running"), "{e}");
        assert!(r.bt.calls().is_empty());
        // degraded is not running either
        r.bt.st.lock().unwrap().refuse_register = true;
        r.rt.enable().await.unwrap();
        assert!(r.rt.start_pairing(60).await.is_err());
        assert!(!r.bt.adapter_now().pairable);
    }

    #[tokio::test(start_paused = true)]
    async fn the_window_length_is_bounded_and_payloads_are_never_echoed() {
        assert_eq!(parse_pairing(b"").unwrap(), WINDOW_DEFAULT);
        assert_eq!(parse_pairing(b"{}").unwrap(), WINDOW_DEFAULT);
        assert_eq!(parse_pairing(br#"{"v":1,"seconds":300}"#).unwrap(), 300);
        assert_eq!(parse_pairing(br#"{"seconds":1}"#).unwrap(), 1);
        for bad in [
            &br#"{"seconds":0}"#[..],
            br#"{"seconds":301}"#,
            br#"{"seconds":-5}"#,
            br#"{"v":2}"#,
            b"not json SECRET-TOKEN",
            br#"{"seconds":"SECRET-TOKEN"}"#,
        ] {
            let e = parse_pairing(bad).unwrap_err().to_string();
            assert!(!e.contains("SECRET-TOKEN"), "{e}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn disabling_closes_the_window_withdraws_and_keeps_the_pairings() {
        let r = rig().await;
        r.rt.enable().await.unwrap();
        r.bt.add_paired("AA:BB:CC:DD:EE:01", "kept");
        r.rt.start_pairing(120).await.unwrap();
        let e = r.rt.disable().await.unwrap();
        assert_eq!(word(&e), "disabled");
        assert!(!e.enabled && e.pairing.is_none() && e.devices.is_empty());
        let s = r.bt.st.lock().unwrap();
        assert!(!s.endpoint && s.gate.is_none());
        let a = s.adapter.as_ref().unwrap();
        assert!(!a.discoverable && !a.pairable);
        assert_eq!(s.devices.len(), 1, "pairings are the stack's and are kept");
        assert!(!state::load(r.dir.path()).enabled);
    }

    #[tokio::test(start_paused = true)]
    async fn enabling_twice_changes_nothing() {
        let r = rig().await;
        r.rt.enable().await.unwrap();
        let calls = r.bt.calls().len();
        let e = r.rt.enable().await.unwrap();
        assert_eq!(word(&e), "ready");
        assert_eq!(r.bt.calls().len(), calls);
    }

    #[tokio::test(start_paused = true)]
    async fn the_switch_survives_a_restart_and_the_adapter_comes_up_closed() {
        let r = rig().await;
        r.rt.enable().await.unwrap();
        r.rt.start_pairing(120).await.unwrap();
        // the plugin dies with the window open: nothing closes it
        let dir = r.dir;
        let bt = r.bt;
        assert!(bt.adapter_now().pairable);
        let r2 = rig_over(bt.clone(), dir).await;
        assert_eq!(
            word(&r2.rt.get_state().await),
            "disabled",
            "nothing before reconcile"
        );
        r2.rt.reconcile_on_load().await;
        let e = r2.rt.get_state().await;
        assert_eq!(word(&e), "ready");
        assert!(e.pairing.is_none());
        let a = bt.adapter_now();
        assert!(
            !a.discoverable && !a.pairable,
            "a restart never comes back open"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_disabled_receiver_stays_disabled_after_a_restart() {
        let r = rig().await;
        r.rt.enable().await.unwrap();
        r.rt.disable().await.unwrap();
        let r2 = rig_over(r.bt.clone(), r.dir).await;
        r2.rt.reconcile_on_load().await;
        assert_eq!(word(&r2.rt.get_state().await), "disabled");
        assert_eq!(
            r2.conn.connects.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_connected_device_is_shown_and_the_state_follows_it() {
        let r = rig().await;
        r.bt.add_paired("AA:BB:CC:DD:EE:01", "phone");
        r.rt.enable().await.unwrap();
        let e = r.rt.get_state().await;
        assert_eq!(word(&e), "ready");
        assert_eq!(e.devices.len(), 1);
        r.bt.set_connected("AA:BB:CC:DD:EE:01", true);
        settle().await;
        let e = r.rt.get_state().await;
        assert_eq!(word(&e), "connected");
        assert!(e.devices[0].connected);
        r.bt.set_connected("AA:BB:CC:DD:EE:01", false);
        settle().await;
        assert_eq!(word(&r.rt.get_state().await), "ready");
        // an open window outranks "connected" in the word, and both are listed
        r.bt.set_connected("AA:BB:CC:DD:EE:01", true);
        settle().await;
        let e = r.rt.start_pairing(60).await.unwrap();
        assert_eq!(word(&e), "pairing");
        assert!(e.devices[0].connected);
    }

    #[tokio::test(start_paused = true)]
    async fn forgetting_a_device_removes_it_and_refuses_what_it_does_not_know()
    {
        let r = rig().await;
        r.bt.add_paired("AA:BB:CC:DD:EE:01", "phone");
        r.rt.enable().await.unwrap();
        let e = r.rt.forget_device("AA:BB:CC:DD:EE:01").await.unwrap();
        assert!(e.devices.is_empty());
        assert!(r
            .bt
            .calls()
            .contains(&"remove=AA:BB:CC:DD:EE:01".to_string()));
        let e = r.rt.forget_device("AA:BB:CC:DD:EE:99").await.unwrap_err();
        assert!(e.to_string().contains("no such device"), "{e}");
        assert_eq!(
            r.bt.st.lock().unwrap().remove_asked,
            ["AA:BB:CC:DD:EE:01"],
            "the stack is asked about the device we know, never about one we do not"
        );
        // while disabled there is nothing to forget through us
        r.rt.disable().await.unwrap();
        assert!(r.rt.forget_device("AA:BB:CC:DD:EE:01").await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn forget_payloads_are_checked_and_never_echoed() {
        assert_eq!(
            parse_forget(br#"{"v":1,"address":"aa:bb:cc:dd:ee:01"}"#).unwrap(),
            "AA:BB:CC:DD:EE:01"
        );
        for bad in [
            &b""[..],
            b"{}",
            br#"{"address":"../../etc/passwd SECRET-TOKEN"}"#,
            br#"{"address":"AA:BB:CC:DD:EE"}"#,
            br#"{"v":2,"address":"AA:BB:CC:DD:EE:01"}"#,
            b"SECRET-TOKEN",
        ] {
            let e = parse_forget(bad).unwrap_err().to_string();
            assert!(
                !e.contains("SECRET-TOKEN") && !e.contains("passwd"),
                "{e}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_stack_going_away_degrades_closes_the_gate_and_registers_again_when_it_is_back(
    ) {
        let r = rig().await;
        r.rt.enable().await.unwrap();
        r.rt.start_pairing(120).await.unwrap();
        let first = r.bt.clone();
        // the stack restarts: the old connection is dead, a new one is offered
        let second = FakeBluez::new(Some(adapter(true)));
        r.conn.set(Ok(second.clone()));
        first.send(Event::Gone);
        settle().await;
        let e = r.rt.get_state().await;
        assert_eq!(e.reason.as_deref(), Some(reason::UNAVAILABLE), "{e:?}");
        assert!(e.pairing.is_none(), "a window does not outlive the stack");
        let gate = first
            .st
            .lock()
            .unwrap()
            .gate
            .clone()
            .expect("registered once");
        assert!(
            !gate.is_open(),
            "the agent must refuse from the moment the stack is gone"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(word(&r.rt.get_state().await), "ready");
        assert!(second.st.lock().unwrap().endpoint);
        let a = second.adapter_now();
        assert!(!a.discoverable && !a.pairable);
    }

    #[tokio::test(start_paused = true)]
    async fn the_state_is_published_on_every_change() {
        let r = rig().await;
        assert_eq!(r.ann.announces.lock().unwrap().len(), 1);
        assert_eq!(
            r.ann.announces.lock().unwrap()[0].subject_type,
            SUBJECT_TYPE
        );
        r.rt.enable().await.unwrap();
        r.rt.start_pairing(60).await.unwrap();
        r.rt.stop_pairing().await.unwrap();
        r.rt.disable().await.unwrap();
        let words: Vec<String> = r
            .ann
            .updates
            .lock()
            .unwrap()
            .iter()
            .map(|u| u["state"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            words,
            ["ready", "pairing", "ready", "disabled"],
            "{words:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_verbs_answer_and_unknown_ones_are_refused() {
        let r = rig().await;
        let get = |v: &str| {
            let rt = r.rt.clone();
            let v = v.to_string();
            async move { rt.dispatch_verb(&v, b"").await }
        };
        let out = get("receiver.bluetooth.get_state").await.unwrap();
        let j: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(j["v"], 1);
        assert_eq!(j["state"]["state"], "disabled");
        assert_eq!(j["state"]["enabled"], false);
        assert_eq!(j["state"]["adapter"]["present"], false);
        let out = get("receiver.bluetooth.enable").await.unwrap();
        let j: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(j["state"]["state"], "ready");
        assert_eq!(j["state"]["adapter"]["address"], "D8:3A:DD:B8:D6:75");
        let out =
            r.rt.dispatch_verb(
                "receiver.bluetooth.start_pairing",
                br#"{"v":1,"seconds":45}"#,
            )
            .await
            .unwrap();
        let j: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(j["state"]["state"], "pairing");
        assert!(j["state"]["pairing"]["until_ms"].as_u64().unwrap() > 0);
        assert!(matches!(
            r.rt.dispatch_verb("receiver.bluetooth.nope", b"").await,
            Err(VerbError::Unknown(_))
        ));
        // no verb ever puts a key in the state
        let text = String::from_utf8(out).unwrap().to_ascii_lowercase();
        assert!(!text.contains("key") && !text.contains("pin"), "{text}");
    }

    #[tokio::test(start_paused = true)]
    async fn every_listed_verb_is_dispatched() {
        let r = rig().await;
        r.rt.enable().await.unwrap();
        for verb in VERBS {
            let payload: &[u8] = match verb {
                "receiver.bluetooth.forget_device" => {
                    br#"{"address":"AA:BB:CC:DD:EE:01"}"#
                }
                _ => b"",
            };
            if let Err(VerbError::Unknown(_)) =
                r.rt.dispatch_verb(verb, payload).await
            {
                panic!("{verb} is listed but not dispatched");
            }
        }
    }
}
