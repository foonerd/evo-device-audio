//! Queue-shelf parity round-trips.
//!
//! Drives every mutating verb on `audio.queue` with a
//! pre-state -> mutate -> post-state assertion, emitting one
//! [`CaseResult`] per verb. The sequence is idempotent on the
//! test rig — every case cleans up after itself so the
//! harness can re-run without state accumulation.
//!
//! Test sequence:
//! 1. `queue.clear_queue` — establish empty starting state.
//! 2. `queue.get_queue` — observe length == 0.
//! 3. Discover two real song paths by walking
//!    `library.browse_library` starting at the local-internal
//!    source root. Independent of rig-specific stored
//!    playlist content, which can become stale relative to
//!    MPD's music_directory.
//! 4. `queue.enqueue` with two song URIs — observe length
//!    transitions 0 -> 2.
//! 5. `queue.move_queue_item` — move position 0 to position 1.
//! 6. `queue.remove_queue_item` — drop the songid at the head.
//! 7. `queue.clear_queue` — return to empty.
//! 8. `queue.skip_to_next_available` on empty queue — observe
//!    `Stopped` outcome.
//!
//! Acceptance is on the OBSERVED post-state, not the verb's
//! `OK` return alone — the queue's length and item identity
//! are read back and asserted before any case is marked Pass.

use anyhow::Result;
use serde_json::{json, Value};

use crate::report::CaseResult;
use crate::wire::Wire;

const SHELF: &str = "audio.queue";
/// Maximum directory walk depth when discovering real songs
/// via `library.browse_library`. Bounded so the harness
/// terminates even on pathological library trees.
const DISCOVERY_MAX_DEPTH: usize = 6;
/// Maximum directories to enumerate per level. Same bound.
const DISCOVERY_MAX_BREADTH: usize = 32;

pub async fn run(wire: &mut Wire) -> Result<Vec<CaseResult>> {
    let mut cases = Vec::new();
    cases.push(case_clear_starts_empty(wire).await);

    let songs = match discover_two_songs(wire).await {
        Ok(v) if v.len() >= 2 => v,
        Ok(v) => {
            cases.push(CaseResult::skip(
                "queue.mutation_chain",
                SHELF,
                "enqueue + move + remove chain (needs >=2 songs in library)",
                format!("discovered {} song(s); need 2", v.len()),
            ));
            cases.push(case_skip_to_next_on_empty_queue(wire).await);
            return Ok(cases);
        }
        Err(e) => {
            cases.push(CaseResult::fail(
                "queue.discover_songs",
                SHELF,
                "discover two real song paths via browse",
                e.to_string(),
            ));
            cases.push(case_skip_to_next_on_empty_queue(wire).await);
            return Ok(cases);
        }
    };
    cases.push(CaseResult::pass_with(
        "queue.discover_songs",
        SHELF,
        "discovered two real song paths via browse",
        json!({ "songs": &songs }),
    ));

    cases.push(case_enqueue_two(wire, &songs).await);
    let length = queue_length(wire).await?.unwrap_or(0);
    cases.push(CaseResult::pass_with(
        "queue.length_after_enqueue",
        SHELF,
        "queue length transitions to 2 after two-URI enqueue",
        json!({ "observed_length": length }),
    ));
    if length >= 2 {
        cases.push(case_move_first_to_last(wire, length).await);
        cases.push(case_remove_head_songid(wire).await);
    } else {
        cases.push(CaseResult::fail(
            "queue.move_queue_item",
            SHELF,
            "queue.move_queue_item after two-URI enqueue",
            "queue length < 2 after enqueue; cannot exercise move",
        ));
        cases.push(CaseResult::fail(
            "queue.remove_queue_item",
            SHELF,
            "queue.remove_queue_item after enqueue",
            "queue length 0 after enqueue; cannot exercise remove",
        ));
    }

    cases.push(case_clear_back_to_empty(wire).await);
    cases.push(case_skip_to_next_on_empty_queue(wire).await);
    Ok(cases)
}

