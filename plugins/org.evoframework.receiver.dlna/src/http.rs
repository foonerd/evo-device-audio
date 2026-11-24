// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! The renderer's HTTP endpoints.
//!
//! `GET /description.xml`, `GET /scpd/<service>.xml`, `POST /ctl/<service>`
//! and `SUBSCRIBE` / `UNSUBSCRIBE /evt/<service>`.
//!
//! This slice serves the descriptions. Control answers every action with
//! the UPnP `401 Invalid Action` fault and eventing answers `501`, so a
//! controller that finds the device learns it is not yet usable instead
//! of being told a half-truth. The action tables arrive with the bridge
//! to the player.
//!
//! Hardening, all of it bounded: LAN-origin peers only (a public peer is
//! closed without a byte), a connection cap, a header read timeout, an
//! overall connection deadline, and a request body cap.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{CONTENT_TYPE, SERVER};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinHandle;

use crate::control::Control;
use crate::description::{device_description, scpd, service};
use crate::events::{initial_event, Subscriptions};
use crate::lan::is_lan_origin;
use crate::soap::{fault_xml, parse_action, Fault};

/// Largest request body read.
pub const MAX_BODY: usize = 64 * 1024;
/// Most connections served at once.
pub const MAX_CONNECTIONS: usize = 32;
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECTION_DEADLINE: Duration = Duration::from_secs(30);

/// What the endpoints present.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// Device UUID, without the `uuid:` prefix.
    pub uuid: String,
    /// Friendly name shown to controllers.
    pub friendly_name: String,
}

/// Everything the endpoints serve from.
pub struct Services {
    /// What the description presents.
    pub info: DeviceInfo,
    /// The control actions.
    pub control: Arc<Control>,
    /// The event subscriptions.
    pub subs: Arc<Subscriptions>,
}

/// A running HTTP server.
pub struct HttpHandle {
    local_addr: SocketAddr,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl HttpHandle {
    /// The address the listener is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Stop accepting and drop the listener.
    pub async fn stop(self) {
        let _ = self.shutdown.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(2), self.task).await;
    }
}

/// Bind `bind` and serve until [`HttpHandle::stop`].
pub async fn start(
    bind: SocketAddr,
    svcs: Arc<Services>,
) -> std::io::Result<HttpHandle> {
    let listener = TcpListener::bind(bind).await?;
    let local_addr = listener.local_addr()?;
    let (tx, mut rx) = watch::channel(false);
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = rx.changed() => break,
                accepted = listener.accept() => {
                    let Ok((stream, peer)) = accepted else { continue };
                    if !is_lan_origin(peer.ip()) {
                        // Public origin: close without a byte.
                        continue;
                    }
                    let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                        continue;
                    };
                    let svcs = Arc::clone(&svcs);
                    tokio::spawn(async move {
                        let _permit = permit;
                        let svc = service_fn(move |req| {
                            let svcs = Arc::clone(&svcs);
                            async move { Ok::<_, Infallible>(handle(req, peer, &svcs).await) }
                        });
                        let conn = http1::Builder::new()
                            .timer(TokioTimer::new())
                            .header_read_timeout(HEADER_TIMEOUT)
                            .keep_alive(false)
                            .serve_connection(TokioIo::new(stream), svc);
                        let _ = tokio::time::timeout(CONNECTION_DEADLINE, conn).await;
                    });
                }
            }
        }
    });
    Ok(HttpHandle {
        local_addr,
        shutdown: tx,
        task,
    })
}

fn xml(status: StatusCode, body: String) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "text/xml; charset=\"utf-8\"")
        .header(SERVER, crate::ssdp::server_string())
        .body(Full::new(Bytes::from(body)))
        .expect("static response parts are valid")
}

fn plain(status: StatusCode) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(SERVER, crate::ssdp::server_string())
        .body(Full::new(Bytes::new()))
        .expect("static response parts are valid")
}

/// The UPnP fault returned for a control request that is not an action.
pub fn invalid_action_fault() -> String {
    fault_xml(&Fault::INVALID_ACTION)
}

fn header<'a>(req: &'a Request<Incoming>, name: &str) -> Option<&'a str> {
    req.headers().get(name).and_then(|v| v.to_str().ok())
}

fn subscribe_ok(sid: &str, secs: u64) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::OK)
        .header("SID", sid)
        .header("TIMEOUT", format!("Second-{secs}"))
        .header(SERVER, crate::ssdp::server_string())
        .body(Full::new(Bytes::new()))
        .expect("static response parts are valid")
}

