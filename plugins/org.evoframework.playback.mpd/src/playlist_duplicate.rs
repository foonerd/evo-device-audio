// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Duplicate a stored playlist under a new name.
//!
//! One stored playlist becomes two. The source keeps its rows in
//! their order; the target is a separate stored playlist holding
//! the same rows. The target is written through MPD's own
//! `playlistadd`, in one command list, so it exists whole or not
//! at all. A target that already exists is refused — a duplicate
//! never overwrites. The favourites playlist is refused as source
//! and as target: its lifecycle is the favourites shelf's.
//!
//! Nothing is written to the music directory, or to any file:
//! the only write is MPD's, into its playlist directory.

use serde::Deserialize;

use crate::mpd::MpdConnection;
use crate::playlist::{
    handle_create_playlist, publish_index, validate_playlist_name,
    CreatePlaylistPayload, PlaylistContext, VerbError,
    PLAYLIST_PAYLOAD_VERSION,
};

/// `playlist.duplicate` request payload.
#[derive(Debug, Deserialize)]
pub(crate) struct DuplicatePayload {
    pub(crate) v: u32,
    /// The stored playlist to copy.
    pub(crate) source: String,
    /// The new stored playlist. Must not exist.
    pub(crate) target: String,
}

/// `playlist.duplicate` — copy `source` into a new `target`.
///
/// Refused before any write: a payload version this shelf does
/// not speak; an invalid source or target name; the favourites
/// name on either side; a target that already exists; a source
/// that does not. The source is read once and never written.
pub(crate) async fn handle_duplicate(
    ctx: &PlaylistContext,
    conn: &mut MpdConnection,
    payload: DuplicatePayload,
) -> Result<(), VerbError> {
    if payload.v != PLAYLIST_PAYLOAD_VERSION {
        return Err(VerbError::PayloadVersion {
            verb: "playlist.duplicate".to_string(),
            got: payload.v,
            expected: PLAYLIST_PAYLOAD_VERSION,
        });
    }
    validate_playlist_name(&payload.source)?;
    validate_playlist_name(&payload.target)?;
    for name in [&payload.source, &payload.target] {
        if *name == ctx.favourites_name {
            return Err(VerbError::FavouritesProtected { name: name.clone() });
        }
    }
    if payload.source == payload.target {
        return Err(VerbError::DuplicateName {
            name: payload.target.clone(),
        });
    }
    let mpd = |what: &str, e: crate::mpd::MpdError| VerbError::Mpd {
        verb: "duplicate".to_string(),
        reason: format!("{what}: {e}"),
    };

    // The target must not exist; the source must.
    let existing = conn
        .listplaylists()
        .await
        .map_err(|e| mpd("listplaylists", e))?;
    if existing.iter().any(|s| s.name == payload.target) {
        return Err(VerbError::DuplicateName {
            name: payload.target.clone(),
        });
    }
    if !existing.iter().any(|s| s.name == payload.source) {
        return Err(VerbError::NotFound {
            name: payload.source.clone(),
        });
    }
    let rows = match conn.listplaylistinfo(&payload.source).await {
        Ok(e) => e,
        Err(crate::mpd::MpdError::Ack { code: 50, .. }) => {
            return Err(VerbError::NotFound {
                name: payload.source.clone(),
            });
        }
        Err(e) => return Err(mpd("listplaylistinfo", e)),
    };

    if rows.is_empty() {
        // MPD creates a playlist by writing a row into it, so an
        // empty one is made the way `create_playlist` makes one:
        // seed and take the seed back out. That path publishes
        // the index itself.
        return handle_create_playlist(
            ctx,
            conn,
            CreatePlaylistPayload {
                v: PLAYLIST_PAYLOAD_VERSION,
                name: payload.target,
            },
        )
        .await;
    }

    // One command list for one round trip — but MPD applies each
    // `playlistadd` as it goes: when one is refused, the rows
    // before it have landed under the new name and the rest never
    // run. A list is not a transaction. So a refused row is
    // followed by `rm` of the name this call created, and the
    // operator gets the error with nothing left behind: the next
    // duplicate is not refused for a name that is only half
    // there. The target did not exist before this call — that was
    // checked above — so removing it removes only our own work.
    let commands: Vec<(&str, Vec<String>)> = rows
        .iter()
        .map(|r| {
            (
                "playlistadd",
                vec![payload.target.clone(), r.file_path.clone()],
            )
        })
        .collect();
    if let Err(e) = conn.command_list(&commands).await {
        if let Err(rm) = conn.rm_playlist(&payload.target).await {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                target = %payload.target,
                error = %rm,
                "duplicate: a row was refused and the partial copy could \
                 not be removed; the name is listed with part of the rows"
            );
        }
        publish_index(ctx, conn).await;
        return Err(mpd("playlistadd", e));
    }
    publish_index(ctx, conn).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playback_supervisor::test_mock::{
        short_timeouts, spawn_mock_mpd, CapturingSubjectAnnouncer,
        ConnBehaviour,
    };
    use crate::playlist::DEFAULT_FAVOURITES_PLAYLIST_NAME;
    use crate::source_registry::SourceRegistry;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    type Held = Arc<std::sync::Mutex<BTreeMap<String, Vec<String>>>>;

    struct Harness {
        ctx: PlaylistContext,
        conn: MpdConnection,
        held: Held,
        commands: Arc<std::sync::Mutex<Vec<String>>>,
        music: tempfile::TempDir,
        subjects: Arc<CapturingSubjectAnnouncer>,
    }

    fn road() -> Vec<String> {
        vec![
            "INTERNAL/a.flac".to_string(),
            "http://s.example/x".to_string(),
            "INTERNAL/b.flac".to_string(),
        ]
    }

    async fn harness() -> Harness {
        let held: Held = Arc::new(std::sync::Mutex::new(BTreeMap::from([
            (
                DEFAULT_FAVOURITES_PLAYLIST_NAME.to_string(),
                vec!["INTERNAL/loved.flac".to_string()],
            ),
            ("Road".to_string(), road()),
            ("Taken".to_string(), vec!["INTERNAL/t.flac".to_string()]),
            ("Empty".to_string(), Vec::new()),
        ])));
        let commands = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (endpoint, _mock) =
            spawn_mock_mpd(vec![ConnBehaviour::StoredPlaylists {
                commands: Arc::clone(&commands),
                playlists: Arc::clone(&held),
                library: vec![(
                    String::new(),
                    Vec::new(),
                    vec!["INTERNAL/seed.flac".to_string()],
                )],
                queue: Vec::new(),
            }])
            .await;
        let conn =
            MpdConnection::connect_with_timeouts(endpoint, short_timeouts())
                .await
                .unwrap();
        let music = tempfile::tempdir().unwrap();
        let subjects = Arc::new(CapturingSubjectAnnouncer::default());
        let ctx = PlaylistContext::new(
            music.path().to_path_buf(),
            SourceRegistry::new(),
            Arc::clone(&subjects)
                as Arc<dyn evo_plugin_sdk::contract::SubjectAnnouncer>,
            DEFAULT_FAVOURITES_PLAYLIST_NAME.to_string(),
            None,
        );
        Harness {
            ctx,
            conn,
            held,
            commands,
            music,
            subjects,
        }
    }

    fn payload(source: &str, target: &str) -> DuplicatePayload {
        DuplicatePayload {
            v: 1,
            source: source.to_string(),
            target: target.to_string(),
        }
    }

    /// One playlist becomes two: the source keeps its rows in
    /// order, the target holds the same rows, the target is
    /// written in one command list, and the index is published.
    /// Nothing is written beside the music.
    #[tokio::test]
    async fn the_source_keeps_its_rows_and_the_target_gets_them() {
        let mut h = harness().await;
        handle_duplicate(&h.ctx, &mut h.conn, payload("Road", "Road Copy"))
            .await
            .unwrap();
        let held = h.held.lock().unwrap();
        assert_eq!(
            held.get("Road").unwrap(),
            &road(),
            "the source is as it was"
        );
        assert_eq!(
            held.get("Road Copy").unwrap(),
            &road(),
            "the target has the rows"
        );
        let seen = h.commands.lock().unwrap();
        let adds = seen.iter().filter(|c| c.starts_with("playlistadd")).count();
        assert_eq!(adds, 3);
        assert!(
            seen.iter().all(|c| !c.starts_with("playlistadd \"Road\"")),
            "the source is never written: {seen:?}"
        );
        assert!(
            seen.iter().any(|c| c == "command_list_begin"),
            "the target is written in one list: {seen:?}"
        );
        assert!(
            seen.iter().all(|c| !c.starts_with("playlistclear")
                && !c.starts_with("rm")
                && !c.starts_with("rename")
                && !c.starts_with("playlistdelete")),
            "nothing is cleared, removed or renamed: {seen:?}"
        );
        assert!(
            h.subjects.state_update_count() > 0,
            "the index is published"
        );
        assert!(
            std::fs::read_dir(h.music.path()).unwrap().next().is_none(),
            "nothing is written beside the music"
        );
    }

    /// A target that already exists is refused and left exactly
    /// as it was. A duplicate never overwrites.
    #[tokio::test]
    async fn an_existing_target_is_refused_and_untouched() {
        let mut h = harness().await;
        let got =
            handle_duplicate(&h.ctx, &mut h.conn, payload("Road", "Taken"))
                .await;
        assert!(
            matches!(got, Err(VerbError::DuplicateName { .. })),
            "{got:?}"
        );
        assert_eq!(
            h.held.lock().unwrap().get("Taken").unwrap(),
            &vec!["INTERNAL/t.flac".to_string()]
        );
        assert!(
            h.commands
                .lock()
                .unwrap()
                .iter()
                .all(|c| !c.starts_with("playlistadd")
                    && !c.starts_with("playlistclear")),
            "a refusal writes nothing"
        );
        // Copying onto itself is the same refusal.
        let same =
            handle_duplicate(&h.ctx, &mut h.conn, payload("Road", "Road"))
                .await;
        assert!(matches!(same, Err(VerbError::DuplicateName { .. })));
    }

    /// Favourites is refused as the source and as the target,
    /// and is neither copied nor replaced.
    #[tokio::test]
    async fn favourites_is_refused_both_ways() {
        let mut h = harness().await;
        let as_source = handle_duplicate(
            &h.ctx,
            &mut h.conn,
            payload(DEFAULT_FAVOURITES_PLAYLIST_NAME, "Loved Copy"),
        )
        .await;
        assert!(
            matches!(as_source, Err(VerbError::FavouritesProtected { .. })),
            "{as_source:?}"
        );
        let as_target = handle_duplicate(
            &h.ctx,
            &mut h.conn,
            payload("Road", DEFAULT_FAVOURITES_PLAYLIST_NAME),
        )
        .await;
        assert!(
            matches!(as_target, Err(VerbError::FavouritesProtected { .. })),
            "{as_target:?}"
        );
        let held = h.held.lock().unwrap();
        assert!(
            held.get("Loved Copy").is_none(),
            "favourites not duplicated"
        );
        assert_eq!(
            held.get(DEFAULT_FAVOURITES_PLAYLIST_NAME).unwrap(),
            &vec!["INTERNAL/loved.flac".to_string()],
            "favourites not replaced"
        );
        assert!(h
            .commands
            .lock()
            .unwrap()
            .iter()
            .all(|c| !c.starts_with("playlistadd")));
    }

    /// A source that does not exist, or a bad name on either
    /// side, is refused before MPD is written.
    #[tokio::test]
    async fn a_missing_source_or_a_bad_name_is_refused() {
        let mut h = harness().await;
        let missing =
            handle_duplicate(&h.ctx, &mut h.conn, payload("Nope", "New")).await;
        assert!(
            matches!(missing, Err(VerbError::NotFound { .. })),
            "{missing:?}"
        );
        for (s, t) in
            [("a/b", "New"), ("Road", "a/b"), ("", "New"), ("Road", "")]
        {
            let got =
                handle_duplicate(&h.ctx, &mut h.conn, payload(s, t)).await;
            assert!(
                matches!(got, Err(VerbError::InvalidName { .. })),
                "{s:?} -> {t:?}: {got:?}"
            );
        }
        assert!(h.held.lock().unwrap().get("New").is_none());
        assert!(h
            .commands
            .lock()
            .unwrap()
            .iter()
            .all(|c| !c.starts_with("playlistadd")));
    }

    /// A row MPD refuses mid-copy does not leave the new name
    /// behind. The rows before it had landed; the partial copy is
    /// removed before the error is returned, the source is as it
    /// was, and the name is free for the next attempt.
    #[tokio::test]
    async fn a_failed_copy_leaves_no_name_behind() {
        let mut h = harness().await;
        h.held.lock().unwrap().insert(
            "Mixed".to_string(),
            vec![
                "INTERNAL/ok1.flac".to_string(),
                "refused:INTERNAL/gone.flac".to_string(),
                "INTERNAL/ok2.flac".to_string(),
            ],
        );
        let got = handle_duplicate(
            &h.ctx,
            &mut h.conn,
            payload("Mixed", "Mixed Copy"),
        )
        .await;
        assert!(matches!(got, Err(VerbError::Mpd { .. })), "{got:?}");
        {
            let held = h.held.lock().unwrap();
            assert!(
                held.get("Mixed Copy").is_none(),
                "the partial copy must be gone from the index: {held:?}"
            );
            assert_eq!(
                held.get("Mixed").unwrap().len(),
                3,
                "the source is as it was"
            );
            let seen = h.commands.lock().unwrap();
            assert!(
                seen.iter().any(|c| c == "rm \"Mixed Copy\""),
                "the name this call created is removed: {seen:?}"
            );
            assert!(
                seen.iter().all(|c| c != "rm \"Mixed\""),
                "the source is never removed: {seen:?}"
            );
        }
        // And the name is free: a second attempt is not refused
        // for a name that is only half there.
        let again = handle_duplicate(
            &h.ctx,
            &mut h.conn,
            payload("Mixed", "Mixed Copy"),
        )
        .await;
        assert!(
            !matches!(again, Err(VerbError::DuplicateName { .. })),
            "the next attempt must not be refused as a duplicate: {again:?}"
        );
    }

    /// An empty playlist duplicates to an empty playlist.
    #[tokio::test]
    async fn an_empty_source_makes_an_empty_target() {
        let mut h = harness().await;
        handle_duplicate(&h.ctx, &mut h.conn, payload("Empty", "Empty Copy"))
            .await
            .unwrap();
        let held = h.held.lock().unwrap();
        assert_eq!(held.get("Empty Copy").unwrap(), &Vec::<String>::new());
        assert_eq!(held.get("Empty").unwrap(), &Vec::<String>::new());
    }

    /// Where nothing is written beside the music: the module has
    /// no filesystem use at all.
    #[test]
    fn the_duplicate_writes_no_file() {
        let src = include_str!("playlist_duplicate.rs");
        let production = src.split("#[cfg(test)]").next().unwrap();
        for forbidden in [
            "std::fs",
            "tokio::fs",
            "File::create",
            "OpenOptions",
            "fs::write",
            "fs::rename",
            "fs::copy",
            "write_all",
        ] {
            assert!(
                !production.contains(forbidden),
                "the duplicate module must not write a file; found {forbidden:?}"
            );
        }
    }
}
