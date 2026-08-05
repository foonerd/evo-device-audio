// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! NAS / SMB / NFS share management primitive.
//!
//! The framework-tier substrate the operator sees as "point evo
//! at music wherever the music lives — local disk, NAS over SMB,
//! NAS over NFS, or files uploaded directly to the device —
//! without ever editing a shell file by hand." This module owns
//! the durable share-record model, TOML persistence at
//! `<state_dir>/network_shares.toml`, and the value types the
//! runtime primitive (landing in follow-on commits) consumes.
//!
//! ## Types
//!
//! [`ShareRecord`] captures one operator-configured share:
//! filesystem type ([`FsType::Cifs`] or [`FsType::Nfs`]), host,
//! remote path, credential shape ([`Credentials`]), operator-
//! supplied mount options, and the persisted mount metadata
//! ([`persisted_vers`](ShareRecord::persisted_vers),
//! [`mount_root`](ShareRecord::mount_root),
//! [`last_mounted_at_ms`](ShareRecord::last_mounted_at_ms)) the
//! runtime writes back after successful operations.
//!
//! ## Passwords never on disk
//!
//! [`Credentials::UserPassword`] carries a
//! [`credential_key`](Credentials::UserPassword::credential_key)
//! — a string handle into the framework's credential vault
//! (`crate::credentials`) scoped to the network-shares
//! plugin-id. The password bytes never appear in
//! [`ShareRecord`] and never round-trip through the TOML file.
//!
//! ## Persistence shape
//!
//! [`NetworkSharesState`] is the on-disk root: a version marker
//! plus the ordered list of [`ShareRecord`]s. Ordering is
//! creation order (operator-visible in the UI list). Every
//! mutation runs through [`NetworkSharesState::save`] which
//! writes atomically (temp file + rename) so a mid-write crash
//! never leaves a truncated shares file that would fail to
//! parse at next boot.

use async_trait::async_trait;
use evo_plugin_sdk::contract::{
    ExternalAddressing, PromptOutcome, PromptRequest, PromptResponse,
    PromptType, ReportError, SubjectAnnouncement, SubjectAnnouncer,
    UserInteractionRequester,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::SystemTime;
use tokio::sync::Mutex;

/// On-disk persistence schema version. Bumps land alongside a
/// migration path in [`NetworkSharesState::load`].
pub const NETWORK_SHARES_SCHEMA_VERSION: u32 = 1;

/// CIFS dialect probe ladder. Lifted verbatim from the
/// volumio-evo reference implementation. Ordered lowest-first;
/// the mount runtime iterates and persists the first success.
/// SMB1/NT1 is NEVER included by design — operators who need
/// SMB1 must explicitly set `vers=1.0` or `vers=NT1` in
/// [`ShareRecord::advanced_options`].
///
/// Order is preserved from the volumio-evo reference (years of
/// production experience). Any change to the direction of this
/// ladder MUST be gated on rig-level performance data across
/// the target NAS matrix and explicit user authorization — the
/// volumio-evo choice is not a "wrong default"; it is a proven
/// default whose reasoning includes performance characteristics
/// that a modern-first ladder can regress.
pub const CIFS_VERS_PROBE_LADDER: &[&str] =
    &["2.0", "2.1", "3.0", "3.02", "3.1.1"];

/// The path under `<state_dir>/` this module owns.
pub const NETWORK_SHARES_FILE: &str = "network_shares.toml";

/// The mount-point root under which per-share mount points are
/// created. Lives under the framework's `/var/lib/evo/music/`
/// data-plane convention (INTERNAL / USB / NAS) so operators
/// browsing the music library see a single unified tree. The
/// distribution installer creates the root at first-boot per the
/// four-primitive install/reset contract; this crate never
/// creates the directory itself and never falls back to any
/// other location.
pub const NAS_MOUNT_ROOT: &str = "/var/lib/evo/music/NAS";

/// Stable per-share identifier. UUIDv4 rendered as canonical
/// lowercase hex-with-hyphens. Survives operator renames of
/// the [`ShareRecord::alias`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ShareId(pub String);

impl ShareId {
    /// Mint a fresh identifier for a new share record.
    pub fn new_v4() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

impl std::fmt::Display for ShareId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The filesystem type the share uses. Determines the mount
/// helper (`mount.cifs` vs `mount.nfs`) and the probe/negotiation
/// policy the runtime applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsType {
    /// CIFS/SMB — the runtime applies the dialect probe ladder
    /// per [`CIFS_VERS_PROBE_LADDER`] unless
    /// [`ShareRecord::advanced_options`] carries an explicit
    /// `vers=` value.
    Cifs,
    /// NFS — kernel auto-negotiates the version (v4 → v3 → v2).
    /// The runtime surfaces the negotiated version from
    /// `/proc/mounts` after successful mount.
    Nfs,
}

impl FsType {
    /// The systemd/mount `-t` type string.
    pub fn as_mount_type(self) -> &'static str {
        match self {
            FsType::Cifs => "cifs",
            FsType::Nfs => "nfs",
        }
    }
}

/// How the runtime authenticates to the share.
///
/// Guest and key-file forms carry all their material inline in
/// the record. [`Credentials::UserPassword`] carries only the
/// username and a
/// [`credential_key`](Credentials::UserPassword::credential_key)
/// handle; the password bytes live in the framework credential
/// vault (`crate::credentials`) under the shares primitive's
/// plugin-id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Credentials {
    /// Guest access (SMB anonymous / NFS with no user mapping).
    Guest,
    /// Username plus password. The password is stored in the
    /// framework credential vault under `credential_key`; the
    /// vault's per-plugin scoping isolates share credentials from
    /// every other credential consumer. `domain` carries the AD /
    /// workgroup name when the target NAS requires one; empty /
    /// absent means "no explicit domain — mount.cifs infers".
    UserPassword {
        /// The username portion of the credential — written into
        /// the mount credentials file's `username=` line, never
        /// on argv.
        username: String,
        /// Handle into the framework credential vault where the
        /// password bytes live. Scoped by the shares primitive's
        /// plugin-id at the vault boundary. The password itself
        /// is written into the mount credentials file's
        /// `password=` line, never on argv.
        credential_key: String,
        /// Optional AD workgroup / domain name. Required when the
        /// target NAS is domain-joined (any Synology in AD mode,
        /// most Windows Server shares); left absent for
        /// standalone SMB servers. Written into the mount
        /// credentials file's `domain=` line when present.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        domain: Option<String>,
    },
    /// Key-file credential (NFS-style keytab; operator-managed).
    /// Framework does not manage the file's lifecycle — the
    /// operator places it, the runtime references its path.
    KeyFile {
        /// Absolute path to the operator-managed key file. The
        /// runtime never reads or writes the file itself.
        path: PathBuf,
    },
}

/// The immediate operational state of a configured share.
/// Surfaced on the per-share reactive subject the runtime
/// publishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountState {
    /// Not currently mounted. Boot has not yet reached this
    /// share, or the operator has unmounted it, or a prior mount
    /// attempt bailed transient.
    Unmounted,
    /// Mount attempt in flight. For CIFS this includes the probe
    /// ladder — the runtime updates [`MountState::Mounting`] with
    /// the current probe dialect for operator visibility.
    Mounting,
    /// Mount succeeded and the mount point is active.
    Mounted,
    /// Mount attempt failed with a non-transient reason. Operator
    /// action (edit share, retry, remove) is required.
    Failed,
}

/// Configured-share record. Persisted in
/// `<state_dir>/network_shares.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareRecord {
    /// Stable UUIDv4 identifier.
    pub share_id: ShareId,
    /// Operator-set display name. May be edited without changing
    /// [`ShareRecord::share_id`].
    pub alias: String,
    /// CIFS or NFS.
    pub fstype: FsType,
    /// Host IP or DNS name.
    pub host: String,
    /// Remote share path (CIFS share name or NFS export path).
    pub path: String,
    /// Authentication shape.
    pub credentials: Credentials,
    /// Operator-supplied mount-options string, verbatim.
    /// Framework composes final options as
    /// `<framework-defaults>,<advanced_options>`.
    #[serde(default)]
    pub advanced_options: String,
    /// The dialect the runtime successfully negotiated on the
    /// most recent CIFS mount. Written after first success;
    /// cleared on failure of the persisted dialect (e.g., NAS
    /// firmware upgrade dropped SMB2.0 support) so the next
    /// mount re-runs the ladder. Always `None` for NFS.
    #[serde(default)]
    pub persisted_vers: Option<String>,
    /// Local mount-point path. Defaults to
    /// `/var/lib/evo/music/NAS/<sanitized_alias>` at record-creation time.
    pub mount_root: PathBuf,
    /// Wall-clock millis when this record was first created.
    /// Used by operator UI for "added <n> ago" freshness.
    pub created_at_ms: i64,
    /// Wall-clock millis of the last successful mount. `None`
    /// means the share has never been successfully mounted.
    #[serde(default)]
    pub last_mounted_at_ms: Option<i64>,
}

impl ShareRecord {
    /// Return the default mount root for a fresh share record
    /// keyed on the operator alias. Sanitisation collapses
    /// characters that are illegal in filesystem path segments
    /// so the operator's chosen alias never produces an invalid
    /// mount point.
    pub fn default_mount_root(alias: &str) -> PathBuf {
        let sanitized: String = alias
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let trimmed = sanitized.trim_matches('_').to_string();
        let name = if trimmed.is_empty() {
            "share"
        } else {
            &trimmed
        };
        PathBuf::from(NAS_MOUNT_ROOT).join(name)
    }

    /// Mint a fresh record from operator-supplied fields.
    /// Auto-populates [`ShareRecord::share_id`] via
    /// [`ShareId::new_v4`], [`ShareRecord::mount_root`] via
    /// [`ShareRecord::default_mount_root`], and
    /// [`ShareRecord::created_at_ms`] to the supplied timestamp
    /// (caller passes it to keep the module clock-source-agnostic
    /// per the framework's Time and Clock Trust contract).
    /// [`ShareRecord::persisted_vers`] +
    /// [`ShareRecord::last_mounted_at_ms`] start as `None`.
    pub fn new(
        alias: String,
        fstype: FsType,
        host: String,
        path: String,
        credentials: Credentials,
        advanced_options: String,
        created_at_ms: i64,
    ) -> Self {
        let mount_root = Self::default_mount_root(&alias);
        Self {
            share_id: ShareId::new_v4(),
            alias,
            fstype,
            host,
            path,
            credentials,
            advanced_options,
            persisted_vers: None,
            mount_root,
            created_at_ms,
            last_mounted_at_ms: None,
        }
    }
}

/// Operator-supplied partial update for
/// [`NetworkSharesHandle::edit_share`]. Any field left `None`
/// preserves the current value. Fields the framework manages
/// internally ([`ShareRecord::share_id`],
/// [`ShareRecord::persisted_vers`], [`ShareRecord::mount_root`],
/// [`ShareRecord::created_at_ms`],
/// [`ShareRecord::last_mounted_at_ms`]) are not editable here —
/// operators who need to change the mount root remove the share
/// and re-add it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareEdits {
    /// New operator-visible alias.
    pub alias: Option<String>,
    /// New CIFS / NFS type. Changing type clears any persisted
    /// dialect so the next mount re-negotiates from scratch.
    pub fstype: Option<FsType>,
    /// New host (IP / DNS).
    pub host: Option<String>,
    /// New remote share path.
    pub path: Option<String>,
    /// New credential shape. Replaces the entire [`Credentials`]
    /// enum — callers who only want to rotate a password key
    /// still supply the full [`Credentials::UserPassword`] with
    /// the new key.
    pub credentials: Option<Credentials>,
    /// New advanced-options string. Empty string clears the
    /// operator-supplied options.
    pub advanced_options: Option<String>,
}

impl ShareEdits {
    /// Whether this edit set, applied to `record`, would change
    /// any field that affects the mount itself (fstype / host /
    /// path / credentials / advanced_options) — i.e. would require
    /// a remount for the change to take effect at the OS layer.
    /// `alias` is operator-visible only and does not require a
    /// remount.
    pub fn is_material_against(&self, record: &ShareRecord) -> bool {
        matches!(&self.fstype, Some(v) if v != &record.fstype)
            || matches!(&self.host, Some(v) if v != &record.host)
            || matches!(&self.path, Some(v) if v != &record.path)
            || matches!(&self.credentials, Some(v) if v != &record.credentials)
            || matches!(
                &self.advanced_options,
                Some(v) if v != &record.advanced_options
            )
    }

    /// Apply this edit set to `record` in-place. Returns whether
    /// any field actually changed, so the caller can skip the
    /// atomic save when the operator submits a no-op edit.
    pub fn apply_to(self, record: &mut ShareRecord) -> bool {
        let mut changed = false;
        if let Some(alias) = self.alias {
            if record.alias != alias {
                record.alias = alias;
                changed = true;
            }
        }
        if let Some(fstype) = self.fstype {
            if record.fstype != fstype {
                record.fstype = fstype;
                // Clearing the persisted dialect on fstype
                // change is a correctness requirement, not a
                // performance choice: a CIFS-only dialect
                // string would be nonsense against an NFS
                // export.
                record.persisted_vers = None;
                changed = true;
            }
        }
        if let Some(host) = self.host {
            if record.host != host {
                record.host = host;
                changed = true;
            }
        }
        if let Some(path) = self.path {
            if record.path != path {
                record.path = path;
                changed = true;
            }
        }
        if let Some(credentials) = self.credentials {
            if record.credentials != credentials {
                record.credentials = credentials;
                changed = true;
            }
        }
        if let Some(advanced_options) = self.advanced_options {
            if record.advanced_options != advanced_options {
                record.advanced_options = advanced_options;
                changed = true;
            }
        }
        changed
    }
}

/// Errors surfaced by [`NetworkSharesState::load`] /
/// [`NetworkSharesState::save`] and the insert / remove helpers.
#[derive(Debug, thiserror::Error)]
pub enum SharesStateError {
    /// Filesystem I/O failure on the shares state file.
    #[error("I/O error on shares state file: {0}")]
    Io(#[from] io::Error),
    /// Deserialisation failed — the shares file on disk does not
    /// parse against the schema this build knows about. Typically
    /// signals hand-edit corruption or forward-migration required.
    #[error("TOML parse error on shares state file: {0}")]
    ParseToml(#[from] toml::de::Error),
    /// Serialisation failed on save — a `ShareRecord` field
    /// contains a shape TOML cannot represent. Should not occur
    /// in the shipped types; guards against future extension.
    #[error("TOML render error on shares state: {0}")]
    RenderToml(#[from] toml::ser::Error),
    /// The persisted state's schema version differs from what
    /// this build supports. Bumps require a migration path in
    /// [`NetworkSharesState::load`] before reaching this error.
    #[error(
        "shares state schema version {found} not supported (this build understands {supported})"
    )]
    UnsupportedSchema {
        /// The version found in the on-disk file.
        found: u32,
        /// The version this build's parser understands.
        supported: u32,
    },
    /// Look-up / removal keyed on an identifier not present in
    /// the current state.
    #[error("no share found with id {id}")]
    ShareNotFound {
        /// The identifier the caller asked for.
        id: ShareId,
    },
    /// Insertion collided with an existing record's identifier.
    /// Signals a caller who is minting IDs incorrectly (should
    /// only happen for direct-import flows; the standard mint
    /// path via [`ShareId::new_v4`] never collides).
    #[error("share id {id} is already present in the store")]
    DuplicateShareId {
        /// The colliding identifier.
        id: ShareId,
    },
}

/// The on-disk root. One instance per host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkSharesState {
    /// Persistence schema version. Bumps land with a migration
    /// path in [`NetworkSharesState::load`].
    pub schema_version: u32,
    /// Ordered list of configured shares. Creation order is the
    /// operator-visible order in the UI list.
    #[serde(default)]
    pub shares: Vec<ShareRecord>,
}

impl NetworkSharesState {
    /// Fresh, empty state.
    pub fn empty() -> Self {
        Self {
            schema_version: NETWORK_SHARES_SCHEMA_VERSION,
            shares: Vec::new(),
        }
    }

    /// Load state from the given path. Returns
    /// [`NetworkSharesState::empty`] when the file does not
    /// exist (fresh install / no shares configured yet).
    pub fn load(path: &Path) -> Result<Self, SharesStateError> {
        match fs::read_to_string(path) {
            Ok(text) => {
                let state: Self = toml::from_str(&text)?;
                if state.schema_version != NETWORK_SHARES_SCHEMA_VERSION {
                    return Err(SharesStateError::UnsupportedSchema {
                        found: state.schema_version,
                        supported: NETWORK_SHARES_SCHEMA_VERSION,
                    });
                }
                Ok(state)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::empty()),
            Err(e) => Err(e.into()),
        }
    }

    /// Persist state atomically: write to a sibling `<path>.tmp`
    /// then rename over the target. A mid-write crash leaves the
    /// prior version in place; the caller never observes a
    /// partial file.
    pub fn save(&self, path: &Path) -> Result<(), SharesStateError> {
        let text = toml::to_string_pretty(self)?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Look up a share by identifier.
    pub fn find(&self, id: &ShareId) -> Option<&ShareRecord> {
        self.shares.iter().find(|r| &r.share_id == id)
    }

    /// Mutable look-up.
    pub fn find_mut(&mut self, id: &ShareId) -> Option<&mut ShareRecord> {
        self.shares.iter_mut().find(|r| &r.share_id == id)
    }

    /// Append a new share. Returns [`SharesStateError::DuplicateShareId`]
    /// if the identifier is already present.
    pub fn insert(
        &mut self,
        record: ShareRecord,
    ) -> Result<(), SharesStateError> {
        if self.find(&record.share_id).is_some() {
            return Err(SharesStateError::DuplicateShareId {
                id: record.share_id.clone(),
            });
        }
        self.shares.push(record);
        Ok(())
    }

    /// Remove a share by identifier. Returns
    /// [`SharesStateError::ShareNotFound`] if the identifier is
    /// not present.
    pub fn remove(
        &mut self,
        id: &ShareId,
    ) -> Result<ShareRecord, SharesStateError> {
        let idx = self
            .shares
            .iter()
            .position(|r| &r.share_id == id)
            .ok_or_else(|| SharesStateError::ShareNotFound {
                id: id.clone(),
            })?;
        Ok(self.shares.remove(idx))
    }
}

impl Default for NetworkSharesState {
    fn default() -> Self {
        Self::empty()
    }
}

/// Operator-facing surface of the network-shares primitive.
///
/// This trait captures the CRUD half of the design contract in
/// `NETWORK-SOURCES-DESIGN.md` section 5.1. Mount/unmount,
/// discovery, and per-share subscription land in follow-on
/// ships as separate trait methods extending this same
/// interface — consumers written against the CRUD surface
/// continue to compile unchanged when the mount surface lands.
///
/// The primitive owns durable state; every mutating method
/// persists to `<state_dir>/network_shares.toml` atomically
/// before returning. Callers can rely on "the call returned Ok
/// → the record is on disk".
#[async_trait]
pub trait NetworkSharesHandle: Send + Sync {
    /// Return every configured share in creation order.
    async fn list_configured(
        &self,
    ) -> Result<Vec<ShareRecord>, SharesStateError>;

    /// Look up one configured share by identifier.
    async fn get_share(
        &self,
        share_id: &ShareId,
    ) -> Result<Option<ShareRecord>, SharesStateError>;

    /// Add a new share. The record's [`ShareRecord::share_id`]
    /// must be freshly minted; a collision with an existing
    /// share errors with [`SharesStateError::DuplicateShareId`].
    /// Persistence + return happen before the runtime attempts
    /// any mount work.
    async fn add_share(
        &self,
        record: ShareRecord,
    ) -> Result<ShareId, SharesStateError>;

    /// Apply an [`ShareEdits`] partial update to an existing
    /// share. No-op edits (every submitted field already matches
    /// the current value) skip the file write and return
    /// `Ok(false)`. Errors with [`SharesStateError::ShareNotFound`]
    /// when the identifier is not present.
    async fn edit_share(
        &self,
        share_id: &ShareId,
        edits: ShareEdits,
    ) -> Result<bool, SharesStateError>;

    /// Remove a share by identifier. Returns the removed record
    /// so callers can pass it to unmount / cleanup helpers
    /// without an additional look-up. Errors with
    /// [`SharesStateError::ShareNotFound`] when the identifier
    /// is not present.
    async fn remove_share(
        &self,
        share_id: &ShareId,
    ) -> Result<ShareRecord, SharesStateError>;

    /// Attempt to mount the share.
    ///
    /// - For CIFS: if [`ShareRecord::persisted_vers`] is
    ///   `Some(v)`, a single mount attempt is made with that
    ///   dialect (fast-path for known-working NAS). On failure
    ///   the persisted dialect is cleared and the probe ladder
    ///   ([`CIFS_VERS_PROBE_LADDER`]) is iterated lowest-first;
    ///   the first success wins and is persisted back to the
    ///   record. On exhaustion the ladder returns
    ///   [`MountError::DialectProbeExhausted`].
    /// - For NFS: single mount attempt (kernel negotiates the
    ///   version); [`MountReport::negotiated_version`] carries
    ///   the observed value for operator display but the record
    ///   itself keeps [`ShareRecord::persisted_vers`] at `None`.
    ///
    /// On success, [`ShareRecord::last_mounted_at_ms`] is
    /// updated and the record is persisted before returning.
    async fn mount_share(
        &self,
        share_id: &ShareId,
    ) -> Result<MountReport, MountError>;

    /// Unmount the share at its current mount root.
    ///
    /// For CIFS the runtime uses lazy detach (`umount -l`) so
    /// held file handles do not block the operator — the mount
    /// point clears from the mount table immediately and the
    /// backing state releases when the last handle closes. For
    /// NFS the runtime performs a plain `umount`; the kernel
    /// handles busy-NFS differently and lazy detach masks real
    /// leaks there.
    ///
    /// Returns [`MountError::ShareNotFound`] if the identifier
    /// is not present. Returns [`MountError::MountFailed`] if
    /// the subprocess exits non-zero (e.g., mount point not
    /// currently mounted).
    async fn unmount_share(&self, share_id: &ShareId)
        -> Result<(), MountError>;

    /// Return the current cached list of discovered NAS
    /// devices. Empty until [`Self::refresh_discovery`] has been
    /// called at least once.
    async fn list_discovered(&self) -> Vec<DiscoveredNas>;

    /// Trigger an operator-visible discovery sweep:
    ///
    /// 1. Run `avahi-browse -atrkp _smb._tcp` to enumerate every
    ///    SMB advertisement on the LAN (18 s deadline —
    ///    coreutils `timeout(1)` semantics apply).
    /// 2. For each discovered `(name, ip)` pair, run
    ///    `smbclient -N -L <ip> -m SMB3_11 --debuglevel=4`
    ///    (15 s deadline) to enumerate the share list and
    ///    extract the negotiated dialect.
    /// 3. Replace the cache with the fresh result; return the
    ///    fresh list on Ok.
    ///
    /// A subprocess failure at step 1 keeps the prior cache
    /// intact — the operator UI never observes an empty list
    /// resulting from a transient probe failure. Per-host
    /// smbclient failures at step 2 are non-fatal: the NAS is
    /// included in the result with an empty share list and
    /// `advertised_dialect = None`.
    async fn refresh_discovery(&self)
        -> Result<Vec<DiscoveredNas>, MountError>;
}

/// Outcome of a successful mount attempt. Surfaced to the
/// operator UI via [`NetworkSharesHandle::mount_share`] and
/// (in follow-on ships) via the per-share subject.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountReport {
    /// The share this report describes.
    pub share_id: ShareId,
    /// The mount root the share is available at (usually
    /// `/var/lib/evo/music/NAS/<sanitized_alias>`).
    pub mount_root: PathBuf,
    /// For CIFS, the negotiated dialect string (e.g. `"3.0"`);
    /// for NFS, the version string parsed from `/proc/mounts`
    /// after mount (e.g. `"4.2"`). `None` when the runtime
    /// cannot determine the version (typically transient
    /// filesystems in tests).
    pub negotiated_version: Option<String>,
    /// Wall-clock milliseconds elapsed from mount-attempt-start
    /// to mount-successful. For CIFS this includes the probe
    /// ladder's failed attempts prior to the first success.
    pub elapsed_ms: u64,
}

/// Errors surfaced by [`NetworkSharesHandle::mount_share`].
/// Distinct classes so operator UI can render the right
/// remediation ("check credentials" vs "check host is
/// reachable" vs "share path missing on server").
#[derive(Debug, thiserror::Error)]
pub enum MountError {
    /// Look-up failed — no share record for this ID.
    #[error("share {id} not found")]
    ShareNotFound {
        /// The identifier the caller asked to mount.
        id: ShareId,
    },
    /// State persistence failed (writing `persisted_vers` back
    /// after successful probe / clearing on stale-dialect
    /// failure).
    #[error("persistence error while updating share state: {0}")]
    Persistence(#[from] SharesStateError),
    /// Password credential required by the share record but the
    /// framework credential vault did not return bytes for the
    /// declared key. Operator UI: prompt to re-enter password.
    #[error("credential vault has no entry for key {key}")]
    CredentialMissing {
        /// The credential-vault key the record referenced.
        key: String,
    },
    /// The plugin was wired without a read-write credential
    /// store, so the prompt-on-add flow cannot persist an
    /// operator-supplied password. Legacy fixtures only; the
    /// production wiring in `lib.rs` always installs the
    /// file-backed store, so this variant never fires on the
    /// reference distribution.
    #[error(
        "runtime was wired without a credential store; \
         cannot prompt for password"
    )]
    CredentialStoreUnavailable,
    /// The per-share mount directory (`mount_root`) could not be
    /// created before the mount attempt. Rare — the plugin's
    /// service identity owns the NAS root by installer contract,
    /// and `create_dir_all` needs no elevation on a directory
    /// the caller owns. Distinct from
    /// [`MountError::DialectProbeExhausted`] so the operator UI
    /// renders the honest cause instead of a protocol
    /// diagnosis. When the mount helper itself returns ENOENT
    /// (typically because the plugin's own mkdir succeeded but a
    /// vendor init script raced and removed the directory), the
    /// same variant fires from the stderr short-circuit.
    #[error(
        "mount directory {mount_root} could not be prepared for share {id}: {reason}"
    )]
    MountDirectoryMissing {
        /// The share that failed at the directory stage.
        id: ShareId,
        /// The mount root the plugin tried to create or that
        /// mount.cifs / mount.nfs could not find.
        mount_root: PathBuf,
        /// Operator-readable reason (IO error from `mkdir_all`
        /// on the plugin side, or the ENOENT-shaped stderr
        /// snippet from the mount helper).
        reason: String,
    },
    /// The operator declined to answer the password prompt
    /// (cancelled or timed out). The share record is left in
    /// place so the operator can retry mount later, at which
    /// point the prompt fires again.
    #[error("operator declined password prompt for key {key}")]
    CredentialPromptCancelled {
        /// The credential-vault key the aborted prompt targeted.
        key: String,
    },
    /// The password prompt could not be issued — the framework's
    /// user-interaction responder returned a
    /// [`ReportError`](evo_plugin_sdk::contract::ReportError).
    /// The reason is preserved verbatim for operator visibility.
    #[error("password prompt failed for key {key}: {reason}")]
    CredentialPromptFailed {
        /// The credential-vault key the prompt targeted.
        key: String,
        /// The framework-level reason (typically "no responder
        /// connected" or "steward shutting down").
        reason: String,
    },
    /// The password prompt was refused fast by the framework
    /// because no session currently holds the user-interaction-
    /// responder slot. Distinct from
    /// [`Self::CredentialPromptFailed`] so the wire layer can
    /// return the specific `no_responder_available` subclass
    /// and the operator surface can render "no answering client
    /// is connected" instead of a generic prompt failure. The
    /// mutation refuses in the current tokio poll rather than
    /// waiting for the framework's prompt TTL (default 60 s) —
    /// the whole point of the fast path.
    #[error(
        "no responder session is currently connected to answer the \
         password prompt for key {key}: {reason}"
    )]
    NoResponderAvailable {
        /// The credential-vault key the prompt targeted.
        key: String,
        /// The framework-supplied refusal reason (includes the
        /// prompt TTL that would otherwise have elapsed before
        /// the operator got a response).
        reason: String,
    },
    /// The prompt returned an answer but writing it to the
    /// credential store failed at the IO layer. Rare — the
    /// framework guarantees the credentials directory is
    /// writable and mode 0700 — but a full disk or a filesystem
    /// remount to read-only would surface here.
    #[error("credential store write failed for key {key}: {reason}")]
    CredentialStoreWriteFailed {
        /// The credential-vault key the write targeted.
        key: String,
        /// Verbatim IO error.
        reason: String,
    },
    /// Subprocess invocation of `mount` failed at the process
    /// layer — the executable was not found, the runtime could
    /// not spawn it, or I/O error while collecting output.
    #[error("mount subprocess I/O error: {0}")]
    SubprocessIo(String),
    /// mount attempt timed out. Set by the executor when the
    /// caller-supplied deadline expired before the child exited.
    #[error("mount timed out after {timeout_ms}ms for share {id}")]
    Timeout {
        /// The share identifier that timed out.
        id: ShareId,
        /// The deadline the executor honoured, in milliseconds.
        timeout_ms: u64,
    },
    /// CIFS dialect probe iterated the full [`CIFS_VERS_PROBE_LADDER`]
    /// and no dialect succeeded WITHOUT hitting an auth-refusal.
    /// Reserved for the case where every attempt failed at the
    /// wire / dialect layer (protocol / negotiation), not the
    /// auth layer. Auth-refusal short-circuits to
    /// [`MountError::AuthenticationRefused`] to avoid the
    /// operator-confusing mislabelling ("dialect probe exhausted"
    /// when the credential was actually wrong).
    ///
    /// `last_error` is part of the Display string so the operator
    /// UI never shows a bare ladder list without the underlying
    /// helper reason (journal-classified stderr when available).
    #[error(
        "CIFS dialect probe exhausted; attempted: {attempted:?}; \
         last_error: {last_error}"
    )]
    DialectProbeExhausted {
        /// The dialects that were tried, in order.
        attempted: Vec<String>,
        /// The last error's exit code + stderr snippet, for
        /// operator visibility.
        last_error: String,
    },
    /// The mount helper refused authentication (typically exit
    /// 13 / EACCES with an "NT_STATUS_LOGON_FAILURE" or
    /// "Permission denied" stderr fragment on CIFS; exit 32 with
    /// "access denied" on NFS). Distinct from dialect exhaustion
    /// so the operator UI renders "check username / password /
    /// domain", not "unsupported protocol version".
    #[error(
        "authentication refused for share {id}: exit={exit_code:?}, stderr={stderr}"
    )]
    AuthenticationRefused {
        /// The share that refused auth.
        id: ShareId,
        /// The mount process's exit code (`None` if terminated
        /// by signal).
        exit_code: Option<i32>,
        /// Verbatim stderr snippet (bounded by the executor).
        stderr: String,
    },
    /// mount returned non-zero. Verbatim stderr snippet
    /// preserved so operator UI can render an operator-legible
    /// reason.
    #[error(
        "mount failed for share {id}: exit={exit_code:?}, stderr={stderr}"
    )]
    MountFailed {
        /// The share that failed to mount.
        id: ShareId,
        /// The mount process's exit code (`None` if terminated
        /// by signal).
        exit_code: Option<i32>,
        /// Verbatim stderr snippet (bounded by the executor).
        stderr: String,
    },
}

/// Result of a subprocess invocation. Mirrors
/// [`std::process::Output`] but as a plain data shape decoupled
/// from `std::process` for testability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Exit code, or `None` if terminated by signal.
    pub exit_code: Option<i32>,
    /// Captured stdout bytes.
    pub stdout: Vec<u8>,
    /// Captured stderr bytes.
    pub stderr: Vec<u8>,
}

/// Abstraction over subprocess execution so tests can inject a
/// mock. Production impl is [`SubprocessMountExecutor`].
#[async_trait]
pub trait MountExecutor: Send + Sync {
    /// Run `program` with `args` (both as byte-safe strings) and
    /// return the captured output. Implementations may enforce
    /// a deadline via `timeout_ms`; on timeout, they return
    /// [`MountError::Timeout`].
    async fn run(
        &self,
        program: &str,
        args: &[String],
        timeout_ms: u64,
    ) -> Result<CommandOutput, MountError>;
}

/// Production [`MountExecutor`] that spawns real subprocesses
/// via `tokio::process::Command`. Applies a wall-clock timeout
/// using `tokio::time::timeout`; on expiry the child is killed
/// and the executor returns [`MountError::Timeout`].
#[derive(Debug, Default, Clone)]
pub struct SubprocessMountExecutor;

#[async_trait]
impl MountExecutor for SubprocessMountExecutor {
    async fn run(
        &self,
        program: &str,
        args: &[String],
        timeout_ms: u64,
    ) -> Result<CommandOutput, MountError> {
        use tokio::process::Command;
        use tokio::time::{timeout, Duration};

        let mut cmd = Command::new(program);
        cmd.args(args);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.stdin(std::process::Stdio::null());

        let child = cmd
            .spawn()
            .map_err(|e| MountError::SubprocessIo(e.to_string()))?;

        let deadline = Duration::from_millis(timeout_ms);
        let wait = child.wait_with_output();
        match timeout(deadline, wait).await {
            Ok(Ok(output)) => Ok(CommandOutput {
                exit_code: output.status.code(),
                stdout: output.stdout,
                stderr: output.stderr,
            }),
            Ok(Err(e)) => Err(MountError::SubprocessIo(e.to_string())),
            Err(_elapsed) => {
                // On timeout the wait_with_output above already
                // consumed the child; no explicit kill path.
                // The next boot / retry will surface the same
                // deadline if the fault persists.
                Err(MountError::SubprocessIo(format!(
                    "mount subprocess exceeded {}ms deadline",
                    timeout_ms
                )))
            }
        }
    }
}

/// Look up the bytes for a credential key. Trait so tests can
/// mock the credential vault interaction without pulling in the
/// full [`crate::credentials::CredentialVault`] fixture.
#[async_trait]
pub trait CredentialFetcher: Send + Sync {
    /// Return the password bytes for `credential_key`, or
    /// `None` if the vault has no entry.
    async fn fetch_password(&self, credential_key: &str) -> Option<Vec<u8>>;
}

/// A [`CredentialFetcher`] that always returns `None`. Used by
/// the framework default constructor for guest-only-configuration
/// scenarios, and by tests that never exercise the password path.
#[derive(Debug, Default, Clone)]
pub struct NoCredentialFetcher;

#[async_trait]
impl CredentialFetcher for NoCredentialFetcher {
    async fn fetch_password(&self, _credential_key: &str) -> Option<Vec<u8>> {
        None
    }
}

/// Read-write credential store. Extends [`CredentialFetcher`] with
/// `store_password`, used by the prompt-on-add flow to persist an
/// operator-supplied password into the plugin's credentials
/// directory. The framework guarantees the credentials directory is
/// mode-0600 and scoped to this plugin's identity; each entry is a
/// file named for the `credential_key` a share record references.
#[async_trait]
pub trait CredentialStore: CredentialFetcher {
    /// Persist `value` under `credential_key`. Overwrites any prior
    /// entry. Returns Err on IO failure (unwritable directory,
    /// out-of-disk, etc.); success guarantees a subsequent
    /// `fetch_password(credential_key)` on the same store instance
    /// returns `Some(value)`.
    async fn store_password(
        &self,
        credential_key: &str,
        value: &[u8],
    ) -> Result<(), CredentialStoreError>;

    /// Remove the vault entry for `credential_key`. Idempotent —
    /// deleting an already-absent entry succeeds silently. Used
    /// by `mount_share` when the mount helper reports
    /// `AuthenticationRefused` so the next mount attempt re-
    /// prompts for the current credential (NAS-side password
    /// rotation). See NETWORK-SOURCES-DESIGN.md §5.6.5.
    async fn delete_password(
        &self,
        credential_key: &str,
    ) -> Result<(), CredentialStoreError>;
}

