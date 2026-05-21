//! Rockchip hardware-audio provider.
//!
//! Owns the managed dtoverlay block in the Rockchip Tinkerboard
//! family's `/boot/hw_intf.conf` file. Unlike the Pi where the
//! overlay tokens live alongside everything else in
//! `/boot/firmware/config.txt`, the Tinkerboard image keeps a
//! dedicated `hw_intf.conf` whose managed lines are prefixed with
//! `intf:` — the Volumio image's `i2s_dacs` plugin writes the
//! same banner-fenced block this provider owns.
//!
//! The block is a banner-fenced 2-line region — banner header
//! plus a single `intf:dtoverlay=<token>` line. On every write,
//! the provider strips any legacy Volumio banner so a host
//! migrated from a prior Volumio install converges on the evo
//! block without operator intervention. Writes go through
//! `sudo -n tee` against a narrow grant; the plugin never runs
//! as root.

use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;

use regex::Regex;

use crate::amixer_subprocess::{
    amixer_cget_via_subprocess, amixer_cset_via_subprocess,
    amixer_scontrols_via_subprocess,
};
#[cfg(test)]
use crate::dsp::LiveControlState;
use crate::dsp::{AmixerListOutcome, AmixerReadOutcome, AmixerReader};
#[cfg(test)]
use crate::dsp_pool::ControlType;
use crate::evo_catalog::DacEntry;
use crate::provider::{
    ActiveConfig, AmixerWriteOutcome, AmixerWriteValue, AmixerWriter,
    ApplyOutcome, HardwareAudioProvider, ProviderError,
};
use crate::provider_pi::{sudo_tee_write, EVO_I2S_BANNER_LINE};

/// Combined managed-block regex matching either the evo banner or
/// the legacy Volumio banner, followed by the
/// `intf:dtoverlay=<token>` line. `(?m)` enables multi-line mode;
/// `\r?\n` admits CRLF line endings.
const MANAGED_BLOCK_REGEX_SRC: &str = r"(?m)^#### (?:evo|Volumio) i2s setting below: do not alter ####\r?\n\s*intf:dtoverlay=[^\r\n]*\r?\n";

fn validate_overlay_token(overlay: &str) -> Result<(), ProviderError> {
    if overlay.is_empty() {
        return Err(ProviderError::InvalidOverlay("empty overlay".into()));
    }
    if !overlay.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || c == ','
            || c == '_'
            || c == '-'
            || c == '.'
    }) {
        return Err(ProviderError::InvalidOverlay(
            "invalid overlay characters".into(),
        ));
    }
    Ok(())
}

fn managed_block_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(MANAGED_BLOCK_REGEX_SRC).expect("managed block regex")
    })
}

/// Strip the managed block from `text` if present. Recognises
/// both the evo banner and the legacy Volumio banner so migration
/// is transparent.
pub fn strip_managed_block(text: &str) -> String {
    managed_block_regex().replace_all(text, "").to_string()
}

/// Strip every standalone `intf:dtoverlay=<overlay>` line so
/// re-applying an overlay that already appears outside the
/// managed block does not end up with the overlay declared
/// twice.
pub fn strip_duplicate_intf_dtoverlay_lines(
    text: &str,
    overlay: &str,
) -> Result<String, ProviderError> {
    let pat = format!(
        r"(?m)^\s*intf:dtoverlay=\s*{}\s*\r?\n",
        regex::escape(overlay)
    );
    let re = Regex::new(&pat).map_err(|e| {
        ProviderError::InvalidOverlay(format!("regex compile: {e}"))
    })?;
    Ok(re.replace_all(text, "").to_string())
}

/// Compose the new `hw_intf.conf` text given the current text +
/// the overlay token to apply. Pure function — no IO, fully
/// unit-testable.
pub fn compose_apply_text(
    current: &str,
    overlay: &str,
) -> Result<String, ProviderError> {
    validate_overlay_token(overlay)?;
    let mut out = strip_managed_block(current);
    out = strip_duplicate_intf_dtoverlay_lines(&out, overlay)?;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(EVO_I2S_BANNER_LINE);
    out.push_str(&format!("intf:dtoverlay={overlay}\n"));
    Ok(out)
}

