// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Tests for the system.backup plugin, against a stub that stands
//! in for the four owners: audio.delivery, audio.zones,
//! audio.playlist and audio.options.

use std::sync::Mutex;

use evo_plugin_sdk::contract::ShelfDispatchError;
use serde_json::json;

use super::*;

/// What the four owners hold.
#[derive(Clone)]
struct Owners {
    /// The `hw:N,M` outputs this device has.
    hardware: Vec<String>,
    /// The output devices `options.set_output_device` accepts.
    devices: Vec<String>,
    logical_outputs: Vec<Value>,
    zones: Vec<Value>,
    next_logical: u32,
    next_zone: u32,
    /// `(name, text)`, in list order.
    playlists: Vec<(String, String)>,
    record: Value,
    presets: Vec<Value>,
}

fn flat() -> Value {
    json!({ "freq_hz": 1000, "gain_db": 0.0, "q": 1.0 })
}

impl Owners {
    fn empty() -> Self {
        Self {
            hardware: vec!["hw:0,0".into(), "hw:1,0".into()],
            devices: vec![String::new(), "hw:0,0".into(), "hw:1,0".into()],
            logical_outputs: Vec::new(),
            zones: Vec::new(),
            next_logical: 1,
            next_zone: 1,
            playlists: Vec::new(),
            record: json!({
                "v": 1,
                "resampling": { "enabled": false },
                "mixer_type": "software",
                "mixer_device": "",
                "mixer_control": "",
                "dop": false,
                "output_device": "",
                "volume_normalization": "disabled",
                "startup_volume_percent": 30,
                "max_volume_percent": 100,
                "volume_curve": "linear",
                "exclusive_mode": false,
                "crossfade_seconds": 0,
                "gapless": true,
                "composition_mode": "passthrough",
                "eq_engaged": false,
                "eq_bands": (0..10).map(|_| flat()).collect::<Vec<_>>(),
                "eq_presets": [],
            }),
            presets: Vec::new(),
        }
    }

    /// A device someone has set up.
    fn set_up() -> Self {
        let mut o = Self::empty();
        o.logical_outputs = vec![
            json!({ "id": "logical:3", "name": "Kitchen", "output_id": "hw:0,0" }),
            json!({ "id": "logical:5", "name": "Porch", "output_id": "hw:1,0" }),
        ];
        o.next_logical = 6;
        let mut bands: Vec<Value> = (0..10).map(|_| flat()).collect();
        bands[2] = json!({ "freq_hz": 250, "gain_db": -3.5, "q": 0.7 });
        o.zones = vec![json!({
            "id": "zone:2",
            "name": "Downstairs",
            "logical_output_ids": ["logical:5", "logical:3"],
            "eq": { "engaged": true, "bands": bands },
        })];
        o.next_zone = 3;
        o.playlists = vec![
            (
                "Morning".into(),
                "#EXTM3U\n#PLAYLIST:Morning\nINTERNAL/a.flac\n".into(),
            ),
            (
                "Late".into(),
                "#EXTM3U\n#PLAYLIST:Late\nNAS/x/b.flac\n".into(),
            ),
        ];
        o.record["mixer_type"] = json!("hardware");
        o.record["mixer_device"] = json!("hw:1");
        o.record["mixer_control"] = json!("PCM");
        o.record["output_device"] = json!("hw:1,0");
        o.record["volume_normalization"] = json!("album");
        o.record["startup_volume_percent"] = json!(60);
        o.record["max_volume_percent"] = json!(80);
        o.record["crossfade_seconds"] = json!(4);
        o.record["composition_mode"] = json!("eq_only");
        o.record["eq_engaged"] = json!(true);
        o.record["eq_bands"][0] =
            json!({ "freq_hz": 60, "gain_db": 2.0, "q": 1.2 });
        o.presets = vec![json!({
            "name": "Warm",
            "bands": (0..10).map(|_| flat()).collect::<Vec<_>>(),
        })];
        o.record["eq_presets"] = json!(o.presets);
        o
    }
}

/// Stands in for the four owners: answers their verbs over the
/// state above, refuses as they refuse, and records every call.
struct Stub {
    owners: Mutex<Owners>,
    calls: Mutex<Vec<(String, String, Value)>>,
    /// A verb that fails as if its owner could not answer.
    broken: Option<&'static str>,
}

