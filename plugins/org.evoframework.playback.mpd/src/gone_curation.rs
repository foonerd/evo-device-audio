// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Gone-curation prune — permanent library loss vs temporary Offline.
//!
//! # Contract
//!
//! Catalogue shelves intentionally **retain** entries when a source
//! is temporarily Offline (NAS unmounted, DLNA down): project
//! `available: false`, skip at play, keep the URI in queue /
//! favourites / stored playlists.
//!
//! That policy must **not** apply when a track is permanently
//! Gone from an Online source (file deleted, mass wipe of
//! `/var/lib/evo/music`, MPD `update` removed the song). Ghost
//! URIs in favourites/playlists/queue after destructive wipe are
//! a curation lie — the classifier below distinguishes the two
//! cases and drives disposition accordingly.
//!
//! **Gone** (prune):
//! - URI is a local MPD file path (not `http(s):`, not `dlna:…`)
//! - URI is absent from MPD's song database after an update
//! - Owning source resolves and is `Online` or `Degraded`
//!
//! **Not Gone** (retain):
//! - Remote stream / DLNA identity URIs
//! - Source `Offline` / `Retired` / `Probing` / unresolved
//!   (temporary Offline keep-curation)
//! - URI still present in MPD DB
//!
//! # When it runs
//!
//! - Warm-start after shelf rehydrate (boot into a wiped library)
//! - Idle `Database` / `Update` after `library::rehydrate_from_mpd`
//!
//! Both call sites route through
//! [`crate::library_triage::run_triage`] — the triage sweep runs
//! this prune as one of its drift classes so the reconcile is
//! surfaced on the `audio_library_triage` subject rather than
//! silently in the log. Direct calls to
//! [`prune_gone_from_curation`] outside the triage substrate
//! are correct but bypass the operator-visible finding.
//!
//! Best-effort: transport errors log and return; never panics.
//! A process-wide gate serialises concurrent prune passes so two
//! idle bursts cannot interleave `playlistdelete` renumbering.

use std::collections::HashSet;
use std::path::Path;
use std::sync::OnceLock;

use crate::availability;
use crate::favourites::{self, FavouritesContext};
use crate::mpd::{MpdConnection, MpdLibraryEntry, MpdSearchField};
use crate::playlist::{self, PlaylistContext};
use crate::queue::{self, resolve_source, QueueContext};
use crate::source_registry::{SourceRegistry, SourceState};

const PLUGIN_NAME: &str = "org.evoframework.playback.mpd";

/// Counts from one prune pass — logged + returned for tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PruneStats {
    pub(crate) queue_removed: u32,
    pub(crate) favourites_removed: u32,
    pub(crate) playlist_entries_removed: u32,
    pub(crate) playlists_touched: u32,
}

fn prune_gate() -> &'static tokio::sync::Mutex<()> {
    static GATE: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    GATE.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// True for URIs that live in MPD's local song DB under
/// `music_directory` (eligible for Gone detection).
pub(crate) fn is_local_mpd_file_uri(uri: &str) -> bool {
    if uri.is_empty() {
        return false;
    }
    if availability::is_remote_stream_uri(uri) {
        return false;
    }
    // `dlna:<service>/<object>` identities are not MPD DB paths.
    if uri.starts_with("dlna:") || uri.starts_with("DLNA:") {
        return false;
    }
    // Any other URI-with-scheme (smb:, nfs:, mpd-path: if ever
    // stored raw) is outside the local file prune surface.
    if uri.contains("://") {
        return false;
    }
    true
}

/// Source states that mean "the library plane is reachable" —
/// only then is absence from the MPD DB permanent Gone rather
/// than temporary Offline retention.
fn source_is_reachable(state: &SourceState) -> bool {
    matches!(state, SourceState::Online | SourceState::Degraded { .. })
}

