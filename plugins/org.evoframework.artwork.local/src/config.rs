//! Operator configuration: library roots to resolve MPD `file` relative paths
//! against local storage, matching paths reported by
//! `org.evoframework.playback.mpd` (see `LoadContext::config`).
//!
//! The primary truth path is MPD's own `music_directory` directive
//! parsed from `/etc/mpd.conf` at plugin load. Operator-supplied
//! `[library] roots` config entries remain supported as additive
//! overrides for unusual layouts (mount-point aliases, multi-root
//! topologies, etc.). The auto-derived MPD root is appended FIRST
//! so the first cascade hit is the canonical source — no operator
//! action required for the default install.

use std::path::{Path, PathBuf};

/// Parsed `/etc/evo/plugins.d/org.evoframework.artwork.local.toml` subset.
#[derive(Debug, Clone)]
pub(crate) struct PluginConfig {
    /// Absolute directory prefixes tried in order for relative
    /// `mpd-path` `file` values. Populated from two sources:
    ///
    /// 1. The MPD `music_directory` directive read from
    ///    `/etc/mpd.conf` at load time — the canonical truth.
    /// 2. Operator-supplied `[library] roots` entries — additive
    ///    overrides for distributions that mount music under
    ///    aliases or multiple roots.
    ///
    /// Empty means MPD is not installed AND no operator overrides
    /// were configured; resolution requires absolute paths only.
    pub(crate) library_roots: Vec<PathBuf>,
}

impl PluginConfig {
    /// Defaults: no library roots; only absolute MPD file paths work.
    pub(crate) fn defaults() -> Self {
        Self {
            library_roots: Vec::new(),
        }
    }

    /// Merge operator table + auto-derived MPD music_directory.
    /// Unknown keys at `[library]` are ignored with a warning;
    /// invalid entries return [`ConfigError`]. The MPD root is
    /// pushed FIRST so the cascade resolves the canonical truth
    /// before the operator override list.
    pub(crate) fn from_toml_table(
        table: &toml::Table,
    ) -> Result<Self, ConfigError> {
        Self::from_toml_table_with_mpd_conf_path(
            table,
            Path::new(evo_device_audio_shared::DEFAULT_MPD_CONF_PATH),
        )
    }

    /// Same as [`Self::from_toml_table`] with the MPD config path
    /// injected — exposed for unit tests so the auto-derive logic
    /// is testable without touching `/etc/mpd.conf` on the host.
    pub(crate) fn from_toml_table_with_mpd_conf_path(
        table: &toml::Table,
        mpd_conf_path: &Path,
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
        let mut library_roots: Vec<PathBuf> = Vec::new();
        // Auto-derive from MPD's canonical config first. Logging
        // surfaces the outcome so operators see exactly which path
        // the plugin will walk for sidecar resolution.
        match evo_device_audio_shared::load_music_directory_from_mpd_conf(
            mpd_conf_path,
        ) {
            Some(p) if p.is_absolute() => {
                tracing::info!(
                    plugin = crate::PLUGIN_NAME,
                    music_directory = %p.display(),
                    "auto-derived MPD music_directory from /etc/mpd.conf"
                );
                library_roots.push(p);
            }
            Some(p) => {
                tracing::warn!(
                    plugin = crate::PLUGIN_NAME,
                    value = %p.display(),
                    "MPD music_directory is not absolute; skipping auto-derived root"
                );
            }
            None => {
                tracing::info!(
                    plugin = crate::PLUGIN_NAME,
                    mpd_conf = %mpd_conf_path.display(),
                    "MPD music_directory not found at canonical path; \
                     relying on operator [library] roots config"
                );
            }
        }
        // Append operator overrides. The cascade walks every root;
        // an operator-supplied alias path can resolve files MPD
        // sees via a mount that does not match its own
        // music_directory.
        if let Some(toml::Value::Table(t)) = table.get("library") {
            let operator_roots = parse_library_roots(t)?;
            for p in operator_roots {
                if !library_roots.contains(&p) {
                    library_roots.push(p);
                }
            }
        } else if let Some(other) = table.get("library") {
            return Err(ConfigError {
                key: "library".into(),
                message: format!("expected a table, got {other:?}"),
            });
        }
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
    use std::io::Write;

    /// Path pinned to a non-existent location so tests are
    /// deterministic regardless of whether the build host has
    /// MPD installed. Auto-derive returns `None` against this
    /// path; operator-supplied roots become the sole source.
    const NO_MPD_CONF: &str = "/this/path/does/not/exist/mpd.conf";

    fn write_mpd_conf_fixture(
        music_directory: &str,
    ) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "music_directory \"{music_directory}\"").unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn empty_table_no_mpd_conf_yields_empty_roots() {
        let t: toml::Table = "".parse().unwrap();
        let c = PluginConfig::from_toml_table_with_mpd_conf_path(
            &t,
            Path::new(NO_MPD_CONF),
        )
        .unwrap();
        assert!(c.library_roots.is_empty());
    }

