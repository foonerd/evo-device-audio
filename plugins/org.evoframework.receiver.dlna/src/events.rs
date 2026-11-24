// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! GENA eventing: a controller subscribes and is told when the state moves.
//!
//! Bounded on every axis: a cap on subscribers in total and per peer, a
//! lifetime that expires, a delivery that gives up after a few seconds and
//! drops a subscriber that keeps failing.
//!
//! The callback is pinned to the peer: the host in a `CALLBACK` header must
//! be the address the `SUBSCRIBE` came from. The renderer therefore never
//! opens a connection to a third party on a controller's say-so. It makes
//! no other outbound request.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::control::{AvSnapshot, Control};
use crate::description::{service, xml_escape};

/// Most live subscriptions.
pub const MAX_SUBSCRIPTIONS: usize = 16;
/// Most live subscriptions from one peer.
pub const MAX_PER_PEER: usize = 4;
/// Longest lifetime granted, in seconds.
pub const MAX_TIMEOUT_SECS: u64 = 1800;
/// Shortest lifetime granted, in seconds.
pub const MIN_TIMEOUT_SECS: u64 = 60;
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_FAILURES: u8 = 3;

/// Why a subscription request was refused: the HTTP status to answer with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Malformed request.
    BadRequest,
    /// The callback is not the peer, or the SID is unknown or expired.
    PreconditionFailed,
    /// Too many subscriptions.
    Unavailable,
}

impl Refusal {
    /// The status code.
    pub fn status(self) -> u16 {
        match self {
            Refusal::BadRequest => 400,
            Refusal::PreconditionFailed => 412,
            Refusal::Unavailable => 503,
        }
    }
}

#[derive(Debug, Clone)]
struct Sub {
    service: &'static str,
    host: SocketAddr,
    path: String,
    seq: u32,
    expires: Instant,
    failures: u8,
}

/// A granted subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Granted {
    /// The subscription id, with its `uuid:` prefix.
    pub sid: String,
    /// The granted lifetime in seconds.
    pub timeout_secs: u64,
    /// Whether this is a new subscription (needs the initial event) or a renewal.
    pub is_new: bool,
}

/// The subscription table.
#[derive(Default)]
pub struct Subscriptions {
    inner: Mutex<HashMap<String, Sub>>,
}

/// Parse the first `<http://host[:port]/path>` of a CALLBACK header.
pub fn parse_callback(h: &str) -> Option<(IpAddr, u16, String)> {
    let start = h.find('<')?;
    let end = h[start..].find('>')? + start;
    let url = &h[start + 1..end];
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => {
            (h, p.parse::<u16>().ok()?)
        }
        _ => (authority, 80),
    };
    let ip: IpAddr =
        host.trim_matches(|c| c == '[' || c == ']').parse().ok()?;
    if path.len() > 512 || path.chars().any(|c| c.is_control() || c == ' ') {
        return None;
    }
    Some((ip, port, path.to_string()))
}

/// Parse `Second-N` (or `infinite`) into a clamped lifetime.
pub fn parse_timeout(h: Option<&str>) -> u64 {
    let n = h
        .and_then(|v| v.trim().strip_prefix("Second-"))
        .and_then(|n| n.trim().parse::<u64>().ok())
        .unwrap_or(MAX_TIMEOUT_SECS);
    n.clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS)
}