impl Stub {
    fn new(owners: Owners) -> Arc<Self> {
        Arc::new(Self {
            owners: Mutex::new(owners),
            calls: Mutex::new(Vec::new()),
            broken: None,
        })
    }

    fn owners(&self) -> Owners {
        self.owners.lock().unwrap().clone()
    }

    fn calls(&self) -> Vec<(String, String, Value)> {
        self.calls.lock().unwrap().clone()
    }

    fn verbs(&self) -> Vec<String> {
        self.calls().into_iter().map(|(_, v, _)| v).collect()
    }

    fn answer(&self, verb: &str, p: &Value) -> Result<Value, String> {
        if self.broken == Some(verb) {
            return Err("owner unavailable".into());
        }
        let mut o = self.owners.lock().unwrap();
        let s = |k: &str| p[k].as_str().unwrap_or_default().to_string();
        let ok = json!({ "v": 1, "status": "ok" });
        match verb {
            "delivery.list_logical_outputs" => {
                Ok(json!({ "v": 1, "logical_outputs": o.logical_outputs }))
            }
            "delivery.add_logical_output" => {
                let (name, output_id) = (s("name"), s("output_id"));
                if !o.hardware.contains(&output_id) {
                    return Err(format!("unknown output {output_id}"));
                }
                if o.logical_outputs.iter().any(|l| {
                    l["name"].as_str().unwrap().eq_ignore_ascii_case(&name)
                }) {
                    return Err(format!("name {name} is taken"));
                }
                let id = format!("logical:{}", o.next_logical);
                o.next_logical += 1;
                let row =
                    json!({ "id": id, "name": name, "output_id": output_id });
                o.logical_outputs.push(row.clone());
                Ok(json!({ "v": 1, "logical_output": row }))
            }
            "zone.list" => Ok(json!({ "v": 1, "zones": o.zones })),
            "zone.add" => {
                let name = s("name");
                if o.zones.iter().any(|z| {
                    z["name"].as_str().unwrap().eq_ignore_ascii_case(&name)
                }) {
                    return Err(format!("zone name {name} is taken"));
                }
                for id in p["logical_output_ids"].as_array().unwrap() {
                    if !o.logical_outputs.iter().any(|l| l["id"] == *id) {
                        return Err(format!("unknown logical output {id}"));
                    }
                }
                let id = format!("zone:{}", o.next_zone);
                o.next_zone += 1;
                let zone = json!({
                    "id": id,
                    "name": name,
                    "logical_output_ids": p["logical_output_ids"],
                    "eq": { "engaged": false, "bands": (0..10).map(|_| flat()).collect::<Vec<_>>() },
                });
                o.zones.push(zone.clone());
                Ok(json!({ "v": 1, "zone": zone }))
            }
            "zone.set_eq_band" | "zone.set_eq_engaged" => {
                let id = p["id"].clone();
                let zone = o
                    .zones
                    .iter_mut()
                    .find(|z| z["id"] == id)
                    .ok_or("unknown zone")?;
                if verb == "zone.set_eq_engaged" {
                    zone["eq"]["engaged"] = p["engaged"].clone();
                } else {
                    let i = p["index"].as_u64().unwrap() as usize;
                    zone["eq"]["bands"][i] = json!({
                        "freq_hz": p["freq_hz"], "gain_db": p["gain_db"], "q": p["q"],
                    });
                }
                Ok(json!({ "v": 1, "zone": zone.clone() }))
            }
            "playlist.list_playlists" => Ok(json!({
                "v": 1,
                "playlists": o.playlists.iter().map(|(n, t)| json!({
                    "name": n, "item_count": t.lines().filter(|l| !l.starts_with('#')).count(),
                    "modified_at_ms": null,
                })).collect::<Vec<_>>(),
            })),
            "playlist.export_file" => {
                let name = s("name");
                let text = o
                    .playlists
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, t)| t.clone())
                    .ok_or("no such playlist")?;
                Ok(json!({
                    "v": 1, "name": name, "format": p["format"],
                    "filename": format!("{name}.m3u"), "entries": 1, "content": text,
                }))
            }
            "playlist.import_file" => {
                let (name, text) = (s("name"), s("content"));
                if o.playlists.iter().any(|(n, _)| *n == name)
                    && p["overwrite"] != json!(true)
                {
                    return Err(format!("playlist {name} exists"));
                }
                // A row this device has no file for is skipped.
                let skipped: Vec<Value> = text
                    .lines()
                    .filter(|l| l.starts_with("GONE/"))
                    .map(|l| json!({ "entry": l, "reason": "not in the library" }))
                    .collect();
                o.playlists.push((name.clone(), text));
                Ok(json!({
                    "v": 1,
                    "status": if skipped.is_empty() { "ok" } else { "partial" },
                    "name": name, "skipped": skipped,
                }))
            }
            "options.get_settings" => Ok(o.record.clone()),
            "options.list_eq_presets" => {
                Ok(json!({ "v": 1, "presets": o.presets }))
            }
            "options.save_eq_preset" => {
                let name = p["name"].clone();
                o.presets.retain(|q| q["name"] != name);
                o.presets.push(json!({ "name": name, "bands": p["bands"] }));
                Ok(ok)
            }
            "options.set_eq_band" => {
                let i = p["index"].as_u64().unwrap() as usize;
                o.record["eq_bands"][i] = json!({
                    "freq_hz": p["freq_hz"], "gain_db": p["gain_db"], "q": p["q"],
                });
                Ok(ok)
            }
            "options.set_eq_engaged" => {
                o.record["eq_engaged"] = p["value"].clone();
                Ok(ok)
            }
            v if v.starts_with("options.set_") => {
                let (field, key) = SETTERS
                    .iter()
                    .find(|(_, verb, _)| *verb == v)
                    .map(|(f, _, k)| (*f, *k))
                    .ok_or_else(|| {
                        format!("{v} is not a setter this stub knows")
                    })?;
                let value = p[key].clone();
                if field == "output_device"
                    && !o.devices.contains(
                        &value.as_str().unwrap_or_default().to_string(),
                    )
                {
                    return Err(format!("no output device {value}"));
                }
                if field == "startup_volume_percent"
                    && value.as_u64() > o.record["max_volume_percent"].as_u64()
                {
                    return Err("startup above max".into());
                }
                o.record[field] = value;
                Ok(ok)
            }
            other => Err(format!("{other} is not a verb the backup may call")),
        }
    }
}

