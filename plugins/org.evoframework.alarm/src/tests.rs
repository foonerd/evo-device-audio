// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Tests for the alarm plugin, against a stub appointment ledger.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use evo_plugin_sdk::contract::AppointmentState;
use serde_json::{json, Value};

use super::*;

/// What the plugin asked of the ledger, in order.
#[derive(Debug, Clone, PartialEq)]
enum LedgerCall {
    Cancel(String),
    Create(AppointmentSpec, AppointmentAction),
}

/// The ledger's entry for `alarm`: what was scheduled, and its
/// state.
#[derive(Debug, Clone, PartialEq)]
struct Entry {
    spec: AppointmentSpec,
    action: AppointmentAction,
    state: AppointmentState,
}

/// Stands in for the framework appointment ledger: records every
/// call and keeps the entry for `alarm` as the ledger does. A
/// create re-issues the id in place as `Pending`, a refused create
/// leaves the entry as it was, and a cancel marks the entry
/// `Cancelled` and keeps it. Told to, it refuses creates.
#[derive(Default)]
struct StubLedger {
    calls: Mutex<Vec<LedgerCall>>,
    entry: Mutex<Option<Entry>>,
    refuse_create: AtomicBool,
}

impl StubLedger {
    fn calls(&self) -> Vec<LedgerCall> {
        self.calls.lock().unwrap().clone()
    }

    fn entry(&self) -> Option<Entry> {
        self.entry.lock().unwrap().clone()
    }

    fn created(&self) -> Vec<(AppointmentSpec, AppointmentAction)> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                LedgerCall::Create(spec, action) => Some((spec, action)),
                LedgerCall::Cancel(_) => None,
            })
            .collect()
    }
}

impl AppointmentScheduler for StubLedger {
    fn create_appointment<'a>(
        &'a self,
        spec: AppointmentSpec,
        action: AppointmentAction,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = Result<AppointmentId, ReportError>> + Send + 'a,
        >,
    > {
        Box::pin(async move {
            let id = AppointmentId::new(spec.appointment_id.clone());
            self.calls
                .lock()
                .unwrap()
                .push(LedgerCall::Create(spec.clone(), action.clone()));
            if self.refuse_create.load(Ordering::SeqCst) {
                return Err(ReportError::Invalid("quota".to_string()));
            }
            *self.entry.lock().unwrap() = Some(Entry {
                spec,
                action,
                state: AppointmentState::Pending,
            });
            Ok(id)
        })
    }

    fn cancel_appointment<'a>(
        &'a self,
        id: AppointmentId,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push(LedgerCall::Cancel(id.as_str().to_string()));
            if let Some(entry) = self.entry.lock().unwrap().as_mut() {
                entry.state = AppointmentState::Cancelled;
            }
            Ok(())
        })
    }
}

