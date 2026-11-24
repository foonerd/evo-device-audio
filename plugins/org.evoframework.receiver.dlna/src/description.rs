// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! The device description and the three service descriptions.
//!
//! A UPnP controller learns what the device is from `description.xml`
//! and what each service can do from its SCPD document. Both are
//! generated here from small tables, so the action list a later slice
//! implements and the document a controller reads are one source.

use std::fmt::Write as _;

/// Device type of a renderer.
pub const DEVICE_TYPE: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";

/// One of the three services a MediaRenderer carries.
#[derive(Debug, Clone, Copy)]
pub struct ServiceDef {
    /// Short name used in the URL paths (`/scpd/<name>.xml`, `/ctl/<name>`,
    /// `/evt/<name>`).
    pub name: &'static str,
    /// `urn:schemas-upnp-org:service:<name>:1`.
    pub service_type: &'static str,
    /// `urn:upnp-org:serviceId:<name>`.
    pub service_id: &'static str,
}

/// The services, in the order they are listed.
pub const SERVICES: [ServiceDef; 3] = [
    ServiceDef {
        name: "AVTransport",
        service_type: "urn:schemas-upnp-org:service:AVTransport:1",
        service_id: "urn:upnp-org:serviceId:AVTransport",
    },
    ServiceDef {
        name: "RenderingControl",
        service_type: "urn:schemas-upnp-org:service:RenderingControl:1",
        service_id: "urn:upnp-org:serviceId:RenderingControl",
    },
    ServiceDef {
        name: "ConnectionManager",
        service_type: "urn:schemas-upnp-org:service:ConnectionManager:1",
        service_id: "urn:upnp-org:serviceId:ConnectionManager",
    },
];

/// One device icon: the Evo mark, as PNG.
#[derive(Debug, Clone, Copy)]
pub struct IconDef {
    /// File name under `/icon/`.
    pub name: &'static str,
    /// Pixels, square.
    pub size: u32,
    /// The PNG bytes.
    pub bytes: &'static [u8],
}

/// The icons the description lists. A controller paints the largest it
/// likes; without any it paints a generic glyph, which loses the device's
/// identity.
pub const ICONS: [IconDef; 3] = [
    IconDef {
        name: "evo-48.png",
        size: 48,
        bytes: include_bytes!("../assets/icons/icon-48.png"),
    },
    IconDef {
        name: "evo-120.png",
        size: 120,
        bytes: include_bytes!("../assets/icons/icon-120.png"),
    },
    IconDef {
        name: "evo-256.png",
        size: 256,
        bytes: include_bytes!("../assets/icons/icon-256.png"),
    },
];

/// Find an icon by file name.
pub fn icon(name: &str) -> Option<&'static IconDef> {
    ICONS.iter().find(|i| i.name == name)
}

/// Find a service by its short name.
pub fn service(name: &str) -> Option<&'static ServiceDef> {
    SERVICES.iter().find(|s| s.name == name)
}

