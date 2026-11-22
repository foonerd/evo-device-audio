// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Tests for the identity.locale plugin: the language against a
//! state directory in a temporary directory, the zone against a
//! stub host and a stub shelf dispatcher on tokio's test clock.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use evo_plugin_sdk::contract::ShelfDispatchError;
use evo_plugin_sdk::manifest::VerbCapability;
use serde_json::{json, Value};

use super::timezone::{
    HostFuture, ALARM_SHELF, FACTORY, SETTLE, VERB_ALARM_RESCHEDULE,
};
use super::*;

/// A plugin booted the way load boots it, over a state file in
/// `dir`, with the glass's catalogues unless others are given.
async fn boot(dir: &Path, catalogues: Option<&[&str]>) -> LocalePlugin {
    let mut p = LocalePlugin::new().with_state_path(dir.join(STATE_FILENAME));
    if let Some(c) = catalogues {
        p.catalogues = c.iter().map(|s| (*s).to_string()).collect();
    }
    p.restore().await;
    p.loaded = true;
    p
}

fn req(payload: Value) -> Request {
    Request {
        request_type: VERB_LANGUAGE_SET.to_string(),
        payload: payload.to_string().into_bytes(),
        correlation_id: 1,
        deadline: None,
        instance_id: None,
        principal_scope: None,
        has_step_up: false,
    }
}

async fn set(p: &LocalePlugin, language: &str) -> Value {
    let resp = p
        .handle_request(&req(json!({ "v": 1, "language": language })))
        .await
        .expect("a set answers; a refused tag is not an error");
    serde_json::from_slice(&resp.payload).unwrap()
}

fn stored(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join(STATE_FILENAME)).ok()
}

// ----- manifest / surface -----

fn both_manifests() -> [Manifest; 2] {
    let oop = Manifest::from_toml(include_str!("../manifest.oop.toml"))
        .expect("manifest.oop.toml parses");
    [manifest(), oop]
}

/// Both manifests stock `system.locale` with the two setters and
/// the two zone reads, a 6000 ms budget for the zone change's wait
/// and reschedule, and one gate: `timezone.set` is a write at
/// `system_admin`.
#[test]
fn both_manifests_stock_system_locale_with_four_verbs() {
    for m in both_manifests() {
        assert_eq!(m.plugin.name, PLUGIN_NAME);
        assert_eq!(m.target.shelf, "system.locale");
        assert_eq!(m.target.shape, 1);
        let resp = m.capabilities.respondent.expect("respondent");
        assert_eq!(
            resp.request_types,
            vec![
                VERB_LANGUAGE_SET,
                VERB_TIMEZONE_SET,
                VERB_TIMEZONE_GET,
                VERB_TIMEZONE_LIST
            ]
        );
        assert_eq!(resp.response_budget_ms, 6000);
        assert_eq!(
            resp.verb_capabilities.into_iter().collect::<Vec<_>>(),
            vec![(
                VERB_TIMEZONE_SET.to_string(),
                VerbCapability::Write {
                    scope: "system_admin".to_string()
                }
            )]
        );
    }
}

/// Pin: language.set stays ungated. Changing the language asks
/// for no password, in either manifest.
#[test]
fn language_set_stays_ungated() {
    for m in both_manifests() {
        let resp = m.capabilities.respondent.expect("respondent");
        assert!(resp.request_types.iter().any(|v| v == VERB_LANGUAGE_SET));
        assert!(
            !resp.verb_capabilities.contains_key(VERB_LANGUAGE_SET),
            "language.set is gated: {:?}",
            resp.verb_capabilities
        );
    }
}

/// Pin: timezone.set stays a write at `system_admin`, not step-up,
/// in either manifest.
#[test]
fn timezone_set_stays_write_system_admin() {
    for m in both_manifests() {
        let resp = m.capabilities.respondent.expect("respondent");
        assert_eq!(
            resp.verb_capabilities.get(VERB_TIMEZONE_SET),
            Some(&VerbCapability::Write {
                scope: "system_admin".to_string()
            }),
            "timezone.set gate: {:?}",
            resp.verb_capabilities
        );
    }
}

