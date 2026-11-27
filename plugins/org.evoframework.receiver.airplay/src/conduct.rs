// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! One streaming session's dealings with the player.
//!
//! While a stream runs the receiver does three things beside moving audio:
//! it keeps the capture's title current, it passes the sender's volume on,
//! and it watches that the capture is still the player's. The last is how
//! the operator's stop, next or previous on the glass ends the session: the
//! player takes the capture out of its queue, the receiver sees it gone, and
//! it ends the stream. When the session ends, the capture is taken out
//! before the session is reported over.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use evo_airplay::SessionControl;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::player::{Player, PlayerError, Tags};
use crate::volume::{self, Level};

/// How many answers in a row may be "the player is unavailable" before the
/// stream is ended: a player that is gone is a capture nobody plays.
const MISSES: u32 = 5;

/// A running session's work.
pub struct Conduct {
    player: Arc<dyn Player>,
    tags: Arc<Mutex<Tags>>,
    levels: watch::Sender<Option<Level>>,
    muted_by_us: Arc<AtomicBool>,
    watch: JoinHandle<()>,
    volume: JoinHandle<()>,
}

impl Conduct {
    /// Start the session's work. `every` is how often the capture is
    /// checked. The check also restates the latest tags, which the player
    /// writes only when they differ.
    pub fn start(
        player: Arc<dyn Player>,
        control: SessionControl,
        every: Duration,
    ) -> Self {
        let tags = Arc::new(Mutex::new(Tags::default()));
        let (levels, rx) = watch::channel(None);
        let muted_by_us = Arc::new(AtomicBool::new(false));
        let watch = tokio::spawn(watch_capture(
            player.clone(),
            control,
            tags.clone(),
            every,
        ));
        let volume =
            tokio::spawn(volume::run(player.clone(), rx, muted_by_us.clone()));
        Self {
            player,
            tags,
            levels,
            muted_by_us,
            watch,
            volume,
        }
    }

    /// The sender's volume changed.
    pub fn volume(&self, db: f32) {
        let _ = self.levels.send(Some(volume::level_from_db(db)));
    }

    /// The sender's statement about the current track. A capture that is
    /// not there is the watch's to find, not an error here.
    pub async fn tags(&self, tags: Tags) {
        *self.tags.lock().unwrap_or_else(|e| e.into_inner()) = tags.clone();
        if let Err(e) = self.player.set_capture_tags(&tags).await {
            tracing::debug!(error = %e, "the capture's tags were not set");
        }
    }

    /// End the work: take the capture out of the player, and unmute what
    /// the receiver muted. Returns once the player has been asked.
    pub async fn finish(self) {
        self.watch.abort();
        self.volume.abort();
        if self.muted_by_us.swap(false, Ordering::AcqRel) {
            let _ = self.player.set_mute(false).await;
        }
        if let Err(e) = self.player.end_capture().await {
            tracing::warn!(error = %e, "the capture was not taken out of the player");
        }
    }
}

async fn watch_capture(
    player: Arc<dyn Player>,
    control: SessionControl,
    tags: Arc<Mutex<Tags>>,
    every: Duration,
) {
    let mut misses = 0u32;
    loop {
        tokio::time::sleep(every).await;
        let latest = tags.lock().unwrap_or_else(|e| e.into_inner()).clone();
        match player.set_capture_tags(&latest).await {
            Ok(()) => misses = 0,
            Err(PlayerError::Absent) => {
                tracing::info!(
                    "the player ended the capture; ending the stream"
                );
                control.end_session();
                return;
            }
            Err(e) => {
                misses += 1;
                tracing::debug!(error = %e, misses, "the capture check failed");
                if misses >= MISSES {
                    tracing::warn!(
                        "the player is not answering; ending the stream"
                    );
                    control.end_session();
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::fake::{Call, FakePlayer};

    const FAST: Duration = Duration::from_millis(20);

    #[tokio::test]
    async fn a_capture_the_player_took_out_ends_the_stream() {
        let player = FakePlayer::new();
        let control = SessionControl::new();
        let c = Conduct::start(player.clone(), control.clone(), FAST);
        tokio::time::sleep(FAST * 3).await;
        assert!(
            player.count(|c| matches!(c, Call::Tags(_))) >= 1,
            "it checks"
        );
        *player.tags_error.lock().unwrap() = Some(PlayerError::Absent);
        tokio::time::sleep(FAST * 5).await;
        assert!(
            c.watch.is_finished(),
            "the watch ended when the capture went"
        );
        c.finish().await;
    }

    #[tokio::test]
    async fn a_player_that_stops_answering_ends_the_stream_only_after_a_run_of_misses(
    ) {
        let player = FakePlayer::new();
        *player.tags_error.lock().unwrap() =
            Some(PlayerError::Unavailable("away".into()));
        let c = Conduct::start(player.clone(), SessionControl::new(), FAST);
        tokio::time::sleep(FAST * 2).await;
        assert!(!c.watch.is_finished(), "one or two misses are not the end");
        tokio::time::sleep(FAST * 8).await;
        assert!(c.watch.is_finished(), "a run of misses is");
        c.finish().await;
    }

    #[tokio::test]
    async fn a_good_answer_clears_the_misses() {
        let player = FakePlayer::new();
        *player.tags_error.lock().unwrap() =
            Some(PlayerError::Unavailable("away".into()));
        let c = Conduct::start(player.clone(), SessionControl::new(), FAST);
        for _ in 0..4 {
            tokio::time::sleep(FAST * 2).await;
            *player.tags_error.lock().unwrap() = None;
            tokio::time::sleep(FAST * 2).await;
            *player.tags_error.lock().unwrap() =
                Some(PlayerError::Unavailable("away".into()));
        }
        assert!(!c.watch.is_finished());
        c.finish().await;
    }

    #[tokio::test]
    async fn the_latest_tags_are_sent_and_restated() {
        let player = FakePlayer::new();
        let c = Conduct::start(player.clone(), SessionControl::new(), FAST);
        let t = Tags::from_metadata(Some("Song"), Some("Band"), None);
        c.tags(t.clone()).await;
        tokio::time::sleep(FAST * 3).await;
        let sent: Vec<Tags> = player
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Tags(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(sent.first(), Some(&t));
        assert!(
            sent.iter().skip(1).all(|s| *s == t),
            "restated, never reverted"
        );
        c.finish().await;
    }

    #[tokio::test]
    async fn finishing_takes_the_capture_out_and_unmutes_only_what_it_muted() {
        let player = FakePlayer::new();
        let c = Conduct::start(player.clone(), SessionControl::new(), FAST);
        c.volume(-144.0);
        tokio::time::sleep(volume::STEP * 3).await;
        c.finish().await;
        let calls = player.calls();
        let mute_on = calls.iter().position(|c| *c == Call::Mute(true));
        let mute_off = calls.iter().position(|c| *c == Call::Mute(false));
        let end = calls.iter().position(|c| *c == Call::End);
        assert!(mute_on.zip(mute_off).is_some_and(|(a, b)| a < b));
        assert!(mute_off.zip(end).is_some_and(|(a, b)| a < b));

        let player = FakePlayer::new();
        let c = Conduct::start(player.clone(), SessionControl::new(), FAST);
        c.volume(-10.0);
        tokio::time::sleep(volume::STEP * 3).await;
        c.finish().await;
        assert_eq!(
            player.count(|c| matches!(c, Call::Mute(_))),
            0,
            "a session that never muted never unmutes"
        );
        assert_eq!(player.count(|c| *c == Call::End), 1);
    }
}
