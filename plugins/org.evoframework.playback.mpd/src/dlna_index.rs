// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! A searchable copy of what each online DLNA source holds.
//!
//! A ContentDirectory has no search this plugin can use: Plex answers
//! `Search` with error 708 and an empty `SearchCaps`. Searching meant
//! walking the tree inside the query, a few dozen browses on a
//! library of thousands, and it never reached a track. The search
//! reads this index instead.
//!
//! One background task walks each online DLNA source, one container
//! at a time with a pause between browses, and keeps what it saw: the
//! rendered item entries, exactly as a browse answers them. The walk
//! goes music first, never enters video or photo trees or the
//! pseudo-containers a server lists beside its library, skips a
//! container that will not browse, and browses a container once
//! however many routes lead to it. The index is written to the
//! plugin's state directory and read back at load, so a restart is
//! warm.
//!
//! The query never waits on a walk. A source with no index yet is
//! named in `stale_sources` and a walk is asked for; a source whose
//! last walk was partial is searched and named stale as well.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify};

use crate::library::{browse_dlna_via_source_dlna, LibraryContext, VerbError};
use crate::source_registry::{SourceKind, SourceRecord, SourceState};

const PLUGIN_NAME: &str = "org.evoframework.playback.mpd";

/// File-format version of the kept index.
pub(crate) const INDEX_VERSION: u32 = 1;

/// The pause between two browses of one walk. The walk is one
/// browse at a time, and this keeps a media server from being asked
/// back to back for the whole of its library.
pub(crate) const INDEX_BROWSE_GAP: Duration = Duration::from_millis(150);

/// The most browses one walk makes. A tree bigger than this is
/// indexed as far as the walk got, and the source stays named stale.
pub(crate) const INDEX_MAX_BROWSES: usize = 3_000;

/// How long an index is used before the source is walked again.
pub(crate) const INDEX_REFRESH: Duration = Duration::from_secs(6 * 3600);

/// How long after a walk that produced nothing the next is tried.
pub(crate) const INDEX_RETRY: Duration = Duration::from_secs(300);

/// How often the worker looks for a source that is due.
pub(crate) const INDEX_TICK: Duration = Duration::from_secs(20);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// What one walk of one source saw.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SourceIndex {
    /// Item entries as the browse rendered them: `name`, `title`,
    /// `artist`, `album`, the stable `dlna:` `uri`.
    pub(crate) items: Vec<serde_json::Value>,
    /// Wall clock at which the walk finished.
    pub(crate) built_at_ms: u64,
    /// Whether the walk went everywhere it meant to: no browse
    /// budget run out, no container skipped.
    pub(crate) complete: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct Persisted {
    v: u32,
    sources: HashMap<String, SourceIndex>,
}

#[derive(Default)]
struct Inner {
    sources: HashMap<String, SourceIndex>,
    walking: HashSet<String>,
    /// When a walk of the source last started, for the retry pause.
    last_attempt_ms: HashMap<String, u64>,
    path: Option<PathBuf>,
}

/// What a search finds in one source's index.
pub(crate) struct IndexAnswer {
    pub(crate) hits: Vec<serde_json::Value>,
    /// False when the index is from a partial walk.
    pub(crate) complete: bool,
}

/// The indexes and where they are kept. Cloneable; the library
/// context holds one and the worker and the search share it.
#[derive(Clone, Default)]
pub(crate) struct DlnaIndex {
    inner: Arc<Mutex<Inner>>,
    wake: Arc<Notify>,
}