/// Pin: neither zone read carries a write grant, in either manifest.
#[test]
fn the_zone_reads_have_no_write_grant() {
    for m in both_manifests() {
        let resp = m.capabilities.respondent.expect("respondent");
        for verb in [VERB_TIMEZONE_GET, VERB_TIMEZONE_LIST] {
            assert!(resp.request_types.iter().any(|v| v == verb), "{verb}");
            assert!(
                !matches!(
                    resp.verb_capabilities.get(verb),
                    Some(
                        VerbCapability::Write { .. }
                            | VerbCapability::StepUp { .. }
                    )
                ),
                "{verb} carries a write grant: {:?}",
                resp.verb_capabilities
            );
        }
    }
}

/// Pin: `alarm.reschedule` declares no verb capability, in either
/// alarm manifest. The shelf dispatcher calls it as this plugin,
/// whose principal holds no capability; a gate there would refuse
/// every zone change's reschedule.
#[test]
fn alarm_reschedule_declares_no_verb_capabilities() {
    for (file, text) in [
        (
            "alarm manifest.toml",
            include_str!("../../org.evoframework.alarm/manifest.toml"),
        ),
        (
            "alarm manifest.oop.toml",
            include_str!("../../org.evoframework.alarm/manifest.oop.toml"),
        ),
    ] {
        let m = Manifest::from_toml(text).expect(file);
        assert_eq!(m.target.shelf, ALARM_SHELF, "{file}");
        let resp = m.capabilities.respondent.expect(file);
        assert!(
            resp.request_types
                .iter()
                .any(|v| v == VERB_ALARM_RESCHEDULE),
            "{file} does not stock {VERB_ALARM_RESCHEDULE}"
        );
        assert!(
            !resp.verb_capabilities.contains_key(VERB_ALARM_RESCHEDULE),
            "{file} gates {VERB_ALARM_RESCHEDULE}: {:?}",
            resp.verb_capabilities
        );
    }
}

#[tokio::test]
async fn describe_names_the_manifest_verbs() {
    let d = LocalePlugin::new().describe().await;
    let declared = manifest().capabilities.respondent.unwrap().request_types;
    assert_eq!(d.runtime_capabilities.request_types, declared);
}

// ----- language.set -----

/// Pin: a fresh plugin answers `en`. Before any set, and with
/// nothing stored, the language in force is `en`; a refused tag
/// on a fresh plugin answers `en` and creates no file.
#[tokio::test]
async fn a_fresh_plugin_answers_en() {
    assert_eq!(DEFAULT_LANGUAGE, "en");
    let dir = tempfile::tempdir().unwrap();
    let p = boot(dir.path(), None).await;
    assert_eq!(*p.language.lock().await, "en");
    assert_eq!(set(&p, "fr").await, json!({ "v": 1, "language": "en" }));
    assert_eq!(stored(dir.path()), None, "a refused tag writes nothing");
}

/// `en` applies, is stored, and is answered; tags compare without
/// regard to case and the catalogue's spelling is stored.
#[tokio::test]
async fn en_applies_and_is_stored() {
    let dir = tempfile::tempdir().unwrap();
    let p = boot(dir.path(), None).await;
    assert_eq!(set(&p, " EN ").await, json!({ "v": 1, "language": "en" }));
    assert_eq!(stored(dir.path()).as_deref(), Some("language = \"en\"\n"));
    assert!(!dir.path().join("language.toml.tmp").exists());
}

