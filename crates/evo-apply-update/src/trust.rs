// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! The worker's trust roots: Ed25519 public keys in a root-owned directory,
//! each with a sidecar naming the plugin names it may sign for (the same
//! format `/etc/evo/trust.d` uses). A key without a sidecar is not used.

use std::fs;
use std::path::Path;

use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::error::{io_err, Res, UpdError};

/// DER prefix of an Ed25519 SubjectPublicKeyInfo; 32 key bytes follow.
const SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// One trusted key.
#[derive(Debug, Clone)]
pub struct TrustedKey {
    /// File stem the key was loaded from.
    pub label: String,
    /// `sha256:<hex of the DER SPKI>`, the form manifests pin.
    pub fingerprint: String,
    key: VerifyingKey,
    name_prefixes: Vec<String>,
}

impl TrustedKey {
    /// May this key sign for this plugin name? A prefix ending `*` matches
    /// by prefix; any other prefix must equal the name.
    pub fn allows(&self, name: &str) -> bool {
        self.name_prefixes
            .iter()
            .any(|p| match p.strip_suffix('*') {
                Some(stem) => name.starts_with(stem),
                None => p == name,
            })
    }

    /// Verify a detached 64-byte Ed25519 signature over `data`.
    pub fn verify(&self, data: &[u8], sig: &[u8]) -> bool {
        let Ok(arr) = <[u8; 64]>::try_from(sig) else {
            return false;
        };
        self.key.verify(data, &Signature::from_bytes(&arr)).is_ok()
    }
}

/// The set of keys in `trust.d`.
#[derive(Debug, Default, Clone)]
pub struct TrustStore {
    keys: Vec<TrustedKey>,
}

/// Decode a PEM `PUBLIC KEY` block holding an Ed25519 SPKI. Returns the
/// verifying key and the SPKI DER (for the fingerprint).
pub fn parse_pem(pem: &str) -> Res<(VerifyingKey, Vec<u8>)> {
    let body: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .map(str::trim)
        .collect();
    let der = base64::engine::general_purpose::STANDARD
        .decode(body.as_bytes())
        .map_err(|_| UpdError::new("trust_key_invalid", "key is not base64"))?;
    if der.len() != 44 || der[..12] != SPKI_PREFIX {
        return Err(UpdError::new(
            "trust_key_invalid",
            "key is not an Ed25519 SubjectPublicKeyInfo",
        ));
    }
    let raw: [u8; 32] = der[12..].try_into().expect("length checked");
    let key = VerifyingKey::from_bytes(&raw).map_err(|_| {
        UpdError::new("trust_key_invalid", "key bytes rejected")
    })?;
    Ok((key, der))
}

impl TrustStore {
    /// Load every `*.pem` that has a `*.meta.toml` beside it.
    pub fn load(dir: &Path) -> Res<Self> {
        let mut keys = Vec::new();
        let rd = fs::read_dir(dir)
            .map_err(|e| io_err("trust_unreadable", "trust.d", e))?;
        for ent in rd.flatten() {
            let path = ent.path();
            if path.extension().and_then(|e| e.to_str()) != Some("pem") {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let meta_path = dir.join(format!("{stem}.meta.toml"));
            let Ok(meta_text) = fs::read_to_string(&meta_path) else {
                continue;
            };
            let Ok(pem) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok((key, der)) = parse_pem(&pem) else {
                continue;
            };
            let Ok(meta) = meta_text.parse::<toml::Table>() else {
                continue;
            };
            let prefixes: Vec<String> = meta
                .get("authorisation")
                .and_then(|a| a.get("name_prefixes"))
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if prefixes.is_empty() {
                continue;
            }
            keys.push(TrustedKey {
                label: stem,
                fingerprint: format!(
                    "sha256:{}",
                    hex::encode(Sha256::digest(&der))
                ),
                key,
                name_prefixes: prefixes,
            });
        }
        keys.sort_by(|a, b| a.label.cmp(&b.label));
        if keys.is_empty() {
            return Err(UpdError::new(
                "trust_empty",
                "no usable key in the trust directory",
            ));
        }
        Ok(Self { keys })
    }

    /// Build from keys already parsed (tests).
    pub fn from_keys(keys: Vec<TrustedKey>) -> Self {
        Self { keys }
    }

    /// The key that verifies `sig` over `data`, if any.
    pub fn verifying_key(
        &self,
        data: &[u8],
        sig: &[u8],
    ) -> Option<&TrustedKey> {
        self.keys.iter().find(|k| k.verify(data, sig))
    }

    /// Number of loaded keys.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// True if no key is loaded.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// Test support: build a [`TrustedKey`] from a signing key.
#[cfg(test)]
pub fn test_key(
    label: &str,
    seed: u8,
    prefixes: &[&str],
) -> (TrustedKey, ed25519_dalek::SigningKey) {
    let sk = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
    let vk = sk.verifying_key();
    let mut der = SPKI_PREFIX.to_vec();
    der.extend_from_slice(vk.as_bytes());
    (
        TrustedKey {
            label: label.into(),
            fingerprint: format!(
                "sha256:{}",
                hex::encode(Sha256::digest(&der))
            ),
            key: vk,
            name_prefixes: prefixes.iter().map(|s| s.to_string()).collect(),
        },
        sk,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Signer;

    #[test]
    fn a_signature_verifies_only_under_its_own_key() {
        let (k1, s1) = test_key("a", 1, &["org.evoframework.*"]);
        let (k2, _) = test_key("b", 2, &["org.evoframework.*"]);
        let sig = s1.sign(b"hello").to_bytes();
        assert!(k1.verify(b"hello", &sig));
        assert!(!k2.verify(b"hello", &sig));
        assert!(!k1.verify(b"hellp", &sig), "one changed byte must fail");
        assert!(
            !k1.verify(b"hello", &sig[..63]),
            "short signature must fail"
        );
    }

    #[test]
    fn name_prefixes_are_a_prefix_or_an_exact_name() {
        let (k, _) = test_key("a", 1, &["org.evoframework.*", "exact.one"]);
        assert!(k.allows("org.evoframework.system.info"));
        assert!(k.allows("exact.one"));
        assert!(!k.allows("exact.one.more"));
        assert!(!k.allows("org.other.x"));
    }

    #[test]
    fn a_pem_loads_with_its_sidecar_and_a_bare_pem_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let pem = "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEAvJqIhluihUhLY435rJZnIjskDS9affTKSDUIYVIjVE0=\n-----END PUBLIC KEY-----\n";
        fs::write(dir.path().join("commons.pem"), pem).unwrap();
        assert_eq!(
            TrustStore::load(dir.path()).unwrap_err().code,
            "trust_empty",
            "a key with no sidecar is not used"
        );
        fs::write(
            dir.path().join("commons.meta.toml"),
            "[authorisation]\nname_prefixes = [\"org.evoframework.*\"]\n",
        )
        .unwrap();
        let t = TrustStore::load(dir.path()).unwrap();
        assert_eq!(t.len(), 1);
        assert!(t.keys[0].allows("org.evoframework.alarm"));
    }

    #[test]
    fn a_pem_that_is_not_ed25519_spki_is_refused() {
        assert!(parse_pem(
            "-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----\n"
        )
        .is_err());
        assert!(parse_pem("not a key").is_err());
    }
}
