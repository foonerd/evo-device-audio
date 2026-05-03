//! # org-evoframework-playback-mpd
//!
//! MPD playback warden for the evo audio domain. Stocks the
//! `audio.playback` shelf in any distribution catalogue that
//! declares it.
//!
//! The plugin connects to an MPD daemon at the configured endpoint,
//! takes custody of playback, applies course corrections (play,
//! pause, stop, next, previous, seek, set_volume) issued by the
//! steward, and announces `track` and `album` subjects with the
//! `album_of` relation as MPD reports song changes. Operator
//! configuration is provided through [`LoadContext::config`] (the
//! steward delivers the parsed table from
//! `/etc/evo/plugins.d/org.evoframework.playback.mpd.toml`); the
//! plugin applies it during `load()` to override the hardcoded
//! defaults set by [`MpdPlaybackPlugin::new`].
//!
//! The wire-transport binary lands after the in-process flow has
//! stabilised in any consuming distribution.
//!
//! ## Operator configuration
//!
//! The schema, defaults, validation rules, and error hierarchy
//! live in the [`config`] module. In brief:
//!
//! ```toml
//! [endpoint]
//! type = "tcp"           # "tcp" or "unix"
//! host = "127.0.0.1"     # for tcp
//! port = 6600            # for tcp
//! # path = "/run/mpd/socket"   # for unix
//!
//! [timeouts]
//! connect_ms = 5000      # 1..=60000
//! welcome_ms = 2000      # 1..=60000
//! command_ms = 3000      # 1..=300000
//! ```
//!
//! All fields optional. Missing sections or fields use the
//! defaults set by [`MpdPlaybackPlugin::new`]. An empty or absent
//! config file is a valid (default-only) configuration.
//!
//! ## Subject assertion
//!
//! On every song change, the warden announces two subjects and
//! one relation to the steward:
//!
//! - `track` subject, keyed by scheme `mpd-path`, value = MPD's
//!   `file` field (relative library path or stream URL).
//! - `album` subject, keyed by scheme `mpd-album`, value =
//!   `"{artist}|{album}"` where `artist` is the `Artist` tag if
//!   present and non-empty, else `"unknown"`. The pipe separator
//!   disambiguates same-titled albums from different artists.
//! - `album_of` relation from the track subject to the album
//!   subject.
//!
//! Emission is additive and best-effort: subjects and relations
//! accumulate in the steward's registry as they are played;
//! announcer errors are logged but do not disrupt playback. A
//! song whose `Album` tag is missing or empty produces only a
//! track subject (no album, no relation). See the
//! [`playback_supervisor::subject_emitter`] module for details.
//!
//! ## Course-correction payload encoding
//!
//! [`CourseCorrection::correction_type`] names the command;
//! [`CourseCorrection::payload`] carries parameters as UTF-8
//! text. Encoding table:
//!
//! | `correction_type` | payload              | maps to                     |
//! |-------------------|----------------------|-----------------------------|
//! | `play`            | empty                | [`PlaybackCommand::Play`]   |
//! | `play`            | `"3"` (u32)          | `PlayPosition(3)`           |
//! | `pause`           | `"1"` / `"true"`     | `Pause(true)`               |
//! | `pause`           | `"0"` / `"false"`   | `Pause(false)`              |
//! | `stop`            | empty                | `Stop`                      |
//! | `next`            | empty                | `Next`                      |
//! | `previous`        | empty                | `Previous`                  |
//! | `seek`            | `"1250"` (u64 ms)    | `Seek(Duration::from_millis(1250))` |
//! | `set_volume`      | `"50"` (u8)          | `SetVolume(50)`             |
//!
//! Unknown correction types, non-UTF-8 payloads, and unparseable
//! numeric values are rejected with [`PluginError::Permanent`]
//! before the supervisor is contacted.
//!
//! The shape of this crate mirrors the reference warden in
//! `evo-core/crates/evo-example-warden/`; deviations are confined
//! to identity (name, trust class, custody exclusivity).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// The SDK's plugin contract deliberately uses return-position
// `impl Future<Output = _> + Send + '_` rather than `async fn` for
// every trait method (see the module docs on
// `evo_plugin_sdk::contract`). The explicit `Send` bound is
// required for the multi-threaded tokio runtime the steward
// dispatches on; `async fn` in trait position would not produce
// it without unstable `return_type_notation`. Clippy's
// `manual_async_fn` lint would push us toward a form that either
// breaks Send auto-trait inference or diverges from the
// upstream reference warden (`evo-core/crates/evo-example-warden`),
// so the lint is allowed crate-wide. This is a trait-contract
// constraint, not a style preference; it applies uniformly to
// every `impl Plugin` / `impl Warden` method.
#![allow(clippy::manual_async_fn)]

mod config;
mod mpd;
mod playback_supervisor;

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use evo_plugin_sdk::contract::{
    Assignment, BuildInfo, CourseCorrection, CustodyHandle, HealthReport,
    LoadContext, Plugin, PluginDescription, PluginError, PluginIdentity,
    RelationAnnouncer, RuntimeCapabilities, SubjectAnnouncer, Warden,
};
use evo_plugin_sdk::Manifest;

