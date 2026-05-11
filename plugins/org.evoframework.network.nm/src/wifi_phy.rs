// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! `iw` helpers for per-PHY capability detection and virtual AP
//! interface lifecycle.
//!
//! Canonical single-PHY AP+STA recipe:
//!
//! 1. Detect a `valid interface combinations` rule containing
//!    `managed` and `AP` on one `phy`.
//! 2. Create a secondary AP vif on that phy via
//!    `iw dev <sta_if> interface add <ap_if> type __ap`.
//! 3. Bind the STA NM profile to `sta_if` and the AP NM profile
//!    to `ap_if`. NetworkManager then keeps both active without
//!    a single-device collision.
//!
//! All I/O routes through a [`PrivilegedExec`] so the per-tool
//! dispatch strategy (direct vs `sudo -n`) is owned in one place.
//! The pure parsers (PHY info, `iw dev` output, frequency-to-
//! channel / band conversion) are extracted as free functions so
//! the test suite exercises them without touching the host.

use std::collections::HashMap;
use std::time::Duration;

use evo_plugin_sdk::contract::PluginError;

use crate::nmcli_dispatch::PrivilegedExec;

/// Per-band support flags for one PHY, derived from the
/// `Frequencies:` blocks of `iw phy <phy> info`. A band is
/// supported when at least one of its centre-frequencies is
/// listed and not flagged `disabled` by the kernel's regulatory
/// domain.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PhyBandSupport {
    /// 2.4 GHz family (channels 1–14).
    pub ghz_2_4: bool,
    /// 5 GHz family (channels 36–177).
    pub ghz_5: bool,
    /// 6 GHz family (UNII-5 through UNII-8 — channels 1–233 in
    /// the 6 GHz numbering scheme).
    pub ghz_6: bool,
}

impl PhyBandSupport {
    /// Number of distinct bands the PHY supports. Used to rank
    /// radios when picking the STA-traffic-bearer.
    pub fn band_count(&self) -> u32 {
        u32::from(self.ghz_2_4) + u32::from(self.ghz_5) + u32::from(self.ghz_6)
    }

    /// `true` when this PHY supports any band above 2.4 GHz —
    /// the "fast-band-capable" predicate the role-assignment
    /// heuristic uses to favour 5 / 6 GHz for STA traffic.
    pub fn has_fast_band(&self) -> bool {
        self.ghz_5 || self.ghz_6
    }
}

/// Per-`phy` capability summary surfaced by [`phy_capability`].
#[derive(Debug, Clone, Default)]
pub struct PhyCapability {
    /// `phy` name (e.g. `phy0`); kept for diagnostics + `Debug`.
    pub phy: String,
    /// True iff any `valid interface combinations` line allows
    /// `managed` and `AP` together — the single-PHY concurrent-
    /// STA+AP indicator.
    pub supports_managed_plus_ap: bool,
    /// Best `#channels <= N` observed across combinations. `≥ 1`
    /// implies AP must share the STA channel when both run on
    /// the same PHY.
    pub max_channels: u32,
    /// `Supported interface modes` list (informational).
    pub interface_modes: Vec<String>,
    /// Per-band support derived from the `Frequencies:` blocks
    /// of the same `iw phy <phy> info` output. Drives multi-
    /// radio role assignment.
    pub bands: PhyBandSupport,
}

/// Per-interface Wi-Fi device row parsed from `iw dev`.
#[derive(Debug, Clone)]
pub struct WifiDev {
    /// Kernel netdev name (e.g. `wlan0`, `ap0`).
    pub ifname: String,
    /// Backing `phy` name (e.g. `phy0`); multiple interfaces on
    /// the same PHY appear with the same `phy` value.
    pub phy: String,
    /// `iw dev … type` value — `managed` (STA), `AP`, `IBSS`,
    /// `monitor`, etc.
    pub iftype: String,
}

/// Best-effort STA link info parsed from `iw dev <sta> link`.
#[derive(Debug, Clone, Default)]
pub struct StaLinkInfo {
    /// `true` when `iw` reported `Connected to …`; `false` when
    /// the link was `Not connected` or the probe failed.
    pub connected: bool,
    /// Centre-frequency in MHz of the current association, if
    /// any. `None` when the probe could not extract it.
    pub freq_mhz: Option<u32>,
    /// Channel number derived from [`Self::freq_mhz`] via
    /// [`freq_to_channel`].
    pub channel: Option<u32>,
    /// NM `802-11-wireless.band` value: `bg` (2.4 GHz), `a`
    /// (5 GHz), `6GHz`.
    pub band: Option<String>,
}