/// Escape text for an XML element body or attribute.
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// The `description.xml` body for a device.
pub fn device_description(udn_uuid: &str, friendly_name: &str) -> String {
    let mut x = String::new();
    x.push_str("<?xml version=\"1.0\"?>\n");
    x.push_str(
        "<root xmlns=\"urn:schemas-upnp-org:device-1-0\" \
         xmlns:dlna=\"urn:schemas-dlna-org:device-1-0\">\n",
    );
    x.push_str(
        "  <specVersion><major>1</major><minor>0</minor></specVersion>\n",
    );
    x.push_str("  <device>\n");
    x.push_str("    <dlna:X_DLNADOC>DMR-1.50</dlna:X_DLNADOC>\n");
    let _ = writeln!(x, "    <deviceType>{DEVICE_TYPE}</deviceType>");
    let _ = writeln!(
        x,
        "    <friendlyName>{}</friendlyName>",
        xml_escape(friendly_name)
    );
    x.push_str("    <manufacturer>evo</manufacturer>\n");
    x.push_str("    <modelName>evo-device-audio</modelName>\n");
    x.push_str("    <modelDescription>evo audio player</modelDescription>\n");
    let _ = writeln!(x, "    <UDN>uuid:{}</UDN>", xml_escape(udn_uuid));
    x.push_str("    <iconList>\n");
    for i in ICONS {
        x.push_str("      <icon>\n");
        x.push_str("        <mimetype>image/png</mimetype>\n");
        let _ = writeln!(x, "        <width>{}</width>", i.size);
        let _ = writeln!(x, "        <height>{}</height>", i.size);
        x.push_str("        <depth>32</depth>\n");
        let _ = writeln!(x, "        <url>/icon/{}</url>", i.name);
        x.push_str("      </icon>\n");
    }
    x.push_str("    </iconList>\n");
    x.push_str("    <serviceList>\n");
    for s in SERVICES {
        x.push_str("      <service>\n");
        let _ = writeln!(
            x,
            "        <serviceType>{}</serviceType>",
            s.service_type
        );
        let _ = writeln!(x, "        <serviceId>{}</serviceId>", s.service_id);
        let _ = writeln!(x, "        <SCPDURL>/scpd/{}.xml</SCPDURL>", s.name);
        let _ = writeln!(x, "        <controlURL>/ctl/{}</controlURL>", s.name);
        let _ =
            writeln!(x, "        <eventSubURL>/evt/{}</eventSubURL>", s.name);
        x.push_str("      </service>\n");
    }
    x.push_str("    </serviceList>\n");
    x.push_str("  </device>\n");
    x.push_str("</root>\n");
    x
}

// -------------------------------------------------------------------
// SCPD tables
// -------------------------------------------------------------------

/// One action argument.
struct Arg {
    name: &'static str,
    input: bool,
    var: &'static str,
}

/// One action.
struct Action {
    name: &'static str,
    args: &'static [Arg],
}

/// One state variable.
struct Var {
    name: &'static str,
    ty: &'static str,
    evented: bool,
    allowed: &'static [&'static str],
    /// `(min, max, step)` for a numeric range.
    range: Option<(i64, i64, i64)>,
    default: Option<&'static str>,
}

const fn i(name: &'static str, var: &'static str) -> Arg {
    Arg {
        name,
        input: true,
        var,
    }
}
const fn o(name: &'static str, var: &'static str) -> Arg {
    Arg {
        name,
        input: false,
        var,
    }
}
const fn var(name: &'static str, ty: &'static str) -> Var {
    Var {
        name,
        ty,
        evented: false,
        allowed: &[],
        range: None,
        default: None,
    }
}

const INST: &str = "A_ARG_TYPE_InstanceID";

const AV_ACTIONS: &[Action] = &[
    Action {
        name: "SetAVTransportURI",
        args: &[
            i("InstanceID", INST),
            i("CurrentURI", "AVTransportURI"),
            i("CurrentURIMetaData", "AVTransportURIMetaData"),
        ],
    },
    Action {
        name: "SetNextAVTransportURI",
        args: &[
            i("InstanceID", INST),
            i("NextURI", "NextAVTransportURI"),
            i("NextURIMetaData", "NextAVTransportURIMetaData"),
        ],
    },
    Action {
        name: "GetMediaInfo",
        args: &[
            i("InstanceID", INST),
            o("NrTracks", "NumberOfTracks"),
            o("MediaDuration", "CurrentMediaDuration"),
            o("CurrentURI", "AVTransportURI"),
            o("CurrentURIMetaData", "AVTransportURIMetaData"),
            o("NextURI", "NextAVTransportURI"),
            o("NextURIMetaData", "NextAVTransportURIMetaData"),
            o("PlayMedium", "PlaybackStorageMedium"),
            o("RecordMedium", "RecordStorageMedium"),
            o("WriteStatus", "RecordMediumWriteStatus"),
        ],
    },
    Action {
        name: "GetTransportInfo",
        args: &[
            i("InstanceID", INST),
            o("CurrentTransportState", "TransportState"),
            o("CurrentTransportStatus", "TransportStatus"),
            o("CurrentSpeed", "TransportPlaySpeed"),
        ],
    },
    Action {
        name: "GetPositionInfo",
        args: &[
            i("InstanceID", INST),
            o("Track", "CurrentTrack"),
            o("TrackDuration", "CurrentTrackDuration"),
            o("TrackMetaData", "CurrentTrackMetaData"),
            o("TrackURI", "CurrentTrackURI"),
            o("RelTime", "RelativeTimePosition"),
            o("AbsTime", "AbsoluteTimePosition"),
            o("RelCount", "RelativeCounterPosition"),
            o("AbsCount", "AbsoluteCounterPosition"),
        ],
    },
    Action {
        name: "GetDeviceCapabilities",
        args: &[
            i("InstanceID", INST),
            o("PlayMedia", "PossiblePlaybackStorageMedia"),
            o("RecMedia", "PossibleRecordStorageMedia"),
            o("RecQualityModes", "PossibleRecordQualityModes"),
        ],
    },
    Action {
        name: "GetTransportSettings",
        args: &[
            i("InstanceID", INST),
            o("PlayMode", "CurrentPlayMode"),
            o("RecQualityMode", "CurrentRecordQualityMode"),
        ],
    },
    Action {
        name: "Stop",
        args: &[i("InstanceID", INST)],
    },
    Action {
        name: "Play",
        args: &[i("InstanceID", INST), i("Speed", "TransportPlaySpeed")],
    },
    Action {
        name: "Pause",
        args: &[i("InstanceID", INST)],
    },
    Action {
        name: "Seek",
        args: &[
            i("InstanceID", INST),
            i("Unit", "A_ARG_TYPE_SeekMode"),
            i("Target", "A_ARG_TYPE_SeekTarget"),
        ],
    },
    Action {
        name: "Next",
        args: &[i("InstanceID", INST)],
    },
    Action {
        name: "Previous",
        args: &[i("InstanceID", INST)],
    },
    Action {
        name: "SetPlayMode",
        args: &[i("InstanceID", INST), i("NewPlayMode", "CurrentPlayMode")],
    },
    Action {
        name: "GetCurrentTransportActions",
        args: &[
            i("InstanceID", INST),
            o("Actions", "CurrentTransportActions"),
        ],
    },
];

