// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Apply the signed host-files carrier: the
//! wrappers and sudoers drop-ins `dist/` ships, then `installed.toml`
//! from the signed release data. Every file is checked against the signed
//! document, the allow list and (for sudoers) `visudo -cf` before the first
//! one is installed, and a failure restores what the run replaced.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::archive::extract_tar_gz;
use crate::check::{host_reason, load_host_files};
use crate::ctx::{installed_release, under_base, Ctx};
use crate::docs::sha256_hex;
use crate::error::{io_err, Res, UpdError};
use crate::state::{atomic_write, AppliedHost};
use crate::timeutil;
use crate::version::Dotted;

struct Planned {
    kind: &'static str,
    name: String,
    dest: PathBuf,
    content: Vec<u8>,
    mode: u32,
    existed: bool,
}

fn plain_token(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'+' | b'-')
        })
}

/// Render a sudoers template for the service user.
pub fn render_sudoers(template: &[u8], user: &str) -> Res<Vec<u8>> {
    let text = std::str::from_utf8(template).map_err(|_| {
        UpdError::new("sudoers_invalid", "template is not UTF-8")
    })?;
    Ok(text.replace("@EVO_SERVICE_USER@", user).into_bytes())
}

/// Apply the host files for `version` (the document's release).
pub fn apply_host(ctx: &Ctx, version: &str) -> Res<Value> {
    ctx.roots.check_for_write()?;
    let doc = load_host_files(ctx)?;
    if doc.host.release != version {
        return Err(UpdError::new(
            "not_offered",
            "that is not the release on offer",
        ));
    }
    if host_reason(ctx, &doc).is_none() {
        return Err(UpdError::new(
            "not_offered",
            "host files are already current",
        ));
    }
    // The record's three values are copied from the document, so each must
    // be plain before anything is installed.
    if !plain_token(&doc.host.release)
        || !plain_token(&doc.host.architecture)
        || timeutil::parse_utc(&doc.host.built_at_utc).is_none()
    {
        return Err(UpdError::new(
            "document_invalid",
            "release, architecture or built_at_utc is not plain",
        ));
    }

    under_base(ctx.cfg, &doc.host.url)?;
    let bytes = ctx.fetch.get(&doc.host.url, ctx.cfg.max_bundle_bytes)?;
    if sha256_hex(&bytes) != doc.host.sha256 {
        return Err(UpdError::new(
            "hash_mismatch",
            "carrier sha256 differs from the signed document",
        ));
    }
    ctx.state.ensure_stage()?;
    let work = ctx
        .state
        .stage_dir()
        .join(format!("host-{}", std::process::id()));
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work)
        .map_err(|e| io_err("stage_failed", "mkdir", e))?;
    let result = install_all(ctx, &doc, &bytes, &work);
    let _ = fs::remove_dir_all(&work);
    let installed = result?;

    let record = write_record(ctx, &doc)?;
    let mut a = ctx.state.applied();
    a.host = Some(AppliedHost {
        release: doc.host.release.clone(),
        sha256: doc.host.sha256.clone(),
        applied_at_utc: timeutil::format_utc(ctx.now),
    });
    ctx.state.write_applied(&a)?;
    Ok(json!({
        "component": crate::grammar::HOST_COMPONENT,
        "to": doc.host.release,
        "files": installed,
        "installed_record": record,
    }))
}