impl DlnaIndex {
    /// The indexes at this path, read back if they are there. A file
    /// that cannot be read or parsed is reported and starts empty.
    pub(crate) async fn open(path: PathBuf) -> Self {
        let index = Self {
            inner: Arc::new(Mutex::new(Inner {
                path: Some(path.clone()),
                ..Inner::default()
            })),
            wake: Arc::new(Notify::new()),
        };
        match tokio::fs::read_to_string(&path).await {
            Ok(body) => match serde_json::from_str::<Persisted>(&body) {
                Ok(p) if p.v == INDEX_VERSION => {
                    let mut inner = index.inner.lock().await;
                    tracing::info!(
                        plugin = PLUGIN_NAME,
                        sources = p.sources.len(),
                        "dlna search index read back"
                    );
                    inner.sources = p.sources;
                }
                Ok(p) => tracing::warn!(
                    plugin = PLUGIN_NAME,
                    version = p.v,
                    "dlna search index is from another format version; \
                     starting empty"
                ),
                Err(e) => tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "dlna search index does not parse; starting empty"
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "dlna search index unreadable; starting empty"
            ),
        }
        index
    }

    /// Ask the worker to look for a due source now rather than at
    /// its next tick.
    pub(crate) fn request(&self) {
        self.wake.notify_one();
    }

    /// The items of `source_id` the matcher accepts, or `None` when
    /// the source has no index yet. Never waits on a walk.
    pub(crate) async fn search(
        &self,
        source_id: &str,
        matches: impl Fn(&serde_json::Value) -> bool,
    ) -> Option<IndexAnswer> {
        let inner = self.inner.lock().await;
        let index = inner.sources.get(source_id)?;
        Some(IndexAnswer {
            hits: index.items.iter().filter(|i| matches(i)).cloned().collect(),
            complete: index.complete,
        })
    }

    /// Whether the source should be walked now: not being walked,
    /// and either never indexed (and not tried in the last
    /// [`INDEX_RETRY`]) or indexed longer ago than [`INDEX_REFRESH`].
    pub(crate) async fn due(&self, source_id: &str, now_ms: u64) -> bool {
        let inner = self.inner.lock().await;
        if inner.walking.contains(source_id) {
            return false;
        }
        let tried = inner.last_attempt_ms.get(source_id).copied();
        if tried.is_some_and(|t| {
            now_ms.saturating_sub(t) < INDEX_RETRY.as_millis() as u64
        }) {
            return false;
        }
        match inner.sources.get(source_id) {
            None => true,
            Some(i) => {
                now_ms.saturating_sub(i.built_at_ms)
                    >= INDEX_REFRESH.as_millis() as u64
            }
        }
    }

    async fn begin(&self, source_id: &str, now_ms: u64) -> bool {
        let mut inner = self.inner.lock().await;
        if !inner.walking.insert(source_id.to_string()) {
            return false;
        }
        inner.last_attempt_ms.insert(source_id.to_string(), now_ms);
        true
    }

    async fn finish(&self, source_id: &str, built: Option<SourceIndex>) {
        let mut inner = self.inner.lock().await;
        inner.walking.remove(source_id);
        if let Some(b) = built {
            inner.sources.insert(source_id.to_string(), b);
            if let Err(e) = Self::persist(&inner).await {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "dlna search index could not be written; it is kept in \
                     memory until the next walk"
                );
            }
        }
    }

    /// Put an index in without walking: for tests and for a restart
    /// that reads one back.
    #[cfg(test)]
    pub(crate) async fn install(&self, source_id: &str, index: SourceIndex) {
        self.inner
            .lock()
            .await
            .sources
            .insert(source_id.to_string(), index);
    }

    async fn persist(inner: &Inner) -> Result<(), String> {
        let Some(path) = inner.path.as_ref() else {
            return Ok(());
        };
        let body = serde_json::to_vec(&Persisted {
            v: INDEX_VERSION,
            sources: inner.sources.clone(),
        })
        .map_err(|e| format!("serialise: {e}"))?;
        let parent = path
            .parent()
            .ok_or_else(|| format!("{path:?} has no parent"))?;
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("mkdir {parent:?}: {e}"))?;
        let staging = parent.join(".dlna-index.json.tmp");
        tokio::fs::write(&staging, body)
            .await
            .map_err(|e| format!("write {staging:?}: {e}"))?;
        tokio::fs::rename(&staging, path)
            .await
            .map_err(|e| format!("rename {staging:?} -> {path:?}: {e}"))?;
        Ok(())
    }
}