impl ShelfRequestDispatcher for Stub {
    fn dispatch<'a>(
        &'a self,
        shelf: &'a str,
        request_type: &'a str,
        payload: Vec<u8>,
        _instance_id: Option<&'a str>,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = Result<Vec<u8>, ShelfDispatchError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let p: Value =
                serde_json::from_slice(&payload).unwrap_or(Value::Null);
            self.calls.lock().unwrap().push((
                shelf.to_string(),
                request_type.to_string(),
                p.clone(),
            ));
            self.answer(request_type, &p)
                .map(|a| a.to_string().into_bytes())
                .map_err(|detail| ShelfDispatchError::Permanent { detail })
        })
    }
}

fn plugin_on(stub: &Arc<Stub>) -> BackupPlugin {
    let mut p = BackupPlugin::new();
    p.dispatcher = Some(Arc::clone(stub) as Arc<dyn ShelfRequestDispatcher>);
    p.loaded = true;
    p
}

fn req(verb: &str, payload: Value) -> Request {
    Request {
        request_type: verb.to_string(),
        payload: payload.to_string().into_bytes(),
        correlation_id: 1,
        deadline: None,
        instance_id: None,
        principal_scope: None,
        has_step_up: true,
    }
}

async fn export_from(owners: Owners) -> (Value, Arc<Stub>) {
    let stub = Stub::new(owners);
    let resp = plugin_on(&stub)
        .handle_request(&req(VERB_EXPORT, json!({ "v": 1 })))
        .await
        .unwrap();
    let answer: Value = serde_json::from_slice(&resp.payload).unwrap();
    (answer["backup"].clone(), stub)
}

async fn import_into(
    stub: &Arc<Stub>,
    backup: Value,
) -> Result<Vec<Value>, PluginError> {
    let resp = plugin_on(stub)
        .handle_request(&req(VERB_IMPORT, json!({ "v": 1, "backup": backup })))
        .await?;
    let answer: Value = serde_json::from_slice(&resp.payload).unwrap();
    assert_eq!(answer["v"], 1);
    Ok(answer["refused"].as_array().unwrap().clone())
}

/// The verbs a backup may call; anything else is a fault.
const READS: &[&str] = &[
    "delivery.list_logical_outputs",
    "zone.list",
    "playlist.list_playlists",
    "playlist.export_file",
    "options.get_settings",
    "options.list_eq_presets",
];

// ----- manifest / surface -----