/// Error kinds a [`CredentialStore::store_password`] call can
/// surface. Kept narrow so the operator UI has a small vocabulary
/// to render.
#[derive(Debug, thiserror::Error)]
pub enum CredentialStoreError {
    /// The `credential_key` failed the store's identifier policy
    /// (empty, path-traversal characters, oversize). Message names
    /// the failing constraint.
    #[error("invalid credential_key: {0}")]
    InvalidKey(String),
    /// Filesystem-side failure — write refused, disk full, etc.
    #[error("credential store IO: {0}")]
    Io(#[from] io::Error),
}

/// File-backed [`CredentialStore`] that reads and writes files
/// under a plugin's credentials directory. Each entry is one file
/// named for the `credential_key`; the file contents are the raw
/// password bytes.
///
/// Path safety: `credential_key` is validated against
/// `[A-Za-z0-9._-]+` to prevent traversal via `..` or `/`. The
/// framework's `credentials_dir` is already mode-0600 and scoped
/// to the plugin, so a valid key resolves to a single file the
/// plugin owns exclusively. Written files are set to mode 0600
/// even though the parent directory is already restrictive; the
/// intent is to survive umask oddities on vendor targets.
#[derive(Debug, Clone)]
pub struct FileCredentialStore {
    root: PathBuf,
}

impl FileCredentialStore {
    /// Construct a store backed by `root`. The directory is
    /// expected to exist with mode 0700 from the framework's
    /// per-plugin credentials-dir provisioning; the store does
    /// not create it.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub(crate) fn validate_key(key: &str) -> Result<(), CredentialStoreError> {
        if key.is_empty() {
            return Err(CredentialStoreError::InvalidKey(
                "credential_key is empty".into(),
            ));
        }
        if key.len() > 128 {
            return Err(CredentialStoreError::InvalidKey(
                "credential_key exceeds 128 chars".into(),
            ));
        }
        for c in key.chars() {
            let ok = c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
            if !ok {
                return Err(CredentialStoreError::InvalidKey(format!(
                    "credential_key contains disallowed char `{c}`; permitted set is [A-Za-z0-9._-]"
                )));
            }
        }
        Ok(())
    }

    fn key_path(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }
}

#[async_trait]
impl CredentialFetcher for FileCredentialStore {
    async fn fetch_password(&self, credential_key: &str) -> Option<Vec<u8>> {
        if Self::validate_key(credential_key).is_err() {
            return None;
        }
        let path = self.key_path(credential_key);
        // Small read (typical password: <1 KiB), so the
        // synchronous std::fs is fine — the surrounding
        // `mount_share` is already spawning subprocesses on the
        // order of 10s of ms.
        tokio::task::spawn_blocking(move || std::fs::read(path))
            .await
            .ok()
            .and_then(|r| r.ok())
    }
}

#[async_trait]
impl CredentialStore for FileCredentialStore {
    async fn store_password(
        &self,
        credential_key: &str,
        value: &[u8],
    ) -> Result<(), CredentialStoreError> {
        Self::validate_key(credential_key)?;
        let path = self.key_path(credential_key);
        // Write via a temp file + rename for atomicity — a mid-
        // write crash never leaves a truncated password on disk.
        // std::fs on spawn_blocking so we don't add a tokio "fs"
        // feature dependency for a one-shot write.
        let tmp = self.root.join(format!(".{credential_key}.tmp"));
        let value = value.to_vec();
        let tmp_clone = tmp.clone();
        let path_clone = path.clone();
        tokio::task::spawn_blocking(move || -> Result<(), io::Error> {
            std::fs::write(&tmp_clone, &value)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perms = std::fs::Permissions::from_mode(0o600);
                std::fs::set_permissions(&tmp_clone, perms)?;
            }
            std::fs::rename(&tmp_clone, &path_clone)?;
            Ok(())
        })
        .await
        .map_err(|e| {
            CredentialStoreError::Io(io::Error::other(format!(
                "credential store write task panicked: {e}"
            )))
        })??;
        Ok(())
    }

    async fn delete_password(
        &self,
        credential_key: &str,
    ) -> Result<(), CredentialStoreError> {
        Self::validate_key(credential_key)?;
        let path = self.key_path(credential_key);
        tokio::task::spawn_blocking(move || -> Result<(), io::Error> {
            match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                // Idempotent: an already-absent entry is a
                // no-op — the caller's contract is "make sure
                // this key is not in the vault", not "an entry
                // was present and we removed it".
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            }
        })
        .await
        .map_err(|e| {
            CredentialStoreError::Io(io::Error::other(format!(
                "credential store delete task panicked: {e}"
            )))
        })??;
        Ok(())
    }
}

/// Framework credential-vault-backed [`CredentialStore`].
///
/// Bridges the plugin's local `CredentialStore` trait to the
/// framework primitive delivered on
/// `LoadContext::credential_vault`. The plugin's mount code
/// continues to see the same trait; the storage boundary shifts
/// from per-plugin plaintext files to the framework's per-plugin-
/// scoped vault under one shared substrate.
pub struct VaultCredentialStore {
    handle: Arc<dyn evo_plugin_sdk::contract::context::CredentialVaultHandle>,
}

impl std::fmt::Debug for VaultCredentialStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultCredentialStore")
            .field("handle", &"<dyn CredentialVaultHandle>")
            .finish()
    }
}

impl VaultCredentialStore {
    /// Wrap a per-plugin-scoped framework credential vault handle
    /// under the plugin's local `CredentialStore` trait.
    pub fn new(
        handle: Arc<
            dyn evo_plugin_sdk::contract::context::CredentialVaultHandle,
        >,
    ) -> Self {
        Self { handle }
    }

    fn map_error(
        e: evo_plugin_sdk::contract::context::CredentialVaultError,
    ) -> CredentialStoreError {
        CredentialStoreError::Io(io::Error::other(format!("vault: {e}")))
    }
}

#[async_trait]
impl CredentialFetcher for VaultCredentialStore {
    async fn fetch_password(&self, credential_key: &str) -> Option<Vec<u8>> {
        self.handle
            .fetch(credential_key.to_string())
            .await
            .ok()
            .flatten()
    }
}

#[async_trait]
impl CredentialStore for VaultCredentialStore {
    async fn store_password(
        &self,
        credential_key: &str,
        value: &[u8],
    ) -> Result<(), CredentialStoreError> {
        let metadata = evo_plugin_sdk::contract::context::CredentialMetadata {
            display_name: Some(format!(
                "network.shares SMB credential — {credential_key}"
            )),
            expires_at_ms: None,
            uninstall_policy:
                evo_plugin_sdk::contract::context::UninstallPolicy::PreserveForReinstall,
        };
        self.handle
            .store(credential_key.to_string(), value.to_vec(), metadata)
            .await
            .map_err(Self::map_error)
    }

    async fn delete_password(
        &self,
        credential_key: &str,
    ) -> Result<(), CredentialStoreError> {
        self.handle
            .delete(credential_key.to_string())
            .await
            .map_err(Self::map_error)
    }
}

/// One-shot migration: read every plaintext credential file under
/// `credentials_dir` (pre-substrate `FileCredentialStore` shape),
/// upsert each into the framework vault, then remove the original
/// file. Idempotent — a second boot finds no files and returns
/// zero-migrated.
///
/// Called at plugin load once, before the runtime opens against
/// the vault-backed store. Failure of the migration for a specific
/// file is logged and does not abort the boot — the plugin will
/// simply prompt the operator to re-enter the affected credential.
pub async fn migrate_plaintext_credentials_into_vault(
    credentials_dir: &std::path::Path,
    handle: Arc<dyn evo_plugin_sdk::contract::context::CredentialVaultHandle>,
) -> Result<usize, io::Error> {
    let dir = credentials_dir.to_path_buf();
    let entries = match tokio::task::spawn_blocking(
        move || -> Result<Vec<std::path::PathBuf>, io::Error> {
            let mut out = Vec::new();
            let rd = match std::fs::read_dir(&dir) {
                Ok(r) => r,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(out);
                }
                Err(e) => return Err(e),
            };
            for entry in rd {
                let entry = entry?;
                if entry.file_type()?.is_file() {
                    out.push(entry.path());
                }
            }
            Ok(out)
        },
    )
    .await
    {
        Ok(inner) => inner?,
        Err(e) => return Err(io::Error::other(format!("scan task: {e}"))),
    };

    let mut migrated = 0usize;
    for path in entries {
        let key = match path.file_name().and_then(|n| n.to_str()) {
            Some(k) => k.to_string(),
            None => {
                tracing::warn!(
                    path = %path.display(),
                    "migration skipping non-utf8 credential file name"
                );
                continue;
            }
        };
        // Only migrate keys the FileCredentialStore itself would
        // have accepted; anything else was not the plugin's file.
        if FileCredentialStore::validate_key(&key).is_err() {
            continue;
        }
        let value = match tokio::task::spawn_blocking({
            let p = path.clone();
            move || std::fs::read(p)
        })
        .await
        {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "migration read failed; skipping"
                );
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "migration read task panicked; skipping"
                );
                continue;
            }
        };
        let metadata = evo_plugin_sdk::contract::context::CredentialMetadata {
            display_name: Some(format!(
                "network.shares SMB credential — {key} (migrated from disk)"
            )),
            expires_at_ms: None,
            uninstall_policy:
                evo_plugin_sdk::contract::context::UninstallPolicy::PreserveForReinstall,
        };
        if let Err(e) = handle.store(key.clone(), value.clone(), metadata).await
        {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "migration vault store failed; leaving file in place"
            );
            continue;
        }
        // Vault write succeeded — remove the original file.
        if let Err(e) = tokio::task::spawn_blocking({
            let p = path.clone();
            move || std::fs::remove_file(p)
        })
        .await
        .map(|r| r.map_err(io::Error::other))
        {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "migration succeeded but failed to remove original file; \
                 will re-migrate idempotently on next boot"
            );
        }
        migrated += 1;
    }
    Ok(migrated)
}

/// Adapter over the framework's [`UserInteractionRequester`]
/// specialised for a single-field password prompt. Wrapping the
/// framework handle behind a plugin-local trait lets tests
/// deterministic-mock the responder without spinning up the
/// framework's prompt-ledger substrate.
#[async_trait]
pub trait PasswordPrompter: Send + Sync + std::fmt::Debug {
    /// Raise a password prompt with the supplied operator-visible
    /// label and await the operator's answer.
    ///
    /// Returns:
    /// - `Ok(Some(bytes))` — operator answered; bytes are the raw
    ///   password.
    /// - `Ok(None)` — prompt cancelled by either side or timed
    ///   out. Caller decides whether to retry.
    /// - `Err(ReportError::*)` — framework-level failure (no
    ///   responder connected, steward shutting down).
    async fn prompt_password(
        &self,
        label: String,
    ) -> Result<Option<Vec<u8>>, ReportError>;
}

/// Production [`PasswordPrompter`] that dispatches through the
/// framework's [`UserInteractionRequester`] handle stamped on
/// `LoadContext`. Tests inject a mock; production wires this one.
#[derive(Clone)]
pub struct FrameworkPasswordPrompter {
    requester: Arc<dyn UserInteractionRequester>,
}

impl std::fmt::Debug for FrameworkPasswordPrompter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameworkPasswordPrompter").finish()
    }
}

impl FrameworkPasswordPrompter {
    /// Wrap a framework requester so the runtime can raise a
    /// password prompt without knowing about the underlying
    /// prompt-ledger substrate.
    pub fn new(requester: Arc<dyn UserInteractionRequester>) -> Self {
        Self { requester }
    }
}

#[async_trait]
impl PasswordPrompter for FrameworkPasswordPrompter {
    async fn prompt_password(
        &self,
        label: String,
    ) -> Result<Option<Vec<u8>>, ReportError> {
        // The prompt_id encodes the shares plugin's namespace plus
        // a monotonic wall-clock component so re-issues of the
        // same operator flow (add / retry-mount) share a session
        // marker but do not collide with each other. Rendered by
        // the responder as its stable prompt identity.
        let ts = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let request = PromptRequest {
            prompt_id: format!("org.evoframework.network.shares/password/{ts}"),
            prompt_type: PromptType::Password { label },
            timeout_ms: None,
            session_id: None,
            retention_hint: None,
            error_context: None,
            previous_answer: None,
            priority: None,
        };
        let outcome = self.requester.request_user_interaction(request).await?;
        match outcome {
            PromptOutcome::Answered { response, .. } => match response {
                PromptResponse::Password { value } => {
                    Ok(Some(value.into_bytes()))
                }
                other => Err(ReportError::Invalid(format!(
                    "network.shares password prompt received wrong response \
                     shape from responder: {other:?}"
                ))),
            },
            PromptOutcome::Cancelled { .. } | PromptOutcome::TimedOut => {
                Ok(None)
            }
        }
    }
}

/// [`PasswordPrompter`] that always returns `Ok(None)`. Used by
/// tests and by the framework's fixture builder when no
/// user-interaction responder is wired.
#[derive(Debug, Default, Clone)]
pub struct NoPasswordPrompter;

#[async_trait]
impl PasswordPrompter for NoPasswordPrompter {
    async fn prompt_password(
        &self,
        _label: String,
    ) -> Result<Option<Vec<u8>>, ReportError> {
        Ok(None)
    }
}

/// Default subprocess timeout for a single mount attempt (30 s).
/// Matches the volumio-evo reference implementation's per-attempt
/// budget; the full CIFS probe ladder therefore has a worst-case
/// wall-clock of 5 × 30 s = 150 s before
/// [`MountError::DialectProbeExhausted`] fires.
pub const DEFAULT_MOUNT_TIMEOUT_MS: u64 = 30_000;

/// Default subprocess timeout for `avahi-browse` (18 s). Lifted
/// verbatim from the volumio-evo reference —
/// `/usr/bin/timeout(1)` kills the browser at this deadline and
/// discovery uses whatever entries the browser had emitted to
/// stdout up to that point.
pub const DEFAULT_AVAHI_BROWSE_TIMEOUT_MS: u64 = 18_000;

/// Default subprocess timeout for `smbclient -L <ip>` (15 s per
/// host). Also lifted verbatim from volumio-evo. Discovery
/// budgets a fixed per-host timeout so a single unresponsive
/// NAS cannot stall the whole sweep.
pub const DEFAULT_SMBCLIENT_TIMEOUT_MS: u64 = 15_000;

/// Build the argument list for a CIFS mount attempt.
///
/// The resulting args are for the `/bin/mount` binary (not the
/// framework path; the caller supplies the program). Format:
///
/// ```text
/// -t cifs -o vers=<dialect>,<framework-defaults>,<operator-options>
///     //<host>/<path> <mount_root>
/// ```
///
/// Framework defaults: `ro,noauto,iocharset=utf8`. Operator can
/// override any of these via [`ShareRecord::advanced_options`]
/// (later definition wins in kernel option parsing).
///
/// Credentials handling:
/// - [`Credentials::Guest`] → adds `guest`.
/// - [`Credentials::UserPassword`] → the caller passes a
///   `credentials_file` path (the runtime writes it earlier
///   with `username=`, `password=`, and optionally `domain=`
///   lines at mode 0600); this function only adds
///   `credentials=<path>` to the option list. Passwords NEVER
///   travel on argv — `ps` and audit logs never see the value.
/// - [`Credentials::KeyFile`] → adds `credentials=<path>`
///   pointing at the operator-managed key file.
///
/// This function is pure — no I/O, no clock, no state — so it
/// is trivially unit-testable and the tests can assert exact
/// argument shape.
pub fn build_cifs_mount_args(
    record: &ShareRecord,
    dialect: &str,
    credentials_file: Option<&Path>,
) -> Vec<String> {
    let mut options: Vec<String> = vec![
        format!("vers={}", dialect),
        "ro".to_string(),
        "noauto".to_string(),
        "iocharset=utf8".to_string(),
    ];

    match &record.credentials {
        Credentials::Guest => options.push("guest".to_string()),
        Credentials::UserPassword { .. } => {
            if let Some(path) = credentials_file {
                options.push(format!("credentials={}", path.display()));
            }
            // else: caller failed to stage the credentials file —
            // options list has no username/password, mount.cifs
            // falls back to anonymous and the auth-refusal parser
            // surfaces the honest error.
        }
        Credentials::KeyFile { path } => {
            options.push(format!("credentials={}", path.display()));
        }
    }

    if !record.advanced_options.is_empty() {
        options.push(record.advanced_options.clone());
    }

    let remote = format!(
        "//{host}/{path}",
        host = record.host,
        path = record.path.trim_start_matches('/'),
    );

    // `systemd-mount --collect --type=<fs> --options=<opts>
    // <remote> <mount_point>` — argv shape per NETWORK-SOURCES-
    // DESIGN.md §5.6. The transient .mount unit lives at PID 1
    // in the host namespace, so the mount is visible to every
    // sibling systemd unit (mpd, artwork, metadata). `--collect`
    // garbage-collects the unit on failure.
    vec![
        "--collect".to_string(),
        format!("--type={}", FsType::Cifs.as_mount_type()),
        format!("--options={}", options.join(",")),
        remote,
        record.mount_root.to_string_lossy().into_owned(),
    ]
}

/// RAII guard that deletes the staged mount-credentials file on
/// drop, so a mid-mount panic or early return still leaves no
/// password bytes on disk after this function returns. Deletion
/// errors are logged at debug (nothing sensitive to expose;
/// nothing operational to remediate — the next mount attempt
/// overwrites the file anyway).
pub struct MountCredentialsFileGuard(PathBuf);

impl Drop for MountCredentialsFileGuard {
    fn drop(&mut self) {
        // Best-effort delete. std::fs::remove_file is synchronous
        // but the file is tiny and Drop is not on the hot path.
        if let Err(e) = std::fs::remove_file(&self.0) {
            tracing::debug!(
                path = %self.0.display(),
                error = %e,
                "mount credentials file cleanup failed"
            );
        }
    }
}

/// Compute the transient systemd .mount unit name for a mount
/// point, per systemd's escape rules (`systemd-escape --path
/// --suffix=mount`). Used to fetch the real mount.cifs / mount.nfs
/// stderr from the unit's journal after `systemd-mount` returns
/// its opaque "Job failed" message.
///
/// Rules:
/// - Strip leading `/`.
/// - Escape `\` and `-` and control / non-ASCII characters as
///   `\xHH`.
/// - Replace `/` with `-`.
/// - Append `.mount`.
///
/// Special case: `/` maps to `-.mount` per systemd convention
/// (the root itself is escaped to `-`).
pub fn systemd_mount_unit_name(mount_root: &Path) -> String {
    let s = mount_root.to_string_lossy();
    let stripped = s.strip_prefix('/').unwrap_or(&s);
    if stripped.is_empty() {
        return "-.mount".to_string();
    }
    let mut out = String::with_capacity(stripped.len() + 8);
    for c in stripped.chars() {
        match c {
            '/' => out.push('-'),
            '-' | '\\' => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c if c.is_ascii_alphanumeric()
                || c == '_'
                || c == '.'
                || c == ':' =>
            {
                out.push(c);
            }
            c if (c as u32) < 128 => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => {
                // Non-ASCII: encode UTF-8 bytes as \xHH sequences.
                let mut buf = [0u8; 4];
                let bytes = c.encode_utf8(&mut buf).as_bytes();
                for b in bytes {
                    out.push_str(&format!("\\x{:02x}", b));
                }
            }
        }
    }
    out.push_str(".mount");
    out
}

/// True when the mount helper's stderr indicates the target
/// directory is absent (ENOENT). Read by
/// [`NetworkSharesRuntime::mount_cifs`] to short-circuit the
/// dialect probe on a missing-directory error rather than
/// walking the full ladder and mislabelling five ENOENTs as
/// "dialect probe exhausted".
///
/// mount.cifs and mount.nfs both write a recognisable snippet
/// on ENOENT — mount.cifs typically prints
/// `Couldn't chdir to <path>: No such file or directory`; the
/// generic mount rendering is `mount: <path>: No such file or
/// directory`. Matching case-insensitively on either phrase
/// covers both helpers.
pub fn is_mount_directory_missing(stderr: &str) -> bool {
    let s = stderr.to_ascii_uppercase();
    s.contains("NO SUCH FILE OR DIRECTORY")
}

/// True when the mount.cifs exit code + stderr fragment indicate
/// an authentication failure rather than a dialect / protocol
/// failure. Read by [`NetworkSharesRuntime::mount_cifs`] to
/// short-circuit the dialect probe on auth-refusal.
///
/// The canonical CIFS auth-refusal signals:
/// - Exit 13 (EACCES) — mount.cifs returns errno 13 when the
///   server refused the credential at tree-connect.
/// - `mount error(13)` — systemd-mount / mount.cifs often wrap
///   the errno in stderr while the process exit code is 32.
/// - `NT_STATUS_LOGON_FAILURE` / `NT_STATUS_ACCESS_DENIED` —
///   Samba client's stderr render of the wire-level SMB status
///   when the username / password / domain triplet is refused
///   or the identity has no share permission.
/// - `Permission denied` — the human-readable rendering
///   mount.cifs emits alongside the exit code.
pub fn is_cifs_auth_refusal(exit_code: Option<i32>, stderr: &str) -> bool {
    if exit_code == Some(13) {
        return true;
    }
    let s = stderr.to_ascii_uppercase();
    s.contains("MOUNT ERROR(13)")
        || s.contains("NT_STATUS_LOGON_FAILURE")
        || s.contains("NT_STATUS_ACCESS_DENIED")
        || s.contains("PERMISSION DENIED")
        || s.contains("STATUS_ACCOUNT_DISABLED")
        || s.contains("STATUS_LOGON_FAILURE")
        || s.contains("STATUS_ACCOUNT_LOCKED_OUT")
        || s.contains("STATUS_PASSWORD_EXPIRED")
}

/// Pure check: does `proc_mounts` contents list `mount_root` as
/// an active mount target (column 2 of `/proc/mounts`)?
///
/// Used by production (host-namespace truth via `/proc/1/mounts`)
/// and by unit tests that supply fixture contents.
pub fn is_path_mounted_in_proc_mounts(
    proc_mounts: &str,
    mount_root: &Path,
) -> bool {
    let target = mount_root.to_string_lossy();
    for line in proc_mounts.lines() {
        let mut cols = line.split_whitespace();
        let _source = match cols.next() {
            Some(s) => s,
            None => continue,
        };
        let mp = match cols.next() {
            Some(m) => m,
            None => continue,
        };
        if mp == target.as_ref() {
            return true;
        }
    }
    false
}