    #[test]
    fn operator_roots_only_when_no_mpd_conf() {
        let t: toml::Table = r#"
            [library]
            roots = ["/a/music", "/b/usb"]
        "#
        .parse()
        .unwrap();
        let c = PluginConfig::from_toml_table_with_mpd_conf_path(
            &t,
            Path::new(NO_MPD_CONF),
        )
        .unwrap();
        assert_eq!(c.library_roots.len(), 2);
        assert_eq!(c.library_roots[0], PathBuf::from("/a/music"));
    }

    #[test]
    fn auto_derived_mpd_root_comes_first() {
        let f = write_mpd_conf_fixture("/var/lib/evo/music");
        let t: toml::Table = "".parse().unwrap();
        let c = PluginConfig::from_toml_table_with_mpd_conf_path(&t, f.path())
            .unwrap();
        assert_eq!(c.library_roots.len(), 1);
        assert_eq!(c.library_roots[0], PathBuf::from("/var/lib/evo/music"));
    }

    #[test]
    fn operator_overrides_append_after_auto_derived() {
        let f = write_mpd_conf_fixture("/var/lib/evo/music");
        let t: toml::Table = r#"
            [library]
            roots = ["/mnt/external"]
        "#
        .parse()
        .unwrap();
        let c = PluginConfig::from_toml_table_with_mpd_conf_path(&t, f.path())
            .unwrap();
        assert_eq!(c.library_roots.len(), 2);
        // MPD's music_directory wins position 0 — the canonical
        // truth resolves first; operator overrides ride behind.
        assert_eq!(c.library_roots[0], PathBuf::from("/var/lib/evo/music"));
        assert_eq!(c.library_roots[1], PathBuf::from("/mnt/external"));
    }

    #[test]
    fn duplicate_operator_root_matching_mpd_dedups() {
        // An operator config that names the same path MPD already
        // gave us must not produce a duplicate entry — the
        // sidecar walk would do the same work twice for no
        // additional resolution power.
        let f = write_mpd_conf_fixture("/var/lib/evo/music");
        let t: toml::Table = r#"
            [library]
            roots = ["/var/lib/evo/music"]
        "#
        .parse()
        .unwrap();
        let c = PluginConfig::from_toml_table_with_mpd_conf_path(&t, f.path())
            .unwrap();
        assert_eq!(c.library_roots.len(), 1);
        assert_eq!(c.library_roots[0], PathBuf::from("/var/lib/evo/music"));
    }

    #[test]
    fn non_absolute_mpd_value_skipped() {
        // A misconfigured MPD with a relative music_directory
        // value must not propagate the bad path. Skip with a
        // warning; operator overrides remain available.
        let f = write_mpd_conf_fixture("relative/path");
        let t: toml::Table = r#"
            [library]
            roots = ["/operator/root"]
        "#
        .parse()
        .unwrap();
        let c = PluginConfig::from_toml_table_with_mpd_conf_path(&t, f.path())
            .unwrap();
        assert_eq!(c.library_roots.len(), 1);
        assert_eq!(c.library_roots[0], PathBuf::from("/operator/root"));
    }
}
