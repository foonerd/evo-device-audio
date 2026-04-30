//! NetworkManager-backed network plugin for the evo framework.
//!
//! This plugin ports the functional `nmcli` flow used by volumio-evo into an
//! evo-framework plugin surface with durable intent persistence:
//! - `network.nm.status`
//! - `network.nm.scan`
//! - `network.nm.intent.get`
//! - `network.nm.intent.set`
//! - `network.nm.intent.apply`
//! - `network.nm.captive.status`
//! - `network.nm.captive.start`
//! - `network.nm.captive.submit`
//! - `network.nm.captive.complete`
//!
//! Request/response contracts and scenario coverage live under:
//! - `docs/NETWORK_NM_REQUESTS_V1.md`
//! - `docs/CAPTIVE_PORTAL_WORKFLOW.md`
//! - `docs/NETWORK_NM_RUNBOOK.md`

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::manual_async_fn)]

use std::collections::HashMap;
use std::fs::Permissions;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities,
};
use evo_plugin_sdk::Manifest;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// Embedded plugin manifest.
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");

/// Reverse-DNS plugin name.
pub const PLUGIN_NAME: &str = "org.evoframework.network.nm";

const REQUEST_NETWORK_STATUS: &str = "network.nm.status";
const REQUEST_NETWORK_SCAN: &str = "network.nm.scan";
const REQUEST_NETWORK_INTENT_GET: &str = "network.nm.intent.get";
const REQUEST_NETWORK_INTENT_SET: &str = "network.nm.intent.set";
const REQUEST_NETWORK_INTENT_APPLY: &str = "network.nm.intent.apply";
const REQUEST_NETWORK_CAPTIVE_STATUS: &str = "network.nm.captive.status";
const REQUEST_NETWORK_CAPTIVE_START: &str = "network.nm.captive.start";
const REQUEST_NETWORK_CAPTIVE_SUBMIT: &str = "network.nm.captive.submit";
const REQUEST_NETWORK_CAPTIVE_COMPLETE: &str = "network.nm.captive.complete";

const NM_CON_ETHERNET: &str = "evo-network-ethernet";
const NM_CON_WIFI_STA: &str = "evo-network-wifi-sta";
const NM_CON_HOTSPOT_DEFAULT: &str = "volumio-hotspot";
const SECRET_FILE_MODE: u32 = 0o600;
const SECRET_SCHEMA_VERSION: u32 = 1;
const SECRET_CIPHER_XCHACHA20POLY1305: &str = "xchacha20poly1305";
const ENV_SECRET_KEY: &str = "EVO_NETWORK_SECRET_KEY";
const ENV_SECRET_REQUIRE: &str = "EVO_NETWORK_SECRET_REQUIRE";

/// Parse the embedded [`Manifest`].
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("org-evoframework-network-nm: embedded manifest must parse")
}

fn plugin_crate_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is valid semver")
}

#[derive(Clone)]
struct PluginConfig {
    nmcli_path: String,
    default_wifi_iface: String,
    captive: CaptiveConfig,
    secret_key: Option<[u8; 32]>,
    require_encrypted_secrets: bool,
    nmcli_timeout_ms: u64,
    curl_timeout_ms: u64,
    scan_cache_ttl_ms: u64,
}

#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default,
)]
#[serde(rename_all = "snake_case")]
enum CaptiveCredentialPolicy {
    #[default]
    ReplayAllowed,
    SingleUseTicket,
    ManualAfterFailure,
}

#[derive(Debug, Clone)]
struct CaptiveConfig {
    credential_policy: CaptiveCredentialPolicy,
    retry_budget: u32,
    replay_window_sec: u64,
}

impl Default for CaptiveConfig {
    fn default() -> Self {
        Self {
            credential_policy: CaptiveCredentialPolicy::ReplayAllowed,
            retry_budget: 2,
            replay_window_sec: 900,
        }
    }
}

