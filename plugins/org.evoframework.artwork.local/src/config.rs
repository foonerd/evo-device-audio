//! Operator configuration: library roots to resolve MPD `file` relative paths
//! against local storage, matching paths reported by
//! `org.evoframework.playback.mpd` (see `LoadContext::config`).

use std::path::PathBuf;

/// Parsed `/etc/evo/plugins.d/org.evoframework.artwork.local.toml` subset.
#[derive(Debug, Clone)]
pub(crate) struct PluginConfig {
    /// Absolute directory prefixes tried in order for relative
    /// `mpd-path` `file` values. Empty means only absolute
    /// `file` paths can be opened.
    pub(crate) library_roots: Vec<PathBuf>,
}

impl PluginConfig {
    /// Defaults: no library roots; only absolute MPD file paths work.
    pub(crate) fn defaults() -> Self {
        Self {
            library_roots: Vec::new(),
        }
    }

    /// Merge operator table. Unknown keys at `[library]` are ignored with a
    /// warning; invalid entries return [`ConfigError`].
    pub(crate) fn from_toml_table(
        table: &toml::Table,
    ) -> Result<Self, ConfigError> {
        for key in table.keys() {
            if key.as_str() != "library" {
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
        Ok(Self { library_roots })
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
    }
}
