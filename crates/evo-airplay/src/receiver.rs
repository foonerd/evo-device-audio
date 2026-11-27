// Copyright (c) 2026 Stefan Arentz; modifications (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0 AND MIT
//! The embedding facade: configure a [`Receiver`], hand it a sink factory and
//! an event channel, and run it on your own tokio runtime.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use tokio::net::TcpListener;
use tracing::warn;

use crate::events::EventSender;
use crate::identity::Identity;
use crate::policy::{SessionControl, SessionGate, SingleSession};
use crate::server::{serve, Context};
use crate::sink::{AudioSink, SinkError, SinkFactory};
use crate::{info, mac, Config};

const DEFAULT_NAME: &str = "Evo Audio";
const DEFAULT_PORT: u16 = 7000;
const DEFAULT_MODEL: &str = "EvoAudio,1";
const DEFAULT_SOURCE_VERSION: &str = "366.0";
/// shairport-sync's known-good AirPlay 2 features (`0x0001_8340_405C_4A00`):
/// transient pairing (bit 48) plus AirPlay 2 audio — with the metadata bits
/// 15/16/17 (covers, progress, DAAP text) set on top here, because senders
/// only push track metadata and artwork when those are advertised (observed
/// against an iPhone: a first capture with the bits clear showed zero
/// metadata `SET_PARAMETER`s). Getting the mask wrong makes senders offer
/// AirPlay 1 or nothing at all — per shairport-sync and pyatv; not tested
/// here.
const DEFAULT_FEATURES: u64 = 0x0001_8340_405F_CA00;
const DEFAULT_STATUS_FLAGS: u32 = 0x4;
/// Status-flag bit 7 ("Password required"), which makes Apple senders prompt
/// for the password and pair with it instead of silently using transient
/// `3939` (proven on iOS 26). Shairport-sync sets the same bit when a
/// password is configured.
const PASSWORD_REQUIRED_FLAG: u32 = 1 << 7;

/// Advertised status flags: audio-attached, plus "password required" when a
/// password is configured.
fn status_flags(password: bool) -> u32 {
    if password {
        DEFAULT_STATUS_FLAGS | PASSWORD_REQUIRED_FLAG
    } else {
        DEFAULT_STATUS_FLAGS
    }
}
/// Locally-administered fallback (starts with 0x02) when discovery finds no
/// interface MAC.
const FALLBACK_MAC: [u8; 6] = [0x02, 0x4f, 0x41, 0x50, 0x32, 0x00];

/// Configures a [`Receiver`]. Create one with [`Receiver::builder`].
///
/// The identity (Ed25519 key + `pi` UUID) must be stable across restarts —
/// senders remember a receiver by it — so exactly one of
/// [`identity`](Self::identity) or [`identity_path`](Self::identity_path) is
/// required; there is no ephemeral default.
pub struct ReceiverBuilder {
    name: String,
    port: u16,
    mac: Option<[u8; 6]>,
    identity: Option<Identity>,
    identity_path: Option<PathBuf>,
    password: Option<String>,
    gate: Option<Arc<dyn SessionGate>>,
}

impl ReceiverBuilder {
    fn new() -> ReceiverBuilder {
        ReceiverBuilder {
            name: DEFAULT_NAME.to_string(),
            port: DEFAULT_PORT,
            mac: None,
            identity: None,
            identity_path: None,
            password: None,
            gate: None,
        }
    }

    /// The receiver name senders see in the AirPlay picker.
    /// Default: `Evo Audio`.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// The control port to listen on (also advertised). Default: 7000.
    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// The MAC used as the AirPlay `deviceid`. Default: discovered from the
    /// first up, non-loopback network interface, with a fixed
    /// locally-administered fallback.
    pub fn mac(mut self, mac: [u8; 6]) -> Self {
        self.mac = Some(mac);
        self
    }

    /// Use this identity. Mutually exclusive with
    /// [`identity_path`](Self::identity_path); the host owns persistence.
    pub fn identity(mut self, identity: Identity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Load the identity from this file, generating and persisting one on
    /// first run.
    pub fn identity_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.identity_path = Some(path.into());
        self
    }

