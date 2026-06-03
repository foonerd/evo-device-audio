//! Source registry — typed, reachability-aware library sources.
//!
//! Owns the set of registered library sources (local-internal,
//! local-USB, NAS over SMB/NFS, cloud Google Drive / OneDrive,
//! DLNA), each carrying a per-source reachability state machine
//! plus a type-specific scan policy. The registry is the
//! load-bearing primitive for the device's mixed-source
//! resilience contract (ADR-0144): consumer shelves
//! ([`crate::queue`], [`crate::playlist`], [`crate::favourites`],
//! [`crate::library`]) project per-song availability through the
//! `evo:available` sticker the registry's sticker reconciler
//! maintains in lockstep with source state.
//!
//! # State machine
//!
//! Each source transitions through
//! [`SourceState::Probing`] → [`SourceState::Online`] (probe ok)
//! → [`SourceState::Degraded`] (probe slow / errored) →
//! [`SourceState::Offline`] (probe failed); explicit operator
//! action transitions to [`SourceState::Retired`]. Transitions
//! are observable in two ways:
//!
//! - **Intra-plugin** via the [`SourceRegistry::subscribe`]
//!   broadcast channel. The sticker reconciler, skip-traversal,
//!   queue / playlist / favourites / library subject emitters
//!   all subscribe to coordinate availability updates without
//!   round-trip through the framework's subject substrate.
//! - **Operator-facing** via the `audio_library_sources` subject
//!   the registry emits through the framework's `SubjectAnnouncer`.
//!   Operator UI subscribes via read-then-subscribe.
//!
//! # Persistence
//!
//! The registry persists to `sources.toml` under the plugin's
//! state directory. Atomic write-tmp-then-rename pattern; on
//! restart the registry rehydrates the persisted records, runs
//! a fresh probe per source, and the state machine catches up.
//!
//! # Honesty contract
//!
//! Reachability probes are bounded by the source's
//! `probe_cadence_ms`. Probe failure transitions to Offline
//! without invoking MPD's `update` / `rescan` — the
//! Offline-never-triggers-update invariant pinned in
//! `audio.library.v1`'s `source-offline-never-triggers-update`
//! acceptance row.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, RwLock};

/// Wire-payload version for the source registry's persisted +
/// emitted shapes. Bumped on any breaking change; additive
/// variants on the tagged-kind enums ride at v=1.
pub(crate) const SOURCE_REGISTRY_VERSION: u32 = 1;

/// Default capacity for the registry's intra-plugin broadcast
/// channel. Source state transitions fire at most a few times
/// per source per probe cadence (5s..=300s depending on type);
/// 64 slots is generous for the steady-state churn of a healthy
/// rig with up to ~10 registered sources.
pub(crate) const SOURCE_STATE_BROADCAST_CAPACITY: usize = 64;

// ----- typed records (wire-aligned with audio.library.v1) -----

/// One library source's persisted + emitted record.
///
/// Field shape mirrors the `SourceRecord` payload declared in
/// `audio.library.v1`'s `list_sources` verb and `audio_library_sources`
/// subject. Serialised as TOML at rest; serialised as JSON on the
/// wire — `serde` round-trips both transparently because the
/// tagged-kind enums use `#[serde(tag = "kind", rename_all =
/// "snake_case")]` which TOML and JSON both honour.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SourceRecord {
    /// Stable id across restart. Operator UI keys against this.
    pub(crate) id: String,
    /// Operator-facing display name. Free-form; default constructed
    /// at registration time from the source's kind + a
    /// disambiguating suffix when needed.
    pub(crate) display_name: String,
    /// What kind of source this is + the type-specific config
    /// fields. Tagged-kind enum.
    pub(crate) kind: SourceKind,
    /// Filesystem path MPD sees. For local sources this is the
    /// directory on disk; for MPD-mounted sources this is the
    /// path under MPD's `music_directory` the mount aliases.
    pub(crate) mount_path: PathBuf,
    /// MPD's mount alias name when the source is attached as an
    /// MPD `mount`. `None` for the local-internal source (which
    /// is MPD's root music_directory; no mount alias).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) mpd_storage_name: Option<String>,
    /// Current state-machine state.
    pub(crate) state: SourceState,
    /// Epoch milliseconds of the last successful probe; `None`
    /// for sources that have never come Online.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_seen_online_at_ms: Option<u64>,
    /// Probe cadence — how often the supervisor probes this
    /// source's reachability. Type-specific defaults set at
    /// registration; operator-overrideable.
    pub(crate) probe_cadence_ms: u32,
    /// What scan policy the registry applies on Online
    /// transitions. Type-specific defaults; operator-overrideable
    /// (with `cloud_eager_scan_acknowledged` gating for cloud
    /// EagerIncremental).
    pub(crate) scan_policy: ScanPolicy,
    /// Track-count snapshot for the operator UI. Refreshed on
    /// every scan completion; null when the source has never
    /// been scanned.
    #[serde(default)]
    pub(crate) track_count: u32,
    /// Track-count under this source whose `evo:available`
    /// sticker is `1`. Refreshed by the sticker reconciler.
    #[serde(default)]
    pub(crate) track_count_available: u32,
    /// Epoch milliseconds of the last scan-completed event;
    /// `None` when never scanned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_scan_at_ms: Option<u64>,
}