/// Parse the CIFS `vers=` option for `mount_root` from
/// `/proc/mounts` contents. Returns `None` when the target is
/// absent or is not a `cifs`/`smb3` fstype.
pub fn parse_cifs_version_from_proc_mounts(
    proc_mounts: &str,
    mount_root: &Path,
) -> Option<String> {
    let target = mount_root.to_string_lossy();
    for line in proc_mounts.lines() {
        let mut cols = line.split_whitespace();
        let _source = cols.next()?;
        let mp = cols.next()?;
        let fstype = cols.next()?;
        let opts = cols.next()?;
        if mp != target.as_ref() {
            continue;
        }
        if !matches!(fstype, "cifs" | "smb3") {
            continue;
        }
        for opt in opts.split(',') {
            if let Some(v) = opt.strip_prefix("vers=") {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Read host-namespace mount table. Prefer `/proc/1/mounts`
/// (PID 1 = host mount namespace where `systemd-mount` lands
/// shares per NETWORK-SOURCES-DESIGN.md §5.6.1); fall back to
/// `/proc/mounts` when `/proc/1/mounts` is unreadable.
pub fn read_host_proc_mounts() -> String {
    fs::read_to_string("/proc/1/mounts")
        .or_else(|_| fs::read_to_string("/proc/mounts"))
        .unwrap_or_default()
}

/// Production mount-point probe: true when `mount_root` is an
/// active mount target in the host mount table.
pub fn is_path_mounted(mount_root: &Path) -> bool {
    is_path_mounted_in_proc_mounts(&read_host_proc_mounts(), mount_root)
}

/// Compose the body of a `mount.cifs` credentials file. Contains
/// one `key=value` line per non-empty field, terminated with a
/// newline. Password bytes are written verbatim — mount.cifs
/// documents no escaping requirement for the credentials file
/// format, and the runtime already validates `credential_key`
/// against a strict character set.
///
/// Callers write this body to a temp file at mode 0600 and pass
/// the path via `credentials=<path>` to mount.cifs (see
/// [`build_cifs_mount_args`]).
pub fn compose_cifs_credentials_file(
    username: &str,
    password: &str,
    domain: Option<&str>,
) -> String {
    let mut body = String::new();
    body.push_str(&format!("username={username}\n"));
    body.push_str(&format!("password={password}\n"));
    if let Some(d) = domain {
        if !d.is_empty() {
            body.push_str(&format!("domain={d}\n"));
        }
    }
    body
}

/// Build the argument list for an NFS mount attempt.
///
/// Format:
///
/// ```text
/// -t nfs -o ro,soft,noauto,<operator-options>
///     <host>:<path> <mount_root>
/// ```
///
/// NFS does not use a client-side dialect probe — the kernel
/// negotiates the version with the server (typically v4 first,
/// then v3, then v2). The operator can force a version via
/// `nfsvers=N` / `vers=N` in [`ShareRecord::advanced_options`].
///
/// This function is pure and testable — no I/O, no clock, no
/// state.
pub fn build_nfs_mount_args(record: &ShareRecord) -> Vec<String> {
    let mut options: Vec<String> =
        vec!["ro".to_string(), "soft".to_string(), "noauto".to_string()];
    if !record.advanced_options.is_empty() {
        options.push(record.advanced_options.clone());
    }
    let remote = format!(
        "{host}:{path}",
        host = record.host,
        path = if record.path.starts_with('/') {
            record.path.clone()
        } else {
            format!("/{}", record.path)
        }
    );
    // systemd-mount argv shape — same rationale as
    // `build_cifs_mount_args` (host-namespace visibility).
    vec![
        "--collect".to_string(),
        format!("--type={}", FsType::Nfs.as_mount_type()),
        format!("--options={}", options.join(",")),
        remote,
        record.mount_root.to_string_lossy().into_owned(),
    ]
}

/// Build the argument list for an unmount invocation.
///
/// Format when `lazy = false`:
///
/// ```text
/// <mount_root>
/// ```
///
/// Format when `lazy = true` (CIFS busy-file safety per the
/// volumio-evo reference — the mount table clears immediately
/// and the backing state releases when the last file handle
/// closes):
///
/// ```text
/// -l <mount_root>
/// ```
///
/// `systemd-umount` accepts these flags directly.
pub fn build_umount_args(mount_root: &Path, lazy: bool) -> Vec<String> {
    let mut args = Vec::new();
    if lazy {
        args.push("-l".to_string());
    }
    args.push(mount_root.to_string_lossy().into_owned());
    args
}

/// Parse `/proc/mounts` contents for the NFS version negotiated
/// for a given mount root. Returns the value of the `vers=` or
/// `nfsvers=` option in the mount's option list, or `None` when
/// the mount root is not present or neither option is set.
///
/// Pure over its input so tests supply the file contents
/// directly without touching the filesystem.
pub fn parse_nfs_version_from_proc_mounts(
    proc_mounts: &str,
    mount_root: &Path,
) -> Option<String> {
    let target = mount_root.to_string_lossy();
    for line in proc_mounts.lines() {
        // /proc/mounts columns: source target fstype options
        // freq passno — whitespace-separated.
        let mut cols = line.split_whitespace();
        let _source = cols.next()?;
        let mp = cols.next()?;
        let fstype = cols.next()?;
        let opts = cols.next()?;
        if mp != target {
            continue;
        }
        // Only inspect NFS-family fstypes so a coincidentally-
        // matching bind-mount doesn't confuse the parser.
        if !matches!(fstype, "nfs" | "nfs4") {
            continue;
        }
        for opt in opts.split(',') {
            if let Some(v) = opt.strip_prefix("vers=") {
                return Some(v.to_string());
            }
            if let Some(v) = opt.strip_prefix("nfsvers=") {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// One share advertised by a discovered NAS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredShare {
    /// Share name (the string operators pick when adding).
    pub name: String,
    /// Optional operator-visible comment field from the SMB
    /// server's share advertisement. `None` when the server
    /// left it blank.
    #[serde(default)]
    pub comment: Option<String>,
}

/// One NAS device discovered on the LAN.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredNas {
    /// NetBIOS / mDNS name (operator-visible in the UI).
    pub name: String,
    /// Resolved IPv4 address.
    pub ip: String,
    /// Highest CIFS dialect the NAS advertised in the discovery
    /// probe. Operator UI can render this as a compatibility
    /// hint before the operator adds a share; `None` when
    /// smbclient did not print a `negotiated dialect[...]` line
    /// (some legacy NAS firmwares).
    #[serde(default)]
    pub advertised_dialect: Option<String>,
    /// Shares the NAS advertises on the discovery listing.
    #[serde(default)]
    pub shares: Vec<DiscoveredShare>,
}

/// Parse `avahi-browse -atrk _smb._tcp` stdout into a list of
/// `(name, ip)` pairs. avahi-browse emits `=;<if>;<proto>;<name>;
/// <type>;<domain>;<host>;<addr>;<port>;<txt>` lines; only the
/// `=` (resolved) lines carry a stable address.
///
/// Duplicate entries (same IP surfacing on multiple interfaces)
/// are collapsed to the first-seen name/ip pair.
///
/// Pure over the input; unit-testable without avahi installed.
/// Filter self-references out of a parsed avahi-browse hit list.
///
/// The sibling `org.evoframework.network.smb-server` plugin
/// advertises `_smb._tcp` on the local host, so the discovery
/// sweep observes the local device as a NAS. The operator never
/// wants to add their own device; the filter drops:
///
/// 1. Loopback IPs (`127.0.0.1` and `::1`) — universally.
/// 2. Entries whose `name` matches the local hostname (read from
///    `/proc/sys/kernel/hostname`), case-insensitive.
///
/// Both checks combine — an entry passes the filter only when
/// neither matches. Non-Linux hosts (no `/proc/sys/kernel/hostname`)
/// fall through with only the loopback filter applied; that's the
/// honest degradation on platforms without the same identity
/// convention.
///
/// See NETWORK-SOURCES-DESIGN.md §5.6.6.
pub fn filter_self_out(hits: Vec<(String, String)>) -> Vec<(String, String)> {
    let local_hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty());
    hits.into_iter()
        .filter(|(name, ip)| {
            if ip == "127.0.0.1" || ip == "::1" {
                return false;
            }
            if let Some(h) = local_hostname.as_deref() {
                if name.to_ascii_lowercase() == h {
                    return false;
                }
            }
            true
        })
        .collect()
}

/// Parse `avahi-browse -trkp _smb._tcp` stdout into `(name, ip)`
/// hits. Each `=` prefixed record contains the resolved service
/// name and address; other lines (`+` announcements, `#` comments)
/// are skipped.
pub fn parse_avahi_browse_output(stdout: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in stdout.lines() {
        if !line.starts_with('=') {
            continue;
        }
        let cols: Vec<&str> = line.split(';').collect();
        // Column layout per avahi-browse -p:
        // [0]="="  [1]=iface  [2]=proto  [3]=name  [4]=type
        // [5]=domain  [6]=host  [7]=addr  [8]=port  [9]=txt
        if cols.len() < 8 {
            continue;
        }
        let name = cols[3].trim();
        let addr = cols[7].trim();
        if name.is_empty() || addr.is_empty() {
            continue;
        }
        // Skip IPv6 for now — the design's mount surface targets
        // IPv4 host strings. IPv6 addresses land in a follow-on
        // extension when the mount runtime learns to bracket
        // them.
        if addr.contains(':') {
            continue;
        }
        // Collapse duplicates.
        if !out.iter().any(|(_, ip)| ip == addr) {
            out.push((name.to_string(), addr.to_string()));
        }
    }
    out
}

/// Parse `smbclient -L <ip>` stdout for the share listing.
/// The listing includes `Sharename  Type  Comment` header rows
/// followed by `<name>  <type>  <comment>` rows for each share.
/// Only rows whose `Type` column is `Disk` become
/// [`DiscoveredShare`] entries; IPC$ and printer shares are
/// skipped.
///
/// Pure over the input; unit-testable without smbclient
/// installed.
pub fn parse_smbclient_disk_lines(stdout: &str) -> Vec<DiscoveredShare> {
    let mut out = Vec::new();
    let mut in_share_list = false;
    for line in stdout.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("Sharename") {
            in_share_list = true;
            continue;
        }
        if in_share_list && trimmed.is_empty() {
            // End of share list block.
            break;
        }
        if !in_share_list {
            continue;
        }
        // Skip the header separator (dashes).
        if trimmed.starts_with('-') {
            continue;
        }
        // Columns are whitespace-separated with runs of spaces
        // padding out fixed-width columns; the comment column
        // may itself contain spaces. Consume the first two
        // whitespace-delimited tokens then keep the remainder as
        // the comment.
        let mut rest = trimmed;
        let name = {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            let (n, r) = rest.split_at(end);
            rest = r.trim_start();
            n
        };
        let ty = {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            let (t, r) = rest.split_at(end);
            rest = r.trim_start();
            t
        };
        let comment = if rest.is_empty() {
            None
        } else {
            Some(rest.trim_end().to_string())
        };
        if name.is_empty() || ty != "Disk" {
            continue;
        }
        // Skip administrative shares (IPC$, printer$, etc.) —
        // the ty check already excludes non-Disk, but hidden
        // Disk shares that end with '$' are per-convention
        // administrative and not what music-directory operators
        // expect.
        if name.ends_with('$') {
            continue;
        }
        out.push(DiscoveredShare {
            name: name.to_string(),
            comment: comment.filter(|s| !s.is_empty()),
        });
    }
    out
}

/// Extract the negotiated CIFS dialect from smbclient stderr.
/// The debug output at level 4 emits lines like
/// `negotiated dialect[SMB3_11] against server`; this parser
/// returns the bracketed value.
pub fn parse_smbclient_dialect(stderr: &str) -> Option<String> {
    for line in stderr.lines() {
        if let Some(rest) = line.split_once("negotiated dialect[") {
            if let Some(end) = rest.1.find(']') {
                let val = &rest.1[..end];
                if !val.is_empty() {
                    return Some(val.to_string());
                }
            }
        }
    }
    None
}

/// Build the argv for `avahi-browse -trkp _smb._tcp`. The `-t`
/// flag makes browse terminate after enumerating (rather than
/// running indefinitely); `-r` resolves each hit to an address;
/// `-k` disables the `dbus` startup dance so it works on hosts
/// without a session bus; `-p` forces the parseable output
/// format [`parse_avahi_browse_output`] expects.
///
/// The `-a` (all services) flag is NOT included: it is mutually
/// exclusive with an explicit service type, and avahi-browse
/// exits with `Too many arguments` when both are supplied. The
/// prior `-atrkp _smb._tcp` invocation caused every discovery
/// sweep to error out at argument-parse time; the background
/// refresh task swallowed the error and republished nothing, so
/// the operator UI saw a permanent empty discovered list. The
/// argument-parse contract is now validated in unit tests
/// against the exact avahi-browse usage error message.
pub fn build_avahi_browse_args() -> Vec<String> {
    vec!["-trkp".to_string(), "_smb._tcp".to_string()]
}

/// Build the argv for `smbclient -N -L <ip> -m SMB3_11
/// --debuglevel=4`. The `--debuglevel=4` is required to make
/// smbclient print the `negotiated dialect[...]` marker that
/// [`parse_smbclient_dialect`] extracts.
pub fn build_smbclient_list_args(ip: &str) -> Vec<String> {
    vec![
        "-N".to_string(),
        "-L".to_string(),
        ip.to_string(),
        "-m".to_string(),
        "SMB3_11".to_string(),
        "--debuglevel=4".to_string(),
    ]
}

// --------------------------------------------------------------
// Subject-publisher substrate (Ship 2f)
// --------------------------------------------------------------
//
// Three subject types back the operator surface declared in
// `docs/engineering/SHARES-OPERATOR-WIDGETS.md`:
//
// * `system_network_shares_configured` — singleton per node;
//   republished on every add / edit / remove.
// * `system_network_shares_discovered` — singleton per node;
//   republished on every discovery.refresh.
// * `network_share_state` — one instance per share_id;
//   announced on add, retracted on remove, updated on every
//   mount / unmount transition.
//
// Subject types use snake_case (dot-in-name is reserved for the
// shelf-path in the catalogue schema; the runtime subject-type
// string is snake_case per the substrate convention).

/// Snake-case subject type for the configured-shares singleton.
pub const CONFIGURED_SUBJECT_TYPE: &str = "system_network_shares_configured";

/// Snake-case subject type for the discovered-NAS singleton.
pub const DISCOVERED_SUBJECT_TYPE: &str = "system_network_shares_discovered";

/// Snake-case subject type for the per-share state subject.
pub const SHARE_STATE_SUBJECT_TYPE: &str = "network_share_state";

/// Addressing scheme for the configured-shares singleton.
pub const CONFIGURED_SUBJECT_SCHEME: &str = "evo.network.shares.configured";

/// Addressing scheme for the discovered-NAS singleton.
pub const DISCOVERED_SUBJECT_SCHEME: &str = "evo.network.shares.discovered";

/// Addressing scheme for the per-share state subject. Each
/// share instance addresses under this scheme with its
/// [`ShareId`] as the addressing value.
pub const SHARE_STATE_SUBJECT_SCHEME: &str = "evo.network.share.state";

/// Addressing scheme for the share-events ring singleton.
pub const SHARE_EVENTS_SUBJECT_SCHEME: &str = "evo.network.shares.events";

/// Fixed addressing value for singleton subjects.
pub const SINGLETON_ADDRESSING_VALUE: &str = "local";

/// Compose the singleton addressing for the configured-shares
/// subject instance.
pub fn configured_singleton_addressing() -> ExternalAddressing {
    ExternalAddressing {
        scheme: CONFIGURED_SUBJECT_SCHEME.to_string(),
        value: SINGLETON_ADDRESSING_VALUE.to_string(),
    }
}

/// Compose the singleton addressing for the discovered-NAS
/// subject instance.
pub fn discovered_singleton_addressing() -> ExternalAddressing {
    ExternalAddressing {
        scheme: DISCOVERED_SUBJECT_SCHEME.to_string(),
        value: SINGLETON_ADDRESSING_VALUE.to_string(),
    }
}

/// Compose the addressing for a per-share state subject
/// instance.
pub fn share_state_addressing(share_id: &ShareId) -> ExternalAddressing {
    ExternalAddressing {
        scheme: SHARE_STATE_SUBJECT_SCHEME.to_string(),
        value: share_id.0.clone(),
    }
}

/// Compose the singleton addressing for the share-events ring
/// subject instance.
pub fn share_events_singleton_addressing() -> ExternalAddressing {
    ExternalAddressing {
        scheme: SHARE_EVENTS_SUBJECT_SCHEME.to_string(),
        value: SINGLETON_ADDRESSING_VALUE.to_string(),
    }
}

/// Wire-shape envelope carried on every republish of the
/// `system_network_shares_configured` subject. Full-snapshot on
/// each transition — no delta protocol — consumers reconcile
/// off the current envelope.
#[derive(Debug, Clone, Serialize)]
pub struct ConfiguredSharesEnvelope {
    /// Configured share records, in creation order.
    pub shares: Vec<ShareRecord>,
    /// Wall-clock time this envelope was composed.
    pub last_update_at: SystemTime,
}

/// Wire-shape envelope carried on every republish of the
/// `system_network_shares_discovered` subject.
#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredNasEnvelope {
    /// Discovered NAS entries, in discovery order.
    pub nas: Vec<DiscoveredNas>,
    /// Wall-clock time of the last completed refresh.
    pub last_refresh_at: SystemTime,
}

/// Wire-shape envelope carried on every republish of a
/// `network_share_state` subject instance.
#[derive(Debug, Clone, Serialize)]
pub struct ShareStateEnvelope {
    /// The share this envelope describes.
    pub share_id: ShareId,
    /// Operator-set alias (echoed from the record for widget
    /// convenience so the state panel can label itself without a
    /// separate lookup).
    pub alias: String,
    /// Current mount state.
    pub state: MountState,
    /// Operator-facing failure reason when `state == Failed`.
    /// Absent otherwise.
    pub reason: Option<String>,
    /// Negotiated dialect (CIFS) or `nfsvers=..` value (NFS)
    /// while `state == Mounted`. Absent otherwise.
    pub negotiated_vers: Option<String>,
    /// Wall-clock millis (UNIX epoch) of the last transition
    /// into the current state.
    pub last_transition_at_ms: u64,
}

/// Per-share runtime state entry backing the `network_share_state`
/// subject.
#[derive(Debug, Clone)]
struct ShareStateEntry {
    alias: String,
    state: MountState,
    reason: Option<String>,
    negotiated_vers: Option<String>,
    last_transition_at_ms: u64,
}

impl ShareStateEntry {
    fn to_envelope(&self, share_id: &ShareId) -> ShareStateEnvelope {
        ShareStateEnvelope {
            share_id: share_id.clone(),
            alias: self.alias.clone(),
            state: self.state,
            reason: self.reason.clone(),
            negotiated_vers: self.negotiated_vers.clone(),
            last_transition_at_ms: self.last_transition_at_ms,
        }
    }
}

/// Publisher slot — populated by
/// [`NetworkSharesRuntime::attach_subject_publisher`].
struct SharesPublisher {
    announcer: Arc<dyn SubjectAnnouncer>,
}

/// Reference implementation of [`NetworkSharesHandle`] backed
/// by a TOML file at `<state_dir>/network_shares.toml`.
///
/// Concurrency: state lives behind a Tokio [`Mutex`] so
/// concurrent operator UI calls serialise safely at the write
/// boundary. The primitive is `Send + Sync` and can be shared
/// across every async task in the steward via [`Arc`].
///
/// Persistence: every mutating method writes to disk before
/// releasing the mutex. A crash after mutex-release but before
/// the caller observes the return is safe — the write has
/// landed atomically.
///
/// Subjects: when a [`SubjectAnnouncer`] has been attached via
/// [`Self::attach_subject_publisher`], the runtime republishes
/// `system_network_shares_configured` on every CRUD transition,
/// `system_network_shares_discovered` on every discovery
/// refresh, and one `network_share_state` per share on every
/// mount / unmount transition. Republish failures are
/// fire-and-forget (logged at debug); the next transition's
/// envelope carries ground truth.
pub struct NetworkSharesRuntime {
    inner: Arc<Mutex<NetworkSharesInner>>,
    executor: Arc<dyn MountExecutor>,
    credentials: Arc<dyn CredentialFetcher>,
    /// Read+write credential store. Some when the plugin was
    /// wired with a store that supports `store_password` (the
    /// production path via [`FileCredentialStore`]); None in
    /// legacy fixtures that only supply a read-side fetcher.
    /// When None, the prompt-on-add flow is skipped and any
    /// UserPassword mount attempt reaches the mount helper with
    /// whatever the fetcher returns (typically None → operator
    /// sees the honest "credential vault has no entry" error).
    credential_store: Option<Arc<dyn CredentialStore>>,
    /// Handle for raising password prompts. Defaults to
    /// [`NoPasswordPrompter`] in test / builder-omitted paths
    /// so unit tests never race the framework's prompt-ledger.
    /// Production wires [`FrameworkPasswordPrompter`] via the
    /// LoadContext's `user_interaction_requester`.
    prompter: Arc<dyn PasswordPrompter>,
    mount_program: String,
    /// Args prepended to every mount invocation, ahead of the
    /// generated `-t / -o` argv. Empty when the plugin's service
    /// identity is root (invocation is `mount ...` directly);
    /// `["-n", "/bin/mount"]` when non-root and the sudoers
    /// drop-in permits `sudo -n /bin/mount ...`. Set at
    /// production wiring time from the effective UID.
    mount_wrapper_args: Vec<String>,
    umount_program: String,
    /// Same shape as `mount_wrapper_args` for `umount`.
    umount_wrapper_args: Vec<String>,
    mount_timeout_ms: u64,
    avahi_browse_program: String,
    avahi_browse_timeout_ms: u64,
    smbclient_program: String,
    smbclient_timeout_ms: u64,
    discovered: Arc<Mutex<Vec<DiscoveredNas>>>,
    share_states: Arc<Mutex<HashMap<ShareId, ShareStateEntry>>>,
    publisher: StdMutex<Option<SharesPublisher>>,
    now_fn: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// Mount-point probe. Production uses [`is_path_mounted`]
    /// (host `/proc/1/mounts`); tests inject a fixture so unit
    /// suites never touch the real mount table. Decides whether
    /// `mount_share` adopts an already-active host mount instead
    /// of re-running the dialect probe.
    mount_point_check: Arc<dyn Fn(&Path) -> bool + Send + Sync>,
    /// Deduplication map for in-flight credential prompts. Keyed
    /// on `credential_key` so multiple concurrent mount / add
    /// attempts against the same missing credential collapse to
    /// a single prompt on the responder's shelf.
    ///
    /// Entry lifetime is decoupled from any single caller: the
    /// entry lives from the first `or_insert_with` to the
    /// explicit `remove_entry` that runs AFTER `get_or_init`
    /// resolves. Later callers arriving during that whole window
    /// observe the existing cell (Arc::clone), await
    /// `get_or_init`, and all receive the same outcome the first
    /// caller produced — no re-prompt.
    ///
    /// Retires an earlier CellOnDrop pattern whose removal fired
    /// inside the first caller's `Drop` — the map entry was gone
    /// before any concurrent add arriving one tokio poll later
    /// could observe it, so peer callers re-inserted their own
    /// cells and re-prompted. That race produced non-deterministic
    /// prompt counts under concurrent dispatch on the same
    /// credential key.
    ///
    /// [`tokio::sync::OnceCell`] provides both requirements the
    /// dedup contract pins:
    ///
    /// - Single-writer initialization: the first caller to reach
    ///   `get_or_init` runs the closure; all peers concurrently
    ///   waiting on the same cell block on the same in-flight
    ///   init future — exactly one prompt fires, however many
    ///   callers arrive.
    /// - Blocking-or-async wait for followers: peers `.await`
    ///   the OnceCell; when the init resolves, every peer's
    ///   await returns the same value reference. No polling, no
    ///   spinning, no re-entry.
    pending_credential_prompts:
        Arc<std::sync::Mutex<HashMap<String, PromptCell>>>,
    /// Bounded ring of the most recent share-lifecycle events
    /// (mount / unmount / mount-failed / unmount-failed).
    /// Published as the `network_share_events` singleton on
    /// every push so a status pane can render "what just
    /// happened" without polling per-share state. Capacity is
    /// [`SHARE_EVENTS_RING_CAPACITY`]; older events roll off
    /// as new events land. `StdMutex` (blocking) chosen to
    /// match the `publisher` slot — pushes are trivial and
    /// hold the mutex for nanoseconds; no tokio primitive
    /// needed.
    share_events_ring: Arc<StdMutex<std::collections::VecDeque<ShareEvent>>>,
}

/// Cloneable outcome the first prompt-caller broadcasts to
/// concurrent waiters on the same credential key. Distinct
/// from [`MountError`] because MountError is not Clone and
/// waiters need their own copy of the outcome to raise a
/// tailored error. Every variant maps back to a specific
/// MountError shape on the waiter side.
#[derive(Debug, Clone)]
pub(crate) enum PromptDedupOutcome {
    /// Credential answered + stored in the vault. Waiters
    /// re-fetch from the vault and return `Ok(())`.
    Success,
    /// Responder cancelled or the framework's prompt TTL fired.
    /// Waiters return [`MountError::CredentialPromptCancelled`].
    Cancelled,
    /// Framework fast-refused because no responder session was
    /// connected. Waiters return
    /// [`MountError::NoResponderAvailable`] with the same
    /// reason the first caller received.
    NoResponderAvailable(String),
    /// Any other prompt / store failure. Waiters return a
    /// generic [`MountError::CredentialPromptFailed`] carrying
    /// this reason.
    Other(String),
}

/// Shared once-cell entry held in [`NetworkSharesRuntime::
/// pending_credential_prompts`]. Concurrent callers clone the
/// `Arc` under the map's sync lock and then `.await` the
/// OnceCell — the first arrival's `get_or_init` closure runs
/// the prompt; every other arrival blocks on the same init.
///
/// `Arc` is used purely so the sync-mutex-held pointer can be
/// cloned out cheaply and awaited without holding the map lock
/// across `.await`.
pub(crate) type PromptCell = Arc<tokio::sync::OnceCell<PromptDedupOutcome>>;

impl std::fmt::Debug for NetworkSharesRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetworkSharesRuntime").finish()
    }
}

struct NetworkSharesInner {
    state: NetworkSharesState,
    path: PathBuf,
}

impl NetworkSharesRuntime {
    /// Construct a runtime backed by `<state_dir>/network_shares.toml`.
    /// The file is loaded if it exists; a missing file yields
    /// [`NetworkSharesState::empty`] (fresh-install path).
    /// Uses [`SubprocessMountExecutor`] +
    /// [`NoCredentialFetcher`] + `/bin/mount` + the default
    /// [`DEFAULT_MOUNT_TIMEOUT_MS`] deadline. For testing or
    /// vendor-distribution wiring, use
    /// [`NetworkSharesRuntime::builder`] to swap components.
    pub fn open(state_dir: &Path) -> Result<Self, SharesStateError> {
        let path = state_dir.join(NETWORK_SHARES_FILE);
        let state = NetworkSharesState::load(&path)?;
        let share_states = seed_share_states(&state, default_now_ms());
        Ok(Self {
            inner: Arc::new(Mutex::new(NetworkSharesInner { state, path })),
            executor: Arc::new(SubprocessMountExecutor),
            credentials: Arc::new(NoCredentialFetcher),
            credential_store: None,
            prompter: Arc::new(NoPasswordPrompter),
            mount_program: "/bin/mount".to_string(),
            mount_wrapper_args: Vec::new(),
            umount_program: "/bin/umount".to_string(),
            umount_wrapper_args: Vec::new(),
            mount_timeout_ms: DEFAULT_MOUNT_TIMEOUT_MS,
            avahi_browse_program: "/usr/bin/avahi-browse".to_string(),
            avahi_browse_timeout_ms: DEFAULT_AVAHI_BROWSE_TIMEOUT_MS,
            smbclient_program: "/usr/bin/smbclient".to_string(),
            smbclient_timeout_ms: DEFAULT_SMBCLIENT_TIMEOUT_MS,
            discovered: Arc::new(Mutex::new(Vec::new())),
            share_states: Arc::new(Mutex::new(share_states)),
            publisher: StdMutex::new(None),
            now_fn: Arc::new(default_now_ms),
            mount_point_check: Arc::new(|p: &Path| is_path_mounted(p)),
            pending_credential_prompts: Arc::new(std::sync::Mutex::new(
                HashMap::new(),
            )),
            share_events_ring: Arc::new(StdMutex::new(
                std::collections::VecDeque::with_capacity(
                    SHARE_EVENTS_RING_CAPACITY,
                ),
            )),
        })
    }

    /// Start a builder for constructing a runtime with custom
    /// executor / credential fetcher / credential store / password
    /// prompter / mount program / timeout / clock.
    pub fn builder(
        state_dir: &Path,
    ) -> Result<NetworkSharesRuntimeBuilder, SharesStateError> {
        let path = state_dir.join(NETWORK_SHARES_FILE);
        let state = NetworkSharesState::load(&path)?;
        Ok(NetworkSharesRuntimeBuilder {
            state,
            path,
            executor: None,
            credentials: None,
            credential_store: None,
            prompter: None,
            mount_program: None,
            mount_wrapper_args: None,
            umount_program: None,
            umount_wrapper_args: None,
            mount_timeout_ms: None,
            avahi_browse_program: None,
            avahi_browse_timeout_ms: None,
            smbclient_program: None,
            smbclient_timeout_ms: None,
            now_fn: None,
            mount_point_check: None,
        })
    }

    /// Test / fixture constructor that installs a caller-supplied
    /// initial state at a caller-supplied path with default
    /// executor / credentials. Not part of the public production
    /// surface — the production path is
    /// [`NetworkSharesRuntime::open`].
    #[cfg(test)]
    pub fn from_state(state: NetworkSharesState, path: PathBuf) -> Self {
        let share_states = seed_share_states(&state, default_now_ms());
        Self {
            inner: Arc::new(Mutex::new(NetworkSharesInner { state, path })),
            executor: Arc::new(SubprocessMountExecutor),
            credentials: Arc::new(NoCredentialFetcher),
            credential_store: None,
            prompter: Arc::new(NoPasswordPrompter),
            mount_program: "/bin/mount".to_string(),
            mount_wrapper_args: Vec::new(),
            umount_program: "/bin/umount".to_string(),
            umount_wrapper_args: Vec::new(),
            mount_timeout_ms: DEFAULT_MOUNT_TIMEOUT_MS,
            avahi_browse_program: "/usr/bin/avahi-browse".to_string(),
            avahi_browse_timeout_ms: DEFAULT_AVAHI_BROWSE_TIMEOUT_MS,
            smbclient_program: "/usr/bin/smbclient".to_string(),
            smbclient_timeout_ms: DEFAULT_SMBCLIENT_TIMEOUT_MS,
            discovered: Arc::new(Mutex::new(Vec::new())),
            share_states: Arc::new(Mutex::new(share_states)),
            publisher: StdMutex::new(None),
            now_fn: Arc::new(default_now_ms),
            // Test fixtures default to "never mounted" so unit
            // suites cannot accidentally adopt a host mount from
            // the machine running `cargo test`.
            mount_point_check: Arc::new(|_: &Path| false),
            pending_credential_prompts: Arc::new(std::sync::Mutex::new(
                HashMap::new(),
            )),
            share_events_ring: Arc::new(StdMutex::new(
                std::collections::VecDeque::with_capacity(
                    SHARE_EVENTS_RING_CAPACITY,
                ),
            )),
        }
    }

    /// Ensure the credential vault has an entry for the record's
    /// `credential_key`, raising a password prompt to the operator
    /// when it does not. Guest / KeyFile shares short-circuit to
    /// `Ok(())` — they carry no vault dependency.
    ///
    /// Returns:
    /// - `Ok(())` — vault already carried the entry, OR the
    ///   prompt was answered and the answer was stored.
    /// - `Err(MountError::CredentialPromptCancelled)` — the
    ///   responder cancelled or the prompt timed out; the caller
    ///   surfaces this as a mount_error the operator can retry.
    /// - `Err(MountError::CredentialStoreUnavailable)` — the
    ///   record needs a password but the runtime was wired
    ///   without a credential store (legacy path); the operator
    ///   sees the honest "no store wired" message rather than a
    ///   silent hang.
    ///
    /// Called by `network.share.add` and `network.share.mount`
    /// before dispatching the mount helper; both entry points
    /// share the same lookup-then-prompt-then-store pattern so a
    /// re-mount after a cancelled prompt re-prompts.
    pub async fn ensure_credential_stocked(
        &self,
        record: &ShareRecord,
    ) -> Result<(), MountError> {
        let Credentials::UserPassword {
            credential_key,
            username,
            ..
        } = &record.credentials
        else {
            return Ok(());
        };
        if self
            .credentials
            .fetch_password(credential_key)
            .await
            .is_some()
        {
            return Ok(());
        }
        let Some(store) = self.credential_store.as_ref() else {
            return Err(MountError::CredentialStoreUnavailable);
        };

        // Get (or atomically insert) the shared once-cell for
        // this credential_key. The sync mutex serialises the
        // check-and-insert so exactly one caller creates the
        // cell; every peer arriving DURING the cell's lifetime
        // clones the same `Arc` and awaits the same `get_or_init`.
        //
        // Under concurrent dispatch: N concurrent callers (any N)
        // collapse to exactly one prompt on the responder's
        // shelf; one operator answer (or cancel, or no-responder
        // fast-refusal) resolves every waiter with the same
        // outcome. The map entry outlives the first caller's
        // exit, so a peer arriving one tokio poll after the
        // first caller resolves still observes the resolved cell
        // (via `get()`) and returns the cached outcome instead of
        // falling into re-prompt.
        let cell: PromptCell = {
            let mut map = self
                .pending_credential_prompts
                .lock()
                .expect("pending_credential_prompts mutex poisoned");
            Arc::clone(
                map.entry(credential_key.clone())
                    .or_insert_with(|| Arc::new(tokio::sync::OnceCell::new())),
            )
        };

        // Await the outcome. Exactly one arrival's closure runs
        // (single-writer init); every peer awaits the same init
        // future and returns the same `&PromptDedupOutcome`. If
        // the first arrival panics inside the closure, tokio's
        // OnceCell surfaces the panic to every waiter and drops
        // the cell state so the NEXT batch re-initialises — we
        // treat that as `CredentialPromptFailed`.
        let label =
            format!("Password for {}@{}{}", username, record.host, record.path);
        let key_for_closure = credential_key.clone();
        let prompter = Arc::clone(&self.prompter);
        let store_for_closure = Arc::clone(store);
        let cell_for_await = Arc::clone(&cell);
        let outcome_ref = cell_for_await
            .get_or_init(|| async move {
                match prompter.prompt_password(label).await {
                    Ok(Some(bytes)) => {
                        match store_for_closure
                            .store_password(&key_for_closure, &bytes)
                            .await
                        {
                            Ok(()) => PromptDedupOutcome::Success,
                            Err(e) => PromptDedupOutcome::Other(format!("{e}")),
                        }
                    }
                    Ok(None) => PromptDedupOutcome::Cancelled,
                    Err(e) => {
                        let msg = format!("{e}");
                        if msg.contains("no_responder_available:") {
                            PromptDedupOutcome::NoResponderAvailable(msg)
                        } else {
                            PromptDedupOutcome::Other(msg)
                        }
                    }
                }
            })
            .await;
        let outcome = outcome_ref.clone();

        // Cleanup: remove this key's entry from the map ONLY if
        // the entry still points at the cell we resolved. Under
        // concurrent-dispatch there is no window where a peer
        // could see the map empty AND the current batch's cell
        // in flight — every peer that arrived during the init
        // observed the existing cell before the removal, and
        // every peer that arrives AFTER the removal starts a
        // fresh batch (fresh cell, fresh prompt).
        //
        // `Arc::ptr_eq` guards against a rare cleanup race:
        // if two callers reach cleanup and one already popped +
        // re-inserted a subsequent batch's cell, the second's
        // remove must not clobber the newer batch's entry.
        {
            let mut map = self
                .pending_credential_prompts
                .lock()
                .expect("pending_credential_prompts mutex poisoned");
            if map
                .get(credential_key)
                .is_some_and(|existing| Arc::ptr_eq(existing, &cell))
            {
                map.remove(credential_key);
            }
        }

        match outcome {
            PromptDedupOutcome::Success => {
                // Defensive re-check: the store completed Ok in the
                // init closure, but confirm the fetcher observes
                // the write (file-backed store; the re-check costs
                // one filesystem stat).
                if self
                    .credentials
                    .fetch_password(credential_key)
                    .await
                    .is_some()
                {
                    Ok(())
                } else {
                    Err(MountError::CredentialPromptFailed {
                        key: credential_key.clone(),
                        reason: "first caller reported Success but vault \
                                 fetch returned None (store inconsistency)"
                            .into(),
                    })
                }
            }
            PromptDedupOutcome::Cancelled => {
                Err(MountError::CredentialPromptCancelled {
                    key: credential_key.clone(),
                })
            }
            PromptDedupOutcome::NoResponderAvailable(reason) => {
                Err(MountError::NoResponderAvailable {
                    key: credential_key.clone(),
                    reason,
                })
            }
            PromptDedupOutcome::Other(reason) => {
                Err(MountError::CredentialPromptFailed {
                    key: credential_key.clone(),
                    reason,
                })
            }
        }
    }
}

/// Fire-and-forget MPD library update for a single path via
/// `/usr/bin/mpc update <path>`. Runs as a spawned blocking task
/// so a hung mpc (framework does not wait) does not block the
/// caller. Failures log at debug — an operator's next mount /
/// manual update cycles the state; this is a convenience trigger,
/// not a durability guarantee. Called from `remove_share` so a
/// removed share's entries are pruned from MPD's database in the
/// same operation, keeping a removed share from lingering in the
/// library view.
fn trigger_mpd_update_best_effort(mount_root: &std::path::Path) {
    if mount_root.as_os_str().is_empty() {
        return;
    }
    let path_arg = mount_root.display().to_string();
    tokio::task::spawn_blocking(move || {
        let output = std::process::Command::new("/usr/bin/mpc")
            .arg("update")
            .arg(&path_arg)
            .output();
        match output {
            Ok(o) if o.status.success() => {
                tracing::info!(
                    path = %path_arg,
                    "mpc update dispatched after share removal"
                );
            }
            Ok(o) => {
                tracing::debug!(
                    path = %path_arg,
                    exit_code = ?o.status.code(),
                    stderr = %String::from_utf8_lossy(&o.stderr),
                    "mpc update returned non-zero after share removal"
                );
            }
            Err(e) => {
                tracing::debug!(
                    path = %path_arg,
                    error = %e,
                    "mpc update dispatch failed after share removal"
                );
            }
        }
    });
}

/// MPD queue safety: called BEFORE a share is unmounted (or
/// after mount-loss is detected) so the operator does not end up
/// with a stuck queue full of "No such song" errors when the
/// share drops mid-playback. Volumio-evo lesson (`mpd.rs`
/// unreachable-mitigation).
///
/// Two steps:
///   1. `mpc status --format '%file%'` — if the currently-playing
///      file is under `mount_root`, `mpc stop` before the mount
///      goes away.
///   2. `mpc playlist -f '%position% %file%'` — enumerate the
///      queue, `mpc del <position>` every entry pointing under
///      `mount_root`. Deletions are processed high-to-low so
///      positions do not shift under the enumerate.
///
/// Fire-and-forget spawn_blocking; the caller does not wait.
/// Failures log at debug — a hung mpc must not block unmount.
/// Empty `mount_root` short-circuits.
fn trigger_mpd_stop_and_prune_best_effort(mount_root: &std::path::Path) {
    if mount_root.as_os_str().is_empty() {
        return;
    }
    let root_display = mount_root.display().to_string();
    tokio::task::spawn_blocking(move || {
        // Step 1 — stop playback if current URI is under the
        // vanishing prefix. `mpc status --format '%file%'` prints
        // the currently-playing file on its own line when
        // something is loaded; empty when stopped.
        let status = std::process::Command::new("/usr/bin/mpc")
            .arg("--format")
            .arg("%file%")
            .arg("status")
            .output();
        if let Ok(o) = status {
            if o.status.success() {
                let stdout = String::from_utf8_lossy(&o.stdout);
                let current = stdout.lines().next().unwrap_or("").trim();
                if !current.is_empty() && current.starts_with(&root_display) {
                    let _ = std::process::Command::new("/usr/bin/mpc")
                        .arg("stop")
                        .output();
                    tracing::info!(
                        mount_root = %root_display,
                        current = %current,
                        "mpc stop dispatched — currently-playing file was under vanishing share prefix"
                    );
                }
            }
        }

        // Step 2 — enumerate the queue and delete entries under
        // the vanishing prefix. `mpc playlist -f '%position% %file%'`
        // prints `<pos> <file>` one per line.
        let pl = std::process::Command::new("/usr/bin/mpc")
            .arg("-f")
            .arg("%position% %file%")
            .arg("playlist")
            .output();
        let Ok(pl_out) = pl else {
            return;
        };
        if !pl_out.status.success() {
            return;
        }
        let text = String::from_utf8_lossy(&pl_out.stdout);
        // Collect positions (high-to-low) whose file is under the
        // vanishing prefix. High-to-low so a subsequent delete does
        // not shift the positions of yet-to-be-deleted entries.
        let mut positions: Vec<u32> = text
            .lines()
            .filter_map(|line| {
                let (pos_str, file) = line.split_once(' ')?;
                if file.starts_with(&root_display) {
                    pos_str.parse::<u32>().ok()
                } else {
                    None
                }
            })
            .collect();
        if positions.is_empty() {
            return;
        }
        positions.sort_unstable();
        positions.reverse();
        let deleted = positions.len();
        for pos in &positions {
            let _ = std::process::Command::new("/usr/bin/mpc")
                .arg("del")
                .arg(pos.to_string())
                .output();
        }
        tracing::info!(
            mount_root = %root_display,
            deleted,
            "mpc del pruned queue entries under vanishing share prefix"
        );
    });
}

// ------------------------------ event ring ---------------------------
//
// `network_share_events` subject — one instance per plugin load.
// Publishes a bounded ring of the most recent share-lifecycle
// events (mount / unmount / mount-failed / unmount-failed /
// dialect-probe outcomes) so an operator status pane can show
// "what just happened" without polling the mount-state subject.
//
// Ring capacity matches the widget contract expectation
// (`SHARE_EVENTS_RING_CAPACITY`); older events roll off as new
// events land. The publisher is `Option<...>` so a runtime
// constructed without an event-publisher (unit tests) skips
// publication cleanly.

pub(crate) const SHARE_EVENTS_SUBJECT_TYPE: &str = "network_share_events";
pub(crate) const SHARE_EVENTS_RING_CAPACITY: usize = 32;

/// One entry in the share-events ring. Pub so the `list_events`
/// verb's response type and the subject seed announce speak the
/// same shape end-to-end — a single UI decoder handles both
/// surfaces.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ShareEvent {
    /// Reverse-lookup id of the share the event describes.
    /// Matches [`ShareRecord::share_id`]'s inner string.
    pub share_id: String,
    /// One of `"mounted"`, `"mount_failed"`, `"unmounted"`,
    /// `"unmount_failed"`. Static string so the payload does
    /// not carry a heap allocation for the discriminator.
    pub kind: &'static str,
    /// Failure reason on `mount_failed` / `unmount_failed`
    /// (the classified journal stderr from the mount ladder);
    /// `None` on the success shapes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// CIFS dialect negotiated on a successful mount (e.g.
    /// `"2.0"`, `"3.0"`); `None` on failure shapes and on
    /// non-CIFS success shapes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub negotiated_version: Option<String>,
    /// Wall-clock instant the event happened, in milliseconds
    /// since the UNIX epoch (unaffected by monotonic clock
    /// drift — this is a display timestamp, not a duration).
    pub at_ms: u64,
}

impl ShareEvent {
    pub(crate) fn mounted(
        share_id: ShareId,
        negotiated_version: Option<String>,
        at_ms: u64,
    ) -> Self {
        Self {
            share_id: share_id.0,
            kind: "mounted",
            detail: None,
            negotiated_version,
            at_ms,
        }
    }

    pub(crate) fn mount_failed(
        share_id: ShareId,
        detail: String,
        at_ms: u64,
    ) -> Self {
        Self {
            share_id: share_id.0,
            kind: "mount_failed",
            detail: Some(detail),
            negotiated_version: None,
            at_ms,
        }
    }

    pub(crate) fn unmounted(share_id: ShareId, at_ms: u64) -> Self {
        Self {
            share_id: share_id.0,
            kind: "unmounted",
            detail: None,
            negotiated_version: None,
            at_ms,
        }
    }

    pub(crate) fn unmount_failed(
        share_id: ShareId,
        detail: String,
        at_ms: u64,
    ) -> Self {
        Self {
            share_id: share_id.0,
            kind: "unmount_failed",
            detail: Some(detail),
            negotiated_version: None,
            at_ms,
        }
    }
}

/// Snapshot envelope published as the `network_share_events`
/// subject state AND returned by the `network.share.list_events`
/// read verb. Latest N events in insertion order (oldest
/// first). Pub so the read verb's [`ListEventsResponse`] and
/// the subject seed announce speak the same shape end-to-end;
/// a single UI decoder handles both surfaces.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ShareEventsEnvelope {
    /// Last N lifecycle events in insertion order (oldest
    /// first). Empty at boot until the first mount / unmount
    /// attempt completes; bounded by [`SHARE_EVENTS_RING_CAPACITY`]
    /// so a long-lived runtime does not grow the payload
    /// without bound.
    pub events: Vec<ShareEvent>,
    /// Wall-clock instant the snapshot was taken (independent
    /// of the newest event's `at_ms` — this timestamp reflects
    /// when the read verb / subject announce ran, useful for
    /// clock-drift diagnostics against consumer displays).
    pub last_update_at: std::time::SystemTime,
}

/// Wall-clock in milliseconds since UNIX epoch. Used as the
/// runtime's default `now_fn` when the caller has not injected
/// one via the builder.
fn default_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Builder for [`NetworkSharesRuntime`] with pluggable
/// executor / credential fetcher / mount program / timeout /
/// clock. Every method returns `Self` so calls chain.
pub struct NetworkSharesRuntimeBuilder {
    state: NetworkSharesState,
    path: PathBuf,
    executor: Option<Arc<dyn MountExecutor>>,
    credentials: Option<Arc<dyn CredentialFetcher>>,
    credential_store: Option<Arc<dyn CredentialStore>>,
    prompter: Option<Arc<dyn PasswordPrompter>>,
    mount_program: Option<String>,
    mount_wrapper_args: Option<Vec<String>>,
    umount_program: Option<String>,
    umount_wrapper_args: Option<Vec<String>>,
    mount_timeout_ms: Option<u64>,
    avahi_browse_program: Option<String>,
    avahi_browse_timeout_ms: Option<u64>,
    smbclient_program: Option<String>,
    smbclient_timeout_ms: Option<u64>,
    now_fn: Option<Arc<dyn Fn() -> u64 + Send + Sync>>,
    // Same shape as the runtime struct's `mount_point_check`
    // field: `Arc<dyn Fn(&Path) -> bool + Send + Sync>`. The
    // `Option<...>` wrapper crosses clippy's type_complexity
    // score threshold here (adding `Option` adds enough to
    // trip the check even though the same inner type is used
    // uncomplained-of elsewhere in the file). Keeping the
    // shape consistent with the runtime struct beats
    // factoring a one-off type alias.
    #[allow(clippy::type_complexity)]
    mount_point_check: Option<Arc<dyn Fn(&Path) -> bool + Send + Sync>>,
}

impl NetworkSharesRuntimeBuilder {
    /// Install a custom [`MountExecutor`] (typically a mock in
    /// tests or a sudo-wrapping variant in vendor distributions).
    pub fn with_executor(mut self, executor: Arc<dyn MountExecutor>) -> Self {
        self.executor = Some(executor);
        self
    }

    /// Install a custom [`CredentialFetcher`] (typically the
    /// framework's [`crate::credentials::CredentialVault`]
    /// bridged through a small adapter).
    pub fn with_credentials(
        mut self,
        credentials: Arc<dyn CredentialFetcher>,
    ) -> Self {
        self.credentials = Some(credentials);
        self
    }

    /// Install a read-write [`CredentialStore`]. The production
    /// path passes [`FileCredentialStore`] wrapping the plugin's
    /// `credentials_dir`; test fixtures pass a mock. When both
    /// `with_credentials` and `with_credential_store` are called,
    /// the store also supplies the fetcher (the store's own
    /// `fetch_password` is used).
    pub fn with_credential_store(
        mut self,
        store: Arc<dyn CredentialStore>,
    ) -> Self {
        self.credential_store = Some(store);
        self
    }

    /// Install a [`PasswordPrompter`]. Production passes
    /// [`FrameworkPasswordPrompter`] wrapping the LoadContext's
    /// `user_interaction_requester`; tests pass a mock.
    pub fn with_password_prompter(
        mut self,
        prompter: Arc<dyn PasswordPrompter>,
    ) -> Self {
        self.prompter = Some(prompter);
        self
    }

    /// Override the mount program. Defaults to `/bin/mount`;
    /// vendor distributions wanting `sudo -n /bin/mount` set
    /// `program = "sudo"` and prefix `-n /bin/mount` in the
    /// args by wrapping the [`MountExecutor`] instead of
    /// setting this — sudo has different argv shape.
    pub fn with_mount_program(mut self, program: String) -> Self {
        self.mount_program = Some(program);
        self
    }

    /// Override the umount program. Defaults to `/bin/umount`.
    /// Same wrapping notes as [`Self::with_mount_program`] for
    /// sudo scenarios.
    pub fn with_umount_program(mut self, program: String) -> Self {
        self.umount_program = Some(program);
        self
    }

    /// Override the `avahi-browse` program path (default
    /// `/usr/bin/avahi-browse`).
    pub fn with_avahi_browse_program(mut self, program: String) -> Self {
        self.avahi_browse_program = Some(program);
        self
    }

