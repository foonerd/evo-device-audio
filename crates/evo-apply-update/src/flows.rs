// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! End-to-end flows over fakes: a signed registry and carrier served from
//! a map, a host that records what it was asked, and real directories in a
//! temp tree. These are the tests that pin the updater's invariants.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signer, SigningKey};

use crate::apply_host::apply_host;
use crate::apply_plugin::{apply_plugin, Outcome};
use crate::archive::build_tar_gz;
use crate::check::run_check;
use crate::ctx::Ctx;
use crate::docs::{sha256_hex, Allow, Config};
use crate::error::{Res, UpdError};
use crate::fetch::Fetch;
use crate::grammar::{Channel, Request, HOST_COMPONENT};
use crate::host::Host;
use crate::roots::Roots;
use crate::state::{AppliedEntry, State};
use crate::trust::{test_key, TrustStore};
use crate::worker::{dispatch, run_token};

const TRIPLE: &str = "aarch64-unknown-linux-gnu";
const BASE: &str = "https://lab.example";
const NOW: u64 = 1_795_000_000;
const PLUGIN: &str = "org.evoframework.system.info";

#[derive(Default)]
struct FakeFetch(RefCell<HashMap<String, Vec<u8>>>);
impl Fetch for FakeFetch {
    fn get(&self, url: &str, max: u64) -> Res<Vec<u8>> {
        let b = self.0.borrow().get(url).cloned().ok_or_else(|| {
            UpdError::new("fetch_failed", format!("404 {url}"))
        })?;
        if b.len() as u64 > max {
            return Err(UpdError::new("fetch_too_large", "bound"));
        }
        Ok(b)
    }
}

struct FakeHost {
    ready: RefCell<Vec<bool>>,
    restarts: RefCell<u32>,
    started: RefCell<Vec<String>>,
    visudo_ok: RefCell<bool>,
}
impl Host for FakeHost {
    fn service_user(&self) -> Res<String> {
        Ok("svcuser".into())
    }
    fn restart_evo(&self) -> Res<()> {
        *self.restarts.borrow_mut() += 1;
        Ok(())
    }
    fn wait_plugin_ready(&self, _name: &str) -> bool {
        let mut r = self.ready.borrow_mut();
        if r.is_empty() {
            true
        } else {
            r.remove(0)
        }
    }
    fn visudo_check(&self, _path: &Path) -> Res<()> {
        if *self.visudo_ok.borrow() {
            Ok(())
        } else {
            Err(UpdError::new("sudoers_invalid", "bad"))
        }
    }
    fn start_unit(&self, instance: &str) -> Res<()> {
        self.started.borrow_mut().push(instance.to_string());
        Ok(())
    }
}

struct Fx {
    _dir: tempfile::TempDir,
    base: PathBuf,
    roots: Roots,
    state: State,
    cfg: Config,
    trust: TrustStore,
    allow: Allow,
    fetch: FakeFetch,
    host: FakeHost,
    sk: SigningKey,
    fp: String,
}

fn manifest(name: &str, version: &str) -> String {
    format!("[plugin]\nname = \"{name}\"\nversion = \"{version}\"\n")
}

fn bundle(name: &str, version: &str, bin: &[u8]) -> Vec<u8> {
    build_tar_gz(&[
        (
            &format!("{name}/manifest.toml"),
            manifest(name, version).as_bytes(),
            0o644,
        ),
        (&format!("{name}/manifest.sig"), b"sig", 0o644),
        (&format!("{name}/plugin.bin"), bin, 0o755),
        (&format!("{name}/privileges.yaml"), b"p: 1", 0o644),
    ])
}