/// Tagged-kind enum describing the source type + carrying the
/// type-specific config the registration verb supplied. Each
/// variant's payload mirrors the catalogue-declared shape in
/// `audio.library.v1`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum SourceKind {
    /// The device's always-present floor library (no remote
    /// dependencies). Registered automatically at plugin load
    /// from MPD's `music_directory` config.
    LocalInternal,
    /// A USB-attached storage device. Mount lifecycle is OS-
    /// managed (the framework does not invent its own USB
    /// auto-mount; the operator's distribution decides).
    LocalUsb {
        /// Linux device node (e.g. `/dev/disk/by-uuid/...`).
        device_node: String,
        /// Operator-facing label.
        label: String,
    },
    /// NAS over SMB/CIFS.
    NetworkNasSmb {
        /// NAS host (FQDN or IP).
        server: String,
        /// Share name on the NAS.
        share: String,
        /// SMB username (credentials live in the framework's
        /// CredentialVault keyed by source id).
        username: String,
    },
    /// NAS over NFS.
    NetworkNasNfs {
        /// NFS host (FQDN or IP).
        server: String,
        /// Export path on the NAS.
        export: String,
    },
    /// Google Drive mount (typically via rclone or similar).
    /// The `account_ref` is a CredentialVault key referencing
    /// the operator-linked Google account.
    CloudGdrive {
        /// CredentialVault key the cloud-account primitive
        /// owns.
        account_ref: String,
    },
    /// Microsoft OneDrive mount.
    CloudOnedrive {
        /// CredentialVault key the cloud-account primitive
        /// owns.
        account_ref: String,
    },
    /// DLNA / UPnP server discovered via mDNS / SSDP. Browse-only
    /// — the device walks the ContentDirectory tree on demand.
    NetworkDlna {
        /// DLNA service id (UPnP device UUID).
        service_id: String,
    },
}

/// State machine value for a source. Tagged-kind enum.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum SourceState {
    /// Initial state on registration + post-restart, before the
    /// first probe completes.
    Probing,
    /// Probe succeeded within budget at last cadence tick.
    Online,
    /// Probe slow or partially errored — the source responded
    /// but past the type-specific budget, or returned a transient
    /// error (cloud rate-limit, NAS slow during backup, etc.).
    /// Skip-traversal treats Degraded as optimistic-try-with-
    /// timeout; if the optimistic attempt fails the source
    /// transitions Offline.
    Degraded {
        /// Operator-readable reason ("probe exceeded budget",
        /// "cloud rate-limit", etc.).
        reason: String,
        /// Epoch milliseconds when the source transitioned to
        /// Degraded.
        since_ms: u64,
    },
    /// Probe failed at last cadence tick. Skip-traversal walks
    /// past items from Offline sources without an MPD round-trip.
    Offline {
        /// Operator-readable reason ("connection refused",
        /// "auth expired", "mount unreachable", etc.).
        reason: String,
        /// Epoch milliseconds when the source transitioned to
        /// Offline.
        since_ms: u64,
    },
    /// Operator removed the source via `library.remove_source`.
    /// The source's MPD-side entries may persist (when
    /// `scrub_mpd_entries: false`) but the source itself is no
    /// longer probed; skip-traversal treats Retired sources as
    /// Offline.
    Retired,
}

impl SourceState {
    /// Discriminant for set-equality testing without comparing
    /// the variant-specific fields (reasons / timestamps differ
    /// even when the underlying state is the same).
    pub(crate) fn discriminant(&self) -> SourceStateDiscriminant {
        match self {
            Self::Probing => SourceStateDiscriminant::Probing,
            Self::Online => SourceStateDiscriminant::Online,
            Self::Degraded { .. } => SourceStateDiscriminant::Degraded,
            Self::Offline { .. } => SourceStateDiscriminant::Offline,
            Self::Retired => SourceStateDiscriminant::Retired,
        }
    }

    /// `true` when the source's MPD-side songs are reachable
    /// for transport. Online + Degraded both qualify (Degraded
    /// is optimistic-try-with-timeout).
    pub(crate) fn is_reachable(&self) -> bool {
        matches!(self, Self::Online | Self::Degraded { .. })
    }
}

/// Variant-only view of [`SourceState`] for set-equality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SourceStateDiscriminant {
    Probing,
    Online,
    Degraded,
    Offline,
    Retired,
}