/// Pin: no tag other than `en` is stored. Every other tag answers
/// the language still in force, not an error, and leaves the stored
/// file byte for byte and time for time as it was.
#[tokio::test]
async fn no_tag_other_than_en_is_stored() {
    let dir = tempfile::tempdir().unwrap();
    let p = boot(dir.path(), None).await;
    set(&p, "en").await;
    let path = dir.path().join(STATE_FILENAME);
    let before = (
        std::fs::read(&path).unwrap(),
        std::fs::metadata(&path).unwrap().modified().unwrap(),
    );
    for tag in [
        "fr", "de-DE", "en-US", "en-GB", "zh-Hans", "", "  ", "english",
    ] {
        assert_eq!(
            set(&p, tag).await,
            json!({ "v": 1, "language": "en" }),
            "{tag:?}"
        );
    }
    let after = (
        std::fs::read(&path).unwrap(),
        std::fs::metadata(&path).unwrap().modified().unwrap(),
    );
    assert_eq!(after, before, "the stored file moved");
}

/// The stored language survives a restart. With a second catalogue
/// so a stored language can differ from the default: a set stores
/// `de`, a plugin booted again over the same state directory has
/// `de` in force, and a refused tag after the restart answers `de`.
#[tokio::test]
async fn the_stored_language_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let first = boot(dir.path(), Some(&["en", "de"])).await;
    assert_eq!(set(&first, "de").await["language"], "de");
    drop(first);
    let again = boot(dir.path(), Some(&["en", "de"])).await;
    assert_eq!(*again.language.lock().await, "de");
    assert_eq!(set(&again, "fr").await["language"], "de");
}

/// A stored tag the glass has no catalogue for, a file that does
/// not parse, and an empty file are all no stored language: `en` is
/// in force, and nothing is rewritten.
#[tokio::test]
async fn a_stored_tag_without_a_catalogue_is_no_stored_language() {
    for body in [
        "language = \"fr\"\n",
        "language = [\n",
        "",
        "language = 7\n",
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(STATE_FILENAME), body).unwrap();
        let p = boot(dir.path(), None).await;
        assert_eq!(*p.language.lock().await, "en", "{body:?}");
        assert_eq!(
            stored(dir.path()).as_deref(),
            Some(body),
            "load rewrote the file"
        );
    }
}

/// A payload of the wrong version or shape is an error; that is a
/// malformed request, not a refused tag.
#[tokio::test]
async fn a_malformed_payload_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let p = boot(dir.path(), None).await;
    for payload in [
        json!({ "v": 2, "language": "en" }),
        json!({ "v": 1 }),
        json!({ "v": 1, "language": 7 }),
    ] {
        assert!(
            matches!(
                p.handle_request(&req(payload.clone())).await,
                Err(PluginError::Permanent(_))
            ),
            "{payload}"
        );
    }
    assert_eq!(stored(dir.path()), None);
}

// ----- timezone.set -----

/// What the host and the dispatcher were asked, in order, on the
/// test clock.
#[derive(Debug, Clone, PartialEq)]
enum Seen {
    Apply(String),
    Reschedule,
}

#[derive(Default)]
struct Timeline(Mutex<Vec<(Seen, tokio::time::Instant)>>);

impl Timeline {
    fn push(&self, seen: Seen) {
        self.0
            .lock()
            .unwrap()
            .push((seen, tokio::time::Instant::now()));
    }

    fn seen(&self) -> Vec<Seen> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|(s, _)| s.clone())
            .collect()
    }

    /// The time from each apply to the reschedule after it.
    fn gaps(&self) -> Vec<Duration> {
        let events = self.0.lock().unwrap();
        events
            .windows(2)
            .filter_map(|w| match (&w[0].0, &w[1].0) {
                (Seen::Apply(_), Seen::Reschedule) => Some(w[1].1 - w[0].1),
                _ => None,
            })
            .collect()
    }
}

/// Stands in for the host: a zone in force, the zones its database
/// holds in the host's order, and an apply that can fail, fail after
/// moving the zone, or report success and move nothing.
struct StubHost {
    timeline: Arc<Timeline>,
    in_force: Mutex<String>,
    held: Mutex<Vec<&'static str>>,
    apply_fails: AtomicBool,
    failed_apply_moves: AtomicBool,
    apply_moves_nothing: AtomicBool,
    zone_unreadable: AtomicBool,
    list_unreadable: AtomicBool,
}

