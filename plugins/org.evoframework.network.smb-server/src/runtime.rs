// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Evo-as-SMB-server primitive.
//!
//! The framework-tier substrate the operator sees as "let
//! phones, laptops, and other hosts on the LAN write music
//! directly onto the device via SMB — without hand-editing
//! `/etc/samba/smb.conf`."
//!
//! ## Scope
//!
//! * Persists the operator-authored SMB-server configuration
//!   at `<state_dir>/smb_server.toml`: the enabled toggle, the
//!   `server min protocol` selector, the operator-defined extra
//!   shares (paths beyond the framework's shipped defaults), and
//!   the operator-created SMB users.
//! * Renders `smb.conf` from the persisted state + a pair of
//!   distribution-supplied identity strings (`netbios_name` +
//!   `workgroup`) and a path allowlist that refuses shares
//!   outside the framework's expected paths (`/mnt/NAS`,
//!   `/data/INTERNAL`, `/mnt/USB`, `/var/lib/evo/uploads` by
//!   default).
//! * Validates the rendered config with `testparm -s` before
//!   installing over `/etc/samba/smb.conf` and restarting
//!   `smbd.service`.
//! * Adds / revokes SMB users via `smbpasswd -a` / `smbpasswd
//!   -x`. Passwords never appear on the persistence surface —
//!   the record carries a `credential_key` into the evo
//!   credential vault; the vault issues bytes at `smbpasswd -a`
//!   time and the runtime pipes them into stdin.
//! * Publishes the reactive `system_smb_server` subject on
//!   every successful apply so operator surfaces reflect the
//!   current server state.
//! * Routes the three shelf verbs
//!   (`network.smb_server.apply` / `.user_add` /
//!   `.user_revoke`) through
//!   [`SambaServerRuntime::dispatch_verb`], with the same
//!   response-envelope shape the network-shares runtime uses.
//!
//! ## Non-scope
//!
//! Discovery of remote SMB shares (that's `network_shares` +
//! `refresh_discovery`); mounting SMB shares (also
//! `network_shares`); the physical `smbd` package install
//! (handled by the distribution's bootstrap script).
//!
//! ## Path allowlist
//!
//! Every extra_share whose `path` does not begin with a prefix
//! in the runtime's allowlist is refused with a structured
//! [`RefusedSetting`] and NOT rendered into `smb.conf`. The
//! refusal surfaces on the [`ApplyReport`] so operator UI can
//! render "path X was refused because it is outside the
//! allowed prefix set" inline.

use async_trait::async_trait;
use evo_plugin_sdk::contract::{
    ExternalAddressing, SubjectAnnouncement, SubjectAnnouncer,
};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::SystemTime;
use tokio::sync::Mutex;

/// On-disk persistence schema version. Bumps land alongside a
/// migration in [`SmbServerState::load`].
pub const SMB_SERVER_SCHEMA_VERSION: u32 = 1;

/// Persistence file basename under the runtime's state_dir.
pub const SMB_SERVER_FILE: &str = "smb_server.toml";

/// Canonical `smb.conf` path the runtime writes on every
/// successful apply.
pub const DEFAULT_SMB_CONF_PATH: &str = "/etc/samba/smb.conf";

/// Default per-subprocess timeout in milliseconds (10 s covers
/// testparm + smbpasswd + systemctl restart on the reference
/// Pi 5).
pub const DEFAULT_SAMBA_SUBPROCESS_TIMEOUT_MS: u64 = 10_000;

/// Default extra_share path allowlist. Paths outside these
/// prefixes are refused at apply time. Distributions with
/// different filesystem conventions override via
/// [`SambaServerRuntimeBuilder::with_path_allowlist`].
pub const DEFAULT_SHARE_PATH_ALLOWLIST: &[&str] = &[
    "/mnt/NAS",
    "/data/INTERNAL",
    "/mnt/USB",
    "/var/lib/evo/uploads",
];

/// Default netbios name the framework advertises when the
/// distribution has not overridden.
pub const DEFAULT_NETBIOS_NAME: &str = "EvoDevice";

/// Default workgroup the framework advertises.
pub const DEFAULT_WORKGROUP: &str = "WORKGROUP";

/// `server min protocol` selector. The wire form is snake_case
/// (`default`, `smb2_02`, `smb3_02`); the smb.conf directive
/// takes the SMB-family strings (`SMB2_02`, `SMB3_02`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum MinProtocol {
    /// Distribution default (typically `SMB2_02` on modern
    /// stacks; the runtime does NOT emit a `server min
    /// protocol` line and smbd chooses).
    #[default]
    Default,
    /// SMB 2.02 minimum.
    Smb2_02,
    /// SMB 3.02 minimum.
    Smb3_02,
}

impl MinProtocol {
    /// Convert to the smb.conf directive value. Returns `None`
    /// for [`MinProtocol::Default`] so the caller can suppress
    /// the line entirely.
    pub fn as_smbd_value(self) -> Option<&'static str> {
        match self {
            MinProtocol::Default => None,
            MinProtocol::Smb2_02 => Some("SMB2_02"),
            MinProtocol::Smb3_02 => Some("SMB3_02"),
        }
    }

    /// Snake-case wire string used on the reactive subject
    /// envelope + the verb payloads.
    pub fn as_wire_str(self) -> &'static str {
        match self {
            MinProtocol::Default => "default",
            MinProtocol::Smb2_02 => "smb2_02",
            MinProtocol::Smb3_02 => "smb3_02",
        }
    }
}

/// One operator-defined extra share beyond the framework
/// defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtraShare {
    /// SMB-visible share name (renders as `//<host>/<name>` on
    /// client browsers).
    pub name: String,
    /// Absolute filesystem path on the evo device the share
    /// exposes. Rejected at apply time if not covered by the
    /// runtime's path allowlist.
    pub path: String,
    /// Whether unauthenticated (guest) access is allowed.
    /// Operator UI defaults to `false`.
    pub guest_ok: bool,
}

/// One operator-created SMB user. Persisted; the
/// `credential_key` field is populated when the record is
/// created from the vault-issued key at
/// [`SambaServerRuntime::add_user`] time and is stripped from
/// the wire envelope by [`SmbUserPublic::from`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmbUserRecord {
    /// SMB user name on this device.
    pub username: String,
    /// Optional canonical domain identity this user maps to.
    #[serde(default)]
    pub mapped_domain_identity: Option<String>,
    /// Wall-clock ms at record creation.
    pub created_at_ms: i64,
    /// Opaque credential-vault key. Never on the reactive
    /// subject envelope; used at apply time to fetch the
    /// password bytes for `smbpasswd -a` stdin.
    pub credential_key: String,
}

/// Subject-envelope form of [`SmbUserRecord`] — the same
/// public fields minus the credential_key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmbUserPublic {
    /// SMB user name on this device.
    pub username: String,
    /// Optional canonical domain identity this user maps to.
    pub mapped_domain_identity: Option<String>,
    /// Wall-clock ms at record creation.
    pub created_at_ms: i64,
}

impl From<&SmbUserRecord> for SmbUserPublic {
    fn from(r: &SmbUserRecord) -> Self {
        Self {
            username: r.username.clone(),
            mapped_domain_identity: r.mapped_domain_identity.clone(),
            created_at_ms: r.created_at_ms,
        }
    }
}

/// Persistence root for the SMB-server substrate. Written
/// atomically (temp file + rename) so a mid-write crash never
/// leaves a truncated file that would fail to parse at boot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmbServerState {
    /// On-disk schema version marker.
    pub schema_version: u32,
    /// Whether smbd is enabled + running on this node.
    pub enabled: bool,
    /// Configured `server min protocol` value.
    #[serde(default)]
    pub min_protocol: MinProtocol,
    /// Operator-defined extra shares.
    #[serde(default)]
    pub extra_shares: Vec<ExtraShare>,
    /// Operator-created SMB users.
    #[serde(default)]
    pub smb_users: Vec<SmbUserRecord>,
    /// Wall-clock ms of the last successful apply. `None` when
    /// no apply has succeeded since state creation.
    #[serde(default)]
    pub last_apply_at_ms: Option<i64>,
}

impl SmbServerState {
    /// Fresh state — disabled, no extra shares, no users.
    pub fn empty() -> Self {
        Self {
            schema_version: SMB_SERVER_SCHEMA_VERSION,
            enabled: false,
            min_protocol: MinProtocol::default(),
            extra_shares: Vec::new(),
            smb_users: Vec::new(),
            last_apply_at_ms: None,
        }
    }

