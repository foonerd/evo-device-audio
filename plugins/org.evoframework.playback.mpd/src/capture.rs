// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Live captures: sound another plugin writes into a reserved local
//! capture device, played by the player as an ordinary queue item.
//!
//! A caller names a capture from a **closed table**. The table, not
//! the caller, holds the device: a request can never carry a device,
//! a URI or a path, so no arbitrary `alsa://` input is reachable
//! through these verbs.
//!
//! A capture lives in the queue only while it is the playing (or
//! paused) item. The moment it is not — the operator pressed stop,
//! chose another song, cleared the queue — it is taken out again, so
//! a capture can never be left in the queue playing silence. The
//! queue's other items and their order are never touched.

use std::path::PathBuf;

use crate::mpd::{MpdConnection, MpdError, MpdQueueItem, PlayState};

/// Longest title, artist or album, in characters.
pub(crate) const MAX_TAG_CHARS: usize = 256;

/// One entry of the closed capture table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CaptureSource {
    /// The name a caller uses.
    pub(crate) name: &'static str,
    /// What the player plays for it.
    pub(crate) uri: &'static str,
    /// Where the kernel says whether the write end is open, relative
    /// to `/proc/asound`.
    writer_status: &'static str,
}

/// The AirPlay receiver's capture: the read end of the reserved
/// Loopback substream whose write end the receiver feeds.
pub(crate) const AIRPLAY: CaptureSource = CaptureSource {
    name: "airplay",
    uri: "alsa://hw:Loopback,1,4?format=44100:16:2",
    writer_status: "Loopback/pcm0p/sub4/status",
};

/// The whole table. Adding a capture is adding a line here.
const TABLE: [CaptureSource; 1] = [AIRPLAY];

/// Every capture in the table.
pub(crate) fn sources() -> &'static [CaptureSource] {
    &TABLE
}

/// The capture a caller named, or `None` when it is not in the table.
pub(crate) fn resolve(name: &str) -> Option<CaptureSource> {
    TABLE.iter().copied().find(|s| s.name == name)
}

/// What a capture shows as: the sender's complete statement about the
/// current track.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct CaptureTags {
    pub(crate) title: Option<String>,
    pub(crate) artist: Option<String>,
    pub(crate) album: Option<String>,
}

impl CaptureTags {
    /// Check the three display strings. An empty string is no tag.
    /// The reason never repeats what was sent.
    pub(crate) fn checked(
        title: Option<String>,
        artist: Option<String>,
        album: Option<String>,
    ) -> Result<CaptureTags, String> {
        Ok(CaptureTags {
            title: clean("title", title)?,
            artist: clean("artist", artist)?,
            album: clean("album", album)?,
        })
    }

    /// The MPD tag name and wanted value of each field.
    fn each(&self) -> [(&'static str, Option<&str>); 3] {
        [
            ("Title", self.title.as_deref()),
            ("Artist", self.artist.as_deref()),
            ("Album", self.album.as_deref()),
        ]
    }
}

fn clean(field: &str, value: Option<String>) -> Result<Option<String>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.chars().count() > MAX_TAG_CHARS {
        return Err(format!(
            "{field} is longer than {MAX_TAG_CHARS} characters"
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("{field} holds a control character"));
    }
    Ok(Some(value.to_string()))
}

/// Whether anything is writing to a capture's device.
pub(crate) trait WriterProbe: Send + Sync {
    /// `true` when the device's write end is open.
    fn has_writer(&self, source: &CaptureSource) -> bool;
}

/// Reads the kernel's own account: a substream nobody has open reads
/// `closed`.
pub(crate) struct ProcWriterProbe {
    root: PathBuf,
}

impl ProcWriterProbe {
    /// The running system's `/proc/asound`.
    pub(crate) fn system() -> Self {
        Self {
            root: PathBuf::from("/proc/asound"),
        }
    }