impl StubHost {
    fn new(timeline: &Arc<Timeline>, in_force: &str) -> Self {
        Self {
            timeline: Arc::clone(timeline),
            in_force: Mutex::new(in_force.to_string()),
            held: Mutex::new(vec![
                "Etc/UTC",
                "Europe/London",
                "America/New_York",
                FACTORY,
            ]),
            apply_fails: AtomicBool::new(false),
            failed_apply_moves: AtomicBool::new(false),
            apply_moves_nothing: AtomicBool::new(false),
            zone_unreadable: AtomicBool::new(false),
            list_unreadable: AtomicBool::new(false),
        }
    }

    fn in_force(&self) -> String {
        self.in_force.lock().unwrap().clone()
    }

    fn hold(&self, zones: &[&'static str]) {
        *self.held.lock().unwrap() = zones.to_vec();
    }
}

impl ZoneHost for StubHost {
    fn zone_in_force(&self) -> HostFuture<'_, String> {
        Box::pin(async {
            if self.zone_unreadable.load(Ordering::SeqCst) {
                return Err("timedatectl show failed".to_string());
            }
            Ok(self.in_force())
        })
    }

    fn holds<'a>(&'a self, zone: &'a str) -> HostFuture<'a, bool> {
        Box::pin(async move {
            if self.list_unreadable.load(Ordering::SeqCst) {
                return Err("timedatectl list-timezones failed".to_string());
            }
            Ok(self.held.lock().unwrap().contains(&zone))
        })
    }

    fn zones(&self) -> HostFuture<'_, Vec<String>> {
        Box::pin(async {
            if self.list_unreadable.load(Ordering::SeqCst) {
                return Err("timedatectl list-timezones failed".to_string());
            }
            Ok(self
                .held
                .lock()
                .unwrap()
                .iter()
                .map(|z| (*z).to_string())
                .collect())
        })
    }

    fn apply<'a>(&'a self, zone: &'a str) -> HostFuture<'a, ()> {
        Box::pin(async move {
            self.timeline.push(Seen::Apply(zone.to_string()));
            let fails = self.apply_fails.load(Ordering::SeqCst);
            let moves = if fails {
                self.failed_apply_moves.load(Ordering::SeqCst)
            } else {
                !self.apply_moves_nothing.load(Ordering::SeqCst)
            };
            if moves {
                *self.in_force.lock().unwrap() = zone.to_string();
            }
            if fails {
                return Err("sudo: evo-set-timezone exited 1".to_string());
            }
            Ok(())
        })
    }
}

/// Stands in for the shelf dispatcher: records each call, and
/// refuses when told to.
struct StubDispatcher {
    timeline: Arc<Timeline>,
    calls: Mutex<Vec<(String, String, Value)>>,
    refuse: AtomicBool,
}

impl StubDispatcher {
    fn new(timeline: &Arc<Timeline>) -> Self {
        Self {
            timeline: Arc::clone(timeline),
            calls: Mutex::new(Vec::new()),
            refuse: AtomicBool::new(false),
        }
    }

    fn calls(&self) -> Vec<(String, String, Value)> {
        self.calls.lock().unwrap().clone()
    }
}

impl ShelfRequestDispatcher for StubDispatcher {
    fn dispatch<'a>(
        &'a self,
        shelf: &'a str,
        request_type: &'a str,
        payload: Vec<u8>,
        _instance_id: Option<&'a str>,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = Result<Vec<u8>, ShelfDispatchError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.timeline.push(Seen::Reschedule);
            self.calls.lock().unwrap().push((
                shelf.to_string(),
                request_type.to_string(),
                serde_json::from_slice(&payload).unwrap(),
            ));
            if self.refuse.load(Ordering::SeqCst) {
                return Err(ShelfDispatchError::Transient {
                    detail: "the alarm plugin is restarting".to_string(),
                });
            }
            Ok(br#"{"v":1,"status":"ok"}"#.to_vec())
        })
    }
}

