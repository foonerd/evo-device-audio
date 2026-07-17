// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Notifications-plane primitive.
//!
//! Receives plugin-emitted [`Notification`] payloads, applies the
//! active mode + quiet-hours overrides, and tracks the active
//! notifications list for the visual UI banner. Audio dispatch
//! (chime through a separate ALSA pcm or pre-empt fallback,
//! voice TTS through pause/resume against the active source) is
//! not in this primitive — it lands when the bit-perfect audio
//! data plane and verb dispatch wire layers are wired into a
//! backend. This primitive surfaces the mode-resolution decision
//! together with the active-notifications registry; an audio
//! backend observes the registry and renders.
//!
//! ## What this module owns
//!
//! - The [`NotificationDispatcher`] hub holding active
//!   notifications + the operator-configured base mode + quiet-
//!   hours policy.
//! - Per-notification handle minting + cancel.
//! - Group-coalescing by [`NotificationGroupId`] (multiple
//!   notifications sharing a group id collapse on the visual
//!   banner with a count).
//! - Pure-function mode resolution
//!   ([`resolve_active_mode`]) so the resolution logic is
//!   testable in isolation.
//!
//! ## What this module does not own
//!
//! - Wire-protocol entry point for plugin emit (lives in the
//!   wire layer + `LoadContext.notifications`).
//! - Audio rendering (lives in the bit-perfect audio data plane
//!   + the verb-dispatch path).
//! - Visual rendering (lives in the UI shell + the
//!   `evo.notifications.banner` widget).
//! - Audit-ledger entries for notification lifecycle (`emitted`
//!   / `dispatched` / `dismissed` / `action_invoked` /
//!   `suppressed_by_quiet_hours`) — wire in at the
//!   wiring-layer call site to the audit ledger.
//!
//! ## Persistence
//!
//! None. Notifications are ephemeral by nature; restart drops
//! the active list. Recurring conditions (a sensor still in
//! error after restart) re-emit; transient conditions (a
//! doorbell ring from before reboot) are gone.

use evo_plugin_sdk::contract::{
    ExternalAddressing, Notification, NotificationError, NotificationGroupId,
    NotificationMode, NotificationPriority, SubjectAnnouncement,
    SubjectAnnouncer,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Instant, SystemTime};

pub use evo_plugin_sdk::contract::NotificationHandle;

/// Subject-type name for the `system.notifications.active`
/// singleton subject. Snake-case per the subject-substrate
/// convention (subject types cannot contain dots; the shelf
/// name `system.notifications.active` in the catalogue schema
/// maps here for wire transport).
pub const ACTIVE_SUBJECT_TYPE: &str = "system_notifications_active";

/// Scheme for the singleton addressing. One instance per node
/// with the addressing value `local` — consumers subscribe to
/// the fixed addressing rather than learning their own node
/// id, matching the `audio_multiroom_local_role` pattern.
pub const ACTIVE_SUBJECT_SCHEME: &str = "evo.system.notifications.active";

/// Addressing value for the singleton subject instance.
pub const ACTIVE_SUBJECT_LOCAL: &str = "local";

/// Time-of-day window for quiet-hours overrides. Times are in
/// minutes since midnight (0..1440) so the window can wrap
/// midnight (e.g., 22:00..07:00).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuietHoursWindow {
    /// Start of the window, in minutes since midnight (0..1440).
    pub start_min: u16,
    /// End of the window, in minutes since midnight (0..1440).
    /// May be less than `start_min` to indicate a window that
    /// wraps midnight.
    pub end_min: u16,
}

impl QuietHoursWindow {
    /// Returns `true` when `now_min` falls inside the window.
    /// Handles the wrap-midnight case where `end_min < start_min`.
    pub fn contains(self, now_min: u16) -> bool {
        if self.start_min <= self.end_min {
            now_min >= self.start_min && now_min < self.end_min
        } else {
            // Window wraps midnight.
            now_min >= self.start_min || now_min < self.end_min
        }
    }
}

/// Operator-configured quiet-hours policy. The dispatcher
/// applies this against the configured base mode at every
/// `send` to compute the effective mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuietHoursPolicy {
    /// Whether quiet hours are enabled.
    pub enabled: bool,
    /// Time window during which the override applies.
    pub window: Option<QuietHoursWindow>,
    /// Mode the override forces during the window. Conventionally
    /// `DisplayOnly` (audiophile + multi-purpose) but operators
    /// can pick `Chime` if they want chime-only-no-voice during
    /// quiet hours.
    pub mode_override: NotificationMode,
    /// When `true`, `Critical`-priority notifications bypass the
    /// override and play under the configured base mode. When
    /// `false`, even Critical respects quiet hours (the operator
    /// asked for silence; respect it).
    pub allow_critical: bool,
}

impl Default for QuietHoursPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            window: None,
            mode_override: NotificationMode::DisplayOnly,
            allow_critical: true,
        }
    }
}

/// Pure-function mode resolution. Given the configured base mode,
/// the quiet-hours policy, the current time-of-day, and the
/// notification's priority, returns the mode the dispatcher
/// should apply.
///
/// Resolution rules:
///
/// 1. If quiet hours are disabled → return `base`.
/// 2. If the window is `None` → return `base`.
/// 3. If `now_min` is not inside the window → return `base`.
/// 4. If the priority is `Critical` and `allow_critical = true`
///    → return `base` (Critical bypasses).
/// 5. Otherwise → return `policy.mode_override`.
pub fn resolve_active_mode(
    base: NotificationMode,
    policy: &QuietHoursPolicy,
    now_min: u16,
    priority: NotificationPriority,
) -> NotificationMode {
    if !policy.enabled {
        return base;
    }
    let Some(window) = policy.window else {
        return base;
    };
    if !window.contains(now_min) {
        return base;
    }
    if priority == NotificationPriority::Critical && policy.allow_critical {
        return base;
    }
    policy.mode_override
}

