// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Shuffle by album: where the next album is chosen.
//!
//! MPD's `random` mode shuffles tracks. There is no album mode in
//! the protocol, and asking for one per track — intercepting each
//! transition to pick the next file — would put this plugin in the
//! path of every song boundary and give the operator a queue whose
//! order is a fiction maintained outside MPD.
//!
//! So the album order is decided once, here, and written into the
//! queue. MPD then plays the queue in order with `random 0`, which
//! means the physical order *is* the playback order: nothing has to
//! follow the choice at run time because the choice is the queue.
//!
//! Albums keep their internal order. That is the whole point of
//! the mode — the quiet track on a loud record stays in its place,
//! and a record that was sequenced to be heard in order is.
//!
//! The planner is pure: queue items and a seed in, a list of moves
//! out. It opens no connection and reads no clock, so the choice it
//! makes is inspectable and the same inputs always give the same
//! answer.

use evo_mpd_shared::types::MpdQueueItem;

/// How shuffle picks what plays next.
///
/// Lives in this plugin. The steward carries no shuffle type: the
/// wire still sends `set_shuffle`, and the operator's four
/// answers are spelled out at that boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ShuffleMode {
    /// Play the queue as it stands. MPD `random 0`.
    #[default]
    Off,
    /// Shuffle tracks. MPD `random 1` — the mode `set_shuffle`
    /// has always meant, and what `1` / `true` still mean.
    Tracks,
    /// Shuffle albums, tracks within each album in order. MPD
    /// `random 0` over a queue this module has reordered.
    Albums,
    /// A weighted walk, each track drawn against the one before
    /// it. MPD `random 0` over a queue
    /// [`crate::similar_shuffle`] has reordered.
    Similar,
}

impl ShuffleMode {
    /// Parse the operator's answer.
    ///
    /// `0` / `1` and `false` / `true` are the fielded spelling and
    /// keep their meaning exactly: `1` is track shuffle, not the
    /// new mode. A glass that has never heard of albums goes on
    /// working unchanged.
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "0" | "false" | "off" => Some(Self::Off),
            "1" | "true" | "tracks" | "track" => Some(Self::Tracks),
            "albums" | "album" => Some(Self::Albums),
            "similar" => Some(Self::Similar),
            _ => None,
        }
    }

    /// The token the now-playing report carries.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Tracks => "tracks",
            Self::Albums => "albums",
            Self::Similar => "similar",
        }
    }

    /// What MPD's `random` must be for this mode. Album and
    /// similar order are carried by the queue, so both want
    /// random off — a random walk over a queue this plugin has
    /// ordered is just a random walk.
    pub(crate) fn mpd_random(self) -> bool {
        matches!(self, Self::Tracks)
    }

    /// Whether this mode's order is written into the queue — the
    /// modes MPD has no primitive for.
    pub(crate) fn orders_the_queue(self) -> bool {
        matches!(self, Self::Albums | Self::Similar)
    }
}

/// Shared record of the mode the operator last asked for.
///
/// MPD has no album mode, so its `status` cannot report one: album
/// order lives in the queue and `random` is off for it. Every
/// now-playing publisher would otherwise have to guess, and the
/// only guess available from `status` is "not shuffling" — which is
/// how the control lost album across a refresh.
///
/// Only `set_shuffle` writes this. Mirrors `MuteCell`, which exists
/// for the same reason: operator intent MPD does not carry.
#[derive(Debug, Clone)]
pub(crate) struct ShuffleModeCell {
    mode: std::sync::Arc<std::sync::atomic::AtomicU8>,
}

impl ShuffleModeCell {
    pub(crate) fn new() -> Self {
        Self {
            mode: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        }
    }

    /// The mode the operator last asked for.
    pub(crate) fn get(&self) -> ShuffleMode {
        match self.mode.load(std::sync::atomic::Ordering::SeqCst) {
            1 => ShuffleMode::Tracks,
            2 => ShuffleMode::Albums,
            3 => ShuffleMode::Similar,
            _ => ShuffleMode::Off,
        }
    }