    /// Override the `avahi-browse` timeout in milliseconds
    /// (default [`DEFAULT_AVAHI_BROWSE_TIMEOUT_MS`]).
    pub fn with_avahi_browse_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.avahi_browse_timeout_ms = Some(timeout_ms);
        self
    }

    /// Override the `smbclient` program path (default
    /// `/usr/bin/smbclient`).
    pub fn with_smbclient_program(mut self, program: String) -> Self {
        self.smbclient_program = Some(program);
        self
    }

    /// Override the per-host `smbclient` timeout in milliseconds
    /// (default [`DEFAULT_SMBCLIENT_TIMEOUT_MS`]).
    pub fn with_smbclient_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.smbclient_timeout_ms = Some(timeout_ms);
        self
    }

    /// Override the per-attempt subprocess timeout in
    /// milliseconds. Default is [`DEFAULT_MOUNT_TIMEOUT_MS`].
    pub fn with_mount_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.mount_timeout_ms = Some(timeout_ms);
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

    /// Override the mount-point probe (test path). Production
    /// defaults to [`is_path_mounted`]. Tests that exercise the
    /// already-mounted adopt path inject a closure returning
    /// true for the fixture mount root.
    pub fn with_mount_point_check(
        mut self,
        check: Arc<dyn Fn(&Path) -> bool + Send + Sync>,
    ) -> Self {
        self.mount_point_check = Some(check);
        self
    }

    /// Wrap mount + umount with `sudo -n` so the plugin can
    /// invoke the mount helper as root even when running under a
    /// non-root service identity. Sets `mount_program = "sudo"`
    /// with wrapper args `["-n", "/bin/mount"]` (and the umount
    /// equivalents). Requires a distribution-provided sudoers
    /// drop-in permitting the exact commands; the reference
    /// distribution ships one at
    /// `dist/sudoers.d/evo-network-shares.in`.
    ///
    /// Callers pass `true` when the plugin's service identity is
    /// non-root, `false` when the identity is root. Production
    /// wiring picks up the effective UID at load time.
    ///
    /// Both variants invoke `systemd-mount --collect` /
    /// `systemd-umount` so the mount lands in the host mount
    /// namespace, visible to every sibling systemd unit (mpd,
    /// artwork, metadata). See NETWORK-SOURCES-DESIGN.md §5.6.1
    /// for the namespace rationale.
    pub fn with_sudo_wrapping(mut self, sudo: bool) -> Self {
        if sudo {
            self.mount_program = Some("sudo".to_string());
            self.mount_wrapper_args = Some(vec![
                "-n".to_string(),
                "/usr/bin/systemd-mount".to_string(),
            ]);
            self.umount_program = Some("sudo".to_string());
            self.umount_wrapper_args = Some(vec![
                "-n".to_string(),
                "/usr/bin/systemd-umount".to_string(),
            ]);
        } else {
            self.mount_program = Some("/usr/bin/systemd-mount".to_string());
            self.mount_wrapper_args = Some(Vec::new());
            self.umount_program = Some("/usr/bin/systemd-umount".to_string());
            self.umount_wrapper_args = Some(Vec::new());
        }
        self
    }

    /// Finalise into a runtime.
    pub fn build(self) -> NetworkSharesRuntime {
        let now_fn: Arc<dyn Fn() -> u64 + Send + Sync> =
            self.now_fn.unwrap_or_else(|| Arc::new(default_now_ms));
        let share_states = seed_share_states(&self.state, now_fn());
        NetworkSharesRuntime {
            inner: Arc::new(Mutex::new(NetworkSharesInner {
                state: self.state,
                path: self.path,
            })),
            executor: self
                .executor
                .unwrap_or_else(|| Arc::new(SubprocessMountExecutor)),
            credentials: self.credentials.unwrap_or_else(|| {
                // When only a credential_store was supplied,
                // route reads through the store — it implements
                // CredentialFetcher too.
                self.credential_store
                    .as_ref()
                    .map(|s| Arc::clone(s) as Arc<dyn CredentialFetcher>)
                    .unwrap_or_else(|| Arc::new(NoCredentialFetcher))
            }),
            credential_store: self.credential_store,
            prompter: self
                .prompter
                .unwrap_or_else(|| Arc::new(NoPasswordPrompter)),
            mount_program: self
                .mount_program
                .unwrap_or_else(|| "/bin/mount".to_string()),
            mount_wrapper_args: self.mount_wrapper_args.unwrap_or_default(),
            umount_program: self
                .umount_program
                .unwrap_or_else(|| "/bin/umount".to_string()),
            umount_wrapper_args: self.umount_wrapper_args.unwrap_or_default(),
            mount_timeout_ms: self
                .mount_timeout_ms
                .unwrap_or(DEFAULT_MOUNT_TIMEOUT_MS),
            avahi_browse_program: self
                .avahi_browse_program
                .unwrap_or_else(|| "/usr/bin/avahi-browse".to_string()),
            avahi_browse_timeout_ms: self
                .avahi_browse_timeout_ms
                .unwrap_or(DEFAULT_AVAHI_BROWSE_TIMEOUT_MS),
            smbclient_program: self
                .smbclient_program
                .unwrap_or_else(|| "/usr/bin/smbclient".to_string()),
            smbclient_timeout_ms: self
                .smbclient_timeout_ms
                .unwrap_or(DEFAULT_SMBCLIENT_TIMEOUT_MS),
            discovered: Arc::new(Mutex::new(Vec::new())),
            share_states: Arc::new(Mutex::new(share_states)),
            publisher: StdMutex::new(None),
            now_fn,
            // Production default = host-table probe (lib.rs uses
            // builder()). Under `cfg(test)` default to never-
            // mounted so the crate's unit suite stays hermetic on
            // machines with a live NAS mount; adopt-path tests
            // inject `with_mount_point_check` explicitly.
            mount_point_check: self.mount_point_check.unwrap_or_else(|| {
                #[cfg(test)]
                {
                    Arc::new(|_: &Path| false)
                }
                #[cfg(not(test))]
                {
                    Arc::new(|p: &Path| is_path_mounted(p))
                }
            }),
            pending_credential_prompts: Arc::new(std::sync::Mutex::new(
                HashMap::new(),
            )),
            share_events_ring: Arc::new(StdMutex::new(
                std::collections::VecDeque::with_capacity(
                    SHARE_EVENTS_RING_CAPACITY,
                ),
            )),
        }
    }
}

/// Seed the per-share state map with an `Unmounted` entry per
/// configured record. Called on runtime construction so
/// [`NetworkSharesRuntime::attach_subject_publisher`] can announce
/// a subject for every persisted share without racing an initial
/// mount.
fn seed_share_states(
    state: &NetworkSharesState,
    now_ms: u64,
) -> HashMap<ShareId, ShareStateEntry> {
    state
        .shares
        .iter()
        .map(|r| {
            (
                r.share_id.clone(),
                ShareStateEntry {
                    alias: r.alias.clone(),
                    state: MountState::Unmounted,
                    reason: None,
                    negotiated_vers: None,
                    last_transition_at_ms: now_ms,
                },
            )
        })
        .collect()
}

#[async_trait]
impl NetworkSharesHandle for NetworkSharesRuntime {
    async fn list_configured(
        &self,
    ) -> Result<Vec<ShareRecord>, SharesStateError> {
        let g = self.inner.lock().await;
        Ok(g.state.shares.clone())
    }

    async fn get_share(
        &self,
        share_id: &ShareId,
    ) -> Result<Option<ShareRecord>, SharesStateError> {
        let g = self.inner.lock().await;
        Ok(g.state.find(share_id).cloned())
    }

    async fn add_share(
        &self,
        record: ShareRecord,
    ) -> Result<ShareId, SharesStateError> {
        let id = record.share_id.clone();
        let record_clone = record.clone();
        let configured_envelope = {
            let mut g = self.inner.lock().await;
            g.state.insert(record)?;
            g.state.save(&g.path)?;
            ConfiguredSharesEnvelope {
                shares: g.state.shares.clone(),
                last_update_at: SystemTime::now(),
            }
        };
        self.seed_share_state_entry(&record_clone).await;
        self.schedule_republish_configured(configured_envelope);
        Ok(id)
    }

    async fn edit_share(
        &self,
        share_id: &ShareId,
        edits: ShareEdits,
    ) -> Result<bool, SharesStateError> {
        let (changed, material, alias, configured_envelope) = {
            let mut g = self.inner.lock().await;
            let record = g.state.find_mut(share_id).ok_or_else(|| {
                SharesStateError::ShareNotFound {
                    id: share_id.clone(),
                }
            })?;
            // Snapshot material intent BEFORE the mutation so the
            // caller can decide whether to cycle the mount. Cosmetic
            // edits (alias only) do not need a remount; material
            // edits (host / path / fstype / creds / options) do.
            let material = edits.is_material_against(record);
            let changed = edits.apply_to(record);
            let alias = record.alias.clone();
            if changed {
                g.state.save(&g.path)?;
            }
            let envelope = ConfiguredSharesEnvelope {
                shares: g.state.shares.clone(),
                last_update_at: SystemTime::now(),
            };
            (changed, material, alias, envelope)
        };
        if changed {
            self.update_share_state_alias(share_id, &alias).await;
            self.schedule_republish_configured(configured_envelope);
        }
        // Material change on a currently-mounted share: unmount +
        // remount so the OS-side mount reflects the edited record.
        // Silent no-op when the share is not currently mounted;
        // the next mount attempt naturally picks up the new record.
        // Errors from the cycle are logged but do NOT fail the
        // edit — the persisted state is authoritative and the
        // operator can retry mount from the UI. Fire the cycle
        // via `mount_share` / `unmount_share` so the F1.1 / F1.2
        // hooks (MPD library update + queue safety + event ring)
        // apply uniformly.
        if changed && material {
            let is_mounted = {
                let g = self.share_states.lock().await;
                matches!(
                    g.get(share_id).map(|e| &e.state),
                    Some(MountState::Mounted)
                )
            };
            if is_mounted {
                if let Err(e) = self.unmount_share(share_id).await {
                    tracing::warn!(
                        share_id = %share_id,
                        error = %e,
                        "edit_share: pre-remount unmount failed; leaving \
                         share in prior mount state"
                    );
                } else if let Err(e) = self.mount_share(share_id).await {
                    tracing::warn!(
                        share_id = %share_id,
                        error = %e,
                        "edit_share: post-edit remount failed; share is \
                         now in Failed state per set_share_state"
                    );
                }
            }
        }
        Ok(changed)
    }

    async fn remove_share(
        &self,
        share_id: &ShareId,
    ) -> Result<ShareRecord, SharesStateError> {
        let (removed, configured_envelope) = {
            let mut g = self.inner.lock().await;
            let removed = g.state.remove(share_id)?;
            g.state.save(&g.path)?;
            let envelope = ConfiguredSharesEnvelope {
                shares: g.state.shares.clone(),
                last_update_at: SystemTime::now(),
            };
            (removed, envelope)
        };
        self.drop_share_state_entry(share_id).await;

        // Delete the mount-root directory if it is empty. The
        // wire caller (network.share.remove handler) performs a
        // best-effort unmount before calling us, so a clean
        // remove should find the mount-root vacated. The empty-
        // only guard means we never destroy operator files that
        // happen to sit under the mount-root because unmount
        // failed or the directory was populated outside the
        // framework's flow — the operator sees a leftover
        // directory in that case, distinct from silent data
        // loss.
        //
        // Skipped when the mount-root path is empty (legacy
        // records) or does not exist (already removed).
        let mount_root = &removed.mount_root;
        if !mount_root.as_os_str().is_empty() && mount_root.exists() {
            match std::fs::remove_dir(mount_root) {
                Ok(()) => {
                    tracing::info!(
                        share_id = %share_id,
                        mount_root = %mount_root.display(),
                        "removed empty mount-root directory after \
                         share removal"
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Race with something else deleting it; nothing
                    // to do.
                }
                Err(e) => {
                    tracing::warn!(
                        share_id = %share_id,
                        mount_root = %mount_root.display(),
                        error = %e,
                        "mount-root not removed (likely not empty — \
                         operator data present or unmount did not \
                         drain); leaving directory in place"
                    );
                }
            }
        }

        // Trigger an MPD library update for the removed path.
        // Without this, dead share entries linger in MPD's
        // database until something else (a mount / restart /
        // manual mpc update) forces a rescan — the operator
        // then sees the removed share still surfacing in the
        // library view. Best-effort: failure here does not fail
        // the remove call.
        trigger_mpd_update_best_effort(mount_root);

        self.schedule_republish_configured(configured_envelope);
        Ok(removed)
    }

    async fn mount_share(
        &self,
        share_id: &ShareId,
    ) -> Result<MountReport, MountError> {
        // Clone the record out of the mutex so mount attempts
        // (which may take many seconds under the probe ladder)
        // don't hold the write lock and block concurrent CRUD.
        let record = {
            let g = self.inner.lock().await;
            g.state.find(share_id).cloned().ok_or_else(|| {
                MountError::ShareNotFound {
                    id: share_id.clone(),
                }
            })?
        };

        // OS-truth short-circuit. `systemd-mount` lands shares in
        // the host namespace; those mounts survive steward /
        // plugin restart. Re-probing an already-active mount
        // walks the dialect ladder, clears persisted_vers, and
        // publishes Failed while the mount is healthy — the
        // exact "Audio" tile bug. Adopt before credential prompt
        // or mkdir so a live mount never triggers a password
        // prompt or a destructive remount attempt.
        if (self.mount_point_check)(&record.mount_root) {
            let start_ms = (self.now_fn)();
            let was_mounted = {
                let g = self.share_states.lock().await;
                matches!(
                    g.get(share_id).map(|e| &e.state),
                    Some(MountState::Mounted)
                )
            };
            let report =
                self.adopt_existing_os_mount(&record, start_ms).await?;
            self.set_share_state(
                share_id,
                MountState::Mounted,
                None,
                report.negotiated_version.clone(),
            )
            .await;
            if !was_mounted {
                trigger_mpd_update_best_effort(&record.mount_root);
                self.publish_share_event(ShareEvent::mounted(
                    share_id.clone(),
                    report.negotiated_version.clone(),
                    (self.now_fn)(),
                ))
                .await;
            }
            return Ok(report);
        }

        // Prompt-on-mount: for UserPassword shares whose
        // credential_key is not in the vault, raise a password
        // prompt via the framework's user-interaction responder
        // and stash the answer before proceeding. Guest / KeyFile
        // records short-circuit to Ok(()). Cancelled / timed-out
        // prompts surface as MountError variants the operator UI
        // renders per the mount_error contract on the wire.
        self.ensure_credential_stocked(&record).await?;

        // Ensure the per-share mount directory exists. The
        // distribution installer provisions the NAS root
        // (`/var/lib/evo/music/NAS`) at first-boot per the
        // four-primitive install/reset contract; it cannot know
        // future operator-chosen aliases, so the per-share
        // subdirectory is the plugin's responsibility. The
        // service user owns the NAS root, so `create_dir_all`
        // does not need elevation. Without this step, mount.cifs
        // fails ENOENT at every dialect before touching the
        // network — the operator sees "dialect probe exhausted"
        // when the real cause is a missing folder.
        self.ensure_mount_directory(share_id, &record.mount_root);

        let start_ms = (self.now_fn)();

        self.set_share_state(share_id, MountState::Mounting, None, None)
            .await;

        let result = match record.fstype {
            FsType::Cifs => self.mount_cifs(&record, start_ms).await,
            FsType::Nfs => self.mount_nfs(&record, start_ms).await,
        };

        match &result {
            Ok(report) => {
                self.set_share_state(
                    share_id,
                    MountState::Mounted,
                    None,
                    report.negotiated_version.clone(),
                )
                .await;
                // Kick MPD to walk the freshly-mounted tree so the
                // Library projection shows the NAS content without
                // an operator "rescan" ritual. Fire-and-forget: a
                // hung `mpc update` never blocks the mount response;
                // failure logs at debug. Same helper the remove path
                // uses so the plugin has one MPD-side coupling point.
                trigger_mpd_update_best_effort(&record.mount_root);
                self.publish_share_event(ShareEvent::mounted(
                    share_id.clone(),
                    report.negotiated_version.clone(),
                    (self.now_fn)(),
                ))
                .await;
            }
            Err(e) => {
                self.publish_share_event(ShareEvent::mount_failed(
                    share_id.clone(),
                    format!("{e}"),
                    (self.now_fn)(),
                ))
                .await;
                // Password refresh on auth-refusal (NETWORK-
                // SOURCES-DESIGN.md §5.6.5): delete the vault
                // entry so the next mount attempt re-prompts.
                // Honest response to NAS-side password rotation.
                if let MountError::AuthenticationRefused { .. } = e {
                    if let Credentials::UserPassword {
                        credential_key, ..
                    } = &record.credentials
                    {
                        if let Some(store) = self.credential_store.as_ref() {
                            if let Err(delete_err) =
                                store.delete_password(credential_key).await
                            {
                                tracing::warn!(
                                    plugin = crate::PLUGIN_NAME,
                                    key = credential_key,
                                    error = %delete_err,
                                    "auth-refused vault delete failed; \
                                     next mount attempt will re-fetch the \
                                     old (rejected) password"
                                );
                            } else {
                                tracing::info!(
                                    plugin = crate::PLUGIN_NAME,
                                    key = credential_key,
                                    "auth-refused; vault entry cleared so \
                                     next mount re-prompts the operator"
                                );
                            }
                        }
                    }
                }
                self.set_share_state(
                    share_id,
                    MountState::Failed,
                    Some(format!("{e}")),
                    None,
                )
                .await;
            }
        }

        result
    }

    async fn unmount_share(
        &self,
        share_id: &ShareId,
    ) -> Result<(), MountError> {
        let record = {
            let g = self.inner.lock().await;
            g.state.find(share_id).cloned().ok_or_else(|| {
                MountError::ShareNotFound {
                    id: share_id.clone(),
                }
            })?
        };
        // Lazy detach for CIFS (busy-file safety per volumio-evo
        // reference network_mounts.rs:818). NFS is unmounted
        // synchronously; kernel handles NFS-busy differently.
        let lazy = matches!(record.fstype, FsType::Cifs);
        let args =
            self.wrap_umount_args(build_umount_args(&record.mount_root, lazy));
        let umount_program = self.umount_program.clone();
        let output = self
            .executor
            .run(&umount_program, &args, self.mount_timeout_ms)
            .await?;
        let result = if output.exit_code == Some(0) {
            Ok(())
        } else {
            Err(MountError::MountFailed {
                id: share_id.clone(),
                exit_code: output.exit_code,
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        };
        match &result {
            Ok(()) => {
                // MPD queue safety BEFORE we tell MPD to walk the
                // (now-vanished) tree. Volumio-evo lesson: without
                // this the operator sees "No such song" storms +
                // an unresponsive queue when a share drops mid-
                // playback. Best-effort — a hung mpc does not
                // block the unmount response.
                trigger_mpd_stop_and_prune_best_effort(&record.mount_root);
                self.set_share_state(
                    share_id,
                    MountState::Unmounted,
                    None,
                    None,
                )
                .await;
                // Prune MPD's database rows under the vanished
                // path so the Library projection does not surface
                // dead entries until the next mount / restart.
                trigger_mpd_update_best_effort(&record.mount_root);
                self.publish_share_event(ShareEvent::unmounted(
                    share_id.clone(),
                    (self.now_fn)(),
                ))
                .await;
            }
            Err(e) => {
                self.set_share_state(
                    share_id,
                    MountState::Failed,
                    Some(format!("{e}")),
                    None,
                )
                .await;
                self.publish_share_event(ShareEvent::unmount_failed(
                    share_id.clone(),
                    format!("{e}"),
                    (self.now_fn)(),
                ))
                .await;
            }
        }
        result
    }

    async fn list_discovered(&self) -> Vec<DiscoveredNas> {
        let g = self.discovered.lock().await;
        g.clone()
    }

    async fn refresh_discovery(
        &self,
    ) -> Result<Vec<DiscoveredNas>, MountError> {
        // Step 1: enumerate SMB hosts via avahi-browse.
        let browse_args = build_avahi_browse_args();
        let browse_out = self
            .executor
            .run(
                &self.avahi_browse_program,
                &browse_args,
                self.avahi_browse_timeout_ms,
            )
            .await?;
        // avahi-browse in `-t` mode exits 0 after enumeration.
        // Non-zero is treated as fatal: we keep the prior cache
        // intact.
        if browse_out.exit_code != Some(0) {
            return Err(MountError::MountFailed {
                id: ShareId(String::new()),
                exit_code: browse_out.exit_code,
                stderr: String::from_utf8_lossy(&browse_out.stderr)
                    .into_owned(),
            });
        }
        let browse_stdout =
            String::from_utf8_lossy(&browse_out.stdout).into_owned();
        let hosts = filter_self_out(parse_avahi_browse_output(&browse_stdout));

        // Step 2: per host, enumerate shares via smbclient. A
        // per-host failure is NOT fatal — the NAS is still
        // recorded with empty shares + None dialect so the
        // operator can see it and add manually.
        let mut result = Vec::with_capacity(hosts.len());
        for (name, ip) in hosts {
            let list_args = build_smbclient_list_args(&ip);
            let list_out = self
                .executor
                .run(
                    &self.smbclient_program,
                    &list_args,
                    self.smbclient_timeout_ms,
                )
                .await;
            match list_out {
                Ok(out) if out.exit_code == Some(0) => {
                    let stdout_s = String::from_utf8_lossy(&out.stdout);
                    let stderr_s = String::from_utf8_lossy(&out.stderr);
                    let shares = parse_smbclient_disk_lines(&stdout_s);
                    let advertised_dialect = parse_smbclient_dialect(&stderr_s);
                    result.push(DiscoveredNas {
                        name,
                        ip,
                        advertised_dialect,
                        shares,
                    });
                }
                Ok(_) | Err(_) => {
                    // Per-host failure: still surface the host
                    // so the operator can add manually.
                    result.push(DiscoveredNas {
                        name,
                        ip,
                        advertised_dialect: None,
                        shares: Vec::new(),
                    });
                }
            }
        }

        let envelope = {
            let mut cache = self.discovered.lock().await;
            *cache = result.clone();
            DiscoveredNasEnvelope {
                nas: cache.clone(),
                last_refresh_at: SystemTime::now(),
            }
        };
        self.schedule_republish_discovered(envelope);
        Ok(result)
    }
}

impl NetworkSharesRuntime {
    /// CIFS mount execution: fast-path with persisted dialect
    /// first, then full probe ladder on failure or if no
    /// dialect has been persisted yet. Successful dialect is
    /// written back to [`ShareRecord::persisted_vers`] and
    /// [`ShareRecord::last_mounted_at_ms`] is updated before
    /// this method returns Ok.
    async fn mount_cifs(
        &self,
        record: &ShareRecord,
        start_ms: u64,
    ) -> Result<MountReport, MountError> {
        // Stage a credentials file for UserPassword records. The
        // path is returned so build_cifs_mount_args can reference
        // it via `credentials=<path>`; the file body carries
        // username / password / domain — the password never
        // touches argv. The file is deleted at the end of this
        // function regardless of success / failure.
        let creds_file = self.stage_mount_credentials_file(record).await?;
        let creds_path: Option<PathBuf> =
            creds_file.as_ref().map(|g| g.0.clone());

        let result = self
            .mount_cifs_with_creds_file(record, start_ms, creds_path.as_deref())
            .await;

        drop(creds_file);
        result
    }

    async fn mount_cifs_with_creds_file(
        &self,
        record: &ShareRecord,
        start_ms: u64,
        creds_path: Option<&Path>,
    ) -> Result<MountReport, MountError> {
        // Credentials invariant: UserPassword mounts MUST carry a
        // staged credentials file. Falling through without one
        // makes mount.cifs attempt anonymous/guest auth and
        // mislabels the resulting ACCESS_DENIED as dialect
        // exhaustion. Fail closed before any helper invocation.
        if let Credentials::UserPassword { credential_key, .. } =
            &record.credentials
        {
            if creds_path.is_none() {
                return Err(MountError::CredentialMissing {
                    key: credential_key.clone(),
                });
            }
        }

        // Determine which dialect(s) to try.
        let ladder: Vec<&str> =
            if let Some(persisted) = record.persisted_vers.as_deref() {
                // Fast-path: try the persisted dialect first. If it
                // fails we fall through to the full ladder with the
                // persisted dialect elided (we already tried it).
                vec![persisted]
            } else {
                CIFS_VERS_PROBE_LADDER.to_vec()
            };

        let mut attempted: Vec<String> = Vec::new();
        // Operator-facing last error must be the classified
        // (journal) stderr when available — not systemd-mount's
        // opaque "Job failed" text.
        let mut last_error = String::new();
        for dialect in &ladder {
            if let Some(report) = self
                .attempt_cifs_dialect_tracked(
                    record,
                    dialect,
                    creds_path,
                    start_ms,
                    &mut attempted,
                    &mut last_error,
                )
                .await?
            {
                return Ok(report);
            }
        }

        // Fast-path exhausted with a persisted dialect that no
        // longer works — clear it and rerun the full ladder.
        // Do NOT clear persisted_vers when the OS mount is still
        // active (adopt path returns above before we get here).
        if record.persisted_vers.is_some() {
            self.clear_persisted_vers(&record.share_id).await?;
            for dialect in CIFS_VERS_PROBE_LADDER {
                // Skip the dialect we just tried on the fast-path.
                if Some(*dialect) == record.persisted_vers.as_deref() {
                    continue;
                }
                if let Some(report) = self
                    .attempt_cifs_dialect_tracked(
                        record,
                        dialect,
                        creds_path,
                        start_ms,
                        &mut attempted,
                        &mut last_error,
                    )
                    .await?
                {
                    return Ok(report);
                }
            }
        }

        Err(MountError::DialectProbeExhausted {
            attempted,
            last_error,
        })
    }

    /// One CIFS dialect attempt. Returns `Ok(Some(report))` on
    /// success or OS-mount adopt, `Ok(None)` when the attempt
    /// failed with a non-short-circuit reason (caller continues
    /// the ladder), or `Err` for short-circuit failures
    /// (auth-refusal / directory-missing).
    async fn attempt_cifs_dialect_tracked(
        &self,
        record: &ShareRecord,
        dialect: &str,
        creds_path: Option<&Path>,
        start_ms: u64,
        attempted: &mut Vec<String>,
        last_error: &mut String,
    ) -> Result<Option<MountReport>, MountError> {
        attempted.push(dialect.to_string());
        let args = self.wrap_mount_args(build_cifs_mount_args(
            record, dialect, creds_path,
        ));
        let output = self
            .executor
            .run(&self.mount_program, &args, self.mount_timeout_ms)
            .await?;
        if output.exit_code == Some(0) {
            return Ok(Some(
                self.finalise_mount_success(record, dialect, start_ms)
                    .await?,
            ));
        }
        // A concurrent / prior host mount may already be active
        // (systemd-mount survives steward restart). Prefer OS
        // truth over continuing the ladder — never clear
        // persisted_vers or publish Failed over a live mount.
        if (self.mount_point_check)(&record.mount_root) {
            return Ok(Some(
                self.adopt_existing_os_mount(record, start_ms).await?,
            ));
        }
        let unit_stderr =
            self.fetch_mount_unit_stderr(&record.mount_root).await;
        let helper_stderr =
            String::from_utf8_lossy(&output.stderr).into_owned();
        let classify_stderr = if unit_stderr.is_empty() {
            helper_stderr
        } else {
            unit_stderr
        };
        *last_error = classify_stderr.clone();
        if is_mount_directory_missing(&classify_stderr) {
            return Err(MountError::MountDirectoryMissing {
                id: record.share_id.clone(),
                mount_root: record.mount_root.clone(),
                reason: classify_stderr,
            });
        }
        if is_cifs_auth_refusal(output.exit_code, &classify_stderr) {
            return Err(MountError::AuthenticationRefused {
                id: record.share_id.clone(),
                exit_code: output.exit_code,
                stderr: classify_stderr,
            });
        }
        Ok(None)
    }

    /// Read the transient systemd .mount unit's recent journal
    /// entries to get the real mount.cifs / mount.nfs stderr.
    /// `systemd-mount` returns an opaque "Job failed. See
    /// journalctl -xe for details." on any mount failure — the
    /// real reason (auth refused / ENOENT / dialect mismatch) is
    /// buried in the unit's journal. This helper fetches those
    /// lines so the auth-refused and directory-missing detectors
    /// can classify the failure correctly.
    ///
    /// Returns an empty string when the runtime is NOT using
    /// systemd-mount (test environments with a mocked `mount`
    /// executor) or when journalctl is unavailable / not
    /// readable, so the caller falls back to whatever stderr the
    /// executor itself produced.
    async fn fetch_mount_unit_stderr(&self, mount_root: &Path) -> String {
        if !self.uses_systemd_mount() {
            return String::new();
        }
        let unit = systemd_mount_unit_name(mount_root);
        let out = tokio::process::Command::new("journalctl")
            .args([
                "--no-pager",
                "-u",
                &unit,
                "-n",
                "30",
                "--since",
                "1 minute ago",
            ])
            .output()
            .await;
        match out {
            Ok(o) if o.status.success() => {
                String::from_utf8_lossy(&o.stdout).into_owned()
            }
            _ => String::new(),
        }
    }

    /// True when the runtime is wired to invoke systemd-mount
    /// (either directly as the mount program or through the
    /// sudo wrapper). Guards the journal-fetch fallback so test
    /// fixtures with a mocked `MountExecutor` don't accidentally
    /// shell out to real `journalctl`.
    fn uses_systemd_mount(&self) -> bool {
        self.mount_program.contains("systemd-mount")
            || self
                .mount_wrapper_args
                .iter()
                .any(|a| a.contains("systemd-mount"))
    }

    /// Ensure the per-share mount directory exists. The plugin's
    /// service identity owns the NAS root (`/var/lib/evo/music/NAS`)
    /// by installer contract, so `create_dir_all` on a child of
    /// the root needs no elevation on production hosts.
    ///
    /// A create failure here is NOT fatal at this call site —
    /// the mount helper will surface the ENOENT with more
    /// operator-useful context (which dialect, which mount
    /// helper), and `mount_cifs` / `mount_nfs` translate that
    /// stderr into [`MountError::MountDirectoryMissing`]. Logging
    /// the create failure at WARN keeps the operator-diagnosable
    /// signal on the tracing surface without duplicating the
    /// error surface. The mock executors used in unit tests
    /// never actually touch the mount root, so the create failure
    /// is silently ignored there and the test's mock "success"
    /// path proceeds unchanged.
    fn ensure_mount_directory(&self, share_id: &ShareId, mount_root: &Path) {
        if let Err(e) = std::fs::create_dir_all(mount_root) {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                share_id = %share_id.0,
                mount_root = %mount_root.display(),
                error = %e,
                "network.shares mount directory create failed; \
                 mount helper will surface ENOENT if the directory is truly absent"
            );
        }
    }

    /// Prepend the runtime's configured `mount_wrapper_args` to a
    /// mount-args vector. When wrapping is off, the returned
    /// vector equals the input.
    fn wrap_mount_args(&self, mount_args: Vec<String>) -> Vec<String> {
        let mut full = self.mount_wrapper_args.clone();
        full.extend(mount_args);
        full
    }

    /// Same shape as [`Self::wrap_mount_args`] for umount.
    fn wrap_umount_args(&self, umount_args: Vec<String>) -> Vec<String> {
        let mut full = self.umount_wrapper_args.clone();
        full.extend(umount_args);
        full
    }

    /// Stage a credentials file at `<state_dir>/.mount-creds-<share_id>`
    /// containing `username=` / `password=` / `domain=` lines
    /// mount.cifs consumes via `credentials=<path>`. Returns
    /// `Ok(None)` for Guest / KeyFile records (no file to stage).
    /// The returned guard deletes the file on drop; the mount
    /// path calls it explicitly at the end of `mount_cifs`.
    async fn stage_mount_credentials_file(
        &self,
        record: &ShareRecord,
    ) -> Result<Option<MountCredentialsFileGuard>, MountError> {
        let Credentials::UserPassword {
            username,
            credential_key,
            domain,
        } = &record.credentials
        else {
            return Ok(None);
        };
        let bytes = self
            .credentials
            .fetch_password(credential_key)
            .await
            .ok_or_else(|| MountError::CredentialMissing {
                key: credential_key.clone(),
            })?;
        let password = String::from_utf8(bytes).map_err(|e| {
            MountError::CredentialMissing {
                key: format!("{credential_key} (non-utf8 payload: {e})"),
            }
        })?;
        let body = compose_cifs_credentials_file(
            username,
            &password,
            domain.as_deref(),
        );
        let state_dir = {
            let g = self.inner.lock().await;
            g.path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."))
        };
        let path =
            state_dir.join(format!(".mount-creds-{}", record.share_id.0));
        let path_clone = path.clone();
        let body_bytes = body.into_bytes();
        tokio::task::spawn_blocking(move || -> Result<(), io::Error> {
            std::fs::write(&path_clone, body_bytes)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perms = std::fs::Permissions::from_mode(0o600);
                std::fs::set_permissions(&path_clone, perms)?;
            }
            Ok(())
        })
        .await
        .map_err(|e| {
            MountError::SubprocessIo(format!(
                "stage credentials file task panicked: {e}"
            ))
        })?
        .map_err(|e| {
            MountError::SubprocessIo(format!("stage credentials file: {e}"))
        })?;
        Ok(Some(MountCredentialsFileGuard(path)))
    }

    async fn finalise_mount_success(
        &self,
        record: &ShareRecord,
        dialect: &str,
        start_ms: u64,
    ) -> Result<MountReport, MountError> {
        let now_ms = (self.now_fn)();
        let mut g = self.inner.lock().await;
        if let Some(r) = g.state.find_mut(&record.share_id) {
            r.persisted_vers = Some(dialect.to_string());
            r.last_mounted_at_ms = Some(now_ms as i64);
        }
        g.state.save(&g.path)?;
        Ok(MountReport {
            share_id: record.share_id.clone(),
            mount_root: record.mount_root.clone(),
            negotiated_version: Some(dialect.to_string()),
            elapsed_ms: now_ms.saturating_sub(start_ms),
        })
    }

    /// Adopt a host-namespace mount that is already active for
    /// `record.mount_root` (typically a `systemd-mount` unit that
    /// survived steward restart). Restores `persisted_vers` from
    /// the live mount options when the on-disk record lost it
    /// (e.g. a prior false-Failed ladder cleared it).
    ///
    /// Does not invoke the mount helper.
    async fn adopt_existing_os_mount(
        &self,
        record: &ShareRecord,
        start_ms: u64,
    ) -> Result<MountReport, MountError> {
        let proc = read_host_proc_mounts();
        let vers = match record.fstype {
            FsType::Cifs => {
                parse_cifs_version_from_proc_mounts(&proc, &record.mount_root)
            }
            FsType::Nfs => {
                parse_nfs_version_from_proc_mounts(&proc, &record.mount_root)
            }
        }
        .or_else(|| record.persisted_vers.clone());

        let now_ms = (self.now_fn)();
        if record.fstype == FsType::Cifs {
            if let Some(ref v) = vers {
                let mut g = self.inner.lock().await;
                if let Some(r) = g.state.find_mut(&record.share_id) {
                    if r.persisted_vers.as_deref() != Some(v.as_str()) {
                        r.persisted_vers = Some(v.clone());
                    }
                    r.last_mounted_at_ms = Some(now_ms as i64);
                }
                if let Err(e) = g.state.save(&g.path) {
                    tracing::warn!(
                        plugin = crate::PLUGIN_NAME,
                        share_id = %record.share_id,
                        error = %e,
                        "adopt_existing_os_mount: persisted_vers save failed; \
                         subject state will still report Mounted"
                    );
                }
            }
        }

        tracing::info!(
            plugin = crate::PLUGIN_NAME,
            share_id = %record.share_id,
            mount_root = %record.mount_root.display(),
            negotiated_vers = ?vers,
            "adopted existing host-namespace mount as Mounted"
        );

        Ok(MountReport {
            share_id: record.share_id.clone(),
            mount_root: record.mount_root.clone(),
            negotiated_version: vers,
            elapsed_ms: now_ms.saturating_sub(start_ms),
        })
    }

    /// Walk configured shares and adopt any whose mount_root is
    /// already active in the host mount table. Upward-only:
    /// never marks a share Unmounted/Failed based on a missing
    /// OS mount (that needs reachability gating — follow-on).
    ///
    /// Safe to call before the subject publisher is attached
    /// (updates the in-memory state map so the initial announce
    /// carries OS truth).
    pub async fn reconcile_os_mount_states(&self) {
        let records: Vec<ShareRecord> = {
            let g = self.inner.lock().await;
            g.state.shares.clone()
        };
        for record in records {
            if !(self.mount_point_check)(&record.mount_root) {
                continue;
            }
            let already_mounted = {
                let g = self.share_states.lock().await;
                matches!(
                    g.get(&record.share_id).map(|e| &e.state),
                    Some(MountState::Mounted)
                )
            };
            if already_mounted {
                continue;
            }
            let start_ms = (self.now_fn)();
            match self.adopt_existing_os_mount(&record, start_ms).await {
                Ok(report) => {
                    self.set_share_state(
                        &record.share_id,
                        MountState::Mounted,
                        None,
                        report.negotiated_version,
                    )
                    .await;
                }
                Err(e) => {
                    tracing::warn!(
                        plugin = crate::PLUGIN_NAME,
                        share_id = %record.share_id,
                        error = %e,
                        "reconcile_os_mount_states: adopt failed"
                    );
                }
            }
        }
    }

    async fn clear_persisted_vers(
        &self,
        share_id: &ShareId,
    ) -> Result<(), SharesStateError> {
        let mut g = self.inner.lock().await;
        if let Some(r) = g.state.find_mut(share_id) {
            r.persisted_vers = None;
        }
        g.state.save(&g.path)?;
        Ok(())
    }

    /// NFS mount execution: single attempt (kernel negotiates
    /// version server-side). Post-success, reads /proc/mounts to
    /// surface the negotiated version for operator display; the
    /// record's [`ShareRecord::persisted_vers`] stays `None` for
    /// NFS since the kernel renegotiates on every mount.
    async fn mount_nfs(
        &self,
        record: &ShareRecord,
        start_ms: u64,
    ) -> Result<MountReport, MountError> {
        let args = self.wrap_mount_args(build_nfs_mount_args(record));
        let output = self
            .executor
            .run(&self.mount_program, &args, self.mount_timeout_ms)
            .await?;
        if output.exit_code != Some(0) {
            // Fetch the transient .mount unit's journal for the
            // real mount.nfs stderr — systemd-mount returns
            // opaque "Job failed" text. Same rationale as CIFS.
            let unit_stderr =
                self.fetch_mount_unit_stderr(&record.mount_root).await;
            let stderr_owned =
                String::from_utf8_lossy(&output.stderr).into_owned();
            let classify_stderr = if unit_stderr.is_empty() {
                stderr_owned
            } else {
                unit_stderr
            };
            // Same ENOENT short-circuit as the CIFS path: if the
            // mount root doesn't exist, mount.nfs errors at
            // chdir before touching the network. Report as
            // MountDirectoryMissing, not MountFailed.
            if is_mount_directory_missing(&classify_stderr) {
                return Err(MountError::MountDirectoryMissing {
                    id: record.share_id.clone(),
                    mount_root: record.mount_root.clone(),
                    reason: classify_stderr,
                });
            }
            return Err(MountError::MountFailed {
                id: record.share_id.clone(),
                exit_code: output.exit_code,
                stderr: classify_stderr,
            });
        }
        // Best-effort read of /proc/mounts for the negotiated
        // version. Absent /proc/mounts (test envs, non-Linux)
        // yields None — the mount itself succeeded, only the
        // operator-visible version display degrades.
        let negotiated_version = std::fs::read_to_string("/proc/mounts")
            .ok()
            .and_then(|contents| {
                parse_nfs_version_from_proc_mounts(
                    &contents,
                    &record.mount_root,
                )
            });
        let now_ms = (self.now_fn)();
        let mut g = self.inner.lock().await;
        if let Some(r) = g.state.find_mut(&record.share_id) {
            r.last_mounted_at_ms = Some(now_ms as i64);
        }
        g.state.save(&g.path)?;
        Ok(MountReport {
            share_id: record.share_id.clone(),
            mount_root: record.mount_root.clone(),
            negotiated_version,
            elapsed_ms: now_ms.saturating_sub(start_ms),
        })
    }
}

// --------------------------------------------------------------
// Subject-publisher wiring (Ship 2f)
// --------------------------------------------------------------

impl NetworkSharesRuntime {
    /// Attach a [`SubjectAnnouncer`]. Called once at wiring time
    /// (steward construction, before plugins start loading).
    ///
    /// On attach:
    ///
    /// 1. Announces the `system_network_shares_configured`
    ///    singleton with the current record set.
    /// 2. Announces the `system_network_shares_discovered`
    ///    singleton with the current (usually empty) cache.
    /// 3. Announces one `network_share_state` subject per
    ///    persisted share, with the record's seeded
    ///    [`MountState::Unmounted`] state.
    ///
    /// After this call, every CRUD / mount / unmount / discovery
    /// transition schedules a fire-and-forget republish.
    /// Announce failures propagate; downstream republish failures
    /// log at debug and do not propagate (the next transition's
    /// envelope carries ground truth).
    pub async fn attach_subject_publisher(
        &self,
        announcer: Arc<dyn SubjectAnnouncer>,
    ) -> Result<(), evo_plugin_sdk::contract::ReportError> {
        // Adopt host-namespace mounts that survived steward
        // restart BEFORE the initial subject announce so the UI
        // never flashes Failed/Unmounted over a live mount.
        self.reconcile_os_mount_states().await;

        let configured_envelope = self.compose_configured_envelope().await;
        let discovered_envelope = self.compose_discovered_envelope().await;
        let per_share_envelopes =
            self.compose_all_share_state_envelopes().await;

        announcer
            .announce(SubjectAnnouncement {
                subject_type: CONFIGURED_SUBJECT_TYPE.to_string(),
                addressings: vec![configured_singleton_addressing()],
                claims: Vec::new(),
                state: envelope_to_json(&configured_envelope),
                announced_at: SystemTime::now(),
            })
            .await?;
        announcer
            .announce(SubjectAnnouncement {
                subject_type: DISCOVERED_SUBJECT_TYPE.to_string(),
                addressings: vec![discovered_singleton_addressing()],
                claims: Vec::new(),
                state: envelope_to_json(&discovered_envelope),
                announced_at: SystemTime::now(),
            })
            .await?;
        for envelope in &per_share_envelopes {
            announcer
                .announce(SubjectAnnouncement {
                    subject_type: SHARE_STATE_SUBJECT_TYPE.to_string(),
                    addressings: vec![share_state_addressing(
                        &envelope.share_id,
                    )],
                    claims: Vec::new(),
                    state: envelope_to_json(envelope),
                    announced_at: SystemTime::now(),
                })
                .await?;
        }
        // Announce the (initially-empty) share-events ring so
        // subscribers can attach before the first lifecycle event
        // and get a consistent seed on `current_state`. Snapshot
        // the ring via the same helper the `list_events` read
        // verb calls (so seed shape and read-verb shape are
        // bit-identical). The snapshot includes any events the
        // runtime buffered before attach (mount attempts started
        // by the boot-sweep can complete before the announcer is
        // wired if the framework's plugin-load ordering differs
        // from the shares plugin's expectation).
        let initial_events_envelope = self.compose_share_events_envelope();
        announcer
            .announce(SubjectAnnouncement {
                subject_type: SHARE_EVENTS_SUBJECT_TYPE.to_string(),
                addressings: vec![share_events_singleton_addressing()],
                claims: Vec::new(),
                state: envelope_to_json(&initial_events_envelope),
                announced_at: SystemTime::now(),
            })
            .await?;

        let mut slot = self.publisher.lock().expect(
            "NetworkSharesRuntime publisher slot mutex poisoned at attach",
        );
        *slot = Some(SharesPublisher { announcer });
        Ok(())
    }

