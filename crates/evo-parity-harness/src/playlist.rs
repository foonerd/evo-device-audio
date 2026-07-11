// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Playlist-shelf parity round-trips.
//!
//! Drives every mutating verb on `audio.playlist`. The
//! sequence creates a uniquely-named test playlist so it
//! cannot collide with the rig's existing stored playlists,
//! mutates it, then deletes it. The harness re-runs cleanly
//! because the trailing delete restores the rig's pre-state.
//!
//! Test sequence:
//! 1. `playlist.create_playlist` — make `__evo_parity_test__`.
//! 2. `playlist.list_playlists` — observe the index includes
//!    the new entry.
//! 3. `playlist.rename_playlist` — rename to
//!    `__evo_parity_renamed__`.
//! 4. `playlist.get_playlist` — observe the renamed playlist
//!    is present.
//! 5. `playlist.delete_playlist` — clean up.
//! 6. `playlist.list_playlists` — observe the index no
//!    longer includes the test entry.
//! 7. NEGATIVE: `playlist.delete_playlist` on the favourites
//!    reserved name (`__favourites__`) — refuse with
//!    `favourites_protected`.

use anyhow::Result;
use serde_json::{json, Value};

use crate::report::CaseResult;
use crate::wire::Wire;

const SHELF: &str = "audio.playlist";
const TEST_NAME: &str = "__evo_parity_test__";
const RENAMED_NAME: &str = "__evo_parity_renamed__";
const FAVOURITES_NAME: &str = "__favourites__";

pub async fn run(wire: &mut Wire) -> Result<Vec<CaseResult>> {
    let mut cases = Vec::new();
    // Defensive pre-clean: a prior interrupted run may have
    // left either of the two test names behind. Best-effort
    // delete; ignore failures.
    let _ = wire
        .request(
            SHELF,
            "playlist.delete_playlist",
            json!({ "v": 1, "name": TEST_NAME }),
        )
        .await;
    let _ = wire
        .request(
            SHELF,
            "playlist.delete_playlist",
            json!({ "v": 1, "name": RENAMED_NAME }),
        )
        .await;

    cases.push(case_create(wire).await);
    cases.push(case_list_includes_created(wire).await);
    cases.push(case_rename(wire).await);
    cases.push(case_get_renamed(wire).await);
    cases.push(case_delete(wire).await);
    cases.push(case_list_excludes_deleted(wire).await);
    cases.push(case_favourites_delete_refused(wire).await);
    Ok(cases)
}

async fn case_create(wire: &mut Wire) -> CaseResult {
    match wire
        .request(
            SHELF,
            "playlist.create_playlist",
            json!({ "v": 1, "name": TEST_NAME }),
        )
        .await
    {
        Ok(_) => CaseResult::pass(
            "playlist.create",
            SHELF,
            format!("create_playlist({TEST_NAME:?})"),
        ),
        Err(e) => CaseResult::fail(
            "playlist.create",
            SHELF,
            format!("create_playlist({TEST_NAME:?})"),
            e.to_string(),
        ),
    }
}

async fn case_list_includes_created(wire: &mut Wire) -> CaseResult {
    match list_playlist_names(wire).await {
        Ok(names) => {
            if names.contains(&TEST_NAME.to_string()) {
                CaseResult::pass_with(
                    "playlist.list_includes_created",
                    SHELF,
                    "list_playlists shows the freshly-created entry",
                    json!({ "names": names }),
                )
            } else {
                CaseResult::fail_with(
                    "playlist.list_includes_created",
                    SHELF,
                    "list_playlists shows the freshly-created entry",
                    format!("expected {TEST_NAME:?} in playlists list"),
                    json!({ "names": names }),
                )
            }
        }
        Err(e) => CaseResult::fail(
            "playlist.list_includes_created",
            SHELF,
            "list_playlists shows the freshly-created entry",
            e.to_string(),
        ),
    }
}

async fn case_rename(wire: &mut Wire) -> CaseResult {
    match wire
        .request(
            SHELF,
            "playlist.rename_playlist",
            json!({ "v": 1, "from_name": TEST_NAME, "to_name": RENAMED_NAME }),
        )
        .await
    {
        Ok(_) => CaseResult::pass(
            "playlist.rename",
            SHELF,
            format!("rename_playlist({TEST_NAME:?} -> {RENAMED_NAME:?})"),
        ),
        Err(e) => CaseResult::fail(
            "playlist.rename",
            SHELF,
            format!("rename_playlist({TEST_NAME:?} -> {RENAMED_NAME:?})"),
            e.to_string(),
        ),
    }
}