    /// Load from the given path. A missing file yields
    /// [`SmbServerState::empty`]. A schema-version mismatch is
    /// a hard error — the runtime refuses to load unknown
    /// versions rather than silently dropping fields.
    pub fn load(path: &Path) -> Result<Self, SmbServerStateError> {
        match fs::read_to_string(path) {
            Ok(text) => {
                let state: SmbServerState =
                    toml::from_str(&text).map_err(|e| {
                        SmbServerStateError::TomlParse {
                            detail: e.to_string(),
                        }
                    })?;
                if state.schema_version != SMB_SERVER_SCHEMA_VERSION {
                    return Err(SmbServerStateError::SchemaVersionMismatch {
                        found: state.schema_version,
                        expected: SMB_SERVER_SCHEMA_VERSION,
                    });
                }
                Ok(state)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::empty()),
            Err(e) => Err(SmbServerStateError::Io {
                detail: e.to_string(),
            }),
        }
    }

    /// Serialise + atomic-save. Writes to `<path>.tmp` then
    /// renames.
    pub fn save(&self, path: &Path) -> Result<(), SmbServerStateError> {
        let text = toml::to_string_pretty(self).map_err(|e| {
            SmbServerStateError::TomlRender {
                detail: e.to_string(),
            }
        })?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                SmbServerStateError::Io {
                    detail: e.to_string(),
                }
            })?;
        }
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, text.as_bytes()).map_err(|e| {
            SmbServerStateError::Io {
                detail: e.to_string(),
            }
        })?;
        fs::rename(&tmp, path).map_err(|e| SmbServerStateError::Io {
            detail: e.to_string(),
        })?;
        Ok(())
    }
}

/// Persistence / decode errors.
#[derive(Debug, thiserror::Error)]
pub enum SmbServerStateError {
    /// I/O error on the state file.
    #[error("smb_server state I/O error: {detail}")]
    Io {
        /// Underlying error text.
        detail: String,
    },
    /// TOML parse error.
    #[error("smb_server TOML parse error: {detail}")]
    TomlParse {
        /// Underlying error text.
        detail: String,
    },
    /// TOML serialise error.
    #[error("smb_server TOML render error: {detail}")]
    TomlRender {
        /// Underlying error text.
        detail: String,
    },
    /// On-disk schema version does not match the compiled version.
    #[error("smb_server schema version mismatch: found {found}, expected {expected}")]
    SchemaVersionMismatch {
        /// Version found on disk.
        found: u32,
        /// Version this build expects.
        expected: u32,
    },
}

/// One entry in the [`ApplyReport::refused_settings`] list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefusedSetting {
    /// The share name (or `"config"` for global refusals).
    pub setting: String,
    /// Operator-readable reason.
    pub reason: String,
}

/// Aggregate outcome of an [`SambaServerRuntime::apply`] call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyReport {
    /// Names of extra_shares that landed in the rendered
    /// `smb.conf`.
    pub applied_shares: Vec<String>,
    /// Shares refused by the path allowlist + any other
    /// framework-level refusals.
    pub refused_settings: Vec<RefusedSetting>,
    /// Whether smbd was restarted after the apply.
    pub smbd_restarted: bool,
    /// Wall-clock ms of the apply.
    pub applied_at_ms: i64,
}

/// Errors surfaced by [`SambaServerRuntime::apply`] +
/// [`add_user`](SambaServerRuntime::add_user) +
/// [`revoke_user`](SambaServerRuntime::revoke_user).
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// Persistence layer failed.
    #[error("smb_server persistence error: {0}")]
    Persistence(#[from] SmbServerStateError),
    /// `testparm -s` returned non-zero on the rendered config.
    /// The rendered config is preserved so operator UI can
    /// render the exact stanza that failed validation.
    #[error("testparm rejected the rendered config: exit={exit_code:?}, stderr={stderr}")]
    TestparmFailed {
        /// testparm's exit code (`None` if terminated by signal).
        exit_code: Option<i32>,
        /// verbatim stderr snippet.
        stderr: String,
    },
    /// Writing the rendered config to `/etc/samba/smb.conf`
    /// failed.
    #[error("smb.conf install failed: {detail}")]
    ConfigInstall {
        /// I/O error text.
        detail: String,
    },
    /// `systemctl restart smbd` returned non-zero.
    #[error("smbd restart failed: exit={exit_code:?}, stderr={stderr}")]
    RestartFailed {
        /// systemctl exit code.
        exit_code: Option<i32>,
        /// verbatim stderr snippet.
        stderr: String,
    },
    /// `smbpasswd -a <user>` (add) or `smbpasswd -x <user>`
    /// (revoke) returned non-zero.
    #[error(
        "smbpasswd failed for {username}: exit={exit_code:?}, stderr={stderr}"
    )]
    SmbpasswdFailed {
        /// The user the smbpasswd command targeted.
        username: String,
        /// smbpasswd exit code.
        exit_code: Option<i32>,
        /// verbatim stderr snippet.
        stderr: String,
    },
    /// The credential vault did not return password bytes for
    /// the declared key.
    #[error("credential vault has no entry for key {key}")]
    CredentialMissing {
        /// The credential-vault key.
        key: String,
    },
    /// Subprocess invocation failed at the process layer.
    #[error("subprocess I/O error: {detail}")]
    SubprocessIo {
        /// error text.
        detail: String,
    },
    /// The user record targeted by a revoke was not found.
    #[error("smb user {username} not found")]
    UserNotFound {
        /// The username the caller asked to revoke.
        username: String,
    },
    /// A duplicate user was added.
    #[error("smb user {username} already exists")]
    UserAlreadyExists {
        /// The username the caller asked to add.
        username: String,
    },
}

/// Rendered subprocess output shape (mirrors
/// [`crate::network_shares::CommandOutput`] for isolation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// exit code (`None` on signal termination).
    pub exit_code: Option<i32>,
    /// stdout bytes.
    pub stdout: Vec<u8>,
    /// stderr bytes.
    pub stderr: Vec<u8>,
}

/// Subprocess abstraction so tests can inject a scripted
/// executor. Production impl is [`SubprocessSambaExecutor`].
#[async_trait]
pub trait SambaExecutor: Send + Sync {
    /// Invoke a program with the supplied argv and a
    /// per-call timeout budget in milliseconds. If
    /// `stdin_bytes` is non-empty, feed it into the child's
    /// stdin (used for `smbpasswd -a` password entry).
    async fn run(
        &self,
        program: &str,
        args: &[String],
        timeout_ms: u64,
        stdin_bytes: &[u8],
    ) -> Result<CommandOutput, ApplyError>;

    /// Write bytes to a filesystem path, replacing any prior
    /// content. Used for installing the rendered smb.conf.
    async fn write_file(
        &self,
        path: &Path,
        contents: &[u8],
    ) -> Result<(), ApplyError>;
}

/// Production [`SambaExecutor`] backed by `tokio::process`.
pub struct SubprocessSambaExecutor;

