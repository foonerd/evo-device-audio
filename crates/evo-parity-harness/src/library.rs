//! Library-shelf parity round-trips.
//!
//! Drives every mutating verb on `audio.library` with safe
//! round-trips that do not affect the rig's persistent
//! library configuration. The harness uses an
//! intentionally-unreachable mount path for `add_source` so
//! the post-add probe transition demonstrates the source
//! state machine's Offline transition WITHOUT actually
//! touching real storage.
//!
//! Test sequence:
//! 1. `library.list_sources` — observe the floor source is
//!    present (`local-internal`).
//! 2. NEGATIVE: `library.remove_source(local-internal)` —
//!    refuse with `non_removable_local_internal`.
//! 3. `library.add_source` for a synthetic local-USB source
//!    pointing at `/tmp/__evo_parity_nonexistent__`.
//! 4. `library.list_sources` — observe the new source is
//!    present.
//! 5. `library.probe_source` — observe the probe result.
//! 6. `library.remove_source` — remove the synthetic source.
//! 7. `library.list_sources` — observe the synthetic source
//!    is gone.
//! 8. `library.search_library` — empty query (read; pass-through).
//!
//! `library.update_source` is exercised separately in the
//! scenarios module because it triggers a real MPD update job
//! the harness must observe via the
//! `audio_library_scan_progress` subject.

use anyhow::Result;
use serde_json::{json, Value};

use crate::report::CaseResult;
use crate::wire::Wire;

const SHELF: &str = "audio.library";
const FLOOR_SOURCE_ID: &str = "local-internal";
const SYNTH_SOURCE_DISPLAY: &str = "__evo_parity_synth__";
const SYNTH_MOUNT_PATH: &str = "/tmp/__evo_parity_nonexistent__";

pub async fn run(wire: &mut Wire) -> Result<Vec<CaseResult>> {
    let mut cases = Vec::new();
    // Defensive pre-clean: prior harness runs that aborted
    // mid-way may have left synthetic sources behind. Sweep
    // every source whose id begins with the harness's
    // sanitised display-name prefix.
    if let Ok(ids) = list_source_ids(wire).await {
        for id in ids {
            if id.starts_with("evo-parity-synth") {
                let _ = wire
                    .request(
                        SHELF,
                        "library.remove_source",
                        json!({ "v": 1, "source_id": id }),
                    )
                    .await;
            }
        }
    }

    cases.push(case_floor_source_present(wire).await);
    cases.push(case_floor_source_remove_refused(wire).await);

    let synth_id = match add_synthetic_source(wire).await {
        Ok(id) => {
            cases.push(CaseResult::pass_with(
                "library.add_synthetic_source",
                SHELF,
                "add_source for synthetic local-USB source",
                json!({ "id": id }),
            ));
            Some(id)
        }
        Err(e) => {
            cases.push(CaseResult::fail(
                "library.add_synthetic_source",
                SHELF,
                "add_source for synthetic local-USB source",
                e.to_string(),
            ));
            None
        }
    };

    if let Some(id) = &synth_id {
        cases.push(case_synth_listed(wire, id).await);
        cases.push(case_probe(wire, id).await);
        cases.push(case_remove_synth(wire, id).await);
        cases.push(case_synth_gone(wire, id).await);
    } else {
        cases.push(CaseResult::skip(
            "library.synth_listed",
            SHELF,
            "synthetic source listed after add",
            "add_source failed",
        ));
        cases.push(CaseResult::skip(
            "library.probe",
            SHELF,
            "probe_source against synthetic source",
            "add_source failed",
        ));
        cases.push(CaseResult::skip(
            "library.remove_synth",
            SHELF,
            "remove_source for synthetic source",
            "add_source failed",
        ));
        cases.push(CaseResult::skip(
            "library.synth_gone",
            SHELF,
            "synthetic source absent after remove",
            "add_source failed",
        ));
    }

    cases.push(case_search_empty(wire).await);
    Ok(cases)
}

async fn case_floor_source_present(wire: &mut Wire) -> CaseResult {
    match list_source_ids(wire).await {
        Ok(ids) => {
            if ids.iter().any(|id| id == FLOOR_SOURCE_ID) {
                CaseResult::pass_with(
                    "library.floor_source_present",
                    SHELF,
                    "list_sources includes the local-internal floor",
                    json!({ "ids": ids }),
                )
            } else {
                CaseResult::fail_with(
                    "library.floor_source_present",
                    SHELF,
                    "list_sources includes the local-internal floor",
                    "local-internal not in source ids",
                    json!({ "ids": ids }),
                )
            }
        }
        Err(e) => CaseResult::fail(
            "library.floor_source_present",
            SHELF,
            "list_sources includes the local-internal floor",
            e.to_string(),
        ),
    }
}

