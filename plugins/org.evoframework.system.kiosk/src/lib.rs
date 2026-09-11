// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! # org-evoframework-system-kiosk
//!
//! Framework-reserved kiosk operator-settings plugin. Stocks the
//! `system.kiosk` shelf with eleven operator-gestured verbs: ten
//! writes and one read. Display and touch alignment:
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
//! - `derive_touch_calibration_from_corners` — takes the four
//!   `(target, actual)` samples the wizard captured, derives the
//!   best rotation/flip triple and persists it, returning the
//!   winning triple and its mean residual.
//!
//!   This verb exists so the wizard's own write is gated. The
//!   kiosk-browser used to do this in-process through an
//!   `evo_sample_touch_calibration_from_corners` WebKit handler
//!   that never reached the framework dispatcher, so nothing
//!   could refuse it — a box whose household level protects
//!   system settings would still have its touch matrix rewritten
//!   from its own glass. That handler is gone from the browser
//!   source; this verb is where the operation lives.
//!
//! The rest of the shelf is the per-device physical settings the
//! same operator flow reaches: `set_enabled`, `set_brightness`,
//! `set_sleep_timeout`, `set_sleep_inhibit_while_playing`,
//! `set_osk`, `set_cursor`, and the read companion
//! `get_display_state`.
//!
//! Every write is gated at the framework dispatcher's per-verb
//! capability gate as `write:system_admin` (no step-up);
//! `get_display_state` is `read:system`. Rationale for no
//! step-up: rotation is a cosmetic-visible change,
//! not a credential mint. A step-up gate would require the
//! operator to enter the kiosk password on the very glass they
//! are trying to fix — recursive breakage. The bootstrap-
//! alignment case (operator not yet paired) is served by the
//! headless preseed-pair path documented in the kiosk-eng repo.
//!
//! ## Why this plugin exists
//!
//! It is the writer. The kiosk-browser
//! (`evo-kiosk-browser`) once exposed these writes as WebKit
//! script-message handlers on its UserContentManager, reachable
//! from JavaScript inside the browser process and from nowhere
//! else. Those handlers called [`evo_kiosk_config`] directly, so
//! they bypassed the framework dispatcher entirely: no
//! capability gate, no household policy, no refusal possible.
//!
//! The browser source no longer registers the two touch
//! handlers, and the one name it still registers —
//! `evo_set_display_rotation` — is a presence probe whose
//! handler only logs. The UI reads that name to tell "am I on
//! the glass" and nothing more. Both the glass UI and a remote
//! paired browser now dispatch the verbs below; they differ in
//! which screen the operator is looking at, not in the path the
//! write takes.
//!
//! Which devices that is true OF is a separate question, and
//! the answer is per-box. The two playground rigs — the glass
//! and the VM — now run the browser built from `75796d9`: the
//! two touch handlers are absent from the shipped image and the
//! rotation name is a presence probe whose handler only logs. On
//! those two boxes this plugin is the writer, and a script in
//! the glass has no ungated path to the overlays.
//!
//! The third playground rig was deliberately not overlaid, and
//! testers on `Latest` still run the older piece, which carries
//! the old handlers — on those, a script inside the browser can
//! still write these overlays ungated. Closing that is a remint,
//! and a remint is not named. Do not read this crate's docs as a
//! claim about every device in the field.
//!
//! ## One writer
//!
//! This plugin calls [`evo_kiosk_config`] for the actual
//! filesystem work. In the current source it is the only caller
//! on the write path — on a box running that source, the only
//! one at all. The wizard's derivation math lives in the same
//! crate, so there is no second overlay format and no second
//! validator to drift from, whichever browser build a box
//! happens to carry.

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

/// Verb name — derive the touch triple from four corner samples
/// and persist it.
///
/// The wizard's own write. It replaces an in-process WebKit
/// handler that performed the same derivation without passing
/// the dispatcher; this one is gated `write:system_admin` like
/// every other write on the shelf.
pub const VERB_DERIVE_TOUCH_CALIBRATION_FROM_CORNERS: &str =
    "derive_touch_calibration_from_corners";

/// Verb name — enable or disable the evo-kiosk.service unit.
pub const VERB_SET_ENABLED: &str = "set_enabled";

/// Verb name — set the compositor display brightness percent.
pub const VERB_SET_BRIGHTNESS: &str = "set_brightness";

