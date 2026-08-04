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
//! ADR-0148 preseed-pair headless first-pair path.
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
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities,
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

/// Trigger file the plugin touches when the operator asks for
/// the wizard from a remote browser. Kiosk-browser polls this
/// file's mtime and dispatches an in-page CustomEvent on
/// change. Path is per-file rather than a signal channel
/// because the browser's polling loop is trivial and no
/// framework happening plumbing is required for the first cut.
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

/// The plugin singleton. Stateless beyond the loaded flag; every
/// verb call is fresh reads + writes against the overlay dir.
pub struct SystemKioskPlugin {
    loaded: bool,
}

impl SystemKioskPlugin {
    /// New instance; call [`Plugin::load`] before handling requests.
    pub fn new() -> Self {
        Self { loaded: false }
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
        _ctx: &'a LoadContext,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
        async move {
            tracing::info!(plugin = PLUGIN_NAME, "system.kiosk plugin load");
            self.loaded = true;
            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            self.loaded = false;
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

/// Silence the compiler about the `TouchSample` re-import
/// staying pinned even though the current cut does not use it
/// server-side; keeps the type available for a follow-on cycle
/// that pipes samples through this plugin directly rather than
/// via the on-glass wizard.
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
