// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! # org.evoframework.source.dlna
//!
//! UPnP / DLNA MediaServer client. Discovers servers via SSDP,
//! browses ContentDirectory with mandatory paging, resolves
//! items to HTTP stream URLs. Writes `discovered.json` for
//! `playback.mpd` to upsert `NetworkDlna` library sources
//! (interim until OOP ShelfRequestDispatcher is wired).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::manual_async_fn)]

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use evo_dlna::{
    browse_page, default_discovered_path, discover_media_servers,
    fetch_media_server, parse_didl, pick_stream_uri, read_discovered,
    write_discovered, BrowseParams, DidlObject, DiscoveredFile,
    DiscoveredServer, DISCOVERED_VERSION, DLNA_PAGE_DEFAULT,
    DLNA_PAGE_HARD_CAP,
};
use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities, ShelfDispatchError, ShelfRequestDispatcher,
};
use evo_plugin_sdk::Manifest;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::RwLock;

/// Embedded manifest.
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");
/// Plugin reverse-DNS name.
pub const PLUGIN_NAME: &str = "org.evoframework.source.dlna";

const VERB_REFRESH: &str = "source.dlna.refresh";
const VERB_LIST: &str = "source.dlna.list";
const VERB_BROWSE: &str = "source.dlna.browse";
const VERB_RESOLVE: &str = "source.dlna.resolve";
const VERB_PLAY_NOW: &str = "play_now";

const URI_SCHEME_DLNA: &str = "dlna";
const PLAYBACK_SHELF: &str = "audio.playback";

/// Parse embedded manifest.
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("org.evoframework.source.dlna: embedded manifest must parse")
}

fn crate_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is valid semver")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Plugin singleton.
pub struct DlnaSourcePlugin {
    loaded: bool,
    discovered_path: PathBuf,
    servers: Arc<RwLock<Vec<DiscoveredServer>>>,
    discover_every: Duration,
    /// Peer shelf dispatcher (in-process always; OOP once
    /// framework P0-2 lands). Used by `play_now` to hand the
    /// resolved HTTP URI to `audio.playback`.
    shelf_dispatcher: Option<Arc<dyn ShelfRequestDispatcher>>,
}

impl DlnaSourcePlugin {
    /// Construct unloaded instance.
    pub fn new() -> Self {
        Self {
            loaded: false,
            discovered_path: default_discovered_path(),
            servers: Arc::new(RwLock::new(Vec::new())),
            discover_every: Duration::from_secs(60),
            shelf_dispatcher: None,
        }
    }
}