#[async_trait]
impl SambaExecutor for SubprocessSambaExecutor {
    async fn run(
        &self,
        program: &str,
        args: &[String],
        timeout_ms: u64,
        stdin_bytes: &[u8],
    ) -> Result<CommandOutput, ApplyError> {
        use tokio::io::AsyncWriteExt;
        use tokio::process::Command;

        let mut cmd = Command::new(program);
        cmd.args(args);
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| ApplyError::SubprocessIo {
            detail: e.to_string(),
        })?;
        if !stdin_bytes.is_empty() {
            if let Some(stdin) = child.stdin.as_mut() {
                stdin.write_all(stdin_bytes).await.map_err(|e| {
                    ApplyError::SubprocessIo {
                        detail: e.to_string(),
                    }
                })?;
            }
        }
        drop(child.stdin.take());
        let output = tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            child.wait_with_output(),
        )
        .await
        .map_err(|_| ApplyError::SubprocessIo {
            detail: format!("timed out after {timeout_ms}ms"),
        })?
        .map_err(|e| ApplyError::SubprocessIo {
            detail: e.to_string(),
        })?;
        Ok(CommandOutput {
            exit_code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    async fn write_file(
        &self,
        path: &Path,
        contents: &[u8],
    ) -> Result<(), ApplyError> {
        tokio::fs::write(path, contents).await.map_err(|e| {
            ApplyError::ConfigInstall {
                detail: e.to_string(),
            }
        })
    }
}

/// Credential-fetch abstraction so tests can inject a stub.
/// Production impl reaches into the framework credential
/// vault; the default [`NoSmbCredentialFetcher`] always
/// returns `None`.
#[async_trait]
pub trait SmbCredentialFetcher: Send + Sync {
    /// Return the vault entry for `key` (typically the SMB
    /// user's canonical key), or `None` if the vault has no
    /// record.
    async fn fetch_password(&self, key: &str) -> Option<Vec<u8>>;
}

/// Default fetcher — returns `None` for every key. Callers
/// that need vault access wire a real fetcher via
/// [`SambaServerRuntimeBuilder::with_credentials`].
pub struct NoSmbCredentialFetcher;

#[async_trait]
impl SmbCredentialFetcher for NoSmbCredentialFetcher {
    async fn fetch_password(&self, _key: &str) -> Option<Vec<u8>> {
        None
    }
}

/// Render an `smb.conf` from the persisted state + the
/// distribution's identity strings + the runtime's path
/// allowlist.
///
/// Returns the rendered text plus the [`ApplyReport::applied_shares`]
/// / [`refused_settings`](ApplyReport::refused_settings) split
/// so the caller can surface the outcome on the wire.
pub fn render_smb_conf(
    state: &SmbServerState,
    netbios_name: &str,
    workgroup: &str,
    path_allowlist: &[String],
) -> (String, Vec<String>, Vec<RefusedSetting>) {
    let mut out = String::new();
    out.push_str("[global]\n");
    out.push_str(&format!("netbios name = {netbios_name}\n"));
    out.push_str("server string = evo audiophile source\n");
    out.push_str(&format!("workgroup = {workgroup}\n"));
    out.push_str("security = user\n");
    out.push_str("map to guest = Bad User\n");
    out.push_str("encrypt passwords = yes\n");
    out.push_str("local master = no\n");
    out.push_str("preferred master = no\n");
    out.push_str("os level = 30\n");
    if let Some(min) = state.min_protocol.as_smbd_value() {
        out.push_str(&format!("server min protocol = {min}\n"));
    }
    out.push('\n');

    let mut applied: Vec<String> = Vec::new();
    let mut refused: Vec<RefusedSetting> = Vec::new();

    for share in &state.extra_shares {
        if !path_matches_allowlist(&share.path, path_allowlist) {
            refused.push(RefusedSetting {
                setting: share.name.clone(),
                reason: format!(
                    "path {} not in framework allowlist ({})",
                    share.path,
                    path_allowlist.join(", "),
                ),
            });
            continue;
        }
        out.push_str(&format!("[{}]\n", share.name));
        out.push_str(&format!(
            "        comment = evo extra share {}\n",
            share.name
        ));
        out.push_str(&format!("        path = {}\n", share.path));
        out.push_str("        read only = no\n");
        out.push_str(&format!(
            "        guest ok = {}\n",
            if share.guest_ok { "yes" } else { "no" },
        ));
        out.push('\n');
        applied.push(share.name.clone());
    }

    (out, applied, refused)
}

fn path_matches_allowlist(candidate: &str, allowlist: &[String]) -> bool {
    let normalised = candidate.trim_end_matches('/');
    for prefix in allowlist {
        let p = prefix.trim_end_matches('/');
        if normalised == p {
            return true;
        }
        if let Some(rest) = normalised.strip_prefix(p) {
            if rest.starts_with('/') {
                return true;
            }
        }
    }
    false
}

/// Build the argv for `testparm -s` (silent mode — prints the
/// canonicalised config to stdout so the runtime can verify
/// smbd will accept it).
pub fn build_testparm_args(config_path: &Path) -> Vec<String> {
    vec!["-s".to_string(), config_path.display().to_string()]
}

/// Build the argv for `smbpasswd -a -s <user>` (add + read
/// password from stdin).
pub fn build_smbpasswd_add_args(username: &str) -> Vec<String> {
    vec!["-a".to_string(), "-s".to_string(), username.to_string()]
}

/// Build the argv for `smbpasswd -x <user>` (delete).
pub fn build_smbpasswd_delete_args(username: &str) -> Vec<String> {
    vec!["-x".to_string(), username.to_string()]
}

/// Build the argv for `systemctl restart smbd`.
pub fn build_systemctl_restart_args() -> Vec<String> {
    vec!["restart".to_string(), "smbd".to_string()]
}

// --------------------------------------------------------------
// Subject publisher constants (Ship 3)
// --------------------------------------------------------------

/// Snake-case subject type for the SMB-server singleton subject.
pub const SYSTEM_SMB_SERVER_SUBJECT_TYPE: &str = "system_smb_server";

/// Addressing scheme for the SMB-server singleton.
pub const SYSTEM_SMB_SERVER_SUBJECT_SCHEME: &str = "evo.system.smb.server";

/// Singleton addressing value.
pub const SINGLETON_ADDRESSING_VALUE: &str = "local";

/// Compose the singleton addressing for `system_smb_server`.
pub fn system_smb_server_addressing() -> ExternalAddressing {
    ExternalAddressing {
        scheme: SYSTEM_SMB_SERVER_SUBJECT_SCHEME.to_string(),
        value: SINGLETON_ADDRESSING_VALUE.to_string(),
    }
}

/// Wire envelope carried on the `system_smb_server` subject.
#[derive(Debug, Clone, Serialize)]
pub struct SystemSmbServerEnvelope {
    /// Whether smbd is enabled + running.
    pub enabled: bool,
    /// `server min protocol` in wire form.
    pub min_protocol: String,
    /// Configured extra shares.
    pub extra_shares: Vec<ExtraShare>,
    /// Configured SMB users (public fields only — no
    /// credential_key on the wire).
    pub smb_users: Vec<SmbUserPublic>,
    /// Wall-clock ms of the last successful apply, or `None`.
    pub last_apply_at_ms: Option<i64>,
    /// Envelope composition time.
    pub last_update_at: SystemTime,
}

struct SambaPublisher {
    announcer: Arc<dyn SubjectAnnouncer>,
}

/// Framework-tier SMB-server runtime primitive.
///
/// Concurrency: state lives behind a Tokio [`Mutex`] so
/// concurrent operator UI calls serialise safely at the write
/// boundary. Send + Sync via [`Arc`] for cross-task sharing.
pub struct SambaServerRuntime {
    inner: Arc<Mutex<SambaServerInner>>,
    executor: Arc<dyn SambaExecutor>,
    credentials: Arc<dyn SmbCredentialFetcher>,
    testparm_program: String,
    smbpasswd_program: String,
    systemctl_program: String,
    smb_conf_path: PathBuf,
    netbios_name: String,
    workgroup: String,
    path_allowlist: Vec<String>,
    subprocess_timeout_ms: u64,
    publisher: StdMutex<Option<SambaPublisher>>,
    now_fn: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl std::fmt::Debug for SambaServerRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SambaServerRuntime").finish()
    }
}

struct SambaServerInner {
    state: SmbServerState,
    path: PathBuf,
}

