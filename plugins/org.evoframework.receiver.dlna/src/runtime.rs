// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! The renderer's lifecycle.
//!
//! Disabled is the default and means nothing is open: no UDP socket, no
//! TCP listener. Enabling binds the description server first, then the
//! SSDP responder pointed at it, and publishes the state. A bind that
//! fails leaves the renderer enabled but `degraded` with a reason code, so
//! the operator can see it and a restart retries.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::SystemTime;

use evo_plugin_sdk::contract::{
    ExternalAddressing, SubjectAnnouncement, SubjectAnnouncer,
};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::bridge::{NoPlayer, Player};
use crate::control::Control;
use crate::events::{self, Subscriptions};
use crate::http::{self, DeviceInfo, HttpHandle, Services};
use crate::ssdp::{self, Identity, SsdpConfig, SsdpHandle};
use crate::state::{self, Persisted};

/// Subject type of the renderer's state.
pub const SUBJECT_TYPE: &str = "audio_receiver_dlna";
/// Addressing scheme of the singleton.
pub const SUBJECT_SCHEME: &str = "evo.audio.receiver.dlna";
/// Addressing value of the singleton.
pub const SUBJECT_VALUE: &str = "local";

/// The verbs this plugin answers.
pub const VERBS: [&str; 3] = [
    "receiver.dlna.get_state",
    "receiver.dlna.enable",
    "receiver.dlna.disable",
];

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

/// Where the renderer stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Off. Nothing is open.
    Disabled,
    /// Enabled and binding.
    Starting,
    /// Enabled, listening and announced.
    Advertising,
    /// Enabled but a listener could not be opened; the code says which.
    Degraded(String),
}

impl Status {
    /// The wire word.
    pub fn word(&self) -> &'static str {
        match self {
            Status::Disabled => "disabled",
            Status::Starting => "starting",
            Status::Advertising => "advertising",
            Status::Degraded(_) => "degraded",
        }
    }
}

/// The envelope the verbs return and the subject carries.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Envelope {
    /// Wire version.
    pub v: u32,
    /// `disabled` | `starting` | `advertising` | `degraded`.
    pub state: &'static str,
    /// Reason code when `degraded`, else absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The operator switch.
    pub enabled: bool,
    /// Name controllers show.
    pub friendly_name: String,
    /// Device UUID (without the `uuid:` prefix).
    pub udn: String,
    /// TCP port of the description server; `0` until first start.
    pub port: u16,
    /// Milliseconds since the Unix epoch at the last change.
    pub last_update_at_ms: u64,
}

/// Where and how the services bind.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// SSDP responder configuration.
    pub ssdp: SsdpConfig,
    /// Address the description server binds.
    pub http_ip: IpAddr,
    /// Fixed friendly name; `None` reads the host name.
    pub friendly_name: Option<String>,
}

impl RuntimeConfig {
    /// Production wiring: the SSDP group, all interfaces.
    pub fn production() -> Self {
        Self {
            ssdp: SsdpConfig::production(),
            http_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            friendly_name: None,
        }
    }
}

struct Running {
    control: Arc<Control>,
    http: HttpHandle,
    ssdp: SsdpHandle,
    monitor_stop: tokio::sync::watch::Sender<bool>,
    monitor: tokio::task::JoinHandle<()>,
}

struct Inner {
    persisted: Persisted,
    status: Status,
    running: Option<Running>,
}

/// The runtime behind the verbs.
pub struct RendererRuntime {
    dir: PathBuf,
    cfg: RuntimeConfig,
    inner: Mutex<Inner>,
    publisher: StdMutex<Option<Arc<dyn SubjectAnnouncer>>>,
    player: StdMutex<Arc<dyn Player>>,
}

/// A verb failed.
#[derive(Debug, thiserror::Error)]
pub enum VerbError {
    /// The verb is not one of ours.
    #[error("unknown request type {0:?}")]
    Unknown(String),
    /// The persisted switch could not be written.
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

impl RendererRuntime {
    /// Open the runtime over `dir`. Nothing is bound; call
    /// [`RendererRuntime::reconcile_on_load`] to honour a persisted switch.
    pub fn open(
        dir: &Path,
        cfg: RuntimeConfig,
    ) -> Result<Self, state::StateError> {
        let persisted = state::load_or_create(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            cfg,
            inner: Mutex::new(Inner {
                persisted,
                status: Status::Disabled,
                running: None,
            }),
            publisher: StdMutex::new(None),
            player: StdMutex::new(Arc::new(NoPlayer)),
        })
    }

    /// Wire the player the control actions drive. Until this is called
    /// every action answers "Action Failed".
    pub fn set_player(&self, p: Arc<dyn Player>) {
        *self.player.lock().expect("player slot") = p;
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
            udn: g.persisted.uuid.clone(),
            port: g.persisted.port,
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
        *self.publisher.lock().expect("publisher slot") = Some(announcer);
        Ok(())
    }

