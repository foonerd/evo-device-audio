// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Just a Nerd

//! The closed request grammar. The caller supplies a
//! component name, a version and a channel value, and nothing else. The
//! same grammar parses the unit instance token the worker is started with,
//! so a token that did not come through the wrapper is refused the same way.

use crate::error::{Res, UpdError};

/// The host-files piece's component name.
pub const HOST_COMPONENT: &str = "evo-host-files";

/// An update channel: `alpha` is the plane's `dev`, `test` is `test`, `production` is `prod`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// The plane's `dev` channel.
    Alpha,
    /// The plane's `test` channel.
    Test,
    /// The plane's `prod` channel.
    Production,
}

impl Channel {
    /// Parse `alpha`, `test` or `production`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "alpha" => Some(Self::Alpha),
            "test" => Some(Self::Test),
            "production" => Some(Self::Production),
            _ => None,
        }
    }

    /// The operator-facing name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Alpha => "alpha",
            Self::Test => "test",
            Self::Production => "production",
        }
    }

    /// The artefacts-plane channel directory suffix (`evo-audio-<this>`).
    pub fn plane(self) -> &'static str {
        match self {
            Self::Alpha => "dev",
            Self::Test => "test",
            Self::Production => "prod",
        }
    }
}

/// One request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Check the recorded channel's registry.
    Check,
    /// Apply one offered component at one version.
    Apply {
        /// A plugin name or [`HOST_COMPONENT`].
        component: String,
        /// A dotted numeric version, or the host-files release.
        version: String,
    },
    /// Record a channel.
    Channel(Channel),
}

/// Is this a component name the grammar accepts?
pub fn valid_component(s: &str) -> bool {
    if s == HOST_COMPONENT {
        return true;
    }
    let Some(rest) = s.strip_prefix("org.evoframework.") else {
        return false;
    };
    (1..=80).contains(&rest.len())
        && rest.bytes().all(|c| {
            c.is_ascii_lowercase()
                || c.is_ascii_digit()
                || c == b'.'
                || c == b'-'
        })
        && !rest.contains("..")
        && !rest.starts_with(['.', '-'])
        && !rest.ends_with(['.', '-'])
}

/// Is this a version the grammar accepts? Dotted digits only.
pub fn valid_version(s: &str) -> bool {
    crate::version::Dotted::parse(s).is_some()
}

/// Parse the wrapper's argument list.
pub fn parse_args(args: &[String]) -> Res<Request> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        ["check"] => Ok(Request::Check),
        ["apply", component, version] => {
            if !valid_component(component) {
                return Err(UpdError::new("bad_component", "component not accepted"));
            }
            if !valid_version(version) {
                return Err(UpdError::new("bad_version", "version not accepted"));
            }
            Ok(Request::Apply {
                component: (*component).to_string(),
                version: (*version).to_string(),
            })
        }
        ["channel", value] => Channel::parse(value)
            .map(Request::Channel)
            .ok_or_else(|| UpdError::new("bad_channel", "channel not accepted")),
        _ => Err(UpdError::new(
            "bad_request",
            "expected: check | apply <component> <version> | channel <alpha|test|production>",
        )),
    }
}

/// The unit instance token for a request. `_` separates fields; the
/// grammar allows no `_` inside any of them.
pub fn token(req: &Request) -> String {
    match req {
        Request::Check => "check".to_string(),
        Request::Apply { component, version } => {
            format!("apply_{component}_{version}")
        }
        Request::Channel(c) => format!("channel_{}", c.name()),
    }
}

/// Parse a unit instance token back to a request, applying the same
/// grammar as [`parse_args`].
pub fn parse_token(t: &str) -> Res<Request> {
    if t.len() > 200 {
        return Err(UpdError::new("bad_request", "token too long"));
    }
    let parts: Vec<&str> = t.split('_').collect();
    let args: Vec<String> = match parts.as_slice() {
        ["check"] => vec!["check".into()],
        ["apply", c, v] => vec!["apply".into(), (*c).into(), (*v).into()],
        ["channel", v] => vec!["channel".into(), (*v).into()],
        _ => return Err(UpdError::new("bad_request", "token not accepted")),
    };
    parse_args(&args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(a: &[&str]) -> Res<Request> {
        parse_args(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn the_three_requests_parse() {
        assert_eq!(p(&["check"]), Ok(Request::Check));
        assert_eq!(
            p(&["apply", "org.evoframework.system.info", "0.1.14"]),
            Ok(Request::Apply {
                component: "org.evoframework.system.info".into(),
                version: "0.1.14".into()
            })
        );
        assert_eq!(
            p(&["apply", HOST_COMPONENT, "0.1.13.3"]).map(|_| ()),
            Ok(())
        );
        assert_eq!(
            p(&["channel", "test"]),
            Ok(Request::Channel(Channel::Test))
        );
    }

    #[test]
    fn anything_else_is_refused_before_any_action() {
        let bad: &[&[&str]] = &[
            &[],
            &["check", "now"],
            &["apply"],
            &["apply", "org.evoframework.x"],
            &["apply", "org.evoframework.x", "1.0", "extra"],
            &["apply", "../etc/passwd", "1.0"],
            &["apply", "org.evoframework.", "1.0"],
            &["apply", "org.evoframework.a..b", "1.0"],
            &["apply", "org.evoframework.a/b", "1.0"],
            &["apply", "org.evoframework.a b", "1.0"],
            &["apply", "org.evoframework.A", "1.0"],
            &["apply", "org.evoframework.a_b", "1.0"],
            &["apply", "com.other.plugin", "1.0"],
            &["apply", "org.evoframework.x", "1.0;rm"],
            &["apply", "org.evoframework.x", "latest"],
            &["apply", "org.evoframework.x", "1_0"],
            &["channel", "nightly"],
            &["channel", ""],
            &["--worker", "x"],
        ];
        for a in bad {
            assert!(p(a).is_err(), "{a:?} must be refused");
        }
    }

    #[test]
    fn a_token_round_trips_and_a_forged_one_is_refused() {
        for r in [
            Request::Check,
            Request::Channel(Channel::Alpha),
            Request::Apply {
                component: "org.evoframework.network.smb-server".into(),
                version: "0.1.13.2".into(),
            },
        ] {
            assert_eq!(parse_token(&token(&r)), Ok(r));
        }
        for t in [
            "",
            "apply",
            "apply_org.evoframework.x",
            "apply_/etc/passwd_1.0",
            "apply_org.evoframework.x_1.0_extra",
            "shell_id",
            "channel_nightly",
        ] {
            assert!(parse_token(t).is_err(), "{t:?}");
        }
    }
}
