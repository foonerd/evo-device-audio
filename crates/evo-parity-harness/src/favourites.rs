// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Favourites-shelf parity round-trips.
//!
//! Drives every mutating verb on `audio.favourites` with the
//! set-semantic idempotence assertion the shelf contract
//! pins (add-add-add yields one membership, not three).
//!
//! Test sequence:
//! 1. `favourites.clear_favourites` — establish empty start.
//! 2. `favourites.add_favourite` for a probe URI — observe
//!    membership goes 0 -> 1.
//! 3. `favourites.add_favourite` for the SAME URI again —
//!    observe membership stays 1 (set semantics; idempotent).
//! 4. `favourites.is_favourite` — observe `true`.
//! 5. `favourites.remove_favourite` — observe 0.
//! 6. `favourites.is_favourite` — observe `false`.
//! 7. `favourites.clear_favourites` — trailing clean-up.
//!
//! The probe URI is a synthetic `mpd-path:` value. Whether it
//! resolves to a real song on the rig is irrelevant — the
//! favourites surface stores URI membership directly without
//! validating the song exists.

use anyhow::Result;
use serde_json::{json, Value};

use crate::report::CaseResult;
use crate::wire::Wire;

const SHELF: &str = "audio.favourites";
// MPD's `playlistadd` validates the URI: relative paths must
// resolve to a song in the database, absolute paths are
// refused over TCP. HTTP / HTTPS URLs are accepted without
// database resolution — MPD treats them as stream
// references. The harness uses a synthetic stream URL so the
// add round-trip exercises the favourites set semantics
// without depending on rig-specific library content.
const PROBE_URI: &str =
    "http://evo-parity-harness.invalid/__evo_parity_probe__.mp3";

pub async fn run(wire: &mut Wire) -> Result<Vec<CaseResult>> {
    let mut cases = Vec::new();
    let _ = wire
        .request(SHELF, "favourites.clear_favourites", json!({ "v": 1 }))
        .await;

    cases.push(case_clear_starts_empty(wire).await);
    cases.push(case_add_first(wire).await);
    cases.push(case_add_idempotent(wire).await);
    cases.push(case_is_favourite_true(wire).await);
    cases.push(case_remove(wire).await);
    cases.push(case_is_favourite_false(wire).await);
    cases.push(case_trailing_clear(wire).await);
    Ok(cases)
}

async fn case_clear_starts_empty(wire: &mut Wire) -> CaseResult {
    if let Err(e) = wire
        .request(SHELF, "favourites.clear_favourites", json!({ "v": 1 }))
        .await
    {
        return CaseResult::fail(
            "favourites.clear_starts_empty",
            SHELF,
            "starting clear_favourites establishes empty state",
            e.to_string(),
        );
    }
    match favourites_count(wire).await {
        Ok(0) => CaseResult::pass(
            "favourites.clear_starts_empty",
            SHELF,
            "starting clear_favourites establishes empty state",
        ),
        Ok(n) => CaseResult::fail_with(
            "favourites.clear_starts_empty",
            SHELF,
            "starting clear_favourites establishes empty state",
            format!("expected count=0; got count={n}"),
            json!({ "observed_count": n }),
        ),
        Err(e) => CaseResult::fail(
            "favourites.clear_starts_empty",
            SHELF,
            "starting clear_favourites establishes empty state",
            e.to_string(),
        ),
    }
}

async fn case_add_first(wire: &mut Wire) -> CaseResult {
    if let Err(e) = wire
        .request(
            SHELF,
            "favourites.add_favourite",
            json!({ "v": 1, "uri": PROBE_URI }),
        )
        .await
    {
        return CaseResult::fail(
            "favourites.add_first",
            SHELF,
            format!("add_favourite({PROBE_URI:?}) first call"),
            e.to_string(),
        );
    }
    match favourites_count(wire).await {
        Ok(1) => CaseResult::pass_with(
            "favourites.add_first",
            SHELF,
            format!("add_favourite({PROBE_URI:?}) first call"),
            json!({ "observed_count": 1 }),
        ),
        Ok(n) => CaseResult::fail_with(
            "favourites.add_first",
            SHELF,
            format!("add_favourite({PROBE_URI:?}) first call"),
            format!("expected count=1; got count={n}"),
            json!({ "observed_count": n }),
        ),
        Err(e) => CaseResult::fail(
            "favourites.add_first",
            SHELF,
            format!("add_favourite({PROBE_URI:?}) first call"),
            e.to_string(),
        ),
    }
}