    /// Record it. Only `set_shuffle` calls this.
    pub(crate) fn set(&self, mode: ShuffleMode) {
        let v = match mode {
            ShuffleMode::Off => 0u8,
            ShuffleMode::Tracks => 1,
            ShuffleMode::Albums => 2,
            ShuffleMode::Similar => 3,
        };
        self.mode.store(v, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Default for ShuffleModeCell {
    fn default() -> Self {
        Self::new()
    }
}

/// What the report should say, given MPD's own flag and the mode
/// the operator asked for.
///
/// MPD's `random` is the authority for track shuffle: another
/// client can set it, and the report must not contradict the flag
/// it publishes beside this. So `random` on is always `tracks` —
/// `albums` can never be reported while that flag is true, which
/// would be a report disagreeing with itself.
///
/// With `random` off, the remembered intent decides: `albums` or
/// `similar` if that is what was asked for, otherwise `off`. A
/// remembered `tracks` with the flag off means something turned
/// MPD's random off underneath us, and the flag wins.
pub(crate) fn reported_shuffle_mode(
    mpd_random: bool,
    remembered: ShuffleMode,
) -> ShuffleMode {
    if mpd_random {
        return ShuffleMode::Tracks;
    }
    match remembered {
        ShuffleMode::Albums => ShuffleMode::Albums,
        ShuffleMode::Similar => ShuffleMode::Similar,
        _ => ShuffleMode::Off,
    }
}

/// One `moveid`: take this song id, put it at this position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QueueMove {
    /// MPD song id. Stable across the moves that precede it,
    /// which is why the plan addresses ids and not positions.
    pub(crate) id: u32,
    /// Target position, applied in ascending order.
    pub(crate) to_position: u32,
}

/// The album a queue item belongs to.
///
/// An item with no album tag is its own album, keyed by its file
/// path: a loose single does not get swept into a group with every
/// other untagged file, and it still gets a turn.
fn album_key(item: &MpdQueueItem) -> String {
    match item.album.as_deref().map(str::trim) {
        Some(a) if !a.is_empty() => format!("a:{a}"),
        _ => format!("f:{}", item.file_path),
    }
}

/// Deterministic shuffle. SplitMix64 — small, well-distributed,
/// and reproducible from the seed, so a plan can be asserted.
/// Shared with [`crate::similar_shuffle`] so both modes draw from
/// the same source and a seed means one thing.
pub(crate) fn next_rand(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Choose the album order and say how to realise it.
///
/// Albums are grouped by tag, in first-appearance order, then that
/// list of groups is shuffled. Each group's tracks keep the order
/// they already had.
///
/// The returned moves are applied in sequence with `moveid`.
/// Ascending target positions mean each move lands its song at a
/// position no later move disturbs, so the queue ends in exactly
/// the planned order — which is why this addresses ids rather
/// than positions, those shifting under every move.
///
/// An empty plan means the queue is already in the chosen order.
/// Fewer than two albums is always an empty plan: there is no
/// album order to choose.
pub(crate) fn album_shuffle_plan(
    items: &[MpdQueueItem],
    seed: u64,
) -> Vec<QueueMove> {
    // Group, keeping first-appearance order and each album's own
    // track order.
    let mut groups: Vec<(String, Vec<u32>)> = Vec::new();
    for item in items {
        let key = album_key(item);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, ids)) => ids.push(item.id),
            None => groups.push((key, vec![item.id])),
        }
    }
    if groups.len() < 2 {
        return Vec::new();
    }

    // Fisher-Yates over the album groups. Only the order of
    // albums moves; nothing inside one does.
    let mut state = seed;
    for i in (1..groups.len()).rev() {
        let j = (next_rand(&mut state) % (i as u64 + 1)) as usize;
        groups.swap(i, j);
    }