    async fn publish(&self, env: &Envelope) {
        let announcer = self.publisher.lock().expect("publisher slot").clone();
        let Some(announcer) = announcer else { return };
        let state =
            serde_json::to_value(env).unwrap_or_else(|_| serde_json::json!({}));
        if let Err(e) = announcer.update_state(addressing(), state).await {
            tracing::debug!(error = %e, "audio_receiver_dlna republish failed");
        }
    }

    /// Bring the listeners up when the persisted switch says on.
    pub async fn reconcile_on_load(&self) {
        let on = self.inner.lock().await.persisted.enabled;
        if on {
            let _ = self.start().await;
        }
    }

    async fn start(&self) -> Envelope {
        let mut g = self.inner.lock().await;
        if g.running.is_some() {
            return self.envelope_of(&g);
        }
        g.status = Status::Starting;

        let control =
            Control::new(self.player.lock().expect("player slot").clone());
        let subs = Subscriptions::new();
        let info = Arc::new(Services {
            info: DeviceInfo {
                uuid: g.persisted.uuid.clone(),
                friendly_name: self.friendly_name(),
            },
            control: Arc::clone(&control),
            subs: Arc::clone(&subs),
        });

        // The description server first: SSDP must point at something.
        let want = g.persisted.port;
        let http = match http::start(
            SocketAddr::new(self.cfg.http_ip, want),
            Arc::clone(&info),
        )
        .await
        {
            Ok(h) => h,
            Err(e) if want != 0 => {
                tracing::warn!(port = want, error = %e, "persisted renderer port unavailable; choosing another");
                match http::start(
                    SocketAddr::new(self.cfg.http_ip, 0),
                    Arc::clone(&info),
                )
                .await
                {
                    Ok(h) => h,
                    Err(e) => {
                        return self
                            .degrade(&mut g, "http_bind_failed", &e.to_string())
                            .await
                    }
                }
            }
            Err(e) => {
                return self
                    .degrade(&mut g, "http_bind_failed", &e.to_string())
                    .await;
            }
        };
        let port = http.local_addr().port();
        if port != g.persisted.port {
            g.persisted.port = port;
            if let Err(e) = state::save(&self.dir, &g.persisted) {
                tracing::warn!(error = %e, "renderer port not persisted; it will change on restart");
            }
        }

        let ssdp = match ssdp::start(
            self.cfg.ssdp.clone(),
            Identity {
                uuid: g.persisted.uuid.clone(),
                port,
            },
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                http.stop().await;
                return self
                    .degrade(&mut g, "ssdp_bind_failed", &e.to_string())
                    .await;
            }
        };

