// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Tests for the system.backup plugin, against a stub that stands
//! in for the six owners: audio.delivery, audio.zones,
//! audio.playlist, audio.options, audio.favourites and audio.alarm.

use std::sync::Mutex;

use evo_plugin_sdk::contract::ShelfDispatchError;
use serde_json::json;

use super::*;

/// What the four owners hold.
#[derive(Clone)]
struct Owners {
    /// The `hw:N,M` outputs this device has.
    hardware: Vec<String>,
    logical_outputs: Vec<Value>,
    zones: Vec<Value>,
    next_logical: u32,
    next_zone: u32,
    /// `(name, text)`, in list order.
    playlists: Vec<(String, String)>,
    record: Value,
    presets: Vec<Value>,
    /// The alarm, as `alarm.get` answers it.
    alarm: Option<Value>,
}

fn flat() -> Value {
    json!({ "freq_hz": 1000, "gain_db": 0.0, "q": 1.0 })
}

impl Owners {
    fn empty() -> Self {
        Self {
            hardware: vec!["hw:0,0".into(), "hw:1,0".into()],
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
            alarm: None,
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
            (
                "Quiet".into(),
                "#EXTM3U\n#PLAYLIST:Quiet\n".into(),
            ),
            (
                "__favourites__".into(),
                "#EXTM3U\n#PLAYLIST:__favourites__\nINTERNAL/a.flac\nNAS/x/b.flac\n"
                    .into(),
            ),
        ];
        o.alarm = Some(json!({
            "weekdays": ["mon", "fri"],
            "time": "06:30",
            "playlist_name": "Morning",
        }));
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
            "zone.set_eq_band"
            | "zone.set_eq_engaged"
            | "zone.remove_eq_band" => {
                let id = p["id"].clone();
                let zone = o
                    .zones
                    .iter_mut()
                    .find(|z| z["id"] == id)
                    .ok_or("unknown zone")?;
                if verb == "zone.set_eq_engaged" {
                    zone["eq"]["engaged"] = p["engaged"].clone();
                } else if verb == "zone.remove_eq_band" {
                    let i = p["index"].as_u64().unwrap() as usize;
                    zone["eq"]["bands"].as_array_mut().unwrap().remove(i);
                } else {
                    let i = p["index"].as_u64().unwrap() as usize;
                    let band = json!({
                        "freq_hz": p["freq_hz"], "gain_db": p["gain_db"], "q": p["q"],
                    });
                    let bands = zone["eq"]["bands"].as_array_mut().unwrap();
                    if i == bands.len() {
                        bands.push(band);
                    } else {
                        bands[i] = band;
                    }
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
            "delivery.list_outputs" => {
                let rows: Vec<Value> = o
                    .hardware
                    .iter()
                    .map(|id| {
                        let (card, dev) = id
                            .trim_start_matches("hw:")
                            .split_once(',')
                            .unwrap();
                        json!({
                            "card_idx": card.parse::<u32>().unwrap(),
                            "device_idx": dev.parse::<u32>().unwrap(),
                            "card_name": format!("Card{card}"),
                            "card_id": format!("card{card}"),
                            "alsa_id": id,
                        })
                    })
                    .collect();
                Ok(json!({ "v": 1, "outputs": rows }))
            }
            "playlist.create_playlist" => {
                let name = s("name");
                if o.playlists.iter().any(|(n, _)| *n == name) {
                    return Err(format!("playlist {name} exists"));
                }
                o.playlists.push((
                    name.clone(),
                    format!("#EXTM3U\n#PLAYLIST:{name}\n"),
                ));
                Ok(ok)
            }
            "favourites.add_favourite" => {
                let uri = s("uri");
                if uri.starts_with("GONE/") {
                    return Err(format!("{uri} is not in the library"));
                }
                let at =
                    o.playlists.iter().position(|(n, _)| n == "__favourites__");
                let at = at.unwrap_or_else(|| {
                    o.playlists.push((
                        "__favourites__".into(),
                        "#EXTM3U\n#PLAYLIST:__favourites__\n".into(),
                    ));
                    o.playlists.len() - 1
                });
                if !o.playlists[at].1.lines().any(|l| l == uri) {
                    o.playlists[at].1.push_str(&format!("{uri}\n"));
                }
                Ok(ok)
            }
            "alarm.get" => Ok(json!({ "v": 1, "alarm": o.alarm })),
            "alarm.set" => {
                o.alarm = Some(json!({
                    "weekdays": p["weekdays"], "time": p["time"],
                    "playlist_name": p["playlist_name"],
                }));
                Ok(json!({ "v": 1, "alarm": o.alarm }))
            }
            "playlist.import_file" => {
                let (name, text) = (s("name"), s("content"));
                if name == "__favourites__" {
                    return Err(
                        "the system-managed favourites playlist; use the favourites shelf"
                            .into(),
                    );
                }
                if !text.lines().any(|l| !l.is_empty() && !l.starts_with('#')) {
                    return Err(
                        "nothing to import: the file holds no rows".into()
                    );
                }
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
                // As the real owner: `output_device` is taken whatever it
                // names.
                let value = p[key].clone();
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
    "alarm.get",
];

// ----- manifest / surface -----

#[test]
fn both_manifests_stock_system_backup_with_two_system_admin_writes() {
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
                Some(evo_plugin_sdk::manifest::VerbCapability::Write {
                    scope,
                }) => {
                    assert_eq!(scope, "system_admin")
                }
                other => {
                    panic!("{verb} must be write/system_admin, got {other:?}")
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
            "audio.options",
            "audio.alarm"
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

/// A zone whose EQ was grown past ten bands, or cut below ten, goes
/// back with the band count it had, each band in its place.
#[tokio::test]
async fn a_zone_eq_goes_back_at_the_band_count_it_had() {
    for count in [31usize, 20, 10, 6, 1] {
        let mut owners = Owners::set_up();
        let bands: Vec<Value> = (0..count)
            .map(|i| json!({ "freq_hz": 100 + i as u32, "gain_db": (i as f64) / 10.0, "q": 1.25 }))
            .collect();
        owners.zones[0]["eq"]["bands"] = json!(bands);
        let (backup, _) = export_from(owners).await;
        let stub = Stub::new(Owners::empty());
        let refused = import_into(&stub, backup).await.unwrap();
        assert!(refused.is_empty(), "{count}: {refused:?}");
        let after = stub.owners();
        let got = after.zones[0]["eq"]["bands"].as_array().unwrap();
        assert_eq!(got.len(), count, "{count} bands in, {} out", got.len());
        for (i, b) in got.iter().enumerate() {
            assert_eq!(b["freq_hz"], 100 + i as u32, "{count}: band {i}");
            assert_eq!(b["gain_db"], (i as f64) / 10.0, "{count}: band {i}");
        }
    }
}

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
    assert_eq!(after.alarm, set_up.alarm, "the alarm went back");

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
        last("playlist.") < first("alarm.set"),
        "the alarm goes back after the playlists it names"
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
    assert_eq!(after.playlists.len(), 4, "every playlist went back");
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

// ----- favourites, empty playlists, the alarm, output_device -----

/// The favourites list is exported as the playlist `__favourites__`
/// and goes back one row at a time through the favourites shelf, in
/// order. The playlist owner is never asked to import it.
#[tokio::test]
async fn favourites_go_back_through_the_favourites_shelf() {
    let (backup, _) = export_from(Owners::set_up()).await;
    let stub = Stub::new(Owners::empty());
    let refused = import_into(&stub, backup).await.unwrap();
    assert!(refused.is_empty(), "{refused:?}");
    let held = stub
        .owners()
        .playlists
        .into_iter()
        .find(|(n, _)| n == "__favourites__")
        .expect("favourites went back")
        .1;
    assert_eq!(
        held,
        "#EXTM3U\n#PLAYLIST:__favourites__\nINTERNAL/a.flac\nNAS/x/b.flac\n"
    );
    let adds: Vec<Value> = stub
        .calls()
        .into_iter()
        .filter(|(s, v, _)| {
            s == "audio.favourites" && v == "favourites.add_favourite"
        })
        .map(|(_, _, p)| p["uri"].clone())
        .collect();
    assert_eq!(adds, [json!("INTERNAL/a.flac"), json!("NAS/x/b.flac")]);
    for (_, verb, payload) in stub.calls() {
        if verb == "playlist.import_file" {
            assert_ne!(payload["name"], "__favourites__");
        }
    }
}

/// A favourite the favourites owner refuses is reported by row, and the
/// rest still go back. One the device already holds is not a refusal.
#[tokio::test]
async fn a_favourite_the_owner_refuses_is_reported_by_row() {
    let mut owners = Owners::set_up();
    owners.playlists.retain(|(n, _)| n != "__favourites__");
    owners.playlists.push((
        "__favourites__".into(),
        "#EXTM3U\n#PLAYLIST:__favourites__\nGONE/x.flac\nINTERNAL/a.flac\n"
            .into(),
    ));
    let (backup, _) = export_from(owners).await;
    let mut holding = Owners::empty();
    holding.playlists.push((
        "__favourites__".into(),
        "#EXTM3U\n#PLAYLIST:__favourites__\nINTERNAL/a.flac\n".into(),
    ));
    let stub = Stub::new(holding);
    let refused = import_into(&stub, backup).await.unwrap();
    let rows: Vec<&Value> = refused
        .iter()
        .filter(|r| r["item"].as_str().unwrap().starts_with("favourite"))
        .collect();
    assert_eq!(rows.len(), 1, "{refused:?}");
    assert_eq!(rows[0]["item"], "favourite GONE/x.flac");
    assert_eq!(rows[0]["domain"], "playlists");
}

/// A playlist with no rows goes back as an empty playlist, made, not
/// imported. An empty list of favourites asks nothing of anyone.
#[tokio::test]
async fn an_empty_playlist_goes_back_empty() {
    let mut owners = Owners::set_up();
    owners.playlists.retain(|(n, _)| n != "__favourites__");
    owners.playlists.push((
        "__favourites__".into(),
        "#EXTM3U\n#PLAYLIST:__favourites__\n".into(),
    ));
    let (backup, _) = export_from(owners).await;
    let stub = Stub::new(Owners::empty());
    let refused = import_into(&stub, backup).await.unwrap();
    assert!(refused.is_empty(), "{refused:?}");
    let after = stub.owners();
    let quiet = after.playlists.iter().find(|(n, _)| n == "Quiet").unwrap();
    assert_eq!(quiet.1, "#EXTM3U\n#PLAYLIST:Quiet\n");
    assert!(
        !after.playlists.iter().any(|(n, _)| n == "__favourites__"),
        "no favourites to add, none made"
    );
    let created: Vec<Value> = stub
        .calls()
        .into_iter()
        .filter(|(_, v, _)| v == "playlist.create_playlist")
        .map(|(_, _, p)| p["name"].clone())
        .collect();
    assert_eq!(created, [json!("Quiet")]);
    for (_, verb, payload) in stub.calls() {
        if verb == "playlist.import_file" {
            assert_ne!(payload["name"], "Quiet");
        }
        assert_ne!(verb, "favourites.add_favourite");
    }
}

/// A name the device already holds is refused for an empty playlist as
/// for any other.
#[tokio::test]
async fn an_empty_playlist_the_device_holds_is_reported() {
    let (backup, _) = export_from(Owners::set_up()).await;
    let mut holding = Owners::empty();
    holding
        .playlists
        .push(("Quiet".into(), "#EXTM3U\n#PLAYLIST:Quiet\n".into()));
    let stub = Stub::new(holding);
    let refused = import_into(&stub, backup).await.unwrap();
    let items: Vec<&str> = refused
        .iter()
        .map(|r| r["item"].as_str().unwrap())
        .collect();
    assert_eq!(items, ["playlist Quiet"], "{refused:?}");
}

/// The alarm is read on export, `null` when none is set, and set on
/// import with the fields it was read with.
#[tokio::test]
async fn the_alarm_is_exported_and_set_on_import() {
    let (backup, _) = export_from(Owners::set_up()).await;
    assert_eq!(
        backup["settings"]["alarm"],
        json!({ "weekdays": ["mon", "fri"], "time": "06:30", "playlist_name": "Morning" })
    );
    let (none, _) = export_from(Owners::empty()).await;
    assert_eq!(none["settings"]["alarm"], Value::Null);

    let stub = Stub::new(Owners::empty());
    import_into(&stub, backup).await.unwrap();
    let sets: Vec<Value> = stub
        .calls()
        .into_iter()
        .filter(|(s, v, _)| s == "audio.alarm" && v == "alarm.set")
        .map(|(_, _, p)| p)
        .collect();
    assert_eq!(
        sets,
        [json!({
            "v": 1, "weekdays": ["mon", "fri"], "time": "06:30",
            "playlist_name": "Morning",
        })]
    );
    let stub = Stub::new(Owners::empty());
    import_into(&stub, none).await.unwrap();
    assert!(
        !stub.verbs().iter().any(|v| v == "alarm.set"),
        "no alarm, none set"
    );
}

/// A backup taken before the alarm was part of one has no `alarm`
/// key. It goes back as it always did and sets no alarm.
#[tokio::test]
async fn a_backup_without_an_alarm_key_still_goes_back() {
    let (mut backup, _) = export_from(Owners::set_up()).await;
    backup["settings"].as_object_mut().unwrap().remove("alarm");
    let mut holding = Owners::empty();
    holding.alarm = Some(
        json!({ "weekdays": ["sun"], "time": "09:00", "playlist_name": "Mine" }),
    );
    let stub = Stub::new(holding);
    let refused = import_into(&stub, backup).await.unwrap();
    assert!(refused.is_empty(), "{refused:?}");
    assert!(!stub.verbs().iter().any(|v| v == "alarm.set"));
    assert_eq!(stub.owners().alarm.unwrap()["playlist_name"], "Mine");
}

/// An alarm its owner refuses is reported and nothing else is lost.
#[tokio::test]
async fn an_alarm_the_owner_refuses_is_reported() {
    let stub = Arc::new(Stub {
        owners: Mutex::new(Owners::empty()),
        calls: Mutex::new(Vec::new()),
        broken: Some("alarm.set"),
    });
    let (backup, _) = export_from(Owners::set_up()).await;
    let refused = import_into(&stub, backup).await.unwrap();
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(refused[0]["domain"], "settings");
    assert_eq!(refused[0]["item"], "alarm");
}

/// `output_device` goes back only if the device has that output, named
/// as the delivery plugin reads a descriptor: `hw:N,M`, `hw:CARD=id,DEV=M`
/// or a bare card. The empty default always goes back.
#[tokio::test]
async fn output_device_goes_back_only_if_the_device_has_it() {
    let (base, _) = export_from(Owners::set_up()).await;
    let with = |device: &str| {
        let mut b = base.clone();
        b["settings"]["record"]["output_device"] = json!(device);
        b
    };
    for (device, goes_back) in [
        ("hw:1,0", true),
        ("hw:CARD=card1,DEV=0", true),
        ("hw:CARD=Card0", true),
        ("hw:1", true),
        ("card1", true),
        ("", true),
        ("hw:9,9", false),
        ("hw:1,3", false),
        ("hw:CARD=Gone,DEV=0", false),
        ("nonsense", false),
    ] {
        let stub = Stub::new(Owners::empty());
        let refused = import_into(&stub, with(device)).await.unwrap();
        let named = refused.iter().any(|r| r["item"] == "output_device");
        assert_eq!(named, !goes_back, "{device:?}: {refused:?}");
        let sent = stub
            .verbs()
            .iter()
            .any(|v| v == "options.set_output_device");
        assert_eq!(sent, goes_back, "{device:?}: sent={sent}");
        let stored = stub.owners().record["output_device"].clone();
        if goes_back {
            assert_eq!(stored, json!(device), "{device:?}");
        } else {
            assert_eq!(stored, json!(""), "the device's own value stays");
        }
    }
}

/// Outputs the device cannot list: the value is not applied, and the
/// reason says why.
#[tokio::test]
async fn an_unreadable_output_list_leaves_output_device_alone() {
    let stub = Arc::new(Stub {
        owners: Mutex::new(Owners::empty()),
        calls: Mutex::new(Vec::new()),
        broken: Some("delivery.list_outputs"),
    });
    let (backup, _) = export_from(Owners::set_up()).await;
    let refused = import_into(&stub, backup).await.unwrap();
    let row = refused
        .iter()
        .find(|r| r["item"] == "output_device")
        .expect("reported");
    assert!(row["reason"].as_str().unwrap().contains("could not check"));
    assert_eq!(stub.owners().record["output_device"], "");
}

// ----- the 16 MiB bound -----

/// The backup's size as the bound measures it: the object written
/// as compact UTF-8 JSON.
fn compact_len(backup: &Value) -> usize {
    serde_json::to_vec(backup).unwrap().len()
}

/// Owners whose export is exactly `size` bytes as compact JSON:
/// the set-up owners plus one playlist padded with ASCII.
async fn owners_exporting(size: usize) -> Owners {
    let mut owners = Owners::set_up();
    owners.playlists.push(("Pad".to_string(), String::new()));
    let (backup, _) = export_from(owners.clone()).await;
    let pad = size - compact_len(&backup);
    owners.playlists.last_mut().unwrap().1 = "a".repeat(pad);
    owners
}

/// Pin: export over the bound is Permanent and answers no backup.
/// At exactly 16 MiB of compact JSON the backup is answered whole;
/// one byte more and the export is refused as Permanent, with no
/// backup and nothing cut short. The measure is the backup object,
/// not the response around it.
#[tokio::test]
async fn export_over_the_bound_is_permanent_and_answers_no_backup() {
    assert_eq!(MAX_BACKUP_BYTES, 16 * 1_048_576);

    let at = owners_exporting(MAX_BACKUP_BYTES).await;
    let (backup, _) = export_from(at.clone()).await;
    assert_eq!(compact_len(&backup), MAX_BACKUP_BYTES);
    assert_eq!(
        backup["playlists"].as_array().unwrap().last().unwrap()["m3u"],
        json!(at.playlists.last().unwrap().1),
        "the backup at the bound was cut short"
    );

    let over = owners_exporting(MAX_BACKUP_BYTES + 1).await;
    let stub = Stub::new(over);
    match plugin_on(&stub)
        .handle_request(&req(VERB_EXPORT, json!({ "v": 1 })))
        .await
    {
        Err(PluginError::Permanent(msg)) => {
            assert!(msg.contains("over the bound"), "{msg}");
            assert!(msg.contains(&(MAX_BACKUP_BYTES + 1).to_string()), "{msg}");
        }
        other => panic!("expected Permanent and no backup, got {other:?}"),
    }
}

/// Pin: import over the bound is Permanent and calls no owner. One
/// byte over 16 MiB of compact JSON is refused before the version is
/// checked: a backup over the bound with a wrong `v`, or one that
/// would not parse, is too large, not an unsupported version. At
/// exactly 16 MiB the backup goes back.
#[tokio::test]
async fn import_over_the_bound_is_permanent_and_calls_no_owner() {
    let (base, _) = export_from(Owners::set_up()).await;
    let padded = |size: usize, edit: &dyn Fn(&mut Value)| {
        let mut b = base.clone();
        edit(&mut b);
        b["playlists"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "name": "Pad", "m3u": "" }));
        let pad = size - compact_len(&b);
        b["playlists"].as_array_mut().unwrap().last_mut().unwrap()["m3u"] =
            json!("a".repeat(pad));
        assert_eq!(compact_len(&b), size);
        b
    };

    let over = [
        padded(MAX_BACKUP_BYTES + 1, &|_| {}),
        padded(MAX_BACKUP_BYTES + 1, &|b| b["v"] = json!(2)),
        padded(MAX_BACKUP_BYTES + 1, &|b| {
            b["config"]["zones"] = json!("not a list")
        }),
    ];
    for backup in over {
        let stub = Stub::new(Owners::empty());
        match import_into(&stub, backup).await {
            Err(PluginError::Permanent(msg)) => {
                assert!(msg.contains("over the bound"), "{msg}");
                assert!(!msg.contains("unsupported"), "{msg}");
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
        assert!(stub.calls().is_empty(), "an owner was called");
    }

    let stub = Stub::new(Owners::empty());
    import_into(&stub, padded(MAX_BACKUP_BYTES, &|_| {}))
        .await
        .expect("a backup at the bound goes back");
    assert!(!stub.calls().is_empty());
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
            "scene.",
            "delete_",
            "reset_to_defaults",
            "restore_last_known_good",
            "\"overwrite\": true",
        ] {
            assert!(!source.contains(token), "{file} names {token:?}");
        }
        // A restore never removes a zone. The one verb of that name it
        // may use is the one that trims a restored zone's extra EQ
        // bands, so every mention of `zone.remove` must be that verb.
        assert_eq!(
            source.matches("zone.remove").count(),
            source.matches("zone.remove_eq_band").count(),
            "{file} names a zone removal other than the band trim"
        );
    }
}
