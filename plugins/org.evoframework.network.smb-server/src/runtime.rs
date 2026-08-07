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
//!   `workgroup`) and a path allow/deny pair. Stock music
//!   shares + delivery shares render whenever the server is
//!   enabled; operator `extra_shares` render only when they
//!   satisfy the allowlist AND do not trip the denylist.
//!   Denylist beats allowlist. See the normative inventory
//!   at `docs/SAMBA-SHARES.md`.
//! * Validates the rendered config with `testparm -s` before
//!   installing over `/etc/samba/smb.conf` and restarting
//!   `smbd.service`.
//! * Adds / revokes SMB users via the narrow
//!   `evo-smb-user-sync` wrapper (non-login NSS account +
//!   Samba passdb). Passwords never appear on the persistence
//!   surface — the record carries a `credential_key` into the
//!   evo credential vault; the vault issues bytes at add time
//!   and the runtime pipes them once into the wrapper's stdin.
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
/// testparm + user-sync + systemctl restart on the reference
/// Pi 5).
pub const DEFAULT_SAMBA_SUBPROCESS_TIMEOUT_MS: u64 = 10_000;

/// Default extra_share path allowlist for the audio
/// distribution. Paths outside these prefixes are refused at
/// apply time. Matches the normative inventory in
/// `docs/SAMBA-SHARES.md` — the evo music plane, the uploads
/// root, and the plugin stage. Classic Volumio prefixes
/// (`/data/INTERNAL`, `/mnt/USB`, `/mnt/NAS`) are deliberately
/// absent — the audio distribution's music plane is
/// `/var/lib/evo/music/{INTERNAL,USB,NAS}`. Vendor
/// distributions with a different layout override via
/// [`SambaServerRuntimeBuilder::with_path_allowlist`].
pub const DEFAULT_SHARE_PATH_ALLOWLIST: &[&str] = &[
    "/var/lib/evo/music",
    "/var/lib/evo/uploads",
    "/var/lib/evo/plugins/stage",
];

