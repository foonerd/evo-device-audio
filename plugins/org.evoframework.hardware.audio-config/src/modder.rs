//! Modder workflow primitives — types + crypto verification.
//!
//! Operator-extensible DTBO overlay surface. Owns:
//!
//! 1. Allowlist file format + parser. The allowlist is a signed
//!    JSON document carrying the operator's vendor public key, a
//!    list of permitted DTBO content-hashes, and an Ed25519
//!    signature over the canonical-JSON-encoded payload. Only
//!    DTBO blobs whose SHA-256 hash appears in the allowlist may
//!    be registered.
//!
//! 2. User-overlay catalog row format. Operator-uploaded `.dtbo`
//!    blobs land alongside a sibling `.toml` describing the
//!    catalog row (id, display name, overlay token, alsa hints,
//!    dsp_options, etc.). The plugin merges these rows into the
//!    base catalog at admission + whenever the modder surface
//!    changes; the merged view publishes as the existing
//!    capabilities subject's catalog list.
//!
//! 3. Verification primitives. `compute_dtbo_hash`, `verify_allowlist`,
//!    `validate_confirmation_token`, `merge_into_catalog`. All
//!    pure functions, fully unit-testable.
//!
//! 4. Filesystem layout constants. The plugin reads
//!    `/etc/evo/hardware/audio/overlays/` for both `.toml` rows
//!    and `.dtbo` blobs; the operator-signed allowlist lives
//!    at `<dir>/allowlist.signed`. DTBO write target on Pi
//!    boards is `/boot/firmware/overlays/` (the kernel reads
//!    overlays from there).
//!
//! Wire-op integration (the register_overlay / list_overlays /
//! remove_overlay surface) consumes these primitives in a
//! follow-on layer; the primitives stand alone for testability.

use std::path::PathBuf;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::evo_catalog::{DacEntry, EvoCatalog};

/// Default operator-overlay directory. The bootstrap script
/// creates this with mode `0775 root:<service-user>` so the
/// plugin (running as the service user) can write user-overlay
/// row metadata without sudo escalation.
pub const USER_OVERLAY_DIR: &str = "/etc/evo/hardware/audio/overlays";

/// Default operator-signed allowlist path. The operator places
/// this file in [`USER_OVERLAY_DIR`] before any register_overlay
/// gesture; the plugin refuses every modder gesture when the
/// allowlist is missing OR its signature does not verify.
pub const ALLOWLIST_FILENAME: &str = "allowlist.signed";

/// Default Pi boot-firmware overlays directory. The plugin
/// copies the operator-uploaded `.dtbo` blob here before
/// rewriting the managed dtoverlay block to reference it.
/// Per-board provider overrides this when a different SBC
/// uses a different overlay-installation directory.
pub const PI_DTBO_INSTALL_DIR: &str = "/boot/firmware/overlays";

/// Operator-signed allowlist document. The vendor (or the
/// reference distribution's operator) generates this offline
/// using their Ed25519 signing key; the signed file lives
/// alongside the operator-uploaded DTBO blobs.
///
/// Wire-format JSON:
///
/// ```json
/// {
///   "schema_version": 1,
///   "signing_key_b64": "<base64 Ed25519 verifying key>",
///   "entries": [
///     {
///       "dtbo_sha256_hex": "abcdef...",
///       "display_name": "MyCustomDAC overlay",
///       "issued_at_ms": 1716240000000
///     },
///     ...
///   ],
///   "signature_b64": "<base64 Ed25519 signature over the canonical-JSON form of the above WITHOUT this field>"
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SignedAllowlist {
    /// Schema version. Pinned at 1 for this release line.
    pub schema_version: u32,
    /// Hex-encoded Ed25519 verifying-key bytes (32 bytes →
    /// 64 hex chars).
    pub signing_key_hex: String,
    /// Per-DTBO allowlist entries.
    #[serde(default)]
    pub entries: Vec<AllowlistEntry>,
    /// Hex-encoded Ed25519 signature over the canonical-JSON
    /// form of this document with `signature_hex` removed.
    pub signature_hex: String,
}

