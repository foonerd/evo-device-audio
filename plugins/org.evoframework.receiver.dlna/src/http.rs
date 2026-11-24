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

use crate::description::{device_description, scpd, service};
use crate::lan::is_lan_origin;

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
    info: Arc<DeviceInfo>,
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
                    let info = Arc::clone(&info);
                    tokio::spawn(async move {
                        let _permit = permit;
                        let svc = service_fn(move |req| {
                            let info = Arc::clone(&info);
                            async move { Ok::<_, Infallible>(handle(req, &info).await) }
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

/// The UPnP fault returned for any control action in this slice.
pub fn invalid_action_fault() -> String {
    "<?xml version=\"1.0\"?>\n\
     <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
     s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
     <s:Body><s:Fault><faultcode>s:Client</faultcode>\
     <faultstring>UPnPError</faultstring><detail>\
     <UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\">\
     <errorCode>401</errorCode><errorDescription>Invalid Action</errorDescription>\
     </UPnPError></detail></s:Fault></s:Body></s:Envelope>"
        .to_string()
}

/// Route one request. Public so the routing is testable without a socket.
pub async fn handle(
    req: Request<Incoming>,
    info: &DeviceInfo,
) -> Response<Full<Bytes>> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    if method == Method::GET || method == Method::HEAD {
        if path == "/description.xml" {
            return xml(
                StatusCode::OK,
                device_description(&info.uuid, &info.friendly_name),
            );
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
        if let Some(name) = path.strip_prefix("/ctl/") {
            if service(name).is_none() {
                return plain(StatusCode::NOT_FOUND);
            }
            // Read, bounded, then discard: no action is implemented yet.
            return match Limited::new(req.into_body(), MAX_BODY).collect().await
            {
                Ok(_) => xml(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    invalid_action_fault(),
                ),
                Err(_) => plain(StatusCode::PAYLOAD_TOO_LARGE),
            };
        }
        return plain(StatusCode::NOT_FOUND);
    }

    if method.as_str() == "SUBSCRIBE" || method.as_str() == "UNSUBSCRIBE" {
        if path.strip_prefix("/evt/").and_then(service).is_some() {
            return plain(StatusCode::NOT_IMPLEMENTED);
        }
        return plain(StatusCode::NOT_FOUND);
    }

    plain(StatusCode::METHOD_NOT_ALLOWED)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    fn info() -> Arc<DeviceInfo> {
        Arc::new(DeviceInfo {
            uuid: "11111111-2222-3333-4444-555555555555".into(),
            friendly_name: "Kitchen".into(),
        })
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
        start("127.0.0.1:0".parse().unwrap(), info()).await.unwrap()
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
    async fn control_answers_invalid_action_and_eventing_is_not_implemented() {
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
        let e = raw(h.local_addr(), "SUBSCRIBE /evt/AVTransport HTTP/1.1\r\nHost: x\r\nCALLBACK: <http://127.0.0.1:1/>\r\nNT: upnp:event\r\n\r\n").await;
        assert!(e.starts_with("HTTP/1.1 501"), "{e}");
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
}