/// Default extra_share path DENYlist. Paths matched here are
/// refused at apply time even when they sit under an
/// allowlisted root — the operator cannot expose the
/// steward's secrets directory or the plugin-stage rejection
/// dumping ground as an SMB share. Matches the denylist table
/// in `docs/SAMBA-SHARES.md`.
///
/// Denylist wins over allowlist: refusal fires the moment any
/// prefix here matches, even when the same path also matches
/// an allowlist prefix.
pub const DEFAULT_SHARE_PATH_DENYLIST: &[&str] = &[
    "/var/lib/evo/settings",
    "/var/lib/evo/plugins/stage/rejected",
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
    /// password bytes for `evo-smb-user-sync add` stdin.
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
    /// renames. Sets file mode `0o600` after the rename — the
    /// state file carries per-user `credential_key` fields that
    /// name entries in the framework credential vault. The
    /// steward's plugin state directory is already `0o700`, so
    /// the file mode is defence-in-depth against a hypothetical
    /// perm-widen on the parent, not the sole barrier.
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            fs::set_permissions(path, perms).map_err(|e| {
                SmbServerStateError::Io {
                    detail: e.to_string(),
                }
            })?;
        }
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
    /// `evo-smb-user-sync add|delete` returned non-zero.
    #[error(
        "smb user sync failed for {username}: exit={exit_code:?}, stderr={stderr}"
    )]
    UserSyncFailed {
        /// The user the sync wrapper targeted.
        username: String,
        /// wrapper exit code.
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
    /// The runtime was constructed with a placeholder fetcher
    /// because `LoadContext::credential_vault` was `None` at
    /// plugin load. `add_user` cannot fetch operator-supplied
    /// passwords in this state; the operator UI renders "SMB
    /// user provisioning unavailable — the framework did not
    /// wire the credential vault for this plugin" rather
    /// than the more ambiguous `CredentialMissing`.
    #[error(
        "SMB user provisioning unavailable: framework credential vault not wired to this plugin"
    )]
    CredentialVaultUnavailable,
    /// The vault-delete step during `revoke_user` failed. The
    /// SMB user (passdb + NSS) has already been revoked
    /// successfully at this point; the vault row is dangling
    /// and requires operator remediation via the credential
    /// admin UI or wire op.
    #[error("credential vault delete failed for key {key}: {detail}")]
    CredentialDeleteFailed {
        /// The credential-vault key the delete targeted.
        key: String,
        /// Human-readable failure text from the vault handle.
        detail: String,
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
    /// `add_user` speculatively persisted the record, then the
    /// wrapper subprocess failed, AND the rollback attempt to
    /// remove the speculative row from `smb_server.toml` also
    /// failed. Both failures are surfaced verbatim so operator
    /// UI can render "add failed AND rollback failed — the
    /// plugin state now carries a phantom row; run `user_revoke`
    /// to reconcile (the wrapper delete + vault delete paths
    /// are idempotent and safe against an absent NSS entry).
    #[error(
        "smb user_add rollback failed for {username}: wrapper stderr={wrapper_stderr}; \
         rollback save error={rollback_detail}"
    )]
    AddRollbackFailed {
        /// The username the caller asked to add.
        username: String,
        /// The wrapper subprocess's stderr (the primary cause).
        wrapper_stderr: String,
        /// The state-save rollback's failure text.
        rollback_detail: String,
    },
    /// Username failed the inventory pattern
    /// (`^[a-z_][a-z0-9_-]{0,31}$` after lowercasing).
    #[error("invalid smb username {username}")]
    InvalidUsername {
        /// The username the caller supplied.
        username: String,
    },
    /// Username is reserved (OS / steward / system identity).
    #[error("smb username {username} is reserved")]
    BlockedUsername {
        /// The username the caller supplied.
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
    /// stdin (used for `evo-smb-user-sync add` password entry).
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

/// Credential store abstraction — the runtime's read + delete
/// surface over whatever backing the distribution's plugin
/// load supplies (framework credential vault in production,
/// stub for tests). `add_user` calls `fetch_password` to read
/// the password bytes the UI staged; `revoke_user` calls
/// `delete_password` so a revoked user's vault row does not
/// linger with a dangling reference.
///
/// The framework vault's own contract on the
/// `CredentialVaultHandle` handle is namespaced per-plugin —
/// the vault-backed impl in production wires the plugin's
/// handle at plugin-load and every read/delete stays scoped
/// to this plugin's rows only.
#[async_trait]
pub trait SmbCredentialFetcher: Send + Sync {
    /// Return the vault entry for `key` (typically the SMB
    /// user's canonical key), or `None` if the vault has no
    /// record.
    async fn fetch_password(&self, key: &str) -> Option<Vec<u8>>;

    /// Remove the vault entry for `key`. Called by
    /// `revoke_user` after a successful passdb delete so the
    /// vault row does not outlive the SMB user. Idempotent by
    /// the vault contract (deleting an already-absent row
    /// succeeds silently).
    ///
    /// Default no-op so tests using the older `StubCredentials`
    /// shape do not need an explicit implementation; production
    /// impls MUST override with the vault's delete call.
    async fn delete_password(&self, _key: &str) -> Result<(), String> {
        Ok(())
    }

    /// Whether this fetcher is backed by an operator-visible
    /// credential store. Returns `false` for the placeholder
    /// fetcher installed when `ctx.credential_vault` is
    /// `None`; runtime `add_user` refuses with
    /// [`ApplyError::CredentialVaultUnavailable`] rather than
    /// the more ambiguous `CredentialMissing` in that case,
    /// so the operator UI can render "vault not wired to
    /// this plugin" instead of "your password did not save".
    fn is_operator_wired(&self) -> bool {
        true
    }
}

/// Placeholder fetcher installed by the plugin's
/// [`crate::SmbServerPlugin::load`] when the LoadContext does
/// not carry a credential vault handle. Every fetch returns
/// `None` and [`Self::is_operator_wired`] is `false` so the
/// runtime's `add_user` path fails with a distinct error class
/// (`CredentialVaultUnavailable`) rather than the generic
/// `CredentialMissing`. Test suites keep using this shape
/// when they exercise the runtime with no vault.
pub struct NoSmbCredentialFetcher;

#[async_trait]
impl SmbCredentialFetcher for NoSmbCredentialFetcher {
    async fn fetch_password(&self, _key: &str) -> Option<Vec<u8>> {
        None
    }

    fn is_operator_wired(&self) -> bool {
        false
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
    path_denylist: &[String],
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
    // Debian's default Samba persona surfaces a printer share
    // (`print$`) driven by CUPS, a `[homes]` section that
    // exposes per-user $HOME (often visible as `nobody` after
    // guest mapping), and a `[printers]` share advertising
    // print queues. None of them are product surfaces on this
    // audio distribution. The `[global]` directives below
    // disable the printer plane wholesale; the plugin's
    // rendered conf omits any `[homes]` / `[printers]`
    // section so Samba does not synthesise them. The
    // `usershare max shares = 0` line disables the parallel
    // per-user share database (`net usershare`) so an
    // operator without root cannot backdoor a share into the
    // running server.
    out.push_str("load printers = no\n");
    out.push_str("printing = bsd\n");
    out.push_str("printcap name = /dev/null\n");
    out.push_str("disable spoolss = yes\n");
    out.push_str("usershare max shares = 0\n");
    // Guest shares below rely on `force user = root` +
    // `force group = root` so writes land under a stable
    // identity regardless of which client (guest or
    // authenticated) sent them. Files land under the music /
    // uploads plane; the steward + MPD run under the service
    // user which is either root or in the file group by
    // bootstrap contract.
    out.push('\n');

    let mut applied: Vec<String> = Vec::new();
    let mut refused: Vec<RefusedSetting> = Vec::new();

    // Stock shares — always rendered when the server is
    // enabled. Order matches `docs/SAMBA-SHARES.md`. Section
    // names carry the operator-facing capitalisation the
    // inventory pins (`Internal Storage`, not
    // `internal-storage`); Samba is case-insensitive on
    // section names but the LAN browse displays the section
    // string verbatim.
    push_stock_share(
        &mut out,
        &mut applied,
        "Internal Storage",
        "/var/lib/evo/music/INTERNAL",
        "evo local music library",
        true,
    );
    push_stock_share(
        &mut out,
        &mut applied,
        "USB",
        "/var/lib/evo/music/USB",
        "evo removable-media library",
        true,
    );
    push_stock_share(
        &mut out,
        &mut applied,
        "NAS",
        "/var/lib/evo/music/NAS",
        "evo NAS mount parent",
        true,
    );

    // Delivery shares — `Uploads` (guest) + `evo-plugins-stage`
    // (authenticated). The authenticated share does not set
    // `guest ok = yes`; smb clients that do not present
    // credentials are refused at the SMB layer and the share
    // does not accept anonymous writes.
    push_stock_share(
        &mut out,
        &mut applied,
        "Uploads",
        "/var/lib/evo/uploads",
        "evo generic upload target",
        true,
    );
    push_stock_share(
        &mut out,
        &mut applied,
        "evo-plugins-stage",
        "/var/lib/evo/plugins/stage",
        "evo plugin bundle stage (framework stage watcher)",
        false,
    );

    // Operator `extra_shares` render after the stock +
    // delivery set. Deny-list beats allow-list: even a path
    // that matches an allowlist root is refused when it also
    // matches a denylist prefix (e.g. an operator trying to
    // expose `/var/lib/evo/settings/` via an `extra_share`).
    for share in &state.extra_shares {
        if let Some(deny) = matched_denylist_prefix(&share.path, path_denylist)
        {
            refused.push(RefusedSetting {
                setting: share.name.clone(),
                reason: format!(
                    "path {} is under a framework denylist prefix ({}) — \
                     shares under secrets / rejected-bundle paths are \
                     never exported",
                    share.path, deny,
                ),
            });
            continue;
        }
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
        out.push_str("        force user = root\n");
        out.push_str("        force group = root\n");
        out.push_str("        create mask = 0664\n");
        out.push_str("        directory mask = 0775\n");
        out.push('\n');
        applied.push(share.name.clone());
    }

    (out, applied, refused)
}

/// Emit one stock or delivery share section into the rendered
/// conf. Shape is identical across every share the inventory
/// pins: `read only = no`, `force user = root`, `force group =
/// root`, `create mask = 0664`, `directory mask = 0775`.
/// `guest ok` is the per-share knob — stock music shares +
/// Uploads are guest-writable; `evo-plugins-stage` is
/// authenticated.
fn push_stock_share(
    out: &mut String,
    applied: &mut Vec<String>,
    name: &str,
    path: &str,
    comment: &str,
    guest_ok: bool,
) {
    out.push_str(&format!("[{name}]\n"));
    out.push_str(&format!("        comment = {comment}\n"));
    out.push_str(&format!("        path = {path}\n"));
    out.push_str("        read only = no\n");
    out.push_str(&format!(
        "        guest ok = {}\n",
        if guest_ok { "yes" } else { "no" },
    ));
    out.push_str("        force user = root\n");
    out.push_str("        force group = root\n");
    out.push_str("        create mask = 0664\n");
    out.push_str("        directory mask = 0775\n");
    out.push('\n');
    applied.push(name.to_string());
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

/// Returns `Some(prefix)` for the FIRST denylist prefix the
/// candidate path matches. Prefix match rules mirror
/// [`path_matches_allowlist`] — exact match or a slash-bounded
/// subpath.
fn matched_denylist_prefix<'a>(
    candidate: &str,
    denylist: &'a [String],
) -> Option<&'a str> {
    let normalised = candidate.trim_end_matches('/');
    for prefix in denylist {
        let p = prefix.trim_end_matches('/');
        if normalised == p {
            return Some(p);
        }
        if let Some(rest) = normalised.strip_prefix(p) {
            if rest.starts_with('/') {
                return Some(p);
            }
        }
    }
    None
}

/// Sudo binary path the plugin invokes to reach each of the
/// four privileged subprocesses (testparm / evo-smb-user-sync /
/// systemctl restart smbd / install → /etc/samba/smb.conf).
/// The distribution's `dist/sudoers.d/evo-samba-server.in`
/// grants `NOPASSWD` on the exact absolute command paths, so
/// every arg builder below emits `["-n", "<absolute-path>",
/// …]` — `-n` refuses any password prompt and the absolute
/// path matches the sudoers alias byte-for-byte.
pub const DEFAULT_SUDO_PROGRAM: &str = "/usr/bin/sudo";

/// Absolute path to `testparm` on Debian/Ubuntu-family hosts.
pub const DEFAULT_TESTPARM_PATH: &str = "/usr/bin/testparm";

/// Absolute path to the SMB user provisioner wrapper installed
/// by bootstrap (`dist/bin/evo-smb-user-sync` →
/// `/usr/local/bin/evo-smb-user-sync`).
pub const DEFAULT_SMB_USER_SYNC_PATH: &str = "/usr/local/bin/evo-smb-user-sync";

/// Absolute path to `systemctl` on Debian/Ubuntu-family hosts.
pub const DEFAULT_SYSTEMCTL_PATH: &str = "/usr/bin/systemctl";

/// Absolute path to `install` on Debian/Ubuntu-family hosts.
pub const DEFAULT_INSTALL_PATH: &str = "/usr/bin/install";

/// Service-user-writable candidate path the runtime renders the
/// smb.conf into BEFORE the `sudo install` step drops it
/// atomically into `/etc/samba/smb.conf`. Living in `/var/tmp`
/// so the distribution's steward service user can write
/// without sudo AND the sudoers alias below can name the
/// exact source path.
pub const DEFAULT_SMB_CONF_CANDIDATE_PATH: &str =
    "/var/tmp/evo-smb.conf.candidate";

/// Build the argv for `sudo -n testparm -s <candidate>`.
/// The path is passed to `testparm` in silent mode so the
/// runtime can verify smbd will accept the candidate before
/// the install step moves it into place.
pub fn build_testparm_args(config_path: &Path) -> Vec<String> {
    vec![
        "-n".to_string(),
        DEFAULT_TESTPARM_PATH.to_string(),
        "-s".to_string(),
        config_path.display().to_string(),
    ]
}

/// Build the argv for `sudo -n evo-smb-user-sync add <user>`
/// (password once on stdin; wrapper doubles for smbpasswd -s).
pub fn build_smb_user_sync_add_args(username: &str) -> Vec<String> {
    vec![
        "-n".to_string(),
        DEFAULT_SMB_USER_SYNC_PATH.to_string(),
        "add".to_string(),
        username.to_string(),
    ]
}

/// Build the argv for `sudo -n evo-smb-user-sync delete <user>`.
pub fn build_smb_user_sync_delete_args(username: &str) -> Vec<String> {
    vec![
        "-n".to_string(),
        DEFAULT_SMB_USER_SYNC_PATH.to_string(),
        "delete".to_string(),
        username.to_string(),
    ]
}

/// Fixed blocklist for SMB login names — generic OS + Samba +
/// audio-plane identities that MUST NOT become LAN
/// file-share credentials on any distribution. The live
/// steward service user (whatever the distribution configured
/// it as) is added dynamically at validation time by
/// [`blocked_smb_usernames`] reading `EVO_SERVICE_USER` and
/// `USER`, so a vendor distribution with a non-audio-reference
/// service-user name inherits the protection without editing
/// this array.
pub const SMB_USERNAME_BLOCKLIST: &[&str] = &[
    "root",
    "nobody",
    "nfsnobody",
    "daemon",
    "bin",
    "sys",
    "sync",
    "games",
    "man",
    "lp",
    "mail",
    "news",
    "uucp",
    "proxy",
    "www-data",
    "backup",
    "list",
    "irc",
    "gnats",
    "systemd-network",
    "systemd-resolve",
    "messagebus",
    "sshd",
    "smbd",
    "nmbd",
    "avahi",
    "mpd",
];

/// Lowercase + shape-check an SMB username
/// (`^[a-z_][a-z0-9_-]{0,31}$`). Used by revoke so a
/// previously-persisted name can still be removed even if it
/// later became blocklisted.
pub fn normalize_smb_username(raw: &str) -> Result<String, ApplyError> {
    let username = raw.trim().to_ascii_lowercase();
    if !is_valid_smb_username_shape(&username) {
        return Err(ApplyError::InvalidUsername { username });
    }
    Ok(username)
}

/// Normalise + refuse blocklisted names per
/// `docs/SAMBA-SHARES.md` § Username rules. Used by add.
pub fn validate_smb_username(raw: &str) -> Result<String, ApplyError> {
    let username = normalize_smb_username(raw)?;
    if blocked_smb_usernames().iter().any(|b| b == &username) {
        return Err(ApplyError::BlockedUsername { username });
    }
    Ok(username)
}

fn is_valid_smb_username_shape(username: &str) -> bool {
    // `^[a-z_][a-z0-9_-]{0,31}$`
    let bytes = username.as_bytes();
    if bytes.is_empty() || bytes.len() > 32 {
        return false;
    }
    let first = bytes[0];
    if !(first.is_ascii_lowercase() || first == b'_') {
        return false;
    }
    bytes[1..].iter().all(|b| {
        b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-'
    })
}

fn blocked_smb_usernames() -> Vec<String> {
    let mut out: Vec<String> = SMB_USERNAME_BLOCKLIST
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    // Live steward identity, in defence-in-depth order:
    //   1. `/proc/self/status` effective UID → `/etc/passwd`
    //      lookup. Independent of env vars — a distribution
    //      whose service manager does NOT set `$USER` still
    //      protects the steward identity. This is the
    //      load-bearing check.
    //   2. `EVO_SERVICE_USER` env var — explicit vendor
    //      override for distributions that publish the name
    //      out-of-band.
    //   3. `USER` env var — systemd sets this from `User=`;
    //      belt-and-braces for the case where the passwd
    //      lookup returns nothing but the shell env is
    //      populated.
    if let Some(uid_derived) = detect_service_user_from_procfs() {
        if !out.iter().any(|b| b == &uid_derived) {
            out.push(uid_derived);
        }
    }
    for key in ["EVO_SERVICE_USER", "USER"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim().to_ascii_lowercase();
            if !v.is_empty() && !out.iter().any(|b| b == &v) {
                out.push(v);
            }
        }
    }
    out
}

