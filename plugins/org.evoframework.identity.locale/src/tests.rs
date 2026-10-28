// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Tests for the identity.locale plugin, against a state directory
//! in a temporary directory.

use serde_json::{json, Value};

use super::*;

/// A plugin booted the way load boots it, over a state file in
/// `dir`, with the glass's catalogues unless others are given.
async fn boot(dir: &Path, catalogues: Option<&[&str]>) -> LocalePlugin {
    let mut p = LocalePlugin::new().with_state_path(dir.join(STATE_FILENAME));
    if let Some(c) = catalogues {
        p.catalogues = c.iter().map(|s| (*s).to_string()).collect();
    }
    p.restore().await;
    p.loaded = true;
    p
}

fn req(payload: Value) -> Request {
    Request {
        request_type: VERB_LANGUAGE_SET.to_string(),
        payload: payload.to_string().into_bytes(),
        correlation_id: 1,
        deadline: None,
        instance_id: None,
        principal_scope: None,
        has_step_up: false,
    }
}

async fn set(p: &LocalePlugin, language: &str) -> Value {
    let resp = p
        .handle_request(&req(json!({ "v": 1, "language": language })))
        .await
        .expect("a set answers; a refused tag is not an error");
    serde_json::from_slice(&resp.payload).unwrap()
}

fn stored(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join(STATE_FILENAME)).ok()
}

// ----- manifest / surface -----

/// Both manifests stock `system.locale` with `language.set` only,
/// and no capability gate: changing the language asks for no
/// password.
#[test]
fn both_manifests_stock_system_locale_with_an_ungated_language_set() {
    let oop = Manifest::from_toml(include_str!("../manifest.oop.toml"))
        .expect("manifest.oop.toml parses");
    for m in [manifest(), oop] {
        assert_eq!(m.plugin.name, PLUGIN_NAME);
        assert_eq!(m.target.shelf, "system.locale");
        assert_eq!(m.target.shape, 1);
        let resp = m.capabilities.respondent.expect("respondent");
        assert_eq!(resp.request_types, vec![VERB_LANGUAGE_SET]);
        assert!(
            resp.verb_capabilities.is_empty(),
            "no gate on language.set: {:?}",
            resp.verb_capabilities
        );
    }
}

#[tokio::test]
async fn describe_names_the_manifest_verb() {
    let d = LocalePlugin::new().describe().await;
    let declared = manifest().capabilities.respondent.unwrap().request_types;
    assert_eq!(d.runtime_capabilities.request_types, declared);
}

// ----- language.set -----

/// Pin: a fresh plugin answers `en`. Before any set, and with
/// nothing stored, the language in force is `en`; a refused tag
/// on a fresh plugin answers `en` and creates no file.
#[tokio::test]
async fn a_fresh_plugin_answers_en() {
    assert_eq!(DEFAULT_LANGUAGE, "en");
    let dir = tempfile::tempdir().unwrap();
    let p = boot(dir.path(), None).await;
    assert_eq!(*p.language.lock().await, "en");
    assert_eq!(set(&p, "fr").await, json!({ "v": 1, "language": "en" }));
    assert_eq!(stored(dir.path()), None, "a refused tag writes nothing");
}

/// `en` applies, is stored, and is answered; tags compare without
/// regard to case and the catalogue's spelling is stored.
#[tokio::test]
async fn en_applies_and_is_stored() {
    let dir = tempfile::tempdir().unwrap();
    let p = boot(dir.path(), None).await;
    assert_eq!(set(&p, " EN ").await, json!({ "v": 1, "language": "en" }));
    assert_eq!(stored(dir.path()).as_deref(), Some("language = \"en\"\n"));
    assert!(!dir.path().join("language.toml.tmp").exists());
}

/// Pin: no tag other than `en` is stored. Every other tag answers
/// the language still in force, not an error, and leaves the stored
/// file byte for byte and time for time as it was.
#[tokio::test]
async fn no_tag_other_than_en_is_stored() {
    let dir = tempfile::tempdir().unwrap();
    let p = boot(dir.path(), None).await;
    set(&p, "en").await;
    let path = dir.path().join(STATE_FILENAME);
    let before = (
        std::fs::read(&path).unwrap(),
        std::fs::metadata(&path).unwrap().modified().unwrap(),
    );
    for tag in [
        "fr", "de-DE", "en-US", "en-GB", "zh-Hans", "", "  ", "english",
    ] {
        assert_eq!(
            set(&p, tag).await,
            json!({ "v": 1, "language": "en" }),
            "{tag:?}"
        );
    }
    let after = (
        std::fs::read(&path).unwrap(),
        std::fs::metadata(&path).unwrap().modified().unwrap(),
    );
    assert_eq!(after, before, "the stored file moved");
}

/// The stored language survives a restart. With a second catalogue
/// so a stored language can differ from the default: a set stores
/// `de`, a plugin booted again over the same state directory has
/// `de` in force, and a refused tag after the restart answers `de`.
#[tokio::test]
async fn the_stored_language_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let first = boot(dir.path(), Some(&["en", "de"])).await;
    assert_eq!(set(&first, "de").await["language"], "de");
    drop(first);
    let again = boot(dir.path(), Some(&["en", "de"])).await;
    assert_eq!(*again.language.lock().await, "de");
    assert_eq!(set(&again, "fr").await["language"], "de");
}

/// A stored tag the glass has no catalogue for, a file that does
/// not parse, and an empty file are all no stored language: `en` is
/// in force, and nothing is rewritten.
#[tokio::test]
async fn a_stored_tag_without_a_catalogue_is_no_stored_language() {
    for body in [
        "language = \"fr\"\n",
        "language = [\n",
        "",
        "language = 7\n",
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(STATE_FILENAME), body).unwrap();
        let p = boot(dir.path(), None).await;
        assert_eq!(*p.language.lock().await, "en", "{body:?}");
        assert_eq!(
            stored(dir.path()).as_deref(),
            Some(body),
            "load rewrote the file"
        );
    }
}

/// A payload of the wrong version or shape is an error; that is a
/// malformed request, not a refused tag.
#[tokio::test]
async fn a_malformed_payload_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let p = boot(dir.path(), None).await;
    for payload in [
        json!({ "v": 2, "language": "en" }),
        json!({ "v": 1 }),
        json!({ "v": 1, "language": 7 }),
    ] {
        assert!(
            matches!(
                p.handle_request(&req(payload.clone())).await,
                Err(PluginError::Permanent(_))
            ),
            "{payload}"
        );
    }
    assert_eq!(stored(dir.path()), None);
}

/// The plugin carries no zone verb and needs no host grant.
#[test]
fn the_plugin_sets_the_language_only() {
    for (file, text) in [
        ("src/lib.rs", include_str!("lib.rs")),
        ("manifest.toml", include_str!("../manifest.toml")),
        ("manifest.oop.toml", include_str!("../manifest.oop.toml")),
    ] {
        for token in ["timezone", "zoneinfo", "localtime", "sudo"] {
            assert!(!text.contains(token), "{file} names {token:?}");
        }
    }
}