    /// The host's gate over who connects and whether a stream starts.
    /// Default: [`SingleSession`] (one stream at a time, from anyone).
    pub fn gate(mut self, gate: Arc<dyn SessionGate>) -> Self {
        self.gate = Some(gate);
        self
    }

    /// Require this password to pair: a sender must enter it. "Password" is
    /// Apple's word — iOS and macOS show a password dialog and accept
    /// alphanumerics, not just digits — and the receiver advertises
    /// "password required" (status-flag bit 7). Unset → the standard
    /// transient code `3939`. Never logged.
    pub fn password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }

    /// Renamed to [`password`](Self::password) — iOS and macOS call it the
    /// password, and accept more than digits.
    #[deprecated(since = "0.5.0", note = "renamed to `password`")]
    pub fn pincode(self, pincode: impl Into<String>) -> Self {
        self.password(pincode)
    }

    /// Resolve the identity and MAC and produce a runnable [`Receiver`].
    pub fn build(self) -> io::Result<Receiver> {
        if let Some(password) = &self.password {
            if password.is_empty()
                || password.len() > 128
                || password.chars().any(char::is_control)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "the password must be 1 to 128 printable characters",
                ));
            }
        }
        let identity = match (self.identity, &self.identity_path) {
            (Some(identity), None) => identity,
            (None, Some(path)) => Identity::load_or_create(path)?,
            (Some(_), Some(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "set either identity or identity_path, not both",
                ))
            }
            (None, None) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "an identity is required (identity or identity_path): \
                     senders remember a receiver by it, so it must be stable",
                ))
            }
        };
        let mac = self.mac.or_else(mac::discover).unwrap_or_else(|| {
            warn!("no network interface MAC found, using a fixed fallback");
            FALLBACK_MAC
        });
        Ok(Receiver {
            config: Config {
                name: self.name,
                port: self.port,
                mac,
                model: DEFAULT_MODEL.to_string(),
                source_version: DEFAULT_SOURCE_VERSION.to_string(),
                features: DEFAULT_FEATURES,
                status_flags: status_flags(self.password.is_some()),
                password: self.password,
            },
            identity,
            gate: self.gate.unwrap_or_else(|| Arc::new(SingleSession::new())),
            control: SessionControl::new(),
        })
    }
}

/// An AirPlay 2 audio receiver: senders discover it, pair with it, and stream
/// to it; the host gets decoded PCM (via its [`AudioSink`]) and
/// [`Event`](crate::Event)s. One sender → one stream → one output.
pub struct Receiver {
    config: Config,
    identity: Identity,
    gate: Arc<dyn SessionGate>,
    control: SessionControl,
}

impl Receiver {
    /// Start configuring a receiver. See [`ReceiverBuilder`] for the options;
    /// an identity is the one required setting.
    pub fn builder() -> ReceiverBuilder {
        ReceiverBuilder::new()
    }

    /// The resolved configuration (name, port, deviceid, features).
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The stable device identity (`pk`, `pi`).
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// The handle with which the host ends the running session from outside.
    /// Clones share the same session.
    pub fn session_control(&self) -> SessionControl {
        self.control.clone()
    }

    /// Whether the receiver has no password, so that any device able to reach
    /// it may pair and stream. The pairing code is then the protocol's fixed,
    /// public one; say so to the operator.
    pub fn is_open(&self) -> bool {
        self.config.password.is_none()
    }

    /// The `_airplay._tcp` TXT records. The host owns mDNS registration:
    /// advertise these on [`Config::port`] under the receiver's name.
    pub fn txt_records(&self) -> Vec<String> {
        info::txt_records(&self.config, &self.identity)
    }

    /// The `_raop._tcp` TXT records. AirPlay 2 receivers advertise this
    /// service alongside `_airplay._tcp`, on the same port, under the
    /// instance name [`raop_instance_name`](Self::raop_instance_name).
    pub fn raop_txt_records(&self) -> Vec<String> {
        info::raop_txt_records(&self.config, &self.identity)
    }

    /// The `_raop._tcp` instance name: the device id without its colons,
    /// an `@`, then the receiver's name.
    pub fn raop_instance_name(&self) -> String {
        info::raop_instance_name(&self.config)
    }

