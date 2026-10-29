// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! `timezone.set`: apply a zone on the host, let the steward's
//! local time move to it, then have the alarm follow.
//!
//! The order is fixed:
//!
//! 1. Read the zone in force, `timedatectl show
//!    --property=Timezone`.
//! 2. Refuse to apply a name that is not an IANA zone the host's
//!    zone database holds, or `Factory`; the answer is the zone
//!    still in force.
//! 3. Apply through `sudo -n /usr/local/bin/evo-set-timezone`,
//!    then read the zone in force again.
//! 4. Wait [`SETTLE`]: the steward re-reads `/etc/localtime` once
//!    its cached zone is a second old.
//! 5. Call `alarm.reschedule` on `audio.alarm` through the shelf
//!    dispatcher, and only then answer.
//!
//! A failed apply and a refused reschedule are errors that name the
//! zone in force. A refused reschedule leaves the new zone applied;
//! sending the same zone again runs the reschedule again.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use evo_plugin_sdk::contract::{PluginError, ShelfRequestDispatcher};

/// Set the device's timezone.
pub const VERB_TIMEZONE_SET: &str = "timezone.set";

/// The shelf whose pending alarm follows a zone change.
pub const ALARM_SHELF: &str = "audio.alarm";

/// The verb that schedules the stored alarm again.
pub const VERB_ALARM_RESCHEDULE: &str = "alarm.reschedule";

/// The wait between an apply and the reschedule. More than the one
/// second the steward's local-time cache is reused without a check.
pub const SETTLE: Duration = Duration::from_millis(1500);

/// The zone database's name for "no zone set". Never applied.
pub const FACTORY: &str = "Factory";

const SUDO: &str = "/usr/bin/sudo";
const WRAPPER: &str = "/usr/local/bin/evo-set-timezone";
const TIMEDATECTL: &str = "/usr/bin/timedatectl";
const ZONEINFO: &str = "/usr/share/zoneinfo";

/// A boxed host call.
pub type HostFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

/// The host side of a zone change.
pub trait ZoneHost: Send + Sync {
    /// The zone in force, as the host names it.
    fn zone_in_force(&self) -> HostFuture<'_, String>;
    /// Whether the host's zone database holds `zone`.
    fn holds<'a>(&'a self, zone: &'a str) -> HostFuture<'a, bool>;
    /// Make `zone` the zone in force.
    fn apply<'a>(&'a self, zone: &'a str) -> HostFuture<'a, ()>;
}

/// The host this plugin runs on: timedatectl to read, the
/// distribution's wrapper to apply.
pub struct Timedatectl;

impl ZoneHost for Timedatectl {
    fn zone_in_force(&self) -> HostFuture<'_, String> {
        Box::pin(async {
            let out =
                run(TIMEDATECTL, &["show", "--property=Timezone", "--value"])
                    .await?;
            let zone = out.trim();
            if zone.is_empty() {
                return Err(format!("{TIMEDATECTL} show names no zone"));
            }
            Ok(zone.to_string())
        })
    }

    fn holds<'a>(&'a self, zone: &'a str) -> HostFuture<'a, bool> {
        Box::pin(async move {
            let listed = run(TIMEDATECTL, &["list-timezones"]).await?;
            if !listed.lines().any(|l| l == zone) {
                return Ok(false);
            }
            // The name passed is_zone_name, so it stays under the
            // zone database.
            let path = format!("{ZONEINFO}/{zone}");
            match tokio::fs::read(&path).await {
                Ok(bytes) => Ok(bytes.starts_with(b"TZif")),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(format!("could not read {path}: {e}")),
            }
        })
    }

    fn apply<'a>(&'a self, zone: &'a str) -> HostFuture<'a, ()> {
        Box::pin(
            async move { run(SUDO, &["-n", WRAPPER, zone]).await.map(drop) },
        )
    }
}

