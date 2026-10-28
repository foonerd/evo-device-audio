// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Tests for the alarm plugin, against a stub appointment ledger.

use std::sync::Mutex;

use serde_json::{json, Value};

use super::*;

/// What the plugin asked of the ledger, in order.
#[derive(Debug, Clone, PartialEq)]
enum LedgerCall {
    Cancel(String),
    Create(AppointmentSpec, AppointmentAction),
}

/// Stands in for the framework appointment ledger: records every
/// call and, when told to, refuses the create.
#[derive(Default)]
struct StubLedger {
    calls: Mutex<Vec<LedgerCall>>,
    refuse_create: bool,
}

impl StubLedger {
    fn calls(&self) -> Vec<LedgerCall> {
        self.calls.lock().unwrap().clone()
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
                .push(LedgerCall::Create(spec, action));
            if self.refuse_create {
                return Err(ReportError::Invalid("quota".to_string()));
            }
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

/// A set cancels the appointment `alarm` and schedules it again:
/// weekly on the named days in the order given, local time,
/// waking the device, no pre-arm; the fire loads the playlist into
/// the queue from its first entry. The answer echoes the days as
/// given and the name trimmed.
#[tokio::test]
async fn a_set_cancels_then_schedules_the_one_appointment() {
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
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0], LedgerCall::Cancel(APPOINTMENT_ID.to_string()));
    let LedgerCall::Create(spec, action) = &calls[1] else {
        panic!("second call is the create: {calls:?}");
    };
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

/// A second set replaces the first: cancel, schedule, cancel,
/// schedule, under the same id.
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
    let calls = ledger.calls();
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[0], LedgerCall::Cancel("alarm".to_string()));
    assert_eq!(calls[2], LedgerCall::Cancel("alarm".to_string()));
    let created = ledger.created();
    assert_eq!(created[1].0.appointment_id, "alarm");
    assert_eq!(created[1].0.time.as_deref(), Some("09:15"));
    assert_eq!(created[1].1.payload["playlist_name"], "Late");
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

/// A ledger that refuses the create after the cancel leaves no
/// alarm, and the set answers that refusal.
#[tokio::test]
async fn a_create_the_ledger_refuses_answers_the_refusal() {
    let ledger = Arc::new(StubLedger {
        refuse_create: true,
        ..StubLedger::default()
    });
    let p = plugin_on(&ledger);
    let err = p
        .handle_request(&set(json!(["mon"]), "07:00", "Morning"))
        .await
        .unwrap_err();
    assert!(matches!(err, PluginError::Permanent(_)), "{err:?}");
    assert_eq!(ledger.calls().len(), 2, "cancel, then the refused create");
}

// ----- alarm.clear -----

/// A clear cancels the appointment `alarm` and answers ok, also
/// when no alarm is set.
#[tokio::test]
async fn a_clear_cancels_and_answers_ok_even_when_none_is_set() {
    let ledger = Arc::new(StubLedger::default());
    let p = plugin_on(&ledger);
    for _ in 0..2 {
        let resp = p
            .handle_request(&req(VERB_CLEAR, json!({ "v": 1 })))
            .await
            .unwrap();
        assert_eq!(body(&resp), json!({ "v": 1, "status": "ok" }));
    }
    assert_eq!(
        ledger.calls(),
        vec![
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
