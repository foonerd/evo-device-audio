// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! # org-evoframework-identity-locale
//!
//! Stocks the `system.locale` shelf with one setter,
//! `language.set { v: 1, language }`, for the operator's language.
//!
//! - It answers `{ v: 1, language }`, the language now in force, as
//!   a BCP 47 tag.
//! - A tag applies only when the glass has a catalogue for it; the
//!   glass ships one, `en`. Tags compare without regard to case,
//!   and the catalogue's own spelling is stored.
//! - Any other tag is not applied and writes nothing. The answer is
//!   the language still in force, not an error.
//! - Before any set, and when no language has been stored, the
//!   language in force is `en`.
//! - The stored language lives in `language.toml` in the plugin's
//!   own state directory and survives a restart. A stored tag the
//!   glass has no catalogue for counts as no stored language.
//!
//! The verb carries no capability gate: changing the language asks
//! for no password.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::manual_async_fn)]

use std::future::Future;
use std::path::{Path, PathBuf};

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
pub const PLUGIN_NAME: &str = "org.evoframework.identity.locale";

/// Set the operator's language.
pub const VERB_LANGUAGE_SET: &str = "language.set";

/// The languages the glass has a catalogue for.
pub const CATALOGUES: &[&str] = &["en"];

/// The language in force before any set, and when none is stored.
pub const DEFAULT_LANGUAGE: &str = "en";

/// The stored language's file in the plugin's state directory.
pub const STATE_FILENAME: &str = "language.toml";

/// Wire payload version.
const PAYLOAD_VERSION: u32 = 1;

/// Parse the embedded [`Manifest`].
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML).expect(
        "org-evoframework-identity-locale: embedded manifest must parse",
    )
}

fn plugin_crate_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is valid semver")
}

/// The plugin singleton.
pub struct LocalePlugin {
    loaded: bool,
    /// The languages a set may apply, in the catalogue's spelling.
    catalogues: Vec<String>,
    /// Where the stored language lives; set at load from the state
    /// directory unless a caller chose one.
    state_path: Option<PathBuf>,
    /// The language in force. Held across a set so two sets do not
    /// interleave their writes.
    language: tokio::sync::Mutex<String>,
}

impl LocalePlugin {
    /// New instance with the glass's catalogues; call
    /// [`Plugin::load`] before handling requests.
    pub fn new() -> Self {
        Self {
            loaded: false,
            catalogues: CATALOGUES.iter().map(|c| (*c).to_string()).collect(),
            state_path: None,
            language: tokio::sync::Mutex::new(DEFAULT_LANGUAGE.to_string()),
        }
    }

    /// Store the language at this path rather than in the state
    /// directory load is given.
    pub fn with_state_path(mut self, path: PathBuf) -> Self {
        self.state_path = Some(path);
        self
    }

    /// The catalogue's spelling of a tag the glass has a catalogue
    /// for, or `None`.
    fn catalogue_tag(&self, tag: &str) -> Option<&str> {
        let tag = tag.trim();
        self.catalogues
            .iter()
            .find(|c| c.eq_ignore_ascii_case(tag))
            .map(String::as_str)
    }

    /// The stored language, if the file holds one the glass has a
    /// catalogue for; otherwise the default.
    async fn stored(&self) -> String {
        let Some(path) = self.state_path.as_deref() else {
            return DEFAULT_LANGUAGE.to_string();
        };
        let stored = tokio::fs::read_to_string(path)
            .await
            .ok()
            .and_then(|body| body.parse::<toml::Table>().ok())
            .and_then(|t| {
                t.get("language")
                    .and_then(|v| v.as_str().map(str::to_string))
            });
        stored
            .as_deref()
            .and_then(|tag| self.catalogue_tag(tag))
            .unwrap_or(DEFAULT_LANGUAGE)
            .to_string()
    }

    /// Make the stored language the language in force. Load calls
    /// this once the state path is known.
    async fn restore(&mut self) {
        let stored = self.stored().await;
        *self.language.get_mut() = stored;
    }

    async fn handle_set(&self, req: &Request) -> Result<Response, PluginError> {
        let payload: SetPayload = serde_json::from_slice(&req.payload).map_err(|e| {
            PluginError::Permanent(format!(
                "{VERB_LANGUAGE_SET} payload is not valid JSON for the expected shape: {e}"
            ))
        })?;
        if payload.v != PAYLOAD_VERSION {
            return Err(PluginError::Permanent(format!(
                "{VERB_LANGUAGE_SET} payload version {} unsupported; expected {PAYLOAD_VERSION}",
                payload.v
            )));
        }
        let mut in_force = self.language.lock().await;
        match self.catalogue_tag(&payload.language) {
            Some(tag) => {
                let tag = tag.to_string();
                if let Some(path) = self.state_path.as_deref() {
                    write_language(path, &tag).await.map_err(|e| {
                        PluginError::Transient(format!(
                            "{VERB_LANGUAGE_SET}: could not store {tag:?}: {e}"
                        ))
                    })?;
                }
                tracing::info!(plugin = PLUGIN_NAME, language = %tag, "language set");
                *in_force = tag;
            }
            None => tracing::info!(
                plugin = PLUGIN_NAME,
                asked = %payload.language,
                in_force = %in_force,
                "language not applied: the glass has no catalogue for it"
            ),
        }
        let body =
            serde_json::json!({ "v": PAYLOAD_VERSION, "language": *in_force });
        let bytes = serde_json::to_vec(&body).map_err(|e| {
            PluginError::Permanent(format!(
                "{VERB_LANGUAGE_SET} response encode failed: {e}"
            ))
        })?;
        Ok(Response::for_request(req, bytes))
    }
}

impl Default for LocalePlugin {
    fn default() -> Self {
        Self::new()
    }
}

/// Store a language: a temporary file beside the target, then a
/// rename.
async fn write_language(path: &Path, tag: &str) -> std::io::Result<()> {
    let mut table = toml::Table::new();
    table.insert("language".to_string(), toml::Value::String(tag.to_string()));
    let body = toml::to_string(&table).map_err(std::io::Error::other)?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    tokio::fs::write(&tmp, body.as_bytes()).await?;
    tokio::fs::rename(&tmp, path).await
}

#[derive(Debug, Deserialize)]
struct SetPayload {
    #[serde(default = "default_payload_version")]
    v: u32,
    language: String,
}

fn default_payload_version() -> u32 {
    PAYLOAD_VERSION
}

impl Plugin for LocalePlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: PLUGIN_NAME.to_string(),
                    version: plugin_crate_version(),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: vec![VERB_LANGUAGE_SET.to_string()],
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
            if self.state_path.is_none() {
                self.state_path = Some(ctx.state_dir.join(STATE_FILENAME));
            }
            self.restore().await;
            self.loaded = true;
            tracing::info!(
                plugin = PLUGIN_NAME,
                language = %self.language.get_mut(),
                "identity.locale plugin loaded"
            );
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
                HealthReport::unhealthy("identity.locale plugin not loaded")
            }
        }
    }
}

impl Respondent for LocalePlugin {
    fn handle_request<'a>(
        &'a self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "identity.locale plugin not loaded".to_string(),
                ));
            }
            if req.is_past_deadline() {
                return Err(PluginError::Transient(
                    "request deadline already expired".to_string(),
                ));
            }
            match req.request_type.as_str() {
                VERB_LANGUAGE_SET => self.handle_set(req).await,
                other => Err(PluginError::Permanent(format!(
                    "identity.locale: unknown verb {other:?}"
                ))),
            }
        }
    }
}

#[cfg(test)]
mod tests;
