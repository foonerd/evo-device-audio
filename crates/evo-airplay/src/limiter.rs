// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! A limit on password guesses, per peer address.
//!
//! Pairing with a password is an online guessing game: every connection is a
//! fresh attempt. After a few failures a peer is told to back off, and each
//! further run of failures doubles the wait, up to an hour. A success clears
//! the record. The table is bounded, so an attacker cannot grow it.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Records kept before the oldest are dropped.
const MAX_TRACKED: usize = 1024;
/// The longest a peer is told to wait.
const MAX_LOCKOUT: Duration = Duration::from_secs(3600);
/// How long an idle record is remembered.
const FORGET_AFTER: Duration = Duration::from_secs(3600);

struct Record {
    failures: u32,
    strikes: u32,
    locked_until: Option<Instant>,
    last_seen: Instant,
}

/// Failed-pairing bookkeeping shared by every connection of a receiver.
pub struct PairingLimiter {
    max_failures: u32,
    base_lockout: Duration,
    records: Mutex<HashMap<IpAddr, Record>>,
}

impl Default for PairingLimiter {
    /// Five failures, then a minute, doubling.
    fn default() -> PairingLimiter {
        PairingLimiter::new(5, Duration::from_secs(60))
    }
}

impl PairingLimiter {
    /// A limiter that locks a peer out after `max_failures` consecutive
    /// failures, first for `base_lockout`, then for twice as long each time.
    pub fn new(max_failures: u32, base_lockout: Duration) -> PairingLimiter {
        PairingLimiter {
            max_failures: max_failures.max(1),
            base_lockout,
            records: Mutex::new(HashMap::new()),
        }
    }

    /// How much longer this peer must wait before it may try again, if it is
    /// locked out.
    pub fn locked_for(&self, peer: IpAddr) -> Option<Duration> {
        self.locked_for_at(peer, Instant::now())
    }

    /// Note a failed pairing attempt by this peer.
    pub fn record_failure(&self, peer: IpAddr) {
        self.record_failure_at(peer, Instant::now());
    }

    /// Note a successful pairing: the peer's record is cleared.
    pub fn record_success(&self, peer: IpAddr) {
        self.table().remove(&peer);
    }

    fn table(&self) -> std::sync::MutexGuard<'_, HashMap<IpAddr, Record>> {
        self.records.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn locked_for_at(&self, peer: IpAddr, now: Instant) -> Option<Duration> {
        let table = self.table();
        let until = table.get(&peer)?.locked_until?;
        until.checked_duration_since(now).filter(|d| !d.is_zero())
    }

