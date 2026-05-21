//! Hardware-audio provider trait + types.
//!
//! The plugin's domain logic is board-class-agnostic: it owns the
//! DAC catalogue, the operator-visible subjects, and the wire-op
//! surface. The board-specific mechanism (probing the host, reading
//! / writing the boot-config dtoverlay block, installing companion
//! module drop-ins) lives behind the [`HardwareAudioProvider`] trait.
//!
//! The reference distribution ships a single concrete provider
//! ([`provider_pi::PiProvider`] against Raspberry Pi `config.txt`).
//! Other board classes (Rockchip, generic x86 with PCI HATs,
//! MCU-class follower devices) admit through a [`NoopProvider`]
//! that surfaces an empty catalogue + a structured "no catalogue
//! for this board class" diagnostic; concrete providers add as
//! the matching hardware reaches the supported set.
//!
//! [`provider_pi::PiProvider`]: crate::provider_pi::PiProvider

use std::future::Future;
use std::pin::Pin;

use crate::dsp::{AmixerReadOutcome, AmixerReader};
use crate::evo_catalog::DacEntry;

/// Errors a provider may return. Variants are explicit so callers
/// can surface board-class-aware diagnostics through verb responses.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// The provider could not resolve the host's board profile from
    /// the available system surfaces (device-tree model, cpuinfo).
    /// The plugin falls back to the `EVO_HARDWARE_AUDIO_PROFILE`
    /// env-var override before returning this.
    #[error("board profile unresolved: {0}")]
    BoardProfileUnresolved(String),

    /// The provider could not read the boot config at the resolved
    /// path. The plugin surfaces this on `current_config` reads;
    /// writes continue to attempt via the sudoers-backed path.
    #[error("boot config read failed: {0}")]
    BootConfigReadFailed(String),

    /// The provider could not write the boot config. The plugin
    /// surfaces this on `select_dac` / `clear_dac` and does NOT
    /// flip the `pending_reboot` subject — the on-disk state is
    /// unchanged.
    #[error("boot config write failed: {0}")]
    BootConfigWriteFailed(String),

    /// The supplied overlay token failed validation (empty or
    /// non-ASCII / contains characters outside the allowed set).
    /// The dtoverlay grammar accepts ASCII alnum + `,` + `_` + `-`
    /// + `.` to admit Pi5-style `,slave` parameter modifiers.
    #[error("invalid overlay token: {0}")]
    InvalidOverlay(String),

    /// Companion module-drop-in installation failed. The plugin
    /// surfaces this as a structured warning; the dtoverlay write
    /// still completes if it succeeded.
    #[error("module drop-in install failed: {0}")]
    ModuleDropInFailed(String),

    /// The provider's mechanism is not applicable to the current
    /// board class. Returned by `NoopProvider` for all write
    /// operations; the plugin surfaces this on `select_dac` /
    /// `clear_dac` so the operator understands the gesture cannot
    /// be applied here.
    #[error("not applicable on this board class")]
    NotApplicable,
}

/// The currently-applied DAC configuration as read from the host.
/// The plugin surfaces this through the `hardware.audio.current_config`
/// verb + the `evo.hardware.audio:active_config` subject.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ActiveConfig {
    /// The raw `dtoverlay=` token currently active in the managed
    /// block (e.g. `hifiberry-dacplus,slave`). Empty string means
    /// no managed block is present on disk.
    pub overlay: String,
    /// The catalogue id whose `overlay` field matches the active
    /// overlay, or None if no catalogue entry matches (the operator
    /// wrote a custom overlay, or the catalogue has changed).
    pub catalogue_id: Option<String>,
    /// The operator-friendly display name from the resolved
    /// catalogue entry, or None when no entry matches. Carried on
    /// the published `evo.hardware.audio:active_config` subject so
    /// downstream consumers (delivery.alsa's outputs enumeration,
    /// UI surfaces) render the operator-facing label without
    /// re-loading the DAC catalogue themselves.
    pub display_name: Option<String>,
    /// The catalogue `alsacard` hint for the resolved entry, or
    /// None if no match. Downstream plugins (delivery.alsa) use this
    /// to resolve the real card index without re-running `aplay -L`.
    pub alsacard_hint: Option<String>,
    /// The catalogue `mixer` hint for the resolved entry, or None.
    /// playback.mpd uses this to bind the in-DAC mixer control.
    pub mixer_hint: Option<String>,
    /// The boot-config path the active overlay was read from
    /// (`/boot/firmware/config.txt` or `/boot/config.txt`). Empty
    /// on board classes without a boot-config path.
    pub boot_config_path: String,
}

impl ActiveConfig {
    /// Construct the "no managed block on disk" variant. Used when
    /// the boot config exists but does not contain the plugin's
    /// banner-fenced block, and as the value the `clear_dac` verb
    /// leaves behind.
    pub fn unset(boot_config_path: impl Into<String>) -> Self {
        Self {
            overlay: String::new(),
            catalogue_id: None,
            display_name: None,
            alsacard_hint: None,
            mixer_hint: None,
            boot_config_path: boot_config_path.into(),
        }
    }
}