impl Fx {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        let uid = fs::metadata(&base).unwrap().uid();
        let mut roots = Roots::system();
        roots.etc_dir = base.join("etc-updater");
        roots.state_dir = base.join("var-updater");
        roots.plugins_dir = base.join("opt/plugins");
        roots.rollback_dir = base.join("opt/rollback");
        roots.bin_dir = base.join("bin");
        roots.sudoers_dir = base.join("sudoers.d");
        roots.installed_record = base.join("evo-installed.toml");
        roots.owner_uid = uid;
        roots.chain_stop = base.clone();
        for d in [
            &roots.etc_dir,
            &roots.state_dir,
            &roots.plugins_dir,
            &roots.rollback_dir,
            &roots.bin_dir,
            &roots.sudoers_dir,
        ] {
            fs::create_dir_all(d).unwrap();
            fs::set_permissions(d, fs::Permissions::from_mode(0o755)).unwrap();
        }
        // The chain check walks to chain_stop; `opt` is a parent of two roots.
        fs::set_permissions(
            base.join("opt"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let (k, sk) = test_key("lab", 5, &["org.evoframework.*"]);
        let fp = k.fingerprint.clone();
        let cfg = Config {
            base_url: BASE.into(),
            ca_file: None,
            max_bundle_bytes: 1 << 20,
        };
        let allow: Allow = toml::from_str(
            r#"
[[file]]
kind = "bin"
name = "evo-set-timezone"
create = true
[[file]]
kind = "sudoers"
name = "evo-set-timezone"
create = true
[[file]]
kind = "sudoers"
name = "evo-update-only"
create = false
"#,
        )
        .unwrap();
        Self {
            state: State::new(&roots.state_dir),
            roots,
            cfg,
            trust: TrustStore::from_keys(vec![k]),
            allow,
            fetch: FakeFetch::default(),
            host: FakeHost {
                ready: RefCell::new(vec![]),
                restarts: RefCell::new(0),
                started: RefCell::new(vec![]),
                visudo_ok: RefCell::new(true),
            },
            sk,
            fp,
            base,
            _dir: dir,
        }
    }

    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            roots: &self.roots,
            state: &self.state,
            cfg: &self.cfg,
            trust: &self.trust,
            allow: &self.allow,
            fetch: &self.fetch,
            host: &self.host,
            now: NOW,
            triple: TRIPLE,
            arch: "aarch64",
        }
    }

    fn put(&self, rel: &str, bytes: Vec<u8>) {
        self.fetch
            .0
            .borrow_mut()
            .insert(format!("{BASE}/{rel}"), bytes);
    }

    fn install(&self, name: &str, version: &str, bin: &[u8]) {
        let d = self.roots.plugins_dir.join(name);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("manifest.toml"), manifest(name, version)).unwrap();
        fs::write(d.join("plugin.bin"), bin).unwrap();
    }

    fn plugin_state(&self, name: &str) -> PathBuf {
        let d = self.base.join("varlib/plugins").join(name).join("state");
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Publish a registry listing `(name, version, bundle bytes)`.
    fn publish(&self, plugins: &[(&str, &str, &[u8])]) {
        self.publish_with(
            plugins,
            &self.sk.clone(),
            &self.fp.clone(),
            "2099-01-01T00:00:00Z",
            &|n| n.to_string(),
        );
    }

    fn publish_with(
        &self,
        plugins: &[(&str, &str, &[u8])],
        signer: &SigningKey,
        fingerprint: &str,
        expires: &str,
        name_of: &dyn Fn(&str) -> String,
    ) {
        let mut t = format!(
            "[registry]\nname=\"evo-audio-prod\"\npublic_key_fingerprint=\"{fingerprint}\"\nschema_version=1\nexpires_at=\"{expires}\"\n"
        );
        for (name, ver, bytes) in plugins {
            let n = name_of(name);
            let url = format!("{BASE}/b/{n}-{ver}.tar.gz");
            self.fetch
                .0
                .borrow_mut()
                .insert(url.clone(), bytes.to_vec());
            t.push_str(&format!(
                "[[plugins]]\nname=\"{n}\"\narchitectures=[\"{TRIPLE}\"]\n[[plugins.versions]]\nversion=\"{ver}\"\nurl=\"{url}\"\nsha256=\"{}\"\n",
                sha256_hex(bytes)
            ));
        }
        let dir = format!("registries/evo-audio-prod/{TRIPLE}");
        self.put(&format!("{dir}/registry.toml"), t.clone().into_bytes());
        self.put(
            &format!("{dir}/registry.sig"),
            signer.sign(t.as_bytes()).to_bytes().to_vec(),
        );
    }

    /// Publish a host-files document and carrier.
    fn publish_host(
        &self,
        release: &str,
        files: &[(&str, &str, &[u8])],
        extra: &[(&str, &[u8])],
    ) {
        let mut entries: Vec<(String, Vec<u8>, u32)> = Vec::new();
        let mut t = String::new();
        let mut listed = String::new();
        for (kind, name, content) in files {
            let rel = if *kind == "bin" {
                format!("bin/{name}")
            } else {
                format!("sudoers.d/{name}.in")
            };
            entries.push((rel, content.to_vec(), 0o644));
            listed.push_str(&format!(
                "[[files]]\nkind=\"{kind}\"\nname=\"{name}\"\nsha256=\"{}\"\n",
                sha256_hex(content)
            ));
        }
        for (rel, content) in extra {
            entries.push((rel.to_string(), content.to_vec(), 0o644));
        }
        let refs: Vec<(&str, &[u8], u32)> = entries
            .iter()
            .map(|(a, b, c)| (a.as_str(), b.as_slice(), *c))
            .collect();
        let carrier = build_tar_gz(&refs);
        let url = format!("{BASE}/hf/carrier-{release}.tar.gz");
        self.fetch
            .0
            .borrow_mut()
            .insert(url.clone(), carrier.clone());
        t.push_str(&format!(
            "[host]\nrelease=\"{release}\"\narchitecture=\"aarch64\"\nbuilt_at_utc=\"2026-11-27T10:00:00Z\"\npublic_key_fingerprint=\"{}\"\nschema_version=1\nexpires_at=\"2099-01-01T00:00:00Z\"\nurl=\"{url}\"\nsha256=\"{}\"\n{listed}",
            self.fp,
            sha256_hex(&carrier)
        ));
        let dir = format!("registries/evo-audio-prod/{TRIPLE}");
        self.put(&format!("{dir}/host-files.toml"), t.clone().into_bytes());
        self.put(
            &format!("{dir}/host-files.sig"),
            self.sk.sign(t.as_bytes()).to_bytes().to_vec(),
        );
    }
}