    fn record_failure_at(&self, peer: IpAddr, now: Instant) {
        let mut table = self.table();
        if table.len() >= MAX_TRACKED && !table.contains_key(&peer) {
            table.retain(|_, r| {
                let quiet_since =
                    r.locked_until.map_or(r.last_seen, |u| u.max(r.last_seen));
                now.saturating_duration_since(quiet_since) < FORGET_AFTER
            });
            if table.len() >= MAX_TRACKED {
                if let Some(oldest) = table
                    .iter()
                    .min_by_key(|(_, r)| r.last_seen)
                    .map(|(ip, _)| *ip)
                {
                    table.remove(&oldest);
                }
            }
        }
        let record = table.entry(peer).or_insert(Record {
            failures: 0,
            strikes: 0,
            locked_until: None,
            last_seen: now,
        });
        // Escalation is forgotten only after a full quiet hour *beyond* any
        // lockout, so the longest lockout cannot expire into a clean slate.
        let quiet_since = record
            .locked_until
            .map_or(record.last_seen, |until| until.max(record.last_seen));
        if now.saturating_duration_since(quiet_since) >= FORGET_AFTER {
            record.failures = 0;
            record.strikes = 0;
        }
        record.last_seen = now;
        record.failures += 1;
        if record.failures >= self.max_failures {
            let factor = 1u32.checked_shl(record.strikes).unwrap_or(u32::MAX);
            let wait = self
                .base_lockout
                .checked_mul(factor)
                .unwrap_or(MAX_LOCKOUT)
                .min(MAX_LOCKOUT);
            record.locked_until = Some(now + wait);
            record.failures = 0;
            record.strikes = record.strikes.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(last: u8) -> IpAddr {
        IpAddr::from([192, 168, 1, last])
    }

    #[test]
    fn a_peer_is_locked_out_after_its_failures_and_freed_after_the_wait() {
        let limiter = PairingLimiter::new(3, Duration::from_secs(10));
        let t0 = Instant::now();
        for _ in 0..2 {
            limiter.record_failure_at(peer(1), t0);
            assert!(limiter.locked_for_at(peer(1), t0).is_none());
        }
        limiter.record_failure_at(peer(1), t0);
        let wait = limiter.locked_for_at(peer(1), t0).expect("locked");
        assert_eq!(wait, Duration::from_secs(10));
        assert!(limiter
            .locked_for_at(peer(1), t0 + Duration::from_secs(11))
            .is_none());
    }

    #[test]
    fn one_peer_does_not_lock_out_another() {
        let limiter = PairingLimiter::new(1, Duration::from_secs(10));
        let t0 = Instant::now();
        limiter.record_failure_at(peer(1), t0);
        assert!(limiter.locked_for_at(peer(1), t0).is_some());
        assert!(limiter.locked_for_at(peer(2), t0).is_none());
    }

    #[test]
    fn each_lockout_is_twice_the_last_up_to_an_hour() {
        let limiter = PairingLimiter::new(1, Duration::from_secs(60));
        let mut now = Instant::now();
        let mut waits = Vec::new();
        for _ in 0..8 {
            limiter.record_failure_at(peer(1), now);
            let wait = limiter.locked_for_at(peer(1), now).expect("locked");
            waits.push(wait.as_secs());
            now += wait + Duration::from_secs(1);
        }
        assert_eq!(waits, vec![60, 120, 240, 480, 960, 1920, 3600, 3600]);
    }

    #[test]
    fn a_quiet_hour_after_the_lockout_forgets_the_escalation() {
        let limiter = PairingLimiter::new(1, Duration::from_secs(60));
        let t0 = Instant::now();
        limiter.record_failure_at(peer(1), t0);
        limiter.record_failure_at(peer(1), t0 + Duration::from_secs(61));
        let second = limiter
            .locked_for_at(peer(1), t0 + Duration::from_secs(61))
            .expect("locked");
        assert_eq!(second, Duration::from_secs(120));
        let much_later = t0 + Duration::from_secs(61 + 120 + 3600);
        limiter.record_failure_at(peer(1), much_later);
        assert_eq!(
            limiter.locked_for_at(peer(1), much_later),
            Some(Duration::from_secs(60)),
            "a quiet hour clears the strikes"
        );
    }

    #[test]
    fn a_success_clears_the_record() {
        let limiter = PairingLimiter::new(2, Duration::from_secs(10));
        let t0 = Instant::now();
        limiter.record_failure_at(peer(1), t0);
        limiter.record_success(peer(1));
        limiter.record_failure_at(peer(1), t0);
        assert!(
            limiter.locked_for_at(peer(1), t0).is_none(),
            "the earlier failure was forgotten"
        );
    }

    #[test]
    fn the_table_stays_bounded() {
        let limiter = PairingLimiter::new(5, Duration::from_secs(10));
        let t0 = Instant::now();
        for n in 0..(MAX_TRACKED + 300) {
            let ip = IpAddr::from([10, 0, (n / 256) as u8, (n % 256) as u8]);
            limiter.record_failure_at(ip, t0 + Duration::from_millis(n as u64));
        }
        assert!(limiter.table().len() <= MAX_TRACKED);
    }
}