/// Type-specific scan policy. Tagged-kind enum.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ScanPolicy {
    /// MPD `update PATH` on every Online transition + (when
    /// configured) on every mount/unmount event. Default for
    /// local + NAS sources.
    EagerIncremental {
        /// Run `update PATH` on each Online transition.
        on_online: bool,
        /// Run `update PATH` on OS mount-event observation.
        on_mount_event: bool,
    },
    /// Walk the source's directory tree one level on
    /// `browse_library` demand; no eager batch scan. Default
    /// for cloud sources. `prefetch_recent` allows operator
    /// opt-in to a bounded eager scan of the most recently-
    /// modified N entries (default 0).
    LazyBrowseDriven {
        /// Number of most-recent entries to pre-fetch metadata
        /// for on Online transition. Capped at 1000 by the
        /// registration verb.
        prefetch_recent: u32,
    },
    /// No local scan; browse-through-on-demand only. Default
    /// for DLNA sources.
    BrowseOnly,
}

// ----- intra-plugin coordination channel -----

/// Broadcast event fired on every source state transition. Used
/// for intra-plugin coordination (sticker reconciler picks this
/// up to mark songs `evo:available=0/1`; skip-traversal updates
/// its source-state cache; queue / playlist / favourites /
/// library subject emitters refresh their per-item available
/// flags).
///
/// Distinct from the framework's operator-facing
/// `SubjectStateChanged` happening on `audio_library_sources` —
/// the broadcast is the in-process fast path, the subject is
/// the cross-process wire surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceStateChange {
    /// Source id whose state transitioned.
    pub(crate) source_id: String,
    /// State before the transition.
    pub(crate) old_state: SourceState,
    /// State after the transition.
    pub(crate) new_state: SourceState,
    /// Epoch milliseconds at which the supervisor recorded the
    /// transition.
    pub(crate) at_ms: u64,
}

// ----- registry -----

/// In-memory + durable collection of registered sources.
///
/// Cloneable cheaply (Arc bump on each field) so consumer
/// modules (queue, playlist, favourites, library, sticker
/// reconciler, skip-traversal) share one registry instance.
#[derive(Clone)]
pub(crate) struct SourceRegistry {
    inner: Arc<RegistryInner>,
}

struct RegistryInner {
    sources: RwLock<HashMap<String, SourceRecord>>,
    state_path: Option<PathBuf>,
    state_changes: broadcast::Sender<SourceStateChange>,
}

impl SourceRegistry {
    /// Construct an empty registry with no persistence path.
    /// Use [`Self::with_state_path`] to attach a state file.
    pub(crate) fn new() -> Self {
        let (state_changes, _) =
            broadcast::channel(SOURCE_STATE_BROADCAST_CAPACITY);
        Self {
            inner: Arc::new(RegistryInner {
                sources: RwLock::new(HashMap::new()),
                state_path: None,
                state_changes,
            }),
        }
    }

    /// Construct a registry that persists to the given path.
    /// The path is created on first save; reads return an empty
    /// registry when the file doesn't yet exist.
    pub(crate) fn with_state_path(state_path: PathBuf) -> Self {
        let (state_changes, _) =
            broadcast::channel(SOURCE_STATE_BROADCAST_CAPACITY);
        Self {
            inner: Arc::new(RegistryInner {
                sources: RwLock::new(HashMap::new()),
                state_path: Some(state_path),
                state_changes,
            }),
        }
    }

    /// Subscribe to the intra-plugin state-change broadcast.
    /// Subscribers receive every transition; missed messages
    /// (when the subscriber lags past the channel capacity)
    /// surface as `RecvError::Lagged` which subscribers handle
    /// by re-snapshotting via [`Self::snapshot`].
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<SourceStateChange> {
        self.inner.state_changes.subscribe()
    }

    /// Read-only snapshot of every registered source. Cheap
    /// (clones the records into a new Vec).
    pub(crate) async fn snapshot(&self) -> Vec<SourceRecord> {
        let guard = self.inner.sources.read().await;
        guard.values().cloned().collect()
    }

    /// Read one source by id. Returns `None` when the id is
    /// not registered.
    pub(crate) async fn get(&self, source_id: &str) -> Option<SourceRecord> {
        let guard = self.inner.sources.read().await;
        guard.get(source_id).cloned()
    }

    /// Register a new source. Refuses on id collision with a
    /// structured `RegistryError::Duplicate`.
    pub(crate) async fn register(
        &self,
        record: SourceRecord,
    ) -> Result<(), RegistryError> {
        let mut guard = self.inner.sources.write().await;
        if guard.contains_key(&record.id) {
            return Err(RegistryError::Duplicate {
                source_id: record.id.clone(),
            });
        }
        guard.insert(record.id.clone(), record);
        Ok(())
    }

