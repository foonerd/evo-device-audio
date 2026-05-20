//! Raspberry Pi hardware-audio provider.
//!
//! Owns the dtoverlay block in the Pi's boot config
//! (`/boot/firmware/config.txt` on Bookworm/Trixie, `/boot/config.txt`
//! on older images) plus the companion `i2c-dev` module drop-in at
//! `/etc/modules-load.d/evo-i2c-dev.conf`.
//!
//! The block is a banner-fenced 2-line region — banner header + a
//! single `dtoverlay=<token>` line. Operator-or-image-owned content
//! elsewhere in the file is untouched. On every write, the provider
//! also strips any legacy banner (Volumio's
//! `#### Volumio i2s setting below: do not alter ####`) so a host
//! migrated from a prior Volumio install converges on the evo block
//! without operator intervention.
//!
//! Writes go through `sudo -n tee` against the narrow grant in
//! `dist/sudoers.d/evo-hardware-audio.in`. The plugin never runs as
//! root; the grant is path-scoped to the two boot-config locations
//! plus the i2c-dev drop-in.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::OnceLock;

use regex::Regex;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::dsp::{AmixerReadOutcome, AmixerReader, LiveControlState};
use crate::dsp_pool::ControlType;
use crate::evo_catalog::DacEntry;
use crate::provider::{
    ActiveConfig, AmixerWriteOutcome, AmixerWriteValue, AmixerWriter,
    ApplyOutcome, HardwareAudioProvider, ProviderError,
};

/// Banner header marking the start of the plugin's managed block in
/// the boot config. Followed immediately by a single `dtoverlay=`
/// line; the two lines together are the full managed block.
pub const EVO_I2S_BANNER_LINE: &str =
    "#### evo i2s setting below: do not alter ####\n";

/// Legacy Volumio banner. Stripped on every write to migrate hosts
/// previously managed by `volumio-evo`'s `i2s.rs` onto the evo
/// block without operator intervention.
pub const VOLUMIO_I2S_BANNER_LINE: &str =
    "#### Volumio i2s setting below: do not alter ####\n";

/// Combined managed-block regex matching either the evo banner or
/// the legacy Volumio banner, followed by the `dtoverlay=` line.
/// `(?m)` enables multi-line mode; `\r?\n` admits CRLF line endings.
const MANAGED_BLOCK_REGEX_SRC: &str = r"(?m)^#### (?:evo|Volumio) i2s setting below: do not alter ####\r?\n\s*dtoverlay=[^\r\n]*\r?\n";

/// `dtoverlay=` payload grammar — ASCII alnum plus `,`, `_`, `-`,
/// `.`. The comma admits Pi5-style `,slave` parameter modifiers.
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

/// Strip the managed block from `text` if present. Recognises both
/// the evo banner and the legacy Volumio banner so migration is
/// transparent.
pub fn strip_managed_block(text: &str) -> String {
    managed_block_regex().replace_all(text, "").to_string()
}

/// Strip every standalone `dtoverlay=<overlay>` line from `text` so
/// re-applying an overlay that already appears outside the managed
/// block (e.g. under `[all]` from an image-time edit) does not end
/// up with the overlay declared twice.
pub fn strip_duplicate_dtoverlay_lines(
    text: &str,
    overlay: &str,
) -> Result<String, ProviderError> {
    let pat =
        format!(r"(?m)^\s*dtoverlay=\s*{}\s*\r?\n", regex::escape(overlay));
    let re = Regex::new(&pat).map_err(|e| {
        ProviderError::InvalidOverlay(format!("regex compile: {e}"))
    })?;
    Ok(re.replace_all(text, "").to_string())
}

/// Ensure `dtparam=i2c_arm=on` and `dtparam=i2s=on` are active in the
/// supplied boot-config text. Uncomments `#dtparam=i2c_arm=on` /
/// `#dtparam=i2s=on`; flips `dtparam=...=off` to `=on`; appends a
/// labelled block when neither variant is present.
pub fn ensure_raspberry_pi_i2c_i2s_dtparams(
    text: String,
) -> Result<String, ProviderError> {
    let mut out = text;
    let pairs: &[(&str, &str, &str)] = &[
        (
            r"(?m)^(\s*)#\s*dtparam=i2c_arm=on\s*$",
            "dtparam=i2c_arm=on",
            "i2c uncomment",
        ),
        (
            r"(?m)^(\s*)#\s*dtparam=i2s=on\s*$",
            "dtparam=i2s=on",
            "i2s uncomment",
        ),
        (
            r"(?m)^(\s*)dtparam=i2c_arm=(off|0|false)\s*$",
            "dtparam=i2c_arm=on",
            "i2c off→on",
        ),
        (
            r"(?m)^(\s*)dtparam=i2s=(off|0|false)\s*$",
            "dtparam=i2s=on",
            "i2s off→on",
        ),
    ];
    for (pat, repl, label) in pairs {
        let re = Regex::new(pat).map_err(|e| {
            ProviderError::InvalidOverlay(format!("{label} regex: {e}"))
        })?;
        out = re.replace_all(&out, *repl).to_string();
    }

    let has_i2c = Regex::new(r"(?m)^\s*dtparam=i2c_arm=on\s*$")
        .map_err(|e| {
            ProviderError::InvalidOverlay(format!("i2c check regex: {e}"))
        })?
        .is_match(&out);
    let has_i2s = Regex::new(r"(?m)^\s*dtparam=i2s=on\s*$")
        .map_err(|e| {
            ProviderError::InvalidOverlay(format!("i2s check regex: {e}"))
        })?
        .is_match(&out);

    if !has_i2c || !has_i2s {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(
            "\n# evo: I2S DAC — enable SoC I2C + I2S (do not remove)\n",
        );
        if !has_i2c {
            out.push_str("dtparam=i2c_arm=on\n");
        }
        if !has_i2s {
            out.push_str("dtparam=i2s=on\n");
        }
    }
    Ok(out)
}