fn default_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl SambaServerRuntime {
    /// Convenience constructor: opens the persistence file
    /// under `state_dir` with production defaults for every
    /// pluggable field. For tests + vendor wiring use
    /// [`Self::builder`].
    pub fn open(state_dir: &Path) -> Result<Self, SmbServerStateError> {
        let path = state_dir.join(SMB_SERVER_FILE);
        let state = SmbServerState::load(&path)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(SambaServerInner { state, path })),
            executor: Arc::new(SubprocessSambaExecutor),
            credentials: Arc::new(NoSmbCredentialFetcher),
            testparm_program: "/usr/bin/testparm".to_string(),
            smbpasswd_program: "/usr/bin/smbpasswd".to_string(),
            systemctl_program: "/usr/bin/systemctl".to_string(),
            smb_conf_path: PathBuf::from(DEFAULT_SMB_CONF_PATH),
            netbios_name: DEFAULT_NETBIOS_NAME.to_string(),
            workgroup: DEFAULT_WORKGROUP.to_string(),
            path_allowlist: DEFAULT_SHARE_PATH_ALLOWLIST
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            subprocess_timeout_ms: DEFAULT_SAMBA_SUBPROCESS_TIMEOUT_MS,
            publisher: StdMutex::new(None),
            now_fn: Arc::new(default_now_ms),
        })
    }

    /// Start a builder for constructing a runtime with custom
    /// executor / credential fetcher / program paths / identity
    /// strings / path allowlist / clock.
    pub fn builder(
        state_dir: &Path,
    ) -> Result<SambaServerRuntimeBuilder, SmbServerStateError> {
        let path = state_dir.join(SMB_SERVER_FILE);
        let state = SmbServerState::load(&path)?;
        Ok(SambaServerRuntimeBuilder {
            state,
            path,
            executor: None,
            credentials: None,
            testparm_program: None,
            smbpasswd_program: None,
            systemctl_program: None,
            smb_conf_path: None,
            netbios_name: None,
            workgroup: None,
            path_allowlist: None,
            subprocess_timeout_ms: None,
            now_fn: None,
        })
    }

    /// Return a clone of the current persisted state (for the
    /// operator UI settings-form seed).
    pub async fn get_state(&self) -> SmbServerState {
        let g = self.inner.lock().await;
        g.state.clone()
    }

    /// Apply new server-side settings. Steps: swap in the new
    /// enabled + min_protocol + extra_shares (users are
    /// managed separately via add_user / revoke_user), render
    /// smb.conf, testparm-validate, install over
    /// /etc/samba/smb.conf, restart smbd. Publishes a
    /// republish on the reactive subject on success.
    pub async fn apply(
        &self,
        new_enabled: bool,
        new_min_protocol: MinProtocol,
        new_extra_shares: Vec<ExtraShare>,
    ) -> Result<ApplyReport, ApplyError> {
        let now_ms = (self.now_fn)() as i64;
        let (rendered, applied_shares, refused_settings) = {
            let mut g = self.inner.lock().await;
            g.state.enabled = new_enabled;
            g.state.min_protocol = new_min_protocol;
            g.state.extra_shares = new_extra_shares;
            render_smb_conf(
                &g.state,
                &self.netbios_name,
                &self.workgroup,
                &self.path_allowlist,
            )
        };

        // Write to a temp path adjacent to the target so
        // testparm reads exactly what will land on the target.
        let candidate_path =
            self.smb_conf_path.with_extension("conf.candidate");
        self.executor
            .write_file(&candidate_path, rendered.as_bytes())
            .await?;

        let testparm_args = build_testparm_args(&candidate_path);
        let testparm_out = self
            .executor
            .run(
                &self.testparm_program,
                &testparm_args,
                self.subprocess_timeout_ms,
                b"",
            )
            .await?;
        if testparm_out.exit_code != Some(0) {
            return Err(ApplyError::TestparmFailed {
                exit_code: testparm_out.exit_code,
                stderr: String::from_utf8_lossy(&testparm_out.stderr)
                    .into_owned(),
            });
        }

        self.executor
            .write_file(&self.smb_conf_path, rendered.as_bytes())
            .await?;

        let smbd_restarted = if new_enabled {
            let systemctl_args = build_systemctl_restart_args();
            let restart_out = self
                .executor
                .run(
                    &self.systemctl_program,
                    &systemctl_args,
                    self.subprocess_timeout_ms,
                    b"",
                )
                .await?;
            if restart_out.exit_code != Some(0) {
                return Err(ApplyError::RestartFailed {
                    exit_code: restart_out.exit_code,
                    stderr: String::from_utf8_lossy(&restart_out.stderr)
                        .into_owned(),
                });
            }
            true
        } else {
            false
        };

        {
            let mut g = self.inner.lock().await;
            g.state.last_apply_at_ms = Some(now_ms);
            g.state.save(&g.path)?;
        }
        self.schedule_republish().await;

        Ok(ApplyReport {
            applied_shares,
            refused_settings,
            smbd_restarted,
            applied_at_ms: now_ms,
        })
    }

    /// Add an SMB user. Fetches password bytes from the vault
    /// via [`SmbCredentialFetcher::fetch_password`] and pipes
    /// them into `smbpasswd -a -s <user>` twice (the tool
    /// prompts for the password twice for confirmation). On
    /// success, persists the [`SmbUserRecord`] and republishes
    /// the reactive subject.
    pub async fn add_user(
        &self,
        username: String,
        credential_key: String,
        mapped_domain_identity: Option<String>,
    ) -> Result<SmbUserRecord, ApplyError> {
        {
            let g = self.inner.lock().await;
            if g.state.smb_users.iter().any(|u| u.username == username) {
                return Err(ApplyError::UserAlreadyExists { username });
            }
        }

        let password = self
            .credentials
            .fetch_password(&credential_key)
            .await
            .ok_or_else(|| ApplyError::CredentialMissing {
            key: credential_key.clone(),
        })?;

        // smbpasswd -a prompts for the password twice.
        let mut stdin = password.clone();
        stdin.push(b'\n');
        stdin.extend_from_slice(&password);
        stdin.push(b'\n');

        let args = build_smbpasswd_add_args(&username);
        let out = self
            .executor
            .run(
                &self.smbpasswd_program,
                &args,
                self.subprocess_timeout_ms,
                &stdin,
            )
            .await?;
        if out.exit_code != Some(0) {
            return Err(ApplyError::SmbpasswdFailed {
                username,
                exit_code: out.exit_code,
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }

        let created_at_ms = (self.now_fn)() as i64;
        let record = SmbUserRecord {
            username,
            mapped_domain_identity,
            created_at_ms,
            credential_key,
        };
        {
            let mut g = self.inner.lock().await;
            g.state.smb_users.push(record.clone());
            g.state.save(&g.path)?;
        }
        self.schedule_republish().await;
        Ok(record)
    }

    /// Revoke an SMB user via `smbpasswd -x <user>` and
    /// remove the persisted record. Republishes the reactive
    /// subject.
    pub async fn revoke_user(&self, username: &str) -> Result<(), ApplyError> {
        let found = {
            let g = self.inner.lock().await;
            g.state.smb_users.iter().any(|u| u.username == username)
        };
        if !found {
            return Err(ApplyError::UserNotFound {
                username: username.to_string(),
            });
        }

        let args = build_smbpasswd_delete_args(username);
        let out = self
            .executor
            .run(
                &self.smbpasswd_program,
                &args,
                self.subprocess_timeout_ms,
                b"",
            )
            .await?;
        if out.exit_code != Some(0) {
            return Err(ApplyError::SmbpasswdFailed {
                username: username.to_string(),
                exit_code: out.exit_code,
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }

        {
            let mut g = self.inner.lock().await;
            g.state.smb_users.retain(|u| u.username != username);
            g.state.save(&g.path)?;
        }
        self.schedule_republish().await;
        Ok(())
    }
}

/// Builder for [`SambaServerRuntime`] with pluggable substrate
/// components.
pub struct SambaServerRuntimeBuilder {
    state: SmbServerState,
    path: PathBuf,
    executor: Option<Arc<dyn SambaExecutor>>,
    credentials: Option<Arc<dyn SmbCredentialFetcher>>,
    testparm_program: Option<String>,
    smbpasswd_program: Option<String>,
    systemctl_program: Option<String>,
    smb_conf_path: Option<PathBuf>,
    netbios_name: Option<String>,
    workgroup: Option<String>,
    path_allowlist: Option<Vec<String>>,
    subprocess_timeout_ms: Option<u64>,
    now_fn: Option<Arc<dyn Fn() -> u64 + Send + Sync>>,
}

impl SambaServerRuntimeBuilder {
    /// Install a custom [`SambaExecutor`].
    pub fn with_executor(mut self, executor: Arc<dyn SambaExecutor>) -> Self {
        self.executor = Some(executor);
        self
    }

    /// Install a custom [`SmbCredentialFetcher`].
    pub fn with_credentials(
        mut self,
        credentials: Arc<dyn SmbCredentialFetcher>,
    ) -> Self {
        self.credentials = Some(credentials);
        self
    }

    /// Override the testparm program path (default `/usr/bin/testparm`).
    pub fn with_testparm_program(mut self, program: String) -> Self {
        self.testparm_program = Some(program);
        self
    }

    /// Override the smbpasswd program path (default `/usr/bin/smbpasswd`).
    pub fn with_smbpasswd_program(mut self, program: String) -> Self {
        self.smbpasswd_program = Some(program);
        self
    }

    /// Override the systemctl program path (default `/usr/bin/systemctl`).
    pub fn with_systemctl_program(mut self, program: String) -> Self {
        self.systemctl_program = Some(program);
        self
    }

    /// Override the smb.conf path (default `/etc/samba/smb.conf`).
    pub fn with_smb_conf_path(mut self, path: PathBuf) -> Self {
        self.smb_conf_path = Some(path);
        self
    }

    /// Override the advertised netbios name.
    pub fn with_netbios_name(mut self, name: String) -> Self {
        self.netbios_name = Some(name);
        self
    }

    /// Override the workgroup.
    pub fn with_workgroup(mut self, workgroup: String) -> Self {
        self.workgroup = Some(workgroup);
        self
    }

    /// Override the extra_share path allowlist. Paths outside
    /// every prefix in the list are refused at apply time.
    pub fn with_path_allowlist(mut self, list: Vec<String>) -> Self {
        self.path_allowlist = Some(list);
        self
    }

    /// Override the per-subprocess timeout in milliseconds.
    pub fn with_subprocess_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.subprocess_timeout_ms = Some(timeout_ms);
        self
    }

    /// Override the clock (test path).
    pub fn with_now_fn(
        mut self,
        now_fn: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Self {
        self.now_fn = Some(now_fn);
        self
    }

    /// Finalise.
    pub fn build(self) -> SambaServerRuntime {
        SambaServerRuntime {
            inner: Arc::new(Mutex::new(SambaServerInner {
                state: self.state,
                path: self.path,
            })),
            executor: self
                .executor
                .unwrap_or_else(|| Arc::new(SubprocessSambaExecutor)),
            credentials: self
                .credentials
                .unwrap_or_else(|| Arc::new(NoSmbCredentialFetcher)),
            testparm_program: self
                .testparm_program
                .unwrap_or_else(|| "/usr/bin/testparm".to_string()),
            smbpasswd_program: self
                .smbpasswd_program
                .unwrap_or_else(|| "/usr/bin/smbpasswd".to_string()),
            systemctl_program: self
                .systemctl_program
                .unwrap_or_else(|| "/usr/bin/systemctl".to_string()),
            smb_conf_path: self
                .smb_conf_path
                .unwrap_or_else(|| PathBuf::from(DEFAULT_SMB_CONF_PATH)),
            netbios_name: self
                .netbios_name
                .unwrap_or_else(|| DEFAULT_NETBIOS_NAME.to_string()),
            workgroup: self
                .workgroup
                .unwrap_or_else(|| DEFAULT_WORKGROUP.to_string()),
            path_allowlist: self.path_allowlist.unwrap_or_else(|| {
                DEFAULT_SHARE_PATH_ALLOWLIST
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect()
            }),
            subprocess_timeout_ms: self
                .subprocess_timeout_ms
                .unwrap_or(DEFAULT_SAMBA_SUBPROCESS_TIMEOUT_MS),
            publisher: StdMutex::new(None),
            now_fn: self.now_fn.unwrap_or_else(|| Arc::new(default_now_ms)),
        }
    }
}

// --------------------------------------------------------------
// Subject publisher wiring (Ship 3)
// --------------------------------------------------------------

impl SambaServerRuntime {
    /// Attach a [`SubjectAnnouncer`]. Called once at wiring
    /// time. Announces `system_smb_server` with the current
    /// state; subsequent apply / add_user / revoke_user
    /// transitions schedule fire-and-forget republishes.
    pub async fn attach_subject_publisher(
        &self,
        announcer: Arc<dyn SubjectAnnouncer>,
    ) -> Result<(), evo_plugin_sdk::contract::ReportError> {
        let envelope = self.compose_envelope().await;
        announcer
            .announce(SubjectAnnouncement {
                subject_type: SYSTEM_SMB_SERVER_SUBJECT_TYPE.to_string(),
                addressings: vec![system_smb_server_addressing()],
                claims: Vec::new(),
                state: serde_json::to_value(&envelope).unwrap_or_else(|_| {
                    serde_json::Value::Object(serde_json::Map::new())
                }),
                announced_at: SystemTime::now(),
            })
            .await?;
        let mut slot = self.publisher.lock().expect(
            "SambaServerRuntime publisher slot mutex poisoned at attach",
        );
        *slot = Some(SambaPublisher { announcer });
        Ok(())
    }

    async fn compose_envelope(&self) -> SystemSmbServerEnvelope {
        let g = self.inner.lock().await;
        SystemSmbServerEnvelope {
            enabled: g.state.enabled,
            min_protocol: g.state.min_protocol.as_wire_str().to_string(),
            extra_shares: g.state.extra_shares.clone(),
            smb_users: g
                .state
                .smb_users
                .iter()
                .map(SmbUserPublic::from)
                .collect(),
            last_apply_at_ms: g.state.last_apply_at_ms,
            last_update_at: SystemTime::now(),
        }
    }

    fn take_publisher(&self) -> Option<Arc<dyn SubjectAnnouncer>> {
        let slot = self
            .publisher
            .lock()
            .expect("SambaServerRuntime publisher slot mutex poisoned at read");
        slot.as_ref().map(|p| Arc::clone(&p.announcer))
    }

    async fn schedule_republish(&self) {
        let Some(announcer) = self.take_publisher() else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let envelope = self.compose_envelope().await;
        handle.spawn(async move {
            let state = serde_json::to_value(&envelope).unwrap_or_else(|_| {
                serde_json::Value::Object(serde_json::Map::new())
            });
            if let Err(e) = announcer
                .update_state(system_smb_server_addressing(), state)
                .await
            {
                tracing::debug!(
                    error = %e,
                    "system_smb_server republish failed"
                );
            }
        });
    }
}

// --------------------------------------------------------------
// Operator verb dispatch (Ship 3)
// --------------------------------------------------------------

/// Request payload for `network.smb_server.apply`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmbServerApplyRequest {
    /// New enabled state.
    pub enabled: bool,
    /// New `server min protocol`.
    pub min_protocol: MinProtocol,
    /// New extra_shares list (full replacement, not a delta).
    pub extra_shares: Vec<ExtraShare>,
    /// New system hostname. When `Some`, the runtime runs
    /// `sudo -n /usr/bin/hostnamectl set-hostname <value>` before
    /// re-rendering smb.conf so both the OS hostname and the SMB
    /// netbios advertisement update in one operator gesture. When
    /// `None` (default for callers that pre-date the field), the
    /// existing hostname stays put.
    ///
    /// avahi-daemon subscribes to hostnamed's D-Bus signals on
    /// standard Debian and re-advertises mDNS automatically on
    /// hostname change; no explicit avahi reload is issued here.
    ///
    /// Refused when the string is empty or contains chars outside
    /// the RFC 1123 hostname set (letters / digits / `-`, must
    /// start + end with letter/digit) — hostnamectl would reject
    /// anyway; refusing early gives the operator a clearer error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_hostname: Option<String>,
}

/// Response payload for `network.smb_server.apply`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmbServerApplyResponse {
    /// The apply outcome.
    pub report: ApplyReport,
}

/// Request payload for `network.smb_server.user_add`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmbServerUserAddRequest {
    /// SMB username to create.
    pub username: String,
    /// Credential-vault key for the password (never on the wire).
    pub credential_key: String,
    /// Optional canonical domain identity to link.
    #[serde(default)]
    pub mapped_domain_identity: Option<String>,
}