const AV_VARS: &[Var] = &[
    Var {
        allowed: &[
            "STOPPED",
            "PAUSED_PLAYBACK",
            "PLAYING",
            "TRANSITIONING",
            "NO_MEDIA_PRESENT",
        ],
        ..var("TransportState", "string")
    },
    Var {
        allowed: &["OK", "ERROR_OCCURRED"],
        ..var("TransportStatus", "string")
    },
    Var {
        allowed: &["NETWORK", "NONE"],
        ..var("PlaybackStorageMedium", "string")
    },
    Var {
        allowed: &["NOT_IMPLEMENTED"],
        ..var("RecordStorageMedium", "string")
    },
    var("PossiblePlaybackStorageMedia", "string"),
    var("PossibleRecordStorageMedia", "string"),
    Var {
        allowed: &["NORMAL", "SHUFFLE", "REPEAT_ONE", "REPEAT_ALL"],
        default: Some("NORMAL"),
        ..var("CurrentPlayMode", "string")
    },
    Var {
        allowed: &["1"],
        default: Some("1"),
        ..var("TransportPlaySpeed", "string")
    },
    Var {
        allowed: &["NOT_IMPLEMENTED"],
        ..var("RecordMediumWriteStatus", "string")
    },
    Var {
        allowed: &["NOT_IMPLEMENTED"],
        ..var("CurrentRecordQualityMode", "string")
    },
    var("PossibleRecordQualityModes", "string"),
    Var {
        range: Some((0, 4_294_967_295, 1)),
        ..var("NumberOfTracks", "ui4")
    },
    Var {
        range: Some((0, 4_294_967_295, 1)),
        ..var("CurrentTrack", "ui4")
    },
    var("CurrentTrackDuration", "string"),
    var("CurrentMediaDuration", "string"),
    var("CurrentTrackMetaData", "string"),
    var("CurrentTrackURI", "string"),
    var("AVTransportURI", "string"),
    var("AVTransportURIMetaData", "string"),
    var("NextAVTransportURI", "string"),
    var("NextAVTransportURIMetaData", "string"),
    var("RelativeTimePosition", "string"),
    var("AbsoluteTimePosition", "string"),
    var("RelativeCounterPosition", "i4"),
    var("AbsoluteCounterPosition", "ui4"),
    var("CurrentTransportActions", "string"),
    Var {
        evented: true,
        ..var("LastChange", "string")
    },
    Var {
        allowed: &[
            "TRACK_NR",
            "REL_TIME",
            "ABS_TIME",
            "ABS_COUNT",
            "REL_COUNT",
            "TAPE-INDEX",
            "FRAME",
        ],
        ..var("A_ARG_TYPE_SeekMode", "string")
    },
    var("A_ARG_TYPE_SeekTarget", "string"),
    var("A_ARG_TYPE_InstanceID", "ui4"),
];