fn install_all(
    ctx: &Ctx,
    doc: &crate::docs::HostFiles,
    bytes: &[u8],
    work: &Path,
) -> Res<Vec<String>> {
    let tree = work.join("carrier");
    fs::create_dir_all(&tree)
        .map_err(|e| io_err("stage_failed", "mkdir", e))?;
    let carried: BTreeSet<_> = extract_tar_gz(bytes, &tree, None)?
        .into_iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();

    let user = ctx.host.service_user()?;
    let mut plan: Vec<Planned> = Vec::new();
    let mut listed: BTreeSet<String> = BTreeSet::new();
    for f in &doc.files {
        let allowed = ctx.allow.find(&f.kind, &f.name).ok_or_else(|| {
            UpdError::new(
                "file_not_allowed",
                format!("{} {} is not on the allow list", f.kind, f.name),
            )
        })?;
        let (rel, dest, mode, kind) = match f.kind.as_str() {
            "bin" => (
                format!("bin/{}", f.name),
                ctx.roots.bin_dir.join(&f.name),
                0o755,
                "bin",
            ),
            "sudoers" => (
                format!("sudoers.d/{}.in", f.name),
                ctx.roots.sudoers_dir.join(&f.name),
                0o440,
                "sudoers",
            ),
            _ => return Err(UpdError::new("file_not_allowed", "unknown kind")),
        };
        listed.insert(rel.clone());
        if !carried.contains(&rel) {
            return Err(UpdError::new(
                "carrier_incomplete",
                format!("{rel} is not in the carrier"),
            ));
        }
        let raw = fs::read(tree.join(&rel))
            .map_err(|e| io_err("carrier_unreadable", &rel, e))?;
        if sha256_hex(&raw) != f.sha256 {
            return Err(UpdError::new(
                "hash_mismatch",
                format!("{rel} differs from the signed document"),
            ));
        }
        let existed = dest.exists();
        if !existed && !allowed.create {
            continue;
        }
        let content = if kind == "sudoers" {
            render_sudoers(&raw, &user)?
        } else {
            raw
        };
        if existed && fs::read(&dest).is_ok_and(|b| b == content) {
            continue;
        }
        plan.push(Planned {
            kind,
            name: f.name.clone(),
            dest,
            content,
            mode,
            existed,
        });
    }
    if let Some(extra) = carried.iter().find(|c| !listed.contains(*c)) {
        return Err(UpdError::new(
            "carrier_unlisted_file",
            format!("{extra} is not named by the signed document"),
        ));
    }

    // Preflight: every sudoers file must parse before any file is replaced.
    for p in plan.iter().filter(|p| p.kind == "sudoers") {
        let probe = work.join(format!("probe-{}", p.name));
        fs::write(&probe, &p.content)
            .map_err(|e| io_err("stage_failed", "probe", e))?;
        ctx.host.visudo_check(&probe)?;
    }

    // Keep what is replaced, bins before sudoers (a grant without its
    // wrapper, or the reverse, is the half-installed state to avoid).
    let keep = ctx
        .roots
        .rollback_dir
        .join(format!("hostfiles-{}", timeutil::compact_utc(ctx.now)));
    fs::create_dir_all(&keep)
        .map_err(|e| io_err("rollback_failed", "mkdir", e))?;
    plan.sort_by_key(|p| p.kind != "bin");
    for p in plan.iter().filter(|p| p.existed) {
        fs::copy(&p.dest, keep.join(format!("{}-{}", p.kind, p.name)))
            .map_err(|e| io_err("rollback_failed", "keep", e))?;
    }
    let mut done: Vec<&Planned> = Vec::new();
    for p in &plan {
        match atomic_write(&p.dest, &p.content, p.mode) {
            Ok(()) => done.push(p),
            Err(e) => {
                for q in done.iter().rev() {
                    if q.existed {
                        if let Ok(old) = fs::read(
                            keep.join(format!("{}-{}", q.kind, q.name)),
                        ) {
                            let _ = atomic_write(&q.dest, &old, q.mode);
                        }
                    } else {
                        let _ = fs::remove_file(&q.dest);
                    }
                }
                return Err(e);
            }
        }
    }
    prune_host_rollbacks(&ctx.roots.rollback_dir);
    Ok(plan
        .iter()
        .map(|p| format!("{} {}", p.kind, p.name))
        .collect())
}

/// Keep the newest two `hostfiles-<utc>` directories the worker made.
fn prune_host_rollbacks(dir: &Path) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut mine: Vec<String> = rd
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| {
            n.strip_prefix("hostfiles-").is_some_and(|r| {
                r.len() == 16
                    && r.as_bytes()[8] == b'T'
                    && r.as_bytes()[15] == b'Z'
            })
        })
        .collect();
    mine.sort();
    while mine.len() > 2 {
        let _ = fs::remove_dir_all(dir.join(mine.remove(0)));
    }
}

/// Write `installed.toml` from the verified document. It never replaces a
/// record whose release is newer.
pub(crate) fn write_record(
    ctx: &Ctx,
    doc: &crate::docs::HostFiles,
) -> Res<&'static str> {
    if let Some(cur) = installed_release(ctx.roots) {
        match (Dotted::parse(&cur), Dotted::parse(&doc.host.release)) {
            (Some(c), Some(n)) if c > n => return Ok("kept_newer"),
            _ => {}
        }
    }
    let mut t = toml::Table::new();
    t.insert("release".into(), doc.host.release.clone().into());
    t.insert("architecture".into(), doc.host.architecture.clone().into());
    t.insert("built_at_utc".into(), doc.host.built_at_utc.clone().into());
    let text = toml::to_string(&t)
        .map_err(|e| UpdError::new("write_failed", e.to_string()))?;
    atomic_write(&ctx.roots.installed_record, text.as_bytes(), 0o644)?;
    Ok("written")
}