/// Response payload for `network.smb_server.user_add`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmbServerUserAddResponse {
    /// Public fields of the created record (credential_key
    /// omitted).
    pub record: SmbUserPublic,
}

/// Request payload for `network.smb_server.user_revoke`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmbServerUserRevokeRequest {
    /// SMB username to remove.
    pub username: String,
}

/// Response payload for `network.smb_server.user_revoke`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmbServerUserRevokeResponse {}

/// Verb-dispatch specific errors.
#[derive(Debug, thiserror::Error)]
pub enum VerbDispatchError {
    /// The request_type did not match any known verb.
    #[error("unknown request_type: {request_type}")]
    UnknownRequestType {
        /// The request_type the caller supplied.
        request_type: String,
    },
    /// Payload undecodable into the expected envelope.
    #[error("payload decode failed for {request_type}: {detail}")]
    PayloadDecode {
        /// The verb whose payload was undecodable.
        request_type: String,
        /// serde error text.
        detail: String,
    },
    /// Underlying apply/user operation failed.
    #[error("verb execution failed: {0}")]
    Apply(#[from] ApplyError),
    /// Response serialise failed.
    #[error("response serialise failed for {request_type}: {detail}")]
    ResponseSerialise {
        /// The verb whose response failed to serialise.
        request_type: String,
        /// serde error text.
        detail: String,
    },
}

/// The set of request_type strings this runtime dispatches.
pub const SMB_SERVER_VERBS: &[&str] = &[
    "network.smb_server.apply",
    "network.smb_server.user_add",
    "network.smb_server.user_revoke",
    // Read verb (read-then-subscribe seed for UI consumers).
    "network.smb_server.get_state",
];

/// Response payload for `network.smb_server.get_state`. Carries
/// the exact envelope shape the `system_smb_server` subject
/// publishes; `smb_users` uses the public-fields projection so
/// credential_key never appears on the wire.
#[derive(Debug, Clone, Serialize)]
pub struct GetSmbServerStateResponse {
    /// Snapshot of the system-SMB-server envelope.
    pub envelope: SystemSmbServerEnvelope,
}

/// Whether the runtime dispatches this request_type.
pub fn is_smb_server_verb(request_type: &str) -> bool {
    SMB_SERVER_VERBS.contains(&request_type)
}

/// Change the OS hostname via `sudo -n /usr/bin/hostnamectl
/// set-hostname <name>`. Best-effort:
///
///   - Refuses empty / RFC-1123-invalid names early with a
///     WARN log; the sudoers grant would refuse the same value
///     but the early check gives the operator a clearer error.
///   - Non-zero exit from hostnamectl logs at WARN and returns
///     (the caller continues with the rest of the apply).
///   - Missing sudo grant / hostnamectl surfaces as non-zero
///     exit with stderr in the log.
///
/// avahi-daemon picks up the hostname change via its D-Bus
/// subscription to systemd-hostnamed on standard Debian and
/// re-advertises mDNS without explicit reload. No SMB restart
/// is issued here — `SambaServerRuntime::apply` re-renders
/// smb.conf after this call and, if `enabled = true`, restarts
/// smbd on the render change.
async fn apply_system_hostname_best_effort(name: &str) {
    if !is_rfc1123_hostname(name) {
        tracing::warn!(
            plugin = crate::PLUGIN_NAME,
            hostname = %name,
            "system_hostname refused: not a valid RFC-1123 hostname \
             (letters / digits / hyphens; must start + end with letter/digit; \
             1..=63 chars per label)"
        );
        return;
    }
    let output = tokio::process::Command::new("/usr/bin/sudo")
        .arg("-n")
        .arg("/usr/bin/hostnamectl")
        .arg("set-hostname")
        .arg(name)
        .output()
        .await;
    match output {
        Ok(o) if o.status.success() => {
            tracing::info!(
                plugin = crate::PLUGIN_NAME,
                hostname = %name,
                "system hostname set via hostnamectl; avahi re-advertises \
                 mDNS on hostnamed D-Bus signal"
            );
        }
        Ok(o) => {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                hostname = %name,
                exit_code = ?o.status.code(),
                stderr = %String::from_utf8_lossy(&o.stderr),
                "hostnamectl set-hostname exited non-zero (check sudoers \
                 grant for /usr/bin/hostnamectl set-hostname *)"
            );
        }
        Err(e) => {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                hostname = %name,
                error = %e,
                "hostnamectl dispatch failed"
            );
        }
    }
}

