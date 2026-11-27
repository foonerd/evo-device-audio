// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! The two signed documents the worker reads, and the worker's own config
//! files. Parsing is separate from verifying: [`verify_document`] is the
//! one place a signature, a fingerprint pin and an expiry are checked.

use std::path::Path;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::error::{Res, UpdError};
use crate::timeutil;
use crate::trust::{TrustStore, TrustedKey};

/// SHA-256 of bytes, lower-case hex.
pub fn sha256_hex(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

/// `[registry]` header of `registry.toml` (the steward's `RegistryManifest`
/// schema 1, the fields the worker uses).
#[derive(Debug, Deserialize)]
pub struct RegistryHeader {
    /// Pinned key fingerprint, `sha256:<hex>`.
    pub public_key_fingerprint: String,
    /// Schema version; only 1 is read.
    pub schema_version: u32,
    /// `YYYY-MM-DDTHH:MM:SSZ`.
    pub expires_at: String,
}

/// One version of a plugin in the registry.
#[derive(Debug, Deserialize)]
pub struct VersionEntry {
    /// Dotted version.
    pub version: String,
    /// HTTPS bundle URL.
    pub url: String,
    /// SHA-256 of the bundle tarball.
    pub sha256: String,
}

/// One plugin in the registry.
#[derive(Debug, Deserialize)]
pub struct PluginEntry {
    /// Plugin name.
    pub name: String,
    /// Target triples listed.
    #[serde(default)]
    pub architectures: Vec<String>,
    /// Versions.
    #[serde(default)]
    pub versions: Vec<VersionEntry>,
}

/// `registry.toml`.
#[derive(Debug, Deserialize)]
pub struct Registry {
    /// Header.
    pub registry: RegistryHeader,
    /// Plugins.
    #[serde(default)]
    pub plugins: Vec<PluginEntry>,
}

/// `[host]` of `host-files.toml`.
#[derive(Debug, Deserialize)]
pub struct HostHeader {
    /// The release whose host layer this carrier is.
    pub release: String,
    /// Architecture written to the installed record.
    pub architecture: String,
    /// Build time written to the installed record.
    pub built_at_utc: String,
    /// Pinned key fingerprint.
    pub public_key_fingerprint: String,
    /// Schema version; only 1 is read.
    pub schema_version: u32,
    /// `YYYY-MM-DDTHH:MM:SSZ`.
    pub expires_at: String,
    /// HTTPS carrier URL.
    pub url: String,
    /// SHA-256 of the carrier tarball.
    pub sha256: String,
}

/// One file the carrier delivers.
#[derive(Debug, Deserialize)]
pub struct HostFile {
    /// `bin` or `sudoers`.
    pub kind: String,
    /// File name (the sudoers drop-in name, without `.in`).
    pub name: String,
    /// SHA-256 of the carrier's file content.
    pub sha256: String,
}

/// `host-files.toml`.
#[derive(Debug, Deserialize)]
pub struct HostFiles {
    /// Header.
    pub host: HostHeader,
    /// Files.
    #[serde(default)]
    pub files: Vec<HostFile>,
}

/// The worker's config, `/etc/evo-updater/updater.toml`.
#[derive(Debug, Deserialize)]
pub struct Config {
    /// HTTPS base of the plane; no trailing slash.
    pub base_url: String,
    /// Optional CA bundle for a lab origin (root-owned file).
    #[serde(default)]
    pub ca_file: Option<String>,
    /// Bundle size bound; default 64 MiB.
    #[serde(default = "default_max")]
    pub max_bundle_bytes: u64,
}

fn default_max() -> u64 {
    64 * 1024 * 1024
}

impl Config {
    /// Load and check a config file.
    pub fn load(path: &Path) -> Res<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            UpdError::new("config_unreadable", format!("updater.toml: {e}"))
        })?;
        let mut c: Config = toml::from_str(&text).map_err(|e| {
            UpdError::new("config_invalid", format!("updater.toml: {e}"))
        })?;
        c.base_url = c.base_url.trim_end_matches('/').to_string();
        if !c.base_url.starts_with("https://") || c.base_url.len() < 12 {
            return Err(UpdError::new(
                "config_invalid",
                "base_url must be https",
            ));
        }
        if !(1..=512 * 1024 * 1024).contains(&c.max_bundle_bytes) {
            return Err(UpdError::new(
                "config_invalid",
                "max_bundle_bytes out of range",
            ));
        }
        Ok(c)
    }
}