    let target: Vec<u32> =
        groups.into_iter().flat_map(|(_, ids)| ids).collect();

    // Already in that order: say nothing rather than issue moves
    // that change nothing.
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
    use std::time::Duration;

    fn item(
        id: u32,
        position: u32,
        album: Option<&str>,
        file: &str,
    ) -> MpdQueueItem {
        MpdQueueItem {
            id,
            position,
            file_path: file.to_string(),
            title: None,
            artist: None,
            album: album.map(str::to_string),
            duration: Some(Duration::from_secs(1)),
            classical: Default::default(),
        }
    }

    /// Two records, three tracks each, interleaved in the queue.
    fn two_albums() -> Vec<MpdQueueItem> {
        vec![
            item(10, 0, Some("Blue"), "blue/1.flac"),
            item(11, 1, Some("Blue"), "blue/2.flac"),
            item(12, 2, Some("Blue"), "blue/3.flac"),
            item(20, 3, Some("Red"), "red/1.flac"),
            item(21, 4, Some("Red"), "red/2.flac"),
            item(22, 5, Some("Red"), "red/3.flac"),
        ]
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

    /// The operator asked for albums, so albums move and tracks
    /// do not. A record sequenced to be heard in order is.
    #[test]
    fn albums_are_reordered_and_their_tracks_are_not() {
        let items = two_albums();
        // Walk seeds so this is not one lucky permutation.
        let mut saw_swap = false;
        for seed in 0..64u64 {
            let order = apply(&items, &album_shuffle_plan(&items, seed));
            assert_eq!(
                order.len(),
                6,
                "every track keeps its place in the queue"
            );
            let blue: Vec<u32> =
                order.iter().copied().filter(|i| *i < 20).collect();
            let red: Vec<u32> =
                order.iter().copied().filter(|i| *i >= 20).collect();
            assert_eq!(
                blue,
                vec![10, 11, 12],
                "seed {seed}: Blue stays in order"
            );
            assert_eq!(
                red,
                vec![20, 21, 22],
                "seed {seed}: Red stays in order"
            );
            // Each album is contiguous: an album is played, then
            // the next one, not shuffled through each other.
            let first_red = order.iter().position(|i| *i >= 20).unwrap();
            let last_red = order.iter().rposition(|i| *i >= 20).unwrap();
            assert_eq!(
                last_red - first_red,
                2,
                "seed {seed}: Red is contiguous: {order:?}"
            );
            if order[0] >= 20 {
                saw_swap = true;
            }
        }
        assert!(
            saw_swap,
            "across 64 seeds the second album must sometimes come first, \
             or nothing is being chosen"
        );
    }

    /// The same seed chooses the same order. A plan nobody can
    /// reproduce is a plan nobody can check.
    #[test]
    fn the_same_seed_chooses_the_same_album_order() {
        let items = two_albums();
        for seed in [0u64, 1, 7, 9_999, u64::MAX] {
            assert_eq!(
                album_shuffle_plan(&items, seed),
                album_shuffle_plan(&items, seed),
                "seed {seed}"
            );
        }
    }

    /// One album is no choice, and an empty queue is no choice.
    /// Neither may emit a move: a queue nobody asked to reorder
    /// must come back untouched.
    #[test]
    fn nothing_to_choose_between_is_left_alone() {
        assert!(album_shuffle_plan(&[], 7).is_empty(), "empty queue");
        let one = vec![
            item(1, 0, Some("Blue"), "blue/1.flac"),
            item(2, 1, Some("Blue"), "blue/2.flac"),
        ];
        assert!(
            album_shuffle_plan(&one, 7).is_empty(),
            "a single album has no album order to choose"
        );
    }

    /// An untagged file is its own album. It gets a turn, and it
    /// does not drag every other untagged file along with it.
    #[test]
    fn an_untagged_file_is_its_own_album() {
        let items = vec![
            item(1, 0, Some("Blue"), "blue/1.flac"),
            item(2, 1, None, "loose/a.flac"),
            item(3, 2, None, "loose/b.flac"),
        ];
        let mut first_positions = std::collections::HashSet::new();
        for seed in 0..64u64 {
            let order = apply(&items, &album_shuffle_plan(&items, seed));
            assert_eq!(order.len(), 3);
            first_positions.insert(order[0]);
        }
        assert!(
            first_positions.len() >= 2,
            "three albums, so more than one of them must get to be \
             first across 64 seeds: {first_positions:?}"
        );
    }

    /// The plan realises exactly the order it chose. Positions
    /// shift under every move, which is why it addresses ids.
    #[test]
    fn the_plan_realises_the_order_it_chose() {
        // Five albums of uneven length: the arithmetic that only
        // works for equal groups fails here.
        let items = vec![
            item(1, 0, Some("A"), "a/1.flac"),
            item(2, 1, Some("A"), "a/2.flac"),
            item(3, 2, Some("B"), "b/1.flac"),
            item(4, 3, Some("C"), "c/1.flac"),
            item(5, 4, Some("C"), "c/2.flac"),
            item(6, 5, Some("C"), "c/3.flac"),
            item(7, 6, Some("D"), "d/1.flac"),
            item(8, 7, Some("E"), "e/1.flac"),
            item(9, 8, Some("E"), "e/2.flac"),
        ];
        for seed in 0..64u64 {
            let plan = album_shuffle_plan(&items, seed);
            let order = apply(&items, &plan);
            assert_eq!(
                order.len(),
                items.len(),
                "seed {seed}: no track lost or duplicated"
            );
            let mut sorted = order.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, (1..=9).collect::<Vec<u32>>(), "seed {seed}");
            // Every album contiguous.
            for album in ["a/", "b/", "c/", "d/", "e/"] {
                let ids: Vec<u32> = items
                    .iter()
                    .filter(|i| i.file_path.starts_with(album))
                    .map(|i| i.id)
                    .collect();
                let first = order.iter().position(|i| ids.contains(i)).unwrap();
                let last = order.iter().rposition(|i| ids.contains(i)).unwrap();
                assert_eq!(
                    last - first + 1,
                    ids.len(),
                    "seed {seed}: {album} must be contiguous in {order:?}"
                );
            }
        }
    }

