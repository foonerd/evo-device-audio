// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! # org-evoframework-system-backup
//!
//! Stocks the `system.backup` shelf with two operator gestures,
//! both gated at `write:system_admin`.
//!
//! - `export_backup { v: 1 }` answers `{ v: 1, backup: { v: 1,
//!   config, playlists, settings } }`:
//!   - `config` is `{ logical_outputs, zones, scenes }`, as
//!     `delivery.list_logical_outputs` and `zone.list` answer the
//!     first two, and `scenes` the scenes `scene.list` names, each
//!     read whole with `scene.get`;
//!   - `playlists` is `[{ name, m3u }]`, every stored playlist as
//!     `playlist.export_file` answers its text, format `m3u`,
//!     library-relative rows;
//!   - `settings` is `{ record, eq_presets, locale, alarm }`, as
//!     `options.get_settings` and `options.list_eq_presets` answer
//!     the first two, `locale` is `{ language, timezone }` as
//!     `language.get` and `timezone.get` answer them, and `alarm` the
//!     alarm `alarm.get` answers, or `null` when none is set.
//!
//!   An owner that cannot answer refuses the export; a backup is
//!   never answered with a domain left out.
//! - `import_backup { v: 1, backup }` refuses a backup whose own `v`
//!   is not 1, or that does not parse, before any write. It then
//!   puts config back, then settings, then playlists, then the
//!   alarm, through the owners' add and set verbs, and answers `{
//!   v: 1, refused: [{ domain, item, reason }] }`. Import adds and
//!   sets; it takes nothing away. An item its owner refuses — a
//!   name the device already holds, hardware the device lacks — is
//!   reported and skipped; nothing is put in its place, and the
//!   other items still go back.
//!   - The favourites list is exported as the playlist
//!     `__favourites__` and goes back one row at a time through
//!     `favourites.add_favourite`, since the playlist owner refuses
//!     that name; a favourite already held is a no-op.
//!   - A playlist with no rows goes back as an empty playlist
//!     through `playlist.create_playlist`.
//!   - Scenes go back after the zones, through `scene.put`, which
//!     stores a scene as given and never over one the device holds.
//!     A scene's logical outputs are mapped to the ids the device
//!     minted; a scene whose logical output did not go back is
//!     reported and skipped. A restore never recalls, saves or
//!     removes a scene, so the live zones are those the zones
//!     import put back and nothing else. A backup without `scenes`
//!     puts back none.
//!   - `output_device` is put back only if the device has that
//!     output, checked against `delivery.list_outputs` the way the
//!     delivery plugin resolves the descriptor; the empty string,
//!     the default, always goes back. An output the device lacks,
//!     or a list it cannot read, is reported and the device's own
//!     `output_device` stays.
//!   - The mixer (`mixer_type`, `mixer_device`, `mixer_control`) goes
//!     back whole or not at all. A hardware mixer is put back only if
//!     the device has that control on that card, read once through
//!     `delivery.list_mixers` before any of the three is sent; one
//!     the device lacks, or cannot be checked, or a hardware mixer that
//!     names no device or control, is reported as the single item
//!     `mixer`, with a reason that names it, and the device's own mixer
//!     stays. A software mixer that names nothing always goes back.
//!   - The language and the timezone go back after the settings and
//!     before the playlists and the alarm, so an alarm set after them
//!     is scheduled on the zone in force. A value the device already
//!     holds is not sent. The language goes through `language.set`;
//!     one the glass has no catalogue for does not apply and is
//!     reported as `language`. The zone is attempted through
//!     `timezone.set`, which is a write at `system_admin`; a call
//!     the plugin dispatches does not carry that grant, so the owner
//!     refuses it, and the zone is reported as `timezone` with the
//!     owner's reason and the zone from the backup. A backup without
//!     `locale` sets neither.
//!   - The alarm goes back through `alarm.set` once the playlists
//!     are in, and replaces the one the device holds, as a setting
//!     does. A backup with no `alarm` key, taken before the alarm
//!     was part of a backup, sets none.
//! - A backup is at most [`MAX_BACKUP_BYTES`], 16 MiB, measured as
//!   the backup object written as compact UTF-8 JSON. Export refuses
//!   a backup over the bound as Permanent and answers none; it never
//!   truncates. Import refuses one as Permanent before its version is
//!   checked and before any owner is called.
//!
//! Every call goes to the owning plugin through the framework's
//! shelf dispatcher. The plugin reads no file and no other
//! plugin's secrets, so a backup holds no music file and no
//! secret. It touches nothing on the shares shelf.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::manual_async_fn)]

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities, ShelfRequestDispatcher,
};
use evo_plugin_sdk::Manifest;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// Embedded manifest source.
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");