/// One allowed file (`allow.toml`).
#[derive(Debug, Deserialize)]
pub struct AllowedFile {
    /// `bin` or `sudoers`.
    pub kind: String,
    /// File name.
    pub name: String,
    /// May the worker create it when absent? If false it only replaces.
    #[serde(default)]
    pub create: bool,
}

/// `/etc/evo-updater/allow.toml`: the only files the worker may ever write
/// outside plugin bundles. Widening it is an installer change.
#[derive(Debug, Deserialize, Default)]
pub struct Allow {
    /// Allowed files.
    #[serde(default)]
    pub file: Vec<AllowedFile>,
}

impl Allow {
    /// Load the allow list.
    pub fn load(path: &Path) -> Res<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            UpdError::new("allow_unreadable", format!("allow.toml: {e}"))
        })?;
        toml::from_str(&text).map_err(|e| {
            UpdError::new("allow_invalid", format!("allow.toml: {e}"))
        })
    }

    /// The entry for a kind and name, if allowed.
    pub fn find(&self, kind: &str, name: &str) -> Option<&AllowedFile> {
        if !valid_file_name(name) {
            return None;
        }
        self.file.iter().find(|f| f.kind == kind && f.name == name)
    }
}

/// `^evo-[a-z0-9-]{1,60}$`.
pub fn valid_file_name(n: &str) -> bool {
    n.strip_prefix("evo-").is_some_and(|r| {
        (1..=60).contains(&r.len())
            && r.bytes().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-'
            })
    })
}

/// What a verified document says about who signed it and until when.
pub struct Verified<'a> {
    /// The trust key whose signature verified.
    pub key: &'a TrustedKey,
}

/// The single check for a signed document: the detached signature verifies
/// under a key in the store, that key may sign for `authority_name`, the
/// document's pinned fingerprint is that key's, and the document has not
/// expired. A document that fails any of these yields nothing.
pub fn verify_document<'a>(
    trust: &'a TrustStore,
    bytes: &[u8],
    sig: &[u8],
    pinned_fingerprint: &str,
    expires_at: &str,
    authority_name: &str,
    now: u64,
) -> Res<Verified<'a>> {
    let key = trust.verifying_key(bytes, sig).ok_or_else(|| {
        UpdError::new(
            "signature_invalid",
            "no trusted key verifies the document",
        )
    })?;
    if !key.allows(authority_name) {
        return Err(UpdError::new(
            "key_not_authorised",
            "the verifying key may not sign for this name",
        ));
    }
    if key.fingerprint != pinned_fingerprint {
        return Err(UpdError::new(
            "fingerprint_mismatch",
            "document pins a different key than the one that signed it",
        ));
    }
    match timeutil::parse_utc(expires_at) {
        Some(t) if t > now => Ok(Verified { key }),
        Some(_) => Err(UpdError::new(
            "document_expired",
            "document is past expires_at",
        )),
        None => Err(UpdError::new(
            "document_invalid",
            "expires_at is not a UTC time",
        )),
    }
}

/// Parse a registry (shape only; verification is separate).
pub fn parse_registry(bytes: &[u8]) -> Res<Registry> {
    let text = std::str::from_utf8(bytes).map_err(|_| {
        UpdError::new("document_invalid", "registry is not UTF-8")
    })?;
    let r: Registry = toml::from_str(text).map_err(|e| {
        UpdError::new("document_invalid", format!("registry: {e}"))
    })?;
    if r.registry.schema_version != 1 {
        return Err(UpdError::new(
            "document_invalid",
            "registry schema_version is not 1",
        ));
    }
    Ok(r)
}