/// One active-notifications registry entry. Carries the
/// notification + the resolved mode + the wall-clock instant at
/// which auto-dismiss fires (when applicable).
#[derive(Debug, Clone)]
pub struct ActiveNotification {
    /// Stable handle minted at `send` time.
    pub handle: NotificationHandle,
    /// The notification payload.
    pub notification: Notification,
    /// The mode the dispatcher resolved for this notification at
    /// `send` time. The audio backend reads this to decide
    /// whether to render audio.
    pub resolved_mode: NotificationMode,
    /// Wall-clock instant at which auto-dismiss fires, if the
    /// notification carried `auto_dismiss_after`.
    pub auto_dismiss_at: Option<Instant>,
    /// When this notification participates in a group, the count
    /// of grouped notifications coalesced onto this entry. The
    /// banner widget renders the count.
    pub group_count: u32,
}

/// Notifications-plane dispatcher. The framework wires one
/// instance behind `LoadContext.notifications` (when that wiring
/// lands). Plugins call `send` / `cancel`; the framework's audio
/// backend + UI shell observe the active list.
///
/// When [`Self::attach_subject_publisher`] is invoked at wiring
/// time, the dispatcher additionally republishes a
/// `system_notifications_active` subject on every state
/// transition. UI consumers subscribe to the fixed
/// (`ACTIVE_SUBJECT_SCHEME`, `ACTIVE_SUBJECT_LOCAL`) addressing
/// and receive full-snapshot envelopes on every send / cancel /
/// mode-change / auto-dismiss.
pub struct NotificationDispatcher {
    inner: Mutex<DispatcherInner>,
    next_handle: AtomicU64,
    /// Optional subject-publisher slot. Populated by
    /// [`Self::attach_subject_publisher`] at wiring time; absent
    /// during unit tests and in bootstrap paths before the
    /// SubjectAnnouncer is available.
    publisher: Mutex<Option<Publisher>>,
}

/// The subject-publisher record the dispatcher retains after
/// [`NotificationDispatcher::attach_subject_publisher`]. Held
/// under a Mutex so publishing works from &self.
struct Publisher {
    announcer: Arc<dyn SubjectAnnouncer>,
    /// Wall-clock closure. Returns minutes-since-midnight
    /// (0..1439). The dispatcher uses this at publish time to
    /// compute `quiet_hours_active` in the envelope. When the
    /// wiring layer has no clock source (tests, bootstrap
    /// early-fault paths), the fallback closure returns 0 —
    /// which resolves `quiet_hours_active` to false unless the
    /// operator's window explicitly covers midnight.
    now_min_fn: Arc<dyn Fn() -> u16 + Send + Sync>,
}

impl std::fmt::Debug for NotificationDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotificationDispatcher").finish()
    }
}

impl Default for NotificationDispatcher {
    fn default() -> Self {
        Self::new(NotificationMode::DisplayOnly, QuietHoursPolicy::default())
    }
}

struct DispatcherInner {
    /// Operator-configured base mode for this device.
    base_mode: NotificationMode,
    /// Operator-configured quiet-hours policy.
    quiet_hours: QuietHoursPolicy,
    /// Active notifications keyed by handle. Auto-dismiss happens
    /// lazily at the next `active_notifications` / `cancel` call;
    /// expired entries are pruned in those calls.
    active: HashMap<NotificationHandle, ActiveNotification>,
    /// Group → handle index. Lets `send` find an existing
    /// representative for a group and coalesce the new notification
    /// onto it (incrementing `group_count`).
    group_index: HashMap<NotificationGroupId, NotificationHandle>,
}

impl NotificationDispatcher {
    /// Construct a dispatcher with the given operator-configured
    /// base mode and quiet-hours policy. No subject publisher is
    /// attached; call [`Self::attach_subject_publisher`] at
    /// wiring time to enable `system_notifications_active`
    /// republishes.
    pub fn new(
        base_mode: NotificationMode,
        quiet_hours: QuietHoursPolicy,
    ) -> Self {
        Self {
            inner: Mutex::new(DispatcherInner {
                base_mode,
                quiet_hours,
                active: HashMap::new(),
                group_index: HashMap::new(),
            }),
            next_handle: AtomicU64::new(0),
            publisher: Mutex::new(None),
        }
    }

    /// Attach a subject publisher. Called once at wiring time
    /// (after the SubjectAnnouncer is constructed but before
    /// plugins begin loading) so the singleton subject exists
    /// before the first plugin-emitted notification arrives.
    ///
    /// The `now_min_fn` closure returns wall-clock minutes-
    /// since-midnight (0..1439). The dispatcher uses it at
    /// publish time to compute `quiet_hours_active` in the
    /// envelope. When wiring cannot provide a real clock
    /// (bootstrap fallback, degraded boot), use a closure
    /// returning 0 — the envelope will render
    /// `quiet_hours_active = false` unless the operator's
    /// window explicitly covers midnight.
    ///
    /// After this call, every send / cancel / set_base_mode /
    /// set_quiet_hours / auto-dismiss-pruning transition
    /// fires an async subject-state update. Failures on the
    /// announcer path log at debug and do not propagate
    /// (notifications are ephemeral; the next transition's
    /// republish carries ground truth).
    pub async fn attach_subject_publisher(
        &self,
        announcer: Arc<dyn SubjectAnnouncer>,
        now_min_fn: Arc<dyn Fn() -> u16 + Send + Sync>,
    ) -> Result<(), evo_plugin_sdk::contract::ReportError> {
        // Announce with the initial (empty-list) envelope so
        // consumers subscribing during the window between
        // wiring and the first plugin-emit see a valid payload
        // rather than a not-found error.
        let envelope = self.compose_envelope(now_min_fn.as_ref());
        let state = serde_json::to_value(&envelope).unwrap_or_else(|_| {
            serde_json::Value::Object(serde_json::Map::new())
        });
        let announcement = SubjectAnnouncement {
            subject_type: ACTIVE_SUBJECT_TYPE.to_string(),
            addressings: vec![singleton_addressing()],
            claims: Vec::new(),
            state,
            announced_at: SystemTime::now(),
        };
        announcer.announce(announcement).await?;
        let mut slot = self
            .publisher
            .lock()
            .expect("publisher slot mutex poisoned at attach");
        *slot = Some(Publisher {
            announcer,
            now_min_fn,
        });
        Ok(())
    }