/// Plugin name (reverse-DNS); same as manifest and tests.
pub const PLUGIN_NAME: &str = "org.evoframework.system.backup";

/// Read the three domains into one backup.
pub const VERB_EXPORT: &str = "export_backup";

/// Put a backup back.
pub const VERB_IMPORT: &str = "import_backup";

/// Wire payload version, and the backup's own version.
const PAYLOAD_VERSION: u32 = 1;

/// The largest backup: 16 MiB of the backup object
/// `{ v, config, playlists, settings }` written as compact UTF-8
/// JSON, not the request or response around it.
pub const MAX_BACKUP_BYTES: usize = 16 * 1_048_576;

const DELIVERY: &str = "audio.delivery";
const ZONES: &str = "audio.zones";
const PLAYLIST: &str = "audio.playlist";
const OPTIONS: &str = "audio.options";
const ALARM: &str = "audio.alarm";
const LOCALE: &str = "system.locale";
const FAVOURITES: &str = "audio.favourites";

/// The stored playlist the favourites list is kept in. The playlist
/// owner refuses it on import; its rows go back through the
/// favourites shelf.
const FAVOURITES_PLAYLIST: &str = "__favourites__";

/// The settings record's fields that go back, in the order they go,
/// each with its setter and the key that setter takes the value
/// under. `mixer_device` and `mixer_control` precede `mixer_type`,
/// which needs them for hardware; `max_volume_percent` precedes
/// `startup_volume_percent`, which may not exceed it. `eq_bands`
/// and `eq_engaged` follow. `composition_mode` is not here: the
/// composition plugin writes it.
const SETTERS: &[(&str, &str, &str)] = &[
    ("resampling", "options.set_resampling", "policy"),
    ("mixer_device", "options.set_mixer_device", "value"),
    ("mixer_control", "options.set_mixer_control", "value"),
    ("mixer_type", "options.set_mixer_type", "value"),
    ("dop", "options.set_dop", "value"),
    ("output_device", "options.set_output_device", "value"),
    (
        "volume_normalization",
        "options.set_volume_normalization",
        "mode",
    ),
    ("max_volume_percent", "options.set_max_volume", "value"),
    (
        "startup_volume_percent",
        "options.set_startup_volume",
        "value",
    ),
    ("volume_curve", "options.set_volume_curve", "value"),
    ("exclusive_mode", "options.set_exclusive_mode", "value"),
    (
        "crossfade_seconds",
        "options.set_crossfade_seconds",
        "value",
    ),
    ("gapless", "options.set_gapless", "value"),
];

/// Parse the embedded [`Manifest`].
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("org-evoframework-system-backup: embedded manifest must parse")
}

fn plugin_crate_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is valid semver")
}

/// One item an owner refused on import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Refused {
    /// `config`, `settings` or `playlists`.
    pub domain: &'static str,
    /// What was refused, e.g. `zone Kitchen`.
    pub item: String,
    /// The owner's reason.
    pub reason: String,
}

/// A backup as import reads it. Unknown keys are ignored: only
/// the three domains go back.
#[derive(Debug, Deserialize)]
struct Backup {
    v: u32,
    config: Config,
    playlists: Vec<PlaylistText>,
    settings: Settings,
}

#[derive(Debug, Deserialize)]
struct Config {
    logical_outputs: Vec<LogicalOutput>,
    zones: Vec<Zone>,
    /// Absent in a backup taken before scenes were part of one.
    #[serde(default)]
    scenes: Vec<SceneBackup>,
}

/// A scene as `scene.get` answers it. Its zones stay JSON: only their
/// logical output ids are mapped, and the owner checks the rest.
#[derive(Debug, Deserialize)]
struct SceneBackup {
    name: String,
    zones: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct LogicalOutput {
    id: String,
    name: String,
    output_id: String,
}

#[derive(Debug, Deserialize)]
struct Zone {
    name: String,
    logical_output_ids: Vec<String>,
    eq: Eq,
}

#[derive(Debug, Deserialize)]
struct Eq {
    engaged: bool,
    bands: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct PlaylistText {
    name: String,
    m3u: String,
}

#[derive(Debug, Deserialize)]
struct Settings {
    record: Map<String, Value>,
    eq_presets: Vec<Preset>,
    /// The alarm, or none. Absent in a backup taken before the
    /// alarm was part of one.
    #[serde(default)]
    alarm: Option<AlarmRecord>,
    /// The language and the zone. Absent in a backup taken before they
    /// were part of one.
    #[serde(default)]
    locale: Option<LocaleRecord>,
}

#[derive(Debug, Deserialize)]
struct LocaleRecord {
    language: String,
    timezone: String,
}

#[derive(Debug, Deserialize)]
struct AlarmRecord {
    weekdays: Vec<String>,
    time: String,
    playlist_name: String,
}

#[derive(Debug, Deserialize)]
struct Preset {
    name: String,
    bands: Value,
}

/// The plugin singleton.
pub struct BackupPlugin {
    loaded: bool,
    /// Shelf dispatcher from `LoadContext`. `None` refuses both
    /// verbs.
    dispatcher: Option<Arc<dyn ShelfRequestDispatcher>>,
}

impl BackupPlugin {
    /// New instance; call [`Plugin::load`] before handling requests.
    pub fn new() -> Self {
        Self {
            loaded: false,
            dispatcher: None,
        }
    }

