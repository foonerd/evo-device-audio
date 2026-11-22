// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Tests for the system.info plugin, against host trees built in a
//! temporary directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{json, Value};

use super::*;

/// A host tree in a temporary directory.
struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn put(&self, rel: &str, body: &str) -> &Self {
        let path = self.dir.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        self
    }

    fn host(&self, machine: fn() -> Option<String>) -> Host {
        Host::at(self.dir.path(), machine)
    }

    fn plugin(&self, machine: fn() -> Option<String>) -> SystemInfoPlugin {
        let mut p = SystemInfoPlugin::reading(self.host(machine));
        p.loaded = true;
        p
    }

    /// A full x86 host with no device tree, and an installed record.
    fn x86() -> Self {
        let f = Self::new();
        f.put("proc/sys/kernel/hostname", "evo-nuc\n")
            .put(
                "etc/os-release",
                "NAME=\"Debian GNU/Linux\"\nPRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"\nID=debian\n",
            )
            .put("proc/sys/kernel/osrelease", "6.1.0-25-amd64\n")
            .put("sys/class/dmi/id/sys_vendor", "Intel Corporation\n")
            .put("sys/class/dmi/id/product_name", "NUC7i5BNH\n")
            .put(
                "proc/cpuinfo",
                "processor\t: 0\nmodel name\t: Intel(R) Core(TM) i5-7260U CPU @ 2.20GHz\n\n\
                 processor\t: 1\nmodel name\t: Intel(R) Core(TM) i5-7260U CPU @ 2.20GHz\n\n\
                 processor\t: 2\nmodel name\t: Intel(R) Core(TM) i5-7260U CPU @ 2.20GHz\n\n\
                 processor\t: 3\nmodel name\t: Intel(R) Core(TM) i5-7260U CPU @ 2.20GHz\n",
            )
            .put("proc/meminfo", "MemTotal:        8041148 kB\nMemFree:  1 kB\n")
            .put("proc/uptime", "12345.99 49000.10\n")
            .put(
                INSTALLED_RECORD,
                "release = \"0.1.13.1\"\narchitecture = \"x86_64-unknown-linux-gnu\"\nbuilt_at_utc = \"2026-10-28T09:00:00Z\"\n",
            );
        f
    }
}

fn x86_64() -> Option<String> {
    Some("x86_64".to_string())
}

fn no_machine() -> Option<String> {
    None
}

fn req(verb: &str, payload: Value) -> Request {
    Request {
        request_type: verb.to_string(),
        payload: payload.to_string().into_bytes(),
        correlation_id: 1,
        deadline: None,
        instance_id: None,
        principal_scope: None,
        has_step_up: false,
    }
}

async fn answer(p: &SystemInfoPlugin, verb: &str) -> Value {
    let resp = p
        .handle_request(&req(verb, json!({ "v": 1 })))
        .await
        .unwrap();
    serde_json::from_slice(&resp.payload).unwrap()
}

// ----- manifest / surface -----

#[test]
fn both_manifests_stock_system_info_with_two_read_gated_verbs() {
    let oop = Manifest::from_toml(include_str!("../manifest.oop.toml"))
        .expect("manifest.oop.toml parses");
    for m in [manifest(), oop] {
        assert_eq!(m.plugin.name, PLUGIN_NAME);
        assert_eq!(m.plugin.version.to_string(), BUILD);
        assert_eq!(m.target.shelf, "system.info");
        assert_eq!(m.target.shape, 1);
        assert!(!m.capabilities.appointments);
        let resp = m.capabilities.respondent.expect("respondent");
        assert_eq!(resp.request_types, vec![VERB_INFO, VERB_VERSION]);
        for verb in [VERB_INFO, VERB_VERSION] {
            match resp.verb_capabilities.get(verb) {
                Some(evo_plugin_sdk::manifest::VerbCapability::Read {
                    scope,
                }) => {
                    assert_eq!(scope, "system")
                }
                other => panic!("{verb} must be read/system, got {other:?}"),
            }
        }
    }
}

/// `build` is the workspace version this crate was compiled from.
#[test]
fn build_is_the_workspace_version() {
    let workspace: toml::Table =
        include_str!("../../../Cargo.toml").parse().unwrap();
    let version = workspace["workspace"]["package"]["version"].as_str();
    assert_eq!(version, Some(BUILD));
    assert_eq!(Fixture::new().host(no_machine).version().build, BUILD);
}

#[tokio::test]
async fn describe_names_the_manifest_verbs() {
    let d = SystemInfoPlugin::new().describe().await;
    assert_eq!(d.identity.name, PLUGIN_NAME);
    let declared = manifest().capabilities.respondent.unwrap().request_types;
    assert_eq!(d.runtime_capabilities.request_types, declared);
}

// ----- get_system_info -----

