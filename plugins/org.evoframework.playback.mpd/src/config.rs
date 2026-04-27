//! Operator-supplied plugin configuration.
//!
//! The steward reads `/etc/evo/plugins.d/org.evoframework.playback.mpd.toml`
//! (if present) and delivers the parsed table via
//! [`LoadContext::config`] when it calls [`Plugin::load`]. This module
//! turns that table into a typed [`PluginConfig`] with validated
//! endpoint and timeout values, or a [`ConfigError`] naming exactly
//! which field was wrong.
//!
//! [`LoadContext::config`]: evo_plugin_sdk::contract::LoadContext::config
//! [`Plugin::load`]: evo_plugin_sdk::contract::Plugin::load
//!
//! # Schema
//!
//! All fields are optional; missing sections or fields use the
//! plugin's hardcoded defaults. An empty table is a valid
//! (default-only) config.
//!
//! ```toml
//! [endpoint]
//! type = "tcp"           # "tcp" or "unix"; default "tcp"
//! host = "127.0.0.1"     # for tcp; default "127.0.0.1"
//! port = 6600            # for tcp; default 6600
//! path = "/run/mpd/socket"  # for unix (required when type = "unix")
//!
//! [timeouts]
//! connect_ms = 5000      # default 5000; range 1-60000
//! welcome_ms = 2000      # default 2000; range 1-60000
//! command_ms = 3000      # default 3000; range 1-300000
//! ```
//!
//! # Validation
//!
//! Strict on types and bounds, permissive on unknown keys.
//! Wrong-typed fields and out-of-range values return
//! [`ConfigError`]. Unknown keys are logged at warn level and
//! ignored, so new config fields added in later phases do not
//! break older operator configs.

use std::path::PathBuf;
use std::time::Duration;

use crate::mpd::{ConnectTimeouts, MpdEndpoint};

/// Default MPD host (mirrors [`crate::DEFAULT_MPD_HOST`], repeated
/// here so `config` has no top-level dependency on the consumer).
const DEFAULT_MPD_HOST: &str = "127.0.0.1";
/// Default MPD TCP port.
const DEFAULT_MPD_PORT: u16 = 6600;

/// Minimum timeout value (milliseconds). Zero is rejected because a
/// zero-budget timeout never succeeds and silently produces only
/// timeout errors.
const TIMEOUT_MIN_MS: u64 = 1;
/// Upper bound on `connect_ms`: 60 seconds. TCP connect to a
/// working local daemon is sub-millisecond; to a dead daemon on a
/// cold network, a minute is generous.
const CONNECT_TIMEOUT_MAX_MS: u64 = 60_000;
/// Upper bound on `welcome_ms`: 60 seconds. The welcome banner is
/// the first line MPD sends; anything approaching 60s means the
/// daemon is broken.
const WELCOME_TIMEOUT_MAX_MS: u64 = 60_000;
/// Upper bound on `command_ms`: 5 minutes. MPD commands against
/// very large libraries (tens of thousands of tracks, `lsinfo` on
/// a root) can take several seconds; 5 minutes is a generous
/// ceiling for any single command.
const COMMAND_TIMEOUT_MAX_MS: u64 = 300_000;

/// Validated plugin configuration.
///
/// Constructed from [`PluginConfig::from_toml_table`] or
/// [`PluginConfig::defaults`]. Carries the two pieces of state the
/// plugin actually uses at runtime: the MPD endpoint and the
/// connection timeouts.
#[derive(Debug)]
pub(crate) struct PluginConfig {
    pub(crate) endpoint: MpdEndpoint,
    pub(crate) timeouts: ConnectTimeouts,
}

impl PluginConfig {
    /// The configuration [`crate::MpdPlaybackPlugin::new`] applies
    /// when no operator config is present. Mirrors the hardcoded
    /// defaults in `lib.rs`.
    pub(crate) fn defaults() -> Self {
        Self {
            endpoint: MpdEndpoint::tcp(DEFAULT_MPD_HOST, DEFAULT_MPD_PORT)
                .expect("default host is non-empty"),
            timeouts: ConnectTimeouts::default(),
        }
    }