/// Classify a local URI as Gone given a pre-built MPD song-path
/// set and the source registry. Pure w.r.t. MPD I/O (async only
/// for registry lookup).
pub(crate) async fn classify_local_uri_gone(
    music_directory: &Path,
    uri: &str,
    songs_in_db: &HashSet<String>,
    registry: &SourceRegistry,
) -> bool {
    if !is_local_mpd_file_uri(uri) {
        return false;
    }
    if songs_in_db.contains(uri) {
        return false;
    }
    let Some(source_id) = resolve_source(music_directory, uri, registry).await
    else {
        // Unresolved local path: do not invent a prune. Safer
        // to retain than to delete curation we cannot attribute.
        return false;
    };
    matches!(
        registry.get(&source_id).await,
        Some(record) if source_is_reachable(&record.state)
    )
}

/// Build the set of every song path currently in MPD's database.
async fn load_song_db_paths(
    conn: &mut MpdConnection,
) -> Result<HashSet<String>, String> {
    let entries = conn.listallinfo("").await.map_err(|e| e.to_string())?;
    let mut set = HashSet::with_capacity(entries.len());
    for entry in entries {
        if let MpdLibraryEntry::File { path, .. } = entry {
            set.insert(path);
        }
    }
    Ok(set)
}

/// Prune Gone local URIs from queue, favourites, and every
/// stored playlist. Republishes affected shelf subjects when
/// anything was removed.
pub(crate) async fn prune_gone_from_curation(
    conn: &mut MpdConnection,
    music_directory: &Path,
    registry: &SourceRegistry,
    queue: &QueueContext,
    playlist: &PlaylistContext,
    favourites: &FavouritesContext,
) -> PruneStats {
    let Ok(_guard) = prune_gate().try_lock() else {
        tracing::debug!(
            plugin = PLUGIN_NAME,
            "gone-curation prune skipped: another pass in flight"
        );
        return PruneStats::default();
    };

    let songs_in_db = match load_song_db_paths(conn).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "gone-curation: listallinfo failed; prune skipped"
            );
            return PruneStats::default();
        }
    };

    let queue_removed =
        prune_queue(conn, music_directory, registry, &songs_in_db).await;
    let favourites_removed = prune_stored_playlist(
        conn,
        music_directory,
        registry,
        &songs_in_db,
        &favourites.favourites_name,
    )
    .await;
    let (playlists_touched, playlist_entries_removed) =
        prune_all_stored_playlists(
            conn,
            music_directory,
            registry,
            &songs_in_db,
            &playlist.favourites_name,
        )
        .await;

    let stats = PruneStats {
        queue_removed,
        favourites_removed,
        playlist_entries_removed,
        playlists_touched,
    };

    if stats.queue_removed > 0 {
        queue::publish_queue(queue, conn).await;
    }
    if stats.favourites_removed > 0 {
        favourites::refresh_favourites(favourites, conn).await;
    }
    if stats.playlists_touched > 0 || stats.playlist_entries_removed > 0 {
        playlist::publish_index(playlist, conn).await;
    }

    if stats.queue_removed
        + stats.favourites_removed
        + stats.playlist_entries_removed
        > 0
    {
        tracing::info!(
            plugin = PLUGIN_NAME,
            queue_removed = stats.queue_removed,
            favourites_removed = stats.favourites_removed,
            playlist_entries_removed = stats.playlist_entries_removed,
            playlists_touched = stats.playlists_touched,
            "gone-curation: pruned local URIs absent from MPD DB \
             under Online/Degraded sources"
        );
    }

    stats
}

async fn prune_queue(
    conn: &mut MpdConnection,
    music_directory: &Path,
    registry: &SourceRegistry,
    songs_in_db: &HashSet<String>,
) -> u32 {
    let items = match conn.playlistinfo().await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "gone-curation: playlistinfo failed"
            );
            return 0;
        }
    };
    let mut remove_ids: Vec<u32> = Vec::new();
    for item in &items {
        if classify_local_uri_gone(
            music_directory,
            &item.file_path,
            songs_in_db,
            registry,
        )
        .await
        {
            remove_ids.push(item.id);
        }
    }
    let mut removed = 0u32;
    for id in remove_ids {
        match conn.deleteid(id).await {
            Ok(()) => removed += 1,
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    songid = id,
                    error = %e,
                    "gone-curation: deleteid failed"
                );
            }
        }
    }
    removed
}