/// One DTBO-hash allowlist entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AllowlistEntry {
    /// SHA-256 of the DTBO blob, hex-encoded (lowercase, 64
    /// chars). Compared verbatim against
    /// [`compute_dtbo_hash`] output for registered overlays.
    pub dtbo_sha256_hex: String,
    /// Operator-readable display name for diagnostic surface
    /// (e.g. "MyCustomDAC v0.1 overlay (signed 2026-05-21)").
    #[serde(default)]
    pub display_name: String,
    /// When the operator signed this entry. Epoch milliseconds.
    /// Currently informational; future revocation surfaces may
    /// gate on this.
    #[serde(default)]
    pub issued_at_ms: u64,
}

/// One operator-supplied user-overlay catalog row. Persisted as
/// a TOML sibling to the DTBO blob under [`USER_OVERLAY_DIR`].
/// On admission + every modder change, the plugin loads every
/// `.toml` in this directory, validates its sibling `.dtbo`'s
/// hash against the allowlist, and merges the row into the
/// runtime catalog under a deterministic conflict policy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserOverlayRow {
    /// Catalog id (must match the file's stem). Operator
    /// gestures reference this id.
    pub id: String,
    /// Operator-readable display name.
    pub display_name: String,
    /// Board profile this row attaches to (e.g. "Raspberry PI").
    pub board_profile: String,
    /// dtoverlay token written into the managed block on
    /// `select_dac`. Must be non-empty.
    pub overlay: String,
    /// SHA-256 of the sibling DTBO blob, hex-encoded.
    /// Verified at every load + register gesture.
    pub dtbo_sha256_hex: String,
    /// Short ALSA card id hint. Optional.
    #[serde(default)]
    pub alsa_card_hint: String,
    /// In-card mixer hint. Optional.
    #[serde(default)]
    pub in_card_mixer: String,
    /// DSP option names (joined with curated pool at runtime).
    #[serde(default)]
    pub dsp_options: Vec<String>,
    /// Whether this row should override a base-catalog row with
    /// the same (board_profile, id). Refused when the matching
    /// base row has `advanced_settings_enabled = false`.
    #[serde(default)]
    pub override_base: bool,
}

/// Activation state surfaced on the `modder_overlays` subject.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum UserOverlayState {
    /// Row is registered, sibling DTBO is on disk, allowlist
    /// signature + hash both verified. Row participates in
    /// the merged catalog and can be selected.
    Active,
    /// Row was loaded but its sibling DTBO's hash did not
    /// appear in the allowlist OR the allowlist signature
    /// failed to verify. Surfaces in the subject so operators
    /// can diagnose without journal-grepping.
    Refused {
        /// Operator-readable diagnostic naming the failure.
        reason: String,
    },
}

/// Errors returned by modder primitive operations.
#[derive(Debug, thiserror::Error)]
pub enum ModderError {
    /// Distribution-tier config refuses the modder surface.
    #[error("AdvancedSettingsDisabled: {0}")]
    AdvancedSettingsDisabled(String),
    /// Allowlist file's Ed25519 signature did not verify.
    #[error("SignatureRefused: {0}")]
    SignatureRefused(String),
    /// DTBO blob's SHA-256 does not match the allowlist entry's
    /// declared hash.
    #[error("HashMismatch: {0}")]
    HashMismatch(String),
    /// DTBO blob's SHA-256 does not match the digest the operator
    /// supplied in the register payload.
    #[error("DigestMismatch: {0}")]
    DigestMismatch(String),
    /// DTBO hash not present in the allowlist.
    #[error("AllowlistEntryMissing: {0}")]
    AllowlistEntryMissing(String),
    /// Two-step-confirm token did not match the expected literal.
    #[error("ConfirmationTokenMismatch: {0}")]
    ConfirmationTokenMismatch(String),
    /// User-overlay row's id collides with a base-catalog row
    /// AND no override / per-DAC flag refuses.
    #[error("CollidesWithBaseCatalog: {0}")]
    CollidesWithBaseCatalog(String),
    /// Remove gesture refused because the row's overlay is
    /// currently active in the on-disk managed block.
    #[error("OverlayActive: {0}")]
    OverlayActive(String),
    /// Underlying IO error (missing dtbo file, write failure, …).
    #[error("DtboFileMissing: {0}")]
    DtboFileMissing(String),
    /// Filesystem write failed.
    #[error("DtboWriteFailed: {0}")]
    DtboWriteFailed(String),
    /// Hex decoding failed on a signature / hash / key field.
    #[error("MalformedHex: {0}")]
    MalformedHex(String),
    /// JSON parse failed on the allowlist or a row TOML.
    #[error("MalformedDocument: {0}")]
    MalformedDocument(String),
}