    /// Compose the current wire-shape envelope for the
    /// `system_notifications_active` subject. Pure — reads
    /// state under the inner lock without mutating.
    fn compose_envelope(
        &self,
        now_min_fn: &(dyn Fn() -> u16 + Send + Sync),
    ) -> ActiveNotificationsEnvelope {
        let g = self.inner.lock().expect(
            "NotificationDispatcher mutex poisoned at compose_envelope",
        );
        let now_min = now_min_fn();
        let quiet_hours_active = if g.quiet_hours.enabled {
            g.quiet_hours
                .window
                .as_ref()
                .is_some_and(|w| w.contains(now_min))
        } else {
            false
        };
        let quiet_hours_policy = ActiveQuietHoursPolicy {
            start_minute: g
                .quiet_hours
                .window
                .as_ref()
                .map_or(0, |w| w.start_min),
            end_minute: g.quiet_hours.window.as_ref().map_or(0, |w| w.end_min),
            downgrade_mode: g.quiet_hours.mode_override.as_str().to_string(),
        };
        let mut active: Vec<ActiveNotificationEnvelope> = g
            .active
            .values()
            .map(ActiveNotificationEnvelope::from)
            .collect();
        // Newest-first ordering matches the schema's stated
        // rendering hint; the tray widget uses the handle as
        // its key so ordering only affects visual layout.
        active.sort_by_key(|e| std::cmp::Reverse(e.sent_at));
        ActiveNotificationsEnvelope {
            active,
            base_mode: g.base_mode.as_str().to_string(),
            quiet_hours_active,
            quiet_hours_policy,
            last_update_at: SystemTime::now(),
        }
    }

    /// Fire-and-forget republish. No-op when no publisher is
    /// attached or when called outside a tokio runtime (unit
    /// tests without a runtime; bootstrap-early paths).
    fn schedule_republish(&self) {
        let publisher = {
            let slot = self
                .publisher
                .lock()
                .expect("publisher slot mutex poisoned at schedule_republish");
            slot.as_ref().map(|p| Publisher {
                announcer: Arc::clone(&p.announcer),
                now_min_fn: Arc::clone(&p.now_min_fn),
            })
        };
        let Some(publisher) = publisher else { return };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let envelope = self.compose_envelope(publisher.now_min_fn.as_ref());
        handle.spawn(async move {
            let state = match serde_json::to_value(&envelope) {
                Ok(v) => v,
                Err(e) => {
                    tracing::debug!(
                        error = %e,
                        "system_notifications_active envelope serialise failed"
                    );
                    return;
                }
            };
            if let Err(e) = publisher
                .announcer
                .update_state(singleton_addressing(), state)
                .await
            {
                tracing::debug!(
                    error = %e,
                    "system_notifications_active republish failed"
                );
            }
        });
    }

    /// Send a notification. The dispatcher resolves the active
    /// mode, registers the notification, coalesces by `group_with`
    /// when present, and returns a handle the caller can use to
    /// cancel before auto-dismiss fires.
    ///
    /// `now_min` is the current time-of-day in minutes since
    /// midnight (0..1440). The caller threads this in so the
    /// dispatcher remains time-source-agnostic; the wiring layer
    /// computes it from the framework's clock primitive at the
    /// call site.
    pub fn send(
        &self,
        notification: Notification,
        now_min: u16,
    ) -> Result<(NotificationHandle, NotificationMode), NotificationError> {
        if notification.source_plugin.is_empty() {
            return Err(NotificationError::Invalid(
                "source_plugin is empty".into(),
            ));
        }
        if notification.title_key.is_empty() {
            return Err(NotificationError::Invalid(
                "title_key is empty".into(),
            ));
        }
        let mut g = self
            .inner
            .lock()
            .expect("NotificationDispatcher mutex poisoned at send");
        // Group coalesce: if a group already has a representative,
        // increment its count and return the existing handle. The
        // resolved mode reported back is the existing entry's mode
        // (the banner already settled on it); a new send into a
        // group does not re-resolve under the live quiet-hours
        // window — that would let a Critical-priority follow-up
        // override a Routine representative's mode.
        if let Some(group_id) = notification.group_with.as_ref() {
            if let Some(existing) = g.group_index.get(group_id).cloned() {
                if let Some(entry) = g.active.get_mut(&existing) {
                    entry.group_count = entry.group_count.saturating_add(1);
                    // Update the representative's notification to
                    // the newest payload so the banner shows the
                    // most recent title/body.
                    entry.notification = notification;
                    let resolved = entry.resolved_mode;
                    drop(g);
                    self.schedule_republish();
                    return Ok((existing, resolved));
                }
            }
        }
        let mode = resolve_active_mode(
            g.base_mode,
            &g.quiet_hours,
            now_min,
            notification.priority,
        );
        let handle = NotificationHandle::from_raw(
            self.next_handle.fetch_add(1, Ordering::Relaxed),
        );
        let auto_dismiss_at =
            notification.auto_dismiss_after.map(|d| Instant::now() + d);
        let entry = ActiveNotification {
            handle: handle.clone(),
            notification: notification.clone(),
            resolved_mode: mode,
            auto_dismiss_at,
            group_count: 1,
        };
        if let Some(group_id) = notification.group_with.clone() {
            g.group_index.insert(group_id, handle.clone());
        }
        g.active.insert(handle.clone(), entry);
        drop(g);
        self.schedule_republish();
        Ok((handle, mode))
    }