impl PluginConfig {
    fn defaults() -> Self {
        let env_key = std::env::var(ENV_SECRET_KEY)
            .ok()
            .map(|v| derive_secret_key(v.trim()));
        let require_encrypted_secrets = std::env::var(ENV_SECRET_REQUIRE)
            .ok()
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false);
        Self {
            nmcli_path: "/usr/bin/nmcli".to_string(),
            default_wifi_iface: "wlan0".to_string(),
            captive: CaptiveConfig::default(),
            secret_key: env_key,
            require_encrypted_secrets,
            nmcli_timeout_ms: 8000,
            curl_timeout_ms: 30000,
            scan_cache_ttl_ms: 3000,
        }
    }

    fn from_toml_table(table: &toml::Table) -> Result<Self, PluginError> {
        let mut out = Self::defaults();
        if let Some(v) = table.get("nmcli_path").and_then(|v| v.as_str()) {
            let t = v.trim();
            if t.is_empty() {
                return Err(PluginError::Permanent(
                    "nmcli_path cannot be empty".to_string(),
                ));
            }
            out.nmcli_path = t.to_string();
        }
        if let Some(v) = table.get("wifi_iface").and_then(|v| v.as_str()) {
            let t = v.trim();
            if t.is_empty() {
                return Err(PluginError::Permanent(
                    "wifi_iface cannot be empty".to_string(),
                ));
            }
            out.default_wifi_iface = t.to_string();
        }
        if let Some(v) =
            table.get("nmcli_timeout_ms").and_then(|v| v.as_integer())
        {
            if v < 100 {
                return Err(PluginError::Permanent(
                    "nmcli_timeout_ms must be >= 100".to_string(),
                ));
            }
            out.nmcli_timeout_ms = v as u64;
        }
        if let Some(v) =
            table.get("curl_timeout_ms").and_then(|v| v.as_integer())
        {
            if v < 100 {
                return Err(PluginError::Permanent(
                    "curl_timeout_ms must be >= 100".to_string(),
                ));
            }
            out.curl_timeout_ms = v as u64;
        }
        if let Some(v) =
            table.get("scan_cache_ttl_ms").and_then(|v| v.as_integer())
        {
            if v < 0 {
                return Err(PluginError::Permanent(
                    "scan_cache_ttl_ms must be >= 0".to_string(),
                ));
            }
            out.scan_cache_ttl_ms = v as u64;
        }
        let captive_table = table
            .get("captive")
            .and_then(|v| v.as_table())
            .unwrap_or(table);

        if let Some(v) = captive_table
            .get("credential_policy")
            .and_then(|v| v.as_str())
        {
            out.captive.credential_policy =
                match v.trim().to_ascii_lowercase().as_str() {
                    "replay_allowed" => CaptiveCredentialPolicy::ReplayAllowed,
                    "single_use_ticket" => {
                        CaptiveCredentialPolicy::SingleUseTicket
                    }
                    "manual_after_failure" => {
                        CaptiveCredentialPolicy::ManualAfterFailure
                    }
                    _ => {
                        return Err(PluginError::Permanent(format!(
                            "invalid captive credential_policy: {}",
                            v
                        )))
                    }
                };
        }
        if let Some(v) = captive_table
            .get("retry_budget")
            .and_then(|v| v.as_integer())
        {
            if v < 1 {
                return Err(PluginError::Permanent(
                    "captive retry_budget must be >= 1".to_string(),
                ));
            }
            out.captive.retry_budget = v as u32;
        }
        if let Some(v) = captive_table
            .get("replay_window_sec")
            .and_then(|v| v.as_integer())
        {
            if v < 1 {
                return Err(PluginError::Permanent(
                    "captive replay_window_sec must be >= 1".to_string(),
                ));
            }
            out.captive.replay_window_sec = v as u64;
        }
        let secrets_table = table
            .get("secrets")
            .and_then(|v| v.as_table())
            .unwrap_or(table);
        if let Some(v) = secrets_table
            .get("require_encrypted")
            .and_then(|v| v.as_bool())
        {
            out.require_encrypted_secrets = v;
        }
        Ok(out)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
enum WifiRole {
    #[default]
    Sta,
    Ap,
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
enum Ipv4Mode {
    #[default]
    Dhcp,
    #[serde(alias = "manual")]
    Static,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
enum StaSelectionMode {
    Legacy,
    #[default]
    AutoStable,
    AutoPerformance,
    PreferBand,
    LockBssid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EthernetIntent {
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default, alias = "ifname")]
    device: String,
    #[serde(default)]
    ipv4_mode: Ipv4Mode,
    #[serde(default)]
    ipv4_address: String,
    #[serde(default)]
    ipv4_gateway: String,
    #[serde(default)]
    ipv4_dns: Vec<String>,
}

impl Default for EthernetIntent {
    fn default() -> Self {
        Self {
            enabled: true,
            device: String::new(),
            ipv4_mode: Ipv4Mode::Dhcp,
            ipv4_address: String::new(),
            ipv4_gateway: String::new(),
            ipv4_dns: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WifiIntent {
    #[serde(default = "default_wlan_if")]
    ifname: String,
    #[serde(default)]
    role: WifiRole,
    #[serde(default)]
    sta_ssid: String,
    #[serde(default)]
    sta_open: bool,
    #[serde(default)]
    sta_ipv4_mode: Ipv4Mode,
    #[serde(default)]
    sta_ipv4_address: String,
    #[serde(default)]
    sta_ipv4_gateway: String,
    #[serde(default)]
    sta_ipv4_dns: Vec<String>,
    #[serde(default)]
    sta_selection_mode: StaSelectionMode,
    #[serde(default)]
    sta_preferred_band: String,
    #[serde(default)]
    sta_lock_bssid: String,
    #[serde(default)]
    ap_ssid: String,
    #[serde(default = "default_hotspot_channel")]
    ap_channel: u32,
    #[serde(default)]
    ap_band: String,
}

impl Default for WifiIntent {
    fn default() -> Self {
        Self {
            ifname: String::new(),
            role: WifiRole::Sta,
            sta_ssid: String::new(),
            sta_open: false,
            sta_ipv4_mode: Ipv4Mode::Dhcp,
            sta_ipv4_address: String::new(),
            sta_ipv4_gateway: String::new(),
            sta_ipv4_dns: Vec::new(),
            sta_selection_mode: StaSelectionMode::AutoStable,
            sta_preferred_band: String::new(),
            sta_lock_bssid: String::new(),
            ap_ssid: default_ap_ssid(),
            ap_channel: default_hotspot_channel(),
            ap_band: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FallbackIntent {
    #[serde(default = "default_true")]
    hotspot_enabled: bool,
    #[serde(default = "default_hotspot_connection_name")]
    hotspot_connection_name: String,
    #[serde(default)]
    hotspot_ifname: String,
    #[serde(default)]
    hotspot_fallback: bool,
}

impl Default for FallbackIntent {
    fn default() -> Self {
        Self {
            hotspot_enabled: true,
            hotspot_connection_name: default_hotspot_connection_name(),
            hotspot_ifname: String::new(),
            hotspot_fallback: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NetworkIntent {
    #[serde(default = "default_intent_version")]
    version: u32,
    #[serde(default)]
    ethernet: EthernetIntent,
    #[serde(default)]
    wifi: WifiIntent,
    #[serde(default)]
    fallback: FallbackIntent,
}

impl Default for NetworkIntent {
    fn default() -> Self {
        Self {
            version: default_intent_version(),
            ethernet: EthernetIntent::default(),
            wifi: WifiIntent::default(),
            fallback: FallbackIntent::default(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_intent_version() -> u32 {
    1
}

fn default_intent_state_schema_version() -> u32 {
    1
}

fn default_captive_state_schema_version() -> u32 {
    1
}

fn default_hotspot_channel() -> u32 {
    4
}

fn default_wlan_if() -> String {
    "wlan0".to_string()
}

fn default_ap_ssid() -> String {
    "Volumio".to_string()
}

fn default_hotspot_connection_name() -> String {
    NM_CON_HOTSPOT_DEFAULT.to_string()
}

fn lkg_shadow_path(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "state".to_string());
    let file_name =
        if let Some(ext) = path.extension().map(|e| e.to_string_lossy()) {
            format!("{stem}.lkg.{ext}")
        } else {
            format!("{stem}.lkg")
        };
    path.with_file_name(file_name)
}

fn tmp_shadow_path(path: &Path) -> PathBuf {
    if let Some(ext) = path.extension().map(|e| e.to_string_lossy().to_string())
    {
        path.with_extension(format!("{ext}.tmp"))
    } else {
        let base = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "state".to_string());
        path.with_file_name(format!("{base}.tmp"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretEnvelope {
    #[serde(default = "default_secret_schema_version")]
    schema_version: u32,
    cipher: String,
    nonce_b64: String,
    ciphertext_b64: String,
}

fn default_secret_schema_version() -> u32 {
    SECRET_SCHEMA_VERSION
}

fn derive_secret_key(raw: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"org.evoframework.network.nm:secret-key:v1:");
    hasher.update(raw.trim().as_bytes());
    let digest = hasher.finalize();
    let mut key = [0u8; 32];
    key.copy_from_slice(&digest[..32]);
    key
}

fn encrypt_secret_value(
    plain: &str,
    key: &[u8; 32],
) -> Result<SecretEnvelope, PluginError> {
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key));
    let mut nonce = [0u8; 24];
    getrandom::fill(&mut nonce).map_err(|e| {
        PluginError::Permanent(format!("cannot generate secret nonce: {e}"))
    })?;
    let ciphertext = cipher
        .encrypt(XNonce::from_slice(&nonce), plain.trim().as_bytes())
        .map_err(|e| {
            PluginError::Permanent(format!("cannot encrypt secret value: {e}"))
        })?;
    Ok(SecretEnvelope {
        schema_version: SECRET_SCHEMA_VERSION,
        cipher: SECRET_CIPHER_XCHACHA20POLY1305.to_string(),
        nonce_b64: base64::engine::general_purpose::STANDARD.encode(nonce),
        ciphertext_b64: base64::engine::general_purpose::STANDARD
            .encode(ciphertext),
    })
}

fn decrypt_secret_value(
    env: &SecretEnvelope,
    key: &[u8; 32],
) -> Result<String, PluginError> {
    if env.schema_version != SECRET_SCHEMA_VERSION {
        return Err(PluginError::Permanent(format!(
            "unsupported secret schema_version {}",
            env.schema_version
        )));
    }
    if env.cipher.trim() != SECRET_CIPHER_XCHACHA20POLY1305 {
        return Err(PluginError::Permanent(format!(
            "unsupported secret cipher {}",
            env.cipher
        )));
    }
    let nonce = base64::engine::general_purpose::STANDARD
        .decode(env.nonce_b64.trim())
        .map_err(|e| {
            PluginError::Permanent(format!(
                "invalid secret nonce encoding: {e}"
            ))
        })?;
    if nonce.len() != 24 {
        return Err(PluginError::Permanent(format!(
            "invalid secret nonce length {}",
            nonce.len()
        )));
    }
    let ciphertext = base64::engine::general_purpose::STANDARD
        .decode(env.ciphertext_b64.trim())
        .map_err(|e| {
            PluginError::Permanent(format!(
                "invalid secret ciphertext encoding: {e}"
            ))
        })?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key));
    let plain = cipher
        .decrypt(XNonce::from_slice(&nonce), ciphertext.as_ref())
        .map_err(|e| {
            PluginError::Permanent(format!("cannot decrypt secret value: {e}"))
        })?;
    let value = String::from_utf8(plain).map_err(|e| {
        PluginError::Permanent(format!("secret payload is not UTF-8: {e}"))
    })?;
    Ok(value.trim().to_string())
}

fn migrate_intent_state(
    schema_version: u32,
    mut intent: NetworkIntent,
) -> Result<NetworkIntent, String> {
    match schema_version {
        0 | 1 => {
            if intent.version == 0 {
                intent.version = default_intent_version();
            }
            Ok(intent)
        }
        _ => Err(format!(
            "unsupported intent state schema_version {schema_version}"
        )),
    }
}

fn parse_intent_state_toml(raw: &str) -> Result<NetworkIntent, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Ok(NetworkIntent::default());
    }
    match toml::from_str::<IntentStateEnvelope>(t) {
        Ok(env) => migrate_intent_state(env.schema_version, env.intent),
        Err(env_err) => match toml::from_str::<NetworkIntent>(t) {
            Ok(legacy_intent) => migrate_intent_state(0, legacy_intent),
            Err(legacy_err) => Err(format!(
                "intent state parse failed (envelope: {env_err}; legacy: {legacy_err})"
            )),
        },
    }
}

fn migrate_captive_state(
    schema_version: u32,
    state: CaptiveSessionState,
) -> Result<CaptiveSessionState, String> {
    match schema_version {
        0 | 1 => Ok(state),
        _ => Err(format!(
            "unsupported captive state schema_version {schema_version}"
        )),
    }
}

fn parse_captive_state_json(raw: &str) -> Result<CaptiveSessionState, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Ok(CaptiveSessionState::default());
    }
    match serde_json::from_str::<CaptiveStateEnvelope>(t) {
        Ok(env) => migrate_captive_state(env.schema_version, env.state),
        Err(env_err) => match serde_json::from_str::<CaptiveSessionState>(t) {
            Ok(legacy_state) => migrate_captive_state(0, legacy_state),
            Err(legacy_err) => Err(format!(
                "captive state parse failed (envelope: {env_err}; legacy: {legacy_err})"
            )),
        },
    }
}

async fn run_command_output_with_timeout(
    mut cmd: Command,
    timeout_ms: u64,
    label: &str,
) -> Result<std::process::Output, PluginError> {
    match tokio::time::timeout(Duration::from_millis(timeout_ms), cmd.output())
        .await
    {
        Ok(v) => v.map_err(|e| {
            PluginError::Transient(format!("spawn {label} failed: {e}"))
        }),
        Err(_) => Err(PluginError::Transient(format!(
            "{label} timed out after {}ms",
            timeout_ms
        ))),
    }
}

#[derive(Debug, Serialize)]
struct DeviceRow {
    device: String,
    kind: String,
    state: String,
    connection: String,
}

#[derive(Debug, Clone, Serialize)]
struct ScanRow {
    ssid: String,
    signal: u8,
    security: String,
    active: bool,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum BandClass {
    Ghz2_4,
    Ghz5,
    Ghz6,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
struct WifiStaCandidate {
    bssid: String,
    ssid: String,
    signal_pct: u8,
    freq_mhz: Option<u32>,
    band: BandClass,
    active: bool,
}

#[derive(Debug, Deserialize)]
struct ScanRequest {
    #[serde(default)]
    ifname: Option<String>,
    #[serde(default)]
    refresh: bool,
}

#[derive(Debug, Clone)]
struct CachedScan {
    available: Vec<ScanRow>,
    candidates: Vec<WifiStaCandidate>,
    captured_at: Instant,
}

#[derive(Debug, Deserialize)]
struct IntentSetRequest {
    intent: NetworkIntent,
    #[serde(default)]
    sta_psk: Option<String>,
    #[serde(default)]
    ap_psk: Option<String>,
    #[serde(default)]
    apply: bool,
}

#[derive(Debug, Deserialize)]
struct IntentApplyRequest {
    #[serde(default)]
    intent: Option<NetworkIntent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum CaptivePhase {
    #[default]
    Idle,
    ProbeDetected,
    AwaitingCredentials,
    Submitting,
    Authenticated,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct CaptiveSessionState {
    phase: CaptivePhase,
    #[serde(default)]
    last_probe_url: Option<String>,
    #[serde(default)]
    portal_url: Option<String>,
    #[serde(default)]
    last_http_code: Option<u16>,
    #[serde(default)]
    last_error: Option<String>,
    #[serde(default)]
    last_submission_fields: Vec<String>,
    #[serde(default)]
    last_submit_fingerprint: Option<String>,
    #[serde(default)]
    last_submit_at_epoch: Option<u64>,
    #[serde(default)]
    submit_attempts: u32,
    #[serde(default)]
    requires_user_confirmation: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IntentStateEnvelope {
    #[serde(default = "default_intent_state_schema_version")]
    schema_version: u32,
    #[serde(default)]
    intent: NetworkIntent,
}

impl Default for IntentStateEnvelope {
    fn default() -> Self {
        Self {
            schema_version: default_intent_state_schema_version(),
            intent: NetworkIntent::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptiveStateEnvelope {
    #[serde(default = "default_captive_state_schema_version")]
    schema_version: u32,
    #[serde(default)]
    state: CaptiveSessionState,
}

impl Default for CaptiveStateEnvelope {
    fn default() -> Self {
        Self {
            schema_version: default_captive_state_schema_version(),
            state: CaptiveSessionState::default(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct CaptiveStatusRequest {
    #[serde(default = "default_true")]
    probe: bool,
    #[serde(default)]
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CaptiveStartRequest {
    #[serde(default)]
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CaptiveSubmitRequest {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    form: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    confirm_replay: bool,
}

#[derive(Debug, Deserialize)]
struct CaptiveCompleteRequest {
    #[serde(default)]
    success: Option<bool>,
}

#[derive(Debug, Serialize)]
struct ApplyReport {
    ok: bool,
    steps: Vec<String>,
}

/// NetworkManager plugin implementing request/response network operations.
pub struct NetworkNmPlugin {
    loaded: bool,
    config: PluginConfig,
    state_dir: Option<PathBuf>,
    requests_handled: u64,
    scan_cache: HashMap<String, CachedScan>,
}

impl NetworkNmPlugin {
    /// Construct a plugin instance.
    pub fn new() -> Self {
        Self {
            loaded: false,
            config: PluginConfig::defaults(),
            state_dir: None,
            requests_handled: 0,
            scan_cache: HashMap::new(),
        }
    }

    fn ensure_state_dir(&self) -> Result<&Path, PluginError> {
        self.state_dir.as_deref().ok_or_else(|| {
            PluginError::Permanent("state_dir not initialised".to_string())
        })
    }

    fn intent_path(&self) -> Result<PathBuf, PluginError> {
        Ok(self.ensure_state_dir()?.join("network-intent.toml"))
    }

    fn sta_psk_path(&self) -> Result<PathBuf, PluginError> {
        Ok(self.ensure_state_dir()?.join("wifi-sta.psk"))
    }

    fn ap_psk_path(&self) -> Result<PathBuf, PluginError> {
        Ok(self.ensure_state_dir()?.join("wifi-ap.psk"))
    }

    fn captive_state_path(&self) -> Result<PathBuf, PluginError> {
        Ok(self.ensure_state_dir()?.join("captive-session.json"))
    }

    async fn write_text_atomic_with_lkg(
        &self,
        path: &Path,
        data: &str,
    ) -> Result<(), PluginError> {
        async fn atomic_write(path: &Path, data: &str) -> std::io::Result<()> {
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let tmp = tmp_shadow_path(path);
            tokio::fs::write(&tmp, data).await?;
            tokio::fs::rename(&tmp, path).await?;
            Ok(())
        }

        atomic_write(path, data).await.map_err(|e| {
            PluginError::Permanent(format!(
                "cannot write {} atomically: {}",
                path.display(),
                e
            ))
        })?;

        let lkg = lkg_shadow_path(path);
        if let Err(e) = atomic_write(&lkg, data).await {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                path = %lkg.display(),
                error = %e,
                "cannot mirror LKG shadow"
            );
        }
        Ok(())
    }

    async fn load_captive_state(
        &self,
    ) -> Result<CaptiveSessionState, PluginError> {
        let path = self.captive_state_path()?;
        let lkg = lkg_shadow_path(&path);
        let primary_raw = tokio::fs::read_to_string(&path).await.ok();
        if let Some(raw) = primary_raw {
            match parse_captive_state_json(&raw) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    tracing::warn!(
                        plugin = PLUGIN_NAME,
                        path = %path.display(),
                        error = %e,
                        "invalid captive state; trying LKG shadow"
                    );
                }
            }
        }
        if let Ok(raw) = tokio::fs::read_to_string(&lkg).await {
            if let Ok(v) = parse_captive_state_json(&raw) {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    path = %lkg.display(),
                    "using LKG captive state shadow"
                );
                return Ok(v);
            }
        }
        Ok(CaptiveSessionState::default())
    }

    async fn save_captive_state(
        &self,
        state: &CaptiveSessionState,
    ) -> Result<(), PluginError> {
        let path = self.captive_state_path()?;
        let envelope = CaptiveStateEnvelope {
            schema_version: default_captive_state_schema_version(),
            state: state.clone(),
        };
        let raw = serde_json::to_string_pretty(&envelope).map_err(|e| {
            PluginError::Permanent(format!(
                "captive state serialization failed: {e}"
            ))
        })?;
        self.write_text_atomic_with_lkg(&path, &raw).await
    }

    async fn nm_connectivity(&self) -> Option<String> {
        let out = self
            .nmcli_output(&["-t", "-f", "CONNECTIVITY", "general"])
            .await
            .ok()?;
        let v = out.lines().next().unwrap_or("").trim().to_ascii_lowercase();
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    }

    async fn curl_probe(
        &self,
        url: &str,
    ) -> Result<(Option<u16>, Option<String>), PluginError> {
        let mut cmd = Command::new("curl");
        cmd.args([
            "-sS",
            "-L",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}|%{url_effective}",
            "--max-time",
            "20",
            url,
        ]);
        let out = run_command_output_with_timeout(
            cmd,
            self.config.curl_timeout_ms,
            "curl probe",
        )
        .await?;
        if !out.status.success() {
            return Err(PluginError::Transient(format!(
                "curl probe failed (exit {}): {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let raw = String::from_utf8_lossy(&out.stdout);
        Ok(parse_http_probe_metrics(raw.trim()))
    }

    async fn captive_detect(
        &self,
        url: Option<&str>,
    ) -> Result<CaptiveSessionState, PluginError> {
        let probe_url = url
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("http://connectivitycheck.gstatic.com/generate_204");
        let (http_code, effective_url) = self.curl_probe(probe_url).await?;
        let mut state = self.load_captive_state().await.unwrap_or_default();
        state.last_probe_url = Some(probe_url.to_string());
        state.last_http_code = http_code;
        if let Some(ref u) = effective_url {
            let changed = u.trim() != probe_url;
            let non_204 = http_code != Some(204);
            if changed || non_204 {
                state.phase = CaptivePhase::ProbeDetected;
                state.portal_url = Some(u.clone());
                state.last_error = None;
                return Ok(state);
            }
        }
        state.phase = CaptivePhase::Authenticated;
        state.portal_url = effective_url;
        state.last_error = None;
        Ok(state)
    }

    async fn captive_submit(
        &self,
        payload: &CaptiveSubmitRequest,
    ) -> Result<CaptiveSessionState, PluginError> {
        let mut state = self.load_captive_state().await?;
        let url = payload
            .url
            .as_deref()
            .or(state.portal_url.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                PluginError::Permanent(
                    "captive submit requires URL (payload url or prior detected portal_url)".to_string(),
                )
            })?;
        let method = payload
            .method
            .as_deref()
            .map(|m| m.trim().to_ascii_uppercase())
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| "POST".to_string());
        let now_epoch = unix_epoch_seconds();
        let fingerprint =
            captive_submit_fingerprint(url, method.as_str(), &payload.form);
        let policy = self.config.captive.credential_policy;

        if state.last_submit_fingerprint.as_deref()
            == Some(fingerprint.as_str())
        {
            if let Some(last) = state.last_submit_at_epoch {
                if now_epoch.saturating_sub(last)
                    > self.config.captive.replay_window_sec
                {
                    state.submit_attempts = 0;
                    state.last_submit_at_epoch = None;
                }
            }
        } else {
            state.submit_attempts = 0;
            state.last_submit_at_epoch = None;
            state.last_submit_fingerprint = Some(fingerprint.clone());
        }

        if state.submit_attempts >= self.config.captive.retry_budget
            && !payload.confirm_replay
        {
            state.phase = CaptivePhase::AwaitingCredentials;
            state.requires_user_confirmation = true;
            state.last_error = Some(format!(
                "captive retry budget ({}) reached for this credential set; resend with confirm_replay=true to proceed",
                self.config.captive.retry_budget
            ));
            return Ok(state);
        }

        if matches!(policy, CaptiveCredentialPolicy::SingleUseTicket)
            && state.submit_attempts > 0
            && !payload.confirm_replay
        {
            state.phase = CaptivePhase::AwaitingCredentials;
            state.requires_user_confirmation = true;
            state.last_error = Some(
                "single_use_ticket policy blocks automatic credential replay; resend with confirm_replay=true only if operator confirms ticket is reusable"
                    .to_string(),
            );
            return Ok(state);
        }

        if matches!(policy, CaptiveCredentialPolicy::ManualAfterFailure)
            && matches!(state.phase, CaptivePhase::Failed)
            && !payload.confirm_replay
        {
            state.phase = CaptivePhase::AwaitingCredentials;
            state.requires_user_confirmation = true;
            state.last_error = Some(
                "manual_after_failure policy requires confirm_replay=true after failure"
                    .to_string(),
            );
            return Ok(state);
        }

        let mut args: Vec<String> = vec![
            "-sS".into(),
            "-L".into(),
            "-o".into(),
            "/dev/null".into(),
            "-w".into(),
            "%{http_code}|%{url_effective}".into(),
            "--max-time".into(),
            "25".into(),
            "-X".into(),
            method,
        ];
        let mut field_names = Vec::new();
        for (k, v) in &payload.form {
            if k.trim().is_empty() {
                continue;
            }
            field_names.push(k.clone());
            args.push("--data-urlencode".into());
            args.push(format!("{}={}", k, v));
        }
        args.push(url.to_string());

        state.phase = CaptivePhase::Submitting;
        state.requires_user_confirmation = false;
        state.submit_attempts = state.submit_attempts.saturating_add(1);
        state.last_submit_fingerprint = Some(fingerprint);
        state.last_submit_at_epoch = Some(now_epoch);
        let mut cmd = Command::new("curl");
        cmd.args(args.iter().map(String::as_str));
        let out = run_command_output_with_timeout(
            cmd,
            self.config.curl_timeout_ms,
            "curl submit",
        )
        .await?;
        if !out.status.success() {
            state.phase = CaptivePhase::Failed;
            state.last_error = Some(format!(
                "curl submit failed (exit {}): {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
            state.last_submission_fields = field_names;
            state.requires_user_confirmation =
                !matches!(policy, CaptiveCredentialPolicy::ReplayAllowed);
            return Ok(state);
        }

        let probe = String::from_utf8_lossy(&out.stdout);
        let (http_code, effective_url) = parse_http_probe_metrics(probe.trim());
        state.last_http_code = http_code;
        state.portal_url = effective_url;
        state.last_submission_fields = field_names;
        let connectivity = self.nm_connectivity().await.unwrap_or_default();
        if connectivity == "full" || http_code == Some(204) {
            state.phase = CaptivePhase::Authenticated;
            state.last_error = None;
            state.requires_user_confirmation = false;
        } else {
            state.phase = CaptivePhase::AwaitingCredentials;
            state.last_error = Some(
                "captivity may still be active; credentials might be invalid or additional portal step is required"
                    .to_string(),
            );
            state.requires_user_confirmation = matches!(
                policy,
                CaptiveCredentialPolicy::ManualAfterFailure
                    | CaptiveCredentialPolicy::SingleUseTicket
            );
        }
        Ok(state)
    }

    async fn load_intent(&self) -> Result<NetworkIntent, PluginError> {
        let path = self.intent_path()?;
        let lkg = lkg_shadow_path(&path);
        if let Ok(raw) = tokio::fs::read_to_string(&path).await {
            if raw.trim().is_empty() {
                return Ok(NetworkIntent::default());
            }
            match parse_intent_state_toml(&raw) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    tracing::warn!(
                        plugin = PLUGIN_NAME,
                        path = %path.display(),
                        error = %e,
                        "invalid intent TOML; trying LKG shadow"
                    );
                }
            }
        }
        if let Ok(raw) = tokio::fs::read_to_string(&lkg).await {
            if let Ok(v) = parse_intent_state_toml(&raw) {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    path = %lkg.display(),
                    "using LKG intent shadow"
                );
                return Ok(v);
            }
        }
        Ok(NetworkIntent::default())
    }

    async fn save_intent(
        &self,
        intent: &NetworkIntent,
    ) -> Result<(), PluginError> {
        let path = self.intent_path()?;
        let envelope = IntentStateEnvelope {
            schema_version: default_intent_state_schema_version(),
            intent: intent.clone(),
        };
        let text = toml::to_string_pretty(&envelope).map_err(|e| {
            PluginError::Permanent(format!("intent serialization failed: {e}"))
        })?;
        self.write_text_atomic_with_lkg(&path, &text).await
    }

    async fn write_secret_bytes_atomic(
        &self,
        path: &Path,
        bytes: &[u8],
    ) -> Result<(), PluginError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                PluginError::Permanent(format!(
                    "cannot create parent {}: {}",
                    parent.display(),
                    e
                ))
            })?;
        }
        let tmp = tmp_shadow_path(path);
        let mut opts = OpenOptions::new();
        opts.create(true).write(true).truncate(true);
        #[cfg(unix)]
        opts.mode(SECRET_FILE_MODE);
        let mut f = opts.open(&tmp).await.map_err(|e| {
            PluginError::Permanent(format!(
                "cannot open {}: {}",
                tmp.display(),
                e
            ))
        })?;
        f.write_all(bytes).await.map_err(|e| {
            PluginError::Permanent(format!(
                "cannot write {}: {}",
                tmp.display(),
                e
            ))
        })?;
        f.flush().await.map_err(|e| {
            PluginError::Permanent(format!(
                "cannot flush {}: {}",
                tmp.display(),
                e
            ))
        })?;
        f.sync_all().await.map_err(|e| {
            PluginError::Permanent(format!(
                "cannot sync {}: {}",
                tmp.display(),
                e
            ))
        })?;
        drop(f);
        tokio::fs::rename(&tmp, path).await.map_err(|e| {
            PluginError::Permanent(format!(
                "cannot rename {} -> {}: {}",
                tmp.display(),
                path.display(),
                e
            ))
        })?;
        #[cfg(unix)]
        {
            let perms = Permissions::from_mode(SECRET_FILE_MODE);
            let _ = tokio::fs::set_permissions(path, perms).await;
        }
        Ok(())
    }

    async fn read_optional_secret(
        &self,
        path: &Path,
    ) -> Result<Option<String>, PluginError> {
        let raw = match tokio::fs::read_to_string(path).await {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
        let t = raw.trim();
        if t.is_empty() {
            return Ok(None);
        }
        match serde_json::from_str::<SecretEnvelope>(t) {
            Ok(env) => {
                let key = self.config.secret_key.as_ref().ok_or_else(|| {
                    PluginError::Permanent(format!(
                        "encrypted secret {} requires {}",
                        path.display(),
                        ENV_SECRET_KEY
                    ))
                })?;
                let plain = decrypt_secret_value(&env, key)?;
                Ok(Some(plain))
            }
            Err(_) => {
                if self.config.require_encrypted_secrets {
                    return Err(PluginError::Permanent(format!(
                        "plaintext secret {} rejected; encrypted secrets required",
                        path.display()
                    )));
                }
                Ok(Some(t.to_string()))
            }
        }
    }

    async fn write_optional_secret(
        &self,
        path: PathBuf,
        value: Option<&str>,
    ) -> Result<(), PluginError> {
        match value.map(str::trim) {
            Some(v) if !v.is_empty() => {
                let payload = if let Some(key) = self.config.secret_key.as_ref()
                {
                    let envelope = encrypt_secret_value(v, key)?;
                    serde_json::to_vec_pretty(&envelope).map_err(|e| {
                        PluginError::Permanent(format!(
                            "cannot serialize encrypted secret envelope: {e}"
                        ))
                    })?
                } else {
                    if self.config.require_encrypted_secrets {
                        return Err(PluginError::Permanent(format!(
                            "{} required by {} but key is missing",
                            ENV_SECRET_KEY, ENV_SECRET_REQUIRE
                        )));
                    }
                    format!("{v}\n").into_bytes()
                };
                self.write_secret_bytes_atomic(&path, &payload).await
            }
            _ => {
                if tokio::fs::metadata(&path).await.is_ok() {
                    tokio::fs::remove_file(&path).await.map_err(|e| {
                        PluginError::Permanent(format!(
                            "cannot remove {}: {}",
                            path.display(),
                            e
                        ))
                    })?;
                }
                Ok(())
            }
        }
    }

    async fn nmcli_output(&self, args: &[&str]) -> Result<String, PluginError> {
        let mut cmd = Command::new(&self.config.nmcli_path);
        cmd.args(args);
        let direct = run_command_output_with_timeout(
            cmd,
            self.config.nmcli_timeout_ms,
            "nmcli direct",
        )
        .await?;
        if direct.status.success() {
            return Ok(String::from_utf8_lossy(&direct.stdout).to_string());
        }

        let mut sudo_cmd = Command::new("sudo");
        sudo_cmd.arg("-n").arg(&self.config.nmcli_path).args(args);
        let sudo = run_command_output_with_timeout(
            sudo_cmd,
            self.config.nmcli_timeout_ms,
            "nmcli sudo fallback",
        )
        .await
        .ok();
        if let Some(out) = sudo {
            if out.status.success() {
                return Ok(String::from_utf8_lossy(&out.stdout).to_string());
            }
        }

        let stderr = String::from_utf8_lossy(&direct.stderr);
        let stdout = String::from_utf8_lossy(&direct.stdout);
        Err(PluginError::Transient(format!(
            "nmcli failed (args {:?}): {} {}",
            args,
            stderr.trim(),
            stdout.trim()
        )))
    }

    async fn nmcli_output_owned(
        &self,
        args: &[String],
    ) -> Result<String, PluginError> {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.nmcli_output(&refs).await
    }

    async fn nm_connection_exists(&self, name: &str) -> bool {
        let mut cmd = Command::new(&self.config.nmcli_path);
        cmd.args(["connection", "show", name]);
        let out = run_command_output_with_timeout(
            cmd,
            self.config.nmcli_timeout_ms,
            "nmcli connection show",
        )
        .await;
        out.map(|o| o.status.success()).unwrap_or(false)
    }

    async fn nm_device_table(&self) -> Result<Vec<DeviceRow>, PluginError> {
        let raw = self
            .nmcli_output(&[
                "-t",
                "-f",
                "DEVICE,TYPE,STATE,CONNECTION",
                "device",
            ])
            .await?;
        let mut rows = Vec::new();
        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let mut parts = line.splitn(4, ':');
            rows.push(DeviceRow {
                device: parts.next().unwrap_or("").to_string(),
                kind: parts.next().unwrap_or("").to_string(),
                state: parts.next().unwrap_or("").to_string(),
                connection: parts.next().unwrap_or("").to_string(),
            });
        }
        Ok(rows)
    }

    async fn wifi_scan(
        &self,
        ifname: Option<&str>,
    ) -> Result<Vec<ScanRow>, PluginError> {
        let mut args: Vec<String> = vec![
            "-t".into(),
            "-f".into(),
            "SSID,SIGNAL,SECURITY,ACTIVE".into(),
            "dev".into(),
            "wifi".into(),
            "list".into(),
        ];
        if let Some(i) = ifname.map(str::trim).filter(|s| !s.is_empty()) {
            args.push("ifname".into());
            args.push(i.to_string());
        }
        let raw = self.nmcli_output_owned(&args).await?;
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let mut parts = line.splitn(4, ':');
            let ssid = parts.next().unwrap_or("").trim().to_string();
            if ssid.is_empty() || !seen.insert(ssid.clone()) {
                continue;
            }
            let signal_pct =
                parts.next().unwrap_or("").trim().parse::<u8>().ok();
            let security_raw = parts.next().unwrap_or("").trim();
            let active_raw =
                parts.next().unwrap_or("").trim().to_ascii_lowercase();
            out.push(ScanRow {
                ssid,
                signal: signal_bars_from_pct(signal_pct),
                security: security_label(security_raw).to_string(),
                active: active_raw == "yes" || active_raw == "y",
            });
        }
        Ok(out)
    }

    async fn wifi_scan_candidates(
        &self,
        ifname: Option<&str>,
    ) -> Result<Vec<WifiStaCandidate>, PluginError> {
        let mut args: Vec<String> = vec![
            "-t".into(),
            "-f".into(),
            "BSSID,SSID,SIGNAL,FREQ,ACTIVE".into(),
            "dev".into(),
            "wifi".into(),
            "list".into(),
        ];
        if let Some(i) = ifname.map(str::trim).filter(|s| !s.is_empty()) {
            args.push("ifname".into());
            args.push(i.to_string());
        }
        let raw = self.nmcli_output_owned(&args).await?;
        let mut out = Vec::new();
        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let mut p = line.splitn(5, ':');
            let bssid = p.next().unwrap_or("").trim().to_string();
            let ssid = p.next().unwrap_or("").trim().to_string();
            let signal_pct = p
                .next()
                .unwrap_or("")
                .trim()
                .parse::<u8>()
                .ok()
                .unwrap_or(0);
            let freq_mhz = p.next().unwrap_or("").trim().parse::<u32>().ok();
            let active = matches!(
                p.next().unwrap_or("").trim().to_ascii_lowercase().as_str(),
                "yes" | "y"
            );
            if bssid.is_empty() || ssid.is_empty() {
                continue;
            }
            out.push(WifiStaCandidate {
                bssid,
                ssid,
                signal_pct,
                freq_mhz,
                band: band_from_freq(freq_mhz),
                active,
            });
        }
        Ok(out)
    }

    async fn wifi_scan_with_cache(
        &mut self,
        ifname: Option<&str>,
        refresh: bool,
    ) -> Result<(Vec<ScanRow>, Vec<WifiStaCandidate>, bool), PluginError> {
        let key = ifname.unwrap_or_default().trim().to_string();
        let ttl = self.config.scan_cache_ttl_ms;
        if !refresh && ttl > 0 {
            if let Some(cached) = self.scan_cache.get(&key) {
                if cached.captured_at.elapsed()
                    <= Duration::from_millis(self.config.scan_cache_ttl_ms)
                {
                    return Ok((
                        cached.available.clone(),
                        cached.candidates.clone(),
                        true,
                    ));
                }
            }
        }
        let available = self.wifi_scan(ifname).await?;
        let candidates = self.wifi_scan_candidates(ifname).await?;
        if ttl > 0 {
            self.scan_cache.insert(
                key,
                CachedScan {
                    available: available.clone(),
                    candidates: candidates.clone(),
                    captured_at: Instant::now(),
                },
            );
        }
        Ok((available, candidates, false))
    }

    fn select_sta_candidate(
        &self,
        wifi: &WifiIntent,
        candidates: &[WifiStaCandidate],
    ) -> Option<WifiStaCandidate> {
        let ssid = wifi.sta_ssid.trim();
        if ssid.is_empty() {
            return None;
        }
        let lock_bssid = wifi.sta_lock_bssid.trim();
        if !lock_bssid.is_empty() {
            return candidates
                .iter()
                .find(|c| {
                    c.ssid == ssid && c.bssid.eq_ignore_ascii_case(lock_bssid)
                })
                .cloned();
        }

        let preferred_band =
            normalize_band_pref(wifi.sta_preferred_band.as_str());
        let mut filtered: Vec<WifiStaCandidate> = candidates
            .iter()
            .filter(|c| c.ssid == ssid)
            .cloned()
            .collect();
        if filtered.is_empty() {
            return None;
        }
        filtered.sort_by(|a, b| {
            let sa = sta_candidate_score(
                a,
                &wifi.sta_selection_mode,
                preferred_band,
            );
            let sb = sta_candidate_score(
                b,
                &wifi.sta_selection_mode,
                preferred_band,
            );
            sb.cmp(&sa)
                .then_with(|| b.active.cmp(&a.active))
                .then_with(|| b.signal_pct.cmp(&a.signal_pct))
        });
        filtered.into_iter().next()
    }

    fn hotspot_connection_name(intent: &NetworkIntent) -> String {
        let t = intent.fallback.hotspot_connection_name.trim();
        if t.is_empty() {
            NM_CON_HOTSPOT_DEFAULT.to_string()
        } else {
            t.to_string()
        }
    }

    fn effective_wifi_ifname(&self, intent: &NetworkIntent) -> String {
        let t = intent.wifi.ifname.trim();
        if !t.is_empty() {
            return t.to_string();
        }
        self.config.default_wifi_iface.clone()
    }

    fn effective_hotspot_ifname(&self, intent: &NetworkIntent) -> String {
        let t = intent.fallback.hotspot_ifname.trim();
        if !t.is_empty() {
            return t.to_string();
        }
        self.effective_wifi_ifname(intent)
    }

    fn nm_ipv4_args(
        mode: &Ipv4Mode,
        address: &str,
        gateway: &str,
        dns: &[String],
    ) -> Vec<String> {
        match mode {
            Ipv4Mode::Dhcp => vec!["ipv4.method".into(), "auto".into()],
            Ipv4Mode::Static => {
                let mut out = vec![
                    "ipv4.method".into(),
                    "manual".into(),
                    "ipv4.addresses".into(),
                    address.trim().to_string(),
                ];
                if !gateway.trim().is_empty() {
                    out.push("ipv4.gateway".into());
                    out.push(gateway.trim().to_string());
                }
                let dns_join = dns
                    .iter()
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                if !dns_join.is_empty() {
                    out.push("ipv4.dns".into());
                    out.push(dns_join);
                }
                out
            }
        }
    }

    async fn ensure_ethernet(
        &self,
        intent: &NetworkIntent,
        steps: &mut Vec<String>,
    ) -> Result<(), PluginError> {
        let eth = &intent.ethernet;
        if !eth.enabled {
            steps.push("skipped ethernet (disabled in intent)".to_string());
            return Ok(());
        }

        if matches!(eth.ipv4_mode, Ipv4Mode::Static)
            && eth.ipv4_address.trim().is_empty()
        {
            return Err(PluginError::Permanent(
                "ethernet static IPv4 requires ipv4_address (CIDR)".to_string(),
            ));
        }

        let ifname = if eth.device.trim().is_empty() {
            let rows = self.nm_device_table().await?;
            match first_ethernet_device(&rows) {
                Some(v) => v,
                None => {
                    steps.push(
                        "warning: ethernet enabled in intent but no ethernet device found; skipping"
                            .to_string(),
                    );
                    return Ok(());
                }
            }
        } else {
            eth.device.trim().to_string()
        };
        let props = Self::nm_ipv4_args(
            &eth.ipv4_mode,
            &eth.ipv4_address,
            &eth.ipv4_gateway,
            &eth.ipv4_dns,
        );

        if self.nm_connection_exists(NM_CON_ETHERNET).await {
            let mut args = vec![
                "connection".into(),
                "modify".into(),
                NM_CON_ETHERNET.into(),
                "connection.interface-name".into(),
                ifname.clone(),
            ];
            args.extend(props);
            self.nmcli_output_owned(&args).await?;
            steps.push(format!("modified {NM_CON_ETHERNET}"));
        } else {
            let mut args = vec![
                "connection".into(),
                "add".into(),
                "type".into(),
                "ethernet".into(),
                "con-name".into(),
                NM_CON_ETHERNET.into(),
                "ifname".into(),
                ifname.clone(),
            ];
            args.extend(props);
            self.nmcli_output_owned(&args).await?;
            steps.push(format!("added {NM_CON_ETHERNET}"));
        }

        self.nmcli_output(&["connection", "up", NM_CON_ETHERNET])
            .await?;
        steps.push(format!("brought up {NM_CON_ETHERNET}"));
        Ok(())
    }

    async fn connection_down_lossy(&self, name: &str) {
        if name.trim().is_empty() {
            return;
        }
        let mut cmd = Command::new(&self.config.nmcli_path);
        cmd.args(["connection", "down", name]);
        let out = run_command_output_with_timeout(
            cmd,
            self.config.nmcli_timeout_ms,
            "nmcli connection down",
        )
        .await;
        match out {
            Ok(v) if v.status.success() => {}
            Ok(v) => {
                let code = v.status.code().unwrap_or(-1);
                if code != 10 {
                    tracing::debug!(
                        plugin = PLUGIN_NAME,
                        connection = name,
                        exit = code,
                        "nmcli connection down non-success"
                    );
                }
            }
            Err(_) => {}
        }
    }

    async fn nmcli_spawn_output(
        &self,
        args: &[&str],
    ) -> Result<std::process::Output, PluginError> {
        let mut cmd = Command::new(&self.config.nmcli_path);
        cmd.args(args);
        run_command_output_with_timeout(
            cmd,
            self.config.nmcli_timeout_ms,
            "nmcli spawn output",
        )
        .await
    }

    async fn connection_up_hotspot_with_retries(
        &self,
        con_name: &str,
        steps: &mut Vec<String>,
    ) -> bool {
        const HOTSPOT_BRINGUP_ATTEMPTS: u32 = 4;
        const HOTSPOT_BRINGUP_DELAY_MS: u64 = 400;

        if con_name.trim().is_empty() {
            return false;
        }
        for attempt in 1..=HOTSPOT_BRINGUP_ATTEMPTS {
            match self
                .nmcli_spawn_output(&["connection", "up", con_name])
                .await
            {
                Ok(out) if out.status.success() => {
                    if attempt > 1 {
                        steps.push(format!(
                            "brought up {} (attempt {} of {})",
                            con_name, attempt, HOTSPOT_BRINGUP_ATTEMPTS
                        ));
                    } else {
                        steps.push(format!("brought up {con_name}"));
                    }
                    return true;
                }
                _ => {}
            }
            if attempt < HOTSPOT_BRINGUP_ATTEMPTS {
                tokio::time::sleep(Duration::from_millis(
                    HOTSPOT_BRINGUP_DELAY_MS,
                ))
                .await;
            }
        }
        steps.push(format!(
            "warning: {} failed connection up after {} attempts",
            con_name, HOTSPOT_BRINGUP_ATTEMPTS
        ));
        false
    }

    async fn ensure_wifi_sta(
        &self,
        wifi_ifname: &str,
        wifi: &WifiIntent,
        sta_psk: Option<&str>,
        sta_connection_up_nonfatal: bool,
        steps: &mut Vec<String>,
    ) -> Result<(), PluginError> {
        let ifname = wifi_ifname.trim();
        let ssid = wifi.sta_ssid.trim();
        if ifname.is_empty() {
            return Err(PluginError::Permanent(
                "wifi interface name is empty".to_string(),
            ));
        }
        if ssid.is_empty() {
            return Err(PluginError::Permanent(
                "wifi.sta_ssid is required for sta role".to_string(),
            ));
        }
        if matches!(wifi.sta_ipv4_mode, Ipv4Mode::Static)
            && wifi.sta_ipv4_address.trim().is_empty()
        {
            return Err(PluginError::Permanent(
                "wifi static IPv4 requires sta_ipv4_address (CIDR)".to_string(),
            ));
        }

        let scan_candidates =
            self.wifi_scan_candidates(Some(ifname)).await.ok();
        let selected_candidate = scan_candidates
            .as_ref()
            .and_then(|c| self.select_sta_candidate(wifi, c));
        let selected_bssid = selected_candidate
            .as_ref()
            .map(|c| c.bssid.trim().to_string())
            .filter(|s| !s.is_empty());
        if let Some(ref cand) = selected_candidate {
            steps.push(format!(
                "STA candidate selected ssid={:?} bssid={} band={:?} signal={} mode={:?}",
                cand.ssid,
                cand.bssid,
                cand.band,
                cand.signal_pct,
                wifi.sta_selection_mode
            ));
        }

        let mut base = vec![
            "connection".to_string(),
            "modify".to_string(),
            NM_CON_WIFI_STA.to_string(),
            "connection.interface-name".to_string(),
            ifname.to_string(),
            "802-11-wireless.ssid".to_string(),
            ssid.to_string(),
        ];
        if let Some(ref bssid) = selected_bssid {
            base.extend([
                "802-11-wireless.bssid".to_string(),
                bssid.to_string(),
            ]);
        } else {
            base.extend(["802-11-wireless.bssid".to_string(), String::new()]);
        }
        if wifi.sta_open {
            base.extend(["wifi-sec.key-mgmt".into(), "none".into()]);
        } else {
            let psk = sta_psk
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    PluginError::Permanent(
                        "wifi-sta.psk missing while sta_open=false".to_string(),
                    )
                })?;
            base.extend([
                "wifi-sec.key-mgmt".into(),
                "wpa-psk".into(),
                "wifi-sec.psk-flags".into(),
                "0".into(),
                "wifi-sec.psk".into(),
                psk.to_string(),
            ]);
        }
        base.extend(Self::nm_ipv4_args(
            &wifi.sta_ipv4_mode,
            wifi.sta_ipv4_address.trim(),
            wifi.sta_ipv4_gateway.trim(),
            &wifi.sta_ipv4_dns,
        ));

        if self.nm_connection_exists(NM_CON_WIFI_STA).await {
            self.nmcli_output_owned(&base).await?;
            steps.push(format!("modified {NM_CON_WIFI_STA}"));
        } else {
            let mut add = vec![
                "connection".into(),
                "add".into(),
                "type".into(),
                "wifi".into(),
                "con-name".into(),
                NM_CON_WIFI_STA.into(),
                "ifname".into(),
                ifname.to_string(),
                "ssid".into(),
                ssid.to_string(),
            ];
            if let Some(ref bssid) = selected_bssid {
                add.extend(["802-11-wireless.bssid".into(), bssid.to_string()]);
            }
            if wifi.sta_open {
                add.extend(["wifi-sec.key-mgmt".into(), "none".into()]);
            } else {
                let psk = sta_psk
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        PluginError::Permanent(
                            "wifi-sta.psk missing while sta_open=false"
                                .to_string(),
                        )
                    })?;
                add.extend([
                    "wifi-sec.key-mgmt".into(),
                    "wpa-psk".into(),
                    "wifi-sec.psk-flags".into(),
                    "0".into(),
                    "wifi-sec.psk".into(),
                    psk.to_string(),
                ]);
            }
            add.extend(Self::nm_ipv4_args(
                &wifi.sta_ipv4_mode,
                wifi.sta_ipv4_address.trim(),
                wifi.sta_ipv4_gateway.trim(),
                &wifi.sta_ipv4_dns,
            ));
            self.nmcli_output_owned(&add).await?;
            steps.push(format!("added {NM_CON_WIFI_STA}"));
        }

        if sta_connection_up_nonfatal {
            match self
                .nmcli_spawn_output(&["connection", "up", NM_CON_WIFI_STA])
                .await
            {
                Ok(out) if out.status.success() => {
                    steps.push(format!("brought up {NM_CON_WIFI_STA}"));
                }
                Ok(out) => {
                    steps.push(format!(
                        "warning: nmcli connection up {} failed (exit {}): {}",
                        NM_CON_WIFI_STA,
                        out.status.code().unwrap_or(-1),
                        String::from_utf8_lossy(&out.stderr).trim()
                    ));
                }
                Err(e) => {
                    steps.push(format!(
                        "warning: nmcli connection up {} spawn: {}",
                        NM_CON_WIFI_STA, e
                    ));
                }
            }
        } else {
            self.nmcli_output(&["connection", "up", NM_CON_WIFI_STA])
                .await?;
            steps.push(format!("brought up {NM_CON_WIFI_STA}"));
        }
        Ok(())
    }

    async fn ensure_wifi_ap(
        &self,
        wifi_ifname: &str,
        wifi: &WifiIntent,
        ap_psk: Option<&str>,
        hotspot_name: &str,
        steps: &mut Vec<String>,
    ) -> Result<(), PluginError> {
        let ifname = wifi_ifname.trim();
        let ssid = wifi.ap_ssid.trim();
        if ssid.is_empty() {
            return Err(PluginError::Permanent(
                "wifi.ap_ssid is required for hotspot".to_string(),
            ));
        }

        let psk = ap_psk.map(str::trim).filter(|s| !s.is_empty());
        let mut modify = vec![
            "connection".into(),
            "modify".into(),
            hotspot_name.to_string(),
            "connection.interface-name".into(),
            ifname.to_string(),
            "802-11-wireless.mode".into(),
            "ap".into(),
            "802-11-wireless.ssid".into(),
            ssid.to_string(),
            "ipv4.method".into(),
            "shared".into(),
            "ipv6.method".into(),
            "ignore".into(),
            "connection.autoconnect".into(),
            "no".into(),
        ];
        push_nm_ap_channel(&mut modify, wifi);

        if let Some(p) = psk {
            modify.extend([
                "wifi-sec.key-mgmt".into(),
                "wpa-psk".into(),
                "wifi-sec.psk-flags".into(),
                "0".into(),
                "wifi-sec.psk".into(),
                p.to_string(),
            ]);
        }

        if self.nm_connection_exists(hotspot_name).await {
            self.nmcli_output_owned(&modify).await?;
            if psk.is_none() {
                let _ = self
                    .nmcli_output(&[
                        "connection",
                        "modify",
                        hotspot_name,
                        "remove",
                        "802-11-wireless-security",
                    ])
                    .await;
            }
            steps.push(format!("modified hotspot profile {hotspot_name}"));
        } else {
            let mut add = vec![
                "connection".into(),
                "add".into(),
                "type".into(),
                "wifi".into(),
                "con-name".into(),
                hotspot_name.to_string(),
                "ifname".into(),
                ifname.to_string(),
                "autoconnect".into(),
                "no".into(),
                "wifi.mode".into(),
                "ap".into(),
                "ssid".into(),
                ssid.to_string(),
                "ipv4.method".into(),
                "shared".into(),
                "ipv6.method".into(),
                "ignore".into(),
            ];
            push_nm_ap_channel(&mut add, wifi);
            if let Some(p) = psk {
                add.extend([
                    "wifi-sec.key-mgmt".into(),
                    "wpa-psk".into(),
                    "wifi-sec.psk-flags".into(),
                    "0".into(),
                    "wifi-sec.psk".into(),
                    p.to_string(),
                ]);
            }
            self.nmcli_output_owned(&add).await?;
            steps.push(format!("added hotspot profile {hotspot_name}"));
        }
        if !self
            .connection_up_hotspot_with_retries(hotspot_name, steps)
            .await
        {
            return Err(PluginError::Transient(
                "nmcli connection up wifi AP failed after retries".to_string(),
            ));
        }
        Ok(())
    }

    async fn ensure_hotspot_profile(
        &self,
        wifi_ifname: &str,
        wifi: &WifiIntent,
        ap_psk: Option<&str>,
        fallback: &FallbackIntent,
        steps: &mut Vec<String>,
    ) -> Result<(), PluginError> {
        if !fallback.hotspot_enabled {
            return Ok(());
        }
        let hs_name = Self::hotspot_connection_name(&NetworkIntent {
            version: 1,
            ethernet: EthernetIntent::default(),
            wifi: wifi.clone(),
            fallback: fallback.clone(),
        });
        self.ensure_wifi_ap(wifi_ifname, wifi, ap_psk, &hs_name, steps)
            .await
    }

    async fn resolved_ethernet_ifname(
        &self,
        eth: &EthernetIntent,
    ) -> Result<Option<String>, PluginError> {
        if !eth.enabled {
            return Ok(None);
        }
        if !eth.device.trim().is_empty() {
            return Ok(Some(eth.device.trim().to_string()));
        }
        let table = self.nm_device_table().await?;
        Ok(first_ethernet_device(&table))
    }

    async fn ethernet_intent_has_no_carrier(
        &self,
        eth: &EthernetIntent,
    ) -> Result<bool, PluginError> {
        if !eth.enabled {
            return Ok(false);
        }
        let Some(iface) = self.resolved_ethernet_ifname(eth).await? else {
            return Ok(false);
        };
        Ok(sysfs_ethernet_no_carrier(&iface))
    }

    async fn try_critical_open_hotspot_recovery(
        &self,
        intent: &NetworkIntent,
        hs_name: &str,
        steps: &mut Vec<String>,
    ) -> Result<bool, PluginError> {
        if !intent.fallback.hotspot_enabled || hs_name.trim().is_empty() {
            return Ok(false);
        }
        if !self
            .ethernet_intent_has_no_carrier(&intent.ethernet)
            .await?
        {
            return Ok(false);
        }
        let _ = self
            .nmcli_output(&[
                "connection",
                "modify",
                hs_name,
                "remove",
                "802-11-wireless-security",
            ])
            .await;
        steps.push(format!(
            "critical: Ethernet no carrier + hotspot failed; forced open AP on {}",
            hs_name
        ));
        Ok(self
            .connection_up_hotspot_with_retries(hs_name, steps)
            .await)
    }

    async fn nm_active_connection_names_on_device(
        &self,
        ifname: &str,
    ) -> Vec<String> {
        let raw = self
            .nmcli_output(&[
                "-t",
                "-f",
                "NAME,DEVICE",
                "connection",
                "show",
                "--active",
            ])
            .await
            .unwrap_or_default();
        raw.lines()
            .filter_map(|line| {
                let mut p = line.splitn(2, ':');
                let name = p.next().unwrap_or("").trim();
                let dev = p.next().unwrap_or("").trim();
                if !name.is_empty() && dev == ifname.trim() {
                    Some(name.to_string())
                } else {
                    None
                }
            })
            .collect()
    }

    async fn restore_sta_after_hotspot_on_shared_radio(
        &self,
        intent: &NetworkIntent,
        sta_ifname: &str,
        hotspot_con: &str,
        steps: &mut Vec<String>,
    ) -> Result<(), PluginError> {
        const SETTLE_ATTEMPTS: u32 = 15;
        const SETTLE_DELAY_MS: u64 = 200;
        const RESTORE_ATTEMPTS: u32 = 4;

        if sta_ifname.trim().is_empty() || hotspot_con.trim().is_empty() {
            return Ok(());
        }
        let mut has_sta = false;
        let mut has_hs = false;
        for attempt in 1..=SETTLE_ATTEMPTS {
            let active =
                self.nm_active_connection_names_on_device(sta_ifname).await;
            has_sta = active.iter().any(|n| n.trim() == NM_CON_WIFI_STA);
            has_hs = active.iter().any(|n| n.trim() == hotspot_con.trim());
            if has_sta && has_hs {
                break;
            }
            if attempt < SETTLE_ATTEMPTS {
                tokio::time::sleep(Duration::from_millis(SETTLE_DELAY_MS))
                    .await;
            }
        }
        if has_sta && has_hs {
            steps.push(format!(
                "shared iface: {} + {} both active (concurrent STA+AP)",
                NM_CON_WIFI_STA,
                hotspot_con.trim()
            ));
            return Ok(());
        }
        if !has_hs {
            return Ok(());
        }
        if self
            .ethernet_intent_has_no_carrier(&intent.ethernet)
            .await?
        {
            steps.push(
                "shared iface: hotspot left up (no LAN carrier); STA cannot share the radio with AP here"
                    .to_string(),
            );
            return Ok(());
        }

        self.connection_down_lossy(hotspot_con).await;
        for attempt in 1..=RESTORE_ATTEMPTS {
            let out = self
                .nmcli_spawn_output(&["connection", "up", NM_CON_WIFI_STA])
                .await?;
            if out.status.success() {
                steps.push(format!(
                    "shared iface: restored {} on {} after hotspot (attempt {})",
                    NM_CON_WIFI_STA, sta_ifname, attempt
                ));
                return Ok(());
            }
            if attempt < RESTORE_ATTEMPTS {
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
        }
        steps.push(format!(
            "warning: could not restore {} on {} after releasing hotspot",
            NM_CON_WIFI_STA, sta_ifname
        ));
        Ok(())
    }

    async fn apply_intent(
        &self,
        intent: &NetworkIntent,
        sta_psk: Option<&str>,
        ap_psk: Option<&str>,
    ) -> Result<ApplyReport, PluginError> {
        static NM_APPLY_LOCK: OnceLock<tokio::sync::Mutex<()>> =
            OnceLock::new();
        let lock = NM_APPLY_LOCK.get_or_init(|| tokio::sync::Mutex::new(()));
        let _guard = lock.lock().await;

        let mut steps = Vec::new();
        let sta_ifname = self.effective_wifi_ifname(intent);
        let hotspot_ifname_intent = self.effective_hotspot_ifname(intent);
        let phy_supports_concurrent = sta_phy_supports_concurrent_sta_ap(
            sta_ifname.as_str(),
            &self.config.nmcli_path,
        )
        .await;
        let (resolved_ap_ifname, intent_hotspot_if_is_explicit) =
            resolve_ap_ifname(
                intent,
                sta_ifname.as_str(),
                hotspot_ifname_intent.as_str(),
                phy_supports_concurrent,
            );
        let hs_name = Self::hotspot_connection_name(intent);

        self.ensure_ethernet(intent, &mut steps).await?;

        match intent.wifi.role {
            WifiRole::Disabled => {
                self.connection_down_lossy(NM_CON_WIFI_STA).await;
                self.connection_down_lossy(&hs_name).await;
                if sta_ifname != resolved_ap_ifname {
                    let _ = ensure_ap_vif_absent(&resolved_ap_ifname).await;
                }
                steps.push("wifi role disabled; brought down STA and hotspot (best effort)".to_string());
            }
            WifiRole::Sta => {
                let same_iface = sta_ifname == resolved_ap_ifname;
                let concurrent_vif = !same_iface
                    && !intent_hotspot_if_is_explicit
                    && phy_supports_concurrent;

                self.connection_down_lossy(&hs_name).await;
                if concurrent_vif {
                    let _ = ensure_ap_vif_absent(&resolved_ap_ifname).await;
                }
                self.connection_down_lossy(NM_CON_WIFI_STA).await;
                steps.push(
                    "pre-STA: nmcli connection down STA profile (best effort before modify/up)"
                        .to_string(),
                );

                let sta_up_nonfatal = intent.fallback.hotspot_enabled;
                self.ensure_wifi_sta(
                    &sta_ifname,
                    &intent.wifi,
                    sta_psk,
                    sta_up_nonfatal,
                    &mut steps,
                )
                .await?;

                if intent.fallback.hotspot_enabled
                    && sta_ifname != resolved_ap_ifname
                    && !intent_hotspot_if_is_explicit
                    && ensure_ap_vif_present(&sta_ifname, &resolved_ap_ifname)
                        .await
                {
                    steps.push(format!(
                        "created AP vif {} on phy of {} (type __ap)",
                        resolved_ap_ifname, sta_ifname
                    ));
                }

                let mut wifi_for_ap = intent.wifi.clone();
                if sta_ifname != resolved_ap_ifname
                    && !intent_hotspot_if_is_explicit
                {
                    if let Some(link) = sta_link_info(&sta_ifname).await {
                        if link.connected {
                            if let (Some(ch), Some(band)) =
                                (link.channel, link.band.clone())
                            {
                                wifi_for_ap.ap_channel = ch;
                                wifi_for_ap.ap_band = band;
                                steps.push(format!(
                                    "AP follows STA: band={} channel={}",
                                    wifi_for_ap.ap_band, wifi_for_ap.ap_channel
                                ));
                            }
                        }
                    }
                }

                self.ensure_hotspot_profile(
                    &resolved_ap_ifname,
                    &wifi_for_ap,
                    ap_psk,
                    &intent.fallback,
                    &mut steps,
                )
                .await?;

                if intent.fallback.hotspot_enabled && !hs_name.trim().is_empty()
                {
                    let ok = self
                        .connection_up_hotspot_with_retries(
                            hs_name.as_str(),
                            &mut steps,
                        )
                        .await;
                    let recovered = if !ok {
                        self.try_critical_open_hotspot_recovery(
                            intent,
                            hs_name.as_str(),
                            &mut steps,
                        )
                        .await?
                    } else {
                        false
                    };
                    if sta_ifname == resolved_ap_ifname {
                        self.restore_sta_after_hotspot_on_shared_radio(
                            intent,
                            sta_ifname.as_str(),
                            hs_name.as_str(),
                            &mut steps,
                        )
                        .await?;
                    } else {
                        steps.push(format!(
                            "intent: hotspot on {}, STA on {}",
                            resolved_ap_ifname, sta_ifname
                        ));
                    }
                    if !ok && !recovered {
                        steps.push(
                            "warning: hotspot did not activate after retries (and critical open recovery if applicable)"
                                .to_string(),
                        );
                    }
                }
            }
            WifiRole::Ap => {
                self.connection_down_lossy(NM_CON_WIFI_STA).await;
                self.ensure_wifi_ap(
                    &resolved_ap_ifname,
                    &intent.wifi,
                    ap_psk,
                    hs_name.as_str(),
                    &mut steps,
                )
                .await?;
            }
        }
        Ok(ApplyReport { ok: true, steps })
    }

    fn parse_request_json<T: for<'de> Deserialize<'de>>(
        req: &Request,
    ) -> Result<T, PluginError> {
        serde_json::from_slice::<T>(&req.payload).map_err(|e| {
            PluginError::Permanent(format!("invalid JSON payload: {e}"))
        })
    }

    fn response_json(
        req: &Request,
        value: serde_json::Value,
    ) -> Result<Response, PluginError> {
        let body = serde_json::to_vec(&value).map_err(|e| {
            PluginError::Permanent(format!(
                "response serialization failed: {e}"
            ))
        })?;
        Ok(Response::for_request(req, body))
    }

    fn with_observability(
        &self,
        req: &Request,
        mut value: serde_json::Value,
    ) -> serde_json::Value {
        if let serde_json::Value::Object(ref mut m) = value {
            m.insert(
                "observability".to_string(),
                json!({
                    "request_type": req.request_type,
                    "correlation_id": req.correlation_id,
                    "requests_handled": self.requests_handled,
                    "secret_encryption": self.config.secret_key.is_some(),
                    "secret_encryption_required": self.config.require_encrypted_secrets,
                }),
            );
        }
        value
    }
}

impl Default for NetworkNmPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for NetworkNmPlugin {
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
                        REQUEST_NETWORK_STATUS.to_string(),
                        REQUEST_NETWORK_SCAN.to_string(),
                        REQUEST_NETWORK_INTENT_GET.to_string(),
                        REQUEST_NETWORK_INTENT_SET.to_string(),
                        REQUEST_NETWORK_INTENT_APPLY.to_string(),
                        REQUEST_NETWORK_CAPTIVE_STATUS.to_string(),
                        REQUEST_NETWORK_CAPTIVE_START.to_string(),
                        REQUEST_NETWORK_CAPTIVE_SUBMIT.to_string(),
                        REQUEST_NETWORK_CAPTIVE_COMPLETE.to_string(),
                    ],
                    accepts_custody: false,
                    flags: Default::default(),
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
            self.config = PluginConfig::from_toml_table(&ctx.config)?;
            self.state_dir = Some(ctx.state_dir.clone());
            self.loaded = true;
            self.scan_cache.clear();
            tracing::info!(
                plugin = PLUGIN_NAME,
                nmcli = %self.config.nmcli_path,
                wifi_iface = %self.config.default_wifi_iface,
                nmcli_timeout_ms = self.config.nmcli_timeout_ms,
                curl_timeout_ms = self.config.curl_timeout_ms,
                scan_cache_ttl_ms = self.config.scan_cache_ttl_ms,
                secret_encryption = self.config.secret_key.is_some(),
                secret_encryption_required = self.config.require_encrypted_secrets,
                "network plugin loaded"
            );
            if self.config.require_encrypted_secrets
                && self.config.secret_key.is_none()
            {
                tracing::error!(
                    plugin = PLUGIN_NAME,
                    env = ENV_SECRET_KEY,
                    "encrypted secrets required but no key is configured"
                );
            }
            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            self.loaded = false;
            self.state_dir = None;
            self.config = PluginConfig::defaults();
            self.scan_cache.clear();
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            if self.loaded {
                HealthReport::healthy()
            } else {
                HealthReport::unhealthy("network plugin not loaded")
            }
        }
    }
}

impl Respondent for NetworkNmPlugin {
    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "network plugin not loaded".to_string(),
                ));
            }
            if req.is_past_deadline() {
                return Err(PluginError::Transient(
                    "request deadline already expired".to_string(),
                ));
            }
            self.requests_handled += 1;

            match req.request_type.as_str() {
                REQUEST_NETWORK_STATUS => {
                    let devices_out = self.nm_device_table().await;
                    let (devices, devices_error) = match devices_out {
                        Ok(v) => (v, Option::<String>::None),
                        Err(e) => (Vec::new(), Some(format!("{e}"))),
                    };
                    let general_out =
                        self.nmcli_output(&["general", "status"]).await;
                    let (general, general_error) = match general_out {
                        Ok(v) => {
                            (Some(v.trim().to_string()), Option::<String>::None)
                        }
                        Err(e) => (None, Some(format!("{e}"))),
                    };
                    let scan_if = self.config.default_wifi_iface.clone();
                    let wifi_scan_error = self
                        .wifi_scan(Some(scan_if.as_str()))
                        .await
                        .err()
                        .map(|e| format!("{e}"));
                    let degraded = devices_error.is_some()
                        || general_error.is_some()
                        || wifi_scan_error.is_some();
                    Self::response_json(
                        req,
                        self.with_observability(
                            req,
                            json!({
                                "v": 1,
                                "status": "ok",
                                "degraded": degraded,
                                "nmcli_path": self.config.nmcli_path,
                                "general_status": general,
                                "devices": devices,
                                "scan_ifname": scan_if,
                                "wifi_scan_error": wifi_scan_error,
                                "domain_health": {
                                    "device_table": {
                                        "ok": devices_error.is_none(),
                                        "error": devices_error,
                                    },
                                    "general_status": {
                                        "ok": general_error.is_none(),
                                        "error": general_error,
                                    },
                                    "wifi_scan": {
                                        "ok": wifi_scan_error.is_none(),
                                        "error": wifi_scan_error,
                                    },
                                }
                            }),
                        ),
                    )
                }
                REQUEST_NETWORK_SCAN => {
                    let scan_req = if req.payload.is_empty() {
                        ScanRequest {
                            ifname: None,
                            refresh: false,
                        }
                    } else {
                        Self::parse_request_json::<ScanRequest>(req)?
                    };
                    let ifname_owned = scan_req.ifname.unwrap_or_else(|| {
                        self.config.default_wifi_iface.clone()
                    });
                    let ifname_trimmed = ifname_owned.trim().to_string();
                    let ifname = if ifname_trimmed.is_empty() {
                        None
                    } else {
                        Some(ifname_trimmed.as_str())
                    };
                    let scan_cache_key =
                        ifname.unwrap_or_default().trim().to_string();
                    let scan_result = self
                        .wifi_scan_with_cache(ifname, scan_req.refresh)
                        .await;
                    let (rows, candidates, cache_hit, cache_stale, scan_error) =
                        match scan_result {
                            Ok((r, c, hit)) => (r, c, hit, false, None),
                            Err(e) => {
                                if let Some(cached) =
                                    self.scan_cache.get(&scan_cache_key)
                                {
                                    (
                                        cached.available.clone(),
                                        cached.candidates.clone(),
                                        true,
                                        true,
                                        Some(format!("{e}")),
                                    )
                                } else {
                                    return Err(e);
                                }
                            }
                        };
                    Self::response_json(
                        req,
                        self.with_observability(
                            req,
                            json!({
                                "v": 1,
                                "status": "ok",
                                "available": rows,
                                "candidates": candidates,
                                "cache": {
                                    "hit": cache_hit,
                                    "stale": cache_stale,
                                    "refresh_requested": scan_req.refresh,
                                },
                                "scan_error": scan_error,
                            }),
                        ),
                    )
                }
                REQUEST_NETWORK_INTENT_GET => {
                    let intent = self.load_intent().await?;
                    let sta_psk = self
                        .read_optional_secret(&self.sta_psk_path()?)
                        .await?
                        .is_some();
                    let ap_psk = self
                        .read_optional_secret(&self.ap_psk_path()?)
                        .await?
                        .is_some();
                    Self::response_json(
                        req,
                        self.with_observability(
                            req,
                            json!({
                                "v": 1,
                                "status": "ok",
                                "intent": intent,
                                "sta_psk_configured": sta_psk,
                                "ap_psk_configured": ap_psk,
                            }),
                        ),
                    )
                }
                REQUEST_NETWORK_INTENT_SET => {
                    let body =
                        Self::parse_request_json::<IntentSetRequest>(req)?;
                    self.scan_cache.clear();
                    self.save_intent(&body.intent).await?;
                    self.write_optional_secret(
                        self.sta_psk_path()?,
                        body.sta_psk.as_deref(),
                    )
                    .await?;
                    self.write_optional_secret(
                        self.ap_psk_path()?,
                        body.ap_psk.as_deref(),
                    )
                    .await?;
                    let report = if body.apply {
                        Some(
                            self.apply_intent(
                                &body.intent,
                                body.sta_psk.as_deref(),
                                body.ap_psk.as_deref(),
                            )
                            .await?,
                        )
                    } else {
                        None
                    };
                    Self::response_json(
                        req,
                        self.with_observability(
                            req,
                            json!({
                                "v": 1,
                                "status": "ok",
                                "saved": true,
                                "apply": report,
                                "notices": report
                                    .as_ref()
                                    .map(build_apply_notices)
                                    .unwrap_or_default(),
                            }),
                        ),
                    )
                }
                REQUEST_NETWORK_INTENT_APPLY => {
                    let payload = if req.payload.is_empty() {
                        IntentApplyRequest { intent: None }
                    } else {
                        Self::parse_request_json::<IntentApplyRequest>(req)?
                    };
                    let intent = match payload.intent {
                        Some(v) => v,
                        None => self.load_intent().await?,
                    };
                    let sta_psk = self
                        .read_optional_secret(&self.sta_psk_path()?)
                        .await?;
                    let ap_psk =
                        self.read_optional_secret(&self.ap_psk_path()?).await?;
                    let report = self
                        .apply_intent(
                            &intent,
                            sta_psk.as_deref(),
                            ap_psk.as_deref(),
                        )
                        .await?;
                    self.scan_cache.clear();
                    Self::response_json(
                        req,
                        self.with_observability(
                            req,
                            json!({
                                "v": 1,
                                "status": "ok",
                                "apply": report,
                                "notices": build_apply_notices(&report),
                            }),
                        ),
                    )
                }
                REQUEST_NETWORK_CAPTIVE_STATUS => {
                    let body = if req.payload.is_empty() {
                        CaptiveStatusRequest {
                            probe: true,
                            url: None,
                        }
                    } else {
                        Self::parse_request_json::<CaptiveStatusRequest>(req)?
                    };
                    let mut state = self.load_captive_state().await?;
                    let connectivity = self.nm_connectivity().await;
                    if body.probe {
                        match self.captive_detect(body.url.as_deref()).await {
                            Ok(v) => {
                                state = v;
                                self.save_captive_state(&state).await?;
                            }
                            Err(e) => {
                                state.phase = CaptivePhase::Failed;
                                state.last_error = Some(format!("{e}"));
                            }
                        }
                    }
                    Self::response_json(req, self.with_observability(req, json!({
                        "v": 1,
                        "status": "ok",
                        "connectivity": connectivity,
                        "captive": state,
                        "notices": build_captive_notices(
                            &state,
                            connectivity.as_deref(),
                        ),
                        "actions": build_captive_actions(&state),
                        "reliability": {
                            "credential_policy": self.config.captive.credential_policy,
                            "retry_budget": self.config.captive.retry_budget,
                            "replay_window_sec": self.config.captive.replay_window_sec,
                        }
                    })))
                }
                REQUEST_NETWORK_CAPTIVE_START => {
                    let body = if req.payload.is_empty() {
                        CaptiveStartRequest { url: None }
                    } else {
                        Self::parse_request_json::<CaptiveStartRequest>(req)?
                    };
                    let mut state =
                        self.captive_detect(body.url.as_deref()).await?;
                    if matches!(state.phase, CaptivePhase::ProbeDetected) {
                        state.phase = CaptivePhase::AwaitingCredentials;
                    }
                    self.save_captive_state(&state).await?;
                    Self::response_json(req, self.with_observability(req, json!({
                        "v": 1,
                        "status": "ok",
                        "captive": state,
                        "notices": build_captive_notices(&state, None),
                        "actions": build_captive_actions(&state),
                        "reliability": {
                            "credential_policy": self.config.captive.credential_policy,
                            "retry_budget": self.config.captive.retry_budget,
                            "replay_window_sec": self.config.captive.replay_window_sec,
                        }
                    })))
                }
                REQUEST_NETWORK_CAPTIVE_SUBMIT => {
                    let body =
                        Self::parse_request_json::<CaptiveSubmitRequest>(req)?;
                    let state = self.captive_submit(&body).await?;
                    self.save_captive_state(&state).await?;
                    Self::response_json(req, self.with_observability(req, json!({
                        "v": 1,
                        "status": "ok",
                        "captive": state,
                        "notices": build_captive_notices(&state, None),
                        "actions": build_captive_actions(&state),
                        "reliability": {
                            "credential_policy": self.config.captive.credential_policy,
                            "retry_budget": self.config.captive.retry_budget,
                            "replay_window_sec": self.config.captive.replay_window_sec,
                        }
                    })))
                }
                REQUEST_NETWORK_CAPTIVE_COMPLETE => {
                    let body = if req.payload.is_empty() {
                        CaptiveCompleteRequest {
                            success: Some(true),
                        }
                    } else {
                        Self::parse_request_json::<CaptiveCompleteRequest>(req)?
                    };
                    let mut state = self.load_captive_state().await?;
                    if body.success.unwrap_or(true) {
                        state.phase = CaptivePhase::Authenticated;
                        state.last_error = None;
                        state.requires_user_confirmation = false;
                    } else {
                        state.phase = CaptivePhase::Failed;
                        state.requires_user_confirmation = true;
                        if state.last_error.is_none() {
                            state.last_error = Some(
                                "captive completion marked as failed"
                                    .to_string(),
                            );
                        }
                    }
                    self.save_captive_state(&state).await?;
                    Self::response_json(req, self.with_observability(req, json!({
                        "v": 1,
                        "status": "ok",
                        "captive": state,
                        "notices": build_captive_notices(&state, None),
                        "actions": build_captive_actions(&state),
                        "reliability": {
                            "credential_policy": self.config.captive.credential_policy,
                            "retry_budget": self.config.captive.retry_budget,
                            "replay_window_sec": self.config.captive.replay_window_sec,
                        }
                    })))
                }
                other => Err(PluginError::Permanent(format!(
                    "unknown request type: {:?}",
                    other
                ))),
            }
        }
    }
}

fn first_ethernet_device(devices: &[DeviceRow]) -> Option<String> {
    devices
        .iter()
        .find(|d| d.kind.eq_ignore_ascii_case("ethernet") && d.device != "lo")
        .map(|d| d.device.clone())
}

fn parse_http_probe_metrics(raw: &str) -> (Option<u16>, Option<String>) {
    let mut parts = raw.splitn(2, '|');
    let code = parts.next().and_then(|s| s.trim().parse::<u16>().ok());
    let url = parts
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    (code, url)
}

fn notice(level: &str, code: &str, message: String) -> serde_json::Value {
    json!({
        "level": level,
        "code": code,
        "message": message,
    })
}

fn build_apply_notices(report: &ApplyReport) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    if report.ok {
        out.push(notice(
            "success",
            "network_apply_ok",
            "Network settings applied".to_string(),
        ));
    } else {
        out.push(notice(
            "error",
            "network_apply_failed",
            report
                .steps
                .last()
                .cloned()
                .unwrap_or_else(|| "Network apply failed".to_string()),
        ));
    }
    for s in &report.steps {
        if s.to_ascii_lowercase().contains("warning:") {
            out.push(notice("warning", "network_apply_warning", s.clone()));
        }
        if s.to_ascii_lowercase().contains("critical:") {
            out.push(notice(
                "warning",
                "network_apply_critical_recovery",
                s.clone(),
            ));
        }
    }
    out
}