#[test]
fn both_manifests_stock_system_backup_with_two_step_up_verbs() {
    let oop = Manifest::from_toml(include_str!("../manifest.oop.toml"))
        .expect("manifest.oop.toml parses");
    for m in [manifest(), oop] {
        assert_eq!(m.plugin.name, PLUGIN_NAME);
        assert_eq!(m.target.shelf, "system.backup");
        assert_eq!(m.target.shape, 1);
        let resp = m.capabilities.respondent.expect("respondent");
        assert_eq!(resp.request_types, vec![VERB_EXPORT, VERB_IMPORT]);
        for verb in [VERB_EXPORT, VERB_IMPORT] {
            match resp.verb_capabilities.get(verb) {
                Some(evo_plugin_sdk::manifest::VerbCapability::StepUp {
                    scope,
                }) => {
                    assert_eq!(scope, "system_admin")
                }
                other => {
                    panic!("{verb} must be step_up/system_admin, got {other:?}")
                }
            }
        }
    }
}

#[tokio::test]
async fn describe_names_the_manifest_verbs() {
    let d = BackupPlugin::new().describe().await;
    let declared = manifest().capabilities.respondent.unwrap().request_types;
    assert_eq!(d.runtime_capabilities.request_types, declared);
}

// ----- export_backup -----

/// The backup is the owners' own answers, read and nothing else.
#[tokio::test]
async fn export_answers_the_three_domains_from_their_owners() {
    let owners = Owners::set_up();
    let (backup, stub) = export_from(owners.clone()).await;
    assert_eq!(backup["v"], 1);
    assert_eq!(
        backup["config"]["logical_outputs"],
        json!(owners.logical_outputs)
    );
    assert_eq!(backup["config"]["zones"], json!(owners.zones));
    assert_eq!(
        backup["playlists"],
        json!(owners
            .playlists
            .iter()
            .map(|(n, t)| json!({ "name": n, "m3u": t }))
            .collect::<Vec<_>>())
    );
    assert_eq!(backup["settings"]["record"], owners.record);
    assert_eq!(backup["settings"]["eq_presets"], json!(owners.presets));
    for (shelf, verb, payload) in stub.calls() {
        assert!(READS.contains(&verb.as_str()), "export called {verb}");
        assert!([
            "audio.delivery",
            "audio.zones",
            "audio.playlist",
            "audio.options"
        ]
        .contains(&shelf.as_str()));
        if verb == "playlist.export_file" {
            assert_eq!(payload["format"], "m3u");
            assert_eq!(payload["absolute"], false, "library-relative rows");
        }
    }
}

/// If an owner cannot answer, there is no backup with that domain
/// left out: the export is refused.
#[tokio::test]
async fn an_owner_that_cannot_answer_refuses_the_export() {
    for verb in READS {
        let stub = Arc::new(Stub {
            owners: Mutex::new(Owners::set_up()),
            calls: Mutex::new(Vec::new()),
            broken: Some(verb),
        });
        let r = plugin_on(&stub)
            .handle_request(&req(VERB_EXPORT, json!({ "v": 1 })))
            .await;
        assert!(matches!(r, Err(PluginError::Transient(_))), "{verb}: {r:?}");
    }
}

// ----- import_backup -----