/// Verb name — set the idle sleep timeout in seconds.
pub const VERB_SET_SLEEP_TIMEOUT: &str = "set_sleep_timeout";

/// Verb name — toggle "keep the screen awake while playing."
pub const VERB_SET_SLEEP_INHIBIT_WHILE_PLAYING: &str =
    "set_sleep_inhibit_while_playing";

/// Verb name — turn the on-screen keyboard on or off.
pub const VERB_SET_OSK: &str = "set_osk";

/// Verb name — show or hide the mouse pointer.
///
/// Takes effect immediately. Pointer visibility is decided by
/// which cursor theme the compositor loads, and a compositor
/// reads that once at startup, so the verb persists the choice
/// and then restarts the kiosk session to apply it. The operator
/// sees a brief flash as the session comes back.
///
/// The restart is the whole applier. There is no compositor
/// action that hides a pointer durably — labwc's `HideCursor`
/// gives it back on the next pointer motion, and does not exist
/// at all on the older labwc in the field.
pub const VERB_SET_CURSOR: &str = "set_cursor";

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
                        VERB_DERIVE_TOUCH_CALIBRATION_FROM_CORNERS.to_string(),
                        VERB_SET_ENABLED.to_string(),
                        VERB_SET_BRIGHTNESS.to_string(),
                        VERB_SET_SLEEP_TIMEOUT.to_string(),
                        VERB_SET_SLEEP_INHIBIT_WHILE_PLAYING.to_string(),
                        VERB_SET_OSK.to_string(),
                        VERB_SET_CURSOR.to_string(),
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
                VERB_DERIVE_TOUCH_CALIBRATION_FROM_CORNERS => {
                    handle_derive_touch_calibration_from_corners(req)
                }
                VERB_SET_ENABLED => handle_set_enabled(req).await,
                VERB_SET_BRIGHTNESS => handle_set_brightness(req),
                VERB_SET_SLEEP_TIMEOUT => handle_set_sleep_timeout(req),
                VERB_SET_SLEEP_INHIBIT_WHILE_PLAYING => {
                    handle_set_sleep_inhibit_while_playing(req)
                }
                VERB_SET_OSK => handle_set_osk(req),
                VERB_SET_CURSOR => handle_set_cursor(req).await,
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

/// `launch_touch_calibration` carries nothing: it is a signal to
/// the on-glass browser to open the wizard. It used to reserve an
/// ignored `samples` field for a direct-samples path; that path
/// now exists as its own gated verb
/// (`derive_touch_calibration_from_corners`), so the dead field
/// is gone rather than sitting behind an `allow`.
#[derive(Deserialize, Default)]
struct LaunchTouchCalibrationReq {}

/// Payload for `derive_touch_calibration_from_corners`.
///
/// Exactly four samples, in the order the wizard drew the
/// targets. The count and coordinate range are enforced by
/// `evo_kiosk_config::derive_touch_calibration`, so this struct
/// deliberately does not re-check them: one validator, in the
/// crate that owns the math.
#[derive(Deserialize)]
struct DeriveTouchCalibrationReq {
    samples: Vec<TouchSampleWire>,
}

/// One `(target, actual)` pair in normalised output space, as it
/// arrives on the wire.
#[derive(Deserialize)]
struct TouchSampleWire {
    target_x: f64,
    target_y: f64,
    actual_x: f64,
    actual_y: f64,
}

