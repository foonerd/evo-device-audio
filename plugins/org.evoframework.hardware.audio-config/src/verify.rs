//! Install-verification probes for the hardware-audio-config
//! plugin.
//!
//! Each probe is a small, independently-runnable check over one
//! runtime precondition the plugin depends on. The
//! `verify_install` wire-op assembles the four probe outcomes
//! into a single operator-readable verdict so the
//! bootstrap-not-yet-run failure mode is observable BEFORE the
//! operator attempts a write that would silently fail at the
//! sudo step.
//!
//! Probes:
//!
//! 1. **Board profile resolution** —
//!    [`probe_board_profile`] — pure check over the resolved
//!    profile string; "Unknown" means the host's board class
//!    was not identified.
//! 2. **Catalogue admission** —
//!    [`probe_catalogue`] — pure check over the embedded
//!    catalogue + the resolved profile; empty DAC list means
//!    no entries for this board class.
//! 3. **Privileged-write grant** —
//!    [`probe_sudoers_grant`] — runs `sudo -n -l <path>`
//!    against representative paths from each Cmnd_Alias the
//!    plugin depends on. Reports which aliases are granted +
//!    which are missing.
//! 4. **Modder staging directory** —
//!    [`probe_modder_staging_dir`] — checks that the
//!    distribution-installed staging directory exists and is
//!    writable by the steward service user.
//!
//! Pure assembly via [`assemble_report`] composes the probe
//! outcomes into a [`VerifyInstallReport`] without running any
//! probe — unit-testable with synthetic outcomes.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Overall verdict status. Worst-case wins:
///
/// * [`VerifyStatus::Ok`] — every probe returned ok.
/// * [`VerifyStatus::Partial`] — at least one probe ok, at
///   least one probe failed. The plugin can run for some
///   gestures but not others; operator should consult the
///   per-probe diagnostics.
/// * [`VerifyStatus::Failed`] — every probe failed. The
///   bootstrap is almost certainly not yet run on this host.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerifyStatus {
    /// All probes returned ok.
    Ok,
    /// Some probes ok, some failed.
    Partial,
    /// No probe returned ok.
    Failed,
}

/// One probe's outcome.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProbeOutcome {
    /// Probe name (e.g. `"board_profile"`).
    pub name: String,
    /// Probe verdict.
    pub ok: bool,
    /// Operator-readable diagnostic. On success, summarises
    /// what was found. On failure, names what was missing and
    /// the recommended remediation (typically "run the
    /// bootstrap script").
    pub diagnostic: String,
}

impl ProbeOutcome {
    /// Construct a successful outcome.
    pub fn ok(name: impl Into<String>, diagnostic: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ok: true,
            diagnostic: diagnostic.into(),
        }
    }

    /// Construct a failed outcome.
    pub fn failed(
        name: impl Into<String>,
        diagnostic: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            ok: false,
            diagnostic: diagnostic.into(),
        }
    }
}

/// Top-level report returned by the `verify_install` wire-op.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerifyInstallReport {
    /// Aggregated status across all probes.
    pub status: VerifyStatus,
    /// Per-probe outcomes in the order probes ran.
    pub probes: Vec<ProbeOutcome>,
}

/// Pure assembly: derive the verdict from a list of probe
/// outcomes. Empty input is treated as `Failed` — the plugin
/// always runs the full probe set so an empty list represents
/// a logic error, not a no-op.
pub fn assemble_report(probes: Vec<ProbeOutcome>) -> VerifyInstallReport {
    let total = probes.len();
    let ok_count = probes.iter().filter(|p| p.ok).count();
    let status = if total == 0 {
        VerifyStatus::Failed
    } else if ok_count == total {
        VerifyStatus::Ok
    } else if ok_count == 0 {
        VerifyStatus::Failed
    } else {
        VerifyStatus::Partial
    };
    VerifyInstallReport { status, probes }
}