/// A backup taken on one device goes back onto an empty one:
/// config, then settings, then playlists, zone membership mapped
/// to the ids the device mints, and composition_mode not sent.
#[tokio::test]
async fn a_backup_goes_back_onto_an_empty_device() {
    let (backup, _) = export_from(Owners::set_up()).await;
    let stub = Stub::new(Owners::empty());
    let refused = import_into(&stub, backup).await.unwrap();
    assert!(refused.is_empty(), "{refused:?}");

    let after = stub.owners();
    let lo = |name: &str| {
        after
            .logical_outputs
            .iter()
            .find(|l| l["name"] == name)
            .unwrap()["id"]
            .clone()
    };
    assert_eq!(after.zones.len(), 1);
    let zone = &after.zones[0];
    assert_eq!(zone["name"], "Downstairs");
    assert_eq!(
        zone["logical_output_ids"],
        json!([lo("Porch"), lo("Kitchen")])
    );
    assert_ne!(lo("Kitchen"), "logical:3", "ids are minted, not copied");
    assert_eq!(zone["eq"]["engaged"], true);
    assert_eq!(
        zone["eq"]["bands"][2],
        json!({ "freq_hz": 250, "gain_db": -3.5, "q": 0.7 })
    );

    let set_up = Owners::set_up();
    for field in [
        "mixer_type",
        "mixer_device",
        "output_device",
        "volume_normalization",
        "startup_volume_percent",
        "max_volume_percent",
        "crossfade_seconds",
        "eq_engaged",
        "eq_bands",
    ] {
        assert_eq!(after.record[field], set_up.record[field], "{field}");
    }
    assert_eq!(
        after.record["composition_mode"], "passthrough",
        "not written"
    );
    assert_eq!(after.presets, set_up.presets);
    assert_eq!(after.playlists, set_up.playlists);

    let verbs = stub.verbs();
    let first = |v: &str| verbs.iter().position(|x| x == v).unwrap();
    let last =
        |pfx: &str| verbs.iter().rposition(|x| x.starts_with(pfx)).unwrap();
    assert!(
        last("zone.") < first("options.set_resampling"),
        "config before settings"
    );
    assert!(
        last("options.") < first("playlist.import_file"),
        "settings before playlists"
    );
    assert!(
        first("options.set_mixer_device") < first("options.set_mixer_type")
    );
    assert!(
        first("options.set_max_volume") < first("options.set_startup_volume")
    );
    assert!(!verbs.iter().any(|v| v == "options.set_composition_mode"));
}

/// A device that lacks hardware the backup names refuses those
/// items; they are reported, nothing is put in their place, and
/// everything else still goes back.
#[tokio::test]
async fn a_missing_device_is_a_refusal_not_a_guess() {
    let (backup, _) = export_from(Owners::set_up()).await;
    let mut lacking = Owners::empty();
    lacking.hardware = vec!["hw:0,0".into()];
    lacking.devices = vec![String::new(), "hw:0,0".into()];
    let stub = Stub::new(lacking);
    let refused = import_into(&stub, backup).await.unwrap();
    let items: Vec<&str> = refused
        .iter()
        .map(|r| r["item"].as_str().unwrap())
        .collect();
    assert_eq!(
        items,
        ["logical output Porch", "zone Downstairs", "output_device"],
        "{refused:?}"
    );
    assert_eq!(
        refused[1]["reason"],
        "its logical output logical:5 did not go back"
    );
    let after = stub.owners();
    assert_eq!(after.logical_outputs.len(), 1, "Kitchen went back");
    assert!(after.zones.is_empty(), "no zone with a shrunken membership");
    assert_eq!(after.record["output_device"], "", "not replaced by a guess");
    assert_eq!(after.record["mixer_type"], "hardware", "the rest went back");
    assert_eq!(after.playlists.len(), 2);
}

/// Pin: import removes nothing the device already has. A name it
/// already holds is refused and reported; what it holds is left as
/// it was; no verb that removes, deletes, clears or resets is sent;
/// and every playlist import says overwrite false.
#[tokio::test]
async fn import_removes_nothing_the_device_has() {
    let (backup, _) = export_from(Owners::set_up()).await;
    let mut holding = Owners::set_up();
    holding.playlists[0].1 =
        "#EXTM3U\n#PLAYLIST:Morning\nINTERNAL/mine.flac\n".into();
    let before = holding.clone();
    let stub = Stub::new(holding);
    let refused = import_into(&stub, backup).await.unwrap();
    let items: Vec<&str> = refused
        .iter()
        .map(|r| r["item"].as_str().unwrap())
        .collect();
    for held in [
        "logical output Kitchen",
        "logical output Porch",
        "zone Downstairs",
        "playlist Morning",
        "playlist Late",
    ] {
        assert!(items.contains(&held), "{held} not reported: {items:?}");
    }
    let after = stub.owners();
    assert_eq!(after.logical_outputs, before.logical_outputs);
    assert_eq!(after.zones, before.zones);
    assert_eq!(
        after.playlists, before.playlists,
        "the device's own Morning stays"
    );
    for (_, verb, payload) in stub.calls() {
        let words: Vec<&str> = verb
            .rsplit('.')
            .next()
            .unwrap_or_default()
            .split('_')
            .collect();
        for word in ["remove", "delete", "clear", "reset", "restore", "forget"]
        {
            assert!(!words.contains(&word), "import sent {verb}");
        }
        if verb == "playlist.import_file" {
            assert_eq!(payload["overwrite"], false);
        }
    }
}