async fn case_floor_source_remove_refused(wire: &mut Wire) -> CaseResult {
    match wire
        .request_expect_error(
            SHELF,
            "library.remove_source",
            json!({ "v": 1, "source_id": FLOOR_SOURCE_ID }),
        )
        .await
    {
        Ok(err) => {
            if err.message.to_lowercase().contains("non-removable")
                || err.message.to_lowercase().contains("local-internal")
            {
                CaseResult::pass_with(
                    "library.floor_source_remove_refused",
                    SHELF,
                    "remove_source(local-internal) refuses",
                    json!({
                        "class":    err.class,
                        "subclass": err.subclass,
                        "message":  err.message,
                    }),
                )
            } else {
                CaseResult::fail_with(
                    "library.floor_source_remove_refused",
                    SHELF,
                    "remove_source(local-internal) refuses",
                    "error did not reference non-removable / local-internal",
                    json!({
                        "class":    err.class,
                        "subclass": err.subclass,
                        "message":  err.message,
                    }),
                )
            }
        }
        Err(e) => CaseResult::fail(
            "library.floor_source_remove_refused",
            SHELF,
            "remove_source(local-internal) refuses",
            e.to_string(),
        ),
    }
}

async fn add_synthetic_source(wire: &mut Wire) -> Result<String> {
    let env = wire
        .request(
            SHELF,
            "library.add_source",
            json!({
                "v":            1,
                "display_name": SYNTH_SOURCE_DISPLAY,
                "kind": {
                    "kind":        "local_usb",
                    "device_node": "/dev/null",
                    "label":       "evo-parity-synth",
                },
                "mount_path":   SYNTH_MOUNT_PATH,
            }),
        )
        .await?;
    let id = env
        .get("source_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            anyhow::anyhow!("add_source response missing source_id")
        })?;
    Ok(id.to_string())
}

async fn case_synth_listed(wire: &mut Wire, expected_id: &str) -> CaseResult {
    match list_source_ids(wire).await {
        Ok(ids) => {
            if ids.iter().any(|id| id == expected_id) {
                CaseResult::pass(
                    "library.synth_listed",
                    SHELF,
                    "synthetic source listed after add",
                )
            } else {
                CaseResult::fail_with(
                    "library.synth_listed",
                    SHELF,
                    "synthetic source listed after add",
                    "synthetic id not in source listing",
                    json!({ "expected_id": expected_id, "ids": ids }),
                )
            }
        }
        Err(e) => CaseResult::fail(
            "library.synth_listed",
            SHELF,
            "synthetic source listed after add",
            e.to_string(),
        ),
    }
}

async fn case_probe(wire: &mut Wire, source_id: &str) -> CaseResult {
    match wire
        .request(
            SHELF,
            "library.probe_source",
            json!({ "v": 1, "source_id": source_id }),
        )
        .await
    {
        Ok(env) => CaseResult::pass_with(
            "library.probe",
            SHELF,
            "probe_source against synthetic source",
            json!({ "envelope": env }),
        ),
        Err(e) => CaseResult::fail(
            "library.probe",
            SHELF,
            "probe_source against synthetic source",
            e.to_string(),
        ),
    }
}

async fn case_remove_synth(wire: &mut Wire, source_id: &str) -> CaseResult {
    match wire
        .request(
            SHELF,
            "library.remove_source",
            json!({ "v": 1, "source_id": source_id }),
        )
        .await
    {
        Ok(_) => CaseResult::pass(
            "library.remove_synth",
            SHELF,
            "remove_source for synthetic source",
        ),
        Err(e) => CaseResult::fail(
            "library.remove_synth",
            SHELF,
            "remove_source for synthetic source",
            e.to_string(),
        ),
    }
}

async fn case_synth_gone(wire: &mut Wire, removed_id: &str) -> CaseResult {
    match list_source_ids(wire).await {
        Ok(ids) => {
            if ids.iter().any(|id| id == removed_id) {
                CaseResult::fail_with(
                    "library.synth_gone",
                    SHELF,
                    "synthetic source absent after remove",
                    "synthetic id still present",
                    json!({ "removed_id": removed_id, "ids": ids }),
                )
            } else {
                CaseResult::pass(
                    "library.synth_gone",
                    SHELF,
                    "synthetic source absent after remove",
                )
            }
        }
        Err(e) => CaseResult::fail(
            "library.synth_gone",
            SHELF,
            "synthetic source absent after remove",
            e.to_string(),
        ),
    }
}

async fn case_search_empty(wire: &mut Wire) -> CaseResult {
    match wire
        .request(
            SHELF,
            "library.search_library",
            json!({
                "v":               1,
                "query":           "__evo_parity_nonmatching__",
                "include_offline": false,
                "max_results":     100,
            }),
        )
        .await
    {
        Ok(env) => CaseResult::pass_with(
            "library.search_empty",
            SHELF,
            "search_library with a non-matching query returns a clean envelope",
            json!({ "result_count": env.get("results").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0) }),
        ),
        Err(e) => CaseResult::fail(
            "library.search_empty",
            SHELF,
            "search_library with a non-matching query returns a clean envelope",
            e.to_string(),
        ),
    }
}

async fn list_source_ids(wire: &mut Wire) -> Result<Vec<String>> {
    let env = wire
        .request(SHELF, "library.list_sources", json!({ "v": 1 }))
        .await?;
    let ids = env
        .get("sources")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|s| {
                    s.get("id").and_then(Value::as_str).map(String::from)
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(ids)
}