const RC_ACTIONS: &[Action] = &[
    Action {
        name: "ListPresets",
        args: &[
            i("InstanceID", INST),
            o("CurrentPresetNameList", "PresetNameList"),
        ],
    },
    Action {
        name: "SelectPreset",
        args: &[
            i("InstanceID", INST),
            i("PresetName", "A_ARG_TYPE_PresetName"),
        ],
    },
    Action {
        name: "GetMute",
        args: &[
            i("InstanceID", INST),
            i("Channel", "A_ARG_TYPE_Channel"),
            o("CurrentMute", "Mute"),
        ],
    },
    Action {
        name: "SetMute",
        args: &[
            i("InstanceID", INST),
            i("Channel", "A_ARG_TYPE_Channel"),
            i("DesiredMute", "Mute"),
        ],
    },
    Action {
        name: "GetVolume",
        args: &[
            i("InstanceID", INST),
            i("Channel", "A_ARG_TYPE_Channel"),
            o("CurrentVolume", "Volume"),
        ],
    },
    Action {
        name: "SetVolume",
        args: &[
            i("InstanceID", INST),
            i("Channel", "A_ARG_TYPE_Channel"),
            i("DesiredVolume", "Volume"),
        ],
    },
];

const RC_VARS: &[Var] = &[
    var("PresetNameList", "string"),
    Var {
        evented: true,
        ..var("LastChange", "string")
    },
    var("Mute", "boolean"),
    Var {
        range: Some((0, 100, 1)),
        ..var("Volume", "ui2")
    },
    Var {
        allowed: &["Master"],
        ..var("A_ARG_TYPE_Channel", "string")
    },
    Var {
        allowed: &["FactoryDefaults"],
        ..var("A_ARG_TYPE_PresetName", "string")
    },
    var("A_ARG_TYPE_InstanceID", "ui4"),
];

const CM_ACTIONS: &[Action] = &[
    Action {
        name: "GetProtocolInfo",
        args: &[
            o("Source", "SourceProtocolInfo"),
            o("Sink", "SinkProtocolInfo"),
        ],
    },
    Action {
        name: "GetCurrentConnectionIDs",
        args: &[o("ConnectionIDs", "CurrentConnectionIDs")],
    },
    Action {
        name: "GetCurrentConnectionInfo",
        args: &[
            i("ConnectionID", "A_ARG_TYPE_ConnectionID"),
            o("RcsID", "A_ARG_TYPE_RcsID"),
            o("AVTransportID", "A_ARG_TYPE_AVTransportID"),
            o("ProtocolInfo", "A_ARG_TYPE_ProtocolInfo"),
            o("PeerConnectionManager", "A_ARG_TYPE_ConnectionManager"),
            o("PeerConnectionID", "A_ARG_TYPE_ConnectionID"),
            o("Direction", "A_ARG_TYPE_Direction"),
            o("Status", "A_ARG_TYPE_ConnectionStatus"),
        ],
    },
];

const CM_VARS: &[Var] = &[
    Var {
        evented: true,
        ..var("SourceProtocolInfo", "string")
    },
    Var {
        evented: true,
        ..var("SinkProtocolInfo", "string")
    },
    Var {
        evented: true,
        ..var("CurrentConnectionIDs", "string")
    },
    Var {
        allowed: &[
            "OK",
            "ContentFormatMismatch",
            "InsufficientBandwidth",
            "UnreliableChannel",
            "Unknown",
        ],
        ..var("A_ARG_TYPE_ConnectionStatus", "string")
    },
    var("A_ARG_TYPE_ConnectionManager", "string"),
    Var {
        allowed: &["Input", "Output"],
        ..var("A_ARG_TYPE_Direction", "string")
    },
    var("A_ARG_TYPE_ProtocolInfo", "string"),
    var("A_ARG_TYPE_ConnectionID", "i4"),
    var("A_ARG_TYPE_AVTransportID", "i4"),
    var("A_ARG_TYPE_RcsID", "i4"),
];

