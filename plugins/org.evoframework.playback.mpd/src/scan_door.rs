// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! The one door that starts a source's index.
//!
//! An attached store is indexed by `library.update_source`, the
//! verb an operator Rescan uses, followed by the same scan-progress
//! watcher that verb starts: it shows the index running and, once
//! MPD's job ends, writes the store's track count from MPD. Its
//! [`ScanPolicy`] says when the library starts that index on its
//! own:
//!
//! - `on_online`: when the store comes Online, and once for an
//!   Online store that has never been scanned;
//! - `on_mount_event`: when an OS mount of the store is seen into
//!   Online, even if it was scanned before.
//!
//! Every caller goes through [`start_scan_if_due`]: the
//! network-shares sync, `library.add_source`, and the restart
//! pass after load.
//!
//! No kick leaves before the [`ScanGate`] has settled. At load the
//! fragment worker can restart MPD once or twice after the shelves
//! are up. An `update` sent before those restarts is lost, and the
//! kick-time `last_scan_at_ms` stamp would then hide the loss. The
//! gate opens once the worker's status has stayed unchanged for
//! [`SETTLE_QUIET`] and MPD answers a connect. Until then warm
//! start only records which USB stores came Online through a
//! mount; the one restart pass after settle starts their index and
//! the index of every Online store that was never scanned.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{watch, Mutex};
use tokio::task::JoinHandle;

use crate::library::{
    handle_update_source, LibraryContext, UpdateSourcePayload,
    LIBRARY_PAYLOAD_VERSION,
};
use crate::mpd::{ConnectTimeouts, MpdConnection, MpdEndpoint};
use crate::source_registry::{
    should_start_online_scan, ScanPolicy, SourceKind, SourceRecord, SourceState,
};

const PLUGIN_NAME: &str = "org.evoframework.playback.mpd";

/// How long the fragment worker's status must stay unchanged
/// before the gate counts MPD as settled. The load-time restarts
/// seen in the field land within a second of each other.
pub(crate) const SETTLE_QUIET: Duration = Duration::from_secs(5);

/// The longest the gate waits for that quiet. A worker that keeps
/// cycling must not hold every first index back for good.
pub(crate) const SETTLE_MAX: Duration = Duration::from_secs(60);

/// Pause between MPD connect attempts once the worker is quiet.
const SETTLE_CONNECT_RETRY: Duration = Duration::from_secs(2);

/// What the caller saw that may make an index due.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScanTrigger {
    /// The store went from not Online to Online on this
    /// observation.
    pub(crate) became_online: bool,
    /// An OS mount of the store was seen into Online.
    pub(crate) mount_event: bool,
    /// When the observation was made, in wall-clock ms. A kick
    /// stamped at or after this time has already answered it.
    pub(crate) observed_at_ms: u64,
}

impl ScanTrigger {
    /// An observation made now.
    pub(crate) fn observed_now(became_online: bool, mount_event: bool) -> Self {
        Self {
            became_online,
            mount_event,
            observed_at_ms: now_ms(),
        }
    }
}

