// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! # org-evoframework-system-kiosk
//!
//! Framework-reserved kiosk operator-settings plugin. Stocks the
//! `system.kiosk` shelf with three operator-gestured verbs:
//!
//! - `set_display_rotation` — persists the compositor display
//!   rotation overlay so the kiosk's in-session watcher picks
//!   it up and re-runs `wlr-randr --transform`.
//! - `set_touch_calibration` — persists the three touch overlay
//!   files atomically so the systemd path unit picks up the
//!   burst and regenerates the LIBINPUT_CALIBRATION_MATRIX udev
//!   rule + triggers a re-detect.
//! - `launch_touch_calibration` — touches a trigger file the
//!   kiosk-browser polls; on change the browser dispatches a
//!   `evo:touch-calibration-launch` CustomEvent on window so
//!   the local UI's Display & Touch panel opens the four-corner
//!   wizard on-glass. Remote-driven wizard launch, on-glass
//!   completion — the operator walks to the device to tap the
//!   corners.
//!
//! All three verbs are gated at the framework dispatcher's
//! per-verb capability gate as `write:system_admin` (no
//! step-up). Rationale: rotation is a cosmetic-visible change,
//! not a credential mint. A step-up gate would require the
//! operator to enter the kiosk password on the very glass they
//! are trying to fix — recursive breakage. The bootstrap-
//! alignment case (operator not yet paired) is served by the
//! headless preseed-pair path documented in the kiosk-eng repo.
//!
//! ## Why this plugin exists
//!
//! The kiosk-browser (`evo-kiosk-browser`) exposes the same
//! writes via WebKit script-message handlers on its
//! UserContentManager. Those handlers are reachable only from
//! JavaScript running inside the kiosk-browser process — a
//! paired laptop or phone browser loading the same UI over WSS
//! cannot reach them (per-webview surface). The initial-
//! alignment recovery case is exactly the scenario where the
//! on-glass touch is unusable, so the on-glass UI cannot drive
//! the fix. This plugin exposes the same writes over WSS so a
//! remote paired browser can drive them.
//!
//! ## Byte parity with the on-glass path
//!
//! Both paths call into [`evo_kiosk_config`] for the actual
//! filesystem work. A drift between them (different overlay
//! bytes for the same operator intent) would surface as a
//! difference in what the kiosk-side apply machinery does. One
//! source of truth eliminates that class.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::manual_async_fn)]

use std::future::Future;
use std::path::PathBuf;
use std::time::SystemTime;

use evo_kiosk_config::{TouchSample, OVERLAY_DIR};
use evo_plugin_sdk::contract::{
    BuildInfo, ExternalAddressing, HealthReport, LoadContext, Plugin,
    PluginDescription, PluginError, PluginIdentity, Request, Respondent,
    Response, RuntimeCapabilities,
};
use evo_plugin_sdk::Manifest;
use serde::Deserialize;

/// Embedded manifest source.
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");

/// Plugin name (reverse-DNS); same as manifest and tests.
pub const PLUGIN_NAME: &str = "org.evoframework.system.kiosk";

/// Verb name — set the compositor display rotation.
pub const VERB_SET_DISPLAY_ROTATION: &str = "set_display_rotation";

/// Verb name — set the touch calibration triple.
pub const VERB_SET_TOUCH_CALIBRATION: &str = "set_touch_calibration";

/// Verb name — signal the on-glass browser to open the wizard.
pub const VERB_LAUNCH_TOUCH_CALIBRATION: &str = "launch_touch_calibration";

/// Verb name — enable or disable the evo-kiosk.service unit.
pub const VERB_SET_ENABLED: &str = "set_enabled";

/// Verb name — set the compositor display brightness percent.
pub const VERB_SET_BRIGHTNESS: &str = "set_brightness";

/// Verb name — set the idle sleep timeout in seconds.
pub const VERB_SET_SLEEP_TIMEOUT: &str = "set_sleep_timeout";

