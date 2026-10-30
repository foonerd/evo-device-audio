// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Every plugin's privileges.yaml parses the way the steward parses it.
//!
//! At admission the steward reads `/opt/evo/plugins/<id>/privileges.yaml`
//! with `PrivilegesV1::from_yaml` and refuses the plugin when that
//! fails, so the plugin never gets a socket. These tests run the same
//! parse over every `plugins/*/privileges.yaml` in the tree, so a file
//! the steward cannot read fails here, before a bundle is built.

use std::path::{Path, PathBuf};

use evo_plugin_sdk::privileges::{PrivilegesError, PrivilegesV1};

fn plugins_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins")
}

/// Every `plugins/*/privileges.yaml`, in name order.
fn privileges_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(plugins_root())
        .expect("plugins/ is readable")
        .map(|entry| {
            entry
                .expect("plugins/ entry")
                .path()
                .join("privileges.yaml")
        })
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    files
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

#[test]
fn every_privileges_yaml_parses_as_the_steward_parses_it() {
    let files = privileges_files();
    assert!(!files.is_empty(), "no plugins/*/privileges.yaml found");
    let failures: Vec<String> = files
        .iter()
        .filter_map(|path| {
            PrivilegesV1::from_yaml(&read(path))
                .err()
                .map(|e| format!("{}: {e}", path.display()))
        })
        .collect();
    assert!(
        failures.is_empty(),
        "the steward would refuse {} of {} plugins at admission:\n{}",
        failures.len(),
        files.len(),
        failures.join("\n")
    );
}

/// The defect this pin exists for: `` `{ v: 1 }` `` in an unquoted
/// `need:` is a YAML mapping inside a plain scalar, and the steward
/// refuses the whole file.
#[test]
fn a_colon_in_a_plain_need_scalar_fails_the_steward_parse() {
    let path =
        plugins_root().join("org.evoframework.identity.locale/privileges.yaml");
    let text = read(&path);
    PrivilegesV1::from_yaml(&text).unwrap_or_else(|e| {
        panic!("{} must parse before the plant: {e}", path.display())
    });

    let plain_need = text
        .lines()
        .find(|line| {
            line.trim_start()
                .strip_prefix("need: ")
                .is_some_and(|value| !value.starts_with(['\'', '"', '>', '|']))
        })
        .expect("a plain need: scalar to plant into");
    let planted = text.replacen(
        plain_need,
        &format!("{plain_need} Then call `alarm.reschedule {{ v: 1 }}`."),
        1,
    );
    assert_ne!(planted, text, "the plant changed nothing");

    match PrivilegesV1::from_yaml(&planted) {
        Err(PrivilegesError::YamlParse(_)) => {}
        Err(other) => panic!("planted file failed, but not as a YAML parse error: {other}"),
        Ok(_) => panic!("a colon in a plain need: scalar parsed; the steward pin is not holding"),
    }
}