fn build_captive_notices(
    state: &CaptiveSessionState,
    connectivity: Option<&str>,
) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    if let Some(c) = connectivity {
        if c.eq_ignore_ascii_case("full") {
            out.push(notice(
                "success",
                "network_connectivity_full",
                "Network connectivity is full".to_string(),
            ));
        } else if c.eq_ignore_ascii_case("portal") {
            out.push(notice(
                "info",
                "network_connectivity_portal",
                "Captive portal detected".to_string(),
            ));
        }
    }
    match state.phase {
        CaptivePhase::Submitting => out.push(notice(
            "info",
            "captive_submitting",
            "Submitting captive credentials".to_string(),
        )),
        CaptivePhase::Authenticated => out.push(notice(
            "success",
            "captive_authenticated",
            "Captive authentication complete".to_string(),
        )),
        CaptivePhase::Failed => out.push(notice(
            "error",
            "captive_failed",
            state
                .last_error
                .clone()
                .unwrap_or_else(|| "Captive authentication failed".to_string()),
        )),
        CaptivePhase::AwaitingCredentials
            if state.requires_user_confirmation =>
        {
            out.push(notice(
                "warning",
                "captive_confirmation_required",
                "Manual confirmation required before credential replay"
                    .to_string(),
            ));
        }
        _ => {}
    }
    out
}