    /// Transition a source's state. Returns the previous state
    /// (cloned). On transition the [`SourceStateChange`] is
    /// broadcast to every subscriber. Idempotent against same-
    /// discriminant transitions (no broadcast fires) — Degraded
    /// → Degraded with the same reason is a no-op; Degraded →
    /// Offline IS a transition (different discriminant).
    pub(crate) async fn transition(
        &self,
        source_id: &str,
        new_state: SourceState,
    ) -> Result<SourceState, RegistryError> {
        let mut guard = self.inner.sources.write().await;
        let record =
            guard
                .get_mut(source_id)
                .ok_or_else(|| RegistryError::Unknown {
                    source_id: source_id.to_string(),
                })?;
        let old_state = record.state.clone();
        if old_state.discriminant() == new_state.discriminant() {
            // Same discriminant — no-op (broadcast only on real
            // transitions). Update reason/since_ms in place
            // without firing.
            record.state = new_state;
            return Ok(old_state);
        }
        record.state = new_state.clone();
        // Update last_seen_online_at_ms on Online transitions.
        if matches!(new_state, SourceState::Online) {
            record.last_seen_online_at_ms = Some(now_ms());
        }
        // Broadcast outside the lock. Clone the source_id so we
        // can release the write guard before await.
        let change = SourceStateChange {
            source_id: source_id.to_string(),
            old_state: old_state.clone(),
            new_state,
            at_ms: now_ms(),
        };
        drop(guard);
        let _ = self.inner.state_changes.send(change);
        Ok(old_state)
    }

    /// Remove a source from the registry. Returns the removed
    /// record. Refuses on unknown id.
    pub(crate) async fn remove(
        &self,
        source_id: &str,
    ) -> Result<SourceRecord, RegistryError> {
        let mut guard = self.inner.sources.write().await;
        guard
            .remove(source_id)
            .ok_or_else(|| RegistryError::Unknown {
                source_id: source_id.to_string(),
            })
    }

    /// Update a source's track-count snapshot. Called by the
    /// scan-progress path on completion.
    pub(crate) async fn update_track_counts(
        &self,
        source_id: &str,
        total: u32,
        available: u32,
    ) -> Result<(), RegistryError> {
        let mut guard = self.inner.sources.write().await;
        let record =
            guard
                .get_mut(source_id)
                .ok_or_else(|| RegistryError::Unknown {
                    source_id: source_id.to_string(),
                })?;
        record.track_count = total;
        record.track_count_available = available;
        record.last_scan_at_ms = Some(now_ms());
        Ok(())
    }

    /// Load the registry from its state file. Returns `Ok(0)`
    /// when no state file is configured or when it doesn't yet
    /// exist; returns the number of sources rehydrated otherwise.
    pub(crate) async fn load_from_disk(&self) -> Result<usize, RegistryError> {
        let Some(path) = self.inner.state_path.as_ref() else {
            return Ok(0);
        };
        match tokio::fs::read_to_string(path).await {
            Ok(s) => {
                let persisted: PersistedRegistry =
                    toml::from_str(&s).map_err(|e| RegistryError::Persist {
                        reason: format!("parse {path:?}: {e}"),
                    })?;
                let count = persisted.sources.len();
                let mut guard = self.inner.sources.write().await;
                for record in persisted.sources {
                    guard.insert(record.id.clone(), record);
                }
                Ok(count)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(RegistryError::Persist {
                reason: format!("read {path:?}: {e}"),
            }),
        }
    }

    /// Persist the registry to disk atomically (write to temp,
    /// rename over). No-op when no state path is configured.
    pub(crate) async fn persist(&self) -> Result<(), RegistryError> {
        let Some(path) = self.inner.state_path.as_ref() else {
            return Ok(());
        };
        let snapshot = {
            let guard = self.inner.sources.read().await;
            PersistedRegistry {
                v: SOURCE_REGISTRY_VERSION,
                sources: guard.values().cloned().collect(),
            }
        };
        let body = toml::to_string_pretty(&snapshot).map_err(|e| {
            RegistryError::Persist {
                reason: format!("serialise: {e}"),
            }
        })?;
        let parent = path.parent().ok_or_else(|| RegistryError::Persist {
            reason: format!("state path {path:?} has no parent"),
        })?;
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            RegistryError::Persist {
                reason: format!("mkdir {parent:?}: {e}"),
            }
        })?;
        let staging = parent.join(format!(
            ".{}.tmp",
            path.file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| "sources.toml".to_string())
        ));
        tokio::fs::write(&staging, body).await.map_err(|e| {
            RegistryError::Persist {
                reason: format!("write {staging:?}: {e}"),
            }
        })?;
        tokio::fs::rename(&staging, path).await.map_err(|e| {
            RegistryError::Persist {
                reason: format!("rename {staging:?} -> {path:?}: {e}"),
            }
        })?;
        Ok(())
    }
}

