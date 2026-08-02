// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Device description XML → MediaServer control URL.

use crate::DlnaError;

/// A resolved UPnP MediaServer with ContentDirectory control URL.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MediaServer {
    /// Stable UPnP UDN / UUID (without `uuid:` prefix normalised in).
    pub service_id: String,
    /// Operator-facing friendly name.
    pub friendly_name: String,
    /// Absolute ContentDirectory control URL for SOAP Browse.
    pub control_url: String,
    /// Origin of the device description (scheme://host[:port]).
    pub base_url: String,
    /// Device description LOCATION URL.
    pub location: String,
}

/// Fetch and parse a device description into a [`MediaServer`].
pub async fn fetch_media_server(
    location: &str,
) -> Result<MediaServer, DlnaError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()?;
    let body = client.get(location).send().await?.text().await?;
    parse_device_description(location, &body)
}

/// Parse device description XML (testable without HTTP).
pub fn parse_device_description(
    location: &str,
    xml: &str,
) -> Result<MediaServer, DlnaError> {
    let doc = roxmltree::Document::parse(xml)
        .map_err(|e| DlnaError::Parse(e.to_string()))?;
    let base = url::Url::parse(location)?;
    let base_url = format!(
        "{}://{}{}",
        base.scheme(),
        base.host_str().unwrap_or("localhost"),
        match base.port() {
            Some(p) => format!(":{p}"),
            None => String::new(),
        }
    );

    let friendly_name = text_of(&doc, "friendlyName")
        .unwrap_or_else(|| "Media Server".to_string());
    let udn = text_of(&doc, "UDN").unwrap_or_default();
    let service_id = normalize_udn(&udn);

    let mut control_path: Option<String> = None;
    for node in doc.descendants() {
        if !node.has_tag_name("service") {
            continue;
        }
        let mut st = None;
        let mut cu = None;
        for child in node.children().filter(|c| c.is_element()) {
            match child.tag_name().name() {
                "serviceType" => st = child.text().map(str::trim),
                "controlURL" => cu = child.text().map(str::trim),
                _ => {}
            }
        }
        if st.is_some_and(|s| s.contains("ContentDirectory")) {
            control_path = cu.map(str::to_string);
            break;
        }
    }
    let control_path = control_path.ok_or_else(|| {
        DlnaError::Parse("no ContentDirectory controlURL".into())
    })?;
    let control_url = resolve_url(&base_url, location, &control_path)?;

    Ok(MediaServer {
        service_id,
        friendly_name,
        control_url,
        base_url,
        location: location.to_string(),
    })
}

fn text_of(doc: &roxmltree::Document<'_>, local: &str) -> Option<String> {
    doc.descendants()
        .find(|n| n.has_tag_name(local))
        .and_then(|n| n.text())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn normalize_udn(udn: &str) -> String {
    let t = udn.trim();
    t.strip_prefix("uuid:")
        .or_else(|| t.strip_prefix("UUID:"))
        .unwrap_or(t)
        .to_string()
}

fn resolve_url(
    base_url: &str,
    location: &str,
    path: &str,
) -> Result<String, DlnaError> {
    if path.starts_with("http://") || path.starts_with("https://") {
        return Ok(path.to_string());
    }
    if path.starts_with('/') {
        return Ok(format!("{base_url}{path}"));
    }
    let loc = url::Url::parse(location)?;
    Ok(loc.join(path)?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <device>
    <friendlyName>Jellyfin</friendlyName>
    <UDN>uuid:abcd-1234</UDN>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:ContentDirectory:1</serviceType>
        <controlURL>/dlna/abcd/contentdirectory/control</controlURL>
      </service>
    </serviceList>
  </device>
</root>"#;

    #[test]
    fn parse_jellyfin_shaped_description() {
        let m = parse_device_description(
            "http://192.0.2.5:8096/dlna/abcd/description.xml",
            SAMPLE,
        )
        .expect("parse");
        assert_eq!(m.service_id, "abcd-1234");
        assert_eq!(m.friendly_name, "Jellyfin");
        assert_eq!(
            m.control_url,
            "http://192.0.2.5:8096/dlna/abcd/contentdirectory/control"
        );
        assert_eq!(m.base_url, "http://192.0.2.5:8096");
    }
}