    /// Parse a [`toml::Table`] into a validated [`PluginConfig`].
    ///
    /// Empty table yields defaults. Unknown top-level keys are
    /// logged at warn level and ignored. Known keys are
    /// type-checked and bounds-checked; any failure returns a
    /// [`ConfigError`] naming the field.
    pub(crate) fn from_toml_table(
        table: &toml::Table,
    ) -> Result<Self, ConfigError> {
        // Warn on unknown top-level keys. Forward-compat: new
        // sections added in later phases will not break older
        // operator configs.
        for key in table.keys() {
            if !matches!(key.as_str(), "endpoint" | "timeouts") {
                tracing::warn!(
                    plugin = crate::PLUGIN_NAME,
                    key = key.as_str(),
                    "unknown top-level config key; ignored"
                );
            }
        }

        let endpoint = parse_endpoint(table.get("endpoint"))?;
        let timeouts = parse_timeouts(table.get("timeouts"))?;

        Ok(Self { endpoint, timeouts })
    }
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

/// Failure modes of [`PluginConfig::from_toml_table`].
///
/// Each variant names the specific field and the specific problem,
/// so operator-visible error messages point directly at the
/// line that needs fixing.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum ConfigError {
    /// `endpoint.type` was neither `"tcp"` nor `"unix"`.
    #[error("endpoint.type must be \"tcp\" or \"unix\", got {0:?}")]
    UnknownEndpointType(String),

    /// A required field for the chosen endpoint type was missing.
    /// Only `unix` has a required field (`path`); `tcp` falls back
    /// to defaults for host and port.
    #[error("endpoint.{field} is required when endpoint.type is \"{endpoint_type}\"")]
    MissingField {
        field: &'static str,
        endpoint_type: &'static str,
    },

    /// A field was present but of the wrong TOML type.
    #[error("{field} must be a {expected}, got {actual}")]
    WrongType {
        field: &'static str,
        expected: &'static str,
        actual: &'static str,
    },

    /// `endpoint.host` was empty or whitespace-only.
    #[error("endpoint.host must not be empty")]
    EmptyHost,

    /// `endpoint.port` was not in the 1-65535 range.
    #[error("endpoint.port must be in 1..=65535, got {0}")]
    PortOutOfRange(i64),

    /// `endpoint.path` was not an absolute path.
    #[error("endpoint.path must be absolute, got {0:?}")]
    RelativePath(String),

    /// A timeout field was outside its allowed range.
    #[error(
        "timeouts.{field} must be in {min}..={max} milliseconds, got {value}"
    )]
    TimeoutOutOfRange {
        field: &'static str,
        value: i64,
        min: u64,
        max: u64,
    },
}

// ----- parse helpers -----