/// Verb name — toggle "keep the screen awake while playing."
pub const VERB_SET_SLEEP_INHIBIT_WHILE_PLAYING: &str =
    "set_sleep_inhibit_while_playing";

/// Verb name — read the complete persisted operator-visible state
/// (display rotation, touch triple, brightness, sleep, inhibit-
/// while-playing, kiosk enabled). Companion to the `set_*` surface
/// so the paired-browser Display & Touch panel opens showing the
/// device's real state on mount instead of a stale browser cache.
pub const VERB_GET_DISPLAY_STATE: &str = "get_display_state";

/// Trigger file the plugin touches when the operator asks for
/// the wizard from a remote browser. Kiosk-browser polls this
/// file's mtime and dispatches an in-page CustomEvent on
/// change. Path is per-file rather than a signal channel
/// because the browser's polling loop is trivial and no
/// framework happening plumbing is required at this layer.
pub const CALIBRATE_TRIGGER_FILE: &str = "calibrate_trigger";

/// Parse the embedded [`Manifest`].
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("org-evoframework-system-kiosk: embedded manifest must parse")
}

fn plugin_crate_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is valid semver")
}

/// External-addressing scheme for the audio-playback now_playing
/// subject the MPD warden publishes. Matches the playback plugin's
/// `SCHEME_STREAM_FORMAT` literal; the two plugins agree on the
/// wire-side addressing constants without cross-plugin coupling.
///
/// The subject registry keys off a UUID canonical id, NOT this
/// string. We use [`ExternalAddressing`] via [`SubjectQuerier::
/// resolve_addressing`] to obtain the UUID, then subscribe /
/// current_state on the UUID.
const NOW_PLAYING_SCHEME: &str = "evo.audio.playback";

/// External-addressing value for the audio-playback now_playing
/// subject.
const NOW_PLAYING_VALUE: &str = "now_playing";

/// Backoff for the resolve-addressing retry loop. The playback
/// plugin's `announce_now_playing` may not have landed by the
/// time this subscriber spawns; the loop polls at this cadence
/// until resolution succeeds.
const RESOLVE_RETRY_INTERVAL_MS: u64 = 500;

/// The plugin singleton. Holds a load flag + a JoinHandle for
/// the background MPD-state subscriber (so `unload` can cancel
/// it cleanly).
pub struct SystemKioskPlugin {
    loaded: bool,
    inhibit_task: Option<tokio::task::JoinHandle<()>>,
}

impl SystemKioskPlugin {
    /// New instance; call [`Plugin::load`] before handling requests.
    pub fn new() -> Self {
        Self {
            loaded: false,
            inhibit_task: None,
        }
    }
}

impl Default for SystemKioskPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for SystemKioskPlugin {
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
                        VERB_SET_DISPLAY_ROTATION.to_string(),
                        VERB_SET_TOUCH_CALIBRATION.to_string(),
                        VERB_LAUNCH_TOUCH_CALIBRATION.to_string(),
                        VERB_SET_ENABLED.to_string(),
                        VERB_SET_BRIGHTNESS.to_string(),
                        VERB_SET_SLEEP_TIMEOUT.to_string(),
                        VERB_SET_SLEEP_INHIBIT_WHILE_PLAYING.to_string(),
                        VERB_GET_DISPLAY_STATE.to_string(),
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
            tracing::info!(plugin = PLUGIN_NAME, "system.kiosk plugin load");