    /// Cancel a notification by handle. Returns
    /// `NotificationError::HandleNotFound` when the handle is not
    /// in the active list (already cancelled, auto-dismissed, or
    /// never registered).
    pub fn cancel(
        &self,
        handle: &NotificationHandle,
    ) -> Result<(), NotificationError> {
        let mut g = self
            .inner
            .lock()
            .expect("NotificationDispatcher mutex poisoned at cancel");
        let entry = g.active.remove(handle).ok_or_else(|| {
            NotificationError::HandleNotFound(handle.to_string())
        })?;
        // Remove the group representative if this entry was the
        // one indexed.
        if let Some(group_id) = entry.notification.group_with.as_ref() {
            if g.group_index.get(group_id) == Some(handle) {
                g.group_index.remove(group_id);
            }
        }
        drop(g);
        self.schedule_republish();
        Ok(())
    }

    /// Return a snapshot of the active-notifications list. Prunes
    /// auto-dismissed entries lazily on this call so the audio /
    /// UI consumers see the live set.
    pub fn active_notifications(&self) -> Vec<ActiveNotification> {
        let mut g = self
            .inner
            .lock()
            .expect("NotificationDispatcher mutex poisoned at active");
        let now = Instant::now();
        let expired: Vec<NotificationHandle> = g
            .active
            .iter()
            .filter_map(|(h, e)| {
                e.auto_dismiss_at
                    .and_then(|t| (t <= now).then(|| h.clone()))
            })
            .collect();
        let pruned_any = !expired.is_empty();
        for h in expired {
            if let Some(entry) = g.active.remove(&h) {
                if let Some(gid) = entry.notification.group_with.as_ref() {
                    if g.group_index.get(gid) == Some(&h) {
                        g.group_index.remove(gid);
                    }
                }
            }
        }
        let mut out: Vec<ActiveNotification> =
            g.active.values().cloned().collect();
        out.sort_by_key(|e| e.handle.raw());
        drop(g);
        if pruned_any {
            // Auto-dismiss pruning changed the active list;
            // republish so subscribers see the new set atomically
            // with the removal (per the shelf's acceptance
            // criterion `auto-dismiss-fires-republish`).
            self.schedule_republish();
        }
        out
    }

    /// Update the operator-configured base mode. Existing active
    /// notifications retain the mode they were dispatched under;
    /// future `send` calls use the new base.
    pub fn set_base_mode(&self, mode: NotificationMode) {
        let mut g = self
            .inner
            .lock()
            .expect("NotificationDispatcher mutex poisoned at set_base_mode");
        g.base_mode = mode;
        drop(g);
        self.schedule_republish();
    }

    /// Update the operator-configured quiet-hours policy.
    pub fn set_quiet_hours(&self, policy: QuietHoursPolicy) {
        let mut g = self
            .inner
            .lock()
            .expect("NotificationDispatcher mutex poisoned at set_quiet_hours");
        g.quiet_hours = policy;
        drop(g);
        self.schedule_republish();
    }

    /// Return the count of currently-active notifications. Useful
    /// for the UI's badge.
    pub fn active_count(&self) -> usize {
        let g = self
            .inner
            .lock()
            .expect("NotificationDispatcher mutex poisoned at active_count");
        g.active.len()
    }