use crate::config::PluginConfig;
use crate::mpd::{ConnectTimeouts, MpdEndpoint};
use crate::playback_supervisor::{
    PlaybackCommand, PlaybackError, SubjectEmitter, SupervisorHandle,
};

/// The plugin's embedded manifest, as a static string.
///
/// Available so callers can validate the manifest at test time or
/// admit the plugin without disk I/O.
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");

/// The plugin's canonical reverse-DNS name. Single source of truth
/// shared between the manifest and [`Plugin::describe`]; the
/// `identity_name_matches_manifest` test enforces parity.
pub const PLUGIN_NAME: &str = "org.evoframework.playback.mpd";

/// Default MPD host for a locally-running daemon.
const DEFAULT_MPD_HOST: &str = "127.0.0.1";
/// Default MPD TCP port (matches MPD's upstream default).
const DEFAULT_MPD_PORT: u16 = 6600;

/// Parse the embedded manifest into a [`Manifest`] struct.
///
/// Panics if the embedded manifest fails to parse. Such a failure
/// is a build-time bug, not a runtime condition, so panicking is
/// acceptable.
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("org-evoframework-playback-mpd's embedded manifest must parse")
}

/// Semver of this plugin crate, from the workspace/Cargo `version`
/// field. [`Plugin::describe`]'s [`PluginIdentity::version`],
/// [`BuildInfo::plugin_build`], and `manifest.toml` `[plugin].version`
/// must stay aligned (release tooling and tests assert this).
fn plugin_crate_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is valid semver")
}

/// Per-custody state retained for the lifetime of a custody.
///
/// Holds the [`SupervisorHandle`] returned by
/// [`playback_supervisor::spawn`] so [`Warden::course_correct`]
/// can dispatch commands and [`Warden::release_custody`] can shut
/// the supervisor down cleanly. `custody_type` is retained for
/// log breadcrumbs.
struct TrackedCustody {
    custody_type: String,
    supervisor: SupervisorHandle,
}

/// MPD playback warden plugin.
///
/// Construct via [`MpdPlaybackPlugin::new`] (default endpoint
/// `127.0.0.1:6600`, default timeouts, no subject emitter).
/// [`Plugin::load`] replaces the defaults with values from
/// [`LoadContext::config`] if the operator has supplied a config
/// file, and populates the [`SubjectEmitter`] from the load
/// context's announcer handles. Tests may also use
/// [`MpdPlaybackPlugin::with_endpoint`] to construct a plugin
/// pointing at a specific endpoint without going through the
/// `load` path; such tests set [`Self::subject_emitter`]
/// directly (typically to [`SubjectEmitter::null`]) before
/// exercising custody verbs.
pub struct MpdPlaybackPlugin {
    loaded: bool,
    endpoint: MpdEndpoint,
    timeouts: ConnectTimeouts,
    /// Bundle of subject and relation announcer handles used by
    /// [`Warden::take_custody`] to equip each spawned supervisor.
    /// `None` until [`Plugin::load`] populates from
    /// [`LoadContext`]; `take_custody` refuses to proceed when
    /// absent.
    subject_emitter: Option<SubjectEmitter>,
    custodies: HashMap<String, TrackedCustody>,
    /// Cumulative count of custodies accepted since construction.
    /// Does not decrement on release.
    custodies_taken: u64,
    /// Cumulative count of course corrections dispatched to the
    /// supervisor since construction. Counts attempts, not
    /// successes: a dispatched command that the supervisor then
    /// fails still increments this counter.
    corrections_dispatched: u64,
}

impl MpdPlaybackPlugin {
    /// Construct a plugin pointing at the default local MPD
    /// endpoint (`127.0.0.1:6600`) with default connect / welcome
    /// / command timeouts. [`Plugin::load`] overrides these from
    /// the operator's on-disk config file if one exists.
    pub fn new() -> Self {
        let endpoint = MpdEndpoint::tcp(DEFAULT_MPD_HOST, DEFAULT_MPD_PORT)
            .expect("default MPD endpoint (127.0.0.1:6600) must be valid");
        Self::with_endpoint(endpoint, ConnectTimeouts::default())
    }

    /// Construct a plugin with an explicit endpoint and timeout
    /// budget. Used by tests (pointing at a mock MPD on an
    /// ephemeral loopback port) and, where needed, by crate-
    /// internal code that bypasses the config-file path.
    pub(crate) fn with_endpoint(
        endpoint: MpdEndpoint,
        timeouts: ConnectTimeouts,
    ) -> Self {
        Self {
            loaded: false,
            endpoint,
            timeouts,
            subject_emitter: None,
            custodies: HashMap::new(),
            custodies_taken: 0,
            corrections_dispatched: 0,
        }
    }

    /// Number of custodies currently held (taken but not yet
    /// released).
    pub fn active_custody_count(&self) -> usize {
        self.custodies.len()
    }

    /// Cumulative count of custodies accepted since construction.
    pub fn custodies_taken(&self) -> u64 {
        self.custodies_taken
    }

    /// Cumulative count of course corrections dispatched to a
    /// supervisor since construction.
    pub fn corrections_dispatched(&self) -> u64 {
        self.corrections_dispatched
    }

