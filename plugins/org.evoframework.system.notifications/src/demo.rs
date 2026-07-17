// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Reference-data emitter for the notifications substrate.
//!
//! Off by default. UI-team developers building widgets against
//! the `system_notifications_active` subject enable this
//! emitter by setting `EVO_NOTIFICATIONS_DEMO=1` before boot.
//! Once enabled, the framework spawns a background task that
//! emits a rotating set of sample notifications on a 5-second
//! cadence — every priority class, every level, grouped and
//! standalone, with and without audio, with and without
//! actions — so widget renderers see the full shape range
//! without waiting on real hardware events.
//!
//! Not for production. The env-gate is deliberate — production
//! deployments will never boot with the demo enabled unless
//! an operator explicitly sets the flag. A distribution's
//! production build script may unset the flag as an extra
//! belt-and-braces gate.
//!
//! ## What gets emitted
//!
//! Rotates through six sample notifications, cycling every
//! 5 seconds:
//!
//! 1. Doorbell chime (Routine, group=`doorbell`, with chime
//!    audio, with `answer` + `snooze_5m` actions,
//!    auto-dismiss 30s).
//! 2. Doorbell chime again (Routine, same group — exercises
//!    group coalescing).
//! 3. Motion alert (Important, level=Warning, standalone,
//!    with `view_camera` action, auto-dismiss 60s).
//! 4. Update available (Info, standalone, with `install_now`
//!    + `remind_later` actions, no auto-dismiss — pinned).
//! 5. Alarm firing (Critical, level=Alert, voice payload,
//!    with `stop_alarm` + `snooze_9m` actions, no auto-dismiss).
//! 6. Backup complete (Info, standalone, no actions,
//!    auto-dismiss 8s).
//!
//! Then cancels the first three and cycles back to step 1.
//! The full loop exercises every ActiveNotificationEnvelope
//! shape variant the UI has to render.

use crate::runtime::NotificationDispatcher;
use evo_plugin_sdk::contract::{
    AudioPayload, Notification, NotificationAction, NotificationGroupId,
    NotificationHandle, NotificationLevel, NotificationPriority,
};
use std::sync::Arc;
use std::time::Duration;

/// Env var operators set to enable the demo emitter. Any
/// non-empty value activates; `EVO_NOTIFICATIONS_DEMO=1` is
/// the canonical form.
pub const ENV_DEMO_ENABLED: &str = "EVO_NOTIFICATIONS_DEMO";

/// Cadence between rotation steps. 5 seconds is fast enough
/// for interactive widget development but slow enough to
/// avoid drowning the subject stream during less-active
/// debugging sessions.
pub const DEMO_CADENCE: Duration = Duration::from_secs(5);

