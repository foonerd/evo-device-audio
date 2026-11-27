// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Who may pair, and what may be authorised.
//!
//! The pairing window is one flag. While it is closed, which is nearly always,
//! the pairing agent refuses every request it is asked about, whatever the
//! adapter's own flags say. (The adapter's *pairable* flag is the first lock:
//! the host stack refuses to pair at all while it is clear. The agent is the
//! second, for the prompts and service authorisations that do reach it.)

use std::sync::atomic::{AtomicBool, Ordering};

/// The pairing window as the agent sees it. Cheap to read from a callback.
#[derive(Debug, Default)]
pub struct PairingGate {
    open: AtomicBool,
}

impl PairingGate {
    /// A closed gate.
    pub fn new() -> Self {
        Self::default()
    }

    /// Open the window to pairing requests.
    pub fn open(&self) {
        self.open.store(true, Ordering::SeqCst);
    }

    /// Close it: every request is refused from the next call on.
    pub fn close(&self) {
        self.open.store(false, Ordering::SeqCst);
    }

    /// Whether pairing requests are being accepted.
    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::SeqCst)
    }
}

/// The audio profiles a sink may authorise: A2DP (source, sink and the
/// umbrella profile) and the audio/video remote control profiles that travel
/// with it. Anything else a stranger asks for is refused: the device is a
/// speaker, not a keyboard, a modem or a file server.
pub const AUDIO_SERVICES: [&str; 6] = [
    "0000110a-0000-1000-8000-00805f9b34fb",
    "0000110b-0000-1000-8000-00805f9b34fb",
    "0000110c-0000-1000-8000-00805f9b34fb",
    "0000110d-0000-1000-8000-00805f9b34fb",
    "0000110e-0000-1000-8000-00805f9b34fb",
    "0000110f-0000-1000-8000-00805f9b34fb",
];

/// Whether `uuid` names one of the audio profiles.
pub fn is_audio_service(uuid: &str) -> bool {
    AUDIO_SERVICES.iter().any(|s| s.eq_ignore_ascii_case(uuid))
}

/// A request to pair, or to confirm a pairing, is accepted only while the
/// window is open.
pub fn allow_pairing(gate_open: bool) -> bool {
    gate_open
}

/// A request to use a service is accepted when it is an audio profile and the
/// device is one the operator paired (or is pairing now).
pub fn allow_service(
    gate_open: bool,
    device_is_paired: bool,
    uuid: &str,
) -> bool {
    is_audio_service(uuid) && (gate_open || device_is_paired)
}

/// Whether `text` is a Bluetooth device address: six hex pairs, colon
/// separated.
pub fn is_address(text: &str) -> bool {
    let parts: Vec<&str> = text.split(':').collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gate_starts_closed_and_follows_open_and_close() {
        let g = PairingGate::new();
        assert!(!g.is_open());
        g.open();
        assert!(g.is_open());
        g.close();
        assert!(!g.is_open());
    }

    #[test]
    fn pairing_is_accepted_only_inside_the_window() {
        assert!(allow_pairing(true));
        assert!(!allow_pairing(false));
    }

    #[test]
    fn only_audio_services_are_authorised() {
        const A2DP: &str = "0000110d-0000-1000-8000-00805f9b34fb";
        const HID: &str = "00001124-0000-1000-8000-00805f9b34fb";
        const OBEX: &str = "00001105-0000-1000-8000-00805f9b34fb";
        // a paired device, window closed: audio yes, the rest no
        assert!(allow_service(false, true, A2DP));
        assert!(!allow_service(false, true, HID));
        assert!(!allow_service(false, true, OBEX));
        // a stranger, window closed: nothing
        assert!(!allow_service(false, false, A2DP));
        // a stranger inside the window: audio only
        assert!(allow_service(true, false, A2DP));
        assert!(!allow_service(true, false, HID));
        // case does not matter
        assert!(allow_service(false, true, &A2DP.to_uppercase()));
    }

    #[test]
    fn addresses_are_six_hex_pairs() {
        assert!(is_address("E0:8F:4C:9E:A7:72"));
        assert!(is_address("e0:8f:4c:9e:a7:72"));
        for bad in [
            "",
            "E0:8F:4C:9E:A7",
            "E0:8F:4C:9E:A7:72:00",
            "E08F4C9EA772",
            "E0-8F-4C-9E-A7-72",
            "G0:8F:4C:9E:A7:72",
            "E0:8F:4C:9E:A7:7",
            "../../etc/passwd",
        ] {
            assert!(!is_address(bad), "{bad:?}");
        }
    }
}