impl Default for DlnaSourcePlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for DlnaSourcePlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: PLUGIN_NAME.to_string(),
                    version: crate_version(),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: vec![
                        VERB_REFRESH.to_string(),
                        VERB_LIST.to_string(),
                        VERB_BROWSE.to_string(),
                        VERB_RESOLVE.to_string(),
                        VERB_PLAY_NOW.to_string(),
                    ],
                    accepts_custody: false,
                    flags: Default::default(),
                    course_correct_verbs: Vec::new(),
                },
                build_info: BuildInfo {
                    plugin_build: env!("CARGO_PKG_VERSION").to_string(),
                    sdk_version: evo_plugin_sdk::VERSION.to_string(),
                    rustc_version: None,
                    built_at: None,
                },
            }
        }
    }

    fn load<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
        async move {
            if let Some(toml::Value::String(p)) =
                ctx.config.get("discovered_path")
            {
                self.discovered_path = PathBuf::from(p);
            }
            if let Some(toml::Value::Integer(secs)) =
                ctx.config.get("discover_every_secs")
            {
                if *secs > 0 {
                    self.discover_every = Duration::from_secs(*secs as u64);
                }
            }
            self.shelf_dispatcher = ctx.shelf_request_dispatcher.clone();
            if self.shelf_dispatcher.is_none() {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    "source.dlna: ShelfRequestDispatcher is None \
                     (OOP wire gap); play_now will return Transient \
                     until framework propagates the dispatcher"
                );
            }
            match read_discovered(&self.discovered_path) {
                Ok(file) => {
                    *self.servers.write().await = file.servers;
                }
                Err(e) => {
                    tracing::warn!(
                        plugin = PLUGIN_NAME,
                        error = %e,
                        "source.dlna: could not read discovered sidecar"
                    );
                }
            }
            let servers = Arc::clone(&self.servers);
            let path = self.discovered_path.clone();
            let every = self.discover_every;
            tokio::spawn(async move {
                loop {
                    if let Err(e) = refresh_discovery(&servers, &path).await {
                        // Classify the error: a transient
                        // network-plane state (routing / IGMP
                        // membership not yet established at
                        // startup) is common when the steward
                        // starts before network-online.target
                        // is reached, and self-resolves within
                        // the next few discovery cadence ticks.
                        // Log at INFO with a "retry on next
                        // cadence" message so the transient
                        // does not pollute the operator's WARN
                        // stream. Any other error class stays
                        // at WARN — the discovery contract is
                        // still that structural / persistent
                        // problems are operator-visible.
                        if e.is_transient_network() {
                            tracing::info!(
                                plugin = PLUGIN_NAME,
                                error = %e,
                                "source.dlna: network not yet routable \
                                 for SSDP multicast; will retry on next \
                                 discovery cadence tick"
                            );
                        } else {
                            tracing::warn!(
                                plugin = PLUGIN_NAME,
                                error = %e,
                                "source.dlna: discovery refresh failed"
                            );
                        }
                    }
                    tokio::time::sleep(every).await;
                }
            });
            self.loaded = true;
            tracing::info!(
                plugin = PLUGIN_NAME,
                path = %self.discovered_path.display(),
                "source.dlna plugin loaded"
            );
            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            self.loaded = false;
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            if self.loaded {
                HealthReport::healthy()
            } else {
                HealthReport::unhealthy("source.dlna plugin not loaded")
            }
        }
    }
}

