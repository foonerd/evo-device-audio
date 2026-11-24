// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! SSDP: how a controller finds the renderer.
//!
//! Two halves. The message functions are pure: they build NOTIFY and
//! search-response text and parse an `M-SEARCH`, so the protocol is
//! tested without a socket. [`start`] binds the sockets and runs the
//! responder and the advertiser.
//!
//! The listening socket is opened with address and port reuse. The
//! `source.dlna` client binds the same group port for its passive NOTIFY
//! listener and falls back to search-only discovery if the bind fails, so
//! the renderer must never take the port exclusively.
//!
//! Only LAN-origin peers are answered; a datagram from anywhere else is
//! dropped without a reply.

use std::net::{
    IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket as StdUdp,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::description::{DEVICE_TYPE, SERVICES};
use crate::lan::is_lan_origin;

/// The SSDP multicast group.
pub const GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
/// The SSDP port.
pub const PORT: u16 = 1900;
/// `CACHE-CONTROL: max-age` carried by announcements and replies.
pub const MAX_AGE: u32 = 1800;

/// One advertised target: its `NT`/`ST` and its `USN`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Notification / search target.
    pub nt: String,
    /// Unique service name.
    pub usn: String,
}

/// Every target the renderer answers to.
pub fn targets(uuid: &str) -> Vec<Target> {
    let udn = format!("uuid:{uuid}");
    let mut v = vec![
        Target {
            nt: "upnp:rootdevice".into(),
            usn: format!("{udn}::upnp:rootdevice"),
        },
        Target {
            nt: udn.clone(),
            usn: udn.clone(),
        },
        Target {
            nt: DEVICE_TYPE.into(),
            usn: format!("{udn}::{DEVICE_TYPE}"),
        },
    ];
    for s in SERVICES {
        v.push(Target {
            nt: s.service_type.into(),
            usn: format!("{udn}::{}", s.service_type),
        });
    }
    v
}

/// Which of `all` a search target `st` selects.
pub fn matching<'a>(st: &str, all: &'a [Target]) -> Vec<&'a Target> {
    if st.eq_ignore_ascii_case("ssdp:all") {
        return all.iter().collect();
    }
    all.iter()
        .filter(|t| t.nt.eq_ignore_ascii_case(st))
        .collect()
}

/// A parsed `M-SEARCH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MSearch {
    /// Search target.
    pub st: String,
    /// Maximum wait the controller allows, in seconds (clamped 1..=5).
    pub mx: u64,
}

/// Parse an `M-SEARCH * HTTP/1.1` datagram. Anything else is `None`.
pub fn parse_msearch(buf: &[u8]) -> Option<MSearch> {
    let text = std::str::from_utf8(buf).ok()?;
    let mut lines = text.split("\r\n");
    let first = lines.next()?;
    let mut parts = first.split_whitespace();
    if parts.next()? != "M-SEARCH" || parts.next()? != "*" {
        return None;
    }
    let (mut man, mut st, mut mx) = (None, None, None);
    for l in lines {
        let Some((k, v)) = l.split_once(':') else {
            continue;
        };
        let v = v.trim();
        match k.trim().to_ascii_uppercase().as_str() {
            "MAN" => man = Some(v.trim_matches('"').to_string()),
            "ST" => st = Some(v.to_string()),
            "MX" => mx = v.parse::<u64>().ok(),
            _ => {}
        }
    }
    if !man?.eq_ignore_ascii_case("ssdp:discover") {
        return None;
    }
    Some(MSearch {
        st: st?,
        mx: mx.unwrap_or(1).clamp(1, 5),
    })
}

/// The kind of announcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notify {
    /// `ssdp:alive`.
    Alive,
    /// `ssdp:byebye`.
    ByeBye,
}

