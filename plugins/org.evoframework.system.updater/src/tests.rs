// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Tests for the system.updater plugin: a fake wrapper, a temp state
//! directory, and a fixed clock.

use std::sync::Mutex;

use super::*;

const NOW: u64 = 1_795_000_100;

fn clock() -> u64 {
    NOW
}

/// A runner that records its calls and answers from a queue.
struct Fake {
    calls: Mutex<Vec<Vec<String>>>,
    answers: Mutex<Vec<Result<String, String>>>,
}

impl Fake {
    fn new(answers: Vec<Result<String, String>>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(vec![]),
            answers: Mutex::new(answers),
        })
    }
    fn accepting(op: &str) -> Arc<Self> {
        Self::new(vec![Ok(format!(
            r#"{{"v":1,"accepted":true,"op":"{op}"}}"#
        ))])
    }
    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
}

impl Runner for Fake {
    fn run(&self, args: &[String]) -> Result<String, String> {
        self.calls.lock().unwrap().push(args.to_vec());
        let mut a = self.answers.lock().unwrap();
        if a.is_empty() {
            Err("no answer queued".into())
        } else {
            a.remove(0)
        }
    }
}

struct Fx {
    dir: tempfile::TempDir,
}

impl Fx {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }
    fn put(&self, name: &str, body: &str) {
        std::fs::write(self.dir.path().join(name), body).unwrap();
    }
    fn plugin(&self, runner: Arc<dyn Runner>) -> UpdaterPlugin {
        let mut p =
            UpdaterPlugin::with(self.dir.path().to_path_buf(), runner, clock);
        p.loaded = true;
        p
    }
    fn listing(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.dir.path())
            .unwrap()
            .flatten()
            .map(|e| {
                format!(
                    "{}:{}",
                    e.file_name().to_string_lossy(),
                    e.metadata().unwrap().len()
                )
            })
            .collect();
        v.sort();
        v
    }
}

fn req(verb: &str, payload: Value) -> Request {
    Request {
        request_type: verb.to_string(),
        payload: payload.to_string().into_bytes(),
        correlation_id: 1,
        deadline: None,
        instance_id: None,
        principal_scope: None,
        has_step_up: false,
    }
}

async fn call(p: &UpdaterPlugin, verb: &str, payload: Value) -> Value {
    let resp = p.handle_request(&req(verb, payload)).await.unwrap();
    serde_json::from_slice(&resp.payload).unwrap()
}

// ----- manifest / surface -----