    /// Verb-dispatch entry point. Called by the plugin's
    /// [`crate::NotificationsPlugin::handle_request`] with the
    /// verb name and the payload bytes. Deserialises the payload
    /// against the verb's expected shape, invokes the
    /// corresponding dispatcher method, and serialises the
    /// response back to bytes.
    ///
    /// Errors:
    ///
    /// - [`VerbDispatchError::UnknownRequestType`] when the verb
    ///   is not one of the five declared on the shelf.
    /// - [`VerbDispatchError::PayloadDecode`] when the payload
    ///   bytes do not deserialise against the verb's shape.
    /// - [`VerbDispatchError::InvalidNotification`] when the
    ///   dispatcher rejects a `send` payload (empty title_key,
    ///   empty source_plugin, etc).
    /// - [`VerbDispatchError::HandleNotFound`] surfaces only on
    ///   the send path when a group representative vanishes mid-
    ///   coalesce (should not happen; defensive).
    /// - [`VerbDispatchError::ResponseSerialise`] when the
    ///   response envelope fails to serialise (should not happen
    ///   with the current serde-derived shapes).
    ///
    /// The `system.notifications.cancel` verb is idempotent per
    /// the shelf's acceptance criterion: cancelling an unknown
    /// handle returns an empty response, not an error.
    pub async fn dispatch_verb(
        &self,
        verb: &str,
        payload: &[u8],
    ) -> Result<Vec<u8>, VerbDispatchError> {
        match verb {
            VERB_LIST_ACTIVE => {
                let publisher_now_min = self
                    .publisher
                    .lock()
                    .ok()
                    .and_then(|slot| slot.as_ref().map(|p| (p.now_min_fn)()))
                    .unwrap_or(0);
                let envelope = self.compose_envelope(&|| publisher_now_min);
                serde_json::to_vec(&ListActiveResponse { envelope }).map_err(
                    |e| VerbDispatchError::ResponseSerialise {
                        verb: verb.to_string(),
                        source: e,
                    },
                )
            }
            VERB_SEND => {
                let req: SendRequest = serde_json::from_slice(payload)
                    .map_err(|e| VerbDispatchError::PayloadDecode {
                        verb: verb.to_string(),
                        source: e,
                    })?;
                let now = self
                    .publisher
                    .lock()
                    .ok()
                    .and_then(|slot| slot.as_ref().map(|p| (p.now_min_fn)()))
                    .unwrap_or(0);
                let (handle, resolved) = self
                    .send(req.notification, now)
                    .map_err(VerbDispatchError::InvalidNotification)?;
                serde_json::to_vec(&SendResponse {
                    handle: handle.raw(),
                    resolved_mode: resolved.as_str().to_string(),
                })
                .map_err(|e| {
                    VerbDispatchError::ResponseSerialise {
                        verb: verb.to_string(),
                        source: e,
                    }
                })
            }
            VERB_CANCEL => {
                let req: CancelRequest = serde_json::from_slice(payload)
                    .map_err(|e| VerbDispatchError::PayloadDecode {
                        verb: verb.to_string(),
                        source: e,
                    })?;
                let handle = NotificationHandle::from_raw(req.handle);
                match self.cancel(&handle) {
                    Ok(()) | Err(NotificationError::HandleNotFound(_)) => {
                        // Idempotent per shelf acceptance
                        // criterion `cancel-verb-idempotent`.
                        serde_json::to_vec(&CancelResponse { cancelled: true })
                            .map_err(|e| VerbDispatchError::ResponseSerialise {
                                verb: verb.to_string(),
                                source: e,
                            })
                    }
                    Err(e) => Err(VerbDispatchError::InvalidNotification(e)),
                }
            }
            VERB_SET_BASE_MODE => {
                let req: SetBaseModeRequest = serde_json::from_slice(payload)
                    .map_err(|e| {
                    VerbDispatchError::PayloadDecode {
                        verb: verb.to_string(),
                        source: e,
                    }
                })?;
                let mode = parse_mode(&req.mode).map_err(|e| {
                    VerbDispatchError::InvalidNotification(
                        NotificationError::Invalid(e),
                    )
                })?;
                self.set_base_mode(mode);
                serde_json::to_vec(&AckResponse { ack: true }).map_err(|e| {
                    VerbDispatchError::ResponseSerialise {
                        verb: verb.to_string(),
                        source: e,
                    }
                })
            }
            VERB_SET_QUIET_HOURS => {
                let req: SetQuietHoursRequest = serde_json::from_slice(payload)
                    .map_err(|e| VerbDispatchError::PayloadDecode {
                        verb: verb.to_string(),
                        source: e,
                    })?;
                let policy = req.to_policy().map_err(|e| {
                    VerbDispatchError::InvalidNotification(
                        NotificationError::Invalid(e),
                    )
                })?;
                self.set_quiet_hours(policy);
                serde_json::to_vec(&AckResponse { ack: true }).map_err(|e| {
                    VerbDispatchError::ResponseSerialise {
                        verb: verb.to_string(),
                        source: e,
                    }
                })
            }
            other => Err(VerbDispatchError::UnknownRequestType {
                verb: other.to_string(),
            }),
        }
    }
}

// --------------------------------------------------------------
// Verb catalogue + verb-dispatch helpers.
// --------------------------------------------------------------

/// The five verbs the notifications shelf declares. Used by the
/// plugin's manifest self-check and by [`is_notifications_verb`].
pub const NOTIFICATIONS_VERBS: &[&str] = &[
    VERB_LIST_ACTIVE,
    VERB_SET_BASE_MODE,
    VERB_SET_QUIET_HOURS,
    VERB_SEND,
    VERB_CANCEL,
];

/// `system.notifications.list_active` — read-then-subscribe seed.
pub const VERB_LIST_ACTIVE: &str = "system.notifications.list_active";
/// `system.notifications.set_base_mode` — operator config.
pub const VERB_SET_BASE_MODE: &str = "system.notifications.set_base_mode";
/// `system.notifications.set_quiet_hours` — operator config.
pub const VERB_SET_QUIET_HOURS: &str = "system.notifications.set_quiet_hours";
/// `system.notifications.send` — plugin-side notification emission.
pub const VERB_SEND: &str = "system.notifications.send";
/// `system.notifications.cancel` — operator + plugin cancel path.
pub const VERB_CANCEL: &str = "system.notifications.cancel";

/// Returns `true` when the given verb is one of the five declared
/// on the shelf. The plugin's `handle_request` uses this as its
/// admission guard before dispatch.
pub fn is_notifications_verb(verb: &str) -> bool {
    NOTIFICATIONS_VERBS.contains(&verb)
}

