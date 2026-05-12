// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Per-platform source-preset selection.
//!
//! The supervisor consumes a [`Vec<Box<dyn LinkEventSource>>`].
//! The composition of that vec is the substrate's most
//! operator-facing dimension: which sources are wired drives
//! which kernel / daemon surfaces the framework observes on
//! this device.
//!
//! Presets encode the canonical compositions for the
//! platforms the framework ships on, plus a `polling-only`
//! universal floor. Each preset is an explicit list of source
//! constructors evaluated at boot; constructors that succeed
//! contribute their source; constructors that fail surface
//! their diagnostic and the supervisor continues without
//! that source (the polling floor + remaining sources carry
//! the load).
//!
//! Operators may either:
//!
//! 1. Specify a preset name via configuration. The named
//!    preset's source list is consulted unconditionally.
//! 2. Leave the choice unspecified. The framework probes the
//!    runtime environment (target OS, presence of NM on the
//!    system bus, …) and picks a default preset.
//!
//! Per-source construction failures are NEVER fatal. The
//! preset's job is to enumerate the *candidates*; whether a
//! given candidate is available on this device at this boot
//! is determined at construction time. A device with
//! NetworkManager installed gets the NM source; a server
//! without NM falls through to the next candidate in the
//! preset list (or to polling alone if every candidate
//! fails).

use crate::source::LinkEventSource;
use std::fmt;

/// Canonical preset names. Each variant is a complete
/// description of the source candidates the supervisor will
/// attempt to mount at boot, in priority order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// Linux distribution shipping NetworkManager
    /// (Debian/Ubuntu/Fedora desktop, Arch with NM).
    /// Candidates: rtnetlink + NetworkManager + Polling.
    LinuxSystemdNm,

    /// Linux distribution without NM, with systemd-networkd
    /// (typical server posture). Candidates: rtnetlink +
    /// systemd-networkd + Polling. systemd-networkd source
    /// is planned for a later backend; today the preset
    /// falls through to rtnetlink + polling alone.
    LinuxSystemdNetworkd,

    /// Embedded Yocto distribution with ConnMan. Candidates:
    /// rtnetlink + ConnMan + Polling. ConnMan source is
    /// planned for a later backend; today the preset
    /// falls through to rtnetlink + polling alone.
    LinuxYoctoConnman,

    /// Linux without any userspace network daemon (bare
    /// embedded distributions, container minimal images).
    /// Candidates: rtnetlink + Polling.
    LinuxBare,

    /// FreeBSD posture. Candidates: devd + Polling. The devd
    /// source is planned for a later backend; today the
    /// preset falls through to polling alone.
    Bsd,

    /// Universal floor — polling source only. Selected on
    /// non-Linux + non-BSD targets that lack any typed
    /// event surface, and as the explicit operator override
    /// for "I want the simplest possible substrate".
    PollingOnly,

    /// Embedded RTOS (ESP32 etc.). No polling — the platform
    /// owns the event surface; the supervisor consumes its
    /// native event source. Reserved for future ESP-IDF
    /// backend.
    EmbeddedRtos,
}

impl Preset {
    /// Stable wire identifier for the preset. Surfaces in
    /// `describe_capabilities`, audit observations, and
    /// operator configuration.
    pub fn name(self) -> &'static str {
        match self {
            Self::LinuxSystemdNm => "linux-systemd-nm",
            Self::LinuxSystemdNetworkd => "linux-systemd-networkd",
            Self::LinuxYoctoConnman => "linux-yocto-connman",
            Self::LinuxBare => "linux-bare",
            Self::Bsd => "bsd",
            Self::PollingOnly => "polling-only",
            Self::EmbeddedRtos => "embedded-rtos",
        }
    }

    /// Parse a preset name. `None` when the input doesn't
    /// match any known preset — operators see a structured
    /// configuration error rather than a silent default.
    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "linux-systemd-nm" => Some(Self::LinuxSystemdNm),
            "linux-systemd-networkd" => Some(Self::LinuxSystemdNetworkd),
            "linux-yocto-connman" => Some(Self::LinuxYoctoConnman),
            "linux-bare" => Some(Self::LinuxBare),
            "bsd" => Some(Self::Bsd),
            "polling-only" => Some(Self::PollingOnly),
            "embedded-rtos" => Some(Self::EmbeddedRtos),
            _ => None,
        }
    }
}

