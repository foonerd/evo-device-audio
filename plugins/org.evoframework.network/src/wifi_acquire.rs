// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Acquisition of an operator's pre-existing Wi-Fi credentials.
//!
//! A device is frequently flashed with a Wi-Fi network already
//! chosen — an imaging tool writes it before the card ever enters
//! the machine. The device then boots onto that network without
//! this plugin knowing the network exists. Everything the operator
//! sees afterwards is wrong in the same way: the settings page
//! shows no station, moving to a different network has nothing to
//! move from, and a reboot can land on a profile nobody owns.
//!
//! Acquisition closes that gap once, at load, from whichever of two
//! sources is present:
//!
//! 1. A credentials file on the boot partition. Imaging tools write
//!    `wpa_supplicant.conf` there, and the file survives as the
//!    only record of the passphrase.
//! 2. Failing that, a NetworkManager profile this plugin did not
//!    create. A current imaging tool writes one of these instead of
//!    a file.
//!
//! Both paths end the same way: the network is ours, recorded where
//! the rest of the plugin looks for it.
//!
//! # Handling of the passphrase
//!
//! A passphrase read out of a boot-partition file is secret
//! material that was sitting in the clear on a partition any reader
//! of the card can mount. It moves into the plugin's encrypted
//! sidecar and the file is retired, so the window closes rather
//! than staying open for the life of the device.
//!
//! [`WpaNetwork`] deliberately implements [`Debug`] by hand and
//! prints `psk: <redacted>`. Nothing here may reach a log, and a
//! struct that prints its own secret makes that a matter of
//! remembering rather than a matter of construction.

use std::fmt;

/// Where imaging tools leave Wi-Fi credentials, most specific
/// first. The two-entry list is the whole convention: newer images
/// mount the boot partition at `/boot/firmware`, older ones at
/// `/boot`.
pub const DEFAULT_BOOT_WIFI_CONF_PATHS: &[&str] = &[
    "/boot/firmware/wpa_supplicant.conf",
    "/boot/wpa_supplicant.conf",
];

/// One `network={...}` block, reduced to what a station profile
/// needs.
#[derive(Clone, PartialEq, Eq)]
pub struct WpaNetwork {
    /// Network name. Never empty in a value returned by
    /// [`parse_wpa_supplicant_conf`].
    pub ssid: String,
    /// Passphrase, or a 64-character precomputed key. `None` for an
    /// open network, or for a block that declared no key.
    pub psk: Option<String>,
    /// Declared as needing no key.
    pub open: bool,
    /// Declared as not broadcasting its name (`scan_ssid=1`).
    pub hidden: bool,
}

impl fmt::Debug for WpaNetwork {
    /// Prints the shape without the secret. See the module note on
    /// handling of the passphrase.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WpaNetwork")
            .field("ssid", &self.ssid)
            .field(
                "psk",
                &if self.psk.is_some() {
                    "<redacted>"
                } else {
                    "<none>"
                },
            )
            .field("open", &self.open)
            .field("hidden", &self.hidden)
            .finish()
    }
}

/// Read the first usable network out of a `wpa_supplicant.conf`.
///
/// Returns the first `network={...}` block carrying a non-empty
/// name. Blocks without one are skipped rather than failing the
/// parse: a file may legitimately open with a commented template or
/// a block an imaging tool left half-written, and one unusable
/// block is no reason to ignore a good one after it.
///
/// Global directives before the first block (`country`,
/// `ctrl_interface`, `update_config`) are not station settings and
/// are ignored.
///
/// Both spellings of each value are accepted, because both are
/// written in practice: a name and a passphrase may arrive quoted
/// (`ssid="home net"`) or as unquoted hex (`ssid=686f6d65`), and a
/// key may be a passphrase or the 64-character precomputed form.
pub fn parse_wpa_supplicant_conf(raw: &str) -> Option<WpaNetwork> {
    let mut in_block = false;
    let mut cur = BlockAccumulator::default();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !in_block {
            // `network={`, or `network = {` — the brace may also sit
            // on the following line.
            let squashed: String =
                line.chars().filter(|c| !c.is_whitespace()).collect();
            if squashed == "network={" || squashed == "network=" {
                in_block = true;
                cur = BlockAccumulator::default();
            }
            continue;
        }
        if line.starts_with('}') {
            in_block = false;
            // Take the accumulator so a block that yielded nothing
            // leaves a clean one behind for the block after it.
            if let Some(net) = std::mem::take(&mut cur).finish() {
                return Some(net);
            }
            continue;
        }
        if line == "{" {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        cur.set(key.trim(), value.trim());
    }
    // Tolerate a file whose final block is missing its closing
    // brace; the content before it is still what the operator meant.
    if in_block {
        return cur.finish();
    }
    None
}