fn tables(name: &str) -> Option<(&'static [Action], &'static [Var])> {
    match name {
        "AVTransport" => Some((AV_ACTIONS, AV_VARS)),
        "RenderingControl" => Some((RC_ACTIONS, RC_VARS)),
        "ConnectionManager" => Some((CM_ACTIONS, CM_VARS)),
        _ => None,
    }
}

/// The action names of a service, in declaration order.
pub fn action_names(service_name: &str) -> Vec<&'static str> {
    tables(service_name)
        .map(|(a, _)| a.iter().map(|x| x.name).collect())
        .unwrap_or_default()
}

/// The SCPD document of a service, or `None` for an unknown name.
pub fn scpd(service_name: &str) -> Option<String> {
    let (actions, vars) = tables(service_name)?;
    let mut x = String::new();
    x.push_str("<?xml version=\"1.0\"?>\n");
    x.push_str("<scpd xmlns=\"urn:schemas-upnp-org:service-1-0\">\n");
    x.push_str(
        "  <specVersion><major>1</major><minor>0</minor></specVersion>\n",
    );
    x.push_str("  <actionList>\n");
    for a in actions {
        x.push_str("    <action>\n");
        let _ = writeln!(x, "      <name>{}</name>", a.name);
        if !a.args.is_empty() {
            x.push_str("      <argumentList>\n");
            for g in a.args {
                x.push_str("        <argument>\n");
                let _ = writeln!(x, "          <name>{}</name>", g.name);
                let _ = writeln!(
                    x,
                    "          <direction>{}</direction>",
                    if g.input { "in" } else { "out" }
                );
                let _ = writeln!(
                    x,
                    "          <relatedStateVariable>{}</relatedStateVariable>",
                    g.var
                );
                x.push_str("        </argument>\n");
            }
            x.push_str("      </argumentList>\n");
        }
        x.push_str("    </action>\n");
    }
    x.push_str("  </actionList>\n");
    x.push_str("  <serviceStateTable>\n");
    for v in vars {
        let _ = writeln!(
            x,
            "    <stateVariable sendEvents=\"{}\">",
            if v.evented { "yes" } else { "no" }
        );
        let _ = writeln!(x, "      <name>{}</name>", v.name);
        let _ = writeln!(x, "      <dataType>{}</dataType>", v.ty);
        if let Some(d) = v.default {
            let _ = writeln!(x, "      <defaultValue>{d}</defaultValue>");
        }
        if !v.allowed.is_empty() {
            x.push_str("      <allowedValueList>\n");
            for a in v.allowed {
                let _ = writeln!(x, "        <allowedValue>{a}</allowedValue>");
            }
            x.push_str("      </allowedValueList>\n");
        }
        if let Some((lo, hi, step)) = v.range {
            x.push_str("      <allowedValueRange>\n");
            let _ = writeln!(x, "        <minimum>{lo}</minimum>");
            let _ = writeln!(x, "        <maximum>{hi}</maximum>");
            let _ = writeln!(x, "        <step>{step}</step>");
            x.push_str("      </allowedValueRange>\n");
        }
        x.push_str("    </stateVariable>\n");
    }
    x.push_str("  </serviceStateTable>\n");
    x.push_str("</scpd>\n");
    Some(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_description_is_a_media_renderer_with_three_services() {
        let x = device_description(
            "11111111-2222-3333-4444-555555555555",
            "Kitchen",
        );
        let doc = roxmltree::Document::parse(&x).expect("well-formed");
        let text = |tag: &str| {
            doc.descendants()
                .find(|n| n.has_tag_name(tag))
                .and_then(|n| n.text())
                .unwrap_or("")
                .to_string()
        };
        assert_eq!(text("deviceType"), DEVICE_TYPE);
        assert_eq!(text("friendlyName"), "Kitchen");
        assert_eq!(text("UDN"), "uuid:11111111-2222-3333-4444-555555555555");
        let types: Vec<String> = doc
            .descendants()
            .filter(|n| n.has_tag_name("serviceType"))
            .filter_map(|n| n.text().map(str::to_string))
            .collect();
        assert_eq!(types.len(), 3);
        for s in SERVICES {
            assert!(types.contains(&s.service_type.to_string()));
        }
        // No ContentDirectory: this is a renderer, not a server.
        assert!(!x.contains("ContentDirectory"));
    }

    /// The device must carry the Evo mark: a description with no icon
    /// list makes a controller paint a generic glyph.
    #[test]
    fn the_description_lists_the_evo_icons() {
        let x = device_description("u", "Kitchen");
        let doc = roxmltree::Document::parse(&x).expect("well-formed");
        let urls: Vec<String> = doc
            .descendants()
            .filter(|n| n.has_tag_name("icon"))
            .filter_map(|n| {
                n.children()
                    .find(|c| c.has_tag_name("url"))
                    .and_then(|c| c.text())
                    .map(str::to_string)
            })
            .collect();
        assert_eq!(urls.len(), ICONS.len());
        assert!(urls.len() >= 2, "a description without icons is RED");
        for u in urls {
            let name = u.strip_prefix("/icon/").expect("served under /icon/");
            assert!(icon(name).is_some(), "{u} is not served");
        }
        // Schema order: the icon list precedes the service list.
        assert!(
            x.find("<iconList>").unwrap() < x.find("<serviceList>").unwrap()
        );
    }

    #[test]
    fn every_icon_is_a_png_of_its_declared_size() {
        for i in ICONS {
            assert_eq!(&i.bytes[..8], b"\x89PNG\r\n\x1a\n", "{}", i.name);
            let w = u32::from_be_bytes(i.bytes[16..20].try_into().unwrap());
            let h = u32::from_be_bytes(i.bytes[20..24].try_into().unwrap());
            assert_eq!((w, h), (i.size, i.size), "{}", i.name);
        }
    }

    #[test]
    fn a_hostile_friendly_name_is_escaped() {
        let x = device_description("u", "A & <b>\"B\"</b>");
        let doc = roxmltree::Document::parse(&x).expect("well-formed");
        let name = doc
            .descendants()
            .find(|n| n.has_tag_name("friendlyName"))
            .and_then(|n| n.text())
            .unwrap();
        assert_eq!(name, "A & <b>\"B\"</b>");
    }

    #[test]
    fn every_service_has_a_parseable_scpd() {
        for s in SERVICES {
            let x = scpd(s.name).expect("known service");
            roxmltree::Document::parse(&x)
                .unwrap_or_else(|e| panic!("{}: {e}", s.name));
        }
        assert!(scpd("ContentDirectory").is_none());
    }

    #[test]
    fn every_argument_names_a_declared_state_variable() {
        for s in SERVICES {
            let (actions, vars) = tables(s.name).unwrap();
            let declared: Vec<&str> = vars.iter().map(|v| v.name).collect();
            for a in actions {
                for g in a.args {
                    assert!(
                        declared.contains(&g.var),
                        "{}.{}: {} is not a declared state variable",
                        s.name,
                        a.name,
                        g.var
                    );
                }
            }
        }
    }

    #[test]
    fn the_required_actions_are_declared() {
        let av = action_names("AVTransport");
        for a in [
            "SetAVTransportURI",
            "Play",
            "Pause",
            "Stop",
            "Seek",
            "Next",
            "Previous",
            "GetTransportInfo",
            "GetPositionInfo",
            "GetMediaInfo",
        ] {
            assert!(av.contains(&a), "AVTransport lacks {a}");
        }
        let rc = action_names("RenderingControl");
        for a in ["GetVolume", "SetVolume", "GetMute", "SetMute"] {
            assert!(rc.contains(&a), "RenderingControl lacks {a}");
        }
        let cm = action_names("ConnectionManager");
        for a in [
            "GetProtocolInfo",
            "GetCurrentConnectionIDs",
            "GetCurrentConnectionInfo",
        ] {
            assert!(cm.contains(&a), "ConnectionManager lacks {a}");
        }
    }

    #[test]
    fn each_service_evented_variable_is_last_change_or_a_cm_set() {
        let (_, av) = tables("AVTransport").unwrap();
        let ev: Vec<&str> =
            av.iter().filter(|v| v.evented).map(|v| v.name).collect();
        assert_eq!(ev, vec!["LastChange"]);
    }
}