/// Errors surfaced by [`NotificationDispatcher::dispatch_verb`].
#[derive(Debug, thiserror::Error)]
pub enum VerbDispatchError {
    /// Verb name is not one of [`NOTIFICATIONS_VERBS`].
    #[error("unknown request_type: {verb}")]
    UnknownRequestType {
        /// The offending verb.
        verb: String,
    },
    /// Payload bytes did not deserialise against the verb's shape.
    #[error("payload decode failed for {verb}: {source}")]
    PayloadDecode {
        /// The verb whose payload failed to decode.
        verb: String,
        /// The underlying serde error.
        #[source]
        source: serde_json::Error,
    },
    /// Response envelope failed to serialise.
    #[error("response serialise failed for {verb}: {source}")]
    ResponseSerialise {
        /// The verb whose response failed to serialise.
        verb: String,
        /// The underlying serde error.
        #[source]
        source: serde_json::Error,
    },
    /// Dispatcher rejected the request (empty title / source /
    /// invalid mode / etc.).
    #[error("invalid notification: {0}")]
    InvalidNotification(NotificationError),
    /// Handle not found (defensive; the cancel path treats this
    /// as idempotent success and never surfaces this variant to
    /// callers).
    #[error("handle not found: {0}")]
    HandleNotFound(String),
}

// --------------------------------------------------------------
// Wire-shape request / response envelopes.
// --------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SendRequest {
    notification: Notification,
}

#[derive(Debug, Serialize)]
struct SendResponse {
    handle: u64,
    resolved_mode: String,
}

#[derive(Debug, Deserialize)]
struct CancelRequest {
    handle: u64,
}

#[derive(Debug, Serialize)]
struct CancelResponse {
    cancelled: bool,
}

#[derive(Debug, Deserialize)]
struct SetBaseModeRequest {
    mode: String,
}

#[derive(Debug, Deserialize)]
struct SetQuietHoursRequest {
    enabled: bool,
    #[serde(default)]
    start_minute: u16,
    #[serde(default)]
    end_minute: u16,
    #[serde(default)]
    mode_override: Option<String>,
    #[serde(default = "default_true")]
    allow_critical: bool,
}

fn default_true() -> bool {
    true
}

impl SetQuietHoursRequest {
    fn to_policy(&self) -> Result<QuietHoursPolicy, String> {
        let window = if self.enabled
            && (self.start_minute != 0 || self.end_minute != 0)
        {
            if self.start_minute >= 1440 || self.end_minute >= 1440 {
                return Err(format!(
                    "quiet_hours minutes out of range: start={} end={}",
                    self.start_minute, self.end_minute
                ));
            }
            Some(QuietHoursWindow {
                start_min: self.start_minute,
                end_min: self.end_minute,
            })
        } else {
            None
        };
        let mode_override = match self.mode_override.as_deref() {
            None | Some("display_only") => NotificationMode::DisplayOnly,
            Some(other) => parse_mode(other)?,
        };
        Ok(QuietHoursPolicy {
            enabled: self.enabled,
            window,
            mode_override,
            allow_critical: self.allow_critical,
        })
    }
}

#[derive(Debug, Serialize)]
struct AckResponse {
    ack: bool,
}

#[derive(Debug, Serialize)]
struct ListActiveResponse {
    envelope: ActiveNotificationsEnvelope,
}

fn parse_mode(s: &str) -> Result<NotificationMode, String> {
    match s {
        "display_only" => Ok(NotificationMode::DisplayOnly),
        "chime" => Ok(NotificationMode::Chime),
        "voice" => Ok(NotificationMode::Voice),
        other => Err(format!("unknown notification mode: {other}")),
    }
}

/// Construct the singleton addressing for the
/// `system_notifications_active` subject instance. Fixed
/// scheme + fixed value, matching the schema shelf's
/// singleton-per-node contract.
fn singleton_addressing() -> ExternalAddressing {
    ExternalAddressing {
        scheme: ACTIVE_SUBJECT_SCHEME.to_string(),
        value: ACTIVE_SUBJECT_LOCAL.to_string(),
    }
}

// --------------------------------------------------------------
// Wire-shape envelope
// --------------------------------------------------------------
//
// These types shape the `system_notifications_active` subject
// state field. They mirror the schema shelf at
// `evo-catalogue-schemas/schemas/org.evoframework/system/
// notifications.v1.toml`. The framework serialises via serde
// (JSON) at publish time; the UI shell deserialises the same
// shape.
//
// Kept in the framework runtime rather than the SDK because
// the envelope is a runtime-emit surface, not a plugin-authored
// type. Plugins emit `Notification` (SDK); the framework
// resolves + publishes `ActiveNotificationsEnvelope` (this
// module) on the shelf's subject.

/// Full-snapshot envelope carried on every republish of the
/// `system_notifications_active` subject.
///
/// Nested types (`AudioPayload`, `NotificationAction`) come
/// from the SDK and serialise with their existing Serde
/// derives — matching the wire shape plugins already know.
#[derive(Debug, Clone, Serialize)]
struct ActiveNotificationsEnvelope {
    active: Vec<ActiveNotificationEnvelope>,
    base_mode: String,
    quiet_hours_active: bool,
    quiet_hours_policy: ActiveQuietHoursPolicy,
    last_update_at: SystemTime,
}

#[derive(Debug, Clone, Serialize)]
struct ActiveNotificationEnvelope {
    handle: u64,
    level: String,
    source_plugin: String,
    title_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    body_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    audio_payload: Option<evo_plugin_sdk::contract::AudioPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_widget: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    actions: Vec<evo_plugin_sdk::contract::NotificationAction>,
    priority: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    group_with: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    auto_dismiss_after_ms: Option<u64>,
    resolved_mode: String,
    group_count: u32,
    sent_at: SystemTime,
}

#[derive(Debug, Clone, Serialize)]
struct ActiveQuietHoursPolicy {
    start_minute: u16,
    end_minute: u16,
    downgrade_mode: String,
}