/// Remove Gone entries from one stored playlist. Deletes from
/// highest position to lowest so renumbering cannot skip rows.
async fn prune_stored_playlist(
    conn: &mut MpdConnection,
    music_directory: &Path,
    registry: &SourceRegistry,
    songs_in_db: &HashSet<String>,
    name: &str,
) -> u32 {
    let entries = match conn.listplaylistinfo(name).await {
        Ok(e) => e,
        Err(_) => {
            // Missing playlist is fine (e.g. favourites never created).
            return 0;
        }
    };
    let mut gone_positions: Vec<u32> = Vec::new();
    for entry in &entries {
        if classify_local_uri_gone(
            music_directory,
            &entry.file_path,
            songs_in_db,
            registry,
        )
        .await
        {
            gone_positions.push(entry.position);
        }
    }
    gone_positions.sort_unstable_by(|a, b| b.cmp(a));
    let mut removed = 0u32;
    for pos in gone_positions {
        match conn.playlistdelete(name, pos).await {
            Ok(()) => removed += 1,
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    playlist = %name,
                    position = pos,
                    error = %e,
                    "gone-curation: playlistdelete failed"
                );
            }
        }
    }
    removed
}

async fn prune_all_stored_playlists(
    conn: &mut MpdConnection,
    music_directory: &Path,
    registry: &SourceRegistry,
    songs_in_db: &HashSet<String>,
    favourites_name: &str,
) -> (u32, u32) {
    let summaries = match conn.listplaylists().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "gone-curation: listplaylists failed"
            );
            return (0, 0);
        }
    };
    let mut touched = 0u32;
    let mut removed = 0u32;
    for summary in summaries {
        // Favourites is handled separately so its subject refresh
        // stays on the favourites shelf path.
        if summary.name == favourites_name {
            continue;
        }
        let n = prune_stored_playlist(
            conn,
            music_directory,
            registry,
            songs_in_db,
            &summary.name,
        )
        .await;
        if n > 0 {
            touched += 1;
            removed += n;
        }
    }
    (touched, removed)
}

/// Single-URI presence check used by the availability cascade
/// when a local file has no sticker under an Online source.
/// Returns `true` when MPD's song DB still knows the path.
pub(crate) async fn local_uri_in_song_db(
    conn: &mut MpdConnection,
    uri: &str,
) -> Result<bool, String> {
    if !is_local_mpd_file_uri(uri) {
        return Ok(true);
    }
    let entries = conn
        .find(MpdSearchField::File, uri)
        .await
        .map_err(|e| e.to_string())?;
    Ok(entries.iter().any(
        |e| matches!(e, MpdLibraryEntry::File { path, .. } if path == uri),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_file_uri_accepts_relative_mpd_paths() {
        assert!(is_local_mpd_file_uri("INTERNAL/a.flac"));
        assert!(is_local_mpd_file_uri("NAS/share/track.mp3"));
        assert!(is_local_mpd_file_uri("USB/stick/x.wav"));
    }

    #[test]
    fn local_file_uri_rejects_streams_and_dlna() {
        assert!(!is_local_mpd_file_uri("http://192.0.2.1/a.mp3"));
        assert!(!is_local_mpd_file_uri("https://cdn.example/a.flac"));
        assert!(!is_local_mpd_file_uri("dlna:svc/obj"));
        assert!(!is_local_mpd_file_uri("smb://host/share/a.flac"));
        assert!(!is_local_mpd_file_uri(""));
    }

    #[test]
    fn reachable_states_are_online_and_degraded_only() {
        assert!(source_is_reachable(&SourceState::Online));
        assert!(source_is_reachable(&SourceState::Degraded {
            reason: "slow".into(),
            since_ms: 0,
        }));
        assert!(!source_is_reachable(&SourceState::Offline {
            reason: "down".into(),
            since_ms: 0,
        }));
        assert!(!source_is_reachable(&SourceState::Retired));
        assert!(!source_is_reachable(&SourceState::Probing));
    }

    #[test]
    fn prune_gate_is_singleton() {
        let a = prune_gate() as *const _;
        let b = prune_gate() as *const _;
        assert_eq!(a, b);
    }
}