    /// Serve AirPlay on the caller's runtime until a listener error.
    ///
    /// `sink_factory` is invoked at `SETUP` phase 2 with the negotiated
    /// sample rate and channel count, once per stream. If it fails, the
    /// stream is refused and nothing is announced. The sink is then fed
    /// from a dedicated playback thread (see [`AudioSink`]). Session
    /// milestones are reported on `events`; dropping the receiving half is
    /// allowed and simply discards them.
    ///
    /// The receiver advertises nothing itself. Cancellation: dropping the
    /// returned future (e.g. in `select!`, or by aborting the task running it)
    /// closes the listener and every accepted connection, ends the running
    /// session and releases the host's gate.
    pub async fn run<F>(
        self,
        sink_factory: F,
        events: EventSender,
    ) -> io::Result<()>
    where
        F: Fn(u32, u8) -> Result<Box<dyn AudioSink>, SinkError>
            + Send
            + Sync
            + 'static,
    {
        self.bind().await?.run(sink_factory, events).await
    }

    /// Bind the control port without serving yet, so the host learns the port
    /// it actually got (port `0` asks for any free one) and can refuse to go
    /// on if binding fails, before it advertises anything.
    pub async fn bind(self) -> io::Result<BoundReceiver> {
        // Dual-stack if possible (IPv4 clients arrive v4-mapped), else IPv4.
        let port = self.config.port;
        let listener = match TcpListener::bind(SocketAddr::from((
            Ipv6Addr::UNSPECIFIED,
            port,
        )))
        .await
        {
            Ok(l) => l,
            Err(_) => TcpListener::bind(SocketAddr::from((
                Ipv4Addr::UNSPECIFIED,
                port,
            )))
            .await
            .map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("cannot bind control port {port}: {e}"),
                )
            })?,
        };
        let bound = listener.local_addr()?.port();
        Ok(BoundReceiver {
            receiver: self,
            listener,
            port: bound,
        })
    }
}

/// A [`Receiver`] whose control port is bound and not yet served.
pub struct BoundReceiver {
    receiver: Receiver,
    listener: TcpListener,
    port: u16,
}

impl BoundReceiver {
    /// The control port actually bound.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The receiver this was bound from, for its records and name.
    pub fn receiver(&self) -> &Receiver {
        &self.receiver
    }