/// Read the env-gate. Returns true when the demo emitter
/// should run.
pub fn demo_enabled() -> bool {
    std::env::var(ENV_DEMO_ENABLED)
        .ok()
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

/// Spawn the demo-emitter background task. No-op when the
/// env-gate is not set. Callers invoke this once at boot
/// after the NotificationDispatcher's subject publisher has
/// been attached.
///
/// Returns a `JoinHandle` when the emitter was spawned;
/// `None` when the env-gate was off. Callers that need to
/// tear the emitter down at shutdown retain the handle;
/// callers running the emitter for the process lifetime
/// discard it.
pub fn spawn_if_enabled(
    dispatcher: Arc<NotificationDispatcher>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !demo_enabled() {
        return None;
    }
    tracing::info!(
        cadence_ms = DEMO_CADENCE.as_millis() as u64,
        "notifications-demo: EVO_NOTIFICATIONS_DEMO set; spawning \
         rotating-sample emitter (development aid — do not enable in \
         production)"
    );
    Some(tokio::spawn(run_demo(dispatcher)))
}

async fn run_demo(dispatcher: Arc<NotificationDispatcher>) {
    // We rotate through six samples per cycle. Handles from
    // the first three (doorbell + motion) are retained so a
    // later step can cancel them and exercise the cancel
    // path. Handles from the pinned entries (update, alarm)
    // are retained for the same reason; they cancel at the
    // end of the cycle.
    let mut retained: Vec<NotificationHandle> = Vec::new();
    let mut cycle: u64 = 0;
    loop {
        // Step 1: doorbell.
        if let Ok((h, _)) = dispatcher.send(sample_doorbell(cycle), now_min()) {
            retained.push(h);
        }
        tokio::time::sleep(DEMO_CADENCE).await;

        // Step 2: doorbell again (same group → coalesces).
        if let Ok((_, _)) =
            dispatcher.send(sample_doorbell(cycle + 1), now_min())
        {
            // Group coalesce returns the existing handle;
            // no new entry to retain.
        }
        tokio::time::sleep(DEMO_CADENCE).await;

        // Step 3: motion alert.
        if let Ok((h, _)) = dispatcher.send(sample_motion(cycle), now_min()) {
            retained.push(h);
        }
        tokio::time::sleep(DEMO_CADENCE).await;

        // Step 4: pinned update available.
        if let Ok((h, _)) =
            dispatcher.send(sample_update_available(cycle), now_min())
        {
            retained.push(h);
        }
        tokio::time::sleep(DEMO_CADENCE).await;

        // Step 5: alarm firing.
        if let Ok((h, _)) = dispatcher.send(sample_alarm(cycle), now_min()) {
            retained.push(h);
        }
        tokio::time::sleep(DEMO_CADENCE).await;

        // Step 6: backup complete.
        let _ = dispatcher.send(sample_backup_complete(cycle), now_min());
        tokio::time::sleep(DEMO_CADENCE).await;

        // Cancel everything we retained. Ignore per-handle
        // NotFound errors — auto-dismiss may have already
        // removed some entries.
        for h in retained.drain(..) {
            let _ = dispatcher.cancel(&h);
        }
        cycle = cycle.wrapping_add(1);
    }
}

fn now_min() -> u16 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| ((d.as_secs() / 60) % 1440) as u16)
        .unwrap_or(0)
}

fn sample_doorbell(cycle: u64) -> Notification {
    Notification {
        level: NotificationLevel::Info,
        source_plugin: "com.example.doorbell".into(),
        title_key: format!("doorbell.front_door.title.{cycle}"),
        body_key: Some("doorbell.front_door.body".into()),
        audio_payload: Some(AudioPayload {
            audio_uri: Some("evo://assets/chime/doorbell.wav".into()),
            tts_text_key: None,
            volume_relative: 1.0,
        }),
        display_widget: None,
        actions: vec![
            NotificationAction::InvokeVerb {
                plugin_id: "com.example.doorbell".into(),
                verb: "answer".into(),
                payload: Vec::new(),
            },
            NotificationAction::InvokeVerb {
                plugin_id: "com.example.doorbell".into(),
                verb: "snooze_5m".into(),
                payload: Vec::new(),
            },
            NotificationAction::Dismiss,
        ],
        priority: NotificationPriority::Routine,
        group_with: Some(
            NotificationGroupId::new("doorbell").expect("stable literal"),
        ),
        auto_dismiss_after: Some(Duration::from_secs(30)),
    }
}

fn sample_motion(cycle: u64) -> Notification {
    Notification {
        level: NotificationLevel::Warning,
        source_plugin: "com.example.security".into(),
        title_key: format!("security.motion.title.{cycle}"),
        body_key: Some("security.motion.body".into()),
        audio_payload: None,
        display_widget: None,
        actions: vec![
            NotificationAction::NavigateTo {
                screen_id: "security/cameras".into(),
                parameters_json: Some("{\"camera\":\"garage\"}".into()),
            },
            NotificationAction::Dismiss,
        ],
        priority: NotificationPriority::Important,
        group_with: None,
        auto_dismiss_after: Some(Duration::from_secs(60)),
    }
}