// =============================================================
// Crypto + hash primitives
// =============================================================

/// Compute the SHA-256 digest of a DTBO blob's raw bytes,
/// returning the lowercase hex-encoded form (64 chars). Used
/// both at allowlist-build time (offline) and at every register
/// gesture (online verification).
pub fn compute_dtbo_hash(dtbo_bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(dtbo_bytes);
    hex::encode(hasher.finalize())
}

/// Verify the allowlist's Ed25519 signature against its embedded
/// signing key. Returns Ok(()) on valid signature; structured
/// error variants on every failure mode.
pub fn verify_allowlist_signature(
    allowlist: &SignedAllowlist,
) -> Result<(), ModderError> {
    let key_bytes = hex::decode(&allowlist.signing_key_hex).map_err(|e| {
        ModderError::MalformedHex(format!("signing_key_hex: {e}"))
    })?;
    if key_bytes.len() != 32 {
        return Err(ModderError::MalformedHex(format!(
            "signing_key_hex must decode to 32 bytes, got {}",
            key_bytes.len()
        )));
    }
    let key_array: [u8; 32] = key_bytes
        .as_slice()
        .try_into()
        .map_err(|e| ModderError::MalformedHex(format!("key length: {e}")))?;
    let verifying_key = VerifyingKey::from_bytes(&key_array).map_err(|e| {
        ModderError::SignatureRefused(format!("verifying key parse: {e}"))
    })?;
    let sig_bytes = hex::decode(&allowlist.signature_hex).map_err(|e| {
        ModderError::MalformedHex(format!("signature_hex: {e}"))
    })?;
    if sig_bytes.len() != 64 {
        return Err(ModderError::MalformedHex(format!(
            "signature_hex must decode to 64 bytes, got {}",
            sig_bytes.len()
        )));
    }
    let sig_array: [u8; 64] = sig_bytes.as_slice().try_into().map_err(|e| {
        ModderError::MalformedHex(format!("signature length: {e}"))
    })?;
    let signature = Signature::from_bytes(&sig_array);
    let message = canonical_allowlist_message(allowlist).map_err(|e| {
        ModderError::MalformedDocument(format!("canonicalise allowlist: {e}"))
    })?;
    verifying_key
        .verify(message.as_bytes(), &signature)
        .map_err(|e| {
            ModderError::SignatureRefused(format!("Ed25519 verify failed: {e}"))
        })?;
    Ok(())
}

/// Produce the canonical-JSON byte-sequence the operator's
/// signing tool signed. The signature covers the
/// allowlist with `signature_hex` removed, serialised with
/// stable key ordering.
fn canonical_allowlist_message(
    allowlist: &SignedAllowlist,
) -> Result<String, String> {
    // Stable shape: build a struct with the fields the signature
    // covers (NOT the signature itself).
    #[derive(Serialize)]
    struct Canonical<'a> {
        schema_version: u32,
        signing_key_hex: &'a str,
        entries: &'a [AllowlistEntry],
    }
    let canonical = Canonical {
        schema_version: allowlist.schema_version,
        signing_key_hex: &allowlist.signing_key_hex,
        entries: &allowlist.entries,
    };
    serde_json::to_string(&canonical).map_err(|e| e.to_string())
}

/// Lookup a DTBO hash in the allowlist. Returns
/// [`ModderError::AllowlistEntryMissing`] when the hash is
/// absent.
pub fn check_hash_against_allowlist<'a>(
    allowlist: &'a SignedAllowlist,
    dtbo_hash_hex: &str,
) -> Result<&'a AllowlistEntry, ModderError> {
    allowlist
        .entries
        .iter()
        .find(|e| e.dtbo_sha256_hex == dtbo_hash_hex)
        .ok_or_else(|| {
            ModderError::AllowlistEntryMissing(format!(
                "DTBO hash {dtbo_hash_hex} not in allowlist (allowlist carries {} entries)",
                allowlist.entries.len()
            ))
        })
}