fn parse_endpoint(
    value: Option<&toml::Value>,
) -> Result<MpdEndpoint, ConfigError> {
    let Some(value) = value else {
        return Ok(MpdEndpoint::tcp(DEFAULT_MPD_HOST, DEFAULT_MPD_PORT)
            .expect("default host is non-empty"));
    };

    let table = value.as_table().ok_or(ConfigError::WrongType {
        field: "endpoint",
        expected: "table",
        actual: type_name_of(value),
    })?;

    // Warn on unknown keys within [endpoint].
    for key in table.keys() {
        if !matches!(key.as_str(), "type" | "host" | "port" | "path") {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                key = format!("endpoint.{}", key),
                "unknown config key; ignored"
            );
        }
    }

    let endpoint_type = match table.get("type") {
        Some(v) => v.as_str().ok_or(ConfigError::WrongType {
            field: "endpoint.type",
            expected: "string",
            actual: type_name_of(v),
        })?,
        None => "tcp",
    };

    match endpoint_type {
        "tcp" => {
            let host = match table.get("host") {
                Some(v) => {
                    let s = v.as_str().ok_or(ConfigError::WrongType {
                        field: "endpoint.host",
                        expected: "string",
                        actual: type_name_of(v),
                    })?;
                    if s.trim().is_empty() {
                        return Err(ConfigError::EmptyHost);
                    }
                    s.to_string()
                }
                None => DEFAULT_MPD_HOST.to_string(),
            };

            let port = match table.get("port") {
                Some(v) => {
                    let p = v.as_integer().ok_or(ConfigError::WrongType {
                        field: "endpoint.port",
                        expected: "integer",
                        actual: type_name_of(v),
                    })?;
                    if !(1..=65535).contains(&p) {
                        return Err(ConfigError::PortOutOfRange(p));
                    }
                    p as u16
                }
                None => DEFAULT_MPD_PORT,
            };

            Ok(MpdEndpoint::tcp(host, port).expect("host validated non-empty"))
        }
        "unix" => {
            let path_str = match table.get("path") {
                Some(v) => v.as_str().ok_or(ConfigError::WrongType {
                    field: "endpoint.path",
                    expected: "string",
                    actual: type_name_of(v),
                })?,
                None => {
                    return Err(ConfigError::MissingField {
                        field: "path",
                        endpoint_type: "unix",
                    });
                }
            };

            let path = PathBuf::from(path_str);
            if !path.is_absolute() {
                return Err(ConfigError::RelativePath(path_str.to_string()));
            }

            Ok(MpdEndpoint::unix(path).expect("path validated non-empty"))
        }
        other => Err(ConfigError::UnknownEndpointType(other.to_string())),
    }
}

fn parse_timeouts(
    value: Option<&toml::Value>,
) -> Result<ConnectTimeouts, ConfigError> {
    let Some(value) = value else {
        return Ok(ConnectTimeouts::default());
    };

    let table = value.as_table().ok_or(ConfigError::WrongType {
        field: "timeouts",
        expected: "table",
        actual: type_name_of(value),
    })?;

    // Warn on unknown keys within [timeouts].
    for key in table.keys() {
        if !matches!(key.as_str(), "connect_ms" | "welcome_ms" | "command_ms") {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                key = format!("timeouts.{}", key),
                "unknown config key; ignored"
            );
        }
    }

    let defaults = ConnectTimeouts::default();

    let connect = parse_timeout_ms(
        table.get("connect_ms"),
        "connect_ms",
        CONNECT_TIMEOUT_MAX_MS,
    )?
    .unwrap_or(defaults.connect);

    let welcome = parse_timeout_ms(
        table.get("welcome_ms"),
        "welcome_ms",
        WELCOME_TIMEOUT_MAX_MS,
    )?
    .unwrap_or(defaults.welcome);

    let command = parse_timeout_ms(
        table.get("command_ms"),
        "command_ms",
        COMMAND_TIMEOUT_MAX_MS,
    )?
    .unwrap_or(defaults.command);

    Ok(ConnectTimeouts {
        connect,
        welcome,
        command,
    })
}

fn parse_timeout_ms(
    value: Option<&toml::Value>,
    field: &'static str,
    max_ms: u64,
) -> Result<Option<Duration>, ConfigError> {
    let Some(value) = value else {
        return Ok(None);
    };

    let ms = value.as_integer().ok_or(ConfigError::WrongType {
        field,
        expected: "integer",
        actual: type_name_of(value),
    })?;

    if ms < TIMEOUT_MIN_MS as i64 || ms > max_ms as i64 {
        return Err(ConfigError::TimeoutOutOfRange {
            field,
            value: ms,
            min: TIMEOUT_MIN_MS,
            max: max_ms,
        });
    }

    Ok(Some(Duration::from_millis(ms as u64)))
}