            // Spawn the MPD-state subscriber if the framework
            // wired it. The overlay this task writes
            // (`sleep_inhibit_active`) is honoured only when
            // the operator's `sleep_inhibit_while_playing`
            // toggle is true, so this task runs
            // unconditionally — cheap when the toggle is off,
            // immediate when the operator flips it on.
            if let (Some(sub), Some(querier)) = (
                ctx.subject_state_subscriber.clone(),
                ctx.subject_querier.clone(),
            ) {
                let handle = tokio::spawn(async move {
                    // The subject registry keys off a UUID canonical
                    // id. The playback plugin publishes on
                    // (scheme="evo.audio.playback", value="now_playing"),
                    // which resolves via SubjectQuerier to that UUID.
                    // Passing the raw string `"evo.audio.playback:
                    // now_playing"` as canonical_id was silently
                    // accepted by `subscribe_subject` in earlier cuts
                    // but delivered ZERO updates — that path is a
                    // no-op. Model matches audio.terminus's
                    // `now_playing_subscriber`.
                    let addressing = ExternalAddressing::new(
                        NOW_PLAYING_SCHEME,
                        NOW_PLAYING_VALUE,
                    );

                    // 1. Resolve canonical id with bounded backoff.
                    //    The playback plugin's announce may not have
                    //    landed yet when this plugin loads.
                    let canonical_id = loop {
                        match querier
                            .resolve_addressing(addressing.clone())
                            .await
                        {
                            Ok(Some(id)) => break id,
                            Ok(None) => {
                                tokio::time::sleep(
                                    tokio::time::Duration::from_millis(
                                        RESOLVE_RETRY_INTERVAL_MS,
                                    ),
                                )
                                .await;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    plugin = PLUGIN_NAME,
                                    error = %e,
                                    "resolve_addressing for now_playing errored; \
                                     retrying"
                                );
                                tokio::time::sleep(
                                    tokio::time::Duration::from_millis(
                                        RESOLVE_RETRY_INTERVAL_MS,
                                    ),
                                )
                                .await;
                            }
                        }
                    };

                    tracing::info!(
                        plugin = PLUGIN_NAME,
                        canonical_id = %canonical_id,
                        "now_playing canonical id resolved"
                    );

                    // 2. Subscribe FIRST (no race window vs step 3).
                    let mut stream = match sub
                        .subscribe_subject(canonical_id.clone())
                        .await
                    {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!(
                                plugin = PLUGIN_NAME,
                                canonical_id = %canonical_id,
                                error = %e,
                                "MPD subject subscribe failed; sleep-inhibit-\
                                 while-playing will not react to playback \
                                 state until next plugin reload"
                            );
                            return;
                        }
                    };
                    tracing::info!(
                        plugin = PLUGIN_NAME,
                        canonical_id = %canonical_id,
                        "MPD state subscriber running"
                    );

                    // 3. Seed from current_state. If MPD was already
                    //    playing at plugin load, the broadcast will
                    //    never fire a "playing" event until the next
                    //    pause/play cycle; the seed covers that.
                    //    The warden publishes `transport_state` as
                    //    one of "playing" | "paused" | "stopped"
                    //    (playback_supervisor/subject_emitter::
                    //    render_now_playing_state).
                    let seed_is_playing =
                        match sub.current_state(canonical_id.clone()).await {
                            Ok(Some(state)) => state
                                .get("transport_state")
                                .and_then(|v| v.as_str())
                                .map(|s| s == "playing")
                                .unwrap_or(false),
                            Ok(None) => false,
                            Err(e) => {
                                tracing::warn!(
                                    plugin = PLUGIN_NAME,
                                    canonical_id = %canonical_id,
                                    error = %e,
                                    "current_state read for MPD seed failed; \
                                     defaulting sleep_inhibit_active=false"
                                );
                                false
                            }
                        };
                    match evo_kiosk_config::set_sleep_inhibit_active(
                        seed_is_playing,
                    ) {
                        Ok(_) => tracing::info!(
                            plugin = PLUGIN_NAME,
                            is_playing = seed_is_playing,
                            "sleep_inhibit_active seeded from current MPD state"
                        ),
                        Err(e) => tracing::warn!(
                            plugin = PLUGIN_NAME,
                            error = %e,
                            "failed to seed sleep_inhibit_active"
                        ),
                    }

                    loop {
                        match stream.recv().await {
                            Ok(update) => {
                                let is_playing = update
                                    .state
                                    .as_ref()
                                    .and_then(|v| v.get("transport_state"))
                                    .and_then(|v| v.as_str())
                                    .map(|s| s == "playing")
                                    .unwrap_or(false);
                                match evo_kiosk_config::set_sleep_inhibit_active(
                                    is_playing,
                                ) {
                                    Ok(_) => tracing::info!(
                                        plugin = PLUGIN_NAME,
                                        is_playing,
                                        "sleep_inhibit_active updated from \
                                         MPD state change"
                                    ),
                                    Err(e) => tracing::warn!(
                                        plugin = PLUGIN_NAME,
                                        error = %e,
                                        "failed to write sleep_inhibit_active overlay"
                                    ),
                                }
                            }
                            Err(_) => {
                                // Stream closed or fatal recv error — the
                                // framework's registry is going down or the
                                // subject was removed; exit gracefully.
                                tracing::info!(
                                    plugin = PLUGIN_NAME,
                                    "MPD state stream closed; subscriber exiting"
                                );
                                break;
                            }
                        }
                    }
                });
                self.inhibit_task = Some(handle);
            } else {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    "subject_state_subscriber not wired by framework — \
                     sleep-inhibit-while-playing will not react to playback \
                     state (manifest declares capabilities.subscribe_subjects=true; \
                     verify framework binding)"
                );
            }

            self.loaded = true;
            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            self.loaded = false;
            if let Some(handle) = self.inhibit_task.take() {
                handle.abort();
                // Not awaiting the abort — the task's only
                // side effect is the overlay write which is
                // idempotent.
            }
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            if self.loaded {
                HealthReport::healthy()
            } else {
                HealthReport::unhealthy("system.kiosk plugin not loaded")
            }
        }
    }
}