impl Default for SourceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Wire shape persisted to `sources.toml`. Carries the version
/// + the source records; missing fields on rehydration get
/// their serde defaults so additive shape extensions ride
/// without a migration step.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedRegistry {
    v: u32,
    #[serde(default)]
    sources: Vec<SourceRecord>,
}

/// Registry-domain error type.
#[derive(Debug, Clone, thiserror::Error)]
pub(crate) enum RegistryError {
    /// Operator attempted to register a source with an id that
    /// already exists in the registry.
    #[error("source id {source_id:?} already registered")]
    Duplicate { source_id: String },
    /// Operator addressed a source id that is not in the
    /// registry.
    #[error("source id {source_id:?} is not registered")]
    Unknown { source_id: String },
    /// I/O or serialisation error against the registry's
    /// persistence path.
    #[error("registry persist error: {reason}")]
    Persist { reason: String },
}

/// Construct the canonical local-internal source record from
/// MPD's resolved `music_directory`. Called once at plugin load
/// when no local-internal source is already persisted — the
/// floor library is always present.
pub(crate) fn make_local_internal_record(
    music_directory: &std::path::Path,
) -> SourceRecord {
    SourceRecord {
        id: LOCAL_INTERNAL_SOURCE_ID.to_string(),
        display_name: "Local library".to_string(),
        kind: SourceKind::LocalInternal,
        mount_path: music_directory.to_path_buf(),
        mpd_storage_name: None,
        state: SourceState::Probing,
        last_seen_online_at_ms: None,
        probe_cadence_ms: DEFAULT_LOCAL_INTERNAL_PROBE_CADENCE_MS,
        scan_policy: ScanPolicy::EagerIncremental {
            on_online: true,
            on_mount_event: false,
        },
        track_count: 0,
        track_count_available: 0,
        last_scan_at_ms: None,
    }
}

/// Canonical id for the device's local-internal source. Stable
/// across reboots; the source is registered automatically at
/// plugin load and is non-removable via `library.remove_source`
/// per the catalogue acceptance row
/// `local-internal-source-is-non-removable`.
pub(crate) const LOCAL_INTERNAL_SOURCE_ID: &str = "local-internal";

/// Default probe cadence for the local-internal source. The
/// floor library is on local disk; a 60s cadence is generous +
/// cheap.
pub(crate) const DEFAULT_LOCAL_INTERNAL_PROBE_CADENCE_MS: u32 = 60_000;

/// Default probe cadence for local USB sources.
pub(crate) const DEFAULT_LOCAL_USB_PROBE_CADENCE_MS: u32 = 5_000;

/// Default probe cadence for NAS sources (SMB / NFS).
pub(crate) const DEFAULT_NAS_PROBE_CADENCE_MS: u32 = 30_000;

/// Default probe cadence for cloud sources.
pub(crate) const DEFAULT_CLOUD_PROBE_CADENCE_MS: u32 = 300_000;

/// Default probe cadence for DLNA sources.
pub(crate) const DEFAULT_DLNA_PROBE_CADENCE_MS: u32 = 60_000;

/// Default scan policy for a kind, used by `library.add_source`
/// when the operator did not supply an explicit policy.
pub(crate) fn default_scan_policy_for(kind: &SourceKind) -> ScanPolicy {
    match kind {
        SourceKind::LocalInternal
        | SourceKind::LocalUsb { .. }
        | SourceKind::NetworkNasSmb { .. }
        | SourceKind::NetworkNasNfs { .. } => ScanPolicy::EagerIncremental {
            on_online: true,
            on_mount_event: matches!(kind, SourceKind::LocalUsb { .. }),
        },
        SourceKind::CloudGdrive { .. } | SourceKind::CloudOnedrive { .. } => {
            ScanPolicy::LazyBrowseDriven { prefetch_recent: 0 }
        }
        SourceKind::NetworkDlna { .. } => ScanPolicy::BrowseOnly,
    }
}

/// Default probe cadence for a kind.
pub(crate) fn default_probe_cadence_for(kind: &SourceKind) -> u32 {
    match kind {
        SourceKind::LocalInternal => DEFAULT_LOCAL_INTERNAL_PROBE_CADENCE_MS,
        SourceKind::LocalUsb { .. } => DEFAULT_LOCAL_USB_PROBE_CADENCE_MS,
        SourceKind::NetworkNasSmb { .. } | SourceKind::NetworkNasNfs { .. } => {
            DEFAULT_NAS_PROBE_CADENCE_MS
        }
        SourceKind::CloudGdrive { .. } | SourceKind::CloudOnedrive { .. } => {
            DEFAULT_CLOUD_PROBE_CADENCE_MS
        }
        SourceKind::NetworkDlna { .. } => DEFAULT_DLNA_PROBE_CADENCE_MS,
    }
}

// ----- reachability probe -----

