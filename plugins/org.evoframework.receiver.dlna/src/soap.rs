// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! SOAP for UPnP control: read an action request, build the response or the
//! fault.

use std::collections::HashMap;

use crate::description::xml_escape;

/// A parsed control request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    /// The action name (the body element's local name).
    pub name: String,
    /// The namespace of the action element, normally the service type.
    pub namespace: String,
    /// Arguments by name.
    pub args: HashMap<String, String>,
}

/// Why a request body could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// Not well-formed XML.
    #[error("body is not well-formed XML")]
    NotXml,
    /// No `Envelope/Body/<action>` element.
    #[error("no action in the SOAP body")]
    NoAction,
}

/// Parse an envelope. Argument values are the text of each child element of
/// the action; whitespace-only values read as empty.
pub fn parse_action(body: &str) -> Result<Action, ParseError> {
    let doc =
        roxmltree::Document::parse(body).map_err(|_| ParseError::NotXml)?;
    let body_el = doc
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == "Body")
        .ok_or(ParseError::NoAction)?;
    let action = body_el
        .children()
        .find(|n| n.is_element())
        .ok_or(ParseError::NoAction)?;
    let mut args = HashMap::new();
    for c in action.children().filter(|n| n.is_element()) {
        let v = c.text().unwrap_or("").to_string();
        args.insert(c.tag_name().name().to_string(), v);
    }
    Ok(Action {
        name: action.tag_name().name().to_string(),
        namespace: action.tag_name().namespace().unwrap_or("").to_string(),
        args,
    })
}

/// A UPnP control fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fault {
    /// UPnP error code.
    pub code: u32,
    /// UPnP error description.
    pub description: &'static str,
}

impl Fault {
    /// 401: no such action on this service.
    pub const INVALID_ACTION: Fault = Fault {
        code: 401,
        description: "Invalid Action",
    };
    /// 402: a missing, extra or malformed argument.
    pub const INVALID_ARGS: Fault = Fault {
        code: 402,
        description: "Invalid Args",
    };
    /// 501: the device could not carry the action out.
    pub const ACTION_FAILED: Fault = Fault {
        code: 501,
        description: "Action Failed",
    };
    /// 701: the action is not valid in the current transport state.
    pub const TRANSITION_NOT_AVAILABLE: Fault = Fault {
        code: 701,
        description: "Transition not available",
    };
    /// 702: nothing is loaded.
    pub const NO_CONTENTS: Fault = Fault {
        code: 702,
        description: "No contents",
    };
    /// 710: seek mode not supported.
    pub const SEEK_MODE: Fault = Fault {
        code: 710,
        description: "Seek mode not supported",
    };
    /// 711: seek target not valid.
    pub const SEEK_TARGET: Fault = Fault {
        code: 711,
        description: "Illegal seek target",
    };
    /// 718: only instance 0 exists.
    pub const INVALID_INSTANCE: Fault = Fault {
        code: 718,
        description: "Invalid InstanceID",
    };
}

/// The fault envelope.
pub fn fault_xml(f: &Fault) -> String {
    format!(
        "<?xml version=\"1.0\"?>\n\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body><s:Fault><faultcode>s:Client</faultcode>\
         <faultstring>UPnPError</faultstring><detail>\
         <UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\">\
         <errorCode>{}</errorCode><errorDescription>{}</errorDescription>\
         </UPnPError></detail></s:Fault></s:Body></s:Envelope>",
        f.code, f.description
    )
}

/// The success envelope: `<u:<Action>Response xmlns:u="<service>">` with the
/// output arguments, in order. Values are escaped.
pub fn response_xml(
    service_type: &str,
    action: &str,
    out: &[(&str, String)],
) -> String {
    let mut args = String::new();
    for (k, v) in out {
        args.push_str(&format!("<{k}>{}</{k}>", xml_escape(v)));
    }
    format!(
        "<?xml version=\"1.0\"?>\n\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body><u:{action}Response xmlns:u=\"{service_type}\">{args}</u:{action}Response>\
         </s:Body></s:Envelope>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SET_URI: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
 <s:Body>
  <u:SetAVTransportURI xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
   <InstanceID>0</InstanceID>
   <CurrentURI>http://192.168.1.5:9000/a.flac?x=1&amp;y=2</CurrentURI>
   <CurrentURIMetaData>&lt;DIDL-Lite&gt;&lt;/DIDL-Lite&gt;</CurrentURIMetaData>
  </u:SetAVTransportURI>
 </s:Body>
</s:Envelope>"#;

    #[test]
    fn a_real_control_request_parses_with_entities_decoded() {
        let a = parse_action(SET_URI).unwrap();
        assert_eq!(a.name, "SetAVTransportURI");
        assert_eq!(a.namespace, "urn:schemas-upnp-org:service:AVTransport:1");
        assert_eq!(a.args["InstanceID"], "0");
        assert_eq!(
            a.args["CurrentURI"],
            "http://192.168.1.5:9000/a.flac?x=1&y=2"
        );
        assert_eq!(a.args["CurrentURIMetaData"], "<DIDL-Lite></DIDL-Lite>");
    }

    #[test]
    fn an_action_with_no_arguments_parses() {
        let a = parse_action(
            r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:GetProtocolInfo xmlns:u="urn:schemas-upnp-org:service:ConnectionManager:1"/></s:Body></s:Envelope>"#,
        )
        .unwrap();
        assert_eq!(a.name, "GetProtocolInfo");
        assert!(a.args.is_empty());
    }

    #[test]
    fn garbage_and_an_empty_body_are_rejected() {
        assert_eq!(parse_action("not xml <<<"), Err(ParseError::NotXml));
        assert_eq!(
            parse_action(r#"<s:Envelope xmlns:s="x"><s:Body/></s:Envelope>"#),
            Err(ParseError::NoAction)
        );
        assert_eq!(parse_action(r#"<a/>"#), Err(ParseError::NoAction));
    }

    #[test]
    fn a_response_escapes_values_and_round_trips() {
        let x = response_xml(
            "urn:schemas-upnp-org:service:AVTransport:1",
            "GetMediaInfo",
            &[
                ("CurrentURI", "http://h/?a=1&b=2".into()),
                ("NrTracks", "1".into()),
            ],
        );
        let doc = roxmltree::Document::parse(&x).expect("well-formed");
        let r = doc
            .descendants()
            .find(|n| n.tag_name().name() == "GetMediaInfoResponse")
            .unwrap();
        assert_eq!(
            r.tag_name().namespace(),
            Some("urn:schemas-upnp-org:service:AVTransport:1")
        );
        let uri = r.children().find(|n| n.has_tag_name("CurrentURI")).unwrap();
        assert_eq!(uri.text(), Some("http://h/?a=1&b=2"));
    }

    #[test]
    fn a_fault_carries_the_code() {
        let x = fault_xml(&Fault::TRANSITION_NOT_AVAILABLE);
        roxmltree::Document::parse(&x).expect("well-formed");
        assert!(x.contains("<errorCode>701</errorCode>"));
    }
}