    fn dispatcher(&self) -> Result<&dyn ShelfRequestDispatcher, PluginError> {
        self.dispatcher.as_deref().ok_or_else(|| {
            PluginError::Permanent(
                "the backup reaches its owners through the shelf \
                 dispatcher, and none was provided at load"
                    .to_string(),
            )
        })
    }

    async fn handle_export(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let _: VersionOnly = parse(req)?;
        let d = self.dispatcher()?;
        let backup = export(d).await.map_err(|e| {
            PluginError::Transient(format!("{VERB_EXPORT} refused: {e}"))
        })?;
        within_bound(VERB_EXPORT, &backup, "no backup is answered")?;
        tracing::info!(plugin = PLUGIN_NAME, "backup exported");
        encode(req, &json!({ "v": PAYLOAD_VERSION, "backup": backup }))
    }

    async fn handle_import(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let payload: ImportPayload = parse(req)?;
        // Before the version check: a backup over the bound is too
        // large, whatever its version says.
        within_bound(VERB_IMPORT, &payload.backup, "nothing was written")?;
        let d = self.dispatcher()?;
        let backup_v = payload.backup.get("v").and_then(Value::as_u64);
        if backup_v != Some(u64::from(PAYLOAD_VERSION)) {
            return Err(PluginError::Permanent(format!(
                "{VERB_IMPORT}: backup version {backup_v:?} unsupported; \
                 expected {PAYLOAD_VERSION}"
            )));
        }
        let backup: Backup =
            serde_json::from_value(payload.backup).map_err(|e| {
                PluginError::Permanent(format!(
                    "{VERB_IMPORT}: the backup does not parse: {e}"
                ))
            })?;
        debug_assert_eq!(backup.v, PAYLOAD_VERSION);
        let refused = import(d, backup).await;
        tracing::info!(
            plugin = PLUGIN_NAME,
            refused = refused.len(),
            "backup imported"
        );
        encode(req, &json!({ "v": PAYLOAD_VERSION, "refused": refused }))
    }
}

impl Default for BackupPlugin {
    fn default() -> Self {
        Self::new()
    }
}

/// The size of a backup written as compact UTF-8 JSON.
fn compact_size(backup: &Value) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    // Writing a JSON value into a sink that never fails cannot fail.
    serde_json::to_writer(&mut count, backup)
        .expect("a JSON value always serialises");
    count.0
}

/// Refuse a backup over [`MAX_BACKUP_BYTES`], as Permanent.
fn within_bound(
    verb: &str,
    backup: &Value,
    consequence: &str,
) -> Result<(), PluginError> {
    let size = compact_size(backup);
    if size > MAX_BACKUP_BYTES {
        return Err(PluginError::Permanent(format!(
            "{verb}: the backup is {size} bytes as compact JSON, over the \
             bound of {MAX_BACKUP_BYTES} bytes; {consequence}"
        )));
    }
    Ok(())
}

/// Call one owner verb and parse its answer.
async fn call(
    d: &dyn ShelfRequestDispatcher,
    shelf: &str,
    verb: &str,
    payload: Value,
) -> Result<Value, String> {
    let bytes = serde_json::to_vec(&payload).map_err(|e| e.to_string())?;
    let answer = d
        .dispatch(shelf, verb, bytes, None)
        .await
        .map_err(|e| format!("{verb}: {e}"))?;
    serde_json::from_slice(&answer).map_err(|e| format!("{verb}: {e}"))
}

/// The value under `key` in an owner's answer, or why not.
fn field(answer: &Value, verb: &str, key: &str) -> Result<Value, String> {
    answer
        .get(key)
        .cloned()
        .ok_or_else(|| format!("{verb} answered no {key:?}"))
}

/// Read the three domains. Any owner that cannot answer refuses
/// the whole export.
async fn export(d: &dyn ShelfRequestDispatcher) -> Result<Value, String> {
    let v = json!({ "v": PAYLOAD_VERSION });
    let outputs =
        call(d, DELIVERY, "delivery.list_logical_outputs", v.clone()).await?;
    let zones = call(d, ZONES, "zone.list", v.clone()).await?;
    let scene_names = call(d, ZONES, "scene.list", v.clone()).await?;
    let mut scenes = Vec::new();
    for name in field(&scene_names, "scene.list", "scenes")?
        .as_array()
        .ok_or("scene.list answered no list")?
    {
        let name = name
            .as_str()
            .ok_or("scene.list answered a nameless scene")?;
        let got = call(
            d,
            ZONES,
            "scene.get",
            json!({ "v": PAYLOAD_VERSION, "name": name }),
        )
        .await?;
        scenes.push(field(&got, "scene.get", "scene")?);
    }
    let config = json!({
        "logical_outputs": field(&outputs, "delivery.list_logical_outputs", "logical_outputs")?,
        "zones": field(&zones, "zone.list", "zones")?,
        "scenes": scenes,
    });

    let stored =
        call(d, PLAYLIST, "playlist.list_playlists", v.clone()).await?;
    let names: Vec<String> =
        field(&stored, "playlist.list_playlists", "playlists")?
            .as_array()
            .ok_or("playlist.list_playlists answered no list")?
            .iter()
            .map(|p| {
                p.get("name")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| {
                        "playlist.list_playlists answered a nameless row"
                            .to_string()
                    })
            })
            .collect::<Result<_, _>>()?;
    let mut playlists = Vec::with_capacity(names.len());
    for name in names {
        let text = call(
            d,
            PLAYLIST,
            "playlist.export_file",
            json!({ "v": PAYLOAD_VERSION, "name": name, "format": "m3u", "absolute": false }),
        )
        .await?;
        let m3u = field(&text, "playlist.export_file", "content")?;
        playlists.push(json!({ "name": name, "m3u": m3u }));
    }