/// RFC 1123 hostname validator (single label — hostnamectl
/// accepts a single label, not an FQDN, for `set-hostname`).
/// Rules: 1..=63 chars, letters/digits/hyphens, must start and
/// end with letter/digit.
fn is_rfc1123_hostname(name: &str) -> bool {
    let len = name.len();
    if !(1..=63).contains(&len) {
        return false;
    }
    let bytes = name.as_bytes();
    let is_alnum = |b: u8| b.is_ascii_alphanumeric();
    let is_alnum_or_hyphen = |b: u8| is_alnum(b) || b == b'-';
    if !is_alnum(bytes[0]) || !is_alnum(bytes[len - 1]) {
        return false;
    }
    bytes.iter().all(|&b| is_alnum_or_hyphen(b))
}

impl SambaServerRuntime {
    /// Route an operator wire verb to the appropriate handle
    /// method.
    pub async fn dispatch_verb(
        &self,
        request_type: &str,
        payload_bytes: &[u8],
    ) -> Result<Vec<u8>, VerbDispatchError> {
        match request_type {
            "network.smb_server.apply" => {
                let req: SmbServerApplyRequest =
                    decode_payload(request_type, payload_bytes)?;
                // Hostname change lands FIRST so the smb.conf
                // render below (via `apply`) picks up any
                // netbios-follows-hostname convention on the
                // operator's chosen name. Best-effort — failure
                // to change hostname does not block the rest of
                // the apply (extra_shares / enabled / min_
                // protocol still commit). The operator sees a
                // clear log line if the sudoers grant is missing
                // or hostnamectl refuses the value.
                if let Some(name) = req.system_hostname.as_deref() {
                    apply_system_hostname_best_effort(name).await;
                }
                let report = self
                    .apply(req.enabled, req.min_protocol, req.extra_shares)
                    .await?;
                encode_response(
                    request_type,
                    &SmbServerApplyResponse { report },
                )
            }
            "network.smb_server.user_add" => {
                let req: SmbServerUserAddRequest =
                    decode_payload(request_type, payload_bytes)?;
                let record = self
                    .add_user(
                        req.username,
                        req.credential_key,
                        req.mapped_domain_identity,
                    )
                    .await?;
                encode_response(
                    request_type,
                    &SmbServerUserAddResponse {
                        record: SmbUserPublic::from(&record),
                    },
                )
            }
            "network.smb_server.user_revoke" => {
                let req: SmbServerUserRevokeRequest =
                    decode_payload(request_type, payload_bytes)?;
                self.revoke_user(&req.username).await?;
                encode_response(request_type, &SmbServerUserRevokeResponse {})
            }
            "network.smb_server.get_state" => {
                let envelope = self.compose_envelope().await;
                encode_response(
                    request_type,
                    &GetSmbServerStateResponse { envelope },
                )
            }
            other => Err(VerbDispatchError::UnknownRequestType {
                request_type: other.to_string(),
            }),
        }
    }
}

fn decode_payload<T: for<'de> Deserialize<'de>>(
    request_type: &str,
    bytes: &[u8],
) -> Result<T, VerbDispatchError> {
    serde_json::from_slice(bytes).map_err(|e| {
        VerbDispatchError::PayloadDecode {
            request_type: request_type.to_string(),
            detail: e.to_string(),
        }
    })
}

