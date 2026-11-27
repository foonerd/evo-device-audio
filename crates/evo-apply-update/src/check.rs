// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! The check: fetch and verify the recorded channel's registry and
//! host-files document, and say what is offered and why. Nothing is installed.

use std::fs;

use serde_json::{json, Value};

use crate::ctx::{installed_plugins, installed_release, plane_url, Ctx};
use crate::docs::{
    parse_host_files, parse_registry, sha256_hex, verify_document, HostFiles,
    Registry,
};
use crate::error::{Res, UpdError};
use crate::grammar::HOST_COMPONENT;
use crate::timeutil;
use crate::version::Dotted;

const DOC_MAX: u64 = 16 * 1024 * 1024;

fn plane_dir(ctx: &Ctx) -> String {
    format!(
        "registries/evo-audio-{}/{}",
        ctx.state.channel().plane(),
        ctx.triple
    )
}

/// Fetch, verify and parse the registry, keeping only plugins the
/// verifying key may sign for and that list this host's triple.
pub fn load_registry(ctx: &Ctx) -> Res<Registry> {
    let dir = plane_dir(ctx);
    let body = ctx.fetch.get(
        &plane_url(ctx.cfg, &format!("{dir}/registry.toml")),
        DOC_MAX,
    )?;
    let sig = ctx
        .fetch
        .get(&plane_url(ctx.cfg, &format!("{dir}/registry.sig")), 4096)?;
    let mut reg = parse_registry(&body)?;
    let key = verify_document(
        ctx.trust,
        &body,
        &sig,
        &reg.registry.public_key_fingerprint,
        &reg.registry.expires_at,
        "org.evoframework.registry",
        ctx.now,
    )?;
    reg.plugins.retain(|p| {
        key.key.allows(&p.name)
            && p.architectures.iter().any(|a| a == ctx.triple)
    });
    Ok(reg)
}

/// Fetch, verify and parse the host-files document.
pub fn load_host_files(ctx: &Ctx) -> Res<HostFiles> {
    let dir = plane_dir(ctx);
    let body = ctx.fetch.get(
        &plane_url(ctx.cfg, &format!("{dir}/host-files.toml")),
        DOC_MAX,
    )?;
    let sig = ctx
        .fetch
        .get(&plane_url(ctx.cfg, &format!("{dir}/host-files.sig")), 4096)?;
    let doc = parse_host_files(&body)?;
    verify_document(
        ctx.trust,
        &body,
        &sig,
        &doc.host.public_key_fingerprint,
        &doc.host.expires_at,
        "org.evoframework.host-files",
        ctx.now,
    )?;
    if doc.host.architecture != ctx.arch {
        return Err(UpdError::new(
            "architecture_mismatch",
            "host-files is for another architecture",
        ));
    }
    Ok(doc)
}

/// The best listed version of a plugin: the highest that parses.
pub fn best_version(
    p: &crate::docs::PluginEntry,
) -> Option<&crate::docs::VersionEntry> {
    p.versions
        .iter()
        .filter_map(|v| Dotted::parse(&v.version).map(|d| (d, v)))
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, v)| v)
}

/// Why a plugin is offered, or `None` if it is current.
/// `installed` is the version on the box; `recorded` is what this worker
/// recorded the last time it applied the plugin.
pub fn plugin_reason(
    installed: &str,
    available: &str,
    manifest_sha: &str,
    recorded: Option<&crate::state::AppliedEntry>,
) -> Option<&'static str> {
    let (i, a) = (Dotted::parse(installed)?, Dotted::parse(available)?);
    match a.cmp(&i) {
        std::cmp::Ordering::Greater => Some("newer"),
        std::cmp::Ordering::Less => None,
        std::cmp::Ordering::Equal => match recorded {
            Some(r) if r.version == available && r.sha256 == manifest_sha => {
                None
            }
            Some(_) => Some("rebuild"),
            None => Some("unknown_bytes"),
        },
    }
}

/// Why the host files are offered, or `None`.
pub fn host_reason(ctx: &Ctx, doc: &HostFiles) -> Option<&'static str> {
    let installed = installed_release(ctx.roots);
    let applied = ctx.state.applied().host;
    let release_cmp = match &installed {
        None => None,
        Some(r) => {
            Some(Dotted::parse(&doc.host.release)?.cmp(&Dotted::parse(r)?))
        }
    };
    match release_cmp {
        None => return Some("no_record"),
        Some(std::cmp::Ordering::Greater) => return Some("release_newer"),
        Some(std::cmp::Ordering::Less) => return None,
        Some(std::cmp::Ordering::Equal) => {}
    }
    if applied.as_ref().is_some_and(|a| {
        a.release == doc.host.release && a.sha256 != doc.host.sha256
    }) {
        return Some("rebuild");
    }
    if host_drift(ctx, doc) {
        return Some("drift");
    }
    None
}

/// Is a file the carrier would create missing, or a wrapper different?
fn host_drift(ctx: &Ctx, doc: &HostFiles) -> bool {
    doc.files.iter().any(|f| {
        let Some(a) = ctx.allow.find(&f.kind, &f.name) else {
            return false;
        };
        let dest = match f.kind.as_str() {
            "bin" => ctx.roots.bin_dir.join(&f.name),
            "sudoers" => ctx.roots.sudoers_dir.join(&f.name),
            _ => return false,
        };
        match fs::read(&dest) {
            Err(_) => a.create,
            Ok(b) => f.kind == "bin" && sha256_hex(&b) != f.sha256,
        }
    })
}

/// Run a check and build the inventory document.
pub fn run_check(ctx: &Ctx) -> Res<Value> {
    ctx.roots.check_for_read()?;
    let reg = load_registry(ctx)?;
    let installed = installed_plugins(ctx.roots);
    let applied = ctx.state.applied();
    let mut items = Vec::new();
    for p in &reg.plugins {
        let (Some(have), Some(best)) =
            (installed.get(&p.name), best_version(p))
        else {
            continue;
        };
        let rec = applied.plugins.get(&p.name);
        if let Some(reason) =
            plugin_reason(have, &best.version, &best.sha256, rec)
        {
            items.push(json!({
                "component": p.name,
                "kind": "plugin",
                "installed_version": have,
                "available_version": best.version,
                "installed_bytes": if rec.is_some() { "known" } else { "unknown" },
                "reason": reason,
            }));
        }
    }
    let host_files = match load_host_files(ctx) {
        Ok(doc) => {
            if let Some(reason) = host_reason(ctx, &doc) {
                items.push(json!({
                    "component": HOST_COMPONENT,
                    "kind": "host-files",
                    "installed_version": installed_release(ctx.roots),
                    "available_version": doc.host.release,
                    "installed_bytes": if applied.host.is_some() { "known" } else { "unknown" },
                    "reason": reason,
                }));
            }
            json!({"state": "ok"})
        }
        Err(e) => json!({"state": "unavailable", "code": e.code}),
    };
    Ok(json!({
        "v": 1,
        "channel": ctx.state.channel().name(),
        "checked_at_utc": timeutil::format_utc(ctx.now),
        "items": items,
        "host_files": host_files,
    }))
}