    let record = call(d, OPTIONS, "options.get_settings", v.clone()).await?;
    let presets =
        call(d, OPTIONS, "options.list_eq_presets", v.clone()).await?;
    let language = call(d, LOCALE, "language.get", v.clone()).await?;
    let timezone = call(d, LOCALE, "timezone.get", v.clone()).await?;
    let alarm = call(d, ALARM, "alarm.get", v).await?;
    let settings = json!({
        "record": record,
        "eq_presets": field(&presets, "options.list_eq_presets", "presets")?,
        "locale": {
            "language": field(&language, "language.get", "language")?,
            "timezone": field(&timezone, "timezone.get", "timezone")?,
        },
        "alarm": field(&alarm, "alarm.get", "alarm")?,
    });

    Ok(json!({
        "v": PAYLOAD_VERSION,
        "config": config,
        "playlists": playlists,
        "settings": settings,
    }))
}

/// Put a parsed backup back: config, then settings, then
/// playlists. Returns every item an owner refused.
async fn import(
    d: &dyn ShelfRequestDispatcher,
    backup: Backup,
) -> Vec<Refused> {
    let mut refused = Vec::new();
    let mut put =
        |domain: &'static str, item: String, result: Result<Value, String>| {
            match result {
                Ok(answer) => Some(answer),
                Err(reason) => {
                    refused.push(Refused {
                        domain,
                        item,
                        reason,
                    });
                    None
                }
            }
        };

    // Config: logical outputs, then zones over the ids just minted.
    let mut minted: HashMap<String, String> = HashMap::new();
    for out in &backup.config.logical_outputs {
        let answer = call(
            d,
            DELIVERY,
            "delivery.add_logical_output",
            json!({ "v": PAYLOAD_VERSION, "name": out.name, "output_id": out.output_id }),
        )
        .await;
        let item = format!("logical output {}", out.name);
        let new_id = answer.and_then(|a| {
            a.pointer("/logical_output/id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| {
                    "delivery.add_logical_output answered no id".to_string()
                })
        });
        if let Some(id) = put("config", item, new_id.map(Value::String)) {
            minted.insert(
                out.id.clone(),
                id.as_str().unwrap_or_default().to_string(),
            );
        }
    }
    for zone in &backup.config.zones {
        let item = format!("zone {}", zone.name);
        let members: Result<Vec<String>, String> = zone
            .logical_output_ids
            .iter()
            .map(|old| {
                minted.get(old).cloned().ok_or_else(|| {
                    format!("its logical output {old} did not go back")
                })
            })
            .collect();
        let members = match members {
            Ok(m) => m,
            Err(reason) => {
                put("config", item, Err(reason));
                continue;
            }
        };
        let answer = call(
            d,
            ZONES,
            "zone.add",
            json!({ "v": PAYLOAD_VERSION, "name": zone.name, "logical_output_ids": members }),
        )
        .await;
        // How many bands a new zone starts with; a saved zone with fewer
        // is trimmed to its own count once its bands are set.
        let starting_bands = answer
            .as_ref()
            .ok()
            .and_then(|a| a.pointer("/zone/eq/bands"))
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        let new_id = answer.and_then(|a| {
            a.pointer("/zone/id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| "zone.add answered no id".to_string())
        });
        let Some(id) = put("config", item, new_id.map(Value::String)) else {
            continue;
        };
        let id = id.as_str().unwrap_or_default().to_string();
        for (index, band) in zone.eq.bands.iter().enumerate() {
            let mut payload =
                json!({ "v": PAYLOAD_VERSION, "id": id, "index": index });
            merge_band(&mut payload, band);
            let r = call(d, ZONES, "zone.set_eq_band", payload).await;
            put("config", format!("zone {} EQ band {index}", zone.name), r);
        }
        // The bands past the saved count, from the end, so the saved
        // ones keep their places.
        for index in (zone.eq.bands.len()..starting_bands).rev() {
            let r = call(
                d,
                ZONES,
                "zone.remove_eq_band",
                json!({ "v": PAYLOAD_VERSION, "id": id, "index": index }),
            )
            .await;
            put(
                "config",
                format!("zone {} EQ band {index} trimmed", zone.name),
                r,
            );
        }
        let r = call(
            d,
            ZONES,
            "zone.set_eq_engaged",
            json!({ "v": PAYLOAD_VERSION, "id": id, "engaged": zone.eq.engaged }),
        )
        .await;
        put("config", format!("zone {} EQ engaged", zone.name), r);
    }

    // Scenes, once the logical outputs they name are back. Stored as
    // given and never over one the device holds; nothing is recalled.
    for scene in &backup.config.scenes {
        let item = format!("scene {}", scene.name);
        match remap_scene(scene, &minted) {
            Ok(zones) => {
                let r = call(
                    d,
                    ZONES,
                    "scene.put",
                    json!({
                        "v": PAYLOAD_VERSION,
                        "scene": { "name": scene.name, "zones": zones },
                    }),
                )
                .await;
                put("config", item, r);
            }
            Err(reason) => {
                put("config", item, Err(reason));
            }
        }
    }

    // Settings: the record through its setters, then the presets.
    let record = &backup.settings.record;
    // The mixer goes back whole or not at all: checked once, before any of
    // its three setters, and reported once.
    let mixer_refusal = check_mixer(d, record).await.err();
    let mut mixer_reported = false;
    for (name, verb, key) in SETTERS {
        let Some(value) = record.get(*name) else {
            continue;
        };
        if MIXER_FIELDS.contains(name) {
            if let Some(reason) = &mixer_refusal {
                if !mixer_reported {
                    mixer_reported = true;
                    put("settings", "mixer".to_string(), Err(reason.clone()));
                }
                continue;
            }
        }
        if *name == "output_device" {
            // The setter takes any name. A device this one lacks would
            // be stored, so it is checked here first.
            if let Err(reason) = check_output_device(d, value).await {
                put("settings", (*name).to_string(), Err(reason));
                continue;
            }
        }
        let mut payload = json!({ "v": PAYLOAD_VERSION });
        payload[*key] = value.clone();
        let r = call(d, OPTIONS, verb, payload).await;
        put("settings", (*name).to_string(), r);
    }
    if let Some(bands) = record.get("eq_bands").and_then(Value::as_array) {
        for (index, band) in bands.iter().enumerate() {
            let mut payload = json!({ "v": PAYLOAD_VERSION, "index": index });
            merge_band(&mut payload, band);
            let r = call(d, OPTIONS, "options.set_eq_band", payload).await;
            put("settings", format!("eq_bands {index}"), r);
        }
    }
    if let Some(engaged) = record.get("eq_engaged") {
        let r = call(
            d,
            OPTIONS,
            "options.set_eq_engaged",
            json!({ "v": PAYLOAD_VERSION, "value": engaged }),
        )
        .await;
        put("settings", "eq_engaged".to_string(), r);
    }
    for preset in &backup.settings.eq_presets {
        let r = call(
            d,
            OPTIONS,
            "options.save_eq_preset",
            json!({ "v": PAYLOAD_VERSION, "name": preset.name, "bands": preset.bands }),
        )
        .await;
        put("settings", format!("EQ preset {}", preset.name), r);
    }

    // The language and the zone, once the settings are in and before the
    // alarm, which is scheduled on the zone in force.
    if let Some(locale) = &backup.settings.locale {
        for (item, reason) in put_locale(d, locale).await {
            put("settings", item, Err(reason));
        }
    }

    // Playlists, never over one the device already holds.
    for playlist in &backup.playlists {
        let rows = m3u_rows(&playlist.m3u);
        if playlist.name == FAVOURITES_PLAYLIST {
            // The playlist owner refuses this name; the favourites
            // shelf is where its rows go. Adding one already held is a
            // no-op there.
            for uri in rows {
                let r = call(
                    d,
                    FAVOURITES,
                    "favourites.add_favourite",
                    json!({ "v": PAYLOAD_VERSION, "uri": uri }),
                )
                .await;
                put("playlists", format!("favourite {uri}"), r);
            }
            continue;
        }
        if rows.is_empty() {
            // The import verb refuses a file with no rows; an empty
            // playlist is made, not imported.
            let r = call(
                d,
                PLAYLIST,
                "playlist.create_playlist",
                json!({ "v": PAYLOAD_VERSION, "name": playlist.name }),
            )
            .await;
            put("playlists", format!("playlist {}", playlist.name), r);
            continue;
        }
        let r = call(
            d,
            PLAYLIST,
            "playlist.import_file",
            json!({
                "v": PAYLOAD_VERSION,
                "name": playlist.name,
                "format": "m3u",
                "content": playlist.m3u,
                "overwrite": false,
            }),
        )
        .await;
        let item = format!("playlist {}", playlist.name);
        if let Some(answer) = put("playlists", item, r) {
            for row in answer
                .get("skipped")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let entry =
                    row.get("entry").and_then(Value::as_str).unwrap_or("?");
                let reason = row
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("skipped");
                put(
                    "playlists",
                    format!("playlist {} row {entry}", playlist.name),
                    Err(reason.to_string()),
                );
            }
        }
    }

    // The alarm last: it names a playlist, which is in by now.
    if let Some(alarm) = &backup.settings.alarm {
        let r = call(
            d,
            ALARM,
            "alarm.set",
            json!({
                "v": PAYLOAD_VERSION,
                "weekdays": alarm.weekdays,
                "time": alarm.time,
                "playlist_name": alarm.playlist_name,
            }),
        )
        .await;
        put("settings", "alarm".to_string(), r);
    }

    refused
}

