// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Shuffle by similarity: where the next track is chosen.
//!
//! A weighted walk over the queue. From the track that is playing,
//! the next one is drawn from everything not yet placed, each
//! candidate weighted by how much it shares with the track before
//! it — artist, album, composer, work, performers, label, decade,
//! length. The walk then continues from the track it drew. Nothing
//! is excluded: a track that shares nothing still carries the floor
//! weight and still gets its turn, so this is a shuffle that leans,
//! not a sort.
//!
//! As with album shuffle, the order is decided once and written
//! into the queue; MPD plays the queue in order with `random 0`.
//! The physical order *is* the playback order, so playback follows
//! the choice without this plugin in the path of every song
//! boundary, and the operator can see the order they will hear.
//!
//! The planner is pure: queue items, the playing position and a
//! seed in, a list of moves out. The same inputs always give the
//! same answer, which is what makes the lean assertable.
//!
//! Similarity is read from the tags the queue already carries.
//! Genre is not among them, so it is not a signal here.

use evo_mpd_shared::types::MpdQueueItem;

use crate::album_shuffle::{next_rand, QueueMove};

/// A tag, trimmed and case-folded, or nothing if it is blank.
fn tag(raw: Option<&str>) -> Option<String> {
    let t = raw?.trim();
    if t.is_empty() {
        return None;
    }
    Some(t.to_lowercase())
}

/// The decade a date tag names, from its leading year.
fn decade(raw: Option<&str>) -> Option<u32> {
    let year: u32 = raw?.trim().get(..4)?.parse().ok()?;
    Some(year / 10)
}

/// The features of one track that similarity is read from, each
/// already normalised so the comparison is a plain equality.
struct Features {
    artist: Option<String>,
    album: Option<String>,
    composer: Option<String>,
    work: Option<String>,
    ensemble: Option<String>,
    performer: Option<String>,
    conductor: Option<String>,
    label: Option<String>,
    decade: Option<u32>,
    duration_secs: Option<u64>,
}

impl Features {
    fn of(item: &MpdQueueItem) -> Self {
        let c = &item.classical;
        Self {
            artist: tag(item.artist.as_deref()),
            album: tag(item.album.as_deref()),
            composer: tag(c.composer.as_deref()),
            work: tag(c.work.as_deref()),
            ensemble: tag(c.ensemble.as_deref()),
            performer: tag(c.performer.as_deref()),
            conductor: tag(c.conductor.as_deref()),
            label: tag(c.label.as_deref()),
            decade: decade(c.original_date.as_deref())
                .or_else(|| decade(c.recording_date.as_deref())),
            duration_secs: item.duration.map(|d| d.as_secs()),
        }
    }
}

/// Points for a shared tag. Both sides must carry it: two tracks
/// with no artist tag are not "the same artist".
fn shared<T: PartialEq>(a: &Option<T>, b: &Option<T>, points: u32) -> u32 {
    match (a, b) {
        (Some(x), Some(y)) if x == y => points,
        _ => 0,
    }
}

/// How much two tracks have in common. Zero is "nothing"; the
/// weights say which tags matter more. Artist and album are the
/// strongest signals a popular-music tag set carries; composer and
/// work are the same signals for a classical one, where the
/// artist tag is often the performer and the album a box set.
fn similarity(a: &Features, b: &Features) -> u32 {
    let mut s = 0;
    s += shared(&a.artist, &b.artist, 3);
    s += shared(&a.album, &b.album, 2);
    s += shared(&a.composer, &b.composer, 2);
    s += shared(&a.work, &b.work, 2);
    s += shared(&a.ensemble, &b.ensemble, 1);
    s += shared(&a.performer, &b.performer, 1);
    s += shared(&a.conductor, &b.conductor, 1);
    s += shared(&a.label, &b.label, 1);
    s += shared(&a.decade, &b.decade, 1);
    // Lengths within a quarter of each other: a four-minute song
    // after a four-minute song, not a twenty-minute movement.
    if let (Some(x), Some(y)) = (a.duration_secs, b.duration_secs) {
        let (lo, hi) = if x < y { (x, y) } else { (y, x) };
        if hi > 0 && (hi - lo) * 4 <= hi {
            s += 1;
        }
    }
    s
}

/// The weight a candidate carries in the draw: a floor of one so
/// every track can be drawn, plus everything it shares with the
/// track before it.
fn weight(prev: &Features, candidate: &Features) -> u64 {
    1 + u64::from(similarity(prev, candidate))
}

