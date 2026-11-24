// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Which peers the renderer talks to.
//!
//! UPnP control carries no authentication, so the renderer only
//! answers peers on the local network. The predicate is the same
//! one the steward applies to its own listeners: RFC 1918 IPv4,
//! loopback, link-local, and IPv6 unique-local. A public-origin
//! peer is refused at accept, with no reply.

use std::net::IpAddr;

/// Whether `addr` is a local-network peer.
pub fn is_lan_origin(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 10
                || (o[0] == 172 && (16..=31).contains(&o[1]))
                || (o[0] == 192 && o[1] == 168)
                || v4.is_loopback()
                || v4.is_link_local()
        }
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_lan_origin(IpAddr::V4(mapped));
            }
            let seg0 = v6.segments()[0];
            v6.is_loopback()
                || (seg0 & 0xffc0) == 0xfe80
                || (seg0 & 0xfe00) == 0xfc00
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn local_ranges_are_lan() {
        for a in [
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "127.0.0.1",
            "169.254.9.9",
            "::1",
            "fe80::1",
            "fd00::5",
            "::ffff:192.168.1.5",
        ] {
            assert!(is_lan_origin(ip(a)), "{a} must be LAN");
        }
    }

    #[test]
    fn public_ranges_are_not_lan() {
        for a in [
            "203.0.113.7",
            "8.8.8.8",
            "172.32.0.1",
            "172.15.0.1",
            "193.168.1.1",
            "2001:db8::1",
            "::ffff:8.8.8.8",
        ] {
            assert!(!is_lan_origin(ip(a)), "{a} must not be LAN");
        }
    }
}