/// Put the language and the zone back. A value the device already
/// holds is not sent. Returns what did not go back, as `(item,
/// reason)`: a language the glass has no catalogue for, or a zone the
/// owner refused or did not apply.
async fn put_locale(
    d: &dyn ShelfRequestDispatcher,
    locale: &LocaleRecord,
) -> Vec<(String, String)> {
    let v = json!({ "v": PAYLOAD_VERSION });
    let mut not_back = Vec::new();
    let held = |answer: Result<Value, String>, key: &str| {
        answer.ok().and_then(|a| {
            a.get(key).and_then(Value::as_str).map(str::to_string)
        })
    };
    let language_held =
        held(call(d, LOCALE, "language.get", v.clone()).await, "language");
    if language_held.as_deref().map(str::to_lowercase)
        != Some(locale.language.to_lowercase())
    {
        match call(
            d,
            LOCALE,
            "language.set",
            json!({ "v": PAYLOAD_VERSION, "language": locale.language }),
        )
        .await
        {
            // The owner answers the language in force, not an error, for
            // a tag the glass has no catalogue for.
            Ok(a) => {
                let now = a.get("language").and_then(Value::as_str);
                if now.map(str::to_lowercase)
                    != Some(locale.language.to_lowercase())
                {
                    not_back.push((
                        "language".to_string(),
                        format!(
                            "this device has no catalogue for {:?}; its own \
                             language, {:?}, stays",
                            locale.language,
                            now.unwrap_or("unknown")
                        ),
                    ));
                }
            }
            Err(reason) => not_back.push(("language".to_string(), reason)),
        }
    }
    let zone_held = held(call(d, LOCALE, "timezone.get", v).await, "timezone");
    if zone_held.as_deref() != Some(locale.timezone.as_str()) {
        match call(
            d,
            LOCALE,
            "timezone.set",
            json!({ "v": PAYLOAD_VERSION, "timezone": locale.timezone }),
        )
        .await
        {
            // As the language: the zone in force is answered, not an
            // error, for a zone the host does not hold.
            Ok(a) => {
                let now = a.get("timezone").and_then(Value::as_str);
                if now != Some(locale.timezone.as_str()) {
                    not_back.push((
                        "timezone".to_string(),
                        format!(
                            "this device does not hold the zone {:?}; its own \
                             zone, {:?}, stays",
                            locale.timezone,
                            now.unwrap_or("unknown")
                        ),
                    ));
                }
            }
            Err(reason) => not_back.push((
                "timezone".to_string(),
                format!(
                    "the zone {:?} in the backup was not applied: {reason}",
                    locale.timezone
                ),
            )),
        }
    }
    not_back
}

