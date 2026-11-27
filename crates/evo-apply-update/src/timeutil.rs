// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! UTC time without a time crate: epoch seconds to and from the
//! `YYYY-MM-DDTHH:MM:SSZ` form the documents use.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch now.
pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `YYYY-MM-DDTHH:MM:SSZ` for an epoch second.
pub fn format_utc(epoch: u64) -> String {
    let days = (epoch / 86_400) as i64;
    let rem = epoch % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// `YYYYMMDDTHHMMSSZ`, for directory names.
pub fn compact_utc(epoch: u64) -> String {
    format_utc(epoch).replace(['-', ':'], "")
}

/// Parse exactly `YYYY-MM-DDTHH:MM:SSZ`. Anything else is `None`.
pub fn parse_utc(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() != 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'Z'
    {
        return None;
    }
    let num = |a: usize, z: usize| -> Option<u64> {
        let t = &s[a..z];
        if t.bytes().all(|c| c.is_ascii_digit()) {
            t.parse().ok()
        } else {
            None
        }
    };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, se) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo)
        || !(1..=31).contains(&d)
        || h > 23
        || mi > 59
        || se > 60
    {
        return None;
    }
    let days = days_from_civil(y as i64, mo as u32, d as u32);
    if days < 0 {
        return None;
    }
    Some(days as u64 * 86_400 + h * 3600 + mi * 60 + se)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for e in [0u64, 86_399, 1_795_423_026, 1_900_000_000, 4_102_444_800] {
            assert_eq!(parse_utc(&format_utc(e)), Some(e), "{e}");
        }
    }

    #[test]
    fn known_instant() {
        assert_eq!(format_utc(1_000_000_000), "2001-09-09T01:46:40Z");
        assert_eq!(compact_utc(1_000_000_000), "20010909T014640Z");
    }

    #[test]
    fn bad_shapes_are_none() {
        for s in [
            "",
            "2027-01-28",
            "2027-01-28T20:34:08+00:00",
            "2027-13-28T20:34:08Z",
            "2027-01-28 20:34:08Z",
            "2027-01-28T24:34:08Z",
            "20x7-01-28T20:34:08Z",
        ] {
            assert_eq!(parse_utc(s), None, "{s}");
        }
    }
}