/// Parse a host-files document (shape only).
pub fn parse_host_files(bytes: &[u8]) -> Res<HostFiles> {
    let text = std::str::from_utf8(bytes).map_err(|_| {
        UpdError::new("document_invalid", "host-files is not UTF-8")
    })?;
    let h: HostFiles = toml::from_str(text).map_err(|e| {
        UpdError::new("document_invalid", format!("host-files: {e}"))
    })?;
    if h.host.schema_version != 1 {
        return Err(UpdError::new(
            "document_invalid",
            "host-files schema_version is not 1",
        ));
    }
    Ok(h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust::test_key;
    use ed25519_dalek::Signer;

    fn store(
        prefixes: &[&str],
    ) -> (TrustStore, ed25519_dalek::SigningKey, String) {
        let (k, s) = test_key("t", 7, prefixes);
        let fp = k.fingerprint.clone();
        (TrustStore::from_keys(vec![k]), s, fp)
    }

    #[test]
    fn a_good_document_verifies() {
        let (t, s, fp) = store(&["org.evoframework.*"]);
        let sig = s.sign(b"doc").to_bytes();
        assert!(verify_document(
            &t,
            b"doc",
            &sig,
            &fp,
            "2099-01-01T00:00:00Z",
            "org.evoframework.x",
            1
        )
        .is_ok());
    }

    #[test]
    fn a_forged_changed_expired_or_misnamed_document_is_refused() {
        let (t, s, fp) = store(&["org.evoframework.*"]);
        let (_, other, _) = store(&["org.evoframework.*"]);
        let good = s.sign(b"doc").to_bytes();
        let forged = ed25519_dalek::SigningKey::from_bytes(&[9; 32])
            .sign(b"doc")
            .to_bytes();
        let _ = other;
        let exp = "2099-01-01T00:00:00Z";
        let code = |r: Res<Verified>| r.err().map(|e| e.code);
        assert_eq!(
            code(verify_document(
                &t,
                b"doc",
                &forged,
                &fp,
                exp,
                "org.evoframework.x",
                1
            )),
            Some("signature_invalid")
        );
        assert_eq!(
            code(verify_document(
                &t,
                b"dox",
                &good,
                &fp,
                exp,
                "org.evoframework.x",
                1
            )),
            Some("signature_invalid")
        );
        assert_eq!(
            code(verify_document(
                &t,
                b"doc",
                &good,
                &fp,
                exp,
                "com.other.x",
                1
            )),
            Some("key_not_authorised")
        );
        assert_eq!(
            code(verify_document(
                &t,
                b"doc",
                &good,
                "sha256:00",
                exp,
                "org.evoframework.x",
                1
            )),
            Some("fingerprint_mismatch")
        );
        assert_eq!(
            code(verify_document(
                &t,
                b"doc",
                &good,
                &fp,
                "2001-01-01T00:00:00Z",
                "org.evoframework.x",
                2_000_000_000
            )),
            Some("document_expired")
        );
        assert_eq!(
            code(verify_document(
                &t,
                b"doc",
                &good,
                &fp,
                "tomorrow",
                "org.evoframework.x",
                1
            )),
            Some("document_invalid")
        );
    }

    #[test]
    fn file_names_are_evo_dash_names_only() {
        assert!(valid_file_name("evo-set-timezone"));
        for n in [
            "set-timezone",
            "evo-",
            "evo-Set",
            "evo-a/b",
            "evo-a b",
            "../evo-a",
            "evo-a.in",
        ] {
            assert!(!valid_file_name(n), "{n}");
        }
    }

    #[test]
    fn config_requires_https_and_the_allow_list_is_exact() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("updater.toml");
        std::fs::write(&p, "base_url = \"http://x.example\"\n").unwrap();
        assert_eq!(Config::load(&p).unwrap_err().code, "config_invalid");
        std::fs::write(&p, "base_url = \"https://x.example/plane/\"\n")
            .unwrap();
        assert_eq!(
            Config::load(&p).unwrap().base_url,
            "https://x.example/plane"
        );
        let a: Allow = toml::from_str(
            "[[file]]\nkind=\"bin\"\nname=\"evo-rtc-wake\"\ncreate=true\n",
        )
        .unwrap();
        assert!(a.find("bin", "evo-rtc-wake").is_some());
        assert!(
            a.find("sudoers", "evo-rtc-wake").is_none(),
            "kind must match"
        );
        assert!(a.find("bin", "evo-other").is_none());
    }
}
