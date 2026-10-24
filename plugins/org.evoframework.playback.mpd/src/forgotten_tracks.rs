// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Forget a track: it leaves the library the operator browses.
//!
//! The file stays on disk. The source stays registered. MPD's
//! database still holds the song, and a library update still
//! finds it. What changes is what this plugin answers: browse,
//! search, the facet drills, and the two play-history reads omit
//! a forgotten URI. The live queue is left exactly as it is —
//! forgetting touches no connection to MPD at all.
//!
//! The forgotten set is the plugin's own state, one file under
//! its state directory, written whole and renamed into place, and
//! read back at load. Forgetting a track that is already forgotten
//! is the same outcome. The identity is the URI the queue uses.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Mutex;

use crate::library::{LibraryContext, VerbError, LIBRARY_PAYLOAD_VERSION};

const PLUGIN_NAME: &str = "org.evoframework.playback.mpd";

/// File-format version of the kept set.
pub(crate) const FORGOTTEN_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct Persisted {
    v: u32,
    uris: Vec<String>,
}

#[derive(Default)]
struct Inner {
    uris: BTreeSet<String>,
    path: Option<PathBuf>,
}

/// The forgotten set and where it is kept. Cloneable; the library
/// context holds it and every answer that lists tracks reads it.
/// The default keeps nothing on disk.
#[derive(Clone, Default)]
pub(crate) struct ForgottenTracks {
    inner: Arc<Mutex<Inner>>,
}

impl ForgottenTracks {
    /// The set at this path, read back if it is there. A file that
    /// cannot be read or parsed is reported and starts empty.
    pub(crate) async fn open(path: PathBuf) -> Self {
        let store = Self {
            inner: Arc::new(Mutex::new(Inner {
                uris: BTreeSet::new(),
                path: Some(path.clone()),
            })),
        };
        match tokio::fs::read_to_string(&path).await {
            Ok(body) => match toml::from_str::<Persisted>(&body) {
                Ok(p) if p.v == FORGOTTEN_VERSION => {
                    let mut inner = store.inner.lock().await;
                    inner.uris = p.uris.into_iter().collect();
                    tracing::info!(
                        plugin = PLUGIN_NAME,
                        forgotten = inner.uris.len(),
                        "forgotten tracks read back"
                    );
                }
                Ok(p) => tracing::warn!(
                    plugin = PLUGIN_NAME,
                    version = p.v,
                    "forgotten tracks are from another format version; \
                     starting empty"
                ),
                Err(e) => tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "forgotten tracks do not parse; starting empty"
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "forgotten tracks unreadable; starting empty"
            ),
        }
        store
    }

    /// Forget one URI. Returns whether it was newly forgotten;
    /// forgetting it again is the same outcome and `false`.
    pub(crate) async fn forget(&self, uri: &str) -> bool {
        let mut inner = self.inner.lock().await;
        let newly = inner.uris.insert(uri.to_string());
        if newly {
            if let Err(e) = Self::persist(&inner).await {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "forgotten tracks could not be written; the set is kept \
                     in memory until the next change"
                );
            }
        }
        newly
    }

    #[cfg(test)]
    pub(crate) async fn contains(&self, uri: &str) -> bool {
        self.inner.lock().await.uris.contains(uri)
    }

    /// The set as it stands, for one answer's filtering.
    pub(crate) async fn snapshot(&self) -> BTreeSet<String> {
        self.inner.lock().await.uris.clone()
    }

    async fn persist(inner: &Inner) -> Result<(), String> {
        let Some(path) = inner.path.as_ref() else {
            return Ok(());
        };
        let body = toml::to_string_pretty(&Persisted {
            v: FORGOTTEN_VERSION,
            uris: inner.uris.iter().cloned().collect(),
        })
        .map_err(|e| format!("serialise: {e}"))?;
        let parent = path
            .parent()
            .ok_or_else(|| format!("{path:?} has no parent"))?;
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("mkdir {parent:?}: {e}"))?;
        let staging = parent.join(format!(
            ".{}.tmp",
            path.file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| "forgotten-tracks.toml".to_string())
        ));
        tokio::fs::write(&staging, body)
            .await
            .map_err(|e| format!("write {staging:?}: {e}"))?;
        tokio::fs::rename(&staging, path)
            .await
            .map_err(|e| format!("rename {staging:?} -> {path:?}: {e}"))?;
        Ok(())
    }
}