        let (monitor_stop, stop_rx) = tokio::sync::watch::channel(false);
        let monitor =
            tokio::spawn(events::monitor(Arc::clone(&control), subs, stop_rx));
        g.running = Some(Running {
            control,
            http,
            ssdp,
            monitor_stop,
            monitor,
        });
        g.status = Status::Advertising;
        let env = self.envelope_of(&g);
        drop(g);
        self.publish(&env).await;
        env
    }

    async fn degrade(
        &self,
        g: &mut tokio::sync::MutexGuard<'_, Inner>,
        reason: &str,
        detail: &str,
    ) -> Envelope {
        tracing::warn!(reason, detail, "renderer enabled but degraded");
        g.status = Status::Degraded(reason.to_string());
        let env = self.envelope_of(g);
        self.publish(&env).await;
        env
    }

    /// Close every socket and say goodbye on the network. The persisted
    /// switch is left as it is, so a restart brings the renderer back.
    pub async fn shutdown(&self) {
        self.stop().await;
    }

    async fn stop(&self) {
        let mut g = self.inner.lock().await;
        if let Some(r) = g.running.take() {
            let _ = r.monitor_stop.send(true);
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                r.monitor,
            )
            .await;
            // Whatever the controller was doing, the operator's play order
            // comes back.
            r.control.end().await;
            r.ssdp.stop().await;
            r.http.stop().await;
        }
        g.status = Status::Disabled;
    }

    /// Turn the renderer on and persist the switch.
    pub async fn enable(&self) -> Result<Envelope, VerbError> {
        {
            let mut g = self.inner.lock().await;
            if !g.persisted.enabled {
                g.persisted.enabled = true;
                state::save(&self.dir, &g.persisted)?;
            }
        }
        Ok(self.start().await)
    }

    /// Turn the renderer off, close every socket, and persist the switch.
    pub async fn disable(&self) -> Result<Envelope, VerbError> {
        self.stop().await;
        let env = {
            let mut g = self.inner.lock().await;
            if g.persisted.enabled {
                g.persisted.enabled = false;
                state::save(&self.dir, &g.persisted)?;
            }
            self.envelope_of(&g)
        };
        self.publish(&env).await;
        Ok(env)
    }

    /// Answer one verb. The payload is ignored (none of the verbs take one).
    pub async fn dispatch_verb(
        &self,
        verb: &str,
    ) -> Result<Vec<u8>, VerbError> {
        let env = match verb {
            "receiver.dlna.get_state" => self.get_state().await,
            "receiver.dlna.enable" => self.enable().await?,
            "receiver.dlna.disable" => self.disable().await?,
            other => return Err(VerbError::Unknown(other.to_string())),
        };
        Ok(
            serde_json::to_vec(&serde_json::json!({ "v": 1, "state": env }))
                .expect("envelope serialises"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn loopback_cfg() -> RuntimeConfig {
        RuntimeConfig {
            ssdp: SsdpConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                multicast: false,
                notify_target: "127.0.0.1:9".parse().unwrap(),
                interfaces: vec![Ipv4Addr::LOCALHOST],
                alive_every: Duration::from_secs(3600),
            },
            http_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            friendly_name: Some("Test renderer".into()),
        }
    }

    #[tokio::test]
    async fn a_new_renderer_is_disabled_and_binds_nothing() {
        let d = tempfile::tempdir().unwrap();
        let rt = RendererRuntime::open(d.path(), loopback_cfg()).unwrap();
        rt.reconcile_on_load().await;
        let e = rt.get_state().await;
        assert_eq!(e.state, "disabled");
        assert!(!e.enabled);
        assert_eq!(e.port, 0, "no port is chosen until the first enable");
        assert!(rt.inner.lock().await.running.is_none());
    }

    #[tokio::test]
    async fn enable_advertises_and_disable_closes_everything() {
        let d = tempfile::tempdir().unwrap();
        let rt = RendererRuntime::open(d.path(), loopback_cfg()).unwrap();
        let on = rt.enable().await.unwrap();
        assert_eq!(on.state, "advertising");
        assert!(on.enabled && on.port != 0);
        let port = on.port;
        // The listener is really there.
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();

        let off = rt.disable().await.unwrap();
        assert_eq!(off.state, "disabled");
        assert!(!off.enabled);
        // And really gone.
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err(),
            "a disabled renderer holds no listener"
        );
    }

    #[tokio::test]
    async fn the_switch_and_identity_survive_a_restart() {
        let d = tempfile::tempdir().unwrap();
        let (udn, port) = {
            let rt = RendererRuntime::open(d.path(), loopback_cfg()).unwrap();
            let on = rt.enable().await.unwrap();
            let r = (on.udn.clone(), on.port);
            rt.stop().await; // process exit: sockets close, switch stays on
            r
        };
        let rt = RendererRuntime::open(d.path(), loopback_cfg()).unwrap();
        rt.reconcile_on_load().await;
        let e = rt.get_state().await;
        assert_eq!(e.state, "advertising", "the persisted switch is honoured");
        assert_eq!(e.udn, udn);
        assert_eq!(e.port, port, "the port is stable across restarts");
        rt.stop().await;
    }

    #[tokio::test]
    async fn a_taken_port_degrades_with_a_reason_and_a_free_one_is_chosen_when_possible(
    ) {
        let d = tempfile::tempdir().unwrap();
        // Occupy a port, persist it as the renderer's, then enable.
        let squat = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let taken = squat.local_addr().unwrap().port();
        let mut p = state::load_or_create(d.path()).unwrap();
        p.port = taken;
        state::save(d.path(), &p).unwrap();
        let rt = RendererRuntime::open(d.path(), loopback_cfg()).unwrap();
        let e = rt.enable().await.unwrap();
        assert_eq!(e.state, "advertising");
        assert_ne!(e.port, taken, "a busy persisted port is replaced");
        rt.stop().await;
    }

    #[tokio::test]
    async fn an_ssdp_bind_failure_is_degraded_and_closes_the_description_server(
    ) {
        let d = tempfile::tempdir().unwrap();
        // An exclusive socket on the SSDP bind address makes the
        // responder's bind fail.
        let hog = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut cfg = loopback_cfg();
        cfg.ssdp.bind = hog.local_addr().unwrap();
        let rt = RendererRuntime::open(d.path(), cfg).unwrap();
        let e = rt.enable().await.unwrap();
        assert_eq!(e.state, "degraded");
        assert_eq!(e.reason.as_deref(), Some("ssdp_bind_failed"));
        assert!(e.enabled, "the switch stays on so a restart retries");
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", e.port))
                .await
                .is_err(),
            "a renderer that cannot advertise does not leave a server up"
        );
    }

    #[tokio::test]
    async fn verbs_return_the_envelope_and_unknown_verbs_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let rt = RendererRuntime::open(d.path(), loopback_cfg()).unwrap();
        let v: serde_json::Value = serde_json::from_slice(
            &rt.dispatch_verb("receiver.dlna.get_state").await.unwrap(),
        )
        .unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["state"]["state"], "disabled");
        assert!(rt.dispatch_verb("receiver.dlna.nope").await.is_err());
        let v: serde_json::Value = serde_json::from_slice(
            &rt.dispatch_verb("receiver.dlna.enable").await.unwrap(),
        )
        .unwrap();
        assert_eq!(v["state"]["state"], "advertising");
        rt.disable().await.unwrap();
    }
}