fn sample_update_available(cycle: u64) -> Notification {
    Notification {
        level: NotificationLevel::Info,
        source_plugin: "org.evoframework.system.updater".into(),
        title_key: format!("system.update.title.{cycle}"),
        body_key: Some("system.update.body".into()),
        audio_payload: None,
        display_widget: None,
        actions: vec![
            NotificationAction::InvokeVerb {
                plugin_id: "org.evoframework.system.updater".into(),
                verb: "install_now".into(),
                payload: Vec::new(),
            },
            NotificationAction::InvokeVerb {
                plugin_id: "org.evoframework.system.updater".into(),
                verb: "remind_later".into(),
                payload: Vec::new(),
            },
            NotificationAction::Dismiss,
        ],
        priority: NotificationPriority::Routine,
        group_with: None,
        auto_dismiss_after: None,
    }
}

fn sample_alarm(cycle: u64) -> Notification {
    Notification {
        level: NotificationLevel::Alert,
        source_plugin: "com.example.alarm".into(),
        title_key: format!("alarm.morning.title.{cycle}"),
        body_key: Some("alarm.morning.body".into()),
        audio_payload: Some(AudioPayload {
            audio_uri: None,
            tts_text_key: Some("alarm.morning.tts".into()),
            volume_relative: 1.0,
        }),
        display_widget: Some("evo.notifications.alarm.tile".into()),
        actions: vec![
            NotificationAction::InvokeVerb {
                plugin_id: "com.example.alarm".into(),
                verb: "stop_alarm".into(),
                payload: Vec::new(),
            },
            NotificationAction::InvokeVerb {
                plugin_id: "com.example.alarm".into(),
                verb: "snooze_9m".into(),
                payload: Vec::new(),
            },
        ],
        priority: NotificationPriority::Critical,
        group_with: None,
        auto_dismiss_after: None,
    }
}

fn sample_backup_complete(cycle: u64) -> Notification {
    Notification {
        level: NotificationLevel::Info,
        source_plugin: "org.evoframework.system.backup".into(),
        title_key: format!("backup.complete.title.{cycle}"),
        body_key: None,
        audio_payload: None,
        display_widget: None,
        actions: vec![],
        priority: NotificationPriority::Routine,
        group_with: None,
        auto_dismiss_after: Some(Duration::from_secs(8)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Env-gate assertions bundled into one sequential test:
    // cargo runs #[test] fns in parallel by default and
    // std::env::set_var / remove_var mutate process-global
    // state; three separate #[test]s would race.
    #[test]
    fn demo_env_gate_recognises_off_zero_and_active_states() {
        std::env::remove_var(ENV_DEMO_ENABLED);
        assert!(!demo_enabled(), "unset env var must resolve to disabled");

        std::env::set_var(ENV_DEMO_ENABLED, "0");
        assert!(!demo_enabled(), "explicit 0 must resolve to disabled");

        std::env::set_var(ENV_DEMO_ENABLED, "1");
        assert!(demo_enabled(), "1 must resolve to enabled");

        std::env::set_var(ENV_DEMO_ENABLED, "yes");
        assert!(
            demo_enabled(),
            "any non-empty non-zero value must resolve to enabled"
        );

        std::env::remove_var(ENV_DEMO_ENABLED);

        // spawn_if_enabled must exit cleanly (return None) when
        // the gate is off — no tokio runtime required. Checked
        // here rather than in a separate test to keep env-var
        // mutations sequential.
        let dispatcher = Arc::new(NotificationDispatcher::default());
        assert!(spawn_if_enabled(dispatcher).is_none());
    }

    #[test]
    fn sample_doorbell_carries_expected_shape() {
        let n = sample_doorbell(0);
        assert_eq!(n.priority, NotificationPriority::Routine);
        assert_eq!(n.level, NotificationLevel::Info);
        assert!(n.group_with.is_some());
        assert!(n.audio_payload.is_some());
        assert_eq!(n.actions.len(), 3);
    }

    #[test]
    fn sample_alarm_is_critical_pinned() {
        let n = sample_alarm(0);
        assert_eq!(n.priority, NotificationPriority::Critical);
        assert_eq!(n.level, NotificationLevel::Alert);
        assert!(n.auto_dismiss_after.is_none());
    }

    // spawn_returns_none_when_disabled folded into the
    // env-gate test above — same env-var race applies.
}