fn encode_response<T: Serialize>(
    request_type: &str,
    response: &T,
) -> Result<Vec<u8>, VerbDispatchError> {
    serde_json::to_vec(response).map_err(|e| {
        VerbDispatchError::ResponseSerialise {
            request_type: request_type.to_string(),
            detail: e.to_string(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_plugin_sdk::contract::ReportError;
    use std::collections::HashMap;
    use std::future::Future;
    use std::pin::Pin;

    fn tempdir() -> PathBuf {
        let mut base = std::env::temp_dir();
        base.push(format!("evo-samba-server-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    // ----- Persistence tests -----

    #[test]
    fn empty_state_round_trips_through_toml() {
        let state = SmbServerState::empty();
        let text = toml::to_string_pretty(&state).unwrap();
        let parsed: SmbServerState = toml::from_str(&text).unwrap();
        assert_eq!(parsed, state);
    }

    #[test]
    fn state_with_shares_and_users_round_trips() {
        let state = SmbServerState {
            schema_version: SMB_SERVER_SCHEMA_VERSION,
            enabled: true,
            min_protocol: MinProtocol::Smb3_02,
            extra_shares: vec![ExtraShare {
                name: "Studio".to_string(),
                path: "/mnt/NAS/studio".to_string(),
                guest_ok: false,
            }],
            smb_users: vec![SmbUserRecord {
                username: "producer".to_string(),
                mapped_domain_identity: Some("device-42".to_string()),
                created_at_ms: 1_700_000_777_000,
                credential_key: "vault:smb:producer".to_string(),
            }],
            last_apply_at_ms: Some(1_700_000_888_000),
        };
        let text = toml::to_string_pretty(&state).unwrap();
        let parsed: SmbServerState = toml::from_str(&text).unwrap();
        assert_eq!(parsed, state);
    }

    #[test]
    fn load_missing_file_returns_empty_state() {
        let dir = tempdir();
        let state =
            SmbServerState::load(&dir.join("does-not-exist.toml")).unwrap();
        assert_eq!(state, SmbServerState::empty());
    }

    #[test]
    fn schema_version_mismatch_errors_on_load() {
        let dir = tempdir();
        let path = dir.join(SMB_SERVER_FILE);
        std::fs::write(
            &path,
            "schema_version = 999\nenabled = false\nmin_protocol = \"default\"\n",
        )
        .unwrap();
        let err = SmbServerState::load(&path).unwrap_err();
        assert!(matches!(
            err,
            SmbServerStateError::SchemaVersionMismatch { found: 999, .. }
        ));
    }

    // ----- Renderer tests -----

    #[test]
    fn render_smb_conf_emits_global_stanza_and_extra_share() {
        let state = SmbServerState {
            schema_version: SMB_SERVER_SCHEMA_VERSION,
            enabled: true,
            min_protocol: MinProtocol::Smb3_02,
            extra_shares: vec![ExtraShare {
                name: "Uploads".to_string(),
                path: "/var/lib/evo/uploads".to_string(),
                guest_ok: true,
            }],
            smb_users: Vec::new(),
            last_apply_at_ms: None,
        };
        let allowlist: Vec<String> = DEFAULT_SHARE_PATH_ALLOWLIST
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let (rendered, applied, refused) =
            render_smb_conf(&state, "EvoTest", "STUDIO", &allowlist);
        assert!(rendered.contains("[global]"));
        assert!(rendered.contains("netbios name = EvoTest"));
        assert!(rendered.contains("workgroup = STUDIO"));
        assert!(rendered.contains("server min protocol = SMB3_02"));
        assert!(rendered.contains("[Uploads]"));
        assert!(rendered.contains("path = /var/lib/evo/uploads"));
        assert!(rendered.contains("guest ok = yes"));
        assert_eq!(applied, vec!["Uploads".to_string()]);
        assert!(refused.is_empty());
    }

    #[test]
    fn render_smb_conf_refuses_path_outside_allowlist() {
        let state = SmbServerState {
            schema_version: SMB_SERVER_SCHEMA_VERSION,
            enabled: true,
            min_protocol: MinProtocol::Default,
            extra_shares: vec![
                ExtraShare {
                    name: "Ok".to_string(),
                    path: "/mnt/NAS/family".to_string(),
                    guest_ok: false,
                },
                ExtraShare {
                    name: "Nope".to_string(),
                    path: "/etc/shadow".to_string(),
                    guest_ok: false,
                },
            ],
            smb_users: Vec::new(),
            last_apply_at_ms: None,
        };
        let allowlist = vec!["/mnt/NAS".to_string()];
        let (rendered, applied, refused) =
            render_smb_conf(&state, "EvoTest", "WG", &allowlist);
        assert!(rendered.contains("[Ok]"));
        assert!(!rendered.contains("[Nope]"));
        assert_eq!(applied, vec!["Ok".to_string()]);
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].setting, "Nope");
    }

    #[test]
    fn render_smb_conf_default_min_protocol_suppresses_directive() {
        let state = SmbServerState::empty();
        let (rendered, _, _) =
            render_smb_conf(&state, "EvoTest", "WG", &["/mnt/NAS".to_string()]);
        assert!(!rendered.contains("server min protocol"));
    }

    #[test]
    fn path_matches_allowlist_handles_trailing_slashes_and_exact() {
        let allow = vec!["/mnt/NAS/".to_string(), "/data/INTERNAL".to_string()];
        assert!(path_matches_allowlist("/mnt/NAS", &allow));
        assert!(path_matches_allowlist("/mnt/NAS/family", &allow));
        assert!(path_matches_allowlist("/data/INTERNAL", &allow));
        assert!(path_matches_allowlist("/data/INTERNAL/x", &allow));
        assert!(!path_matches_allowlist("/mnt/NASfoo", &allow));
        assert!(!path_matches_allowlist("/etc/shadow", &allow));
    }

    // ----- argv builder tests -----

    #[test]
    fn testparm_args_shape() {
        assert_eq!(
            build_testparm_args(Path::new("/tmp/smb.conf.candidate")),
            vec!["-s".to_string(), "/tmp/smb.conf.candidate".to_string()]
        );
    }

    #[test]
    fn smbpasswd_add_args_shape() {
        assert_eq!(
            build_smbpasswd_add_args("producer"),
            vec!["-a".to_string(), "-s".to_string(), "producer".to_string(),]
        );
    }

    #[test]
    fn smbpasswd_delete_args_shape() {
        assert_eq!(
            build_smbpasswd_delete_args("producer"),
            vec!["-x".to_string(), "producer".to_string()]
        );
    }

    #[test]
    fn systemctl_restart_args_shape() {
        assert_eq!(
            build_systemctl_restart_args(),
            vec!["restart".to_string(), "smbd".to_string()]
        );
    }

    // ----- Scripted executor + credential stub -----

    type RecordedSambaCall = (String, Vec<String>, Vec<u8>);
    type RecordedWrite = (PathBuf, Vec<u8>);

    struct ScriptedSamba {
        outputs: Arc<Mutex<Vec<CommandOutput>>>,
        calls: Arc<Mutex<Vec<RecordedSambaCall>>>,
        writes: Arc<Mutex<Vec<RecordedWrite>>>,
    }

    impl ScriptedSamba {
        fn new(outputs: Vec<CommandOutput>) -> Arc<Self> {
            Arc::new(Self {
                outputs: Arc::new(Mutex::new(outputs)),
                calls: Arc::new(Mutex::new(Vec::new())),
                writes: Arc::new(Mutex::new(Vec::new())),
            })
        }
    }

    #[async_trait]
    impl SambaExecutor for ScriptedSamba {
        async fn run(
            &self,
            program: &str,
            args: &[String],
            _timeout_ms: u64,
            stdin_bytes: &[u8],
        ) -> Result<CommandOutput, ApplyError> {
            self.calls.lock().await.push((
                program.to_string(),
                args.to_vec(),
                stdin_bytes.to_vec(),
            ));
            let mut outs = self.outputs.lock().await;
            if outs.is_empty() {
                Ok(CommandOutput {
                    exit_code: Some(0),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            } else {
                Ok(outs.remove(0))
            }
        }

        async fn write_file(
            &self,
            path: &Path,
            contents: &[u8],
        ) -> Result<(), ApplyError> {
            self.writes
                .lock()
                .await
                .push((path.to_path_buf(), contents.to_vec()));
            Ok(())
        }
    }

    struct StubCredentials {
        map: HashMap<String, Vec<u8>>,
    }

    #[async_trait]
    impl SmbCredentialFetcher for StubCredentials {
        async fn fetch_password(&self, key: &str) -> Option<Vec<u8>> {
            self.map.get(key).cloned()
        }
    }

    fn ok() -> CommandOutput {
        CommandOutput {
            exit_code: Some(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    fn err_output() -> CommandOutput {
        CommandOutput {
            exit_code: Some(1),
            stdout: Vec::new(),
            stderr: b"failure".to_vec(),
        }
    }

    fn built_runtime(
        dir: &Path,
        outputs: Vec<CommandOutput>,
        creds: HashMap<String, Vec<u8>>,
    ) -> (SambaServerRuntime, Arc<ScriptedSamba>) {
        let executor = ScriptedSamba::new(outputs);
        let rt = SambaServerRuntime::builder(dir)
            .unwrap()
            .with_executor(executor.clone())
            .with_credentials(Arc::new(StubCredentials { map: creds }))
            .with_testparm_program("testparm".to_string())
            .with_smbpasswd_program("smbpasswd".to_string())
            .with_systemctl_program("systemctl".to_string())
            .with_smb_conf_path(dir.join("smb.conf"))
            .with_path_allowlist(vec!["/mnt/NAS".to_string()])
            .with_netbios_name("EvoTest".to_string())
            .with_workgroup("STUDIO".to_string())
            .with_subprocess_timeout_ms(1_000)
            .with_now_fn(Arc::new(|| 1_700_000_777_000))
            .build();
        (rt, executor)
    }

    // ----- Runtime tests -----

    #[tokio::test]
    async fn apply_success_writes_conf_runs_testparm_and_restarts() {
        let dir = tempdir();
        let (rt, executor) = built_runtime(
            &dir,
            vec![ok(), ok()], // testparm + systemctl
            HashMap::new(),
        );
        let report = rt
            .apply(
                true,
                MinProtocol::Smb3_02,
                vec![ExtraShare {
                    name: "Music".to_string(),
                    path: "/mnt/NAS/music".to_string(),
                    guest_ok: false,
                }],
            )
            .await
            .unwrap();
        assert_eq!(report.applied_shares, vec!["Music".to_string()]);
        assert!(report.refused_settings.is_empty());
        assert!(report.smbd_restarted);
        let calls = executor.calls.lock().await;
        assert_eq!(calls.len(), 2);
        assert!(calls[0].0.ends_with("testparm"));
        assert!(calls[1].0.ends_with("systemctl"));
        let writes = executor.writes.lock().await;
        assert_eq!(writes.len(), 2);
    }

    #[tokio::test]
    async fn apply_when_disabled_skips_systemctl_restart() {
        let dir = tempdir();
        let (rt, executor) = built_runtime(&dir, vec![ok()], HashMap::new()); // testparm only
        let report = rt
            .apply(false, MinProtocol::Default, Vec::new())
            .await
            .unwrap();
        assert!(!report.smbd_restarted);
        let calls = executor.calls.lock().await;
        assert_eq!(calls.len(), 1);
    }

    #[tokio::test]
    async fn apply_testparm_failure_returns_testparm_error() {
        let dir = tempdir();
        let (rt, _) = built_runtime(&dir, vec![err_output()], HashMap::new());
        let err = rt
            .apply(true, MinProtocol::Default, Vec::new())
            .await
            .unwrap_err();
        assert!(matches!(err, ApplyError::TestparmFailed { .. }));
    }

    #[tokio::test]
    async fn apply_restart_failure_returns_restart_error() {
        let dir = tempdir();
        let (rt, _) = built_runtime(
            &dir,
            vec![ok(), err_output()], // testparm ok, restart fail
            HashMap::new(),
        );
        let err = rt
            .apply(true, MinProtocol::Default, Vec::new())
            .await
            .unwrap_err();
        assert!(matches!(err, ApplyError::RestartFailed { .. }));
    }

    #[tokio::test]
    async fn apply_refuses_path_outside_allowlist_but_still_writes_config() {
        let dir = tempdir();
        let (rt, executor) =
            built_runtime(&dir, vec![ok(), ok()], HashMap::new());
        let report = rt
            .apply(
                true,
                MinProtocol::Default,
                vec![
                    ExtraShare {
                        name: "Ok".to_string(),
                        path: "/mnt/NAS/x".to_string(),
                        guest_ok: false,
                    },
                    ExtraShare {
                        name: "Nope".to_string(),
                        path: "/etc/shadow".to_string(),
                        guest_ok: false,
                    },
                ],
            )
            .await
            .unwrap();
        assert_eq!(report.applied_shares, vec!["Ok".to_string()]);
        assert_eq!(report.refused_settings.len(), 1);
        assert_eq!(report.refused_settings[0].setting, "Nope");
        let writes = executor.writes.lock().await;
        // Written file does not include the refused share.
        let text = String::from_utf8_lossy(&writes[0].1);
        assert!(text.contains("[Ok]"));
        assert!(!text.contains("[Nope]"));
    }

    #[tokio::test]
    async fn add_user_pipes_password_twice_and_persists_record() {
        let dir = tempdir();
        let mut creds = HashMap::new();
        creds.insert("vault:smb:producer".to_string(), b"s3cret".to_vec());
        let (rt, executor) = built_runtime(&dir, vec![ok()], creds);
        let record = rt
            .add_user(
                "producer".to_string(),
                "vault:smb:producer".to_string(),
                Some("device-42".to_string()),
            )
            .await
            .unwrap();
        assert_eq!(record.username, "producer");
        assert_eq!(record.credential_key, "vault:smb:producer");
        let calls = executor.calls.lock().await;
        assert_eq!(calls.len(), 1);
        // Password piped twice separated by newlines.
        assert_eq!(calls[0].2, b"s3cret\ns3cret\n".to_vec());
        // Persisted.
        let state = rt.get_state().await;
        assert_eq!(state.smb_users.len(), 1);
        assert_eq!(state.smb_users[0].username, "producer");
    }

    #[tokio::test]
    async fn add_user_missing_credential_key_returns_credential_missing() {
        let dir = tempdir();
        let (rt, _) = built_runtime(&dir, Vec::new(), HashMap::new());
        let err = rt
            .add_user(
                "producer".to_string(),
                "vault:smb:producer".to_string(),
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ApplyError::CredentialMissing { .. }));
    }

    #[tokio::test]
    async fn add_user_duplicate_returns_user_already_exists() {
        let dir = tempdir();
        let mut creds = HashMap::new();
        creds.insert("k".to_string(), b"pw".to_vec());
        let (rt, _) = built_runtime(&dir, vec![ok()], creds);
        rt.add_user("dup".to_string(), "k".to_string(), None)
            .await
            .unwrap();
        let err = rt
            .add_user("dup".to_string(), "k".to_string(), None)
            .await
            .unwrap_err();
        assert!(matches!(err, ApplyError::UserAlreadyExists { .. }));
    }

    #[tokio::test]
    async fn revoke_user_removes_persisted_record() {
        let dir = tempdir();
        let mut creds = HashMap::new();
        creds.insert("k".to_string(), b"pw".to_vec());
        let (rt, _) = built_runtime(&dir, vec![ok(), ok()], creds);
        rt.add_user("target".to_string(), "k".to_string(), None)
            .await
            .unwrap();
        rt.revoke_user("target").await.unwrap();
        assert!(rt.get_state().await.smb_users.is_empty());
    }

    #[tokio::test]
    async fn revoke_user_unknown_returns_user_not_found() {
        let dir = tempdir();
        let (rt, _) = built_runtime(&dir, Vec::new(), HashMap::new());
        let err = rt.revoke_user("ghost").await.unwrap_err();
        assert!(matches!(err, ApplyError::UserNotFound { .. }));
    }

    // ----- Subject publisher tests -----

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum AnnouncerCall {
        Announce(ExternalAddressing),
        UpdateState(ExternalAddressing),
    }

    struct RecordingAnnouncer {
        calls: Arc<StdMutex<Vec<AnnouncerCall>>>,
        latest: Arc<StdMutex<HashMap<ExternalAddressing, serde_json::Value>>>,
    }

    impl RecordingAnnouncer {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Arc::new(StdMutex::new(Vec::new())),
                latest: Arc::new(StdMutex::new(HashMap::new())),
            })
        }

        fn calls(&self) -> Vec<AnnouncerCall> {
            self.calls.lock().unwrap().clone()
        }

        fn latest_of(
            &self,
            a: &ExternalAddressing,
        ) -> Option<serde_json::Value> {
            self.latest.lock().unwrap().get(a).cloned()
        }
    }

    impl SubjectAnnouncer for RecordingAnnouncer {
        fn announce<'a>(
            &'a self,
            announcement: SubjectAnnouncement,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            let addressing = announcement.addressings[0].clone();
            self.calls
                .lock()
                .unwrap()
                .push(AnnouncerCall::Announce(addressing.clone()));
            self.latest
                .lock()
                .unwrap()
                .insert(addressing, announcement.state);
            Box::pin(async { Ok(()) })
        }

        fn retract<'a>(
            &'a self,
            _addressing: ExternalAddressing,
            _reason: Option<String>,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            Box::pin(async { Ok(()) })
        }

        fn update_state<'a>(
            &'a self,
            addressing: ExternalAddressing,
            state: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            self.calls
                .lock()
                .unwrap()
                .push(AnnouncerCall::UpdateState(addressing.clone()));
            self.latest.lock().unwrap().insert(addressing, state);
            Box::pin(async { Ok(()) })
        }
    }

    async fn drain() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn attach_publisher_announces_singleton_with_current_state() {
        let dir = tempdir();
        let (rt, _) = built_runtime(&dir, Vec::new(), HashMap::new());
        let announcer = RecordingAnnouncer::new();
        rt.attach_subject_publisher(announcer.clone())
            .await
            .unwrap();
        let calls = announcer.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0],
            AnnouncerCall::Announce(system_smb_server_addressing())
        );
    }

    #[tokio::test]
    async fn apply_republishes_subject_with_new_state() {
        let dir = tempdir();
        let (rt, _) = built_runtime(&dir, vec![ok(), ok()], HashMap::new());
        let announcer = RecordingAnnouncer::new();
        rt.attach_subject_publisher(announcer.clone())
            .await
            .unwrap();
        rt.apply(
            true,
            MinProtocol::Smb3_02,
            vec![ExtraShare {
                name: "Music".to_string(),
                path: "/mnt/NAS/music".to_string(),
                guest_ok: false,
            }],
        )
        .await
        .unwrap();
        drain().await;
        let latest = announcer
            .latest_of(&system_smb_server_addressing())
            .unwrap();
        assert_eq!(latest["enabled"], serde_json::json!(true));
        assert_eq!(latest["min_protocol"], serde_json::json!("smb3_02"));
        assert_eq!(
            latest["extra_shares"][0]["name"],
            serde_json::json!("Music")
        );
    }

    #[tokio::test]
    async fn add_user_omits_credential_key_from_subject_envelope() {
        let dir = tempdir();
        let mut creds = HashMap::new();
        creds.insert("k".to_string(), b"pw".to_vec());
        let (rt, _) = built_runtime(&dir, vec![ok()], creds);
        let announcer = RecordingAnnouncer::new();
        rt.attach_subject_publisher(announcer.clone())
            .await
            .unwrap();
        rt.add_user("op".to_string(), "k".to_string(), None)
            .await
            .unwrap();
        drain().await;
        let latest = announcer
            .latest_of(&system_smb_server_addressing())
            .unwrap();
        let user = &latest["smb_users"][0];
        assert_eq!(user["username"], serde_json::json!("op"));
        assert!(user.get("credential_key").is_none());
    }

    // ----- Verb dispatch tests -----

    #[test]
    fn is_smb_server_verb_recognises_declared_ops() {
        for op in SMB_SERVER_VERBS {
            assert!(is_smb_server_verb(op));
        }
        assert!(!is_smb_server_verb("network.smb_server.bogus"));
    }

    #[tokio::test]
    async fn dispatch_verb_unknown_returns_error() {
        let dir = tempdir();
        let (rt, _) = built_runtime(&dir, Vec::new(), HashMap::new());
        let err = rt
            .dispatch_verb("network.smb_server.bogus", b"{}")
            .await
            .unwrap_err();
        assert!(matches!(err, VerbDispatchError::UnknownRequestType { .. }));
    }

    #[tokio::test]
    async fn dispatch_verb_apply_round_trips() {
        let dir = tempdir();
        let (rt, _) = built_runtime(&dir, vec![ok(), ok()], HashMap::new());
        let req = SmbServerApplyRequest {
            enabled: true,
            min_protocol: MinProtocol::Smb3_02,
            extra_shares: vec![ExtraShare {
                name: "Uploads".to_string(),
                path: "/mnt/NAS/uploads".to_string(),
                guest_ok: true,
            }],
            system_hostname: None,
        };
        let payload = serde_json::to_vec(&req).unwrap();
        let bytes = rt
            .dispatch_verb("network.smb_server.apply", &payload)
            .await
            .unwrap();
        let resp: SmbServerApplyResponse =
            serde_json::from_slice(&bytes).unwrap();
        assert!(resp.report.smbd_restarted);
    }

    #[tokio::test]
    async fn dispatch_verb_user_add_round_trips() {
        let dir = tempdir();
        let mut creds = HashMap::new();
        creds.insert("k".to_string(), b"pw".to_vec());
        let (rt, _) = built_runtime(&dir, vec![ok()], creds);
        let req = SmbServerUserAddRequest {
            username: "op".to_string(),
            credential_key: "k".to_string(),
            mapped_domain_identity: None,
        };
        let payload = serde_json::to_vec(&req).unwrap();
        let bytes = rt
            .dispatch_verb("network.smb_server.user_add", &payload)
            .await
            .unwrap();
        let resp: SmbServerUserAddResponse =
            serde_json::from_slice(&bytes).unwrap();
        assert_eq!(resp.record.username, "op");
    }

    #[tokio::test]
    async fn dispatch_verb_user_revoke_round_trips() {
        let dir = tempdir();
        let mut creds = HashMap::new();
        creds.insert("k".to_string(), b"pw".to_vec());
        let (rt, _) = built_runtime(&dir, vec![ok(), ok()], creds);
        rt.add_user("op".to_string(), "k".to_string(), None)
            .await
            .unwrap();
        let req = SmbServerUserRevokeRequest {
            username: "op".to_string(),
        };
        let payload = serde_json::to_vec(&req).unwrap();
        let bytes = rt
            .dispatch_verb("network.smb_server.user_revoke", &payload)
            .await
            .unwrap();
        let _: SmbServerUserRevokeResponse =
            serde_json::from_slice(&bytes).unwrap();
    }
}