fn build_captive_actions(
    state: &CaptiveSessionState,
) -> Vec<serde_json::Value> {
    let mut actions = Vec::new();
    actions.push(json!({
        "id": "captive.start_probe",
        "label": "Probe captive connectivity",
        "request_type": REQUEST_NETWORK_CAPTIVE_START,
    }));
    if matches!(state.phase, CaptivePhase::AwaitingCredentials)
        && state.requires_user_confirmation
    {
        actions.push(json!({
            "id": "captive.confirm_replay",
            "label": "Confirm guarded credential replay",
            "request_type": REQUEST_NETWORK_CAPTIVE_SUBMIT,
            "payload_patch": { "confirm_replay": true },
        }));
    }
    if matches!(state.phase, CaptivePhase::Failed) {
        actions.push(json!({
            "id": "captive.mark_complete_failed",
            "label": "Mark captive flow failed",
            "request_type": REQUEST_NETWORK_CAPTIVE_COMPLETE,
            "payload_patch": { "success": false },
        }));
    }
    actions
}

fn unix_epoch_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn captive_submit_fingerprint(
    url: &str,
    method: &str,
    form: &std::collections::BTreeMap<String, String>,
) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    url.trim().hash(&mut hasher);
    method.trim().to_ascii_uppercase().hash(&mut hasher);
    for (k, v) in form {
        k.hash(&mut hasher);
        "=".hash(&mut hasher);
        v.hash(&mut hasher);
        "&".hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

fn sysfs_ethernet_no_carrier(ifname: &str) -> bool {
    let path = format!("/sys/class/net/{}/carrier", ifname.trim());
    match std::fs::read_to_string(path) {
        Ok(s) => s.trim() == "0",
        Err(_) => true,
    }
}

fn normalize_nm_ap_band(raw: &str) -> Option<&'static str> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    if t.eq_ignore_ascii_case("bg") {
        return Some("bg");
    }
    if t.eq_ignore_ascii_case("a") {
        return Some("a");
    }
    if t.eq_ignore_ascii_case("6ghz") {
        return Some("6GHz");
    }
    None
}