    /// The report must carry the mode, and must never claim
    /// MPD's flag is on under album shuffle.
    ///
    /// Album order lives in the queue with `random` off, so a
    /// report that read only `shuffle` said "not shuffling" and
    /// the control lost album across a refresh. These are the two
    /// ways that can go wrong: the mode omitted, or the flag
    /// turned true to carry it.
    #[test]
    fn the_report_carries_album_mode_without_touching_the_flag() {
        // Album shuffle in force: random is off, mode is albums.
        assert_eq!(
            reported_shuffle_mode(false, ShuffleMode::Albums),
            ShuffleMode::Albums,
            "album shuffle in force must report albums"
        );
        assert_eq!(
            reported_shuffle_mode(false, ShuffleMode::Albums).as_str(),
            "albums",
            "and carry it as a token the glass can read"
        );

        // The flag is MPD's and album mode leaves it off. A
        // report claiming random on under album shuffle would be
        // disagreeing with the flag it publishes beside it.
        for remembered in
            [ShuffleMode::Off, ShuffleMode::Tracks, ShuffleMode::Albums]
        {
            if remembered == ShuffleMode::Albums {
                assert!(
                    !remembered.mpd_random(),
                    "album mode must never ask for random on"
                );
            }
            // Whatever was remembered, random on reports tracks
            // and never albums: another client can set that flag.
            assert_eq!(
                reported_shuffle_mode(true, remembered),
                ShuffleMode::Tracks,
                "random on is always tracks, never albums ({remembered:?})"
            );
        }

        // Off stays off, and a remembered tracks with the flag
        // off means something turned MPD's random off under us.
        assert_eq!(
            reported_shuffle_mode(false, ShuffleMode::Off),
            ShuffleMode::Off
        );
        assert_eq!(
            reported_shuffle_mode(false, ShuffleMode::Tracks),
            ShuffleMode::Off,
            "the flag wins when it disagrees with the memory"
        );
    }