    /// A probe over another root, for tests.
    #[cfg(test)]
    pub(crate) fn at(root: PathBuf) -> Self {
        Self { root }
    }
}

impl WriterProbe for ProcWriterProbe {
    fn has_writer(&self, source: &CaptureSource) -> bool {
        match std::fs::read_to_string(self.root.join(source.writer_status)) {
            Ok(text) => {
                let text = text.trim();
                !text.is_empty() && !text.starts_with("closed")
            }
            Err(_) => false,
        }
    }
}

/// What a caller asks of the supervisor.
#[derive(Debug, Clone)]
pub(crate) enum CaptureRequest {
    /// Put the capture at the end of the queue, tag it, play it.
    Play {
        source: CaptureSource,
        tags: CaptureTags,
    },
    /// Replace the capture's tags.
    SetTags {
        source: CaptureSource,
        tags: CaptureTags,
    },
    /// Take the capture out.
    End { source: CaptureSource },
}

/// What a finished request reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CaptureOutcome {
    /// The capture's queue id, for `Play`.
    pub(crate) queue_id: Option<u32>,
}

/// Why a capture request did not finish.
#[derive(Debug)]
pub(crate) enum CaptureFailure {
    /// There is no capture in the queue.
    Absent,
    /// MPD refused, or could not be reached.
    Mpd(MpdError),
}

impl From<MpdError> for CaptureFailure {
    fn from(e: MpdError) -> Self {
        CaptureFailure::Mpd(e)
    }
}

/// Run one request on a command connection.
pub(crate) async fn run(
    conn: &mut MpdConnection,
    request: &CaptureRequest,
) -> Result<CaptureOutcome, CaptureFailure> {
    match request {
        CaptureRequest::Play { source, tags } => {
            let id = play(conn, source, tags).await?;
            Ok(CaptureOutcome { queue_id: Some(id) })
        }
        CaptureRequest::SetTags { source, tags } => {
            set_tags(conn, source, tags).await?;
            Ok(CaptureOutcome { queue_id: None })
        }
        CaptureRequest::End { source } => {
            end(conn, source).await?;
            Ok(CaptureOutcome { queue_id: None })
        }
    }
}

async fn find(
    conn: &mut MpdConnection,
    source: &CaptureSource,
) -> Result<Option<MpdQueueItem>, MpdError> {
    Ok(conn
        .playlistfind("file", source.uri)
        .await?
        .into_iter()
        .next())
}

/// Bring an item's tags to `wanted`, touching MPD only where they
/// differ: an unchanged statement is no write, so it wakes nobody.
async fn apply_tags(
    conn: &mut MpdConnection,
    id: u32,
    have: Option<&MpdQueueItem>,
    wanted: &CaptureTags,
) -> Result<(), MpdError> {
    let present = [
        have.and_then(|i| i.title.as_deref()),
        have.and_then(|i| i.artist.as_deref()),
        have.and_then(|i| i.album.as_deref()),
    ];
    for ((name, want), now) in wanted.each().into_iter().zip(present) {
        if want == now {
            continue;
        }
        if now.is_some() {
            conn.cleartagid(id, name).await?;
        }
        if let Some(value) = want {
            conn.addtagid(id, name, value).await?;
        }
    }
    Ok(())
}

async fn play(
    conn: &mut MpdConnection,
    source: &CaptureSource,
    tags: &CaptureTags,
) -> Result<u32, MpdError> {
    let existing = find(conn, source).await?;
    let (id, fresh) = match &existing {
        Some(item) => (item.id, false),
        None => (conn.addid(source.uri, None).await?, true),
    };
    let started = async {
        apply_tags(conn, id, existing.as_ref(), tags).await?;
        conn.play_id(id).await
    }
    .await;
    if let Err(e) = started {
        if fresh {
            // Never leave an item the player could not start.
            let _ = conn.deleteid(id).await;
        }
        return Err(e);
    }
    Ok(id)
}