fn normalize_band_pref(raw: &str) -> Option<BandClass> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    if t.eq_ignore_ascii_case("2.4")
        || t.eq_ignore_ascii_case("2.4ghz")
        || t.eq_ignore_ascii_case("2g")
        || t.eq_ignore_ascii_case("bg")
    {
        return Some(BandClass::Ghz2_4);
    }
    if t.eq_ignore_ascii_case("5")
        || t.eq_ignore_ascii_case("5ghz")
        || t.eq_ignore_ascii_case("5g")
        || t.eq_ignore_ascii_case("a")
    {
        return Some(BandClass::Ghz5);
    }
    if t.eq_ignore_ascii_case("6")
        || t.eq_ignore_ascii_case("6ghz")
        || t.eq_ignore_ascii_case("6g")
    {
        return Some(BandClass::Ghz6);
    }
    None
}

fn band_from_freq(freq_mhz: Option<u32>) -> BandClass {
    match freq_mhz {
        Some(v) if (2412..=2484).contains(&v) => BandClass::Ghz2_4,
        Some(v) if (5000..=5895).contains(&v) => BandClass::Ghz5,
        Some(v) if (5925..=7125).contains(&v) => BandClass::Ghz6,
        _ => BandClass::Unknown,
    }
}

fn band_weight_stable(band: &BandClass) -> i32 {
    match band {
        BandClass::Ghz2_4 => 120,
        BandClass::Ghz5 => 100,
        BandClass::Ghz6 => 80,
        BandClass::Unknown => 0,
    }
}