/// Validate the two-step-confirm token. The operator must
/// supply the literal `CONFIRM:<dac_id>` matching the row's id
/// verbatim. Returns [`ModderError::ConfirmationTokenMismatch`]
/// on any other input.
pub fn validate_confirmation_token(
    supplied_token: &str,
    expected_dac_id: &str,
) -> Result<(), ModderError> {
    let expected = format!("CONFIRM:{expected_dac_id}");
    if supplied_token != expected {
        return Err(ModderError::ConfirmationTokenMismatch(format!(
            "expected {expected:?}, got {supplied_token:?}"
        )));
    }
    Ok(())
}

// =============================================================
// Merge: base catalog + user-overlay catalog
// =============================================================

/// Merge a user-overlay row into the base catalog under the
/// deterministic conflict policy:
///
/// * Row's `board_profile` must exist in the base catalog.
/// * If `override_base = false`, refuses when a base DAC entry
///   shares the same `id` ([`ModderError::CollidesWithBaseCatalog`]).
/// * If `override_base = true`, replaces the base entry only
///   when its `advanced_settings_enabled` is true; refuses
///   otherwise (base-catalog flag is authoritative per the
///   shelf-shape contract).
///
/// The returned [`EvoCatalog`] is a clone of the input with the
/// row applied; the original is not modified.
pub fn merge_user_overlay_into_catalog(
    base: &EvoCatalog,
    row: &UserOverlayRow,
) -> Result<EvoCatalog, ModderError> {
    let mut merged = base.clone();
    let board = merged
        .boards
        .iter_mut()
        .find(|b| b.name == row.board_profile)
        .ok_or_else(|| {
            ModderError::CollidesWithBaseCatalog(format!(
                "board profile {:?} not in base catalog",
                row.board_profile
            ))
        })?;
    let existing_idx = board.dacs.iter().position(|d| d.id == row.id);
    let new_entry = user_overlay_to_dac_entry(row);
    match (existing_idx, row.override_base) {
        (Some(_), false) => Err(ModderError::CollidesWithBaseCatalog(format!(
            "id {:?} already present in base catalog for profile {:?}; \
             supply override = true to replace it",
            row.id, row.board_profile
        ))),
        (Some(idx), true) => {
            if !board.dacs[idx].advanced_settings_enabled {
                return Err(ModderError::CollidesWithBaseCatalog(format!(
                    "base catalog row {:?} has advanced_settings_enabled = false; \
                     refusing override gesture",
                    row.id
                )));
            }
            board.dacs[idx] = new_entry;
            Ok(merged)
        }
        (None, _) => {
            board.dacs.push(new_entry);
            Ok(merged)
        }
    }
}

fn user_overlay_to_dac_entry(row: &UserOverlayRow) -> DacEntry {
    DacEntry {
        id: row.id.clone(),
        display_name: row.display_name.clone(),
        overlay: row.overlay.clone(),
        alsa_card_hint: row.alsa_card_hint.clone(),
        alsa_num_hint: 0,
        in_card_mixer: row.in_card_mixer.clone(),
        companion_modules: Vec::new(),
        init_script: String::new(),
        eeprom_names: Vec::new(),
        i2c_address: String::new(),
        needs_reboot_on_apply: true,
        advanced_settings_enabled: true,
        dsp_options: row.dsp_options.clone(),
        provenance: format!("modder:{}", row.id),
    }
}

// =============================================================
// Filesystem layout helpers
// =============================================================

/// Resolve the row metadata path for a given overlay id under
/// the default user-overlay directory.
pub fn row_path(overlay_id: &str) -> PathBuf {
    PathBuf::from(USER_OVERLAY_DIR).join(format!("{overlay_id}.toml"))
}

/// Resolve the DTBO blob path for a given overlay id under the
/// default user-overlay directory (where the operator uploads
/// it BEFORE the plugin copies it to the boot-firmware overlays
/// directory).
pub fn dtbo_staging_path(overlay_id: &str) -> PathBuf {
    PathBuf::from(USER_OVERLAY_DIR).join(format!("{overlay_id}.dtbo"))
}

/// Resolve the DTBO blob's final install path under the Pi
/// boot-firmware overlays directory. Operator's existing
/// `dtoverlay=<token>` reference resolves against this
/// directory after the plugin's copy step.
pub fn dtbo_install_path(overlay_id: &str) -> PathBuf {
    PathBuf::from(PI_DTBO_INSTALL_DIR).join(format!("{overlay_id}.dtbo"))
}