impl Respondent for SystemKioskPlugin {
    fn handle_request<'a>(
        &'a self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "system.kiosk plugin not loaded".to_string(),
                ));
            }
            if req.is_past_deadline() {
                return Err(PluginError::Transient(
                    "request deadline already expired".to_string(),
                ));
            }
            tracing::info!(
                plugin = PLUGIN_NAME,
                verb = req.request_type.as_str(),
                cid = req.correlation_id,
                scope = req.principal_scope.as_deref().unwrap_or("<none>"),
                "system.kiosk: dispatcher-authorised verb"
            );
            match req.request_type.as_str() {
                VERB_SET_DISPLAY_ROTATION => handle_set_display_rotation(req),
                VERB_SET_TOUCH_CALIBRATION => handle_set_touch_calibration(req),
                VERB_LAUNCH_TOUCH_CALIBRATION => {
                    handle_launch_touch_calibration(req)
                }
                VERB_SET_ENABLED => handle_set_enabled(req).await,
                VERB_SET_BRIGHTNESS => handle_set_brightness(req),
                VERB_SET_SLEEP_TIMEOUT => handle_set_sleep_timeout(req),
                VERB_SET_SLEEP_INHIBIT_WHILE_PLAYING => {
                    handle_set_sleep_inhibit_while_playing(req)
                }
                VERB_GET_DISPLAY_STATE => handle_get_display_state(req),
                other => Err(PluginError::Permanent(format!(
                    "system.kiosk: unknown verb {other:?}"
                ))),
            }
        }
    }
}

#[derive(Deserialize)]
struct DisplayRotationReq {
    rotation: String,
}

#[derive(Deserialize)]
struct TouchCalibrationReq {
    rotation: String,
    #[serde(default)]
    hflip: bool,
    #[serde(default)]
    vflip: bool,
}

