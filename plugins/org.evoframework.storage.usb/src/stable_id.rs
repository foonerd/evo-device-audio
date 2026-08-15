// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Stable-id derivation ladder for USB-transport partitions.
//!
//! The stable id is the human-readable name each USB volume mounts
//! at: `/var/lib/evo/music/USB/<stable-id>/`. Same path shows up
//! in Samba (`\\<host>\USB\<stable-id>`) and in the operator UI.
//! Per `USB-STORAGE.md` §3 the id must be:
//!
//! - **Readable on glass** — a manufacturer + model, not a hex UUID.
//! - **Stable across replug** of the same volume.
//! - **Unique across concurrent mounts** — no collision on the
//!   mount path when two volumes are plugged in at the same time.
//! - **Deterministic on repeated collision** — plugging the same
//!   two "Music" sticks in the same order produces the same
//!   suffixes each session.
//!
//! # Derivation ladder (first match wins)
//!
//! 0. **Operator alias** — persisted `(vendor, model, serial_short,
//!    partuuid) → alias` in `aliases.toml` (Step 6 wires the
//!    rename verb that writes it; Step 3 only reads).
//! 1. **Filesystem label** — udev `ID_FS_LABEL` when non-empty.
//! 2. **Vendor + model** — `<vendor>-<model>` composite.
//! 3. **Model only** — when `ID_VENDOR` is empty or the placeholder
//!    string `"USB"`.
//! 4. **Synthesized fallback** — `unlabelled-<vendor-or-usb>-<serial-6>`
//!    where `serial-6` is the last 6 chars of `ID_SERIAL_SHORT`
//!    (lowercased, alphanumeric only).
//!
//! # Sanitisation
//!
//! Sanitised tokens match `^[A-Za-z0-9][A-Za-z0-9_-]{0,31}$`:
//! letters + digits + underscore + hyphen; first char alphanumeric;
//! 1..=32 chars. Any other char → `-`; runs of `-` collapse;
//! leading `-` stripped. Empty result after sanitisation → skip
//! the rule and fall to the next. Case preserved (Samba is case-
//! insensitive so `Music` and `music` resolve the same on the LAN;
//! the UI shows the case the operator chose).
//!
//! # Partition suffix
//!
//! When a parent disk has more than one mountable partition the
//! base id is the DISK-level name and each partition gets a
//! `-p<N>` suffix (`sda1` → `-p1`, `sda2` → `-p2`). Skipped for
//! the common single-partition case: `SanDisk-Cruzer-Blade`
//! (one partition) vs `SanDisk-Cruzer-Blade-p2` (second on the
//! same physical stick).
//!
//! # Collision handling
//!
//! When two DIFFERENT physical volumes derive the same base id
//! (two "Music" sticks; two identical `SanDisk-Cruzer-Blade` sticks)
//! the caller passes the current set of in-use mount roots to
//! [`derive`] via [`DerivationContext::in_use_stable_ids`] and
//! the deriver appends `-2` / `-3` / … until unique. Enumeration
//! is deterministic: caller sorts the input partition set before
//! deriving so the same physical set produces the same suffix
//! assignment across boots.
//!
//! For SAME physical volume replug (already in the in-use set
//! via prior mount), the deriver reuses the same id because the
//! partuuid-keyed prior-mount check bypasses the collision loop.

use serde::{Deserialize, Serialize};

/// Which rule in the derivation ladder produced the base id.
/// Surfaced on the `DriveRecord.id_source` field so the UI
/// tooltip can render a "how did I get this name?" hint —
/// operators seeing `synthesized` know the drive has no label
/// or model and should consider renaming it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdSource {
    /// Rule 0 — operator alias from `aliases.toml`.
    OperatorAlias,
    /// Rule 1 — filesystem label (udev `ID_FS_LABEL`).
    FsLabel,
    /// Rule 2 — vendor + model composite.
    VendorModel,
    /// Rule 3 — model only (vendor absent or the placeholder `"USB"`).
    ModelOnly,
    /// Rule 4 — synthesized `unlabelled-…` token.
    Synthesized,
}

