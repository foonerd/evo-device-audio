// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Scan-time artwork content hashes.
//!
//! A list surface paints N tiles. Each used to carry a
//! `cover_url` that the browser resolved with one HTTP request
//! per tile, and each of those requests walked a provider
//! cascade on the operator's clock. An album whose art is on disk
//! needs none of that: the art can be found once, when the
//! library is scanned, and its content hash carried on the
//! envelope. The UI then fetches `/audio/artwork/{hash}` — an
//! immutable, content-addressed URL — and never touches the
//! resolve endpoint.
//!
//! - **Local only.** The pass asks `artwork.local` (verb
//!   `artwork.resolve`, `outbound_network = false`) and nothing
//!   else. An album with no art on disk gets no hash; its tile
//!   keeps its `artwork_url`, which the framework resolves and
//!   enriches in the background.
//! - **Keyed like the browse tiles.** An album is its canonical
//!   folder, the same identity the facet tiles use; a track row
//!   finds its album through `canonical_album_folder`. The
//!   hash is that of the album's representative track at the
//!   `small` size — the size every list surface requests.
//! - **Incremental.** A folder is asked again only when its track
//!   count or representative track changes, and every folder is
//!   re-asked once per plugin load so replaced art does not
//!   outlive a restart.
//! - **Off the operator's clock.** The pass is sequential, paused
//!   between calls, single-flight, and abandoned after repeated
//!   dispatch failures; an answer already stored stays.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use evo_plugin_sdk::contract::shelf_dispatch::ShelfRequestDispatcher;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Shelf and verb the pass asks. Local tier only.
pub(crate) const ARTWORK_SHELF: &str = "artwork.providers";
pub(crate) const ARTWORK_RESOLVE_VERB: &str = "artwork.resolve";
/// The size every list surface requests, and so the size hashed.
pub(crate) const HASH_SIZE: &str = "small";

/// Pause between calls, so the art plugin is never queued behind
/// a run of scan-time work while a paint is waiting on it.
const PASS_PAUSE: Duration = Duration::from_millis(20);
/// Consecutive dispatch failures after which a pass is abandoned.
const ABORT_AFTER_FAILURES: usize = 5;
/// Waits before each retry of a pass that did not settle: the
/// art tier or MPD may simply not be up yet at plugin load.
pub(crate) const RETRY_SCHEDULE: [Duration; 3] = [
    Duration::from_secs(15),
    Duration::from_secs(60),
    Duration::from_secs(180),
];

/// One album as the pass sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PassAlbum {
    pub(crate) folder: String,
    pub(crate) representative_path: String,
    pub(crate) track_count: usize,
}

impl PassAlbum {
    fn signature(&self) -> String {
        format!("{}|{}", self.track_count, self.representative_path)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Entry {
    /// `None`: asked, and the local tier has no art for it.
    hash: Option<String>,
    sig: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    entries: BTreeMap<String, Entry>,
}

/// What a pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct PassReport {
    pub(crate) asked: usize,
    pub(crate) hashed: usize,
    pub(crate) no_art: usize,
    pub(crate) failed: usize,
    pub(crate) abandoned: bool,
}

/// Album folder → local artwork content hash. Cheap to clone;
/// every clone shares one table.
#[derive(Clone, Default)]
pub(crate) struct ArtworkHashIndex {
    state: Arc<Mutex<State>>,
    persist: Option<Arc<PathBuf>>,
    running: Arc<AtomicBool>,
    verified_this_load: Arc<AtomicBool>,
}

impl ArtworkHashIndex {
    /// An index persisted at `path`. A missing or unreadable file
    /// starts empty: the next pass rebuilds it.
    pub(crate) fn open(path: PathBuf) -> Self {
        let state = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<State>(&bytes)
                .unwrap_or_else(|e| {
                    tracing::warn!(
                        path = %path.display(), error = %e,
                        "artwork hash index unreadable; rebuilding"
                    );
                    State::default()
                }),
            Err(_) => State::default(),
        };
        Self {
            state: Arc::new(Mutex::new(state)),
            persist: Some(Arc::new(path)),
            ..Self::default()
        }
    }