struct Rig {
    timeline: Arc<Timeline>,
    host: Arc<StubHost>,
    dispatcher: Arc<StubDispatcher>,
    plugin: LocalePlugin,
}

/// A loaded plugin over a stub host with `in_force` as its zone and
/// a stub dispatcher.
fn rig(in_force: &str) -> Rig {
    let timeline = Arc::new(Timeline::default());
    let host = Arc::new(StubHost::new(&timeline, in_force));
    let dispatcher = Arc::new(StubDispatcher::new(&timeline));
    let mut plugin = LocalePlugin::new();
    plugin.zone_host = Arc::clone(&host) as Arc<dyn ZoneHost>;
    plugin.dispatcher =
        Some(Arc::clone(&dispatcher) as Arc<dyn ShelfRequestDispatcher>);
    plugin.loaded = true;
    Rig {
        timeline,
        host,
        dispatcher,
        plugin,
    }
}

fn zone_req(payload: Value) -> Request {
    Request {
        request_type: VERB_TIMEZONE_SET.to_string(),
        ..req(payload)
    }
}

async fn set_zone(p: &LocalePlugin, zone: &str) -> Result<Value, PluginError> {
    p.handle_request(&zone_req(json!({ "v": 1, "timezone": zone })))
        .await
        .map(|resp| serde_json::from_slice(&resp.payload).unwrap())
}

fn transient(r: Result<Value, PluginError>) -> String {
    match r {
        Err(PluginError::Transient(msg)) => msg,
        other => panic!("expected a Transient error, got {other:?}"),
    }
}

/// Pin: timezone.set applies, waits more than one second, then
/// calls `alarm.reschedule { v: 1 }` on `audio.alarm` through the
/// shelf dispatcher, and answers the zone the host reports in
/// force.
#[tokio::test(start_paused = true)]
async fn timezone_set_applies_waits_then_reschedules() {
    let r = rig("Etc/UTC");
    assert!(SETTLE > Duration::from_secs(1));
    assert_eq!(
        set_zone(&r.plugin, "Europe/London").await.unwrap(),
        json!({ "v": 1, "timezone": "Europe/London" })
    );
    assert_eq!(
        r.timeline.seen(),
        [Seen::Apply("Europe/London".into()), Seen::Reschedule]
    );
    let gaps = r.timeline.gaps();
    assert_eq!(gaps.len(), 1);
    assert!(gaps[0] > Duration::from_secs(1), "waited {:?}", gaps[0]);
    assert_eq!(
        r.dispatcher.calls(),
        [(
            ALARM_SHELF.to_string(),
            VERB_ALARM_RESCHEDULE.to_string(),
            json!({ "v": 1 })
        )]
    );
}

/// Pin: a failed apply is not a clean answer and names the zone in
/// force. With the zone unmoved there is nothing for the alarm to
/// follow. A failed apply that moved the zone anyway still has the
/// alarm follow, after the wait, and the error names the new zone.
#[tokio::test(start_paused = true)]
async fn a_failed_apply_is_not_clean_and_names_the_zone_in_force() {
    let r = rig("Etc/UTC");
    r.host.apply_fails.store(true, Ordering::SeqCst);
    let msg = transient(set_zone(&r.plugin, "Europe/London").await);
    assert!(msg.contains("apply failed"), "{msg}");
    assert!(msg.contains("zone in force is Etc/UTC"), "{msg}");
    assert_eq!(r.timeline.seen(), [Seen::Apply("Europe/London".into())]);

    r.host.failed_apply_moves.store(true, Ordering::SeqCst);
    let msg = transient(set_zone(&r.plugin, "Europe/London").await);
    assert!(msg.contains("zone in force is Europe/London"), "{msg}");
    assert_eq!(
        r.timeline.seen()[1..],
        [Seen::Apply("Europe/London".into()), Seen::Reschedule]
    );
    assert!(r.timeline.gaps()[0] > Duration::from_secs(1));
}

