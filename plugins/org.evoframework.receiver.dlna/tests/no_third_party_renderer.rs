// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Contract pin: the DLNA renderer is built here, in Rust, and owned like
//! the DLNA client is.
//!
//! The renderer plugin must not admit, document, install or depend on a
//! third-party renderer, and it must not reach the player except through
//! the `playback.mpd` verbs. This test goes RED the moment any of these is
//! true:
//!
//! 1. the plugin's manifests or `privileges.yaml` declare a required
//!    binary, or an exec capability, for a renderer daemon;
//! 2. anything under the plugin directory documents or ships a
//!    third-party package source or install step for a renderer;
//! 3. the distribution's install scripts, unit drop-ins or package lists
//!    name such a package;
//! 4. the plugin's source speaks to the MPD socket or port itself.
//!
//! The forbidden-name list is the one place a new name is added, in the
//! open. This file is the one exemption, because it has to contain the
//! names it forbids.

use std::path::{Path, PathBuf};

/// Renderer daemons and the packaging that would bring one in.
const FORBIDDEN_RENDERERS: &[&str] = &[
    "upmpdcli",
    "gmediarender",
    "gmrender",
    "rygel",
    "libupnpp",
    "libnpupnp",
    "lesbonscomptes",
];

/// Ways of reaching the player directly, not through its verbs.
const FORBIDDEN_DIRECT_MPD: &[&str] = &[
    "/run/mpd",
    "mpd_client",
    "MpdClient",
    "mpd::Client",
    ":6600",
];

const THIS_FILE: &str = "tests/no_third_party_renderer.rs";

/// One scanned file: its path relative to the scan root and its text.
struct Scanned {
    rel: String,
    text: String,
}

/// Every violation in `files`. `src_prefix` marks the plugin's own source
/// tree, which additionally may not reach MPD directly.
fn violations(files: &[Scanned], src_prefix: &str) -> Vec<String> {
    let mut out = Vec::new();
    for f in files {
        if f.rel == THIS_FILE {
            continue;
        }
        let lower = f.text.to_ascii_lowercase();
        for name in FORBIDDEN_RENDERERS {
            if lower.contains(name) {
                out.push(format!(
                    "{}: names the third-party renderer `{name}`",
                    f.rel
                ));
            }
        }
        if f.rel.starts_with(src_prefix) {
            for pat in FORBIDDEN_DIRECT_MPD {
                if f.text.contains(pat) {
                    out.push(format!(
                        "{}: reaches MPD directly (`{pat}`); use the playback.mpd verbs",
                        f.rel
                    ));
                }
            }
        }
    }
    out
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<Scanned>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if name == "target" || name == ".git" || name == "node_modules" {
            continue;
        }
        if p.is_dir() {
            collect(root, &p, out);
        } else if let Ok(text) = std::fs::read_to_string(&p) {
            let rel = p
                .strip_prefix(root)
                .unwrap_or(&p)
                .to_string_lossy()
                .into_owned();
            out.push(Scanned { rel, text });
        }
    }
}

fn plugin_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn repo_root() -> PathBuf {
    plugin_dir()
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn scanned(root: &Path, subs: &[&str], files: &[&str]) -> Vec<Scanned> {
    let mut v = Vec::new();
    for s in subs {
        collect(root, &root.join(s), &mut v);
    }
    for f in files {
        if let Ok(text) = std::fs::read_to_string(root.join(f)) {
            v.push(Scanned {
                rel: (*f).to_string(),
                text,
            });
        }
    }
    v
}

fn s(rel: &str, text: &str) -> Scanned {
    Scanned {
        rel: rel.into(),
        text: text.into(),
    }
}

// ----- positive controls: the pin can fail -----

#[test]
fn a_manifest_requiring_a_renderer_daemon_is_a_violation() {
    let v = violations(
        &[s(
            "privileges.yaml",
            "required_binaries:\n  - name: upmpdcli\n    min_version: \"1.0\"\n",
        )],
        "src/",
    );
    assert_eq!(v.len(), 1, "{v:?}");
}

#[test]
fn every_forbidden_name_is_caught_in_any_case() {
    for name in FORBIDDEN_RENDERERS {
        let v = violations(
            &[s(
                "docs/INSTALL.md",
                &format!("apt install {}", name.to_uppercase()),
            )],
            "src/",
        );
        assert_eq!(v.len(), 1, "{name}: {v:?}");
    }
}

#[test]
fn a_third_party_apt_source_in_dist_is_a_violation() {
    let v = violations(
        &[s(
            "dist/apt.list",
            "deb https://www.lesbonscomptes.com/upmpdcli/downloads/debian trixie main",
        )],
        "src/",
    );
    assert!(!v.is_empty());
}

#[test]
fn a_direct_mpd_client_in_the_plugin_source_is_a_violation() {
    for text in [
        "UnixStream::connect(\"/run/mpd/socket\")",
        "use mpd_client::Client;",
        "TcpStream::connect(\"127.0.0.1:6600\")",
    ] {
        let v = violations(&[s("src/bridge.rs", text)], "src/");
        assert_eq!(v.len(), 1, "{text}: {v:?}");
    }
    // The same text outside the plugin source is not this rule's business.
    assert!(
        violations(&[s("docs/NOTES.md", "/run/mpd/socket")], "src/").is_empty()
    );
}

#[test]
fn this_file_is_the_only_exemption() {
    assert!(
        violations(&[s(THIS_FILE, "upmpdcli gmediarender")], "src/").is_empty()
    );
    assert_eq!(violations(&[s("src/lib.rs", "upmpdcli")], "src/").len(), 1);
}

// ----- the real tree -----

#[test]
fn the_plugin_names_no_third_party_renderer_and_reaches_no_mpd_directly() {
    let root = plugin_dir();
    let files = scanned(&root, &["."], &[]);
    assert!(
        files.iter().any(|f| f.rel == "manifest.oop.toml"),
        "the scan must see the manifests (scanned {} files)",
        files.len()
    );
    let v = violations(&files, "src/");
    assert!(
        v.is_empty(),
        "third-party renderer or direct MPD path:\n{}",
        v.join("\n")
    );
}

#[test]
fn the_distribution_installs_no_third_party_renderer() {
    let root = repo_root();
    let files = scanned(
        &root,
        &["dist", "scripts", "ci"],
        &["PRIVILEGES.yaml", "INSTALL.md", "README.md"],
    );
    assert!(
        files.iter().any(|f| f.rel.starts_with("dist/")),
        "the scan must see the distribution tree"
    );
    let v = violations(&files, "no-source-prefix-here/");
    assert!(
        v.is_empty(),
        "the distribution names a renderer package:\n{}",
        v.join("\n")
    );
}