/// Outcome of an `apply` (select_dac / clear_dac) operation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ApplyOutcome {
    /// The dtoverlay token written (empty string on clear).
    pub overlay: String,
    /// The boot-config path the write landed at.
    pub boot_config_path: String,
    /// Whether a companion module drop-in was installed (Pi-only).
    pub module_drop_in_installed: bool,
    /// Whether the operator must reboot for the new overlay to load
    /// (always true for dtoverlay changes on Pi-class boards).
    pub reboot_required: bool,
}

/// Provider trait — the board-class-specific mechanism behind the
/// plugin's wire-op surface. Implementations live in modules paired
/// to the board class (`provider_pi`, future `provider_rockchip`,
/// etc.); a single [`NoopProvider`] absorbs any unsupported class.
///
/// All methods are async-returning to admit `tokio::fs` /
/// `tokio::process::Command` implementations without blocking the
/// steward's executor.
/// Live amixer-write abstraction. The wire-op layer's
/// `set_dsp_control` handler invokes this once per operator
/// gesture; the production implementation shells into
/// `amixer -c <card> cset name='<control>' <value>` against the
/// bound card; tests inject stubs.
pub trait AmixerWriter: Send + Sync {
    /// Set one ALSA mixer control's value on the bound card.
    fn write_control<'a>(
        &'a self,
        card_hint: &'a str,
        control_name: &'a str,
        value: AmixerWriteValue,
    ) -> Pin<Box<dyn Future<Output = AmixerWriteOutcome> + Send + 'a>>;
}

/// Operator-supplied value to write to an ALSA mixer control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AmixerWriteValue {
    /// For enum controls: the value as the human-readable label
    /// (e.g. `"Slow Roll-Off"`). The provider's write impl
    /// passes this verbatim to `amixer cset`; amixer accepts
    /// enum values by label OR by zero-based index.
    EnumLabel(String),
    /// For integer / db_scale controls: the integer value.
    Integer(i64),
    /// For boolean controls: `true` or `false`. The provider's
    /// write impl encodes as `on` / `off` for amixer.
    Boolean(bool),
}

/// Outcome variant returned by [`AmixerWriter::write_control`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AmixerWriteOutcome {
    /// Write succeeded; amixer accepted the value.
    Applied,
    /// No ALSA card matches the supplied hint on this host.
    CardUnknown {
        /// Operator-readable diagnostic.
        reason: String,
    },
    /// Amixer enumerates the card but does not expose this
    /// control — operator likely has an overlay / driver mismatch.
    NotPresent {
        /// Operator-readable diagnostic.
        reason: String,
    },
    /// Amixer refused the value (out of declared range, not a
    /// legal enum value, type-mismatch, …). Carries the
    /// amixer stderr verbatim so the operator-facing surface
    /// preserves the debugging detail.
    ValueRejected {
        /// Operator-readable diagnostic.
        reason: String,
    },
    /// Subprocess failed for some other reason (process exec
    /// error, IO error, …).
    InvocationFailed {
        /// Operator-readable diagnostic.
        reason: String,
    },
}

/// Provider trait — the board-class-specific mechanism behind the
/// plugin's wire-op surface. Implementations live in modules
/// paired to the board class (`provider_pi`, future
/// `provider_rockchip`, etc.); a single [`NoopProvider`] absorbs
/// any unsupported class.
///
/// `AmixerReader` + `AmixerWriter` are supertraits so the DSP
/// capability resolver depends on the narrower trait without
/// needing the full provider surface.
pub trait HardwareAudioProvider:
    AmixerReader + AmixerWriter + Send + Sync
{
    /// Board profile name keying the DAC catalogue (e.g.
    /// "Raspberry PI"). The plugin uses this to filter the
    /// catalogue at every list-or-select gesture.
    fn board_profile<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<String, ProviderError>> + Send + 'a>>;

    /// Read the host's current active configuration. Returns
    /// `ActiveConfig::unset(...)` when no managed block is present.
    fn current_config<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ActiveConfig, ProviderError>>
                + Send
                + 'a,
        >,
    >;

    /// Failsafe discovery: when [`Self::current_config`] returns an
    /// empty overlay (no managed block on disk), scan the boot
    /// config for bare `dtoverlay=<token>` lines OUTSIDE the
    /// plugin's managed banner block and return the first token
    /// whose value is in `known_overlays`. Returns `Ok(None)` when
    /// no bare DAC overlay is present.
    ///
    /// This honours operators who configure a DAC by editing the
    /// boot config directly (or by using a non-evo tool) without
    /// gestures through the plugin. The plugin then surfaces the
    /// detected overlay on the published `active_config` subject
    /// so downstream consumers (delivery.alsa's outputs
    /// enrichment, UI surfaces) can still resolve operator-
    /// friendly labels and mixer hints. The managed-vs-discovered
    /// distinction does not affect the published shape; gestures
    /// that write (apply / clear) continue to operate only on the
    /// managed block.
    ///
    /// Default implementation returns `Ok(None)` — provider
    /// implementations without boot-config introspection (e.g.
    /// `NoopProvider`) need no further work.
    fn discover_bare_overlay<'a>(
        &'a self,
        _known_overlays: Vec<String>,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<String>, ProviderError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move { Ok(None) })
    }

    /// Apply a catalogue entry: validate the overlay, write the
    /// managed block to the boot config, optionally install the
    /// i2c-dev module drop-in (Pi-only). Returns the
    /// [`ApplyOutcome`] describing what was actually done.
    fn apply<'a>(
        &'a self,
        entry: &'a DacEntry,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ApplyOutcome, ProviderError>>
                + Send
                + 'a,
        >,
    >;

    /// Clear the managed block from the boot config. Returns the
    /// [`ApplyOutcome`] describing the post-clear state. No-op when
    /// no managed block was present (still returns Ok, with
    /// `reboot_required = false`).
    fn clear<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ApplyOutcome, ProviderError>>
                + Send
                + 'a,
        >,
    >;
}