/// Pin: a refused reschedule is not a clean answer and names the
/// zone in force, which stays applied: one apply, no roll-back.
/// Sending the same zone again runs the reschedule again.
#[tokio::test(start_paused = true)]
async fn a_refused_reschedule_is_not_clean_and_names_the_zone_in_force() {
    let r = rig("Etc/UTC");
    r.dispatcher.refuse.store(true, Ordering::SeqCst);
    let msg = transient(set_zone(&r.plugin, "Europe/London").await);
    assert!(msg.contains("zone in force is Europe/London"), "{msg}");
    assert!(msg.contains(VERB_ALARM_RESCHEDULE), "{msg}");
    assert_eq!(r.host.in_force(), "Europe/London", "the zone stays applied");
    assert_eq!(
        r.timeline.seen(),
        [Seen::Apply("Europe/London".into()), Seen::Reschedule]
    );

    r.dispatcher.refuse.store(false, Ordering::SeqCst);
    assert_eq!(
        set_zone(&r.plugin, "Europe/London").await.unwrap(),
        json!({ "v": 1, "timezone": "Europe/London" })
    );
    assert_eq!(
        r.timeline.seen()[2..],
        [Seen::Apply("Europe/London".into()), Seen::Reschedule]
    );
    assert!(r
        .timeline
        .gaps()
        .iter()
        .all(|g| *g > Duration::from_secs(1)));
}

/// Pin: an unknown zone, and `Factory`, answer the zone still in
/// force and run no apply. The stub's database holds `Factory`, so
/// only the plugin's own refusal keeps it out.
#[tokio::test(start_paused = true)]
async fn an_unknown_zone_and_factory_answer_the_zone_in_force_and_apply_nothing(
) {
    let r = rig("Etc/UTC");
    for zone in [
        FACTORY,
        "Not/Held",
        "",
        "../Europe/London",
        "Europe/../London",
        "/Europe/London",
        "Europe/London/",
        "Europe/London -H host",
        "Europe/London\n",
        "Europe//London",
    ] {
        assert_eq!(
            set_zone(&r.plugin, zone).await.unwrap(),
            json!({ "v": 1, "timezone": "Etc/UTC" }),
            "{zone:?}"
        );
    }
    assert_eq!(r.timeline.seen(), [], "nothing was applied or rescheduled");
    assert_eq!(r.host.in_force(), "Etc/UTC");
}

/// A name reaches the host only in the shape of an IANA zone, so it
/// cannot walk out of the zone database when its file is read.
#[test]
fn zone_names_have_the_iana_shape() {
    for name in [
        "UTC",
        "Etc/UTC",
        "Etc/GMT+5",
        "Etc/GMT-14",
        "America/Argentina/Buenos_Aires",
        "America/Port-au-Prince",
    ] {
        assert!(timezone::is_zone_name(name), "{name:?} is refused");
    }
    for name in [
        "",
        "/",
        "..",
        "../etc/passwd",
        "Europe/../London",
        "/Europe/London",
        "Europe/London/",
        "Europe//London",
        "Europe/London -H host",
        "Europe/London\n",
        "Europe/Lon.don",
        "Europe\\London",
    ] {
        assert!(!timezone::is_zone_name(name), "{name:?} is accepted");
    }
}

/// Without a shelf dispatcher the alarm cannot follow, so nothing
/// is applied; the refusal names the zone in force.
#[tokio::test(start_paused = true)]
async fn without_a_dispatcher_nothing_is_applied() {
    let mut r = rig("Etc/UTC");
    r.plugin.dispatcher = None;
    match set_zone(&r.plugin, "Europe/London").await {
        Err(PluginError::Permanent(msg)) => {
            assert!(msg.contains("zone in force is Etc/UTC"), "{msg}")
        }
        other => panic!("expected Permanent, got {other:?}"),
    }
    assert_eq!(r.timeline.seen(), []);
}