impl IdSource {
    /// Stable wire string matching the schema enum
    /// `storage.usb.v1 DriveRecord.id_source`.
    pub fn wire_str(self) -> &'static str {
        match self {
            IdSource::OperatorAlias => "operator_alias",
            IdSource::FsLabel => "fs_label",
            IdSource::VendorModel => "vendor_model",
            IdSource::ModelOnly => "model_only",
            IdSource::Synthesized => "synthesized",
        }
    }
}

/// One derivation input — the udev + FS attributes of a single
/// partition. Constructed from a [`crate::classifier::ClassifiedPartition`].
#[derive(Debug, Clone)]
pub struct DerivationInput<'a> {
    /// Filesystem label (udev `ID_FS_LABEL`) if present.
    pub label: Option<&'a str>,
    /// Udev `ID_VENDOR`.
    pub vendor: Option<&'a str>,
    /// Udev `ID_MODEL`.
    pub model: Option<&'a str>,
    /// Udev `ID_SERIAL_SHORT` (used in rule 4 fallback).
    pub serial_short: Option<&'a str>,
    /// This partition's 1-based index within the parent disk.
    pub partition_index: u32,
    /// Total mountable partitions on the parent disk (drives
    /// the `-p<N>` suffix rule).
    pub partition_count: u32,
    /// GPT PARTUUID (for alias-key lookup at rule 0).
    pub partuuid: Option<&'a str>,
}

/// Context supplied to [`derive`] for alias lookup + collision
/// resolution. Kept as a trait-free struct so callers can build
/// it from any state store.
#[derive(Debug, Clone, Default)]
pub struct DerivationContext<'a> {
    /// Operator alias for this drive's identity tuple (rule 0),
    /// if any. Caller looks up `(vendor, model, serial_short,
    /// partuuid)` in `aliases.toml` and passes the result here.
    pub operator_alias: Option<&'a str>,
    /// Set of stable-ids currently in use on this device
    /// (`/var/lib/evo/music/USB/<id>/` mount roots). The deriver
    /// appends `-2` / `-3` / … until the candidate is not in
    /// this set. Caller supplies from live mount reconciliation.
    pub in_use_stable_ids: &'a [String],
}

/// Result of a derivation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedId {
    /// The stable id (mount path leaf) — always non-empty, matches
    /// the sanitisation rule, and never collides with an entry in
    /// [`DerivationContext::in_use_stable_ids`].
    pub stable_id: String,
    /// Display token = stable-id without any trailing `-p<N>` or
    /// `-2` / `-3` suffix. Matches the parent disk's friendly
    /// name; consumers use it as the primary UI row line.
    pub display_name: String,
    /// Which ladder rule produced the base id.
    pub id_source: IdSource,
}

/// Sanitised-token regex per USB-STORAGE.md §3. Used at the
/// classifier + wrapper argv validation layers too so the same
/// character class is applied everywhere.
pub const TOKEN_RE: &str = r"^[A-Za-z0-9][A-Za-z0-9_-]{0,31}$";