/// A scene's zones with each logical output id replaced by the id the
/// device minted for it. A scene that names one that did not go back is
/// not put.
fn remap_scene(
    scene: &SceneBackup,
    minted: &HashMap<String, String>,
) -> Result<Vec<Value>, String> {
    let mut zones = Vec::with_capacity(scene.zones.len());
    for zone in &scene.zones {
        let mut zone = zone.clone();
        let ids = zone
            .get("logical_output_ids")
            .and_then(Value::as_array)
            .ok_or("a zone of the scene names no logical_output_ids list")?
            .iter()
            .map(|old| {
                let old = old
                    .as_str()
                    .ok_or("a scene's logical output id is not text")?;
                minted.get(old).cloned().ok_or_else(|| {
                    format!("its logical output {old} did not go back")
                })
            })
            .collect::<Result<Vec<String>, String>>()?;
        zone["logical_output_ids"] = json!(ids);
        zones.push(zone);
    }
    Ok(zones)
}

/// The rows of an m3u: every line that is not blank and not a
/// comment, trimmed.
fn m3u_rows(text: &str) -> Vec<&str> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
}

/// The card token of an `hw:CARD=NAME,DEV=N` or `hw:N,M` descriptor,
/// as the delivery plugin reads it.
fn card_token(descriptor: &str) -> &str {
    let rest = descriptor
        .strip_prefix("hw:CARD=")
        .or_else(|| descriptor.strip_prefix("hw:"))
        .unwrap_or(descriptor);
    if rest.len() == descriptor.len() {
        return descriptor;
    }
    rest.split(',').next().unwrap_or(rest)
}