/// Choose the order and say how to realise it.
///
/// The walk starts from the track at `playing` when that position
/// is in the queue, so what plays next is chosen against what is
/// playing now and the playing track leads the new order. With
/// nothing playing, the start is drawn uniformly.
///
/// From there, each next track is drawn from the ones not yet
/// placed with probability proportional to its weight against the
/// one just placed. The returned moves are applied in sequence with
/// `moveid`, ascending target positions, exactly as album shuffle's
/// are — see [`crate::album_shuffle::album_shuffle_plan`] for why
/// that realises the chosen order.
///
/// An empty plan means there is nothing to choose between (fewer
/// than two tracks) or the queue is already in the chosen order.
pub(crate) fn similar_shuffle_plan(
    items: &[MpdQueueItem],
    playing: Option<u32>,
    seed: u64,
) -> Vec<QueueMove> {
    if items.len() < 2 {
        return Vec::new();
    }
    let features: Vec<Features> = items.iter().map(Features::of).collect();
    let mut state = seed;

    let mut remaining: Vec<usize> = (0..items.len()).collect();
    let start = match playing {
        Some(p) if (p as usize) < items.len() => p as usize,
        _ => (next_rand(&mut state) % items.len() as u64) as usize,
    };
    remaining.retain(|i| *i != start);
    let mut order = vec![start];

    while !remaining.is_empty() {
        let prev = &features[*order.last().expect("order is never empty")];
        let weights: Vec<u64> = remaining
            .iter()
            .map(|i| weight(prev, &features[*i]))
            .collect();
        let total: u64 = weights.iter().sum();
        let mut r = next_rand(&mut state) % total;
        let mut chosen = remaining.len() - 1;
        for (k, w) in weights.iter().enumerate() {
            if r < *w {
                chosen = k;
                break;
            }
            r -= w;
        }
        order.push(remaining.swap_remove(chosen));
    }

    let target: Vec<u32> = order.into_iter().map(|i| items[i].id).collect();
    let current: Vec<u32> = items.iter().map(|i| i.id).collect();
    if current == target {
        return Vec::new();
    }
    target
        .into_iter()
        .enumerate()
        .map(|(pos, id)| QueueMove {
            id,
            to_position: pos as u32,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_mpd_shared::types::ClassicalTags;
    use std::time::Duration;

    fn item(
        id: u32,
        position: u32,
        artist: Option<&str>,
        album: Option<&str>,
        secs: u64,
    ) -> MpdQueueItem {
        MpdQueueItem {
            id,
            position,
            file_path: format!("{id}.flac"),
            title: None,
            artist: artist.map(str::to_string),
            album: album.map(str::to_string),
            duration: Some(Duration::from_secs(secs)),
            classical: ClassicalTags::default(),
        }
    }

    /// Apply a plan the way the command path does and return the
    /// resulting id order.
    fn apply(items: &[MpdQueueItem], plan: &[QueueMove]) -> Vec<u32> {
        let mut ids: Vec<u32> = items.iter().map(|i| i.id).collect();
        for mv in plan {
            let from =
                ids.iter().position(|i| *i == mv.id).expect("id present");
            let id = ids.remove(from);
            ids.insert((mv.to_position as usize).min(ids.len()), id);
        }
        ids
    }

    /// The playing track, one that shares its artist and album,
    /// and one that shares nothing.
    fn one_kin_one_stranger() -> Vec<MpdQueueItem> {
        vec![
            item(1, 0, Some("Low"), Some("Trust"), 240),
            item(2, 1, Some("Low"), Some("Trust"), 250),
            item(3, 2, Some("Autechre"), Some("Confield"), 400),
        ]
    }

    /// The walk leans: the track that shares the most with the
    /// one playing follows it more often than not. The lean is
    /// the whole point of the mode — without it this is track
    /// shuffle under another name.
    #[test]
    fn the_walk_prefers_the_similar_track() {
        let items = one_kin_one_stranger();
        // Kin weighs 1 + 3 + 2 + 1 = 7 against the stranger's 1,
        // so kin follows seven times in eight.
        let mut kin_follows = 0;
        for seed in 0..256u64 {
            let order =
                apply(&items, &similar_shuffle_plan(&items, Some(0), seed));
            assert_eq!(order[0], 1, "seed {seed}: the playing track leads");
            if order[1] == 2 {
                kin_follows += 1;
            }
        }
        assert!(
            kin_follows > 180,
            "the similar track must follow far more often than the \
             stranger; it followed {kin_follows} times in 256"
        );
    }

    /// A track that shares nothing still gets drawn. The floor
    /// weight is what keeps this a shuffle rather than a sort.
    #[test]
    fn a_stranger_still_gets_a_turn() {
        let items = one_kin_one_stranger();
        let mut stranger_follows = 0;
        for seed in 0..256u64 {
            let order =
                apply(&items, &similar_shuffle_plan(&items, Some(0), seed));
            if order[1] == 3 {
                stranger_follows += 1;
            }
        }
        assert!(
            stranger_follows > 0,
            "the stranger must follow at least once in 256, or it \
             can never be played until last"
        );
    }

    /// The playing track leads the new order whatever the seed,
    /// so what the operator hears next is chosen against what
    /// they are hearing now, and the current song is not moved
    /// out from under the player.
    #[test]
    fn the_playing_track_leads_the_order() {
        let items = vec![
            item(1, 0, Some("A"), None, 100),
            item(2, 1, Some("B"), None, 100),
            item(3, 2, Some("C"), None, 100),
            item(4, 3, Some("D"), None, 100),
        ];
        for seed in 0..64u64 {
            let order =
                apply(&items, &similar_shuffle_plan(&items, Some(2), seed));
            assert_eq!(order[0], 3, "seed {seed}: {order:?}");
        }
        // Nothing playing: the start is chosen, and across seeds
        // it is not always the same track.
        let firsts: std::collections::HashSet<u32> = (0..64u64)
            .map(|seed| {
                apply(&items, &similar_shuffle_plan(&items, None, seed))[0]
            })
            .collect();
        assert!(firsts.len() >= 2, "{firsts:?}");
        // A position past the end is nothing playing, not a panic.
        let _ = similar_shuffle_plan(&items, Some(99), 7);
    }

    /// Every track is placed exactly once, whatever the tags and
    /// whatever the seed. A walk that drops or doubles a track is
    /// a queue the operator did not build.
    #[test]
    fn every_track_is_placed_once() {
        let items = vec![
            item(1, 0, Some("A"), Some("x"), 100),
            item(2, 1, Some("A"), Some("x"), 110),
            item(3, 2, Some("A"), Some("y"), 300),
            item(4, 3, None, None, 100),
            item(5, 4, Some(""), Some(" "), 100),
            item(6, 5, Some("B"), Some("z"), 50),
            item(7, 6, Some("b"), Some("Z"), 55),
        ];
        for seed in 0..128u64 {
            let plan = similar_shuffle_plan(&items, Some(1), seed);
            let order = apply(&items, &plan);
            let mut sorted = order.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, (1..=7).collect::<Vec<u32>>(), "seed {seed}");
        }
    }

    /// The same seed chooses the same order. A plan nobody can
    /// reproduce is a plan nobody can check.
    #[test]
    fn the_same_seed_chooses_the_same_order() {
        let items = one_kin_one_stranger();
        for seed in [0u64, 1, 7, 9_999, u64::MAX] {
            assert_eq!(
                similar_shuffle_plan(&items, Some(0), seed),
                similar_shuffle_plan(&items, Some(0), seed),
                "seed {seed}"
            );
        }
    }

    /// One track is no choice, and an empty queue is no choice.
    #[test]
    fn nothing_to_choose_between_is_left_alone() {
        assert!(similar_shuffle_plan(&[], None, 7).is_empty());
        let one = vec![item(1, 0, Some("A"), None, 100)];
        assert!(similar_shuffle_plan(&one, Some(0), 7).is_empty());
    }

    /// Tags are compared as the operator reads them: `Low` and
    /// ` low ` are the same artist, and a blank tag is no tag —
    /// two untagged tracks are not "the same artist".
    #[test]
    fn tags_are_compared_without_case_or_padding() {
        let a = Features::of(&item(1, 0, Some("Low"), Some("Trust"), 240));
        let b = Features::of(&item(2, 1, Some(" low "), Some("TRUST"), 240));
        assert_eq!(similarity(&a, &b), 3 + 2 + 1);

        let blank = Features::of(&item(3, 2, Some(""), Some("  "), 240));
        let none = Features::of(&item(4, 3, None, None, 240));
        assert_eq!(
            similarity(&blank, &none),
            1,
            "only the length matches; blank is not a shared artist"
        );
    }

    /// Classical tags count. On a classical tag set the artist is
    /// often the performer and the album a box, so composer and
    /// work carry the lean there.
    #[test]
    fn classical_tags_carry_the_lean() {
        let mut x = item(1, 0, None, None, 600);
        x.classical = ClassicalTags {
            composer: Some("Shostakovich".into()),
            work: Some("String Quartet No. 8".into()),
            ensemble: Some("Borodin Quartet".into()),
            label: Some("Melodiya".into()),
            original_date: Some("1962-03".into()),
            ..ClassicalTags::default()
        };
        let mut y = item(2, 1, None, None, 580);
        y.classical = ClassicalTags {
            composer: Some("shostakovich".into()),
            work: Some("String Quartet No. 8".into()),
            ensemble: Some("Borodin Quartet".into()),
            label: Some("Melodiya".into()),
            recording_date: Some("1967".into()),
            ..ClassicalTags::default()
        };
        assert_eq!(
            similarity(&Features::of(&x), &Features::of(&y)),
            2 + 2 + 1 + 1 + 1 + 1,
            "composer, work, ensemble, label, decade, length"
        );
    }
}