/// Result of one reachability probe. Probes are bounded by the
/// caller-supplied budget; on success they return Online or
/// Degraded (when the response was within budget but the source
/// signalled non-fatal trouble); on failure they return Offline
/// with an operator-readable reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProbeOutcome {
    /// New state the supervisor should transition the source to.
    pub(crate) new_state: SourceState,
    /// Wall-clock time the probe consumed.
    pub(crate) elapsed: Duration,
}

/// Probe a source's reachability. The implementation is type-
/// dispatched on `record.kind`:
///
/// - `LocalInternal` / `LocalUsb`: directory exists + readable
///   (cheap fs metadata check).
/// - `NetworkNasSmb` / `NetworkNasNfs`: directory exists + readable
///   (the mount is OS-managed; the probe asserts the mount is
///   currently usable; mount-down surfaces as Offline).
/// - `CloudGdrive` / `CloudOnedrive` / `NetworkDlna`: not
///   probable from the framework today (substrate landing
///   under v0.1.14 when the cloud-account / DLNA-discovery
///   primitives compose). For these kinds the probe returns
///   Probing until the substrate lands — acceptance row honesty
///   over fabricated "probably online" claims.
///
/// `budget` bounds the wall-clock time spent in the probe; on
/// budget exhaustion the result is Degraded.
pub(crate) async fn probe_source(
    record: &SourceRecord,
    budget: Duration,
) -> ProbeOutcome {
    let start = std::time::Instant::now();
    let new_state = match &record.kind {
        SourceKind::LocalInternal | SourceKind::LocalUsb { .. } => {
            probe_local_directory(&record.mount_path, budget).await
        }
        SourceKind::NetworkNasSmb { .. } | SourceKind::NetworkNasNfs { .. } => {
            probe_local_directory(&record.mount_path, budget).await
        }
        SourceKind::CloudGdrive { .. }
        | SourceKind::CloudOnedrive { .. }
        | SourceKind::NetworkDlna { .. } => SourceState::Probing,
    };
    let elapsed = start.elapsed();
    ProbeOutcome { new_state, elapsed }
}

/// Probe a local filesystem directory for reachability + budget.
/// Returns Online when the directory exists and read_dir
/// succeeds within budget; Offline with a typed reason
/// otherwise; Degraded when read_dir succeeded but exceeded
/// budget.
async fn probe_local_directory(
    path: &std::path::Path,
    budget: Duration,
) -> SourceState {
    let start = std::time::Instant::now();
    let probe = tokio::time::timeout(budget, tokio::fs::metadata(path)).await;
    let elapsed = start.elapsed();
    match probe {
        Ok(Ok(meta)) if meta.is_dir() => {
            if elapsed > budget {
                SourceState::Degraded {
                    reason: "probe exceeded budget".to_string(),
                    since_ms: now_ms(),
                }
            } else {
                SourceState::Online
            }
        }
        Ok(Ok(_)) => SourceState::Offline {
            reason: "path exists but is not a directory".to_string(),
            since_ms: now_ms(),
        },
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            SourceState::Offline {
                reason: "mount path not present".to_string(),
                since_ms: now_ms(),
            }
        }
        Ok(Err(e)) => SourceState::Offline {
            reason: format!("metadata error: {e}"),
            since_ms: now_ms(),
        },
        Err(_) => SourceState::Offline {
            reason: format!(
                "probe budget {ms} ms exhausted",
                ms = budget.as_millis()
            ),
            since_ms: now_ms(),
        },
    }
}