/// The device index of a descriptor; 0 when it names none.
fn card_device(descriptor: &str) -> u64 {
    let Some(rest) = descriptor
        .strip_prefix("hw:CARD=")
        .or_else(|| descriptor.strip_prefix("hw:"))
    else {
        return 0;
    };
    rest.split(',')
        .nth(1)
        .and_then(|s| s.strip_prefix("DEV=").unwrap_or(s).trim().parse().ok())
        .unwrap_or(0)
}

/// Whether one of the device's outputs is the one `descriptor` names:
/// its `hw:N,M` id, or the same device index on a card matched by
/// name, short id or index. The delivery plugin resolves
/// `output_device` the same way.
fn output_known(outputs: &[Value], descriptor: &str) -> bool {
    if outputs
        .iter()
        .any(|o| o["alsa_id"].as_str() == Some(descriptor))
    {
        return true;
    }
    let card = card_token(descriptor);
    let device = card_device(descriptor);
    outputs.iter().any(|o| {
        o["device_idx"].as_u64() == Some(device)
            && (o["card_name"].as_str() == Some(card)
                || o["card_id"]
                    .as_str()
                    .is_some_and(|id| !id.is_empty() && id == card)
                || o["card_idx"].as_u64().map(|i| i.to_string()).as_deref()
                    == Some(card))
    })
}

/// Whether `output_device` may go back: the empty string, which is
/// the default, or an output this device has. The reason if not.
async fn check_output_device(
    d: &dyn ShelfRequestDispatcher,
    value: &Value,
) -> Result<(), String> {
    let Some(name) = value.as_str() else {
        return Err("output_device is not a name".to_string());
    };
    if name.is_empty() {
        return Ok(());
    }
    let listed = call(
        d,
        DELIVERY,
        "delivery.list_outputs",
        json!({ "v": PAYLOAD_VERSION }),
    )
    .await
    .map_err(|e| {
        format!("could not check {name:?} against the device's outputs: {e}")
    })?;
    let outputs =
        listed
            .get("outputs")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                format!(
                "could not check {name:?}: delivery.list_outputs answered no \
                 outputs"
            )
            })?;
    if output_known(outputs, name) {
        Ok(())
    } else {
        Err(format!(
            "this device has no output {name:?}; its own output_device stays"
        ))
    }
}

/// The three record fields that make up the mixer. They go back together
/// or not at all.
const MIXER_FIELDS: [&str; 3] = ["mixer_type", "mixer_device", "mixer_control"];