/// Which part of a DLNA tree to walk first, from a container's name.
///
/// The browse answer carries no `upnp:class` for a container, so the
/// name is what there is. Last (2): video, photo and image trees,
/// and the pseudo-containers a media server lists beside the library
/// (Watch Later, Recommended, `* Queue`, `* Recommendations`), which
/// answer a browse with an error or with what is not the library.
/// First (0): what names music. Everything else between (1). The
/// pseudo-container check comes first, so "Music Queue" is last.
/// A container the name does not place follows the one it is in, so
/// a video tree stays last all the way down and a music tree first.
pub(crate) fn dlna_container_tier(name: &str) -> usize {
    const LAST: &[&str] = &[
        "video",
        "photo",
        "image",
        "picture",
        "movie",
        "watch later",
        "recommended",
        "recommendations",
        "queue",
    ];
    const FIRST: &[&str] =
        &["music", "audio", "album", "artist", "song", "track"];
    let n = name.to_lowercase();
    if LAST.iter().any(|w| n.contains(w)) {
        2
    } else if FIRST.iter().any(|w| n.contains(w)) {
        0
    } else {
        1
    }
}

/// Walk one source and return what it holds.
///
/// Music first, by [`dlna_container_tier`]; containers in the last
/// tier (video, photo, pseudo-containers) are not entered at all, so
/// they cost nothing and put nothing in the index. A container is
/// browsed once however many routes lead to it. One that will not
/// browse is skipped and counted, and the index is then not
/// complete. A root that will not browse is an error: nothing was
/// seen. `gap` is the pause between browses; `max_browses` the
/// budget, after which the index is returned partial.
pub(crate) async fn walk_source(
    dispatcher: &Arc<dyn evo_plugin_sdk::contract::ShelfRequestDispatcher>,
    record: &SourceRecord,
    service_id: &str,
    gap: Duration,
    max_browses: usize,
) -> Result<SourceIndex, VerbError> {
    let mut items: Vec<serde_json::Value> = Vec::new();
    let mut seen_items: HashSet<String> = HashSet::new();
    let mut seen_containers: HashSet<String> = HashSet::new();
    let mut frontier: [VecDeque<String>; 2] =
        [VecDeque::from([String::new()]), VecDeque::new()];
    seen_containers.insert(String::new());
    let mut browses = 0usize;
    let mut skipped = 0usize;
    let mut budget_out = false;
    'walk: loop {
        let Some((parent_tier, container)) = frontier
            .iter_mut()
            .enumerate()
            .find_map(|(i, tier)| tier.pop_front().map(|c| (i, c)))
        else {
            break;
        };
        // The root's neutral children rank below those named for music.
        let parent_tier = if container.is_empty() { 1 } else { parent_tier };
        let mut page = 0usize;
        loop {
            if browses >= max_browses {
                budget_out = true;
                break 'walk;
            }
            if browses > 0 && !gap.is_zero() {
                tokio::time::sleep(gap).await;
            }
            browses += 1;
            let answer = match browse_dlna_via_source_dlna(
                dispatcher,
                &record.id,
                service_id,
                &container,
                page,
                evo_dlna::DLNA_PAGE_HARD_CAP as usize,
                &record.state,
            )
            .await
            {
                Ok(a) => a,
                // Nothing was seen: the source is down.
                Err(e) if browses == 1 => return Err(e),
                Err(e) => {
                    tracing::debug!(
                        plugin = PLUGIN_NAME,
                        source_id = %record.id,
                        container = %container,
                        error = %e,
                        "dlna index: a container would not browse; skipped"
                    );
                    skipped += 1;
                    continue 'walk;
                }
            };
            let entries = answer
                .get("entries")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            for entry in entries {
                match entry.get("kind").and_then(|v| v.as_str()) {
                    Some("directory") => {
                        let Some(id) =
                            entry.get("uri").and_then(|v| v.as_str())
                        else {
                            continue;
                        };
                        let name = entry
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let tier =
                            match (parent_tier, dlna_container_tier(name)) {
                                (_, 2) => continue,
                                (_, 0) => 0,
                                (parent, _) => parent,
                            };
                        if seen_containers.insert(id.to_string()) {
                            frontier[tier].push_back(id.to_string());
                        }
                    }
                    _ => {
                        let key = entry
                            .get("uri")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        if key.is_empty() || seen_items.insert(key) {
                            items.push(entry);
                        }
                    }
                }
            }
            match answer.get("next_page").and_then(|v| v.as_u64()) {
                Some(next) => page = next as usize,
                None => break,
            }
        }
    }
    Ok(SourceIndex {
        items,
        built_at_ms: now_ms(),
        complete: !budget_out && skipped == 0,
    })
}

