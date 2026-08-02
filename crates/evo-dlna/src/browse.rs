// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Paged ContentDirectory Browse.

use crate::didl::{parse_didl, DidlObject};
use crate::DlnaError;

/// Default page size for a single DLNA SOAP Browse.
pub const DLNA_PAGE_DEFAULT: u32 = 50;
/// Hard cap for a single DLNA SOAP Browse. Requests above this
/// are clamped so a misconfigured operator page-size cannot force
/// an unbounded SOAP call.
pub const DLNA_PAGE_HARD_CAP: u32 = 100;

/// Parameters for one paged Browse call.
#[derive(Debug, Clone)]
pub struct BrowseParams {
    /// Absolute ContentDirectory control URL.
    pub control_url: String,
    /// Object id to browse (`"0"` = root).
    pub object_id: String,
    /// Zero-based page index.
    pub page: u32,
    /// Requested page size (clamped to [`DLNA_PAGE_HARD_CAP`]).
    pub page_size: u32,
}

/// One page of Browse results.
#[derive(Debug, Clone)]
pub struct BrowsePage {
    /// Objects on this page.
    pub objects: Vec<DidlObject>,
    /// Echoed page index.
    pub page: u32,
    /// Effective page size used in the SOAP call.
    pub page_size: u32,
    /// `NumberReturned` from the server when parseable.
    pub number_returned: u32,
    /// `TotalMatches` from the server when parseable.
    pub total_matches: Option<u32>,
    /// More pages available.
    pub truncated: bool,
    /// Next page index when [`Self::truncated`].
    pub next_page: Option<u32>,
}

/// Issue a single bounded ContentDirectory `Browse` (DirectChildren).
pub async fn browse_page(
    params: BrowseParams,
) -> Result<BrowsePage, DlnaError> {
    let page_size = params.page_size.clamp(1, DLNA_PAGE_HARD_CAP);
    let starting = params.page.saturating_mul(page_size);
    let envelope = soap_browse_envelope(&params.object_id, starting, page_size);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(12))
        .build()?;
    let resp = client
        .post(&params.control_url)
        .header("Content-Type", "text/xml; charset=\"utf-8\"")
        .header(
            "SOAPACTION",
            "\"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"",
        )
        .body(envelope)
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(DlnaError::Soap(format!(
            "HTTP {status}: {}",
            truncate(&body, 240)
        )));
    }
    if body.contains("Fault") && body.contains("UPnPError") {
        return Err(DlnaError::Soap(truncate(&body, 240)));
    }
    let result_xml = extract_tag(&body, "Result").ok_or_else(|| {
        DlnaError::Soap("Browse response missing Result".into())
    })?;
    // Result is often XML-escaped inside the SOAP body.
    let didl_xml = xml_unescape(&result_xml);
    let objects = parse_didl(&didl_xml)?;
    let number_returned = extract_tag(&body, "NumberReturned")
        .and_then(|s| s.parse().ok())
        .unwrap_or(objects.len() as u32);
    let total_matches =
        extract_tag(&body, "TotalMatches").and_then(|s| s.parse().ok());
    let truncated = match total_matches {
        Some(total) => starting.saturating_add(number_returned) < total,
        None => number_returned >= page_size,
    };
    let next_page = if truncated {
        Some(params.page.saturating_add(1))
    } else {
        None
    };
    Ok(BrowsePage {
        objects,
        page: params.page,
        page_size,
        number_returned,
        total_matches,
        truncated,
        next_page,
    })
}

fn soap_browse_envelope(
    object_id: &str,
    starting: u32,
    requested: u32,
) -> String {
    let oid = xml_escape(object_id);
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
      <ObjectID>{oid}</ObjectID>
      <BrowseFlag>BrowseDirectChildren</BrowseFlag>
      <Filter>*</Filter>
      <StartingIndex>{starting}</StartingIndex>
      <RequestedCount>{requested}</RequestedCount>
      <SortCriteria></SortCriteria>
    </u:Browse>
  </s:Body>
</s:Envelope>"#
    )
}

fn extract_tag(xml: &str, local: &str) -> Option<String> {
    // Namespace-tolerant: <Result> or <ns:Result>
    let needle = format!("{local}>");
    let mut search_from = 0;
    let start = loop {
        let rel = xml[search_from..].find(&needle)?;
        let abs = search_from + rel;
        // Must be preceded by '<' or ':'
        if abs > 0 {
            let prev = xml.as_bytes()[abs - 1];
            if prev == b'<' || prev == b':' {
                break abs + needle.len();
            }
        }
        search_from = abs + 1;
    };
    let close_needle = format!("/{local}>");
    let rel = xml[start..].find(&close_needle)?;
    let close_at = start + rel;
    // Walk back to '<' of the closing tag.
    let content_end = xml[..close_at].rfind('<')?;
    Some(xml[start..content_end].to_string())
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn xml_unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soap_envelope_includes_paging() {
        let env = soap_browse_envelope("2", 50, 50);
        assert!(env.contains("<StartingIndex>50</StartingIndex>"));
        assert!(env.contains("<RequestedCount>50</RequestedCount>"));
        assert!(env.contains("<ObjectID>2</ObjectID>"));
    }

    #[test]
    fn page_size_constants_match_plan() {
        assert_eq!(DLNA_PAGE_DEFAULT, 50);
        assert_eq!(DLNA_PAGE_HARD_CAP, 100);
    }

    #[test]
    fn extract_result_unescapes() {
        let body = r#"<s:Envelope><s:Body><u:BrowseResponse>
<Result>&lt;DIDL-Lite&gt;&lt;container id=&quot;1&quot; parentID=&quot;0&quot;&gt;&lt;dc:title&gt;A&lt;/dc:title&gt;&lt;/container&gt;&lt;/DIDL-Lite&gt;</Result>
<NumberReturned>1</NumberReturned>
<TotalMatches>20</TotalMatches>
</u:BrowseResponse></s:Body></s:Envelope>"#;
        let result = extract_tag(body, "Result").expect("result");
        let didl = xml_unescape(&result);
        assert!(didl.contains("<container"));
        assert_eq!(extract_tag(body, "TotalMatches").as_deref(), Some("20"));
    }
}