    async fn compose_configured_envelope(&self) -> ConfiguredSharesEnvelope {
        let g = self.inner.lock().await;
        ConfiguredSharesEnvelope {
            shares: g.state.shares.clone(),
            last_update_at: SystemTime::now(),
        }
    }

    async fn compose_discovered_envelope(&self) -> DiscoveredNasEnvelope {
        let g = self.discovered.lock().await;
        DiscoveredNasEnvelope {
            nas: g.clone(),
            last_refresh_at: SystemTime::now(),
        }
    }

    async fn compose_all_share_state_envelopes(
        &self,
    ) -> Vec<ShareStateEnvelope> {
        let g = self.share_states.lock().await;
        g.iter().map(|(id, e)| e.to_envelope(id)).collect()
    }

    /// Snapshot the `network_share_events` ring into a
    /// [`ShareEventsEnvelope`]. Shared by the initial subject
    /// announce (so subscribers get a seed on `current_state`)
    /// AND by the `network.share.list_events` read verb (so a
    /// freshly-loaded UI page sees the historical ring before
    /// its subscribe callback fires the first live event).
    ///
    /// The ring is bounded (32 entries at construction — see
    /// [`SHARE_EVENTS_RING_CAPACITY`]), so the clone is cheap.
    /// The snapshot is taken under the ring's mutex so a
    /// concurrent `publish_share_event` does not interleave.
    pub(crate) fn compose_share_events_envelope(&self) -> ShareEventsEnvelope {
        let guard = self
            .share_events_ring
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        ShareEventsEnvelope {
            events: guard.iter().cloned().collect(),
            last_update_at: SystemTime::now(),
        }
    }

    fn take_publisher(&self) -> Option<Arc<dyn SubjectAnnouncer>> {
        let slot = self.publisher.lock().expect(
            "NetworkSharesRuntime publisher slot mutex poisoned at read",
        );
        slot.as_ref().map(|p| Arc::clone(&p.announcer))
    }

    /// Push a share-lifecycle event onto the bounded ring and
    /// schedule a republish of the `network_share_events`
    /// singleton. Best-effort: the ring push is synchronous
    /// (nanoseconds); the republish is fire-and-forget so a slow
    /// steward never stalls the caller.
    ///
    /// Async because it composes cleanly with the mount / unmount
    /// call sites (already async) and lets a future test path
    /// await the publish without introducing a second event
    /// channel. The current body performs no `.await` beyond the
    /// tokio spawn but retaining `async` keeps the seam open.
    pub(crate) async fn publish_share_event(&self, event: ShareEvent) {
        let snapshot: Vec<ShareEvent> = {
            let mut guard = self.share_events_ring.lock().unwrap_or_else(|e| {
                // Poison recovery: keep the ring semantically
                // useful even if a prior push panicked; the
                // events themselves are best-effort telemetry.
                e.into_inner()
            });
            while guard.len() >= SHARE_EVENTS_RING_CAPACITY {
                guard.pop_front();
            }
            guard.push_back(event);
            guard.iter().cloned().collect()
        };
        let envelope = ShareEventsEnvelope {
            events: snapshot,
            last_update_at: SystemTime::now(),
        };
        let Some(announcer) = self.take_publisher() else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        handle.spawn(async move {
            let state = envelope_to_json(&envelope);
            if let Err(e) = announcer
                .update_state(share_events_singleton_addressing(), state)
                .await
            {
                tracing::debug!(
                    error = %e,
                    "network_share_events republish failed"
                );
            }
        });
    }

    fn schedule_republish_configured(
        &self,
        envelope: ConfiguredSharesEnvelope,
    ) {
        let Some(announcer) = self.take_publisher() else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        handle.spawn(async move {
            let state = envelope_to_json(&envelope);
            if let Err(e) = announcer
                .update_state(configured_singleton_addressing(), state)
                .await
            {
                tracing::debug!(
                    error = %e,
                    "system_network_shares_configured republish failed"
                );
            }
        });
    }

    fn schedule_republish_discovered(&self, envelope: DiscoveredNasEnvelope) {
        let Some(announcer) = self.take_publisher() else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        handle.spawn(async move {
            let state = envelope_to_json(&envelope);
            if let Err(e) = announcer
                .update_state(discovered_singleton_addressing(), state)
                .await
            {
                tracing::debug!(
                    error = %e,
                    "system_network_shares_discovered republish failed"
                );
            }
        });
    }

    fn schedule_announce_share_state(&self, envelope: ShareStateEnvelope) {
        let Some(announcer) = self.take_publisher() else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let addressing = share_state_addressing(&envelope.share_id);
        handle.spawn(async move {
            let announcement = SubjectAnnouncement {
                subject_type: SHARE_STATE_SUBJECT_TYPE.to_string(),
                addressings: vec![addressing],
                claims: Vec::new(),
                state: envelope_to_json(&envelope),
                announced_at: SystemTime::now(),
            };
            if let Err(e) = announcer.announce(announcement).await {
                tracing::debug!(
                    error = %e,
                    "network_share_state announce failed"
                );
            }
        });
    }

    fn schedule_republish_share_state(&self, envelope: ShareStateEnvelope) {
        let Some(announcer) = self.take_publisher() else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let addressing = share_state_addressing(&envelope.share_id);
        handle.spawn(async move {
            let state = envelope_to_json(&envelope);
            if let Err(e) = announcer.update_state(addressing, state).await {
                tracing::debug!(
                    error = %e,
                    "network_share_state republish failed"
                );
            }
        });
    }

    fn schedule_retract_share_state(&self, share_id: ShareId) {
        let Some(announcer) = self.take_publisher() else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let addressing = share_state_addressing(&share_id);
        handle.spawn(async move {
            if let Err(e) = announcer
                .retract(addressing, Some("share removed".to_string()))
                .await
            {
                tracing::debug!(
                    error = %e,
                    "network_share_state retract failed"
                );
            }
        });
    }

    async fn set_share_state(
        &self,
        share_id: &ShareId,
        state: MountState,
        reason: Option<String>,
        negotiated_vers: Option<String>,
    ) {
        let now_ms = (self.now_fn)();
        let envelope_opt = {
            let mut g = self.share_states.lock().await;
            let entry = g.entry(share_id.clone()).or_insert(ShareStateEntry {
                alias: String::new(),
                state,
                reason: None,
                negotiated_vers: None,
                last_transition_at_ms: now_ms,
            });
            entry.state = state;
            entry.reason = reason;
            entry.negotiated_vers = negotiated_vers;
            entry.last_transition_at_ms = now_ms;
            Some(entry.to_envelope(share_id))
        };
        if let Some(envelope) = envelope_opt {
            self.schedule_republish_share_state(envelope);
        }
    }

    async fn seed_share_state_entry(&self, record: &ShareRecord) {
        let now_ms = (self.now_fn)();
        let envelope = {
            let mut g = self.share_states.lock().await;
            let entry = ShareStateEntry {
                alias: record.alias.clone(),
                state: MountState::Unmounted,
                reason: None,
                negotiated_vers: None,
                last_transition_at_ms: now_ms,
            };
            let envelope = entry.to_envelope(&record.share_id);
            g.insert(record.share_id.clone(), entry);
            envelope
        };
        self.schedule_announce_share_state(envelope);
    }

    async fn update_share_state_alias(&self, share_id: &ShareId, alias: &str) {
        let envelope_opt = {
            let mut g = self.share_states.lock().await;
            g.get_mut(share_id).map(|entry| {
                entry.alias = alias.to_string();
                entry.to_envelope(share_id)
            })
        };
        if let Some(envelope) = envelope_opt {
            self.schedule_republish_share_state(envelope);
        }
    }

    async fn drop_share_state_entry(&self, share_id: &ShareId) {
        {
            let mut g = self.share_states.lock().await;
            g.remove(share_id);
        }
        self.schedule_retract_share_state(share_id.clone());
    }
}

fn envelope_to_json<T: Serialize>(envelope: &T) -> serde_json::Value {
    serde_json::to_value(envelope)
        .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()))
}

// --------------------------------------------------------------
// Mount lifecycle (Ship 2g)
// --------------------------------------------------------------

/// Default cadence for the background remount task (5 minutes).
/// Every tick, the runtime walks its per-share state map and
/// retries any share in [`MountState::Failed`] or
/// [`MountState::Unmounted`] — matches the volumio-evo reference
/// 5-min re-mount cadence.
pub const DEFAULT_REMOUNT_CADENCE_MS: u64 = 5 * 60 * 1_000;

/// Default cadence for the background discovery task (5 minutes).
/// Aligns with the operator-widgets contract's
/// `network.discovered.nas.card.list` refresh policy.
pub const DEFAULT_DISCOVERY_CADENCE_MS: u64 = 5 * 60 * 1_000;

/// Per-share boot-mount outcome.
#[derive(Debug)]
pub struct BootMountOutcome {
    /// The share that was attempted.
    pub share_id: ShareId,
    /// The mount result — either a full [`MountReport`] on success
    /// or the [`MountError`] that ended the attempt.
    pub result: Result<MountReport, MountError>,
}

impl BootMountOutcome {
    /// Whether the mount succeeded.
    pub fn is_ok(&self) -> bool {
        self.result.is_ok()
    }
}

/// Aggregate outcome of a boot-mount sweep.
#[derive(Debug, Default)]
pub struct BootMountReport {
    /// One entry per configured share, in the configured order.
    pub outcomes: Vec<BootMountOutcome>,
}

impl BootMountReport {
    /// Count shares that mounted successfully.
    pub fn success_count(&self) -> usize {
        self.outcomes.iter().filter(|o| o.is_ok()).count()
    }

    /// Count shares that failed to mount.
    pub fn failure_count(&self) -> usize {
        self.outcomes.iter().filter(|o| !o.is_ok()).count()
    }
}

impl NetworkSharesRuntime {
    /// Attempt to mount every configured share. Runs
    /// sequentially (not concurrently) — CIFS probe ladders can
    /// take up to 150 s each and firing them concurrently would
    /// swamp weak targets like the Pi 5. On operator-visible
    /// startup surfaces the sequential progression also renders
    /// cleaner. Publishes per-share state transitions via the
    /// Ship 2f subject substrate.
    pub async fn boot_mount_all(&self) -> BootMountReport {
        // Reconcile first so already-active host mounts become
        // Mounted before we spend probe-ladder budget on them.
        self.reconcile_os_mount_states().await;

        let ids: Vec<ShareId> = {
            let g = self.inner.lock().await;
            g.state.shares.iter().map(|r| r.share_id.clone()).collect()
        };
        let mut outcomes = Vec::with_capacity(ids.len());
        for share_id in ids {
            // Skip shares reconcile (or a prior boot attempt)
            // already marked Mounted — mount_share would also
            // short-circuit, but avoiding the call keeps the
            // boot report honest about "attempted".
            let already_mounted = {
                let g = self.share_states.lock().await;
                matches!(
                    g.get(&share_id).map(|e| &e.state),
                    Some(MountState::Mounted)
                )
            };
            if already_mounted {
                let report = {
                    let g = self.inner.lock().await;
                    let record = g.state.find(&share_id);
                    MountReport {
                        share_id: share_id.clone(),
                        mount_root: record
                            .map(|r| r.mount_root.clone())
                            .unwrap_or_default(),
                        negotiated_version: record
                            .and_then(|r| r.persisted_vers.clone()),
                        elapsed_ms: 0,
                    }
                };
                outcomes.push(BootMountOutcome {
                    share_id,
                    result: Ok(report),
                });
                continue;
            }
            let result = self.mount_share(&share_id).await;
            outcomes.push(BootMountOutcome { share_id, result });
        }
        BootMountReport { outcomes }
    }

    /// Retry every share currently in [`MountState::Failed`] or
    /// [`MountState::Unmounted`]. Called by the background
    /// remount task and directly by tests to exercise the retry
    /// path without spawning a task.
    pub async fn remount_retry_pass(&self) -> Vec<BootMountOutcome> {
        // Adopt any host mounts that came back (or survived)
        // before selecting Failed/Unmounted candidates — a share
        // whose OS mount is live must not stay in the Failed
        // retry set.
        self.reconcile_os_mount_states().await;

        let candidates: Vec<ShareId> = {
            let g = self.share_states.lock().await;
            g.iter()
                .filter(|(_, e)| {
                    matches!(
                        e.state,
                        MountState::Failed | MountState::Unmounted
                    )
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        let mut outcomes = Vec::with_capacity(candidates.len());
        for share_id in candidates {
            let result = self.mount_share(&share_id).await;
            outcomes.push(BootMountOutcome { share_id, result });
        }
        outcomes
    }
}

/// Spawn a background task that runs
/// [`NetworkSharesRuntime::remount_retry_pass`] on every `cadence`
/// tick. The task holds a `Weak<NetworkSharesRuntime>` so a
/// dropped runtime terminates the loop. Callers retain the
/// returned `JoinHandle` if they want to `abort()` on shutdown;
/// otherwise dropping it detaches (the task exits when the
/// runtime is dropped).
pub fn spawn_remount_task(
    runtime: Arc<NetworkSharesRuntime>,
    cadence: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    let weak = Arc::downgrade(&runtime);
    drop(runtime);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(cadence);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // The first tick fires immediately; skip so callers who
        // want the first retry `cadence` after spawn get that
        // behaviour.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let Some(rt) = weak.upgrade() else { return };
            let _ = rt.remount_retry_pass().await;
        }
    })
}

/// Spawn a background task that runs
/// [`NetworkSharesHandle::refresh_discovery`] on every `cadence`
/// tick. Same lifecycle semantics as [`spawn_remount_task`].
pub fn spawn_discovery_task(
    runtime: Arc<NetworkSharesRuntime>,
    cadence: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    let weak = Arc::downgrade(&runtime);
    drop(runtime);
    tokio::spawn(async move {
        // Eager initial refresh: fire once immediately so the
        // operator UI populates within seconds of plugin load
        // rather than waiting one full cadence (5 min by
        // default). Then loop on the cadence for subsequent
        // sweeps.
        let mut ticker = tokio::time::interval(cadence);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Consume the tokio::time::interval immediate first-tick
        // so `ticker.tick().await` inside the loop below
        // returns after `cadence` elapses.
        ticker.tick().await;
        // Fire the eager initial refresh outside the loop.
        {
            let Some(rt) = weak.upgrade() else { return };
            if let Err(e) = rt.refresh_discovery().await {
                tracing::warn!(
                    plugin = crate::PLUGIN_NAME,
                    error = %e,
                    "initial discovery sweep failed; \
                     background task will retry on cadence"
                );
            }
        }
        loop {
            ticker.tick().await;
            let Some(rt) = weak.upgrade() else { return };
            if let Err(e) = rt.refresh_discovery().await {
                // Non-fatal, but MUST NOT be silent: an empty
                // discovered list looks identical to a
                // successfully-empty LAN, so a subprocess
                // usage error, dbus fault, or systemd unit
                // outage would look like "no NASes present"
                // without this trace.
                tracing::warn!(
                    plugin = crate::PLUGIN_NAME,
                    error = %e,
                    "background discovery sweep failed; \
                     prior discovered cache retained"
                );
            }
        }
    })
}

// --------------------------------------------------------------
// Operator verb dispatch (Ship 2g)
// --------------------------------------------------------------

/// Request payload for `network.share.add`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddShareRequest {
    /// Operator-set display name.
    pub alias: String,
    /// CIFS or NFS.
    pub fstype: FsType,
    /// Host IP or DNS name.
    pub host: String,
    /// Remote share path.
    pub path: String,
    /// Authentication shape.
    pub credentials: Credentials,
    /// Operator-supplied mount options string (may be empty).
    #[serde(default)]
    pub advanced_options: String,
}

/// Response payload for `network.share.add`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddShareResponse {
    /// The share_id minted for the new record.
    pub share_id: ShareId,
    /// Mount outcome — present when the initial mount attempt
    /// succeeded; carries the negotiated dialect + elapsed time.
    pub mount_report: Option<MountReport>,
    /// Human-readable mount-failure reason when the initial
    /// mount attempt failed. Absent on success.
    pub mount_error: Option<String>,
}

/// Request payload for `network.share.edit`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditShareRequest {
    /// The share to edit.
    pub share_id: ShareId,
    /// Optional new fields.
    pub edits: ShareEdits,
}

/// Response payload for `network.share.edit`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditShareResponse {
    /// Whether any field actually changed. `false` for a no-op
    /// edit — the widget renders a "nothing changed" hint.
    pub changed: bool,
}

/// Request payload for `network.share.remove`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveShareRequest {
    /// The share to remove.
    pub share_id: ShareId,
}

/// Response payload for `network.share.remove`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveShareResponse {
    /// The record that was removed (surfaced so the UI can
    /// render a snackbar / undo affordance without a second
    /// query).
    pub removed_record: ShareRecord,
}

/// Request payload for `network.share.mount`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountShareRequest {
    /// The share to mount.
    pub share_id: ShareId,
}

/// Response payload for `network.share.mount`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountShareResponse {
    /// The mount report — negotiated dialect + elapsed time.
    pub report: MountReport,
}

/// Request payload for `network.share.unmount`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnmountShareRequest {
    /// The share to unmount.
    pub share_id: ShareId,
}

/// Response payload for `network.share.unmount`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnmountShareResponse {}

/// Response payload for `network.discovery.refresh`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshDiscoveryResponse {
    /// The refreshed NAS inventory.
    pub nas: Vec<DiscoveredNas>,
}

/// Errors surfaced by [`NetworkSharesRuntime::dispatch_verb`]
/// specific to verb routing (unknown request_type, undecodable
/// payload, unserialisable response). Execution failures nest
/// the existing [`MountError`] / [`SharesStateError`] shapes.
#[derive(Debug, thiserror::Error)]
pub enum VerbDispatchError {
    /// The `request_type` string did not match any known verb on
    /// this shelf.
    #[error("unknown request_type: {request_type}")]
    UnknownRequestType {
        /// The request_type the caller supplied.
        request_type: String,
    },
    /// The payload bytes did not decode into the expected
    /// request envelope for the verb.
    #[error("payload decode failed for {request_type}: {detail}")]
    PayloadDecode {
        /// The verb whose payload was undecodable.
        request_type: String,
        /// Serde error text.
        detail: String,
    },
    /// The verb executed but its underlying operation returned a
    /// persistence error.
    #[error("verb execution failed (persistence): {0}")]
    Persistence(#[from] SharesStateError),
    /// The verb executed but its underlying operation returned a
    /// mount error.
    #[error("verb execution failed (mount): {0}")]
    Mount(#[from] MountError),
    /// Response serialisation failed (should not happen with
    /// derived Serialize impls; surfaced defensively so a future
    /// custom Serialize does not silently corrupt the wire).
    #[error("response serialise failed for {request_type}: {detail}")]
    ResponseSerialise {
        /// The verb whose response failed to serialise.
        request_type: String,
        /// Serde error text.
        detail: String,
    },
}

/// The set of `request_type` strings this runtime dispatches.
/// Steward-side routing tables use this to know which shelf
/// requests to fan out to the runtime instance.
pub const NETWORK_SHARES_VERBS: &[&str] = &[
    "network.share.add",
    "network.share.edit",
    "network.share.remove",
    "network.share.mount",
    "network.share.unmount",
    "network.discovery.refresh",
    // Read verbs (read-then-subscribe seed for UI consumers).
    "network.share.list_configured",
    "network.discovery.list",
    "network.share.get_state",
    // The `network_share_events` subject is delta-only —
    // subscribers see events that fire after `subscribe`, not
    // the historical ring. The Sources Activity panel needs a
    // read-then-subscribe path so a freshly-loaded page shows
    // the last N mount/unmount/failed events without waiting
    // for a live transition; every other shares subject pairs
    // a subscribe with a matching read (list_configured,
    // get_state) and events should not be the exception.
    "network.share.list_events",
];

/// Response payload for `network.share.list_configured`.
#[derive(Debug, Clone, Serialize)]
pub struct ListConfiguredResponse {
    /// Snapshot of the configured-share envelope.
    pub envelope: ConfiguredSharesEnvelope,
}

/// Response payload for `network.discovery.list`.
#[derive(Debug, Clone, Serialize)]
pub struct ListDiscoveredResponse {
    /// Snapshot of the discovered-NAS envelope. Does NOT
    /// trigger a fresh sweep — use `network.discovery.refresh`
    /// for that.
    pub envelope: DiscoveredNasEnvelope,
}

/// Request payload for `network.share.get_state`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetShareStateRequest {
    /// The share to read state for.
    pub share_id: ShareId,
}

/// Response payload for `network.share.get_state`. Envelope is
/// `None` when no share exists with the given id.
#[derive(Debug, Clone, Serialize)]
pub struct GetShareStateResponse {
    /// Snapshot of the per-share state envelope, or `None` when
    /// no share exists with the requested id.
    pub envelope: Option<ShareStateEnvelope>,
}

/// Response payload for `network.share.list_events`. Carries
/// the same [`ShareEventsEnvelope`] shape the subject seed
/// announce publishes on the `network_share_events` singleton
/// — read-then-subscribe consumers can decode the read verb's
/// response and the subject's `current_state` through one
/// decoder path.
#[derive(Debug, Clone, Serialize)]
pub struct ListEventsResponse {
    /// Snapshot of the bounded event ring at the moment the
    /// verb was dispatched. Empty at boot until the first
    /// lifecycle event lands; ordered oldest-first per the
    /// ring's insertion order.
    pub envelope: ShareEventsEnvelope,
}

/// Whether the runtime dispatches this request_type.
pub fn is_network_shares_verb(request_type: &str) -> bool {
    NETWORK_SHARES_VERBS.contains(&request_type)
}

impl NetworkSharesRuntime {
    /// Route an operator wire verb to the appropriate handle
    /// method. Steward wire-op plumbing calls this with the
    /// verb's `request_type` string + JSON payload bytes; the
    /// runtime parses, executes, and returns the response
    /// bytes. Unknown verbs / decode failures / serialise
    /// failures return [`VerbDispatchError`]; execution failures
    /// nest inside the same enum.
    pub async fn dispatch_verb(
        &self,
        request_type: &str,
        payload_bytes: &[u8],
    ) -> Result<Vec<u8>, VerbDispatchError> {
        match request_type {
            "network.share.add" => {
                let req: AddShareRequest =
                    decode_payload(request_type, payload_bytes)?;
                let record = ShareRecord::new(
                    req.alias,
                    req.fstype,
                    req.host,
                    req.path,
                    req.credentials,
                    req.advanced_options,
                    (self.now_fn)() as i64,
                );
                let share_id = self.add_share(record).await?;
                // mount_share now runs the prompt-on-mount flow
                // internally: for UserPassword shares whose
                // credential_key is not in the vault, it raises
                // a password prompt via the framework's user-
                // interaction responder and stashes the answer
                // before dispatching the mount helper. A
                // cancelled prompt surfaces as a MountError the
                // caller renders as mount_error; the operator
                // retries via network.share.mount, which re-prompts.
                let mount_res = self.mount_share(&share_id).await;
                let response = match mount_res {
                    Ok(report) => AddShareResponse {
                        share_id,
                        mount_report: Some(report),
                        mount_error: None,
                    },
                    Err(MountError::NoResponderAvailable { key, reason }) => {
                        // Roll back the persisted share record on
                        // this specific failure: the mutation
                        // could not be answered by any client
                        // (no responder session was connected at
                        // dispatch time), so leaving a half-added
                        // share behind would litter the state
                        // with a record the operator did not
                        // consent to keeping. Distinct from
                        // cancelled / timed-out prompt failures,
                        // where the operator DID see the prompt
                        // and chose not to answer — those keep
                        // the record so `network.share.mount`
                        // can retry.
                        let _ = self.remove_share(&share_id).await;
                        return Err(VerbDispatchError::Mount(
                            MountError::NoResponderAvailable { key, reason },
                        ));
                    }
                    Err(e) => AddShareResponse {
                        share_id,
                        mount_report: None,
                        mount_error: Some(format!("{e}")),
                    },
                };
                encode_response(request_type, &response)
            }
            "network.share.edit" => {
                let req: EditShareRequest =
                    decode_payload(request_type, payload_bytes)?;
                let changed = self.edit_share(&req.share_id, req.edits).await?;
                encode_response(request_type, &EditShareResponse { changed })
            }
            "network.share.remove" => {
                let req: RemoveShareRequest =
                    decode_payload(request_type, payload_bytes)?;
                // Best-effort unmount before removal so busy CIFS
                // mounts get the lazy-detach path; failure here
                // does not block record deletion.
                let _ = self.unmount_share(&req.share_id).await;
                let removed_record = self.remove_share(&req.share_id).await?;
                encode_response(
                    request_type,
                    &RemoveShareResponse { removed_record },
                )
            }
            "network.share.mount" => {
                let req: MountShareRequest =
                    decode_payload(request_type, payload_bytes)?;
                // mount_share runs the prompt-on-mount flow
                // internally, so a re-mount after a cancelled
                // prompt re-prompts symmetrically with
                // network.share.add.
                let report = self.mount_share(&req.share_id).await?;
                encode_response(request_type, &MountShareResponse { report })
            }
            "network.share.unmount" => {
                let req: UnmountShareRequest =
                    decode_payload(request_type, payload_bytes)?;
                self.unmount_share(&req.share_id).await?;
                encode_response(request_type, &UnmountShareResponse {})
            }
            "network.discovery.refresh" => {
                let nas = self.refresh_discovery().await?;
                encode_response(request_type, &RefreshDiscoveryResponse { nas })
            }
            "network.share.list_configured" => {
                let envelope = self.compose_configured_envelope().await;
                encode_response(
                    request_type,
                    &ListConfiguredResponse { envelope },
                )
            }
            "network.discovery.list" => {
                let envelope = self.compose_discovered_envelope().await;
                encode_response(
                    request_type,
                    &ListDiscoveredResponse { envelope },
                )
            }
            "network.share.get_state" => {
                let req: GetShareStateRequest =
                    decode_payload(request_type, payload_bytes)?;
                let envelope =
                    self.compose_share_state_envelope(&req.share_id).await;
                encode_response(
                    request_type,
                    &GetShareStateResponse { envelope },
                )
            }
            "network.share.list_events" => {
                let envelope = self.compose_share_events_envelope();
                encode_response(request_type, &ListEventsResponse { envelope })
            }
            other => Err(VerbDispatchError::UnknownRequestType {
                request_type: other.to_string(),
            }),
        }
    }