/// A zone in force or a zone database that cannot be read applies
/// nothing.
#[tokio::test(start_paused = true)]
async fn an_unreadable_host_applies_nothing() {
    let r = rig("Etc/UTC");
    r.host.list_unreadable.store(true, Ordering::SeqCst);
    let msg = transient(set_zone(&r.plugin, "Europe/London").await);
    assert!(msg.contains("zone in force is Etc/UTC"), "{msg}");
    r.host.list_unreadable.store(false, Ordering::SeqCst);
    r.host.zone_unreadable.store(true, Ordering::SeqCst);
    let msg = transient(set_zone(&r.plugin, "Europe/London").await);
    assert!(msg.contains("could not be read"), "{msg}");
    assert_eq!(r.timeline.seen(), []);
}

/// An apply that reports success but leaves another zone in force
/// is not clean: the answer is never a zone that was asked for and
/// not applied.
#[tokio::test(start_paused = true)]
async fn an_apply_that_moves_nothing_is_not_clean() {
    let r = rig("Etc/UTC");
    r.host.apply_moves_nothing.store(true, Ordering::SeqCst);
    let msg = transient(set_zone(&r.plugin, "Europe/London").await);
    assert!(msg.contains("zone in force is Etc/UTC"), "{msg}");
    assert!(msg.contains("not Europe/London"), "{msg}");
}

/// A payload of the wrong version or shape is a malformed request,
/// and touches nothing.
#[tokio::test(start_paused = true)]
async fn a_malformed_zone_payload_is_refused() {
    let r = rig("Etc/UTC");
    for payload in [
        json!({ "v": 2, "timezone": "Europe/London" }),
        json!({ "v": 1 }),
        json!({ "v": 1, "timezone": 7 }),
    ] {
        assert!(
            matches!(
                r.plugin.handle_request(&zone_req(payload.clone())).await,
                Err(PluginError::Permanent(_))
            ),
            "{payload}"
        );
    }
    assert_eq!(r.timeline.seen(), []);
}

// ----- timezone.get and timezone.list -----

async fn read(p: &LocalePlugin, verb: &str) -> Result<Value, PluginError> {
    let request = Request {
        request_type: verb.to_string(),
        ..req(json!({ "v": 1 }))
    };
    p.handle_request(&request)
        .await
        .map(|resp| serde_json::from_slice(&resp.payload).unwrap())
}

/// Pin: timezone.get names the zone in force, as the host names it
/// on this read: after a set, and after the host moved it on its
/// own. A read that fails is an error, never a zone.
#[tokio::test(start_paused = true)]
async fn timezone_get_names_only_the_zone_in_force() {
    let r = rig("Europe/London");
    assert_eq!(
        read(&r.plugin, VERB_TIMEZONE_GET).await.unwrap(),
        json!({ "v": 1, "timezone": "Europe/London" })
    );
    set_zone(&r.plugin, "America/New_York").await.unwrap();
    assert_eq!(
        read(&r.plugin, VERB_TIMEZONE_GET).await.unwrap()["timezone"],
        "America/New_York"
    );
    *r.host.in_force.lock().unwrap() = "Etc/UTC".to_string();
    assert_eq!(
        read(&r.plugin, VERB_TIMEZONE_GET).await.unwrap()["timezone"],
        "Etc/UTC"
    );

    r.host.zone_unreadable.store(true, Ordering::SeqCst);
    let msg = transient(read(&r.plugin, VERB_TIMEZONE_GET).await);
    assert!(msg.contains("could not be read"), "{msg}");
    assert_eq!(
        r.timeline.seen(),
        [Seen::Apply("America/New_York".into()), Seen::Reschedule],
        "a read applied or rescheduled something"
    );
}