impl fmt::Display for Preset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Pick a default preset for the current build target. Used
/// when the operator does not supply an explicit preset
/// override. The choice is conservative: it picks the
/// preset whose candidates are most likely to succeed on the
/// target, and the supervisor's per-source construction loop
/// gracefully handles cases where a candidate is absent.
///
/// `cfg!` evaluates at compile time, so cross-compilation
/// for different targets selects different defaults
/// statically. Operators may override the choice at runtime
/// via configuration.
pub fn default_preset() -> Preset {
    if cfg!(target_os = "linux") {
        // On Linux the default assumes a desktop-class
        // distribution with NetworkManager. Servers without
        // NM and embedded targets without any daemon get the
        // benefit of rtnetlink being universal — its
        // candidate succeeds; the NM candidate fails
        // gracefully and falls through.
        Preset::LinuxSystemdNm
    } else if cfg!(target_os = "freebsd") {
        Preset::Bsd
    } else {
        // macOS / Windows / unknown: the polling floor is
        // the universal correctness backstop.
        Preset::PollingOnly
    }
}

/// Build the source candidate set for a preset on the current
/// boot. Per-source construction is async because most typed
/// sources open a socket / connect to D-Bus.
///
/// Returns the constructed sources alongside the per-
/// candidate construction diagnostic (`Ok(name)` for success,
/// `Err((name, error_string))` for failure). The supervisor
/// prints every diagnostic at boot so operators see which
/// candidates the framework attempted + which it admitted.
pub async fn build_sources(
    preset: Preset,
    polling_interval_ms: u64,
) -> (Vec<Box<dyn LinkEventSource>>, Vec<CandidateOutcome>) {
    let mut sources: Vec<Box<dyn LinkEventSource>> = Vec::new();
    let mut outcomes: Vec<CandidateOutcome> = Vec::new();

    match preset {
        Preset::LinuxSystemdNm => {
            try_rtnetlink(&mut sources, &mut outcomes).await;
            try_nm(&mut sources, &mut outcomes).await;
            add_polling(&mut sources, &mut outcomes, polling_interval_ms);
        }
        Preset::LinuxSystemdNetworkd | Preset::LinuxYoctoConnman => {
            try_rtnetlink(&mut sources, &mut outcomes).await;
            add_polling(&mut sources, &mut outcomes, polling_interval_ms);
        }
        Preset::LinuxBare => {
            try_rtnetlink(&mut sources, &mut outcomes).await;
            add_polling(&mut sources, &mut outcomes, polling_interval_ms);
        }
        Preset::Bsd => {
            add_polling(&mut sources, &mut outcomes, polling_interval_ms);
        }
        Preset::PollingOnly => {
            add_polling(&mut sources, &mut outcomes, polling_interval_ms);
        }
        Preset::EmbeddedRtos => {
            // No polling on embedded RTOS — the platform owns
            // the event surface. The supervisor refuses an
            // empty source set elsewhere; the embedded RTOS
            // source itself will land with the ESP-IDF
            // backend.
        }
    }

    (sources, outcomes)
}

/// Outcome of one per-source construction attempt within a
/// preset's candidate list. Surfaced on the wire-op
/// `describe_capabilities` response so operators see what
/// the framework tried.
#[derive(Debug, Clone)]
pub struct CandidateOutcome {
    /// Source name the candidate corresponds to.
    pub name: &'static str,
    /// Outcome of the construction attempt.
    pub result: CandidateResult,
}

/// Per-candidate result.
#[derive(Debug, Clone)]
pub enum CandidateResult {
    /// Source constructed + admitted to the supervisor.
    Admitted,
    /// Source construction failed; the operator-readable
    /// diagnostic is captured for surfacing on the wire.
    /// Non-fatal — the supervisor continues without this
    /// source.
    Refused {
        /// Why this candidate refused to mount.
        reason: String,
    },
}