#[test]
fn both_manifests_stock_the_updater_shelf_and_gate_the_verbs() {
    let oop = Manifest::from_toml(include_str!("../manifest.oop.toml"))
        .expect("manifest.oop.toml parses");
    for m in [manifest(), oop] {
        assert_eq!(m.plugin.name, PLUGIN_NAME);
        assert_eq!(m.plugin.version.to_string(), BUILD);
        assert_eq!(m.target.shelf, "system.updater");
        assert_eq!(m.target.shape, 1);
    }
    use evo_plugin_sdk::manifest::VerbCapability;
    let resp = manifest().capabilities.respondent.expect("respondent");
    for v in [VERB_CHECK, VERB_APPLY, VERB_CHANNEL_SET] {
        match resp.verb_capabilities.get(v) {
            Some(VerbCapability::Write { scope }) => {
                assert_eq!(scope, "system_admin", "{v}")
            }
            other => panic!(
                "{v} starts work and must be write/system_admin, got {other:?}"
            ),
        }
    }
    for v in [VERB_STATUS, VERB_HISTORY, VERB_CHANNEL_GET] {
        match resp.verb_capabilities.get(v) {
            Some(VerbCapability::Read { scope }) => {
                assert_eq!(scope, "system", "{v}")
            }
            other => panic!("{v} must be read/system, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn describe_lists_exactly_the_manifest_verbs() {
    let p = Fx::new().plugin(Fake::new(vec![]));
    let d = p.describe().await;
    let mut got = d.runtime_capabilities.request_types.clone();
    got.sort();
    let mut want = manifest().capabilities.respondent.unwrap().request_types;
    want.sort();
    assert_eq!(got, want);
    assert_eq!(got.len(), 6);
}

// ----- reads -----

#[tokio::test]
async fn status_is_idle_with_the_default_channel_before_any_run() {
    let p = Fx::new().plugin(Fake::new(vec![]));
    let s = call(&p, VERB_STATUS, json!({"v": 1})).await;
    assert_eq!(s["state"], "idle");
    assert_eq!(s["channel"], "production");
    assert_eq!(s["inventory"], Value::Null);
}

#[tokio::test]
async fn status_passes_through_what_the_helper_recorded() {
    let f = Fx::new();
    f.put(
        "status.json",
        r#"{"v":1,"op":"apply","state":"applied","component":"org.evoframework.alarm","version":"0.1.3","started_at_utc":"2026-11-27T10:00:00Z","finished_at_utc":"2026-11-27T10:00:09Z","summary":{"to":"0.1.3"},"junk":"dropped"}"#,
    );
    f.put(
        "inventory.json",
        r#"{"v":1,"channel":"production","items":[{"component":"x"}]}"#,
    );
    f.put("channel", "test\n");
    let s =
        call(&f.plugin(Fake::new(vec![])), VERB_STATUS, json!({"v": 1})).await;
    assert_eq!(s["state"], "applied");
    assert_eq!(s["component"], "org.evoframework.alarm");
    assert_eq!(s["summary"]["to"], "0.1.3");
    assert_eq!(s["channel"], "test");
    assert_eq!(s["inventory"]["items"][0]["component"], "x");
    assert!(
        s.get("junk").is_none(),
        "only the named fields are passed on"
    );
}

#[tokio::test]
async fn an_accepted_request_reads_as_starting_until_a_newer_status_appears() {
    let f = Fx::new();
    f.put(
        "status.json",
        r#"{"v":1,"op":"check","state":"checked","started_at_utc":"2026-01-01T00:00:00Z","finished_at_utc":"2026-01-01T00:00:05Z"}"#,
    );
    let runner = Fake::accepting("apply");
    let p = f.plugin(runner);
    let a = call(&p, VERB_APPLY, json!({"v": 1, "component": "org.evoframework.alarm", "version": "0.1.3"})).await;
    assert_eq!(a["accepted"], true);
    let s = call(&p, VERB_STATUS, json!({"v": 1})).await;
    assert_eq!(s["state"], "starting", "the run before is not this run");
    assert_eq!(s["op"], "apply");
    assert_eq!(s["component"], "org.evoframework.alarm");
    assert!(
        s.get("finished_at_utc").is_none(),
        "the older run's end is not shown"
    );
    // The helper starts and records its own status, newer than the request.
    f.put("status.json", &format!(r#"{{"v":1,"op":"apply","state":"applying","started_at_utc":"{}"}}"#, timeutil::format_utc(NOW + 1)));
    assert_eq!(
        call(&p, VERB_STATUS, json!({"v": 1})).await["state"],
        "applying"
    );
}

#[tokio::test]
async fn history_is_newest_first_bounded_and_skips_a_bad_line() {
    let f = Fx::new();
    let mut lines = String::new();
    for i in 0..60 {
        lines.push_str(&format!("{{\"n\":{i}}}\n"));
    }
    lines.push_str("not json\n");
    f.put("history.jsonl", &lines);
    let p = f.plugin(Fake::new(vec![]));
    let d = call(&p, VERB_HISTORY, json!({"v": 1})).await;
    let e = d["entries"].as_array().unwrap();
    assert_eq!(e.len(), 20, "default");
    assert_eq!(e[0]["n"], 59, "newest first");
    assert_eq!(
        call(&p, VERB_HISTORY, json!({"v": 1, "limit": 500})).await["entries"]
            .as_array()
            .unwrap()
            .len(),
        50
    );
    assert_eq!(
        call(&p, VERB_HISTORY, json!({"v": 1, "limit": 0})).await["entries"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn channel_get_answers_the_stored_channel_and_production_for_anything_else(
) {
    let f = Fx::new();
    let p = f.plugin(Fake::new(vec![]));
    assert_eq!(
        call(&p, VERB_CHANNEL_GET, json!({"v": 1})).await["channel"],
        "production"
    );
    f.put("channel", "alpha\n");
    assert_eq!(
        call(&p, VERB_CHANNEL_GET, json!({"v": 1})).await["channel"],
        "alpha"
    );
    f.put("channel", "nightly\n");
    assert_eq!(
        call(&p, VERB_CHANNEL_GET, json!({"v": 1})).await["channel"],
        "production"
    );
}

#[tokio::test]
async fn the_reads_write_nothing() {
    let f = Fx::new();
    f.put("status.json", r#"{"state":"checked"}"#);
    f.put("history.jsonl", "{\"n\":1}\n");
    let before = f.listing();
    let p = f.plugin(Fake::new(vec![]));
    for v in [VERB_STATUS, VERB_HISTORY, VERB_CHANNEL_GET] {
        call(&p, v, json!({"v": 1})).await;
    }
    assert_eq!(f.listing(), before);
}

// ----- starting work -----

#[tokio::test]
async fn check_asks_the_helper_for_exactly_check() {
    let r = Fake::accepting("check");
    let p = Fx::new().plugin(r.clone());
    let a = call(&p, VERB_CHECK, json!({"v": 1})).await;
    assert_eq!(a, json!({"v": 1, "accepted": true, "op": "check"}));
    assert_eq!(r.calls(), vec![vec!["check".to_string()]]);
}

#[tokio::test]
async fn apply_passes_only_a_valid_component_and_version() {
    let r = Fake::accepting("apply");
    let p = Fx::new().plugin(r.clone());
    for (c, v, code) in [
        ("../etc/passwd", "1.0", "bad_component"),
        ("org.evoframework.a b", "1.0", "bad_component"),
        ("com.other.x", "1.0", "bad_component"),
        ("org.evoframework.alarm", "latest", "bad_version"),
        ("org.evoframework.alarm", "1.0;id", "bad_version"),
    ] {
        let a = call(
            &p,
            VERB_APPLY,
            json!({"v": 1, "component": c, "version": v}),
        )
        .await;
        assert_eq!(a["accepted"], false, "{c} {v}");
        assert_eq!(a["error"]["code"], code, "{c} {v}");
    }
    assert!(
        r.calls().is_empty(),
        "a refused request never reaches the helper"
    );
    let ok = call(
        &p,
        VERB_APPLY,
        json!({"v": 1, "component": "evo-host-files", "version": "0.1.13.3"}),
    )
    .await;
    assert_eq!(ok["accepted"], true);
    assert_eq!(
        r.calls(),
        vec![vec![
            "apply".to_string(),
            "evo-host-files".into(),
            "0.1.13.3".into()
        ]]
    );
}

#[tokio::test]
async fn channel_set_passes_only_a_known_channel() {
    let r = Fake::accepting("channel");
    let p = Fx::new().plugin(r.clone());
    let bad =
        call(&p, VERB_CHANNEL_SET, json!({"v": 1, "channel": "nightly"})).await;
    assert_eq!(bad["error"]["code"], "bad_channel");
    assert!(r.calls().is_empty());
    assert_eq!(
        call(&p, VERB_CHANNEL_SET, json!({"v": 1, "channel": "test"})).await
            ["accepted"],
        true
    );
    assert_eq!(r.calls(), vec![vec!["channel".to_string(), "test".into()]]);
}

#[tokio::test]
async fn a_helper_that_cannot_be_run_or_is_not_understood_is_a_structured_refusal(
) {
    for answer in [
        Err("sudo: a password is required".to_string()),
        Ok("<html>not json</html>".to_string()),
        Ok("{}".to_string()),
    ] {
        let p = Fx::new().plugin(Fake::new(vec![answer]));
        let a = call(&p, VERB_CHECK, json!({"v": 1})).await;
        assert_eq!(a["accepted"], false);
        assert_eq!(a["error"]["code"], "helper_unavailable");
        let s = call(&p, VERB_STATUS, json!({"v": 1})).await;
        assert_eq!(
            s["state"], "idle",
            "a request that was not accepted does not read as starting"
        );
    }
}

#[tokio::test]
async fn a_helper_refusal_is_passed_on_and_does_not_read_as_starting() {
    let r = Fake::new(vec![Ok(r#"{"v":1,"accepted":false,"error":{"code":"busy","detail":"another update is running"}}"#.into())]);
    let p = Fx::new().plugin(r);
    let a = call(&p, VERB_CHECK, json!({"v": 1})).await;
    assert_eq!(a["accepted"], false);
    assert_eq!(a["error"]["code"], "busy");
    assert_eq!(
        call(&p, VERB_STATUS, json!({"v": 1})).await["state"],
        "idle"
    );
}

// ----- refusals -----

#[tokio::test]
async fn a_bad_payload_version_shape_verb_or_unloaded_plugin_is_refused() {
    let f = Fx::new();
    let p = f.plugin(Fake::new(vec![]));
    for r in [
        req(VERB_STATUS, json!({"v": 2})),
        req(VERB_CHECK, json!({"v": 2})),
        req(VERB_APPLY, json!({"v": 1})),
        req(
            VERB_APPLY,
            json!({"v": 2, "component": "org.evoframework.alarm", "version": "1"}),
        ),
        req(VERB_CHANNEL_SET, json!({"v": 1})),
        req("updater.rollback", json!({"v": 1})),
    ] {
        assert!(
            matches!(
                p.handle_request(&r).await,
                Err(PluginError::Permanent(_))
            ),
            "{} {:?}",
            r.request_type,
            String::from_utf8_lossy(&r.payload)
        );
    }
    let mut raw = req(VERB_STATUS, json!({}));
    raw.payload = b"not json".to_vec();
    assert!(p.handle_request(&raw).await.is_err());
    let unloaded = UpdaterPlugin::with(
        f.dir.path().to_path_buf(),
        Fake::new(vec![]),
        clock,
    );
    assert!(unloaded
        .handle_request(&req(VERB_STATUS, json!({"v": 1})))
        .await
        .is_err());
}

#[tokio::test]
async fn health_follows_load() {
    let mut p = Fx::new().plugin(Fake::new(vec![]));
    let healthy = format!("{:?}", p.health_check().await);
    p.unload().await.unwrap();
    let unhealthy = format!("{:?}", p.health_check().await);
    assert_ne!(healthy, unhealthy, "health changes when the plugin unloads");
    assert!(unhealthy.contains("not loaded"), "{unhealthy}");
}