/// Walk one source and keep the result. A source already being
/// walked is left alone; a walk that fails keeps the old index.
pub(crate) async fn refresh_source(
    ctx: &LibraryContext,
    record: &SourceRecord,
    gap: Duration,
    max_browses: usize,
) {
    let SourceKind::NetworkDlna { service_id, .. } = &record.kind else {
        return;
    };
    let Some(dispatcher) = ctx.shelf_dispatcher.as_ref() else {
        return;
    };
    if !ctx.dlna_index.begin(&record.id, now_ms()).await {
        return;
    }
    let started = std::time::Instant::now();
    let built =
        match walk_source(dispatcher, record, service_id, gap, max_browses)
            .await
        {
            Ok(i) => {
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    source_id = %record.id,
                    items = i.items.len(),
                    complete = i.complete,
                    seconds = started.elapsed().as_secs(),
                    "dlna search index built"
                );
                Some(i)
            }
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    source_id = %record.id,
                    error = %e,
                    "dlna search index: the source would not browse; \
                     asking again later"
                );
                None
            }
        };
    ctx.dlna_index.finish(&record.id, built).await;
}

/// Walk every online DLNA source that is due, one at a time.
pub(crate) async fn refresh_due(
    ctx: &LibraryContext,
    gap: Duration,
    max_browses: usize,
) {
    for record in ctx.registry.snapshot().await {
        if !matches!(record.kind, SourceKind::NetworkDlna { .. })
            || !matches!(record.state, SourceState::Online)
        {
            continue;
        }
        if ctx.dlna_index.due(&record.id, now_ms()).await {
            refresh_source(ctx, &record, gap, max_browses).await;
        }
    }
}

/// Holds the worker so unload can stop it.
pub(crate) struct DlnaIndexHandle {
    task: tokio::task::JoinHandle<()>,
    shutdown: Arc<Notify>,
}

impl DlnaIndexHandle {
    /// Signal shutdown and wait for the task, which leaves a walk
    /// in progress where it is.
    pub(crate) async fn stop(self) {
        self.shutdown.notify_one();
        let _ = self.task.await;
    }
}