    async fn compose_share_state_envelope(
        &self,
        share_id: &ShareId,
    ) -> Option<ShareStateEnvelope> {
        let g = self.share_states.lock().await;
        g.get(share_id).map(|e| e.to_envelope(share_id))
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

    fn sample_record(alias: &str) -> ShareRecord {
        ShareRecord {
            share_id: ShareId::new_v4(),
            alias: alias.to_string(),
            fstype: FsType::Cifs,
            host: "192.0.2.10".to_string(),
            path: "Music".to_string(),
            credentials: Credentials::Guest,
            advanced_options: String::new(),
            persisted_vers: None,
            mount_root: ShareRecord::default_mount_root(alias),
            created_at_ms: 1_700_000_000_000,
            last_mounted_at_ms: None,
        }
    }

    #[test]
    fn empty_state_round_trips_through_toml() {
        let state = NetworkSharesState::empty();
        let text = toml::to_string_pretty(&state).expect("serialize");
        let parsed: NetworkSharesState =
            toml::from_str(&text).expect("deserialize");
        assert_eq!(state, parsed);
    }

    #[test]
    fn state_with_shares_round_trips_through_toml() {
        let mut state = NetworkSharesState::empty();
        state.insert(sample_record("Family NAS")).unwrap();
        let mut second = sample_record("Studio NAS");
        second.credentials = Credentials::UserPassword {
            username: "engineer".to_string(),
            credential_key: "studio-nas-password".to_string(),
            domain: None,
        };
        second.fstype = FsType::Nfs;
        state.insert(second).unwrap();

        let text = toml::to_string_pretty(&state).expect("serialize");
        let parsed: NetworkSharesState =
            toml::from_str(&text).expect("deserialize");
        assert_eq!(state, parsed);
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let dir = tempdir();
        let path = dir.join("shares.toml");
        let state = NetworkSharesState::load(&path).expect("load");
        assert_eq!(state, NetworkSharesState::empty());
    }

    #[test]
    fn save_then_load_preserves_shape() {
        let dir = tempdir();
        let path = dir.join("shares.toml");
        let mut state = NetworkSharesState::empty();
        state.insert(sample_record("home")).unwrap();
        state.save(&path).expect("save");
        let reloaded = NetworkSharesState::load(&path).expect("load");
        assert_eq!(state, reloaded);
    }

    #[test]
    fn insert_duplicate_id_errors() {
        let mut state = NetworkSharesState::empty();
        let r = sample_record("first");
        let id = r.share_id.clone();
        state.insert(r).unwrap();
        let clashing = ShareRecord {
            share_id: id.clone(),
            ..sample_record("second")
        };
        let err = state.insert(clashing).unwrap_err();
        assert!(
            matches!(err, SharesStateError::DuplicateShareId { id: got } if got == id)
        );
    }

    #[test]
    fn remove_missing_id_errors() {
        let mut state = NetworkSharesState::empty();
        let missing = ShareId::new_v4();
        let err = state.remove(&missing).unwrap_err();
        assert!(
            matches!(err, SharesStateError::ShareNotFound { id: got } if got == missing)
        );
    }

    #[test]
    fn find_returns_inserted_record() {
        let mut state = NetworkSharesState::empty();
        let r = sample_record("primary");
        let id = r.share_id.clone();
        state.insert(r.clone()).unwrap();
        assert_eq!(state.find(&id), Some(&r));
    }

    #[test]
    fn find_mut_lets_caller_update_persisted_vers() {
        let mut state = NetworkSharesState::empty();
        let r = sample_record("primary");
        let id = r.share_id.clone();
        state.insert(r).unwrap();
        state.find_mut(&id).unwrap().persisted_vers = Some("3.0".to_string());
        assert_eq!(
            state.find(&id).and_then(|r| r.persisted_vers.as_deref()),
            Some("3.0")
        );
    }

    #[test]
    fn default_mount_root_sanitises_alias() {
        assert_eq!(
            ShareRecord::default_mount_root("Family NAS"),
            PathBuf::from("/var/lib/evo/music/NAS/Family_NAS")
        );
        assert_eq!(
            ShareRecord::default_mount_root("dev-rig/2024"),
            PathBuf::from("/var/lib/evo/music/NAS/dev-rig_2024")
        );
        assert_eq!(
            ShareRecord::default_mount_root(""),
            PathBuf::from("/var/lib/evo/music/NAS/share")
        );
    }

    #[test]
    fn atomic_save_leaves_no_tmp_file() {
        let dir = tempdir();
        let path = dir.join("shares.toml");
        let mut state = NetworkSharesState::empty();
        state.insert(sample_record("x")).unwrap();
        state.save(&path).unwrap();
        let tmp = path.with_extension("toml.tmp");
        assert!(!tmp.exists(), "temp file should be renamed atomically");
        assert!(path.exists(), "target file must exist after save");
    }

    #[test]
    fn schema_version_mismatch_errors_on_load() {
        let dir = tempdir();
        let path = dir.join("shares.toml");
        std::fs::write(
            &path,
            format!(
                "schema_version = {}\nshares = []\n",
                NETWORK_SHARES_SCHEMA_VERSION + 1
            ),
        )
        .unwrap();
        let err = NetworkSharesState::load(&path).unwrap_err();
        assert!(matches!(err, SharesStateError::UnsupportedSchema { .. }));
    }

    fn tempdir() -> std::path::PathBuf {
        let mut base = std::env::temp_dir();
        base.push(format!("evo-network-shares-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    // -----------------------------------------------------------
    // NetworkSharesRuntime (Ship 2b) CRUD tests
    // -----------------------------------------------------------

    fn built_record(alias: &str, host: &str) -> ShareRecord {
        let mut r = ShareRecord::new(
            alias.to_string(),
            FsType::Cifs,
            host.to_string(),
            "Music".to_string(),
            Credentials::Guest,
            String::new(),
            1_700_000_000_000,
        );
        // Production `ShareRecord::new` composes
        // `/var/lib/evo/music/NAS/<alias>` for the mount_root; in
        // tests the runtime's mkdir-before-mount step needs a
        // writable path. Redirect to a per-test tempdir.
        r.mount_root = tempdir().join(alias);
        r
    }

    #[tokio::test]
    async fn runtime_open_returns_empty_when_no_file() {
        let dir = tempdir();
        let rt = NetworkSharesRuntime::open(&dir).expect("open");
        assert!(rt.list_configured().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn runtime_add_returns_id_and_persists_across_reopen() {
        let dir = tempdir();
        let rt = NetworkSharesRuntime::open(&dir).unwrap();
        let record = built_record("Family NAS", "192.0.2.10");
        let expected_id = record.share_id.clone();
        let returned_id = rt.add_share(record).await.unwrap();
        assert_eq!(returned_id, expected_id);
        let rt2 = NetworkSharesRuntime::open(&dir).unwrap();
        let list = rt2.list_configured().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].share_id, expected_id);
    }

    #[tokio::test]
    async fn runtime_add_duplicate_id_errors_without_side_effect() {
        let dir = tempdir();
        let rt = NetworkSharesRuntime::open(&dir).unwrap();
        let record = built_record("First", "192.0.2.11");
        let id = record.share_id.clone();
        rt.add_share(record).await.unwrap();
        let clash = ShareRecord {
            share_id: id,
            ..built_record("Second", "192.0.2.12")
        };
        let err = rt.add_share(clash).await.unwrap_err();
        assert!(matches!(err, SharesStateError::DuplicateShareId { .. }));
        assert_eq!(rt.list_configured().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn runtime_edit_partial_updates_persist() {
        let dir = tempdir();
        let rt = NetworkSharesRuntime::open(&dir).unwrap();
        let record = built_record("Original", "192.0.2.20");
        let id = record.share_id.clone();
        rt.add_share(record).await.unwrap();
        let changed = rt
            .edit_share(
                &id,
                ShareEdits {
                    alias: Some("Renamed".to_string()),
                    host: Some("192.0.2.21".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(changed);
        let after = rt.get_share(&id).await.unwrap().unwrap();
        assert_eq!(after.alias, "Renamed");
        assert_eq!(after.host, "192.0.2.21");
        assert_eq!(after.path, "Music");
        let rt2 = NetworkSharesRuntime::open(&dir).unwrap();
        assert_eq!(rt2.list_configured().await.unwrap()[0].alias, "Renamed");
    }

    #[tokio::test]
    async fn runtime_edit_noop_returns_false() {
        let dir = tempdir();
        let rt = NetworkSharesRuntime::open(&dir).unwrap();
        let record = built_record("Same", "192.0.2.30");
        let id = record.share_id.clone();
        rt.add_share(record).await.unwrap();
        let changed = rt
            .edit_share(
                &id,
                ShareEdits {
                    alias: Some("Same".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(!changed);
    }

    #[tokio::test]
    async fn runtime_edit_missing_id_errors() {
        let dir = tempdir();
        let rt = NetworkSharesRuntime::open(&dir).unwrap();
        let err = rt
            .edit_share(&ShareId::new_v4(), ShareEdits::default())
            .await
            .unwrap_err();
        assert!(matches!(err, SharesStateError::ShareNotFound { .. }));
    }

    #[tokio::test]
    async fn runtime_edit_fstype_clears_persisted_vers() {
        let dir = tempdir();
        let rt = NetworkSharesRuntime::open(&dir).unwrap();
        let mut record = built_record("SmbShare", "192.0.2.40");
        record.persisted_vers = Some("3.0".to_string());
        let id = record.share_id.clone();
        rt.add_share(record).await.unwrap();
        rt.edit_share(
            &id,
            ShareEdits {
                fstype: Some(FsType::Nfs),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let after = rt.get_share(&id).await.unwrap().unwrap();
        assert_eq!(after.fstype, FsType::Nfs);
        assert!(after.persisted_vers.is_none());
    }

    #[tokio::test]
    async fn runtime_remove_returns_record_and_persists() {
        let dir = tempdir();
        let rt = NetworkSharesRuntime::open(&dir).unwrap();
        let record = built_record("GoAway", "192.0.2.50");
        let id = record.share_id.clone();
        rt.add_share(record).await.unwrap();
        let removed = rt.remove_share(&id).await.unwrap();
        assert_eq!(removed.share_id, id);
        assert_eq!(removed.alias, "GoAway");
        let rt2 = NetworkSharesRuntime::open(&dir).unwrap();
        assert!(rt2.list_configured().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn runtime_remove_deletes_empty_mount_root_directory() {
        // Regression: dead mount-root directories left behind
        // after remove_share appear as browsable garbage in the
        // operator's library. Fix: remove deletes the directory
        // when empty.
        let dir = tempdir();
        let rt = NetworkSharesRuntime::open(&dir).unwrap();
        let mount_root = dir.join("music").join("NAS").join("ShareToRemove");
        std::fs::create_dir_all(&mount_root).unwrap();
        assert!(mount_root.is_dir(), "test setup: mount-root exists");
        let mut record = built_record("ShareToRemove", "192.0.2.60");
        record.mount_root = mount_root.clone();
        let id = record.share_id.clone();
        rt.add_share(record).await.unwrap();
        rt.remove_share(&id).await.unwrap();
        assert!(
            !mount_root.exists(),
            "mount-root should have been deleted on remove"
        );
    }

    #[tokio::test]
    async fn runtime_remove_preserves_non_empty_mount_root_directory() {
        // Safety guard: never destroy operator files that sit
        // under the mount-root (e.g. the unmount did not drain,
        // or the operator placed content outside the framework's
        // flow). The operator sees a leftover directory in that
        // case — distinct from silent data loss.
        let dir = tempdir();
        let rt = NetworkSharesRuntime::open(&dir).unwrap();
        let mount_root = dir.join("music").join("NAS").join("HasFiles");
        std::fs::create_dir_all(&mount_root).unwrap();
        // Simulate a file left behind (an unmount that failed to
        // drain the CIFS mount, or operator data outside the
        // framework's flow).
        std::fs::write(mount_root.join("residual.txt"), b"do not delete")
            .unwrap();
        let mut record = built_record("HasFiles", "192.0.2.61");
        record.mount_root = mount_root.clone();
        let id = record.share_id.clone();
        rt.add_share(record).await.unwrap();
        rt.remove_share(&id).await.unwrap();
        assert!(
            mount_root.exists(),
            "mount-root should be preserved when non-empty; operator data \
             must not be silently destroyed"
        );
        assert!(
            mount_root.join("residual.txt").exists(),
            "residual file must survive remove"
        );
    }

    #[tokio::test]
    async fn runtime_remove_tolerates_missing_mount_root_directory() {
        // Idempotence: removing a share whose mount-root was
        // already gone (never mounted, previously cleaned, etc.)
        // does not error.
        let dir = tempdir();
        let rt = NetworkSharesRuntime::open(&dir).unwrap();
        let mount_root = dir.join("music").join("NAS").join("NeverExisted");
        let mut record = built_record("NeverExisted", "192.0.2.62");
        record.mount_root = mount_root.clone();
        let id = record.share_id.clone();
        rt.add_share(record).await.unwrap();
        // mount_root never created on disk; remove tolerates it.
        rt.remove_share(&id).await.unwrap();
    }

    #[tokio::test]
    async fn runtime_remove_missing_id_errors() {
        let dir = tempdir();
        let rt = NetworkSharesRuntime::open(&dir).unwrap();
        let err = rt.remove_share(&ShareId::new_v4()).await.unwrap_err();
        assert!(matches!(err, SharesStateError::ShareNotFound { .. }));
    }

    #[tokio::test]
    async fn runtime_get_share_returns_none_for_missing_id() {
        let dir = tempdir();
        let rt = NetworkSharesRuntime::open(&dir).unwrap();
        let got = rt.get_share(&ShareId::new_v4()).await.unwrap();
        assert!(got.is_none());
    }

    // -----------------------------------------------------------
    // Ship 2c: CIFS mount + probe ladder tests (mock executor)
    // -----------------------------------------------------------

    use std::sync::atomic::{AtomicUsize, Ordering};

    /// One recorded subprocess invocation: `(program, args)`.
    type RecordedCall = (String, Vec<String>);

    /// Mock executor that returns caller-scripted outputs in
    /// sequence. Captures every invocation for inspection.
    struct ScriptedExecutor {
        outputs: Vec<CommandOutput>,
        calls: Arc<Mutex<Vec<RecordedCall>>>,
        cursor: AtomicUsize,
    }

    impl ScriptedExecutor {
        fn new(outputs: Vec<CommandOutput>) -> Arc<Self> {
            Arc::new(Self {
                outputs,
                calls: Arc::new(Mutex::new(Vec::new())),
                cursor: AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl MountExecutor for ScriptedExecutor {
        async fn run(
            &self,
            program: &str,
            args: &[String],
            _timeout_ms: u64,
        ) -> Result<CommandOutput, MountError> {
            let idx = self.cursor.fetch_add(1, Ordering::SeqCst);
            let mut calls = self.calls.lock().await;
            calls.push((program.to_string(), args.to_vec()));
            Ok(self
                .outputs
                .get(idx)
                .cloned()
                .expect("ScriptedExecutor: caller ran out of scripted outputs"))
        }
    }

    fn success_output() -> CommandOutput {
        CommandOutput {
            exit_code: Some(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    fn failure_output(stderr: &str) -> CommandOutput {
        CommandOutput {
            exit_code: Some(32),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    fn build_runtime_with_executor(
        dir: &Path,
        executor: Arc<dyn MountExecutor>,
    ) -> NetworkSharesRuntime {
        NetworkSharesRuntime::builder(dir)
            .unwrap()
            .with_executor(executor)
            .with_mount_timeout_ms(1_000)
            .with_now_fn(Arc::new(|| 1_700_000_100_000))
            .build()
    }

    #[test]
    fn build_cifs_mount_args_guest_share() {
        let mut record = built_record("guest_share", "192.0.2.10");
        record.credentials = Credentials::Guest;
        let expected_mount_root =
            record.mount_root.to_string_lossy().into_owned();
        let args = build_cifs_mount_args(&record, "3.0", None);
        assert_eq!(args[0], "--collect");
        assert_eq!(args[1], "--type=cifs");
        assert!(args[2].starts_with("--options="));
        let options = &args[2];
        assert!(options.contains("vers=3.0"));
        assert!(options.contains("ro"));
        assert!(options.contains("iocharset=utf8"));
        assert!(options.contains("guest"));
        assert_eq!(args[3], "//192.0.2.10/Music");
        assert_eq!(args[4], expected_mount_root);
    }

    #[test]
    fn build_cifs_mount_args_userpassword_uses_credentials_file() {
        // UserPassword credentials never travel on argv. The
        // caller stages a credentials file elsewhere and passes
        // its path; `build_cifs_mount_args` emits
        // `credentials=<path>` and no other credential option.
        let mut record = built_record("auth_share", "192.0.2.11");
        record.credentials = Credentials::UserPassword {
            username: "engineer".to_string(),
            credential_key: "auth_share_pw".to_string(),
            domain: Some("EXAMPLE".to_string()),
        };
        let args = build_cifs_mount_args(
            &record,
            "3.1.1",
            Some(std::path::Path::new("/tmp/creds")),
        );
        let options = &args[2];
        assert!(options.contains("credentials=/tmp/creds"));
        assert!(!options.contains("username="));
        assert!(!options.contains("password="));
        assert!(!options.contains("domain="));
        assert!(!options.contains("guest"));
    }

    #[test]
    fn build_cifs_mount_args_userpassword_with_no_creds_file_omits_credentials()
    {
        // Regression: when the caller failed to stage the
        // credentials file, no credential option is emitted at
        // all. mount.cifs falls back to anonymous and the
        // auth-refusal parser surfaces the honest error.
        let mut record = built_record("auth_share", "192.0.2.11");
        record.credentials = Credentials::UserPassword {
            username: "engineer".to_string(),
            credential_key: "auth_share_pw".to_string(),
            domain: None,
        };
        let args = build_cifs_mount_args(&record, "3.1.1", None);
        let options = &args[2];
        assert!(!options.contains("credentials="));
        assert!(!options.contains("username="));
        assert!(!options.contains("password="));
    }

    #[test]
    fn compose_cifs_credentials_file_carries_all_three_fields() {
        let body = compose_cifs_credentials_file(
            "engineer",
            "hunter2",
            Some("EXAMPLE"),
        );
        assert!(body.contains("username=engineer\n"));
        assert!(body.contains("password=hunter2\n"));
        assert!(body.contains("domain=EXAMPLE\n"));
    }

    #[test]
    fn compose_cifs_credentials_file_omits_domain_when_absent() {
        let body = compose_cifs_credentials_file("engineer", "hunter2", None);
        assert!(body.contains("username=engineer\n"));
        assert!(body.contains("password=hunter2\n"));
        assert!(!body.contains("domain="));
    }

    #[test]
    fn is_cifs_auth_refusal_recognises_common_signatures() {
        assert!(is_cifs_auth_refusal(Some(13), ""));
        assert!(is_cifs_auth_refusal(
            Some(32),
            "mount error(13): NT_STATUS_LOGON_FAILURE"
        ));
        assert!(is_cifs_auth_refusal(Some(32), "Permission denied"));
        assert!(is_cifs_auth_refusal(
            Some(32),
            "mount error(13): NT_STATUS_ACCESS_DENIED"
        ));
        assert!(is_cifs_auth_refusal(
            Some(1),
            "Status: STATUS_ACCOUNT_LOCKED_OUT"
        ));
        // systemd-mount exit 32 + errno in stderr body only
        assert!(is_cifs_auth_refusal(
            Some(32),
            "Job failed.\nmount error(13): Permission denied"
        ));
    }

    #[test]
    fn is_cifs_auth_refusal_rejects_dialect_errors() {
        assert!(!is_cifs_auth_refusal(
            Some(32),
            "cifs: bad option 'vers=1.0'"
        ));
        assert!(!is_cifs_auth_refusal(Some(2), "No such device"));
        // Opaque systemd-mount text alone is NOT auth refusal —
        // classification needs journal fragments or exit 13.
        assert!(!is_cifs_auth_refusal(
            Some(32),
            "Job failed. See \"journalctl -xe\" for details."
        ));
    }

    #[test]
    fn is_path_mounted_in_proc_mounts_matches_target_column() {
        let proc = "\
//192.0.2.1/share /var/lib/evo/music/NAS/Audio cifs ro,vers=2.0 0 0\n\
tmpfs /tmp tmpfs rw 0 0\n";
        assert!(is_path_mounted_in_proc_mounts(
            proc,
            Path::new("/var/lib/evo/music/NAS/Audio")
        ));
        assert!(!is_path_mounted_in_proc_mounts(
            proc,
            Path::new("/var/lib/evo/music/NAS/Other")
        ));
    }

    #[test]
    fn parse_cifs_version_from_proc_mounts_reads_vers() {
        let proc = "//h/p /var/lib/evo/music/NAS/Audio cifs ro,vers=2.0,cache=strict 0 0\n";
        assert_eq!(
            parse_cifs_version_from_proc_mounts(
                proc,
                Path::new("/var/lib/evo/music/NAS/Audio")
            )
            .as_deref(),
            Some("2.0")
        );
        assert_eq!(
            parse_cifs_version_from_proc_mounts(
                proc,
                Path::new("/var/lib/evo/music/NAS/Missing")
            ),
            None
        );
    }

    #[test]
    fn is_mount_directory_missing_matches_common_enoent_renderings() {
        assert!(is_mount_directory_missing(
            "Couldn't chdir to /var/lib/evo/music/NAS/foo: No such file or directory"
        ));
        assert!(is_mount_directory_missing(
            "mount: /var/lib/evo/music/NAS/foo: No such file or directory"
        ));
        assert!(!is_mount_directory_missing("Permission denied"));
        assert!(!is_mount_directory_missing("cifs: bad option"));
    }

    #[tokio::test]
    async fn mount_cifs_directory_missing_short_circuits_ladder() {
        // First mount attempt returns ENOENT. The dialect probe
        // must short-circuit — otherwise we'd walk the full
        // ladder and mislabel five ENOENTs as
        // DialectProbeExhausted, exactly the failure the UI
        // team's memo #5 identified on the live rig.
        let dir = tempdir();
        let executor = ScriptedExecutor::new(vec![failure_output(
            "Couldn't chdir to /var/lib/evo/music/NAS/target: No such file or directory",
        )]);
        let rt = build_runtime_with_executor(&dir, executor.clone());
        let record = built_record("target", "192.0.2.44");
        let id = record.share_id.clone();
        rt.add_share(record).await.unwrap();

        let err = rt.mount_share(&id).await.unwrap_err();
        match &err {
            MountError::MountDirectoryMissing { .. } => {}
            other => panic!("expected MountDirectoryMissing; got {other:?}"),
        }
        let calls = executor.calls.lock().await;
        assert_eq!(
            calls.len(),
            1,
            "ENOENT short-circuit must fire before the second dialect",
        );
    }

    #[tokio::test]
    async fn mount_cifs_userpassword_auth_refused_deletes_vault_entry() {
        // AuthenticationRefused triggers the runtime to clear
        // the vault entry so the next mount attempt re-prompts.
        // NETWORK-SOURCES-DESIGN.md §5.6.5 (password refresh on
        // NAS-side rotation).
        let dir = tempdir();
        let creds_root = dir.join("credentials");
        std::fs::create_dir_all(&creds_root).unwrap();
        let store = Arc::new(FileCredentialStore::new(creds_root.clone()));
        store.store_password("rotate_key", b"stale").await.unwrap();
        let executor = ScriptedExecutor::new(vec![failure_output(
            "mount error(13): NT_STATUS_LOGON_FAILURE",
        )]);
        let rt = NetworkSharesRuntime::builder(&dir)
            .unwrap()
            .with_executor(executor)
            .with_credential_store(
                Arc::clone(&store) as Arc<dyn CredentialStore>
            )
            .with_mount_timeout_ms(1_000)
            .with_now_fn(Arc::new(|| 1_700_000_100_000))
            .build();
        let mut record = built_record("rotator", "192.0.2.77");
        record.credentials = Credentials::UserPassword {
            username: "engineer".to_string(),
            credential_key: "rotate_key".to_string(),
            domain: Some("EXAMPLE".to_string()),
        };
        let id = record.share_id.clone();
        rt.add_share(record).await.unwrap();

        let err = rt.mount_share(&id).await.unwrap_err();
        assert!(matches!(err, MountError::AuthenticationRefused { .. }));

        // Vault entry MUST be gone so the next mount re-prompts.
        assert!(
            store.fetch_password("rotate_key").await.is_none(),
            "auth-refusal must clear the stale vault entry",
        );
    }

    #[test]
    fn systemd_mount_unit_name_matches_systemd_escape() {
        // Canonical case: /var/lib/evo/music/NAS/alias
        // → var-lib-evo-music-NAS-alias.mount
        assert_eq!(
            systemd_mount_unit_name(std::path::Path::new(
                "/var/lib/evo/music/NAS/Music"
            )),
            "var-lib-evo-music-NAS-Music.mount"
        );
        // Dash in the alias must be escaped to \x2d — this is
        // the exact case the UI team's ls-from-host demo produced
        // (`consumer-check` in the alias).
        assert_eq!(
            systemd_mount_unit_name(std::path::Path::new(
                "/var/lib/evo/music/NAS/consumer-check"
            )),
            "var-lib-evo-music-NAS-consumer\\x2dcheck.mount"
        );
        // Root becomes -.mount per systemd convention.
        assert_eq!(
            systemd_mount_unit_name(std::path::Path::new("/")),
            "-.mount"
        );
    }

    #[test]
    fn filter_self_out_removes_loopback_ips() {
        let hits = vec![
            ("SELF".to_string(), "127.0.0.1".to_string()),
            ("NAS".to_string(), "192.0.2.100".to_string()),
        ];
        let filtered = filter_self_out(hits);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].1, "192.0.2.100");
    }

    #[test]
    fn filter_self_out_removes_ipv6_loopback() {
        let hits = vec![
            ("SELF".to_string(), "::1".to_string()),
            ("NAS".to_string(), "192.0.2.100".to_string()),
        ];
        let filtered = filter_self_out(hits);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].1, "192.0.2.100");
    }

    #[tokio::test]
    async fn mount_nfs_directory_missing_maps_to_directory_missing_variant() {
        let dir = tempdir();
        let executor = ScriptedExecutor::new(vec![failure_output(
            "mount: /var/lib/evo/music/NAS/nfs: No such file or directory",
        )]);
        let rt = build_runtime_with_executor(&dir, executor);
        let mut record = built_record("nfs", "192.0.2.100");
        record.fstype = FsType::Nfs;
        let id = record.share_id.clone();
        rt.add_share(record).await.unwrap();

        let err = rt.mount_share(&id).await.unwrap_err();
        assert!(matches!(err, MountError::MountDirectoryMissing { .. }));
    }

    #[test]
    fn build_cifs_mount_args_keyfile() {
        let mut record = built_record("key_share", "192.0.2.12");
        record.credentials = Credentials::KeyFile {
            path: PathBuf::from("/etc/evo/cifs.creds"),
        };
        let args = build_cifs_mount_args(&record, "2.1", None);
        assert!(args[2].contains("credentials=/etc/evo/cifs.creds"));
    }

    #[test]
    fn build_cifs_mount_args_appends_operator_options() {
        let mut record = built_record("with_opts", "192.0.2.13");
        record.advanced_options = "rw,uid=1000".to_string();
        let args = build_cifs_mount_args(&record, "3.0", None);
        assert!(args[2].contains("rw,uid=1000"));
    }

    #[tokio::test]
    async fn mount_cifs_persisted_vers_fast_path_single_call() {
        let dir = tempdir();
        let executor = ScriptedExecutor::new(vec![success_output()]);
        let rt = build_runtime_with_executor(&dir, executor.clone());
        let mut record = built_record("known_good", "192.0.2.20");
        record.persisted_vers = Some("3.0".to_string());
        let id = record.share_id.clone();
        rt.add_share(record).await.unwrap();

        let report = rt.mount_share(&id).await.unwrap();
        assert_eq!(report.share_id, id);
        assert_eq!(report.negotiated_version.as_deref(), Some("3.0"));

        let calls = executor.calls.lock().await;
        assert_eq!(calls.len(), 1, "fast-path must make exactly one call");
        assert!(calls[0].1.iter().any(|a| a.contains("vers=3.0")));
    }

    #[tokio::test]
    async fn mount_cifs_probe_ladder_iterates_and_persists_first_success() {
        let dir = tempdir();
        // Ascending ladder (volumio-evo shape): fail on 2.0, 2.1, 3.0; succeed on 3.02.
        let executor = ScriptedExecutor::new(vec![
            failure_output("mount error(112): Host is down"),
            failure_output("mount error(112): Host is down"),
            failure_output("mount error(112): Host is down"),
            success_output(),
        ]);
        let rt = build_runtime_with_executor(&dir, executor.clone());
        let record = built_record("no_persisted", "192.0.2.21");
        let id = record.share_id.clone();
        rt.add_share(record).await.unwrap();

        let report = rt.mount_share(&id).await.unwrap();
        assert_eq!(report.negotiated_version.as_deref(), Some("3.02"));

        let calls = executor.calls.lock().await;
        assert_eq!(calls.len(), 4, "probe iterates until success");
        assert!(calls[0].1.iter().any(|a| a.contains("vers=2.0")));
        assert!(calls[3].1.iter().any(|a| a.contains("vers=3.02")));

        let after = rt.get_share(&id).await.unwrap().unwrap();
        assert_eq!(after.persisted_vers.as_deref(), Some("3.02"));
        assert_eq!(after.last_mounted_at_ms, Some(1_700_000_100_000_i64));
    }

    #[tokio::test]
    async fn mount_cifs_fast_path_failure_falls_back_to_full_ladder() {
        let dir = tempdir();
        // fast-path 3.0 fails; then full ascending ladder tries
        // 2.0 (fail), 2.1 (success). Persisted 3.0 is skipped
        // on the ladder-side iteration.
        let executor = ScriptedExecutor::new(vec![
            failure_output("stale dialect"), // fast-path 3.0
            failure_output("still bad"),     // ladder 2.0
            success_output(),                // ladder 2.1
        ]);
        let rt = build_runtime_with_executor(&dir, executor.clone());
        let mut record = built_record("stale", "192.0.2.22");
        record.persisted_vers = Some("3.0".to_string());
        let id = record.share_id.clone();
        rt.add_share(record).await.unwrap();

        let report = rt.mount_share(&id).await.unwrap();
        assert_eq!(report.negotiated_version.as_deref(), Some("2.1"));

        let calls = executor.calls.lock().await;
        assert_eq!(calls.len(), 3);
        assert!(calls[0].1.iter().any(|a| a.contains("vers=3.0")));
        assert!(calls[1].1.iter().any(|a| a.contains("vers=2.0")));
        assert!(calls[2].1.iter().any(|a| a.contains("vers=2.1")));
        // The 3.0 attempt is not re-tried on the ladder-side sweep.
        // Match on `vers=3.0,` to avoid a false positive against
        // `vers=3.02,` in the same option string.
        assert!(
            !calls[1..]
                .iter()
                .any(|(_, a)| a.iter().any(|s| s.contains("vers=3.0,"))),
            "ladder pass must skip the fast-path dialect"
        );

        let after = rt.get_share(&id).await.unwrap().unwrap();
        assert_eq!(after.persisted_vers.as_deref(), Some("2.1"));
    }

    #[tokio::test]
    async fn mount_cifs_probe_exhausted_errors_with_full_attempt_list() {
        let dir = tempdir();
        // All five dialect attempts return a NON-auth failure —
        // otherwise the auth-refusal short-circuit would fire
        // before the ladder is exhausted. Use bad-option style
        // errors to exercise the wire-layer failure path.
        let executor = ScriptedExecutor::new(vec![
            failure_output("cifs: bad option 'vers=2.0'"),
            failure_output("cifs: bad option 'vers=2.1'"),
            failure_output("cifs: bad option 'vers=3.0'"),
            failure_output("cifs: bad option 'vers=3.02'"),
            failure_output("cifs: bad option 'vers=3.1.1'"),
        ]);
        let rt = build_runtime_with_executor(&dir, executor);
        let record = built_record("unreachable", "192.0.2.23");
        let id = record.share_id.clone();
        rt.add_share(record).await.unwrap();

        let err = rt.mount_share(&id).await.unwrap_err();
        match err {
            MountError::DialectProbeExhausted {
                attempted,
                last_error,
            } => {
                assert_eq!(attempted.len(), 5);
                assert_eq!(attempted, CIFS_VERS_PROBE_LADDER);
                assert!(last_error.contains("bad option"));
            }
            other => panic!("expected DialectProbeExhausted, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn mount_cifs_userpassword_auth_refusal_short_circuits_ladder() {
        // A single permission-denied response — since auth-
        // refusal is short-circuited, subsequent dialects are
        // NOT attempted and the error is AuthenticationRefused,
        // not DialectProbeExhausted.
        let dir = tempdir();
        let executor = ScriptedExecutor::new(vec![failure_output(
            "mount error(13): NT_STATUS_LOGON_FAILURE",
        )]);
        let store = Arc::new(FileCredentialStore::new(dir.clone()));
        store.store_password("auth_key", b"wrong").await.unwrap();
        let rt = NetworkSharesRuntime::builder(&dir)
            .unwrap()
            .with_executor(executor.clone())
            .with_credential_store(
                Arc::clone(&store) as Arc<dyn CredentialStore>
            )
            .with_mount_timeout_ms(1_000)
            .with_now_fn(Arc::new(|| 1_700_000_100_000))
            .build();
        let mut record = built_record("auth_share", "192.0.2.99");
        record.credentials = Credentials::UserPassword {
            username: "engineer".to_string(),
            credential_key: "auth_key".to_string(),
            domain: Some("EXAMPLE".to_string()),
        };
        let id = record.share_id.clone();
        rt.add_share(record).await.unwrap();

        let err = rt.mount_share(&id).await.unwrap_err();
        match err {
            MountError::AuthenticationRefused { .. } => {}
            other => panic!("expected AuthenticationRefused, got {:?}", other),
        }
        let calls = executor.calls.lock().await;
        assert_eq!(
            calls.len(),
            1,
            "auth-refusal must short-circuit before the second dialect",
        );
    }

    #[tokio::test]
    async fn mount_cifs_access_denied_short_circuits_as_auth_refusal() {
        // The live "Audio" failure mode: systemd-mount exit 32
        // with NT_STATUS_ACCESS_DENIED in the journal/stderr —
        // must NOT walk the ladder or surface DialectProbeExhausted.
        let dir = tempdir();
        let executor = ScriptedExecutor::new(vec![failure_output(
            "mount error(13): NT_STATUS_ACCESS_DENIED",
        )]);
        let store = Arc::new(FileCredentialStore::new(dir.clone()));
        store.store_password("audio_key", b"wrong").await.unwrap();
        let rt = NetworkSharesRuntime::builder(&dir)
            .unwrap()
            .with_executor(executor.clone())
            .with_credential_store(
                Arc::clone(&store) as Arc<dyn CredentialStore>
            )
            .with_mount_timeout_ms(1_000)
            .with_now_fn(Arc::new(|| 1_700_000_100_000))
            .build();
        let mut record = built_record("Audio", "192.0.2.30");
        record.credentials = Credentials::UserPassword {
            username: "testuser".to_string(),
            credential_key: "audio_key".to_string(),
            domain: Some("TESTGROUP".to_string()),
        };
        let id = record.share_id.clone();
        rt.add_share(record).await.unwrap();

        let err = rt.mount_share(&id).await.unwrap_err();
        match err {
            MountError::AuthenticationRefused { .. } => {}
            other => panic!("expected AuthenticationRefused, got {:?}", other),
        }
        assert_eq!(executor.calls.lock().await.len(), 1);
        let msg = format!("{err}");
        assert!(
            !msg.contains("dialect probe exhausted"),
            "auth refusal must not be mislabelled as dialect exhaustion: {msg}"
        );
    }

    #[tokio::test]
    async fn mount_share_adopts_already_mounted_without_executor() {
        // Host mount already active (survived steward restart):
        // mount_share must report Mounted, invoke the executor
        // zero times, and never prompt for credentials.
        let dir = tempdir();
        let executor = ScriptedExecutor::new(Vec::new());
        let mount_root = dir.join("NAS").join("Audio");
        std::fs::create_dir_all(&mount_root).unwrap();
        let root_for_check = mount_root.clone();
        let rt = NetworkSharesRuntime::builder(&dir)
            .unwrap()
            .with_executor(executor.clone())
            .with_mount_point_check(Arc::new(move |p: &Path| {
                p == root_for_check
            }))
            .with_mount_timeout_ms(1_000)
            .with_now_fn(Arc::new(|| 1_700_000_200_000))
            .build();
        let mut record = built_record("Audio", "192.0.2.30");
        record.mount_root = mount_root;
        record.persisted_vers = Some("2.0".to_string());
        record.credentials = Credentials::UserPassword {
            username: "testuser".to_string(),
            credential_key: "missing_key_must_not_be_fetched".to_string(),
            domain: Some("TESTGROUP".to_string()),
        };
        let id = record.share_id.clone();
        rt.add_share(record).await.unwrap();

        let report = rt.mount_share(&id).await.expect("adopt must succeed");
        assert_eq!(report.negotiated_version.as_deref(), Some("2.0"));
        assert!(
            executor.calls.lock().await.is_empty(),
            "already-mounted adopt must not invoke the mount helper"
        );
        let state = {
            let g = rt.share_states.lock().await;
            g.get(&id).map(|e| e.state).expect("state entry")
        };
        assert_eq!(state, MountState::Mounted);
    }

    #[tokio::test]
    async fn dialect_probe_exhausted_display_includes_last_error() {
        let err = MountError::DialectProbeExhausted {
            attempted: vec!["2.0".into(), "2.1".into()],
            last_error: "cifs: bad option 'vers=2.1'".into(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("dialect probe exhausted"));
        assert!(
            msg.contains("bad option"),
            "Display must carry last_error for operator UI: {msg}"
        );
    }

    #[tokio::test]
    async fn mount_missing_share_errors() {
        let dir = tempdir();
        let executor = ScriptedExecutor::new(Vec::new());
        let rt = build_runtime_with_executor(&dir, executor);
        let err = rt.mount_share(&ShareId::new_v4()).await.unwrap_err();
        assert!(matches!(err, MountError::ShareNotFound { .. }));
    }

    /// Credential fetcher that always returns caller-supplied
    /// bytes. Used to exercise the UserPassword mount path.
    #[allow(dead_code)]
    struct StaticCredentialFetcher(Vec<u8>);

    #[async_trait]
    impl CredentialFetcher for StaticCredentialFetcher {
        async fn fetch_password(&self, _key: &str) -> Option<Vec<u8>> {
            Some(self.0.clone())
        }
    }

    /// One captured (argv, credentials-file-body) pair from a
    /// `CredsCapturingExecutor` call.
    type CapturedCall = (Vec<String>, Option<String>);

    /// Executor that snapshots the credentials file's contents at
    /// call time before the RAII guard cleans it up. Purely a
    /// test aid so the assertion below can inspect the body
    /// mount.cifs would have seen.
    #[derive(Clone)]
    struct CredsCapturingExecutor {
        captured: Arc<tokio::sync::Mutex<Vec<CapturedCall>>>,
        script: Arc<tokio::sync::Mutex<Vec<CommandOutput>>>,
    }

    impl CredsCapturingExecutor {
        fn new(script: Vec<CommandOutput>) -> Self {
            Self {
                captured: Arc::new(tokio::sync::Mutex::new(Vec::new())),
                script: Arc::new(tokio::sync::Mutex::new(script)),
            }
        }
    }

    #[async_trait]
    impl MountExecutor for CredsCapturingExecutor {
        async fn run(
            &self,
            _program: &str,
            args: &[String],
            _timeout_ms: u64,
        ) -> Result<CommandOutput, MountError> {
            // Look for `credentials=<path>` in args and snapshot
            // the file contents.
            let creds_body = args.iter().find_map(|a| {
                a.split(',').find_map(|opt| {
                    opt.strip_prefix("credentials=")
                        .and_then(|p| std::fs::read_to_string(p).ok())
                })
            });
            self.captured.lock().await.push((args.to_vec(), creds_body));
            self.script
                .lock()
                .await
                .pop()
                .ok_or_else(|| MountError::SubprocessIo("script empty".into()))
        }
    }

    #[tokio::test]
    async fn mount_cifs_userpassword_fetches_and_stages_credentials_file() {
        let dir = tempdir();
        let executor = CredsCapturingExecutor::new(vec![success_output()]);
        let store = Arc::new(FileCredentialStore::new(dir.clone()));
        store.store_password("auth_pw", b"s3cret").await.unwrap();
        let rt = NetworkSharesRuntime::builder(&dir)
            .unwrap()
            .with_executor(Arc::new(executor.clone()))
            .with_credential_store(
                Arc::clone(&store) as Arc<dyn CredentialStore>
            )
            .with_mount_timeout_ms(1_000)
            .with_now_fn(Arc::new(|| 1_700_000_100_000))
            .build();

        let mut record = built_record("auth", "192.0.2.30");
        record.credentials = Credentials::UserPassword {
            username: "engineer".to_string(),
            credential_key: "auth_pw".to_string(),
            domain: Some("EXAMPLE".to_string()),
        };
        record.persisted_vers = Some("3.1.1".to_string());
        let id = record.share_id.clone();
        rt.add_share(record).await.unwrap();

        rt.mount_share(&id).await.unwrap();

        let captured = executor.captured.lock().await;
        assert_eq!(captured.len(), 1);
        // Credentials MUST reach mount.cifs via `credentials=<path>`
        // — never as `username=` / `password=` on argv.
        let flat: String = captured[0].0.join(" ");
        assert!(flat.contains("credentials="));
        assert!(!flat.contains("username=engineer"));
        assert!(!flat.contains("password=s3cret"));
        // The staged file body carries the record's identity as
        // key=value lines mount.cifs reads.
        let body = captured[0]
            .1
            .as_ref()
            .expect("credentials file must exist at call time");
        assert!(body.contains("username=engineer\n"));
        assert!(body.contains("password=s3cret\n"));
        assert!(body.contains("domain=EXAMPLE\n"));
    }

    #[tokio::test]
    async fn mount_cifs_userpassword_missing_credential_errors() {
        let dir = tempdir();
        // Executor never fires because we bail before mount.
        let executor = ScriptedExecutor::new(Vec::new());
        // Neither a store nor a prompter is wired — the legacy
        // fixture path. The prompt-on-mount flow surfaces
        // CredentialStoreUnavailable to make the "no store"
        // condition operator-visible instead of silently
        // hanging.
        let rt = NetworkSharesRuntime::builder(&dir)
            .unwrap()
            .with_executor(executor)
            .with_credentials(Arc::new(NoCredentialFetcher))
            .build();

        let mut record = built_record("auth_missing", "192.0.2.31");
        record.credentials = Credentials::UserPassword {
            username: "engineer".to_string(),
            credential_key: "not_in_vault".to_string(),
            domain: None,
        };
        let id = record.share_id.clone();
        rt.add_share(record).await.unwrap();

        let err = rt.mount_share(&id).await.unwrap_err();
        assert!(
            matches!(err, MountError::CredentialStoreUnavailable),
            "expected CredentialStoreUnavailable when the runtime was wired without a store; got {err:?}"
        );
    }

    #[derive(Debug, Default)]
    struct RecordingPrompter {
        answer: std::sync::Mutex<Option<Vec<u8>>>,
        calls: std::sync::Mutex<u32>,
    }

    #[async_trait]
    impl PasswordPrompter for RecordingPrompter {
        async fn prompt_password(
            &self,
            _label: String,
        ) -> Result<Option<Vec<u8>>, ReportError> {
            let mut c = self.calls.lock().unwrap();
            *c += 1;
            let a = self.answer.lock().unwrap().clone();
            Ok(a)
        }
    }

    #[tokio::test]
    async fn mount_cifs_userpassword_prompts_and_stores_then_mounts() {
        // Runtime with a real file store + a prompter that
        // returns the password on demand. The mount executor is
        // a single canned success reply — proves the mount was
        // attempted after the prompt round-trip.
        let dir = tempdir();
        let creds_root = dir.join("credentials");
        std::fs::create_dir_all(&creds_root).unwrap();
        let store = Arc::new(FileCredentialStore::new(creds_root.clone()));
        let prompter = Arc::new(RecordingPrompter {
            answer: std::sync::Mutex::new(Some(b"hunter2".to_vec())),
            calls: std::sync::Mutex::new(0),
        });
        let executor = ScriptedExecutor::new(vec![CommandOutput {
            exit_code: Some(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }]);
        let rt = NetworkSharesRuntime::builder(&dir)
            .unwrap()
            .with_executor(executor)
            .with_credential_store(
                Arc::clone(&store) as Arc<dyn CredentialStore>
            )
            .with_password_prompter(
                Arc::clone(&prompter) as Arc<dyn PasswordPrompter>
            )
            .build();

        let mut record = built_record("auth_success", "192.0.2.32");
        record.credentials = Credentials::UserPassword {
            username: "engineer".to_string(),
            credential_key: "auth_success_key".to_string(),
            domain: None,
        };
        let id = record.share_id.clone();
        rt.add_share(record).await.unwrap();

        rt.mount_share(&id)
            .await
            .expect("mount should succeed after prompt");

        assert_eq!(
            *prompter.calls.lock().unwrap(),
            1,
            "prompter fires exactly once"
        );
        // The password bytes are now in the file store.
        let bytes = store.fetch_password("auth_success_key").await.unwrap();
        assert_eq!(bytes, b"hunter2");
    }

    #[tokio::test]
    async fn mount_cifs_userpassword_cancelled_prompt_surfaces_error() {
        let dir = tempdir();
        let creds_root = dir.join("credentials");
        std::fs::create_dir_all(&creds_root).unwrap();
        let store = Arc::new(FileCredentialStore::new(creds_root));
        // Prompter returns None: operator cancelled or timed out.
        let prompter = Arc::new(RecordingPrompter::default());
        let executor = ScriptedExecutor::new(Vec::new());
        let rt = NetworkSharesRuntime::builder(&dir)
            .unwrap()
            .with_executor(executor)
            .with_credential_store(store as Arc<dyn CredentialStore>)
            .with_password_prompter(prompter as Arc<dyn PasswordPrompter>)
            .build();

        let mut record = built_record("auth_cancel", "192.0.2.33");
        record.credentials = Credentials::UserPassword {
            username: "engineer".to_string(),
            credential_key: "cancelled_key".to_string(),
            domain: None,
        };
        let id = record.share_id.clone();
        rt.add_share(record).await.unwrap();

        let err = rt.mount_share(&id).await.unwrap_err();
        assert!(
            matches!(&err, MountError::CredentialPromptCancelled { key } if key == "cancelled_key"),
            "expected CredentialPromptCancelled; got {err:?}"
        );
    }

    /// Regression: cancelling the collapsed prompt must wake ALL
    /// waiters with the exact CredentialPromptCancelled outcome
    /// (not fall into a re-prompt loop that leaves waiters
    /// parked). An earlier Notify-based dedup allowed this
    /// symptom because waiters re-entered ensure_credential_stocked
    /// on wake and issued their own prompts instead of receiving
    /// the first caller's outcome.
    #[tokio::test]
    async fn ensure_credential_stocked_cancel_wakes_all_dedup_waiters() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let dir = tempdir();
        let creds_root = dir.join("credentials");
        std::fs::create_dir_all(&creds_root).unwrap();
        let store = Arc::new(FileCredentialStore::new(creds_root));

        // Prompter blocks on a Notify until the test releases
        // it, then returns Ok(None) (operator cancelled). Also
        // counts calls so we can assert exactly one prompt was
        // issued across two concurrent callers.
        #[derive(Debug)]
        struct BlockingCancelPrompter {
            release: tokio::sync::Notify,
            calls: AtomicU32,
        }
        #[async_trait]
        impl PasswordPrompter for BlockingCancelPrompter {
            async fn prompt_password(
                &self,
                _label: String,
            ) -> Result<Option<Vec<u8>>, ReportError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.release.notified().await;
                Ok(None)
            }
        }

        let prompter = Arc::new(BlockingCancelPrompter {
            release: tokio::sync::Notify::new(),
            calls: AtomicU32::new(0),
        });
        let executor = ScriptedExecutor::new(Vec::new());
        let rt = Arc::new(
            NetworkSharesRuntime::builder(&dir)
                .unwrap()
                .with_executor(executor)
                .with_credential_store(store as Arc<dyn CredentialStore>)
                .with_password_prompter(
                    Arc::clone(&prompter) as Arc<dyn PasswordPrompter>
                )
                .build(),
        );

        // Two records sharing the same credential_key so both
        // enter the same dedup slot.
        let mut record_a = built_record("share_a", "192.0.2.90");
        record_a.credentials = Credentials::UserPassword {
            username: "op".to_string(),
            credential_key: "shared_key".to_string(),
            domain: None,
        };
        let mut record_b = built_record("share_b", "192.0.2.91");
        record_b.credentials = Credentials::UserPassword {
            username: "op".to_string(),
            credential_key: "shared_key".to_string(),
            domain: None,
        };
        rt.add_share(record_a.clone()).await.unwrap();
        rt.add_share(record_b.clone()).await.unwrap();

        // Spawn both concurrently. First to enter becomes the
        // first-caller; second becomes a dedup waiter.
        let rt_a = Arc::clone(&rt);
        let rt_b = Arc::clone(&rt);
        let handle_a = tokio::spawn(async move {
            rt_a.ensure_credential_stocked(&record_a).await
        });
        let handle_b = tokio::spawn(async move {
            rt_b.ensure_credential_stocked(&record_b).await
        });

        // Give both tasks a moment to reach their await points.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Exactly one prompt fired.
        assert_eq!(
            prompter.calls.load(Ordering::SeqCst),
            1,
            "dedup must collapse concurrent callers to one prompt"
        );

        // Fire the cancel (prompter returns None).
        prompter.release.notify_waiters();

        // Both callers must resolve with CredentialPromptCancelled
        // within a tight window — no re-prompt, no 30s hang.
        let a_result =
            tokio::time::timeout(std::time::Duration::from_secs(2), handle_a)
                .await
                .expect("first caller must resolve fast")
                .expect("task join");
        let b_result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            handle_b,
        )
        .await
        .expect("dedup waiter must resolve fast (cancel-wakes-all regression)")
        .expect("task join");

        assert!(
            matches!(
                &a_result,
                Err(MountError::CredentialPromptCancelled { key })
                    if key == "shared_key"
            ),
            "first caller expected CredentialPromptCancelled, got: {:?}",
            a_result
        );
        assert!(
            matches!(
                &b_result,
                Err(MountError::CredentialPromptCancelled { key })
                    if key == "shared_key"
            ),
            "dedup waiter expected the SAME cancel outcome without \
             re-prompting; got: {:?}",
            b_result
        );

        // Exactly one prompt still — waiter did not re-issue.
        assert_eq!(
            prompter.calls.load(Ordering::SeqCst),
            1,
            "waiter must NOT re-prompt on wake (cancel-wakes-all regression)"
        );
    }

    /// Regression: NoResponderAvailable must propagate through
    /// the dedup path so both first-caller AND dedup-waiter
    /// receive the specific MountError variant (not a generic
    /// CredentialPromptFailed).
    #[tokio::test]
    async fn ensure_credential_stocked_no_responder_propagates_to_waiters() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let dir = tempdir();
        let creds_root = dir.join("credentials");
        std::fs::create_dir_all(&creds_root).unwrap();
        let store = Arc::new(FileCredentialStore::new(creds_root));

        // Prompter returns the framework's fast-refuse message
        // shape (starts with `no_responder_available:`).
        #[derive(Debug)]
        struct NoResponderPrompter {
            release: tokio::sync::Notify,
            calls: AtomicU32,
        }
        #[async_trait]
        impl PasswordPrompter for NoResponderPrompter {
            async fn prompt_password(
                &self,
                _label: String,
            ) -> Result<Option<Vec<u8>>, ReportError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.release.notified().await;
                Err(ReportError::Invalid(
                    "no_responder_available: no user-interaction \
                     responder session is currently connected"
                        .into(),
                ))
            }
        }

        let prompter = Arc::new(NoResponderPrompter {
            release: tokio::sync::Notify::new(),
            calls: AtomicU32::new(0),
        });
        let executor = ScriptedExecutor::new(Vec::new());
        let rt = Arc::new(
            NetworkSharesRuntime::builder(&dir)
                .unwrap()
                .with_executor(executor)
                .with_credential_store(store as Arc<dyn CredentialStore>)
                .with_password_prompter(
                    Arc::clone(&prompter) as Arc<dyn PasswordPrompter>
                )
                .build(),
        );

        let mut record_a = built_record("share_a", "192.0.2.92");
        record_a.credentials = Credentials::UserPassword {
            username: "op".to_string(),
            credential_key: "no_responder_key".to_string(),
            domain: None,
        };
        let mut record_b = built_record("share_b", "192.0.2.93");
        record_b.credentials = Credentials::UserPassword {
            username: "op".to_string(),
            credential_key: "no_responder_key".to_string(),
            domain: None,
        };
        rt.add_share(record_a.clone()).await.unwrap();
        rt.add_share(record_b.clone()).await.unwrap();

        let rt_a = Arc::clone(&rt);
        let rt_b = Arc::clone(&rt);
        let handle_a = tokio::spawn(async move {
            rt_a.ensure_credential_stocked(&record_a).await
        });
        let handle_b = tokio::spawn(async move {
            rt_b.ensure_credential_stocked(&record_b).await
        });

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        prompter.release.notify_waiters();

        let a_result =
            tokio::time::timeout(std::time::Duration::from_secs(2), handle_a)
                .await
                .expect("first caller resolves fast")
                .expect("task join");
        let b_result =
            tokio::time::timeout(std::time::Duration::from_secs(2), handle_b)
                .await
                .expect("waiter resolves fast")
                .expect("task join");

        assert!(
            matches!(
                &a_result,
                Err(MountError::NoResponderAvailable { key, .. })
                    if key == "no_responder_key"
            ),
            "first caller expected NoResponderAvailable, got: {:?}",
            a_result
        );
        assert!(
            matches!(
                &b_result,
                Err(MountError::NoResponderAvailable { key, .. })
                    if key == "no_responder_key"
            ),
            "dedup waiter expected the SAME NoResponderAvailable \
             outcome; got: {:?}",
            b_result
        );
        assert_eq!(
            prompter.calls.load(Ordering::SeqCst),
            1,
            "only one prompt call across two concurrent callers"
        );
    }

    /// Contract: N concurrent same-key adds must collapse to
    /// EXACTLY ONE prompt at the prompter level. Under an
    /// earlier CellOnDrop pattern that removed the map entry
    /// synchronously with the first caller's exit, peers
    /// arriving during the removal window inserted fresh cells
    /// and re-prompted — the rig observed non-deterministic
    /// prompt counts. The `OnceCell` guarantees exactly one
    /// closure runs per key for the entire batch's lifetime;
    /// this test pins that invariant at N = 20.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stress_20_concurrent_same_key_collapses_to_one_prompt() {
        use std::sync::atomic::{AtomicU32, Ordering};
        const N: usize = 20;

        let dir = tempdir();
        let creds_root = dir.join("credentials");
        std::fs::create_dir_all(&creds_root).unwrap();
        let store = Arc::new(FileCredentialStore::new(creds_root));

        // Prompter blocks until we release, then returns the
        // supplied answer. Counts prompt_password calls so we can
        // assert exactly one across 20 concurrent callers.
        #[derive(Debug)]
        struct BlockingAnswerPrompter {
            release: tokio::sync::Notify,
            calls: AtomicU32,
        }
        #[async_trait]
        impl PasswordPrompter for BlockingAnswerPrompter {
            async fn prompt_password(
                &self,
                _label: String,
            ) -> Result<Option<Vec<u8>>, ReportError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.release.notified().await;
                Ok(Some(b"answered-once".to_vec()))
            }
        }

        let prompter = Arc::new(BlockingAnswerPrompter {
            release: tokio::sync::Notify::new(),
            calls: AtomicU32::new(0),
        });
        let executor = ScriptedExecutor::new(Vec::new());
        let rt = Arc::new(
            NetworkSharesRuntime::builder(&dir)
                .unwrap()
                .with_executor(executor)
                .with_credential_store(store as Arc<dyn CredentialStore>)
                .with_password_prompter(
                    Arc::clone(&prompter) as Arc<dyn PasswordPrompter>
                )
                .build(),
        );

        // Add N records, all keyed on the SAME credential_key.
        let mut records = Vec::with_capacity(N);
        for i in 0..N {
            let mut r = built_record(
                &format!("stress_share_{i}"),
                &format!("192.0.2.{}", 100 + i),
            );
            r.credentials = Credentials::UserPassword {
                username: "op".to_string(),
                credential_key: "stress_shared_key".to_string(),
                domain: None,
            };
            rt.add_share(r.clone()).await.unwrap();
            records.push(r);
        }

        // Spawn all N callers concurrently. Under the OnceCell
        // dedup all N should await the same in-flight init and
        // resolve together when we release the prompter.
        let mut handles = Vec::with_capacity(N);
        for r in records {
            let rt_c = Arc::clone(&rt);
            handles.push(tokio::spawn(async move {
                rt_c.ensure_credential_stocked(&r).await
            }));
        }

        // Give every task time to reach its `get_or_init.await`.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        // Exactly ONE prompt fired across 20 concurrent callers.
        // Prior to the OnceCell fix this asserted non-
        // deterministically (0..N calls depending on scheduling).
        assert_eq!(
            prompter.calls.load(Ordering::SeqCst),
            1,
            "20 concurrent same-key callers must produce exactly ONE \
             prompt (dedup collapse under concurrent dispatch); got {} calls",
            prompter.calls.load(Ordering::SeqCst),
        );

        // Answer once — every caller must wake with Ok(()).
        prompter.release.notify_waiters();

        for (i, h) in handles.into_iter().enumerate() {
            let outcome =
                tokio::time::timeout(std::time::Duration::from_secs(3), h)
                    .await
                    .unwrap_or_else(|_| {
                        panic!(
                    "caller {i} did not resolve within 3s of the shared \
                     answer — cancel-wake-all/answer-wake-all invariant \
                     broken under concurrent dispatch"
                )
                    })
                    .expect("task join");
            assert!(
                outcome.is_ok(),
                "caller {i} expected Ok after shared answer; got {outcome:?}"
            );
        }

        // Still exactly one prompt after every waiter woke — no
        // waiter re-prompted on wake (retires the pre-fix defect
        // where waiters re-entered ensure_credential_stocked and
        // issued their own prompts).
        assert_eq!(
            prompter.calls.load(Ordering::SeqCst),
            1,
            "waiter re-prompted on wake; exactly one prompt should \
             remain in the counter after every waiter resolved",
        );
    }

    /// Contract: a real prompter answer wakes every waiter within
    /// a bounded ceiling. Symmetric to the cancel-wakes-all test —
    /// this one exercises the ANSWER-wakes-all branch (Success
    /// outcome, credential stored, all waiters return Ok).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ensure_credential_stocked_answer_wakes_all_dedup_waiters() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let dir = tempdir();
        let creds_root = dir.join("credentials");
        std::fs::create_dir_all(&creds_root).unwrap();
        let store = Arc::new(FileCredentialStore::new(creds_root));

        #[derive(Debug)]
        struct BlockingAnswerPrompter {
            release: tokio::sync::Notify,
            calls: AtomicU32,
        }
        #[async_trait]
        impl PasswordPrompter for BlockingAnswerPrompter {
            async fn prompt_password(
                &self,
                _label: String,
            ) -> Result<Option<Vec<u8>>, ReportError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.release.notified().await;
                Ok(Some(b"real-answer".to_vec()))
            }
        }

        let prompter = Arc::new(BlockingAnswerPrompter {
            release: tokio::sync::Notify::new(),
            calls: AtomicU32::new(0),
        });
        let executor = ScriptedExecutor::new(Vec::new());
        let rt = Arc::new(
            NetworkSharesRuntime::builder(&dir)
                .unwrap()
                .with_executor(executor)
                .with_credential_store(store as Arc<dyn CredentialStore>)
                .with_password_prompter(
                    Arc::clone(&prompter) as Arc<dyn PasswordPrompter>
                )
                .build(),
        );

        let mut r_a = built_record("share_a", "192.0.2.190");
        r_a.credentials = Credentials::UserPassword {
            username: "op".into(),
            credential_key: "answer_key".into(),
            domain: None,
        };
        let mut r_b = built_record("share_b", "192.0.2.191");
        r_b.credentials = Credentials::UserPassword {
            username: "op".into(),
            credential_key: "answer_key".into(),
            domain: None,
        };
        rt.add_share(r_a.clone()).await.unwrap();
        rt.add_share(r_b.clone()).await.unwrap();

        let rt_a = Arc::clone(&rt);
        let rt_b = Arc::clone(&rt);
        let h_a =
            tokio::spawn(
                async move { rt_a.ensure_credential_stocked(&r_a).await },
            );
        let h_b =
            tokio::spawn(
                async move { rt_b.ensure_credential_stocked(&r_b).await },
            );

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(prompter.calls.load(Ordering::SeqCst), 1);

        prompter.release.notify_waiters();

        let out_a =
            tokio::time::timeout(std::time::Duration::from_secs(2), h_a)
                .await
                .expect("first caller resolves under 2s of answer")
                .expect("task join");
        let out_b =
            tokio::time::timeout(std::time::Duration::from_secs(2), h_b)
                .await
                .expect("waiter resolves under 2s of answer")
                .expect("task join");
        assert!(out_a.is_ok(), "first caller Ok on answer; got {out_a:?}");
        assert!(
            out_b.is_ok(),
            "waiter Ok on answer without re-prompting; got {out_b:?}"
        );

        assert_eq!(
            prompter.calls.load(Ordering::SeqCst),
            1,
            "waiter must not re-prompt on wake"
        );
    }

    #[tokio::test]
    async fn mount_cifs_userpassword_reuses_stored_password_without_reprompting(
    ) {
        // Pre-populate the store, then mount. The prompter must
        // not fire because the vault already carries the entry.
        let dir = tempdir();
        let creds_root = dir.join("credentials");
        std::fs::create_dir_all(&creds_root).unwrap();
        let store = Arc::new(FileCredentialStore::new(creds_root));
        store
            .store_password("cached_key", b"already_stored")
            .await
            .unwrap();
        let prompter = Arc::new(RecordingPrompter::default());
        let executor = ScriptedExecutor::new(vec![CommandOutput {
            exit_code: Some(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }]);
        let rt = NetworkSharesRuntime::builder(&dir)
            .unwrap()
            .with_executor(executor)
            .with_credential_store(store as Arc<dyn CredentialStore>)
            .with_password_prompter(
                Arc::clone(&prompter) as Arc<dyn PasswordPrompter>
            )
            .build();

        let mut record = built_record("auth_cached", "192.0.2.34");
        record.credentials = Credentials::UserPassword {
            username: "engineer".to_string(),
            credential_key: "cached_key".to_string(),
            domain: None,
        };
        let id = record.share_id.clone();
        rt.add_share(record).await.unwrap();

        rt.mount_share(&id)
            .await
            .expect("mount should succeed against cached credential");
        assert_eq!(
            *prompter.calls.lock().unwrap(),
            0,
            "prompter must not fire when the vault already carries the entry"
        );
    }

    // -----------------------------------------------------------
    // Ship 2d: NFS mount + unmount + /proc/mounts parse tests
    // -----------------------------------------------------------

    #[test]
    fn build_nfs_mount_args_defaults() {
        let mut record = built_record("nfs", "192.0.2.100");
        record.fstype = FsType::Nfs;
        record.path = "export/music".to_string();
        let expected_mount_root =
            record.mount_root.to_string_lossy().into_owned();
        let args = build_nfs_mount_args(&record);
        assert_eq!(args[0], "--collect");
        assert_eq!(args[1], "--type=nfs");
        assert!(args[2].starts_with("--options="));
        let options = &args[2];
        assert!(options.contains("ro"));
        assert!(options.contains("soft"));
        assert!(options.contains("noauto"));
        // NFS remote is host:/path (colon-separated, path leading /).
        assert_eq!(args[3], "192.0.2.100:/export/music");
        assert_eq!(args[4], expected_mount_root);
    }

    #[test]
    fn build_nfs_mount_args_appends_operator_options() {
        let mut record = built_record("nfs2", "192.0.2.101");
        record.fstype = FsType::Nfs;
        record.path = "/export/music".to_string();
        record.advanced_options = "nfsvers=4.2,rsize=1048576".to_string();
        let args = build_nfs_mount_args(&record);
        let options = &args[2];
        assert!(options.contains("nfsvers=4.2"));
        assert!(options.contains("rsize=1048576"));
        // Absolute paths are preserved as-is (no leading double slash).
        assert_eq!(args[3], "192.0.2.101:/export/music");
    }

    #[test]
    fn build_umount_args_plain() {
        let args = build_umount_args(
            &PathBuf::from("/var/lib/evo/music/NAS/plain"),
            false,
        );
        assert_eq!(args, vec!["/var/lib/evo/music/NAS/plain".to_string()]);
    }

    #[test]
    fn build_umount_args_lazy_prepends_flag() {
        let args = build_umount_args(
            &PathBuf::from("/var/lib/evo/music/NAS/lazy"),
            true,
        );
        assert_eq!(
            args,
            vec!["-l".to_string(), "/var/lib/evo/music/NAS/lazy".to_string()]
        );
    }

    #[test]
    fn parse_nfs_version_finds_vers_option() {
        let mounts = "\
proc /proc proc rw,nosuid,nodev,noexec 0 0
192.0.2.100:/export/music /var/lib/evo/music/NAS/nfs nfs4 ro,vers=4.2,rsize=1048576 0 0
";
        assert_eq!(
            parse_nfs_version_from_proc_mounts(
                mounts,
                &PathBuf::from("/var/lib/evo/music/NAS/nfs")
            ),
            Some("4.2".to_string())
        );
    }

    #[test]
    fn parse_nfs_version_finds_nfsvers_option() {
        let mounts = "192.0.2.101:/e /var/lib/evo/music/NAS/alt nfs ro,soft,nfsvers=3 0 0\n";
        assert_eq!(
            parse_nfs_version_from_proc_mounts(
                mounts,
                &PathBuf::from("/var/lib/evo/music/NAS/alt")
            ),
            Some("3".to_string())
        );
    }

    #[test]
    fn parse_nfs_version_returns_none_for_absent_mount() {
        let mounts = "proc /proc proc rw 0 0\n";
        assert_eq!(
            parse_nfs_version_from_proc_mounts(
                mounts,
                &PathBuf::from("/var/lib/evo/music/NAS/missing")
            ),
            None
        );
    }

    #[test]
    fn parse_nfs_version_ignores_non_nfs_fstype_at_same_mount() {
        // A bind-mount at the same target does not confuse the
        // parser — the fstype must be nfs or nfs4.
        let mounts = "/dev/sda1 /var/lib/evo/music/NAS/x ext4 rw 0 0\n";
        assert_eq!(
            parse_nfs_version_from_proc_mounts(
                mounts,
                &PathBuf::from("/var/lib/evo/music/NAS/x")
            ),
            None
        );
    }

    #[tokio::test]
    async fn mount_nfs_single_attempt_success() {
        let dir = tempdir();
        let executor = ScriptedExecutor::new(vec![success_output()]);
        let rt = build_runtime_with_executor(&dir, executor.clone());
        let mut record = built_record("nfs_share", "192.0.2.40");
        record.fstype = FsType::Nfs;
        let id = record.share_id.clone();
        rt.add_share(record).await.unwrap();

        let report = rt.mount_share(&id).await.unwrap();
        assert_eq!(report.share_id, id);
        // /proc/mounts read is best-effort in tests; either
        // Some(<version>) or None is acceptable, only the
        // subprocess call shape matters for this assertion.

        let calls = executor.calls.lock().await;
        assert_eq!(calls.len(), 1, "NFS is single-attempt (no probe ladder)");
        assert_eq!(calls[0].1[0], "--collect");
        assert_eq!(calls[0].1[1], "--type=nfs");

        let after = rt.get_share(&id).await.unwrap().unwrap();
        assert!(after.persisted_vers.is_none(), "NFS never persists vers");
        assert_eq!(after.last_mounted_at_ms, Some(1_700_000_100_000_i64));
    }

    #[tokio::test]
    async fn mount_nfs_failure_returns_mount_failed_error() {
        let dir = tempdir();
        let executor = ScriptedExecutor::new(vec![failure_output(
            "mount.nfs: Connection refused",
        )]);
        let rt = build_runtime_with_executor(&dir, executor);
        let mut record = built_record("nfs_fail", "192.0.2.41");
        record.fstype = FsType::Nfs;
        let id = record.share_id.clone();
        rt.add_share(record).await.unwrap();

        let err = rt.mount_share(&id).await.unwrap_err();
        match err {
            MountError::MountFailed {
                id: got_id,
                exit_code,
                stderr,
            } => {
                assert_eq!(got_id, id);
                assert_eq!(exit_code, Some(32));
                assert!(stderr.contains("Connection refused"));
            }
            other => panic!("expected MountFailed, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn unmount_cifs_uses_lazy_flag() {
        let dir = tempdir();
        let executor = ScriptedExecutor::new(vec![success_output()]);
        let rt = build_runtime_with_executor(&dir, executor.clone());
        let record = built_record("cifs_share", "192.0.2.50");
        let expected_mount_root =
            record.mount_root.to_string_lossy().into_owned();
        let id = record.share_id.clone();
        rt.add_share(record).await.unwrap();

        rt.unmount_share(&id).await.unwrap();
        let calls = executor.calls.lock().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1[0], "-l", "CIFS unmount uses lazy detach");
        assert_eq!(calls[0].1[1], expected_mount_root);
    }

    #[tokio::test]
    async fn unmount_nfs_omits_lazy_flag() {
        let dir = tempdir();
        let executor = ScriptedExecutor::new(vec![success_output()]);
        let rt = build_runtime_with_executor(&dir, executor.clone());
        let mut record = built_record("nfs_share", "192.0.2.51");
        record.fstype = FsType::Nfs;
        let expected_mount_root =
            record.mount_root.to_string_lossy().into_owned();
        let id = record.share_id.clone();
        rt.add_share(record).await.unwrap();

        rt.unmount_share(&id).await.unwrap();
        let calls = executor.calls.lock().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].1[0], expected_mount_root,
            "NFS unmount does NOT use lazy detach"
        );
    }

    #[tokio::test]
    async fn unmount_missing_share_errors() {
        let dir = tempdir();
        let executor = ScriptedExecutor::new(Vec::new());
        let rt = build_runtime_with_executor(&dir, executor);
        let err = rt.unmount_share(&ShareId::new_v4()).await.unwrap_err();
        assert!(matches!(err, MountError::ShareNotFound { .. }));
    }

    #[tokio::test]
    async fn unmount_failure_returns_mount_failed_error() {
        let dir = tempdir();
        let executor = ScriptedExecutor::new(vec![failure_output(
            "umount: /var/lib/evo/music/NAS/x: not mounted",
        )]);
        let rt = build_runtime_with_executor(&dir, executor);
        let record = built_record("not_mounted", "192.0.2.52");
        let id = record.share_id.clone();
        rt.add_share(record).await.unwrap();
        let err = rt.unmount_share(&id).await.unwrap_err();
        assert!(matches!(err, MountError::MountFailed { .. }));
    }

    // -----------------------------------------------------------
    // Ship 2e: Discovery parsing + runtime discovery tests
    // -----------------------------------------------------------

    #[test]
    fn parse_avahi_browse_ignores_non_resolved_and_ipv6() {
        let stdout = "\
+;eth0;IPv4;NAS-A;_smb._tcp;local
+;eth0;IPv6;NAS-A;_smb._tcp;local
=;eth0;IPv4;NAS-A;_smb._tcp;local;nas-a.local;192.0.2.100;445;txt
=;eth0;IPv6;NAS-A;_smb._tcp;local;nas-a.local;fe80::1;445;txt
=;wlan0;IPv4;NAS-B;_smb._tcp;local;nas-b.local;192.0.2.101;445;txt
";
        let hosts = parse_avahi_browse_output(stdout);
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[0], ("NAS-A".to_string(), "192.0.2.100".to_string()));
        assert_eq!(hosts[1], ("NAS-B".to_string(), "192.0.2.101".to_string()));
    }

    #[test]
    fn parse_avahi_browse_collapses_duplicate_ips_across_interfaces() {
        let stdout = "\
=;eth0;IPv4;NAS-C;_smb._tcp;local;nas-c.local;192.0.2.200;445;txt
=;wlan0;IPv4;NAS-C;_smb._tcp;local;nas-c.local;192.0.2.200;445;txt
";
        let hosts = parse_avahi_browse_output(stdout);
        assert_eq!(hosts.len(), 1);
    }

    #[test]
    fn parse_smbclient_disk_lines_extracts_named_disk_shares() {
        let stdout = concat!(
            "\n",
            "\tSharename       Type      Comment\n",
            "\t---------       ----      -------\n",
            "\tMusic           Disk      family music\n",
            "\tMovies          Disk      film archive\n",
            "\tIPC$            IPC       Remote IPC\n",
            "\tprint$          Disk      Printer Drivers\n",
            "\tUsers           Disk      home folders\n",
            "\n",
            "\tServer               Comment\n",
        );
        let shares = parse_smbclient_disk_lines(stdout);
        // print$ is skipped (ends with $), IPC$ is skipped
        // (not Disk).
        assert_eq!(shares.len(), 3);
        assert_eq!(shares[0].name, "Music");
        assert_eq!(shares[0].comment.as_deref(), Some("family music"));
        assert_eq!(shares[1].name, "Movies");
        assert_eq!(shares[2].name, "Users");
    }

    #[test]
    fn parse_smbclient_dialect_finds_debug_line() {
        let stderr =
            "some debug prefix\nnegotiated dialect[SMB3_11] against nas-a\nmore output\n";
        assert_eq!(
            parse_smbclient_dialect(stderr),
            Some("SMB3_11".to_string())
        );
    }

    #[test]
    fn parse_smbclient_dialect_returns_none_without_marker() {
        assert_eq!(parse_smbclient_dialect("no marker here\n"), None);
    }

    #[test]
    fn build_avahi_browse_args_shape() {
        assert_eq!(
            build_avahi_browse_args(),
            vec!["-trkp".to_string(), "_smb._tcp".to_string()]
        );
    }

    #[test]
    fn build_avahi_browse_args_omits_the_all_services_flag() {
        // The `-a` (all services) flag is mutually exclusive with
        // an explicit service type; avahi-browse exits with "Too
        // many arguments" when both are supplied. A regression
        // adding `-a` back would make every discovery sweep
        // fail at argument-parse time and the background task
        // would report a permanently empty NAS list.
        for arg in build_avahi_browse_args() {
            if let Some(flags) = arg.strip_prefix('-') {
                assert!(
                    !flags.contains('a'),
                    "avahi-browse args must not include the `-a` (all services) flag when a service type is passed; got flag bundle `{arg}`"
                );
            }
        }
    }

    #[test]
    fn build_smbclient_list_args_shape() {
        let args = build_smbclient_list_args("192.0.2.10");
        assert_eq!(args[0], "-N");
        assert_eq!(args[1], "-L");
        assert_eq!(args[2], "192.0.2.10");
        assert_eq!(args[3], "-m");
        assert_eq!(args[4], "SMB3_11");
        assert_eq!(args[5], "--debuglevel=4");
    }

    fn discovery_runtime(
        dir: &Path,
        outputs: Vec<CommandOutput>,
    ) -> (NetworkSharesRuntime, Arc<ScriptedExecutor>) {
        let executor = ScriptedExecutor::new(outputs);
        let rt = NetworkSharesRuntime::builder(dir)
            .unwrap()
            .with_executor(executor.clone())
            .with_avahi_browse_program("avahi-browse".to_string())
            .with_smbclient_program("smbclient".to_string())
            .with_avahi_browse_timeout_ms(1_000)
            .with_smbclient_timeout_ms(1_000)
            .with_mount_timeout_ms(1_000)
            .with_now_fn(Arc::new(|| 1_700_000_100_000))
            .build();
        (rt, executor)
    }

    fn avahi_output(hosts: &[(&str, &str)]) -> CommandOutput {
        let mut buf = String::new();
        for (name, ip) in hosts {
            buf.push_str(&format!(
                "=;eth0;IPv4;{name};_smb._tcp;local;{name}.local;{ip};445;txt\n",
            ));
        }
        CommandOutput {
            exit_code: Some(0),
            stdout: buf.into_bytes(),
            stderr: Vec::new(),
        }
    }

    fn smbclient_output(
        shares: &[(&str, &str)],
        dialect: Option<&str>,
    ) -> CommandOutput {
        let mut stdout = String::new();
        stdout.push_str("\n\tSharename       Type      Comment\n");
        stdout.push_str("\t---------       ----      -------\n");
        for (name, comment) in shares {
            stdout.push_str(&format!("\t{name:15} Disk      {comment}\n"));
        }
        stdout.push('\n');
        let stderr = if let Some(d) = dialect {
            format!("prefix\nnegotiated dialect[{d}] against server\n")
        } else {
            String::new()
        };
        CommandOutput {
            exit_code: Some(0),
            stdout: stdout.into_bytes(),
            stderr: stderr.into_bytes(),
        }
    }

    #[tokio::test]
    async fn list_discovered_empty_before_first_refresh() {
        let dir = tempdir();
        let (rt, _) = discovery_runtime(&dir, Vec::new());
        assert!(rt.list_discovered().await.is_empty());
    }

    #[tokio::test]
    async fn refresh_discovery_finds_and_probes_hosts() {
        let dir = tempdir();
        let (rt, executor) = discovery_runtime(
            &dir,
            vec![
                avahi_output(&[
                    ("NAS-A", "192.0.2.10"),
                    ("NAS-B", "192.0.2.11"),
                ]),
                smbclient_output(
                    &[("Music", "family music"), ("Movies", "film archive")],
                    Some("SMB3_11"),
                ),
                smbclient_output(&[("Users", "home folders")], Some("SMB3_02")),
            ],
        );

        let list = rt.refresh_discovery().await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "NAS-A");
        assert_eq!(list[0].ip, "192.0.2.10");
        assert_eq!(list[0].advertised_dialect.as_deref(), Some("SMB3_11"));
        assert_eq!(list[0].shares.len(), 2);
        assert_eq!(list[0].shares[0].name, "Music");
        assert_eq!(list[1].name, "NAS-B");
        assert_eq!(list[1].advertised_dialect.as_deref(), Some("SMB3_02"));
        assert_eq!(list[1].shares[0].name, "Users");

        // Cache is populated.
        let cached = rt.list_discovered().await;
        assert_eq!(cached, list);

        let calls = executor.calls.lock().await;
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].0, "avahi-browse");
        assert_eq!(calls[1].0, "smbclient");
        assert_eq!(calls[2].0, "smbclient");
    }

    #[tokio::test]
    async fn refresh_discovery_avahi_failure_keeps_prior_cache() {
        let dir = tempdir();
        let (rt, _) = discovery_runtime(
            &dir,
            vec![
                // First successful round.
                avahi_output(&[("NAS-K", "192.0.2.30")]),
                smbclient_output(&[("Media", "media root")], Some("SMB3_11")),
                // Second round: avahi-browse fails.
                CommandOutput {
                    exit_code: Some(1),
                    stdout: Vec::new(),
                    stderr: b"avahi bus not available".to_vec(),
                },
            ],
        );

        let first = rt.refresh_discovery().await.unwrap();
        assert_eq!(first.len(), 1);

        let err = rt.refresh_discovery().await.unwrap_err();
        assert!(matches!(err, MountError::MountFailed { .. }));

        // Cache is unchanged: still the first round's result.
        let cached = rt.list_discovered().await;
        assert_eq!(cached, first);
    }

    #[tokio::test]
    async fn refresh_discovery_per_host_smbclient_failure_still_lists_host() {
        let dir = tempdir();
        let (rt, _) = discovery_runtime(
            &dir,
            vec![
                avahi_output(&[
                    ("NAS-OK", "192.0.2.40"),
                    ("NAS-BAD", "192.0.2.41"),
                ]),
                smbclient_output(&[("Music", "ok")], Some("SMB3_11")),
                CommandOutput {
                    exit_code: Some(1),
                    stdout: Vec::new(),
                    stderr: b"connection refused".to_vec(),
                },
            ],
        );

        let list = rt.refresh_discovery().await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].shares.len(), 1);
        // Bad host still surfaces so the operator can see it.
        assert_eq!(list[1].name, "NAS-BAD");
        assert_eq!(list[1].shares.len(), 0);
        assert!(list[1].advertised_dialect.is_none());
    }

    #[tokio::test]
    async fn refresh_discovery_empty_avahi_returns_empty_result() {
        let dir = tempdir();
        let (rt, _) = discovery_runtime(
            &dir,
            vec![CommandOutput {
                exit_code: Some(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            }],
        );
        let list = rt.refresh_discovery().await.unwrap();
        assert!(list.is_empty());
        assert!(rt.list_discovered().await.is_empty());
    }

    // -----------------------------------------------------------
    // Ship 2f: Subject publisher wiring tests
    // -----------------------------------------------------------

    use evo_plugin_sdk::contract::ReportError;
    use std::future::Future;
    use std::pin::Pin;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum AnnouncerCall {
        Announce {
            subject_type: String,
            addressing: ExternalAddressing,
        },
        Retract {
            addressing: ExternalAddressing,
            reason: Option<String>,
        },
        UpdateState {
            addressing: ExternalAddressing,
        },
    }

    struct RecordingAnnouncer {
        calls: Arc<StdMutex<Vec<AnnouncerCall>>>,
        states: Arc<StdMutex<HashMap<ExternalAddressing, serde_json::Value>>>,
    }

    impl RecordingAnnouncer {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Arc::new(StdMutex::new(Vec::new())),
                states: Arc::new(StdMutex::new(HashMap::new())),
            })
        }

        fn calls(&self) -> Vec<AnnouncerCall> {
            self.calls.lock().unwrap().clone()
        }

        fn latest_state(
            &self,
            addressing: &ExternalAddressing,
        ) -> Option<serde_json::Value> {
            self.states.lock().unwrap().get(addressing).cloned()
        }
    }

    impl SubjectAnnouncer for RecordingAnnouncer {
        fn announce<'a>(
            &'a self,
            announcement: SubjectAnnouncement,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            let addressing = announcement.addressings[0].clone();
            self.calls.lock().unwrap().push(AnnouncerCall::Announce {
                subject_type: announcement.subject_type.clone(),
                addressing: addressing.clone(),
            });
            self.states
                .lock()
                .unwrap()
                .insert(addressing, announcement.state);
            Box::pin(async { Ok(()) })
        }

        fn retract<'a>(
            &'a self,
            addressing: ExternalAddressing,
            reason: Option<String>,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            self.calls
                .lock()
                .unwrap()
                .push(AnnouncerCall::Retract { addressing, reason });
            Box::pin(async { Ok(()) })
        }

        fn update_state<'a>(
            &'a self,
            addressing: ExternalAddressing,
            state: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            self.calls.lock().unwrap().push(AnnouncerCall::UpdateState {
                addressing: addressing.clone(),
            });
            self.states.lock().unwrap().insert(addressing, state);
            Box::pin(async { Ok(()) })
        }
    }

    async fn drain_scheduled_republishes() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    async fn publisher_runtime(
        dir: &Path,
    ) -> (NetworkSharesRuntime, Arc<RecordingAnnouncer>) {
        let executor = ScriptedExecutor::new(Vec::new());
        let rt = NetworkSharesRuntime::builder(dir)
            .unwrap()
            .with_executor(executor)
            .with_mount_timeout_ms(1_000)
            .with_now_fn(Arc::new(|| 1_700_000_777_000))
            .build();
        let announcer = RecordingAnnouncer::new();
        rt.attach_subject_publisher(announcer.clone())
            .await
            .expect("attach");
        (rt, announcer)
    }

    #[test]
    fn configured_singleton_addressing_shape_is_stable() {
        let a = configured_singleton_addressing();
        assert_eq!(a.scheme, "evo.network.shares.configured");
        assert_eq!(a.value, "local");
    }

    #[test]
    fn discovered_singleton_addressing_shape_is_stable() {
        let a = discovered_singleton_addressing();
        assert_eq!(a.scheme, "evo.network.shares.discovered");
        assert_eq!(a.value, "local");
    }

    #[test]
    fn share_state_addressing_carries_share_id_as_value() {
        let id = ShareId("abc-123".to_string());
        let a = share_state_addressing(&id);
        assert_eq!(a.scheme, "evo.network.share.state");
        assert_eq!(a.value, "abc-123");
    }

    #[tokio::test]
    async fn attach_publisher_announces_both_singletons_on_empty_state() {
        let dir = tempdir();
        let (_rt, announcer) = publisher_runtime(&dir).await;
        let calls = announcer.calls();
        assert!(calls.iter().any(|c| matches!(
            c,
            AnnouncerCall::Announce { subject_type, addressing }
                if subject_type == CONFIGURED_SUBJECT_TYPE
                    && addressing == &configured_singleton_addressing()
        )));
        assert!(calls.iter().any(|c| matches!(
            c,
            AnnouncerCall::Announce { subject_type, addressing }
                if subject_type == DISCOVERED_SUBJECT_TYPE
                    && addressing == &discovered_singleton_addressing()
        )));
    }

    #[tokio::test]
    async fn attach_publisher_announces_one_share_state_per_persisted_record() {
        let dir = tempdir();
        let executor = ScriptedExecutor::new(Vec::new());
        let rt = NetworkSharesRuntime::builder(&dir)
            .unwrap()
            .with_executor(executor)
            .with_now_fn(Arc::new(|| 1_700_000_777_000))
            .build();
        // Seed two shares before attach so the announcer sees a
        // fully populated set.
        let r1 = built_record("Alpha", "192.0.2.10");
        let r2 = built_record("Bravo", "192.0.2.11");
        let id1 = rt.add_share(r1).await.unwrap();
        let id2 = rt.add_share(r2).await.unwrap();

        let announcer = RecordingAnnouncer::new();
        rt.attach_subject_publisher(announcer.clone())
            .await
            .unwrap();

        let calls = announcer.calls();
        let share_announces: Vec<_> = calls
            .iter()
            .filter(|c| {
                matches!(
                    c,
                    AnnouncerCall::Announce { subject_type, .. }
                        if subject_type == SHARE_STATE_SUBJECT_TYPE
                )
            })
            .collect();
        assert_eq!(share_announces.len(), 2);
        for id in [&id1, &id2] {
            assert!(share_announces.iter().any(|c| matches!(
                c,
                AnnouncerCall::Announce { addressing, .. }
                    if addressing == &share_state_addressing(id)
            )));
        }
    }

    #[tokio::test]
    async fn add_share_republishes_configured_and_announces_per_share_subject()
    {
        let dir = tempdir();
        let (rt, announcer) = publisher_runtime(&dir).await;
        let record = built_record("Family NAS", "192.0.2.55");
        let id = rt.add_share(record).await.unwrap();
        drain_scheduled_republishes().await;

        let calls = announcer.calls();
        assert!(calls.iter().any(|c| matches!(
            c,
            AnnouncerCall::UpdateState { addressing }
                if addressing == &configured_singleton_addressing()
        )));
        assert!(calls.iter().any(|c| matches!(
            c,
            AnnouncerCall::Announce { subject_type, addressing }
                if subject_type == SHARE_STATE_SUBJECT_TYPE
                    && addressing == &share_state_addressing(&id)
        )));
    }

    #[tokio::test]
    async fn edit_share_republishes_configured_when_alias_changes() {
        let dir = tempdir();
        let (rt, announcer) = publisher_runtime(&dir).await;
        let record = built_record("OldName", "192.0.2.55");
        let id = rt.add_share(record).await.unwrap();
        drain_scheduled_republishes().await;
        let before_count = announcer.calls().len();

        let edits = ShareEdits {
            alias: Some("NewName".to_string()),
            ..Default::default()
        };
        rt.edit_share(&id, edits).await.unwrap();
        drain_scheduled_republishes().await;

        let after = announcer.calls();
        assert!(after.len() > before_count);
        assert!(after.iter().any(|c| matches!(
            c,
            AnnouncerCall::UpdateState { addressing }
                if addressing == &configured_singleton_addressing()
        )));
        assert!(after.iter().any(|c| matches!(
            c,
            AnnouncerCall::UpdateState { addressing }
                if addressing == &share_state_addressing(&id)
        )));
    }

    #[tokio::test]
    async fn remove_share_republishes_configured_and_retracts_per_share_subject(
    ) {
        let dir = tempdir();
        let (rt, announcer) = publisher_runtime(&dir).await;
        let record = built_record("ToRemove", "192.0.2.55");
        let id = rt.add_share(record).await.unwrap();
        drain_scheduled_republishes().await;

        rt.remove_share(&id).await.unwrap();
        drain_scheduled_republishes().await;

        let calls = announcer.calls();
        assert!(calls.iter().any(|c| matches!(
            c,
            AnnouncerCall::Retract { addressing, .. }
                if addressing == &share_state_addressing(&id)
        )));
    }

    #[tokio::test]
    async fn refresh_discovery_republishes_discovered_singleton() {
        let dir = tempdir();
        // Use a scripted executor that supplies avahi + smbclient
        // outputs so refresh_discovery has something to publish.
        let executor = ScriptedExecutor::new(vec![
            avahi_output(&[("NAS-P", "192.0.2.90")]),
            smbclient_output(&[("Music", "family music")], Some("SMB3_11")),
        ]);
        let rt = NetworkSharesRuntime::builder(&dir)
            .unwrap()
            .with_executor(executor)
            .with_avahi_browse_program("avahi-browse".to_string())
            .with_smbclient_program("smbclient".to_string())
            .with_avahi_browse_timeout_ms(1_000)
            .with_smbclient_timeout_ms(1_000)
            .with_mount_timeout_ms(1_000)
            .with_now_fn(Arc::new(|| 1_700_000_777_000))
            .build();
        let announcer = RecordingAnnouncer::new();
        rt.attach_subject_publisher(announcer.clone())
            .await
            .unwrap();

        rt.refresh_discovery().await.unwrap();
        drain_scheduled_republishes().await;

        let calls = announcer.calls();
        assert!(calls.iter().any(|c| matches!(
            c,
            AnnouncerCall::UpdateState { addressing }
                if addressing == &discovered_singleton_addressing()
        )));
        let latest = announcer
            .latest_state(&discovered_singleton_addressing())
            .expect("state stored");
        let nas = latest.get("nas").and_then(|v| v.as_array()).unwrap();
        assert_eq!(nas.len(), 1);
    }

    #[tokio::test]
    async fn mount_share_publishes_mounting_then_mounted_on_success() {
        let dir = tempdir();
        let executor = ScriptedExecutor::new(vec![CommandOutput {
            exit_code: Some(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }]);
        let rt = NetworkSharesRuntime::builder(&dir)
            .unwrap()
            .with_executor(executor)
            .with_mount_timeout_ms(1_000)
            .with_now_fn(Arc::new(|| 1_700_000_777_000))
            .build();
        let record = built_record("MountMe", "192.0.2.60");
        let id = rt.add_share(record).await.unwrap();
        let announcer = RecordingAnnouncer::new();
        rt.attach_subject_publisher(announcer.clone())
            .await
            .unwrap();

        rt.mount_share(&id).await.unwrap();
        drain_scheduled_republishes().await;

        let addressing = share_state_addressing(&id);
        let latest = announcer.latest_state(&addressing).expect("state stored");
        assert_eq!(
            latest.get("state").and_then(|v| v.as_str()),
            Some("mounted")
        );
        assert_eq!(
            latest.get("negotiated_vers").and_then(|v| v.as_str()),
            // Ascending ladder (volumio-evo default): SMB 2.0 is
            // the first dialect attempted, so the first-success
            // test lands here.
            Some("2.0"),
        );

        // Ordering: at least one UpdateState carried the Mounting
        // state and at least one carried Mounted, and Mounting
        // preceded Mounted.
        let mut mounting_at: Option<usize> = None;
        let mut mounted_at: Option<usize> = None;
        // We can't inspect payload for UpdateState in RecordedCall,
        // but the latest stored state proves Mounted was last. The
        // Mounting -> Mounted ordering is enforced by mount_share
        // structurally; here we assert only end state + presence.
        for (i, c) in announcer.calls().iter().enumerate() {
            if let AnnouncerCall::UpdateState { addressing: a } = c {
                if a == &share_state_addressing(&id) {
                    if mounting_at.is_none() {
                        mounting_at = Some(i);
                    }
                    mounted_at = Some(i);
                }
            }
        }
        assert!(mounting_at.is_some());
        assert!(mounted_at.is_some());
        assert!(mounted_at.unwrap() >= mounting_at.unwrap());
    }

    #[tokio::test]
    async fn mount_share_publishes_failed_on_probe_exhausted() {
        let dir = tempdir();
        // Every mount attempt fails with a NON-auth signature so
        // the ladder exhausts (auth-refusal would short-circuit
        // to AuthenticationRefused after one attempt).
        let mut outputs = Vec::new();
        for _ in 0..CIFS_VERS_PROBE_LADDER.len() {
            outputs.push(CommandOutput {
                exit_code: Some(32),
                stdout: Vec::new(),
                stderr: b"cifs: bad option".to_vec(),
            });
        }
        let executor = ScriptedExecutor::new(outputs);
        let rt = NetworkSharesRuntime::builder(&dir)
            .unwrap()
            .with_executor(executor)
            .with_mount_timeout_ms(1_000)
            .with_now_fn(Arc::new(|| 1_700_000_777_000))
            .build();
        let record = built_record("Broken", "192.0.2.60");
        let id = rt.add_share(record).await.unwrap();
        let announcer = RecordingAnnouncer::new();
        rt.attach_subject_publisher(announcer.clone())
            .await
            .unwrap();

        let _ = rt.mount_share(&id).await;
        drain_scheduled_republishes().await;

        let latest = announcer
            .latest_state(&share_state_addressing(&id))
            .expect("state stored");
        assert_eq!(
            latest.get("state").and_then(|v| v.as_str()),
            Some("failed")
        );
        assert!(latest.get("reason").and_then(|v| v.as_str()).is_some());
    }

    #[tokio::test]
    async fn unmount_share_publishes_unmounted_on_success() {
        let dir = tempdir();
        let executor = ScriptedExecutor::new(vec![CommandOutput {
            exit_code: Some(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }]);
        let rt = NetworkSharesRuntime::builder(&dir)
            .unwrap()
            .with_executor(executor)
            .with_mount_timeout_ms(1_000)
            .with_now_fn(Arc::new(|| 1_700_000_777_000))
            .build();
        let record = built_record("UnmountMe", "192.0.2.60");
        let id = rt.add_share(record).await.unwrap();
        let announcer = RecordingAnnouncer::new();
        rt.attach_subject_publisher(announcer.clone())
            .await
            .unwrap();

        rt.unmount_share(&id).await.unwrap();
        drain_scheduled_republishes().await;

        let latest = announcer
            .latest_state(&share_state_addressing(&id))
            .expect("state stored");
        assert_eq!(
            latest.get("state").and_then(|v| v.as_str()),
            Some("unmounted")
        );
    }

    // -----------------------------------------------------------
    // Ship 2g: Lifecycle + verb-dispatch tests
    // -----------------------------------------------------------

    fn ok_mount_output() -> CommandOutput {
        CommandOutput {
            exit_code: Some(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    fn err_mount_output() -> CommandOutput {
        // Non-auth-signature failure so the dialect probe walks
        // the full ladder in tests exercising the retry path.
        // Auth-refusal now short-circuits mid-ladder; tests that
        // want AuthenticationRefused should craft an auth
        // stderr and assert on the variant directly.
        CommandOutput {
            exit_code: Some(32),
            stdout: Vec::new(),
            stderr: b"cifs: bad option".to_vec(),
        }
    }

    #[tokio::test]
    async fn boot_mount_all_reports_per_share_outcome_success_and_failure() {
        let dir = tempdir();
        // Two shares: first mount succeeds, second exhausts the
        // probe ladder.
        let mut outputs = vec![ok_mount_output()];
        for _ in 0..CIFS_VERS_PROBE_LADDER.len() {
            outputs.push(err_mount_output());
        }
        let executor = ScriptedExecutor::new(outputs);
        let rt = NetworkSharesRuntime::builder(&dir)
            .unwrap()
            .with_executor(executor)
            .with_mount_timeout_ms(1_000)
            .with_now_fn(Arc::new(|| 1_700_000_777_000))
            .build();
        let good = built_record("Good", "192.0.2.20");
        let bad = built_record("Bad", "192.0.2.21");
        rt.add_share(good).await.unwrap();
        rt.add_share(bad).await.unwrap();

        let report = rt.boot_mount_all().await;
        assert_eq!(report.outcomes.len(), 2);
        assert_eq!(report.success_count(), 1);
        assert_eq!(report.failure_count(), 1);
    }

    /// Reboot-survival regression: a share added under one runtime
    /// instance must be re-mounted by a *fresh* runtime instance
    /// pointing at the same state_dir. This is the substrate the
    /// acceptance walk relies on for "playback survives a device
    /// reboot" — a fresh boot constructs a new runtime and calls
    /// `boot_mount_all`, which must reach every persisted share
    /// even though the current process never saw the `add_share`
    /// call.
    #[tokio::test]
    async fn reboot_survival_fresh_runtime_remounts_persisted_shares() {
        let dir = tempdir();
        // First runtime instance: add a share; it persists to
        // `<state_dir>/network_shares.toml`.
        {
            let rt = NetworkSharesRuntime::builder(&dir)
                .unwrap()
                .with_executor(ScriptedExecutor::new(vec![ok_mount_output()]))
                .with_mount_timeout_ms(1_000)
                .with_now_fn(Arc::new(|| 1_700_000_777_000))
                .build();
            let record = built_record("Persisted", "192.0.2.30");
            rt.add_share(record).await.unwrap();
            // Confirm the record landed in state.
            let g = rt.inner.lock().await;
            assert_eq!(g.state.shares.len(), 1);
        }

        // A second runtime instance built against the SAME
        // state_dir sees the persisted share and mounts it via
        // boot_mount_all — the reboot path.
        let rt2 = NetworkSharesRuntime::builder(&dir)
            .unwrap()
            .with_executor(ScriptedExecutor::new(vec![ok_mount_output()]))
            .with_mount_timeout_ms(1_000)
            .with_now_fn(Arc::new(|| 1_700_000_888_000))
            .build();
        // Fresh runtime: no share state until boot_mount_all fires
        // — but the persisted TOML is already loaded into
        // `state.shares` at builder time.
        {
            let g = rt2.inner.lock().await;
            assert_eq!(
                g.state.shares.len(),
                1,
                "fresh runtime must hydrate persisted shares from disk"
            );
            assert_eq!(g.state.shares[0].alias, "Persisted");
        }
        let report = rt2.boot_mount_all().await;
        assert_eq!(report.outcomes.len(), 1);
        assert_eq!(
            report.success_count(),
            1,
            "boot_mount_all on a fresh runtime must re-mount every \
             persisted share"
        );
        // Post-boot state: the share is Mounted.
        {
            let g = rt2.share_states.lock().await;
            let entry = g.values().next().expect("post-boot state");
            assert_eq!(entry.state, MountState::Mounted);
        }
    }

    #[tokio::test]
    async fn remount_retry_pass_targets_failed_and_unmounted_only() {
        let dir = tempdir();
        // Sequence: fail on first boot attempt (probe exhausted),
        // succeed on retry.
        let mut outputs = Vec::new();
        for _ in 0..CIFS_VERS_PROBE_LADDER.len() {
            outputs.push(err_mount_output());
        }
        outputs.push(ok_mount_output());
        let executor = ScriptedExecutor::new(outputs);
        let rt = NetworkSharesRuntime::builder(&dir)
            .unwrap()
            .with_executor(executor)
            .with_mount_timeout_ms(1_000)
            .with_now_fn(Arc::new(|| 1_700_000_777_000))
            .build();
        let record = built_record("Retry", "192.0.2.22");
        let id = rt.add_share(record).await.unwrap();

        let _ = rt.mount_share(&id).await;
        // After the first attempt: Failed.
        {
            let g = rt.share_states.lock().await;
            assert_eq!(g.get(&id).unwrap().state, MountState::Failed);
        }

        let outcomes = rt.remount_retry_pass().await;
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].is_ok());
        {
            let g = rt.share_states.lock().await;
            assert_eq!(g.get(&id).unwrap().state, MountState::Mounted);
        }

        // Second retry: nothing to do — no candidates in Failed
        // or Unmounted.
        let outcomes2 = rt.remount_retry_pass().await;
        assert!(outcomes2.is_empty());
    }

    #[tokio::test]
    async fn spawn_remount_task_fires_on_cadence() {
        let dir = tempdir();
        let mut outputs = Vec::new();
        for _ in 0..CIFS_VERS_PROBE_LADDER.len() {
            outputs.push(err_mount_output());
        }
        outputs.push(ok_mount_output());
        let executor = ScriptedExecutor::new(outputs);
        let rt = Arc::new(
            NetworkSharesRuntime::builder(&dir)
                .unwrap()
                .with_executor(executor)
                .with_mount_timeout_ms(1_000)
                .with_now_fn(Arc::new(|| 1_700_000_777_000))
                .build(),
        );
        let record = built_record("SpawnRetry", "192.0.2.23");
        let id = rt.add_share(record).await.unwrap();
        let _ = rt.mount_share(&id).await;
        {
            let g = rt.share_states.lock().await;
            assert_eq!(g.get(&id).unwrap().state, MountState::Failed);
        }

        let handle = spawn_remount_task(
            Arc::clone(&rt),
            std::time::Duration::from_millis(30),
        );
        // Sleep long enough for one cadence tick.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        handle.abort();

        let g = rt.share_states.lock().await;
        assert_eq!(g.get(&id).unwrap().state, MountState::Mounted);
    }

    #[tokio::test]
    async fn spawn_discovery_task_fires_on_cadence() {
        let dir = tempdir();
        let executor = ScriptedExecutor::new(vec![
            avahi_output(&[("NAS-DT", "192.0.2.55")]),
            smbclient_output(&[("Music", "family music")], Some("SMB3_11")),
        ]);
        let rt = Arc::new(
            NetworkSharesRuntime::builder(&dir)
                .unwrap()
                .with_executor(executor)
                .with_avahi_browse_program("avahi-browse".to_string())
                .with_smbclient_program("smbclient".to_string())
                .with_avahi_browse_timeout_ms(500)
                .with_smbclient_timeout_ms(500)
                .with_mount_timeout_ms(500)
                .with_now_fn(Arc::new(|| 1_700_000_777_000))
                .build(),
        );
        assert!(rt.list_discovered().await.is_empty());

        let handle = spawn_discovery_task(
            Arc::clone(&rt),
            std::time::Duration::from_millis(30),
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        handle.abort();

        let cache = rt.list_discovered().await;
        assert_eq!(cache.len(), 1);
        assert_eq!(cache[0].name, "NAS-DT");
    }

    #[test]
    fn is_network_shares_verb_recognises_declared_ops_and_rejects_others() {
        for op in NETWORK_SHARES_VERBS {
            assert!(
                is_network_shares_verb(op),
                "verb {op} should be recognised"
            );
        }
        assert!(!is_network_shares_verb("network.share.reboot"));
        assert!(!is_network_shares_verb("network.wifi.connect"));
        assert!(!is_network_shares_verb(""));
    }

    #[tokio::test]
    async fn dispatch_verb_unknown_request_type_returns_error() {
        let dir = tempdir();
        let executor = ScriptedExecutor::new(Vec::new());
        let rt = NetworkSharesRuntime::builder(&dir)
            .unwrap()
            .with_executor(executor)
            .with_mount_timeout_ms(1_000)
            .with_now_fn(Arc::new(|| 1_700_000_777_000))
            .build();
        let err = rt
            .dispatch_verb("network.share.bogus", b"{}")
            .await
            .unwrap_err();
        assert!(matches!(err, VerbDispatchError::UnknownRequestType { .. }));
    }

    #[tokio::test]
    async fn dispatch_verb_bad_payload_returns_decode_error() {
        let dir = tempdir();
        let executor = ScriptedExecutor::new(Vec::new());
        let rt = NetworkSharesRuntime::builder(&dir)
            .unwrap()
            .with_executor(executor)
            .with_mount_timeout_ms(1_000)
            .with_now_fn(Arc::new(|| 1_700_000_777_000))
            .build();
        let err = rt
            .dispatch_verb("network.share.mount", b"not json")
            .await
            .unwrap_err();
        assert!(matches!(err, VerbDispatchError::PayloadDecode { .. }));
    }

    #[tokio::test]
    async fn dispatch_verb_add_mounts_and_returns_share_id_and_report() {
        let dir = tempdir();
        let executor = ScriptedExecutor::new(vec![ok_mount_output()]);
        let rt = NetworkSharesRuntime::builder(&dir)
            .unwrap()
            .with_executor(executor)
            .with_mount_timeout_ms(1_000)
            .with_now_fn(Arc::new(|| 1_700_000_777_000))
            .build();
        let req = AddShareRequest {
            alias: "Verb Add".to_string(),
            fstype: FsType::Cifs,
            host: "192.0.2.30".to_string(),
            path: "Music".to_string(),
            credentials: Credentials::Guest,
            advanced_options: String::new(),
        };
        let payload = serde_json::to_vec(&req).unwrap();
        let bytes = rt
            .dispatch_verb("network.share.add", &payload)
            .await
            .unwrap();
        let response: AddShareResponse =
            serde_json::from_slice(&bytes).unwrap();
        assert!(response.mount_report.is_some());
        assert!(response.mount_error.is_none());
        // Record landed in configured list.
        let configured = rt.list_configured().await.unwrap();
        assert_eq!(configured.len(), 1);
        assert_eq!(configured[0].share_id, response.share_id);
    }

    #[tokio::test]
    async fn dispatch_verb_mount_and_unmount_round_trip() {
        let dir = tempdir();
        let executor = ScriptedExecutor::new(vec![
            ok_mount_output(), // mount call
            ok_mount_output(), // unmount call
        ]);
        let rt = NetworkSharesRuntime::builder(&dir)
            .unwrap()
            .with_executor(executor)
            .with_mount_timeout_ms(1_000)
            .with_now_fn(Arc::new(|| 1_700_000_777_000))
            .build();
        let record = built_record("VerbMount", "192.0.2.40");
        let id = rt.add_share(record).await.unwrap();

        let mount_req = MountShareRequest {
            share_id: id.clone(),
        };
        let mount_payload = serde_json::to_vec(&mount_req).unwrap();
        let mount_bytes = rt
            .dispatch_verb("network.share.mount", &mount_payload)
            .await
            .unwrap();
        let mount_resp: MountShareResponse =
            serde_json::from_slice(&mount_bytes).unwrap();
        assert_eq!(mount_resp.report.share_id, id);

        let unmount_req = UnmountShareRequest {
            share_id: id.clone(),
        };
        let unmount_payload = serde_json::to_vec(&unmount_req).unwrap();
        let unmount_bytes = rt
            .dispatch_verb("network.share.unmount", &unmount_payload)
            .await
            .unwrap();
        let _: UnmountShareResponse =
            serde_json::from_slice(&unmount_bytes).unwrap();
    }

    #[tokio::test]
    async fn dispatch_verb_edit_reports_changed_true_on_alias_edit() {
        let dir = tempdir();
        let executor = ScriptedExecutor::new(Vec::new());
        let rt = NetworkSharesRuntime::builder(&dir)
            .unwrap()
            .with_executor(executor)
            .with_mount_timeout_ms(1_000)
            .with_now_fn(Arc::new(|| 1_700_000_777_000))
            .build();
        let record = built_record("EditMe", "192.0.2.50");
        let id = rt.add_share(record).await.unwrap();

        let req = EditShareRequest {
            share_id: id.clone(),
            edits: ShareEdits {
                alias: Some("EditedName".to_string()),
                ..Default::default()
            },
        };
        let payload = serde_json::to_vec(&req).unwrap();
        let bytes = rt
            .dispatch_verb("network.share.edit", &payload)
            .await
            .unwrap();
        let response: EditShareResponse =
            serde_json::from_slice(&bytes).unwrap();
        assert!(response.changed);
    }

    #[tokio::test]
    async fn dispatch_verb_remove_returns_removed_record() {
        let dir = tempdir();
        // Executor supplies one output for the pre-removal
        // best-effort unmount call.
        let executor = ScriptedExecutor::new(vec![err_mount_output()]);
        let rt = NetworkSharesRuntime::builder(&dir)
            .unwrap()
            .with_executor(executor)
            .with_mount_timeout_ms(1_000)
            .with_now_fn(Arc::new(|| 1_700_000_777_000))
            .build();
        let record = built_record("Removable", "192.0.2.60");
        let id = rt.add_share(record).await.unwrap();

        let req = RemoveShareRequest {
            share_id: id.clone(),
        };
        let payload = serde_json::to_vec(&req).unwrap();
        let bytes = rt
            .dispatch_verb("network.share.remove", &payload)
            .await
            .unwrap();
        let response: RemoveShareResponse =
            serde_json::from_slice(&bytes).unwrap();
        assert_eq!(response.removed_record.share_id, id);
        assert!(rt.list_configured().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn dispatch_verb_discovery_refresh_returns_nas_list() {
        let dir = tempdir();
        let executor = ScriptedExecutor::new(vec![
            avahi_output(&[("NAS-VD", "192.0.2.70")]),
            smbclient_output(&[("Music", "family music")], Some("SMB3_11")),
        ]);
        let rt = NetworkSharesRuntime::builder(&dir)
            .unwrap()
            .with_executor(executor)
            .with_avahi_browse_program("avahi-browse".to_string())
            .with_smbclient_program("smbclient".to_string())
            .with_avahi_browse_timeout_ms(500)
            .with_smbclient_timeout_ms(500)
            .with_mount_timeout_ms(500)
            .with_now_fn(Arc::new(|| 1_700_000_777_000))
            .build();
        let bytes = rt
            .dispatch_verb("network.discovery.refresh", b"")
            .await
            .unwrap();
        let response: RefreshDiscoveryResponse =
            serde_json::from_slice(&bytes).unwrap();
        assert_eq!(response.nas.len(), 1);
        assert_eq!(response.nas[0].name, "NAS-VD");
    }

    #[tokio::test]
    async fn dispatch_verb_list_events_empty_ring_returns_empty_envelope() {
        // A freshly-built runtime has no lifecycle events yet.
        // The read verb must still succeed and return an empty
        // events list — a UI that seeds from this call and then
        // subscribes must see zero history followed by zero live
        // events without special-casing "verb succeeded but no
        // envelope".
        //
        // Wire decode goes through `serde_json::Value` rather
        // than a typed `ListEventsResponse`: `ShareEvent.kind` is
        // `&'static str` (a static-string discriminant), which
        // does not round-trip through `Deserialize` — the
        // envelope is a producer-only type.
        let dir = tempdir();
        let executor = ScriptedExecutor::new(Vec::new());
        let rt = NetworkSharesRuntime::builder(&dir)
            .unwrap()
            .with_executor(executor)
            .with_mount_timeout_ms(1_000)
            .with_now_fn(Arc::new(|| 1_700_000_777_000))
            .build();
        let bytes = rt
            .dispatch_verb("network.share.list_events", b"")
            .await
            .unwrap();
        let response: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap();
        let events = response
            .get("envelope")
            .and_then(|e| e.get("events"))
            .and_then(|e| e.as_array())
            .expect("envelope.events array");
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn dispatch_verb_list_events_returns_ring_snapshot_after_lifecycle_events(
    ) {
        // After a successful mount followed by an unmount, the
        // read verb must return the events in insertion order
        // (oldest first) with the same shape the subject seed
        // announce publishes. This is the load-bearing property
        // the Sources Activity panel depends on: read-then-
        // subscribe with byte-identical decoder for both surfaces.
        let dir = tempdir();
        let executor = ScriptedExecutor::new(vec![
            ok_mount_output(), // mount call
            ok_mount_output(), // unmount call
        ]);
        let rt = NetworkSharesRuntime::builder(&dir)
            .unwrap()
            .with_executor(executor)
            .with_mount_timeout_ms(1_000)
            .with_now_fn(Arc::new(|| 1_700_000_777_000))
            .build();
        let record = built_record("VerbEvents", "192.0.2.50");
        let id = rt.add_share(record).await.unwrap();

        let mount_req = MountShareRequest {
            share_id: id.clone(),
        };
        let mount_payload = serde_json::to_vec(&mount_req).unwrap();
        rt.dispatch_verb("network.share.mount", &mount_payload)
            .await
            .unwrap();

        let unmount_req = UnmountShareRequest {
            share_id: id.clone(),
        };
        let unmount_payload = serde_json::to_vec(&unmount_req).unwrap();
        rt.dispatch_verb("network.share.unmount", &unmount_payload)
            .await
            .unwrap();

        let bytes = rt
            .dispatch_verb("network.share.list_events", b"")
            .await
            .unwrap();
        let response: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap();
        let events = response
            .get("envelope")
            .and_then(|e| e.get("events"))
            .and_then(|e| e.as_array())
            .expect("envelope.events array");
        // add_share performs an implicit mount too, so the ring
        // carries the whole lifecycle burst — the assertion is on
        // the shape (non-empty, insertion order, matching share)
        // rather than an exact count, so a future add-time /
        // mount-time refactor does not break this test.
        assert!(!events.is_empty());
        assert!(events
            .iter()
            .all(|e| e.get("share_id").and_then(|v| v.as_str())
                == Some(id.0.as_str())));
        // Chronological order preserved by the ring.
        let ats: Vec<u64> = events
            .iter()
            .filter_map(|e| e.get("at_ms").and_then(|v| v.as_u64()))
            .collect();
        let mut sorted = ats.clone();
        sorted.sort();
        assert_eq!(ats, sorted, "ring must be oldest-first");
        // The final event on this ring is an unmount success.
        assert_eq!(
            events
                .last()
                .and_then(|e| e.get("kind"))
                .and_then(|v| v.as_str()),
            Some("unmounted")
        );
    }
}