impl From<&ActiveNotification> for ActiveNotificationEnvelope {
    fn from(e: &ActiveNotification) -> Self {
        // The dispatcher stores auto-dismiss as an Instant
        // deadline; the SDK-carried Duration is the operator-
        // visible value the wire envelope reports. Recomputing
        // from Instant would drift under wall-clock
        // adjustments; the Duration is stable.
        let auto_dismiss_after_ms = e
            .notification
            .auto_dismiss_after
            .map(|d| d.as_millis() as u64);
        // Sent_at approximated at envelope construction — the
        // dispatcher does not currently store the wall-clock
        // stamp; UI treats this field as advisory. A follow-on
        // dispatcher change can persist the true stamp when
        // per-entry age matters for rendering.
        let sent_at = SystemTime::now();
        ActiveNotificationEnvelope {
            handle: e.handle.raw(),
            level: e.notification.level.as_str().to_string(),
            source_plugin: e.notification.source_plugin.clone(),
            title_key: e.notification.title_key.clone(),
            body_key: e.notification.body_key.clone(),
            audio_payload: e.notification.audio_payload.clone(),
            display_widget: e.notification.display_widget.clone(),
            actions: e.notification.actions.clone(),
            priority: e.notification.priority.as_str().to_string(),
            group_with: e
                .notification
                .group_with
                .as_ref()
                .map(|g| g.as_str().to_string()),
            auto_dismiss_after_ms,
            resolved_mode: e.resolved_mode.as_str().to_string(),
            group_count: e.group_count,
            sent_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_plugin_sdk::contract::{
        AudioPayload, NotificationLevel, NotificationPriority,
    };
    use std::time::Duration;

    fn sample(priority: NotificationPriority) -> Notification {
        Notification {
            level: NotificationLevel::Info,
            source_plugin: "org.example.plugin".into(),
            title_key: "title.key".into(),
            body_key: None,
            audio_payload: Some(AudioPayload::default()),
            display_widget: None,
            actions: vec![],
            priority,
            group_with: None,
            auto_dismiss_after: None,
        }
    }

    fn quiet_hours_22_to_07() -> QuietHoursPolicy {
        QuietHoursPolicy {
            enabled: true,
            window: Some(QuietHoursWindow {
                start_min: 22 * 60, // 22:00
                end_min: 7 * 60,    // 07:00 (next morning; wraps midnight)
            }),
            mode_override: NotificationMode::DisplayOnly,
            allow_critical: true,
        }
    }

    // --- QuietHoursWindow ---

    #[test]
    fn quiet_hours_window_non_wrapping() {
        let w = QuietHoursWindow {
            start_min: 9 * 60,
            end_min: 17 * 60,
        };
        assert!(!w.contains(8 * 60));
        assert!(w.contains(9 * 60));
        assert!(w.contains(12 * 60));
        assert!(!w.contains(17 * 60)); // exclusive end
        assert!(!w.contains(20 * 60));
    }

    #[test]
    fn quiet_hours_window_wraps_midnight() {
        let w = QuietHoursWindow {
            start_min: 22 * 60,
            end_min: 7 * 60,
        };
        // After start: in window.
        assert!(w.contains(22 * 60));
        assert!(w.contains(23 * 60));
        // Around midnight: in window.
        assert!(w.contains(0));
        assert!(w.contains(3 * 60));
        // Before end: in window.
        assert!(w.contains(6 * 60 + 59));
        // At/after end: not in window.
        assert!(!w.contains(7 * 60));
        assert!(!w.contains(12 * 60));
    }

    // --- resolve_active_mode ---

    #[test]
    fn resolve_returns_base_when_quiet_hours_disabled() {
        let policy = QuietHoursPolicy::default();
        let m = resolve_active_mode(
            NotificationMode::Voice,
            &policy,
            22 * 60,
            NotificationPriority::Routine,
        );
        assert_eq!(m, NotificationMode::Voice);
    }

    #[test]
    fn resolve_returns_base_when_outside_window() {
        let policy = quiet_hours_22_to_07();
        let m = resolve_active_mode(
            NotificationMode::Chime,
            &policy,
            12 * 60, // noon
            NotificationPriority::Routine,
        );
        assert_eq!(m, NotificationMode::Chime);
    }

    #[test]
    fn resolve_overrides_inside_window_for_routine() {
        let policy = quiet_hours_22_to_07();
        let m = resolve_active_mode(
            NotificationMode::Chime,
            &policy,
            23 * 60,
            NotificationPriority::Routine,
        );
        assert_eq!(m, NotificationMode::DisplayOnly);
    }

    #[test]
    fn resolve_critical_bypasses_when_allow_critical() {
        let policy = quiet_hours_22_to_07();
        let m = resolve_active_mode(
            NotificationMode::Chime,
            &policy,
            23 * 60,
            NotificationPriority::Critical,
        );
        assert_eq!(m, NotificationMode::Chime);
    }

    #[test]
    fn resolve_critical_respects_override_when_disallowed() {
        let mut policy = quiet_hours_22_to_07();
        policy.allow_critical = false;
        let m = resolve_active_mode(
            NotificationMode::Chime,
            &policy,
            23 * 60,
            NotificationPriority::Critical,
        );
        assert_eq!(m, NotificationMode::DisplayOnly);
    }

    // --- NotificationDispatcher ---

    #[test]
    fn send_records_notification_and_returns_handle() {
        let d = NotificationDispatcher::default();
        let (h, mode) = d
            .send(sample(NotificationPriority::Routine), 12 * 60)
            .unwrap();
        // The resolved mode returned alongside the handle matches
        // the mode the dispatcher recorded internally.
        assert_eq!(mode, NotificationMode::DisplayOnly);
        let active = d.active_notifications();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].handle, h);
        assert_eq!(active[0].resolved_mode, NotificationMode::DisplayOnly);
        assert_eq!(active[0].group_count, 1);
    }