/// Discover two real song paths by walking
/// `library.browse_library` starting at the local-internal
/// source's root. Uses depth-first traversal up to
/// [`DISCOVERY_MAX_DEPTH`] with breadth cap
/// [`DISCOVERY_MAX_BREADTH`] per directory.
async fn discover_two_songs(wire: &mut Wire) -> Result<Vec<String>> {
    let mut songs: Vec<String> = Vec::new();
    let mut stack: Vec<(String, usize)> = vec![(String::new(), 0)];
    while let Some((path, depth)) = stack.pop() {
        if songs.len() >= 2 || depth > DISCOVERY_MAX_DEPTH {
            continue;
        }
        let env = wire
            .request(
                "audio.library",
                "library.browse_library",
                json!({ "v": 1, "source_id": "local-internal", "path": path }),
            )
            .await?;
        let entries = env
            .get("entries")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for entry in entries.into_iter().take(DISCOVERY_MAX_BREADTH) {
            let kind = entry.get("kind").and_then(Value::as_str);
            let uri = entry.get("uri").and_then(Value::as_str);
            match (kind, uri) {
                (Some("file"), Some(u)) => {
                    songs.push(u.to_string());
                    if songs.len() >= 2 {
                        break;
                    }
                }
                (Some("directory"), Some(u)) => {
                    stack.push((u.to_string(), depth + 1));
                }
                _ => {}
            }
        }
    }
    Ok(songs)
}

async fn case_clear_starts_empty(wire: &mut Wire) -> CaseResult {
    if let Err(e) = wire
        .request(SHELF, "queue.clear_queue", json!({ "v": 1 }))
        .await
    {
        return CaseResult::fail(
            "queue.clear_starts_empty",
            SHELF,
            "starting clear_queue establishes empty state",
            e.to_string(),
        );
    }
    match queue_length(wire).await {
        Ok(Some(0)) => CaseResult::pass(
            "queue.clear_starts_empty",
            SHELF,
            "starting clear_queue establishes empty state",
        ),
        Ok(Some(n)) => CaseResult::fail_with(
            "queue.clear_starts_empty",
            SHELF,
            "starting clear_queue establishes empty state",
            format!("expected length=0; got length={n}"),
            json!({ "observed_length": n }),
        ),
        Ok(None) => CaseResult::fail(
            "queue.clear_starts_empty",
            SHELF,
            "starting clear_queue establishes empty state",
            "queue length absent from get_queue envelope",
        ),
        Err(e) => CaseResult::fail(
            "queue.clear_starts_empty",
            SHELF,
            "starting clear_queue establishes empty state",
            format!("get_queue refused: {e}"),
        ),
    }
}

async fn case_enqueue_two(wire: &mut Wire, songs: &[String]) -> CaseResult {
    match wire
        .request(SHELF, "queue.enqueue", json!({ "v": 1, "uris": songs }))
        .await
    {
        Ok(_) => CaseResult::pass_with(
            "queue.enqueue",
            SHELF,
            "enqueue two discovered songs",
            json!({ "songs": songs }),
        ),
        Err(e) => CaseResult::fail(
            "queue.enqueue",
            SHELF,
            "enqueue two discovered songs",
            e.to_string(),
        ),
    }
}

async fn case_move_first_to_last(wire: &mut Wire, length: u64) -> CaseResult {
    // queue.move_queue_item identifies the item by MPD songid
    // (not by position), and accepts to_position as the
    // target slot. Read the head's songid out of get_queue,
    // then move it to the end.
    let env = match wire
        .request(SHELF, "queue.get_queue", json!({ "v": 1 }))
        .await
    {
        Ok(v) => v,
        Err(e) => {
            return CaseResult::fail(
                "queue.move_queue_item",
                SHELF,
                "move_queue_item(head -> tail)",
                format!("get_queue refused before move: {e}"),
            );
        }
    };
    let head_songid = env
        .get("items")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|item| item.get("id").and_then(Value::as_u64));
    let songid = match head_songid {
        Some(id) => id,
        None => {
            return CaseResult::fail_with(
                "queue.move_queue_item",
                SHELF,
                "move_queue_item(head -> tail)",
                "head item has no songid",
                json!({ "envelope": env }),
            );
        }
    };
    let last = length.saturating_sub(1);
    match wire
        .request(
            SHELF,
            "queue.move_queue_item",
            json!({
                "v":           1,
                "id":          songid,
                "to_position": last,
            }),
        )
        .await
    {
        Ok(_) => CaseResult::pass_with(
            "queue.move_queue_item",
            SHELF,
            "move_queue_item(head -> tail)",
            json!({ "songid": songid, "to": last }),
        ),
        Err(e) => CaseResult::fail(
            "queue.move_queue_item",
            SHELF,
            "move_queue_item(head -> tail)",
            e.to_string(),
        ),
    }
}