/// Drop every rendered entry whose `uri` is forgotten. Directories
/// and anything without a `uri` pass through.
pub(crate) fn omit_forgotten(
    entries: Vec<serde_json::Value>,
    forgotten: &BTreeSet<String>,
) -> Vec<serde_json::Value> {
    if forgotten.is_empty() {
        return entries;
    }
    entries
        .into_iter()
        .filter(|e| {
            !e.get("uri")
                .and_then(|u| u.as_str())
                .is_some_and(|u| forgotten.contains(u))
        })
        .collect()
}

/// `library.forget_track` request payload.
#[derive(Debug, Deserialize)]
pub(crate) struct ForgetTrackPayload {
    pub(crate) v: u32,
    /// The URI the queue uses for the track.
    pub(crate) uri: String,
}

/// `library.forget_track` — the track leaves the library the
/// operator browses. Takes no MPD connection: nothing about MPD,
/// the queue, or the file changes. An empty URI is refused.
pub(crate) async fn handle_forget_track(
    ctx: &LibraryContext,
    payload: ForgetTrackPayload,
) -> Result<serde_json::Value, VerbError> {
    if payload.v != LIBRARY_PAYLOAD_VERSION {
        return Err(VerbError::PayloadVersion {
            verb: "library.forget_track".to_string(),
            got: payload.v,
            expected: LIBRARY_PAYLOAD_VERSION,
        });
    }
    let uri = payload.uri.trim();
    if uri.is_empty() {
        return Err(VerbError::ForgetUriEmpty);
    }
    let newly = ctx.forgotten.forget(uri).await;
    tracing::info!(
        plugin = PLUGIN_NAME,
        uri,
        already_forgotten = !newly,
        "track forgotten"
    );
    Ok(json!({
        "v": LIBRARY_PAYLOAD_VERSION,
        "status": "ok",
        "uri": uri,
        "already_forgotten": !newly,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Forget, and it is forgotten; again, and it is the same
    /// outcome; it survives a restart; a bad file starts empty.
    #[tokio::test]
    async fn the_set_is_kept_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("forgotten-tracks.toml");
        {
            let f = ForgottenTracks::open(path.clone()).await;
            assert!(!f.contains("INTERNAL/a.flac").await);
            assert!(f.forget("INTERNAL/a.flac").await, "newly forgotten");
            assert!(!f.forget("INTERNAL/a.flac").await, "the same outcome");
            assert!(f.contains("INTERNAL/a.flac").await);
        }
        let f = ForgottenTracks::open(path.clone()).await;
        assert!(f.contains("INTERNAL/a.flac").await, "survives a restart");
        assert_eq!(f.snapshot().await.len(), 1);
        assert!(!path.with_file_name(".forgotten-tracks.toml.tmp").exists());

        std::fs::write(&path, "v = [").unwrap();
        assert!(ForgottenTracks::open(path)
            .await
            .snapshot()
            .await
            .is_empty());
    }

    // ===== the verb and the answers that must omit it =====

    use crate::library::{
        handle_browse_library, handle_search_library, BrowseLibraryPayload,
        SearchLibraryPayload,
    };
    use crate::mpd::MpdConnection;
    use crate::play_history::{
        handle_most_played, handle_recently_played, HistoryReadPayload,
        PlayHistory,
    };
    use crate::playback_supervisor::test_mock::{
        short_timeouts, spawn_mock_mpd, CapturingSubjectAnnouncer,
        ConnBehaviour,
    };
    use crate::source_registry::{
        ScanPolicy, SourceKind, SourceRecord, SourceRegistry, SourceState,
    };
    use std::collections::BTreeMap;

    struct Harness {
        ctx: LibraryContext,
        conn: MpdConnection,
        commands: Arc<std::sync::Mutex<Vec<String>>>,
        music: tempfile::TempDir,
        state: tempfile::TempDir,
    }

    const A: &str = "INTERNAL/Low/Trust/01 Words.flac";
    const B: &str = "INTERNAL/Low/Trust/02 Canada.flac";

    /// A library of two tracks under `INTERNAL`, really on disk
    /// under a tempdir music directory, an Online local source
    /// mounted there, and a forgotten set kept under a tempdir
    /// state directory.
    async fn harness() -> Harness {
        let commands = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (endpoint, _mock) =
            spawn_mock_mpd(vec![ConnBehaviour::StoredPlaylists {
                commands: Arc::clone(&commands),
                playlists: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
                library: vec![(
                    "INTERNAL".to_string(),
                    Vec::new(),
                    vec![A.to_string(), B.to_string()],
                )],
                queue: vec![A.to_string(), B.to_string()],
            }])
            .await;
        let conn =
            MpdConnection::connect_with_timeouts(endpoint, short_timeouts())
                .await
                .unwrap();
        let music = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(music.path().join("INTERNAL/Low/Trust"))
            .unwrap();
        std::fs::write(music.path().join(A), b"a").unwrap();
        std::fs::write(music.path().join(B), b"b").unwrap();
        let state = tempfile::tempdir().unwrap();
        let registry = SourceRegistry::new();
        registry
            .upsert(SourceRecord {
                id: "local-internal".into(),
                display_name: "Local".into(),
                kind: SourceKind::LocalInternal,
                mount_path: music.path().join("INTERNAL"),
                mpd_storage_name: Some("internal".into()),
                state: SourceState::Online,
                last_seen_online_at_ms: Some(1_000),
                probe_cadence_ms: 60_000,
                scan_policy: ScanPolicy::EagerIncremental {
                    on_online: true,
                    on_mount_event: false,
                },
                track_count: 2,
                track_count_available: 2,
                last_scan_at_ms: Some(2_000),
            })
            .await;
        let ctx = LibraryContext::new(
            music.path().to_path_buf(),
            registry,
            Arc::new(CapturingSubjectAnnouncer::default()),
            None,
        )
        .with_forgotten(
            ForgottenTracks::open(state.path().join("forgotten-tracks.toml"))
                .await,
        );
        Harness {
            ctx,
            conn,
            commands,
            music,
            state,
        }
    }

    fn uris(v: &serde_json::Value, key: &str) -> Vec<String> {
        v[key]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["uri"].as_str().map(str::to_string))
            .collect()
    }

    async fn browse(h: &mut Harness) -> serde_json::Value {
        handle_browse_library(
            &h.ctx,
            &mut h.conn,
            BrowseLibraryPayload {
                v: 1,
                source_id: "local-internal".into(),
                path: None,
                page: None,
                page_size: None,
            },
        )
        .await
        .unwrap()
    }

    async fn search(h: &mut Harness) -> serde_json::Value {
        handle_search_library(
            &h.ctx,
            &mut h.conn,
            SearchLibraryPayload {
                v: 1,
                query: "trust".into(),
                source_ids: None,
                include_offline: false,
                max_results: 50,
            },
        )
        .await
        .unwrap()
    }

    fn tree(root: &std::path::Path) -> Vec<String> {
        fn walk(
            dir: &std::path::Path,
            root: &std::path::Path,
            out: &mut Vec<String>,
        ) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                out.push(
                    p.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
                if p.is_dir() {
                    walk(&p, root, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort();
        out
    }

    /// Forgotten, the track is gone from browse and from search —
    /// the other track and the directory stay — and the music
    /// file is still on disk. MPD was sent nothing by the forget.
    #[tokio::test]
    async fn a_forgotten_track_leaves_browse_and_search_and_stays_on_disk() {
        let mut h = harness().await;
        let before_tree = tree(h.music.path());
        assert_eq!(uris(&browse(&mut h).await, "entries"), vec![A, B]);
        assert_eq!(uris(&search(&mut h).await, "results"), vec![A, B]);
        let sent_before = h.commands.lock().unwrap().len();

        let r = handle_forget_track(
            &h.ctx,
            ForgetTrackPayload {
                v: 1,
                uri: A.into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(r["status"], "ok");
        assert_eq!(r["already_forgotten"], false);
        assert_eq!(
            h.commands.lock().unwrap().len(),
            sent_before,
            "forgetting sends MPD nothing: no file, no queue, no update"
        );

        let b = browse(&mut h).await;
        assert_eq!(uris(&b, "entries"), vec![B], "{b}");
        assert_eq!(b["total"], 1);
        let s = search(&mut h).await;
        assert_eq!(uris(&s, "results"), vec![B], "{s}");
        assert_eq!(tree(h.music.path()), before_tree, "the file stays on disk");
        assert!(h.music.path().join(A).is_file());

        // Again: the same outcome.
        let again = handle_forget_track(
            &h.ctx,
            ForgetTrackPayload {
                v: 1,
                uri: A.into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(again["already_forgotten"], true);
        assert_eq!(uris(&browse(&mut h).await, "entries"), vec![B]);
    }

    /// A browse served from the cache — the source gone Offline —
    /// omits the forgotten track too, without a refresh.
    #[tokio::test]
    async fn a_cached_browse_omits_it_too() {
        let mut h = harness().await;
        let _ = browse(&mut h).await; // fills the cache
        handle_forget_track(
            &h.ctx,
            ForgetTrackPayload {
                v: 1,
                uri: B.into(),
            },
        )
        .await
        .unwrap();
        let mut rec = h.ctx.registry.get("local-internal").await.unwrap();
        rec.state = SourceState::Offline {
            since_ms: 5_000,
            reason: "test".into(),
        };
        h.ctx.registry.upsert(rec).await;
        let b = browse(&mut h).await;
        assert_eq!(b["stale"], true, "served from the cache: {b}");
        assert_eq!(uris(&b, "entries"), vec![A], "{b}");
    }

    /// Both history reads omit a forgotten track, and the limit
    /// counts what is shown, not what was skipped.
    #[tokio::test]
    async fn the_history_reads_omit_it() {
        let h = harness().await;
        let history = PlayHistory::disabled();
        history.record_finish(A, 1).await;
        history.record_finish(B, 2).await;
        history.record_finish(A, 3).await;
        handle_forget_track(
            &h.ctx,
            ForgetTrackPayload {
                v: 1,
                uri: A.into(),
            },
        )
        .await
        .unwrap();
        let recent = handle_recently_played(
            &history,
            &h.ctx.forgotten,
            HistoryReadPayload {
                v: 1,
                limit: Some(1),
            },
        )
        .await
        .unwrap();
        assert_eq!(uris(&recent, "plays"), vec![B], "{recent}");
        let most = handle_most_played(
            &history,
            &h.ctx.forgotten,
            HistoryReadPayload { v: 1, limit: None },
        )
        .await
        .unwrap();
        assert_eq!(uris(&most, "tracks"), vec![B], "{most}");
    }

    /// A library update does not bring the track back: the
    /// rehydrate the Database / Update idle path runs leaves the
    /// forgotten set as it was, on disk and in the answers.
    #[tokio::test]
    async fn an_update_does_not_bring_it_back() {
        let mut h = harness().await;
        handle_forget_track(
            &h.ctx,
            ForgetTrackPayload {
                v: 1,
                uri: A.into(),
            },
        )
        .await
        .unwrap();
        crate::library::rehydrate_from_mpd(&h.ctx, &mut h.conn).await;
        assert!(h.ctx.forgotten.contains(A).await);
        assert_eq!(uris(&browse(&mut h).await, "entries"), vec![B]);
        // And across a restart of the plugin: read back from the
        // state directory.
        let reopened =
            ForgottenTracks::open(h.state.path().join("forgotten-tracks.toml"))
                .await;
        assert!(reopened.contains(A).await, "survives a restart");
    }

    /// An empty URI is refused, and nothing is forgotten by it.
    #[tokio::test]
    async fn an_empty_uri_is_refused() {
        let h = harness().await;
        for uri in ["", "   "] {
            let got = handle_forget_track(
                &h.ctx,
                ForgetTrackPayload {
                    v: 1,
                    uri: uri.into(),
                },
            )
            .await;
            assert!(matches!(got, Err(VerbError::ForgetUriEmpty)), "{got:?}");
        }
        assert!(h.ctx.forgotten.snapshot().await.is_empty());
    }

    // ===== forgetting an album =====

    const T1: &str = "INTERNAL/Low/Trust/01 Words.flac";
    const T2: &str = "INTERNAL/Low/Trust/02 Canada.flac";
    const O1: &str = "INTERNAL/Other/One/x.flac";
    const T3_LATER: &str = "INTERNAL/Low/Trust/03 Later.flac";

    struct AlbumHarness {
        ctx: LibraryContext,
        conn: MpdConnection,
        later: MpdConnection,
        commands: Arc<std::sync::Mutex<Vec<String>>>,
        music: tempfile::TempDir,
    }

    /// Two albums by folder, `Low/Trust` with two tracks and
    /// `Other/One` with one, really on disk. The first connection
    /// sees the library as it is; the second sees it after a
    /// third file arrived in `Low/Trust`.
    async fn album_harness() -> AlbumHarness {
        let commands = Arc::new(std::sync::Mutex::new(Vec::new()));
        let now = vec![T1.to_string(), T2.to_string(), O1.to_string()];
        let later = vec![
            T1.to_string(),
            T2.to_string(),
            T3_LATER.to_string(),
            O1.to_string(),
        ];
        let behaviour = |files: Vec<String>| ConnBehaviour::StoredPlaylists {
            commands: Arc::clone(&commands),
            playlists: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            library: vec![("INTERNAL".to_string(), Vec::new(), files)],
            queue: vec![T1.to_string()],
        };
        let (endpoint, _mock) =
            spawn_mock_mpd(vec![behaviour(now), behaviour(later)]).await;
        let conn = MpdConnection::connect_with_timeouts(
            endpoint.clone(),
            short_timeouts(),
        )
        .await
        .unwrap();
        let later =
            MpdConnection::connect_with_timeouts(endpoint, short_timeouts())
                .await
                .unwrap();
        let music = tempfile::tempdir().unwrap();
        for f in [T1, T2, O1] {
            let p = music.path().join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"x").unwrap();
        }
        let registry = SourceRegistry::new();
        registry
            .upsert(SourceRecord {
                id: "local-internal".into(),
                display_name: "Local".into(),
                kind: SourceKind::LocalInternal,
                mount_path: music.path().join("INTERNAL"),
                mpd_storage_name: Some("internal".into()),
                state: SourceState::Online,
                last_seen_online_at_ms: Some(1_000),
                probe_cadence_ms: 60_000,
                scan_policy: ScanPolicy::EagerIncremental {
                    on_online: true,
                    on_mount_event: false,
                },
                track_count: 3,
                track_count_available: 3,
                last_scan_at_ms: Some(2_000),
            })
            .await;
        let ctx = LibraryContext::new(
            music.path().to_path_buf(),
            registry,
            Arc::new(CapturingSubjectAnnouncer::default()),
            None,
        )
        .with_forgotten(ForgottenTracks::default());
        AlbumHarness {
            ctx,
            conn,
            later,
            commands,
            music,
        }
    }

    async fn album_tiles(
        ctx: &LibraryContext,
        conn: &mut MpdConnection,
    ) -> Vec<(String, u64)> {
        let v = crate::library::handle_browse_by_tag(
            conn,
            &ctx.forgotten,
            crate::library::BrowseByTagPayload {
                v: 1,
                page: None,
                page_size: None,
                select: None,
            },
            "library.browse_by_album",
            "album",
            "album",
            crate::library::identity_post_process,
        )
        .await
        .unwrap();
        v["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["album_id"].as_str().unwrap().to_string(),
                    e["track_count"].as_u64().unwrap(),
                )
            })
            .collect()
    }

    async fn forget_album(
        ctx: &LibraryContext,
        conn: &mut MpdConnection,
        album: &str,
    ) -> Result<serde_json::Value, VerbError> {
        crate::library::handle_forget_album(
            ctx,
            conn,
            crate::library::ForgetAlbumPayload {
                v: 1,
                album: album.to_string(),
                parent: None,
            },
        )
        .await
    }

    /// Forgetting an album forgets the tracks its drill lists
    /// now, through the one set: each is then already forgotten
    /// for `forget_track`; browse and search omit them; the files
    /// stay on disk; the only MPD traffic is the drill's read.
    #[tokio::test]
    async fn forgetting_an_album_forgets_its_current_tracks() {
        let mut h = album_harness().await;
        let before_tree = tree(h.music.path());
        let r = forget_album(&h.ctx, &mut h.conn, "INTERNAL/Low/Trust")
            .await
            .unwrap();
        assert_eq!(r["status"], "ok");
        assert_eq!(r["newly_forgotten"], 2, "{r}");
        assert_eq!(r["already_forgotten"], 0);
        assert!(h.ctx.forgotten.contains(T1).await);
        assert!(h.ctx.forgotten.contains(T2).await);
        assert!(!h.ctx.forgotten.contains(O1).await, "the other album stays");

        // Already forgotten for the one-track verb.
        let one = handle_forget_track(
            &h.ctx,
            ForgetTrackPayload {
                v: 1,
                uri: T1.into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(one["already_forgotten"], true);

        // Browse and search omit them; the files are still there.
        let b = handle_browse_library(
            &h.ctx,
            &mut h.conn,
            BrowseLibraryPayload {
                v: 1,
                source_id: "local-internal".into(),
                path: None,
                page: None,
                page_size: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(uris(&b, "entries"), vec![O1], "{b}");
        let s = handle_search_library(
            &h.ctx,
            &mut h.conn,
            SearchLibraryPayload {
                v: 1,
                query: "flac".into(),
                source_ids: None,
                include_offline: false,
                max_results: 50,
            },
        )
        .await
        .unwrap();
        assert_eq!(uris(&s, "results"), vec![O1], "{s}");
        assert_eq!(tree(h.music.path()), before_tree, "no file removed");

        // The only MPD traffic was the drill's own read.
        let seen = h.commands.lock().unwrap();
        let writes: Vec<&String> = seen
            .iter()
            .filter(|c| {
                !(c.starts_with("listallinfo")
                    || c.starts_with("lsinfo")
                    || c.starts_with("search")
                    || c.starts_with("list "))
            })
            .collect();
        assert!(
            writes.is_empty(),
            "forgetting an album only reads: {writes:?}"
        );
    }

    /// The album tile goes when none of its tracks remain, and
    /// counts what remains while some do.
    #[tokio::test]
    async fn the_album_tile_goes_when_none_of_its_tracks_remain() {
        let mut h = album_harness().await;
        assert_eq!(
            album_tiles(&h.ctx, &mut h.conn).await,
            vec![
                ("INTERNAL/Other/One".to_string(), 1),
                ("INTERNAL/Low/Trust".to_string(), 2),
            ]
        );
        handle_forget_track(
            &h.ctx,
            ForgetTrackPayload {
                v: 1,
                uri: T1.into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            album_tiles(&h.ctx, &mut h.conn).await,
            vec![
                ("INTERNAL/Other/One".to_string(), 1),
                ("INTERNAL/Low/Trust".to_string(), 1),
            ],
            "one track forgotten: the tile stays and counts one"
        );
        forget_album(&h.ctx, &mut h.conn, "INTERNAL/Low/Trust")
            .await
            .unwrap();
        assert_eq!(
            album_tiles(&h.ctx, &mut h.conn).await,
            vec![("INTERNAL/Other/One".to_string(), 1)],
            "none remain: the tile is gone"
        );
    }

    /// A file added to the album later is not forgotten: the set
    /// holds URIs, not the folder. It appears in browse and the
    /// tile is back for it alone.
    #[tokio::test]
    async fn a_later_file_in_the_album_is_not_forgotten() {
        let mut h = album_harness().await;
        forget_album(&h.ctx, &mut h.conn, "INTERNAL/Low/Trust")
            .await
            .unwrap();
        assert!(!h.ctx.forgotten.contains(T3_LATER).await);
        let b = handle_browse_library(
            &h.ctx,
            &mut h.later,
            BrowseLibraryPayload {
                v: 1,
                source_id: "local-internal".into(),
                path: None,
                page: None,
                page_size: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(uris(&b, "entries"), vec![T3_LATER, O1], "{b}");
        assert_eq!(
            album_tiles(&h.ctx, &mut h.later).await,
            vec![
                ("INTERNAL/Other/One".to_string(), 1),
                ("INTERNAL/Low/Trust".to_string(), 1),
            ],
            "the tile is back, counting the later file alone"
        );
    }

    /// An empty identity, and an album that resolves to no track,
    /// are refused, and nothing is forgotten by either.
    #[tokio::test]
    async fn an_empty_or_unknown_album_is_refused() {
        let mut h = album_harness().await;
        let empty = forget_album(&h.ctx, &mut h.conn, "  ").await;
        assert!(
            matches!(empty, Err(VerbError::ForgetAlbumEmpty)),
            "{empty:?}"
        );
        let unknown = forget_album(&h.ctx, &mut h.conn, "INTERNAL/Nope").await;
        assert!(
            matches!(unknown, Err(VerbError::ForgetAlbumUnknown { .. })),
            "{unknown:?}"
        );
        assert!(h.ctx.forgotten.snapshot().await.is_empty());
    }

    /// Nothing in the plugin can shrink the set: the module
    /// offers no clear, remove or retain on it, so no update,
    /// rescan or rehydrate has a way to bring a track back. The
    /// production source is held to that. (Remembering a track
    /// again is a verb of its own, not this row's.)
    #[test]
    fn the_forgotten_set_has_no_way_to_shrink() {
        let src = include_str!("forgotten_tracks.rs");
        let production = src.split("#[cfg(test)]").next().unwrap();
        for forbidden in [
            "uris.clear(",
            "uris.remove(",
            "uris.retain(",
            "uris.pop_first(",
            "uris.pop_last(",
        ] {
            assert!(
                !production.contains(forbidden),
                "the forgotten set must have no way to shrink; found {forbidden:?}"
            );
        }
    }

    /// Forgetting has no connection to MPD: the production source
    /// of this module never names one, so the verb cannot touch
    /// the live queue, the database or a file through MPD. The
    /// only filesystem use is the set's own state file.
    #[test]
    fn the_forget_verb_has_no_connection_to_mpd() {
        let src = include_str!("forgotten_tracks.rs");
        let production = src.split("#[cfg(test)]").next().unwrap();
        for forbidden in [
            "MpdConnection",
            "MpdEndpoint",
            "conn.",
            "remove_file",
            "remove_dir",
            "fs::copy",
        ] {
            assert!(
                !production.contains(forbidden),
                "forgetting must not reach MPD or the music files; found \
                 {forbidden:?}"
            );
        }
    }

    /// Only the forgotten `uri` goes; directories and the rest
    /// stay in order.
    #[test]
    fn omit_drops_only_the_forgotten_uri() {
        let entries = vec![
            json!({ "kind": "directory", "uri": "INTERNAL/x" }),
            json!({ "kind": "file", "uri": "INTERNAL/x/1.flac" }),
            json!({ "kind": "file", "uri": "INTERNAL/x/2.flac" }),
            json!({ "name": "no uri at all" }),
        ];
        let set: BTreeSet<String> = ["INTERNAL/x/1.flac".to_string()].into();
        let out = omit_forgotten(entries.clone(), &set);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0]["uri"], "INTERNAL/x");
        assert_eq!(out[1]["uri"], "INTERNAL/x/2.flac");
        assert_eq!(out[2]["name"], "no uri at all");
        assert_eq!(omit_forgotten(entries.clone(), &BTreeSet::new()), entries);
    }
}
