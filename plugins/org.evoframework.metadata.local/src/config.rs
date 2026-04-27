//! Operator configuration: library roots for MPD `file` paths (same convention as
//! `org.evoframework.artwork.local`) and `metadata` response profile
//! (see `docs/METADATA_QUERY_V1.md` — **Metadata profiles**).

use std::path::PathBuf;

/// Which field groups appear in a successful `metadata.query` body.
/// Default is standard (small, UI-friendly). Extended adds nested / technical / unmapped data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum MetadataProfile {
    /// Light payload: core flat tag fields, duration, and simple flags. No nested groups,
    /// no technical `file` block, no unmapped `extras`, no `lyrics`.
    #[default]
    Standard,
    /// Full readout: all mapped groups, container technicals, vendor `extras`, and lyrics
    /// (as supported by the tag and caps).
    Extended,
}

impl MetadataProfile {
    pub(crate) const fn as_wire(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Extended => "extended",
        }
    }
}

/// Parsed `plugins.d/org.evoframework.metadata.local.toml` subset.
#[derive(Debug, Clone)]
pub(crate) struct PluginConfig {
    /// Absolute directory prefixes for relative `mpd-path` values.
    pub(crate) library_roots: Vec<PathBuf>,
    /// Filters successful response shape (see `MetadataProfile`).
    pub(crate) metadata_profile: MetadataProfile,
}

impl PluginConfig {
    /// Defaults: no library roots; only absolute MPD file paths resolve; standard profile.
    pub(crate) fn defaults() -> Self {
        Self {
            library_roots: Vec::new(),
            metadata_profile: MetadataProfile::default(),
        }
    }

    /// Merge operator table. Unknown top-level keys are ignored with a warning.
    pub(crate) fn from_toml_table(
        table: &toml::Table,
    ) -> Result<Self, ConfigError> {
        for key in table.keys() {
            if key.as_str() != "library" && key.as_str() != "metadata" {
                tracing::warn!(
                    plugin = crate::PLUGIN_NAME,
                    key = key.as_str(),
                    "unknown top-level config key; ignored"
                );
            }
        }
        let library_roots = match table.get("library") {
            None => Vec::new(),
            Some(toml::Value::Table(t)) => parse_library_roots(t)?,
            other => {
                return Err(ConfigError {
                    key: "library".into(),
                    message: format!("expected a table, got {other:?}"),
                });
            }
        };
        let metadata_profile = match table.get("metadata") {
            None => MetadataProfile::default(),
            Some(toml::Value::Table(t)) => parse_metadata_profile(t)?,
            other => {
                return Err(ConfigError {
                    key: "metadata".into(),
                    message: format!("expected a table, got {other:?}"),
                });
            }
        };
        Ok(Self {
            library_roots,
            metadata_profile,
        })
    }
}

fn parse_metadata_profile(
    table: &toml::Table,
) -> Result<MetadataProfile, ConfigError> {
    for k in table.keys() {
        if k.as_str() != "profile" {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                key = k.as_str(),
                "unknown [metadata] key; ignored"
            );
        }
    }
    let s = table
        .get("profile")
        .and_then(toml::Value::as_str)
        .unwrap_or(MetadataProfile::default().as_wire());
    let s = s.trim();
    if s.is_empty() {
        return Ok(MetadataProfile::default());
    }
    match s.to_ascii_lowercase().as_str() {
        "standard" => Ok(MetadataProfile::Standard),
        "extended" => Ok(MetadataProfile::Extended),
        _ => Err(ConfigError {
            key: "metadata.profile".into(),
            message: format!(
                "expected \"standard\" or \"extended\", got {s:?}"
            ),
        }),
    }
}

fn parse_library_roots(
    table: &toml::Table,
) -> Result<Vec<PathBuf>, ConfigError> {
    for k in table.keys() {
        if k.as_str() != "root" && k.as_str() != "roots" {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                key = k.as_str(),
                "unknown [library] key; ignored"
            );
        }
    }

    let mut out: Vec<PathBuf> = Vec::new();
    if let Some(toml::Value::Array(roots)) = table.get("roots") {
        for (i, v) in roots.iter().enumerate() {
            let s = v.as_str().ok_or_else(|| ConfigError {
                key: format!("library.roots[{i}]"),
                message: "expected a string path".to_string(),
            })?;
            let p = PathBuf::from(s);
            if !p.is_absolute() {
                return Err(ConfigError {
                    key: format!("library.roots[{i}]"),
                    message: "library root must be an absolute path"
                        .to_string(),
                });
            }
            if !p.as_os_str().is_empty() {
                out.push(p);
            }
        }
    }
    if let Some(toml::Value::String(s)) = table.get("root") {
        let p = PathBuf::from(s);
        if !p.is_absolute() {
            return Err(ConfigError {
                key: "library.root".into(),
                message: "library root must be an absolute path".to_string(),
            });
        }
        if !p.as_os_str().is_empty() {
            out.push(p);
        }
    }
    Ok(out)
}

/// Invalid operator configuration.
#[derive(Debug, thiserror::Error)]
pub(crate) struct ConfigError {
    key: String,
    message: String,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.key, self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_table() {
        let t: toml::Table = "".parse().unwrap();
        let c = PluginConfig::from_toml_table(&t).unwrap();
        assert!(c.library_roots.is_empty());
        assert_eq!(c.metadata_profile, MetadataProfile::Standard);
    }

    #[test]
    fn roots_list() {
        let t: toml::Table = r#"
            [library]
            roots = ["/a/music", "/b/usb"]
        "#
        .parse()
        .unwrap();
        let c = PluginConfig::from_toml_table(&t).unwrap();
        assert_eq!(c.library_roots.len(), 2);
        assert_eq!(c.library_roots[0], PathBuf::from("/a/music"));
        assert_eq!(c.metadata_profile, MetadataProfile::Standard);
    }

    #[test]
    fn metadata_profile_extended() {
        let t: toml::Table = r#"
            [metadata]
            profile = "extended"
        "#
        .parse()
        .unwrap();
        let c = PluginConfig::from_toml_table(&t).unwrap();
        assert_eq!(c.metadata_profile, MetadataProfile::Extended);
    }

    #[test]
    fn metadata_profile_rejects_garbage() {
        let t: toml::Table = r#"
            [metadata]
            profile = "audiophile"
        "#
        .parse()
        .unwrap();
        let e = PluginConfig::from_toml_table(&t).unwrap_err();
        assert_eq!(e.key, "metadata.profile");
    }
}