async fn set_tags(
    conn: &mut MpdConnection,
    source: &CaptureSource,
    tags: &CaptureTags,
) -> Result<(), CaptureFailure> {
    let Some(item) = find(conn, source).await? else {
        return Err(CaptureFailure::Absent);
    };
    apply_tags(conn, item.id, Some(&item), tags).await?;
    Ok(())
}

/// Whether `item` is the song the player is playing or has paused.
async fn is_live(
    conn: &mut MpdConnection,
    item: &MpdQueueItem,
) -> Result<bool, MpdError> {
    let status = conn.status().await?;
    Ok(
        matches!(status.state, PlayState::Playing | PlayState::Paused)
            && status.song_position == Some(item.position),
    )
}

async fn end(
    conn: &mut MpdConnection,
    source: &CaptureSource,
) -> Result<(), MpdError> {
    let Some(item) = find(conn, source).await? else {
        return Ok(());
    };
    // Stop first: deleting the playing song would otherwise move the
    // player on to whatever follows it.
    if is_live(conn, &item).await? {
        conn.stop().await?;
    }
    conn.deleteid(item.id).await
}

/// Take a capture out of the queue if it is not the playing or paused
/// item. Returns whether it did.
pub(crate) async fn reap(
    conn: &mut MpdConnection,
    source: &CaptureSource,
) -> Result<bool, MpdError> {
    let Some(item) = find(conn, source).await? else {
        return Ok(false);
    };
    if is_live(conn, &item).await? {
        return Ok(false);
    }
    conn.deleteid(item.id).await?;
    Ok(true)
}

/// [`reap`] over the whole table.
pub(crate) async fn reap_all(conn: &mut MpdConnection) -> Result<(), MpdError> {
    for source in &TABLE {
        reap(conn, source).await?;
    }
    Ok(())
}