#[tokio::test]
async fn a_full_host_answers_every_field() {
    let f = Fixture::x86();
    assert_eq!(
        answer(&f.plugin(x86_64), VERB_INFO).await,
        json!({
            "v": 1,
            "hostname": "evo-nuc",
            "os": "Debian GNU/Linux 12 (bookworm)",
            "kernel": "6.1.0-25-amd64",
            "architecture": "x86_64",
            "model": "Intel Corporation NUC7i5BNH",
            "cpu": {
                "model": "Intel(R) Core(TM) i5-7260U CPU @ 2.20GHz",
                "cores": 4,
            },
            "memory_total_bytes": 8041148u64 * 1024,
            "uptime_s": 12345,
        })
    );
}

/// Pin: a missing source answers null, never a guess, and the verb
/// still answers. An unreadable source is the same.
#[tokio::test]
async fn a_missing_source_answers_null_not_a_guess() {
    let empty = Fixture::new();
    let nulls = json!({
        "v": 1,
        "hostname": null,
        "os": null,
        "kernel": null,
        "architecture": null,
        "model": null,
        "cpu": { "model": null, "cores": null },
        "memory_total_bytes": null,
        "uptime_s": null,
    });
    assert_eq!(answer(&empty.plugin(no_machine), VERB_INFO).await, nulls);
    assert_eq!(
        answer(&empty.plugin(no_machine), VERB_VERSION).await,
        json!({
            "v": 1,
            "release": null,
            "build": BUILD,
            "architecture": null,
            "built_at_utc": null,
        })
    );

    // Present but unreadable (a directory where a file belongs), or
    // present with no text: still null.
    let odd = Fixture::new();
    std::fs::create_dir_all(odd.dir.path().join("proc/sys/kernel/hostname"))
        .unwrap();
    odd.put("proc/sys/kernel/osrelease", " \n")
        .put("etc/os-release", "NAME=Debian\n")
        .put("proc/cpuinfo", "flags\t: fpu\n")
        .put("proc/meminfo", "MemFree: 1 kB\n")
        .put("proc/uptime", "\n");
    assert_eq!(
        answer(&odd.plugin(|| Some("  ".to_string())), VERB_INFO).await,
        nulls
    );
}

#[test]
fn the_model_is_the_device_tree_then_dmi() {
    let pi = Fixture::new();
    pi.put("proc/device-tree/model", "Raspberry Pi 5 Model B Rev 1.0\0")
        .put("sys/class/dmi/id/sys_vendor", "Ignored\n");
    assert_eq!(
        pi.host(no_machine).info().model.as_deref(),
        Some("Raspberry Pi 5 Model B Rev 1.0")
    );

    let blank_tree = Fixture::new();
    blank_tree
        .put("proc/device-tree/model", "\0\n")
        .put("sys/class/dmi/id/sys_vendor", "LENOVO\n")
        .put("sys/class/dmi/id/product_name", "ThinkCentre M720q\n");
    assert_eq!(
        blank_tree.host(no_machine).info().model.as_deref(),
        Some("LENOVO ThinkCentre M720q")
    );

    let vendor_only = Fixture::new();
    vendor_only.put("sys/class/dmi/id/sys_vendor", "QEMU\n");
    assert_eq!(
        vendor_only.host(no_machine).info().model.as_deref(),
        Some("QEMU")
    );

    assert_eq!(Fixture::new().host(no_machine).info().model, None);
}

#[test]
fn os_is_pretty_name_with_its_quotes_removed() {
    assert_eq!(
        pretty_name("PRETTY_NAME=\"Debian 12\"\n").as_deref(),
        Some("Debian 12")
    );
    assert_eq!(
        pretty_name("PRETTY_NAME='Raspbian'\n").as_deref(),
        Some("Raspbian")
    );
    assert_eq!(
        pretty_name("PRETTY_NAME=Alpine\n").as_deref(),
        Some("Alpine")
    );
    assert_eq!(pretty_name("NAME=\"Debian\"\n"), None);
    assert_eq!(pretty_name("PRETTY_NAME=\"\"\n"), None);
}

#[test]
fn the_cpu_is_the_first_model_name_and_the_processor_count() {
    let arm = "processor\t: 0\nBogoMIPS\t: 108.00\n\nprocessor\t: 1\n\n\
               processor\t: 2\n\nprocessor\t: 3\nModel\t\t: Raspberry Pi 5\n";
    assert_eq!(cpu_model(arm), None, "no model name line on this ARM");
    assert_eq!(cpu_cores(arm), Some(4));
    assert_eq!(
        cpu_model("model name : First\nmodel name : Second\n").as_deref(),
        Some("First")
    );
    assert_eq!(cpu_cores("model name : X\n"), None);
}

#[test]
fn memory_is_mem_total_from_kb_to_bytes() {
    assert_eq!(mem_total_bytes("MemTotal:  16 kB\n"), Some(16 * 1024));
    assert_eq!(
        mem_total_bytes("MemTotal:  16\n"),
        None,
        "no unit, no guess"
    );
    assert_eq!(mem_total_bytes("MemTotal:  lots kB\n"), None);
}