    /// Parse an operator config table into a [`PluginConfig`] and
    /// apply it to `self`, replacing the fields set by
    /// [`MpdPlaybackPlugin::new`].
    ///
    /// Shared between the [`Plugin::load`] path (which gets the
    /// table from [`LoadContext::config`]) and tests (which
    /// construct the table directly). Does not change the
    /// `loaded` flag; that is the caller's responsibility.
    fn apply_config_table(
        &mut self,
        table: &toml::Table,
    ) -> Result<(), PluginError> {
        let config = PluginConfig::from_toml_table(table).map_err(|e| {
            PluginError::Permanent(format!("invalid plugin config: {e}"))
        })?;
        self.endpoint = config.endpoint;
        self.timeouts = config.timeouts;
        Ok(())
    }
}

impl Default for MpdPlaybackPlugin {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a [`CourseCorrection`] into the concrete
/// [`PlaybackCommand`] the supervisor understands.
///
/// See the module-level documentation for the encoding table.
/// Errors classify at the warden boundary: every rejection from
/// this function maps to [`PluginError::Permanent`] because the
/// correction is malformed and the same bytes will fail the same
/// way on retry.
fn parse_correction(
    correction: &CourseCorrection,
) -> Result<PlaybackCommand, PluginError> {
    let payload_str =
        std::str::from_utf8(&correction.payload).map_err(|_| {
            PluginError::Permanent(
                "course correction payload is not valid UTF-8".to_string(),
            )
        })?;
    let trimmed = payload_str.trim();

    match correction.correction_type.as_str() {
        "play" => {
            if trimmed.is_empty() {
                Ok(PlaybackCommand::Play)
            } else {
                let pos = trimmed.parse::<u32>().map_err(|_| {
                    PluginError::Permanent(format!(
                        "play position must be a non-negative u32, got {:?}",
                        trimmed
                    ))
                })?;
                Ok(PlaybackCommand::PlayPosition(pos))
            }
        }
        "pause" => match trimmed {
            "1" | "true" => Ok(PlaybackCommand::Pause(true)),
            "0" | "false" => Ok(PlaybackCommand::Pause(false)),
            other => Err(PluginError::Permanent(format!(
                "pause payload must be '0'/'1' or 'true'/'false', got {:?}",
                other
            ))),
        },
        "stop" => Ok(PlaybackCommand::Stop),
        "next" => Ok(PlaybackCommand::Next),
        "previous" => Ok(PlaybackCommand::Previous),
        "seek" => {
            let ms = trimmed.parse::<u64>().map_err(|_| {
                PluginError::Permanent(format!(
                    "seek payload must be a non-negative u64 of milliseconds, got {:?}",
                    trimmed
                ))
            })?;
            Ok(PlaybackCommand::Seek(Duration::from_millis(ms)))
        }
        "set_volume" => {
            let v = trimmed.parse::<u8>().map_err(|_| {
                PluginError::Permanent(format!(
                    "set_volume payload must be a u8 (0-255), got {:?}",
                    trimmed
                ))
            })?;
            Ok(PlaybackCommand::SetVolume(v))
        }
        other => Err(PluginError::Permanent(format!(
            "unknown course correction type: {:?}",
            other
        ))),
    }
}

/// Map a [`PlaybackError`] from the supervisor into the
/// [`PluginError`] variant the steward expects.
///
/// - [`PlaybackError::Ack`] is command-level: the connection is
///   healthy, MPD said no. Retrying will get the same answer.
///   Maps to [`PluginError::Permanent`].
/// - [`PlaybackError::ConnectionExhausted`] is transient: MPD was
///   unreachable across all reconnect attempts. The steward can
///   retry at a higher level. Maps to [`PluginError::Transient`].
/// - [`PlaybackError::Protocol`] is fatal: MPD is not speaking
///   the protocol correctly. Maps to [`PluginError::Fatal`] via
///   the SDK's `fatal(context, source)` helper, with the
///   [`PlaybackError`] itself as the source (it implements
///   [`std::error::Error`] via `thiserror`).
/// - [`PlaybackError::Shutdown`] means the supervisor is gone.
///   Maps to [`PluginError::Permanent`]; the caller should
///   release and re-take.
fn playback_error_to_plugin_error(e: PlaybackError) -> PluginError {
    match e {
        PlaybackError::Ack { code, message } => PluginError::Permanent(
            format!("MPD rejected command: [{}] {}", code, message),
        ),
        PlaybackError::ConnectionExhausted { attempts } => {
            PluginError::Transient(format!(
                "MPD unreachable after {} reconnect attempts",
                attempts
            ))
        }
        err @ PlaybackError::Protocol(_) => {
            PluginError::fatal("MPD protocol violation", err)
        }
        PlaybackError::Shutdown => PluginError::Permanent(
            "playback supervisor is shut down".to_string(),
        ),
    }
}

impl Plugin for MpdPlaybackPlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: PLUGIN_NAME.to_string(),
                    version: plugin_crate_version(),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: vec![],
                    accepts_custody: true,
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
                config_keys = ctx.config.len(),
                "plugin load beginning"
            );

            self.apply_config_table(&ctx.config)?;

            // Equip the subject emitter from the announcer
            // handles the steward supplied. The Arcs are cloned
            // cheaply; the emitter clones them again per custody
            // (one clone per spawn() call).
            self.subject_emitter = Some(SubjectEmitter::new(
                Arc::clone(&ctx.subject_announcer) as Arc<dyn SubjectAnnouncer>,
                Arc::clone(&ctx.relation_announcer)
                    as Arc<dyn RelationAnnouncer>,
            ));

            self.loaded = true;

            tracing::info!(
                plugin = PLUGIN_NAME,
                endpoint = %self.endpoint,
                connect_ms = self.timeouts.connect.as_millis() as u64,
                welcome_ms = self.timeouts.welcome.as_millis() as u64,
                command_ms = self.timeouts.command.as_millis() as u64,
                "plugin loaded; config applied; subject emitter equipped"
            );

            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            let active = self.custodies.len();
            tracing::info!(
                plugin = PLUGIN_NAME,
                active = active,
                taken = self.custodies_taken,
                dispatched = self.corrections_dispatched,
                "plugin unload; draining active custodies"
            );

            // Drain and shut down each supervisor in sequence.
            let custodies = std::mem::take(&mut self.custodies);
            for (id, tracked) in custodies {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    handle = %id,
                    custody_type = %tracked.custody_type,
                    "shutting down supervisor during unload"
                );
                tracked.supervisor.shutdown().await;
            }

            self.loaded = false;
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            if self.loaded {
                HealthReport::healthy()
            } else {
                HealthReport::unhealthy("playback plugin not loaded")
            }
        }
    }
}