    /// The hash for an album folder, when the local tier had art.
    pub(crate) fn hash_for_folder(&self, folder: &str) -> Option<String> {
        lock(&self.state)
            .entries
            .get(folder)
            .and_then(|e| e.hash.clone())
    }

    /// The hash for the album a track belongs to.
    pub(crate) fn hash_for_path(&self, file_path: &str) -> Option<String> {
        let folder =
            evo_device_audio_shared::folder_album::canonical_album_folder(
                file_path,
            );
        if folder.is_empty() {
            return None;
        }
        self.hash_for_folder(&folder)
    }

    /// Claim the single pass slot. `false`: a pass is already
    /// running. A scan that lands meanwhile is not lost: whatever
    /// it changed is picked up by the next scan's pass, and until
    /// then the row simply has no `artwork_hash`.
    pub(crate) fn begin_pass(&self) -> bool {
        self.running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Release the pass slot.
    pub(crate) fn finish_pass(&self) {
        self.running.store(false, Ordering::Release);
    }

    /// Whether the next pass must re-ask every folder: true once
    /// per plugin load.
    pub(crate) fn take_first_pass(&self) -> bool {
        !self.verified_this_load.swap(true, Ordering::AcqRel)
    }

    /// Run a pass and, while it has not settled, run it again.
    ///
    /// A pass settles when the album list is non-empty, the art
    /// tier answered every ask, and the pass was not abandoned.
    /// Anything else — no list yet, an empty list, an abandoned or
    /// partly failed pass — is retried after each wait in
    /// `schedule`, listing the albums afresh through `relist`. The
    /// first attempt uses `first`, listed by the caller on the scan
    /// event's own connection.
    ///
    /// This exists because the first pass of a plugin load runs
    /// while MPD's database and the art plugin may still be coming
    /// up; without a retry a fresh install keeps an empty table
    /// until the next scan.
    pub(crate) async fn run_with_retry<L, Fut>(
        &self,
        first: Option<Vec<PassAlbum>>,
        mut relist: L,
        dispatcher: &dyn ShelfRequestDispatcher,
        mut ask_all: bool,
        schedule: &[Duration],
    ) -> PassReport
    where
        L: FnMut() -> Fut,
        Fut: std::future::Future<Output = Option<Vec<PassAlbum>>>,
    {
        let mut albums = first;
        let mut report = PassReport::default();
        for attempt in 0..=schedule.len() {
            if attempt > 0 {
                tokio::time::sleep(schedule[attempt - 1]).await;
                albums = relist().await;
            }
            match albums.as_deref() {
                Some(list) if !list.is_empty() => {
                    report = self.run_pass(list, dispatcher, ask_all).await;
                    ask_all = false;
                    if report.abandoned {
                        tracing::warn!(
                            attempt,
                            failed = report.failed,
                            "artwork hash pass abandoned: the local art tier \
                             did not answer; will retry"
                        );
                    }
                    if !report.abandoned && report.failed == 0 {
                        return report;
                    }
                }
                Some(_) => report = PassReport::default(),
                None => {
                    tracing::warn!(
                        attempt,
                        "artwork hash pass: could not list albums; will retry"
                    );
                    report = PassReport::default();
                }
            }
        }
        if report.abandoned || report.failed > 0 || albums.is_none() {
            tracing::warn!(
                "artwork hash pass gave up after its retries; rows without a \
                 hash fall back to artwork_url until the next scan"
            );
        }
        report
    }

    /// Ask the local art tier about every album that needs it.
    pub(crate) async fn run_pass(
        &self,
        albums: &[PassAlbum],
        dispatcher: &dyn ShelfRequestDispatcher,
        ask_all: bool,
    ) -> PassReport {
        let mut report = PassReport::default();
        let mut consecutive = 0usize;
        for album in albums {
            let sig = album.signature();
            let known = lock(&self.state)
                .entries
                .get(&album.folder)
                .map(|e| e.sig == sig)
                .unwrap_or(false);
            if known && !ask_all {
                continue;
            }
            report.asked += 1;
            match ask(dispatcher, &album.representative_path).await {
                Ok(hash) => {
                    consecutive = 0;
                    if hash.is_some() {
                        report.hashed += 1;
                    } else {
                        report.no_art += 1;
                    }
                    lock(&self.state)
                        .entries
                        .insert(album.folder.clone(), Entry { hash, sig });
                }
                Err(e) => {
                    report.failed += 1;
                    consecutive += 1;
                    tracing::debug!(
                        folder = %album.folder, error = %e,
                        "artwork hash pass: local art tier did not answer"
                    );
                    if consecutive >= ABORT_AFTER_FAILURES {
                        report.abandoned = true;
                        break;
                    }
                }
            }
            tokio::time::sleep(PASS_PAUSE).await;
        }
        if !report.abandoned {
            let live: std::collections::BTreeSet<&str> =
                albums.iter().map(|a| a.folder.as_str()).collect();
            lock(&self.state)
                .entries
                .retain(|folder, _| live.contains(folder.as_str()));
        }
        self.save();
        report
    }

    fn save(&self) {
        let Some(path) = &self.persist else {
            return;
        };
        let body = match serde_json::to_vec(&*lock(&self.state)) {
            Ok(b) => b,
            Err(_) => return,
        };
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, body)
            .and_then(|_| std::fs::rename(&tmp, path.as_ref()))
            .is_err()
        {
            tracing::warn!(
                path = %path.display(),
                "artwork hash index could not be saved; it will be \
                 rebuilt on the next load"
            );
        }
    }
}