#[derive(Default)]
struct BlockAccumulator {
    ssid: Option<String>,
    psk: Option<String>,
    key_mgmt_none: bool,
    hidden: bool,
}

impl BlockAccumulator {
    fn set(&mut self, key: &str, value: &str) {
        match key {
            "ssid" => self.ssid = decode_value(value),
            "psk" => self.psk = decode_value(value),
            "key_mgmt" => {
                // `key_mgmt` may list several mechanisms. Open only
                // when NONE is the only one offered.
                let mechanisms: Vec<String> = unquote(value)
                    .split_whitespace()
                    .map(|m| m.to_ascii_uppercase())
                    .collect();
                self.key_mgmt_none = !mechanisms.is_empty()
                    && mechanisms.iter().all(|m| m == "NONE");
            }
            "scan_ssid" => self.hidden = value.trim() == "1",
            _ => {}
        }
    }

    fn finish(self) -> Option<WpaNetwork> {
        let ssid = self.ssid.filter(|s| !s.is_empty())?;
        let psk = self.psk.filter(|p| !p.is_empty());
        // A block with no key is an open network whether or not it
        // said so, and a block that said NONE is open whether or not
        // a stale key is still sitting in it.
        let open = self.key_mgmt_none || psk.is_none();
        Some(WpaNetwork {
            ssid,
            psk: if open { None } else { psk },
            open,
            hidden: self.hidden,
        })
    }
}

/// Decode a value that may be quoted text or unquoted hex.
///
/// A 64-character hex string in a `psk` is the precomputed key and
/// must be passed through as written, not decoded to bytes — so
/// hex decoding is attempted only when the result is valid UTF-8,
/// which the precomputed form is not.
fn decode_value(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if raw.starts_with('"') {
        return Some(unquote(raw));
    }
    if raw.len() >= 2
        && raw.len() % 2 == 0
        && raw.chars().all(|c| c.is_ascii_hexdigit())
    {
        let bytes: Option<Vec<u8>> = (0..raw.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&raw[i..i + 2], 16).ok())
            .collect();
        if let Some(bytes) = bytes {
            if let Ok(text) = String::from_utf8(bytes) {
                if !text.is_empty() && !text.chars().any(|c| c.is_control()) {
                    return Some(text);
                }
            }
        }
    }
    Some(raw.to_string())
}

/// Strip surrounding quotes and resolve backslash escapes.
fn unquote(raw: &str) -> String {
    let raw = raw.trim();
    let inner = raw
        .strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .unwrap_or(raw);
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// A NetworkManager connection profile, reduced to the two fields
/// the choice below needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NmProfileRow {
    /// `connection.id`.
    pub name: String,
    /// `connection.type` — `802-11-wireless` for a Wi-Fi profile.
    pub kind: String,
}

/// Choose a Wi-Fi profile this plugin did not create.
///
/// Skips the plugin's own station and hotspot profiles by name.
/// Everything else that is Wi-Fi is a candidate, and the first is
/// taken: NetworkManager lists profiles most-recently-used first,
/// so on a device flashed with one network the first is the only
/// one, and on a device with several it is the one last connected —
/// which is the one the operator is on.
pub fn pick_foreign_wifi_profile<'a>(
    rows: &'a [NmProfileRow],
    ours: &str,
    hotspot: &str,
) -> Option<&'a NmProfileRow> {
    rows.iter().find(|r| {
        r.kind.trim() == "802-11-wireless"
            && r.name.trim() != ours.trim()
            && r.name.trim() != hotspot.trim()
            && !r.name.trim().is_empty()
    })
}

