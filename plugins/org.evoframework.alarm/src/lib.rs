// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! # org-evoframework-alarm
//!
//! One alarm on the `audio.alarm` shelf: the weekdays the
//! operator names, a local `HH:MM`, and a playlist name.
//!
//! - `alarm.set { v, weekdays, time, playlist_name }` schedules
//!   the appointment `alarm` from the new values without
//!   cancelling first: the ledger re-issues an id in place, so a
//!   create that succeeds is the one alarm and a create the
//!   ledger refuses leaves the previous appointment pending. It
//!   answers `{ v, alarm: { weekdays, time, playlist_name } }`:
//!   the weekdays in the order given, the time, and the name
//!   trimmed. An empty weekday list, a day named twice, an
//!   unknown day token, a time that is not `HH:MM`, and a name
//!   that is empty once trimmed are refused before any schedule,
//!   so the alarm in place stays. The set does not check that the
//!   playlist exists.
//! - `alarm.clear { v }` cancels the appointment `alarm` and
//!   answers `{ v, status: "ok" }`, also when no alarm is set.
//!
//! The alarm is one appointment on the framework appointment
//! ledger, id `alarm`: weekly on the named days, zone `Local`,
//! `must_wake_device` true, the ledger's default miss policy, and
//! no wake pre-arm. Its action is `queue.load_playlist_to_queue`
//! on `audio.queue` with `{ v: 1, playlist_name, start_position:
//! 0 }`, so the playlist replaces the queue and plays from its
//! first entry.
//!
//! ## What this plugin does NOT do
//!
//! - Keep state. The ledger keeps the appointment across
//!   restarts and the id is a constant, so set and clear reach it
//!   after a restart without a store of their own.
//! - Fire, time, or wake anything itself. Recurrence, local time,
//!   missed fires and the device wake are the ledger's.
//! - Fade, or touch any playback setting. It is not the sleep
//!   timer.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::manual_async_fn)]

use std::future::Future;
use std::sync::Arc;

use evo_plugin_sdk::contract::{
    AppointmentAction, AppointmentId, AppointmentMissPolicy,
    AppointmentRecurrence, AppointmentScheduler, AppointmentSpec,
    AppointmentTimeZone, BuildInfo, DayOfWeek, HealthReport, LoadContext,
    Plugin, PluginDescription, PluginError, PluginIdentity, ReportError,
    Request, Respondent, Response, RuntimeCapabilities,
    DEFAULT_APPOINTMENT_MISS_GRACE_MS,
};
use evo_plugin_sdk::Manifest;
use serde::{Deserialize, Serialize};

/// Embedded manifest source.
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");

/// Plugin name (reverse-DNS); same as manifest and tests.
pub const PLUGIN_NAME: &str = "org.evoframework.alarm";

/// Set the alarm.
pub const VERB_SET: &str = "alarm.set";

/// Take the alarm away.
pub const VERB_CLEAR: &str = "alarm.clear";

/// The one appointment's id on the ledger. A constant, so a set
/// or a clear after a restart reaches the appointment an earlier
/// run scheduled.
pub const APPOINTMENT_ID: &str = "alarm";

/// The shelf the alarm's fire dispatches to.
const QUEUE_SHELF: &str = "audio.queue";

/// The verb the alarm's fire dispatches.
const LOAD_PLAYLIST: &str = "queue.load_playlist_to_queue";

/// Wire payload version.
const PAYLOAD_VERSION: u32 = 1;

/// The day tokens, in the order the ledger's weeks run.
const DAYS: [(&str, DayOfWeek); 7] = [
    ("mon", DayOfWeek::Mon),
    ("tue", DayOfWeek::Tue),
    ("wed", DayOfWeek::Wed),
    ("thu", DayOfWeek::Thu),
    ("fri", DayOfWeek::Fri),
    ("sat", DayOfWeek::Sat),
    ("sun", DayOfWeek::Sun),
];

/// Parse the embedded [`Manifest`].
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("org-evoframework-alarm: embedded manifest must parse")
}

fn plugin_crate_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is valid semver")
}

/// The alarm as set and as answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Alarm {
    /// The day tokens, in the order given.
    pub weekdays: Vec<String>,
    /// Local `HH:MM`.
    pub time: String,
    /// The playlist name, trimmed.
    pub playlist_name: String,
}

impl Alarm {
    /// Check a set's fields. Refuses an empty weekday list, a day
    /// named twice, an unknown day token, a time that is not
    /// `HH:MM`, and a playlist name that is empty once trimmed.
    pub fn new(
        weekdays: Vec<String>,
        time: String,
        playlist_name: &str,
    ) -> Result<Self, String> {
        days_of(&weekdays)?;
        if !is_hh_mm(&time) {
            return Err(format!(
                "time must be HH:MM, the hour 00 through 23 and the \
                 minute 00 through 59; got {time:?}"
            ));
        }
        let playlist_name = playlist_name.trim();
        if playlist_name.is_empty() {
            return Err("playlist_name must not be empty".to_string());
        }
        Ok(Self {
            weekdays,
            time,
            playlist_name: playlist_name.to_string(),
        })
    }