fn plugin_on(ledger: &Arc<StubLedger>) -> AlarmPlugin {
    let mut p = AlarmPlugin::new();
    p.appointments = Some(Arc::clone(ledger) as Arc<dyn AppointmentScheduler>);
    p.loaded = true;
    p
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

fn set(weekdays: Value, time: &str, playlist_name: &str) -> Request {
    req(
        VERB_SET,
        json!({
            "v": 1,
            "weekdays": weekdays,
            "time": time,
            "playlist_name": playlist_name,
        }),
    )
}

fn body(resp: &Response) -> Value {
    serde_json::from_slice(&resp.payload).unwrap()
}

// ----- manifest / surface -----

#[test]
fn both_manifests_stock_audio_alarm_with_two_verbs_and_the_ledger() {
    let oop = Manifest::from_toml(include_str!("../manifest.oop.toml"))
        .expect("manifest.oop.toml parses");
    for m in [manifest(), oop] {
        assert_eq!(m.plugin.name, PLUGIN_NAME);
        assert_eq!(m.target.shelf, "audio.alarm");
        assert_eq!(m.target.shape, 1);
        assert!(m.capabilities.appointments, "the ledger capability");
        let resp = m.capabilities.respondent.expect("respondent");
        assert_eq!(resp.request_types, vec![VERB_SET, VERB_CLEAR]);
    }
}

#[tokio::test]
async fn describe_names_the_manifest_verbs() {
    let d = AlarmPlugin::new().describe().await;
    assert_eq!(d.identity.name, PLUGIN_NAME);
    let declared = manifest().capabilities.respondent.unwrap().request_types;
    assert_eq!(d.runtime_capabilities.request_types, declared);
}

// ----- alarm.set -----

/// A set schedules the appointment `alarm` and does not cancel
/// first: weekly on the named days in the order given, local time,
/// waking the device, no pre-arm; the fire loads the playlist into
/// the queue from its first entry. The answer echoes the days as
/// given and the name trimmed.
#[tokio::test]
async fn a_set_schedules_the_one_appointment() {
    let ledger = Arc::new(StubLedger::default());
    let p = plugin_on(&ledger);
    let resp = p
        .handle_request(&set(json!(["fri", "mon"]), "07:30", "  Morning "))
        .await
        .unwrap();
    assert_eq!(
        body(&resp),
        json!({
            "v": 1,
            "alarm": {
                "weekdays": ["fri", "mon"],
                "time": "07:30",
                "playlist_name": "Morning",
            },
        })
    );
    let calls = ledger.calls();
    assert_eq!(calls.len(), 1, "one create, no cancel: {calls:?}");
    let LedgerCall::Create(spec, action) = &calls[0] else {
        panic!("the one call is the create: {calls:?}");
    };
    assert_eq!(
        ledger.entry(),
        Some(Entry {
            spec: spec.clone(),
            action: action.clone(),
            state: AppointmentState::Pending,
        })
    );
    assert_eq!(spec.appointment_id, "alarm");
    assert_eq!(spec.time.as_deref(), Some("07:30"));
    assert_eq!(spec.zone, AppointmentTimeZone::Local);
    assert_eq!(
        spec.recurrence,
        AppointmentRecurrence::Weekly {
            days: vec![DayOfWeek::Fri, DayOfWeek::Mon],
        }
    );
    assert!(spec.must_wake_device);
    assert_eq!(spec.wake_pre_arm_ms, None);
    assert_eq!(spec.pre_fire_ms, None);
    assert_eq!(spec.end_time_ms, None);
    assert_eq!(spec.max_fires, None);
    assert!(spec.except.is_empty());
    assert_eq!(action.target_shelf, "audio.queue");
    assert_eq!(action.request_type, "queue.load_playlist_to_queue");
    assert_eq!(
        action.payload,
        json!({ "v": 1, "playlist_name": "Morning", "start_position": 0 })
    );
}

/// The miss policy is whatever the ledger gives a spec that names
/// none.
#[test]
fn the_miss_policy_is_the_ledger_default() {
    let alarm = Alarm::new(vec!["mon".into()], "06:00".into(), "Wake").unwrap();
    let (spec, _) = alarm.appointment();
    let unnamed: AppointmentSpec = serde_json::from_value(json!({
        "appointment_id": "alarm",
        "time": "06:00",
        "recurrence": { "kind": "weekly", "days": ["mon"] },
    }))
    .unwrap();
    assert_eq!(spec.miss_policy, unnamed.miss_policy);
}

/// A second set replaces the first: two creates under the same id,
/// no cancel, and the second is the one alarm.
#[tokio::test]
async fn a_new_set_replaces_the_alarm() {
    let ledger = Arc::new(StubLedger::default());
    let p = plugin_on(&ledger);
    p.handle_request(&set(json!(["mon"]), "06:00", "Early"))
        .await
        .unwrap();
    p.handle_request(&set(json!(["sat", "sun"]), "09:15", "Late"))
        .await
        .unwrap();
    let created = ledger.created();
    assert_eq!(ledger.calls().len(), 2, "two creates, no cancel");
    assert_eq!(created[1].0.appointment_id, "alarm");
    let entry = ledger.entry().expect("an alarm stands");
    assert_eq!(entry.state, AppointmentState::Pending);
    assert_eq!(entry.spec.time.as_deref(), Some("09:15"));
    assert_eq!(entry.action.payload["playlist_name"], "Late");
}

/// Every refusal comes before the ledger is touched, so the alarm
/// in place stays.
#[tokio::test]
async fn a_refused_set_schedules_nothing() {
    let ledger = Arc::new(StubLedger::default());
    let p = plugin_on(&ledger);
    let refused = [
        set(json!([]), "07:00", "Morning"),
        set(json!(["mon", "mon"]), "07:00", "Morning"),
        set(json!(["mon", "tue", "mon"]), "07:00", "Morning"),
        set(json!(["monday"]), "07:00", "Morning"),
        set(json!(["Mon"]), "07:00", "Morning"),
        set(json!(["mon"]), "7:00", "Morning"),
        set(json!(["mon"]), "24:00", "Morning"),
        set(json!(["mon"]), "12:60", "Morning"),
        set(json!(["mon"]), "12-30", "Morning"),
        set(json!(["mon"]), "1230", "Morning"),
        set(json!(["mon"]), "ab:cd", "Morning"),
        set(json!(["mon"]), " 07:00", "Morning"),
        set(json!(["mon"]), "07:00 ", "Morning"),
        set(json!(["mon"]), "07:00", ""),
        set(json!(["mon"]), "07:00", "   "),
        req(
            VERB_SET,
            json!({
                "v": 2, "weekdays": ["mon"], "time": "07:00",
                "playlist_name": "Morning",
            }),
        ),
    ];
    for r in &refused {
        match p.handle_request(r).await {
            Err(PluginError::Permanent(_)) => {}
            other => panic!(
                "{} must be refused, got {other:?}",
                String::from_utf8_lossy(&r.payload)
            ),
        }
    }
    assert!(ledger.calls().is_empty(), "nothing reached the ledger");
    // The edges of the clock are accepted.
    for time in ["00:00", "23:59"] {
        p.handle_request(&set(json!(["sun"]), time, "Edge"))
            .await
            .unwrap();
    }
}

/// Pin: a failed create leaves the previous appointment pending.
/// The set answers the ledger's refusal, nothing is cancelled, and
/// the appointment that was set is neither cancelled nor absent.
#[tokio::test]
async fn a_failed_create_leaves_the_alarm_that_was_set() {
    let ledger = Arc::new(StubLedger::default());
    let p = plugin_on(&ledger);
    p.handle_request(&set(json!(["mon"]), "06:00", "Early"))
        .await
        .unwrap();
    let before = ledger.entry().expect("the first alarm stands");
    assert_eq!(before.state, AppointmentState::Pending);

    ledger.refuse_create.store(true, Ordering::SeqCst);
    let err = p
        .handle_request(&set(json!(["sat"]), "09:15", "Late"))
        .await
        .unwrap_err();
    assert!(matches!(err, PluginError::Permanent(_)), "{err:?}");
    match ledger.entry() {
        None => panic!("the alarm that was set is absent"),
        Some(after) => {
            assert_eq!(
                after.state,
                AppointmentState::Pending,
                "the alarm that was set is {:?}",
                after.state
            );
            assert_eq!(after, before, "the alarm that was set");
        }
    }
    assert!(
        !ledger
            .calls()
            .iter()
            .any(|c| matches!(c, LedgerCall::Cancel(_))),
        "a set never cancels"
    );
}

// ----- alarm.clear -----

/// A clear cancels the appointment `alarm` and answers ok, and a
/// second clear, with no alarm set, answers ok too.
#[tokio::test]
async fn a_clear_cancels_and_answers_ok_even_when_none_is_set() {
    let ledger = Arc::new(StubLedger::default());
    let p = plugin_on(&ledger);
    p.handle_request(&set(json!(["mon"]), "06:00", "Early"))
        .await
        .unwrap();
    for _ in 0..2 {
        let resp = p
            .handle_request(&req(VERB_CLEAR, json!({ "v": 1 })))
            .await
            .unwrap();
        assert_eq!(body(&resp), json!({ "v": 1, "status": "ok" }));
        assert_eq!(
            ledger.entry().map(|e| e.state),
            Some(AppointmentState::Cancelled),
            "no alarm remains"
        );
    }
    assert_eq!(
        ledger.calls()[1..],
        [
            LedgerCall::Cancel("alarm".to_string()),
            LedgerCall::Cancel("alarm".to_string()),
        ]
    );
}

/// Without the ledger handle neither verb can do its work, and both
/// refuse.
#[tokio::test]
async fn without_the_ledger_both_verbs_refuse() {
    let mut p = AlarmPlugin::new();
    p.loaded = true;
    for r in [
        set(json!(["mon"]), "07:00", "Morning"),
        req(VERB_CLEAR, json!({ "v": 1 })),
    ] {
        match p.handle_request(&r).await {
            Err(PluginError::Permanent(msg)) => {
                assert!(msg.contains("appointment ledger"), "got: {msg}")
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }
}

// ----- pin: the alarm reaches only the ledger -----

/// Pin: the alarm does not call the sleep timer, does not write
/// crossfade, and does not program the RTC itself. Its only effect
/// is the ledger appointment, whose action is the queue load and
/// nothing else; and no source this crate ships, and none of its
/// manifests, names the sleep-timer verb, the options shelf,
/// crossfade, the RTC hook or its sysfs node, or the shelf
/// dispatcher. The crate does not depend on the steward crate, where
/// the RTC hook lives.
#[tokio::test]
async fn the_alarm_reaches_only_the_ledger() {
    let ledger = Arc::new(StubLedger::default());
    let p = plugin_on(&ledger);
    p.handle_request(&set(json!(["mon", "wed"]), "06:45", "Morning"))
        .await
        .unwrap();
    p.handle_request(&req(VERB_CLEAR, json!({ "v": 1 })))
        .await
        .unwrap();
    for (_, action) in ledger.created() {
        assert_eq!(action.target_shelf, "audio.queue");
        assert_eq!(action.request_type, "queue.load_playlist_to_queue");
        let mut keys: Vec<&str> = action
            .payload
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["playlist_name", "start_position", "v"]);
    }

    let shipped = [
        ("src/lib.rs", include_str!("lib.rs")),
        ("src/bin/alarm-wire.rs", include_str!("bin/alarm-wire.rs")),
        ("manifest.toml", include_str!("../manifest.toml")),
        ("manifest.oop.toml", include_str!("../manifest.oop.toml")),
        ("Cargo.toml", include_str!("../Cargo.toml")),
    ];
    let forbidden = [
        "set_sleep_timer",
        "audio.options",
        "crossfade",
        "RtcWakeCallback",
        "program_wake",
        "wakealarm",
        "/sys/class/rtc",
        "shelf_request_dispatcher",
    ];
    for (file, text) in shipped {
        for token in forbidden {
            assert!(!text.contains(token), "{file} names {token:?}");
        }
    }
    let cargo = include_str!("../Cargo.toml");
    assert!(
        !cargo.lines().any(|l| {
            let l = l.trim_start();
            l.starts_with("evo ")
                || l.starts_with("evo=")
                || l.starts_with("evo.")
        }),
        "the crate depends on the steward crate"
    );
}
