// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Just a Nerd

//! Apply one plugin. The old bundle is kept, the new
//! one is swapped in by rename, the steward is restarted, and the result is
//! checked; a failed check restores the old bundle. Nothing under
//! `/var/lib/evo/plugins/<name>` is read or written: plugin state does
//! not move.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::archive::extract_tar_gz;
use crate::check::{best_version, load_registry, plugin_reason};
use crate::ctx::{installed_plugins, manifest_version, under_base, Ctx};
use crate::docs::sha256_hex;
use crate::error::{io_err, Res, UpdError};
use crate::state::AppliedEntry;
use crate::timeutil;

/// How an apply that got past verification ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The new bundle is in place and admitted.
    Applied,
    /// The new bundle failed its check; the old one is back.
    RolledBack(String),
}

const REQUIRED: [&str; 4] = [
    "manifest.toml",
    "manifest.sig",
    "plugin.bin",
    "privileges.yaml",
];
const KEEP_ROLLBACKS: usize = 2;

/// Copy a directory tree, files and directories only, keeping modes.
pub fn copy_tree(src: &Path, dst: &Path) -> Res<()> {
    fs::create_dir_all(dst).map_err(|e| io_err("copy_failed", "mkdir", e))?;
    let mode = fs::metadata(src)
        .map_err(|e| io_err("copy_failed", "stat", e))?
        .permissions()
        .mode();
    fs::set_permissions(dst, fs::Permissions::from_mode(mode & 0o755))
        .map_err(|e| io_err("copy_failed", "chmod", e))?;
    for ent in
        fs::read_dir(src).map_err(|e| io_err("copy_failed", "read_dir", e))?
    {
        let ent = ent.map_err(|e| io_err("copy_failed", "entry", e))?;
        let ty = ent
            .file_type()
            .map_err(|e| io_err("copy_failed", "type", e))?;
        let to = dst.join(ent.file_name());
        if ty.is_dir() {
            copy_tree(&ent.path(), &to)?;
        } else if ty.is_file() {
            fs::copy(ent.path(), &to)
                .map_err(|e| io_err("copy_failed", "copy", e))?;
        } else {
            return Err(UpdError::new(
                "copy_refused",
                format!("{} is not a file or directory", ent.path().display()),
            ));
        }
    }
    Ok(())
}

/// Put back a bundle the last run left aside if the live one is gone.
fn recover_interrupted(plugins: &Path, name: &str) {
    let live = plugins.join(name);
    let prefix_old = format!(".{name}.old-");
    let prefix_new = format!(".{name}.new-");
    let Ok(rd) = fs::read_dir(plugins) else {
        return;
    };
    for ent in rd.flatten() {
        let n = ent.file_name().to_string_lossy().to_string();
        if n.starts_with(&prefix_old) && !live.exists() {
            let _ = fs::rename(ent.path(), &live);
        } else if n.starts_with(&prefix_new) {
            let _ = fs::remove_dir_all(ent.path());
        }
    }
}

/// Keep only the newest `KEEP_ROLLBACKS` copies for a plugin.
fn prune_rollbacks(dir: &Path, name: &str) {
    let prefix = format!("{name}-");
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut mine: Vec<String> = rd
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| {
            n.strip_prefix(&prefix).is_some_and(|rest| {
                rest.len() == 16
                    && rest.as_bytes()[8] == b'T'
                    && rest.as_bytes()[15] == b'Z'
                    && rest[..8].bytes().all(|c| c.is_ascii_digit())
            })
        })
        .collect();
    mine.sort();
    while mine.len() > KEEP_ROLLBACKS {
        let _ = fs::remove_dir_all(dir.join(mine.remove(0)));
    }
}

/// Check an extracted tree is the bundle that was asked for.
fn check_tree(tree: &Path, name: &str, version: &str) -> Res<()> {
    for f in REQUIRED {
        if !tree.join(f).is_file() {
            return Err(UpdError::new(
                "bundle_incomplete",
                format!("{f} missing from the bundle"),
            ));
        }
    }
    match manifest_version(&tree.join("manifest.toml"), name) {
        Some(v) if v == version => Ok(()),
        Some(v) => Err(UpdError::new(
            "bundle_version_mismatch",
            format!("bundle manifest says {v}, not {version}"),
        )),
        None => Err(UpdError::new(
            "bundle_name_mismatch",
            "bundle manifest does not name the requested plugin",
        )),
    }
}