/// Compose the new boot-config text given the current text + the
/// overlay token to apply. Pure function — no IO, fully unit-testable.
pub fn compose_apply_text(
    current: &str,
    overlay: &str,
) -> Result<String, ProviderError> {
    validate_overlay_token(overlay)?;
    let mut out = strip_managed_block(current);
    out = strip_duplicate_dtoverlay_lines(&out, overlay)?;
    out = ensure_raspberry_pi_i2c_i2s_dtparams(out)?;
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(EVO_I2S_BANNER_LINE);
    out.push_str(&format!("dtoverlay={overlay}\n"));
    Ok(out)
}

/// Compose the new boot-config text for a clear operation. Pure
/// function — strips the managed block, leaves the rest intact.
pub fn compose_clear_text(current: &str) -> String {
    strip_managed_block(current)
}

/// Extract the active dtoverlay token from a boot-config text. Returns
/// the empty string when no managed block is present.
pub fn extract_active_overlay(text: &str) -> String {
    let re = match Regex::new(
        r"(?m)^#### (?:evo|Volumio) i2s setting below: do not alter ####\r?\n\s*dtoverlay=(?P<token>[^\r\n]+)\r?\n",
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

/// Resolve the boot-config path the provider should target. Prefers
/// `/boot/firmware/config.txt` (Bookworm/Trixie default on Pi), falls
/// back to `/boot/config.txt`. Honours the `EVO_BOOT_CONFIG_PATH`
/// override for tests + non-standard images.
pub fn resolve_boot_config_path() -> String {
    if let Ok(override_path) = std::env::var("EVO_BOOT_CONFIG_PATH") {
        if !override_path.is_empty() {
            return override_path;
        }
    }
    if Path::new("/boot/firmware/config.txt").exists() {
        "/boot/firmware/config.txt".into()
    } else {
        "/boot/config.txt".into()
    }
}

/// Path the Pi i2c-dev modules-load.d drop-in lands at.
pub const PI_I2C_DEV_MODULES_FILE: &str =
    "/etc/modules-load.d/evo-i2c-dev.conf";

/// Resolve the host's board profile by reading
/// `/proc/device-tree/model` (Pi exposes the model string here) and
/// falling back to `/proc/cpuinfo`'s Model line. Honours the
/// `EVO_HARDWARE_AUDIO_PROFILE` override.
pub async fn resolve_board_profile() -> String {
    if let Ok(override_profile) = std::env::var("EVO_HARDWARE_AUDIO_PROFILE") {
        if !override_profile.is_empty() {
            return override_profile;
        }
    }
    if let Ok(model) =
        tokio::fs::read_to_string("/proc/device-tree/model").await
    {
        let model_trim = model.trim_end_matches('\0').trim();
        if model_trim.contains("Raspberry Pi") {
            return "Raspberry PI".into();
        }
    }
    if let Ok(cpuinfo) = tokio::fs::read_to_string("/proc/cpuinfo").await {
        for line in cpuinfo.lines() {
            if let Some(rest) = line.strip_prefix("Model") {
                if rest.contains("Raspberry Pi") {
                    return "Raspberry PI".into();
                }
            }
        }
    }
    "Unknown".into()
}

/// Read the boot config — direct read first; falls back to `sudo -n
/// cat` against the narrow grant when direct-read returns permission
/// denied.
async fn read_boot_config(path: &str) -> Result<String, ProviderError> {
    match tokio::fs::read_to_string(path).await {
        Ok(s) => return Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {}
        Err(e) => {
            return Err(ProviderError::BootConfigReadFailed(format!(
                "{path}: {e}"
            )));
        }
    }
    let out = Command::new("sudo")
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

/// Write the boot config via `sudo -n tee`. The grant in
/// `dist/sudoers.d/evo-hardware-audio.in` is path-scoped.
/// `sudo -n rm <path>` runner — the cleanup counterpart to
/// [`sudo_tee_write`]. Used by the modder remove path to clean
/// up DTBO blobs from `/boot/firmware/overlays/`. Missing files
/// (already-removed; idempotent) treated as success.
pub(crate) async fn sudo_rm(path: &str) -> Result<(), ProviderError> {
    let output = Command::new("sudo")
        .args(["-n", "rm", "-f", path])
        .output()
        .await
        .map_err(|e| {
            ProviderError::BootConfigWriteFailed(format!(
                "spawn sudo rm {path}: {e}"
            ))
        })?;
    if !output.status.success() {
        return Err(ProviderError::BootConfigWriteFailed(format!(
            "sudo rm {path} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

/// `sudo -n tee <path>` writer reused by both the boot-config
/// dtoverlay path and the modder DTBO install path. Crate-public
/// so the modder module can install user-uploaded DTBO blobs to
/// `/boot/firmware/overlays/` via the narrow sudoers grant
/// declared in `dist/sudoers.d/evo-hardware-audio.in`.
pub(crate) async fn sudo_tee_write(
    path: &str,
    bytes: &[u8],
) -> Result<(), ProviderError> {
    let mut child = Command::new("sudo")
        .args(["-n", "tee", path])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            ProviderError::BootConfigWriteFailed(format!(
                "spawn sudo tee {path}: {e}"
            ))
        })?;
    {
        let stdin = child.stdin.as_mut().ok_or_else(|| {
            ProviderError::BootConfigWriteFailed("stdin unavailable".into())
        })?;
        stdin.write_all(bytes).await.map_err(|e| {
            ProviderError::BootConfigWriteFailed(format!("write {path}: {e}"))
        })?;
    }
    let out = child.wait_with_output().await.map_err(|e| {
        ProviderError::BootConfigWriteFailed(format!(
            "wait sudo tee {path}: {e}"
        ))
    })?;
    if !out.status.success() {
        return Err(ProviderError::BootConfigWriteFailed(format!(
            "sudo tee {path} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

async fn install_i2c_dev_module_drop_in() -> Result<bool, ProviderError> {
    let already = tokio::fs::read_to_string(PI_I2C_DEV_MODULES_FILE)
        .await
        .ok()
        .is_some_and(|s| {
            s.lines().any(|l| {
                let t = l.split('#').next().unwrap_or("").trim();
                t == "i2c-dev"
            })
        });
    if already {
        return Ok(false);
    }
    sudo_tee_write(PI_I2C_DEV_MODULES_FILE, b"i2c-dev\n")
        .await
        .map_err(|e| match e {
            ProviderError::BootConfigWriteFailed(s) => {
                ProviderError::ModuleDropInFailed(s)
            }
            other => other,
        })?;
    Ok(true)
}

/// Concrete Pi provider — backed by the boot-config file at
/// [`resolve_boot_config_path`].
pub struct PiProvider {
    boot_config_path: String,
    /// Test override: when set, [`apply`] / [`clear`] write to this
    /// in-memory state rather than calling `sudo -n tee`. Production
    /// path always uses the file system.
    #[cfg(test)]
    test_state: tokio::sync::Mutex<Option<String>>,
    /// Test override for amixer reads: keyed by (card, control) →
    /// outcome. When populated, [`AmixerReader::read_control`]
    /// returns the stubbed outcome instead of shelling to amixer.
    #[cfg(test)]
    test_amixer_reads: tokio::sync::Mutex<
        std::collections::HashMap<(String, String), AmixerReadOutcome>,
    >,
    /// Test capture for amixer writes: every write_control call
    /// pushes (card, control, value) here so tests can assert on
    /// what would have been invoked. The production amixer path is
    /// NOT executed when this field is populated; the provider
    /// returns `AmixerWriteOutcome::Applied` unconditionally.
    #[cfg(test)]
    test_amixer_writes:
        tokio::sync::Mutex<Vec<(String, String, AmixerWriteValue)>>,
    #[cfg(test)]
    test_amixer_write_capture_active: std::sync::atomic::AtomicBool,
}

impl PiProvider {
    /// Construct a Pi provider bound to the resolved boot-config
    /// path (`/boot/firmware/config.txt` preferred, falling back to
    /// `/boot/config.txt`).
    pub fn new() -> Self {
        Self {
            boot_config_path: resolve_boot_config_path(),
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

    /// Test-only constructor seeding the in-memory boot-config state.
    /// The provider returns the seeded text on reads and updates it
    /// on writes; no filesystem IO happens.
    #[cfg(test)]
    pub fn for_tests(initial: impl Into<String>) -> Self {
        Self {
            boot_config_path: "/tmp/evo-hardware-audio-test/config.txt".into(),
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

    /// Test-only helper: register a canned amixer-cget outcome for
    /// the given (card, control) pair. Subsequent calls to
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

impl Default for PiProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl HardwareAudioProvider for PiProvider {
    fn board_profile<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<String, ProviderError>> + Send + 'a>>
    {
        Box::pin(async move { Ok("Raspberry PI".into()) })
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
                        alsacard_hint: None,
                        mixer_hint: None,
                        boot_config_path: self.boot_config_path.clone(),
                    });
                }
            }
            let text = read_boot_config(&self.boot_config_path).await?;
            let overlay = extract_active_overlay(&text);
            Ok(ActiveConfig {
                overlay,
                catalogue_id: None,
                alsacard_hint: None,
                mixer_hint: None,
                boot_config_path: self.boot_config_path.clone(),
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
                        boot_config_path: self.boot_config_path.clone(),
                        module_drop_in_installed: false,
                        reboot_required: true,
                    });
                }
            }
            let current = read_boot_config(&self.boot_config_path).await?;
            let new_text = compose_apply_text(&current, &entry.overlay)?;
            sudo_tee_write(&self.boot_config_path, new_text.as_bytes()).await?;
            let module_drop_in_installed = match install_i2c_dev_module_drop_in(
            )
            .await
            {
                Ok(installed) => installed,
                Err(e) => {
                    tracing::warn!(
                        plugin = "org.evoframework.hardware.audio-config",
                        error = %e,
                        "i2c-dev module drop-in install failed; dtoverlay write completed"
                    );
                    false
                }
            };
            Ok(ApplyOutcome {
                overlay: entry.overlay.clone(),
                boot_config_path: self.boot_config_path.clone(),
                module_drop_in_installed,
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
                        boot_config_path: self.boot_config_path.clone(),
                        module_drop_in_installed: false,
                        reboot_required: changed,
                    });
                }
            }
            let current = read_boot_config(&self.boot_config_path).await?;
            let new_text = compose_clear_text(&current);
            let changed = new_text != current;
            if changed {
                sudo_tee_write(&self.boot_config_path, new_text.as_bytes())
                    .await?;
            }
            Ok(ApplyOutcome {
                overlay: String::new(),
                boot_config_path: self.boot_config_path.clone(),
                module_drop_in_installed: false,
                reboot_required: changed,
            })
        })
    }
}

// =============================================================
// PiProvider amixer adapter
// =============================================================

/// Parse `amixer cget` output into a [`LiveControlState`].
///
/// The amixer-cget output format is well-known but multi-line.
/// Example for an enum control:
///
/// ```text
/// numid=12,iface=MIXER,name='DSP Program'
///   ; type=ENUMERATED,access=rw------,values=1,items=4
///   ; Item #0 'None'
///   ; Item #1 'DAC'
///   ; Item #2 'DAC+Headphone'
///   ; Item #3 'Headphone'
///   : values=0
/// ```
///
/// For integer / db_scale controls:
///
/// ```text
/// numid=N,iface=MIXER,name='Clock Missing Period'
///   ; type=INTEGER,access=rw------,values=1,min=0,max=10000,step=0
///   : values=0
/// ```
///
/// For boolean controls:
///
/// ```text
/// numid=N,iface=MIXER,name='Soft Mute'
///   ; type=BOOLEAN,access=rw------,values=1
///   : values=on
/// ```
///
/// Returns `Err` with an operator-readable diagnostic on any
/// parse failure (missing type line, unknown type, malformed
/// items list, etc.).
pub fn parse_amixer_cget(output: &str) -> Result<LiveControlState, String> {
    let mut control_type: Option<ControlType> = None;
    let mut enum_items: Vec<String> = Vec::new();
    let mut integer_min: Option<i64> = None;
    let mut integer_max: Option<i64> = None;
    let mut raw_values: Option<String> = None;

    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("; type=") {
            // Type line: parse type token + (for integer) min/max.
            let mut parts = rest.split(',');
            let type_token = parts
                .next()
                .ok_or_else(|| format!("missing type token in '{trimmed}'"))?;
            control_type = Some(match type_token {
                "ENUMERATED" => ControlType::Enum,
                "INTEGER" => ControlType::Integer,
                "BOOLEAN" => ControlType::Boolean,
                other => {
                    return Err(format!(
                        "unrecognised amixer control type {other:?} in '{trimmed}'"
                    ));
                }
            });
            for part in parts {
                if let Some(min_str) = part.trim().strip_prefix("min=") {
                    integer_min = min_str.parse::<i64>().ok();
                } else if let Some(max_str) = part.trim().strip_prefix("max=") {
                    integer_max = max_str.parse::<i64>().ok();
                }
            }
        } else if let Some(rest) = trimmed.strip_prefix("; Item #") {
            // Enum item line. Format: `N 'label'`.
            if let Some(label_start) = rest.find('\'') {
                if let Some(label_end) = rest.rfind('\'') {
                    if label_end > label_start {
                        let label = &rest[label_start + 1..label_end];
                        enum_items.push(label.to_string());
                    }
                }
            }
        } else if let Some(rest) = trimmed.strip_prefix(": values=") {
            raw_values = Some(rest.to_string());
        }
    }

    let control_type = control_type
        .ok_or_else(|| "amixer cget output missing 'type=' line".to_string())?;
    let raw = raw_values.ok_or_else(|| {
        "amixer cget output missing final ': values=' line".to_string()
    })?;

    let (current_value, enum_values, integer_min, integer_max) =
        match control_type {
            ControlType::Enum => {
                // raw is the zero-based index into items.
                let idx: usize = raw.parse::<usize>().map_err(|e| {
                format!("amixer enum 'values=' is not an integer: {raw:?} ({e})")
            })?;
                let current = enum_items.get(idx).cloned().ok_or_else(|| {
                format!(
                    "amixer enum 'values={idx}' out of range for items list of length {}",
                    enum_items.len()
                )
            })?;
                (serde_json::Value::String(current), enum_items, None, None)
            }
            ControlType::Integer | ControlType::DbScale => {
                // raw is the integer value (possibly comma-separated
                // for multi-channel controls; take the first value).
                let first =
                    raw.split(',').next().unwrap_or(raw.as_str()).trim();
                let v: i64 = first.parse::<i64>().map_err(|e| {
                format!("amixer integer 'values=' is not an integer: {first:?} ({e})")
            })?;
                (
                    serde_json::Value::Number(v.into()),
                    Vec::new(),
                    integer_min,
                    integer_max,
                )
            }
            ControlType::Boolean => {
                // raw is "on" or "off" (sometimes "true"/"false").
                // Multi-channel controls emit comma-separated values;
                // take the first.
                let first =
                    raw.split(',').next().unwrap_or(raw.as_str()).trim();
                let v = matches!(
                    first.to_ascii_lowercase().as_str(),
                    "on" | "true" | "1"
                );
                (serde_json::Value::Bool(v), Vec::new(), None, None)
            }
        };

    Ok(LiveControlState {
        control_type,
        current_value,
        enum_values,
        integer_min,
        integer_max,
    })
}

/// Encode an [`AmixerWriteValue`] into the value string `amixer
/// cset` accepts. Enum labels pass verbatim (amixer accepts the
/// human-readable label); integers as decimal; booleans as on/off.
fn encode_amixer_write_value(value: &AmixerWriteValue) -> String {
    match value {
        AmixerWriteValue::EnumLabel(s) => s.clone(),
        AmixerWriteValue::Integer(n) => n.to_string(),
        AmixerWriteValue::Boolean(true) => "on".to_string(),
        AmixerWriteValue::Boolean(false) => "off".to_string(),
    }
}

impl AmixerReader for PiProvider {
    fn read_control<'a>(
        &'a self,
        card_hint: &'a str,
        control_name: &'a str,
    ) -> Pin<Box<dyn Future<Output = AmixerReadOutcome> + Send + 'a>> {
        Box::pin(async move {
            #[cfg(test)]
            {
                // Test override: when an in-memory stub is registered,
                // return its response instead of shelling to amixer.
                if let Some(outcome) =
                    self.test_amixer_read(card_hint, control_name).await
                {
                    return outcome;
                }
            }
            let name_arg = format!("name='{control_name}'");
            let output = match Command::new("amixer")
                .args(["-c", card_hint, "cget", &name_arg])
                .output()
                .await
            {
                Ok(o) => o,
                Err(e) => {
                    return AmixerReadOutcome::IntrospectionFailed {
                        reason: format!("spawn amixer cget: {e}"),
                    };
                }
            };
            if !output.status.success() {
                let stderr =
                    String::from_utf8_lossy(&output.stderr).to_string();
                // amixer returns non-zero when card or control are
                // unknown; distinguish by stderr content.
                let stderr_lc = stderr.to_ascii_lowercase();
                if stderr_lc.contains("no such file")
                    || stderr_lc.contains("card")
                {
                    return AmixerReadOutcome::CardUnknown {
                        reason: format!("amixer cget refused: {stderr}"),
                    };
                }
                if stderr_lc.contains("not found")
                    || stderr_lc.contains("unable to find")
                    || stderr_lc.contains("cannot find")
                {
                    return AmixerReadOutcome::NotPresent {
                        reason: format!(
                            "amixer reports control '{control_name}' not on card '{card_hint}': {stderr}"
                        ),
                    };
                }
                return AmixerReadOutcome::IntrospectionFailed {
                    reason: format!(
                        "amixer cget exit {}: {stderr}",
                        output.status.code().unwrap_or(-1)
                    ),
                };
            }
            let stdout = String::from_utf8_lossy(&output.stdout);
            match parse_amixer_cget(&stdout) {
                Ok(state) => AmixerReadOutcome::Found(state),
                Err(e) => AmixerReadOutcome::IntrospectionFailed {
                    reason: format!("amixer cget parse failed: {e}"),
                },
            }
        })
    }
}

impl AmixerWriter for PiProvider {
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
            let name_arg = format!("name='{control_name}'");
            let value_str = encode_amixer_write_value(&value);
            let output = match Command::new("amixer")
                .args(["-c", card_hint, "cset", &name_arg, &value_str])
                .output()
                .await
            {
                Ok(o) => o,
                Err(e) => {
                    return AmixerWriteOutcome::InvocationFailed {
                        reason: format!("spawn amixer cset: {e}"),
                    };
                }
            };
            if !output.status.success() {
                let stderr =
                    String::from_utf8_lossy(&output.stderr).to_string();
                let stderr_lc = stderr.to_ascii_lowercase();
                if stderr_lc.contains("no such file")
                    || stderr_lc.contains("card")
                {
                    return AmixerWriteOutcome::CardUnknown {
                        reason: format!("amixer cset refused: {stderr}"),
                    };
                }
                if stderr_lc.contains("not found")
                    || stderr_lc.contains("unable to find")
                    || stderr_lc.contains("cannot find")
                {
                    return AmixerWriteOutcome::NotPresent {
                        reason: format!(
                            "amixer reports control '{control_name}' not on card '{card_hint}': {stderr}"
                        ),
                    };
                }
                if stderr_lc.contains("invalid")
                    || stderr_lc.contains("out of range")
                    || stderr_lc.contains("bad value")
                {
                    return AmixerWriteOutcome::ValueRejected {
                        reason: format!(
                            "amixer rejected value '{value_str}': {stderr}"
                        ),
                    };
                }
                return AmixerWriteOutcome::InvocationFailed {
                    reason: format!(
                        "amixer cset exit {}: {stderr}",
                        output.status.code().unwrap_or(-1)
                    ),
                };
            }
            AmixerWriteOutcome::Applied
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_overlay_token_admits_pi5_slave_modifier() {
        assert!(validate_overlay_token("hifiberry-dacplus-std,slave").is_ok());
        assert!(validate_overlay_token("iqaudio-dacplus,unmute_amp").is_ok());
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
        let sample = "[all]\nenable_uart=1\n\n#### evo i2s setting below: do not alter ####\ndtoverlay=hifiberry-dac\n";
        let out = strip_managed_block(sample);
        assert!(!out.contains("dtoverlay=hifiberry-dac"));
        assert!(!out.contains("#### evo i2s"));
        assert!(out.contains("enable_uart=1"));
    }

    #[test]
    fn strip_managed_block_removes_legacy_volumio_banner_block() {
        let sample = "[all]\nenable_uart=1\n\n#### Volumio i2s setting below: do not alter ####\ndtoverlay=hifiberry-dacplus\n";
        let out = strip_managed_block(sample);
        assert!(!out.contains("dtoverlay=hifiberry-dacplus"));
        assert!(!out.contains("Volumio i2s"));
        assert!(out.contains("enable_uart=1"));
    }

    #[test]
    fn strip_duplicate_dtoverlay_lines_removes_outside_block() {
        let sample = "[all]\ndtoverlay=hifiberry-dac\n#### evo i2s setting below: do not alter ####\ndtoverlay=hifiberry-dac\n";
        let stripped = strip_managed_block(sample);
        let out = strip_duplicate_dtoverlay_lines(&stripped, "hifiberry-dac")
            .expect("dedupe ok");
        assert_eq!(out.matches("dtoverlay=hifiberry-dac").count(), 0);
    }

    #[test]
    fn ensure_dtparams_uncomments_stock_lines() {
        let sample =
            "# Optional hardware\n#dtparam=i2c_arm=on\n#dtparam=i2s=on\n";
        let out = ensure_raspberry_pi_i2c_i2s_dtparams(sample.to_string())
            .expect("ok");
        assert!(out.lines().any(|l| l.trim() == "dtparam=i2c_arm=on"));
        assert!(out.lines().any(|l| l.trim() == "dtparam=i2s=on"));
        assert!(!out.contains("#dtparam=i2c_arm=on"));
        assert!(!out.contains("#dtparam=i2s=on"));
    }

    #[test]
    fn ensure_dtparams_appends_when_absent() {
        let sample = "[all]\nenable_uart=1\n";
        let out = ensure_raspberry_pi_i2c_i2s_dtparams(sample.to_string())
            .expect("ok");
        assert!(out.contains("dtparam=i2c_arm=on"));
        assert!(out.contains("dtparam=i2s=on"));
        assert!(out.contains("evo: I2S DAC"));
    }

    #[test]
    fn ensure_dtparams_flips_off_to_on() {
        let sample = "dtparam=i2c_arm=off\ndtparam=i2s=off\n";
        let out = ensure_raspberry_pi_i2c_i2s_dtparams(sample.to_string())
            .expect("ok");
        assert_eq!(out.matches("dtparam=i2c_arm=on").count(), 1, "{out}");
        assert_eq!(out.matches("dtparam=i2s=on").count(), 1);
    }

    #[test]
    fn compose_apply_text_renders_block_at_end_with_evo_banner() {
        let sample = "[all]\nenable_uart=1\n";
        let out =
            compose_apply_text(sample, "hifiberry-dac").expect("compose ok");
        assert!(out.contains("enable_uart=1"));
        assert!(out.contains(EVO_I2S_BANNER_LINE.trim_end()));
        assert_eq!(out.matches("dtoverlay=hifiberry-dac").count(), 1);
        assert!(out.contains("dtparam=i2c_arm=on"));
        assert!(out.contains("dtparam=i2s=on"));
    }

    #[test]
    fn compose_apply_text_migrates_volumio_block_to_evo_block() {
        let sample = "[all]\nenable_uart=1\n#### Volumio i2s setting below: do not alter ####\ndtoverlay=hifiberry-dac\n";
        let out = compose_apply_text(sample, "hifiberry-dacplus,slave")
            .expect("compose ok");
        assert!(!out.contains("Volumio i2s"));
        assert!(out.contains(EVO_I2S_BANNER_LINE.trim_end()));
        assert_eq!(out.matches("dtoverlay=hifiberry-dacplus,slave").count(), 1);
        assert_eq!(out.matches("dtoverlay=hifiberry-dac\n").count(), 0);
    }

    #[test]
    fn compose_clear_text_removes_block() {
        let sample = "[all]\nenable_uart=1\n#### evo i2s setting below: do not alter ####\ndtoverlay=hifiberry-dac\n";
        let out = compose_clear_text(sample);
        assert!(!out.contains("evo i2s"));
        assert!(!out.contains("dtoverlay=hifiberry-dac"));
        assert!(out.contains("enable_uart=1"));
    }

    #[test]
    fn extract_active_overlay_reads_evo_banner_block() {
        let sample = "[all]\n#### evo i2s setting below: do not alter ####\ndtoverlay=allo-katana-dac-audio\n";
        assert_eq!(extract_active_overlay(sample), "allo-katana-dac-audio");
    }

    #[test]
    fn extract_active_overlay_reads_legacy_volumio_block() {
        let sample = "[all]\n#### Volumio i2s setting below: do not alter ####\ndtoverlay=hifiberry-digi\n";
        assert_eq!(extract_active_overlay(sample), "hifiberry-digi");
    }

    #[test]
    fn extract_active_overlay_returns_empty_when_no_block() {
        let sample = "[all]\nenable_uart=1\n";
        assert_eq!(extract_active_overlay(sample), "");
    }

    #[tokio::test]
    async fn pi_provider_apply_then_clear_round_trips_in_memory() {
        let provider = PiProvider::for_tests("[all]\nenable_uart=1\n");
        let entry = DacEntry {
            id: "hifiberry-dacplus".into(),
            display_name: "HiFiBerry DAC Plus".into(),
            overlay: "hifiberry-dacplus".into(),
            alsa_card_hint: "sndrpihifiberry".into(),
            alsa_num_hint: 2,
            in_card_mixer: "Digital".into(),
            companion_modules: Vec::new(),
            init_script: String::new(),
            eeprom_names: vec!["HiFiBerry DAC+".into()],
            i2c_address: "4d".into(),
            needs_reboot_on_apply: true,
            advanced_settings_enabled: true,
            dsp_options: vec![
                "DSP Program".into(),
                "Clock Missing Period".into(),
            ],
            provenance: "volumio:dacs.json#hifiberry-dacplus".into(),
        };
        let outcome = provider.apply(&entry).await.expect("apply ok");
        assert_eq!(outcome.overlay, "hifiberry-dacplus");
        assert!(outcome.reboot_required);

        let active = provider.current_config().await.expect("read ok");
        assert_eq!(active.overlay, "hifiberry-dacplus");

        let clear = provider.clear().await.expect("clear ok");
        assert_eq!(clear.overlay, "");
        assert!(clear.reboot_required);

        let final_active = provider.current_config().await.expect("read ok");
        assert_eq!(final_active.overlay, "");
    }

    #[tokio::test]
    async fn pi_provider_apply_rejects_empty_catalogue_overlay() {
        let provider = PiProvider::for_tests("[all]\n");
        let entry = DacEntry {
            id: "module-only".into(),
            display_name: "Module-only DAC".into(),
            overlay: String::new(),
            alsa_card_hint: String::new(),
            alsa_num_hint: 1,
            in_card_mixer: String::new(),
            companion_modules: vec!["snd-soc-allo-piano-dac".into()],
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
    async fn pi_provider_clear_no_op_when_no_managed_block() {
        let provider = PiProvider::for_tests("[all]\nenable_uart=1\n");
        let outcome = provider.clear().await.expect("clear ok on no-op");
        assert!(!outcome.reboot_required);
    }

    // ===== amixer cget parser =====

    #[test]
    fn parse_amixer_cget_enum_returns_resolved_label_and_value_set() {
        let raw = "numid=12,iface=MIXER,name='DSP Program'\n  ; type=ENUMERATED,access=rw------,values=1,items=4\n  ; Item #0 'None'\n  ; Item #1 'DAC'\n  ; Item #2 'DAC+Headphone'\n  ; Item #3 'Headphone'\n  : values=2\n";
        let state = parse_amixer_cget(raw).expect("parse ok");
        assert!(matches!(state.control_type, ControlType::Enum));
        assert_eq!(
            state.current_value,
            serde_json::Value::String("DAC+Headphone".into())
        );
        assert_eq!(
            state.enum_values,
            vec!["None", "DAC", "DAC+Headphone", "Headphone"]
        );
        assert!(state.integer_min.is_none());
        assert!(state.integer_max.is_none());
    }

    #[test]
    fn parse_amixer_cget_integer_returns_min_max_and_value() {
        let raw = "numid=4,iface=MIXER,name='Clock Missing Period'\n  ; type=INTEGER,access=rw------,values=1,min=0,max=10000,step=0\n  : values=2500\n";
        let state = parse_amixer_cget(raw).expect("parse ok");
        assert!(matches!(state.control_type, ControlType::Integer));
        assert_eq!(state.current_value, serde_json::json!(2500));
        assert_eq!(state.integer_min, Some(0));
        assert_eq!(state.integer_max, Some(10000));
        assert!(state.enum_values.is_empty());
    }

    #[test]
    fn parse_amixer_cget_boolean_returns_bool_value() {
        let raw_on = "numid=7,iface=MIXER,name='Soft Mute'\n  ; type=BOOLEAN,access=rw------,values=1\n  : values=on\n";
        let state_on = parse_amixer_cget(raw_on).expect("parse ok");
        assert!(matches!(state_on.control_type, ControlType::Boolean));
        assert_eq!(state_on.current_value, serde_json::Value::Bool(true));
        let raw_off = "numid=7,iface=MIXER,name='Soft Mute'\n  ; type=BOOLEAN,access=rw------,values=1\n  : values=off\n";
        let state_off = parse_amixer_cget(raw_off).expect("parse ok");
        assert_eq!(state_off.current_value, serde_json::Value::Bool(false));
    }

    #[test]
    fn parse_amixer_cget_handles_multi_channel_integer() {
        // Stereo controls emit values=N,M; the parser takes the
        // first channel's value (downstream wire-op surface
        // assumes mono-equivalent operator gestures; multi-channel
        // bind is a later refinement).
        let raw = "numid=1,iface=MIXER,name='Master Playback Volume'\n  ; type=INTEGER,access=rw------,values=2,min=-80,max=0,step=0\n  : values=-10,-10\n";
        let state = parse_amixer_cget(raw).expect("parse ok");
        assert_eq!(state.current_value, serde_json::json!(-10));
        assert_eq!(state.integer_min, Some(-80));
        assert_eq!(state.integer_max, Some(0));
    }

    #[test]
    fn parse_amixer_cget_refuses_missing_type_line() {
        let raw = "numid=N,iface=MIXER,name='x'\n  : values=0\n";
        let err = parse_amixer_cget(raw).unwrap_err();
        assert!(err.contains("type="));
    }

    #[test]
    fn parse_amixer_cget_refuses_missing_values_line() {
        let raw =
            "numid=N,iface=MIXER,name='x'\n  ; type=BOOLEAN,access=rw------,values=1\n";
        let err = parse_amixer_cget(raw).unwrap_err();
        assert!(err.contains("values="));
    }

    #[test]
    fn parse_amixer_cget_refuses_unrecognised_type() {
        let raw =
            "numid=N,iface=MIXER,name='x'\n  ; type=BYTES,access=rw------,values=1\n  : values=0\n";
        let err = parse_amixer_cget(raw).unwrap_err();
        assert!(err.contains("BYTES"));
    }

    #[test]
    fn parse_amixer_cget_refuses_enum_index_out_of_range() {
        let raw = "numid=N,iface=MIXER,name='x'\n  ; type=ENUMERATED,access=rw------,values=1,items=2\n  ; Item #0 'a'\n  ; Item #1 'b'\n  : values=5\n";
        let err = parse_amixer_cget(raw).unwrap_err();
        assert!(err.contains("out of range"));
    }

    // ===== amixer write encoder =====

    #[test]
    fn encode_amixer_write_value_round_trips() {
        assert_eq!(
            encode_amixer_write_value(&AmixerWriteValue::EnumLabel(
                "Slow Roll-Off".into()
            )),
            "Slow Roll-Off"
        );
        assert_eq!(
            encode_amixer_write_value(&AmixerWriteValue::Integer(-10)),
            "-10"
        );
        assert_eq!(
            encode_amixer_write_value(&AmixerWriteValue::Boolean(true)),
            "on"
        );
        assert_eq!(
            encode_amixer_write_value(&AmixerWriteValue::Boolean(false)),
            "off"
        );
    }

    // ===== PiProvider AmixerReader / AmixerWriter (stub path) =====

    #[tokio::test]
    async fn pi_provider_amixer_read_returns_stub_outcome_when_registered() {
        let provider = PiProvider::for_tests("[all]\n");
        provider
            .stub_amixer_read(
                "TestCard",
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
        let outcome = provider.read_control("TestCard", "DSP Program").await;
        match outcome {
            AmixerReadOutcome::Found(state) => {
                assert_eq!(state.enum_values, vec!["None", "DAC"]);
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pi_provider_amixer_write_capture_records_gesture() {
        let provider = PiProvider::for_tests("[all]\n");
        provider.enable_amixer_write_capture();
        let outcome = provider
            .write_control(
                "TestCard",
                "DSP Program",
                AmixerWriteValue::EnumLabel("DAC".into()),
            )
            .await;
        assert!(matches!(outcome, AmixerWriteOutcome::Applied));
        let captured = provider.captured_amixer_writes().await;
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].0, "TestCard");
        assert_eq!(captured[0].1, "DSP Program");
        assert_eq!(captured[0].2, AmixerWriteValue::EnumLabel("DAC".into()));
    }

    // ===== NoopProvider amixer (sanity) =====

    #[tokio::test]
    async fn noop_provider_amixer_read_returns_card_unknown() {
        use crate::provider::NoopProvider;
        let p = NoopProvider::default();
        let outcome = p.read_control("anycard", "anycontrol").await;
        assert!(matches!(outcome, AmixerReadOutcome::CardUnknown { .. }));
    }

    #[tokio::test]
    async fn noop_provider_amixer_write_returns_card_unknown() {
        use crate::provider::NoopProvider;
        let p = NoopProvider::default();
        let outcome = p
            .write_control(
                "anycard",
                "anycontrol",
                AmixerWriteValue::Boolean(true),
            )
            .await;
        assert!(matches!(outcome, AmixerWriteOutcome::CardUnknown { .. }));
    }
}