/// `Ok(Some(hash))` art found; `Ok(None)` the local tier has none;
/// `Err` the tier did not answer (nothing is recorded).
async fn ask(
    dispatcher: &dyn ShelfRequestDispatcher,
    representative_path: &str,
) -> Result<Option<String>, String> {
    let request = json!({
        "v": 1,
        "target": {"scheme": "mpd-path", "value": representative_path},
        "size": HASH_SIZE,
    });
    let bytes = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
    let reply = dispatcher
        .dispatch(ARTWORK_SHELF, ARTWORK_RESOLVE_VERB, bytes, None)
        .await
        .map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_slice(&reply).map_err(|e| e.to_string())?;
    if let Some(h) = v.get("content_hash").and_then(Value::as_str) {
        return Ok(Some(h.to_string()));
    }
    match v.get("status").and_then(Value::as_str) {
        Some("not_found") | Some("bad_request") => Ok(None),
        other => Err(format!("art tier answered {other:?}")),
    }
}

/// Put `artwork_hash` on an envelope row when there is one. An
/// album with no local art carries no key at all, so the UI falls
/// through to `artwork_url`.
pub(crate) fn attach(row: &mut Value, hash: Option<String>) {
    if let (Some(h), Some(obj)) = (hash, row.as_object_mut()) {
        obj.insert("artwork_hash".to_string(), Value::String(h));
    }
}