#[test]
fn uptime_is_whole_seconds_of_the_first_field() {
    assert_eq!(uptime_s("12345.99 49000.10\n"), Some(12345));
    assert_eq!(uptime_s("7 1\n"), Some(7));
    assert_eq!(uptime_s("soon 1\n"), None);
}

/// The real sources answer on the Linux host the tests run on: the
/// machine name through `uname(2)`, and procfs.
#[test]
fn the_local_host_answers_from_uname_and_procfs() {
    let info = Host::local().info();
    assert!(info.architecture.is_some(), "uname(2) answered no machine");
    assert!(info.kernel.is_some(), "procfs answered no kernel release");
    assert!(info.uptime_s.is_some(), "procfs answered no uptime");
}

// ----- get_system_version -----

#[tokio::test]
async fn the_version_reads_the_installed_record() {
    let f = Fixture::x86();
    assert_eq!(
        answer(&f.plugin(no_machine), VERB_VERSION).await,
        json!({
            "v": 1,
            "release": "0.1.13.1",
            "build": BUILD,
            "architecture": "x86_64-unknown-linux-gnu",
            "built_at_utc": "2026-10-28T09:00:00Z",
        })
    );
}

/// Pin: `release` is the record's `release` key or null. It is
/// never the record's `version`, never this crate's build, and
/// never a health endpoint's version.
#[tokio::test]
async fn release_is_never_the_cargo_version_or_the_health_version() {
    let only_version = Fixture::new();
    only_version.put(INSTALLED_RECORD, &format!("version = \"{BUILD}\"\n"));
    let v = answer(&only_version.plugin(no_machine), VERB_VERSION).await;
    assert_eq!(v["release"], Value::Null, "the key version is not release");
    assert_eq!(v["build"], BUILD);

    let not_text = Fixture::new();
    not_text.put(INSTALLED_RECORD, "release = 13\narchitecture = true\n");
    let v = not_text.host(no_machine).version();
    assert_eq!((v.release, v.architecture), (None, None));

    let broken = Fixture::new();
    broken.put(INSTALLED_RECORD, "release = \"0.1.13.1\n");
    assert_eq!(broken.host(no_machine).version().release, None);

    let lib = include_str!("lib.rs");
    assert!(!lib.contains("\"version\""), "lib.rs reads a version key");
    assert!(!lib.contains("/health"), "lib.rs reaches a health endpoint");
    assert!(!manifest().prerequisites.outbound_network);
}

// ----- neither verb writes -----

fn snapshot(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, SystemTime)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = std::fs::metadata(&path).unwrap();
            if meta.is_dir() {
                stack.push(path.clone());
                out.insert(path, (Vec::new(), meta.modified().unwrap()));
            } else {
                let bytes = std::fs::read(&path).unwrap();
                out.insert(path, (bytes, meta.modified().unwrap()));
            }
        }
    }
    out
}

/// Pin: neither verb writes. The host tree is the same, byte for
/// byte and time for time, after both verbs as before; and no
/// source this crate ships names a write.
#[tokio::test]
async fn neither_verb_writes() {
    let f = Fixture::x86();
    let before = snapshot(f.dir.path());
    let p = f.plugin(x86_64);
    for _ in 0..2 {
        answer(&p, VERB_INFO).await;
        answer(&p, VERB_VERSION).await;
    }
    assert_eq!(snapshot(f.dir.path()), before, "the host tree moved");

    let empty = Fixture::new();
    let p = empty.plugin(no_machine);
    answer(&p, VERB_INFO).await;
    answer(&p, VERB_VERSION).await;
    assert!(
        snapshot(empty.dir.path()).is_empty(),
        "a verb created a file"
    );

    for (file, text) in [
        ("src/lib.rs", include_str!("lib.rs")),
        (
            "src/bin/system-info-wire.rs",
            include_str!("bin/system-info-wire.rs"),
        ),
    ] {
        for token in [
            "fs::write",
            "File::create",
            "OpenOptions",
            "create_dir",
            "remove_file",
            "remove_dir",
            "rename(",
            "set_permissions",
            "announce",
            "update_state",
        ] {
            assert!(!text.contains(token), "{file} names {token:?}");
        }
    }
    let cargo = include_str!("../Cargo.toml");
    assert!(
        !cargo.lines().any(|l| {
            let l = l.trim_start();
            l.starts_with("evo ")
                || l.starts_with("evo=")
                || l.starts_with("evo.")
        }),
        "the crate depends on the steward crate"
    );
}

// ----- refusals -----

#[tokio::test]
async fn an_unknown_payload_version_or_verb_is_refused() {
    let p = Fixture::new().plugin(no_machine);
    for r in [
        req(VERB_INFO, json!({ "v": 2 })),
        req(VERB_VERSION, json!({ "v": 2 })),
        req("get_system_mood", json!({ "v": 1 })),
    ] {
        assert!(
            matches!(
                p.handle_request(&r).await,
                Err(PluginError::Permanent(_))
            ),
            "{} must be refused",
            r.request_type
        );
    }
}
