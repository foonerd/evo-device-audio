// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Contract pin: the Bluetooth sink is built here, in Rust, and owned like
//! the DLNA renderer and the AirPlay receiver are.
//!
//! The device may rely on the platform's Bluetooth stack (it owns the radio,
//! the pairing keys and the link) and nothing else: no separate audio daemon
//! may be admitted, packaged, documented or depended on as the way sound gets
//! from a phone to the player. This test goes RED the moment any of these is
//! true:
//!
//! 1. a manifest or `privileges.yaml` of the Bluetooth plugin declares a
//!    required binary, or an exec capability, for a Bluetooth audio daemon;
//! 2. anything under the plugin, this crate, or the distribution's install
//!    scripts, unit drop-ins and package lists names such a daemon or its
//!    package;
//! 3. the Bluetooth plugin's source speaks to the MPD socket or port itself
//!    instead of going through the `playback.mpd` verbs;
//! 4. `playback.mpd` resolves a capture to anything but a fixed Loopback
//!    capture address of its own table;
//! 5. the decoder crate is not plain safe Rust: it gains a dependency, a build
//!    script, a native library, `unsafe`, or a Rust version above 1.85.
//!
//! The forbidden-name list is the one place a new name is added, in the open.
//! This file is the one exemption, because it has to contain the names it
//! forbids.

use std::path::{Path, PathBuf};

/// Bluetooth audio daemons and the packaging that would bring one in.
const FORBIDDEN_SINKS: &[&str] = &[
    "bluealsa",
    "bluez-alsa",
    "bluez_alsa",
    "pulseaudio",
    "pipewire",
    "wireplumber",
    "libspa-0.2-bluetooth",
];

/// Ways of reaching the player directly, not through its verbs.
const FORBIDDEN_DIRECT_MPD: &[&str] = &[
    "/run/mpd",
    "mpd_client",
    "MpdClient",
    "mpd::Client",
    ":6600",
];

/// Files that mention a forbidden name for a reason that has nothing to do
/// with admitting it, each with the reason.
const EXEMPT: &[(&str, &str)] = &[
    (
        "tests/no_third_party_bluetooth.rs",
        "this file lists the names it forbids",
    ),
    (
        "dist/scripts/tests/mpd-journal-classifier.test.sh",
        "a test of how MPD's own log lines are classified; it names other hosts' sound servers in its fixtures",
    ),
];

struct Scanned {
    rel: String,
    text: String,
}

fn exempt(rel: &str) -> bool {
    EXEMPT
        .iter()
        .any(|(path, _)| *path == rel || rel.ends_with(path))
}