/// Parse `nmcli -t -f NAME,TYPE connection show` output.
///
/// The terse format escapes a literal `:` inside a field as `\:`,
/// so the split has to respect the escape or a profile named
/// `Cafe: Free` would parse as a row with the wrong type.
pub fn parse_nm_profile_rows(raw: &str) -> Vec<NmProfileRow> {
    let mut out = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let fields = split_terse(line);
        if fields.len() < 2 {
            continue;
        }
        out.push(NmProfileRow {
            name: fields[0].clone(),
            kind: fields[1].clone(),
        });
    }
    out
}

fn split_terse(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
            }
            ':' => fields.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    fields.push(cur);
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape an imaging tool writes: globals, then one block.
    #[test]
    fn parses_the_shape_an_imager_writes() {
        let raw = "country=GB\n\
                   ctrl_interface=DIR=/var/run/wpa_supplicant GROUP=netdev\n\
                   update_config=1\n\
                   \n\
                   network={\n\
                   \tssid=\"Guest (Lobby) Net\"\n\
                   \tpsk=\"correct horse battery\"\n\
                   }\n";
        let net = parse_wpa_supplicant_conf(raw).expect("a network");
        assert_eq!(net.ssid, "Guest (Lobby) Net");
        assert_eq!(net.psk.as_deref(), Some("correct horse battery"));
        assert!(!net.open);
        assert!(!net.hidden);
    }

    /// The secret must not be printable by accident. A `{:?}` on
    /// this type is the most likely way one would ever reach a log.
    #[test]
    fn debug_never_prints_the_passphrase() {
        let net = parse_wpa_supplicant_conf(
            "network={\nssid=\"n\"\npsk=\"hunter2\"\n}\n",
        )
        .expect("a network");
        let shown = format!("{net:?}");
        assert!(
            !shown.contains("hunter2"),
            "Debug leaked the passphrase: {shown}"
        );
        assert!(shown.contains("<redacted>"), "{shown}");
        // And the absence of one is distinguishable from its
        // presence, so a reader can tell an open network apart.
        let open = parse_wpa_supplicant_conf(
            "network={\nssid=\"n\"\nkey_mgmt=NONE\n}\n",
        )
        .expect("a network");
        assert!(format!("{open:?}").contains("<none>"));
    }

    #[test]
    fn open_network_declares_itself_open_and_carries_no_key() {
        let raw = "network={\n\
                   \tssid=\"Airport Free\"\n\
                   \tkey_mgmt=NONE\n\
                   }\n";
        let net = parse_wpa_supplicant_conf(raw).expect("a network");
        assert!(net.open);
        assert!(net.psk.is_none());
    }

    /// `key_mgmt=NONE` alongside a stale key means open. The key is
    /// dropped rather than carried into a profile that would then
    /// refuse to associate.
    #[test]
    fn key_mgmt_none_wins_over_a_leftover_key() {
        let raw = "network={\n\
                   \tssid=\"Open Net\"\n\
                   \tpsk=\"leftover\"\n\
                   \tkey_mgmt=NONE\n\
                   }\n";
        let net = parse_wpa_supplicant_conf(raw).expect("a network");
        assert!(net.open);
        assert!(net.psk.is_none(), "a stale key must not be carried");
    }

    /// A block listing several mechanisms is not open.
    #[test]
    fn several_key_mechanisms_is_not_open() {
        let raw = "network={\n\
                   \tssid=\"Mixed\"\n\
                   \tkey_mgmt=WPA-PSK NONE\n\
                   \tpsk=\"secret\"\n\
                   }\n";
        let net = parse_wpa_supplicant_conf(raw).expect("a network");
        assert!(!net.open);
        assert_eq!(net.psk.as_deref(), Some("secret"));
    }

    /// A name may be written as hex. The precomputed 64-character
    /// key must survive as written — decoding it would produce
    /// rubbish and a profile that cannot associate.
    #[test]
    fn hex_name_decodes_and_a_precomputed_key_does_not() {
        let pmk = "0123456789abcdef0123456789abcdef\
                   0123456789abcdef0123456789abcdef";
        let raw = format!("network={{\n\tssid=486f6d65\n\tpsk={pmk}\n}}\n");
        let net = parse_wpa_supplicant_conf(&raw).expect("a network");
        assert_eq!(net.ssid, "Home");
        assert_eq!(net.psk.as_deref(), Some(pmk));
    }

    #[test]
    fn escapes_and_hidden_and_spacing_variants() {
        let raw = "network = {\n\
                   \tssid=\"Say \\\"Hi\\\"\"\n\
                   \tpsk=\"a b\"\n\
                   \tscan_ssid=1\n\
                   }\n";
        let net = parse_wpa_supplicant_conf(raw).expect("a network");
        assert_eq!(net.ssid, "Say \"Hi\"");
        assert!(net.hidden);
    }

    /// A leading block with no name is skipped, not fatal.
    #[test]
    fn skips_an_unusable_block_and_takes_the_next() {
        let raw = "network={\n\
                   \tkey_mgmt=NONE\n\
                   }\n\
                   network={\n\
                   \tssid=\"Real\"\n\
                   \tpsk=\"k\"\n\
                   }\n";
        let net = parse_wpa_supplicant_conf(raw).expect("a network");
        assert_eq!(net.ssid, "Real");
    }

    /// Two usable blocks: the first wins, as the file's own order
    /// is the only preference expressed.
    #[test]
    fn first_usable_block_wins() {
        let raw = "network={\n\tssid=\"One\"\n\tpsk=\"a\"\n}\n\
                   network={\n\tssid=\"Two\"\n\tpsk=\"b\"\n}\n";
        assert_eq!(
            parse_wpa_supplicant_conf(raw).expect("a network").ssid,
            "One"
        );
    }

    #[test]
    fn comments_and_empty_and_globals_only_yield_nothing() {
        assert!(parse_wpa_supplicant_conf("").is_none());
        assert!(parse_wpa_supplicant_conf("country=GB\n").is_none());
        assert!(parse_wpa_supplicant_conf(
            "# network={\n#\tssid=\"commented\"\n# }\n"
        )
        .is_none());
    }

    #[test]
    fn tolerates_a_missing_closing_brace() {
        let raw = "network={\n\tssid=\"Truncated\"\n\tpsk=\"k\"\n";
        let net = parse_wpa_supplicant_conf(raw).expect("a network");
        assert_eq!(net.ssid, "Truncated");
    }

    #[test]
    fn picks_a_foreign_wifi_profile_and_skips_our_own() {
        let rows = vec![
            NmProfileRow {
                name: "evo-network-wifi-sta".into(),
                kind: "802-11-wireless".into(),
            },
            NmProfileRow {
                name: "evo-network-hotspot".into(),
                kind: "802-11-wireless".into(),
            },
            NmProfileRow {
                name: "Wired connection 1".into(),
                kind: "802-3-ethernet".into(),
            },
            NmProfileRow {
                name: "preconfigured".into(),
                kind: "802-11-wireless".into(),
            },
        ];
        let picked = pick_foreign_wifi_profile(
            &rows,
            "evo-network-wifi-sta",
            "evo-network-hotspot",
        )
        .expect("a foreign profile");
        assert_eq!(picked.name, "preconfigured");
    }

    #[test]
    fn no_foreign_profile_when_only_ours_exist() {
        let rows = vec![
            NmProfileRow {
                name: "evo-network-wifi-sta".into(),
                kind: "802-11-wireless".into(),
            },
            NmProfileRow {
                name: "lo".into(),
                kind: "loopback".into(),
            },
        ];
        assert!(pick_foreign_wifi_profile(
            &rows,
            "evo-network-wifi-sta",
            "evo-network-hotspot"
        )
        .is_none());
    }

    /// A profile name may contain the field separator.
    #[test]
    fn terse_rows_respect_the_escaped_separator() {
        let raw = "Cafe\\: Free:802-11-wireless\n\
                   Wired connection 1:802-3-ethernet\n";
        let rows = parse_nm_profile_rows(raw);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "Cafe: Free");
        assert_eq!(rows[0].kind, "802-11-wireless");
        let picked =
            pick_foreign_wifi_profile(&rows, "evo-network-wifi-sta", "hs")
                .expect("a foreign profile");
        assert_eq!(picked.name, "Cafe: Free");
    }
}