impl From<TouchSampleWire> for TouchSample {
    fn from(w: TouchSampleWire) -> Self {
        TouchSample {
            target_x: w.target_x,
            target_y: w.target_y,
            actual_x: w.actual_x,
            actual_y: w.actual_y,
        }
    }
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
        | KioskConfigError::SampleOutOfRange(_)
        | KioskConfigError::InvalidOsk(_)
        | KioskConfigError::InvalidCursor(_) => {
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

/// Derive the touch triple from four corner samples and persist
/// it.
///
/// The derivation and the write are both
/// `evo_kiosk_config::derive_and_apply_touch_calibration`, the
/// same function the retired WebKit handler used to call
/// in-process — so the overlay bytes are unchanged by the move,
/// and there is no second format. What this path adds is the
/// dispatcher: the framework has already checked
/// `write:system_admin` (and, where a household policy protects
/// that scope, refused with `household_policy_locked`) before
/// the request arrives here.
///
/// Refuses a sample count other than four and coordinates
/// outside [0, 1] — both as `Permanent`, since a retry with the
/// same payload cannot succeed.
fn handle_derive_touch_calibration_from_corners(
    req: &Request,
) -> Result<Response, PluginError> {
    let parsed: DeriveTouchCalibrationReq =
        parse_payload(req, VERB_DERIVE_TOUCH_CALIBRATION_FROM_CORNERS)?;
    let samples: Vec<TouchSample> =
        parsed.samples.into_iter().map(TouchSample::from).collect();
    let derived =
        evo_kiosk_config::derive_and_apply_touch_calibration(&samples)
            .map_err(|e| {
                kiosk_config_error(
                    VERB_DERIVE_TOUCH_CALIBRATION_FROM_CORNERS,
                    e,
                )
            })?;
    let body = serde_json::json!({
        "ok": true,
        "touch_rotation": derived.rotation,
        "touch_hflip": derived.hflip,
        "touch_vflip": derived.vflip,
        // Mean per-sample residual in normalised units. The
        // operator surface uses it to offer "this looks off,
        // try again" rather than to gate the write.
        "mean_error": derived.mean_error,
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

// ------------------------------ set_osk -------------------------------

#[derive(Deserialize)]
struct SetOskReq {
    enabled: bool,
}

fn handle_set_osk(req: &Request) -> Result<Response, PluginError> {
    let parsed: SetOskReq = parse_payload(req, VERB_SET_OSK)?;
    // The overlay write is the whole action. The kiosk-side
    // watcher owns starting and stopping the keyboard when the
    // overlay changes, so this verb never spawns or kills a
    // process itself — one writer, one applier.
    let applied = evo_kiosk_config::set_osk(parsed.enabled)
        .map_err(|e| kiosk_config_error(VERB_SET_OSK, e))?;
    let body = serde_json::json!({
        "ok": true,
        "osk_enabled": applied,
    });
    Ok(Response::for_request(
        req,
        serde_json::to_vec(&body)
            .expect("system.kiosk response JSON always serialises"),
    ))
}

// ------------------------------ set_cursor ----------------------------

#[derive(Deserialize)]
struct SetCursorReq {
    visible: bool,
}

async fn handle_set_cursor(req: &Request) -> Result<Response, PluginError> {
    let parsed: SetCursorReq = parse_payload(req, VERB_SET_CURSOR)?;
    // Persist first, so a read taken while the session is
    // bouncing already reports the operator's choice.
    let applied = evo_kiosk_config::set_cursor(parsed.visible)
        .map_err(|e| kiosk_config_error(VERB_SET_CURSOR, e))?;

    // A stopped session is left stopped. `systemctl restart`
    // would start it, so an operator who has turned the kiosk
    // off would find a pointer preference had switched their
    // screen back on. The overlay is already written and
    // `evo-kiosk-launch` exports the theme at exec, so the
    // choice still applies whenever the session next starts.
    if !kiosk_session_running().await {
        tracing::info!(
            plugin = PLUGIN_NAME,
            cursor_visible = applied,
            "set_cursor: session not running; persisted for next start"
        );
        return cursor_response(req, applied);
    }

    // Sudo grant is enumerated by the paired
    // /etc/sudoers.d/evo-system-kiosk drop-in as
    // EVO_SYSTEM_KIOSK_RESTART. Argv must match the alias
    // exactly; no shell interpolation, and deliberately no
    // `--now` — this restarts a session, it never changes
    // whether the unit is enabled.
    let output = tokio::process::Command::new("/usr/bin/sudo")
        .arg("-n")
        .arg("/usr/bin/systemctl")
        .arg("restart")
        .arg("evo-kiosk.service")
        .output()
        .await
        .map_err(|e| {
            PluginError::Transient(format!(
                "set_cursor: spawning sudo systemctl failed: {e}"
            ))
        })?;
    if !output.status.success() {
        // Roll the overlay back: the pointer on screen did not
        // change, so the read must not claim it did.
        let _ = evo_kiosk_config::set_cursor(!parsed.visible);
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(PluginError::Transient(format!(
            "set_cursor: systemctl restart evo-kiosk.service exited {:?}: {stderr}",
            output.status.code()
        )));
    }
    cursor_response(req, applied)
}

/// Is the kiosk session currently up? Read-only and unprivileged
/// — `is-active` needs no grant, and its exit status is the
/// answer.
async fn kiosk_session_running() -> bool {
    tokio::process::Command::new("/usr/bin/systemctl")
        .arg("is-active")
        .arg("--quiet")
        .arg("evo-kiosk.service")
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

fn cursor_response(
    req: &Request,
    cursor_visible: bool,
) -> Result<Response, PluginError> {
    let body = serde_json::json!({
        "ok": true,
        "cursor_visible": cursor_visible,
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
        "osk_enabled": state.osk_enabled,
        "cursor_visible": state.cursor_visible,
    });
    Ok(Response::for_request(
        req,
        serde_json::to_vec(&body)
            .expect("system.kiosk response JSON always serialises"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses() {
        let m = manifest();
        assert_eq!(m.plugin.name, PLUGIN_NAME);
        assert_eq!(m.plugin.version, plugin_crate_version());
    }

    /// The out-of-process manifest is the one that ships. A verb
    /// declared in only one of the two manifests is a verb the
    /// device refuses, so both are parsed and compared here.
    const MANIFEST_OOP_TOML: &str = include_str!("../manifest.oop.toml");

    static OVERLAY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Points the kiosk-settings read/write surface at a scratch
    /// directory so a fixture can drive the real verb handler and
    /// then read the bytes it actually wrote.
    struct ScratchOverlays {
        dir: std::path::PathBuf,
        previous: Option<String>,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl ScratchOverlays {
        fn new(tag: &str) -> Self {
            let guard = OVERLAY_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let dir = std::env::temp_dir()
                .join(format!("evo-kiosk-plugin-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch dir");
            let previous = std::env::var("KIOSK_SETTINGS_DIR").ok();
            std::env::set_var("KIOSK_SETTINGS_DIR", &dir);
            Self {
                dir,
                previous,
                _guard: guard,
            }
        }

        fn bytes(&self, name: &str) -> Option<String> {
            std::fs::read_to_string(self.dir.join(name)).ok()
        }
    }

    impl Drop for ScratchOverlays {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => std::env::set_var("KIOSK_SETTINGS_DIR", v),
                None => std::env::remove_var("KIOSK_SETTINGS_DIR"),
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn request(verb: &str, payload: serde_json::Value) -> Request {
        Request {
            request_type: verb.to_string(),
            payload: serde_json::to_vec(&payload).unwrap(),
            correlation_id: 1,
            deadline: None,
            instance_id: None,
            principal_scope: Some("system_admin".to_string()),
            has_step_up: false,
        }
    }

    fn body(resp: &Response) -> serde_json::Value {
        serde_json::from_slice(&resp.payload).expect("response is JSON")
    }

    #[test]
    fn set_osk_is_declared_and_scoped_in_both_manifests() {
        for (label, toml) in
            [("manifest", MANIFEST_TOML), ("oop", MANIFEST_OOP_TOML)]
        {
            let m = Manifest::from_toml(toml)
                .unwrap_or_else(|e| panic!("{label} manifest parses: {e}"));
            let r = m
                .capabilities
                .respondent
                .as_ref()
                .unwrap_or_else(|| panic!("{label} declares a respondent"));
            assert!(
                r.request_types.iter().any(|v| v == VERB_SET_OSK),
                "{label} manifest must stock {VERB_SET_OSK}"
            );
            // Write scope, not step-up: the on-screen keyboard is
            // how a touch operator would type a step-up password,
            // so gating it behind step-up can lock them out.
            match r.verb_capabilities.get(VERB_SET_OSK) {
                Some(evo_plugin_sdk::manifest::VerbCapability::Write {
                    scope,
                }) => assert_eq!(scope, "system_admin", "{label}"),
                other => {
                    panic!("{label}: {VERB_SET_OSK} must be write/system_admin, got {other:?}")
                }
            }
        }
    }

    /// The wizard's write must be declared exactly like the
    /// control it sits beside. If the two ever differ, one of
    /// them is reachable under a policy that refuses the other.
    #[test]
    fn derive_from_corners_is_declared_exactly_like_set_touch_calibration() {
        for (label, toml) in
            [("manifest", MANIFEST_TOML), ("oop", MANIFEST_OOP_TOML)]
        {
            let m = Manifest::from_toml(toml)
                .unwrap_or_else(|e| panic!("{label} manifest parses: {e}"));
            let r = m
                .capabilities
                .respondent
                .as_ref()
                .unwrap_or_else(|| panic!("{label} declares a respondent"));
            assert!(
                r.request_types
                    .iter()
                    .any(|v| v == VERB_DERIVE_TOUCH_CALIBRATION_FROM_CORNERS),
                "{label} manifest must stock \
                 {VERB_DERIVE_TOUCH_CALIBRATION_FROM_CORNERS}"
            );
            let derived = r
                .verb_capabilities
                .get(VERB_DERIVE_TOUCH_CALIBRATION_FROM_CORNERS);
            let sibling = r.verb_capabilities.get(VERB_SET_TOUCH_CALIBRATION);
            assert_eq!(
                format!("{derived:?}"),
                format!("{sibling:?}"),
                "{label}: the wizard write must carry the same capability \
                 as {VERB_SET_TOUCH_CALIBRATION}"
            );
            match derived {
                Some(evo_plugin_sdk::manifest::VerbCapability::Write {
                    scope,
                }) => assert_eq!(scope, "system_admin", "{label}"),
                other => panic!(
                    "{label}: \
                     {VERB_DERIVE_TOUCH_CALIBRATION_FROM_CORNERS} must be \
                     write/system_admin, got {other:?}"
                ),
            }
        }
    }

    /// Every write on this shelf rides ONE scope. That is what
    /// lets a distribution protect the whole surface by putting a
    /// single scope in one household group: a verb added later at
    /// a different scope would silently escape the group, and
    /// this fails before it ships.
    #[test]
    fn every_write_on_this_shelf_rides_the_one_scope() {
        use evo_plugin_sdk::manifest::VerbCapability;
        for (label, toml) in
            [("manifest", MANIFEST_TOML), ("oop", MANIFEST_OOP_TOML)]
        {
            let m = Manifest::from_toml(toml).expect("manifest parses");
            let r = m.capabilities.respondent.as_ref().expect("respondent");
            let mut writes = 0usize;
            for (verb, cap) in r.verb_capabilities.iter() {
                if let VerbCapability::Write { scope } = cap {
                    writes += 1;
                    assert_eq!(
                        scope, "system_admin",
                        "{label}: write verb {verb} escapes the shelf scope"
                    );
                }
            }
            assert!(writes >= 10, "{label}: expected the full write surface");
        }
    }

    fn corner_samples(invert: bool) -> serde_json::Value {
        // The four targets the wizard draws, in its own order.
        let corners = [(0.1, 0.1), (0.9, 0.1), (0.9, 0.9), (0.1, 0.9)];
        let samples: Vec<serde_json::Value> = corners
            .iter()
            .map(|(tx, ty)| {
                let (ax, ay) = if invert {
                    (1.0 - tx, 1.0 - ty)
                } else {
                    (*tx, *ty)
                };
                serde_json::json!({
                    "target_x": tx, "target_y": ty,
                    "actual_x": ax, "actual_y": ay
                })
            })
            .collect();
        serde_json::json!({ "samples": samples })
    }

    fn derive(payload: serde_json::Value) -> Result<Response, PluginError> {
        handle_derive_touch_calibration_from_corners(&request(
            VERB_DERIVE_TOUCH_CALIBRATION_FROM_CORNERS,
            payload,
        ))
    }

    /// The wizard verb writes the SAME three overlay files, with
    /// the same bytes, that set_touch_calibration writes. No
    /// second format, no second applier.
    #[test]
    fn derive_from_corners_writes_the_set_touch_calibration_overlays() {
        let ov = ScratchOverlays::new("derive-corners");
        let read = |ov: &ScratchOverlays| {
            (
                ov.bytes("touch_rotation"),
                ov.bytes("touch_hflip"),
                ov.bytes("touch_vflip"),
            )
        };

        // Start from a deliberately non-identity state so a
        // derive that wrote nothing would be visible.
        handle_set_touch_calibration(&request(
            VERB_SET_TOUCH_CALIBRATION,
            serde_json::json!({"rotation":"90","hflip":true,"vflip":true}),
        ))
        .expect("seed write");
        let seeded = read(&ov);
        assert_eq!(seeded.0.as_deref(), Some("90"));

        // Operator tapped exactly on the targets: identity.
        let b = body(&derive(corner_samples(false)).expect("derive applies"));
        assert_eq!(b["ok"], serde_json::json!(true));
        assert_eq!(b["touch_rotation"], serde_json::json!("0"));
        assert_eq!(b["touch_hflip"], serde_json::json!(false));
        assert_eq!(b["touch_vflip"], serde_json::json!(false));
        assert!(
            b["mean_error"].as_f64().expect("mean_error is a number") < 1e-9,
            "a clean fit must report ~0 residual, got {}",
            b["mean_error"]
        );
        let after_derive = read(&ov);
        assert_ne!(after_derive, seeded, "derive must have written");

        // The same triple through the plain setter must produce
        // byte-identical overlays.
        handle_set_touch_calibration(&request(
            VERB_SET_TOUCH_CALIBRATION,
            serde_json::json!({"rotation":"0","hflip":false,"vflip":false}),
        ))
        .expect("equivalent write");
        assert_eq!(
            read(&ov),
            after_derive,
            "the wizard verb and set_touch_calibration must write the same \
             overlay bytes"
        );
    }

    /// The derivation is real, not a fixed answer: inverted taps
    /// resolve to a non-identity triple that still fits cleanly.
    #[test]
    fn derive_from_corners_actually_derives() {
        let _ov = ScratchOverlays::new("derive-corners-inverted");
        let b = body(&derive(corner_samples(true)).expect("derive applies"));
        let triple = (
            b["touch_rotation"].as_str().expect("rotation"),
            b["touch_hflip"].as_bool().expect("hflip"),
            b["touch_vflip"].as_bool().expect("vflip"),
        );
        assert_ne!(
            triple,
            ("0", false, false),
            "inverted taps must not resolve to identity"
        );
        assert!(
            b["mean_error"].as_f64().expect("mean_error") < 1e-9,
            "the inverted set has an exact fit"
        );
    }

    /// Count and range are enforced by the shared crate, and a
    /// bad payload is Permanent - retrying it cannot help.
    #[test]
    fn derive_from_corners_refuses_a_short_sample_set() {
        let _ov = ScratchOverlays::new("derive-corners-short");
        let three = serde_json::json!({"samples": [
            {"target_x":0.1,"target_y":0.1,"actual_x":0.1,"actual_y":0.1},
            {"target_x":0.9,"target_y":0.1,"actual_x":0.9,"actual_y":0.1},
            {"target_x":0.9,"target_y":0.9,"actual_x":0.9,"actual_y":0.9}
        ]});
        match derive(three) {
            Err(PluginError::Permanent(_)) => {}
            other => panic!("a 3-sample set must be Permanent, got {other:?}"),
        }
    }

    #[test]
    fn derive_from_corners_refuses_out_of_range_coordinates() {
        let _ov = ScratchOverlays::new("derive-corners-range");
        let mut payload = corner_samples(false);
        payload["samples"][0]["actual_x"] = serde_json::json!(1.5);
        match derive(payload) {
            Err(PluginError::Permanent(_)) => {}
            other => {
                panic!("an out-of-range tap must be Permanent, got {other:?}")
            }
        }
    }

    #[test]
    fn derive_from_corners_refuses_an_empty_payload() {
        // Unlike launch_touch_calibration, this verb has no
        // defaultable shape: samples are required.
        let _ov = ScratchOverlays::new("derive-corners-empty");
        match derive(serde_json::json!({})) {
            Err(PluginError::Permanent(_)) => {}
            other => {
                panic!("a missing sample set must be Permanent, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn describe_and_the_manifests_declare_the_same_verbs() {
        // The steward refuses admission when the manifest and the
        // runtime describe() disagree, so a verb added to one and
        // not the other unloads the plugin on the device. This
        // pins all three lists to each other.
        let described: std::collections::BTreeSet<String> =
            SystemKioskPlugin::default()
                .describe()
                .await
                .runtime_capabilities
                .request_types
                .into_iter()
                .collect();
        for (label, toml) in
            [("manifest", MANIFEST_TOML), ("oop", MANIFEST_OOP_TOML)]
        {
            let m = Manifest::from_toml(toml).expect("manifest parses");
            let declared: std::collections::BTreeSet<String> = m
                .capabilities
                .respondent
                .as_ref()
                .expect("respondent")
                .request_types
                .iter()
                .cloned()
                .collect();
            assert_eq!(
                declared, described,
                "{label} manifest and describe() must stock the same verbs; \
                 a mismatch fails admission on the device"
            );
        }
    }

    #[test]
    fn set_osk_writes_the_overlay_and_echoes_the_applied_state() {
        let scratch = ScratchOverlays::new("setosk");

        let resp = handle_set_osk(&request(
            VERB_SET_OSK,
            serde_json::json!({"enabled": false}),
        ))
        .expect("set_osk false");
        assert_eq!(body(&resp)["ok"], true);
        assert_eq!(body(&resp)["osk_enabled"], false);
        assert_eq!(
            scratch.bytes("osk").as_deref(),
            Some("none"),
            "the verb must write the bytes the session script reads"
        );

        let resp = handle_set_osk(&request(
            VERB_SET_OSK,
            serde_json::json!({"enabled": true}),
        ))
        .expect("set_osk true");
        assert_eq!(body(&resp)["osk_enabled"], true);
        assert_eq!(scratch.bytes("osk").as_deref(), Some("squeekboard"));
    }

    #[test]
    fn set_osk_refuses_a_malformed_payload() {
        let _scratch = ScratchOverlays::new("badpayload");
        for payload in [
            serde_json::json!({}),
            serde_json::json!({"enabled": "yes"}),
            serde_json::json!({"enable": true}),
        ] {
            assert!(
                handle_set_osk(&request(VERB_SET_OSK, payload.clone()))
                    .is_err(),
                "must refuse {payload}"
            );
        }
    }

    #[test]
    fn set_osk_refuses_an_unrecognised_overlay_rather_than_clobbering() {
        let scratch = ScratchOverlays::new("clobber");
        std::fs::write(scratch.dir.join("osk"), "some-future-engine").unwrap();
        let err = handle_set_osk(&request(
            VERB_SET_OSK,
            serde_json::json!({"enabled": true}),
        ))
        .expect_err("must refuse");
        assert!(
            matches!(err, PluginError::Permanent(_)),
            "an unparseable overlay is permanent, not retryable: {err:?}"
        );
        assert_eq!(
            scratch.bytes("osk").as_deref(),
            Some("some-future-engine"),
            "the refused write must leave the overlay untouched"
        );
    }

    #[test]
    fn set_cursor_is_declared_and_scoped_in_both_manifests() {
        for (label, toml) in
            [("manifest", MANIFEST_TOML), ("oop", MANIFEST_OOP_TOML)]
        {
            let m = Manifest::from_toml(toml)
                .unwrap_or_else(|e| panic!("{label} manifest parses: {e}"));
            let r = m
                .capabilities
                .respondent
                .as_ref()
                .unwrap_or_else(|| panic!("{label} declares a respondent"));
            assert!(
                r.request_types.iter().any(|v| v == VERB_SET_CURSOR),
                "{label} manifest must stock {VERB_SET_CURSOR}"
            );
            match r.verb_capabilities.get(VERB_SET_CURSOR) {
                Some(evo_plugin_sdk::manifest::VerbCapability::Write {
                    scope,
                }) => assert_eq!(scope, "system_admin", "{label}"),
                other => panic!(
                    "{label}: {VERB_SET_CURSOR} must be write/system_admin, got {other:?}"
                ),
            }
        }
    }

    /// The sudoers template that grants this plugin its three
    /// systemctl invocations. Read here so the argv the code
    /// builds and the alias the drop-in authorises are pinned to
    /// each other: a mismatch is invisible until a device denies
    /// the sudo call at the moment an operator presses the
    /// button.
    const SUDOERS_TEMPLATE: &str =
        include_str!("../../../dist/sudoers.d/evo-system-kiosk.in");

    #[test]
    fn sudoers_authorises_exactly_the_restart_argv_the_verb_uses() {
        // The argv in handle_set_cursor, as a single line.
        let argv = "/usr/bin/systemctl restart evo-kiosk.service";
        let alias = format!("Cmnd_Alias EVO_SYSTEM_KIOSK_RESTART = {argv}");
        assert!(
            SUDOERS_TEMPLATE.lines().any(|l| l.trim() == alias),
            "sudoers must authorise exactly `{argv}`; sudo matches argv \
             literally, so any drift denies the operator's toggle"
        );
        assert!(
            SUDOERS_TEMPLATE.lines().any(|l| l
                .trim()
                .ends_with("NOPASSWD: EVO_SYSTEM_KIOSK_RESTART")),
            "the restart alias must be granted, not merely defined"
        );
        // No `--now` on the restart alias: this restarts a
        // session and must never change whether the unit is
        // enabled.
        assert!(
            !SUDOERS_TEMPLATE
                .lines()
                .any(|l| l.contains("EVO_SYSTEM_KIOSK_RESTART =")
                    && l.contains("--now")),
            "the restart alias must not carry --now"
        );
    }

    /// These fixtures drive the real verb, which restarts the
    /// kiosk session when one is running. A build host has no
    /// such unit, so the verb takes its persist-and-return path
    /// and spawns nothing. Asserted rather than assumed: running
    /// the suite on a device would otherwise bounce the
    /// operator's screen.
    async fn refuse_if_a_live_session_would_be_restarted() {
        assert!(
            !kiosk_session_running().await,
            "evo-kiosk.service is active here; these fixtures would \
             restart a live session. Run them on a build host."
        );
    }

    #[tokio::test]
    async fn set_cursor_writes_the_policy_the_session_script_reads() {
        refuse_if_a_live_session_would_be_restarted().await;
        // `evo-kiosk-session` matches the literal `hide`, so the
        // bool has to land as those exact bytes to have any effect
        // at the next session start.
        let scratch = ScratchOverlays::new("setcursor");

        let resp = handle_set_cursor(&request(
            VERB_SET_CURSOR,
            serde_json::json!({"visible": false}),
        ))
        .await
        .expect("set_cursor false");
        assert_eq!(body(&resp)["ok"], true);
        assert_eq!(body(&resp)["cursor_visible"], false);
        assert_eq!(scratch.bytes("cursor").as_deref(), Some("hide"));

        let resp = handle_set_cursor(&request(
            VERB_SET_CURSOR,
            serde_json::json!({"visible": true}),
        ))
        .await
        .expect("set_cursor true");
        assert_eq!(body(&resp)["cursor_visible"], true);
        assert_eq!(scratch.bytes("cursor").as_deref(), Some("show"));
    }

    #[tokio::test]
    async fn set_cursor_refuses_a_malformed_payload() {
        refuse_if_a_live_session_would_be_restarted().await;
        let _scratch = ScratchOverlays::new("badcursor");
        for payload in [
            serde_json::json!({}),
            serde_json::json!({"visible": "yes"}),
            serde_json::json!({"show": true}),
        ] {
            assert!(
                handle_set_cursor(&request(VERB_SET_CURSOR, payload.clone()))
                    .await
                    .is_err(),
                "must refuse {payload}"
            );
        }
    }

    #[tokio::test]
    async fn set_cursor_refuses_an_unrecognised_overlay_rather_than_clobbering()
    {
        refuse_if_a_live_session_would_be_restarted().await;
        let scratch = ScratchOverlays::new("cursorclobber");
        std::fs::write(scratch.dir.join("cursor"), "dim").unwrap();
        let err = handle_set_cursor(&request(
            VERB_SET_CURSOR,
            serde_json::json!({"visible": true}),
        ))
        .await
        .expect_err("must refuse");
        assert!(
            matches!(err, PluginError::Permanent(_)),
            "an unparseable overlay is permanent, not retryable: {err:?}"
        );
        assert_eq!(scratch.bytes("cursor").as_deref(), Some("dim"));
    }

    #[tokio::test]
    async fn set_cursor_round_trips_through_get_display_state() {
        refuse_if_a_live_session_would_be_restarted().await;
        let _scratch = ScratchOverlays::new("cursorroundtrip");
        handle_set_cursor(&request(
            VERB_SET_CURSOR,
            serde_json::json!({"visible": false}),
        ))
        .await
        .unwrap();
        let resp = handle_get_display_state(&request(
            VERB_GET_DISPLAY_STATE,
            serde_json::json!({}),
        ))
        .unwrap();
        assert_eq!(body(&resp)["cursor_visible"], false);
    }

    #[test]
    fn get_display_state_reports_the_keyboard_and_pointer_axes() {
        let _scratch = ScratchOverlays::new("getstate");
        handle_set_osk(&request(
            VERB_SET_OSK,
            serde_json::json!({"enabled": false}),
        ))
        .unwrap();

        let resp = handle_get_display_state(&request(
            VERB_GET_DISPLAY_STATE,
            serde_json::json!({}),
        ))
        .expect("get_display_state");
        let b = body(&resp);
        assert_eq!(b["osk_enabled"], false);
        // No cursor overlay was written, so this is the documented
        // default rather than a stale value.
        assert_eq!(b["cursor_visible"], true);
        // The pre-existing surface must not have shifted.
        for key in [
            "display_rotation",
            "brightness_percent",
            "sleep_timeout_seconds",
            "sleep_inhibit_while_playing",
            "enabled",
        ] {
            assert!(!b[key].is_null(), "{key} missing from get_display_state");
        }
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