/// Friendly name for a [`toml::Value`] for use in error messages.
fn type_name_of(v: &toml::Value) -> &'static str {
    match v {
        toml::Value::String(_) => "string",
        toml::Value::Integer(_) => "integer",
        toml::Value::Float(_) => "float",
        toml::Value::Boolean(_) => "boolean",
        toml::Value::Datetime(_) => "datetime",
        toml::Value::Array(_) => "array",
        toml::Value::Table(_) => "table",
    }
}

// ----- tests -----

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a config from a TOML source string. Test-only
    /// convenience wrapper over [`PluginConfig::from_toml_table`].
    fn parse(src: &str) -> Result<PluginConfig, ConfigError> {
        let table: toml::Table =
            src.parse().expect("test input must be valid TOML");
        PluginConfig::from_toml_table(&table)
    }

    // ===== acceptance paths =====

    #[test]
    fn defaults_returns_expected_endpoint_and_timeouts() {
        let c = PluginConfig::defaults();
        assert_eq!(c.endpoint, MpdEndpoint::tcp("127.0.0.1", 6600).unwrap());
        let dt = ConnectTimeouts::default();
        assert_eq!(c.timeouts.connect, dt.connect);
        assert_eq!(c.timeouts.welcome, dt.welcome);
        assert_eq!(c.timeouts.command, dt.command);
    }

    #[test]
    fn empty_table_yields_defaults() {
        let c = parse("").unwrap();
        let d = PluginConfig::defaults();
        assert_eq!(c.endpoint, d.endpoint);
        assert_eq!(c.timeouts.connect, d.timeouts.connect);
        assert_eq!(c.timeouts.welcome, d.timeouts.welcome);
        assert_eq!(c.timeouts.command, d.timeouts.command);
    }

    #[test]
    fn tcp_with_all_fields() {
        let c = parse(
            r#"
            [endpoint]
            type = "tcp"
            host = "mpd.example"
            port = 6700
        "#,
        )
        .unwrap();
        assert_eq!(c.endpoint, MpdEndpoint::tcp("mpd.example", 6700).unwrap());
    }

    #[test]
    fn tcp_with_partial_fields_fills_defaults() {
        let c = parse(
            r#"
            [endpoint]
            host = "mpd.example"
        "#,
        )
        .unwrap();
        // type defaults to tcp; port defaults to 6600.
        assert_eq!(c.endpoint, MpdEndpoint::tcp("mpd.example", 6600).unwrap());
    }

    #[test]
    fn tcp_type_is_default_when_type_omitted() {
        let c = parse(
            r#"
            [endpoint]
            port = 7000
        "#,
        )
        .unwrap();
        assert_eq!(c.endpoint, MpdEndpoint::tcp("127.0.0.1", 7000).unwrap());
    }

    #[test]
    fn unix_endpoint_with_absolute_path() {
        let c = parse(
            r#"
            [endpoint]
            type = "unix"
            path = "/run/mpd/socket"
        "#,
        )
        .unwrap();
        assert_eq!(c.endpoint, MpdEndpoint::unix("/run/mpd/socket").unwrap());
    }

    #[test]
    fn custom_timeouts() {
        let c = parse(
            r#"
            [timeouts]
            connect_ms = 1000
            welcome_ms = 500
            command_ms = 2000
        "#,
        )
        .unwrap();
        assert_eq!(c.timeouts.connect, Duration::from_millis(1000));
        assert_eq!(c.timeouts.welcome, Duration::from_millis(500));
        assert_eq!(c.timeouts.command, Duration::from_millis(2000));
    }

    #[test]
    fn timeouts_are_independently_overridable() {
        // Only `welcome_ms` set; connect and command keep defaults.
        let c = parse(
            r#"
            [timeouts]
            welcome_ms = 100
        "#,
        )
        .unwrap();
        let d = ConnectTimeouts::default();
        assert_eq!(c.timeouts.welcome, Duration::from_millis(100));
        assert_eq!(c.timeouts.connect, d.connect);
        assert_eq!(c.timeouts.command, d.command);
    }

    #[test]
    fn max_timeouts_are_accepted() {
        let c = parse(
            r#"
            [timeouts]
            connect_ms = 60000
            welcome_ms = 60000
            command_ms = 300000
        "#,
        )
        .unwrap();
        assert_eq!(c.timeouts.connect, Duration::from_millis(60_000));
        assert_eq!(c.timeouts.welcome, Duration::from_millis(60_000));
        assert_eq!(c.timeouts.command, Duration::from_millis(300_000));
    }

    #[test]
    fn min_timeout_is_accepted() {
        let c = parse(
            r#"
            [timeouts]
            connect_ms = 1
        "#,
        )
        .unwrap();
        assert_eq!(c.timeouts.connect, Duration::from_millis(1));
    }

    // ===== forward-compat =====

    #[test]
    fn unknown_top_level_keys_are_ignored() {
        let c = parse(
            r#"
            unknown_section = "some value"
            [endpoint]
            host = "mpd.example"
        "#,
        )
        .unwrap();
        assert_eq!(c.endpoint, MpdEndpoint::tcp("mpd.example", 6600).unwrap());
    }

    #[test]
    fn unknown_endpoint_keys_are_ignored() {
        let c = parse(
            r#"
            [endpoint]
            host = "mpd.example"
            future_field = true
        "#,
        )
        .unwrap();
        assert_eq!(c.endpoint, MpdEndpoint::tcp("mpd.example", 6600).unwrap());
    }

    #[test]
    fn unknown_timeouts_keys_are_ignored() {
        let c = parse(
            r#"
            [timeouts]
            connect_ms = 1000
            future_budget_ms = 999
        "#,
        )
        .unwrap();
        assert_eq!(c.timeouts.connect, Duration::from_millis(1000));
    }

    // ===== rejection paths =====

    #[test]
    fn rejects_unknown_endpoint_type() {
        let e = parse(
            r#"
            [endpoint]
            type = "websocket"
        "#,
        )
        .unwrap_err();
        assert_eq!(
            e,
            ConfigError::UnknownEndpointType("websocket".to_string())
        );
    }

    #[test]
    fn rejects_unix_without_path() {
        let e = parse(
            r#"
            [endpoint]
            type = "unix"
        "#,
        )
        .unwrap_err();
        assert_eq!(
            e,
            ConfigError::MissingField {
                field: "path",
                endpoint_type: "unix"
            }
        );
    }

    #[test]
    fn rejects_empty_host() {
        let e = parse(
            r#"
            [endpoint]
            host = ""
        "#,
        )
        .unwrap_err();
        assert_eq!(e, ConfigError::EmptyHost);
    }

    #[test]
    fn rejects_whitespace_host() {
        let e = parse(
            r#"
            [endpoint]
            host = "   "
        "#,
        )
        .unwrap_err();
        assert_eq!(e, ConfigError::EmptyHost);
    }

    #[test]
    fn rejects_port_zero() {
        let e = parse(
            r#"
            [endpoint]
            port = 0
        "#,
        )
        .unwrap_err();
        assert_eq!(e, ConfigError::PortOutOfRange(0));
    }

    #[test]
    fn rejects_port_above_u16_range() {
        let e = parse(
            r#"
            [endpoint]
            port = 70000
        "#,
        )
        .unwrap_err();
        assert_eq!(e, ConfigError::PortOutOfRange(70000));
    }

    #[test]
    fn rejects_negative_port() {
        let e = parse(
            r#"
            [endpoint]
            port = -1
        "#,
        )
        .unwrap_err();
        assert_eq!(e, ConfigError::PortOutOfRange(-1));
    }

    #[test]
    fn rejects_relative_unix_path() {
        let e = parse(
            r#"
            [endpoint]
            type = "unix"
            path = "run/mpd/socket"
        "#,
        )
        .unwrap_err();
        assert_eq!(e, ConfigError::RelativePath("run/mpd/socket".to_string()));
    }

    #[test]
    fn rejects_zero_connect_timeout() {
        let e = parse(
            r#"
            [timeouts]
            connect_ms = 0
        "#,
        )
        .unwrap_err();
        assert!(matches!(
            e,
            ConfigError::TimeoutOutOfRange {
                field: "connect_ms",
                value: 0,
                ..
            }
        ));
    }

    #[test]
    fn rejects_negative_timeout() {
        let e = parse(
            r#"
            [timeouts]
            welcome_ms = -500
        "#,
        )
        .unwrap_err();
        assert!(matches!(
            e,
            ConfigError::TimeoutOutOfRange {
                field: "welcome_ms",
                value: -500,
                ..
            }
        ));
    }

    #[test]
    fn rejects_connect_timeout_above_cap() {
        let e = parse(
            r#"
            [timeouts]
            connect_ms = 60001
        "#,
        )
        .unwrap_err();
        assert!(matches!(
            e,
            ConfigError::TimeoutOutOfRange {
                field: "connect_ms",
                value: 60001,
                max: 60_000,
                ..
            }
        ));
    }

    #[test]
    fn rejects_command_timeout_above_cap() {
        let e = parse(
            r#"
            [timeouts]
            command_ms = 300001
        "#,
        )
        .unwrap_err();
        assert!(matches!(
            e,
            ConfigError::TimeoutOutOfRange {
                field: "command_ms",
                value: 300001,
                max: 300_000,
                ..
            }
        ));
    }

    // ===== wrong-type rejections =====

    #[test]
    fn rejects_non_table_endpoint() {
        let e = parse(r#"endpoint = "tcp://127.0.0.1:6600""#).unwrap_err();
        assert!(matches!(
            e,
            ConfigError::WrongType {
                field: "endpoint",
                expected: "table",
                actual: "string",
            }
        ));
    }

    #[test]
    fn rejects_non_string_host() {
        let e = parse(
            r#"
            [endpoint]
            host = 127
        "#,
        )
        .unwrap_err();
        assert!(matches!(
            e,
            ConfigError::WrongType {
                field: "endpoint.host",
                expected: "string",
                actual: "integer",
            }
        ));
    }

    #[test]
    fn rejects_non_integer_port() {
        let e = parse(
            r#"
            [endpoint]
            port = "6600"
        "#,
        )
        .unwrap_err();
        assert!(matches!(
            e,
            ConfigError::WrongType {
                field: "endpoint.port",
                expected: "integer",
                actual: "string",
            }
        ));
    }

    #[test]
    fn rejects_non_integer_timeout() {
        let e = parse(
            r#"
            [timeouts]
            connect_ms = "fast"
        "#,
        )
        .unwrap_err();
        assert!(matches!(
            e,
            ConfigError::WrongType {
                field: "connect_ms",
                expected: "integer",
                actual: "string",
            }
        ));
    }

    #[test]
    fn rejects_non_string_endpoint_type() {
        let e = parse(
            r#"
            [endpoint]
            type = 42
        "#,
        )
        .unwrap_err();
        assert!(matches!(
            e,
            ConfigError::WrongType {
                field: "endpoint.type",
                expected: "string",
                actual: "integer",
            }
        ));
    }

    #[test]
    fn rejects_non_string_unix_path() {
        let e = parse(
            r#"
            [endpoint]
            type = "unix"
            path = 42
        "#,
        )
        .unwrap_err();
        assert!(matches!(
            e,
            ConfigError::WrongType {
                field: "endpoint.path",
                expected: "string",
                actual: "integer",
            }
        ));
    }

    #[test]
    fn rejects_non_table_timeouts() {
        let e = parse(r#"timeouts = 5000"#).unwrap_err();
        assert!(matches!(
            e,
            ConfigError::WrongType {
                field: "timeouts",
                expected: "table",
                actual: "integer",
            }
        ));
    }
}