/// Pin: timezone.list answers the zones a set would apply, in the
/// host's order: not `Factory`, not a name of another shape, and
/// every name it lists is one a set applies.
#[tokio::test(start_paused = true)]
async fn timezone_list_answers_only_zones_a_set_would_apply() {
    let r = rig("Etc/UTC");
    r.host.hold(&[
        "Etc/UTC",
        FACTORY,
        "Europe/London",
        "posix/Not.A.Zone",
        "America/New_York",
        "Bad Name",
    ]);
    let answer = read(&r.plugin, VERB_TIMEZONE_LIST).await.unwrap();
    assert_eq!(
        answer,
        json!({
            "v": 1,
            "timezones": ["Etc/UTC", "Europe/London", "America/New_York"],
        })
    );
    assert_eq!(r.timeline.seen(), [], "listing applied something");
    for zone in answer["timezones"].as_array().unwrap() {
        let zone = zone.as_str().unwrap();
        assert_eq!(
            set_zone(&r.plugin, zone).await.unwrap()["timezone"],
            zone,
            "{zone} is listed, and a set does not apply it"
        );
    }
}

/// Pin: a list that cannot be read is an error, never `[]`. Nor is a
/// zone database that holds no zone a set would apply answered as
/// `[]`.
#[tokio::test(start_paused = true)]
async fn a_list_that_cannot_be_read_is_not_clean() {
    let r = rig("Etc/UTC");
    r.host.list_unreadable.store(true, Ordering::SeqCst);
    let msg = transient(read(&r.plugin, VERB_TIMEZONE_LIST).await);
    assert!(msg.contains("could not be read"), "{msg}");
    r.host.list_unreadable.store(false, Ordering::SeqCst);
    for held in [
        &[][..],
        &[FACTORY][..],
        &["Bad Name", "posix/Not.A.Zone"][..],
    ] {
        r.host.hold(held);
        transient(read(&r.plugin, VERB_TIMEZONE_LIST).await);
    }
}

/// Pin: the host's list is never answered in part. A zone file that
/// cannot be read fails the whole list; a file that is not there
/// drops only its name; a name of another shape is dropped before
/// its file is looked at.
#[tokio::test]
async fn the_host_list_is_never_answered_in_part() {
    let listed = "Etc/UTC\nEurope/London\n../etc/passwd\nAmerica/New_York\n";
    let looked = Mutex::new(Vec::new());
    let file = |unreadable: &'static str, absent: &'static str| {
        let looked = &looked;
        move |zone: String| {
            looked.lock().unwrap().push(zone.clone());
            let answer = if zone == unreadable {
                Err(format!("could not read {zone}"))
            } else {
                Ok(zone != absent)
            };
            async move { answer }
        }
    };

    assert!(
        timezone::zones_with_files(listed, file("Europe/London", ""))
            .await
            .is_err(),
        "a list with an unreadable zone file was answered"
    );
    assert_eq!(
        timezone::zones_with_files(listed, file("", "Europe/London"))
            .await
            .unwrap(),
        ["Etc/UTC", "America/New_York"]
    );
    assert!(
        !looked.lock().unwrap().iter().any(|z| z.contains("..")),
        "a name of another shape had its file looked at: {:?}",
        looked.lock().unwrap()
    );
}

/// A read's payload of the wrong version or shape is a malformed
/// request.
#[tokio::test(start_paused = true)]
async fn a_malformed_read_payload_is_refused() {
    let r = rig("Etc/UTC");
    for verb in [VERB_TIMEZONE_GET, VERB_TIMEZONE_LIST] {
        for payload in [json!({ "v": 2 }), json!(null), json!("v")] {
            let request = Request {
                request_type: verb.to_string(),
                ..req(payload.clone())
            };
            assert!(
                matches!(
                    r.plugin.handle_request(&request).await,
                    Err(PluginError::Permanent(_))
                ),
                "{verb} {payload}"
            );
        }
    }
}