/// Probe 1 — board profile resolution.
///
/// `Unknown` (the placeholder the resolver returns when no
/// model-string marker matches) is a failure: the plugin
/// cannot select a provider when the board class is unknown.
pub fn probe_board_profile(profile: &str) -> ProbeOutcome {
    if profile == "Unknown" || profile.is_empty() {
        ProbeOutcome::failed(
            "board_profile",
            "host board class not identified by /proc/device-tree/model or /proc/cpuinfo; the plugin cannot select a provider. \
             Set EVO_HARDWARE_AUDIO_PROFILE to override (e.g. for VMs/containers), or run on supported hardware (Raspberry Pi, Tinkerboard).",
        )
    } else {
        ProbeOutcome::ok(
            "board_profile",
            format!("host board class resolved to {profile:?}"),
        )
    }
}

/// Probe 2 — catalogue admission.
///
/// An empty DAC list for the resolved profile means the
/// embedded catalogue does not include this board class.
/// Operator gestures against `list_dac_catalogue` /
/// `select_dac` will refuse with an empty-catalogue error.
pub fn probe_catalogue(profile: &str, dac_count: usize) -> ProbeOutcome {
    if dac_count == 0 {
        ProbeOutcome::failed(
            "catalogue",
            format!(
                "embedded catalogue has no DAC entries for board profile {profile:?}; \
                 either the board class is not in the bundled catalogue \
                 (generic x86 hosts), or the catalogue failed to admit"
            ),
        )
    } else {
        ProbeOutcome::ok(
            "catalogue",
            format!(
                "embedded catalogue admitted; {dac_count} DAC entries available for board profile {profile:?}"
            ),
        )
    }
}

/// Probe 3 — privileged-write grant.
///
/// Runs `sudo -n -l <bin> <path>` against representative
/// paths from each `Cmnd_Alias` declared in
/// `dist/sudoers.d/evo-hardware-audio.in`. Reports which
/// aliases are granted + which are missing.
///
/// The probe is read-only: `sudo -n -l` lists what the user
/// is allowed to run; it does NOT execute the command.
pub async fn probe_sudoers_grant() -> ProbeOutcome {
    // Each probe targets one representative path from each
    // Cmnd_Alias. If the bootstrap-installed drop-in is
    // missing, all three probes fail; if only one alias is
    // missing (drift from manual edits), the diagnostic
    // names exactly which.
    let probes: &[(&str, &[&str])] = &[
        ("read", &["/usr/bin/cat", "/boot/firmware/config.txt"]),
        (
            "write",
            &["/usr/bin/tee", "/etc/modules-load.d/evo-i2c-dev.conf"],
        ),
        (
            "modder",
            &["/usr/bin/tee", "/boot/firmware/overlays/probe.dtbo"],
        ),
    ];

    let mut granted: Vec<String> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    for (alias, args) in probes {
        let mut cmd = tokio::process::Command::new("sudo");
        cmd.arg("-n").arg("-l");
        for a in *args {
            cmd.arg(a);
        }
        let out = cmd.output().await;
        let admitted = matches!(out, Ok(ref o) if o.status.success());
        if admitted {
            granted.push((*alias).to_string());
        } else {
            missing.push((*alias).to_string());
        }
    }

    if missing.is_empty() {
        ProbeOutcome::ok(
            "sudoers_grant",
            format!(
                "all three privileged-write Cmnd_Aliases granted: {}",
                granted.join(", ")
            ),
        )
    } else {
        ProbeOutcome::failed(
            "sudoers_grant",
            format!(
                "privileged-write grant incomplete (granted: [{}], missing: [{}]); \
                 run the distribution bootstrap script to install \
                 /etc/sudoers.d/evo-hardware-audio",
                granted.join(", "),
                missing.join(", "),
            ),
        )
    }
}