/// Rows the playlist owner skips are reported per row.
#[tokio::test]
async fn skipped_playlist_rows_are_reported() {
    let mut owners = Owners::set_up();
    owners.playlists = vec![(
        "Old".into(),
        "#EXTM3U\nGONE/x.flac\nINTERNAL/y.flac\n".into(),
    )];
    let (backup, _) = export_from(owners).await;
    let stub = Stub::new(Owners::empty());
    let refused = import_into(&stub, backup).await.unwrap();
    let rows: Vec<&Value> = refused
        .iter()
        .filter(|r| r["domain"] == "playlists")
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["item"], "playlist Old row GONE/x.flac");
    assert_eq!(rows[0]["reason"], "not in the library");
}

/// A backup whose own version is not 1, or that does not parse, is
/// refused before anything is sent to an owner.
#[tokio::test]
async fn a_foreign_or_broken_backup_is_refused_before_any_write() {
    let (backup, _) = export_from(Owners::set_up()).await;
    let mut v2 = backup.clone();
    v2["v"] = json!(2);
    let mut broken = backup.clone();
    broken["config"]["zones"] = json!("not a list");
    let mut unversioned = backup.clone();
    unversioned.as_object_mut().unwrap().remove("v");
    for bad in [v2, broken, unversioned] {
        let stub = Stub::new(Owners::empty());
        let r = import_into(&stub, bad).await;
        assert!(matches!(r, Err(PluginError::Permanent(_))), "{r:?}");
        assert!(stub.calls().is_empty(), "an owner was called");
    }
}

#[tokio::test]
async fn without_the_dispatcher_both_verbs_refuse() {
    let mut p = BackupPlugin::new();
    p.loaded = true;
    for r in [
        req(VERB_EXPORT, json!({ "v": 1 })),
        req(VERB_IMPORT, json!({ "v": 1, "backup": { "v": 1 } })),
    ] {
        match p.handle_request(&r).await {
            Err(PluginError::Permanent(msg)) => {
                assert!(msg.contains("dispatcher"), "{msg}")
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }
}

// ----- pin: no music file, no share secret, no share add -----

/// Pin: a backup holds no music file and no share secret, and
/// nothing reaches the shares shelf. The export reads only the four
/// owners' read verbs, and its playlists are the owner's text. A
/// hand-edited backup carrying shares, or a secret, is imported
/// without either reaching an owner. No source this crate ships
/// names the shares shelf, a password, a credential, or the file
/// system.
#[tokio::test]
async fn no_music_file_no_share_secret_and_no_share_add() {
    let (backup, stub) = export_from(Owners::set_up()).await;
    let text = backup.to_string();
    for word in ["password", "credential", "secret", "share"] {
        assert!(
            !text.to_lowercase().contains(word),
            "the backup names {word:?}"
        );
    }
    for p in backup["playlists"].as_array().unwrap() {
        let keys: Vec<&String> = p.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            ["m3u", "name"],
            "a playlist is its name and its text"
        );
    }
    assert!(stub
        .calls()
        .iter()
        .all(|(_, v, _)| READS.contains(&v.as_str())));

    let mut doctored = backup.clone();
    doctored["shares"] = json!([{
        "alias": "nas", "fstype": "cifs", "host": "10.0.0.2", "path": "music",
        "credentials": { "kind": "user_password", "username": "u", "password": "p" },
    }]);
    doctored["config"]["password"] = json!("p");
    let stub = Stub::new(Owners::empty());
    import_into(&stub, doctored).await.unwrap();
    for (shelf, verb, payload) in stub.calls() {
        assert!(!shelf.starts_with("networking"), "import reached {shelf}");
        assert!(!verb.starts_with("network."), "import sent {verb}");
        assert!(
            !payload.to_string().contains("\"p\""),
            "the secret reached {verb}"
        );
    }

    for (file, source) in [
        ("src/lib.rs", include_str!("lib.rs")),
        (
            "src/bin/system-backup-wire.rs",
            include_str!("bin/system-backup-wire.rs"),
        ),
    ] {
        for token in [
            "network.share",
            "networking",
            "password",
            "credential",
            "std::fs",
            "tokio::fs",
            "File::",
            "read_dir",
            "/var/lib/evo",
            "remove_logical_output",
            "zone.remove",
            "scene.",
            "delete_",
            "reset_to_defaults",
            "restore_last_known_good",
            "\"overwrite\": true",
        ] {
            assert!(!source.contains(token), "{file} names {token:?}");
        }
    }
}
