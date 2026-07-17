// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! # org-evoframework-system-notifications
//!
//! Framework-reference plugin that stocks the
//! `system.notifications` shelf. Owns the in-memory
//! [`runtime::NotificationDispatcher`] (active list + operator-
//! configured base mode + quiet-hours policy + group coalescing)
//! and the `system_notifications_active` singleton subject.
//!
//! ## What this plugin does
//!
//! On [`Plugin::load`] the plugin:
//!
//! 1. Constructs a fresh [`runtime::NotificationDispatcher`]
//!    with the default base mode ([`NotificationMode::DisplayOnly`])
//!    and no quiet-hours window. Persistence is not in scope for
//!    notifications — they are ephemeral by charter; restart drops
//!    the active list. Operator base-mode + quiet-hours settings
//!    survive via the plugin's config file (a follow-on ship);
//!    v0.1.13 keeps them in-memory.
//! 2. Attaches the [`LoadContext::subject_announcer`] handle so
//!    the dispatcher republishes `system_notifications_active`
//!    on every send / cancel / mode-change / auto-dismiss
//!    transition.
//! 3. Spawns the [`demo`] emitter when
//!    `EVO_NOTIFICATIONS_DEMO=1` — a rotating sample set that
//!    exercises every envelope shape variant for UI-widget
//!    development. Off by default.
//!
//! On [`Respondent::handle_request`] the plugin routes the
//! `request_type` through the runtime's `dispatch_verb` helper
//! which handles every declared verb (`list_active`, `send`,
//! `cancel`, `set_base_mode`, `set_quiet_hours`).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::manual_async_fn)]

use std::future::Future;
use std::sync::Arc;

use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities,
};
use evo_plugin_sdk::Manifest;

pub mod demo;
pub mod runtime;

use runtime::{
    is_notifications_verb, NotificationDispatcher, VerbDispatchError,
    NOTIFICATIONS_VERBS,
};

/// Embedded manifest source.
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");

/// Plugin name (reverse-DNS); same as manifest and tests.
pub const PLUGIN_NAME: &str = "org.evoframework.system.notifications";

/// Parse the embedded [`Manifest`].
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML).expect(
        "org-evoframework-system-notifications: embedded manifest must parse",
    )
}

fn plugin_crate_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is valid semver")
}

/// Wall-clock minutes-since-midnight (0..1439). SystemTime::now
/// is monotonic-enough for quiet-hours resolution (windows are
/// minute-grained; a clock jump moves the resolution once, not
/// continuously). Any failure (system clock pre-epoch, subtle
/// overflow) falls back to 0, which leaves quiet_hours_active
/// false unless the operator's window explicitly covers midnight.
pub fn now_min() -> u16 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| ((d.as_secs() / 60) % 1440) as u16)
        .unwrap_or(0)
}

/// The plugin singleton. Holds the dispatcher + the demo-emitter
/// join handle so shutdown can abort it.
pub struct NotificationsPlugin {
    loaded: bool,
    dispatcher: Option<Arc<NotificationDispatcher>>,
    demo_task: Option<tokio::task::JoinHandle<()>>,
}

impl NotificationsPlugin {
    /// New instance; call [`Plugin::load`] before handling requests.
    pub fn new() -> Self {
        Self {
            loaded: false,
            dispatcher: None,
            demo_task: None,
        }
    }
}

impl Default for NotificationsPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for NotificationsPlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: PLUGIN_NAME.to_string(),
                    version: plugin_crate_version(),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: NOTIFICATIONS_VERBS
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
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
            tracing::info!(
                plugin = PLUGIN_NAME,
                state_dir = %ctx.state_dir.display(),
                "system.notifications plugin load"
            );

            let dispatcher = Arc::new(NotificationDispatcher::default());

            let announcer = Arc::clone(&ctx.subject_announcer);
            let now_min_fn: Arc<dyn Fn() -> u16 + Send + Sync> =
                Arc::new(now_min);
            if let Err(e) = dispatcher
                .attach_subject_publisher(announcer, now_min_fn)
                .await
            {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "system.notifications subject publisher attach failed; \
                     dispatcher runs without republishing until next restart"
                );
            }

            self.demo_task = demo::spawn_if_enabled(Arc::clone(&dispatcher));

            self.dispatcher = Some(dispatcher);
            self.loaded = true;
            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            if let Some(h) = self.demo_task.take() {
                h.abort();
            }
            self.dispatcher = None;
            self.loaded = false;
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            if self.loaded && self.dispatcher.is_some() {
                HealthReport::healthy()
            } else {
                HealthReport::unhealthy(
                    "system.notifications plugin not loaded",
                )
            }
        }
    }
}

impl Respondent for NotificationsPlugin {
    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            let Some(disp) = self.dispatcher.as_ref() else {
                return Err(PluginError::Permanent(
                    "system.notifications plugin not loaded".to_string(),
                ));
            };
            if req.is_past_deadline() {
                return Err(PluginError::Transient(
                    "request deadline already expired".to_string(),
                ));
            }
            let verb = req.request_type.as_str();
            if !is_notifications_verb(verb) {
                return Err(PluginError::Permanent(format!(
                    "system.notifications: unknown verb {verb:?}"
                )));
            }
            tracing::debug!(
                plugin = PLUGIN_NAME,
                verb,
                cid = req.correlation_id,
                scope = req.principal_scope.as_deref().unwrap_or("<none>"),
                has_step_up = req.has_step_up,
                "system.notifications: dispatcher-authorised verb"
            );
            match disp.dispatch_verb(verb, &req.payload).await {
                Ok(bytes) => Ok(Response::for_request(req, bytes)),
                Err(e) => Err(verb_error_to_plugin_error(e)),
            }
        }
    }
}

fn verb_error_to_plugin_error(e: VerbDispatchError) -> PluginError {
    match e {
        VerbDispatchError::UnknownRequestType { .. }
        | VerbDispatchError::PayloadDecode { .. }
        | VerbDispatchError::ResponseSerialise { .. }
        | VerbDispatchError::InvalidNotification(_) => {
            PluginError::Permanent(e.to_string())
        }
        VerbDispatchError::HandleNotFound(_) => {
            // Idempotent per shelf acceptance criterion:
            // cancel-verb-idempotent surfaces NotFound as a
            // clean response, not a plugin error. The plugin
            // returns Ok with an empty body upstream; this
            // branch is defensive for send-error paths that
            // route through the error type by mistake.
            PluginError::Transient(e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses() {
        let _ = manifest();
    }

    #[test]
    fn manifest_declares_every_runtime_verb() {
        let m = manifest();
        let resp = m
            .capabilities
            .respondent
            .as_ref()
            .expect("respondent capabilities present");
        for v in NOTIFICATIONS_VERBS {
            assert!(
                resp.request_types.iter().any(|s| s == v),
                "manifest request_types missing {v:?}"
            );
        }
        assert_eq!(resp.request_types.len(), NOTIFICATIONS_VERBS.len());
    }

    #[test]
    fn manifest_target_names_the_system_notifications_shelf() {
        let m = manifest();
        assert_eq!(m.target.shelf, "system.notifications");
        assert_eq!(m.target.shape, 1);
    }
}