/// Default provider for board classes without a hardware-audio
/// mechanism (generic x86, NUC, MCU follower devices). Returns
/// "Unknown" board profile, empty `ActiveConfig`, and
/// `ProviderError::NotApplicable` on every apply / clear.
pub struct NoopProvider {
    /// Profile label this provider reports through [`board_profile`].
    /// Defaults to `"Unknown"`.
    ///
    /// [`board_profile`]: HardwareAudioProvider::board_profile
    pub profile_label: String,
}

impl NoopProvider {
    /// Construct a noop provider with an explicit profile label.
    pub fn new(profile_label: impl Into<String>) -> Self {
        Self {
            profile_label: profile_label.into(),
        }
    }
}

impl Default for NoopProvider {
    fn default() -> Self {
        Self::new("Unknown")
    }
}

impl AmixerReader for NoopProvider {
    fn read_control<'a>(
        &'a self,
        _card_hint: &'a str,
        control_name: &'a str,
    ) -> Pin<Box<dyn Future<Output = AmixerReadOutcome> + Send + 'a>> {
        let control = control_name.to_string();
        Box::pin(async move {
            AmixerReadOutcome::CardUnknown {
                reason: format!(
                    "noop provider has no bound ALSA card; cannot probe '{control}'"
                ),
            }
        })
    }
}

impl AmixerWriter for NoopProvider {
    fn write_control<'a>(
        &'a self,
        _card_hint: &'a str,
        control_name: &'a str,
        _value: AmixerWriteValue,
    ) -> Pin<Box<dyn Future<Output = AmixerWriteOutcome> + Send + 'a>> {
        let control = control_name.to_string();
        Box::pin(async move {
            AmixerWriteOutcome::CardUnknown {
                reason: format!(
                    "noop provider has no bound ALSA card; cannot apply '{control}'"
                ),
            }
        })
    }
}

impl HardwareAudioProvider for NoopProvider {
    fn board_profile<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<String, ProviderError>> + Send + 'a>>
    {
        Box::pin(async move { Ok(self.profile_label.clone()) })
    }

    fn current_config<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ActiveConfig, ProviderError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move { Ok(ActiveConfig::unset(String::new())) })
    }

    fn apply<'a>(
        &'a self,
        _entry: &'a DacEntry,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ApplyOutcome, ProviderError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move { Err(ProviderError::NotApplicable) })
    }

    fn clear<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ApplyOutcome, ProviderError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move { Err(ProviderError::NotApplicable) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_provider_returns_unknown_profile() {
        let p = NoopProvider::default();
        let profile = p.board_profile().await.expect("noop returns Ok");
        assert_eq!(profile, "Unknown");
    }

    #[tokio::test]
    async fn noop_provider_apply_returns_not_applicable() {
        let p = NoopProvider::default();
        let entry = DacEntry {
            id: "x".into(),
            display_name: "Y".into(),
            overlay: "hifiberry-dac".into(),
            alsa_card_hint: String::new(),
            alsa_num_hint: 0,
            in_card_mixer: String::new(),
            companion_modules: Vec::new(),
            init_script: String::new(),
            eeprom_names: Vec::new(),
            i2c_address: String::new(),
            needs_reboot_on_apply: true,
            advanced_settings_enabled: true,
            dsp_options: Vec::new(),
            provenance: String::new(),
        };
        let err = p.apply(&entry).await.expect_err("expected NotApplicable");
        assert!(matches!(err, ProviderError::NotApplicable));
    }

    #[tokio::test]
    async fn noop_provider_current_config_is_unset() {
        let p = NoopProvider::default();
        let cfg = p.current_config().await.expect("Ok");
        assert!(cfg.overlay.is_empty());
        assert!(cfg.catalogue_id.is_none());
        assert!(cfg.alsacard_hint.is_none());
        assert!(cfg.mixer_hint.is_none());
        assert!(cfg.boot_config_path.is_empty());
    }
}