/// Build one NOTIFY datagram.
pub fn build_notify(
    kind: Notify,
    t: &Target,
    location: &str,
    server: &str,
) -> String {
    let mut s = String::new();
    s.push_str("NOTIFY * HTTP/1.1\r\n");
    s.push_str("HOST: 239.255.255.250:1900\r\n");
    match kind {
        Notify::Alive => {
            s.push_str(&format!("CACHE-CONTROL: max-age={MAX_AGE}\r\n"));
            s.push_str(&format!("LOCATION: {location}\r\n"));
            s.push_str(&format!("NT: {}\r\n", t.nt));
            s.push_str("NTS: ssdp:alive\r\n");
            s.push_str(&format!("SERVER: {server}\r\n"));
        }
        Notify::ByeBye => {
            s.push_str(&format!("NT: {}\r\n", t.nt));
            s.push_str("NTS: ssdp:byebye\r\n");
        }
    }
    s.push_str(&format!("USN: {}\r\n\r\n", t.usn));
    s
}

/// Build one unicast search response.
pub fn build_search_response(
    t: &Target,
    location: &str,
    server: &str,
) -> String {
    format!(
        "HTTP/1.1 200 OK\r\n\
         CACHE-CONTROL: max-age={MAX_AGE}\r\n\
         EXT:\r\n\
         LOCATION: {location}\r\n\
         SERVER: {server}\r\n\
         ST: {}\r\n\
         USN: {}\r\n\r\n",
        t.nt, t.usn
    )
}

/// The `SERVER` header value.
pub fn server_string() -> String {
    format!(
        "Linux UPnP/1.0 evo-receiver-dlna/{}",
        env!("CARGO_PKG_VERSION")
    )
}

// -------------------------------------------------------------------
// Sockets and tasks
// -------------------------------------------------------------------

/// Where and how the responder runs.
#[derive(Debug, Clone)]
pub struct SsdpConfig {
    /// Listening address: `0.0.0.0:1900` in production.
    pub bind: SocketAddr,
    /// Join the multicast group on each interface.
    pub multicast: bool,
    /// Where announcements are sent: the group in production.
    pub notify_target: SocketAddr,
    /// IPv4 addresses to announce on and join. Empty means "discover the
    /// LAN interfaces now and at every announcement".
    pub interfaces: Vec<Ipv4Addr>,
    /// Interval between `alive` rounds.
    pub alive_every: Duration,
}

impl SsdpConfig {
    /// The production configuration.
    pub fn production() -> Self {
        Self {
            bind: SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::UNSPECIFIED,
                PORT,
            )),
            multicast: true,
            notify_target: SocketAddr::V4(SocketAddrV4::new(GROUP, PORT)),
            interfaces: Vec::new(),
            alive_every: Duration::from_secs(u64::from(MAX_AGE) / 2),
        }
    }
}

/// What the responder advertises.
#[derive(Debug, Clone)]
pub struct Identity {
    /// Device UUID, without the `uuid:` prefix.
    pub uuid: String,
    /// TCP port the description is served on.
    pub port: u16,
}

/// Why the responder could not start.
#[derive(Debug, thiserror::Error)]
pub enum SsdpError {
    /// Binding or configuring the socket failed.
    #[error("ssdp socket: {0}")]
    Socket(#[from] std::io::Error),
}

/// A running responder. Dropping it without [`SsdpHandle::stop`] aborts
/// the tasks but sends no `byebye`.
pub struct SsdpHandle {
    shutdown: watch::Sender<bool>,
    responder: JoinHandle<()>,
    advertiser: JoinHandle<()>,
    local_addr: SocketAddr,
    identity: Identity,
    cfg: SsdpConfig,
}

impl SsdpHandle {
    /// The address the responder socket is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Say goodbye, then stop. Best effort: a send failure is logged.
    pub async fn stop(self) {
        let _ = self.shutdown.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            let _ = self.responder.await;
            let _ = self.advertiser.await;
        })
        .await;
        announce(&self.cfg, &self.identity, Notify::ByeBye).await;
    }
}

fn open_listener(cfg: &SsdpConfig) -> std::io::Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    #[cfg(unix)]
    sock.set_reuse_port(true)?;
    sock.bind(&cfg.bind.into())?;
    sock.set_nonblocking(true)?;
    if cfg.multicast {
        for ip in lan_interfaces(cfg) {
            // A repeat join on an interface already joined is harmless.
            if let Err(e) = sock.join_multicast_v4(&GROUP, &ip) {
                tracing::debug!(%ip, error = %e, "ssdp multicast join failed");
            }
        }
    }
    UdpSocket::from_std(sock.into())
}