/// Whether `record` is due an index for `trigger`.
///
/// Only an Online store with an `EagerIncremental` policy is ever
/// due. It is due when it came Online and `on_online` is set, when
/// a mount was seen and `on_mount_event` is set, or when it is
/// Online, `on_online` is set and it has never been scanned. A
/// store kicked at or after the observation is not due again for
/// it.
pub(crate) fn scan_due(record: &SourceRecord, trigger: ScanTrigger) -> bool {
    let ScanPolicy::EagerIncremental {
        on_online,
        on_mount_event,
    } = &record.scan_policy
    else {
        return false;
    };
    if !matches!(record.state, SourceState::Online) {
        return false;
    }
    if record
        .last_scan_at_ms
        .is_some_and(|at| at >= trigger.observed_at_ms)
    {
        return false;
    }
    (trigger.became_online && *on_online)
        || (trigger.mount_event && *on_mount_event)
        || should_start_online_scan(record)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateState {
    Waiting,
    Settled,
    Stopped,
}

/// Whether the library may start an index yet, and the one turn
/// every kick takes.
///
/// Clones share the gate: the library context, the shares sync
/// and the restart pass all wait on the same one.
#[derive(Clone)]
pub(crate) struct ScanGate {
    state: Arc<watch::Sender<GateState>>,
    mpd: Option<(MpdEndpoint, ConnectTimeouts)>,
    turn: Arc<Mutex<()>>,
    mount_events: Arc<std::sync::Mutex<HashMap<String, u64>>>,
}

impl ScanGate {
    /// A gate that is already open and holds no MPD handles: what a
    /// context has unless the plugin wires its own. Callers that
    /// pass MPD handles to the door may kick through it at once;
    /// `library.add_source` and the restart pass find no handles
    /// and start nothing.
    pub(crate) fn settled() -> Self {
        Self::with_state(GateState::Settled, None)
    }

    /// The plugin's gate: closed until [`spawn_settle`] opens it.
    pub(crate) fn waiting(
        endpoint: MpdEndpoint,
        timeouts: ConnectTimeouts,
    ) -> Self {
        Self::with_state(GateState::Waiting, Some((endpoint, timeouts)))
    }

    fn with_state(
        state: GateState,
        mpd: Option<(MpdEndpoint, ConnectTimeouts)>,
    ) -> Self {
        let (tx, _rx) = watch::channel(state);
        Self {
            state: Arc::new(tx),
            mpd,
            turn: Arc::new(Mutex::new(())),
            mount_events: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// The MPD connection parameters this gate kicks with.
    pub(crate) fn mpd(&self) -> Option<(MpdEndpoint, ConnectTimeouts)> {
        self.mpd.clone()
    }

    /// Let kicks through.
    pub(crate) fn open(&self) {
        self.state.send_if_modified(|s| {
            if *s == GateState::Waiting {
                *s = GateState::Settled;
                true
            } else {
                false
            }
        });
    }

    /// Release every waiter without a kick: the plugin is
    /// unloading.
    pub(crate) fn stop(&self) {
        self.state.send_replace(GateState::Stopped);
    }

    #[cfg(test)]
    pub(crate) fn is_settled(&self) -> bool {
        *self.state.borrow() == GateState::Settled
    }

    /// Wait until the gate opens. False when the plugin stops
    /// first.
    pub(crate) async fn wait_settled(&self) -> bool {
        let mut rx = self.state.subscribe();
        loop {
            match *rx.borrow_and_update() {
                GateState::Settled => return true,
                GateState::Stopped => return false,
                GateState::Waiting => {}
            }
            if rx.changed().await.is_err() {
                return false;
            }
        }
    }

    /// Warm start saw `source_id`, a USB store, come Online through
    /// a mount just now. The restart pass starts its index.
    pub(crate) fn record_mount_event(&self, source_id: &str) {
        self.mount_events
            .lock()
            .expect("scan gate: mount-event lock poisoned")
            .insert(source_id.to_string(), now_ms());
    }

    fn take_mount_events(&self) -> HashMap<String, u64> {
        std::mem::take(
            &mut *self
                .mount_events
                .lock()
                .expect("scan gate: mount-event lock poisoned"),
        )
    }
}

/// Start one index of `source_id` when `trigger` makes it due.
///
/// Waits for the gate first, so nothing kicks before settle. Then,
/// one kick at a time, re-reads the record, decides with
/// [`scan_due`], sends `library.update_source` and stamps
/// `last_scan_at_ms` at the kick, so a later observation does not
/// start a second index while this one walks. Then it starts the
/// scan-progress watcher, as an operator Rescan does, and returns
/// without waiting for it. Fire and warn: whatever asked has
/// already landed. Returns whether it kicked.
pub(crate) async fn start_scan_if_due(
    library: &LibraryContext,
    endpoint: &MpdEndpoint,
    timeouts: ConnectTimeouts,
    source_id: &str,
    trigger: ScanTrigger,
) -> bool {
    let gate = &library.scan_gate;
    if !gate.wait_settled().await {
        return false;
    }
    let _turn = gate.turn.lock().await;
    let Some(record) = library.registry.get(source_id).await else {
        return false;
    };
    if !scan_due(&record, trigger) {
        return false;
    }
    let mut conn =
        match MpdConnection::connect_with_timeouts(endpoint.clone(), timeouts)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    source_id = %source_id,
                    error = %e,
                    "scan door: no MPD connection to start the index; \
                     operator Rescan remains"
                );
                return false;
            }
        };
    let payload = UpdateSourcePayload {
        v: LIBRARY_PAYLOAD_VERSION,
        source_id: source_id.to_string(),
        force_rescan: false,
    };
    if let Err(e) = handle_update_source(library, &mut conn, payload).await {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            source_id = %source_id,
            error = %e,
            "scan door: the index did not start; operator Rescan remains"
        );
        return false;
    }
    // Stamp at the kick, keeping the counts the record has: the
    // index is only queued.
    let _ = library
        .registry
        .update_track_counts(
            source_id,
            record.track_count,
            record.track_count_available,
        )
        .await;
    // The watcher an operator Rescan starts. When MPD's job ends it
    // writes this store's count from what MPD holds under its path
    // and stamps `last_scan_at_ms` again. The turn is not held for
    // it. Only one watcher runs at a time: while another is in
    // flight this one does not start, and the count waits for the
    // store's next index.
    drop(crate::scan_progress::spawn(
        library.clone(),
        endpoint.clone(),
        timeouts,
        source_id.to_string(),
        crate::scan_progress::ScanKind::Update,
    ));
    true
}

