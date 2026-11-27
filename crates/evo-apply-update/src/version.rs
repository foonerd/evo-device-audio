// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Dotted numeric versions. The train's versions have four parts
//! (`0.1.13.1`), which semver does not accept, so they are compared here.

use std::cmp::Ordering;

/// A dotted numeric version, up to five parts.
#[derive(Debug, Clone, Eq)]
pub struct Dotted(Vec<u64>);

impl Dotted {
    /// Parse `N(.N){0,4}` with each part at most six digits. Anything else
    /// is `None`.
    pub fn parse(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.split('.').collect();
        if parts.is_empty() || parts.len() > 5 {
            return None;
        }
        let mut out = Vec::with_capacity(parts.len());
        for p in parts {
            if p.is_empty()
                || p.len() > 6
                || !p.bytes().all(|c| c.is_ascii_digit())
            {
                return None;
            }
            out.push(p.parse().ok()?);
        }
        Some(Self(out))
    }
}

impl PartialEq for Dotted {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl PartialOrd for Dotted {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Dotted {
    /// Missing trailing parts count as zero, so `0.1.13` equals `0.1.13.0`.
    fn cmp(&self, other: &Self) -> Ordering {
        let n = self.0.len().max(other.0.len());
        for i in 0..n {
            let a = self.0.get(i).copied().unwrap_or(0);
            let b = other.0.get(i).copied().unwrap_or(0);
            match a.cmp(&b) {
                Ordering::Equal => {}
                o => return o,
            }
        }
        Ordering::Equal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Dotted {
        Dotted::parse(s).unwrap()
    }

    #[test]
    fn four_part_versions_order() {
        assert!(d("0.1.13.1") > d("0.1.13"));
        assert!(d("0.1.13.10") > d("0.1.13.9"));
        assert!(d("0.2") > d("0.1.99.99"));
        assert_eq!(d("0.1.13"), d("0.1.13.0"));
    }

    #[test]
    fn bad_versions_are_none() {
        for s in ["", "1..2", "1.x", "1.2.3.4.5.6", "1234567", "-1", "1.2 "] {
            assert!(Dotted::parse(s).is_none(), "{s:?}");
        }
    }
}