/// LAN IPv4 interface addresses to use.
fn lan_interfaces(cfg: &SsdpConfig) -> Vec<Ipv4Addr> {
    if !cfg.interfaces.is_empty() {
        return cfg.interfaces.clone();
    }
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| !i.is_loopback())
        .filter_map(|i| match i.ip() {
            IpAddr::V4(v4) if is_lan_origin(IpAddr::V4(v4)) => Some(v4),
            _ => None,
        })
        .collect()
}

/// The local IPv4 address the kernel would use to reach `peer`.
fn local_ip_towards(peer: IpAddr) -> Option<Ipv4Addr> {
    let s = StdUdp::bind("0.0.0.0:0").ok()?;
    s.connect(SocketAddr::new(peer, 9)).ok()?;
    match s.local_addr().ok()?.ip() {
        IpAddr::V4(v4) => Some(v4),
        _ => None,
    }
}

fn location_for(ip: Ipv4Addr, port: u16) -> String {
    format!("http://{ip}:{port}/description.xml")
}

/// Send one round of announcements on every interface.
async fn announce(cfg: &SsdpConfig, id: &Identity, kind: Notify) {
    let all = targets(&id.uuid);
    let server = server_string();
    let ifaces = lan_interfaces(cfg);
    let cfg = cfg.clone();
    let port = id.port;
    let _ = tokio::task::spawn_blocking(move || {
        for ip in ifaces {
            let Ok(sock) =
                Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
            else {
                continue;
            };
            if cfg.multicast {
                let _ = sock.set_multicast_if_v4(&ip);
                let _ = sock.set_multicast_ttl_v4(2);
            }
            if sock
                .bind(&SocketAddr::V4(SocketAddrV4::new(ip, 0)).into())
                .is_err()
            {
                continue;
            }
            let loc = location_for(ip, port);
            // Three rounds: SSDP is lossy and the specification asks for
            // repeats.
            for _ in 0..3 {
                for t in &all {
                    let msg = build_notify(kind, t, &loc, &server);
                    let _ =
                        sock.send_to(msg.as_bytes(), &cfg.notify_target.into());
                }
                std::thread::sleep(Duration::from_millis(60));
            }
        }
    })
    .await;
}

/// A small fixed-window limiter so one chatty peer cannot turn the
/// responder into a firehose.
struct Window {
    start: Instant,
    count: u32,
}

impl Window {
    fn allow(&mut self, per_second: u32) -> bool {
        if self.start.elapsed() >= Duration::from_secs(1) {
            self.start = Instant::now();
            self.count = 0;
        }
        if self.count >= per_second {
            return false;
        }
        self.count += 1;
        true
    }
}