/// Hand `library.add_source`'s observation to the door without
/// holding the verb: a spawned kick that waits for the gate.
/// Nothing is spawned when the context has no MPD handles.
pub(crate) fn kick_after_add(
    library: &LibraryContext,
    source_id: &str,
    trigger: ScanTrigger,
) {
    let Some((endpoint, timeouts)) = library.scan_gate.mpd() else {
        return;
    };
    let library = library.clone();
    let source_id = source_id.to_string();
    tokio::spawn(async move {
        start_scan_if_due(&library, &endpoint, timeouts, &source_id, trigger)
            .await;
    });
}

/// Whether warm start's probe of `record` was a USB mount into
/// Online.
pub(crate) fn warm_start_mount_event(
    record: &SourceRecord,
    new_state: &SourceState,
) -> bool {
    matches!(record.kind, SourceKind::LocalUsb { .. })
        && !matches!(record.state, SourceState::Online)
        && matches!(new_state, SourceState::Online)
}

/// The single pass after settle. Starts the index of every Online
/// store that was never scanned, and of every USB store warm start
/// saw come Online through a mount. Each due store is kicked at
/// most once. Returns how many it kicked.
pub(crate) async fn restart_pass(library: &LibraryContext) -> usize {
    let Some((endpoint, timeouts)) = library.scan_gate.mpd() else {
        return 0;
    };
    let mounted = library.scan_gate.take_mount_events();
    let mut kicked = 0;
    for record in library.registry.snapshot().await {
        let trigger = match mounted.get(&record.id) {
            Some(&at) => ScanTrigger {
                became_online: false,
                mount_event: true,
                observed_at_ms: at,
            },
            None => ScanTrigger::observed_now(false, false),
        };
        if start_scan_if_due(library, &endpoint, timeouts, &record.id, trigger)
            .await
        {
            kicked += 1;
        }
    }
    kicked
}

/// The settle task, held so unload can stop it.
pub(crate) struct ScanSettleHandle {
    task: JoinHandle<()>,
    gate: ScanGate,
}