/// Run a host command; its stdout, or why it failed.
async fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let out = tokio::process::Command::new(program)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("could not run {program}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{program} {} failed ({}): {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The shape of an IANA zone name: components of letters, digits,
/// `_`, `+` and `-`, joined by `/`. No dot, so no `..`, and no
/// leading `/`.
pub fn is_zone_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('/').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_+-".contains(&b))
        })
}

/// Set the zone and have the alarm follow. The zone to answer, or
/// the error.
pub async fn set_zone(
    host: &dyn ZoneHost,
    dispatcher: Option<&dyn ShelfRequestDispatcher>,
    asked: &str,
) -> Result<String, PluginError> {
    let before = host.zone_in_force().await.map_err(|e| {
        PluginError::Transient(format!(
            "{VERB_TIMEZONE_SET}: the zone in force could not be read: {e}"
        ))
    })?;
    let held = if is_zone_name(asked) && asked != FACTORY {
        host.holds(asked).await.map_err(|e| {
            PluginError::Transient(format!(
                "{VERB_TIMEZONE_SET}: the host's zone database could not be \
                 read: {e}; nothing was applied and the zone in force is \
                 {before}"
            ))
        })?
    } else {
        false
    };
    if !held {
        tracing::info!(
            asked,
            in_force = %before,
            "zone not applied: the host's zone database does not hold it"
        );
        return Ok(before);
    }
    let Some(dispatcher) = dispatcher else {
        return Err(PluginError::Permanent(format!(
            "{VERB_TIMEZONE_SET}: the alarm follows a zone change through \
             the shelf dispatcher, and none was provided at load; nothing \
             was applied and the zone in force is {before}"
        )));
    };

    let applied = host.apply(asked).await;
    let after = host.zone_in_force().await;
    // The alarm follows every zone that may have moved: after an
    // apply that succeeded, and after a failed one that moved it
    // anyway.
    let moved = applied.is_ok() || matches!(&after, Ok(z) if *z != before);
    let rescheduled = if moved {
        tokio::time::sleep(SETTLE).await;
        Some(reschedule(dispatcher).await)
    } else {
        None
    };
    let in_force = match &after {
        Ok(zone) => zone.clone(),
        Err(e) => format!("unknown ({e})"),
    };

    if let Err(e) = applied {
        let alarm = match rescheduled {
            Some(Err(r)) => {
                format!("; {VERB_ALARM_RESCHEDULE} was refused: {r}")
            }
            _ => String::new(),
        };
        return Err(PluginError::Transient(format!(
            "{VERB_TIMEZONE_SET}: the apply failed: {e}; the zone in force \
             is {in_force}{alarm}"
        )));
    }
    if let Some(Err(r)) = rescheduled {
        return Err(PluginError::Transient(format!(
            "{VERB_TIMEZONE_SET}: the zone in force is {in_force}, and the \
             alarm did not follow: {VERB_ALARM_RESCHEDULE} was refused: {r}; \
             sending the same zone again runs the reschedule again"
        )));
    }
    match after {
        Ok(zone) if zone == asked => {
            tracing::info!(timezone = %zone, "timezone set; the alarm followed");
            Ok(zone)
        }
        Ok(zone) => Err(PluginError::Transient(format!(
            "{VERB_TIMEZONE_SET}: the apply reported success, but the zone in \
             force is {zone}, not {asked}"
        ))),
        Err(e) => Err(PluginError::Transient(format!(
            "{VERB_TIMEZONE_SET}: the zone was applied, but the zone in force \
             could not be read: {e}"
        ))),
    }
}

/// Have the stored alarm scheduled again on the zone now in force.
async fn reschedule(d: &dyn ShelfRequestDispatcher) -> Result<(), String> {
    let body = serde_json::to_vec(&serde_json::json!({ "v": 1 }))
        .map_err(|e| e.to_string())?;
    d.dispatch(ALARM_SHELF, VERB_ALARM_RESCHEDULE, body, None)
        .await
        .map(drop)
        .map_err(|e| e.to_string())
}