/// Apply `component` at `version`.
pub fn apply_plugin(
    ctx: &Ctx,
    component: &str,
    version: &str,
) -> Res<(Outcome, Value)> {
    ctx.roots.check_for_write()?;
    recover_interrupted(&ctx.roots.plugins_dir, component);

    let live = ctx.roots.plugins_dir.join(component);
    let have = installed_plugins(ctx.roots)
        .get(component)
        .cloned()
        .ok_or_else(|| {
            UpdError::new("not_installed", "first installs are not offered")
        })?;

    let reg = load_registry(ctx)?;
    let entry = reg
        .plugins
        .iter()
        .find(|p| p.name == component)
        .ok_or_else(|| {
            UpdError::new(
                "not_offered",
                "the registry does not list this plugin",
            )
        })?;
    let best = best_version(entry).ok_or_else(|| {
        UpdError::new("not_offered", "no usable version is listed")
    })?;
    if best.version != version {
        return Err(UpdError::new(
            "not_offered",
            "that is not the version on offer",
        ));
    }
    let applied = ctx.state.applied();
    if plugin_reason(
        &have,
        &best.version,
        &best.sha256,
        applied.plugins.get(component),
    )
    .is_none()
    {
        return Err(UpdError::new("not_offered", "already current"));
    }

    under_base(ctx.cfg, &best.url)?;
    let bytes = ctx.fetch.get(&best.url, ctx.cfg.max_bundle_bytes)?;
    if sha256_hex(&bytes) != best.sha256 {
        return Err(UpdError::new(
            "hash_mismatch",
            "bundle sha256 differs from the signed manifest",
        ));
    }

    ctx.state.ensure_stage()?;
    let work = ctx
        .state
        .stage_dir()
        .join(format!("work-{}", std::process::id()));
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work)
        .map_err(|e| io_err("stage_failed", "mkdir", e))?;
    let result = swap_in(ctx, component, version, &bytes, &work, &live);
    let _ = fs::remove_dir_all(&work);
    let (outcome, rollback_copy) = result?;

    let now = timeutil::format_utc(ctx.now);
    if outcome == Outcome::Applied {
        let mut a = ctx.state.applied();
        a.plugins.insert(
            component.to_string(),
            AppliedEntry {
                version: version.to_string(),
                sha256: best.sha256.clone(),
                applied_at_utc: now.clone(),
            },
        );
        ctx.state.write_applied(&a)?;
        prune_rollbacks(&ctx.roots.rollback_dir, component);
    }
    let summary = json!({
        "component": component,
        "from": have,
        "to": version,
        "rollback_copy": rollback_copy.file_name().map(|n| n.to_string_lossy().to_string()),
    });
    Ok((outcome, summary))
}

fn swap_in(
    ctx: &Ctx,
    name: &str,
    version: &str,
    bytes: &[u8],
    work: &Path,
    live: &Path,
) -> Res<(Outcome, PathBuf)> {
    let tree = work.join("tree");
    fs::create_dir_all(&tree)
        .map_err(|e| io_err("stage_failed", "mkdir", e))?;
    extract_tar_gz(bytes, &tree, Some(name))?;
    check_tree(&tree, name, version)?;

    let pid = std::process::id();
    let plugins = &ctx.roots.plugins_dir;
    let new = plugins.join(format!(".{name}.new-{pid}"));
    let old = plugins.join(format!(".{name}.old-{pid}"));
    let rollback = ctx
        .roots
        .rollback_dir
        .join(format!("{name}-{}", timeutil::compact_utc(ctx.now)));
    copy_tree(live, &rollback)?;
    if let Err(e) = copy_tree(&tree, &new) {
        let _ = fs::remove_dir_all(&new);
        return Err(e);
    }

    // Two renames: at no point is a half-written tree at the live path.
    fs::rename(live, &old).map_err(|e| {
        let _ = fs::remove_dir_all(&new);
        io_err("swap_failed", "set old aside", e)
    })?;
    if let Err(e) = fs::rename(&new, live) {
        let _ = fs::rename(&old, live);
        let _ = fs::remove_dir_all(&new);
        return Err(io_err("swap_failed", "put new in place", e));
    }

    let healthy =
        ctx.host.restart_evo().is_ok() && ctx.host.wait_plugin_ready(name);
    if healthy {
        let _ = fs::remove_dir_all(&old);
        return Ok((Outcome::Applied, rollback));
    }

    // The new bundle did not come up: put the old one back.
    let failed = plugins.join(format!(".{name}.failed-{pid}"));
    fs::rename(live, &failed)
        .map_err(|e| io_err("rollback_failed", "set new aside", e))?;
    fs::rename(&old, live)
        .map_err(|e| io_err("rollback_failed", "restore old", e))?;
    let _ = fs::remove_dir_all(&failed);
    let _ = ctx.host.restart_evo();
    let back = ctx.host.wait_plugin_ready(name);
    let reason = if back {
        "the new bundle was not admitted; the old one is back"
    } else {
        "the new bundle was not admitted and the old one did not answer after restore"
    };
    Ok((Outcome::RolledBack(reason.to_string()), rollback))
}