async fn case_get_renamed(wire: &mut Wire) -> CaseResult {
    match wire
        .request(
            SHELF,
            "playlist.get_playlist",
            json!({ "v": 1, "name": RENAMED_NAME }),
        )
        .await
    {
        Ok(env) => {
            let name = env.get("name").and_then(Value::as_str);
            if name == Some(RENAMED_NAME) {
                CaseResult::pass_with(
                    "playlist.get_renamed",
                    SHELF,
                    "get_playlist returns the renamed entry",
                    json!({ "name": name }),
                )
            } else {
                CaseResult::fail_with(
                    "playlist.get_renamed",
                    SHELF,
                    "get_playlist returns the renamed entry",
                    format!("expected name={RENAMED_NAME:?}; got {name:?}"),
                    json!({ "envelope": env }),
                )
            }
        }
        Err(e) => CaseResult::fail(
            "playlist.get_renamed",
            SHELF,
            "get_playlist returns the renamed entry",
            e.to_string(),
        ),
    }
}

async fn case_delete(wire: &mut Wire) -> CaseResult {
    match wire
        .request(
            SHELF,
            "playlist.delete_playlist",
            json!({ "v": 1, "name": RENAMED_NAME }),
        )
        .await
    {
        Ok(_) => CaseResult::pass(
            "playlist.delete",
            SHELF,
            format!("delete_playlist({RENAMED_NAME:?})"),
        ),
        Err(e) => CaseResult::fail(
            "playlist.delete",
            SHELF,
            format!("delete_playlist({RENAMED_NAME:?})"),
            e.to_string(),
        ),
    }
}

async fn case_list_excludes_deleted(wire: &mut Wire) -> CaseResult {
    match list_playlist_names(wire).await {
        Ok(names) => {
            let leaked =
                names.iter().any(|n| n == TEST_NAME || n == RENAMED_NAME);
            if leaked {
                CaseResult::fail_with(
                    "playlist.list_excludes_deleted",
                    SHELF,
                    "list_playlists no longer shows the test entries",
                    format!(
                        "test entries leaked: TEST={TEST_NAME:?} \
                         RENAMED={RENAMED_NAME:?}"
                    ),
                    json!({ "names": names }),
                )
            } else {
                CaseResult::pass_with(
                    "playlist.list_excludes_deleted",
                    SHELF,
                    "list_playlists no longer shows the test entries",
                    json!({ "names_count": names.len() }),
                )
            }
        }
        Err(e) => CaseResult::fail(
            "playlist.list_excludes_deleted",
            SHELF,
            "list_playlists no longer shows the test entries",
            e.to_string(),
        ),
    }
}

async fn case_favourites_delete_refused(wire: &mut Wire) -> CaseResult {
    match wire
        .request_expect_error(
            SHELF,
            "playlist.delete_playlist",
            json!({ "v": 1, "name": FAVOURITES_NAME }),
        )
        .await
    {
        Ok(err) => {
            if err.message.contains("favourites") {
                CaseResult::pass_with(
                    "playlist.favourites_delete_refused",
                    SHELF,
                    "delete_playlist on reserved favourites name refuses",
                    json!({
                        "class":    err.class,
                        "subclass": err.subclass,
                        "message":  err.message,
                    }),
                )
            } else {
                CaseResult::fail_with(
                    "playlist.favourites_delete_refused",
                    SHELF,
                    "delete_playlist on reserved favourites name refuses",
                    "error did not reference favourites protection",
                    json!({
                        "class":    err.class,
                        "subclass": err.subclass,
                        "message":  err.message,
                    }),
                )
            }
        }
        Err(e) => CaseResult::fail(
            "playlist.favourites_delete_refused",
            SHELF,
            "delete_playlist on reserved favourites name refuses",
            e.to_string(),
        ),
    }
}

async fn list_playlist_names(wire: &mut Wire) -> Result<Vec<String>> {
    let env = wire
        .request(SHELF, "playlist.list_playlists", json!({ "v": 1 }))
        .await?;
    let names = env
        .get("playlists")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|p| {
                    p.get("name").and_then(Value::as_str).map(String::from)
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(names)
}
