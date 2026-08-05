// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! DIDL-Lite decode for ContentDirectory Browse results.

use crate::DlnaError;

/// Parsed DIDL object (container or item).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DidlObject {
    /// Browseable container.
    Container(DidlContainer),
    /// Playable (or metadata) item.
    Item(DidlItem),
}

/// DIDL container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DidlContainer {
    /// Object id (opaque path token).
    pub id: String,
    /// Parent object id.
    pub parent_id: String,
    /// Title.
    pub title: String,
    /// Child count when server provides it.
    pub child_count: Option<u32>,
}

/// DIDL item with optional stream resources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DidlItem {
    /// Object id.
    pub id: String,
    /// Parent object id.
    pub parent_id: String,
    /// Title.
    pub title: String,
    /// Artist / creator when present.
    pub artist: Option<String>,
    /// Album when present.
    pub album: Option<String>,
    /// Genre when present (`upnp:genre`).
    pub genre: Option<String>,
    /// Date when present (`dc:date`) — often ISO-8601 or a bare year.
    pub date: Option<String>,
    /// Composer when present — `upnp:composer`, or
    /// `upnp:author` / `upnp:artist` with `role="Composer"`
    /// (common on classical ContentDirectory trees).
    pub composer: Option<String>,
    /// Album art URI when present.
    pub album_art_uri: Option<String>,
    /// Duration string from DIDL (raw).
    pub duration: Option<String>,
    /// Resource URIs (http/https preferred by [`pick_stream_uri`]).
    pub res: Vec<DidlResource>,
}

/// One `<res>` element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DidlResource {
    /// Absolute or server-relative URI.
    pub uri: String,
    /// protocolInfo attribute.
    pub protocol_info: Option<String>,
}

/// Parse a DIDL-Lite XML document into objects.
pub fn parse_didl(xml: &str) -> Result<Vec<DidlObject>, DlnaError> {
    let doc = roxmltree::Document::parse(xml)
        .map_err(|e| DlnaError::Parse(e.to_string()))?;
    let mut out = Vec::new();
    for node in doc.descendants() {
        if node.has_tag_name("container") {
            out.push(DidlObject::Container(DidlContainer {
                id: attr(node, "id").unwrap_or_default(),
                parent_id: attr(node, "parentID").unwrap_or_default(),
                title: child_text(node, "title").unwrap_or_default(),
                child_count: attr(node, "childCount")
                    .and_then(|s| s.parse().ok()),
            }));
        } else if node.has_tag_name("item") {
            let mut res = Vec::new();
            for child in node.children().filter(|c| c.is_element()) {
                if child.has_tag_name("res") {
                    if let Some(uri) =
                        child.text().map(str::trim).filter(|s| !s.is_empty())
                    {
                        res.push(DidlResource {
                            uri: uri.to_string(),
                            protocol_info: attr(child, "protocolInfo"),
                        });
                    }
                }
            }
            out.push(DidlObject::Item(DidlItem {
                id: attr(node, "id").unwrap_or_default(),
                parent_id: attr(node, "parentID").unwrap_or_default(),
                title: child_text(node, "title").unwrap_or_default(),
                artist: child_text(node, "artist")
                    .or_else(|| child_text(node, "creator")),
                album: child_text(node, "album"),
                genre: child_text(node, "genre"),
                date: child_text(node, "date"),
                composer: extract_composer(node),
                album_art_uri: child_text(node, "albumArtURI"),
                duration: node
                    .children()
                    .find(|c| c.has_tag_name("res"))
                    .and_then(|c| attr(c, "duration")),
                res,
            }));
        }
    }
    Ok(out)
}

/// Prefer lossless-ish http(s) resources, else first http(s), else first res.
pub fn pick_stream_uri(item: &DidlItem) -> Option<String> {
    let http: Vec<&DidlResource> = item
        .res
        .iter()
        .filter(|r| {
            r.uri.starts_with("http://") || r.uri.starts_with("https://")
        })
        .collect();
    let rank = |r: &DidlResource| -> i32 {
        let p = r
            .protocol_info
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase();
        if p.contains("flac") || p.contains("alac") || p.contains("wav") {
            0
        } else if p.contains("mp3") || p.contains("mpeg") || p.contains("aac") {
            1
        } else {
            2
        }
    };
    http.iter()
        .copied()
        .min_by_key(|r| rank(r))
        .map(|r| r.uri.clone())
        .or_else(|| item.res.first().map(|r| r.uri.clone()))
}

fn attr(node: roxmltree::Node<'_, '_>, name: &str) -> Option<String> {
    node.attribute(name).map(|s| s.to_string())
}

fn child_text(node: roxmltree::Node<'_, '_>, local: &str) -> Option<String> {
    node.children()
        .find(|c| c.has_tag_name(local))
        .and_then(|c| c.text())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Composer extraction order for classical ContentDirectory trees:
/// 1. dedicated `composer` element (some servers emit it),
/// 2. `author` / `artist` whose `role` attribute is `Composer`
///    (UPnP ContentDirectory convention).
fn extract_composer(node: roxmltree::Node<'_, '_>) -> Option<String> {
    child_text(node, "composer")
        .or_else(|| child_text_with_role(node, "author", "Composer"))
        .or_else(|| child_text_with_role(node, "artist", "Composer"))
}

fn child_text_with_role(
    node: roxmltree::Node<'_, '_>,
    local: &str,
    role: &str,
) -> Option<String> {
    node.children()
        .find(|c| {
            c.has_tag_name(local)
                && c.attribute("role")
                    .is_some_and(|r| r.eq_ignore_ascii_case(role))
        })
        .and_then(|c| c.text())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mixed_didl() {
        let xml = r#"<?xml version="1.0"?>
<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"
           xmlns:dc="http://purl.org/dc/elements/1.1/"
           xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">
  <container id="2" parentID="0" childCount="12">
    <dc:title>Music</dc:title>
  </container>
  <item id="99" parentID="2">
    <dc:title>Song</dc:title>
    <upnp:artist>Artist</upnp:artist>
    <upnp:album>Album</upnp:album>
    <upnp:genre>Classical</upnp:genre>
    <dc:date>1997-01-01</dc:date>
    <upnp:author role="Composer">Ludwig van Beethoven</upnp:author>
    <res protocolInfo="http-get:*:audio/flac:*">http://x/a.flac</res>
    <res protocolInfo="http-get:*:audio/mpeg:*">http://x/a.mp3</res>
  </item>
</DIDL-Lite>"#;
        let objs = parse_didl(xml).expect("didl");
        assert_eq!(objs.len(), 2);
        match &objs[0] {
            DidlObject::Container(c) => {
                assert_eq!(c.id, "2");
                assert_eq!(c.title, "Music");
                assert_eq!(c.child_count, Some(12));
            }
            _ => panic!("expected container"),
        }
        match &objs[1] {
            DidlObject::Item(i) => {
                assert_eq!(
                    pick_stream_uri(i).as_deref(),
                    Some("http://x/a.flac")
                );
                assert_eq!(i.genre.as_deref(), Some("Classical"));
                assert_eq!(i.date.as_deref(), Some("1997-01-01"));
                assert_eq!(i.composer.as_deref(), Some("Ludwig van Beethoven"));
            }
            _ => panic!("expected item"),
        }
    }
}
