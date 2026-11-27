// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Pins on what this library is allowed to contain.
//!
//! The AirPlay receiver is built and owned here: no third-party AirPlay
//! daemon, no system mDNS binding inside the library, the toolchain pin
//! unmoved, and the Apple-derived interoperability data kept in exactly one
//! source file. Each pin reads the working tree, so it fails the moment a
//! change moves the line it guards.

use std::fs;
use std::path::{Path, PathBuf};

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn repo_root() -> PathBuf {
    crate_dir()
        .parent()
        .and_then(Path::parent)
        .expect("crate sits two levels under the repository root")
        .to_path_buf()
}

/// Every file under `dir` whose name ends in one of `exts`, skipping build
/// output and version-control metadata.
fn files_under(dir: &Path, exts: &[&str], out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if name != "target" && name != ".git" {
                files_under(&path, exts, out);
            }
        } else if exts.iter().any(|e| name.ends_with(e)) {
            out.push(path);
        }
    }
}

/// The bytes written as `0x..` tokens in a Rust source text, in order, with
/// all other text ignored. A table is found whatever its line wrapping.
fn hex_tokens(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == b'0' && bytes[i + 1] == b'x' {
            let pair = &text[i + 2..i + 4];
            if let Ok(v) = u8::from_str_radix(pair, 16) {
                out.push(v);
                i += 4;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn contains_run(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).expect("hex"))
        .collect()
}

/// Distinctive runs from the interoperability tables, kept as plain hex
/// strings (no `0x` prefix) so this file never matches its own scan.
const TABLE_RUNS: [&str; 4] = [
    "0f9f3f9e0a2521dbdf312ab2bfb29e8d",
    "cf32a25714b2524f8aa0ad7af164e37b",
    "c169a352eeed35b18cdd9c58d64f16c1",
    "9001e1727e0f57f9f5880db104a6257a",
];

#[test]
fn the_apple_derived_tables_live_in_one_file() {
    let isolated = crate_dir().join("src").join("apple_interop.rs");
    let mut all = Vec::new();
    files_under(&repo_root().join("crates"), &[".rs"], &mut all);
    files_under(&repo_root().join("plugins"), &[".rs"], &mut all);
    let mut homes = Vec::new();
    for path in all {
        let text = fs::read_to_string(&path).unwrap_or_default();
        let toks = hex_tokens(&text);
        let hit = TABLE_RUNS
            .iter()
            .filter(|r| !r.is_empty())
            .any(|r| contains_run(&toks, &unhex(r)));
        if hit {
            homes.push(path);
        }
    }
    assert_eq!(
        homes,
        vec![isolated.clone()],
        "the interoperability tables must appear in {} and nowhere else",
        isolated.display()
    );
}

#[test]
fn the_isolated_file_says_what_it_holds() {
    let text =
        fs::read_to_string(crate_dir().join("src").join("apple_interop.rs"))
            .expect("src/apple_interop.rs exists");
    for word in ["Apple", "interoperability", "NOTICE"] {
        assert!(
            text.contains(word),
            "src/apple_interop.rs must state its origin; missing {word:?}"
        );
    }
}

#[test]
fn the_library_binds_no_system_mdns_client() {
    let manifest =
        fs::read_to_string(crate_dir().join("Cargo.toml")).expect("manifest");
    for name in ["zbus", "zvariant", "avahi", "dbus"] {
        assert!(
            !manifest.contains(name),
            "Cargo.toml must not depend on {name}: advertisement belongs to the host"
        );
    }
    assert!(
        !crate_dir().join("src").join("avahi.rs").exists(),
        "src/avahi.rs must not exist"
    );
}

#[test]
fn the_toolchain_pin_does_not_move() {
    let manifest =
        fs::read_to_string(crate_dir().join("Cargo.toml")).expect("manifest");
    assert!(
        manifest.contains("rust-version.workspace = true"),
        "the crate inherits the workspace toolchain pin"
    );
    let root =
        fs::read_to_string(repo_root().join("Cargo.toml")).expect("root");
    assert!(
        root.contains("rust-version = \"1.85\""),
        "the workspace toolchain pin stays at 1.85"
    );
}

#[test]
fn no_third_party_airplay_daemon_is_installed_or_shipped() {
    let names = ["shairport", "uxplay", "rpiplay", "owntone"];
    let mut files = Vec::new();
    files_under(
        &crate_dir(),
        &[".rs", ".toml", ".md", ".sh", ".yaml", ".yml", ".service"],
        &mut files,
    );
    for path in files {
        // This file names the forbidden words in order to forbid them.
        if path.ends_with("tests/pins.rs") {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap_or_default().to_lowercase();
        for line in text.lines() {
            let installs = line.contains("apt install")
                || line.contains("apt-get install")
                || line.contains("required_binaries")
                || line.contains("systemctl");
            if installs && names.iter().any(|n| line.contains(n)) {
                panic!(
                    "{} names a third-party AirPlay daemon in an install step: {line}",
                    path.display()
                );
            }
        }
    }
}