impl Respondent for DlnaSourcePlugin {
    fn handle_request<'a>(
        &'a self,
        request: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "source.dlna plugin not loaded".into(),
                ));
            }
            if request.is_past_deadline() {
                return Err(PluginError::Transient(
                    "request deadline already expired".into(),
                ));
            }
            match request.request_type.as_str() {
                VERB_REFRESH => {
                    refresh_discovery(&self.servers, &self.discovered_path)
                        .await
                        .map_err(|e| PluginError::Transient(e.to_string()))?;
                    let servers = self.servers.read().await;
                    Ok(Response::for_request(
                        request,
                        serde_json::to_vec(&json!({
                            "v": 1,
                            "status": "ok",
                            "count": servers.len(),
                            "servers": *servers,
                        }))
                        .unwrap_or_default(),
                    ))
                }
                VERB_LIST => {
                    let servers = self.servers.read().await;
                    Ok(Response::for_request(
                        request,
                        serde_json::to_vec(&json!({
                            "v": 1,
                            "status": "ok",
                            "servers": *servers,
                        }))
                        .unwrap_or_default(),
                    ))
                }
                VERB_BROWSE => {
                    let body: BrowseBody = serde_json::from_slice(
                        &request.payload,
                    )
                    .map_err(|e| PluginError::Permanent(e.to_string()))?;
                    check_wire_version(VERB_BROWSE, body.v)?;
                    let payload = handle_browse(&self.servers, body).await?;
                    Ok(Response::for_request(request, payload))
                }
                VERB_RESOLVE => {
                    let body: ResolveBody = serde_json::from_slice(
                        &request.payload,
                    )
                    .map_err(|e| PluginError::Permanent(e.to_string()))?;
                    check_wire_version(VERB_RESOLVE, body.v)?;
                    let payload = handle_resolve(&self.servers, body).await?;
                    Ok(Response::for_request(request, payload))
                }
                VERB_PLAY_NOW => {
                    let body: PlayNowBody = serde_json::from_slice(
                        &request.payload,
                    )
                    .map_err(|e| PluginError::Permanent(e.to_string()))?;
                    check_wire_version(VERB_PLAY_NOW, body.v)?;
                    let http_uri =
                        resolve_play_now_uri(&self.servers, &body.uri).await?;
                    let dispatcher =
                        self.shelf_dispatcher.as_ref().ok_or_else(|| {
                            PluginError::Transient(
                                "source.dlna play_now: ShelfRequestDispatcher \
                             unavailable (OOP wire not yet propagating \
                             LoadContext.shelf_request_dispatcher) — cannot \
                             hand resolved HTTP URI to audio.playback"
                                    .into(),
                            )
                        })?;
                    let peer_payload = serde_json::to_vec(&json!({
                        "v": 1,
                        "uri": http_uri,
                    }))
                    .map_err(|e| PluginError::Permanent(e.to_string()))?;
                    dispatcher
                        .dispatch(
                            PLAYBACK_SHELF,
                            VERB_PLAY_NOW,
                            peer_payload,
                            None,
                        )
                        .await
                        .map_err(shelf_err_to_plugin)?;
                    Ok(Response::for_request(
                        request,
                        serde_json::to_vec(&json!({
                            "v": 1,
                            "status": "ok",
                            "uri": body.uri,
                            "resolved_uri": http_uri,
                        }))
                        .unwrap_or_default(),
                    ))
                }
                other => Err(PluginError::Permanent(format!(
                    "source.dlna: unknown verb {other:?}"
                ))),
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct BrowseBody {
    #[serde(default = "one")]
    v: u32,
    service_id: String,
    #[serde(default = "root_oid")]
    object_id: String,
    #[serde(default)]
    page: Option<u32>,
    #[serde(default)]
    page_size: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct ResolveBody {
    #[serde(default = "one")]
    v: u32,
    service_id: String,
    object_id: String,
}

#[derive(Debug, Deserialize)]
struct PlayNowBody {
    #[serde(default = "one")]
    v: u32,
    uri: String,
}

fn one() -> u32 {
    1
}
fn root_oid() -> String {
    "0".into()
}

/// The wire-payload version this build understands. Every
/// request-body struct carries a `v` field defaulting to `1`;
/// an unknown version is refused with `PluginError::Permanent`
/// so a caller from a newer contract cannot be silently
/// misinterpreted as v1 shape.
const WIRE_VERSION: u32 = 1;

fn check_wire_version(verb: &str, got: u32) -> Result<(), PluginError> {
    if got != WIRE_VERSION {
        return Err(PluginError::Permanent(format!(
            "source.dlna: verb {verb:?} requires wire version {WIRE_VERSION} \
             (got {got})"
        )));
    }
    Ok(())
}

/// Parse `dlna:<service_id>/<objectId>`.
///
/// The `service_id` component may itself contain colons because
/// UPnP device UDNs use the `uuid:xxxxxxxx-xxxx-xxxx-xxxx-
/// xxxxxxxxxxxx` shape; the split is on the first `/` after the
/// scheme.
fn parse_dlna_uri(uri: &str) -> Result<(String, String), PluginError> {
    let prefix = format!("{URI_SCHEME_DLNA}:");
    let rest = uri.strip_prefix(&prefix).ok_or_else(|| {
        PluginError::Permanent(format!(
            "play_now URI {uri:?} does not bear the {URI_SCHEME_DLNA:?} \
             scheme this plugin owns"
        ))
    })?;
    let (service_id, object_id) = rest.split_once('/').ok_or_else(|| {
        PluginError::Permanent(format!(
            "play_now URI {uri:?} must be {URI_SCHEME_DLNA}:<service_id>/<objectId>"
        ))
    })?;
    if service_id.is_empty() || object_id.is_empty() {
        return Err(PluginError::Permanent(format!(
            "play_now URI {uri:?} has empty service_id or objectId"
        )));
    }
    Ok((service_id.to_string(), object_id.to_string()))
}

async fn resolve_play_now_uri(
    servers: &RwLock<Vec<DiscoveredServer>>,
    uri: &str,
) -> Result<String, PluginError> {
    // Already-resolved absolute streams (defensive): pass through.
    if uri.starts_with("http://") || uri.starts_with("https://") {
        return Ok(uri.to_string());
    }
    let (service_id, object_id) = parse_dlna_uri(uri)?;
    let control_url = lookup_control(servers, &service_id).await?;
    browse_metadata_uri(&control_url, &object_id)
        .await
        .map_err(|e| PluginError::Transient(e.to_string()))
}

fn shelf_err_to_plugin(e: ShelfDispatchError) -> PluginError {
    match e {
        ShelfDispatchError::Transient { detail }
        | ShelfDispatchError::SubstrateFailure { detail } => {
            PluginError::Transient(detail)
        }
        ShelfDispatchError::DeadlineExceeded { budget_ms } => {
            PluginError::Transient(format!(
                "audio.playback play_now exceeded budget {budget_ms}ms"
            ))
        }
        ShelfDispatchError::Permanent { detail } => {
            PluginError::Permanent(detail)
        }
        ShelfDispatchError::NoPluginOnShelf { shelf } => {
            PluginError::Permanent(format!("no plugin on shelf {shelf}"))
        }
        ShelfDispatchError::VerbNotStockedOnShelf {
            shelf,
            request_type,
        } => PluginError::Permanent(format!(
            "verb {request_type:?} not stocked on shelf {shelf:?}"
        )),
    }
}

async fn refresh_discovery(
    servers: &RwLock<Vec<DiscoveredServer>>,
    path: &std::path::Path,
) -> Result<(), evo_dlna::DlnaError> {
    let hits = discover_media_servers(Duration::from_secs(3)).await?;
    let mut merged: Vec<DiscoveredServer> = servers.read().await.clone();
    let seen_ms = now_ms();
    for hit in hits {
        match fetch_media_server(&hit.location).await {
            Ok(m) => {
                if let Some(existing) =
                    merged.iter_mut().find(|s| s.service_id == m.service_id)
                {
                    existing.friendly_name = m.friendly_name;
                    existing.control_url = m.control_url;
                    existing.base_url = m.base_url;
                    existing.location = m.location;
                    existing.last_seen_ms = seen_ms;
                } else {
                    merged.push(DiscoveredServer {
                        service_id: m.service_id,
                        friendly_name: m.friendly_name,
                        control_url: m.control_url,
                        base_url: m.base_url,
                        location: m.location,
                        last_seen_ms: seen_ms,
                    });
                }
            }
            Err(e) => {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    location = %hit.location,
                    error = %e,
                    "source.dlna: skip device (desc parse/fetch failed)"
                );
            }
        }
    }
    let grace_ms = 10 * 60 * 1000u64;
    merged.retain(|s| seen_ms.saturating_sub(s.last_seen_ms) <= grace_ms);
    merged.sort_by(|a, b| a.friendly_name.cmp(&b.friendly_name));
    write_discovered(
        path,
        &DiscoveredFile {
            v: DISCOVERED_VERSION,
            servers: merged.clone(),
        },
    )?;
    *servers.write().await = merged;
    Ok(())
}

async fn handle_browse(
    servers: &RwLock<Vec<DiscoveredServer>>,
    body: BrowseBody,
) -> Result<Vec<u8>, PluginError> {
    let control_url = lookup_control(servers, &body.service_id).await?;
    let page = body.page.unwrap_or(0);
    let page_size = body
        .page_size
        .unwrap_or(DLNA_PAGE_DEFAULT)
        .clamp(1, DLNA_PAGE_HARD_CAP);
    let result = browse_page(BrowseParams {
        control_url,
        object_id: body.object_id.clone(),
        page,
        page_size,
    })
    .await
    .map_err(|e| PluginError::Transient(e.to_string()))?;

    let entries: Vec<serde_json::Value> = result
        .objects
        .iter()
        .map(|o| match o {
            DidlObject::Container(c) => json!({
                "kind": "directory",
                "name": c.title,
                "path": c.id,
                "child_count": c.child_count,
            }),
            DidlObject::Item(i) => json!({
                "kind": "file",
                "name": i.title,
                "path": i.id,
                "title": i.title,
                "artist": i.artist,
                "album": i.album,
                "genre": i.genre,
                "date": i.date,
                "composer": i.composer,
                "artwork_url": i.album_art_uri,
                // The stable identity — `dlna:<service_id>/<objectId>`
                // — is the only URI form the operator glass ever
                // sees or stores for a DLNA item. The concrete
                // `http(s)` stream is an internal detail of
                // resolve/play_now and NEVER leaves this plugin
                // through the browse surface: putting it on the
                // wire would force favourites and playlists to
                // persist an identity that churns with the
                // MediaServer's IP and session token, which is
                // exactly the bug the stable-identity design was
                // introduced to close.
                "uri": format!("{URI_SCHEME_DLNA}:{}/{}", body.service_id, i.id),
                "playable": pick_stream_uri(i).is_some(),
            }),
        })
        .collect();

    Ok(serde_json::to_vec(&json!({
        "v": 1,
        "status": "ok",
        "service_id": body.service_id,
        "path": body.object_id,
        "entries": entries,
        "page": result.page,
        "page_size": result.page_size,
        "total": result.total_matches,
        "truncated": result.truncated,
        "next_page": result.next_page,
    }))
    .unwrap_or_default())
}

async fn handle_resolve(
    servers: &RwLock<Vec<DiscoveredServer>>,
    body: ResolveBody,
) -> Result<Vec<u8>, PluginError> {
    let control_url = lookup_control(servers, &body.service_id).await?;
    // Parse the full DIDL item so we can carry DIDL tags across the
    // resolve boundary. Historically this hop returned only the
    // stream `uri`; that left the enqueue path with no way to hand
    // MPD tags for HTTP streams, so `mpc playlistinfo` returned
    // empty title/artist/album for DLNA queue entries even though
    // browse had the metadata in hand. Preserving the DIDL fields
    // here — no extra SOAP round-trip, the BrowseMetadata call
    // already returned them — closes that gap.
    let item = browse_metadata_item(&control_url, &body.object_id)
        .await
        .map_err(|e| PluginError::Transient(e.to_string()))?;
    let uri = pick_stream_uri(&item).ok_or_else(|| {
        PluginError::Transient(
            "BrowseMetadata returned no playable res".to_string(),
        )
    })?;
    Ok(serde_json::to_vec(&json!({
        "v": 1,
        "status": "ok",
        "service_id": body.service_id,
        "object_id": body.object_id,
        "uri": uri,
        "title": item.title,
        "artist": item.artist,
        "album": item.album,
        "genre": item.genre,
        "date": item.date,
        "composer": item.composer,
        "artwork_url": item.album_art_uri,
        // DIDL duration is `HH:MM:SS[.f]`; downstream consumers
        // that want milliseconds re-parse. Leave as the DIDL
        // wire shape so nothing lossy happens here.
        "duration": item.duration,
    }))
    .unwrap_or_default())
}

async fn lookup_control(
    servers: &RwLock<Vec<DiscoveredServer>>,
    service_id: &str,
) -> Result<String, PluginError> {
    servers
        .read()
        .await
        .iter()
        .find(|s| s.service_id == service_id)
        .map(|s| s.control_url.clone())
        .ok_or_else(|| {
            PluginError::Permanent(format!("unknown service_id {service_id}"))
        })
}

/// Full-item variant of [`browse_metadata_uri`] used by
/// [`handle_resolve`] so DIDL tags (title, artist, album,
/// genre, date, composer, albumArtURI, duration) cross the
/// resolve boundary alongside the stream URI. The BrowseMetadata
/// SOAP call already returns every field parsed into a
/// [`DidlItem`]; returning the item instead of just its `res`
/// URI is a strict superset.
///
/// Kept alongside `browse_metadata_uri` (URI-only) so callers
/// that don't need the tags — none today post-2026-08-05 —
/// still have the minimal path available if a future consumer
/// wants it.
async fn browse_metadata_item(
    control_url: &str,
    object_id: &str,
) -> Result<evo_dlna::DidlItem, evo_dlna::DlnaError> {
    let didl = browse_metadata_didl(control_url, object_id).await?;
    let objs = parse_didl(&didl)?;
    for o in objs {
        if let DidlObject::Item(i) = o {
            return Ok(i);
        }
    }
    Err(evo_dlna::DlnaError::Soap(
        "BrowseMetadata returned no <item>".into(),
    ))
}

/// Shared SOAP-envelope send + DIDL-Result extract. Factored out
/// so the URI-only and full-item paths agree on the wire shape.
async fn browse_metadata_didl(
    control_url: &str,
    object_id: &str,
) -> Result<String, evo_dlna::DlnaError> {
    let oid = object_id
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let envelope = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
      <ObjectID>{oid}</ObjectID>
      <BrowseFlag>BrowseMetadata</BrowseFlag>
      <Filter>*</Filter>
      <StartingIndex>0</StartingIndex>
      <RequestedCount>1</RequestedCount>
      <SortCriteria></SortCriteria>
    </u:Browse>
  </s:Body>
</s:Envelope>"#
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .build()
        .map_err(|e| evo_dlna::DlnaError::Http(e.to_string()))?;
    let resp = client
        .post(control_url)
        .header("Content-Type", "text/xml; charset=\"utf-8\"")
        .header(
            "SOAPACTION",
            "\"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"",
        )
        .body(envelope)
        .send()
        .await
        .map_err(|e| evo_dlna::DlnaError::Http(e.to_string()))?;
    let body = resp
        .text()
        .await
        .map_err(|e| evo_dlna::DlnaError::Http(e.to_string()))?;
    let start = body
        .find("Result>")
        .map(|i| i + "Result>".len())
        .ok_or_else(|| evo_dlna::DlnaError::Soap("no Result".into()))?;
    let end = body[start..]
        .find("</")
        .map(|i| start + i)
        .ok_or_else(|| evo_dlna::DlnaError::Soap("no Result close".into()))?;
    let escaped = &body[start..end];
    let didl = escaped
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    Ok(didl)
}

/// URI-only wrapper that composes `browse_metadata_didl` with
/// `pick_stream_uri`. Kept for callers wanting only the URI —
/// none today, but the seam stays open.
#[allow(dead_code)]
async fn browse_metadata_uri(
    control_url: &str,
    object_id: &str,
) -> Result<String, evo_dlna::DlnaError> {
    let didl = browse_metadata_didl(control_url, object_id).await?;
    let objs = parse_didl(&didl)?;
    for o in objs {
        if let DidlObject::Item(i) = o {
            if let Some(uri) = pick_stream_uri(&i) {
                return Ok(uri);
            }
        }
    }
    Err(evo_dlna::DlnaError::Soap(
        "BrowseMetadata returned no playable res".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses() {
        let m = manifest();
        assert_eq!(m.plugin.name, PLUGIN_NAME);
        assert!(
            m.capabilities
                .respondent
                .as_ref()
                .map(|r| r.request_types.iter().any(|t| t == "play_now"))
                .unwrap_or(false),
            "play_now required for capabilities.source admission"
        );
        let schemes = m
            .capabilities
            .source
            .as_ref()
            .map(|s| s.uri_schemes.as_slice())
            .unwrap_or(&[]);
        assert_eq!(schemes, ["dlna"]);
    }

    #[test]
    fn parse_dlna_uri_accepts_uuid_service_id() {
        let (sid, oid) =
            parse_dlna_uri("dlna:uuid:abc-def/0$1").expect("parse");
        assert_eq!(sid, "uuid:abc-def");
        assert_eq!(oid, "0$1");
    }

    #[test]
    fn parse_dlna_uri_refuses_wrong_scheme() {
        assert!(parse_dlna_uri("http://x/y").is_err());
    }
}