#[derive(Deserialize, Default)]
struct LaunchTouchCalibrationReq {
    /// Optional sample set — if present, framework skips the
    /// on-glass wizard and applies the derived matrix directly.
    /// Reserved for a future path where a remote-tap flow can
    /// pipe samples through without on-glass involvement; for
    /// this cut the field is accepted but ignored (samples
    /// captured on-glass only).
    #[serde(default)]
    #[allow(dead_code)]
    samples: Option<Vec<TouchSampleWire>>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct TouchSampleWire {
    target_x: f64,
    target_y: f64,
    actual_x: f64,
    actual_y: f64,
}

fn parse_payload<T: for<'de> Deserialize<'de>>(
    req: &Request,
    verb: &'static str,
) -> Result<T, PluginError> {
    if req.payload.is_empty() {
        // Verbs with all-defaulted fields (currently only
        // launch_touch_calibration) can accept an empty payload.
        return serde_json::from_slice(b"{}").map_err(|e| {
            PluginError::Permanent(format!(
                "{verb}: default payload deserialise failed: {e}"
            ))
        });
    }
    serde_json::from_slice(&req.payload).map_err(|e| {
        PluginError::Permanent(format!("{verb}: payload JSON invalid: {e}"))
    })
}

fn kiosk_config_error(
    verb: &'static str,
    err: evo_kiosk_config::KioskConfigError,
) -> PluginError {
    use evo_kiosk_config::KioskConfigError;
    match &err {
        KioskConfigError::InvalidRotation(_)
        | KioskConfigError::SampleCountMismatch(_)
        | KioskConfigError::SampleOutOfRange(_) => {
            PluginError::Permanent(format!("{verb}: {err}"))
        }
        KioskConfigError::Io(_) => {
            PluginError::Transient(format!("{verb}: {err}"))
        }
    }
}

fn handle_set_display_rotation(req: &Request) -> Result<Response, PluginError> {
    let parsed: DisplayRotationReq =
        parse_payload(req, VERB_SET_DISPLAY_ROTATION)?;
    let applied = evo_kiosk_config::set_display_rotation(&parsed.rotation)
        .map_err(|e| kiosk_config_error(VERB_SET_DISPLAY_ROTATION, e))?;
    let body = serde_json::json!({
        "ok": true,
        "display_rotation": applied,
    });
    Ok(Response::for_request(
        req,
        serde_json::to_vec(&body)
            .expect("system.kiosk response JSON always serialises"),
    ))
}

fn handle_set_touch_calibration(
    req: &Request,
) -> Result<Response, PluginError> {
    let parsed: TouchCalibrationReq =
        parse_payload(req, VERB_SET_TOUCH_CALIBRATION)?;
    let (rot, hf, vf) = evo_kiosk_config::set_touch_calibration(
        &parsed.rotation,
        parsed.hflip,
        parsed.vflip,
    )
    .map_err(|e| kiosk_config_error(VERB_SET_TOUCH_CALIBRATION, e))?;
    let body = serde_json::json!({
        "ok": true,
        "touch_rotation": rot,
        "touch_hflip": hf,
        "touch_vflip": vf,
    });
    Ok(Response::for_request(
        req,
        serde_json::to_vec(&body)
            .expect("system.kiosk response JSON always serialises"),
    ))
}

fn handle_launch_touch_calibration(
    req: &Request,
) -> Result<Response, PluginError> {
    let _parsed: LaunchTouchCalibrationReq =
        parse_payload(req, VERB_LAUNCH_TOUCH_CALIBRATION)?;
    let path = PathBuf::from(OVERLAY_DIR).join(CALIBRATE_TRIGGER_FILE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            PluginError::Transient(format!(
                "launch_touch_calibration: create overlay dir failed: {e}"
            ))
        })?;
    }
    // Content is the current wall-clock as ms since Unix epoch;
    // gives a monotonic-ish token the browser side can log for
    // correlation. Overwrite semantics: the browser reacts to
    // mtime change, so the content is diagnostic only.
    let now_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    std::fs::write(&path, format!("{now_ms}\n")).map_err(|e| {
        PluginError::Transient(format!(
            "launch_touch_calibration: write trigger file failed: {e}"
        ))
    })?;
    let body = serde_json::json!({
        "ok": true,
        "launched_at_ms": now_ms,
    });
    Ok(Response::for_request(
        req,
        serde_json::to_vec(&body)
            .expect("system.kiosk response JSON always serialises"),
    ))
}

