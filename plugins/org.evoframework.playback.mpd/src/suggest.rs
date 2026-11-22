// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Suggestions while the operator is still typing.
//!
//! `library.suggest` takes a prefix and answers with up to eight
//! strings that begin with it — artist names, album titles and
//! track titles from the MPD library — case-insensitively, with
//! no duplicates. An empty prefix is an empty list. The title of a
//! forgotten track is not offered.
//!
//! This is not the search. It reads MPD's own tag lists and one
//! title search, and nothing else: no DLNA walk, no cloud named,
//! no cloud invented. The submitted search is
//! `library.search_library`, unchanged.

use serde::Deserialize;
use serde_json::json;

use crate::library::{LibraryContext, VerbError, LIBRARY_PAYLOAD_VERSION};
use crate::mpd::{MpdConnection, MpdLibraryEntry, MpdSearchField};

/// How many suggestions one answer carries at most.
pub(crate) const SUGGEST_CAP: usize = 8;

/// `library.suggest` request payload.
#[derive(Debug, Deserialize)]
pub(crate) struct SuggestPayload {
    pub(crate) v: u32,
    pub(crate) prefix: String,
}

/// The pure choice: artists, then albums, then track titles, each
/// in the order given, kept when it begins with the prefix
/// case-insensitively, deduplicated case-insensitively, at most
/// [`SUGGEST_CAP`]. A title whose only carriers are forgotten is
/// not offered; one a kept track also carries still is.
pub(crate) fn suggest_from(
    prefix: &str,
    artists: &[String],
    albums: &[String],
    tracks: &[(String, Option<String>)],
    forgotten: &std::collections::BTreeSet<String>,
) -> Vec<String> {
    let prefix = prefix.trim();
    if prefix.is_empty() {
        return Vec::new();
    }
    let needle = prefix.to_lowercase();
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut offer = |candidate: &str| {
        if out.len() >= SUGGEST_CAP {
            return;
        }
        let c = candidate.trim();
        if c.is_empty() {
            return;
        }
        let lower = c.to_lowercase();
        if !lower.starts_with(&needle) {
            return;
        }
        if seen.insert(lower) {
            out.push(c.to_string());
        }
    };
    for a in artists {
        offer(a);
    }
    for a in albums {
        offer(a);
    }
    for (path, title) in tracks {
        if forgotten.contains(path) {
            continue;
        }
        if let Some(t) = title {
            offer(t);
        }
    }
    out
}

