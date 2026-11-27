// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! The sender's volume is the device's volume.
//!
//! AirPlay volume is in dB: 0 is full, about -30 is the lowest, -144 is
//! mute. It maps linearly onto the player's 0 to 100, through the player's
//! own `set_volume` and `set_mute`. The receiver keeps no second volume.
//!
//! A slider drag sends many changes. Only the latest is acted on, and at
//! most one change a step, so the player is not flooded.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::player::Player;

/// The dB at and below which a sender means mute.
const MUTE_DB: f32 = -143.5;
/// The lowest dB on the slider.
const FLOOR_DB: f32 = -30.0;

/// How often a volume change is passed on.
pub const STEP: Duration = Duration::from_millis(100);

/// What a sender's volume asks of the player.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Silence the device.
    Mute,
    /// Set the device volume, 0 to 100.
    Volume(u8),
}

/// Map a sender's dB onto the player.
pub fn level_from_db(db: f32) -> Level {
    if db <= MUTE_DB {
        return Level::Mute;
    }
    let share = (db.clamp(FLOOR_DB, 0.0) - FLOOR_DB) / -FLOOR_DB;
    Level::Volume((share * 100.0).round() as u8)
}

/// Pass the latest level to the player until the sender is dropped.
/// `muted_by_us` records that the receiver, not the operator, muted.
pub async fn run(
    player: Arc<dyn Player>,
    mut levels: watch::Receiver<Option<Level>>,
    muted_by_us: Arc<AtomicBool>,
) {
    let mut last = None;
    while levels.changed().await.is_ok() {
        let Some(level) = *levels.borrow_and_update() else {
            continue;
        };
        if Some(level) == last {
            continue;
        }
        let sent = match level {
            Level::Mute => {
                let r = player.set_mute(true).await;
                if r.is_ok() {
                    muted_by_us.store(true, Ordering::Release);
                }
                r
            }
            Level::Volume(v) => {
                let r = player.set_volume(v).await;
                if r.is_ok() && v > 0 {
                    // The player treats a non-zero volume as an unmute.
                    muted_by_us.store(false, Ordering::Release);
                }
                r
            }
        };
        match sent {
            Ok(()) => last = Some(level),
            Err(e) => {
                tracing::debug!(error = %e, "the volume was not passed on")
            }
        }
        tokio::time::sleep(STEP).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::fake::{Call, FakePlayer};

    #[test]
    fn the_scale_runs_from_full_to_the_floor_to_mute() {
        assert_eq!(level_from_db(0.0), Level::Volume(100));
        assert_eq!(level_from_db(-15.0), Level::Volume(50));
        assert_eq!(level_from_db(-30.0), Level::Volume(0));
        assert_eq!(
            level_from_db(-60.0),
            Level::Volume(0),
            "below the floor is silent, not mute"
        );
        assert_eq!(level_from_db(-144.0), Level::Mute);
        assert_eq!(
            level_from_db(3.0),
            Level::Volume(100),
            "above full is full"
        );
    }

    #[test]
    fn a_louder_sender_is_never_a_quieter_device() {
        let mut last = 0;
        let mut db = -30.0;
        while db <= 0.0 {
            let Level::Volume(v) = level_from_db(db) else {
                panic!("{db} is not mute");
            };
            assert!(v >= last, "{db} dB gave {v}, below {last}");
            last = v;
            db += 0.25;
        }
    }

    #[tokio::test]
    async fn only_the_latest_of_a_drag_is_sent_and_mute_is_remembered() {
        let player = FakePlayer::new();
        let (tx, rx) = watch::channel(None);
        let muted = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(run(player.clone(), rx, muted.clone()));
        for v in [10, 20, 30, 40] {
            tx.send(Some(Level::Volume(v))).unwrap();
        }
        tokio::time::sleep(STEP * 3).await;
        assert_eq!(player.calls().last(), Some(&Call::Volume(40)));
        tx.send(Some(Level::Mute)).unwrap();
        tokio::time::sleep(STEP * 3).await;
        assert_eq!(player.calls().last(), Some(&Call::Mute(true)));
        assert!(muted.load(Ordering::Acquire), "the receiver muted it");
        tx.send(Some(Level::Volume(25))).unwrap();
        tokio::time::sleep(STEP * 3).await;
        assert!(
            !muted.load(Ordering::Acquire),
            "a volume above zero is an unmute"
        );
        drop(tx);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn the_same_level_twice_is_sent_once() {
        let player = FakePlayer::new();
        let (tx, rx) = watch::channel(None);
        let task = tokio::spawn(run(player.clone(), rx, Arc::default()));
        tx.send(Some(Level::Volume(60))).unwrap();
        tokio::time::sleep(STEP * 2).await;
        tx.send(Some(Level::Volume(60))).unwrap();
        tokio::time::sleep(STEP * 2).await;
        drop(tx);
        task.await.unwrap();
        assert_eq!(player.count(|c| matches!(c, Call::Volume(_))), 1);
    }
}