/// Spawn the worker that keeps the indexes. It looks every `tick`,
/// and at once when a search asks.
pub(crate) fn spawn_index_worker(
    ctx: LibraryContext,
    tick: Duration,
    gap: Duration,
    max_browses: usize,
) -> DlnaIndexHandle {
    let shutdown = Arc::new(Notify::new());
    let task_shutdown = Arc::clone(&shutdown);
    let wake = Arc::clone(&ctx.dlna_index.wake);
    let task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(tick);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = task_shutdown.notified() => break,
                _ = ticker.tick() => {}
                _ = wake.notified() => {}
            }
            tokio::select! {
                _ = task_shutdown.notified() => break,
                _ = refresh_due(&ctx, gap, max_browses) => {}
            }
        }
    });
    DlnaIndexHandle { task, shutdown }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(items: usize, complete: bool, built_at_ms: u64) -> SourceIndex {
        SourceIndex {
            items: (0..items)
                .map(|i| serde_json::json!({ "name": format!("n{i}"), "uri": format!("dlna:s/{i}") }))
                .collect(),
            built_at_ms,
            complete,
        }
    }

    #[test]
    fn the_container_tiers() {
        assert_eq!(dlna_container_tier("Music"), 0);
        assert_eq!(dlna_container_tier("Family Music"), 0);
        assert_eq!(dlna_container_tier("Albums"), 0);
        assert_eq!(dlna_container_tier("Recently Added"), 1);
        assert_eq!(dlna_container_tier("Video"), 2);
        assert_eq!(dlna_container_tier("Photos"), 2);
        assert_eq!(dlna_container_tier("Watch Later"), 2);
        assert_eq!(dlna_container_tier("Recommended"), 2);
        assert_eq!(dlna_container_tier("Music Queue"), 2);
        assert_eq!(dlna_container_tier("Music Recommendations"), 2);
    }

    /// The walk is rate-limited and bounded, in numbers a change has
    /// to touch on purpose.
    #[test]
    fn the_walk_is_paused_and_bounded() {
        assert!(INDEX_BROWSE_GAP >= Duration::from_millis(50));
        assert!(INDEX_BROWSE_GAP <= Duration::from_secs(2));
        assert!((500..=10_000).contains(&INDEX_MAX_BROWSES));
        assert!(INDEX_REFRESH >= Duration::from_secs(3600));
        assert!(INDEX_RETRY >= Duration::from_secs(60));
    }

    /// A source is due when it has no index, or the index is old; not
    /// while it is being walked; and not again at once after a try.
    #[tokio::test]
    async fn a_source_is_due_when_unindexed_or_old() {
        let idx = DlnaIndex::default();
        let day = 24 * 3600 * 1000u64;
        assert!(idx.due("s", day).await, "no index: due");
        idx.install("s", index(1, true, day)).await;
        assert!(!idx.due("s", day + 1000).await, "fresh index: not due");
        assert!(
            idx.due("s", day + INDEX_REFRESH.as_millis() as u64).await,
            "old index: due"
        );
        assert!(idx.begin("t", day).await);
        assert!(!idx.begin("t", day).await, "one walk at a time");
        assert!(!idx.due("t", day + 1).await, "being walked: not due");
        idx.finish("t", None).await;
        assert!(!idx.due("t", day + 1000).await, "just tried: not due");
        assert!(
            idx.due("t", day + INDEX_RETRY.as_millis() as u64).await,
            "tried a while ago and nothing came of it: due"
        );
    }

    /// The index survives a restart: written at the end of a walk,
    /// read back by `open`, searched without any walk.
    #[tokio::test]
    async fn the_index_is_kept_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dlna-index.json");
        let idx = DlnaIndex::open(path.clone()).await;
        assert!(idx.search("s", |_| true).await.is_none());
        assert!(idx.begin("s", 1).await);
        idx.finish("s", Some(index(3, false, 42))).await;
        let again = DlnaIndex::open(path).await;
        let a = again
            .search("s", |i| i["name"] == "n1")
            .await
            .expect("read back");
        assert_eq!(a.hits.len(), 1);
        assert!(!a.complete, "completeness is kept with the index");
    }

    /// A damaged or foreign file starts empty and is never fatal.
    #[tokio::test]
    async fn a_damaged_index_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dlna-index.json");
        std::fs::write(&path, b"{ not json").unwrap();
        let idx = DlnaIndex::open(path.clone()).await;
        assert!(idx.search("s", |_| true).await.is_none());
        std::fs::write(&path, br#"{"v":99,"sources":{}}"#).unwrap();
        let idx = DlnaIndex::open(path).await;
        assert!(idx.search("s", |_| true).await.is_none());
    }
}