    /// Serve until a listener error. The same contract as [`Receiver::run`]:
    /// dropping the returned future closes the listener and every accepted
    /// connection.
    pub async fn run<F>(
        self,
        sink_factory: F,
        events: EventSender,
    ) -> io::Result<()>
    where
        F: Fn(u32, u8) -> Result<Box<dyn AudioSink>, SinkError>
            + Send
            + Sync
            + 'static,
    {
        let Receiver {
            config,
            identity,
            gate,
            control,
        } = self.receiver;
        let sink_factory: SinkFactory = Arc::new(sink_factory);
        let context = Arc::new(Context {
            config,
            identity,
            sink_factory,
            events,
            gate,
            control,
            limiter: crate::limiter::PairingLimiter::default(),
        });
        serve(self.listener, context).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_sets_password_required_status_flag() {
        // No password: just audio attached.
        assert_eq!(status_flags(false), 0x4);
        // Password set: also advertise "password required" (status bit 7),
        // which is what makes iOS prompt for it.
        assert_eq!(status_flags(true), 0x4 | (1 << 7));
    }

    #[test]
    fn build_applies_the_password() {
        let receiver = Receiver::builder()
            .identity(Identity::generate())
            .password("sekrit1")
            .build()
            .unwrap();
        assert_eq!(receiver.config().password.as_deref(), Some("sekrit1"));
        assert_eq!(receiver.config().status_flags, 0x4 | (1 << 7));
    }

    #[test]
    #[allow(deprecated)]
    fn the_deprecated_pincode_spelling_still_builds() {
        let receiver = Receiver::builder()
            .identity(Identity::generate())
            .pincode("1212")
            .build()
            .unwrap();
        assert_eq!(receiver.config().password.as_deref(), Some("1212"));
    }

    #[test]
    fn build_requires_an_identity() {
        let err = Receiver::builder()
            .build()
            .err()
            .expect("no identity must be rejected");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

        let err = Receiver::builder()
            .identity(Identity::generate())
            .identity_path("/nonexistent/identity")
            .build()
            .err()
            .expect("both identity sources must be rejected");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn build_applies_defaults_and_overrides() {
        let receiver = Receiver::builder()
            .identity(Identity::generate())
            .mac([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
            .build()
            .unwrap();
        let config = receiver.config();
        assert_eq!(config.name, "Evo Audio");
        assert_eq!(config.port, 7000);
        assert_eq!(config.features, 0x0001_8340_405F_CA00);
        assert_eq!(config.device_id(), "AA:BB:CC:DD:EE:FF");

        let receiver = Receiver::builder()
            .identity(Identity::generate())
            .name("Office")
            .port(7100)
            .build()
            .unwrap();
        assert_eq!(receiver.config().name, "Office");
        assert_eq!(receiver.config().port, 7100);
    }

    #[test]
    fn a_receiver_without_a_password_is_open() {
        let open = Receiver::builder()
            .identity(Identity::generate())
            .build()
            .unwrap();
        assert!(open.is_open());
        let closed = Receiver::builder()
            .identity(Identity::generate())
            .password("sekrit1")
            .build()
            .unwrap();
        assert!(!closed.is_open());
    }

    #[test]
    fn a_password_that_cannot_be_typed_is_refused_at_build() {
        for bad in ["", "line\nbreak", &"x".repeat(129)] {
            let outcome = Receiver::builder()
                .identity(Identity::generate())
                .password(bad)
                .build();
            let err = outcome.err().expect("refused");
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{bad:?}");
        }
        assert!(Receiver::builder()
            .identity(Identity::generate())
            .password(
                "exactly sixty four characters long and nothing else here ok"
            )
            .build()
            .is_ok());
    }

    #[test]
    fn raop_records_describe_the_same_device() {
        let receiver = Receiver::builder()
            .identity(Identity::generate())
            .mac([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
            .name("Office")
            .build()
            .unwrap();
        let records = receiver.raop_txt_records();
        for want in [
            "cn=0,1",
            "da=true",
            "et=0,4",
            "md=0,1,2",
            "tp=UDP",
            "vn=65537",
            "sf=0x4",
            "ft=0x405FCA00,0x18340",
        ] {
            assert!(records.iter().any(|r| r == want), "missing {want}");
        }
        assert!(records.iter().any(|r| r.starts_with("pk=")));
        assert!(!records.iter().any(|r| r.starts_with("pw=")));
        assert_eq!(receiver.raop_instance_name(), "AABBCCDDEEFF@Office");
    }

    #[test]
    fn raop_records_flag_a_required_password() {
        let receiver = Receiver::builder()
            .identity(Identity::generate())
            .password("sekrit1")
            .build()
            .unwrap();
        let records = receiver.raop_txt_records();
        assert!(records.iter().any(|r| r == "pw=true"));
        assert!(records.iter().any(|r| r == "sf=0x84"));
    }

    #[test]
    fn txt_records_match_identity_and_config() {
        let receiver = Receiver::builder()
            .identity(Identity::generate())
            .mac([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
            .build()
            .unwrap();
        let records = receiver.txt_records();
        assert!(records.iter().any(|r| r == "deviceid=AA:BB:CC:DD:EE:FF"));
        let pk = format!("pk={}", receiver.identity().public_key_hex());
        assert!(records.iter().any(|r| r == &pk));
    }

    #[tokio::test]
    async fn bind_reports_the_port_it_got_and_a_busy_port_is_an_error() {
        let first = Receiver::builder()
            .identity(Identity::generate())
            .port(0)
            .build()
            .unwrap()
            .bind()
            .await
            .unwrap();
        assert_ne!(first.port(), 0, "port 0 asks for any free port");
        let busy = Receiver::builder()
            .identity(Identity::generate())
            .port(first.port())
            .build()
            .unwrap()
            .bind()
            .await;
        let err = busy.err().expect("the port is taken");
        assert!(err.to_string().contains("cannot bind control port"));
    }
}