    /// The appointment this alarm is on the ledger.
    pub fn appointment(&self) -> (AppointmentSpec, AppointmentAction) {
        let days = days_of(&self.weekdays)
            .expect("an Alarm's weekdays were checked when it was made");
        let spec = AppointmentSpec {
            appointment_id: APPOINTMENT_ID.to_string(),
            time: Some(self.time.clone()),
            zone: AppointmentTimeZone::Local,
            recurrence: AppointmentRecurrence::Weekly { days },
            end_time_ms: None,
            max_fires: None,
            except: Vec::new(),
            // The ledger's own default for a spec that names none.
            miss_policy: AppointmentMissPolicy::CatchupWithinGrace {
                grace_ms: DEFAULT_APPOINTMENT_MISS_GRACE_MS,
            },
            pre_fire_ms: None,
            must_wake_device: true,
            wake_pre_arm_ms: None,
        };
        let action = AppointmentAction {
            target_shelf: QUEUE_SHELF.to_string(),
            request_type: LOAD_PLAYLIST.to_string(),
            payload: serde_json::json!({
                "v": 1,
                "playlist_name": self.playlist_name,
                "start_position": 0,
            }),
        };
        (spec, action)
    }
}

/// The ledger's days for a list of tokens, in the order given.
fn days_of(tokens: &[String]) -> Result<Vec<DayOfWeek>, String> {
    if tokens.is_empty() {
        return Err("weekdays must name at least one day".to_string());
    }
    let mut days = Vec::with_capacity(tokens.len());
    for (i, token) in tokens.iter().enumerate() {
        let day = DAYS
            .iter()
            .find(|(name, _)| name == token)
            .map(|(_, day)| *day)
            .ok_or_else(|| {
                format!(
                    "unknown day {token:?}; the days are mon, tue, wed, \
                     thu, fri, sat, sun"
                )
            })?;
        if tokens[..i].contains(token) {
            return Err(format!("{token:?} is named twice"));
        }
        days.push(day);
    }
    Ok(days)
}

/// `HH:MM`, two digits each, the hour 00 through 23 and the
/// minute 00 through 59.
fn is_hh_mm(time: &str) -> bool {
    let b = time.as_bytes();
    if b.len() != 5 || b[2] != b':' {
        return false;
    }
    let digits = [b[0], b[1], b[3], b[4]];
    if !digits.iter().all(u8::is_ascii_digit) {
        return false;
    }
    let hour = (b[0] - b'0') * 10 + (b[1] - b'0');
    let minute = (b[3] - b'0') * 10 + (b[4] - b'0');
    hour <= 23 && minute <= 59
}

/// The plugin singleton.
pub struct AlarmPlugin {
    loaded: bool,
    /// Ledger handle from `LoadContext`. `None` refuses both verbs.
    appointments: Option<Arc<dyn AppointmentScheduler>>,
    /// Serialises set and clear, so the last gesture to reach the
    /// ledger is the one that stands.
    gesture: tokio::sync::Mutex<()>,
}

impl AlarmPlugin {
    /// New instance; call [`Plugin::load`] before handling requests.
    pub fn new() -> Self {
        Self {
            loaded: false,
            appointments: None,
            gesture: tokio::sync::Mutex::new(()),
        }
    }

    fn ledger(&self) -> Result<&Arc<dyn AppointmentScheduler>, PluginError> {
        self.appointments.as_ref().ok_or_else(|| {
            PluginError::Permanent(
                "the alarm schedules on the appointment ledger, and no \
                 ledger handle was provided at load"
                    .to_string(),
            )
        })
    }

    async fn handle_set(&self, req: &Request) -> Result<Response, PluginError> {
        let payload: SetPayload = parse_versioned(req)?;
        let alarm =
            Alarm::new(payload.weekdays, payload.time, &payload.playlist_name)
                .map_err(|e| {
                    PluginError::Permanent(format!("{VERB_SET}: {e}"))
                })?;
        let ledger = self.ledger()?;
        let (spec, action) = alarm.appointment();
        let _gesture = self.gesture.lock().await;
        // No cancel first: the ledger re-issues `alarm` in place,
        // so a refused create leaves the previous appointment
        // pending.
        ledger
            .create_appointment(spec, action)
            .await
            .map_err(|e| ledger_refused(VERB_SET, e))?;
        tracing::info!(
            plugin = PLUGIN_NAME,
            weekdays = ?alarm.weekdays,
            time = %alarm.time,
            playlist = %alarm.playlist_name,
            "alarm set"
        );
        encode(
            req,
            &AlarmResponse {
                v: PAYLOAD_VERSION,
                alarm,
            },
        )
    }