/// Bind the sockets, announce, and run the responder and the advertiser.
pub async fn start(
    cfg: SsdpConfig,
    identity: Identity,
) -> Result<SsdpHandle, SsdpError> {
    let sock = Arc::new(open_listener(&cfg)?);
    let local_addr = sock.local_addr()?;
    let (tx, rx) = watch::channel(false);

    let responder = {
        let sock = Arc::clone(&sock);
        let id = identity.clone();
        let mut rx = rx.clone();
        tokio::spawn(async move {
            let all = targets(&id.uuid);
            let server = server_string();
            let mut win = Window {
                start: Instant::now(),
                count: 0,
            };
            let mut buf = [0u8; 2048];
            loop {
                tokio::select! {
                    _ = rx.changed() => break,
                    got = sock.recv_from(&mut buf) => {
                        let Ok((n, peer)) = got else { continue };
                        if !is_lan_origin(peer.ip()) {
                            continue;
                        }
                        let Some(ms) = parse_msearch(&buf[..n]) else { continue };
                        let hits = matching(&ms.st, &all);
                        if hits.is_empty() || !win.allow(50) {
                            continue;
                        }
                        let Some(ip) = local_ip_towards(peer.ip()) else { continue };
                        let loc = location_for(ip, id.port);
                        // Spread replies over part of the controller's
                        // window, as the specification asks.
                        let span = (ms.mx * 1000 / 2).clamp(1, 1500);
                        let jitter = u64::from(
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.subsec_nanos())
                                .unwrap_or(0),
                        ) % span;
                        tokio::time::sleep(Duration::from_millis(jitter)).await;
                        for t in hits {
                            let msg = build_search_response(t, &loc, &server);
                            let _ = sock.send_to(msg.as_bytes(), peer).await;
                        }
                    }
                }
            }
        })
    };

    let advertiser = {
        let cfg = cfg.clone();
        let id = identity.clone();
        let mut rx = rx.clone();
        tokio::spawn(async move {
            announce(&cfg, &id, Notify::Alive).await;
            loop {
                tokio::select! {
                    _ = rx.changed() => break,
                    _ = tokio::time::sleep(cfg.alive_every) => {
                        announce(&cfg, &id, Notify::Alive).await;
                    }
                }
            }
        })
    };

    Ok(SsdpHandle {
        shutdown: tx,
        responder,
        advertiser,
        local_addr,
        identity,
        cfg,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const U: &str = "11111111-2222-3333-4444-555555555555";

    #[test]
    fn six_targets_with_the_right_usn_shape() {
        let t = targets(U);
        assert_eq!(t.len(), 6);
        assert_eq!(t[0].nt, "upnp:rootdevice");
        assert_eq!(t[0].usn, format!("uuid:{U}::upnp:rootdevice"));
        assert_eq!(t[1].nt, format!("uuid:{U}"));
        assert_eq!(t[1].usn, format!("uuid:{U}"));
        assert!(t.iter().any(|x| x.nt == DEVICE_TYPE));
        for s in SERVICES {
            assert!(t.iter().any(|x| x.nt == s.service_type));
        }
    }

    #[test]
    fn search_targets_select_what_they_name() {
        let t = targets(U);
        assert_eq!(matching("ssdp:all", &t).len(), 6);
        assert_eq!(matching("upnp:rootdevice", &t).len(), 1);
        assert_eq!(matching(DEVICE_TYPE, &t).len(), 1);
        assert_eq!(
            matching("urn:schemas-upnp-org:service:AVTransport:1", &t).len(),
            1
        );
        assert!(matching("urn:schemas-upnp-org:device:MediaServer:1", &t)
            .is_empty());
        assert!(matching(
            "urn:schemas-upnp-org:service:ContentDirectory:1",
            &t
        )
        .is_empty());
    }

    #[test]
    fn a_real_msearch_parses() {
        let m = b"M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 3\r\nST: ssdp:all\r\n\r\n";
        assert_eq!(
            parse_msearch(m),
            Some(MSearch {
                st: "ssdp:all".into(),
                mx: 3
            })
        );
    }

    #[test]
    fn mx_is_clamped_and_headers_are_case_insensitive() {
        let m = b"M-SEARCH * HTTP/1.1\r\nman: \"ssdp:discover\"\r\nmx: 900\r\nst: upnp:rootdevice\r\n\r\n";
        assert_eq!(parse_msearch(m).unwrap().mx, 5);
    }

    #[test]
    fn not_a_search_is_not_parsed() {
        assert!(parse_msearch(b"NOTIFY * HTTP/1.1\r\nNT: x\r\n\r\n").is_none());
        assert!(parse_msearch(
            b"M-SEARCH * HTTP/1.1\r\nMAN: \"other\"\r\nST: a\r\n\r\n"
        )
        .is_none());
        assert!(parse_msearch(
            b"M-SEARCH * HTTP/1.1\r\nMAN: \"ssdp:discover\"\r\n\r\n"
        )
        .is_none());
        assert!(parse_msearch(&[0xff, 0xfe]).is_none());
    }

    #[test]
    fn alive_carries_location_and_byebye_does_not() {
        let t = &targets(U)[0];
        let a = build_notify(
            Notify::Alive,
            t,
            "http://10.0.0.5:49000/description.xml",
            "S",
        );
        assert!(a.starts_with("NOTIFY * HTTP/1.1\r\n"));
        assert!(a.contains("NTS: ssdp:alive\r\n"));
        assert!(
            a.contains("LOCATION: http://10.0.0.5:49000/description.xml\r\n")
        );
        assert!(a.contains("CACHE-CONTROL: max-age=1800\r\n"));
        assert!(a.ends_with("\r\n\r\n"));
        let b = build_notify(Notify::ByeBye, t, "ignored", "S");
        assert!(b.contains("NTS: ssdp:byebye\r\n"));
        assert!(!b.contains("LOCATION"));
    }

    #[test]
    fn a_response_names_the_target_and_the_location() {
        let t = &targets(U)[2];
        let r =
            build_search_response(t, "http://10.0.0.5:1/description.xml", "S");
        assert!(r.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(r.contains(&format!("ST: {DEVICE_TYPE}\r\n")));
        assert!(r.contains("EXT:\r\n"));
        assert!(r.contains("LOCATION: http://10.0.0.5:1/description.xml\r\n"));
    }

    /// The DLNA client's NOTIFY listener holds the SSDP port with address
    /// reuse. The responder must bind the same address beside it, in
    /// either order.
    #[tokio::test]
    async fn the_responder_shares_the_port_with_a_reuse_listener() {
        use socket2::{Domain, Protocol, Socket, Type};
        // The client's socket, opened first.
        let client =
            Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
                .unwrap();
        client.set_reuse_address(true).unwrap();
        let any: SocketAddr = "127.0.0.1:0".parse().unwrap();
        client.bind(&any.into()).unwrap();
        let addr = client.local_addr().unwrap().as_socket().unwrap();
        let cfg = SsdpConfig {
            bind: addr,
            multicast: false,
            notify_target: "127.0.0.1:9".parse().unwrap(),
            interfaces: vec![Ipv4Addr::LOCALHOST],
            alive_every: Duration::from_secs(3600),
        };
        let h = start(
            cfg.clone(),
            Identity {
                uuid: U.into(),
                port: 1,
            },
        )
        .await
        .expect("the responder binds beside a reuse listener");
        assert_eq!(h.local_addr(), addr);
        // And a reuse listener can still bind after the responder.
        let late = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
            .unwrap();
        late.set_reuse_address(true).unwrap();
        late.bind(&addr.into())
            .expect("a client binding after the responder shares the port");
        h.stop().await;
    }

    #[tokio::test]
    async fn a_loopback_search_is_answered_and_a_foreign_one_is_not() {
        // A unicast responder on loopback stands in for the multicast
        // group: the parsing, matching and reply path is the real one.
        let cfg = SsdpConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            multicast: false,
            notify_target: "127.0.0.1:9".parse().unwrap(),
            interfaces: vec![Ipv4Addr::LOCALHOST],
            alive_every: Duration::from_secs(3600),
        };
        let h = start(
            cfg,
            Identity {
                uuid: U.into(),
                port: 49123,
            },
        )
        .await
        .unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client
            .send_to(
                b"M-SEARCH * HTTP/1.1\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: urn:schemas-upnp-org:service:AVTransport:1\r\n\r\n",
                h.local_addr(),
            )
            .await
            .unwrap();
        let mut buf = [0u8; 1024];
        let (n, _) = tokio::time::timeout(
            Duration::from_secs(3),
            client.recv_from(&mut buf),
        )
        .await
        .expect("a reply within the window")
        .unwrap();
        let reply = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(reply.starts_with("HTTP/1.1 200 OK"), "{reply}");
        assert!(
            reply.contains("LOCATION: http://127.0.0.1:49123/description.xml"),
            "{reply}"
        );
        assert!(reply.contains("AVTransport:1"), "{reply}");

        // A search for something the renderer is not gets nothing.
        client
            .send_to(
                b"M-SEARCH * HTTP/1.1\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: urn:schemas-upnp-org:device:MediaServer:1\r\n\r\n",
                h.local_addr(),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(1800),
                client.recv_from(&mut buf)
            )
            .await
            .is_err(),
            "a MediaServer search must not be answered by a renderer"
        );
        h.stop().await;
    }
}