/// Every violation in `files`. `plugin_src` marks the Bluetooth plugin's own
/// source tree, which additionally may not reach MPD directly.
fn violations(files: &[Scanned], plugin_src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for f in files {
        if exempt(&f.rel) {
            continue;
        }
        let lower = f.text.to_ascii_lowercase();
        for name in FORBIDDEN_SINKS {
            if lower.contains(name) {
                out.push(format!(
                    "{}: names the Bluetooth audio daemon `{name}`",
                    f.rel
                ));
            }
        }
        if f.rel.starts_with(plugin_src) {
            for pat in FORBIDDEN_DIRECT_MPD {
                if f.text.contains(pat) {
                    out.push(format!("{}: reaches MPD directly (`{pat}`); use the playback.mpd verbs", f.rel));
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
        if matches!(
            name.as_str(),
            "target" | ".git" | "node_modules" | "vectors"
        ) {
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

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn repo_root() -> PathBuf {
    crate_dir().join("../..").canonicalize().expect("repo root")
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
fn a_manifest_requiring_a_bluetooth_audio_daemon_is_a_violation() {
    let v = violations(
        &[s("privileges.yaml", "required_binaries:\n  - name: bluealsa\n    min_version: \"4.0\"\n")],
        "src/",
    );
    assert_eq!(v.len(), 1, "{v:?}");
}

#[test]
fn every_forbidden_name_is_caught_in_any_case() {
    for name in FORBIDDEN_SINKS {
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
fn a_package_for_one_in_the_distribution_is_a_violation() {
    for (rel, text) in [
        (
            "dist/scripts/install.sh",
            "apt-get install -y bluez-alsa-utils",
        ),
        ("dist/etc/packages.list", "pulseaudio-module-bluetooth"),
        (
            "dist/apt.list",
            "deb https://example.invalid/bluealsa/debian trixie main",
        ),
        ("scripts/enable.sh", "systemctl enable --now wireplumber"),
    ] {
        assert!(!violations(&[s(rel, text)], "src/").is_empty(), "{rel}");
    }
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
    assert!(
        violations(&[s("docs/NOTES.md", "/run/mpd/socket")], "src/").is_empty()
    );
}

#[test]
fn the_exemptions_are_exactly_the_listed_files() {
    assert!(violations(
        &[s("tests/no_third_party_bluetooth.rs", "bluealsa pipewire")],
        "src/"
    )
    .is_empty());
    assert!(violations(
        &[s(
            "dist/scripts/tests/mpd-journal-classifier.test.sh",
            "Pulse/PipeWire"
        )],
        "src/"
    )
    .is_empty());
    assert_eq!(violations(&[s("src/lib.rs", "bluealsa")], "src/").len(), 1);
    assert_eq!(
        violations(&[s("dist/scripts/other.sh", "pipewire")], "src/").len(),
        1
    );
    for (path, why) in EXEMPT {
        assert!(!why.is_empty(), "{path} needs a reason");
    }
}

// ----- the real tree -----

#[test]
fn the_decoder_crate_names_no_bluetooth_audio_daemon() {
    let files = scanned(&crate_dir(), &["."], &[]);
    assert!(
        files.iter().any(|f| f.rel == "Cargo.toml"),
        "the scan must see the manifest"
    );
    let v = violations(&files, "no-source-prefix-here/");
    assert!(
        v.is_empty(),
        "a Bluetooth audio daemon is named:\n{}",
        v.join("\n")
    );
}

#[test]
fn the_bluetooth_plugin_names_no_audio_daemon_and_reaches_no_mpd_directly() {
    let plugin =
        repo_root().join("plugins/org.evoframework.receiver.bluetooth");
    if !plugin.is_dir() {
        // The plugin lands in a later change; this test then scans it. Nothing
        // is exempt from the rule by being absent: the distribution test below
        // already covers every other place a daemon could come from.
        return;
    }
    let files = scanned(&plugin, &["."], &[]);
    assert!(
        files.iter().any(|f| f.rel == "manifest.oop.toml"),
        "the scan must see the plugin's manifests (scanned {} files)",
        files.len()
    );
    let v = violations(&files, "src/");
    assert!(
        v.is_empty(),
        "third-party Bluetooth audio daemon or direct MPD path:\n{}",
        v.join("\n")
    );
    let privileges = std::fs::read_to_string(plugin.join("privileges.yaml"))
        .expect("privileges.yaml");
    assert!(
        privileges.contains("required_binaries: []"),
        "the sink needs no host binary of its own"
    );
}

#[test]
fn the_distribution_installs_no_bluetooth_audio_daemon() {
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
        "the distribution names a Bluetooth audio daemon:\n{}",
        v.join("\n")
    );
}

#[test]
fn the_player_resolves_a_capture_only_to_a_fixed_loopback_address_of_its_own() {
    let table = std::fs::read_to_string(
        repo_root()
            .join("plugins/org.evoframework.playback.mpd/src/capture.rs"),
    )
    .expect("the playback plugin's capture table");
    let uris: Vec<&str> = table
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("uri: \""))
        .collect();
    assert!(!uris.is_empty(), "the scan must find the capture table");
    for line in uris {
        assert!(
            line.starts_with("uri: \"alsa://hw:Loopback,1,")
                && line.ends_with("\","),
            "a capture that is not a fixed Loopback capture address: {line}"
        );
    }
}

// ----- the decoder is plain safe Rust -----

#[test]
fn the_decoder_has_no_dependencies_no_build_script_and_no_native_code() {
    let manifest =
        std::fs::read_to_string(crate_dir().join("Cargo.toml")).unwrap();
    let after_deps = manifest
        .split("[dependencies]")
        .nth(1)
        .expect("[dependencies]");
    let deps = after_deps.split("\n[").next().unwrap();
    assert!(
        deps.trim().is_empty(),
        "the decoder takes no dependencies:\n{deps}"
    );
    for section in [
        "[build-dependencies]",
        "[dev-dependencies]",
        "build =",
        "links =",
    ] {
        assert!(!manifest.contains(section), "Cargo.toml has `{section}`");
    }
    assert!(
        manifest.contains("rust-version.workspace = true"),
        "the toolchain pin is the workspace's"
    );
    assert!(!crate_dir().join("build.rs").exists(), "no build script");
}

#[test]
fn the_decoder_source_has_no_unsafe_and_forbids_it() {
    let lib = std::fs::read_to_string(crate_dir().join("src/lib.rs")).unwrap();
    assert!(lib.contains("#![forbid(unsafe_code)]"));
    let mut files = Vec::new();
    collect(&crate_dir(), &crate_dir().join("src"), &mut files);
    assert!(files.len() >= 6, "{} source files", files.len());
    for f in files {
        for (n, line) in f.text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            assert!(!code.contains("unsafe "), "{}:{}: `unsafe`", f.rel, n + 1);
            assert!(
                !code.contains("#[allow("),
                "{}:{}: `#[allow]`",
                f.rel,
                n + 1
            );
        }
    }
}

#[test]
fn the_workspace_toolchain_pin_has_not_moved() {
    let root = std::fs::read_to_string(repo_root().join("Cargo.toml")).unwrap();
    assert!(
        root.contains("rust-version = \"1.85\""),
        "the workspace Rust version moved"
    );
}