fn items(v: &serde_json::Value) -> Vec<(String, String)> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["component"].as_str().unwrap().to_string(),
                i["reason"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

// ---- check ----------------------------------------------------------------

#[test]
fn a_newer_version_is_offered_and_a_current_one_is_not() {
    let f = Fx::new();
    f.install(PLUGIN, "0.1.13", b"old");
    f.install("org.evoframework.alarm", "0.1.2", b"a");
    f.publish(&[
        (PLUGIN, "0.1.14", &bundle(PLUGIN, "0.1.14", b"new")),
        (
            "org.evoframework.alarm",
            "0.1.2",
            &bundle("org.evoframework.alarm", "0.1.2", b"a"),
        ),
    ]);
    let inv = run_check(&f.ctx()).unwrap();
    let got = items(&inv);
    assert!(got.contains(&(PLUGIN.into(), "newer".into())), "{got:?}");
    // Same version, never applied by this worker: bytes unknown, offered once.
    assert!(
        got.contains(&(
            "org.evoframework.alarm".into(),
            "unknown_bytes".into()
        )),
        "{got:?}"
    );
}

#[test]
fn a_same_version_rebuild_is_offered_when_the_hash_differs_and_not_when_it_matches(
) {
    let f = Fx::new();
    f.install(PLUGIN, "0.1.13", b"x");
    let b1 = bundle(PLUGIN, "0.1.13", b"v1");
    f.publish(&[(PLUGIN, "0.1.13", &b1)]);
    let mut a = f.state.applied();
    a.plugins.insert(
        PLUGIN.into(),
        AppliedEntry {
            version: "0.1.13".into(),
            sha256: sha256_hex(&b1),
            applied_at_utc: "t".into(),
        },
    );
    f.state.write_applied(&a).unwrap();
    assert!(
        items(&run_check(&f.ctx()).unwrap()).is_empty(),
        "matching hash: current"
    );
    let b2 = bundle(PLUGIN, "0.1.13", b"v2 rebuilt");
    f.publish(&[(PLUGIN, "0.1.13", &b2)]);
    assert_eq!(
        items(&run_check(&f.ctx()).unwrap()),
        vec![(PLUGIN.into(), "rebuild".into())]
    );
}

#[test]
fn an_older_listed_version_and_a_plugin_not_installed_are_not_offered() {
    let f = Fx::new();
    f.install(PLUGIN, "0.1.14", b"x");
    f.publish(&[
        (PLUGIN, "0.1.13", &bundle(PLUGIN, "0.1.13", b"o")),
        (
            "org.evoframework.alarm",
            "9.9",
            &bundle("org.evoframework.alarm", "9.9", b"n"),
        ),
    ]);
    assert!(items(&run_check(&f.ctx()).unwrap()).is_empty());
}

#[test]
fn a_registry_signed_by_a_key_that_is_not_trusted_is_refused() {
    let f = Fx::new();
    f.install(PLUGIN, "0.1.13", b"x");
    let forger = SigningKey::from_bytes(&[66; 32]);
    f.publish_with(
        &[(PLUGIN, "0.1.14", &bundle(PLUGIN, "0.1.14", b"evil"))],
        &forger,
        &f.fp.clone(),
        "2099-01-01T00:00:00Z",
        &|n| n.into(),
    );
    assert_eq!(run_check(&f.ctx()).unwrap_err().code, "signature_invalid");
}

#[test]
fn a_registry_with_one_changed_byte_is_refused() {
    let f = Fx::new();
    f.install(PLUGIN, "0.1.13", b"x");
    f.publish(&[(PLUGIN, "0.1.14", &bundle(PLUGIN, "0.1.14", b"n"))]);
    let key =
        format!("{BASE}/registries/evo-audio-prod/{TRIPLE}/registry.toml");
    let mut body = f.fetch.0.borrow()[&key].clone();
    let at = body.windows(6).position(|w| w == b"0.1.14").unwrap();
    body[at + 5] = b'9';
    f.fetch.0.borrow_mut().insert(key, body);
    assert_eq!(run_check(&f.ctx()).unwrap_err().code, "signature_invalid");
}

#[test]
fn an_expired_registry_and_a_pin_of_another_key_are_refused() {
    let f = Fx::new();
    f.install(PLUGIN, "0.1.13", b"x");
    let b = bundle(PLUGIN, "0.1.14", b"n");
    f.publish_with(
        &[(PLUGIN, "0.1.14", &b)],
        &f.sk.clone(),
        &f.fp.clone(),
        "2001-01-01T00:00:00Z",
        &|n| n.into(),
    );
    assert_eq!(run_check(&f.ctx()).unwrap_err().code, "document_expired");
    f.publish_with(
        &[(PLUGIN, "0.1.14", &b)],
        &f.sk.clone(),
        "sha256:00",
        "2099-01-01T00:00:00Z",
        &|n| n.into(),
    );
    assert_eq!(
        run_check(&f.ctx()).unwrap_err().code,
        "fingerprint_mismatch"
    );
}

#[test]
fn a_plugin_the_key_may_not_sign_for_is_never_listed() {
    let f = Fx::new();
    f.install("com.other.thing", "1.0", b"x");
    f.publish_with(
        &[(
            "com.other.thing",
            "2.0",
            &bundle("com.other.thing", "2.0", b"n"),
        )],
        &f.sk.clone(),
        &f.fp.clone(),
        "2099-01-01T00:00:00Z",
        &|n| n.into(),
    );
    assert!(items(&run_check(&f.ctx()).unwrap()).is_empty());
}

#[test]
fn a_check_changes_nothing_on_the_box() {
    let f = Fx::new();
    f.install(PLUGIN, "0.1.13", b"x");
    f.publish(&[(PLUGIN, "0.1.14", &bundle(PLUGIN, "0.1.14", b"n"))]);
    run_check(&f.ctx()).unwrap();
    assert_eq!(
        fs::read(f.roots.plugins_dir.join(PLUGIN).join("plugin.bin")).unwrap(),
        b"x"
    );
    assert_eq!(*f.host.restarts.borrow(), 0);
}

// ---- apply a plugin ---------------------------------------------------------

#[test]
fn an_apply_replaces_the_bundle_keeps_the_old_one_and_never_touches_plugin_state(
) {
    let f = Fx::new();
    f.install(PLUGIN, "0.1.13", b"old-bin");
    let state = f.plugin_state(PLUGIN);
    fs::write(state.join("alarm.toml"), b"precious").unwrap();
    let before = fs::metadata(state.join("alarm.toml"))
        .unwrap()
        .modified()
        .unwrap();
    f.publish(&[(PLUGIN, "0.1.14", &bundle(PLUGIN, "0.1.14", b"new-bin"))]);

    let (outcome, _) = apply_plugin(&f.ctx(), PLUGIN, "0.1.14").unwrap();
    assert_eq!(outcome, Outcome::Applied);
    let live = f.roots.plugins_dir.join(PLUGIN);
    assert_eq!(fs::read(live.join("plugin.bin")).unwrap(), b"new-bin");
    assert_eq!(
        fs::metadata(live.join("plugin.bin"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0o111
    );
    assert_eq!(*f.host.restarts.borrow(), 1);

    let kept: Vec<_> = fs::read_dir(&f.roots.rollback_dir)
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(kept.len(), 1);
    assert_eq!(
        fs::read(kept[0].path().join("plugin.bin")).unwrap(),
        b"old-bin"
    );

    assert_eq!(fs::read(state.join("alarm.toml")).unwrap(), b"precious");
    assert_eq!(
        fs::metadata(state.join("alarm.toml"))
            .unwrap()
            .modified()
            .unwrap(),
        before
    );
    let leftovers: Vec<_> = fs::read_dir(&f.roots.plugins_dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(
        leftovers.len(),
        1,
        "no .new/.old tree is left behind: {leftovers:?}"
    );
    assert_eq!(f.state.applied().plugins[PLUGIN].version, "0.1.14");
}

#[test]
fn a_bundle_that_does_not_come_up_is_rolled_back_and_not_recorded() {
    let f = Fx::new();
    f.install(PLUGIN, "0.1.13", b"old-bin");
    f.publish(&[(PLUGIN, "0.1.14", &bundle(PLUGIN, "0.1.14", b"bad-bin"))]);
    *f.host.ready.borrow_mut() = vec![false, true]; // new fails, old answers after restore
    let (outcome, _) = apply_plugin(&f.ctx(), PLUGIN, "0.1.14").unwrap();
    assert!(matches!(outcome, Outcome::RolledBack(_)), "{outcome:?}");
    assert_eq!(
        fs::read(f.roots.plugins_dir.join(PLUGIN).join("plugin.bin")).unwrap(),
        b"old-bin"
    );
    assert_eq!(*f.host.restarts.borrow(), 2);
    assert!(
        f.state.applied().plugins.is_empty(),
        "a rolled-back apply is not recorded as applied"
    );
}

#[test]
fn a_bundle_whose_hash_differs_from_the_signed_manifest_changes_nothing() {
    let f = Fx::new();
    f.install(PLUGIN, "0.1.13", b"old-bin");
    f.publish(&[(PLUGIN, "0.1.14", &bundle(PLUGIN, "0.1.14", b"new-bin"))]);
    f.fetch.0.borrow_mut().insert(
        format!("{BASE}/b/{PLUGIN}-0.1.14.tar.gz"),
        bundle(PLUGIN, "0.1.14", b"swapped"),
    );
    assert_eq!(
        apply_plugin(&f.ctx(), PLUGIN, "0.1.14").unwrap_err().code,
        "hash_mismatch"
    );
    assert_eq!(
        fs::read(f.roots.plugins_dir.join(PLUGIN).join("plugin.bin")).unwrap(),
        b"old-bin"
    );
    assert_eq!(*f.host.restarts.borrow(), 0);
}

#[test]
fn a_bundle_that_names_another_plugin_or_version_is_refused() {
    let f = Fx::new();
    f.install(PLUGIN, "0.1.13", b"old");
    let wrong_name = build_tar_gz(&[
        (
            &format!("{PLUGIN}/manifest.toml"),
            manifest("org.evoframework.alarm", "0.1.14").as_bytes(),
            0o644,
        ),
        (&format!("{PLUGIN}/manifest.sig"), b"s", 0o644),
        (&format!("{PLUGIN}/plugin.bin"), b"n", 0o755),
        (&format!("{PLUGIN}/privileges.yaml"), b"p", 0o644),
    ]);
    f.publish(&[(PLUGIN, "0.1.14", &wrong_name)]);
    assert_eq!(
        apply_plugin(&f.ctx(), PLUGIN, "0.1.14").unwrap_err().code,
        "bundle_name_mismatch"
    );
    f.publish(&[(
        PLUGIN,
        "0.1.14",
        &bundle("org.evoframework.alarm", "0.1.14", b"n"),
    )]);
    assert_eq!(
        apply_plugin(&f.ctx(), PLUGIN, "0.1.14").unwrap_err().code,
        "bundle_incomplete",
        "a bundle under another top directory is not this plugin"
    );
    f.publish(&[(PLUGIN, "0.1.14", &bundle(PLUGIN, "0.1.99", b"n"))]);
    assert_eq!(
        apply_plugin(&f.ctx(), PLUGIN, "0.1.14").unwrap_err().code,
        "bundle_version_mismatch"
    );
    let incomplete = build_tar_gz(&[(
        &format!("{PLUGIN}/manifest.toml"),
        manifest(PLUGIN, "0.1.14").as_bytes(),
        0o644,
    )]);
    f.publish(&[(PLUGIN, "0.1.14", &incomplete)]);
    assert_eq!(
        apply_plugin(&f.ctx(), PLUGIN, "0.1.14").unwrap_err().code,
        "bundle_incomplete"
    );
    assert_eq!(
        fs::read(f.roots.plugins_dir.join(PLUGIN).join("plugin.bin")).unwrap(),
        b"old"
    );
}

#[test]
fn only_the_offered_version_of_an_installed_plugin_can_be_applied() {
    let f = Fx::new();
    f.install(PLUGIN, "0.1.13", b"old");
    f.publish(&[(PLUGIN, "0.1.14", &bundle(PLUGIN, "0.1.14", b"n"))]);
    assert_eq!(
        apply_plugin(&f.ctx(), PLUGIN, "0.1.12").unwrap_err().code,
        "not_offered"
    );
    assert_eq!(
        apply_plugin(&f.ctx(), "org.evoframework.alarm", "1.0")
            .unwrap_err()
            .code,
        "not_installed"
    );
}

#[test]
fn an_apply_refuses_when_a_root_is_not_root_owned() {
    let mut f = Fx::new();
    f.install(PLUGIN, "0.1.13", b"old");
    f.publish(&[(PLUGIN, "0.1.14", &bundle(PLUGIN, "0.1.14", b"n"))]);
    f.roots.owner_uid += 1;
    assert_eq!(
        apply_plugin(&f.ctx(), PLUGIN, "0.1.14").unwrap_err().code,
        "root_not_owned"
    );
    assert_eq!(
        fs::read(f.roots.plugins_dir.join(PLUGIN).join("plugin.bin")).unwrap(),
        b"old"
    );
}

#[test]
fn a_bundle_url_outside_the_configured_base_is_refused() {
    let f = Fx::new();
    f.install(PLUGIN, "0.1.13", b"old");
    let b = bundle(PLUGIN, "0.1.14", b"n");
    let t = format!(
        "[registry]\nname=\"x\"\npublic_key_fingerprint=\"{}\"\nschema_version=1\nexpires_at=\"2099-01-01T00:00:00Z\"\n[[plugins]]\nname=\"{PLUGIN}\"\narchitectures=[\"{TRIPLE}\"]\n[[plugins.versions]]\nversion=\"0.1.14\"\nurl=\"https://evil.example/b.tar.gz\"\nsha256=\"{}\"\n",
        f.fp,
        sha256_hex(&b)
    );
    f.put(
        &format!("registries/evo-audio-prod/{TRIPLE}/registry.toml"),
        t.clone().into_bytes(),
    );
    f.put(
        &format!("registries/evo-audio-prod/{TRIPLE}/registry.sig"),
        f.sk.sign(t.as_bytes()).to_bytes().to_vec(),
    );
    assert_eq!(
        apply_plugin(&f.ctx(), PLUGIN, "0.1.14").unwrap_err().code,
        "url_outside_base"
    );
}

#[test]
fn an_interrupted_swap_is_recovered_on_the_next_run() {
    let f = Fx::new();
    // A crash between the two renames: the live path is gone, the old tree is aside.
    let aside = f.roots.plugins_dir.join(format!(".{PLUGIN}.old-77"));
    fs::create_dir_all(&aside).unwrap();
    fs::write(aside.join("manifest.toml"), manifest(PLUGIN, "0.1.13")).unwrap();
    fs::write(aside.join("plugin.bin"), b"old").unwrap();
    f.publish(&[(PLUGIN, "0.1.14", &bundle(PLUGIN, "0.1.14", b"n"))]);
    let (o, _) = apply_plugin(&f.ctx(), PLUGIN, "0.1.14").unwrap();
    assert_eq!(o, Outcome::Applied);
}

#[test]
fn rollback_copies_are_pruned_to_the_newest_two() {
    let f = Fx::new();
    for stamp in ["20260101T000000Z", "20260102T000000Z", "20260103T000000Z"] {
        fs::create_dir_all(
            f.roots.rollback_dir.join(format!("{PLUGIN}-{stamp}")),
        )
        .unwrap();
    }
    fs::create_dir_all(f.roots.rollback_dir.join("I4-2026-11-27")).unwrap();
    f.install(PLUGIN, "0.1.13", b"old");
    f.publish(&[(PLUGIN, "0.1.14", &bundle(PLUGIN, "0.1.14", b"n"))]);
    apply_plugin(&f.ctx(), PLUGIN, "0.1.14").unwrap();
    let mut names: Vec<String> = fs::read_dir(&f.roots.rollback_dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    assert_eq!(
        names.len(),
        3,
        "two plugin copies and the foreign directory: {names:?}"
    );
    assert!(
        names.contains(&"I4-2026-11-27".to_string()),
        "a directory the worker did not make is never pruned"
    );
    assert!(!names.contains(&format!("{PLUGIN}-20260101T000000Z")));
}

// ---- host files and installed.toml -----------------------------------------

const WRAPPER: &[u8] = b"#!/bin/sh\necho tz\n";
const SUDOERS_IN: &[u8] = b"Cmnd_Alias X = /usr/local/bin/evo-set-timezone\n@EVO_SERVICE_USER@ ALL=(root) NOPASSWD: X\n";

#[test]
fn host_files_are_installed_rendered_and_the_record_comes_from_the_signed_document(
) {
    let f = Fx::new();
    f.publish_host(
        "0.1.13.3",
        &[
            ("bin", "evo-set-timezone", WRAPPER),
            ("sudoers", "evo-set-timezone", SUDOERS_IN),
        ],
        &[],
    );
    let v = apply_host(&f.ctx(), "0.1.13.3").unwrap();
    let w = f.roots.bin_dir.join("evo-set-timezone");
    assert_eq!(fs::read(&w).unwrap(), WRAPPER);
    assert_eq!(
        fs::metadata(&w).unwrap().permissions().mode() & 0o7777,
        0o755
    );
    let s = f.roots.sudoers_dir.join("evo-set-timezone");
    let text = fs::read_to_string(&s).unwrap();
    assert!(
        text.contains("svcuser ALL=(root) NOPASSWD: X") && !text.contains('@'),
        "{text}"
    );
    assert_eq!(
        fs::metadata(&s).unwrap().permissions().mode() & 0o7777,
        0o440
    );
    let rec: toml::Table = fs::read_to_string(&f.roots.installed_record)
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(rec["release"].as_str(), Some("0.1.13.3"));
    assert_eq!(rec["architecture"].as_str(), Some("aarch64"));
    assert_eq!(rec["built_at_utc"].as_str(), Some("2026-11-27T10:00:00Z"));
    assert_eq!(rec.len(), 3, "no other key is ever written");
    assert_eq!(v["installed_record"], "written");
    assert!(f.state.applied().host.is_some());
}

#[test]
fn a_file_the_allow_list_does_not_name_is_refused_and_nothing_is_written() {
    let f = Fx::new();
    f.publish_host(
        "0.1.13.3",
        &[
            ("bin", "evo-set-timezone", WRAPPER),
            ("bin", "evo-sneaky", b"#!/bin/sh\n"),
            ("sudoers", "evo-set-timezone", SUDOERS_IN),
        ],
        &[],
    );
    assert_eq!(
        apply_host(&f.ctx(), "0.1.13.3").unwrap_err().code,
        "file_not_allowed"
    );
    assert!(
        fs::read_dir(&f.roots.bin_dir).unwrap().next().is_none(),
        "not even the allowed wrapper is installed"
    );
    assert!(!f.roots.installed_record.exists());
}

#[test]
fn a_carrier_file_the_signed_document_does_not_list_is_refused() {
    let f = Fx::new();
    f.publish_host(
        "0.1.13.3",
        &[("bin", "evo-set-timezone", WRAPPER)],
        &[("bin/evo-extra", b"x")],
    );
    assert_eq!(
        apply_host(&f.ctx(), "0.1.13.3").unwrap_err().code,
        "carrier_unlisted_file"
    );
    assert!(fs::read_dir(&f.roots.bin_dir).unwrap().next().is_none());
}

#[test]
fn a_carrier_file_that_differs_from_its_signed_hash_is_refused() {
    let f = Fx::new();
    f.publish_host("0.1.13.3", &[("bin", "evo-set-timezone", WRAPPER)], &[]);
    // Re-pack the carrier with different bytes but keep the signed document.
    let url = format!("{BASE}/hf/carrier-0.1.13.3.tar.gz");
    let other =
        build_tar_gz(&[("bin/evo-set-timezone", b"#!/bin/sh\nevil\n", 0o644)]);
    f.fetch.0.borrow_mut().insert(url, other);
    assert_eq!(
        apply_host(&f.ctx(), "0.1.13.3").unwrap_err().code,
        "hash_mismatch"
    );
}

#[test]
fn a_sudoers_file_that_does_not_parse_installs_nothing() {
    let f = Fx::new();
    f.publish_host(
        "0.1.13.3",
        &[
            ("bin", "evo-set-timezone", WRAPPER),
            ("sudoers", "evo-set-timezone", SUDOERS_IN),
        ],
        &[],
    );
    *f.host.visudo_ok.borrow_mut() = false;
    assert_eq!(
        apply_host(&f.ctx(), "0.1.13.3").unwrap_err().code,
        "sudoers_invalid"
    );
    assert!(
        fs::read_dir(&f.roots.bin_dir).unwrap().next().is_none(),
        "no wrapper without its grant"
    );
    assert!(fs::read_dir(&f.roots.sudoers_dir).unwrap().next().is_none());
    assert!(!f.roots.installed_record.exists());
}

#[test]
fn a_drop_in_marked_replace_only_is_not_created_where_it_is_absent() {
    let f = Fx::new();
    f.publish_host(
        "0.1.13.3",
        &[
            ("bin", "evo-set-timezone", WRAPPER),
            ("sudoers", "evo-update-only", SUDOERS_IN),
        ],
        &[],
    );
    apply_host(&f.ctx(), "0.1.13.3").unwrap();
    assert!(!f.roots.sudoers_dir.join("evo-update-only").exists());
    fs::write(f.roots.sudoers_dir.join("evo-update-only"), "old\n").unwrap();
    f.state.write_applied(&Default::default()).unwrap();
    fs::remove_file(&f.roots.installed_record).unwrap();
    apply_host(&f.ctx(), "0.1.13.3").unwrap();
    assert!(
        fs::read_to_string(f.roots.sudoers_dir.join("evo-update-only"))
            .unwrap()
            .contains("svcuser")
    );
}

#[test]
fn a_replaced_file_is_kept_in_the_rollback_directory() {
    let f = Fx::new();
    fs::write(f.roots.bin_dir.join("evo-set-timezone"), b"old wrapper")
        .unwrap();
    f.publish_host("0.1.13.3", &[("bin", "evo-set-timezone", WRAPPER)], &[]);
    apply_host(&f.ctx(), "0.1.13.3").unwrap();
    let kept: Vec<_> = fs::read_dir(&f.roots.rollback_dir)
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(kept.len(), 1);
    assert_eq!(
        fs::read(kept[0].path().join("bin-evo-set-timezone")).unwrap(),
        b"old wrapper"
    );
}

#[test]
fn an_older_release_is_not_offered_and_never_replaces_a_newer_record() {
    let f = Fx::new();
    fs::write(&f.roots.installed_record, "release = \"0.1.14\"\n").unwrap();
    f.publish_host("0.1.13.3", &[("bin", "evo-set-timezone", WRAPPER)], &[]);
    assert_eq!(
        apply_host(&f.ctx(), "0.1.13.3").unwrap_err().code,
        "not_offered",
        "no downgrade of the host layer"
    );
    assert!(!f.roots.bin_dir.join("evo-set-timezone").exists());
    // Defence in depth: the record writer itself keeps a newer record.
    let doc = crate::check::load_host_files(&f.ctx()).unwrap();
    assert_eq!(
        crate::apply_host::write_record(&f.ctx(), &doc).unwrap(),
        "kept_newer"
    );
    assert_eq!(
        fs::read_to_string(&f.roots.installed_record).unwrap(),
        "release = \"0.1.14\"\n"
    );
}

#[test]
fn host_files_are_offered_with_no_record_a_newer_release_or_a_missing_wrapper()
{
    let f = Fx::new();
    f.publish(&[]);
    f.publish_host("0.1.13.3", &[("bin", "evo-set-timezone", WRAPPER)], &[]);
    let r = |f: &Fx| {
        items(&run_check(&f.ctx()).unwrap())
            .into_iter()
            .find(|(c, _)| c == HOST_COMPONENT)
            .map(|x| x.1)
    };
    assert_eq!(r(&f), Some("no_record".into()));
    fs::write(&f.roots.installed_record, "release = \"0.1.13.3\"\n").unwrap();
    assert_eq!(r(&f), Some("drift".into()), "the wrapper is missing");
    fs::write(f.roots.bin_dir.join("evo-set-timezone"), WRAPPER).unwrap();
    assert_eq!(r(&f), None, "present and identical: current");
    fs::write(&f.roots.installed_record, "release = \"0.1.13.2\"\n").unwrap();
    assert_eq!(r(&f), Some("release_newer".into()));
}

#[test]
fn host_files_for_another_architecture_are_refused() {
    let mut f = Fx::new();
    f.publish_host("0.1.13.3", &[("bin", "evo-set-timezone", WRAPPER)], &[]);
    let _ = &mut f;
    let mut c = f.ctx();
    c.arch = "x86_64";
    assert_eq!(
        apply_host(&c, "0.1.13.3").unwrap_err().code,
        "architecture_mismatch"
    );
}

// ---- worker and request ------------------------------------------------------

#[test]
fn the_worker_records_how_each_run_ended() {
    let f = Fx::new();
    f.install(PLUGIN, "0.1.13", b"old");
    f.publish(&[(PLUGIN, "0.1.14", &bundle(PLUGIN, "0.1.14", b"n"))]);
    let (s, ok) = dispatch(&f.ctx(), &Request::Check);
    assert!(ok && s["state"] == "checked", "{s}");
    assert!(f.roots.state_dir.join("inventory.json").exists());
    let (s, ok) = dispatch(
        &f.ctx(),
        &Request::Apply {
            component: PLUGIN.into(),
            version: "0.1.14".into(),
        },
    );
    assert!(ok && s["state"] == "applied", "{s}");
    let (s, ok) = dispatch(
        &f.ctx(),
        &Request::Apply {
            component: PLUGIN.into(),
            version: "0.1.14".into(),
        },
    );
    assert!(
        !ok && s["state"] == "refused" && s["error"]["code"] == "not_offered",
        "{s}"
    );
    let hist =
        fs::read_to_string(f.roots.state_dir.join("history.jsonl")).unwrap();
    assert_eq!(
        hist.lines().count(),
        2,
        "applies and refusals are recorded, checks are not"
    );
    let (s, ok) = dispatch(&f.ctx(), &Request::Channel(Channel::Test));
    assert!(ok && s["state"] == "channel_set");
    assert_eq!(f.state.channel(), Channel::Test);
}

#[test]
fn a_rolled_back_apply_is_reported_as_rolled_back() {
    let f = Fx::new();
    f.install(PLUGIN, "0.1.13", b"old");
    f.publish(&[(PLUGIN, "0.1.14", &bundle(PLUGIN, "0.1.14", b"n"))]);
    *f.host.ready.borrow_mut() = vec![false, true];
    let (s, ok) = dispatch(
        &f.ctx(),
        &Request::Apply {
            component: PLUGIN.into(),
            version: "0.1.14".into(),
        },
    );
    assert!(!ok && s["state"] == "rolled_back", "{s}");
}

#[test]
fn a_token_that_did_not_come_through_the_wrapper_is_refused() {
    let f = Fx::new();
    for t in [
        "apply_../../etc_1.0",
        "shell",
        "apply_org.evoframework.x_1.0_x",
    ] {
        let (s, ok) = run_token(&f.ctx(), t);
        assert!(!ok && s["state"] == "refused", "{t}: {s}");
    }
}

#[test]
fn a_second_worker_while_one_runs_is_busy() {
    let f = Fx::new();
    fs::write(f.roots.state_dir.join("lock"), "1\n").unwrap();
    let (s, ok) = run_token(&f.ctx(), "check");
    assert!(!ok && s["error"]["code"] == "busy", "{s}");
}

#[test]
fn the_wrapper_starts_one_unit_for_a_good_request_and_none_for_a_bad_one() {
    let f = Fx::new();
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let v = crate::request::handle(
        &args(&["apply", PLUGIN, "0.1.14"]),
        &f.roots,
        &f.host,
    )
    .unwrap();
    assert_eq!(v["accepted"], true);
    assert_eq!(
        f.host.started.borrow().as_slice(),
        [format!("apply_{PLUGIN}_0.1.14")]
    );
    assert!(!f.roots.state_dir.join("status.json").exists(), "the wrapper runs in the steward's read-only sandbox and writes nothing");
    for bad in [&["apply", "../x", "1.0"][..], &["rm", "-rf"][..], &[][..]] {
        assert!(crate::request::handle(&args(bad), &f.roots, &f.host).is_err());
    }
    assert_eq!(
        f.host.started.borrow().len(),
        1,
        "a refused request starts nothing"
    );
    fs::write(f.roots.state_dir.join("lock"), "1\n").unwrap();
    assert_eq!(
        crate::request::handle(&args(&["check"]), &f.roots, &f.host)
            .unwrap_err()
            .code,
        "busy"
    );
}

#[test]
fn a_failure_part_way_through_the_install_puts_back_what_the_run_replaced() {
    let f = Fx::new();
    fs::write(f.roots.bin_dir.join("evo-set-timezone"), b"old wrapper")
        .unwrap();
    f.publish_host(
        "0.1.13.3",
        &[
            ("bin", "evo-set-timezone", WRAPPER),
            ("sudoers", "evo-set-timezone", SUDOERS_IN),
        ],
        &[],
    );
    // Wrappers go first. Make the sudoers directory refuse a new file so the
    // run fails after the wrapper was already replaced.
    fs::set_permissions(
        &f.roots.sudoers_dir,
        fs::Permissions::from_mode(0o555),
    )
    .unwrap();
    let err = apply_host(&f.ctx(), "0.1.13.3");
    fs::set_permissions(
        &f.roots.sudoers_dir,
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    if fs::File::create(f.roots.sudoers_dir.join(".probe")).is_ok()
        && err.is_ok()
    {
        // Running as a user that ignores directory permissions: nothing to prove here.
        return;
    }
    assert!(err.is_err());
    assert_eq!(
        fs::read(f.roots.bin_dir.join("evo-set-timezone")).unwrap(),
        b"old wrapper",
        "the wrapper is back"
    );
    assert!(
        !f.roots.installed_record.exists(),
        "no record from a failed run"
    );
    assert!(f.state.applied().host.is_none());
}

#[test]
fn host_rollback_directories_are_pruned_to_the_newest_two() {
    let f = Fx::new();
    for stamp in ["20260101T000000Z", "20260102T000000Z", "20260103T000000Z"] {
        fs::create_dir_all(
            f.roots.rollback_dir.join(format!("hostfiles-{stamp}")),
        )
        .unwrap();
    }
    fs::create_dir_all(f.roots.rollback_dir.join("hostfiles-by-hand")).unwrap();
    fs::write(f.roots.bin_dir.join("evo-set-timezone"), b"old").unwrap();
    f.publish_host("0.1.13.3", &[("bin", "evo-set-timezone", WRAPPER)], &[]);
    apply_host(&f.ctx(), "0.1.13.3").unwrap();
    let mut names: Vec<String> = fs::read_dir(&f.roots.rollback_dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    assert_eq!(names.len(), 3, "{names:?}");
    assert!(
        names.contains(&"hostfiles-by-hand".to_string()),
        "a directory the worker did not name is never pruned"
    );
    assert!(!names.contains(&"hostfiles-20260101T000000Z".to_string()));
}

#[test]
fn a_carrier_file_whose_own_hash_is_not_the_signed_one_is_refused_even_if_the_carrier_matches(
) {
    let f = Fx::new();
    // The carrier is consistent with the document's carrier hash, but one
    // file's own signed hash is not the file's content.
    f.publish_host("0.1.13.3", &[("bin", "evo-set-timezone", WRAPPER)], &[]);
    let dir = format!("registries/evo-audio-prod/{TRIPLE}");
    let key = format!("{BASE}/{dir}/host-files.toml");
    let text = String::from_utf8(f.fetch.0.borrow()[&key].clone()).unwrap();
    let want = sha256_hex(WRAPPER);
    let doc = text.replace(
        &format!("sha256=\"{want}\"\n"),
        &format!("sha256=\"{}\"\n", sha256_hex(b"something else")),
    );
    assert_ne!(doc, text, "the per-file hash was replaced");
    f.put(&format!("{dir}/host-files.toml"), doc.clone().into_bytes());
    f.put(
        &format!("{dir}/host-files.sig"),
        f.sk.sign(doc.as_bytes()).to_bytes().to_vec(),
    );
    assert_eq!(
        apply_host(&f.ctx(), "0.1.13.3").unwrap_err().code,
        "hash_mismatch"
    );
    assert!(fs::read_dir(&f.roots.bin_dir).unwrap().next().is_none());
}

#[test]
fn a_carrier_whose_bytes_are_not_the_signed_ones_is_refused_even_if_every_file_in_it_is_right(
) {
    let f = Fx::new();
    f.publish_host("0.1.13.3", &[("bin", "evo-set-timezone", WRAPPER)], &[]);
    // The same files, packed differently: only the carrier's own signed hash can tell.
    let url = format!("{BASE}/hf/carrier-0.1.13.3.tar.gz");
    let same_files_other_bytes =
        build_tar_gz(&[("bin/evo-set-timezone", WRAPPER, 0o755)]);
    assert_ne!(
        sha256_hex(&same_files_other_bytes),
        sha256_hex(&f.fetch.0.borrow()[&url])
    );
    f.fetch.0.borrow_mut().insert(url, same_files_other_bytes);
    assert_eq!(
        apply_host(&f.ctx(), "0.1.13.3").unwrap_err().code,
        "hash_mismatch"
    );
    assert!(fs::read_dir(&f.roots.bin_dir).unwrap().next().is_none());
}