/// Compose the new `hw_intf.conf` text for a clear operation.
/// Pure function — strips the managed block, leaves the rest
/// intact.
pub fn compose_clear_text(current: &str) -> String {
    strip_managed_block(current)
}

/// Extract the active dtoverlay token from a `hw_intf.conf`
/// text. Returns the empty string when no managed block is
/// present.
pub fn extract_active_overlay(text: &str) -> String {
    let re = match Regex::new(
        r"(?m)^#### (?:evo|Volumio) i2s setting below: do not alter ####\r?\n\s*intf:dtoverlay=(?P<token>[^\r\n]+)\r?\n",
    ) {
        Ok(r) => r,
        Err(_) => return String::new(),
    };
    re.captures(text)
        .and_then(|caps| {
            caps.name("token").map(|m| m.as_str().trim().to_string())
        })
        .unwrap_or_default()
}

/// Resolve the Tinkerboard family's boot-config path. Honours
/// the `EVO_HW_INTF_PATH` override for tests + non-standard
/// images.
pub fn resolve_hw_intf_path() -> String {
    if let Ok(override_path) = std::env::var("EVO_HW_INTF_PATH") {
        if !override_path.is_empty() {
            return override_path;
        }
    }
    "/boot/hw_intf.conf".into()
}

/// Read the hw_intf.conf file. Direct read first; on
/// hardened images where the file is not world-readable, falls
/// back to `sudo -n cat` against the narrow grant declared in
/// `dist/sudoers.d/evo-hardware-audio.in`. Mirrors PiProvider's
/// `read_boot_config` discipline so the two providers behave
/// the same on permission-denied.
async fn read_hw_intf(path: &str) -> Result<String, ProviderError> {
    match tokio::fs::read_to_string(path).await {
        Ok(s) => return Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {}
        Err(e) => {
            return Err(ProviderError::BootConfigReadFailed(format!(
                "{path}: {e}"
            )));
        }
    }
    let out = tokio::process::Command::new("sudo")
        .args(["-n", "cat", path])
        .output()
        .await
        .map_err(|e| {
            ProviderError::BootConfigReadFailed(format!("sudo cat {path}: {e}"))
        })?;
    if !out.status.success() {
        return Err(ProviderError::BootConfigReadFailed(format!(
            "sudo cat {path} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Concrete Rockchip provider — backed by the `hw_intf.conf`
/// file at [`resolve_hw_intf_path`].
pub struct RockchipProvider {
    hw_intf_path: String,
    /// Test override: when set, [`apply`] / [`clear`] write to
    /// this in-memory state rather than calling `sudo -n tee`.
    /// Production path always uses the file system.
    #[cfg(test)]
    test_state: tokio::sync::Mutex<Option<String>>,
    #[cfg(test)]
    test_amixer_reads: tokio::sync::Mutex<
        std::collections::HashMap<(String, String), AmixerReadOutcome>,
    >,
    #[cfg(test)]
    test_amixer_writes:
        tokio::sync::Mutex<Vec<(String, String, AmixerWriteValue)>>,
    #[cfg(test)]
    test_amixer_write_capture_active: std::sync::atomic::AtomicBool,
}

impl RockchipProvider {
    /// Construct a Rockchip provider bound to the resolved
    /// `hw_intf.conf` path.
    pub fn new() -> Self {
        Self {
            hw_intf_path: resolve_hw_intf_path(),
            #[cfg(test)]
            test_state: tokio::sync::Mutex::new(None),
            #[cfg(test)]
            test_amixer_reads: tokio::sync::Mutex::new(
                std::collections::HashMap::new(),
            ),
            #[cfg(test)]
            test_amixer_writes: tokio::sync::Mutex::new(Vec::new()),
            #[cfg(test)]
            test_amixer_write_capture_active:
                std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Test-only constructor seeding the in-memory hw_intf.conf
    /// state. Reads return the seeded text; writes update it.
    /// No filesystem IO happens.
    #[cfg(test)]
    pub fn for_tests(initial: impl Into<String>) -> Self {
        Self {
            hw_intf_path: "/tmp/evo-hardware-audio-test/hw_intf.conf".into(),
            test_state: tokio::sync::Mutex::new(Some(initial.into())),
            test_amixer_reads: tokio::sync::Mutex::new(
                std::collections::HashMap::new(),
            ),
            test_amixer_writes: tokio::sync::Mutex::new(Vec::new()),
            test_amixer_write_capture_active:
                std::sync::atomic::AtomicBool::new(false),
        }
    }

    #[cfg(test)]
    async fn current_text(&self) -> Option<String> {
        self.test_state.lock().await.clone()
    }

    /// Test-only helper: register a canned amixer-cget outcome
    /// for the given (card, control) pair. Subsequent calls to
    /// [`AmixerReader::read_control`] with this pair return the
    /// stubbed outcome instead of shelling to the system amixer.
    #[cfg(test)]
    pub async fn stub_amixer_read(
        &self,
        card: &str,
        control: &str,
        outcome: AmixerReadOutcome,
    ) {
        self.test_amixer_reads
            .lock()
            .await
            .insert((card.to_string(), control.to_string()), outcome);
    }

    /// Test-only helper: activate write capture. After this call,
    /// every [`AmixerWriter::write_control`] invocation pushes
    /// (card, control, value) into the internal capture vec and
    /// returns `AmixerWriteOutcome::Applied`; no amixer subprocess
    /// runs.
    #[cfg(test)]
    pub fn enable_amixer_write_capture(&self) {
        self.test_amixer_write_capture_active
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Test-only helper: drain the captured writes for assertion.
    #[cfg(test)]
    pub async fn captured_amixer_writes(
        &self,
    ) -> Vec<(String, String, AmixerWriteValue)> {
        self.test_amixer_writes.lock().await.clone()
    }

    #[cfg(test)]
    async fn test_amixer_read(
        &self,
        card_hint: &str,
        control_name: &str,
    ) -> Option<AmixerReadOutcome> {
        let key = (card_hint.to_string(), control_name.to_string());
        self.test_amixer_reads.lock().await.get(&key).cloned()
    }

    #[cfg(test)]
    async fn test_amixer_write(
        &self,
        card_hint: &str,
        control_name: &str,
        value: &AmixerWriteValue,
    ) -> Option<AmixerWriteOutcome> {
        if !self
            .test_amixer_write_capture_active
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return None;
        }
        self.test_amixer_writes.lock().await.push((
            card_hint.to_string(),
            control_name.to_string(),
            value.clone(),
        ));
        Some(AmixerWriteOutcome::Applied)
    }
}

impl Default for RockchipProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl HardwareAudioProvider for RockchipProvider {
    fn board_profile<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<String, ProviderError>> + Send + 'a>>
    {
        Box::pin(async move { Ok("Tinkerboard".into()) })
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
        Box::pin(async move {
            #[cfg(test)]
            {
                if let Some(text) = self.current_text().await {
                    let overlay = extract_active_overlay(&text);
                    return Ok(ActiveConfig {
                        overlay,
                        catalogue_id: None,
                        display_name: None,
                        alsacard_hint: None,
                        mixer_hint: None,
                        boot_config_path: self.hw_intf_path.clone(),
                    });
                }
            }
            let text = read_hw_intf(&self.hw_intf_path).await?;
            let overlay = extract_active_overlay(&text);
            Ok(ActiveConfig {
                overlay,
                catalogue_id: None,
                display_name: None,
                alsacard_hint: None,
                mixer_hint: None,
                boot_config_path: self.hw_intf_path.clone(),
            })
        })
    }

    fn apply<'a>(
        &'a self,
        entry: &'a DacEntry,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ApplyOutcome, ProviderError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            if entry.overlay.is_empty() {
                return Err(ProviderError::InvalidOverlay(format!(
                    "catalogue id {} has no overlay token (module-only DACs are not supported in v1)",
                    entry.id
                )));
            }
            #[cfg(test)]
            {
                let mut guard = self.test_state.lock().await;
                if let Some(current) = guard.as_ref() {
                    let new_text = compose_apply_text(current, &entry.overlay)?;
                    *guard = Some(new_text);
                    return Ok(ApplyOutcome {
                        overlay: entry.overlay.clone(),
                        boot_config_path: self.hw_intf_path.clone(),
                        module_drop_in_installed: false,
                        reboot_required: true,
                    });
                }
            }
            let current = read_hw_intf(&self.hw_intf_path).await?;
            let new_text = compose_apply_text(&current, &entry.overlay)?;
            sudo_tee_write(&self.hw_intf_path, new_text.as_bytes()).await?;
            Ok(ApplyOutcome {
                overlay: entry.overlay.clone(),
                boot_config_path: self.hw_intf_path.clone(),
                module_drop_in_installed: false,
                reboot_required: true,
            })
        })
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
        Box::pin(async move {
            #[cfg(test)]
            {
                let mut guard = self.test_state.lock().await;
                if let Some(current) = guard.as_ref() {
                    let new_text = compose_clear_text(current);
                    let changed = new_text != *current;
                    *guard = Some(new_text);
                    return Ok(ApplyOutcome {
                        overlay: String::new(),
                        boot_config_path: self.hw_intf_path.clone(),
                        module_drop_in_installed: false,
                        reboot_required: changed,
                    });
                }
            }
            let current = read_hw_intf(&self.hw_intf_path).await?;
            let new_text = compose_clear_text(&current);
            let changed = new_text != current;
            if changed {
                sudo_tee_write(&self.hw_intf_path, new_text.as_bytes()).await?;
            }
            Ok(ApplyOutcome {
                overlay: String::new(),
                boot_config_path: self.hw_intf_path.clone(),
                module_drop_in_installed: false,
                reboot_required: changed,
            })
        })
    }
}

impl AmixerReader for RockchipProvider {
    fn list_controls<'a>(
        &'a self,
        card_hint: &'a str,
    ) -> Pin<Box<dyn Future<Output = AmixerListOutcome> + Send + 'a>> {
        Box::pin(async move {
            #[cfg(test)]
            {
                let stubs = self.test_amixer_reads.lock().await;
                let names: Vec<String> = stubs
                    .keys()
                    .filter(|(card, _)| card == card_hint)
                    .map(|(_, control)| control.clone())
                    .collect();
                if !names.is_empty() {
                    return AmixerListOutcome::Found(names);
                }
            }
            amixer_scontrols_via_subprocess(card_hint).await
        })
    }

    fn read_control<'a>(
        &'a self,
        card_hint: &'a str,
        control_name: &'a str,
    ) -> Pin<Box<dyn Future<Output = AmixerReadOutcome> + Send + 'a>> {
        Box::pin(async move {
            #[cfg(test)]
            {
                if let Some(outcome) =
                    self.test_amixer_read(card_hint, control_name).await
                {
                    return outcome;
                }
            }
            amixer_cget_via_subprocess(card_hint, control_name).await
        })
    }
}