impl Warden for MpdPlaybackPlugin {
    fn take_custody(
        &mut self,
        assignment: Assignment,
    ) -> impl Future<Output = Result<CustodyHandle, PluginError>> + Send + '_
    {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "playback plugin not loaded".to_string(),
                ));
            }

            // Defense in depth: load() populates the emitter
            // alongside setting `loaded = true`, so the two gates
            // are coupled in practice. An explicit check here
            // makes the invariant local and survives any future
            // restructuring of load().
            let emitter = match self.subject_emitter.as_ref() {
                Some(e) => e.clone(),
                None => {
                    return Err(PluginError::Permanent(
                        "subject emitter not initialised; load() was not called".to_string(),
                    ));
                }
            };

            let handle = CustodyHandle::new(format!(
                "custody-{}",
                assignment.correlation_id
            ));

            // Spawn the supervisor. Opens two MPD connections,
            // emits the initial state report, returns a handle for
            // command dispatch and shutdown. Failure maps to the
            // steward-visible PluginError variant.
            let supervisor = match playback_supervisor::spawn(
                self.endpoint.clone(),
                self.timeouts,
                handle.clone(),
                assignment.custody_state_reporter,
                emitter,
            )
            .await
            {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!(
                        plugin = PLUGIN_NAME,
                        handle = %handle.id,
                        error = %e,
                        "supervisor spawn failed; rejecting custody"
                    );
                    return Err(playback_error_to_plugin_error(e));
                }
            };

            self.custodies.insert(
                handle.id.clone(),
                TrackedCustody {
                    custody_type: assignment.custody_type.clone(),
                    supervisor,
                },
            );
            self.custodies_taken += 1;

            tracing::info!(
                plugin = PLUGIN_NAME,
                handle = %handle.id,
                custody_type = %assignment.custody_type,
                cid = assignment.correlation_id,
                "custody accepted"
            );

            Ok(handle)
        }
    }

    fn course_correct<'a>(
        &'a mut self,
        handle: &'a CustodyHandle,
        correction: CourseCorrection,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "playback plugin not loaded".to_string(),
                ));
            }

            // Parse first: a malformed correction fails with a
            // clear "request was bad" signal before we ever touch
            // the custody map or the supervisor.
            let cmd = parse_correction(&correction)?;

            let tracked = self.custodies.get(&handle.id).ok_or_else(|| {
                PluginError::Permanent(format!(
                    "unknown custody handle: {}",
                    handle.id
                ))
            })?;

            self.corrections_dispatched += 1;

            tracing::info!(
                plugin = PLUGIN_NAME,
                handle = %handle.id,
                correction_type = %correction.correction_type,
                cid = correction.correlation_id,
                "course correction dispatching to supervisor"
            );

            tracked
                .supervisor
                .command(cmd)
                .await
                .map_err(playback_error_to_plugin_error)
        }
    }

    fn release_custody(
        &mut self,
        handle: CustodyHandle,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "playback plugin not loaded".to_string(),
                ));
            }

            let tracked =
                self.custodies.remove(&handle.id).ok_or_else(|| {
                    PluginError::Permanent(format!(
                        "unknown custody handle: {}",
                        handle.id
                    ))
                })?;

            tracing::info!(
                plugin = PLUGIN_NAME,
                handle = %handle.id,
                custody_type = %tracked.custody_type,
                "custody releasing; shutting down supervisor"
            );

            tracked.supervisor.shutdown().await;

            tracing::info!(
                plugin = PLUGIN_NAME,
                handle = %handle.id,
                "custody released"
            );

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use evo_plugin_sdk::contract::{CustodyStateReporter, HealthStatus};

    use crate::playback_supervisor::test_mock::{
        capturing_emitter, short_timeouts, spawn_mock_mpd,
        spawn_unresponsive_mock, CapturingReporter, ConnBehaviour,
    };
    use crate::playback_supervisor::SubjectEmitter;

    // ----- helpers -----

    fn assignment(
        reporter: Arc<dyn CustodyStateReporter>,
        correlation_id: u64,
    ) -> Assignment {
        Assignment {
            custody_type: "playback-session".into(),
            payload: b"track-1".to_vec(),
            correlation_id,
            deadline: None,
            custody_state_reporter: reporter,
        }
    }

    fn correction(
        correction_type: &str,
        payload: &[u8],
        correlation_id: u64,
    ) -> CourseCorrection {
        CourseCorrection {
            correction_type: correction_type.to_string(),
            payload: payload.to_vec(),
            correlation_id,
        }
    }

    async fn loaded_plugin_with_mock(
        behaviours: Vec<ConnBehaviour>,
    ) -> (MpdPlaybackPlugin, tokio::task::JoinHandle<()>) {
        let (endpoint, mock_task) = spawn_mock_mpd(behaviours).await;
        let mut p =
            MpdPlaybackPlugin::with_endpoint(endpoint, short_timeouts());
        p.loaded = true;
        // Tests using this helper are not exercising the subject
        // emission pipeline; equip a null emitter to satisfy
        // take_custody's gate without recording anything.
        p.subject_emitter = Some(SubjectEmitter::null());
        (p, mock_task)
    }

    // ===== surface / manifest tests (pure) =====

    #[test]
    fn embedded_manifest_parses() {
        let m = manifest();
        assert_eq!(m.plugin.name, PLUGIN_NAME);
        assert_eq!(m.plugin.contract, 1);
        assert_eq!(
            m.kind.interaction,
            evo_plugin_sdk::manifest::InteractionShape::Warden
        );
    }

    #[tokio::test]
    async fn identity_name_and_version_match_manifest() {
        let p = MpdPlaybackPlugin::new();
        let d = p.describe().await;
        let m = manifest();
        assert_eq!(d.identity.name, m.plugin.name);
        assert_eq!(d.identity.name, PLUGIN_NAME);
        assert_eq!(
            d.identity.version, m.plugin.version,
            "CARGO_PKG_VERSION / describe() / manifest [plugin].version must match"
        );
    }

    #[tokio::test]
    async fn describe_returns_expected_identity() {
        let p = MpdPlaybackPlugin::new();
        let d = p.describe().await;
        assert_eq!(d.identity.name, PLUGIN_NAME);
        assert_eq!(d.identity.version, plugin_crate_version());
        assert_eq!(d.build_info.plugin_build, d.identity.version.to_string());
        assert_eq!(d.identity.contract, 1);
        assert!(d.runtime_capabilities.accepts_custody);
        assert!(d.runtime_capabilities.request_types.is_empty());
    }

    #[tokio::test]
    async fn health_is_unhealthy_before_load() {
        let p = MpdPlaybackPlugin::new();
        let r = p.health_check().await;
        assert!(matches!(r.status, HealthStatus::Unhealthy));
    }

    #[tokio::test]
    async fn load_unload_cycle_with_no_custodies() {
        let mut p = MpdPlaybackPlugin::new();
        p.loaded = true;
        assert!(matches!(
            p.health_check().await.status,
            HealthStatus::Healthy
        ));
        p.unload().await.unwrap();
        assert!(matches!(
            p.health_check().await.status,
            HealthStatus::Unhealthy
        ));
        assert_eq!(p.active_custody_count(), 0);
    }

    #[test]
    fn new_uses_default_endpoint_and_timeouts() {
        let p = MpdPlaybackPlugin::new();
        assert_eq!(p.endpoint, MpdEndpoint::tcp("127.0.0.1", 6600).unwrap());
        let d = ConnectTimeouts::default();
        assert_eq!(p.timeouts.connect, d.connect);
        assert_eq!(p.timeouts.welcome, d.welcome);
        assert_eq!(p.timeouts.command, d.command);
    }

    // ===== apply_config_table tests =====

    #[test]
    fn apply_config_table_empty_keeps_defaults() {
        let mut p = MpdPlaybackPlugin::new();
        let before_endpoint = p.endpoint.clone();
        let before_connect = p.timeouts.connect;

        let table: toml::Table = "".parse().unwrap();
        p.apply_config_table(&table).unwrap();

        assert_eq!(p.endpoint, before_endpoint);
        assert_eq!(p.timeouts.connect, before_connect);
    }

    #[test]
    fn apply_config_table_tcp_overrides_endpoint() {
        let mut p = MpdPlaybackPlugin::new();

        let table: toml::Table = r#"
            [endpoint]
            type = "tcp"
            host = "mpd.example"
            port = 6700
        "#
        .parse()
        .unwrap();
        p.apply_config_table(&table).unwrap();

        assert_eq!(p.endpoint, MpdEndpoint::tcp("mpd.example", 6700).unwrap());
    }

    #[test]
    fn apply_config_table_unix_overrides_endpoint() {
        let mut p = MpdPlaybackPlugin::new();

        let table: toml::Table = r#"
            [endpoint]
            type = "unix"
            path = "/run/mpd/socket"
        "#
        .parse()
        .unwrap();
        p.apply_config_table(&table).unwrap();

        assert_eq!(p.endpoint, MpdEndpoint::unix("/run/mpd/socket").unwrap());
    }

    #[test]
    fn apply_config_table_overrides_timeouts() {
        let mut p = MpdPlaybackPlugin::new();

        let table: toml::Table = r#"
            [timeouts]
            connect_ms = 1234
            welcome_ms = 567
            command_ms = 8910
        "#
        .parse()
        .unwrap();
        p.apply_config_table(&table).unwrap();

        assert_eq!(p.timeouts.connect, Duration::from_millis(1234));
        assert_eq!(p.timeouts.welcome, Duration::from_millis(567));
        assert_eq!(p.timeouts.command, Duration::from_millis(8910));
    }

    #[test]
    fn apply_config_table_invalid_config_returns_permanent() {
        let mut p = MpdPlaybackPlugin::new();

        let table: toml::Table = r#"
            [endpoint]
            type = "carrier-pigeon"
        "#
        .parse()
        .unwrap();
        let e = p.apply_config_table(&table).unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));

        // Failed apply leaves state unchanged.
        assert_eq!(p.endpoint, MpdEndpoint::tcp("127.0.0.1", 6600).unwrap());
    }

    #[test]
    fn apply_config_table_wraps_error_message() {
        let mut p = MpdPlaybackPlugin::new();

        let table: toml::Table = r#"
            [endpoint]
            port = 0
        "#
        .parse()
        .unwrap();
        let e = p.apply_config_table(&table).unwrap_err();
        match e {
            PluginError::Permanent(msg) => {
                assert!(
                    msg.contains("invalid plugin config"),
                    "message should namespace the error: {msg:?}"
                );
                assert!(
                    msg.contains("port"),
                    "message should mention the offending field: {msg:?}"
                );
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    // ===== gate tests (pure) =====

    #[tokio::test]
    async fn take_custody_rejects_before_load() {
        let mut p = MpdPlaybackPlugin::new();
        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());
        let a = assignment(reporter, 1);
        let e = p.take_custody(a).await.unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    #[tokio::test]
    async fn course_correct_rejects_before_load() {
        let mut p = MpdPlaybackPlugin::new();
        let handle = CustodyHandle::new("custody-1");
        let e = p
            .course_correct(&handle, correction("play", b"", 1))
            .await
            .unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    #[tokio::test]
    async fn course_correct_rejects_unknown_handle() {
        let mut p = MpdPlaybackPlugin::new();
        p.loaded = true;
        let handle = CustodyHandle::new("custody-does-not-exist");
        let e = p
            .course_correct(&handle, correction("play", b"", 1))
            .await
            .unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
        assert_eq!(p.corrections_dispatched(), 0);
    }

    #[tokio::test]
    async fn release_custody_rejects_unknown_handle() {
        let mut p = MpdPlaybackPlugin::new();
        p.loaded = true;
        let handle = CustodyHandle::new("custody-phantom");
        let e = p.release_custody(handle).await.unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    // ===== parse_correction tests (pure) =====

    #[test]
    fn parse_play_empty_payload_returns_play() {
        let c = correction("play", b"", 1);
        assert_eq!(parse_correction(&c).unwrap(), PlaybackCommand::Play);
    }

    #[test]
    fn parse_play_with_position() {
        let c = correction("play", b"3", 1);
        assert_eq!(
            parse_correction(&c).unwrap(),
            PlaybackCommand::PlayPosition(3)
        );
    }

    #[test]
    fn parse_pause_accepts_one_and_true() {
        for variant in [b"1" as &[u8], b"true"] {
            let c = correction("pause", variant, 1);
            assert_eq!(
                parse_correction(&c).unwrap(),
                PlaybackCommand::Pause(true)
            );
        }
    }

    #[test]
    fn parse_pause_accepts_zero_and_false() {
        for variant in [b"0" as &[u8], b"false"] {
            let c = correction("pause", variant, 1);
            assert_eq!(
                parse_correction(&c).unwrap(),
                PlaybackCommand::Pause(false)
            );
        }
    }

    #[test]
    fn parse_stop_next_previous_with_empty_payload() {
        assert_eq!(
            parse_correction(&correction("stop", b"", 1)).unwrap(),
            PlaybackCommand::Stop
        );
        assert_eq!(
            parse_correction(&correction("next", b"", 1)).unwrap(),
            PlaybackCommand::Next
        );
        assert_eq!(
            parse_correction(&correction("previous", b"", 1)).unwrap(),
            PlaybackCommand::Previous
        );
    }

    #[test]
    fn parse_seek_with_milliseconds() {
        let c = correction("seek", b"1250", 1);
        assert_eq!(
            parse_correction(&c).unwrap(),
            PlaybackCommand::Seek(Duration::from_millis(1250))
        );
    }

    #[test]
    fn parse_seek_with_zero_is_valid() {
        let c = correction("seek", b"0", 1);
        assert_eq!(
            parse_correction(&c).unwrap(),
            PlaybackCommand::Seek(Duration::from_millis(0))
        );
    }

    #[test]
    fn parse_set_volume() {
        let c = correction("set_volume", b"50", 1);
        assert_eq!(
            parse_correction(&c).unwrap(),
            PlaybackCommand::SetVolume(50)
        );
    }

    #[test]
    fn parse_set_volume_accepts_bounds() {
        assert_eq!(
            parse_correction(&correction("set_volume", b"0", 1)).unwrap(),
            PlaybackCommand::SetVolume(0)
        );
        assert_eq!(
            parse_correction(&correction("set_volume", b"255", 1)).unwrap(),
            PlaybackCommand::SetVolume(255)
        );
    }

    #[test]
    fn parse_rejects_unknown_correction_type() {
        let e = parse_correction(&correction("jitter", b"", 1)).unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    #[test]
    fn parse_rejects_non_utf8_payload() {
        let c = correction("play", &[0xff, 0xfe], 1);
        let e = parse_correction(&c).unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    #[test]
    fn parse_rejects_malformed_play_position() {
        let e = parse_correction(&correction("play", b"not-a-number", 1))
            .unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    #[test]
    fn parse_rejects_negative_play_position() {
        let e = parse_correction(&correction("play", b"-1", 1)).unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    #[test]
    fn parse_rejects_malformed_pause_value() {
        let e =
            parse_correction(&correction("pause", b"maybe", 1)).unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    #[test]
    fn parse_rejects_malformed_seek_value() {
        let e = parse_correction(&correction("seek", b"soon", 1)).unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    #[test]
    fn parse_rejects_malformed_volume_value() {
        let e = parse_correction(&correction("set_volume", b"loud", 1))
            .unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    #[test]
    fn parse_trims_payload_whitespace() {
        let c = correction("play", b"  3\n", 1);
        assert_eq!(
            parse_correction(&c).unwrap(),
            PlaybackCommand::PlayPosition(3)
        );
    }

    // ===== error mapping tests =====

    #[test]
    fn ack_maps_to_permanent() {
        let e = playback_error_to_plugin_error(PlaybackError::Ack {
            code: 2,
            message: "Bad song index".to_string(),
        });
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    #[test]
    fn exhausted_maps_to_transient() {
        let e = playback_error_to_plugin_error(
            PlaybackError::ConnectionExhausted { attempts: 10 },
        );
        assert!(matches!(e, PluginError::Transient(_)));
    }

    #[test]
    fn protocol_maps_to_fatal() {
        let e = playback_error_to_plugin_error(PlaybackError::Protocol(
            "unexpected token".to_string(),
        ));
        assert!(e.is_fatal());
    }

    #[test]
    fn shutdown_maps_to_permanent() {
        let e = playback_error_to_plugin_error(PlaybackError::Shutdown);
        assert!(matches!(e, PluginError::Permanent(_)));
    }

    // ===== integration tests (mock MPD) =====

    #[tokio::test]
    async fn take_custody_spawns_supervisor_and_emits_toml_initial_report() {
        let (mut p, _mock) = loaded_plugin_with_mock(vec![
            ConnBehaviour::Standard,
            ConnBehaviour::HoldAfterWelcome,
        ])
        .await;

        let reporter = Arc::new(CapturingReporter::default());
        let reporter_dyn: Arc<dyn CustodyStateReporter> = reporter.clone();

        let handle =
            p.take_custody(assignment(reporter_dyn, 42)).await.unwrap();
        assert_eq!(handle.id, "custody-42");
        assert_eq!(p.active_custody_count(), 1);
        assert_eq!(p.custodies_taken(), 1);

        assert_eq!(reporter.count(), 1);
        let payload = reporter.last_payload().unwrap();
        let text = String::from_utf8(payload).unwrap();
        assert!(
            text.contains("state = \"stopped\""),
            "expected TOML state field in initial report: {text:?}"
        );

        p.release_custody(handle).await.unwrap();
    }

    #[tokio::test]
    async fn take_custody_maps_exhausted_to_transient() {
        let (endpoint, _mock) = spawn_unresponsive_mock().await;
        let mut p =
            MpdPlaybackPlugin::with_endpoint(endpoint, short_timeouts());
        p.loaded = true;
        p.subject_emitter = Some(SubjectEmitter::null());

        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());
        let e = p.take_custody(assignment(reporter, 1)).await.unwrap_err();
        assert!(
            matches!(e, PluginError::Transient(_)),
            "expected Transient, got {e:?}"
        );
        assert_eq!(p.active_custody_count(), 0);
        assert_eq!(p.custodies_taken(), 0);
    }

    #[tokio::test]
    async fn course_correct_play_reaches_supervisor() {
        let (mut p, _mock) = loaded_plugin_with_mock(vec![
            ConnBehaviour::Standard,
            ConnBehaviour::HoldAfterWelcome,
        ])
        .await;

        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());
        let handle = p.take_custody(assignment(reporter, 7)).await.unwrap();

        p.course_correct(&handle, correction("play", b"", 99))
            .await
            .unwrap();
        assert_eq!(p.corrections_dispatched(), 1);

        p.release_custody(handle).await.unwrap();
    }

    #[tokio::test]
    async fn course_correct_maps_ack_to_permanent() {
        let (mut p, _mock) = loaded_plugin_with_mock(vec![
            ConnBehaviour::AckOnNth {
                nth: 3,
                code: 2,
                message: "Bad song index".to_string(),
            },
            ConnBehaviour::HoldAfterWelcome,
        ])
        .await;

        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());
        let handle = p.take_custody(assignment(reporter, 11)).await.unwrap();

        let e = p
            .course_correct(&handle, correction("play", b"", 1))
            .await
            .unwrap_err();
        assert!(
            matches!(e, PluginError::Permanent(_)),
            "expected Permanent from Ack, got {e:?}"
        );
        assert_eq!(p.corrections_dispatched(), 1);
        p.release_custody(handle).await.unwrap();
    }

    #[tokio::test]
    async fn release_custody_shuts_down_supervisor_and_removes_from_tracking() {
        let (mut p, _mock) = loaded_plugin_with_mock(vec![
            ConnBehaviour::Standard,
            ConnBehaviour::HoldAfterWelcome,
        ])
        .await;

        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());
        let handle = p.take_custody(assignment(reporter, 5)).await.unwrap();
        assert_eq!(p.active_custody_count(), 1);

        p.release_custody(handle).await.unwrap();
        assert_eq!(p.active_custody_count(), 0);
        assert_eq!(p.custodies_taken(), 1);
    }

    #[tokio::test]
    async fn unload_drains_active_custodies() {
        let (mut p, _mock) = loaded_plugin_with_mock(vec![
            ConnBehaviour::Standard,
            ConnBehaviour::HoldAfterWelcome,
            ConnBehaviour::Standard,
            ConnBehaviour::HoldAfterWelcome,
        ])
        .await;

        let reporter_a: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());
        let reporter_b: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());

        let _h1 = p.take_custody(assignment(reporter_a, 100)).await.unwrap();
        let _h2 = p.take_custody(assignment(reporter_b, 200)).await.unwrap();
        assert_eq!(p.active_custody_count(), 2);

        p.unload().await.unwrap();
        assert_eq!(p.active_custody_count(), 0);
        assert!(matches!(
            p.health_check().await.status,
            HealthStatus::Unhealthy
        ));
    }

    // ===== Phase 3.4: subject emission through the warden =====

    #[tokio::test]
    async fn take_custody_rejects_when_subject_emitter_not_initialised() {
        // Simulate the path where loaded=true has been set
        // manually (e.g. by pre-3.4 legacy test code) but
        // subject_emitter was not populated. Defense-in-depth
        // gate should catch this.
        let (endpoint, _mock) = spawn_mock_mpd(vec![
            ConnBehaviour::Standard,
            ConnBehaviour::HoldAfterWelcome,
        ])
        .await;
        let mut p =
            MpdPlaybackPlugin::with_endpoint(endpoint, short_timeouts());
        p.loaded = true;
        // subject_emitter is intentionally left as None.

        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());
        let e = p.take_custody(assignment(reporter, 1)).await.unwrap_err();
        match e {
            PluginError::Permanent(msg) => {
                assert!(
                    msg.contains("subject emitter"),
                    "error should mention the emitter gate, got {msg:?}"
                );
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
        assert_eq!(p.active_custody_count(), 0);
    }

    #[tokio::test]
    async fn take_custody_with_populated_song_emits_subjects() {
        let (endpoint, mock_task) = spawn_mock_mpd(vec![
            ConnBehaviour::StandardWithSong {
                file: "library/a/b/01.flac".to_string(),
                title: "Track".to_string(),
                artist: "Artist".to_string(),
                album: "Album".to_string(),
            },
            ConnBehaviour::HoldAfterWelcome,
        ])
        .await;
        let mut p =
            MpdPlaybackPlugin::with_endpoint(endpoint, short_timeouts());
        p.loaded = true;

        // Equip a capturing emitter so this test can verify the
        // announcement actually reached the SDK surface.
        let (subjects, relations, emitter) = capturing_emitter();
        p.subject_emitter = Some(emitter);

        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());
        let handle = p.take_custody(assignment(reporter, 77)).await.unwrap();

        // Initial emission from spawn's emit_initial_report.
        assert_eq!(subjects.count(), 2, "track + album at take-custody");
        assert_eq!(relations.count(), 1, "album_of at take-custody");

        let track = subjects.at(0).unwrap();
        assert_eq!(track.subject_type, "track");
        assert_eq!(track.addressings[0].scheme, "mpd-path");
        assert_eq!(track.addressings[0].value, "library/a/b/01.flac");

        let album = subjects.at(1).unwrap();
        assert_eq!(album.subject_type, "album");
        assert_eq!(album.addressings[0].scheme, "mpd-album");
        assert_eq!(album.addressings[0].value, "Artist|Album");

        let rel = relations.at(0).unwrap();
        assert_eq!(rel.predicate, "album_of");
        assert_eq!(rel.source.value, "library/a/b/01.flac");
        assert_eq!(rel.target.value, "Artist|Album");

        p.release_custody(handle).await.unwrap();
        drop(mock_task);
    }

    #[tokio::test]
    async fn take_custody_with_empty_song_emits_no_subjects() {
        let (mut p, _mock) = loaded_plugin_with_mock(vec![
            ConnBehaviour::Standard,
            ConnBehaviour::HoldAfterWelcome,
        ])
        .await;

        // Replace the null emitter the helper installed with a
        // capturing one so we can verify nothing was announced.
        let (subjects, relations, emitter) = capturing_emitter();
        p.subject_emitter = Some(emitter);

        let reporter: Arc<dyn CustodyStateReporter> =
            Arc::new(CapturingReporter::default());
        let handle = p.take_custody(assignment(reporter, 9)).await.unwrap();

        // Standard mock returns empty currentsong; no subjects.
        assert_eq!(subjects.count(), 0);
        assert_eq!(relations.count(), 0);

        p.release_custody(handle).await.unwrap();
    }
}