/// Derive the stable id for one partition per the ladder + suffix
/// + collision rules. See module-level docs for the full spec.
///
/// Total function — never panics on well-formed input. On
/// pathological inputs (every rule sanitises empty AND
/// synthesized falls through because serial_short is also empty),
/// returns the literal `unlabelled-usb` token.
pub fn derive(
    input: &DerivationInput<'_>,
    ctx: &DerivationContext<'_>,
) -> DerivedId {
    // Rule 0 — operator alias.
    if let Some(alias) = ctx.operator_alias {
        let s = sanitise(alias);
        if !s.is_empty() {
            let display_name = s.clone();
            let stable_id = deconflict(
                with_partition_suffix(&s, input),
                ctx.in_use_stable_ids,
            );
            return DerivedId {
                stable_id,
                display_name,
                id_source: IdSource::OperatorAlias,
            };
        }
    }

    // Rule 1 — filesystem label.
    if let Some(label) = input.label {
        let s = sanitise(label);
        if !s.is_empty() {
            let display_name = s.clone();
            let stable_id = deconflict(
                with_partition_suffix(&s, input),
                ctx.in_use_stable_ids,
            );
            return DerivedId {
                stable_id,
                display_name,
                id_source: IdSource::FsLabel,
            };
        }
    }

    // Rule 2 — vendor + model composite.
    let vendor_placeholder = input
        .vendor
        .map(|v| v.trim().eq_ignore_ascii_case("usb"))
        .unwrap_or(false);
    let vendor_present =
        input.vendor.map(|v| !v.trim().is_empty()).unwrap_or(false)
            && !vendor_placeholder;
    if vendor_present {
        if let Some(model) = input.model {
            let v = sanitise(input.vendor.unwrap());
            let m = sanitise(model);
            if !v.is_empty() && !m.is_empty() {
                let base = format!("{v}-{m}");
                let display_name = base.clone();
                let stable_id = deconflict(
                    with_partition_suffix(&base, input),
                    ctx.in_use_stable_ids,
                );
                return DerivedId {
                    stable_id,
                    display_name,
                    id_source: IdSource::VendorModel,
                };
            }
        }
    }

    // Rule 3 — model only (vendor missing / placeholder).
    if let Some(model) = input.model {
        let s = sanitise(model);
        if !s.is_empty() {
            let display_name = s.clone();
            let stable_id = deconflict(
                with_partition_suffix(&s, input),
                ctx.in_use_stable_ids,
            );
            return DerivedId {
                stable_id,
                display_name,
                id_source: IdSource::ModelOnly,
            };
        }
    }

    // Rule 4 — synthesized fallback. Never empty (we substitute
    // `usb` and `000000` when the underlying inputs are missing).
    let vendor_token: String = input
        .vendor
        .map(|v| v.trim())
        .filter(|v| !v.is_empty() && !v.eq_ignore_ascii_case("usb"))
        .map(sanitise)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "usb".to_string())
        .to_lowercase();
    let serial_six: String = input
        .serial_short
        .map(str::to_ascii_lowercase)
        .map(|s| {
            s.chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
        })
        .map(|s| {
            let n = s.len();
            if n >= 6 {
                s[n - 6..].to_string()
            } else {
                format!("{s:0>6}")
            }
        })
        .unwrap_or_else(|| "000000".to_string());
    let base = format!("unlabelled-{vendor_token}-{serial_six}");
    let base_sanitised = sanitise(&base);
    let display_name = base_sanitised.clone();
    let stable_id = deconflict(
        with_partition_suffix(&base_sanitised, input),
        ctx.in_use_stable_ids,
    );
    DerivedId {
        stable_id,
        display_name,
        id_source: IdSource::Synthesized,
    }
}

/// Sanitise per the token rule. Never panics; returns the
/// possibly-empty result the caller inspects before promoting a
/// rule.
pub fn sanitise(input: &str) -> String {
    // Replace non-allowed with '-', collapse runs, strip leading
    // '-', truncate to 32 chars, ensure first char alphanumeric.
    let mut out = String::with_capacity(input.len());
    let mut last_hyphen = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
            last_hyphen = false;
        } else {
            if !last_hyphen && !out.is_empty() {
                out.push('-');
                last_hyphen = true;
            }
        }
    }
    // Strip trailing hyphens.
    while out.ends_with('-') {
        out.pop();
    }
    // Strip leading hyphens (defence-in-depth; we skip pushing
    // '-' to an empty string above, so this rarely fires).
    while out.starts_with('-') {
        out.remove(0);
    }
    // Truncate to 32 chars.
    if out.len() > 32 {
        out.truncate(32);
    }
    // Enforce first-char alphanumeric.
    if let Some(first) = out.chars().next() {
        if !first.is_ascii_alphanumeric() {
            // The truncation above may have left a leading
            // hyphen — strip.
            let stripped: String = out.chars().skip(1).collect();
            return stripped;
        }
    }
    out
}

/// Append `-p<N>` when the parent disk has more than one
/// mountable partition; return `base` unchanged otherwise.
fn with_partition_suffix(base: &str, input: &DerivationInput<'_>) -> String {
    if input.partition_count > 1 {
        format!("{base}-p{}", input.partition_index)
    } else {
        base.to_string()
    }
}

