// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! # org-evoframework-system-backup
//!
//! Stocks the `system.backup` shelf with two operator gestures,
//! both step-up gated at `system_admin`.
//!
//! - `export_backup { v: 1 }` answers `{ v: 1, backup: { v: 1,
//!   config, playlists, settings } }`:
//!   - `config` is `{ logical_outputs, zones }`, as
//!     `delivery.list_logical_outputs` and `zone.list` answer them;
//!   - `playlists` is `[{ name, m3u }]`, every stored playlist as
//!     `playlist.export_file` answers its text, format `m3u`,
//!     library-relative rows;
//!   - `settings` is `{ record, eq_presets }`, as
//!     `options.get_settings` and `options.list_eq_presets` answer
//!     them.
//!
//!   An owner that cannot answer refuses the export; a backup is
//!   never answered with a domain left out.
//! - `import_backup { v: 1, backup }` refuses a backup whose own `v`
//!   is not 1, or that does not parse, before any write. It then
//!   puts config back, then settings, then playlists, through the
//!   owners' add and set verbs, and answers `{ v: 1, refused: [{
//!   domain, item, reason }] }`. Import adds and sets; it takes
//!   nothing away. An item its owner refuses — a name the device
//!   already holds, hardware the device lacks — is reported and
//!   skipped; nothing is put in its place, and the other items
//!   still go back.
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
    let config = json!({
        "logical_outputs": field(&outputs, "delivery.list_logical_outputs", "logical_outputs")?,
        "zones": field(&zones, "zone.list", "zones")?,
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
    let presets = call(d, OPTIONS, "options.list_eq_presets", v).await?;
    let settings = json!({
        "record": record,
        "eq_presets": field(&presets, "options.list_eq_presets", "presets")?,
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
        let r = call(
            d,
            ZONES,
            "zone.set_eq_engaged",
            json!({ "v": PAYLOAD_VERSION, "id": id, "engaged": zone.eq.engaged }),
        )
        .await;
        put("config", format!("zone {} EQ engaged", zone.name), r);
    }

    // Settings: the record through its setters, then the presets.
    let record = &backup.settings.record;
    for (name, verb, key) in SETTERS {
        let Some(value) = record.get(*name) else {
            continue;
        };
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

    // Playlists, never over one the device already holds.
    for playlist in &backup.playlists {
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

    refused
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