/// Route one request. Public so the routing is testable without a socket.
pub async fn handle(
    req: Request<Incoming>,
    peer: SocketAddr,
    svcs: &Services,
) -> Response<Full<Bytes>> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    if method == Method::GET || method == Method::HEAD {
        if path == "/description.xml" {
            return xml(
                StatusCode::OK,
                device_description(&svcs.info.uuid, &svcs.info.friendly_name),
            );
        }
        if let Some(def) = path
            .strip_prefix("/icon/")
            .and_then(crate::description::icon)
        {
            return Response::builder()
                .status(StatusCode::OK)
                .header(CONTENT_TYPE, "image/png")
                .header("Cache-Control", "max-age=86400")
                .header(SERVER, crate::ssdp::server_string())
                .body(Full::new(Bytes::from_static(def.bytes)))
                .expect("static response parts are valid");
        }
        if let Some(name) = path
            .strip_prefix("/scpd/")
            .and_then(|p| p.strip_suffix(".xml"))
        {
            return match scpd(name) {
                Some(body) => xml(StatusCode::OK, body),
                None => plain(StatusCode::NOT_FOUND),
            };
        }
        return plain(StatusCode::NOT_FOUND);
    }

    if method == Method::POST {
        let Some(name) = path.strip_prefix("/ctl/") else {
            return plain(StatusCode::NOT_FOUND);
        };
        let Some(def) = service(name) else {
            return plain(StatusCode::NOT_FOUND);
        };
        let body = match Limited::new(req.into_body(), MAX_BODY).collect().await
        {
            Ok(b) => b.to_bytes(),
            Err(_) => return plain(StatusCode::PAYLOAD_TOO_LARGE),
        };
        let fault =
            |f: Fault| xml(StatusCode::INTERNAL_SERVER_ERROR, fault_xml(&f));
        let Ok(text) = std::str::from_utf8(&body) else {
            return fault(Fault::INVALID_ACTION);
        };
        let Ok(action) = parse_action(text) else {
            return fault(Fault::INVALID_ACTION);
        };
        // The action must be addressed to this service's type.
        let base = def
            .service_type
            .rsplit_once(':')
            .map(|(b, _)| b)
            .unwrap_or("");
        if !action.namespace.starts_with(base) {
            return fault(Fault::INVALID_ACTION);
        }
        return match svcs.control.handle(def.name, &action).await {
            Ok(body) => xml(StatusCode::OK, body),
            Err(f) => fault(f),
        };
    }

    if method.as_str() == "SUBSCRIBE" || method.as_str() == "UNSUBSCRIBE" {
        let Some(def) = path.strip_prefix("/evt/").and_then(service) else {
            return plain(StatusCode::NOT_FOUND);
        };
        if method.as_str() == "UNSUBSCRIBE" {
            if header(&req, "callback").is_some()
                || header(&req, "nt").is_some()
            {
                return plain(StatusCode::BAD_REQUEST);
            }
            return match header(&req, "sid") {
                Some(sid) if svcs.subs.unsubscribe(sid.trim(), peer.ip()) => {
                    plain(StatusCode::OK)
                }
                Some(_) => plain(StatusCode::PRECONDITION_FAILED),
                None => plain(StatusCode::BAD_REQUEST),
            };
        }
        let granted = svcs.subs.subscribe(
            def.name,
            peer.ip(),
            header(&req, "callback"),
            header(&req, "nt"),
            header(&req, "timeout"),
            header(&req, "sid").map(str::trim),
        );
        return match granted {
            Ok(g) => {
                if g.is_new {
                    let svcs_subs = Arc::clone(&svcs.subs);
                    let control = Arc::clone(&svcs.control);
                    let sid = g.sid.clone();
                    let name = def.name;
                    tokio::spawn(async move {
                        // The initial event follows the response.
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        if let Some(body) = initial_event(&control, name).await
                        {
                            svcs_subs.deliver_to(&sid, &body).await;
                        }
                    });
                }
                subscribe_ok(&g.sid, g.timeout_secs)
            }
            Err(r) => plain(
                StatusCode::from_u16(r.status())
                    .unwrap_or(StatusCode::BAD_REQUEST),
            ),
        };
    }

    plain(StatusCode::METHOD_NOT_ALLOWED)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    fn svcs() -> Arc<Services> {
        Arc::new(Services {
            info: DeviceInfo {
                uuid: "11111111-2222-3333-4444-555555555555".into(),
                friendly_name: "Kitchen".into(),
            },
            control: Control::new(crate::bridge::fake::FakePlayer::new()),
            subs: Subscriptions::new(),
        })
    }

    fn soap_body(service: &str, action: &str, args: &str) -> String {
        format!(
            "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
             s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>\
             <u:{action} xmlns:u=\"urn:schemas-upnp-org:service:{service}:1\">{args}</u:{action}>\
             </s:Body></s:Envelope>"
        )
    }

    async fn post(
        addr: SocketAddr,
        service: &str,
        action: &str,
        args: &str,
    ) -> String {
        let body = soap_body(service, action, args);
        raw(
            addr,
            &format!(
                "POST /ctl/{service} HTTP/1.1\r\nHost: x\r\nContent-Type: text/xml\r\n\
                 SOAPACTION: \"urn:schemas-upnp-org:service:{service}:1#{action}\"\r\n\
                 Content-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        )
        .await
    }

    async fn raw(addr: SocketAddr, request: &str) -> String {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(request.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        let _ = tokio::time::timeout(
            Duration::from_secs(3),
            s.read_to_end(&mut out),
        )
        .await;
        String::from_utf8_lossy(&out).into_owned()
    }

    async fn up() -> HttpHandle {
        start("127.0.0.1:0".parse().unwrap(), svcs()).await.unwrap()
    }

    #[tokio::test]
    async fn every_listed_icon_is_served_as_png() {
        let h = up().await;
        let d = raw(
            h.local_addr(),
            "GET /description.xml HTTP/1.1\r\nHost: x\r\n\r\n",
        )
        .await;
        let mut n = 0;
        for line in d.lines() {
            let Some(rest) = line.trim().strip_prefix("<url>") else {
                continue;
            };
            let url = rest.trim_end_matches("</url>");
            let r = raw(
                h.local_addr(),
                &format!("GET {url} HTTP/1.1\r\nHost: x\r\n\r\n"),
            )
            .await;
            assert!(r.starts_with("HTTP/1.1 200"), "{url}: {r:.80}");
            assert!(
                r.to_lowercase().contains("content-type: image/png"),
                "{url}"
            );
            assert!(r.contains("PNG"), "{url} body is a PNG");
            n += 1;
        }
        assert_eq!(n, crate::description::ICONS.len());
        let missing = raw(
            h.local_addr(),
            "GET /icon/nope.png HTTP/1.1\r\nHost: x\r\n\r\n",
        )
        .await;
        assert!(missing.starts_with("HTTP/1.1 404"), "{missing:.60}");
        h.stop().await;
    }

    #[tokio::test]
    async fn the_description_and_scpds_are_served() {
        let h = up().await;
        let d = raw(
            h.local_addr(),
            "GET /description.xml HTTP/1.1\r\nHost: x\r\n\r\n",
        )
        .await;
        assert!(d.starts_with("HTTP/1.1 200"), "{d}");
        assert!(
            d.contains("MediaRenderer:1") && d.contains("AVTransport:1"),
            "{d}"
        );
        for s in ["AVTransport", "RenderingControl", "ConnectionManager"] {
            let r = raw(
                h.local_addr(),
                &format!("GET /scpd/{s}.xml HTTP/1.1\r\nHost: x\r\n\r\n"),
            )
            .await;
            assert!(
                r.starts_with("HTTP/1.1 200") && r.contains("<scpd"),
                "{s}: {r}"
            );
        }
        let nf = raw(
            h.local_addr(),
            "GET /scpd/ContentDirectory.xml HTTP/1.1\r\nHost: x\r\n\r\n",
        )
        .await;
        assert!(nf.starts_with("HTTP/1.1 404"), "{nf}");
        h.stop().await;
    }

    #[tokio::test]
    async fn a_body_that_is_not_an_action_answers_invalid_action() {
        let h = up().await;
        let body = "<x/>";
        let c = raw(
            h.local_addr(),
            &format!(
                "POST /ctl/AVTransport HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        )
        .await;
        assert!(c.starts_with("HTTP/1.1 500"), "{c}");
        assert!(c.contains("<errorCode>401</errorCode>"), "{c}");

        let u = raw(
            h.local_addr(),
            "POST /ctl/Nope HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        assert!(u.starts_with("HTTP/1.1 404"), "{u}");
        h.stop().await;
    }

    #[tokio::test]
    async fn an_oversized_body_is_refused_not_buffered() {
        let h = up().await;
        let big = "a".repeat(MAX_BODY + 1024);
        let r = raw(
            h.local_addr(),
            &format!(
                "POST /ctl/AVTransport HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{big}",
                big.len()
            ),
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 413"), "{}", &r[..r.len().min(80)]);
        h.stop().await;
    }

    #[tokio::test]
    async fn other_methods_are_refused() {
        let h = up().await;
        let r = raw(
            h.local_addr(),
            "DELETE /description.xml HTTP/1.1\r\nHost: x\r\n\r\n",
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 405"), "{r}");
        h.stop().await;
    }

    #[tokio::test]
    async fn a_soap_play_round_trips_over_http() {
        let h = up().await;
        let a = h.local_addr();
        let r = post(a, "AVTransport", "SetAVTransportURI",
            "<InstanceID>0</InstanceID><CurrentURI>http://192.168.1.5/a.flac</CurrentURI><CurrentURIMetaData></CurrentURIMetaData>").await;
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        assert!(r.contains("SetAVTransportURIResponse"), "{r}");
        let r = post(
            a,
            "AVTransport",
            "Play",
            "<InstanceID>0</InstanceID><Speed>1</Speed>",
        )
        .await;
        assert!(
            r.starts_with("HTTP/1.1 200") && r.contains("PlayResponse"),
            "{r}"
        );
        let r = post(
            a,
            "AVTransport",
            "GetTransportInfo",
            "<InstanceID>0</InstanceID>",
        )
        .await;
        assert!(
            r.contains(
                "<CurrentTransportState>PLAYING</CurrentTransportState>"
            ),
            "{r}"
        );
        let r = post(
            a,
            "RenderingControl",
            "GetVolume",
            "<InstanceID>0</InstanceID><Channel>Master</Channel>",
        )
        .await;
        assert!(r.contains("<CurrentVolume>30</CurrentVolume>"), "{r}");
        h.stop().await;
    }

    #[tokio::test]
    async fn a_fault_is_a_500_with_the_upnp_code_and_a_wrong_namespace_is_refused(
    ) {
        let h = up().await;
        let a = h.local_addr();
        let r = post(
            a,
            "AVTransport",
            "Play",
            "<InstanceID>0</InstanceID><Speed>1</Speed>",
        )
        .await;
        assert!(
            r.starts_with("HTTP/1.1 500")
                && r.contains("<errorCode>702</errorCode>"),
            "nothing set: {r}"
        );
        let r =
            post(a, "AVTransport", "Teleport", "<InstanceID>0</InstanceID>")
                .await;
        assert!(r.contains("<errorCode>401</errorCode>"), "{r}");
        // An RCS action sent to the AVTransport endpoint.
        let body = soap_body(
            "RenderingControl",
            "GetVolume",
            "<InstanceID>0</InstanceID><Channel>Master</Channel>",
        );
        let r = raw(a, &format!("POST /ctl/AVTransport HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{body}", body.len())).await;
        assert!(r.contains("<errorCode>401</errorCode>"), "{r}");
        h.stop().await;
    }

    #[tokio::test]
    async fn subscribe_grants_a_sid_and_a_timeout_and_unsubscribe_ends_it() {
        let h = up().await;
        let a = h.local_addr();
        let s = raw(a, "SUBSCRIBE /evt/AVTransport HTTP/1.1\r\nHost: x\r\nCALLBACK: <http://127.0.0.1:9/cb>\r\nNT: upnp:event\r\nTIMEOUT: Second-300\r\n\r\n").await;
        assert!(s.starts_with("HTTP/1.1 200"), "{s}");
        let sid = s
            .lines()
            .find_map(|l| {
                l.strip_prefix("sid: ").or_else(|| l.strip_prefix("SID: "))
            })
            .expect("a SID header")
            .trim()
            .to_string();
        assert!(sid.starts_with("uuid:"), "{sid}");
        assert!(
            s.to_ascii_lowercase().contains("timeout: second-300"),
            "{s}"
        );
        // A callback that is not the peer is refused.
        let bad = raw(a, "SUBSCRIBE /evt/AVTransport HTTP/1.1\r\nHost: x\r\nCALLBACK: <http://10.9.9.9:80/cb>\r\nNT: upnp:event\r\n\r\n").await;
        assert!(bad.starts_with("HTTP/1.1 412"), "{bad}");
        let u = raw(a, &format!("UNSUBSCRIBE /evt/AVTransport HTTP/1.1\r\nHost: x\r\nSID: {sid}\r\n\r\n")).await;
        assert!(u.starts_with("HTTP/1.1 200"), "{u}");
        let again = raw(a, &format!("UNSUBSCRIBE /evt/AVTransport HTTP/1.1\r\nHost: x\r\nSID: {sid}\r\n\r\n")).await;
        assert!(again.starts_with("HTTP/1.1 412"), "{again}");
        h.stop().await;
    }
}