    async fn handle_clear(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let _: EmptyPayload = parse_versioned(req)?;
        let ledger = self.ledger()?;
        let _gesture = self.gesture.lock().await;
        ledger
            .cancel_appointment(AppointmentId::new(APPOINTMENT_ID))
            .await
            .map_err(|e| ledger_refused(VERB_CLEAR, e))?;
        tracing::info!(plugin = PLUGIN_NAME, "alarm cleared");
        encode(
            req,
            &StatusResponse {
                v: PAYLOAD_VERSION,
                status: "ok",
            },
        )
    }
}

impl Default for AlarmPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for AlarmPlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: PLUGIN_NAME.to_string(),
                    version: plugin_crate_version(),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: vec![
                        VERB_SET.to_string(),
                        VERB_CLEAR.to_string(),
                    ],
                    accepts_custody: false,
                    flags: Default::default(),
                    course_correct_verbs: Vec::new(),
                },
                build_info: BuildInfo {
                    plugin_build: env!("CARGO_PKG_VERSION").to_string(),
                    sdk_version: evo_plugin_sdk::VERSION.to_string(),
                    rustc_version: None,
                    built_at: None,
                },
            }
        }
    }

    fn load<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
        async move {
            self.appointments = ctx.appointments.as_ref().map(Arc::clone);
            if self.appointments.is_none() {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    "no appointment ledger handle; alarm.set and \
                     alarm.clear will refuse"
                );
            }
            self.loaded = true;
            tracing::info!(plugin = PLUGIN_NAME, "alarm plugin loaded");
            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            // The alarm is the ledger's appointment, not this
            // process's: it stays scheduled across an unload.
            self.loaded = false;
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            if self.loaded {
                HealthReport::healthy()
            } else {
                HealthReport::unhealthy("alarm plugin not loaded")
            }
        }
    }
}

impl Respondent for AlarmPlugin {
    fn handle_request<'a>(
        &'a self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "alarm plugin not loaded".to_string(),
                ));
            }
            if req.is_past_deadline() {
                return Err(PluginError::Transient(
                    "request deadline already expired".to_string(),
                ));
            }
            match req.request_type.as_str() {
                VERB_SET => self.handle_set(req).await,
                VERB_CLEAR => self.handle_clear(req).await,
                other => Err(PluginError::Permanent(format!(
                    "alarm: unknown verb {other:?}"
                ))),
            }
        }
    }
}

/// A ledger refusal as the verb's error. An invalid spec is
/// permanent; anything else (shutting down, rate limited, …) may
/// pass on a retry.
fn ledger_refused(verb: &str, e: ReportError) -> PluginError {
    match e {
        ReportError::Invalid(detail) => PluginError::Permanent(format!(
            "{verb}: the appointment ledger refused: {detail}"
        )),
        other => PluginError::Transient(format!(
            "{verb}: the appointment ledger refused: {other}"
        )),
    }
}

trait HasPayloadVersion {
    fn payload_version(&self) -> u32;
}

fn parse_versioned<T>(req: &Request) -> Result<T, PluginError>
where
    T: serde::de::DeserializeOwned + HasPayloadVersion,
{
    let parsed: T = serde_json::from_slice(&req.payload).map_err(|e| {
        PluginError::Permanent(format!(
            "{:?} payload is not valid JSON for the expected shape: {e}",
            req.request_type
        ))
    })?;
    if parsed.payload_version() != PAYLOAD_VERSION {
        return Err(PluginError::Permanent(format!(
            "{:?} payload version {} unsupported; expected {}",
            req.request_type,
            parsed.payload_version(),
            PAYLOAD_VERSION
        )));
    }
    Ok(parsed)
}

fn default_payload_version() -> u32 {
    PAYLOAD_VERSION
}

fn encode<T: Serialize>(
    req: &Request,
    payload: &T,
) -> Result<Response, PluginError> {
    let body = serde_json::to_vec(payload).map_err(|e| {
        PluginError::Permanent(format!(
            "{:?} response encode failed: {e}",
            req.request_type
        ))
    })?;
    Ok(Response::for_request(req, body))
}

#[derive(Debug, Deserialize)]
struct SetPayload {
    #[serde(default = "default_payload_version")]
    v: u32,
    weekdays: Vec<String>,
    time: String,
    playlist_name: String,
}

impl HasPayloadVersion for SetPayload {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

#[derive(Debug, Deserialize)]
struct EmptyPayload {
    #[serde(default = "default_payload_version")]
    v: u32,
}

impl HasPayloadVersion for EmptyPayload {
    fn payload_version(&self) -> u32 {
        self.v
    }
}

#[derive(Debug, Serialize)]
struct AlarmResponse {
    v: u32,
    alarm: Alarm,
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    v: u32,
    status: &'static str,
}

#[cfg(test)]
mod tests;