/// Whether the mixer in the record may go back: a software mixer that
/// names no device and no control always may; a hardware mixer (or any
/// that names a device or control) only if the device has that control on
/// that card. The reason names the mixer when it may not, and the device's
/// own mixer stays.
async fn check_mixer(
    d: &dyn ShelfRequestDispatcher,
    record: &serde_json::Map<String, Value>,
) -> Result<(), String> {
    let text = |k: &str| record.get(k).and_then(Value::as_str);
    if MIXER_FIELDS.iter().all(|k| record.get(*k).is_none()) {
        return Ok(());
    }
    let (kind, device, control) = (
        text("mixer_type").unwrap_or(""),
        text("mixer_device").unwrap_or(""),
        text("mixer_control").unwrap_or(""),
    );
    if device.is_empty() && control.is_empty() && kind != "hardware" {
        return Ok(());
    }
    if device.is_empty() || control.is_empty() {
        return Err(format!(
            "the backup's {kind:?} mixer names no complete device and \
             control ({device:?}, {control:?}); its own mixer stays"
        ));
    }
    let listed = call(
        d,
        DELIVERY,
        "delivery.list_mixers",
        json!({ "v": PAYLOAD_VERSION, "card": card_token(device) }),
    )
    .await
    .map_err(|e| {
        format!(
            "could not check the mixer {control:?} on {device:?} against \
             the device's mixers: {e}"
        )
    })?;
    let controls = listed
        .get("controls")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            format!(
                "could not check the mixer {control:?} on {device:?}: \
                 delivery.list_mixers answered no controls"
            )
        })?;
    if controls.iter().any(|c| c.as_str() == Some(control)) {
        Ok(())
    } else {
        Err(format!(
            "this device has no mixer control {control:?} on {device:?}; its \
             own mixer stays"
        ))
    }
}

/// Copy a band's `freq_hz`, `gain_db` and `q` into a payload.
fn merge_band(payload: &mut Value, band: &Value) {
    for key in ["freq_hz", "gain_db", "q"] {
        if let Some(v) = band.get(key) {
            payload[key] = v.clone();
        }
    }
}

impl Plugin for BackupPlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: PLUGIN_NAME.to_string(),
                    version: plugin_crate_version(),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: vec![
                        VERB_EXPORT.to_string(),
                        VERB_IMPORT.to_string(),
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
            self.dispatcher =
                ctx.shelf_request_dispatcher.as_ref().map(Arc::clone);
            if self.dispatcher.is_none() {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    "no shelf dispatcher; export_backup and import_backup will refuse"
                );
            }
            self.loaded = true;
            tracing::info!(plugin = PLUGIN_NAME, "system.backup plugin loaded");
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
                HealthReport::unhealthy("system.backup plugin not loaded")
            }
        }
    }
}

impl Respondent for BackupPlugin {
    fn handle_request<'a>(
        &'a self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "system.backup plugin not loaded".to_string(),
                ));
            }
            if req.is_past_deadline() {
                return Err(PluginError::Transient(
                    "request deadline already expired".to_string(),
                ));
            }
            match req.request_type.as_str() {
                VERB_EXPORT => self.handle_export(req).await,
                VERB_IMPORT => self.handle_import(req).await,
                other => Err(PluginError::Permanent(format!(
                    "system.backup: unknown verb {other:?}"
                ))),
            }
        }
    }
}

trait HasPayloadVersion {
    fn payload_version(&self) -> u32;
}

fn default_payload_version() -> u32 {
    PAYLOAD_VERSION
}

#[derive(Debug, Deserialize)]
struct VersionOnly {
    #[serde(default = "default_payload_version")]
    v: u32,
}

impl HasPayloadVersion for VersionOnly {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

#[derive(Debug, Deserialize)]
struct ImportPayload {
    #[serde(default = "default_payload_version")]
    v: u32,
    backup: Value,
}

impl HasPayloadVersion for ImportPayload {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

fn parse<T>(req: &Request) -> Result<T, PluginError>
where
    T: serde::de::DeserializeOwned + HasPayloadVersion,
{
    let parsed: T = serde_json::from_slice(&req.payload).map_err(|e| {
        PluginError::Permanent(format!(
            "{:?} payload is not valid JSON for the expected shape: {e}",
            req.request_type
        ))
    })?;
    if parsed.payload_version() != PAYLOAD_VERSION {
        return Err(PluginError::Permanent(format!(
            "{:?} payload version {} unsupported; expected {}",
            req.request_type,
            parsed.payload_version(),
            PAYLOAD_VERSION
        )));
    }
    Ok(parsed)
}

fn encode(req: &Request, body: &Value) -> Result<Response, PluginError> {
    let bytes = serde_json::to_vec(body).map_err(|e| {
        PluginError::Permanent(format!(
            "{:?} response encode failed: {e}",
            req.request_type
        ))
    })?;
    Ok(Response::for_request(req, bytes))
}

#[cfg(test)]
mod tests;