/// Run `iw <args>` through `exec` and return stdout (UTF-8 lossy).
/// On non-zero exit, returns the stderr as a transient error so
/// callers can decide whether to log + continue or surface.
pub async fn iw_output(
    exec: &dyn PrivilegedExec,
    iw_path: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<String, PluginError> {
    let out = exec.dispatch(iw_path, args, timeout).await?;
    if !out.status.success() {
        let code = out.status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        return Err(PluginError::Transient(format!(
            "iw {} failed (exit {}): {}\n{}",
            args.join(" "),
            code,
            stderr.trim(),
            stdout.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run `iw phy <phy> info` and parse the capability summary.
pub async fn phy_capability(
    exec: &dyn PrivilegedExec,
    iw_path: &str,
    phy: &str,
    timeout: Duration,
) -> Result<PhyCapability, PluginError> {
    let phy_norm = phy.trim().trim_start_matches("phy");
    let phy_arg = format!("phy{phy_norm}");
    let out = iw_output(exec, iw_path, &[&phy_arg, "info"], timeout).await?;
    Ok(parse_phy_info(&phy_arg, &out))
}

/// Pure parser for `iw phy <phy> info` — extracted so tests can
/// exercise canned fixtures without spawning `iw`.
pub fn parse_phy_info(phy_name: &str, raw: &str) -> PhyCapability {
    let mut cap = PhyCapability {
        phy: phy_name.to_string(),
        ..Default::default()
    };

    let mut in_modes = false;
    for line in raw.lines() {
        let lt = line.trim();
        if lt == "Supported interface modes:" {
            in_modes = true;
            continue;
        }
        if in_modes {
            if let Some(rest) = lt.strip_prefix('*') {
                cap.interface_modes.push(rest.trim().to_string());
            } else if !lt.is_empty()
                && !line.starts_with(' ')
                && !line.starts_with('\t')
            {
                in_modes = false;
            }
        }
    }

    let combos = extract_interface_combinations(raw);
    for combo in &combos {
        if combo_has_managed(combo) && combo_has_ap(combo) {
            cap.supports_managed_plus_ap = true;
        }
        if let Some(n) = parse_channel_count(combo) {
            if n > cap.max_channels {
                cap.max_channels = n;
            }
        }
    }
    cap.bands = parse_phy_band_support(raw);
    cap
}

/// Parse the `Frequencies:` blocks of `iw phy <phy> info` and
/// flag the bands that contain at least one non-disabled channel.
/// `iw` renders disabled channels with a `(disabled)` suffix;
/// regulatory-restricted channels still count as "supported" for
/// inventory purposes.
fn parse_phy_band_support(raw: &str) -> PhyBandSupport {
    let mut bands = PhyBandSupport::default();
    let mut in_frequencies = false;
    for line in raw.lines() {
        let lt = line.trim();
        if lt.starts_with("Frequencies:") {
            in_frequencies = true;
            continue;
        }
        if in_frequencies {
            let is_indented = line.starts_with(' ') || line.starts_with('\t');
            if !is_indented && !lt.is_empty() {
                in_frequencies = false;
                continue;
            }
            // Expected shape: `* 5180 MHz [36] (22.0 dBm)` or
            // `* 2412 MHz [1] (20.0 dBm)`; disabled channels
            // end in `(disabled)`.
            let Some(rest) = lt.strip_prefix("* ") else {
                continue;
            };
            if rest.contains("(disabled)") {
                continue;
            }
            let mhz_token = rest.split_whitespace().next().unwrap_or("");
            let Ok(mhz) = mhz_token.parse::<u32>() else {
                continue;
            };
            match mhz {
                2400..=2500 => bands.ghz_2_4 = true,
                4900..=5900 => bands.ghz_5 = true,
                5901..=7200 => bands.ghz_6 = true,
                _ => {}
            }
        }
    }
    bands
}

/// Extract each `* …` combination line under
/// `valid interface combinations:`, joining wrapped lines.
fn extract_interface_combinations(raw: &str) -> Vec<String> {
    let mut combos: Vec<String> = Vec::new();
    let mut in_combos = false;
    let mut current = String::new();
    for line in raw.lines() {
        let lt = line.trim_end();
        if lt.trim() == "valid interface combinations:" {
            in_combos = true;
            continue;
        }
        if !in_combos {
            continue;
        }
        let is_indented = line.starts_with(' ') || line.starts_with('\t');
        if !is_indented && !lt.is_empty() {
            if !current.is_empty() {
                combos.push(current.clone());
                current.clear();
            }
            in_combos = false;
            continue;
        }
        let trimmed = lt.trim();
        if let Some(rest) = trimmed.strip_prefix("* ") {
            if !current.is_empty() {
                combos.push(current.clone());
                current.clear();
            }
            current.push_str(rest);
        } else if !trimmed.is_empty() {
            current.push(' ');
            current.push_str(trimmed);
        }
    }
    if !current.is_empty() {
        combos.push(current);
    }
    combos
}

fn combo_has_managed(combo: &str) -> bool {
    has_mode_token(combo, "managed")
}

fn combo_has_ap(combo: &str) -> bool {
    has_mode_token(combo, "AP")
}

/// Match a bracketed-mode token (`managed`, `AP`, ...) on a word
/// boundary. The brackets / separator characters around modes in
/// `iw` output are: ` , { ( /` before and ` , } ) /` after.
fn has_mode_token(combo: &str, token: &str) -> bool {
    let bytes = combo.as_bytes();
    let tb = token.as_bytes();
    let mut i = 0;
    while i + tb.len() <= bytes.len() {
        if &bytes[i..i + tb.len()] == tb {
            let before_ok = i == 0
                || matches!(bytes[i - 1] as char, ' ' | ',' | '{' | '(' | '/');
            let after_idx = i + tb.len();
            let after_ok = after_idx >= bytes.len()
                || matches!(
                    bytes[after_idx] as char,
                    ' ' | ',' | '}' | ')' | '/'
                );
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn parse_channel_count(combo: &str) -> Option<u32> {
    let needle = "#channels <=";
    let idx = combo.find(needle)?;
    let rest = &combo[idx + needle.len()..];
    let s: String = rest
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    s.parse().ok()
}

/// `iw dev` → mapping `ifname → phy` (e.g. `wlan0` → `phy0`).
pub async fn wifi_iface_to_phy_map(
    exec: &dyn PrivilegedExec,
    iw_path: &str,
    timeout: Duration,
) -> Result<HashMap<String, String>, PluginError> {
    let out = iw_output(exec, iw_path, &["dev"], timeout).await?;
    Ok(parse_iface_to_phy_map(&out))
}

fn parse_iface_to_phy_map(raw: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut cur_phy: Option<String> = None;
    for line in raw.lines() {
        let lt = line.trim();
        if let Some(rest) = lt.strip_prefix("phy#") {
            cur_phy = Some(format!("phy{}", rest.trim()));
            continue;
        }
        if let Some(rest) = lt.strip_prefix("Interface ") {
            if let Some(phy) = cur_phy.as_ref() {
                map.insert(rest.trim().to_string(), phy.clone());
            }
        }
    }
    map
}

/// Resolve the backing `phy` for `ifname` by parsing `iw dev`.
/// Returns `Ok(None)` when the interface is not present.
pub async fn phy_for_ifname(
    exec: &dyn PrivilegedExec,
    iw_path: &str,
    ifname: &str,
    timeout: Duration,
) -> Result<Option<String>, PluginError> {
    let map = wifi_iface_to_phy_map(exec, iw_path, timeout).await?;
    Ok(map.get(ifname.trim()).cloned())
}

/// Parse `iw dev` and return one row per `Interface <name>`
/// block with its phy and `type`.
pub async fn list_wifi_devices(
    exec: &dyn PrivilegedExec,
    iw_path: &str,
    timeout: Duration,
) -> Result<Vec<WifiDev>, PluginError> {
    let out = iw_output(exec, iw_path, &["dev"], timeout).await?;
    Ok(parse_wifi_devices(&out))
}

fn parse_wifi_devices(raw: &str) -> Vec<WifiDev> {
    let mut rows: Vec<WifiDev> = Vec::new();
    let mut cur_phy: Option<String> = None;
    let mut cur_iface: Option<String> = None;
    let mut cur_type: Option<String> = None;
    for line in raw.lines() {
        let lt = line.trim();
        if let Some(rest) = lt.strip_prefix("phy#") {
            flush_wifi_dev(
                &mut rows,
                &mut cur_phy,
                &mut cur_iface,
                &mut cur_type,
            );
            cur_phy = Some(format!("phy{}", rest.trim()));
            continue;
        }
        if let Some(rest) = lt.strip_prefix("Interface ") {
            flush_wifi_dev_iface(
                &mut rows,
                &cur_phy,
                &mut cur_iface,
                &mut cur_type,
            );
            cur_iface = Some(rest.trim().to_string());
            continue;
        }
        if let Some(rest) = lt.strip_prefix("type ") {
            cur_type = Some(rest.trim().to_string());
        }
    }
    flush_wifi_dev(&mut rows, &mut cur_phy, &mut cur_iface, &mut cur_type);
    rows
}

fn flush_wifi_dev(
    rows: &mut Vec<WifiDev>,
    phy: &mut Option<String>,
    iface: &mut Option<String>,
    iftype: &mut Option<String>,
) {
    if let (Some(ifn), Some(ty)) = (iface.take(), iftype.take()) {
        if let Some(p) = phy.clone() {
            rows.push(WifiDev {
                ifname: ifn,
                phy: p,
                iftype: ty,
            });
        }
    }
}

fn flush_wifi_dev_iface(
    rows: &mut Vec<WifiDev>,
    phy: &Option<String>,
    iface: &mut Option<String>,
    iftype: &mut Option<String>,
) {
    if let (Some(ifn), Some(ty)) = (iface.take(), iftype.take()) {
        if let Some(p) = phy.clone() {
            rows.push(WifiDev {
                ifname: ifn,
                phy: p,
                iftype: ty,
            });
        }
    }
}

/// `iw dev <if>` present check — used to detect whether the AP
/// vif already exists before [`ensure_ap_vif_present`] adds it.
pub async fn iw_dev_exists(
    exec: &dyn PrivilegedExec,
    iw_path: &str,
    ifname: &str,
    timeout: Duration,
) -> bool {
    let name = ifname.trim();
    if name.is_empty() {
        return false;
    }
    match wifi_iface_to_phy_map(exec, iw_path, timeout).await {
        Ok(map) => map.contains_key(name),
        Err(_) => false,
    }
}

/// STA-capable := `iw` reports the interface type `managed`
/// (client) and not `AP`. Used by the UI / wire layer to filter
/// out virtual AP vifs (created via `iw … type __ap`) when
/// presenting Preferred Wi-Fi interface choices or enumerating
/// devices for client operations. When the interface is in some
/// other state (newly added, monitor, etc.) the PHY capability
/// list provides the fallback signal.
pub async fn is_sta_capable(
    exec: &dyn PrivilegedExec,
    iw_path: &str,
    ifname: &str,
    timeout: Duration,
) -> bool {
    let name = ifname.trim();
    if name.is_empty() {
        return false;
    }
    let Ok(devs) = list_wifi_devices(exec, iw_path, timeout).await else {
        return false;
    };
    let Some(dev) = devs.iter().find(|d| d.ifname == name) else {
        return false;
    };
    let ty_lc = dev.iftype.to_ascii_lowercase();
    if ty_lc == "ap" || ty_lc.contains("__ap") {
        return false;
    }
    if ty_lc == "managed" {
        return true;
    }
    match phy_capability(exec, iw_path, &dev.phy, timeout).await {
        Ok(cap) => cap
            .interface_modes
            .iter()
            .any(|m| m.eq_ignore_ascii_case("managed")),
        Err(_) => false,
    }
}

/// Create the AP vif on the same phy as `sta_if` iff it doesn't
/// already exist. Bringing the device up is NetworkManager's
/// responsibility when the AP profile activates — this helper
/// only registers the vif with the kernel.
pub async fn ensure_ap_vif_present(
    exec: &dyn PrivilegedExec,
    iw_path: &str,
    sta_if: &str,
    ap_if: &str,
    timeout: Duration,
) -> Result<(), PluginError> {
    let sta = sta_if.trim();
    let ap = ap_if.trim();
    if sta.is_empty() || ap.is_empty() || sta == ap {
        return Ok(());
    }
    if iw_dev_exists(exec, iw_path, ap, timeout).await {
        tracing::debug!(
            plugin = crate::PLUGIN_NAME,
            ap_if = ap,
            "iw: ap vif already present on phy; skipping add"
        );
        return Ok(());
    }
    tracing::info!(
        plugin = crate::PLUGIN_NAME,
        sta_if = sta,
        ap_if = ap,
        "iw: creating ap vif type __ap"
    );
    iw_output(
        exec,
        iw_path,
        &["dev", sta, "interface", "add", ap, "type", "__ap"],
        timeout,
    )
    .await?;
    Ok(())
}

/// Remove the AP vif if it exists. Idempotent.
pub async fn ensure_ap_vif_absent(
    exec: &dyn PrivilegedExec,
    iw_path: &str,
    ap_if: &str,
    timeout: Duration,
) -> Result<(), PluginError> {
    let ap = ap_if.trim();
    if ap.is_empty() {
        return Ok(());
    }
    if !iw_dev_exists(exec, iw_path, ap, timeout).await {
        return Ok(());
    }
    tracing::info!(
        plugin = crate::PLUGIN_NAME,
        ap_if = ap,
        "iw: removing ap vif"
    );
    iw_output(exec, iw_path, &["dev", ap, "del"], timeout).await?;
    Ok(())
}

/// Best-effort STA link info — never returns an error; a probe
/// failure surfaces as a `default()` (disconnected) view so
/// callers can render diagnostics without branching.
pub async fn sta_link_info(
    exec: &dyn PrivilegedExec,
    iw_path: &str,
    sta_if: &str,
    timeout: Duration,
) -> StaLinkInfo {
    let sta = sta_if.trim();
    if sta.is_empty() {
        return StaLinkInfo::default();
    }
    let out =
        match iw_output(exec, iw_path, &["dev", sta, "link"], timeout).await {
            Ok(s) => s,
            Err(_) => return StaLinkInfo::default(),
        };
    parse_sta_link_info(&out)
}

fn parse_sta_link_info(raw: &str) -> StaLinkInfo {
    let mut info = StaLinkInfo::default();
    for line in raw.lines() {
        let lt = line.trim();
        if lt.starts_with("Not connected") {
            info.connected = false;
            break;
        }
        if lt.starts_with("Connected to") {
            info.connected = true;
        }
        if let Some(rest) = lt.strip_prefix("freq:") {
            if let Ok(v) = rest.trim().parse::<u32>() {
                info.freq_mhz = Some(v);
                info.channel = freq_to_channel(v);
                info.band = freq_to_band(v);
            }
        }
    }
    info
}

/// Convert a centre-frequency in MHz to its NM channel number.
/// Covers 2.4 GHz (channels 1–13 / 14), 5 GHz, and 6 GHz UNII-5+.
pub fn freq_to_channel(mhz: u32) -> Option<u32> {
    match mhz {
        2412..=2472 => Some((mhz - 2407) / 5),
        2484 => Some(14),
        5000..=5895 => Some((mhz - 5000) / 5),
        5955..=7115 => Some((mhz - 5950) / 5),
        _ => None,
    }
}

/// Convert a centre-frequency to the NM `802-11-wireless.band`
/// value (`bg` / `a` / `6GHz`).
pub fn freq_to_band(mhz: u32) -> Option<String> {
    if mhz < 3000 {
        Some("bg".into())
    } else if (3000..5900).contains(&mhz) {
        Some("a".into())
    } else if mhz >= 5900 {
        Some("6GHz".into())
    } else {
        None
    }
}

/// Convenience: does the phy backing `sta_if` support concurrent
/// `managed + AP`? Logs the probed phy, detected capability,
/// and channel limit at `debug`.
pub async fn sta_phy_supports_concurrent_sta_ap(
    exec: &dyn PrivilegedExec,
    iw_path: &str,
    sta_if: &str,
    timeout: Duration,
) -> bool {
    let phy = match phy_for_ifname(exec, iw_path, sta_if, timeout).await {
        Ok(Some(phy)) => phy,
        Ok(None) | Err(_) => {
            tracing::debug!(
                plugin = crate::PLUGIN_NAME,
                sta_if = sta_if,
                "wifi_phy: no phy resolved (iw dev failed or ifname missing)"
            );
            return false;
        }
    };
    match phy_capability(exec, iw_path, &phy, timeout).await {
        Ok(cap) => {
            tracing::debug!(
                plugin = crate::PLUGIN_NAME,
                sta_if = sta_if,
                phy = %cap.phy,
                supports_managed_plus_ap = cap.supports_managed_plus_ap,
                max_channels = cap.max_channels,
                modes = ?cap.interface_modes,
                "wifi_phy: capability probe complete"
            );
            cap.supports_managed_plus_ap
        }
        Err(e) => {
            tracing::debug!(
                plugin = crate::PLUGIN_NAME,
                phy = %phy,
                error = %e,
                "wifi_phy: phy_capability failed"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PI5_PHY_INFO: &str = r#"
Wiphy phy0
	Supported interface modes:
		 * IBSS
		 * managed
		 * AP
		 * P2P-client
		 * P2P-GO
		 * P2P-device
	valid interface combinations:
		 * #{ managed } <= 2, #{ P2P-device } <= 1, #{ P2P-client, P2P-GO } <= 1,
		   total <= 3, #channels <= 2
		 * #{ managed } <= 1, #{ AP } <= 1, #{ P2P-client } <= 1, #{ P2P-device } <= 1,
		   total <= 4, #channels <= 1
	Device supports SAE with AUTHENTICATE command.
"#;

    const STA_ONLY_PHY_INFO: &str = r#"
Wiphy phy1
	Supported interface modes:
		 * managed
		 * monitor
	valid interface combinations:
		 * #{ managed } <= 1, #{ P2P-client, P2P-GO } <= 1,
		   total <= 2, #channels <= 1
"#;

    /// Dual-band phy info — 2.4 GHz channels 1–13 + 5 GHz UNII-1
    /// channels 36/40/44/48. Used to exercise `PhyBandSupport`.
    const DUAL_BAND_PHY_INFO: &str = r#"
Wiphy phy0
	Supported interface modes:
		 * managed
		 * AP
	valid interface combinations:
		 * #{ managed } <= 1, #{ AP } <= 1,
		   total <= 2, #channels <= 1
	Band 1:
		Frequencies:
			* 2412 MHz [1] (20.0 dBm)
			* 2417 MHz [2] (20.0 dBm)
			* 2462 MHz [11] (20.0 dBm)
	Band 2:
		Frequencies:
			* 5180 MHz [36] (22.0 dBm)
			* 5200 MHz [40] (22.0 dBm)
			* 5220 MHz [44] (22.0 dBm)
			* 5240 MHz [48] (22.0 dBm)
"#;

    /// Tri-band phy info — adds 6 GHz UNII-5. Mirrors what a
    /// WiFi 6E adapter advertises after the regulatory load.
    const TRI_BAND_PHY_INFO: &str = r#"
Wiphy phy2
	Supported interface modes:
		 * managed
	valid interface combinations:
		 * #{ managed } <= 1, total <= 1, #channels <= 1
	Band 1:
		Frequencies:
			* 2412 MHz [1] (20.0 dBm)
	Band 2:
		Frequencies:
			* 5180 MHz [36] (22.0 dBm)
			* 5200 MHz [40] (22.0 dBm) (disabled)
	Band 4:
		Frequencies:
			* 5955 MHz [1] (23.0 dBm)
			* 5975 MHz [5] (23.0 dBm)
"#;

    const IW_DEV_SAMPLE: &str = r#"phy#0
	Interface ap0
		ifindex 8
		wdev 0x2
		type AP
	Interface wlan0
		ifindex 3
		wdev 0x1
		type managed
phy#1
	Interface wlan1
		ifindex 5
		wdev 0x3
		type managed
"#;

    #[test]
    fn pi5_phy_supports_managed_plus_ap() {
        let cap = parse_phy_info("phy0", PI5_PHY_INFO);
        assert!(
            cap.supports_managed_plus_ap,
            "combos: {:?}",
            extract_interface_combinations(PI5_PHY_INFO)
        );
        assert!(cap.interface_modes.iter().any(|m| m == "managed"));
        assert!(cap.interface_modes.iter().any(|m| m == "AP"));
        assert_eq!(cap.max_channels, 2);
    }

    #[test]
    fn sta_only_phy_does_not_support_concurrent_ap() {
        let cap = parse_phy_info("phy1", STA_ONLY_PHY_INFO);
        assert!(!cap.supports_managed_plus_ap);
        assert_eq!(cap.max_channels, 1);
    }

    #[test]
    fn dual_band_phy_reports_2_4_and_5_ghz_support() {
        let cap = parse_phy_info("phy0", DUAL_BAND_PHY_INFO);
        assert!(cap.bands.ghz_2_4);
        assert!(cap.bands.ghz_5);
        assert!(!cap.bands.ghz_6);
        assert_eq!(cap.bands.band_count(), 2);
        assert!(cap.bands.has_fast_band());
    }

    #[test]
    fn tri_band_phy_reports_6_ghz_support_and_ignores_disabled() {
        let cap = parse_phy_info("phy2", TRI_BAND_PHY_INFO);
        assert!(cap.bands.ghz_2_4);
        assert!(cap.bands.ghz_5);
        assert!(cap.bands.ghz_6);
        assert_eq!(cap.bands.band_count(), 3);
    }

    #[test]
    fn sta_only_phy_with_no_frequencies_reports_no_bands() {
        let cap = parse_phy_info("phy1", STA_ONLY_PHY_INFO);
        assert!(!cap.bands.ghz_2_4);
        assert!(!cap.bands.ghz_5);
        assert!(!cap.bands.ghz_6);
        assert_eq!(cap.bands.band_count(), 0);
        assert!(!cap.bands.has_fast_band());
    }

    #[test]
    fn iw_dev_parses_iface_to_phy_map() {
        let map = parse_iface_to_phy_map(IW_DEV_SAMPLE);
        assert_eq!(map.get("wlan0").map(String::as_str), Some("phy0"));
        assert_eq!(map.get("ap0").map(String::as_str), Some("phy0"));
        assert_eq!(map.get("wlan1").map(String::as_str), Some("phy1"));
    }

    #[test]
    fn iw_dev_parses_wifi_devices() {
        let rows = parse_wifi_devices(IW_DEV_SAMPLE);
        assert_eq!(rows.len(), 3);
        let wlan0 = rows.iter().find(|r| r.ifname == "wlan0").expect("wlan0");
        assert_eq!(wlan0.phy, "phy0");
        assert_eq!(wlan0.iftype, "managed");
        let ap0 = rows.iter().find(|r| r.ifname == "ap0").expect("ap0");
        assert_eq!(ap0.iftype, "AP");
        let wlan1 = rows.iter().find(|r| r.ifname == "wlan1").expect("wlan1");
        assert_eq!(wlan1.phy, "phy1");
    }

    #[test]
    fn freq_to_channel_basics() {
        assert_eq!(freq_to_channel(2412), Some(1));
        assert_eq!(freq_to_channel(2437), Some(6));
        assert_eq!(freq_to_channel(2484), Some(14));
        assert_eq!(freq_to_channel(5180), Some(36));
        assert_eq!(freq_to_channel(5955), Some(1));
    }

    #[test]
    fn freq_to_band_basics() {
        assert_eq!(freq_to_band(2412).as_deref(), Some("bg"));
        assert_eq!(freq_to_band(5180).as_deref(), Some("a"));
        assert_eq!(freq_to_band(5955).as_deref(), Some("6GHz"));
    }

    #[test]
    fn parse_sta_link_info_connected_with_freq() {
        let raw = "Connected to aa:bb:cc:dd:ee:ff (on wlan0)\n\
                   \tSSID: HomeNet\n\
                   \tfreq: 5180\n";
        let info = parse_sta_link_info(raw);
        assert!(info.connected);
        assert_eq!(info.freq_mhz, Some(5180));
        assert_eq!(info.channel, Some(36));
        assert_eq!(info.band.as_deref(), Some("a"));
    }

    #[test]
    fn parse_sta_link_info_not_connected() {
        let info = parse_sta_link_info("Not connected.");
        assert!(!info.connected);
        assert!(info.freq_mhz.is_none());
    }
}