impl ScanSettleHandle {
    /// Release every waiting kick without starting it, then end the
    /// task.
    pub(crate) async fn stop(self) {
        self.gate.stop();
        self.task.abort();
        let _ = self.task.await;
    }
}

/// Open the context's gate once `fragment_status` has stayed
/// unchanged for `quiet` (at most `max`) and MPD answers a connect,
/// then run [`restart_pass`] once.
pub(crate) fn spawn_settle<T: Send + Sync + 'static>(
    library: LibraryContext,
    fragment_status: Option<watch::Receiver<T>>,
    quiet: Duration,
    max: Duration,
) -> ScanSettleHandle {
    let gate = library.scan_gate.clone();
    let task = tokio::spawn(async move {
        wait_for_quiet(fragment_status, quiet, max).await;
        if let Some((endpoint, timeouts)) = library.scan_gate.mpd() {
            while let Err(e) =
                MpdConnection::connect_with_timeouts(endpoint.clone(), timeouts)
                    .await
            {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "scan gate: MPD not answering yet; first indexes wait"
                );
                tokio::time::sleep(SETTLE_CONNECT_RETRY).await;
            }
        }
        library.scan_gate.open();
        tracing::info!(
            plugin = PLUGIN_NAME,
            "scan gate settled; first indexes may start"
        );
        let kicked = restart_pass(&library).await;
        tracing::info!(
            plugin = PLUGIN_NAME,
            kicked,
            "scan door: restart pass done"
        );
    });
    ScanSettleHandle { task, gate }
}