impl ArtworkHashIndex {
    /// Test seam: record an answer without a pass.
    #[cfg(test)]
    pub(crate) fn seed(&self, folder: &str, hash: Option<&str>) {
        lock(&self.state).entries.insert(
            folder.to_string(),
            Entry {
                hash: hash.map(String::from),
                sig: "seed".to_string(),
            },
        );
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_plugin_sdk::contract::shelf_dispatch::ShelfDispatchError;
    use std::future::Future;
    use std::pin::Pin;

    const H1: &str =
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    type Answer =
        Box<dyn Fn(&str) -> Result<Value, ShelfDispatchError> + Send + Sync>;

    /// Answers every ask from `answer(path)`; records what was asked.
    struct Art {
        asked: Mutex<Vec<(String, String, Value)>>,
        answer: Answer,
    }

    impl Art {
        fn new(
            f: impl Fn(&str) -> Result<Value, ShelfDispatchError>
                + Send
                + Sync
                + 'static,
        ) -> Arc<Self> {
            Arc::new(Self {
                asked: Mutex::new(Vec::new()),
                answer: Box::new(f),
            })
        }
        fn count(&self) -> usize {
            lock(&self.asked).len()
        }
    }

    impl ShelfRequestDispatcher for Art {
        fn dispatch<'a>(
            &'a self,
            shelf: &'a str,
            request_type: &'a str,
            payload: Vec<u8>,
            _instance_id: Option<&'a str>,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<Vec<u8>, ShelfDispatchError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                let req: Value = serde_json::from_slice(&payload).unwrap();
                let path = req["target"]["value"].as_str().unwrap().to_string();
                lock(&self.asked).push((
                    shelf.to_string(),
                    request_type.to_string(),
                    req,
                ));
                (self.answer)(&path).map(|v| v.to_string().into_bytes())
            })
        }
    }

    fn hit(h: &str) -> Result<Value, ShelfDispatchError> {
        Ok(json!({"status": "ok", "content_hash": h}))
    }
    fn none() -> Result<Value, ShelfDispatchError> {
        Ok(json!({"status": "not_found"}))
    }
    fn down() -> Result<Value, ShelfDispatchError> {
        Err(ShelfDispatchError::NoPluginOnShelf {
            shelf: "artwork.providers".into(),
        })
    }
    fn album(folder: &str, n: usize) -> PassAlbum {
        PassAlbum {
            folder: folder.into(),
            representative_path: format!("{folder}/01.flac"),
            track_count: n,
        }
    }

    /// The pass reaches the LOCAL art tier and nothing else, for
    /// the representative track, at the size every list requests.
    /// RED if it is pointed at an online verb.
    #[tokio::test]
    async fn the_pass_asks_the_local_tier_only() {
        let art = Art::new(|_| hit(H1));
        let idx = ArtworkHashIndex::default();
        idx.run_pass(&[album("A/One", 3)], art.as_ref(), true).await;
        let asked = lock(&art.asked);
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].0, "artwork.providers");
        assert_eq!(asked[0].1, "artwork.resolve");
        assert_eq!(asked[0].2["target"]["scheme"], "mpd-path");
        assert_eq!(asked[0].2["target"]["value"], "A/One/01.flac");
        assert_eq!(asked[0].2["size"], "small");
    }

    /// A track finds its album's hash through the same folder
    /// identity the facet tiles use, disc folders included.
    #[tokio::test]
    async fn a_track_finds_its_albums_hash() {
        let art = Art::new(|_| hit(H1));
        let idx = ArtworkHashIndex::default();
        idx.run_pass(&[album("A/One", 3)], art.as_ref(), true).await;
        assert_eq!(idx.hash_for_folder("A/One").as_deref(), Some(H1));
        assert_eq!(idx.hash_for_path("A/One/01.flac").as_deref(), Some(H1));
        assert_eq!(
            idx.hash_for_path("A/One/Disc 2/05.flac").as_deref(),
            Some(H1)
        );
        assert_eq!(idx.hash_for_path("A/Other/01.flac"), None);
    }

    /// An unchanged album is not asked again; a changed one is;
    /// the first pass of a load asks all.
    #[tokio::test]
    async fn only_changed_albums_are_asked_again() {
        let art = Art::new(|_| hit(H1));
        let idx = ArtworkHashIndex::default();
        let a = [album("A/One", 3), album("A/Two", 2)];
        idx.run_pass(&a, art.as_ref(), false).await;
        assert_eq!(art.count(), 2);
        idx.run_pass(&a, art.as_ref(), false).await;
        assert_eq!(art.count(), 2, "unchanged: nothing asked");
        idx.run_pass(
            &[album("A/One", 4), album("A/Two", 2)],
            art.as_ref(),
            false,
        )
        .await;
        assert_eq!(art.count(), 3, "only the changed album");
        idx.run_pass(&a, art.as_ref(), true).await;
        assert_eq!(art.count(), 5, "ask_all re-asks everything");
    }

    /// Local tier has no art: no hash, recorded, not asked again.
    #[tokio::test]
    async fn an_album_without_local_art_gets_no_hash_and_is_not_reasked() {
        let art = Art::new(|_| none());
        let idx = ArtworkHashIndex::default();
        let a = [album("A/One", 3)];
        let r = idx.run_pass(&a, art.as_ref(), false).await;
        assert_eq!((r.hashed, r.no_art), (0, 1));
        assert_eq!(idx.hash_for_folder("A/One"), None);
        idx.run_pass(&a, art.as_ref(), false).await;
        assert_eq!(art.count(), 1);
    }

    /// An art tier that does not answer records nothing, is
    /// abandoned after repeated failure, and is asked again next
    /// pass.
    #[tokio::test]
    async fn a_silent_art_tier_records_nothing_and_ends_the_pass() {
        let up = Arc::new(AtomicBool::new(false));
        let up2 = Arc::clone(&up);
        let art = Art::new(move |_| {
            if up2.load(Ordering::SeqCst) {
                hit(H1)
            } else {
                down()
            }
        });
        let idx = ArtworkHashIndex::default();
        let albums: Vec<PassAlbum> =
            (0..9).map(|i| album(&format!("A/{i}"), 1)).collect();
        let r = idx.run_pass(&albums, art.as_ref(), false).await;
        assert!(r.abandoned);
        assert_eq!(r.failed, ABORT_AFTER_FAILURES);
        assert_eq!(art.count(), ABORT_AFTER_FAILURES, "stopped asking");
        assert_eq!(idx.hash_for_folder("A/0"), None);
        up.store(true, Ordering::SeqCst);
        let r = idx.run_pass(&albums, art.as_ref(), false).await;
        assert_eq!(r.hashed, 9, "the failures were retried");
    }

    /// An album that left the library leaves the index — but only
    /// after a pass that finished.
    #[tokio::test]
    async fn a_departed_album_is_pruned() {
        let art = Art::new(|_| hit(H1));
        let idx = ArtworkHashIndex::default();
        idx.run_pass(
            &[album("A/One", 1), album("A/Two", 1)],
            art.as_ref(),
            false,
        )
        .await;
        idx.run_pass(&[album("A/One", 1)], art.as_ref(), false)
            .await;
        assert!(idx.hash_for_folder("A/Two").is_none());
        assert!(idx.hash_for_folder("A/One").is_some());
    }

    /// The table survives a restart.
    #[tokio::test]
    async fn the_index_persists() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("artwork-hashes.json");
        let art = Art::new(|_| hit(H1));
        ArtworkHashIndex::open(path.clone())
            .run_pass(&[album("A/One", 1)], art.as_ref(), false)
            .await;
        let reopened = ArtworkHashIndex::open(path);
        assert_eq!(reopened.hash_for_folder("A/One").as_deref(), Some(H1));
    }

    const FAST: [Duration; 3] = [
        Duration::from_millis(5),
        Duration::from_millis(5),
        Duration::from_millis(5),
    ];

    fn nine() -> Vec<PassAlbum> {
        (0..9).map(|i| album(&format!("A/{i}"), 1)).collect()
    }

    /// The row's residual. A pass abandoned because the art tier is
    /// not up yet recovers on its own: no manual scan. RED if the
    /// retry is removed (the table stays empty).
    #[tokio::test]
    async fn an_abandoned_pass_recovers_without_a_scan() {
        let asks = Arc::new(Mutex::new(0usize));
        let a2 = Arc::clone(&asks);
        let art = Art::new(move |_| {
            let mut n = lock(&a2);
            *n += 1;
            if *n <= ABORT_AFTER_FAILURES {
                down()
            } else {
                hit(H1)
            }
        });
        let idx = ArtworkHashIndex::default();
        let r = idx
            .run_with_retry(
                Some(nine()),
                || async { Some(nine()) },
                art.as_ref(),
                true,
                &FAST,
            )
            .await;
        assert!(!r.abandoned, "the retry settled: {r:?}");
        for i in 0..9 {
            assert_eq!(
                idx.hash_for_folder(&format!("A/{i}")).as_deref(),
                Some(H1),
                "album {i} filled"
            );
        }
    }

    /// No album list yet (MPD not ready) is retried and then filled.
    #[tokio::test]
    async fn an_empty_first_listing_is_filled_by_a_retry() {
        let art = Art::new(|_| hit(H1));
        let idx = ArtworkHashIndex::default();
        let calls = Arc::new(Mutex::new(0usize));
        let c2 = Arc::clone(&calls);
        let r = idx
            .run_with_retry(
                Some(Vec::new()),
                move || {
                    let c = Arc::clone(&c2);
                    async move {
                        *lock(&c) += 1;
                        Some(nine())
                    }
                },
                art.as_ref(),
                true,
                &FAST,
            )
            .await;
        assert_eq!(r.hashed, 9);
        assert_eq!(*lock(&calls), 1, "one re-listing was enough");
    }

    /// A pass that settles is not retried.
    #[tokio::test]
    async fn a_settled_pass_does_not_retry() {
        let art = Art::new(|_| hit(H1));
        let idx = ArtworkHashIndex::default();
        let relisted = Arc::new(AtomicBool::new(false));
        let r2 = Arc::clone(&relisted);
        idx.run_with_retry(
            Some(nine()),
            move || {
                r2.store(true, Ordering::SeqCst);
                async { Some(nine()) }
            },
            art.as_ref(),
            true,
            &FAST,
        )
        .await;
        assert!(!relisted.load(Ordering::SeqCst));
    }

    /// A tier that never answers ends: bounded asks, then give up.
    #[tokio::test]
    async fn a_tier_that_never_answers_is_given_up_on() {
        let art = Art::new(|_| down());
        let idx = ArtworkHashIndex::default();
        let r = idx
            .run_with_retry(
                Some(nine()),
                || async { Some(nine()) },
                art.as_ref(),
                true,
                &FAST,
            )
            .await;
        assert!(r.abandoned);
        assert_eq!(
            art.count(),
            ABORT_AFTER_FAILURES * (FAST.len() + 1),
            "five asks per attempt, four attempts"
        );
        assert_eq!(idx.hash_for_folder("A/0"), None);
    }

    /// One pass at a time.
    #[test]
    fn passes_are_single_flight() {
        let idx = ArtworkHashIndex::default();
        assert!(idx.begin_pass());
        assert!(!idx.begin_pass());
        idx.finish_pass();
        assert!(idx.begin_pass());
    }

    /// The row carries `artwork_hash` when there is one and no key
    /// at all when there is not.
    #[test]
    fn an_absent_hash_leaves_no_key() {
        let mut with = json!({"uri": "a"});
        attach(&mut with, Some(H1.to_string()));
        assert_eq!(with["artwork_hash"], H1);
        let mut without = json!({"uri": "a"});
        attach(&mut without, None);
        assert!(without.get("artwork_hash").is_none());
    }

    /// Every list surface asks the same index. A surface that
    /// stopped doing so would silently fall back to the resolve
    /// path for every row.
    #[test]
    fn every_list_surface_reads_the_index() {
        for (name, src) in [
            ("queue", include_str!("queue.rs")),
            ("favourites", include_str!("favourites.rs")),
            ("playlist", include_str!("playlist.rs")),
            ("library", include_str!("library.rs")),
        ] {
            assert!(
                src.contains("artwork_hashes.hash_for_")
                    || src.contains("hashes.hash_for_"),
                "{name} no longer stamps artwork_hash"
            );
        }
    }
}