impl AmixerWriter for RockchipProvider {
    fn write_control<'a>(
        &'a self,
        card_hint: &'a str,
        control_name: &'a str,
        value: AmixerWriteValue,
    ) -> Pin<Box<dyn Future<Output = AmixerWriteOutcome> + Send + 'a>> {
        Box::pin(async move {
            #[cfg(test)]
            {
                if let Some(outcome) = self
                    .test_amixer_write(card_hint, control_name, &value)
                    .await
                {
                    return outcome;
                }
            }
            amixer_cset_via_subprocess(card_hint, control_name, &value).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_overlay_token_admits_alnum_and_dot_underscore_hyphen() {
        assert!(validate_overlay_token("allo-piano-dac").is_ok());
        assert!(validate_overlay_token("rk3288_audio.iface").is_ok());
    }

    #[test]
    fn validate_overlay_token_rejects_empty_and_whitespace() {
        assert!(matches!(
            validate_overlay_token(""),
            Err(ProviderError::InvalidOverlay(_))
        ));
        assert!(matches!(
            validate_overlay_token("bad overlay"),
            Err(ProviderError::InvalidOverlay(_))
        ));
        assert!(matches!(
            validate_overlay_token("with$shell"),
            Err(ProviderError::InvalidOverlay(_))
        ));
    }

    #[test]
    fn strip_managed_block_removes_evo_banner_block() {
        let sample = "intf:i2c=on\n\n#### evo i2s setting below: do not alter ####\nintf:dtoverlay=allo-piano-dac\n";
        let out = strip_managed_block(sample);
        assert!(!out.contains("intf:dtoverlay=allo-piano-dac"));
        assert!(!out.contains("#### evo i2s"));
        assert!(out.contains("intf:i2c=on"));
    }

    #[test]
    fn strip_managed_block_removes_legacy_volumio_banner_block() {
        let sample = "intf:i2c=on\n\n#### Volumio i2s setting below: do not alter ####\nintf:dtoverlay=allo-piano-dac\n";
        let out = strip_managed_block(sample);
        assert!(!out.contains("intf:dtoverlay=allo-piano-dac"));
        assert!(!out.contains("Volumio i2s"));
        assert!(out.contains("intf:i2c=on"));
    }

    #[test]
    fn strip_duplicate_intf_dtoverlay_lines_removes_outside_block() {
        let sample = "intf:dtoverlay=allo-piano-dac\n#### evo i2s setting below: do not alter ####\nintf:dtoverlay=allo-piano-dac\n";
        let stripped = strip_managed_block(sample);
        let out =
            strip_duplicate_intf_dtoverlay_lines(&stripped, "allo-piano-dac")
                .expect("dedupe ok");
        assert_eq!(out.matches("intf:dtoverlay=allo-piano-dac").count(), 0);
    }

    #[test]
    fn compose_apply_text_renders_block_at_end_with_evo_banner() {
        let sample = "intf:i2c=on\n";
        let out =
            compose_apply_text(sample, "allo-piano-dac").expect("compose ok");
        assert!(out.contains("intf:i2c=on"));
        assert!(out.contains(EVO_I2S_BANNER_LINE.trim_end()));
        assert_eq!(out.matches("intf:dtoverlay=allo-piano-dac").count(), 1);
    }

    #[test]
    fn compose_apply_text_migrates_volumio_block_to_evo_block() {
        let sample = "intf:i2c=on\n#### Volumio i2s setting below: do not alter ####\nintf:dtoverlay=allo-piano-dac\n";
        let out = compose_apply_text(sample, "allo-boss").expect("compose ok");
        assert!(!out.contains("Volumio i2s"));
        assert!(out.contains(EVO_I2S_BANNER_LINE.trim_end()));
        assert_eq!(out.matches("intf:dtoverlay=allo-boss").count(), 1);
        assert_eq!(out.matches("intf:dtoverlay=allo-piano-dac").count(), 0);
    }

    #[test]
    fn compose_apply_text_on_empty_input_produces_block_only() {
        let out = compose_apply_text("", "allo-piano-dac").expect("compose ok");
        assert!(out.contains(EVO_I2S_BANNER_LINE.trim_end()));
        assert!(out.contains("intf:dtoverlay=allo-piano-dac"));
    }

    #[test]
    fn compose_clear_text_removes_block() {
        let sample = "intf:i2c=on\n#### evo i2s setting below: do not alter ####\nintf:dtoverlay=allo-piano-dac\n";
        let out = compose_clear_text(sample);
        assert!(!out.contains("evo i2s"));
        assert!(!out.contains("intf:dtoverlay=allo-piano-dac"));
        assert!(out.contains("intf:i2c=on"));
    }

    #[test]
    fn extract_active_overlay_reads_evo_banner_block() {
        let sample = "intf:i2c=on\n#### evo i2s setting below: do not alter ####\nintf:dtoverlay=allo-piano-dac\n";
        assert_eq!(extract_active_overlay(sample), "allo-piano-dac");
    }

    #[test]
    fn extract_active_overlay_reads_legacy_volumio_block() {
        let sample = "intf:i2c=on\n#### Volumio i2s setting below: do not alter ####\nintf:dtoverlay=allo-boss\n";
        assert_eq!(extract_active_overlay(sample), "allo-boss");
    }

    #[test]
    fn extract_active_overlay_returns_empty_when_no_block() {
        let sample = "intf:i2c=on\n";
        assert_eq!(extract_active_overlay(sample), "");
    }

    #[tokio::test]
    async fn rockchip_provider_apply_then_clear_round_trips_in_memory() {
        let provider = RockchipProvider::for_tests("intf:i2c=on\n");
        let entry = DacEntry {
            id: "allo-piano-dac".into(),
            display_name: "Allo Piano DAC".into(),
            overlay: "allo-piano-dac".into(),
            alsa_card_hint: "sndallopianodac".into(),
            alsa_num_hint: 0,
            in_card_mixer: String::new(),
            companion_modules: Vec::new(),
            init_script: String::new(),
            eeprom_names: Vec::new(),
            i2c_address: String::new(),
            needs_reboot_on_apply: true,
            advanced_settings_enabled: true,
            dsp_options: Vec::new(),
            provenance: "volumio:dacs.json#allo-piano-dac".into(),
        };
        let outcome = provider.apply(&entry).await.expect("apply ok");
        assert_eq!(outcome.overlay, "allo-piano-dac");
        assert!(outcome.reboot_required);

        let active = provider.current_config().await.expect("read ok");
        assert_eq!(active.overlay, "allo-piano-dac");

        let clear = provider.clear().await.expect("clear ok");
        assert_eq!(clear.overlay, "");
        assert!(clear.reboot_required);

        let final_active = provider.current_config().await.expect("read ok");
        assert_eq!(final_active.overlay, "");
    }

    #[tokio::test]
    async fn rockchip_provider_apply_rejects_empty_catalogue_overlay() {
        let provider = RockchipProvider::for_tests("");
        let entry = DacEntry {
            id: "module-only".into(),
            display_name: "Module-only DAC".into(),
            overlay: String::new(),
            alsa_card_hint: String::new(),
            alsa_num_hint: 0,
            in_card_mixer: String::new(),
            companion_modules: vec!["snd-soc-module".into()],
            init_script: String::new(),
            eeprom_names: Vec::new(),
            i2c_address: String::new(),
            needs_reboot_on_apply: true,
            advanced_settings_enabled: true,
            dsp_options: Vec::new(),
            provenance: "test:module-only".into(),
        };
        let err = provider
            .apply(&entry)
            .await
            .expect_err("module-only rejected");
        assert!(matches!(err, ProviderError::InvalidOverlay(_)));
    }

    #[tokio::test]
    async fn rockchip_provider_clear_no_op_when_no_managed_block() {
        let provider = RockchipProvider::for_tests("intf:i2c=on\n");
        let outcome = provider.clear().await.expect("clear ok on no-op");
        assert!(!outcome.reboot_required);
    }

    #[tokio::test]
    async fn rockchip_provider_amixer_read_returns_stub_outcome_when_registered(
    ) {
        let provider = RockchipProvider::for_tests("");
        provider
            .stub_amixer_read(
                "TinkerCard",
                "DSP Program",
                AmixerReadOutcome::Found(LiveControlState {
                    control_type: ControlType::Enum,
                    current_value: serde_json::Value::String("None".into()),
                    enum_values: vec!["None".into(), "DAC".into()],
                    integer_min: None,
                    integer_max: None,
                }),
            )
            .await;
        let outcome = provider.read_control("TinkerCard", "DSP Program").await;
        match outcome {
            AmixerReadOutcome::Found(state) => {
                assert_eq!(state.enum_values, vec!["None", "DAC"]);
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rockchip_provider_amixer_write_capture_records_gesture() {
        let provider = RockchipProvider::for_tests("");
        provider.enable_amixer_write_capture();
        let outcome = provider
            .write_control(
                "TinkerCard",
                "DSP Program",
                AmixerWriteValue::EnumLabel("DAC".into()),
            )
            .await;
        assert!(matches!(outcome, AmixerWriteOutcome::Applied));
        let captured = provider.captured_amixer_writes().await;
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].0, "TinkerCard");
        assert_eq!(captured[0].1, "DSP Program");
        assert_eq!(captured[0].2, AmixerWriteValue::EnumLabel("DAC".into()));
    }

    #[tokio::test]
    async fn rockchip_provider_board_profile_returns_tinkerboard() {
        let provider = RockchipProvider::for_tests("");
        assert_eq!(provider.board_profile().await.expect("ok"), "Tinkerboard");
    }
}