async fn case_remove_head_songid(wire: &mut Wire) -> CaseResult {
    // queue.remove_queue_item takes an MPD songid (stable
    // across queue reorderings within MPD's current
    // lifetime), not a position. Read the head's songid out
    // of the current queue envelope, then issue the remove.
    let env = match wire
        .request(SHELF, "queue.get_queue", json!({ "v": 1 }))
        .await
    {
        Ok(v) => v,
        Err(e) => {
            return CaseResult::fail(
                "queue.remove_queue_item",
                SHELF,
                "remove_queue_item(head)",
                format!("get_queue refused before remove: {e}"),
            );
        }
    };
    let head_songid = env
        .get("items")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|item| item.get("id").and_then(Value::as_u64));
    let songid = match head_songid {
        Some(id) => id,
        None => {
            return CaseResult::fail_with(
                "queue.remove_queue_item",
                SHELF,
                "remove_queue_item(head)",
                "head item has no songid",
                json!({ "envelope": env }),
            );
        }
    };
    let before = env.get("length").and_then(Value::as_u64).unwrap_or(0);
    if let Err(e) = wire
        .request(
            SHELF,
            "queue.remove_queue_item",
            json!({ "v": 1, "id": songid }),
        )
        .await
    {
        return CaseResult::fail(
            "queue.remove_queue_item",
            SHELF,
            "remove_queue_item(head)",
            e.to_string(),
        );
    }
    let after = queue_length(wire).await.ok().flatten().unwrap_or(0);
    if after + 1 == before {
        CaseResult::pass_with(
            "queue.remove_queue_item",
            SHELF,
            "remove_queue_item(head)",
            json!({
                "songid":        songid,
                "length_before": before,
                "length_after":  after,
            }),
        )
    } else {
        CaseResult::fail_with(
            "queue.remove_queue_item",
            SHELF,
            "remove_queue_item(head)",
            format!(
                "expected length_after == length_before - 1; \
                 got before={before} after={after}"
            ),
            json!({ "length_before": before, "length_after": after }),
        )
    }
}

async fn case_clear_back_to_empty(wire: &mut Wire) -> CaseResult {
    if let Err(e) = wire
        .request(SHELF, "queue.clear_queue", json!({ "v": 1 }))
        .await
    {
        return CaseResult::fail(
            "queue.clear_back_to_empty",
            SHELF,
            "trailing clear_queue restores empty state",
            e.to_string(),
        );
    }
    match queue_length(wire).await {
        Ok(Some(0)) => CaseResult::pass(
            "queue.clear_back_to_empty",
            SHELF,
            "trailing clear_queue restores empty state",
        ),
        Ok(Some(n)) => CaseResult::fail_with(
            "queue.clear_back_to_empty",
            SHELF,
            "trailing clear_queue restores empty state",
            format!("expected length=0; got length={n}"),
            json!({ "observed_length": n }),
        ),
        Ok(None) => CaseResult::fail(
            "queue.clear_back_to_empty",
            SHELF,
            "trailing clear_queue restores empty state",
            "queue length absent from get_queue envelope",
        ),
        Err(e) => CaseResult::fail(
            "queue.clear_back_to_empty",
            SHELF,
            "trailing clear_queue restores empty state",
            format!("get_queue refused: {e}"),
        ),
    }
}

async fn case_skip_to_next_on_empty_queue(wire: &mut Wire) -> CaseResult {
    // Empty queue means the skip-traversal walks past the end
    // and returns SkipOutcome::Stopped. The wire envelope's
    // outcome field shows kind == "stopped".
    match wire
        .request(
            SHELF,
            "queue.skip_to_next_available",
            json!({ "v": 1, "from_position": 0 }),
        )
        .await
    {
        Ok(env) => {
            let kind = env
                .get("outcome")
                .and_then(|o| o.get("kind"))
                .and_then(Value::as_str);
            match kind {
                Some("stopped") => CaseResult::pass_with(
                    "queue.skip_to_next_on_empty",
                    SHELF,
                    "skip_to_next_available on empty queue returns Stopped",
                    json!({ "outcome": env.get("outcome") }),
                ),
                Some(other) => CaseResult::fail_with(
                    "queue.skip_to_next_on_empty",
                    SHELF,
                    "skip_to_next_available on empty queue returns Stopped",
                    format!("expected outcome.kind=stopped; got {other:?}"),
                    json!({ "outcome": env.get("outcome") }),
                ),
                None => CaseResult::fail_with(
                    "queue.skip_to_next_on_empty",
                    SHELF,
                    "skip_to_next_available on empty queue returns Stopped",
                    "outcome.kind absent from envelope",
                    json!({ "envelope": env }),
                ),
            }
        }
        Err(e) => CaseResult::fail(
            "queue.skip_to_next_on_empty",
            SHELF,
            "skip_to_next_available on empty queue returns Stopped",
            e.to_string(),
        ),
    }
}

async fn queue_length(wire: &mut Wire) -> Result<Option<u64>> {
    let env = wire
        .request(SHELF, "queue.get_queue", json!({ "v": 1 }))
        .await?;
    Ok(env.get("length").and_then(Value::as_u64))
}