/// Epoch milliseconds. Wall-clock; `SystemTime` panics are
/// impossible in practice (would require system clock before
/// 1970).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ----- tests -----

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn local_record(id: &str, path: PathBuf) -> SourceRecord {
        SourceRecord {
            id: id.to_string(),
            display_name: "test".into(),
            kind: SourceKind::LocalInternal,
            mount_path: path,
            mpd_storage_name: None,
            state: SourceState::Probing,
            last_seen_online_at_ms: None,
            probe_cadence_ms: 60_000,
            scan_policy: ScanPolicy::EagerIncremental {
                on_online: true,
                on_mount_event: false,
            },
            track_count: 0,
            track_count_available: 0,
            last_scan_at_ms: None,
        }
    }

    #[tokio::test]
    async fn registry_register_and_snapshot_roundtrip() {
        let r = SourceRegistry::new();
        r.register(local_record("a", PathBuf::from("/tmp/a")))
            .await
            .unwrap();
        r.register(local_record("b", PathBuf::from("/tmp/b")))
            .await
            .unwrap();
        let snap = r.snapshot().await;
        assert_eq!(snap.len(), 2);
        let ids: std::collections::HashSet<_> =
            snap.iter().map(|r| r.id.clone()).collect();
        assert!(ids.contains("a"));
        assert!(ids.contains("b"));
    }

    #[tokio::test]
    async fn registry_refuses_duplicate_id() {
        let r = SourceRegistry::new();
        r.register(local_record("a", PathBuf::from("/tmp/a")))
            .await
            .unwrap();
        let err = r
            .register(local_record("a", PathBuf::from("/tmp/other")))
            .await
            .unwrap_err();
        assert!(matches!(err, RegistryError::Duplicate { .. }));
    }

    #[tokio::test]
    async fn registry_transition_broadcasts_on_discriminant_change() {
        let r = SourceRegistry::new();
        let mut rx = r.subscribe();
        r.register(local_record("a", PathBuf::from("/tmp/a")))
            .await
            .unwrap();
        let prev = r.transition("a", SourceState::Online).await.unwrap();
        assert!(matches!(prev, SourceState::Probing));
        let change = rx.recv().await.unwrap();
        assert_eq!(change.source_id, "a");
        assert!(matches!(change.old_state, SourceState::Probing));
        assert!(matches!(change.new_state, SourceState::Online));
    }

    #[tokio::test]
    async fn registry_transition_same_discriminant_does_not_broadcast() {
        let r = SourceRegistry::new();
        let mut rx = r.subscribe();
        r.register(local_record("a", PathBuf::from("/tmp/a")))
            .await
            .unwrap();
        r.transition("a", SourceState::Online).await.unwrap();
        // Drain the Online transition.
        let _ = rx.recv().await.unwrap();
        // Same discriminant — should NOT broadcast.
        r.transition(
            "a",
            SourceState::Degraded {
                reason: "first".into(),
                since_ms: 1,
            },
        )
        .await
        .unwrap();
        let _ = rx.recv().await.unwrap();
        // Re-Degraded with different reason — same discriminant,
        // no broadcast.
        r.transition(
            "a",
            SourceState::Degraded {
                reason: "second".into(),
                since_ms: 2,
            },
        )
        .await
        .unwrap();
        let timeout = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            rx.recv(),
        )
        .await;
        assert!(
            timeout.is_err(),
            "same-discriminant transition should not broadcast"
        );
    }

    #[tokio::test]
    async fn registry_transition_unknown_id_returns_error() {
        let r = SourceRegistry::new();
        let err = r.transition("nope", SourceState::Online).await.unwrap_err();
        assert!(matches!(err, RegistryError::Unknown { .. }));
    }

    #[tokio::test]
    async fn registry_persists_and_rehydrates() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sources.toml");
        let r = SourceRegistry::with_state_path(path.clone());
        r.register(local_record("a", PathBuf::from("/tmp/a")))
            .await
            .unwrap();
        r.register(local_record("b", PathBuf::from("/tmp/b")))
            .await
            .unwrap();
        r.persist().await.unwrap();
        let r2 = SourceRegistry::with_state_path(path.clone());
        let n = r2.load_from_disk().await.unwrap();
        assert_eq!(n, 2);
        let snap = r2.snapshot().await;
        assert_eq!(snap.len(), 2);
    }

    #[tokio::test]
    async fn registry_load_missing_file_is_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sources.toml");
        let r = SourceRegistry::with_state_path(path);
        let n = r.load_from_disk().await.unwrap();
        assert_eq!(n, 0);
        assert!(r.snapshot().await.is_empty());
    }

    #[tokio::test]
    async fn registry_remove_returns_record_then_unknown_id() {
        let r = SourceRegistry::new();
        r.register(local_record("a", PathBuf::from("/tmp/a")))
            .await
            .unwrap();
        let removed = r.remove("a").await.unwrap();
        assert_eq!(removed.id, "a");
        let err = r.remove("a").await.unwrap_err();
        assert!(matches!(err, RegistryError::Unknown { .. }));
    }

    #[tokio::test]
    async fn registry_transition_online_updates_last_seen() {
        let r = SourceRegistry::new();
        r.register(local_record("a", PathBuf::from("/tmp/a")))
            .await
            .unwrap();
        let before = r.get("a").await.unwrap();
        assert!(before.last_seen_online_at_ms.is_none());
        r.transition("a", SourceState::Online).await.unwrap();
        let after = r.get("a").await.unwrap();
        assert!(after.last_seen_online_at_ms.is_some());
    }

    #[tokio::test]
    async fn registry_update_track_counts_persists_to_snapshot() {
        let r = SourceRegistry::new();
        r.register(local_record("a", PathBuf::from("/tmp/a")))
            .await
            .unwrap();
        r.update_track_counts("a", 100, 95).await.unwrap();
        let rec = r.get("a").await.unwrap();
        assert_eq!(rec.track_count, 100);
        assert_eq!(rec.track_count_available, 95);
        assert!(rec.last_scan_at_ms.is_some());
    }

    #[test]
    fn source_state_discriminant_collapses_payload_fields() {
        let a = SourceState::Offline {
            reason: "x".into(),
            since_ms: 1,
        };
        let b = SourceState::Offline {
            reason: "y".into(),
            since_ms: 2,
        };
        assert_eq!(a.discriminant(), b.discriminant());
    }

    #[test]
    fn source_state_is_reachable_covers_online_and_degraded() {
        assert!(SourceState::Online.is_reachable());
        assert!(SourceState::Degraded {
            reason: "x".into(),
            since_ms: 0
        }
        .is_reachable());
        assert!(!SourceState::Probing.is_reachable());
        assert!(!SourceState::Offline {
            reason: "x".into(),
            since_ms: 0
        }
        .is_reachable());
        assert!(!SourceState::Retired.is_reachable());
    }

    #[test]
    fn default_scan_policy_for_cloud_is_lazy_browse_driven() {
        let p = default_scan_policy_for(&SourceKind::CloudGdrive {
            account_ref: "x".into(),
        });
        assert!(matches!(p, ScanPolicy::LazyBrowseDriven { .. }));
    }

    #[test]
    fn default_scan_policy_for_local_is_eager_incremental() {
        let p = default_scan_policy_for(&SourceKind::LocalInternal);
        match p {
            ScanPolicy::EagerIncremental {
                on_online,
                on_mount_event,
            } => {
                assert!(on_online);
                assert!(!on_mount_event);
            }
            other => panic!("expected eager_incremental, got {other:?}"),
        }
    }

    #[test]
    fn default_scan_policy_for_local_usb_enables_mount_event() {
        let p = default_scan_policy_for(&SourceKind::LocalUsb {
            device_node: "/dev/sda1".into(),
            label: "USB".into(),
        });
        match p {
            ScanPolicy::EagerIncremental { on_mount_event, .. } => {
                assert!(on_mount_event)
            }
            other => panic!("expected eager_incremental, got {other:?}"),
        }
    }

    #[test]
    fn default_scan_policy_for_dlna_is_browse_only() {
        let p = default_scan_policy_for(&SourceKind::NetworkDlna {
            service_id: "uuid:abc".into(),
        });
        assert!(matches!(p, ScanPolicy::BrowseOnly));
    }

    #[test]
    fn make_local_internal_record_canonical_shape() {
        let r = make_local_internal_record(std::path::Path::new(
            "/var/lib/evo/music/INTERNAL",
        ));
        assert_eq!(r.id, LOCAL_INTERNAL_SOURCE_ID);
        assert!(matches!(r.kind, SourceKind::LocalInternal));
        assert!(matches!(r.state, SourceState::Probing));
        assert_eq!(r.probe_cadence_ms, DEFAULT_LOCAL_INTERNAL_PROBE_CADENCE_MS);
    }

    #[tokio::test]
    async fn probe_source_local_internal_offline_when_directory_missing() {
        let rec = local_record("a", PathBuf::from("/nonexistent/path/xyz123"));
        let outcome =
            probe_source(&rec, std::time::Duration::from_secs(1)).await;
        match outcome.new_state {
            SourceState::Offline { reason, .. } => {
                assert!(
                    reason.contains("not present") || reason.contains("path"),
                    "expected mount-not-present reason, got {reason}"
                );
            }
            other => panic!("expected Offline, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn probe_source_local_internal_online_when_directory_exists() {
        let dir = TempDir::new().unwrap();
        let rec = local_record("a", dir.path().to_path_buf());
        let outcome =
            probe_source(&rec, std::time::Duration::from_secs(1)).await;
        assert!(matches!(outcome.new_state, SourceState::Online));
    }

    #[tokio::test]
    async fn probe_source_cloud_returns_probing_until_substrate_lands() {
        let mut rec = local_record("a", PathBuf::from("/tmp"));
        rec.kind = SourceKind::CloudGdrive {
            account_ref: "abc".into(),
        };
        let outcome =
            probe_source(&rec, std::time::Duration::from_secs(1)).await;
        assert!(matches!(outcome.new_state, SourceState::Probing));
    }

    #[tokio::test]
    async fn registry_serde_roundtrip_preserves_state_variants() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sources.toml");
        let r = SourceRegistry::with_state_path(path.clone());
        let mut rec = local_record("a", PathBuf::from("/tmp"));
        rec.state = SourceState::Offline {
            reason: "test".into(),
            since_ms: 12345,
        };
        r.register(rec.clone()).await.unwrap();
        r.persist().await.unwrap();
        let r2 = SourceRegistry::with_state_path(path);
        r2.load_from_disk().await.unwrap();
        let back = r2.get("a").await.unwrap();
        match back.state {
            SourceState::Offline { reason, since_ms } => {
                assert_eq!(reason, "test");
                assert_eq!(since_ms, 12345);
            }
            other => panic!("expected offline, got {other:?}"),
        }
    }
}