    #[test]
    fn send_resolves_quiet_hours_override_per_priority() {
        let d = NotificationDispatcher::new(
            NotificationMode::Chime,
            quiet_hours_22_to_07(),
        );
        // Routine inside window → DisplayOnly.
        let (h_routine, _) = d
            .send(sample(NotificationPriority::Routine), 23 * 60)
            .unwrap();
        // Critical inside window → still Chime (allow_critical = true).
        let (h_critical, _) = d
            .send(sample(NotificationPriority::Critical), 23 * 60)
            .unwrap();
        let active = d.active_notifications();
        let r = active.iter().find(|a| a.handle == h_routine).unwrap();
        let c = active.iter().find(|a| a.handle == h_critical).unwrap();
        assert_eq!(r.resolved_mode, NotificationMode::DisplayOnly);
        assert_eq!(c.resolved_mode, NotificationMode::Chime);
    }

    #[test]
    fn cancel_removes_active_entry() {
        let d = NotificationDispatcher::default();
        let (h, _) = d
            .send(sample(NotificationPriority::Routine), 12 * 60)
            .unwrap();
        d.cancel(&h).unwrap();
        assert_eq!(d.active_count(), 0);
        // Cancelling again returns HandleNotFound.
        let err = d.cancel(&h).unwrap_err();
        assert!(matches!(err, NotificationError::HandleNotFound(_)));
    }

    #[test]
    fn group_with_coalesces_onto_existing_representative() {
        let d = NotificationDispatcher::default();
        let group = NotificationGroupId::new("doorbell").unwrap();
        let mut n1 = sample(NotificationPriority::Routine);
        n1.group_with = Some(group.clone());
        n1.title_key = "first.title".into();
        let (h1, _) = d.send(n1, 12 * 60).unwrap();

        let mut n2 = sample(NotificationPriority::Routine);
        n2.group_with = Some(group.clone());
        n2.title_key = "second.title".into();
        let (h2, _) = d.send(n2, 12 * 60).unwrap();

        // Coalesce: same handle returned; group_count = 2; the
        // representative's notification is the newest payload.
        assert_eq!(h1, h2);
        assert_eq!(d.active_count(), 1);
        let active = d.active_notifications();
        assert_eq!(active[0].group_count, 2);
        assert_eq!(active[0].notification.title_key, "second.title");
    }

    #[test]
    fn cancel_clears_group_index() {
        let d = NotificationDispatcher::default();
        let group = NotificationGroupId::new("doorbell").unwrap();
        let mut n1 = sample(NotificationPriority::Routine);
        n1.group_with = Some(group.clone());
        let (h1, _) = d.send(n1, 12 * 60).unwrap();
        d.cancel(&h1).unwrap();

        // After cancel, a fresh notification with the same group
        // gets a new handle (no stale coalesce target).
        let mut n2 = sample(NotificationPriority::Routine);
        n2.group_with = Some(group.clone());
        let (h2, _) = d.send(n2, 12 * 60).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn auto_dismiss_prunes_expired_entries() {
        let d = NotificationDispatcher::default();
        let mut n = sample(NotificationPriority::Routine);
        n.auto_dismiss_after = Some(Duration::from_millis(1));
        d.send(n, 12 * 60).unwrap();
        // Sleep past the dismiss timeout.
        std::thread::sleep(Duration::from_millis(5));
        let active = d.active_notifications();
        assert!(active.is_empty(), "expired entry should be pruned");
        assert_eq!(d.active_count(), 0);
    }

    #[test]
    fn empty_arguments_refused() {
        let d = NotificationDispatcher::default();
        let mut n = sample(NotificationPriority::Routine);
        n.source_plugin = "".into();
        let err = d.send(n, 12 * 60).unwrap_err();
        assert!(matches!(err, NotificationError::Invalid(_)));

        let mut n = sample(NotificationPriority::Routine);
        n.title_key = "".into();
        let err = d.send(n, 12 * 60).unwrap_err();
        assert!(matches!(err, NotificationError::Invalid(_)));
    }

    #[test]
    fn set_base_mode_affects_subsequent_sends() {
        let d = NotificationDispatcher::default(); // base = DisplayOnly
        let (h1, _) = d
            .send(sample(NotificationPriority::Routine), 12 * 60)
            .unwrap();
        d.set_base_mode(NotificationMode::Voice);
        let (h2, _) = d
            .send(sample(NotificationPriority::Routine), 12 * 60)
            .unwrap();
        let active = d.active_notifications();
        let a1 = active.iter().find(|a| a.handle == h1).unwrap();
        let a2 = active.iter().find(|a| a.handle == h2).unwrap();
        assert_eq!(a1.resolved_mode, NotificationMode::DisplayOnly);
        assert_eq!(a2.resolved_mode, NotificationMode::Voice);
    }

    #[test]
    fn set_quiet_hours_changes_resolution() {
        let d = NotificationDispatcher::new(
            NotificationMode::Chime,
            QuietHoursPolicy::default(),
        );
        let (h1, _) = d
            .send(sample(NotificationPriority::Routine), 23 * 60)
            .unwrap();
        // Now flip quiet hours on; future sends override.
        d.set_quiet_hours(quiet_hours_22_to_07());
        let (h2, _) = d
            .send(sample(NotificationPriority::Routine), 23 * 60)
            .unwrap();
        let active = d.active_notifications();
        let a1 = active.iter().find(|a| a.handle == h1).unwrap();
        let a2 = active.iter().find(|a| a.handle == h2).unwrap();
        assert_eq!(a1.resolved_mode, NotificationMode::Chime);
        assert_eq!(a2.resolved_mode, NotificationMode::DisplayOnly);
    }
}