/// Trigger file absolute path (helper for tests + on-target
/// diagnostics).
pub fn calibrate_trigger_path() -> PathBuf {
    PathBuf::from(OVERLAY_DIR).join(CALIBRATE_TRIGGER_FILE)
}

// ------------------------------ set_enabled ---------------------------

#[derive(Deserialize)]
struct SetEnabledReq {
    enabled: bool,
}

async fn handle_set_enabled(req: &Request) -> Result<Response, PluginError> {
    let parsed: SetEnabledReq = parse_payload(req, VERB_SET_ENABLED)?;
    // Persist the operator-visible flag first so a subsequent
    // UI read reflects the intended state even if the systemctl
    // call is slow. The subsequent systemctl call is the
    // authority — if it fails we roll back the overlay.
    evo_kiosk_config::set_kiosk_enabled(parsed.enabled)
        .map_err(|e| kiosk_config_error(VERB_SET_ENABLED, e))?;

    // Sudo grant is enumerated by the paired
    // /etc/sudoers.d/evo-system-kiosk drop-in: one Cmnd_Alias
    // per (enable | disable) with `--now` baked in. Argv must
    // match the alias exactly; no shell interpolation.
    let sudo_cmd = if parsed.enabled { "enable" } else { "disable" };
    let output = tokio::process::Command::new("/usr/bin/sudo")
        .arg("-n")
        .arg("/usr/bin/systemctl")
        .arg(sudo_cmd)
        .arg("--now")
        .arg("evo-kiosk.service")
        .output()
        .await
        .map_err(|e| {
            PluginError::Transient(format!(
                "set_enabled: spawning sudo systemctl failed: {e}"
            ))
        })?;
    if !output.status.success() {
        // Roll back overlay so UI reflects the actual on-disk
        // reality (nothing changed).
        let _ = evo_kiosk_config::set_kiosk_enabled(!parsed.enabled);
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(PluginError::Transient(format!(
            "set_enabled: systemctl {sudo_cmd} --now evo-kiosk.service exited {:?}: {stderr}",
            output.status.code()
        )));
    }
    let body = serde_json::json!({
        "ok": true,
        "enabled": parsed.enabled,
    });
    Ok(Response::for_request(
        req,
        serde_json::to_vec(&body)
            .expect("system.kiosk response JSON always serialises"),
    ))
}

// ------------------------------ set_brightness ------------------------

#[derive(Deserialize)]
struct SetBrightnessReq {
    percent: u8,
}

fn handle_set_brightness(req: &Request) -> Result<Response, PluginError> {
    let parsed: SetBrightnessReq = parse_payload(req, VERB_SET_BRIGHTNESS)?;
    let applied = evo_kiosk_config::set_brightness(parsed.percent)
        .map_err(|e| kiosk_config_error(VERB_SET_BRIGHTNESS, e))?;
    let body = serde_json::json!({
        "ok": true,
        "brightness_percent": applied,
    });
    Ok(Response::for_request(
        req,
        serde_json::to_vec(&body)
            .expect("system.kiosk response JSON always serialises"),
    ))
}

// ------------------------------ set_sleep_timeout ---------------------

#[derive(Deserialize)]
struct SetSleepTimeoutReq {
    seconds: u32,
}

fn handle_set_sleep_timeout(req: &Request) -> Result<Response, PluginError> {
    let parsed: SetSleepTimeoutReq =
        parse_payload(req, VERB_SET_SLEEP_TIMEOUT)?;
    let applied = evo_kiosk_config::set_sleep_timeout(parsed.seconds)
        .map_err(|e| kiosk_config_error(VERB_SET_SLEEP_TIMEOUT, e))?;
    let body = serde_json::json!({
        "ok": true,
        "sleep_timeout_seconds": applied,
    });
    Ok(Response::for_request(
        req,
        serde_json::to_vec(&body)
            .expect("system.kiosk response JSON always serialises"),
    ))
}