/// `library.suggest` — three reads of the MPD library: the
/// artist list, the album list, and a title search for the
/// prefix. Nothing else is asked.
pub(crate) async fn handle_suggest(
    ctx: &LibraryContext,
    conn: &mut MpdConnection,
    payload: SuggestPayload,
) -> Result<serde_json::Value, VerbError> {
    if payload.v != LIBRARY_PAYLOAD_VERSION {
        return Err(VerbError::PayloadVersion {
            verb: "library.suggest".to_string(),
            got: payload.v,
            expected: LIBRARY_PAYLOAD_VERSION,
        });
    }
    let prefix = payload.prefix.trim();
    if prefix.is_empty() {
        return Ok(json!({
            "v": LIBRARY_PAYLOAD_VERSION,
            "suggestions": Vec::<String>::new(),
        }));
    }
    let mpd = |what: &str, e: crate::mpd::MpdError| VerbError::Mpd {
        verb: "suggest".to_string(),
        reason: format!("{what}: {e}"),
    };
    let artists = conn
        .list_tag("artist")
        .await
        .map_err(|e| mpd("list artist", e))?;
    let albums = conn
        .list_tag("album")
        .await
        .map_err(|e| mpd("list album", e))?;
    // MPD's `search` is a substring match; the prefix rule is
    // applied on top of what comes back.
    let tracks: Vec<(String, Option<String>)> = conn
        .search(MpdSearchField::Title, prefix)
        .await
        .map_err(|e| mpd("search title", e))?
        .into_iter()
        .filter_map(|e| match e {
            MpdLibraryEntry::File { path, title, .. } => Some((path, title)),
            _ => None,
        })
        .collect();
    let forgotten = ctx.forgotten.snapshot().await;
    let suggestions =
        suggest_from(prefix, &artists, &albums, &tracks, &forgotten);
    Ok(json!({
        "v": LIBRARY_PAYLOAD_VERSION,
        "suggestions": suggestions,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// An empty prefix — or one that is only spaces — is an empty
    /// list, whatever the library holds.
    #[test]
    fn an_empty_prefix_is_an_empty_list() {
        let artists = s(&["Low"]);
        for p in ["", "   "] {
            assert!(suggest_from(p, &artists, &[], &[], &BTreeSet::new())
                .is_empty());
        }
    }

    /// Only strings that begin with the prefix, case-insensitively;
    /// a string that merely contains it is not offered.
    #[test]
    fn only_prefixes_are_offered() {
        let artists = s(&["Low", "Slowdive", "LOW ROAR", "Lower Dens"]);
        let albums = s(&["Lowlife", "Things We Lost in the Fire"]);
        let got = suggest_from("lo", &artists, &albums, &[], &BTreeSet::new());
        assert_eq!(got, s(&["Low", "LOW ROAR", "Lower Dens", "Lowlife"]));
    }

    /// No duplicates, case-insensitively; the first spelling wins.
    #[test]
    fn duplicates_are_folded() {
        let artists = s(&["Low", "low", "LOW"]);
        let albums = s(&["Low"]);
        let tracks = vec![("a/1.flac".to_string(), Some("Low".to_string()))];
        let got =
            suggest_from("l", &artists, &albums, &tracks, &BTreeSet::new());
        assert_eq!(got, s(&["Low"]));
    }

    /// Never more than eight, artists first, then albums, then
    /// titles.
    #[test]
    fn the_list_stops_at_eight() {
        let artists = s(&["A1", "A2", "A3", "A4"]);
        let albums = s(&["A5", "A6", "A7"]);
        let tracks: Vec<(String, Option<String>)> = (8..=12)
            .map(|i| (format!("t/{i}.flac"), Some(format!("A{i}"))))
            .collect();
        let got =
            suggest_from("a", &artists, &albums, &tracks, &BTreeSet::new());
        assert_eq!(got.len(), SUGGEST_CAP);
        assert_eq!(got, s(&["A1", "A2", "A3", "A4", "A5", "A6", "A7", "A8"]));
    }

    /// A forgotten track's title is not offered — unless a kept
    /// track carries the same title.
    #[test]
    fn a_forgotten_tracks_title_is_not_offered() {
        let tracks = vec![
            ("x/gone.flac".to_string(), Some("Words".to_string())),
            ("x/kept.flac".to_string(), Some("Canada".to_string())),
            ("x/also-gone.flac".to_string(), Some("Canada".to_string())),
        ];
        let forgotten: BTreeSet<String> =
            ["x/gone.flac".to_string(), "x/also-gone.flac".to_string()].into();
        let got = suggest_from("", &[], &[], &tracks, &forgotten);
        assert!(got.is_empty());
        let got = suggest_from("w", &[], &[], &tracks, &forgotten);
        assert!(got.is_empty(), "the forgotten track's title: {got:?}");
        let got = suggest_from("c", &[], &[], &tracks, &forgotten);
        assert_eq!(got, s(&["Canada"]), "still carried by a kept track");
    }

    /// Nothing in this module reaches a peer shelf or a DLNA
    /// browse: the production source names no dispatcher.
    #[test]
    fn the_suggest_verb_walks_no_dlna() {
        let src = include_str!("suggest.rs");
        let production = src.split("#[cfg(test)]").next().unwrap();
        for forbidden in [
            "shelf_dispatcher",
            "dispatch(",
            "source.dlna",
            "dlna_search",
            "browse_dlna",
            "Cloud",
        ] {
            assert!(
                !production.contains(forbidden),
                "suggest must not walk DLNA or name cloud; found {forbidden:?}"
            );
        }
    }

    // ===== against the mock =====

    use crate::playback_supervisor::test_mock::{
        short_timeouts, spawn_mock_mpd, CapturingSubjectAnnouncer,
        ConnBehaviour,
    };
    use crate::source_registry::SourceRegistry;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    /// The verb asks MPD for its artist list, its album list and
    /// a title search — and nothing else — and answers from them.
    /// A forgotten track's title is left out on the wire too.
    #[tokio::test]
    async fn the_verb_reads_three_lists_and_nothing_else() {
        let commands = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (endpoint, _mock) =
            spawn_mock_mpd(vec![ConnBehaviour::StoredPlaylists {
                commands: Arc::clone(&commands),
                playlists: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
                library: vec![(
                    String::new(),
                    Vec::new(),
                    vec![
                        "Low/Trust/Lonely.flac".to_string(),
                        "Low/Trust/Words.flac".to_string(),
                        "Lorde/Melodrama/Liability.flac".to_string(),
                        "Other/Lowlife/Loop.flac".to_string(),
                    ],
                )],
                queue: Vec::new(),
            }])
            .await;
        let mut conn =
            MpdConnection::connect_with_timeouts(endpoint, short_timeouts())
                .await
                .unwrap();
        let ctx = LibraryContext::new(
            std::path::PathBuf::from("/m"),
            SourceRegistry::new(),
            Arc::new(CapturingSubjectAnnouncer::default()),
            None,
        );
        ctx.forgotten.forget("Low/Trust/Lonely.flac").await;

        let v = handle_suggest(
            &ctx,
            &mut conn,
            SuggestPayload {
                v: 1,
                prefix: "lo".into(),
            },
        )
        .await;
        let v = v.unwrap();
        let got: Vec<&str> = v["suggestions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        // Artists Low, Lorde; albums Lowlife; titles Loop
        // (Lonely is forgotten, Words and Liability are not
        // prefixes).
        assert_eq!(got, vec!["Low", "Lorde", "Lowlife", "Loop"], "{v}");

        let seen = commands.lock().unwrap().clone();
        let reads: Vec<&String> = seen
            .iter()
            .filter(|c| !c.starts_with("list \"") && !c.starts_with("search "))
            .collect();
        assert!(reads.is_empty(), "only the three reads: {seen:?}");
        assert_eq!(seen.iter().filter(|c| c.starts_with("list \"")).count(), 2);
        assert_eq!(
            seen.iter()
                .filter(|c| c.starts_with("search \"title\""))
                .count(),
            1
        );

        // An empty prefix asks MPD nothing at all.
        let before = commands.lock().unwrap().len();
        let v = handle_suggest(
            &ctx,
            &mut conn,
            SuggestPayload {
                v: 1,
                prefix: "  ".into(),
            },
        )
        .await
        .unwrap();
        assert!(v["suggestions"].as_array().unwrap().is_empty());
        assert_eq!(commands.lock().unwrap().len(), before, "nothing asked");
    }
}