    /// Every mode has a token, and they are the four the glass
    /// reads.
    #[test]
    fn every_mode_has_a_wire_token() {
        assert_eq!(ShuffleMode::Off.as_str(), "off");
        assert_eq!(ShuffleMode::Tracks.as_str(), "tracks");
        assert_eq!(ShuffleMode::Albums.as_str(), "albums");
        assert_eq!(ShuffleMode::Similar.as_str(), "similar");
        // Round-trips, so the glass can send back what it read.
        for m in [
            ShuffleMode::Off,
            ShuffleMode::Tracks,
            ShuffleMode::Albums,
            ShuffleMode::Similar,
        ] {
            assert_eq!(ShuffleMode::parse(m.as_str()), Some(m));
        }
    }

    /// The cell remembers what the operator asked for. Only
    /// `set_shuffle` writes it; every publisher reads it.
    #[test]
    fn the_cell_remembers_the_mode() {
        let cell = ShuffleModeCell::new();
        assert_eq!(cell.get(), ShuffleMode::Off, "a fresh session is off");
        for m in [
            ShuffleMode::Albums,
            ShuffleMode::Similar,
            ShuffleMode::Tracks,
            ShuffleMode::Off,
        ] {
            cell.set(m);
            assert_eq!(cell.get(), m);
            assert_eq!(cell.clone().get(), m, "shared, not copied");
        }
    }

    /// `1` and `true` still mean track shuffle. The fielded glass
    /// sends those and must not be moved onto a new mode.
    #[test]
    fn the_fielded_spelling_still_means_track_shuffle() {
        for raw in ["1", "true"] {
            assert_eq!(ShuffleMode::parse(raw), Some(ShuffleMode::Tracks));
        }
        for raw in ["0", "false", "off"] {
            assert_eq!(ShuffleMode::parse(raw), Some(ShuffleMode::Off));
        }
        for raw in ["albums", "album"] {
            assert_eq!(ShuffleMode::parse(raw), Some(ShuffleMode::Albums));
        }
        assert_eq!(ShuffleMode::parse("similar"), Some(ShuffleMode::Similar));
        for raw in ["", "yes", "Albums", "random", "smart"] {
            assert_eq!(ShuffleMode::parse(raw), None, "{raw:?}");
        }
    }

    /// Album order is the queue's, so MPD's own random stays off
    /// for it. A random walk over an album-ordered queue is just a
    /// random walk.
    #[test]
    fn album_mode_leaves_mpd_random_off() {
        assert!(!ShuffleMode::Off.mpd_random());
        assert!(ShuffleMode::Tracks.mpd_random());
        assert!(!ShuffleMode::Albums.mpd_random());
    }

    /// Similar order is the queue's too: random off, and the
    /// order written. The modes MPD has no primitive for are
    /// exactly the ones that order the queue.
    #[test]
    fn similar_mode_is_written_into_the_queue_with_random_off() {
        assert!(!ShuffleMode::Similar.mpd_random());
        assert!(ShuffleMode::Similar.orders_the_queue());
        assert!(ShuffleMode::Albums.orders_the_queue());
        assert!(!ShuffleMode::Tracks.orders_the_queue());
        assert!(!ShuffleMode::Off.orders_the_queue());
        assert_eq!(
            reported_shuffle_mode(false, ShuffleMode::Similar),
            ShuffleMode::Similar,
            "similar in force must be reported as similar"
        );
        assert_eq!(
            reported_shuffle_mode(true, ShuffleMode::Similar),
            ShuffleMode::Tracks,
            "random on is always tracks, whatever was remembered"
        );
    }
}