/// Whether any capture is in the queue.
pub(crate) async fn present(
    conn: &mut MpdConnection,
) -> Result<bool, MpdError> {
    for source in &TABLE {
        if find(conn, source).await?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// End a capture that nothing is writing to: its writer went away (the
/// receiver crashed or was stopped) and the player would play silence for
/// good. Returns whether one was ended.
pub(crate) async fn end_unwritten(
    conn: &mut MpdConnection,
    probe: &dyn WriterProbe,
) -> Result<bool, MpdError> {
    for source in &TABLE {
        if find(conn, source).await?.is_some() && !probe.has_writer(source) {
            end(conn, source).await?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether any capture is the song the player is on. A capture is a
/// live source, so it cannot be seeked.
pub(crate) async fn any_live(
    conn: &mut MpdConnection,
) -> Result<bool, MpdError> {
    for source in &TABLE {
        if let Some(item) = find(conn, source).await? {
            if is_live(conn, &item).await? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playback_supervisor::test_mock::{
        short_timeouts, spawn_mock_mpd, ConnBehaviour, FakeMpd,
    };
    use std::sync::{Arc, Mutex};

    const MUSIC: [&str; 3] = ["a/1.flac", "b/2.flac", "c/3.flac"];

    async fn connect(model: &Arc<Mutex<FakeMpd>>) -> MpdConnection {
        let (endpoint, _task) =
            spawn_mock_mpd(vec![ConnBehaviour::Model(model.clone())]).await;
        MpdConnection::connect_with_timeouts(endpoint, short_timeouts())
            .await
            .expect("connect to the model")
    }

    fn tags(t: &str, a: &str, b: &str) -> CaptureTags {
        CaptureTags::checked(Some(t.into()), Some(a.into()), Some(b.into()))
            .expect("tags")
    }

    fn queue(model: &Arc<Mutex<FakeMpd>>) -> Vec<String> {
        model.lock().unwrap().uris()
    }

    // ----- the closed table -----

    #[test]
    fn only_a_named_capture_resolves_and_it_is_the_loopback_read_end() {
        assert_eq!(resolve("airplay"), Some(AIRPLAY));
        for other in ["", "AirPlay", "airplay ", "hw:Loopback,1,4", "mpd"] {
            assert_eq!(resolve(other), None, "{other:?}");
        }
        for source in &TABLE {
            assert!(
                source.uri.starts_with("alsa://hw:Loopback,1,"),
                "{}: a capture is the read end of a loopback",
                source.name
            );
        }
    }

    #[test]
    fn tags_are_bounded_trimmed_and_free_of_control_characters() {
        let ok = CaptureTags::checked(
            Some("  Song ".into()),
            Some(String::new()),
            None,
        )
        .unwrap();
        assert_eq!(ok.title.as_deref(), Some("Song"));
        assert_eq!(ok.artist, None, "an empty string is no tag");
        let long = "x".repeat(MAX_TAG_CHARS + 1);
        let edge = "y".repeat(MAX_TAG_CHARS);
        assert!(CaptureTags::checked(Some(long.clone()), None, None).is_err());
        assert!(CaptureTags::checked(None, Some(edge), None).is_ok());
        let bad = CaptureTags::checked(None, None, Some("a\nb".into()));
        let reason = bad.expect_err("a newline is refused");
        assert!(!reason.contains('\n'), "the reason repeats nothing sent");
        let reason = CaptureTags::checked(Some(long), None, None).unwrap_err();
        assert!(!reason.contains("xxx"), "the reason repeats nothing sent");
    }

    #[test]
    fn a_writer_is_what_the_kernel_says_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let status = dir.path().join("Loopback/pcm0p/sub4");
        std::fs::create_dir_all(&status).unwrap();
        let probe = ProcWriterProbe::at(dir.path().to_path_buf());
        assert!(!probe.has_writer(&AIRPLAY), "no file: no Loopback");
        std::fs::write(status.join("status"), "closed\n").unwrap();
        assert!(!probe.has_writer(&AIRPLAY), "closed: nobody has it open");
        std::fs::write(
            status.join("status"),
            "state: PREPARED\nowner_pid: 1\n",
        )
        .unwrap();
        assert!(probe.has_writer(&AIRPLAY), "open, whatever the state");
        std::fs::write(status.join("status"), "").unwrap();
        assert!(
            !probe.has_writer(&AIRPLAY),
            "an empty report is not a writer"
        );
    }

    // ----- play -----

    #[tokio::test]
    async fn play_adds_to_the_end_tags_it_and_leaves_the_rest_alone() {
        let model = FakeMpd::new(&MUSIC);
        model.lock().unwrap().start_at(1);
        let mut conn = connect(&model).await;
        let out = run(
            &mut conn,
            &CaptureRequest::Play {
                source: AIRPLAY,
                tags: tags("Song", "Band", "Disc"),
            },
        )
        .await
        .unwrap();
        let id = out.queue_id.expect("an id");
        let m = model.lock().unwrap();
        let mut want = MUSIC.map(String::from).to_vec();
        want.push(AIRPLAY.uri.to_string());
        assert_eq!(m.uris(), want, "added at the end, others untouched");
        let item = m.queue.last().unwrap();
        assert_eq!(item.id, id);
        assert_eq!(
            item.tags,
            vec![
                ("Title".into(), "Song".into()),
                ("Artist".into(), "Band".into()),
                ("Album".into(), "Disc".into()),
            ]
        );
        assert_eq!(m.current, Some(3), "it is the item playing");
        assert_eq!(m.state, "play");
        assert!(
            m.log.iter().all(|c| !c.starts_with("clear")),
            "a capture never clears the queue: {:?}",
            m.log
        );
    }

    #[tokio::test]
    async fn play_again_is_the_same_item_with_the_new_tags() {
        let model = FakeMpd::new(&MUSIC);
        let mut conn = connect(&model).await;
        let play = |t: CaptureTags| CaptureRequest::Play {
            source: AIRPLAY,
            tags: t,
        };
        let first = run(&mut conn, &play(tags("One", "A", "X"))).await.unwrap();
        let second =
            run(&mut conn, &play(tags("Two", "A", "X"))).await.unwrap();
        assert_eq!(first.queue_id, second.queue_id);
        let m = model.lock().unwrap();
        assert_eq!(m.queue.len(), MUSIC.len() + 1, "one capture item");
        assert!(m
            .queue
            .last()
            .unwrap()
            .tags
            .contains(&("Title".to_string(), "Two".to_string())));
        assert_eq!(
            m.queue
                .last()
                .unwrap()
                .tags
                .iter()
                .filter(|(t, _)| t == "Title")
                .count(),
            1,
            "the old title is replaced, not joined"
        );
    }

    #[tokio::test]
    async fn a_play_the_player_refuses_leaves_nothing_behind() {
        let model = FakeMpd::new(&MUSIC);
        model.lock().unwrap().refuse.push("playid");
        let mut conn = connect(&model).await;
        let r = run(
            &mut conn,
            &CaptureRequest::Play {
                source: AIRPLAY,
                tags: tags("a", "b", "c"),
            },
        )
        .await;
        assert!(matches!(r, Err(CaptureFailure::Mpd(_))), "{r:?}");
        assert_eq!(
            queue(&model),
            MUSIC.map(String::from).to_vec(),
            "an item the player could not start is not left queued"
        );
    }

    // ----- tags -----

    #[tokio::test]
    async fn tags_with_nothing_to_change_write_nothing() {
        let model = FakeMpd::new(&MUSIC);
        let mut conn = connect(&model).await;
        let t = tags("Song", "Band", "Disc");
        run(
            &mut conn,
            &CaptureRequest::Play {
                source: AIRPLAY,
                tags: t.clone(),
            },
        )
        .await
        .unwrap();
        let before = model.lock().unwrap().log.len();
        run(
            &mut conn,
            &CaptureRequest::SetTags {
                source: AIRPLAY,
                tags: t,
            },
        )
        .await
        .unwrap();
        let log = model.lock().unwrap().log[before..].to_vec();
        assert!(
            log.iter()
                .all(|c| !c.starts_with("addtagid")
                    && !c.starts_with("cleartagid")),
            "an unchanged statement must not touch the queue: {log:?}"
        );
    }

    #[tokio::test]
    async fn a_new_statement_replaces_and_clears_what_it_leaves_out() {
        let model = FakeMpd::new(&MUSIC);
        let mut conn = connect(&model).await;
        run(
            &mut conn,
            &CaptureRequest::Play {
                source: AIRPLAY,
                tags: tags("Song", "Band", "Disc"),
            },
        )
        .await
        .unwrap();
        run(
            &mut conn,
            &CaptureRequest::SetTags {
                source: AIRPLAY,
                tags: CaptureTags::checked(Some("Other".into()), None, None)
                    .unwrap(),
            },
        )
        .await
        .unwrap();
        let m = model.lock().unwrap();
        assert_eq!(
            m.queue.last().unwrap().tags,
            vec![("Title".to_string(), "Other".to_string())],
            "fields the sender no longer states are cleared"
        );
    }

    #[tokio::test]
    async fn tags_for_a_capture_that_is_not_there_are_refused() {
        let model = FakeMpd::new(&MUSIC);
        let mut conn = connect(&model).await;
        let r = run(
            &mut conn,
            &CaptureRequest::SetTags {
                source: AIRPLAY,
                tags: tags("a", "b", "c"),
            },
        )
        .await;
        assert!(matches!(r, Err(CaptureFailure::Absent)), "{r:?}");
    }

    // ----- end -----

    #[tokio::test]
    async fn ending_takes_out_only_the_capture_and_stops_the_player() {
        let model = FakeMpd::new(&MUSIC);
        model.lock().unwrap().start_at(0);
        let mut conn = connect(&model).await;
        run(
            &mut conn,
            &CaptureRequest::Play {
                source: AIRPLAY,
                tags: tags("a", "b", "c"),
            },
        )
        .await
        .unwrap();
        run(&mut conn, &CaptureRequest::End { source: AIRPLAY })
            .await
            .unwrap();
        let m = model.lock().unwrap();
        assert_eq!(
            m.uris(),
            MUSIC.map(String::from).to_vec(),
            "the queue is as it was"
        );
        assert_eq!(m.state, "stop", "nothing else starts in its place");
        assert_ne!(
            m.current,
            Some(0),
            "the song the session interrupted is not resumed"
        );
    }

    #[tokio::test]
    async fn ending_a_capture_that_is_not_there_succeeds() {
        let model = FakeMpd::new(&MUSIC);
        model.lock().unwrap().start_at(2);
        let mut conn = connect(&model).await;
        run(&mut conn, &CaptureRequest::End { source: AIRPLAY })
            .await
            .unwrap();
        let m = model.lock().unwrap();
        assert_eq!(m.state, "play", "music that was playing plays on");
        assert!(m.log.iter().all(|c| c != "stop"), "{:?}", m.log);
    }

    #[tokio::test]
    async fn ending_a_capture_that_is_not_playing_does_not_stop_the_music() {
        let model = FakeMpd::new(&MUSIC);
        let mut conn = connect(&model).await;
        run(
            &mut conn,
            &CaptureRequest::Play {
                source: AIRPLAY,
                tags: tags("a", "b", "c"),
            },
        )
        .await
        .unwrap();
        // The operator chose another song: the capture is queued, idle.
        model.lock().unwrap().start_at(1);
        run(&mut conn, &CaptureRequest::End { source: AIRPLAY })
            .await
            .unwrap();
        let m = model.lock().unwrap();
        assert_eq!(m.uris(), MUSIC.map(String::from).to_vec());
        assert_eq!(m.state, "play");
        assert_eq!(m.current, Some(1));
    }

    // ----- reap -----

    #[tokio::test]
    async fn a_capture_that_is_not_the_item_playing_is_taken_out() {
        for state in ["stop", "play-other"] {
            let model = FakeMpd::new(&MUSIC);
            let mut conn = connect(&model).await;
            run(
                &mut conn,
                &CaptureRequest::Play {
                    source: AIRPLAY,
                    tags: tags("a", "b", "c"),
                },
            )
            .await
            .unwrap();
            {
                let mut m = model.lock().unwrap();
                if state == "stop" {
                    m.state = "stop";
                } else {
                    m.start_at(0);
                }
            }
            assert!(reap(&mut conn, &AIRPLAY).await.unwrap(), "{state}");
            assert_eq!(
                queue(&model),
                MUSIC.map(String::from).to_vec(),
                "{state}"
            );
        }
    }

    #[tokio::test]
    async fn a_playing_or_paused_capture_is_left_in_the_queue() {
        let model = FakeMpd::new(&MUSIC);
        let mut conn = connect(&model).await;
        run(
            &mut conn,
            &CaptureRequest::Play {
                source: AIRPLAY,
                tags: tags("a", "b", "c"),
            },
        )
        .await
        .unwrap();
        assert!(!reap(&mut conn, &AIRPLAY).await.unwrap(), "playing");
        model.lock().unwrap().state = "pause";
        assert!(!reap(&mut conn, &AIRPLAY).await.unwrap(), "paused");
        assert_eq!(queue(&model).len(), MUSIC.len() + 1);
    }

    #[tokio::test]
    async fn any_live_says_whether_the_player_is_on_a_capture() {
        let model = FakeMpd::new(&MUSIC);
        let mut conn = connect(&model).await;
        assert!(!any_live(&mut conn).await.unwrap(), "no capture");
        run(
            &mut conn,
            &CaptureRequest::Play {
                source: AIRPLAY,
                tags: tags("a", "b", "c"),
            },
        )
        .await
        .unwrap();
        assert!(any_live(&mut conn).await.unwrap());
        model.lock().unwrap().start_at(0);
        assert!(!any_live(&mut conn).await.unwrap(), "on music again");
    }
}