/// Return once `status` has not changed for `quiet`, once its
/// sender is gone, or once `max` has passed. With no status to
/// watch, wait `quiet`.
async fn wait_for_quiet<T>(
    status: Option<watch::Receiver<T>>,
    quiet: Duration,
    max: Duration,
) {
    let Some(mut rx) = status else {
        tokio::time::sleep(quiet).await;
        return;
    };
    let deadline = tokio::time::Instant::now() + max;
    loop {
        let quiet_until = tokio::time::Instant::now() + quiet;
        if quiet_until > deadline {
            tokio::time::sleep_until(deadline).await;
            tracing::warn!(
                plugin = PLUGIN_NAME,
                "scan gate: MPD output still changing at the settle \
                 limit; first indexes start anyway"
            );
            return;
        }
        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            _ = tokio::time::sleep_until(quiet_until) => return,
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use evo_plugin_sdk::contract::SubjectAnnouncer;

    use crate::library::{handle_add_source, AddSourcePayload};
    use crate::playback_supervisor::test_mock::{
        short_timeouts, spawn_mock_mpd, CapturingSubjectAnnouncer,
        ConnBehaviour,
    };
    use crate::source_registry::{default_scan_policy_for, SourceRegistry};

    type Log = Arc<std::sync::Mutex<Vec<String>>>;

    /// One MPD connection that records every command into `log`.
    fn recording(log: &Log) -> ConnBehaviour {
        ConnBehaviour::StoredPlaylists {
            commands: Arc::clone(log),
            playlists: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            library: Vec::new(),
            queue: Vec::new(),
        }
    }

    fn updates(log: &Log) -> Vec<String> {
        log.lock()
            .unwrap()
            .iter()
            .filter(|c| c.starts_with("update"))
            .cloned()
            .collect()
    }

    /// Wait for `want` updates, then a grace period that would
    /// catch any extra one, and return the count.
    async fn updates_after(log: &Log, want: usize) -> usize {
        for _ in 0..200 {
            if updates(log).len() >= want {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        updates(log).len()
    }

    fn library_with(music: &Path, gate: ScanGate) -> LibraryContext {
        LibraryContext::new(
            music.to_path_buf(),
            SourceRegistry::new(),
            Arc::new(CapturingSubjectAnnouncer::default())
                as Arc<dyn SubjectAnnouncer>,
            None,
        )
        .with_scan_gate(gate)
    }

    fn record(
        id: &str,
        kind: SourceKind,
        mount_path: PathBuf,
        state: SourceState,
        last_scan_at_ms: Option<u64>,
    ) -> SourceRecord {
        SourceRecord {
            id: id.to_string(),
            display_name: id.to_string(),
            scan_policy: default_scan_policy_for(&kind),
            kind,
            mount_path,
            mpd_storage_name: None,
            state,
            last_seen_online_at_ms: None,
            probe_cadence_ms: 60_000,
            track_count: 0,
            track_count_available: 0,
            last_scan_at_ms,
        }
    }

    fn usb_kind() -> SourceKind {
        SourceKind::LocalUsb {
            device_node: "/dev/disk/by-uuid/test".to_string(),
            label: "STICK".to_string(),
        }
    }

    fn nas_kind() -> SourceKind {
        SourceKind::NetworkNasNfs {
            server: "nas.local".to_string(),
            export: "/music".to_string(),
        }
    }

    fn usb(
        id: &str,
        state: SourceState,
        last_scan: Option<u64>,
    ) -> SourceRecord {
        record(id, usb_kind(), PathBuf::from("/m/USB/x"), state, last_scan)
    }

    fn nas(
        id: &str,
        state: SourceState,
        last_scan: Option<u64>,
    ) -> SourceRecord {
        record(id, nas_kind(), PathBuf::from("/m/NAS/x"), state, last_scan)
    }

    fn offline() -> SourceState {
        SourceState::Offline {
            reason: "gone".to_string(),
            since_ms: 1,
        }
    }

    fn at(
        observed_at_ms: u64,
        became_online: bool,
        mount_event: bool,
    ) -> ScanTrigger {
        ScanTrigger {
            became_online,
            mount_event,
            observed_at_ms,
        }
    }

    #[test]
    fn scan_due_follows_the_policy() {
        let t = 10_000;
        // A USB store that was never scanned is due on any look.
        assert!(scan_due(
            &usb("u", SourceState::Online, None),
            at(t, false, false)
        ));
        // Already scanned, Online, no mount: nothing.
        assert!(!scan_due(
            &usb("u", SourceState::Online, Some(5_000)),
            at(t, false, false)
        ));
        // Already scanned, but a mount brought it Online: due.
        assert!(scan_due(
            &usb("u", SourceState::Online, Some(5_000)),
            at(t, false, true)
        ));
        // A kick at or after the mount already answered it.
        assert!(!scan_due(
            &usb("u", SourceState::Online, Some(t)),
            at(t, false, true)
        ));
        assert!(!scan_due(
            &usb("u", SourceState::Online, Some(t + 1)),
            at(t, true, true)
        ));
        // Not Online: never.
        assert!(!scan_due(&usb("u", offline(), None), at(t, true, true)));
        assert!(!scan_due(
            &usb("u", SourceState::Probing, None),
            at(t, true, true)
        ));
        // A NAS coming Online is due, as before.
        assert!(scan_due(
            &nas("n", SourceState::Online, Some(5_000)),
            at(t, true, false)
        ));
        assert!(scan_due(
            &nas("n", SourceState::Online, None),
            at(t, false, false)
        ));
        // A NAS has no mount-event index: no second scan.
        assert!(!scan_due(
            &nas("n", SourceState::Online, Some(5_000)),
            at(t, false, true)
        ));
        // A browse-only store is never indexed.
        let mut browse = usb("b", SourceState::Online, None);
        browse.scan_policy = ScanPolicy::BrowseOnly;
        assert!(!scan_due(&browse, at(t, true, true)));
    }

    #[test]
    fn warm_start_records_only_a_usb_mount_into_online() {
        assert!(warm_start_mount_event(
            &usb("u", offline(), Some(1)),
            &SourceState::Online
        ));
        assert!(warm_start_mount_event(
            &usb("u", SourceState::Probing, Some(1)),
            &SourceState::Online
        ));
        // Online across the restart: no mount transition.
        assert!(!warm_start_mount_event(
            &usb("u", SourceState::Online, Some(1)),
            &SourceState::Online
        ));
        assert!(!warm_start_mount_event(
            &usb("u", SourceState::Online, Some(1)),
            &offline()
        ));
        // A NAS coming Online is not a USB mount.
        assert!(!warm_start_mount_event(
            &nas("n", offline(), Some(1)),
            &SourceState::Online
        ));
    }

    fn add_usb(mount_path: PathBuf) -> AddSourcePayload {
        AddSourcePayload {
            v: LIBRARY_PAYLOAD_VERSION,
            display_name: "Stick".to_string(),
            kind: usb_kind(),
            mount_path,
            scan_policy: None,
            probe_cadence_ms: None,
            cloud_eager_scan_acknowledged: None,
        }
    }

    #[tokio::test]
    async fn usb_add_source_starts_exactly_one_index_after_settle() {
        let _watcher = crate::scan_progress::exclusive_watcher().await;
        let music = tempfile::tempdir().unwrap();
        let mount = music.path().join("USB").join("stick");
        std::fs::create_dir_all(&mount).unwrap();
        let log: Log = Arc::default();
        // A second connection is there to catch a second index.
        let (endpoint, _mock) =
            spawn_mock_mpd(vec![recording(&log), recording(&log)]).await;
        let gate = ScanGate::waiting(endpoint, short_timeouts());
        gate.open();
        let library = library_with(music.path(), gate);

        let res = handle_add_source(&library, add_usb(mount)).await.unwrap();

        assert_eq!(
            updates_after(&log, 1).await,
            1,
            "{:?}",
            log.lock().unwrap()
        );
        assert_eq!(updates(&log)[0], "update \"USB/stick\"");
        let rec = library.registry.get(&res.source_id).await.unwrap();
        assert!(rec.last_scan_at_ms.is_some(), "stamped at the kick");
    }

    #[tokio::test]
    async fn nothing_kicks_before_settle() {
        let _watcher = crate::scan_progress::exclusive_watcher().await;
        let music = tempfile::tempdir().unwrap();
        let stick = music.path().join("USB").join("stick");
        let share = music.path().join("NAS").join("share");
        std::fs::create_dir_all(&stick).unwrap();
        std::fs::create_dir_all(&share).unwrap();
        let log: Log = Arc::default();
        let (endpoint, _mock) = spawn_mock_mpd(vec![
            recording(&log),
            recording(&log),
            recording(&log),
        ])
        .await;
        let gate = ScanGate::waiting(endpoint.clone(), short_timeouts());
        let library = library_with(music.path(), gate.clone());

        // add_source, a share admission and a warm-start mount,
        // all before settle.
        let added = handle_add_source(&library, add_usb(stick)).await.unwrap();
        library
            .registry
            .upsert(record(
                "nas-1",
                nas_kind(),
                share,
                SourceState::Online,
                None,
            ))
            .await;
        let share_kick = {
            let library = library.clone();
            let endpoint = endpoint.clone();
            tokio::spawn(async move {
                start_scan_if_due(
                    &library,
                    &endpoint,
                    short_timeouts(),
                    "nas-1",
                    ScanTrigger::observed_now(true, false),
                )
                .await
            })
        };
        gate.record_mount_event(&added.source_id);

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!gate.is_settled());
        assert!(
            log.lock().unwrap().is_empty(),
            "no MPD command before settle: {:?}",
            log.lock().unwrap()
        );
        assert!(!share_kick.is_finished(), "the share kick waits for settle");
        let usb = library.registry.get(&added.source_id).await.unwrap();
        assert_eq!(usb.last_scan_at_ms, None, "no stamp before settle");

        gate.open();
        assert!(share_kick.await.unwrap());
        // One for the stick, one for the share; the recorded mount
        // waits for the restart pass, which this test does not run.
        assert_eq!(
            updates_after(&log, 2).await,
            2,
            "{:?}",
            log.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn restart_pass_kicks_due_rows_exactly_once() {
        let _watcher = crate::scan_progress::exclusive_watcher().await;
        let music = tempfile::tempdir().unwrap();
        let dir = |leaf: &str| {
            let p = music.path().join("USB").join(leaf);
            std::fs::create_dir_all(&p).unwrap();
            p
        };
        let log: Log = Arc::default();
        // Two due rows; the extra connections catch any more.
        let (endpoint, _mock) = spawn_mock_mpd(vec![
            recording(&log),
            recording(&log),
            recording(&log),
            recording(&log),
        ])
        .await;
        let gate = ScanGate::waiting(endpoint, short_timeouts());
        let library = library_with(music.path(), gate.clone());
        let long_ago = Some(1_000);
        for r in [
            // Never scanned, Online: due.
            record(
                "fresh",
                usb_kind(),
                dir("fresh"),
                SourceState::Online,
                None,
            ),
            // Scanned, but warm start saw it mount into Online: due.
            record(
                "mounted",
                usb_kind(),
                dir("mounted"),
                SourceState::Online,
                long_ago,
            ),
            // Scanned, Online across the restart: not due.
            record(
                "stayed",
                usb_kind(),
                dir("stayed"),
                SourceState::Online,
                long_ago,
            ),
            // A scanned share: not due.
            record(
                "nas-1",
                nas_kind(),
                dir("nas"),
                SourceState::Online,
                long_ago,
            ),
            // Never scanned but Offline: not due.
            record("gone", usb_kind(), dir("gone"), offline(), None),
        ] {
            library.registry.upsert(r).await;
        }
        gate.record_mount_event("mounted");
        gate.open();

        assert_eq!(restart_pass(&library).await, 2);
        assert_eq!(updates_after(&log, 2).await, 2);
        let mut sent = updates(&log);
        sent.sort();
        assert_eq!(sent, ["update \"USB/fresh\"", "update \"USB/mounted\""]);

        // The pass is one pass: running it again kicks nothing.
        assert_eq!(restart_pass(&library).await, 0);
        assert_eq!(updates_after(&log, 2).await, 2);
    }

    #[tokio::test]
    async fn the_gate_settles_after_the_quiet_window_then_runs_the_pass_once() {
        let _watcher = crate::scan_progress::exclusive_watcher().await;
        let music = tempfile::tempdir().unwrap();
        let stick = music.path().join("USB").join("stick");
        std::fs::create_dir_all(&stick).unwrap();
        let log: Log = Arc::default();
        // The first connection is the settle check.
        let (endpoint, _mock) = spawn_mock_mpd(vec![
            ConnBehaviour::Standard,
            recording(&log),
            recording(&log),
        ])
        .await;
        let gate = ScanGate::waiting(endpoint, short_timeouts());
        let library = library_with(music.path(), gate.clone());
        library
            .registry
            .upsert(record(
                "fresh",
                usb_kind(),
                stick,
                SourceState::Online,
                None,
            ))
            .await;

        let (status_tx, status_rx) = watch::channel(0u32);
        let quiet = Duration::from_millis(300);
        let settle = spawn_settle(
            library.clone(),
            Some(status_rx),
            quiet,
            Duration::from_secs(10),
        );

        // MPD restarting under the worker: three status changes.
        for n in 1..=3 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            status_tx.send_replace(n);
        }
        let last_change = tokio::time::Instant::now();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!gate.is_settled(), "still inside the quiet window");
        assert!(updates(&log).is_empty(), "nothing kicks before settle");

        for _ in 0..200 {
            if gate.is_settled() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(gate.is_settled());
        assert!(last_change.elapsed() >= quiet);
        assert_eq!(
            updates_after(&log, 1).await,
            1,
            "the pass kicks the row once"
        );
        settle.stop().await;
        assert_eq!(updates(&log).len(), 1);
    }

    #[tokio::test]
    async fn stopping_the_gate_releases_a_waiting_kick_without_an_index() {
        let music = tempfile::tempdir().unwrap();
        let stick = music.path().join("USB").join("stick");
        std::fs::create_dir_all(&stick).unwrap();
        let log: Log = Arc::default();
        let (endpoint, _mock) = spawn_mock_mpd(vec![recording(&log)]).await;
        let gate = ScanGate::waiting(endpoint.clone(), short_timeouts());
        let library = library_with(music.path(), gate.clone());
        library
            .registry
            .upsert(record(
                "fresh",
                usb_kind(),
                stick,
                SourceState::Online,
                None,
            ))
            .await;
        let settle = spawn_settle::<u32>(
            library.clone(),
            None,
            Duration::from_secs(30),
            Duration::from_secs(60),
        );
        let kick = {
            let library = library.clone();
            tokio::spawn(async move {
                start_scan_if_due(
                    &library,
                    &endpoint,
                    short_timeouts(),
                    "fresh",
                    ScanTrigger::observed_now(true, true),
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        settle.stop().await;
        let kicked = tokio::time::timeout(Duration::from_secs(1), kick)
            .await
            .expect("released at once")
            .unwrap();
        assert!(!kicked);
        gate.open();
        assert!(!gate.is_settled(), "a stopped gate stays shut");
        assert!(log.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unreachable_mpd_warns_and_add_source_still_lands() {
        let music = tempfile::tempdir().unwrap();
        let stick = music.path().join("USB").join("stick");
        std::fs::create_dir_all(&stick).unwrap();
        // No behaviours: every connection is dropped on accept.
        let (endpoint, _mock) = spawn_mock_mpd(Vec::new()).await;
        let gate = ScanGate::waiting(endpoint.clone(), short_timeouts());
        gate.open();
        let library = library_with(music.path(), gate);

        let res = handle_add_source(&library, add_usb(stick))
            .await
            .expect("add_source does not wait on the index");
        let kicked = start_scan_if_due(
            &library,
            &endpoint,
            short_timeouts(),
            &res.source_id,
            ScanTrigger::observed_now(true, true),
        )
        .await;
        assert!(!kicked);
        // Give the spawned add kick time to fail too.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let rec = library.registry.get(&res.source_id).await.unwrap();
        assert_eq!(rec.last_scan_at_ms, None, "no stamp without a kick");
    }

    #[tokio::test]
    async fn a_usb_door_index_settles_the_count_mpd_holds_for_its_path() {
        // The door starts the watcher an operator Rescan starts.
        // When MPD's job ends the row carries MPD's count for its
        // path, and last_scan_at_ms is stamped again.
        let _watcher = crate::scan_progress::exclusive_watcher().await;
        let music = tempfile::tempdir().unwrap();
        let stick = music.path().join("USB").join("stick");
        std::fs::create_dir_all(&stick).unwrap();
        let held: Vec<String> = ["a.flac", "b.flac", "c.flac"]
            .iter()
            .map(|f| format!("USB/stick/{f}"))
            .collect();
        let job_done = || ConnBehaviour::PrunesAfterUpdate {
            internal: Vec::new(),
            usb: held.clone(),
            in_flight_polls: 0,
        };
        let log: Log = Arc::default();
        // The kick, then the watcher's poll and its count.
        let (endpoint, _mock) =
            spawn_mock_mpd(vec![recording(&log), job_done(), job_done()]).await;
        let gate = ScanGate::waiting(endpoint, short_timeouts());
        gate.open();
        let library = library_with(music.path(), gate);

        let res = handle_add_source(&library, add_usb(stick)).await.unwrap();

        let mut settled = None;
        for _ in 0..500 {
            let rec = library.registry.get(&res.source_id).await.unwrap();
            if rec.track_count == 3 {
                settled = Some(rec);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let rec = settled.expect("the watcher settles the count");
        assert_eq!(rec.track_count_available, 3);
        assert!(rec.last_scan_at_ms.is_some());
        assert_eq!(updates(&log), ["update \"USB/stick\""], "one index");
    }
}