fn band_weight_performance(band: &BandClass) -> i32 {
    match band {
        BandClass::Ghz2_4 => 100,
        BandClass::Ghz5 => 220,
        BandClass::Ghz6 => 260,
        BandClass::Unknown => 0,
    }
}

fn sta_candidate_score(
    c: &WifiStaCandidate,
    mode: &StaSelectionMode,
    preferred_band: Option<BandClass>,
) -> i32 {
    let signal = i32::from(c.signal_pct);
    let pref = preferred_band
        .as_ref()
        .filter(|b| **b == c.band)
        .map(|_| 2000)
        .unwrap_or(0);
    match mode {
        StaSelectionMode::Legacy => signal,
        StaSelectionMode::AutoStable => {
            (signal * 3) + band_weight_stable(&c.band)
        }
        StaSelectionMode::AutoPerformance => {
            (signal * 2) + band_weight_performance(&c.band)
        }
        StaSelectionMode::PreferBand => (signal * 2) + pref,
        StaSelectionMode::LockBssid => signal,
    }
}

fn push_nm_ap_channel(seq: &mut Vec<String>, wifi: &WifiIntent) {
    let ch = wifi.ap_channel;
    if ch == 0 {
        return;
    }
    let band = if let Some(b) = normalize_nm_ap_band(&wifi.ap_band) {
        b.to_string()
    } else if !wifi.ap_band.trim().is_empty() {
        return;
    } else {
        match ch {
            1..=14 => "bg".to_string(),
            36..=177 => "a".to_string(),
            _ => return,
        }
    };
    seq.push("802-11-wireless.band".into());
    seq.push(band);
    seq.push("802-11-wireless.channel".into());
    seq.push(ch.to_string());
}

#[derive(Debug, Clone)]
struct StaLinkInfo {
    connected: bool,
    channel: Option<u32>,
    band: Option<String>,
}

fn freq_to_channel_and_band(freq: u32) -> Option<(u32, String)> {
    if (2412..=2484).contains(&freq) {
        let ch = if freq == 2484 { 14 } else { (freq - 2407) / 5 };
        return Some((ch, "bg".to_string()));
    }
    if (5000..=5895).contains(&freq) {
        return Some(((freq - 5000) / 5, "a".to_string()));
    }
    if (5925..=7125).contains(&freq) {
        return Some(((freq - 5950) / 5, "6GHz".to_string()));
    }
    None
}

async fn sta_link_info(sta_ifname: &str) -> Option<StaLinkInfo> {
    let out = Command::new("iw")
        .args(["dev", sta_ifname, "link"])
        .output()
        .await
        .ok()?;
    let body = String::from_utf8_lossy(&out.stdout);
    let connected = body.contains("Connected to");
    let mut channel = None;
    let mut band = None;
    for line in body.lines() {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("freq:") {
            let freq = v.trim().parse::<u32>().ok();
            if let Some(f) = freq {
                if let Some((ch, b)) = freq_to_channel_and_band(f) {
                    channel = Some(ch);
                    band = Some(b);
                }
            }
        }
    }
    Some(StaLinkInfo {
        connected,
        channel,
        band,
    })
}

async fn wifi_iface_exists(ifname: &str) -> bool {
    tokio::fs::metadata(format!("/sys/class/net/{}", ifname.trim()))
        .await
        .is_ok()
}

async fn ensure_ap_vif_absent(ap_ifname: &str) -> bool {
    if ap_ifname.trim().is_empty() {
        return false;
    }
    if !wifi_iface_exists(ap_ifname).await {
        return true;
    }
    Command::new("iw")
        .args(["dev", ap_ifname, "del"])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

async fn ensure_ap_vif_present(sta_ifname: &str, ap_ifname: &str) -> bool {
    if sta_ifname.trim().is_empty() || ap_ifname.trim().is_empty() {
        return false;
    }
    if wifi_iface_exists(ap_ifname).await {
        return true;
    }
    Command::new("iw")
        .args([
            "dev",
            sta_ifname,
            "interface",
            "add",
            ap_ifname,
            "type",
            "__ap",
        ])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

async fn sta_phy_supports_concurrent_sta_ap(
    sta_ifname: &str,
    _nmcli_path: &str,
) -> bool {
    if std::env::var("EVO_NETWORK_ASSUME_CONCURRENT_STA_AP")
        .ok()
        .as_deref()
        == Some("1")
    {
        return true;
    }
    let out = Command::new("iw")
        .args(["dev", sta_ifname, "info"])
        .output()
        .await;
    let Ok(out) = out else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let info = String::from_utf8_lossy(&out.stdout);
    let mut phy_name: Option<String> = None;
    for line in info.lines() {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("wiphy") {
            let n = v.trim();
            if !n.is_empty() {
                phy_name = Some(format!("phy{}", n));
                break;
            }
        }
    }
    let Some(phy) = phy_name else {
        return false;
    };
    let out = Command::new("iw")
        .args(["phy", &phy, "info"])
        .output()
        .await;
    let Ok(out) = out else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let txt = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
    txt.contains("valid interface combinations")
        && txt.contains("managed")
        && txt.contains(" ap")
}

fn resolve_ap_ifname(
    intent: &NetworkIntent,
    sta_ifname: &str,
    hotspot_ifname_intent: &str,
    phy_supports_concurrent: bool,
) -> (String, bool) {
    let intent_hotspot_if_is_explicit =
        !intent.fallback.hotspot_ifname.trim().is_empty()
            && hotspot_ifname_intent != sta_ifname;
    let resolved = if intent_hotspot_if_is_explicit {
        hotspot_ifname_intent.to_string()
    } else if phy_supports_concurrent {
        std::env::var("VOLUMIO_EVO_AP_IFNAME")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "ap0".to_string())
    } else {
        sta_ifname.to_string()
    };
    (resolved, intent_hotspot_if_is_explicit)
}

fn security_label(nm: &str) -> &'static str {
    let s = nm.to_ascii_lowercase();
    if s.is_empty() || s.contains("--") {
        "open"
    } else if s.contains("wpa3") {
        "wpa3"
    } else if s.contains("wpa2") || s.contains("wpa") {
        "wpa2"
    } else if s.contains("wep") {
        "wep"
    } else {
        "open"
    }
}