async fn case_add_idempotent(wire: &mut Wire) -> CaseResult {
    if let Err(e) = wire
        .request(
            SHELF,
            "favourites.add_favourite",
            json!({ "v": 1, "uri": PROBE_URI }),
        )
        .await
    {
        return CaseResult::fail(
            "favourites.add_idempotent",
            SHELF,
            "add_favourite second call is idempotent (set semantics)",
            e.to_string(),
        );
    }
    match favourites_count(wire).await {
        Ok(1) => CaseResult::pass_with(
            "favourites.add_idempotent",
            SHELF,
            "add_favourite second call is idempotent (set semantics)",
            json!({ "observed_count": 1 }),
        ),
        Ok(n) => CaseResult::fail_with(
            "favourites.add_idempotent",
            SHELF,
            "add_favourite second call is idempotent (set semantics)",
            format!("expected count=1 after duplicate add; got count={n}"),
            json!({ "observed_count": n }),
        ),
        Err(e) => CaseResult::fail(
            "favourites.add_idempotent",
            SHELF,
            "add_favourite second call is idempotent (set semantics)",
            e.to_string(),
        ),
    }
}

async fn case_is_favourite_true(wire: &mut Wire) -> CaseResult {
    match wire
        .request(
            SHELF,
            "favourites.is_favourite",
            json!({ "v": 1, "uri": PROBE_URI }),
        )
        .await
    {
        Ok(env) => {
            let is_fav = env.get("is_favourite").and_then(Value::as_bool);
            if is_fav == Some(true) {
                CaseResult::pass(
                    "favourites.is_favourite_true",
                    SHELF,
                    format!("is_favourite({PROBE_URI:?}) -> true"),
                )
            } else {
                CaseResult::fail_with(
                    "favourites.is_favourite_true",
                    SHELF,
                    format!("is_favourite({PROBE_URI:?}) -> true"),
                    format!("expected is_favourite=true; got {is_fav:?}"),
                    json!({ "envelope": env }),
                )
            }
        }
        Err(e) => CaseResult::fail(
            "favourites.is_favourite_true",
            SHELF,
            format!("is_favourite({PROBE_URI:?}) -> true"),
            e.to_string(),
        ),
    }
}

async fn case_remove(wire: &mut Wire) -> CaseResult {
    if let Err(e) = wire
        .request(
            SHELF,
            "favourites.remove_favourite",
            json!({ "v": 1, "uri": PROBE_URI }),
        )
        .await
    {
        return CaseResult::fail(
            "favourites.remove",
            SHELF,
            format!("remove_favourite({PROBE_URI:?})"),
            e.to_string(),
        );
    }
    match favourites_count(wire).await {
        Ok(0) => CaseResult::pass(
            "favourites.remove",
            SHELF,
            format!("remove_favourite({PROBE_URI:?})"),
        ),
        Ok(n) => CaseResult::fail_with(
            "favourites.remove",
            SHELF,
            format!("remove_favourite({PROBE_URI:?})"),
            format!("expected count=0; got count={n}"),
            json!({ "observed_count": n }),
        ),
        Err(e) => CaseResult::fail(
            "favourites.remove",
            SHELF,
            format!("remove_favourite({PROBE_URI:?})"),
            e.to_string(),
        ),
    }
}

async fn case_is_favourite_false(wire: &mut Wire) -> CaseResult {
    match wire
        .request(
            SHELF,
            "favourites.is_favourite",
            json!({ "v": 1, "uri": PROBE_URI }),
        )
        .await
    {
        Ok(env) => {
            let is_fav = env.get("is_favourite").and_then(Value::as_bool);
            if is_fav == Some(false) {
                CaseResult::pass(
                    "favourites.is_favourite_false",
                    SHELF,
                    format!(
                        "is_favourite({PROBE_URI:?}) -> false (after remove)"
                    ),
                )
            } else {
                CaseResult::fail_with(
                    "favourites.is_favourite_false",
                    SHELF,
                    format!(
                        "is_favourite({PROBE_URI:?}) -> false (after remove)"
                    ),
                    format!("expected is_favourite=false; got {is_fav:?}"),
                    json!({ "envelope": env }),
                )
            }
        }
        Err(e) => CaseResult::fail(
            "favourites.is_favourite_false",
            SHELF,
            format!("is_favourite({PROBE_URI:?}) -> false (after remove)"),
            e.to_string(),
        ),
    }
}

async fn case_trailing_clear(wire: &mut Wire) -> CaseResult {
    match wire
        .request(SHELF, "favourites.clear_favourites", json!({ "v": 1 }))
        .await
    {
        Ok(_) => CaseResult::pass(
            "favourites.trailing_clear",
            SHELF,
            "trailing clear_favourites cleans up",
        ),
        Err(e) => CaseResult::fail(
            "favourites.trailing_clear",
            SHELF,
            "trailing clear_favourites cleans up",
            e.to_string(),
        ),
    }
}

async fn favourites_count(wire: &mut Wire) -> Result<u64> {
    let env = wire
        .request(SHELF, "favourites.list_favourites", json!({ "v": 1 }))
        .await?;
    Ok(env.get("count").and_then(Value::as_u64).unwrap_or(0))
}