/// Resolve the allowlist path under the user-overlay directory.
pub fn allowlist_path() -> PathBuf {
    PathBuf::from(USER_OVERLAY_DIR).join(ALLOWLIST_FILENAME)
}

// =============================================================
// Distribution-tier config
// =============================================================

/// Plugin-config flag governing the modder surface at the
/// distribution layer. Showcase distributions default this to
/// `true`; vendor distributions override to `false` to refuse
/// all modder gestures.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ModderSurfaceState {
    /// Modder gestures admitted (showcase default).
    #[default]
    Enabled,
    /// Modder gestures refused at the distribution layer.
    Disabled,
}

impl ModderSurfaceState {
    /// Refuse the gesture if the distribution-tier flag is
    /// disabled. Used as a single guard at the top of every
    /// modder wire-op handler.
    pub fn guard_or_refuse(&self) -> Result<(), ModderError> {
        match self {
            Self::Enabled => Ok(()),
            Self::Disabled => Err(ModderError::AdvancedSettingsDisabled(
                "distribution-tier modder-surface config flag is disabled"
                    .into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evo_catalog::parse_evo_catalog;
    use ed25519_dalek::{Signer, SigningKey};

    /// Helper: generate a fresh Ed25519 keypair for signing
    /// tests. Returns (signing_key, verifying_key_hex_string).
    fn fresh_keypair() -> (SigningKey, String) {
        // ed25519-dalek's SigningKey::generate needs a CSPRNG;
        // tests use a deterministic seed for reproducibility.
        let seed = [42u8; 32];
        let sk = SigningKey::from_bytes(&seed);
        let vk_hex = hex::encode(sk.verifying_key().to_bytes());
        (sk, vk_hex)
    }

    fn build_signed_allowlist(entries: Vec<AllowlistEntry>) -> SignedAllowlist {
        let (sk, vk_hex) = fresh_keypair();
        let unsigned = SignedAllowlist {
            schema_version: 1,
            signing_key_hex: vk_hex,
            entries,
            signature_hex: String::new(),
        };
        let message = canonical_allowlist_message(&unsigned).unwrap();
        let signature = sk.sign(message.as_bytes());
        SignedAllowlist {
            signature_hex: hex::encode(signature.to_bytes()),
            ..unsigned
        }
    }

    #[test]
    fn compute_dtbo_hash_is_lowercase_sha256() {
        let h = compute_dtbo_hash(b"hello world");
        // SHA-256("hello world") = b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9
        assert_eq!(
            h,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn compute_dtbo_hash_is_deterministic() {
        let a = compute_dtbo_hash(b"abc");
        let b = compute_dtbo_hash(b"abc");
        assert_eq!(a, b);
        assert_ne!(compute_dtbo_hash(b"abc"), compute_dtbo_hash(b"abd"));
    }

    #[test]
    fn verify_allowlist_signature_accepts_well_formed() {
        let entries = vec![AllowlistEntry {
            dtbo_sha256_hex: "deadbeef".repeat(8),
            display_name: "Test DAC overlay".into(),
            issued_at_ms: 1716240000000,
        }];
        let allowlist = build_signed_allowlist(entries);
        verify_allowlist_signature(&allowlist).expect("signature verifies");
    }

    #[test]
    fn verify_allowlist_signature_refuses_tampered_entries() {
        let entries = vec![AllowlistEntry {
            dtbo_sha256_hex: "deadbeef".repeat(8),
            display_name: "Original".into(),
            issued_at_ms: 1716240000000,
        }];
        let mut allowlist = build_signed_allowlist(entries);
        // Tamper with the display name AFTER signing.
        allowlist.entries[0].display_name = "Tampered".into();
        let err = verify_allowlist_signature(&allowlist).unwrap_err();
        assert!(matches!(err, ModderError::SignatureRefused(_)));
    }

    #[test]
    fn verify_allowlist_signature_refuses_wrong_key() {
        let mut allowlist = build_signed_allowlist(vec![]);
        // Replace the signing key with a different one (the
        // signature was made with the original; verifying with
        // the new key should fail).
        let (_other_sk, other_vk_hex) = {
            let seed = [99u8; 32];
            let sk = SigningKey::from_bytes(&seed);
            (sk.clone(), hex::encode(sk.verifying_key().to_bytes()))
        };
        allowlist.signing_key_hex = other_vk_hex;
        let err = verify_allowlist_signature(&allowlist).unwrap_err();
        assert!(matches!(err, ModderError::SignatureRefused(_)));
    }

    #[test]
    fn verify_allowlist_signature_refuses_malformed_hex() {
        let mut allowlist = build_signed_allowlist(vec![]);
        allowlist.signing_key_hex = "not hex!".into();
        let err = verify_allowlist_signature(&allowlist).unwrap_err();
        assert!(matches!(err, ModderError::MalformedHex(_)));
    }

    #[test]
    fn check_hash_against_allowlist_resolves_present_entry() {
        let entries = vec![AllowlistEntry {
            dtbo_sha256_hex: "abc".repeat(21) + "a", // 64 chars
            display_name: "Entry".into(),
            issued_at_ms: 0,
        }];
        let allowlist = build_signed_allowlist(entries);
        let entry =
            check_hash_against_allowlist(&allowlist, &("abc".repeat(21) + "a"))
                .expect("present");
        assert_eq!(entry.display_name, "Entry");
    }

    #[test]
    fn check_hash_against_allowlist_refuses_absent_entry() {
        let allowlist = build_signed_allowlist(vec![]);
        let err = check_hash_against_allowlist(&allowlist, "no such hash")
            .unwrap_err();
        assert!(matches!(err, ModderError::AllowlistEntryMissing(_)));
    }

    #[test]
    fn validate_confirmation_token_accepts_exact_match() {
        validate_confirmation_token("CONFIRM:my-dac", "my-dac").expect("match");
    }

    #[test]
    fn validate_confirmation_token_refuses_wrong_id() {
        let err =
            validate_confirmation_token("CONFIRM:other", "my-dac").unwrap_err();
        assert!(matches!(err, ModderError::ConfirmationTokenMismatch(_)));
    }

    #[test]
    fn validate_confirmation_token_refuses_missing_prefix() {
        let err = validate_confirmation_token("my-dac", "my-dac").unwrap_err();
        assert!(matches!(err, ModderError::ConfirmationTokenMismatch(_)));
    }

    const EMBEDDED_CATALOG: &str = include_str!("../data/evo-catalog.toml");

    #[test]
    fn merge_appends_new_row_into_existing_profile() {
        let base = parse_evo_catalog(EMBEDDED_CATALOG).expect("parse");
        let row = UserOverlayRow {
            id: "my-custom-dac".into(),
            display_name: "My Custom DAC".into(),
            board_profile: "Raspberry PI".into(),
            overlay: "my-custom-dac-overlay".into(),
            dtbo_sha256_hex: "00".repeat(32),
            alsa_card_hint: "MyCustom".into(),
            in_card_mixer: String::new(),
            dsp_options: vec!["DSP Program".into()],
            override_base: false,
        };
        let merged = merge_user_overlay_into_catalog(&base, &row).expect("ok");
        let dacs = merged.dac_list_for_profile("Raspberry PI");
        assert!(dacs.iter().any(|d| d.id == "my-custom-dac"));
        let appended = dacs
            .iter()
            .find(|d| d.id == "my-custom-dac")
            .expect("appended row");
        assert_eq!(appended.provenance, "modder:my-custom-dac");
    }

    #[test]
    fn merge_refuses_collision_without_override() {
        let base = parse_evo_catalog(EMBEDDED_CATALOG).expect("parse");
        let row = UserOverlayRow {
            id: "hifiberry-dacplus".into(), // Collides with base
            display_name: "Replacement".into(),
            board_profile: "Raspberry PI".into(),
            overlay: "anything".into(),
            dtbo_sha256_hex: "00".repeat(32),
            alsa_card_hint: String::new(),
            in_card_mixer: String::new(),
            dsp_options: vec![],
            override_base: false,
        };
        let err = merge_user_overlay_into_catalog(&base, &row).unwrap_err();
        assert!(matches!(err, ModderError::CollidesWithBaseCatalog(_)));
    }

    #[test]
    fn merge_allows_override_when_base_permits() {
        let base = parse_evo_catalog(EMBEDDED_CATALOG).expect("parse");
        // hifiberry-dacplus has advanced_settings_enabled = true,
        // so override = true succeeds.
        let row = UserOverlayRow {
            id: "hifiberry-dacplus".into(),
            display_name: "Custom replacement".into(),
            board_profile: "Raspberry PI".into(),
            overlay: "custom-replacement".into(),
            dtbo_sha256_hex: "00".repeat(32),
            alsa_card_hint: "CustomCard".into(),
            in_card_mixer: "Digital".into(),
            dsp_options: vec!["DSP Program".into()],
            override_base: true,
        };
        let merged = merge_user_overlay_into_catalog(&base, &row).expect("ok");
        let entry = merged
            .find_dac("Raspberry PI", "hifiberry-dacplus")
            .expect("replaced");
        assert_eq!(entry.overlay, "custom-replacement");
        assert_eq!(entry.alsa_card_hint, "CustomCard");
        assert_eq!(entry.provenance, "modder:hifiberry-dacplus");
    }

    #[test]
    fn merge_refuses_override_when_base_locks_advanced_settings() {
        // Hand-author a base catalog with the per-DAC flag false.
        let base_toml = r#"
schema_version = 1
[[boards]]
name = "Raspberry PI"
provider = "pi"
[[boards.dacs]]
id = "vendor-locked"
display_name = "Vendor-locked DAC"
overlay = "vendor-overlay"
alsa_card_hint = "Vendor"
needs_reboot_on_apply = false
advanced_settings_enabled = false
dsp_options = []
provenance = "vendor"
"#;
        let base = parse_evo_catalog(base_toml).expect("parse");
        let row = UserOverlayRow {
            id: "vendor-locked".into(),
            display_name: "Trying to override".into(),
            board_profile: "Raspberry PI".into(),
            overlay: "different".into(),
            dtbo_sha256_hex: "00".repeat(32),
            alsa_card_hint: String::new(),
            in_card_mixer: String::new(),
            dsp_options: vec![],
            override_base: true,
        };
        let err = merge_user_overlay_into_catalog(&base, &row).unwrap_err();
        assert!(matches!(err, ModderError::CollidesWithBaseCatalog(_)));
        // The base catalog flag must surface in the diagnostic so
        // operators understand the refusal.
        match err {
            ModderError::CollidesWithBaseCatalog(msg) => {
                assert!(msg.contains("advanced_settings_enabled = false"));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn merge_refuses_unknown_board_profile() {
        let base = parse_evo_catalog(EMBEDDED_CATALOG).expect("parse");
        let row = UserOverlayRow {
            id: "x".into(),
            display_name: "X".into(),
            board_profile: "Unknown SBC".into(),
            overlay: "anything".into(),
            dtbo_sha256_hex: "00".repeat(32),
            alsa_card_hint: String::new(),
            in_card_mixer: String::new(),
            dsp_options: vec![],
            override_base: false,
        };
        let err = merge_user_overlay_into_catalog(&base, &row).unwrap_err();
        assert!(matches!(err, ModderError::CollidesWithBaseCatalog(_)));
    }

    #[test]
    fn modder_surface_state_default_is_enabled() {
        assert_eq!(ModderSurfaceState::default(), ModderSurfaceState::Enabled);
    }

    #[test]
    fn modder_surface_state_disabled_refuses_gestures() {
        let err = ModderSurfaceState::Disabled.guard_or_refuse().unwrap_err();
        assert!(matches!(err, ModderError::AdvancedSettingsDisabled(_)));
    }

    #[test]
    fn modder_surface_state_enabled_admits_gestures() {
        ModderSurfaceState::Enabled
            .guard_or_refuse()
            .expect("enabled admits");
    }

    #[test]
    fn filesystem_layout_helpers_resolve_expected_paths() {
        let row = row_path("my-overlay");
        assert_eq!(
            row.to_string_lossy(),
            "/etc/evo/hardware/audio/overlays/my-overlay.toml"
        );
        let dtbo = dtbo_staging_path("my-overlay");
        assert_eq!(
            dtbo.to_string_lossy(),
            "/etc/evo/hardware/audio/overlays/my-overlay.dtbo"
        );
        let install = dtbo_install_path("my-overlay");
        assert_eq!(
            install.to_string_lossy(),
            "/boot/firmware/overlays/my-overlay.dtbo"
        );
        let allowlist = allowlist_path();
        assert_eq!(
            allowlist.to_string_lossy(),
            "/etc/evo/hardware/audio/overlays/allowlist.signed"
        );
    }
}