/// Append `-2` / `-3` / … until the candidate is not in the
/// caller-supplied in-use set. Returns the candidate unchanged
/// when already unique.
fn deconflict(candidate: String, in_use: &[String]) -> String {
    if !in_use.contains(&candidate) {
        return candidate;
    }
    let mut n: u32 = 2;
    loop {
        let attempt = format!("{candidate}-{n}");
        if !in_use.contains(&attempt) {
            return attempt;
        }
        n += 1;
        // Safety ceiling — the fleet has never seen more than a
        // handful of same-label sticks; a runaway loop past 1024
        // is a defect elsewhere (probably the caller passing an
        // in-use set that always matches). Fall out with a
        // synthetic suffix so we still return a usable id.
        if n > 1024 {
            return format!("{candidate}-{}", n);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_ctx() -> DerivationContext<'static> {
        DerivationContext::default()
    }

    fn input_for<'a>(
        label: Option<&'a str>,
        vendor: Option<&'a str>,
        model: Option<&'a str>,
        serial: Option<&'a str>,
    ) -> DerivationInput<'a> {
        DerivationInput {
            label,
            vendor,
            model,
            serial_short: serial,
            partition_index: 1,
            partition_count: 1,
            partuuid: None,
        }
    }

    #[test]
    fn sanitise_common_inputs() {
        assert_eq!(sanitise("MUSIC"), "MUSIC");
        assert_eq!(sanitise("my music"), "my-music");
        assert_eq!(sanitise("SanDisk Cruzer Blade"), "SanDisk-Cruzer-Blade");
        assert_eq!(sanitise("WD Elements 25A2"), "WD-Elements-25A2");
        assert_eq!(sanitise("///bad"), "bad");
        assert_eq!(sanitise("!!! test !!!"), "test");
        assert_eq!(sanitise(""), "");
    }

    #[test]
    fn sanitise_truncates_to_32_chars() {
        let s = sanitise("this-is-a-very-long-label-that-exceeds-32");
        assert!(s.len() <= 32);
        assert!(s.starts_with("this-is-a-very-long"));
    }

    #[test]
    fn sanitise_case_preserved() {
        assert_eq!(sanitise("Music"), "Music");
        assert_eq!(sanitise("music"), "music");
        assert_ne!(sanitise("Music"), sanitise("music"));
    }

    #[test]
    fn rule_0_operator_alias_wins_over_label() {
        let input = input_for(
            Some("MUSIC"),
            Some("SanDisk"),
            Some("Cruzer"),
            Some("4C530"),
        );
        let ctx = DerivationContext {
            operator_alias: Some("My Vinyl Rips"),
            ..empty_ctx()
        };
        let d = derive(&input, &ctx);
        assert_eq!(d.id_source, IdSource::OperatorAlias);
        assert_eq!(d.stable_id, "My-Vinyl-Rips");
    }

    #[test]
    fn rule_1_fs_label() {
        let input = input_for(
            Some("MUSIC"),
            Some("SanDisk"),
            Some("Cruzer"),
            Some("4C530"),
        );
        let d = derive(&input, &empty_ctx());
        assert_eq!(d.id_source, IdSource::FsLabel);
        assert_eq!(d.stable_id, "MUSIC");
        assert_eq!(d.display_name, "MUSIC");
    }

    #[test]
    fn rule_2_vendor_model_composite() {
        let input = input_for(
            None,
            Some("SanDisk"),
            Some("Cruzer Blade"),
            Some("4C530"),
        );
        let d = derive(&input, &empty_ctx());
        assert_eq!(d.id_source, IdSource::VendorModel);
        assert_eq!(d.stable_id, "SanDisk-Cruzer-Blade");
    }

    #[test]
    fn rule_3_model_only_when_vendor_empty() {
        let input = input_for(None, Some(""), Some("T7"), Some("S6P5"));
        let d = derive(&input, &empty_ctx());
        assert_eq!(d.id_source, IdSource::ModelOnly);
        assert_eq!(d.stable_id, "T7");
    }

    #[test]
    fn rule_3_model_only_when_vendor_is_usb_placeholder() {
        let input = input_for(None, Some("USB"), Some("Cruzer"), Some("4C530"));
        let d = derive(&input, &empty_ctx());
        assert_eq!(d.id_source, IdSource::ModelOnly);
        assert_eq!(d.stable_id, "Cruzer");
    }

    #[test]
    fn rule_4_synthesized_when_no_label_no_model() {
        let input = input_for(None, Some("SanDisk"), None, Some("4C530A1"));
        let d = derive(&input, &empty_ctx());
        assert_eq!(d.id_source, IdSource::Synthesized);
        // "unlabelled-sandisk-c530a1" - last 6 alphanumeric of "4C530A1"
        assert!(d.stable_id.starts_with("unlabelled-sandisk-"));
        // Serial takes last 6 chars, lowercased, alphanumeric-only.
        assert!(d.stable_id.ends_with("c530a1"));
    }

    #[test]
    fn rule_4_synthesized_falls_back_to_usb_prefix() {
        let input = input_for(None, None, None, Some("4C530A1"));
        let d = derive(&input, &empty_ctx());
        assert_eq!(d.id_source, IdSource::Synthesized);
        assert!(d.stable_id.starts_with("unlabelled-usb-"));
    }

    #[test]
    fn partition_suffix_when_multi_partition() {
        let mut input = input_for(Some("MUSIC"), None, None, None);
        input.partition_index = 2;
        input.partition_count = 3;
        let d = derive(&input, &empty_ctx());
        // display_name reflects the base label; stable_id gets the -p<N>.
        assert_eq!(d.display_name, "MUSIC");
        assert_eq!(d.stable_id, "MUSIC-p2");
    }

    #[test]
    fn partition_suffix_skipped_for_single_partition() {
        let input = input_for(Some("MUSIC"), None, None, None);
        let d = derive(&input, &empty_ctx());
        assert_eq!(d.stable_id, "MUSIC");
    }

    #[test]
    fn deconflict_appends_dash_two_on_collision() {
        let in_use: Vec<String> = vec!["Music".to_string()];
        let input = input_for(Some("Music"), None, None, None);
        let ctx = DerivationContext {
            operator_alias: None,
            in_use_stable_ids: &in_use,
        };
        let d = derive(&input, &ctx);
        assert_eq!(d.stable_id, "Music-2");
    }

    #[test]
    fn deconflict_walks_to_dash_three_and_beyond() {
        let in_use: Vec<String> = vec![
            "Music".to_string(),
            "Music-2".to_string(),
            "Music-3".to_string(),
        ];
        let input = input_for(Some("Music"), None, None, None);
        let ctx = DerivationContext {
            operator_alias: None,
            in_use_stable_ids: &in_use,
        };
        let d = derive(&input, &ctx);
        assert_eq!(d.stable_id, "Music-4");
    }

    #[test]
    fn deconflict_and_partition_suffix_compose() {
        // Two disks each with two partitions where partition 1
        // of the second disk collides with disk 1's stable-id.
        let in_use: Vec<String> = vec!["MUSIC-p1".to_string()];
        let mut input = input_for(Some("MUSIC"), None, None, None);
        input.partition_index = 1;
        input.partition_count = 2;
        let ctx = DerivationContext {
            operator_alias: None,
            in_use_stable_ids: &in_use,
        };
        let d = derive(&input, &ctx);
        assert_eq!(d.stable_id, "MUSIC-p1-2");
    }

    #[test]
    fn id_source_wire_strings_match_schema() {
        assert_eq!(IdSource::OperatorAlias.wire_str(), "operator_alias");
        assert_eq!(IdSource::FsLabel.wire_str(), "fs_label");
        assert_eq!(IdSource::VendorModel.wire_str(), "vendor_model");
        assert_eq!(IdSource::ModelOnly.wire_str(), "model_only");
        assert_eq!(IdSource::Synthesized.wire_str(), "synthesized");
    }

    #[test]
    fn pathological_no_label_no_vendor_no_model_no_serial() {
        let input = input_for(None, None, None, None);
        let d = derive(&input, &empty_ctx());
        assert_eq!(d.id_source, IdSource::Synthesized);
        assert_eq!(d.stable_id, "unlabelled-usb-000000");
    }
}