/// Helper: attempt the rtnetlink source. No-op on
/// non-Linux builds.
#[allow(clippy::too_many_arguments)]
async fn try_rtnetlink(
    sources: &mut Vec<Box<dyn LinkEventSource>>,
    outcomes: &mut Vec<CandidateOutcome>,
) {
    #[cfg(all(feature = "source-rtnetlink", target_os = "linux"))]
    {
        match crate::source::rtnetlink::RtnetlinkEventSource::connect() {
            Ok(s) => {
                sources.push(Box::new(s));
                outcomes.push(CandidateOutcome {
                    name: "rtnetlink",
                    result: CandidateResult::Admitted,
                });
            }
            Err(e) => {
                outcomes.push(CandidateOutcome {
                    name: "rtnetlink",
                    result: CandidateResult::Refused {
                        reason: e.to_string(),
                    },
                });
            }
        }
    }
    #[cfg(not(all(feature = "source-rtnetlink", target_os = "linux")))]
    {
        let _ = sources;
        outcomes.push(CandidateOutcome {
            name: "rtnetlink",
            result: CandidateResult::Refused {
                reason: "rtnetlink source not compiled into this build".into(),
            },
        });
    }
}

/// Helper: attempt the NetworkManager source. No-op on
/// non-Linux builds + when the `source-nm` feature is off.
#[allow(clippy::too_many_arguments)]
async fn try_nm(
    sources: &mut Vec<Box<dyn LinkEventSource>>,
    outcomes: &mut Vec<CandidateOutcome>,
) {
    #[cfg(all(feature = "source-nm", target_os = "linux"))]
    {
        match crate::source::nm::NetworkManagerEventSource::connect().await {
            Ok(s) => {
                sources.push(Box::new(s));
                outcomes.push(CandidateOutcome {
                    name: "network-manager",
                    result: CandidateResult::Admitted,
                });
            }
            Err(e) => {
                outcomes.push(CandidateOutcome {
                    name: "network-manager",
                    result: CandidateResult::Refused {
                        reason: e.to_string(),
                    },
                });
            }
        }
    }
    #[cfg(not(all(feature = "source-nm", target_os = "linux")))]
    {
        let _ = sources;
        outcomes.push(CandidateOutcome {
            name: "network-manager",
            result: CandidateResult::Refused {
                reason: "network-manager source not compiled into this build"
                    .into(),
            },
        });
    }
}

/// Helper: unconditionally add the polling source. Polling
/// is the universal correctness floor on every platform that
/// can host a shell.
fn add_polling(
    sources: &mut Vec<Box<dyn LinkEventSource>>,
    outcomes: &mut Vec<CandidateOutcome>,
    interval_ms: u64,
) {
    let polling = crate::source::polling::PollingEventSource::new(interval_ms);
    sources.push(Box::new(polling));
    outcomes.push(CandidateOutcome {
        name: "polling",
        result: CandidateResult::Admitted,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_name_round_trips() {
        for p in [
            Preset::LinuxSystemdNm,
            Preset::LinuxSystemdNetworkd,
            Preset::LinuxYoctoConnman,
            Preset::LinuxBare,
            Preset::Bsd,
            Preset::PollingOnly,
            Preset::EmbeddedRtos,
        ] {
            let name = p.name();
            let back = Preset::from_name(name).unwrap();
            assert_eq!(p, back);
        }
    }

    #[test]
    fn from_name_refuses_unknown_input() {
        assert!(Preset::from_name("not-a-preset").is_none());
        assert!(Preset::from_name("").is_none());
    }

    #[test]
    fn default_preset_compiles_to_a_real_variant_on_every_target() {
        // The result varies by target; the assertion just
        // confirms a real variant is returned (no panic).
        let _ = default_preset();
    }

    #[test]
    fn preset_display_matches_name() {
        assert_eq!(format!("{}", Preset::LinuxSystemdNm), "linux-systemd-nm");
    }

    #[tokio::test]
    async fn polling_only_preset_yields_one_polling_source() {
        let (sources, outcomes) =
            build_sources(Preset::PollingOnly, 10_000).await;
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].name(), "polling");
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(outcomes[0].result, CandidateResult::Admitted));
    }

    #[tokio::test]
    async fn embedded_rtos_preset_yields_no_sources() {
        let (sources, outcomes) =
            build_sources(Preset::EmbeddedRtos, 10_000).await;
        assert!(sources.is_empty());
        assert!(outcomes.is_empty());
    }
}