impl Subscriptions {
    /// An empty table.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Number of live subscriptions.
    pub fn len(&self) -> usize {
        self.inner.lock().expect("subs").len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Handle a `SUBSCRIBE`: a new subscription (CALLBACK + NT) or a renewal (SID).
    pub fn subscribe(
        &self,
        service_name: &str,
        peer: IpAddr,
        callback: Option<&str>,
        nt: Option<&str>,
        timeout: Option<&str>,
        sid: Option<&str>,
    ) -> Result<Granted, Refusal> {
        let def = service(service_name).ok_or(Refusal::PreconditionFailed)?;
        let secs = parse_timeout(timeout);
        let mut g = self.inner.lock().expect("subs");
        let now = Instant::now();
        g.retain(|_, s| s.expires > now);

        if let Some(sid) = sid {
            // A renewal carries a SID and neither CALLBACK nor NT.
            if callback.is_some() || nt.is_some() {
                return Err(Refusal::BadRequest);
            }
            let s = g.get_mut(sid).ok_or(Refusal::PreconditionFailed)?;
            if s.service != def.name || s.host.ip() != peer {
                return Err(Refusal::PreconditionFailed);
            }
            s.expires = now + Duration::from_secs(secs);
            return Ok(Granted {
                sid: sid.to_string(),
                timeout_secs: secs,
                is_new: false,
            });
        }

        if nt.map(str::trim) != Some("upnp:event") {
            return Err(Refusal::PreconditionFailed);
        }
        let (ip, port, path) =
            parse_callback(callback.ok_or(Refusal::PreconditionFailed)?)
                .ok_or(Refusal::PreconditionFailed)?;
        if ip != peer {
            // A callback that is not the controller itself is refused.
            return Err(Refusal::PreconditionFailed);
        }
        if g.len() >= MAX_SUBSCRIPTIONS
            || g.values().filter(|s| s.host.ip() == peer).count()
                >= MAX_PER_PEER
        {
            return Err(Refusal::Unavailable);
        }
        let sid = format!("uuid:{}", uuid::Uuid::new_v4());
        g.insert(
            sid.clone(),
            Sub {
                service: def.name,
                host: SocketAddr::new(ip, port),
                path,
                seq: 0,
                expires: now + Duration::from_secs(secs),
                failures: 0,
            },
        );
        Ok(Granted {
            sid,
            timeout_secs: secs,
            is_new: true,
        })
    }

    /// Handle an `UNSUBSCRIBE`. `false` when the SID is not known.
    pub fn unsubscribe(&self, sid: &str, peer: IpAddr) -> bool {
        let mut g = self.inner.lock().expect("subs");
        match g.get(sid) {
            Some(s) if s.host.ip() == peer => {
                g.remove(sid);
                true
            }
            _ => false,
        }
    }

    /// Send `body` to the subscriber `sid` of its service (the initial event).
    pub async fn deliver_to(&self, sid: &str, body: &str) {
        let target = {
            let mut g = self.inner.lock().expect("subs");
            g.get_mut(sid).map(|s| {
                let seq = s.seq;
                s.seq = s.seq.wrapping_add(1);
                (s.host, s.path.clone(), seq)
            })
        };
        if let Some((host, path, seq)) = target {
            let ok = send_notify(host, &path, sid, seq, body).await;
            self.record(sid, ok);
        }
    }

    /// Send `body` to every subscriber of `service_name`.
    pub async fn notify_all(&self, service_name: &str, body: &str) {
        let targets: Vec<(String, SocketAddr, String, u32)> = {
            let mut g = self.inner.lock().expect("subs");
            let now = Instant::now();
            g.retain(|_, s| s.expires > now);
            g.iter_mut()
                .filter(|(_, s)| s.service == service_name)
                .map(|(sid, s)| {
                    let seq = s.seq;
                    s.seq = s.seq.wrapping_add(1);
                    (sid.clone(), s.host, s.path.clone(), seq)
                })
                .collect()
        };
        for (sid, host, path, seq) in targets {
            let ok = send_notify(host, &path, &sid, seq, body).await;
            self.record(&sid, ok);
        }
    }

    fn record(&self, sid: &str, ok: bool) {
        let mut g = self.inner.lock().expect("subs");
        if let Some(s) = g.get_mut(sid) {
            if ok {
                s.failures = 0;
            } else {
                s.failures += 1;
                if s.failures >= MAX_FAILURES {
                    g.remove(sid);
                }
            }
        }
    }
}

/// POST one event. `true` when the subscriber answered 2xx.
async fn send_notify(
    host: SocketAddr,
    path: &str,
    sid: &str,
    seq: u32,
    body: &str,
) -> bool {
    let req = format!(
        "NOTIFY {path} HTTP/1.1\r\nHOST: {host}\r\nCONTENT-TYPE: text/xml; charset=\"utf-8\"\r\n\
         NT: upnp:event\r\nNTS: upnp:propchange\r\nSID: {sid}\r\nSEQ: {seq}\r\n\
         CONTENT-LENGTH: {}\r\nCONNECTION: close\r\n\r\n{body}",
        body.len()
    );
    let go = async {
        let mut s = TcpStream::connect(host).await.ok()?;
        s.write_all(req.as_bytes()).await.ok()?;
        let mut buf = [0u8; 64];
        let n = s.read(&mut buf).await.ok()?;
        let line = std::str::from_utf8(&buf[..n]).ok()?;
        Some(
            line.split_whitespace()
                .nth(1)
                .is_some_and(|c| c.starts_with('2')),
        )
    };
    matches!(
        tokio::time::timeout(DELIVERY_TIMEOUT, go).await,
        Ok(Some(true))
    )
}

// -------------------------------------------------------------------
// Event bodies
// -------------------------------------------------------------------

fn propertyset(props: &[(&str, String)]) -> String {
    let mut x = String::from(
        "<?xml version=\"1.0\"?>\n<e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\">",
    );
    for (k, v) in props {
        x.push_str(&format!(
            "<e:property><{k}>{}</{k}></e:property>",
            xml_escape(v)
        ));
    }
    x.push_str("</e:propertyset>");
    x
}

/// The AVTransport `LastChange` body for a snapshot.
pub fn av_event(s: &AvSnapshot) -> String {
    let v = |name: &str, val: &str| {
        format!("<{name} val=\"{}\"/>", xml_escape(val))
    };
    let inner = format!(
        "<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/AVT/\"><InstanceID val=\"0\">{}{}{}{}{}{}{}</InstanceID></Event>",
        v("TransportState", s.transport_state),
        v("TransportStatus", "OK"),
        v("CurrentTransportActions", s.actions),
        v("AVTransportURI", &s.uri),
        v("AVTransportURIMetaData", &s.meta),
        v("NextAVTransportURI", &s.next_uri),
        v("CurrentTrackDuration", &s.duration),
    );
    propertyset(&[("LastChange", inner)])
}

/// The RenderingControl `LastChange` body.
pub fn rc_event(volume: u8, muted: bool) -> String {
    let inner = format!(
        "<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/RCS/\"><InstanceID val=\"0\">\
         <Volume channel=\"Master\" val=\"{volume}\"/><Mute channel=\"Master\" val=\"{}\"/>\
         </InstanceID></Event>",
        if muted { 1 } else { 0 }
    );
    propertyset(&[("LastChange", inner)])
}

/// The ConnectionManager property set.
pub fn cm_event() -> String {
    propertyset(&[
        ("SourceProtocolInfo", String::new()),
        (
            "SinkProtocolInfo",
            crate::control::SINK_PROTOCOLS.to_string(),
        ),
        ("CurrentConnectionIDs", "0".to_string()),
    ])
}

/// The current full event body for a service, for the initial event.
pub async fn initial_event(
    control: &Control,
    service_name: &str,
) -> Option<String> {
    match service_name {
        "AVTransport" => Some(av_event(&control.av_snapshot().await)),
        "RenderingControl" => {
            let (v, m) = control.rc_snapshot().await.unwrap_or((0, false));
            Some(rc_event(v, m))
        }
        "ConnectionManager" => Some(cm_event()),
        _ => None,
    }
}

/// Watch the player and tell subscribers when something moves. Runs until
/// `stop` fires.
pub async fn monitor(
    control: Arc<Control>,
    subs: Arc<Subscriptions>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut last_av: Option<AvSnapshot> = None;
    let mut last_rc: Option<(u8, bool)> = None;
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
        control.tick().await;
        if subs.is_empty() {
            last_av = None;
            last_rc = None;
            continue;
        }
        let av = control.av_snapshot().await;
        if last_av.as_ref() != Some(&av) {
            if last_av.is_some() {
                subs.notify_all("AVTransport", &av_event(&av)).await;
            }
            last_av = Some(av);
        }
        let rc = control.rc_snapshot().await;
        if rc.is_some() && last_rc != rc {
            if let (Some(_), Some((v, m))) = (last_rc, rc) {
                subs.notify_all("RenderingControl", &rc_event(v, m)).await;
            }
            last_rc = rc;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::fake::FakePlayer;
    use tokio::net::TcpListener;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn sub(
        subs: &Subscriptions,
        peer: &str,
        cb: &str,
    ) -> Result<Granted, Refusal> {
        subs.subscribe(
            "AVTransport",
            ip(peer),
            Some(cb),
            Some("upnp:event"),
            Some("Second-300"),
            None,
        )
    }

    #[test]
    fn callbacks_parse_and_odd_ones_are_refused() {
        assert_eq!(
            parse_callback("<http://192.168.1.5:49152/evt>"),
            Some((ip("192.168.1.5"), 49152, "/evt".into()))
        );
        assert_eq!(parse_callback("<http://192.168.1.5/>").unwrap().1, 80);
        assert_eq!(
            parse_callback("<http://[::1]:5000/x>").unwrap().0,
            ip("::1")
        );
        for bad in [
            "",
            "http://1.2.3.4/",
            "<https://1.2.3.4/>",
            "<http://host.example/x>",
            "<http://1.2.3.4:99999/x>",
            "<ftp://1.2.3.4/>",
        ] {
            assert_eq!(parse_callback(bad), None, "{bad}");
        }
    }

    #[test]
    fn timeouts_are_clamped() {
        assert_eq!(parse_timeout(Some("Second-300")), 300);
        assert_eq!(parse_timeout(Some("Second-1")), MIN_TIMEOUT_SECS);
        assert_eq!(parse_timeout(Some("Second-99999")), MAX_TIMEOUT_SECS);
        assert_eq!(parse_timeout(Some("infinite")), MAX_TIMEOUT_SECS);
        assert_eq!(parse_timeout(None), MAX_TIMEOUT_SECS);
    }

    #[test]
    fn a_callback_that_is_not_the_peer_is_refused() {
        let s = Subscriptions::new();
        let r = sub(&s, "192.168.1.5", "<http://192.168.1.99:80/x>");
        assert_eq!(r.unwrap_err(), Refusal::PreconditionFailed);
        assert!(s.is_empty(), "nothing is recorded for a refused callback");
        assert!(sub(&s, "192.168.1.5", "<http://192.168.1.5:80/x>").is_ok());
    }

    #[test]
    fn nt_is_required_for_a_new_subscription() {
        let s = Subscriptions::new();
        let r = s.subscribe(
            "AVTransport",
            ip("10.0.0.2"),
            Some("<http://10.0.0.2/x>"),
            None,
            None,
            None,
        );
        assert_eq!(r.unwrap_err(), Refusal::PreconditionFailed);
    }

    #[test]
    fn the_per_peer_and_total_caps_hold() {
        let s = Subscriptions::new();
        for _ in 0..MAX_PER_PEER {
            assert!(sub(&s, "10.0.0.2", "<http://10.0.0.2/x>").is_ok());
        }
        assert_eq!(
            sub(&s, "10.0.0.2", "<http://10.0.0.2/x>").unwrap_err(),
            Refusal::Unavailable
        );
        for i in 0..(MAX_SUBSCRIPTIONS - MAX_PER_PEER) {
            let p = format!("10.0.1.{}", i + 1);
            assert!(sub(&s, &p, &format!("<http://{p}/x>")).is_ok());
        }
        assert_eq!(s.len(), MAX_SUBSCRIPTIONS);
        assert_eq!(
            sub(&s, "10.0.9.9", "<http://10.0.9.9/x>").unwrap_err(),
            Refusal::Unavailable
        );
    }

    #[test]
    fn renew_and_unsubscribe_are_bound_to_the_subscriber() {
        let s = Subscriptions::new();
        let g = sub(&s, "10.0.0.2", "<http://10.0.0.2/x>").unwrap();
        assert!(g.is_new);
        let r = s
            .subscribe(
                "AVTransport",
                ip("10.0.0.2"),
                None,
                None,
                Some("Second-600"),
                Some(&g.sid),
            )
            .unwrap();
        assert!(!r.is_new);
        assert_eq!(r.timeout_secs, 600);
        // Another host cannot renew or cancel it.
        assert_eq!(
            s.subscribe(
                "AVTransport",
                ip("10.0.0.3"),
                None,
                None,
                None,
                Some(&g.sid)
            )
            .unwrap_err(),
            Refusal::PreconditionFailed
        );
        assert!(!s.unsubscribe(&g.sid, ip("10.0.0.3")));
        // Not on another service either.
        assert_eq!(
            s.subscribe(
                "RenderingControl",
                ip("10.0.0.2"),
                None,
                None,
                None,
                Some(&g.sid)
            )
            .unwrap_err(),
            Refusal::PreconditionFailed
        );
        assert!(s.unsubscribe(&g.sid, ip("10.0.0.2")));
        assert!(s.is_empty());
        // CALLBACK with a SID is malformed.
        assert_eq!(
            s.subscribe(
                "AVTransport",
                ip("10.0.0.2"),
                Some("<http://10.0.0.2/x>"),
                None,
                None,
                Some("uuid:x")
            )
            .unwrap_err(),
            Refusal::BadRequest
        );
    }

    #[tokio::test]
    async fn the_initial_event_and_a_change_reach_a_real_listener_in_order() {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let got: Arc<Mutex<Vec<String>>> = Arc::default();
        let g2 = Arc::clone(&got);
        tokio::spawn(async move {
            loop {
                let Ok((mut c, _)) = l.accept().await else {
                    return;
                };
                let g3 = Arc::clone(&g2);
                tokio::spawn(async move {
                    let mut data = Vec::new();
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = c.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        data.extend_from_slice(&buf[..n]);
                        if String::from_utf8_lossy(&data)
                            .contains("</e:propertyset>")
                        {
                            break;
                        }
                    }
                    let _ = c
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
                        )
                        .await;
                    g3.lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&data).into_owned());
                });
            }
        });
        let subs = Subscriptions::new();
        let g =
            sub(&subs, "127.0.0.1", &format!("<http://127.0.0.1:{port}/cb>"))
                .unwrap();
        let control = Control::new(FakePlayer::new());
        let body = initial_event(&control, "AVTransport").await.unwrap();
        subs.deliver_to(&g.sid, &body).await;
        subs.notify_all("AVTransport", &av_event(&control.av_snapshot().await))
            .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let got = got.lock().unwrap().clone();
        assert_eq!(got.len(), 2, "{got:?}");
        let mut seqs: Vec<&str> = got
            .iter()
            .filter_map(|r| r.lines().find(|l| l.starts_with("SEQ:")))
            .collect();
        seqs.sort();
        assert_eq!(seqs, vec!["SEQ: 0", "SEQ: 1"]);
        assert!(got[0].starts_with("NOTIFY /cb HTTP/1.1"));
        assert!(got[0].contains(&format!("SID: {}", g.sid)));
        assert!(got[0].contains("LastChange"));
    }

    #[tokio::test]
    async fn a_subscriber_that_keeps_failing_is_dropped() {
        // Nothing listens on this port.
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let subs = Subscriptions::new();
        sub(&subs, "127.0.0.1", &format!("<http://127.0.0.1:{port}/cb>"))
            .unwrap();
        for _ in 0..MAX_FAILURES {
            subs.notify_all("AVTransport", "<x/>").await;
        }
        assert!(
            subs.is_empty(),
            "a dead subscriber is removed after repeated failures"
        );
    }

    #[test]
    fn event_bodies_are_well_formed_and_carry_the_state() {
        let snap = AvSnapshot {
            transport_state: "PLAYING",
            uri: "http://h/a?x=1&y=2".into(),
            meta: "<DIDL-Lite/>".into(),
            next_uri: String::new(),
            duration: "0:03:00".into(),
            actions: "Pause,Stop,Seek",
        };
        let x = av_event(&snap);
        let doc = roxmltree::Document::parse(&x).expect("well-formed");
        let last = doc
            .descendants()
            .find(|n| n.has_tag_name("LastChange"))
            .unwrap()
            .text()
            .unwrap()
            .to_string();
        let inner = roxmltree::Document::parse(&last)
            .expect("LastChange decodes to well-formed XML");
        let ts = inner
            .descendants()
            .find(|n| n.has_tag_name("TransportState"))
            .unwrap();
        assert_eq!(ts.attribute("val"), Some("PLAYING"));
        let u = inner
            .descendants()
            .find(|n| n.has_tag_name("AVTransportURI"))
            .unwrap();
        assert_eq!(u.attribute("val"), Some("http://h/a?x=1&y=2"));
        let r = rc_event(40, true);
        let doc = roxmltree::Document::parse(&r).unwrap();
        let last = doc
            .descendants()
            .find(|n| n.has_tag_name("LastChange"))
            .unwrap()
            .text()
            .unwrap()
            .to_string();
        assert!(
            last.contains("Volume channel=\"Master\" val=\"40\"")
                && last.contains("Mute channel=\"Master\" val=\"1\"")
        );
        roxmltree::Document::parse(&cm_event()).unwrap();
    }

    #[tokio::test]
    async fn the_monitor_tells_a_subscriber_when_the_transport_moves() {
        use crate::soap::Action;
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let got: Arc<Mutex<Vec<String>>> = Arc::default();
        let g2 = Arc::clone(&got);
        tokio::spawn(async move {
            loop {
                let Ok((mut c, _)) = l.accept().await else {
                    return;
                };
                let g3 = Arc::clone(&g2);
                tokio::spawn(async move {
                    let mut data = Vec::new();
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = c.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        data.extend_from_slice(&buf[..n]);
                        if String::from_utf8_lossy(&data)
                            .contains("</e:propertyset>")
                        {
                            break;
                        }
                    }
                    let _ = c
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
                        )
                        .await;
                    g3.lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&data).into_owned());
                });
            }
        });
        let player = FakePlayer::new();
        let control = Control::new(player.clone());
        let subs = Subscriptions::new();
        let g =
            sub(&subs, "127.0.0.1", &format!("<http://127.0.0.1:{port}/cb>"))
                .unwrap();
        subs.deliver_to(
            &g.sid,
            &initial_event(&control, "AVTransport").await.unwrap(),
        )
        .await;
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let mon = tokio::spawn(monitor(
            Arc::clone(&control),
            Arc::clone(&subs),
            stop_rx,
        ));
        tokio::time::sleep(Duration::from_millis(1300)).await; // monitor takes its baseline
        let mk = |n: &str, args: &[(&str, &str)]| Action {
            name: n.into(),
            namespace: String::new(),
            args: args
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        control
            .handle(
                "AVTransport",
                &mk(
                    "SetAVTransportURI",
                    &[
                        ("InstanceID", "0"),
                        ("CurrentURI", "http://h/a.flac"),
                        ("CurrentURIMetaData", ""),
                    ],
                ),
            )
            .await
            .unwrap();
        control
            .handle(
                "AVTransport",
                &mk("Play", &[("InstanceID", "0"), ("Speed", "1")]),
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(2500)).await;
        let _ = stop_tx.send(true);
        let _ = mon.await;
        let got = got.lock().unwrap().clone();
        let playing = got.iter().any(|r| {
            r.contains("TransportState val=&quot;PLAYING&quot;")
                || r.contains("TransportState val=\"PLAYING\"")
        });
        assert!(playing, "a PLAYING change reached the subscriber: {got:?}");
    }
}