/// Probe 4 — modder staging directory.
///
/// The bootstrap script creates the staging directory at mode
/// `0775 root:<service-user>`. The probe checks existence +
/// that the calling process can write to it (via a no-op
/// `tempfile_in` round-trip — create + delete an
/// auto-named temp file).
pub async fn probe_modder_staging_dir(path: &Path) -> ProbeOutcome {
    if !path.exists() {
        return ProbeOutcome::failed(
            "modder_staging_dir",
            format!(
                "staging directory {} does not exist; run the distribution bootstrap script with the modder install enabled (EVO_INSTALL_MODDER_DIR=1, default)",
                path.display()
            ),
        );
    }
    if !path.is_dir() {
        return ProbeOutcome::failed(
            "modder_staging_dir",
            format!("path {} exists but is not a directory", path.display()),
        );
    }
    // Writability probe: try to create + remove a probe file.
    let probe_name = format!(".verify_install_probe.{}", std::process::id());
    let probe_path = path.join(&probe_name);
    let write_outcome =
        tokio::fs::write(&probe_path, b"verify_install probe\n").await;
    let _ = tokio::fs::remove_file(&probe_path).await;
    match write_outcome {
        Ok(()) => ProbeOutcome::ok(
            "modder_staging_dir",
            format!(
                "staging directory {} exists and is writable by the steward service user",
                path.display()
            ),
        ),
        Err(e) => ProbeOutcome::failed(
            "modder_staging_dir",
            format!(
                "staging directory {} is not writable by the steward service user: {e}; \
                 the bootstrap script's chown/chmod step may not have run",
                path.display()
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assemble_report_all_ok_yields_ok_status() {
        let probes =
            vec![ProbeOutcome::ok("a", "ok"), ProbeOutcome::ok("b", "ok")];
        let report = assemble_report(probes);
        assert_eq!(report.status, VerifyStatus::Ok);
        assert_eq!(report.probes.len(), 2);
    }

    #[test]
    fn assemble_report_all_failed_yields_failed_status() {
        let probes = vec![
            ProbeOutcome::failed("a", "missing"),
            ProbeOutcome::failed("b", "missing"),
        ];
        let report = assemble_report(probes);
        assert_eq!(report.status, VerifyStatus::Failed);
    }

    #[test]
    fn assemble_report_mixed_yields_partial_status() {
        let probes = vec![
            ProbeOutcome::ok("a", "ok"),
            ProbeOutcome::failed("b", "missing"),
        ];
        let report = assemble_report(probes);
        assert_eq!(report.status, VerifyStatus::Partial);
    }

    #[test]
    fn assemble_report_empty_input_treats_as_failed() {
        let report = assemble_report(Vec::new());
        assert_eq!(report.status, VerifyStatus::Failed);
        assert!(report.probes.is_empty());
    }

    #[test]
    fn probe_board_profile_unknown_yields_failed() {
        let p = probe_board_profile("Unknown");
        assert!(!p.ok);
        assert_eq!(p.name, "board_profile");
        assert!(p.diagnostic.contains("not identified"));
    }

    #[test]
    fn probe_board_profile_empty_yields_failed() {
        let p = probe_board_profile("");
        assert!(!p.ok);
    }

    #[test]
    fn probe_board_profile_known_yields_ok() {
        let p = probe_board_profile("Raspberry PI");
        assert!(p.ok);
        assert!(p.diagnostic.contains("Raspberry PI"));
    }

    #[test]
    fn probe_catalogue_empty_yields_failed() {
        let p = probe_catalogue("Generic x86", 0);
        assert!(!p.ok);
        assert!(p.diagnostic.contains("Generic x86"));
    }

    #[test]
    fn probe_catalogue_non_empty_yields_ok() {
        let p = probe_catalogue("Raspberry PI", 42);
        assert!(p.ok);
        assert!(p.diagnostic.contains("42"));
    }

    #[tokio::test]
    async fn probe_modder_staging_dir_missing_path_yields_failed() {
        let p = probe_modder_staging_dir(Path::new(
            "/nonexistent/evo-verify-install-test",
        ))
        .await;
        assert!(!p.ok);
        assert!(p.diagnostic.contains("does not exist"));
    }

    #[tokio::test]
    async fn probe_modder_staging_dir_writable_path_yields_ok() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = probe_modder_staging_dir(tmp.path()).await;
        assert!(p.ok, "expected ok, got {p:?}");
        assert!(p.diagnostic.contains("writable"));
    }

    #[tokio::test]
    async fn probe_modder_staging_dir_file_not_directory_yields_failed() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let p = probe_modder_staging_dir(tmp.path()).await;
        assert!(!p.ok);
        assert!(p.diagnostic.contains("not a directory"));
    }
}