// ------------------------------ set_sleep_inhibit_while_playing -------

#[derive(Deserialize)]
struct SetSleepInhibitWhilePlayingReq {
    enabled: bool,
}

fn handle_set_sleep_inhibit_while_playing(
    req: &Request,
) -> Result<Response, PluginError> {
    let parsed: SetSleepInhibitWhilePlayingReq =
        parse_payload(req, VERB_SET_SLEEP_INHIBIT_WHILE_PLAYING)?;
    let applied =
        evo_kiosk_config::set_sleep_inhibit_while_playing(parsed.enabled)
            .map_err(|e| {
                kiosk_config_error(VERB_SET_SLEEP_INHIBIT_WHILE_PLAYING, e)
            })?;
    let body = serde_json::json!({
        "ok": true,
        "sleep_inhibit_while_playing": applied,
    });
    Ok(Response::for_request(
        req,
        serde_json::to_vec(&body)
            .expect("system.kiosk response JSON always serialises"),
    ))
}

// ------------------------------ get_display_state ---------------------

fn handle_get_display_state(req: &Request) -> Result<Response, PluginError> {
    // Read verb, no payload accepted (empty object per the UI-team
    // contract). Anything non-empty is refused so a future writer
    // adding query params cannot silently degrade to an unfiltered
    // read.
    if !req.payload.is_empty() {
        let parsed: serde_json::Value = serde_json::from_slice(&req.payload)
            .map_err(|e| {
                PluginError::Permanent(format!(
                    "{VERB_GET_DISPLAY_STATE}: payload JSON invalid: {e}"
                ))
            })?;
        if !matches!(&parsed, serde_json::Value::Object(m) if m.is_empty()) {
            return Err(PluginError::Permanent(format!(
                "{VERB_GET_DISPLAY_STATE}: payload must be an empty object; got {parsed}"
            )));
        }
    }

    // Compose the full state — each field falls back to its
    // apply-time default when the overlay is absent, so a paired
    // browser mounting on a fresh boot with no operator changes
    // yet made sees the system's actual defaults (see
    // evo_kiosk_config::read_display_state for the fallback ladder).
    let state = evo_kiosk_config::read_display_state();
    let body = serde_json::json!({
        "ok": true,
        "display_rotation": state.display_rotation,
        "touch": {
            "rotation": state.touch.rotation,
            "hflip": state.touch.hflip,
            "vflip": state.touch.vflip,
        },
        "brightness_percent": state.brightness_percent,
        "sleep_timeout_seconds": state.sleep_timeout_seconds,
        "sleep_inhibit_while_playing": state.sleep_inhibit_while_playing,
        "enabled": state.enabled,
    });
    Ok(Response::for_request(
        req,
        serde_json::to_vec(&body)
            .expect("system.kiosk response JSON always serialises"),
    ))
}

/// Silence the compiler about the `TouchSample` re-import
/// staying pinned even though the current implementation does
/// not use it server-side; keeps the type available for a
/// direct-samples path that pipes samples through this plugin
/// rather than via the on-glass wizard.
#[allow(dead_code)]
fn _touch_sample_type_pinned(_s: TouchSample) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses() {
        let m = manifest();
        assert_eq!(m.plugin.name, PLUGIN_NAME);
        assert_eq!(m.plugin.version, plugin_crate_version());
    }

    #[test]
    fn calibrate_trigger_path_under_overlay_dir() {
        let p = calibrate_trigger_path();
        assert!(p.starts_with(OVERLAY_DIR));
        assert_eq!(
            p.file_name().and_then(|s| s.to_str()),
            Some(CALIBRATE_TRIGGER_FILE)
        );
    }
}