/// Read the effective UID from `/proc/self/status`, then
/// resolve the matching username by scanning `/etc/passwd`.
/// Returns `None` on any I/O error, malformed input, or when
/// the UID has no `/etc/passwd` entry.
///
/// The plugin crate forbids `unsafe_code` (blocks a direct
/// libc `geteuid` call) and does NOT carry `nix` / `rustix` as
/// a dependency; the sibling `network.shares` plugin uses the
/// same `/proc/self/status` pattern for its
/// service-user-detection path.
fn detect_service_user_from_procfs() -> Option<String> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    // /proc/self/status Uid: line is `Uid:\tRUID\tEUID\tSUID\tFSUID`.
    // Take the EUID (third whitespace-separated field).
    let effective_uid: u32 = status
        .lines()
        .find(|l| l.starts_with("Uid:"))?
        .split_whitespace()
        .nth(2)?
        .parse()
        .ok()?;
    // /etc/passwd rows are `name:x:uid:gid:gecos:home:shell`.
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    for line in passwd.lines() {
        let mut it = line.splitn(7, ':');
        let name = it.next()?;
        let _x = it.next()?;
        let uid: u32 = it.next()?.parse().ok()?;
        if uid == effective_uid {
            let name = name.trim().to_ascii_lowercase();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

/// Build the argv for `sudo -n systemctl restart smbd`.
pub fn build_systemctl_restart_args() -> Vec<String> {
    vec![
        "-n".to_string(),
        DEFAULT_SYSTEMCTL_PATH.to_string(),
        "restart".to_string(),
        "smbd".to_string(),
    ]
}

/// Build the argv for `sudo -n install -m 0644 -o root -g root
/// <candidate> <target>` — the atomic drop of the validated
/// candidate over `/etc/samba/smb.conf`. `install` copies to a
/// temp file adjacent to the target, fsyncs, then renames —
/// the operating system never observes a partial rewrite.
/// Both paths are passed as literal strings so the sudoers
/// alias can name them exactly (bounded grant).
pub fn build_install_conf_args(candidate: &Path, target: &Path) -> Vec<String> {
    vec![
        "-n".to_string(),
        DEFAULT_INSTALL_PATH.to_string(),
        "-m".to_string(),
        "0644".to_string(),
        "-o".to_string(),
        "root".to_string(),
        "-g".to_string(),
        "root".to_string(),
        candidate.display().to_string(),
        target.display().to_string(),
    ]
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
    /// Absolute path to `sudo` — every privileged subprocess
    /// (testparm / evo-smb-user-sync / systemctl / install)
    /// shells out through this so the sudoers alias grants
    /// match byte-for-byte. Overridable for tests + hosts with
    /// non-standard sudo locations.
    sudo_program: String,
    smb_conf_path: PathBuf,
    /// Service-user-writable candidate path the runtime
    /// renders smb.conf into BEFORE the `sudo install` step
    /// drops it atomically over `smb_conf_path`. Default
    /// [`DEFAULT_SMB_CONF_CANDIDATE_PATH`] (`/var/tmp/…`) so
    /// the service user can write without sudo.
    candidate_path: PathBuf,
    netbios_name: String,
    workgroup: String,
    path_allowlist: Vec<String>,
    path_denylist: Vec<String>,
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
    /// Start a builder for constructing a runtime. The plugin's
    /// `load` path wires it up with the framework credential
    /// vault handle from [`LoadContext`]; test suites use the
    /// same builder with an in-process fetcher. There is no
    /// convenience `open()` constructor — a runtime with no
    /// explicit credential fetcher would silently return
    /// `CredentialVaultUnavailable` on every `add_user`, which
    /// is worse than making the call site declare its intent.
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
            sudo_program: None,
            smb_conf_path: None,
            candidate_path: None,
            netbios_name: None,
            workgroup: None,
            path_allowlist: None,
            path_denylist: None,
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
                &self.path_denylist,
            )
        };

        // Write the rendered conf to the service-user-writable
        // candidate path (default `/var/tmp/evo-smb.conf.candidate`)
        // so the plugin process — which runs as the service user,
        // not root — can produce the bytes without sudo. The
        // subsequent `install` step (bounded via sudoers) drops
        // the candidate atomically over `smb_conf_path` with
        // root ownership + mode 0644.
        self.executor
            .write_file(&self.candidate_path, rendered.as_bytes())
            .await?;

        let testparm_args = build_testparm_args(&self.candidate_path);
        let testparm_out = self
            .executor
            .run(
                &self.sudo_program,
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

        // Install the validated candidate atomically over the
        // target smb.conf. `install(1)` copies to a temp file
        // adjacent to the target, fsyncs, then renames — smbd
        // never observes a partial rewrite. The sudoers alias
        // names both source and target verbatim so operators
        // can audit the exact grant.
        let install_args =
            build_install_conf_args(&self.candidate_path, &self.smb_conf_path);
        let install_out = self
            .executor
            .run(
                &self.sudo_program,
                &install_args,
                self.subprocess_timeout_ms,
                b"",
            )
            .await?;
        if install_out.exit_code != Some(0) {
            return Err(ApplyError::ConfigInstall {
                detail: format!(
                    "sudo install returned exit code {:?}: {}",
                    install_out.exit_code,
                    String::from_utf8_lossy(&install_out.stderr),
                ),
            });
        }

        let smbd_restarted = if new_enabled {
            let systemctl_args = build_systemctl_restart_args();
            let restart_out = self
                .executor
                .run(
                    &self.sudo_program,
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

    /// Add an SMB user with three-substrate atomic-commit
    /// semantics.
    ///
    /// The verb straddles three substrates: the plugin's on-disk
    /// state file, the framework credential vault, and the OS
    /// (NSS entry + Samba passdb via the sudo-elevated wrapper).
    /// A partial commit across those substrates leaves the
    /// device in an operator-visible-inconsistent state that
    /// only manual sudo can recover from, so the sequence is:
    ///
    /// 1. Validate the username, refuse if the credential vault
    ///    handle was never wired at load time, and reject
    ///    duplicates against the current in-memory `smb_users`.
    /// 2. Speculatively push the record into `smb_users` and
    ///    `state.save`. If the save fails, discard the in-memory
    ///    push and surface `Persistence` — no other substrate
    ///    has been touched, so the operator can retry cleanly.
    /// 3. Fetch the password bytes from the vault. On absent
    ///    entry, roll back the speculative row + save; surface
    ///    `CredentialMissing`.
    /// 4. Fire the wrapper subprocess. On any subprocess or
    ///    non-zero-exit failure, roll back the speculative row +
    ///    save; surface `UserSyncFailed` (or the underlying
    ///    `SubprocessIo`). If the rollback save itself also
    ///    fails, surface `AddRollbackFailed` carrying both
    ///    stderr strings so the operator sees the composite
    ///    failure — the wrapper's `add` gate refuses any
    ///    pre-existing NSS entry, and `revoke_user` is idempotent
    ///    against absent NSS + passdb, so recovering from a
    ///    phantom row is one operator `user_revoke` call.
    ///
    /// A plugin crash between step 2's save-success and step 4's
    /// wrapper-success leaves a phantom row; the same recovery
    /// path (operator `user_revoke`) reconciles it.
    pub async fn add_user(
        &self,
        username: String,
        credential_key: String,
        mapped_domain_identity: Option<String>,
    ) -> Result<SmbUserRecord, ApplyError> {
        let username = validate_smb_username(&username)?;
        // Fail fast when the plugin's load path did not receive
        // a real credential vault handle. The wrapper's useradd
        // + smbpasswd path is unreachable without operator
        // password bytes; better to refuse with an
        // operator-visible explanation than to always return
        // the ambiguous `CredentialMissing`.
        if !self.credentials.is_operator_wired() {
            return Err(ApplyError::CredentialVaultUnavailable);
        }

        let created_at_ms = (self.now_fn)() as i64;
        let record = SmbUserRecord {
            username: username.clone(),
            mapped_domain_identity,
            created_at_ms,
            credential_key: credential_key.clone(),
        };

        // Step 2: duplicate check + speculative persist in ONE
        // lock scope. Doing both under the same guard closes the
        // race window in which two concurrent adds could both
        // pass the "already exists" check.
        {
            let mut g = self.inner.lock().await;
            if g.state.smb_users.iter().any(|u| u.username == username) {
                return Err(ApplyError::UserAlreadyExists { username });
            }
            g.state.smb_users.push(record.clone());
            if let Err(e) = g.state.save(&g.path) {
                // Save failed before any side effect. Roll back
                // the in-memory push so subsequent calls in this
                // process see the original state, then surface
                // the persistence error.
                g.state.smb_users.retain(|u| u.username != username);
                return Err(e.into());
            }
        }

        // Step 3: fetch the vault password. On failure, roll
        // back the speculative row so the plugin state stays
        // consistent with "user not added". Best-effort rollback
        // save; on rollback failure we return `AddRollbackFailed`
        // carrying both stderr strings.
        let password = match self
            .credentials
            .fetch_password(&credential_key)
            .await
        {
            Some(p) => p,
            None => {
                let rollback = {
                    let mut g = self.inner.lock().await;
                    g.state.smb_users.retain(|u| u.username != username);
                    g.state.save(&g.path)
                };
                if let Err(rb) = rollback {
                    return Err(ApplyError::AddRollbackFailed {
                        username,
                        wrapper_stderr: format!(
                            "credential vault has no entry for key {credential_key}"
                        ),
                        rollback_detail: rb.to_string(),
                    });
                }
                return Err(ApplyError::CredentialMissing {
                    key: credential_key,
                });
            }
        };

        // Step 4: fire the wrapper. Wrapper reads password once;
        // it doubles for `smbpasswd -s`.
        let mut stdin = password;
        stdin.push(b'\n');
        let args = build_smb_user_sync_add_args(&username);
        let outcome = self
            .executor
            .run(
                &self.sudo_program,
                &args,
                self.subprocess_timeout_ms,
                &stdin,
            )
            .await;

        match outcome {
            Ok(out) if out.exit_code == Some(0) => {
                self.schedule_republish().await;
                Ok(record)
            }
            Ok(out) => {
                let wrapper_stderr =
                    String::from_utf8_lossy(&out.stderr).into_owned();
                let exit_code = out.exit_code;
                let rollback = {
                    let mut g = self.inner.lock().await;
                    g.state.smb_users.retain(|u| u.username != username);
                    g.state.save(&g.path)
                };
                if let Err(rb) = rollback {
                    return Err(ApplyError::AddRollbackFailed {
                        username,
                        wrapper_stderr,
                        rollback_detail: rb.to_string(),
                    });
                }
                Err(ApplyError::UserSyncFailed {
                    username,
                    exit_code,
                    stderr: wrapper_stderr,
                })
            }
            Err(io_err) => {
                let io_msg = io_err.to_string();
                let rollback = {
                    let mut g = self.inner.lock().await;
                    g.state.smb_users.retain(|u| u.username != username);
                    g.state.save(&g.path)
                };
                if let Err(rb) = rollback {
                    return Err(ApplyError::AddRollbackFailed {
                        username,
                        wrapper_stderr: format!(
                            "subprocess I/O error: {io_msg}"
                        ),
                        rollback_detail: rb.to_string(),
                    });
                }
                Err(io_err)
            }
        }
    }

    /// Revoke an SMB user with three-substrate reconciliation.
    ///
    /// The order is (a) wrapper delete (idempotent against
    /// absent NSS + absent passdb), (b) vault delete
    /// (idempotent per the vault contract), (c) in-memory
    /// mutation + state.save. If (c) fails after (a) + (b)
    /// succeed, the plugin's in-memory state is re-hydrated
    /// from the on-disk state file so memory matches disk —
    /// the operator sees the row still present via `get_state`,
    /// consistent with what a plugin restart would show. The
    /// operator retries `user_revoke`, which is idempotent
    /// across all three substrates and converges on the next
    /// successful save.
    pub async fn revoke_user(&self, username: &str) -> Result<(), ApplyError> {
        let username = normalize_smb_username(username)?;
        // Snapshot the record's credential_key before we delete
        // the row — need it to retract the vault entry after
        // the passdb + NSS removal succeeds.
        let credential_key = {
            let g = self.inner.lock().await;
            g.state
                .smb_users
                .iter()
                .find(|u| u.username == username)
                .map(|u| u.credential_key.clone())
                .ok_or_else(|| ApplyError::UserNotFound {
                    username: username.clone(),
                })?
        };

        let args = build_smb_user_sync_delete_args(&username);
        let out = self
            .executor
            .run(&self.sudo_program, &args, self.subprocess_timeout_ms, b"")
            .await?;
        if out.exit_code != Some(0) {
            return Err(ApplyError::UserSyncFailed {
                username: username.clone(),
                exit_code: out.exit_code,
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }

        // Retract the vault row. Ordered AFTER the wrapper
        // delete: if the wrapper failed above, the vault entry
        // stays and a subsequent revoke retries. A vault
        // delete failure here surfaces as
        // `CredentialDeleteFailed`; the SMB user is already
        // gone at this point so the operator UI must show a
        // "vault row dangling" state and offer manual
        // remediation via the credential-admin surface.
        if let Err(detail) =
            self.credentials.delete_password(&credential_key).await
        {
            return Err(ApplyError::CredentialDeleteFailed {
                key: credential_key,
                detail,
            });
        }

        // In-memory mutation + save. On save failure, re-hydrate
        // in-memory state from disk so the plugin's memory
        // matches its persistence — otherwise a subsequent
        // `get_state` would report the user as revoked while
        // the on-disk record still names them, and a plugin
        // restart would resurrect the row against the operator's
        // last observation. The wrapper + vault deletes are
        // both idempotent, so the operator's next `user_revoke`
        // will converge cleanly on any subsequent save-success.
        let save_result = {
            let mut g = self.inner.lock().await;
            g.state.smb_users.retain(|u| u.username != username);
            g.state.save(&g.path)
        };
        if let Err(e) = save_result {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                username = %username,
                error = %e,
                "revoke_user: state.save failed after wrapper + vault \
                 delete succeeded; re-hydrating in-memory state from disk \
                 so the plugin's memory matches persistence — the operator's \
                 next `user_revoke` will converge (wrapper + vault deletes \
                 are idempotent)"
            );
            let rehydrated = {
                let g = self.inner.lock().await;
                SmbServerState::load(&g.path)
            };
            match rehydrated {
                Ok(loaded) => {
                    let mut g = self.inner.lock().await;
                    g.state = loaded;
                }
                Err(reload_err) => {
                    tracing::error!(
                        plugin = crate::PLUGIN_NAME,
                        username = %username,
                        save_error = %e,
                        reload_error = %reload_err,
                        "revoke_user: state.save failed AND the follow-up \
                         disk re-read also failed; in-memory state now \
                         drifts from disk until the next successful save"
                    );
                }
            }
            return Err(e.into());
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
    sudo_program: Option<String>,
    smb_conf_path: Option<PathBuf>,
    candidate_path: Option<PathBuf>,
    netbios_name: Option<String>,
    workgroup: Option<String>,
    path_allowlist: Option<Vec<String>>,
    path_denylist: Option<Vec<String>>,
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

    /// Override the sudo binary path (default
    /// [`DEFAULT_SUDO_PROGRAM`] = `/usr/bin/sudo`). Every
    /// privileged subprocess (testparm / evo-smb-user-sync /
    /// systemctl / install) shells through this program with
    /// the underlying command's absolute path as the first arg
    /// so the sudoers alias grants match byte-for-byte.
    pub fn with_sudo_program(mut self, program: String) -> Self {
        self.sudo_program = Some(program);
        self
    }

    /// Override the smb.conf path (default `/etc/samba/smb.conf`).
    pub fn with_smb_conf_path(mut self, path: PathBuf) -> Self {
        self.smb_conf_path = Some(path);
        self
    }

    /// Override the candidate path — the service-user-writable
    /// location the runtime renders smb.conf into BEFORE the
    /// sudo install step drops it atomically over
    /// `smb_conf_path`. Tests inject a tempdir path here.
    pub fn with_candidate_path(mut self, path: PathBuf) -> Self {
        self.candidate_path = Some(path);
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

    /// Override the extra_share path denylist. Any path that
    /// matches a denylist prefix is refused at apply time
    /// even when it would otherwise satisfy the allowlist —
    /// denylist beats allowlist (secrets under
    /// `/var/lib/evo/settings` cannot be exposed via an
    /// `extra_share`, even though `/var/lib/evo/settings`
    /// sits under an allowlisted root in a vendor override).
    pub fn with_path_denylist(mut self, list: Vec<String>) -> Self {
        self.path_denylist = Some(list);
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
            sudo_program: self
                .sudo_program
                .unwrap_or_else(|| DEFAULT_SUDO_PROGRAM.to_string()),
            smb_conf_path: self
                .smb_conf_path
                .unwrap_or_else(|| PathBuf::from(DEFAULT_SMB_CONF_PATH)),
            candidate_path: self.candidate_path.unwrap_or_else(|| {
                PathBuf::from(DEFAULT_SMB_CONF_CANDIDATE_PATH)
            }),
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
            path_denylist: self.path_denylist.unwrap_or_else(|| {
                DEFAULT_SHARE_PATH_DENYLIST
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
                path: "/var/lib/evo/music/NAS/studio".to_string(),
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

    /// Reference default allowlist + denylist for tests. Paths
    /// under `/tmp` are added to keep unit tests hermetic when
    /// they want to exercise operator `extra_shares` against a
    /// tempdir rather than the on-target evo music plane.
    fn test_allowlist() -> Vec<String> {
        let mut v: Vec<String> = DEFAULT_SHARE_PATH_ALLOWLIST
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        v.push("/tmp".to_string());
        v
    }

    fn test_denylist() -> Vec<String> {
        DEFAULT_SHARE_PATH_DENYLIST
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    }

    #[test]
    fn render_smb_conf_emits_global_stanza_and_extra_share() {
        let state = SmbServerState {
            schema_version: SMB_SERVER_SCHEMA_VERSION,
            enabled: true,
            min_protocol: MinProtocol::Smb3_02,
            extra_shares: vec![ExtraShare {
                name: "Studio".to_string(),
                path: "/var/lib/evo/music/NAS/studio".to_string(),
                guest_ok: true,
            }],
            smb_users: Vec::new(),
            last_apply_at_ms: None,
        };
        let (rendered, applied, refused) = render_smb_conf(
            &state,
            "EvoTest",
            "STUDIO",
            &test_allowlist(),
            &test_denylist(),
        );
        assert!(rendered.contains("[global]"));
        assert!(rendered.contains("netbios name = EvoTest"));
        assert!(rendered.contains("workgroup = STUDIO"));
        assert!(rendered.contains("server min protocol = SMB3_02"));
        // Stock + delivery shares are ALWAYS present.
        assert!(rendered.contains("[Internal Storage]"));
        assert!(rendered.contains("[USB]"));
        assert!(rendered.contains("[NAS]"));
        assert!(rendered.contains("[Uploads]"));
        assert!(rendered.contains("[evo-plugins-stage]"));
        // Operator extra share also rendered.
        assert!(rendered.contains("[Studio]"));
        assert!(rendered.contains("path = /var/lib/evo/music/NAS/studio"));
        // `applied` carries every share the caller can inspect
        // for the wire response — stock + delivery + operator
        // extras.
        assert!(applied.contains(&"Internal Storage".to_string()));
        assert!(applied.contains(&"USB".to_string()));
        assert!(applied.contains(&"NAS".to_string()));
        assert!(applied.contains(&"Uploads".to_string()));
        assert!(applied.contains(&"evo-plugins-stage".to_string()));
        assert!(applied.contains(&"Studio".to_string()));
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
                    path: "/var/lib/evo/music/NAS/family".to_string(),
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
        let (rendered, applied, refused) = render_smb_conf(
            &state,
            "EvoTest",
            "WG",
            &test_allowlist(),
            &test_denylist(),
        );
        assert!(rendered.contains("[Ok]"));
        assert!(!rendered.contains("[Nope]"));
        assert!(applied.contains(&"Ok".to_string()));
        assert!(!applied.contains(&"Nope".to_string()));
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].setting, "Nope");
        assert!(refused[0].reason.contains("not in framework allowlist"));
    }

    #[test]
    fn render_smb_conf_default_min_protocol_suppresses_directive() {
        let state = SmbServerState::empty();
        let (rendered, _, _) = render_smb_conf(
            &state,
            "EvoTest",
            "WG",
            &test_allowlist(),
            &test_denylist(),
        );
        assert!(!rendered.contains("server min protocol"));
    }

    #[test]
    fn path_matches_allowlist_handles_trailing_slashes_and_exact() {
        let allow = vec![
            "/var/lib/evo/music/".to_string(),
            "/var/lib/evo/uploads".to_string(),
        ];
        assert!(path_matches_allowlist("/var/lib/evo/music", &allow));
        assert!(path_matches_allowlist("/var/lib/evo/music/NAS", &allow));
        assert!(path_matches_allowlist("/var/lib/evo/uploads", &allow));
        assert!(path_matches_allowlist("/var/lib/evo/uploads/inbox", &allow));
        // The suffix `foo` sits at the same nesting level as
        // `music`; must not be admitted as a subpath.
        assert!(!path_matches_allowlist("/var/lib/evo/musicfoo", &allow));
        assert!(!path_matches_allowlist("/etc/shadow", &allow));
    }

    // ----- Inventory-invariant tests (see docs/SAMBA-SHARES.md) -----

    #[test]
    fn render_emits_stock_music_triad_on_evo_music_plane_paths() {
        // Every rendered conf carries the three stock music
        // shares pointing at the evo music plane
        // (`/var/lib/evo/music/{INTERNAL,USB,NAS}`) — NOT the
        // classic Volumio `/data/INTERNAL`, `/mnt/USB`,
        // `/mnt/NAS` layout. This test is the load-bearing
        // check that the inventory paths reach the wire.
        let (rendered, applied, _) = render_smb_conf(
            &SmbServerState::empty(),
            "EvoTest",
            "WG",
            &test_allowlist(),
            &test_denylist(),
        );
        assert!(rendered.contains("[Internal Storage]"));
        assert!(rendered.contains("path = /var/lib/evo/music/INTERNAL"));
        assert!(rendered.contains("[USB]"));
        assert!(rendered.contains("path = /var/lib/evo/music/USB"));
        assert!(rendered.contains("[NAS]"));
        assert!(rendered.contains("path = /var/lib/evo/music/NAS"));
        // Classic Volumio prefixes MUST NOT surface in the
        // rendered conf.
        assert!(!rendered.contains("/data/INTERNAL"));
        assert!(!rendered.contains("/mnt/USB"));
        assert!(!rendered.contains("/mnt/NAS"));
        assert!(applied.contains(&"Internal Storage".to_string()));
        assert!(applied.contains(&"USB".to_string()));
        assert!(applied.contains(&"NAS".to_string()));
    }

    #[test]
    fn render_emits_delivery_shares_with_correct_guest_split() {
        // Uploads is guest-writable; evo-plugins-stage is
        // authenticated. Both always rendered when enabled.
        let (rendered, applied, _) = render_smb_conf(
            &SmbServerState::empty(),
            "EvoTest",
            "WG",
            &test_allowlist(),
            &test_denylist(),
        );
        assert!(rendered.contains("[Uploads]"));
        assert!(rendered.contains("path = /var/lib/evo/uploads"));
        assert!(rendered.contains("[evo-plugins-stage]"));
        assert!(rendered.contains("path = /var/lib/evo/plugins/stage"));
        // Split assertion: after the [Uploads] header the next
        // `guest ok = yes` is expected; after
        // [evo-plugins-stage] we expect `guest ok = no`. Slice
        // the rendered text at each section to check.
        let uploads_section =
            rendered.split("[Uploads]").nth(1).unwrap_or_default();
        let uploads_section = uploads_section
            .split("[evo-plugins-stage]")
            .next()
            .unwrap_or_default();
        assert!(uploads_section.contains("guest ok = yes"));
        let stage_section = rendered
            .split("[evo-plugins-stage]")
            .nth(1)
            .unwrap_or_default();
        // The stage section is the last stock section — take
        // everything after its header. `guest ok = no` MUST
        // appear before the next section (if any).
        assert!(stage_section.contains("guest ok = no"));
        assert!(applied.contains(&"Uploads".to_string()));
        assert!(applied.contains(&"evo-plugins-stage".to_string()));
    }

    #[test]
    fn render_disables_debian_printer_persona_in_global() {
        // Debian's default Samba surfaces `print$`,
        // `[printers]`, and — via CUPS — a printer stanza the
        // operator sees as `nobody` after guest mapping.
        // `[global]` MUST contain the printer-lockout knobs so
        // Samba does not synthesise the printer plane on this
        // device.
        let (rendered, _, _) = render_smb_conf(
            &SmbServerState::empty(),
            "EvoTest",
            "WG",
            &test_allowlist(),
            &test_denylist(),
        );
        assert!(rendered.contains("load printers = no"));
        assert!(rendered.contains("printing = bsd"));
        assert!(rendered.contains("printcap name = /dev/null"));
        assert!(rendered.contains("disable spoolss = yes"));
        assert!(rendered.contains("usershare max shares = 0"));
    }

    #[test]
    fn render_never_emits_debian_default_share_sections() {
        // The plugin's rendered conf is the sole writer of
        // /etc/samba/smb.conf on this distribution — a
        // successful apply overwrites the Debian default
        // persona. Assertion: no `[homes]`, `[printers]`,
        // `[print$]`, or `[nobody]` section header appears in
        // the render, regardless of extra_shares.
        let state = SmbServerState {
            schema_version: SMB_SERVER_SCHEMA_VERSION,
            enabled: true,
            min_protocol: MinProtocol::Default,
            extra_shares: vec![ExtraShare {
                name: "Studio".to_string(),
                path: "/var/lib/evo/music/NAS/studio".to_string(),
                guest_ok: false,
            }],
            smb_users: Vec::new(),
            last_apply_at_ms: None,
        };
        let (rendered, _, _) = render_smb_conf(
            &state,
            "EvoTest",
            "WG",
            &test_allowlist(),
            &test_denylist(),
        );
        for forbidden in &["[homes]", "[printers]", "[print$]", "[nobody]"] {
            assert!(
                !rendered.contains(forbidden),
                "rendered conf must not contain {forbidden}; \
                 got:\n{rendered}"
            );
        }
    }

    #[test]
    fn render_refuses_extra_share_under_denylist_secrets_root() {
        // An operator attempting to expose secrets under
        // `/var/lib/evo/settings/*` must be refused even
        // though a vendor override could allowlist
        // `/var/lib/evo` (the parent). Denylist beats
        // allowlist.
        let state = SmbServerState {
            schema_version: SMB_SERVER_SCHEMA_VERSION,
            enabled: true,
            min_protocol: MinProtocol::Default,
            extra_shares: vec![ExtraShare {
                name: "SecretsLeak".to_string(),
                path: "/var/lib/evo/settings/vault".to_string(),
                guest_ok: false,
            }],
            smb_users: Vec::new(),
            last_apply_at_ms: None,
        };
        // Deliberately widen the allowlist to match `/var/lib/evo`
        // so the ONLY thing stopping the share is the denylist.
        let widened = vec!["/var/lib/evo".to_string()];
        let (rendered, applied, refused) = render_smb_conf(
            &state,
            "EvoTest",
            "WG",
            &widened,
            &test_denylist(),
        );
        assert!(!rendered.contains("[SecretsLeak]"));
        assert!(!applied.contains(&"SecretsLeak".to_string()));
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].setting, "SecretsLeak");
        assert!(refused[0]
            .reason
            .contains("under a framework denylist prefix"));
    }

    #[test]
    fn render_refuses_extra_share_under_denylist_rejected_bundles_dir() {
        // The plugin stage's `rejected/` subdirectory is a
        // dumping ground for bundles that failed admission —
        // not a re-drop target. Operator extras there are
        // refused.
        let state = SmbServerState {
            schema_version: SMB_SERVER_SCHEMA_VERSION,
            enabled: true,
            min_protocol: MinProtocol::Default,
            extra_shares: vec![ExtraShare {
                name: "RejectedRedrop".to_string(),
                path: "/var/lib/evo/plugins/stage/rejected".to_string(),
                guest_ok: false,
            }],
            smb_users: Vec::new(),
            last_apply_at_ms: None,
        };
        // The rejected/ path is UNDER an allowlisted root
        // (`/var/lib/evo/plugins/stage`), so denylist is the
        // only thing that can catch it.
        let (_, applied, refused) = render_smb_conf(
            &state,
            "EvoTest",
            "WG",
            &test_allowlist(),
            &test_denylist(),
        );
        assert!(!applied.contains(&"RejectedRedrop".to_string()));
        assert_eq!(refused.len(), 1);
        assert!(refused[0]
            .reason
            .contains("under a framework denylist prefix"));
    }

    #[test]
    fn matched_denylist_prefix_recognises_exact_and_subpath() {
        let deny = vec![
            "/var/lib/evo/settings".to_string(),
            "/var/lib/evo/plugins/stage/rejected".to_string(),
        ];
        assert_eq!(
            matched_denylist_prefix("/var/lib/evo/settings", &deny),
            Some("/var/lib/evo/settings")
        );
        assert_eq!(
            matched_denylist_prefix("/var/lib/evo/settings/vault", &deny),
            Some("/var/lib/evo/settings")
        );
        // Sibling directories at the same nesting level MUST
        // NOT match a denylist prefix.
        assert_eq!(
            matched_denylist_prefix("/var/lib/evo/settingsfoo", &deny),
            None
        );
        // The plugin-stage rejected/ dir matches; the
        // top-level stage/ does not.
        assert_eq!(
            matched_denylist_prefix(
                "/var/lib/evo/plugins/stage/rejected/bundle.tar.gz",
                &deny
            ),
            Some("/var/lib/evo/plugins/stage/rejected")
        );
        assert_eq!(
            matched_denylist_prefix("/var/lib/evo/plugins/stage", &deny),
            None
        );
    }

    // ----- argv builder tests -----
    //
    // Every privileged subprocess shells out through `sudo -n
    // <absolute-path> …`; the sudoers alias grants match
    // byte-for-byte. Assertions include the `sudo -n` prefix +
    // the absolute path so a future author reading a test sees
    // the exact grant contract.

    #[test]
    fn testparm_args_shape() {
        assert_eq!(
            build_testparm_args(Path::new("/tmp/smb.conf.candidate")),
            vec![
                "-n".to_string(),
                "/usr/bin/testparm".to_string(),
                "-s".to_string(),
                "/tmp/smb.conf.candidate".to_string(),
            ]
        );
    }

    #[test]
    fn smb_user_sync_add_args_shape() {
        assert_eq!(
            build_smb_user_sync_add_args("producer"),
            vec![
                "-n".to_string(),
                "/usr/local/bin/evo-smb-user-sync".to_string(),
                "add".to_string(),
                "producer".to_string(),
            ]
        );
    }

    #[test]
    fn smb_user_sync_delete_args_shape() {
        assert_eq!(
            build_smb_user_sync_delete_args("producer"),
            vec![
                "-n".to_string(),
                "/usr/local/bin/evo-smb-user-sync".to_string(),
                "delete".to_string(),
                "producer".to_string(),
            ]
        );
    }

    #[test]
    fn validate_smb_username_accepts_canonical_names() {
        assert_eq!(validate_smb_username("Producer").unwrap(), "producer");
        assert_eq!(validate_smb_username("a").unwrap(), "a");
        assert_eq!(
            validate_smb_username("user_name-1").unwrap(),
            "user_name-1"
        );
    }

    #[test]
    fn validate_smb_username_rejects_invalid_shape() {
        assert!(matches!(
            validate_smb_username("Bad Name"),
            Err(ApplyError::InvalidUsername { .. })
        ));
        assert!(matches!(
            validate_smb_username("9bad"),
            Err(ApplyError::InvalidUsername { .. })
        ));
        assert!(matches!(
            validate_smb_username(""),
            Err(ApplyError::InvalidUsername { .. })
        ));
    }

    #[test]
    fn validate_smb_username_rejects_blocklist() {
        // Fixed-array blocklist — generic OS + Samba
        // identities that MUST NOT become SMB logins on any
        // distribution.
        assert!(matches!(
            validate_smb_username("root"),
            Err(ApplyError::BlockedUsername { .. })
        ));
        assert!(matches!(
            validate_smb_username("nobody"),
            Err(ApplyError::BlockedUsername { .. })
        ));
        assert!(matches!(
            validate_smb_username("smbd"),
            Err(ApplyError::BlockedUsername { .. })
        ));
        assert!(matches!(
            validate_smb_username("mpd"),
            Err(ApplyError::BlockedUsername { .. })
        ));
        // The distribution-configured service user is added
        // dynamically at validation time via env-var pickup
        // in `blocked_smb_usernames`; asserted separately in
        // `validate_smb_username_rejects_env_service_user`
        // where the env var is injected under test scope.
    }

    /// The runtime picks up the live steward service user from
    /// `EVO_SERVICE_USER` (or `USER` as fallback) and blocks
    /// that name from becoming an SMB login. This test injects
    /// a synthetic service-user name into `EVO_SERVICE_USER`
    /// so the assertion does not depend on the ambient test
    /// process's `$USER`.
    ///
    /// Uses `serial_test`'s serial marker if present; here we
    /// unset the env var immediately after the assertion so a
    /// parallel test cannot observe the injected value beyond
    /// this scope. The window is small enough that concurrent
    /// tests reading `blocked_smb_usernames()` at exactly the
    /// wrong instant would still pass their own assertions
    /// (they check for their own hardcoded names, not this
    /// synthetic one).
    #[test]
    fn validate_smb_username_rejects_env_service_user() {
        const SENTINEL: &str = "test-injected-service-user";
        // SAFETY: env-var mutation is unsound in multi-threaded
        // tests. Rust 1.86+ marks `set_var`/`remove_var`
        // `unsafe` for that reason; older toolchains hide the
        // marker but the discipline is the same. We accept the
        // small window because the sentinel string does not
        // collide with any other test's assertions and the
        // scope of the injection is bounded to this function.
        std::env::set_var("EVO_SERVICE_USER", SENTINEL);
        let outcome = validate_smb_username(SENTINEL);
        std::env::remove_var("EVO_SERVICE_USER");
        assert!(
            matches!(outcome, Err(ApplyError::BlockedUsername { .. })),
            "expected env-var-detected service user to be blocked, got: {outcome:?}"
        );
    }

    #[test]
    fn systemctl_restart_args_shape() {
        assert_eq!(
            build_systemctl_restart_args(),
            vec![
                "-n".to_string(),
                "/usr/bin/systemctl".to_string(),
                "restart".to_string(),
                "smbd".to_string(),
            ]
        );
    }

    #[test]
    fn install_conf_args_shape() {
        // The install argv MUST name every knob the sudoers
        // alias pins — mode 0644, owner root, group root,
        // and both source + target paths as literal strings.
        // Any deviation here mismatches the sudoers alias and
        // apply hard-fails with "sudo: a password is required"
        // (because the operator's sudo config falls back to
        // password-required for un-aliased commands).
        assert_eq!(
            build_install_conf_args(
                Path::new("/var/tmp/evo-smb.conf.candidate"),
                Path::new("/etc/samba/smb.conf")
            ),
            vec![
                "-n".to_string(),
                "/usr/bin/install".to_string(),
                "-m".to_string(),
                "0644".to_string(),
                "-o".to_string(),
                "root".to_string(),
                "-g".to_string(),
                "root".to_string(),
                "/var/tmp/evo-smb.conf.candidate".to_string(),
                "/etc/samba/smb.conf".to_string(),
            ]
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

    /// Test stub. Records every `delete_password` call so a
    /// revoke test can assert the vault-retract step fires
    /// against the correct key.
    struct StubCredentials {
        map: HashMap<String, Vec<u8>>,
        deletes: Arc<Mutex<Vec<String>>>,
    }

    impl StubCredentials {
        fn new(
            map: HashMap<String, Vec<u8>>,
        ) -> (Arc<Self>, Arc<Mutex<Vec<String>>>) {
            let deletes = Arc::new(Mutex::new(Vec::new()));
            let stub = Arc::new(Self {
                map,
                deletes: Arc::clone(&deletes),
            });
            (stub, deletes)
        }
    }

    #[async_trait]
    impl SmbCredentialFetcher for StubCredentials {
        async fn fetch_password(&self, key: &str) -> Option<Vec<u8>> {
            self.map.get(key).cloned()
        }
        async fn delete_password(&self, key: &str) -> Result<(), String> {
            self.deletes.lock().await.push(key.to_string());
            Ok(())
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
            .with_credentials(StubCredentials::new(creds).0)
            .with_sudo_program("sudo".to_string())
            .with_smb_conf_path(dir.join("smb.conf"))
            .with_candidate_path(dir.join("smb.conf.candidate"))
            .with_path_allowlist(vec![
                "/var/lib/evo/music".to_string(),
                "/var/lib/evo/uploads".to_string(),
                "/var/lib/evo/plugins/stage".to_string(),
            ])
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
            // testparm + install (sudo install candidate →
            // smb.conf) + systemctl restart smbd
            vec![ok(), ok(), ok()],
            HashMap::new(),
        );
        let report = rt
            .apply(
                true,
                MinProtocol::Smb3_02,
                vec![ExtraShare {
                    name: "Music".to_string(),
                    path: "/var/lib/evo/music/NAS/music".to_string(),
                    guest_ok: false,
                }],
            )
            .await
            .unwrap();
        // Post-inventory: the applied list carries stock +
        // delivery shares alongside the operator's `Music`
        // extra.
        assert!(report.applied_shares.contains(&"Music".to_string()));
        assert!(report
            .applied_shares
            .contains(&"Internal Storage".to_string()));
        assert!(report.applied_shares.contains(&"USB".to_string()));
        assert!(report.applied_shares.contains(&"NAS".to_string()));
        assert!(report.applied_shares.contains(&"Uploads".to_string()));
        assert!(report
            .applied_shares
            .contains(&"evo-plugins-stage".to_string()));
        assert!(report.refused_settings.is_empty());
        assert!(report.smbd_restarted);
        let calls = executor.calls.lock().await;
        // Three subprocess invocations — all through the same
        // sudo wrapper program string, distinguished by their
        // first arg (`-n` + the underlying binary path).
        assert_eq!(calls.len(), 3);
        // Every call goes through sudo — first argv element is
        // `-n`, second is the underlying binary path.
        for c in calls.iter() {
            assert_eq!(c.1[0], "-n");
        }
        assert!(calls[0].1[1].ends_with("/testparm"));
        assert!(calls[1].1[1].ends_with("/install"));
        assert!(calls[2].1[1].ends_with("/systemctl"));
        // One filesystem write — the candidate. The install
        // step (subprocess) handles the atomic drop to the
        // target so there is no second write_file call.
        let writes = executor.writes.lock().await;
        assert_eq!(writes.len(), 1);
        assert!(writes[0]
            .0
            .to_string_lossy()
            .ends_with("smb.conf.candidate"));
    }

    #[tokio::test]
    async fn apply_when_disabled_skips_systemctl_restart() {
        let dir = tempdir();
        let (rt, executor) = built_runtime(
            &dir,
            // testparm + install — no systemctl (server
            // disabled → conf still rewritten, but smbd
            // does not restart)
            vec![ok(), ok()],
            HashMap::new(),
        );
        let report = rt
            .apply(false, MinProtocol::Default, Vec::new())
            .await
            .unwrap();
        assert!(!report.smbd_restarted);
        let calls = executor.calls.lock().await;
        assert_eq!(calls.len(), 2);
        assert!(calls[0].1[1].ends_with("/testparm"));
        assert!(calls[1].1[1].ends_with("/install"));
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
    async fn apply_install_failure_returns_config_install_error() {
        let dir = tempdir();
        let (rt, _) = built_runtime(
            &dir,
            // testparm ok, install err
            vec![ok(), err_output()],
            HashMap::new(),
        );
        let err = rt
            .apply(true, MinProtocol::Default, Vec::new())
            .await
            .unwrap_err();
        assert!(matches!(err, ApplyError::ConfigInstall { .. }));
    }

    #[tokio::test]
    async fn apply_restart_failure_returns_restart_error() {
        let dir = tempdir();
        let (rt, _) = built_runtime(
            &dir,
            // testparm ok, install ok, restart fail
            vec![ok(), ok(), err_output()],
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
            built_runtime(&dir, vec![ok(), ok(), ok()], HashMap::new());
        let report = rt
            .apply(
                true,
                MinProtocol::Default,
                vec![
                    ExtraShare {
                        name: "Ok".to_string(),
                        path: "/var/lib/evo/music/NAS/x".to_string(),
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
        // Post-inventory: the applied list carries stock +
        // delivery + the operator's `Ok` extra. `Nope` remains
        // the sole refusal.
        assert!(report.applied_shares.contains(&"Ok".to_string()));
        assert!(report
            .applied_shares
            .contains(&"Internal Storage".to_string()));
        assert_eq!(report.refused_settings.len(), 1);
        assert_eq!(report.refused_settings[0].setting, "Nope");
        let writes = executor.writes.lock().await;
        // Written file does not include the refused share.
        let text = String::from_utf8_lossy(&writes[0].1);
        assert!(text.contains("[Ok]"));
        assert!(!text.contains("[Nope]"));
    }

    #[tokio::test]
    async fn add_user_pipes_password_once_and_persists_record() {
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
        assert_eq!(calls[0].1, build_smb_user_sync_add_args("producer"));
        // Password piped once; wrapper doubles for smbpasswd -s.
        assert_eq!(calls[0].2, b"s3cret\n".to_vec());
        // Persisted.
        let state = rt.get_state().await;
        assert_eq!(state.smb_users.len(), 1);
        assert_eq!(state.smb_users[0].username, "producer");
    }

    #[tokio::test]
    async fn add_user_blocklisted_returns_blocked_username() {
        let dir = tempdir();
        let mut creds = HashMap::new();
        creds.insert("k".to_string(), b"pw".to_vec());
        let (rt, _) = built_runtime(&dir, Vec::new(), creds);
        let err = rt
            .add_user("root".to_string(), "k".to_string(), None)
            .await
            .unwrap_err();
        assert!(matches!(err, ApplyError::BlockedUsername { .. }));
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

    /// The runtime's `add_user` path MUST refuse with a
    /// distinct [`ApplyError::CredentialVaultUnavailable`]
    /// when the fetcher is not operator-wired — that is the
    /// production shape when `LoadContext::credential_vault`
    /// is `None` at plugin load and the plugin installed the
    /// [`NoSmbCredentialFetcher`] placeholder. Distinct from
    /// `CredentialMissing` (which fires when the fetcher IS
    /// wired but the operator did not stage the specific
    /// credential key).
    #[tokio::test]
    async fn add_user_refuses_when_vault_unwired() {
        let dir = tempdir();
        // Build a runtime whose credential fetcher is the
        // unwired placeholder. Bypasses `built_runtime` which
        // wires the recording stub.
        let executor = ScriptedSamba::new(Vec::new());
        let rt = SambaServerRuntime::builder(&dir)
            .unwrap()
            .with_executor(executor)
            .with_credentials(Arc::new(NoSmbCredentialFetcher))
            .with_sudo_program("sudo".to_string())
            .with_smb_conf_path(dir.join("smb.conf"))
            .with_candidate_path(dir.join("smb.conf.candidate"))
            .with_netbios_name("EvoTest".to_string())
            .with_workgroup("STUDIO".to_string())
            .with_subprocess_timeout_ms(1_000)
            .with_now_fn(Arc::new(|| 1_700_000_777_000))
            .build();
        let err = rt
            .add_user("alice".to_string(), "vault:smb:alice".to_string(), None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ApplyError::CredentialVaultUnavailable),
            "expected CredentialVaultUnavailable, got: {err:?}"
        );
    }

    /// `revoke_user` MUST call `delete_password` on the
    /// credential fetcher after the wrapper delete succeeds
    /// so the vault row is retracted alongside the SMB user.
    /// A dangling vault row after revoke leaves the operator
    /// UI showing "credential exists" for a user who no
    /// longer authenticates.
    #[tokio::test]
    async fn revoke_user_deletes_vault_credential() {
        let dir = tempdir();
        // Build the runtime by hand so we can hold a reference
        // to the deletes recorder — `built_runtime` swallows
        // the second `StubCredentials::new` return value.
        let mut creds_map = HashMap::new();
        creds_map.insert("vault:smb:target".to_string(), b"pw".to_vec());
        let (stub, deletes) = StubCredentials::new(creds_map);
        let executor = ScriptedSamba::new(vec![ok(), ok()]); // add + delete
        let rt = SambaServerRuntime::builder(&dir)
            .unwrap()
            .with_executor(executor)
            .with_credentials(stub)
            .with_sudo_program("sudo".to_string())
            .with_smb_conf_path(dir.join("smb.conf"))
            .with_candidate_path(dir.join("smb.conf.candidate"))
            .with_netbios_name("EvoTest".to_string())
            .with_workgroup("STUDIO".to_string())
            .with_subprocess_timeout_ms(1_000)
            .with_now_fn(Arc::new(|| 1_700_000_777_000))
            .build();
        rt.add_user("target".to_string(), "vault:smb:target".to_string(), None)
            .await
            .unwrap();
        rt.revoke_user("target").await.unwrap();
        // Vault-delete fired exactly once with the record's
        // credential_key.
        let deleted = deletes.lock().await;
        assert_eq!(deleted.len(), 1);
        assert_eq!(deleted[0], "vault:smb:target");
    }

    /// State-file persistence MUST set mode 0o600 so the
    /// `credential_key` fields (which name entries in the
    /// framework credential vault) are not world-readable.
    /// The steward's plugin state directory is already 0o700
    /// in production; this is defence-in-depth against a
    /// hypothetical perm-widen on the parent.
    #[test]
    #[cfg(unix)]
    fn state_save_sets_file_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir();
        let path = dir.join(SMB_SERVER_FILE);
        SmbServerState::empty().save(&path).unwrap();
        let mode =
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "state file must be 0o600, got 0o{mode:o}");
    }

    /// `add_user` MUST roll back the speculative `smb_users`
    /// push when the wrapper subprocess exits non-zero. Without
    /// rollback, an operator seeing a spurious wrapper failure
    /// would end up with a plugin state row that does not
    /// correspond to a real NSS + passdb entry — the next add
    /// would refuse "already exists" from the plugin's
    /// in-memory state, and the next revoke would issue a
    /// wrapper delete against an absent user. Rollback keeps
    /// the plugin's admitted-user list authoritative.
    #[tokio::test]
    async fn add_user_rolls_back_state_when_wrapper_fails() {
        let dir = tempdir();
        let mut creds = HashMap::new();
        creds.insert("k".to_string(), b"pw".to_vec());
        // ScriptedSamba is scripted to return a non-zero exit on
        // the single wrapper call — the wrapper add failure path.
        let (rt, _executor) = built_runtime(&dir, vec![err_output()], creds);
        let err = rt
            .add_user("scrubme".to_string(), "k".to_string(), None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ApplyError::UserSyncFailed { .. }),
            "expected UserSyncFailed, got: {err:?}"
        );
        // The rollback ran: in-memory smb_users MUST be empty.
        let state = rt.get_state().await;
        assert!(
            state.smb_users.is_empty(),
            "expected empty smb_users after rollback, got: {:?}",
            state.smb_users
        );
        // Disk state MUST match — otherwise a plugin restart
        // would resurrect the phantom row.
        let on_disk = SmbServerState::load(&dir.join(SMB_SERVER_FILE)).unwrap();
        assert!(
            on_disk.smb_users.is_empty(),
            "expected empty smb_users on disk after rollback, got: {:?}",
            on_disk.smb_users
        );
    }

    /// When `fetch_password` returns None AFTER the speculative
    /// persist has committed to disk, `add_user` MUST roll back
    /// the row so a subsequent `credential_put` + `user_add`
    /// retry finds a clean slate. This is the same shape as
    /// `add_user_missing_credential_key_returns_credential_missing`
    /// but with an explicit rollback assertion on top.
    #[tokio::test]
    async fn add_user_rolls_back_state_when_credential_missing() {
        let dir = tempdir();
        let (rt, _executor) = built_runtime(&dir, Vec::new(), HashMap::new());
        let err = rt
            .add_user(
                "scrubme".to_string(),
                "vault:smb:scrubme".to_string(),
                None,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ApplyError::CredentialMissing { .. }),
            "expected CredentialMissing, got: {err:?}"
        );
        // In-memory + on-disk state must both be empty.
        assert!(rt.get_state().await.smb_users.is_empty());
        let on_disk = SmbServerState::load(&dir.join(SMB_SERVER_FILE)).unwrap();
        assert!(on_disk.smb_users.is_empty());
    }

    /// A successful `add_user` immediately followed by a
    /// `add_user` for the SAME username must refuse via the
    /// plugin's in-memory dup check (`UserAlreadyExists`) —
    /// this proves the speculative-persist path still commits
    /// to in-memory state on success (not just to disk).
    #[tokio::test]
    async fn add_user_dedup_survives_speculative_persist_ordering() {
        let dir = tempdir();
        let mut creds = HashMap::new();
        creds.insert("k".to_string(), b"pw".to_vec());
        // One successful wrapper call, then no more scripted
        // outputs (the second add should be refused before it
        // reaches the wrapper).
        let (rt, executor) = built_runtime(&dir, vec![ok()], creds);
        rt.add_user("keep".to_string(), "k".to_string(), None)
            .await
            .unwrap();
        let err = rt
            .add_user("keep".to_string(), "k".to_string(), None)
            .await
            .unwrap_err();
        assert!(matches!(err, ApplyError::UserAlreadyExists { .. }));
        // Wrapper was called exactly once — the second add's
        // in-memory dup check must fire BEFORE any subprocess.
        let calls = executor.calls.lock().await;
        assert_eq!(
            calls.len(),
            1,
            "wrapper must not be invoked on the duplicate add"
        );
    }

    /// The euid-derived service-user detection MUST return
    /// SOME string when the test process is running under a
    /// real Linux uid with a `/etc/passwd` row. Guards against
    /// regressing the `/proc/self/status` + `/etc/passwd`
    /// parsers used by [`blocked_smb_usernames`].
    #[test]
    #[cfg(target_os = "linux")]
    fn detect_service_user_from_procfs_returns_a_name() {
        let name = detect_service_user_from_procfs();
        assert!(
            name.is_some(),
            "expected /proc/self/status + /etc/passwd to resolve a name; \
             this test runs on the host that ran the test, whose uid \
             should have a passwd row"
        );
        let name = name.unwrap();
        assert!(!name.is_empty());
        // Confirm lowercasing — the blocklist compares
        // case-insensitively via lowercased-normalised names,
        // so the euid-derived entry MUST already be lowercase.
        assert_eq!(name, name.to_ascii_lowercase());
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
        let (rt, _) =
            built_runtime(&dir, vec![ok(), ok(), ok()], HashMap::new());
        let announcer = RecordingAnnouncer::new();
        rt.attach_subject_publisher(announcer.clone())
            .await
            .unwrap();
        rt.apply(
            true,
            MinProtocol::Smb3_02,
            vec![ExtraShare {
                name: "Music".to_string(),
                path: "/var/lib/evo/music/NAS/music".to_string(),
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
        let (rt, _) =
            built_runtime(&dir, vec![ok(), ok(), ok()], HashMap::new());
        let req = SmbServerApplyRequest {
            enabled: true,
            min_protocol: MinProtocol::Smb3_02,
            extra_shares: vec![ExtraShare {
                name: "Uploads".to_string(),
                path: "/var/lib/evo/uploads/incoming".to_string(),
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