fn signal_bars_from_pct(pct: Option<u8>) -> u8 {
    let Some(p) = pct else {
        return 0;
    };
    if p >= 80 {
        5
    } else if p >= 60 {
        4
    } else if p >= 40 {
        3
    } else if p >= 20 {
        2
    } else if p > 0 {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_plugin_sdk::contract::{HealthStatus, Request};
    use serde_json::Value;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn manifest_parses() {
        let m = manifest();
        assert_eq!(m.plugin.name, PLUGIN_NAME);
        let cap = m
            .capabilities
            .respondent
            .as_ref()
            .expect("manifest must have respondent capabilities");
        assert!(cap
            .request_types
            .iter()
            .any(|v| v == REQUEST_NETWORK_STATUS));
        assert!(cap
            .request_types
            .iter()
            .any(|v| v == REQUEST_NETWORK_INTENT_SET));
        assert!(cap
            .request_types
            .iter()
            .any(|v| v == REQUEST_NETWORK_CAPTIVE_STATUS));
        assert!(cap
            .request_types
            .iter()
            .any(|v| v == REQUEST_NETWORK_CAPTIVE_SUBMIT));
    }

    #[tokio::test]
    async fn describe_matches_manifest() {
        let p = NetworkNmPlugin::new();
        let d = p.describe().await;
        let m = manifest();
        assert_eq!(d.identity.name, m.plugin.name);
        assert_eq!(d.identity.version, m.plugin.version);
        assert!(!d.runtime_capabilities.accepts_custody);
    }

    #[tokio::test]
    async fn health_unhealthy_before_load() {
        let p = NetworkNmPlugin::new();
        assert!(matches!(
            p.health_check().await.status,
            HealthStatus::Unhealthy
        ));
    }

    #[test]
    fn signal_bar_mapping() {
        assert_eq!(signal_bars_from_pct(None), 0);
        assert_eq!(signal_bars_from_pct(Some(1)), 1);
        assert_eq!(signal_bars_from_pct(Some(25)), 2);
        assert_eq!(signal_bars_from_pct(Some(45)), 3);
        assert_eq!(signal_bars_from_pct(Some(65)), 4);
        assert_eq!(signal_bars_from_pct(Some(85)), 5);
    }

    #[test]
    fn security_label_mapping() {
        assert_eq!(security_label(""), "open");
        assert_eq!(security_label("WPA2"), "wpa2");
        assert_eq!(security_label("WPA3 SAE"), "wpa3");
        assert_eq!(security_label("WEP"), "wep");
    }

    #[test]
    fn intent_toml_compat_parses_manual_and_ifname_alias() {
        let src = r#"
version = 1

[ethernet]
enabled = true
ifname = "enp1s0"
ipv4_mode = "manual"
ipv4_address = "10.0.0.10/24"
ipv4_gateway = "10.0.0.1"
ipv4_dns = ["1.1.1.1"]

[wifi]
ifname = "wlan1"
role = "sta"
sta_ssid = "HotelWiFi"
sta_open = true
sta_ipv4_mode = "dhcp"
ap_ssid = "Volumio-Guest"
ap_channel = 36
ap_band = "a"

[fallback]
hotspot_enabled = true
hotspot_connection_name = "volumio-hotspot"
hotspot_ifname = "wlan0"
hotspot_fallback = true
"#;
        let intent: NetworkIntent =
            toml::from_str(src).expect("intent toml parses");
        assert_eq!(intent.ethernet.device, "enp1s0");
        assert_eq!(intent.ethernet.ipv4_mode, Ipv4Mode::Static);
        assert_eq!(intent.wifi.ifname, "wlan1");
        assert_eq!(intent.fallback.hotspot_ifname, "wlan0");
    }

    #[test]
    fn resolve_ap_ifname_prefers_explicit_hotspot_iface() {
        let mut intent = NetworkIntent::default();
        intent.fallback.hotspot_ifname = "wlan0".to_string();
        let (resolved, explicit) =
            resolve_ap_ifname(&intent, "wlan1", "wlan0", true);
        assert_eq!(resolved, "wlan0");
        assert!(explicit);
    }

    #[test]
    fn resolve_ap_ifname_uses_ap0_when_concurrent_supported() {
        let intent = NetworkIntent::default();
        let (resolved, explicit) =
            resolve_ap_ifname(&intent, "wlan1", "wlan1", true);
        assert_eq!(resolved, "ap0");
        assert!(!explicit);
    }

    #[test]
    fn resolve_ap_ifname_falls_back_to_sta_when_not_concurrent() {
        let intent = NetworkIntent::default();
        let (resolved, explicit) =
            resolve_ap_ifname(&intent, "wlan1", "wlan1", false);
        assert_eq!(resolved, "wlan1");
        assert!(!explicit);
    }

    #[test]
    fn ap_channel_mapping_respects_explicit_band_and_defaults() {
        let wifi = WifiIntent {
            ap_channel: 6,
            ap_band: String::new(),
            ..WifiIntent::default()
        };
        let mut seq = Vec::new();
        push_nm_ap_channel(&mut seq, &wifi);
        assert_eq!(
            seq,
            vec!["802-11-wireless.band", "bg", "802-11-wireless.channel", "6",]
        );

        let wifi6 = WifiIntent {
            ap_channel: 5,
            ap_band: "6GHz".to_string(),
            ..WifiIntent::default()
        };
        let mut seq6 = Vec::new();
        push_nm_ap_channel(&mut seq6, &wifi6);
        assert_eq!(
            seq6,
            vec![
                "802-11-wireless.band",
                "6GHz",
                "802-11-wireless.channel",
                "5",
            ]
        );
    }

    #[test]
    fn band_from_freq_maps_common_ranges() {
        assert_eq!(band_from_freq(Some(2412)), BandClass::Ghz2_4);
        assert_eq!(band_from_freq(Some(5180)), BandClass::Ghz5);
        assert_eq!(band_from_freq(Some(5975)), BandClass::Ghz6);
        assert_eq!(band_from_freq(Some(1000)), BandClass::Unknown);
    }

    #[test]
    fn select_sta_candidate_honors_lock_bssid() {
        let plugin = NetworkNmPlugin::new();
        let wifi = WifiIntent {
            sta_ssid: "HotelWiFi".to_string(),
            sta_lock_bssid: "AA:BB:CC:DD:EE:FF".to_string(),
            ..WifiIntent::default()
        };
        let candidates = vec![
            WifiStaCandidate {
                bssid: "11:22:33:44:55:66".to_string(),
                ssid: "HotelWiFi".to_string(),
                signal_pct: 90,
                freq_mhz: Some(5180),
                band: BandClass::Ghz5,
                active: false,
            },
            WifiStaCandidate {
                bssid: "AA:BB:CC:DD:EE:FF".to_string(),
                ssid: "HotelWiFi".to_string(),
                signal_pct: 40,
                freq_mhz: Some(2412),
                band: BandClass::Ghz2_4,
                active: false,
            },
        ];
        let picked = plugin
            .select_sta_candidate(&wifi, &candidates)
            .expect("candidate selected");
        assert_eq!(picked.bssid, "AA:BB:CC:DD:EE:FF");
    }

    #[test]
    fn select_sta_candidate_prefers_performance_band() {
        let plugin = NetworkNmPlugin::new();
        let wifi = WifiIntent {
            sta_ssid: "HotelWiFi".to_string(),
            sta_selection_mode: StaSelectionMode::AutoPerformance,
            ..WifiIntent::default()
        };
        let candidates = vec![
            WifiStaCandidate {
                bssid: "11:22:33:44:55:66".to_string(),
                ssid: "HotelWiFi".to_string(),
                signal_pct: 80,
                freq_mhz: Some(2412),
                band: BandClass::Ghz2_4,
                active: false,
            },
            WifiStaCandidate {
                bssid: "AA:BB:CC:DD:EE:FF".to_string(),
                ssid: "HotelWiFi".to_string(),
                signal_pct: 65,
                freq_mhz: Some(5180),
                band: BandClass::Ghz5,
                active: false,
            },
        ];
        let picked = plugin
            .select_sta_candidate(&wifi, &candidates)
            .expect("candidate selected");
        assert_eq!(picked.bssid, "AA:BB:CC:DD:EE:FF");
    }

    #[test]
    fn parse_http_probe_metrics_parses_code_and_url() {
        let (code, url) =
            parse_http_probe_metrics("302|http://portal.example/login");
        assert_eq!(code, Some(302));
        assert_eq!(url.as_deref(), Some("http://portal.example/login"));
    }

    #[test]
    fn parse_http_probe_metrics_handles_code_only() {
        let (code, url) = parse_http_probe_metrics("204|");
        assert_eq!(code, Some(204));
        assert!(url.is_none());
    }

    #[test]
    fn parse_http_probe_metrics_handles_malformed_payload() {
        let (code, url) = parse_http_probe_metrics("not-a-code");
        assert_eq!(code, None);
        assert!(url.is_none());
    }

    #[test]
    fn captive_submit_payload_preserves_mixed_case_values() {
        let raw = serde_json::json!({
            "url": "http://portal/login",
            "method": "POST",
            "form": {
                "roomNumber": "A10",
                "guestName": "McAllen",
                "accessCode": "AbC123xY"
            }
        });
        let parsed: CaptiveSubmitRequest =
            serde_json::from_value(raw).expect("payload parses");
        assert_eq!(
            parsed.form.get("roomNumber").map(String::as_str),
            Some("A10")
        );
        assert_eq!(
            parsed.form.get("guestName").map(String::as_str),
            Some("McAllen")
        );
        assert_eq!(
            parsed.form.get("accessCode").map(String::as_str),
            Some("AbC123xY")
        );
    }

    #[test]
    fn captive_submit_payload_parses_confirm_replay() {
        let raw = serde_json::json!({
            "url": "http://portal/login",
            "form": { "ticket": "ABC123" },
            "confirm_replay": true
        });
        let parsed: CaptiveSubmitRequest =
            serde_json::from_value(raw).expect("payload parses");
        assert!(parsed.confirm_replay);
    }

    #[test]
    fn captive_submit_fingerprint_is_deterministic_and_sensitive_to_values() {
        let mut form_a = std::collections::BTreeMap::new();
        form_a.insert("room".to_string(), "A10".to_string());
        form_a.insert("code".to_string(), "AbC123".to_string());

        let mut form_b = std::collections::BTreeMap::new();
        form_b.insert("room".to_string(), "A10".to_string());
        form_b.insert("code".to_string(), "AbC123".to_string());

        let mut form_c = std::collections::BTreeMap::new();
        form_c.insert("room".to_string(), "A10".to_string());
        form_c.insert("code".to_string(), "AbC124".to_string());

        let f1 =
            captive_submit_fingerprint("http://portal/login", "POST", &form_a);
        let f2 =
            captive_submit_fingerprint("http://portal/login", "POST", &form_b);
        let f3 =
            captive_submit_fingerprint("http://portal/login", "POST", &form_c);

        assert_eq!(f1, f2);
        assert_ne!(f1, f3);
    }

    #[test]
    fn plugin_config_parses_captive_reliability_policy() {
        let t: toml::Table = toml::from_str(
            r#"
nmcli_path = "/usr/bin/nmcli"
wifi_iface = "wlan1"
nmcli_timeout_ms = 12000
curl_timeout_ms = 45000
scan_cache_ttl_ms = 6000

[captive]
credential_policy = "single_use_ticket"
retry_budget = 1
replay_window_sec = 120

[secrets]
require_encrypted = true
"#,
        )
        .expect("toml parses");
        let cfg =
            PluginConfig::from_toml_table(&t).expect("plugin config parses");
        assert_eq!(
            cfg.captive.credential_policy,
            CaptiveCredentialPolicy::SingleUseTicket
        );
        assert_eq!(cfg.captive.retry_budget, 1);
        assert_eq!(cfg.captive.replay_window_sec, 120);
        assert!(cfg.require_encrypted_secrets);
        assert_eq!(cfg.nmcli_timeout_ms, 12000);
        assert_eq!(cfg.curl_timeout_ms, 45000);
        assert_eq!(cfg.scan_cache_ttl_ms, 6000);
    }

    #[test]
    fn build_apply_notices_includes_success_or_error() {
        let ok = ApplyReport {
            ok: true,
            steps: vec!["step a".to_string()],
        };
        let err = ApplyReport {
            ok: false,
            steps: vec!["error: failed to apply".to_string()],
        };
        let ok_notices = build_apply_notices(&ok);
        let err_notices = build_apply_notices(&err);
        assert!(ok_notices.iter().any(|n| n["code"] == "network_apply_ok"));
        assert!(err_notices
            .iter()
            .any(|n| n["code"] == "network_apply_failed"));
    }

    #[test]
    fn build_captive_notices_marks_confirmation_required() {
        let s = CaptiveSessionState {
            phase: CaptivePhase::AwaitingCredentials,
            requires_user_confirmation: true,
            ..Default::default()
        };
        let notices = build_captive_notices(&s, Some("portal"));
        assert!(notices
            .iter()
            .any(|n| n["code"] == "network_connectivity_portal"));
        assert!(notices
            .iter()
            .any(|n| n["code"] == "captive_confirmation_required"));
    }

    #[test]
    fn build_captive_actions_includes_guarded_replay_action() {
        let s = CaptiveSessionState {
            phase: CaptivePhase::AwaitingCredentials,
            requires_user_confirmation: true,
            ..Default::default()
        };
        let actions = build_captive_actions(&s);
        assert!(actions.iter().any(|a| a["id"] == "captive.confirm_replay"));
    }

    #[test]
    fn lkg_naming_matches_steward_style() {
        let p = std::path::Path::new(
            "/var/lib/evo/plugins/x/state/network-intent.toml",
        );
        let lkg = lkg_shadow_path(p);
        let tmp = tmp_shadow_path(p);
        assert_eq!(
            lkg.to_string_lossy(),
            "/var/lib/evo/plugins/x/state/network-intent.lkg.toml"
        );
        assert_eq!(
            tmp.to_string_lossy(),
            "/var/lib/evo/plugins/x/state/network-intent.toml.tmp"
        );
    }

    #[tokio::test]
    async fn load_intent_falls_back_to_lkg_shadow_on_primary_parse_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut p = NetworkNmPlugin::new();
        p.state_dir = Some(dir.path().to_path_buf());
        let primary = p.intent_path().expect("intent path");
        let lkg = lkg_shadow_path(&primary);

        tokio::fs::write(&primary, "not-valid-toml")
            .await
            .expect("write primary");
        tokio::fs::write(
            &lkg,
            r#"
version = 1
[ethernet]
enabled = true
[wifi]
ifname = "wlan1"
role = "sta"
sta_ssid = "HotelWiFi"
[fallback]
hotspot_enabled = true
"#,
        )
        .await
        .expect("write lkg");

        let intent = p.load_intent().await.expect("load intent");
        assert_eq!(intent.wifi.ifname, "wlan1");
        assert_eq!(intent.wifi.sta_ssid, "HotelWiFi");
    }

    #[test]
    fn parse_intent_state_toml_accepts_legacy_and_envelope() {
        let legacy = r#"
version = 1
[wifi]
ifname = "wlan1"
role = "sta"
sta_ssid = "HotelWiFi"
"#;
        let parsed_legacy =
            parse_intent_state_toml(legacy).expect("legacy parses");
        assert_eq!(parsed_legacy.wifi.ifname, "wlan1");
        assert_eq!(parsed_legacy.wifi.sta_ssid, "HotelWiFi");

        let envelope = r#"
schema_version = 1
[intent]
version = 1
[intent.wifi]
ifname = "wlan0"
role = "sta"
sta_ssid = "OfficeWiFi"
"#;
        let parsed_envelope =
            parse_intent_state_toml(envelope).expect("envelope parses");
        assert_eq!(parsed_envelope.wifi.ifname, "wlan0");
        assert_eq!(parsed_envelope.wifi.sta_ssid, "OfficeWiFi");
    }

    #[test]
    fn parse_intent_state_toml_rejects_unknown_schema() {
        let src = r#"
schema_version = 99
[intent]
version = 1
"#;
        let err = parse_intent_state_toml(src).expect_err("must fail");
        assert!(err.contains("unsupported intent state schema_version"));
    }

    #[test]
    fn parse_captive_state_json_accepts_legacy_and_envelope() {
        let legacy = r#"{"phase":"authenticated","submit_attempts":1}"#;
        let parsed_legacy =
            parse_captive_state_json(legacy).expect("legacy parses");
        assert!(matches!(parsed_legacy.phase, CaptivePhase::Authenticated));
        assert_eq!(parsed_legacy.submit_attempts, 1);

        let envelope = r#"{
  "schema_version": 1,
  "state": {
    "phase": "awaiting_credentials",
    "requires_user_confirmation": true
  }
}"#;
        let parsed_envelope =
            parse_captive_state_json(envelope).expect("envelope parses");
        assert!(matches!(
            parsed_envelope.phase,
            CaptivePhase::AwaitingCredentials
        ));
        assert!(parsed_envelope.requires_user_confirmation);
    }

    #[tokio::test]
    async fn save_intent_and_captive_state_write_schema_envelopes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut p = NetworkNmPlugin::new();
        p.state_dir = Some(dir.path().to_path_buf());

        let intent = NetworkIntent::default();
        p.save_intent(&intent).await.expect("save intent");
        let intent_raw =
            tokio::fs::read_to_string(p.intent_path().expect("path"))
                .await
                .expect("read intent");
        assert!(intent_raw.contains("schema_version = 1"));
        assert!(intent_raw.contains("[intent]"));

        let captive = CaptiveSessionState {
            phase: CaptivePhase::Authenticated,
            ..Default::default()
        };
        p.save_captive_state(&captive).await.expect("save captive");
        let captive_raw =
            tokio::fs::read_to_string(p.captive_state_path().expect("path"))
                .await
                .expect("read captive");
        assert!(captive_raw.contains("\"schema_version\": 1"));
        assert!(captive_raw.contains("\"state\""));
    }

    #[test]
    fn secret_crypto_roundtrip() {
        let key = derive_secret_key("test-key-material");
        let env = encrypt_secret_value("AbC123xY", &key).expect("encrypt");
        let plain = decrypt_secret_value(&env, &key).expect("decrypt");
        assert_eq!(plain, "AbC123xY");
    }

    #[tokio::test]
    async fn write_secret_encrypts_when_key_configured() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut p = NetworkNmPlugin::new();
        p.state_dir = Some(dir.path().to_path_buf());
        p.config.secret_key = Some(derive_secret_key("test-key"));
        p.write_optional_secret(
            p.sta_psk_path().expect("path"),
            Some("SeCrEt9"),
        )
        .await
        .expect("write");

        let raw = tokio::fs::read_to_string(p.sta_psk_path().expect("path"))
            .await
            .expect("read");
        assert!(raw.contains("\"schema_version\": 1"));
        let decoded = p
            .read_optional_secret(&p.sta_psk_path().expect("path"))
            .await
            .expect("decode");
        assert_eq!(decoded.as_deref(), Some("SeCrEt9"));
    }

    #[tokio::test]
    async fn read_plaintext_secret_rejected_when_encryption_required() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut p = NetworkNmPlugin::new();
        p.state_dir = Some(dir.path().to_path_buf());
        p.config.require_encrypted_secrets = true;
        let path = p.sta_psk_path().expect("path");
        tokio::fs::write(&path, "plainsecret\n")
            .await
            .expect("write");

        let err = p
            .read_optional_secret(&path)
            .await
            .expect_err("must reject plaintext");
        assert!(format!("{err}").contains("plaintext secret"));
    }

    fn req(
        request_type: &str,
        payload: serde_json::Value,
        correlation_id: u64,
    ) -> Request {
        Request {
            request_type: request_type.to_string(),
            payload: serde_json::to_vec(&payload).expect("payload"),
            correlation_id,
            deadline: None,
            instance_id: None,
        }
    }

    #[tokio::test]
    async fn request_flow_intent_set_and_get_roundtrip() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut p = NetworkNmPlugin::new();
        p.loaded = true;
        p.state_dir = Some(dir.path().to_path_buf());
        p.config = PluginConfig::defaults();
        p.config.secret_key = Some(derive_secret_key("integration-key"));

        let set_req = req(
            REQUEST_NETWORK_INTENT_SET,
            serde_json::json!({
                "intent": {
                    "version": 1,
                    "ethernet": { "enabled": false },
                    "wifi": { "role": "sta", "ifname": "wlan0", "sta_ssid": "HotelWiFi", "sta_open": false },
                    "fallback": { "hotspot_enabled": false }
                },
                "sta_psk": "AbC123xY",
                "ap_psk": "HotspotPass9",
                "apply": false
            }),
            1001,
        );
        let set_out = p.handle_request(&set_req).await.expect("intent.set");
        let set_v: Value =
            serde_json::from_slice(&set_out.payload).expect("json");
        assert_eq!(set_v["status"], "ok");
        assert_eq!(set_v["saved"], true);
        assert_eq!(
            set_v["observability"]["request_type"],
            REQUEST_NETWORK_INTENT_SET
        );
        assert_eq!(set_v["observability"]["correlation_id"], 1001);

        let get_req =
            req(REQUEST_NETWORK_INTENT_GET, serde_json::json!({}), 1002);
        let get_out = p.handle_request(&get_req).await.expect("intent.get");
        let get_v: Value =
            serde_json::from_slice(&get_out.payload).expect("json");
        assert_eq!(get_v["status"], "ok");
        assert_eq!(get_v["sta_psk_configured"], true);
        assert_eq!(get_v["ap_psk_configured"], true);
        assert_eq!(get_v["intent"]["wifi"]["sta_ssid"], "HotelWiFi");
        assert_eq!(
            get_v["observability"]["request_type"],
            REQUEST_NETWORK_INTENT_GET
        );
        assert_eq!(get_v["observability"]["correlation_id"], 1002);

        let sta_psk_raw =
            tokio::fs::read_to_string(p.sta_psk_path().expect("path"))
                .await
                .expect("read sta secret");
        assert!(sta_psk_raw.contains("\"cipher\": \"xchacha20poly1305\""));
    }

    #[tokio::test]
    async fn request_flow_apply_works_with_mock_nmcli() {
        let dir = tempfile::tempdir().expect("temp dir");
        let nmcli_path = dir.path().join("nmcli-mock.sh");
        let nmcli_log_path = dir.path().join("nmcli.log");
        std::fs::write(
            &nmcli_path,
            format!(
                "#!/usr/bin/env bash\n\
echo \"$@\" >> \"{}\"\n\
if [[ \"$1\" == \"-t\" && \"$2\" == \"-f\" && \"$3\" == \"DEVICE,TYPE,STATE,CONNECTION\" && \"$4\" == \"device\" ]]; then\n\
  echo \"wlan0:wifi:connected:evo-network-wifi-sta\"\n\
  exit 0\n\
fi\n\
if [[ \"$1\" == \"general\" && \"$2\" == \"status\" ]]; then\n\
  echo \"connected\"\n\
  exit 0\n\
fi\n\
if [[ \"$1\" == \"connection\" && \"$2\" == \"show\" ]]; then\n\
  exit 1\n\
fi\n\
if [[ \"$1\" == \"-t\" && \"$2\" == \"-f\" && \"$3\" == \"SSID,SIGNAL,SECURITY,ACTIVE\" ]]; then\n\
  echo \"HotelWiFi:80:WPA2:yes\"\n\
  exit 0\n\
fi\n\
if [[ \"$1\" == \"-t\" && \"$2\" == \"-f\" && \"$3\" == \"BSSID,SSID,SIGNAL,FREQ,ACTIVE\" ]]; then\n\
  echo \"AA:BB:CC:DD:EE:FF:HotelWiFi:80:5180:yes\"\n\
  exit 0\n\
fi\n\
exit 0\n",
                nmcli_log_path.display()
            ),
        )
        .expect("write mock");
        #[cfg(unix)]
        std::fs::set_permissions(
            &nmcli_path,
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod");

        let mut p = NetworkNmPlugin::new();
        p.loaded = true;
        p.state_dir = Some(dir.path().to_path_buf());
        p.config = PluginConfig::defaults();
        p.config.nmcli_path = nmcli_path.to_string_lossy().to_string();

        let apply_req = req(
            REQUEST_NETWORK_INTENT_APPLY,
            serde_json::json!({
                "intent": {
                    "version": 1,
                    "ethernet": { "enabled": false },
                    "wifi": { "role": "disabled", "ifname": "wlan0" },
                    "fallback": { "hotspot_enabled": false }
                }
            }),
            1003,
        );
        let apply_out =
            p.handle_request(&apply_req).await.expect("intent.apply");
        let apply_v: Value =
            serde_json::from_slice(&apply_out.payload).expect("json");
        assert_eq!(apply_v["status"], "ok");
        assert_eq!(apply_v["apply"]["ok"], true);
        assert_eq!(
            apply_v["observability"]["request_type"],
            REQUEST_NETWORK_INTENT_APPLY
        );
        assert_eq!(apply_v["observability"]["correlation_id"], 1003);
        assert!(apply_v["notices"]
            .as_array()
            .expect("notices")
            .iter()
            .any(|n| n["code"] == "network_apply_ok"));

        let nmcli_log = tokio::fs::read_to_string(&nmcli_log_path)
            .await
            .expect("nmcli log");
        assert!(nmcli_log.contains("connection down evo-network-wifi-sta"));
    }

    #[tokio::test]
    async fn request_flow_status_is_degraded_on_partial_backend_failure() {
        let dir = tempfile::tempdir().expect("temp dir");
        let nmcli_path = dir.path().join("nmcli-mock-status.sh");
        std::fs::write(
            &nmcli_path,
            "#!/usr/bin/env bash\n\
if [[ \"$1\" == \"-t\" && \"$2\" == \"-f\" && \"$3\" == \"DEVICE,TYPE,STATE,CONNECTION\" && \"$4\" == \"device\" ]]; then\n\
  echo \"device-table-failed\" 1>&2\n\
  exit 10\n\
fi\n\
if [[ \"$1\" == \"general\" && \"$2\" == \"status\" ]]; then\n\
  echo \"connected\"\n\
  exit 0\n\
fi\n\
if [[ \"$1\" == \"-t\" && \"$2\" == \"-f\" && \"$3\" == \"SSID,SIGNAL,SECURITY,ACTIVE\" ]]; then\n\
  echo \"HotelWiFi:80:WPA2:yes\"\n\
  exit 0\n\
fi\n\
if [[ \"$1\" == \"-t\" && \"$2\" == \"-f\" && \"$3\" == \"BSSID,SSID,SIGNAL,FREQ,ACTIVE\" ]]; then\n\
  echo \"AA:BB:CC:DD:EE:FF:HotelWiFi:80:5180:yes\"\n\
  exit 0\n\
fi\n\
exit 0\n",
        )
        .expect("write mock");
        #[cfg(unix)]
        std::fs::set_permissions(
            &nmcli_path,
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod");

        let mut p = NetworkNmPlugin::new();
        p.loaded = true;
        p.state_dir = Some(dir.path().to_path_buf());
        p.config = PluginConfig::defaults();
        p.config.nmcli_path = nmcli_path.to_string_lossy().to_string();

        let status_req =
            req(REQUEST_NETWORK_STATUS, serde_json::json!({}), 1004);
        let status_out = p.handle_request(&status_req).await.expect("status");
        let status_v: Value =
            serde_json::from_slice(&status_out.payload).expect("json");
        assert_eq!(status_v["status"], "ok");
        assert_eq!(status_v["degraded"], true);
        assert_eq!(status_v["domain_health"]["device_table"]["ok"], false);
        assert_eq!(status_v["domain_health"]["general_status"]["ok"], true);
    }

    #[tokio::test]
    async fn request_flow_scan_uses_cache_until_refresh_requested() {
        let dir = tempfile::tempdir().expect("temp dir");
        let nmcli_path = dir.path().join("nmcli-scan-cache.sh");
        let nmcli_log_path = dir.path().join("nmcli-cache.log");
        std::fs::write(
            &nmcli_path,
            format!(
                "#!/usr/bin/env bash\n\
echo \"$@\" >> \"{}\"\n\
if [[ \"$1\" == \"-t\" && \"$2\" == \"-f\" && \"$3\" == \"SSID,SIGNAL,SECURITY,ACTIVE\" ]]; then\n\
  echo \"HotelWiFi:80:WPA2:yes\"\n\
  exit 0\n\
fi\n\
if [[ \"$1\" == \"-t\" && \"$2\" == \"-f\" && \"$3\" == \"BSSID,SSID,SIGNAL,FREQ,ACTIVE\" ]]; then\n\
  echo \"AA:BB:CC:DD:EE:FF:HotelWiFi:80:5180:yes\"\n\
  exit 0\n\
fi\n\
exit 0\n",
                nmcli_log_path.display()
            ),
        )
        .expect("write mock");
        #[cfg(unix)]
        std::fs::set_permissions(
            &nmcli_path,
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod");

        let mut p = NetworkNmPlugin::new();
        p.loaded = true;
        p.state_dir = Some(dir.path().to_path_buf());
        p.config = PluginConfig::defaults();
        p.config.nmcli_path = nmcli_path.to_string_lossy().to_string();
        p.config.scan_cache_ttl_ms = 60000;

        let scan_req_1 = req(REQUEST_NETWORK_SCAN, serde_json::json!({}), 1101);
        p.handle_request(&scan_req_1).await.expect("scan-1");
        let scan_req_2 = req(REQUEST_NETWORK_SCAN, serde_json::json!({}), 1102);
        p.handle_request(&scan_req_2).await.expect("scan-2");
        let scan_req_3 = req(
            REQUEST_NETWORK_SCAN,
            serde_json::json!({ "refresh": true }),
            1103,
        );
        p.handle_request(&scan_req_3).await.expect("scan-3");

        let nmcli_log = tokio::fs::read_to_string(&nmcli_log_path)
            .await
            .expect("read log");
        let available_calls =
            nmcli_log.matches("SSID,SIGNAL,SECURITY,ACTIVE").count();
        let candidate_calls =
            nmcli_log.matches("BSSID,SSID,SIGNAL,FREQ,ACTIVE").count();
        assert_eq!(available_calls, 2);
        assert_eq!(candidate_calls, 2);
    }

    #[tokio::test]
    async fn request_flow_scan_returns_stale_cache_on_backend_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let nmcli_path = dir.path().join("nmcli-scan-fail.sh");
        std::fs::write(
            &nmcli_path,
            "#!/usr/bin/env bash\necho scan-failed 1>&2\nexit 10\n",
        )
        .expect("write mock");
        #[cfg(unix)]
        std::fs::set_permissions(
            &nmcli_path,
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod");

        let mut p = NetworkNmPlugin::new();
        p.loaded = true;
        p.state_dir = Some(dir.path().to_path_buf());
        p.config = PluginConfig::defaults();
        p.config.nmcli_path = nmcli_path.to_string_lossy().to_string();
        p.config.scan_cache_ttl_ms = 60000;
        p.scan_cache.insert(
            "wlan0".to_string(),
            CachedScan {
                available: vec![ScanRow {
                    ssid: "HotelWiFi".to_string(),
                    signal: 4,
                    security: "wpa2".to_string(),
                    active: true,
                }],
                candidates: vec![WifiStaCandidate {
                    bssid: "AA:BB:CC:DD:EE:FF".to_string(),
                    ssid: "HotelWiFi".to_string(),
                    signal_pct: 80,
                    freq_mhz: Some(5180),
                    band: BandClass::Ghz5,
                    active: true,
                }],
                captured_at: Instant::now(),
            },
        );

        let scan_req = req(
            REQUEST_NETWORK_SCAN,
            serde_json::json!({ "ifname": "wlan0", "refresh": true }),
            1104,
        );
        let out = p.handle_request(&scan_req).await.expect("scan");
        let v: Value = serde_json::from_slice(&out.payload).expect("json");
        assert_eq!(v["status"], "ok");
        assert_eq!(v["cache"]["hit"], true);
        assert_eq!(v["cache"]["stale"], true);
        assert_eq!(v["available"][0]["ssid"], "HotelWiFi");
        assert!(v["scan_error"]
            .as_str()
            .unwrap_or("")
            .contains("scan-failed"));
    }

    #[tokio::test]
    async fn nmcli_output_times_out_on_slow_backend() {
        let dir = tempfile::tempdir().expect("temp dir");
        let nmcli_path = dir.path().join("nmcli-slow.sh");
        std::fs::write(
            &nmcli_path,
            "#!/usr/bin/env bash\nsleep 2\necho connected\n",
        )
        .expect("write mock");
        #[cfg(unix)]
        std::fs::set_permissions(
            &nmcli_path,
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod");

        let mut p = NetworkNmPlugin::new();
        p.config = PluginConfig::defaults();
        p.config.nmcli_path = nmcli_path.to_string_lossy().to_string();
        p.config.nmcli_timeout_ms = 50;
        let err = p
            .nmcli_output(&["general", "status"])
            .await
            .expect_err("must timeout");
        assert!(format!("{err}").contains("timed out"));
    }
}
