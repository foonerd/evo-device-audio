// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Long-lived MPD connection.
//!
//! One [`MpdConnection`] wraps one logical connection to an MPD
//! daemon, held for the duration of a custody (per the plugin's
//! warden contract). The connection delivers connect, status,
//! currentsong, the transport commands (play, pause, stop, next,
//! previous, seek, set_volume), and the idle subprotocol. The
//! supervisor orchestrates two of these (one for commands, one
//! for idle — MPD blocks the connection during idle).
//!
//! Every operation has an explicit deadline. No unbounded waits.
//! The connection is failure-honest: classified errors surface the
//! cause without masking transient conditions as permanent or vice
//! versa.

use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, UnixStream};
use tokio::time;

use super::endpoint::MpdEndpoint;
use super::error::{MpdError, ProtocolError, TransportError};
use super::framing::Framing;
use super::protocol::{self, ClassifiedLine, Field};
use super::types::{
    IdleSubsystem, MpdLibraryEntry, MpdMount, MpdNeighbor, MpdPlaylistEntry,
    MpdPlaylistSummary, MpdQueueItem, MpdSearchField, MpdSong, MpdStats,
    MpdStatus, MpdSticker, MpdStickerMatch, MpdVersion, PlayState,
};

/// Timeout budgets for a single connection.
///
/// Defaults tuned for a healthy local MPD: generous enough to
/// tolerate a loaded daemon, tight enough that a dead MPD does not
/// stall the warden. All values overridable via the configuration
/// layer.
#[derive(Debug, Clone, Copy)]
pub struct ConnectTimeouts {
    /// Budget for completing the TCP or Unix connect syscall.
    pub connect: Duration,
    /// Budget for reading the welcome banner after the transport is
    /// up.
    pub welcome: Duration,
    /// Budget for a single command dispatch (write + read until OK
    /// or ACK).
    pub command: Duration,
}

impl Default for ConnectTimeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(5),
            welcome: Duration::from_secs(2),
            command: Duration::from_secs(3),
        }
    }
}

/// A live, one-shot connection to an MPD daemon.
///
/// Not cloneable, not reusable after failure: once a method returns
/// an error that indicates the connection is done for (closed,
/// protocol violation), the caller should drop this connection and
/// construct a new one. The supervisor wraps this connection
/// type to do the reconnection automatically.
pub struct MpdConnection {
    framing: Framing<
        Box<dyn AsyncRead + Send + Unpin>,
        Box<dyn AsyncWrite + Send + Unpin>,
    >,
    version: MpdVersion,
    endpoint: MpdEndpoint,
    connected_at: Instant,
    command_timeout: Duration,
}

impl std::fmt::Debug for MpdConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MpdConnection")
            .field("endpoint", &self.endpoint)
            .field("version", &self.version)
            .field("connected_at", &self.connected_at)
            .field("command_timeout", &self.command_timeout)
            .finish()
    }
}

impl MpdConnection {
    /// Connect to `endpoint` with the default timeout budget, read
    /// the welcome banner, and return the live connection.
    pub async fn connect(endpoint: MpdEndpoint) -> Result<Self, MpdError> {
        Self::connect_with_timeouts(endpoint, ConnectTimeouts::default()).await
    }

    /// Connect with a caller-specified timeout budget.
    ///
    /// Used by tests and by the configuration layer where the
    /// operator can override the defaults.
    pub async fn connect_with_timeouts(
        endpoint: MpdEndpoint,
        timeouts: ConnectTimeouts,
    ) -> Result<Self, MpdError> {
        let (reader, writer) =
            open_streams(&endpoint, timeouts.connect).await?;
        handshake(reader, writer, endpoint, timeouts).await
    }

    /// The MPD protocol version negotiated at connect.
    pub fn version(&self) -> MpdVersion {
        self.version
    }

    /// The endpoint this connection points at. Useful for log
    /// context and for future reconnection logic.
    pub fn endpoint(&self) -> &MpdEndpoint {
        &self.endpoint
    }

    /// When this connection completed its handshake.
    pub fn connected_at(&self) -> Instant {
        self.connected_at
    }

    // ----- read-only queries -----

    /// Dispatch `status` and project the response into [`MpdStatus`].
    pub async fn status(&mut self) -> Result<MpdStatus, MpdError> {
        let fields = self.dispatch("status", &[]).await?;
        parse_status(&fields)
    }

    /// Dispatch `currentsong` and project the response into
    /// `Option<MpdSong>`. Returns `None` when MPD's response is
    /// empty (no current song; queue empty or player stopped).
    pub async fn current_song(&mut self) -> Result<Option<MpdSong>, MpdError> {
        let fields = self.dispatch("currentsong", &[]).await?;
        parse_current_song(&fields)
    }

    /// Dispatch `ping`. A zero-argument no-op useful for liveness
    /// probes; the supervisor uses it to verify a dormant
    /// connection is still alive.
    pub async fn ping(&mut self) -> Result<(), MpdError> {
        self.dispatch("ping", &[]).await?;
        Ok(())
    }

    /// Dispatch `stats` and project the response into [`MpdStats`].
    ///
    /// Drives library-state rehydration: `audio_library_state`'s
    /// `total_tracks` field comes from [`MpdStats::songs`];
    /// `last_full_scan_at_ms` derives from
    /// [`MpdStats::db_update_unix_s`].
    pub async fn stats(&mut self) -> Result<MpdStats, MpdError> {
        let fields = self.dispatch("stats", &[]).await?;
        parse_stats(&fields)
    }

    // ----- transport commands -----

    /// Start or resume playback from the current queue position.
    ///
    /// Wire form: `play\n`. If the queue is empty MPD may ACK; the
    /// error surfaces as [`MpdError::Ack`].
    pub async fn play(&mut self) -> Result<(), MpdError> {
        self.dispatch("play", &[]).await?;
        Ok(())
    }

    /// Start playback at a specific queue position.
    ///
    /// Wire form: `play "<pos>"\n`. Out-of-range positions ACK.
    pub async fn play_position(&mut self, pos: u32) -> Result<(), MpdError> {
        let arg = pos.to_string();
        self.dispatch("play", &[arg.as_str()]).await?;
        Ok(())
    }

    /// Pause (`paused=true`) or resume (`paused=false`) playback.
    ///
    /// Wire form: `pause "1"\n` or `pause "0"\n`. MPD's pause
    /// command is idempotent; sending the same state twice is not
    /// an error.
    pub async fn pause(&mut self, paused: bool) -> Result<(), MpdError> {
        let arg = if paused { "1" } else { "0" };
        self.dispatch("pause", &[arg]).await?;
        Ok(())
    }

    /// Stop playback. Position is not preserved; a subsequent
    /// `play` starts from the beginning of the queue.
    ///
    /// Wire form: `stop\n`.
    pub async fn stop(&mut self) -> Result<(), MpdError> {
        self.dispatch("stop", &[]).await?;
        Ok(())
    }

    /// Clear the queue. Removes every queued song;
    /// playback stops if it was running. Used by the
    /// source-verb dispatch path on `play_now` (which
    /// replaces the queue with a single URI before
    /// playing).
    ///
    /// Wire form: `clear\n`.
    pub async fn clear(&mut self) -> Result<(), MpdError> {
        self.dispatch("clear", &[]).await?;
        Ok(())
    }

    /// Add a URI / library path to the end of the queue.
    /// MPD's `add` accepts library-relative paths (the
    /// `mpd-path:` URI scheme strips the prefix) and
    /// some external URIs (HTTP / Icecast streams) when
    /// MPD is built with the matching input plugins.
    /// `play_now` issues `clear` then `add` then `play`
    /// to replace the queue with a single URI.
    ///
    /// Wire form: `add "<path>"\n`.
    pub async fn add(&mut self, path: &str) -> Result<(), MpdError> {
        self.dispatch("add", &[path]).await?;
        Ok(())
    }

    /// Skip to the next song in the queue.
    ///
    /// Wire form: `next\n`. If the queue has no next song MPD may
    /// ACK or silently wrap depending on repeat mode; the caller
    /// reads `status` to know what happened.
    pub async fn next(&mut self) -> Result<(), MpdError> {
        self.dispatch("next", &[]).await?;
        Ok(())
    }

    /// Skip to the previous song in the queue.
    ///
    /// Wire form: `previous\n`.
    pub async fn previous(&mut self) -> Result<(), MpdError> {
        self.dispatch("previous", &[]).await?;
        Ok(())
    }

    /// Seek within the current song to an absolute position.
    ///
    /// Wire form: `seekcur "<seconds>"\n`, with `seconds` formatted
    /// to millisecond precision (e.g. `12.500`). Uses `seekcur`
    /// (seek within current song) rather than `seek` (seek by
    /// position and song), because the warden's course-correct
    /// primitive is "move the playhead" rather than "switch song".
    pub async fn seek(&mut self, pos: Duration) -> Result<(), MpdError> {
        let arg = format!("{:.3}", pos.as_secs_f64());
        self.dispatch("seekcur", &[arg.as_str()]).await?;
        Ok(())
    }

    /// Seek by a signed millisecond delta relative to the current
    /// playhead position.
    ///
    /// Wire form: `seekcur "<+|-><seconds.millis>"\n`. MPD's
    /// `seekcur` accepts a leading `+` or `-` to interpret the time
    /// as a delta from the current position; the resulting absolute
    /// position is clamped by MPD to the track's bounds. A delta of
    /// 0 ms is a no-op (still issued, MPD ACKs cleanly).
    pub async fn seek_relative(
        &mut self,
        delta_ms: i64,
    ) -> Result<(), MpdError> {
        let abs_ms = delta_ms.unsigned_abs();
        let secs = abs_ms / 1000;
        let millis = abs_ms % 1000;
        let sign = if delta_ms < 0 { "-" } else { "+" };
        let arg = format!("{sign}{secs}.{millis:03}");
        self.dispatch("seekcur", &[arg.as_str()]).await?;
        Ok(())
    }

    /// Set the output volume.
    ///
    /// Wire form: `setvol "<volume>"\n`. MPD accepts 0-100; values
    /// above 100 (legal as `u8` but out of MPD's range) surface as
    /// [`MpdError::Ack`] rather than being silently clamped.
    pub async fn set_volume(&mut self, volume: u8) -> Result<(), MpdError> {
        let arg = volume.to_string();
        self.dispatch("setvol", &[arg.as_str()]).await?;
        Ok(())
    }

    /// Set between-track crossfade duration in seconds.
    ///
    /// Wire form: `crossfade "<seconds>"\n`. `0` disables
    /// crossfade entirely. The operator-facing setter
    /// (`options.set_crossfade_seconds`) caps the value at 30 so
    /// the wire arg never exceeds MPD's accepted range; values
    /// MPD rejects surface as [`MpdError::Ack`] (no silent
    /// clamping).
    pub async fn set_crossfade(
        &mut self,
        seconds: u32,
    ) -> Result<(), MpdError> {
        let arg = seconds.to_string();
        self.dispatch("crossfade", &[arg.as_str()]).await?;
        Ok(())
    }

    /// Set MPD's `single` mode.
    ///
    /// Wire form: `single "<0|1>"\n`. Controls whether MPD stops
    /// at the end of each track (`single 1` — operator's
    /// "let-me-think-about-it" mode) or continues through the
    /// queue (`single 0` — the canonical "play through the
    /// album" mode). The operator-facing `gapless` setting maps
    /// to this verb: `gapless = true` → `single 0`, `gapless =
    /// false` → `single 1`. Note that MPD's byte-level gapless
    /// playback between same-format tracks is decided at the
    /// audio-output layer, not by this command; this verb is the
    /// queue-traversal lever the operator actually cares about.
    pub async fn set_single(&mut self, enabled: bool) -> Result<(), MpdError> {
        let arg = if enabled { "1" } else { "0" };
        self.dispatch("single", &[arg]).await?;
        Ok(())
    }

    /// Set MPD's `repeat` mode.
    ///
    /// Wire form: `repeat "<0|1>"\n`. When set, MPD restarts the
    /// queue from position 0 after the last song ends — turning
    /// a one-song queue into an infinite loop, which is the
    /// behaviour the `emit_test_tone` diagnostic must neutralise
    /// before play and restore on completion so the operator's
    /// normal music-listening state survives the diagnostic.
    pub async fn set_repeat(&mut self, enabled: bool) -> Result<(), MpdError> {
        let arg = if enabled { "1" } else { "0" };
        self.dispatch("repeat", &[arg]).await?;
        Ok(())
    }

    /// Set MPD's `random` mode.
    ///
    /// Wire form: `random "<0|1>"\n`. When set, MPD plays queue
    /// entries in random order. The `emit_test_tone` diagnostic
    /// neutralises this before play so the queue's lone test
    /// WAV is the song MPD plays, then restores the operator's
    /// prior value.
    pub async fn set_random(&mut self, enabled: bool) -> Result<(), MpdError> {
        let arg = if enabled { "1" } else { "0" };
        self.dispatch("random", &[arg]).await?;
        Ok(())
    }

    /// Set MPD's `consume` mode.
    ///
    /// Wire form: `consume "<0|1>"\n`. When set, MPD removes
    /// each song from the queue after it plays. The
    /// `emit_test_tone` diagnostic neutralises this before play
    /// (the diagnostic owns the queue and clears it cleanly on
    /// its own terms), then restores the operator's prior
    /// value.
    pub async fn set_consume(&mut self, enabled: bool) -> Result<(), MpdError> {
        let arg = if enabled { "1" } else { "0" };
        self.dispatch("consume", &[arg]).await?;
        Ok(())
    }

    /// Disable an MPD output by index, dropping the underlying
    /// `snd_pcm_t` handle. The primitive the supervisor uses
    /// when a downstream ALSA composition change (multi-room
    /// drop-in install/remove, operator-options rewrite) demands
    /// MPD reopen its ALSA output without a systemd bounce.
    ///
    /// Wire form: `disableoutput "<index>"\n`. Idempotent against
    /// already-disabled outputs.
    pub async fn disable_output(&mut self, index: u32) -> Result<(), MpdError> {
        let arg = index.to_string();
        self.dispatch("disableoutput", &[arg.as_str()]).await?;
        Ok(())
    }

    /// Enable an MPD output by index, opening a fresh
    /// `snd_pcm_t` handle. Paired with [`disable_output`] to
    /// cycle MPD's ALSA output so the next PCM open re-resolves
    /// `pcm.evo` against the current `/etc/asound.d/`
    /// composition.
    ///
    /// Wire form: `enableoutput "<index>"\n`. Idempotent against
    /// already-enabled outputs.
    pub async fn enable_output(&mut self, index: u32) -> Result<(), MpdError> {
        let arg = index.to_string();
        self.dispatch("enableoutput", &[arg.as_str()]).await?;
        Ok(())
    }

    // ----- idle subprotocol -----

    /// Subscribe to subsystem change events.
    ///
    /// Sends `idle` (optionally with a subsystem allow-list as
    /// arguments) and blocks until MPD reports that one or more
    /// subsystems have changed. Returns the list of subsystems that
    /// changed. An empty vec is returned when MPD responds with an
    /// immediate `OK` (the supervisor can trigger this by sending
    /// `noidle` from the command connection, though the current
    /// implementation does not).
    ///
    /// `budget` bounds the total wall-clock time spent inside this
    /// method. If no change arrives within the budget, returns
    /// [`MpdError::Timeout`] with `operation = "idle"` and `elapsed`
    /// equal to the wall-clock time from entry. The caller should
    /// then consider the connection suspect (drop and reconnect;
    /// the supervisor does exactly that).
    ///
    /// The connection may only be used for idle while idle is
    /// in-flight. Calling `play`, `status`, etc. from another task
    /// while idle is pending is not supported; MPD will see the
    /// extra command, treat it as `noidle` intent, and may respond
    /// in ways this layer does not handle. The supervisor enforces
    /// separation by holding idle on a dedicated connection.
    pub async fn idle(
        &mut self,
        subsystems: &[IdleSubsystem],
        budget: Duration,
    ) -> Result<Vec<IdleSubsystem>, MpdError> {
        let start = Instant::now();

        let args: Vec<&str> =
            subsystems.iter().map(|s| s.as_protocol_str()).collect();
        let bytes = protocol::serialise_command("idle", &args)?;

        tracing::debug!(
            plugin = "evo-mpd-shared",
            endpoint = %self.endpoint,
            subsystem_count = subsystems.len(),
            budget_ms = budget.as_millis() as u64,
            "mpd idle dispatch"
        );

        // The write uses the standard command timeout: getting bytes
        // onto the socket should always be fast regardless of how
        // long we are willing to wait for a change event.
        self.framing
            .write_all_with_timeout(&bytes, self.command_timeout, "write_idle")
            .await?;

        // Per-read deadlines are computed against a single overall
        // deadline so the total wait never exceeds `budget`, no
        // matter how MPD paces its response lines. If any internal
        // read times out, it is re-wrapped as an `idle` timeout
        // with caller-visible wall-clock elapsed: the internal
        // `read_idle` operation name and the last read's budget
        // are implementation details that do not belong in the
        // caller's error.
        let deadline = start.checked_add(budget);
        let mut changed: Vec<IdleSubsystem> = Vec::new();
        loop {
            let remaining = match deadline {
                Some(d) => d.saturating_duration_since(Instant::now()),
                None => budget, // budget overflowed Instant; fall back.
            };
            if remaining.is_zero() {
                return Err(MpdError::Timeout {
                    operation: "idle",
                    elapsed: start.elapsed(),
                });
            }
            let line = match self
                .framing
                .read_line_with_timeout(remaining, "read_idle")
                .await
            {
                Ok(l) => l,
                Err(MpdError::Timeout { .. }) => {
                    return Err(MpdError::Timeout {
                        operation: "idle",
                        elapsed: start.elapsed(),
                    });
                }
                Err(other) => return Err(other),
            };
            match protocol::classify_line(&line)? {
                ClassifiedLine::Ok => return Ok(changed),
                ClassifiedLine::ListOk => {
                    return Err(MpdError::Protocol(
                        ProtocolError::UnexpectedListOk,
                    ));
                }
                ClassifiedLine::Ack {
                    code,
                    list_position,
                    command,
                    message,
                } => {
                    return Err(MpdError::Ack {
                        code,
                        list_position,
                        command,
                        message,
                    });
                }
                ClassifiedLine::Field(f) => {
                    if f.key == "changed" {
                        changed
                            .push(IdleSubsystem::from_protocol_str(&f.value));
                    }
                    // Other keys (MPD may gain new ones) ignored.
                }
            }
        }
    }

    // ----- internal dispatch -----

    /// Send a command and collect its body fields until OK or ACK.
    async fn dispatch(
        &mut self,
        command: &str,
        args: &[&str],
    ) -> Result<Vec<Field>, MpdError> {
        let bytes = protocol::serialise_command(command, args)?;

        tracing::debug!(
            plugin = "evo-mpd-shared",
            endpoint = %self.endpoint,
            command,
            "mpd command dispatch"
        );

        self.framing
            .write_all_with_timeout(
                &bytes,
                self.command_timeout,
                "write_command",
            )
            .await?;

        let mut fields = Vec::new();
        loop {
            let line = self
                .framing
                .read_line_with_timeout(self.command_timeout, "read_response")
                .await?;
            match protocol::classify_line(&line)? {
                ClassifiedLine::Ok => return Ok(fields),
                ClassifiedLine::ListOk => {
                    return Err(MpdError::Protocol(
                        ProtocolError::UnexpectedListOk,
                    ));
                }
                ClassifiedLine::Ack {
                    code,
                    list_position,
                    command,
                    message,
                } => {
                    return Err(MpdError::Ack {
                        code,
                        list_position,
                        command,
                        message,
                    });
                }
                ClassifiedLine::Field(f) => fields.push(f),
            }
        }
    }

    // ----- queue inspection + mutation -----

    /// Read the full queue listing.
    ///
    /// Wire form: `playlistinfo\n`. MPD returns one repeated
    /// block per queue item, each starting with `file:` and
    /// containing the per-song metadata + `Pos:` + `Id:`.
    pub async fn playlistinfo(
        &mut self,
    ) -> Result<Vec<MpdQueueItem>, MpdError> {
        let fields = self.dispatch("playlistinfo", &[]).await?;
        Ok(parse_queue_items(&fields))
    }

    /// Find queue items whose tag `TYPE` equals `WHAT`.
    ///
    /// Wire form: `playlistfind "<type>" "<what>"\n`. Unlike
    /// [`Self::find`] (library/DB), this searches the **current
    /// queue** — the only place `addtagid` tags for HTTP/DLNA
    /// streams are visible. Typical call: `playlistfind("file",
    /// "http://…/file.mp3")` to recover Title/Artist/Album for a
    /// remote URI that is not in MPD's database.
    pub async fn playlistfind(
        &mut self,
        tag: &str,
        value: &str,
    ) -> Result<Vec<MpdQueueItem>, MpdError> {
        let fields = self.dispatch("playlistfind", &[tag, value]).await?;
        Ok(parse_queue_items(&fields))
    }

    /// Add a URI to the queue and return its assigned songid.
    /// Distinct from [`Self::add`] which does not return the
    /// songid (and is consequently fine for `play_now`-shape
    /// fire-and-forget appends but not for the queue.enqueue
    /// verb which the wire surface needs the id for).
    ///
    /// Wire form: `addid "<uri>" "<pos>"\n` (when position
    /// non-null) or `addid "<uri>"\n` (when null).
    pub async fn addid(
        &mut self,
        uri: &str,
        position: Option<u32>,
    ) -> Result<u32, MpdError> {
        let pos_str: String;
        let args: Vec<&str> = match position {
            Some(p) => {
                pos_str = p.to_string();
                vec![uri, pos_str.as_str()]
            }
            None => vec![uri],
        };
        let fields = self.dispatch("addid", &args).await?;
        for f in &fields {
            if f.key == "Id" {
                return parse_u32_field("Id", &f.value);
            }
        }
        Err(MpdError::Protocol(ProtocolError::MissingField {
            field: "Id",
        }))
    }

    /// Attach a metadata tag to a queue item by songid.
    ///
    /// Used by the enqueue path for HTTP streams (DLNA-resolved
    /// `http(s)://…` URIs) so `playlistinfo` / `currentsong`
    /// report the tag on the queue projection. MPD does not
    /// extract ID3 tags from arbitrary HTTP streams on `add`, so
    /// the plugin has to hand the DIDL-derived tags in via
    /// `addtagid` immediately after the `addid`.
    ///
    /// `tag` is a case-insensitive tag name — `Title`, `Artist`,
    /// `Album`, `AlbumArtist`, `Composer`, `Date`, etc. — the
    /// MPD tag-name set. `value` is the operator-visible string.
    ///
    /// Wire form: `addtagid "<id>" "<tag>" "<value>"\n`. Same
    /// silent-drop semantics as `addid` for the leading `Id`
    /// field: MPD acknowledges with an empty response on success.
    pub async fn addtagid(
        &mut self,
        id: u32,
        tag: &str,
        value: &str,
    ) -> Result<(), MpdError> {
        let id_str = id.to_string();
        self.dispatch("addtagid", &[id_str.as_str(), tag, value])
            .await?;
        Ok(())
    }

    /// Remove a queue item by songid.
    ///
    /// Wire form: `deleteid "<id>"\n`.
    pub async fn deleteid(&mut self, id: u32) -> Result<(), MpdError> {
        let arg = id.to_string();
        self.dispatch("deleteid", &[arg.as_str()]).await?;
        Ok(())
    }

    /// Move a queue item to a new position by songid.
    ///
    /// Wire form: `moveid "<id>" "<to_position>"\n`.
    pub async fn moveid(
        &mut self,
        id: u32,
        to_position: u32,
    ) -> Result<(), MpdError> {
        let id_str = id.to_string();
        let pos_str = to_position.to_string();
        self.dispatch("moveid", &[id_str.as_str(), pos_str.as_str()])
            .await?;
        Ok(())
    }

    // ----- stored playlist operations -----

    /// List every stored playlist.
    ///
    /// Wire form: `listplaylists\n`. MPD returns one repeated
    /// block per playlist with `playlist:` and optional
    /// `Last-Modified:`.
    pub async fn listplaylists(
        &mut self,
    ) -> Result<Vec<MpdPlaylistSummary>, MpdError> {
        let fields = self.dispatch("listplaylists", &[]).await?;
        Ok(parse_playlist_summaries(&fields))
    }

    /// Fetch the file-line count of every named stored playlist
    /// in a single batched round-trip via
    /// [`Self::command_list_ok`].
    ///
    /// Wire form (for N names): one
    /// `command_list_ok_begin` ... `listplaylist NAME1` ...
    /// `listplaylist NAME2` ... `command_list_end\n` payload.
    /// MPD responds with the file lines of each playlist
    /// separated by `list_OK` terminators; this method counts
    /// the `file:` lines per group and returns the count for
    /// every input name in the input order.
    ///
    /// Returns an empty `Vec` for an empty input. The result is
    /// `Vec<Option<u32>>` not `Vec<u32>`: if MPD's response
    /// carries fewer groups than commands sent (a corner case
    /// some MPD versions trigger when a playlist name is
    /// invalid mid-list), missing entries surface as `None`
    /// rather than silently aliasing to a neighbour. The caller
    /// can decide whether `None` means "absent" or "skip this
    /// row" based on the wider contract.
    ///
    /// Used by the audio.playlist warden's index rehydration to
    /// populate `audio_playlist_index.items[].item_count` with
    /// MPD truth in O(1) round-trips instead of O(N).
    pub async fn playlist_file_counts(
        &mut self,
        names: &[&str],
    ) -> Result<Vec<Option<u32>>, MpdError> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let commands: Vec<(&str, Vec<String>)> = names
            .iter()
            .map(|n| ("listplaylist", vec![(*n).to_string()]))
            .collect();
        let groups = self.command_list_ok(&commands).await?;
        let mut out: Vec<Option<u32>> = Vec::with_capacity(names.len());
        for i in 0..names.len() {
            if let Some(group) = groups.get(i) {
                let count = group.iter().filter(|f| f.key == "file").count();
                out.push(Some(count as u32));
            } else {
                out.push(None);
            }
        }
        Ok(out)
    }

    /// Read one stored playlist's contents.
    ///
    /// Wire form: `listplaylistinfo "<name>"\n`.
    pub async fn listplaylistinfo(
        &mut self,
        name: &str,
    ) -> Result<Vec<MpdPlaylistEntry>, MpdError> {
        let fields = self.dispatch("listplaylistinfo", &[name]).await?;
        Ok(parse_playlist_entries(&fields))
    }

    /// Load a stored playlist's contents to the queue, appending.
    ///
    /// Wire form: `load "<name>"\n`.
    pub async fn load_playlist(&mut self, name: &str) -> Result<(), MpdError> {
        self.dispatch("load", &[name]).await?;
        Ok(())
    }

    /// Save the current queue as a stored playlist.
    ///
    /// Wire form: `save "<name>"\n`. MPD refuses with ACK 56
    /// (exists) when the playlist already exists.
    pub async fn save_playlist(&mut self, name: &str) -> Result<(), MpdError> {
        self.dispatch("save", &[name]).await?;
        Ok(())
    }

    /// Append a URI to a stored playlist.
    ///
    /// Wire form: `playlistadd "<name>" "<uri>"\n`.
    pub async fn playlistadd(
        &mut self,
        name: &str,
        uri: &str,
    ) -> Result<(), MpdError> {
        self.dispatch("playlistadd", &[name, uri]).await?;
        Ok(())
    }

    /// Count tracks that match a tag filter without
    /// materialising the URI list. One MPD roundtrip.
    ///
    /// Wire form: `count TAG1 "V1" [TAG2 "V2" ...]\n`. MPD
    /// responds with a `songs:` line + `playtime:` line;
    /// this method returns the parsed songs count.
    ///
    /// Used by the `queue.enqueue_selection` and
    /// `playlist.save_selection` verbs to detect a
    /// zero-match Filter selection before dispatching the
    /// mutating `findadd` / `searchaddpl` — so the caller
    /// can surface an explicit empty response instead of a
    /// silent no-op.
    pub async fn count_matching(
        &mut self,
        pairs: &[(&str, &str)],
    ) -> Result<u64, MpdError> {
        if pairs.is_empty() {
            return Ok(0);
        }
        let mut args: Vec<&str> = Vec::with_capacity(pairs.len() * 2);
        for (tag, value) in pairs {
            args.push(tag);
            args.push(value);
        }
        let fields = self.dispatch("count", &args).await?;
        for f in &fields {
            if f.key == "songs" {
                return f.value.trim().parse::<u64>().map_err(|_| {
                    MpdError::Protocol(ProtocolError::UnparseableField {
                        field: "songs",
                        value: f.value.clone(),
                    })
                });
            }
        }
        Ok(0)
    }

    /// Resolve a tag-filter to a track set and add every match
    /// to the CURRENT queue, in MPD's canonical order (disc-
    /// then-track within album, album order within artist).
    /// One MPD roundtrip — no per-track URI dispatch.
    ///
    /// Wire form: `findadd TAG1 "V1" [TAG2 "V2" ...]\n`. Empty
    /// pairs is a no-op (no wire dispatch).
    ///
    /// Used by the `queue.enqueue_selection` verb for
    /// dimensions that map cleanly to an MPD tag filter
    /// (`artist`, `albumartist`, `genre`, exact `date`).
    /// Substring dimensions (`year` against `date` = `1996-05`)
    /// use [`Self::searchadd`] instead.
    pub async fn findadd(
        &mut self,
        pairs: &[(&str, &str)],
    ) -> Result<(), MpdError> {
        if pairs.is_empty() {
            return Ok(());
        }
        let mut args: Vec<&str> = Vec::with_capacity(pairs.len() * 2);
        for (tag, value) in pairs {
            args.push(tag);
            args.push(value);
        }
        self.dispatch("findadd", &args).await?;
        Ok(())
    }

    /// Substring-match variant of [`Self::findadd`]. Resolves
    /// `TAG` values that contain the given substring
    /// (case-insensitive per MPD's `search` semantics) and
    /// adds every match to the CURRENT queue.
    ///
    /// Wire form: `searchadd TAG1 "V1" [TAG2 "V2" ...]\n`.
    ///
    /// Used for the `year` dimension against MPD's `date` tag
    /// so a track tagged `1996-05-13` matches the operator's
    /// `1996` bucket without listing every date suffix.
    pub async fn searchadd(
        &mut self,
        pairs: &[(&str, &str)],
    ) -> Result<(), MpdError> {
        if pairs.is_empty() {
            return Ok(());
        }
        let mut args: Vec<&str> = Vec::with_capacity(pairs.len() * 2);
        for (tag, value) in pairs {
            args.push(tag);
            args.push(value);
        }
        self.dispatch("searchadd", &args).await?;
        Ok(())
    }

    /// Substring-match resolve + write to a STORED playlist.
    /// Creates the playlist if it doesn't exist; appends to it
    /// if it does. One MPD roundtrip.
    ///
    /// Wire form: `searchaddpl "<name>" TAG1 "V1" [TAG2 "V2" ...]\n`.
    ///
    /// Used by the `playlist.save_selection` verb. The
    /// `mode = "create"` variant uses [`Self::playlistclear`]
    /// (or delete) via a command list before this call so the
    /// playlist starts empty.
    pub async fn searchaddpl(
        &mut self,
        name: &str,
        pairs: &[(&str, &str)],
    ) -> Result<(), MpdError> {
        if pairs.is_empty() {
            return Ok(());
        }
        let mut args: Vec<&str> = Vec::with_capacity(pairs.len() * 2 + 1);
        args.push(name);
        for (tag, value) in pairs {
            args.push(tag);
            args.push(value);
        }
        self.dispatch("searchaddpl", &args).await?;
        Ok(())
    }

    /// Remove an entry from a stored playlist by position.
    ///
    /// Wire form: `playlistdelete "<name>" "<position>"\n`.
    pub async fn playlistdelete(
        &mut self,
        name: &str,
        position: u32,
    ) -> Result<(), MpdError> {
        let pos = position.to_string();
        self.dispatch("playlistdelete", &[name, pos.as_str()])
            .await?;
        Ok(())
    }

    /// Empty a stored playlist (without removing it).
    ///
    /// Wire form: `playlistclear "<name>"\n`.
    pub async fn playlistclear(&mut self, name: &str) -> Result<(), MpdError> {
        self.dispatch("playlistclear", &[name]).await?;
        Ok(())
    }

    /// Move an entry within a stored playlist.
    ///
    /// Wire form: `playlistmove "<name>" "<from>" "<to>"\n`.
    pub async fn playlistmove(
        &mut self,
        name: &str,
        from_position: u32,
        to_position: u32,
    ) -> Result<(), MpdError> {
        let from = from_position.to_string();
        let to = to_position.to_string();
        self.dispatch("playlistmove", &[name, from.as_str(), to.as_str()])
            .await?;
        Ok(())
    }

    /// Rename a stored playlist.
    ///
    /// Wire form: `rename "<from>" "<to>"\n`.
    pub async fn rename_playlist(
        &mut self,
        from_name: &str,
        to_name: &str,
    ) -> Result<(), MpdError> {
        self.dispatch("rename", &[from_name, to_name]).await?;
        Ok(())
    }

    /// Delete a stored playlist.
    ///
    /// Wire form: `rm "<name>"\n`.
    pub async fn rm_playlist(&mut self, name: &str) -> Result<(), MpdError> {
        self.dispatch("rm", &[name]).await?;
        Ok(())
    }

    // ----- sticker operations -----
    //
    // MPD's sticker subsystem attaches durable per-song key-value
    // pairs that survive update / rescan / mount-unmount / MPD
    // restart. The framework uses `evo:available` as the canonical
    // per-song availability flag (0 = unavailable, 1 = available).

    /// Read a single sticker on a song.
    ///
    /// Wire form: `sticker get song "<uri>" "<name>"\n`.
    /// Returns `None` when the sticker is not set (MPD ACKs with
    /// code 50; the method translates that to `Ok(None)` for the
    /// caller's ergonomic).
    pub async fn sticker_get(
        &mut self,
        uri: &str,
        name: &str,
    ) -> Result<Option<String>, MpdError> {
        match self.dispatch("sticker", &["get", "song", uri, name]).await {
            Ok(fields) => {
                for f in &fields {
                    if f.key == "sticker" {
                        if let Some(value) = sticker_parse_value(&f.value, name)
                        {
                            return Ok(Some(value));
                        }
                    }
                }
                Ok(None)
            }
            Err(MpdError::Ack { code: 50, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Set a sticker on a song. Overwrites the existing value
    /// when one is already set.
    ///
    /// Wire form: `sticker set song "<uri>" "<name>" "<value>"\n`.
    pub async fn sticker_set(
        &mut self,
        uri: &str,
        name: &str,
        value: &str,
    ) -> Result<(), MpdError> {
        self.dispatch("sticker", &["set", "song", uri, name, value])
            .await?;
        Ok(())
    }

    /// Delete a single sticker on a song.
    ///
    /// Wire form: `sticker delete song "<uri>" "<name>"\n`. MPD
    /// ACKs with code 50 when the sticker is not set; the method
    /// translates that to `Ok(())` for idempotency.
    pub async fn sticker_delete(
        &mut self,
        uri: &str,
        name: &str,
    ) -> Result<(), MpdError> {
        match self
            .dispatch("sticker", &["delete", "song", uri, name])
            .await
        {
            Ok(_) => Ok(()),
            Err(MpdError::Ack { code: 50, .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// List every sticker on a song.
    ///
    /// Wire form: `sticker list song "<uri>"\n`.
    pub async fn sticker_list(
        &mut self,
        uri: &str,
    ) -> Result<Vec<MpdSticker>, MpdError> {
        let fields = self.dispatch("sticker", &["list", "song", uri]).await?;
        Ok(parse_sticker_list(&fields))
    }

    /// Find every song under `base` whose sticker `name` matches
    /// (existence only when `equal_to` is `None`; exact value match
    /// when set).
    ///
    /// Wire form: `sticker find song "<base>" "<name>"\n` or
    /// `sticker find song "<base>" "<name>" = "<value>"\n`.
    pub async fn sticker_find(
        &mut self,
        base: &str,
        name: &str,
        equal_to: Option<&str>,
    ) -> Result<Vec<MpdStickerMatch>, MpdError> {
        let fields = match equal_to {
            Some(value) => {
                self.dispatch(
                    "sticker",
                    &["find", "song", base, name, "=", value],
                )
                .await?
            }
            None => {
                self.dispatch("sticker", &["find", "song", base, name])
                    .await?
            }
        };
        Ok(parse_sticker_find(&fields))
    }

    // ----- library browse / search / update -----

    /// List the contents of a directory in MPD's library.
    ///
    /// Wire form: `lsinfo "<path>"\n`. Empty path lists the root.
    pub async fn lsinfo(
        &mut self,
        path: &str,
    ) -> Result<Vec<MpdLibraryEntry>, MpdError> {
        let fields = if path.is_empty() {
            self.dispatch("lsinfo", &[]).await?
        } else {
            self.dispatch("lsinfo", &[path]).await?
        };
        Ok(parse_library_entries(&fields))
    }

    /// Recursive listing of every song in MPD's database with
    /// full tag content. Returns only `MpdLibraryEntry::File`
    /// entries (directories and playlists are suppressed by
    /// MPD's `listallinfo` itself).
    ///
    /// Wire form: `listallinfo "<path>"\n`. Empty path walks
    /// the entire database. Used by the works-aggregation path
    /// (`library.list_works` and `library.get_work_recordings`)
    /// as well as the library-state counter computation
    /// (composer count, distinct-works count, and the
    /// works-with-multiple-recordings count). The walk visits
    /// every track once; for a 1134-track library this is
    /// sub-second over the Unix socket.
    ///
    /// MPD's response carries the full classical-tag set per
    /// entry, so the aggregation consumer sees Work / Composer
    /// / Conductor / Ensemble / etc. through the same
    /// `ClassicalTags` projection the per-track envelopes use.
    pub async fn listallinfo(
        &mut self,
        path: &str,
    ) -> Result<Vec<MpdLibraryEntry>, MpdError> {
        let fields = if path.is_empty() {
            self.dispatch("listallinfo", &[]).await?
        } else {
            self.dispatch("listallinfo", &[path]).await?
        };
        Ok(parse_library_entries(&fields))
    }

    /// Exact-match search across MPD's library.
    ///
    /// Wire form: `find "<field>" "<query>"\n`. Case-sensitive
    /// exact match; for substring search use [`Self::search`].
    pub async fn find(
        &mut self,
        field: MpdSearchField,
        query: &str,
    ) -> Result<Vec<MpdLibraryEntry>, MpdError> {
        let fields = self
            .dispatch("find", &[field.as_protocol_str(), query])
            .await?;
        Ok(parse_library_entries(&fields))
    }

    /// Multi-field case-sensitive exact match — MPD accepts
    /// paired `TYPE VALUE` arguments and ANDs the constraints
    /// together.
    ///
    /// Wire form: `find "<field1>" "<query1>" "<field2>" "<query2>"…`.
    ///
    /// The browse-by facet drill uses this when a parent
    /// context is present: selecting album "Older" scoped to
    /// artist "George Michael" issues
    /// `find album "Older" albumartist "George Michael"`,
    /// dropping every "Older" pressing under a different
    /// credited artist that would surface on a single-field
    /// find.
    pub async fn find_multi(
        &mut self,
        pairs: &[(MpdSearchField, &str)],
    ) -> Result<Vec<MpdLibraryEntry>, MpdError> {
        // Materialise the borrowed pairs into a flat &[&str]
        // list the dispatch layer expects.
        let mut args: Vec<&str> = Vec::with_capacity(pairs.len() * 2);
        for (field, value) in pairs {
            args.push(field.as_protocol_str());
            args.push(*value);
        }
        let fields = self.dispatch("find", &args).await?;
        Ok(parse_library_entries(&fields))
    }

    /// Case-insensitive substring search across MPD's library.
    ///
    /// Wire form: `search "<field>" "<query>"\n`.
    pub async fn search(
        &mut self,
        field: MpdSearchField,
        query: &str,
    ) -> Result<Vec<MpdLibraryEntry>, MpdError> {
        let fields = self
            .dispatch("search", &[field.as_protocol_str(), query])
            .await?;
        Ok(parse_library_entries(&fields))
    }

    /// List distinct values of a single MPD tag across the
    /// library.
    ///
    /// Wire form: `list "<tag>"\n` — MPD returns lines like
    /// `Artist: <name>` or `Album: <title>` (the key mirrors the
    /// TYPE argument). This method extracts the value strings
    /// (after the `<key>: ` prefix), preserving MPD's own
    /// ordering — for tags that MPD indexes, that ordering is
    /// stable across calls so pagination is deterministic
    /// without a sort pass here.
    ///
    /// `tag` accepts any MPD tag name string: `artist`,
    /// `album`, `genre`, `date`, `albumartist`, etc. Callers
    /// pass the canonical protocol-level string; the browse
    /// verbs hardcode the four the operator surface exposes.
    ///
    /// Empty values (MPD emits `Artist: ` on tag-less files
    /// when it wants to represent the tag-absent bucket) are
    /// filtered out before return — a browse-by-artist facet
    /// shouldn't include a blank entry that operator UI has
    /// no useful label for.
    pub async fn list_tag(
        &mut self,
        tag: &str,
    ) -> Result<Vec<String>, MpdError> {
        let fields = self.dispatch("list", &[tag]).await?;
        Ok(fields
            .into_iter()
            .filter_map(|f| {
                let trimmed = f.value.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            })
            .collect())
    }

    /// List distinct values of `tag` grouped by `group_by`.
    ///
    /// Wire form: `list "<tag>" group "<group_by>"\n` — MPD
    /// returns interleaved pairs
    /// (`GroupBy: <group_value>\n<Tag>: <tag_value>\n…`)
    /// emitting the group header once and then every distinct
    /// `tag` value under it.
    ///
    /// Returns `(tag_value, group_value)` pairs. Empty group
    /// headers (MPD's tag-absent bucket) are mapped to empty
    /// strings on the group side; callers decide whether the
    /// blank group is operator-visible.
    ///
    /// One roundtrip for the entire library — replaces
    /// `list <tag>` + N × `list <group> filter <tag>=<val>` on
    /// the album-enumeration path.
    pub async fn list_tag_grouped(
        &mut self,
        tag: &str,
        group_by: &str,
    ) -> Result<Vec<(String, String)>, MpdError> {
        let fields = self.dispatch("list", &[tag, "group", group_by]).await?;
        // The response is interleaved: one Group line, then the
        // Tag lines under it, then next Group line, then its
        // tags, etc. Walk the field list once and pair each
        // Tag value with the most-recent Group value.
        let mut current_group = String::new();
        let tag_lower = tag.to_ascii_lowercase();
        let group_lower = group_by.to_ascii_lowercase();
        let mut out: Vec<(String, String)> = Vec::new();
        for field in fields {
            let key_lower = field.key.to_ascii_lowercase();
            if key_lower == group_lower {
                current_group = field.value.trim().to_string();
            } else if key_lower == tag_lower {
                let value = field.value.trim().to_string();
                if !value.is_empty() {
                    out.push((value, current_group.clone()));
                }
            }
            // Silently ignore other keys — MPD may prefix the
            // response with an OK/status line depending on the
            // dispatch layer.
        }
        Ok(out)
    }

    /// Per-value song counts for `tag`, grouped in one MPD
    /// roundtrip.
    ///
    /// Wire form: `count group "<tag>"\n` — MPD emits, for
    /// every distinct value of `tag`, a `TagName: <value>\n`
    /// header followed by `songs: <n>\n` and `playtime: <s>\n`.
    /// This method walks the response once and returns a
    /// `{tag_value: song_count}` map.
    ///
    /// Playtime is dropped — the browse envelope's shipped
    /// shape is track counts only.
    pub async fn count_grouped_by_tag(
        &mut self,
        tag: &str,
    ) -> Result<std::collections::HashMap<String, u64>, MpdError> {
        let fields = self.dispatch("count", &["group", tag]).await?;
        let mut current_value: Option<String> = None;
        let tag_lower = tag.to_ascii_lowercase();
        let mut out = std::collections::HashMap::new();
        for field in fields {
            let key_lower = field.key.to_ascii_lowercase();
            if key_lower == tag_lower {
                current_value = Some(field.value.trim().to_string());
            } else if key_lower == "songs" {
                if let Some(v) = current_value.as_ref() {
                    if !v.is_empty() {
                        if let Ok(n) = field.value.trim().parse::<u64>() {
                            out.insert(v.clone(), n);
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    /// List distinct values of `tag` filtered to `<filter_tag>
    /// = <filter_value>`.
    ///
    /// Wire form: `list "<tag>" "<filter_tag>" "<filter_value>"\n`
    /// — MPD's shape for the filtered form of `list`. Used to
    /// enumerate distinct albums for a specific album-artist,
    /// distinct tracks for a specific album, etc.
    ///
    /// Empty values are filtered out at the connection layer,
    /// mirroring [`Self::list_tag`]'s discipline.
    pub async fn list_tag_filtered(
        &mut self,
        tag: &str,
        filter_tag: &str,
        filter_value: &str,
    ) -> Result<Vec<String>, MpdError> {
        let fields = self
            .dispatch("list", &[tag, filter_tag, filter_value])
            .await?;
        Ok(fields
            .into_iter()
            .filter_map(|f| {
                let trimmed = f.value.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            })
            .collect())
    }

    /// Trigger an incremental library scan.
    ///
    /// Wire form: `update "<path>"\n` (when path non-empty) or
    /// `update\n`. MPD reads files whose mtime changed since the
    /// last scan; for a force-rescan that re-reads every file see
    /// [`Self::rescan`].
    pub async fn update(&mut self, path: Option<&str>) -> Result<(), MpdError> {
        match path {
            Some(p) => {
                self.dispatch("update", &[p]).await?;
            }
            None => {
                self.dispatch("update", &[]).await?;
            }
        }
        Ok(())
    }

    /// Trigger a full library rescan.
    ///
    /// Wire form: `rescan "<path>"\n` or `rescan\n`. Re-reads
    /// every file's metadata regardless of mtime.
    pub async fn rescan(&mut self, path: Option<&str>) -> Result<(), MpdError> {
        match path {
            Some(p) => {
                self.dispatch("rescan", &[p]).await?;
            }
            None => {
                self.dispatch("rescan", &[]).await?;
            }
        }
        Ok(())
    }

    /// List MPD's current mounts.
    ///
    /// Wire form: `listmounts\n`.
    pub async fn listmounts(&mut self) -> Result<Vec<MpdMount>, MpdError> {
        let fields = self.dispatch("listmounts", &[]).await?;
        Ok(parse_mounts(&fields))
    }

    /// Mount a storage URI under an alias.
    ///
    /// Wire form: `mount "<name>" "<storage>"\n`. MPD ACKs when
    /// the storage URI scheme is not supported.
    pub async fn mount_storage(
        &mut self,
        name: &str,
        storage: &str,
    ) -> Result<(), MpdError> {
        self.dispatch("mount", &[name, storage]).await?;
        Ok(())
    }

    /// Unmount a storage by alias.
    ///
    /// Wire form: `unmount "<name>"\n`.
    pub async fn unmount_storage(
        &mut self,
        name: &str,
    ) -> Result<(), MpdError> {
        self.dispatch("unmount", &[name]).await?;
        Ok(())
    }

    /// List discovered storage neighbours.
    ///
    /// Wire form: `listneighbors\n`.
    pub async fn listneighbors(
        &mut self,
    ) -> Result<Vec<MpdNeighbor>, MpdError> {
        let fields = self.dispatch("listneighbors", &[]).await?;
        Ok(parse_neighbors(&fields))
    }

    // ----- command list batching -----

    /// Run a batch of commands in a single MPD round-trip via
    /// `command_list_begin` / `command_list_end`. Each command's
    /// fields are concatenated into one response; the caller
    /// reads aggregate Ok or the first Ack.
    ///
    /// Used by the sticker reconciler for bulk
    /// `sticker set song <uri> evo:available <0|1>` writes
    /// against an entire source's mount path — 100k sticker
    /// writes batch into ~1k command-list groups instead of
    /// 100k individual round-trips.
    ///
    /// Each entry is `(command, args)`; arguments are quoted
    /// per the existing dispatch path. Empty batch is a no-op
    /// (no command_list issued).
    pub async fn command_list(
        &mut self,
        commands: &[(&str, Vec<String>)],
    ) -> Result<(), MpdError> {
        if commands.is_empty() {
            return Ok(());
        }
        // Build the serialised command_list_begin ... command_list_end
        // payload as a single write.
        let mut payload: Vec<u8> = Vec::with_capacity(
            commands
                .iter()
                .map(|(c, a)| {
                    c.len() + a.iter().map(|s| s.len() + 3).sum::<usize>() + 2
                })
                .sum::<usize>()
                + 40,
        );
        payload.extend_from_slice(b"command_list_begin\n");
        for (cmd, args) in commands {
            let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            let bytes = protocol::serialise_command(cmd, &args_ref)?;
            payload.extend_from_slice(&bytes);
        }
        payload.extend_from_slice(b"command_list_end\n");

        tracing::debug!(
            plugin = "evo-mpd-shared",
            endpoint = %self.endpoint,
            command_count = commands.len(),
            payload_bytes = payload.len(),
            "mpd command_list dispatch"
        );

        let budget = self.command_timeout;
        self.framing
            .write_all_with_timeout(&payload, budget, "command_list_write")
            .await?;

        // Drain fields until OK / Ack.
        let mut _fields: Vec<Field> = Vec::new();
        loop {
            let line = self
                .framing
                .read_line_with_timeout(budget, "command_list_response")
                .await?;
            match protocol::classify_line(&line)? {
                ClassifiedLine::Ok => return Ok(()),
                ClassifiedLine::ListOk => {
                    return Err(MpdError::Protocol(
                        ProtocolError::UnexpectedListOk,
                    ));
                }
                ClassifiedLine::Ack {
                    code,
                    list_position,
                    command,
                    message,
                } => {
                    return Err(MpdError::Ack {
                        code,
                        list_position,
                        command,
                        message,
                    });
                }
                ClassifiedLine::Field(f) => _fields.push(f),
            }
        }
    }

    /// Dispatch a batched group of commands with per-command
    /// response groups via MPD's `command_list_ok_begin` ...
    /// `command_list_end` protocol. Returns one
    /// `Vec<Field>` per command in the input order, separated
    /// on the wire by `list_OK` terminators.
    ///
    /// Use this when N commands must round-trip together with
    /// per-command results — e.g. fetching the file-line count
    /// of every stored playlist (`listplaylist NAME` ×N) in one
    /// TCP round-trip instead of N. Fire-and-forget batches
    /// (where per-command results are not needed) belong on the
    /// existing [`Self::command_list`] method.
    ///
    /// Each entry is `(command, args)`; arguments are quoted
    /// per the existing dispatch path. Empty batch is a no-op
    /// returning an empty `Vec`.
    ///
    /// A `command_list` ACK (any single command in the batch
    /// fails) aborts the whole dispatch — no partial results
    /// are returned. The caller can split the batch and retry
    /// per-command if that semantic matters.
    pub async fn command_list_ok(
        &mut self,
        commands: &[(&str, Vec<String>)],
    ) -> Result<Vec<Vec<Field>>, MpdError> {
        if commands.is_empty() {
            return Ok(Vec::new());
        }
        let mut payload: Vec<u8> = Vec::with_capacity(
            commands
                .iter()
                .map(|(c, a)| {
                    c.len() + a.iter().map(|s| s.len() + 3).sum::<usize>() + 2
                })
                .sum::<usize>()
                + 48,
        );
        payload.extend_from_slice(b"command_list_ok_begin\n");
        for (cmd, args) in commands {
            let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            let bytes = protocol::serialise_command(cmd, &args_ref)?;
            payload.extend_from_slice(&bytes);
        }
        payload.extend_from_slice(b"command_list_end\n");

        tracing::debug!(
            plugin = "evo-mpd-shared",
            endpoint = %self.endpoint,
            command_count = commands.len(),
            payload_bytes = payload.len(),
            "mpd command_list_ok dispatch"
        );

        let budget = self.command_timeout;
        self.framing
            .write_all_with_timeout(&payload, budget, "command_list_ok_write")
            .await?;

        let mut groups: Vec<Vec<Field>> = Vec::with_capacity(commands.len());
        let mut current: Vec<Field> = Vec::new();
        loop {
            let line = self
                .framing
                .read_line_with_timeout(budget, "command_list_ok_response")
                .await?;
            match protocol::classify_line(&line)? {
                ClassifiedLine::ListOk => {
                    groups.push(std::mem::take(&mut current));
                }
                ClassifiedLine::Ok => {
                    // MPD emits exactly one `list_OK` per
                    // command in the list. If the caller
                    // accidentally finished before all groups
                    // were closed, the residual `current`
                    // would otherwise be silently dropped; we
                    // include it as the final group so the
                    // count matches `commands.len()`. In
                    // practice MPD's contract makes this a
                    // no-op (current is empty here).
                    if !current.is_empty() {
                        groups.push(std::mem::take(&mut current));
                    }
                    return Ok(groups);
                }
                ClassifiedLine::Ack {
                    code,
                    list_position,
                    command,
                    message,
                } => {
                    return Err(MpdError::Ack {
                        code,
                        list_position,
                        command,
                        message,
                    });
                }
                ClassifiedLine::Field(f) => current.push(f),
            }
        }
    }
}

/// Open the appropriate transport for `endpoint`, with a hard
/// connect-timeout budget. Returns the two type-erased halves ready
/// to be handed to [`Framing`].
async fn open_streams(
    endpoint: &MpdEndpoint,
    connect_budget: Duration,
) -> Result<
    (
        Box<dyn AsyncRead + Send + Unpin>,
        Box<dyn AsyncWrite + Send + Unpin>,
    ),
    MpdError,
> {
    match endpoint {
        MpdEndpoint::Tcp { host, port } => {
            let addr = format!("{}:{}", host, port);
            let stream =
                time::timeout(connect_budget, TcpStream::connect(&addr))
                    .await
                    .map_err(|_| MpdError::Timeout {
                        operation: "tcp_connect",
                        elapsed: connect_budget,
                    })?
                    .map_err(|e| {
                        MpdError::Transport(TransportError::TcpConnect {
                            endpoint: addr.clone(),
                            source: e,
                        })
                    })?;
            // Disable Nagle: MPD dispatch is request-response on small
            // commands; coalescing adds latency without throughput gain.
            if let Err(e) = stream.set_nodelay(true) {
                tracing::warn!(
                    plugin = "evo-mpd-shared",
                    error = %e,
                    "failed to set TCP_NODELAY; continuing"
                );
            }
            let (r, w) = stream.into_split();
            Ok((Box::new(r), Box::new(w)))
        }
        MpdEndpoint::Unix { path } => {
            let stream =
                time::timeout(connect_budget, UnixStream::connect(path))
                    .await
                    .map_err(|_| MpdError::Timeout {
                        operation: "unix_connect",
                        elapsed: connect_budget,
                    })?
                    .map_err(|e| {
                        MpdError::Transport(TransportError::UnixConnect {
                            path: path.display().to_string(),
                            source: e,
                        })
                    })?;
            let (r, w) = stream.into_split();
            Ok((Box::new(r), Box::new(w)))
        }
    }
}

/// Read the welcome banner and construct the connection wrapper.
///
/// Extracted so tests can feed it a duplex pair without going
/// through real sockets.
async fn handshake(
    reader: Box<dyn AsyncRead + Send + Unpin>,
    writer: Box<dyn AsyncWrite + Send + Unpin>,
    endpoint: MpdEndpoint,
    timeouts: ConnectTimeouts,
) -> Result<MpdConnection, MpdError> {
    let mut framing = Framing::new(reader, writer);
    let welcome = framing
        .read_line_with_timeout(timeouts.welcome, "welcome")
        .await?;
    let version = protocol::parse_welcome(&welcome)?;

    tracing::info!(
        plugin = "evo-mpd-shared",
        endpoint = %endpoint,
        mpd_version = %version,
        "mpd connection established"
    );

    Ok(MpdConnection {
        framing,
        version,
        endpoint,
        connected_at: Instant::now(),
        command_timeout: timeouts.command,
    })
}

// ----- Field projection into narrow types -----

fn parse_status(fields: &[Field]) -> Result<MpdStatus, MpdError> {
    let mut state: Option<PlayState> = None;
    let mut song_position: Option<u32> = None;
    let mut elapsed: Option<Duration> = None;
    let mut duration: Option<Duration> = None;
    let mut volume: Option<u8> = None;
    let mut repeat = false;
    let mut random = false;
    let mut single = false;
    let mut consume = false;
    let mut crossfade_seconds: u32 = 0;

    for f in fields {
        match f.key.as_str() {
            "state" => {
                state = Some(match f.value.as_str() {
                    "play" => PlayState::Playing,
                    "pause" => PlayState::Paused,
                    "stop" => PlayState::Stopped,
                    _ => {
                        return Err(MpdError::Protocol(
                            ProtocolError::UnknownPlayState(f.value.clone()),
                        ));
                    }
                });
            }
            "song" => {
                song_position = Some(parse_u32_field("song", &f.value)?);
            }
            "elapsed" => {
                elapsed = parse_duration_secs_field("elapsed", &f.value)?;
            }
            "duration" => {
                duration = parse_duration_secs_field("duration", &f.value)?;
            }
            "volume" => {
                volume = parse_volume_field(&f.value)?;
            }
            "repeat" => {
                repeat = f.value.as_str() == "1";
            }
            "random" => {
                random = f.value.as_str() == "1";
            }
            "single" => {
                // MPD 0.21+ extended `single` to a three-state
                // ("0" / "1" / "oneshot"); the warden's restore
                // path collapses both non-zero values to `true`
                // because the diagnostic's intent (`single 0` is
                // the canonical "play through" mode) is binary.
                // Restore via `set_single(true)` carries the
                // operator's prior commitment forward; if they
                // had `oneshot` engaged, the restore reapplies
                // `single 1` which is a close-enough subset
                // (oneshot decays to `single 0` after one track
                // anyway).
                single = f.value.as_str() != "0";
            }
            "consume" => {
                consume = f.value.as_str() == "1";
            }
            "xfade" => {
                crossfade_seconds =
                    parse_u32_field("xfade", &f.value).unwrap_or(0);
            }
            _ => {}
        }
    }

    let state =
        state.ok_or(MpdError::Protocol(ProtocolError::MissingField {
            field: "state",
        }))?;

    Ok(MpdStatus {
        state,
        song_position,
        elapsed,
        duration,
        volume,
        repeat,
        random,
        single,
        consume,
        crossfade_seconds,
    })
}

fn parse_current_song(fields: &[Field]) -> Result<Option<MpdSong>, MpdError> {
    if fields.is_empty() {
        return Ok(None);
    }

    let mut file_path: Option<String> = None;
    let mut title: Option<String> = None;
    let mut artist: Option<String> = None;
    let mut album: Option<String> = None;
    let mut duration: Option<Duration> = None;
    let mut classical = super::types::ClassicalTags::default();

    for f in fields {
        // Classical tags first — `try_apply` consumes the field
        // when it matches and returns true so we skip the
        // default arm. The bespoke arms below cover MPD fields
        // not handled by the classical tag set (file/Title/
        // Artist/Album/duration/Time).
        if classical.try_apply(&f.key, &f.value) {
            continue;
        }
        match f.key.as_str() {
            "file" => file_path = Some(f.value.clone()),
            "Title" => title = Some(f.value.clone()),
            "Artist" => artist = Some(f.value.clone()),
            "Album" => album = Some(f.value.clone()),
            "duration" => {
                duration = parse_duration_secs_field("duration", &f.value)?;
            }
            "Time" if duration.is_none() => {
                // Older MPD versions use integer seconds under
                // `Time`. Only used as fallback when `duration`
                // was not present.
                if let Ok(secs) = f.value.parse::<u64>() {
                    duration = Some(Duration::from_secs(secs));
                }
            }
            _ => {}
        }
    }

    let Some(file_path) = file_path else {
        // `currentsong` returned fields but no `file`. Unusual; treat
        // as no current song rather than error, matching MPD's own
        // edge-case behaviour.
        return Ok(None);
    };

    let codec_name = crate::types::derive_source_codec_name(&file_path);

    Ok(Some(MpdSong {
        file_path,
        title,
        artist,
        album,
        duration,
        codec_name,
        classical,
    }))
}

// ----- queue + playlist + sticker + library parsers -----

/// Parse MPD's `playlistinfo` response into queue items. Each
/// repeated block starts with `file:`; subsequent fields up to
/// the next `file:` belong to that item.
fn parse_queue_items(fields: &[Field]) -> Vec<MpdQueueItem> {
    let mut items: Vec<MpdQueueItem> = Vec::new();
    let mut current: Option<QueueItemBuilder> = None;
    for f in fields {
        if f.key == "file" {
            if let Some(b) = current.take() {
                if let Some(item) = b.build() {
                    items.push(item);
                }
            }
            current = Some(QueueItemBuilder {
                file_path: f.value.clone(),
                title: None,
                artist: None,
                album: None,
                duration: None,
                position: None,
                id: None,
                classical: super::types::ClassicalTags::default(),
            });
            continue;
        }
        let Some(b) = current.as_mut() else { continue };
        if b.classical.try_apply(&f.key, &f.value) {
            continue;
        }
        match f.key.as_str() {
            "Title" => b.title = Some(f.value.clone()),
            "Artist" => b.artist = Some(f.value.clone()),
            "Album" => b.album = Some(f.value.clone()),
            "duration" => {
                if let Ok(Some(d)) =
                    parse_duration_secs_field("duration", &f.value)
                {
                    b.duration = Some(d);
                }
            }
            "Time" if b.duration.is_none() => {
                if let Ok(secs) = f.value.parse::<u64>() {
                    b.duration = Some(Duration::from_secs(secs));
                }
            }
            "Pos" => {
                if let Ok(p) = f.value.parse::<u32>() {
                    b.position = Some(p);
                }
            }
            "Id" => {
                if let Ok(id) = f.value.parse::<u32>() {
                    b.id = Some(id);
                }
            }
            _ => {}
        }
    }
    if let Some(b) = current.take() {
        if let Some(item) = b.build() {
            items.push(item);
        }
    }
    items
}

struct QueueItemBuilder {
    file_path: String,
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    duration: Option<Duration>,
    position: Option<u32>,
    id: Option<u32>,
    classical: super::types::ClassicalTags,
}

impl QueueItemBuilder {
    fn build(self) -> Option<MpdQueueItem> {
        let position = self.position?;
        let id = self.id?;
        Some(MpdQueueItem {
            id,
            position,
            file_path: self.file_path,
            title: self.title,
            artist: self.artist,
            album: self.album,
            duration: self.duration,
            classical: self.classical,
        })
    }
}

/// Parse MPD's `listplaylists` response into playlist summaries.
/// Each block starts with `playlist:`; an optional
/// `Last-Modified:` follows.
fn parse_playlist_summaries(fields: &[Field]) -> Vec<MpdPlaylistSummary> {
    let mut out: Vec<MpdPlaylistSummary> = Vec::new();
    let mut current: Option<MpdPlaylistSummary> = None;
    for f in fields {
        if f.key == "playlist" {
            if let Some(s) = current.take() {
                out.push(s);
            }
            current = Some(MpdPlaylistSummary {
                name: f.value.clone(),
                last_modified: None,
            });
            continue;
        }
        if f.key == "Last-Modified" {
            if let Some(s) = current.as_mut() {
                s.last_modified = Some(f.value.clone());
            }
        }
    }
    if let Some(s) = current.take() {
        out.push(s);
    }
    out
}

/// Parse MPD's `listplaylistinfo NAME` response into playlist
/// entries. The block shape mirrors `playlistinfo` minus the
/// queue-specific `Pos:` / `Id:` (positions are assigned by
/// parse order).
fn parse_playlist_entries(fields: &[Field]) -> Vec<MpdPlaylistEntry> {
    let mut entries: Vec<MpdPlaylistEntry> = Vec::new();
    let mut current: Option<MpdPlaylistEntry> = None;
    let mut next_pos: u32 = 0;
    for f in fields {
        if f.key == "file" {
            if let Some(e) = current.take() {
                entries.push(e);
            }
            current = Some(MpdPlaylistEntry {
                position: next_pos,
                file_path: f.value.clone(),
                title: None,
                artist: None,
                album: None,
                duration: None,
                classical: super::types::ClassicalTags::default(),
            });
            next_pos = next_pos.saturating_add(1);
            continue;
        }
        let Some(e) = current.as_mut() else { continue };
        if e.classical.try_apply(&f.key, &f.value) {
            continue;
        }
        match f.key.as_str() {
            "Title" => e.title = Some(f.value.clone()),
            "Artist" => e.artist = Some(f.value.clone()),
            "Album" => e.album = Some(f.value.clone()),
            "duration" => {
                if let Ok(Some(d)) =
                    parse_duration_secs_field("duration", &f.value)
                {
                    e.duration = Some(d);
                }
            }
            "Time" if e.duration.is_none() => {
                if let Ok(secs) = f.value.parse::<u64>() {
                    e.duration = Some(Duration::from_secs(secs));
                }
            }
            _ => {}
        }
    }
    if let Some(e) = current.take() {
        entries.push(e);
    }
    entries
}

/// Parse the `sticker:` value field. MPD encodes it as
/// `NAME=VALUE`; the method strips the leading `NAME=` to return
/// just the value.
fn sticker_parse_value(raw: &str, expected_name: &str) -> Option<String> {
    let prefix = format!("{}=", expected_name);
    raw.strip_prefix(prefix.as_str()).map(str::to_string)
}

/// Parse a generic `NAME=VALUE` sticker line into the name and
/// value parts. MPD's `sticker list` response repeats this shape
/// without a separating field, so the parser uses the first `=`
/// as the boundary.
fn sticker_split_pair(raw: &str) -> Option<(String, String)> {
    let eq = raw.find('=')?;
    Some((raw[..eq].to_string(), raw[eq + 1..].to_string()))
}

/// Parse MPD's `sticker list song <uri>` response.
fn parse_sticker_list(fields: &[Field]) -> Vec<MpdSticker> {
    fields
        .iter()
        .filter(|f| f.key == "sticker")
        .filter_map(|f| sticker_split_pair(&f.value))
        .map(|(name, value)| MpdSticker { name, value })
        .collect()
}

/// Parse MPD's `sticker find` response. Each match is a `file:`
/// followed by one or more `sticker:` lines.
fn parse_sticker_find(fields: &[Field]) -> Vec<MpdStickerMatch> {
    let mut out: Vec<MpdStickerMatch> = Vec::new();
    let mut current_file: Option<String> = None;
    for f in fields {
        if f.key == "file" {
            current_file = Some(f.value.clone());
            continue;
        }
        if f.key == "sticker" {
            if let (Some(path), Some((name, value))) =
                (current_file.as_ref(), sticker_split_pair(&f.value))
            {
                out.push(MpdStickerMatch {
                    file_path: path.clone(),
                    sticker: MpdSticker { name, value },
                });
            }
        }
    }
    out
}

/// Parse MPD's `lsinfo PATH` / `find` / `search` response into
/// library entries. The response interleaves `directory:`,
/// `file:`, and `playlist:` blocks.
fn parse_library_entries(fields: &[Field]) -> Vec<MpdLibraryEntry> {
    let mut out: Vec<MpdLibraryEntry> = Vec::new();
    let mut current: Option<LibraryEntryBuilder> = None;
    for f in fields {
        match f.key.as_str() {
            "directory" => {
                if let Some(b) = current.take() {
                    if let Some(e) = b.build() {
                        out.push(e);
                    }
                }
                current = Some(LibraryEntryBuilder::Directory {
                    path: f.value.clone(),
                    last_modified: None,
                });
            }
            "file" => {
                if let Some(b) = current.take() {
                    if let Some(e) = b.build() {
                        out.push(e);
                    }
                }
                current = Some(LibraryEntryBuilder::File {
                    path: f.value.clone(),
                    title: None,
                    artist: None,
                    albumartist: None,
                    album: None,
                    duration: None,
                    classical: super::types::ClassicalTags::default(),
                });
            }
            "playlist" => {
                if let Some(b) = current.take() {
                    if let Some(e) = b.build() {
                        out.push(e);
                    }
                }
                current = Some(LibraryEntryBuilder::Playlist {
                    path: f.value.clone(),
                    last_modified: None,
                });
            }
            _ => {
                if let Some(b) = current.as_mut() {
                    b.absorb_field(&f.key, &f.value);
                }
            }
        }
    }
    if let Some(b) = current.take() {
        if let Some(e) = b.build() {
            out.push(e);
        }
    }
    out
}

/// In-flight accumulator paired with [`MpdLibraryEntry`]; see
/// that type for the variant-size rationale (short-lived
/// projection types in a streaming parser, allocating per
/// entry harms the hot path with no upside).
#[allow(clippy::large_enum_variant)]
enum LibraryEntryBuilder {
    Directory {
        path: String,
        last_modified: Option<String>,
    },
    File {
        path: String,
        title: Option<String>,
        artist: Option<String>,
        albumartist: Option<String>,
        album: Option<String>,
        duration: Option<Duration>,
        classical: super::types::ClassicalTags,
    },
    Playlist {
        path: String,
        last_modified: Option<String>,
    },
}

impl LibraryEntryBuilder {
    fn absorb_field(&mut self, key: &str, value: &str) {
        // Classical tags first on File entries — try_apply
        // consumes the field when matched.
        if let Self::File { classical, .. } = self {
            if classical.try_apply(key, value) {
                return;
            }
        }
        match (self, key) {
            (Self::Directory { last_modified, .. }, "Last-Modified") => {
                *last_modified = Some(value.to_string());
            }
            (Self::Playlist { last_modified, .. }, "Last-Modified") => {
                *last_modified = Some(value.to_string());
            }
            (Self::File { title, .. }, "Title") => {
                *title = Some(value.to_string())
            }
            (Self::File { artist, .. }, "Artist") => {
                *artist = Some(value.to_string())
            }
            (Self::File { albumartist, .. }, "AlbumArtist") => {
                *albumartist = Some(value.to_string())
            }
            (Self::File { album, .. }, "Album") => {
                *album = Some(value.to_string())
            }
            (Self::File { duration, .. }, "duration") => {
                if let Ok(Some(d)) =
                    parse_duration_secs_field("duration", value)
                {
                    *duration = Some(d);
                }
            }
            (Self::File { duration, .. }, "Time") if duration.is_none() => {
                if let Ok(secs) = value.parse::<u64>() {
                    *duration = Some(Duration::from_secs(secs));
                }
            }
            _ => {}
        }
    }

    fn build(self) -> Option<MpdLibraryEntry> {
        Some(match self {
            Self::Directory {
                path,
                last_modified,
            } => MpdLibraryEntry::Directory {
                path,
                last_modified,
            },
            Self::File {
                path,
                title,
                artist,
                albumartist,
                album,
                duration,
                classical,
            } => MpdLibraryEntry::File {
                path,
                title,
                artist,
                albumartist,
                album,
                duration,
                classical,
            },
            Self::Playlist {
                path,
                last_modified,
            } => MpdLibraryEntry::Playlist {
                path,
                last_modified,
            },
        })
    }
}

/// Parse MPD's `listmounts` response into mount records. Each
/// mount is a `mount:` followed by a `storage:` line.
fn parse_mounts(fields: &[Field]) -> Vec<MpdMount> {
    let mut out: Vec<MpdMount> = Vec::new();
    let mut current: Option<MpdMount> = None;
    for f in fields {
        match f.key.as_str() {
            "mount" => {
                if let Some(m) = current.take() {
                    out.push(m);
                }
                current = Some(MpdMount {
                    name: f.value.clone(),
                    storage: String::new(),
                });
            }
            "storage" => {
                if let Some(m) = current.as_mut() {
                    m.storage = f.value.clone();
                }
            }
            _ => {}
        }
    }
    if let Some(m) = current.take() {
        out.push(m);
    }
    out
}

/// Parse MPD's `listneighbors` response. Each neighbour is a
/// `neighbor:` (URI) followed by a `name:` (display name).
fn parse_neighbors(fields: &[Field]) -> Vec<MpdNeighbor> {
    let mut out: Vec<MpdNeighbor> = Vec::new();
    let mut current: Option<MpdNeighbor> = None;
    for f in fields {
        match f.key.as_str() {
            "neighbor" => {
                if let Some(n) = current.take() {
                    out.push(n);
                }
                current = Some(MpdNeighbor {
                    uri: f.value.clone(),
                    name: String::new(),
                });
            }
            "name" => {
                if let Some(n) = current.as_mut() {
                    n.name = f.value.clone();
                }
            }
            _ => {}
        }
    }
    if let Some(n) = current.take() {
        out.push(n);
    }
    out
}

fn parse_u32_field(field: &'static str, value: &str) -> Result<u32, MpdError> {
    value.parse::<u32>().map_err(|_| {
        MpdError::Protocol(ProtocolError::UnparseableField {
            field,
            value: value.to_string(),
        })
    })
}

fn parse_u64_field(field: &'static str, value: &str) -> Result<u64, MpdError> {
    value.parse::<u64>().map_err(|_| {
        MpdError::Protocol(ProtocolError::UnparseableField {
            field,
            value: value.to_string(),
        })
    })
}

fn parse_stats(fields: &[Field]) -> Result<MpdStats, MpdError> {
    let mut stats = MpdStats::default();
    for f in fields {
        match f.key.as_str() {
            "artists" => stats.artists = parse_u32_field("artists", &f.value)?,
            "albums" => stats.albums = parse_u32_field("albums", &f.value)?,
            "songs" => stats.songs = parse_u32_field("songs", &f.value)?,
            "db_update" => {
                stats.db_update_unix_s =
                    Some(parse_u64_field("db_update", &f.value)?);
            }
            _ => {}
        }
    }
    Ok(stats)
}

fn parse_duration_secs_field(
    field: &'static str,
    value: &str,
) -> Result<Option<Duration>, MpdError> {
    let secs = value.parse::<f64>().map_err(|_| {
        MpdError::Protocol(ProtocolError::UnparseableField {
            field,
            value: value.to_string(),
        })
    })?;
    if !secs.is_finite() || secs < 0.0 {
        return Ok(None);
    }
    Ok(Some(Duration::from_secs_f64(secs)))
}

fn parse_volume_field(value: &str) -> Result<Option<u8>, MpdError> {
    let raw = value.parse::<i32>().map_err(|_| {
        MpdError::Protocol(ProtocolError::UnparseableField {
            field: "volume",
            value: value.to_string(),
        })
    })?;
    // MPD reports -1 when no mixer is configured. Other out-of-range
    // values are treated as "unknown" rather than erroring out,
    // matching MPD's own liberal clamping behaviour.
    if (0..=100).contains(&raw) {
        Ok(Some(raw as u8))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::time::Duration;

    use tokio::io::{duplex, AsyncWriteExt};
    use tokio::net::{TcpListener, UnixListener};
    use tokio::sync::oneshot;

    // ----- helpers -----

    fn fake_endpoint() -> MpdEndpoint {
        MpdEndpoint::tcp("mock", 0).unwrap()
    }

    fn short_timeouts() -> ConnectTimeouts {
        ConnectTimeouts {
            connect: Duration::from_millis(500),
            welcome: Duration::from_millis(500),
            command: Duration::from_millis(500),
        }
    }

    /// Spawn a mock MPD on the given duplex-server half. The script
    /// is written to the client; everything the client writes is
    /// drained silently.
    fn spawn_script(
        mut server: tokio::io::DuplexStream,
        script: &'static [u8],
    ) {
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            server.write_all(script).await.unwrap();
            server.flush().await.unwrap();
            // Drain whatever the client writes, until disconnect.
            let mut buf = vec![0u8; 1024];
            loop {
                match server.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        });
    }

    /// Spawn a mock MPD that writes `script` and, once it detects the
    /// client has sent any command, follows up with `response`.
    fn spawn_scripted_exchange(
        mut server: tokio::io::DuplexStream,
        welcome: &'static [u8],
        response: &'static [u8],
    ) {
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            server.write_all(welcome).await.unwrap();
            server.flush().await.unwrap();

            // Wait until the client has sent a full command line
            // (anything ending in '\n').
            let mut accum: Vec<u8> = Vec::new();
            let mut buf = vec![0u8; 1024];
            loop {
                let n = match server.read(&mut buf).await {
                    Ok(0) => return,
                    Ok(n) => n,
                    Err(_) => return,
                };
                accum.extend_from_slice(&buf[..n]);
                if accum.contains(&b'\n') {
                    break;
                }
            }

            server.write_all(response).await.unwrap();
            server.flush().await.unwrap();

            // Keep connection open so the client's subsequent OK-read
            // does not see EOF.
            loop {
                match server.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
            }
        });
    }

    /// Like [`spawn_scripted_exchange`] but returns the bytes the
    /// client sent (up to and including its first newline) over a
    /// oneshot channel. Use to assert on the wire bytes of outgoing
    /// commands.
    fn spawn_capturing_exchange(
        mut server: tokio::io::DuplexStream,
        welcome: &'static [u8],
        response: &'static [u8],
    ) -> oneshot::Receiver<Vec<u8>> {
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            server.write_all(welcome).await.unwrap();
            server.flush().await.unwrap();

            let mut captured: Vec<u8> = Vec::new();
            let mut buf = vec![0u8; 1024];
            loop {
                let n = match server.read(&mut buf).await {
                    Ok(0) => {
                        let _ = tx.send(captured);
                        return;
                    }
                    Ok(n) => n,
                    Err(_) => {
                        let _ = tx.send(captured);
                        return;
                    }
                };
                captured.extend_from_slice(&buf[..n]);
                if captured.contains(&b'\n') {
                    break;
                }
            }
            let captured_report = captured.clone();
            let _ = tx.send(captured_report);

            server.write_all(response).await.unwrap();
            server.flush().await.unwrap();

            loop {
                match server.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
            }
        });
        rx
    }

    async fn handshake_from_duplex(
        server: tokio::io::DuplexStream,
        client: tokio::io::DuplexStream,
        welcome: &'static [u8],
    ) -> Result<MpdConnection, MpdError> {
        spawn_script(server, welcome);
        let (r, w) = tokio::io::split(client);
        handshake(Box::new(r), Box::new(w), fake_endpoint(), short_timeouts())
            .await
    }

    async fn handshake_for_exchange(
        client: tokio::io::DuplexStream,
    ) -> MpdConnection {
        let (r, w) = tokio::io::split(client);
        handshake(Box::new(r), Box::new(w), fake_endpoint(), short_timeouts())
            .await
            .unwrap()
    }

    // ----- handshake behaviour -----

    #[tokio::test]
    async fn connect_parses_welcome_banner() {
        let (server, client) = duplex(1024);
        let conn = handshake_from_duplex(server, client, b"OK MPD 0.23.5\n")
            .await
            .unwrap();
        assert_eq!(conn.version(), MpdVersion::new(0, 23, 5));
    }

    #[tokio::test]
    async fn connect_rejects_bad_welcome() {
        let (server, client) = duplex(1024);
        let err = handshake_from_duplex(server, client, b"NOT A WELCOME\n")
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            MpdError::Protocol(ProtocolError::BadWelcome(_))
        ));
    }

    #[tokio::test]
    async fn connect_rejects_bad_version() {
        let (server, client) = duplex(1024);
        let err = handshake_from_duplex(server, client, b"OK MPD something\n")
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            MpdError::Protocol(ProtocolError::BadVersion(_))
        ));
    }

    #[tokio::test]
    async fn connect_returns_closed_when_peer_closes_without_welcome() {
        let (server, client) = duplex(1024);
        drop(server);
        let (r, w) = tokio::io::split(client);
        let err = handshake(
            Box::new(r),
            Box::new(w),
            fake_endpoint(),
            short_timeouts(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, MpdError::Transport(TransportError::Closed)));
    }

    // ----- status dispatch -----

    #[tokio::test]
    async fn status_parses_play_state_and_fields() {
        let (server, client) = duplex(4096);
        spawn_scripted_exchange(
            server,
            b"OK MPD 0.23.5\n",
            b"volume: 50\nstate: play\nsong: 3\nelapsed: 12.345\nduration: 180.0\nOK\n",
        );
        let mut conn = handshake_for_exchange(client).await;

        let s = conn.status().await.unwrap();
        assert_eq!(s.state, PlayState::Playing);
        assert_eq!(s.song_position, Some(3));
        assert_eq!(s.volume, Some(50));
        assert_eq!(s.elapsed, Some(Duration::from_millis(12_345)));
        assert_eq!(s.duration, Some(Duration::from_millis(180_000)));
    }

    #[tokio::test]
    async fn status_handles_volume_minus_one_as_unknown() {
        let (server, client) = duplex(4096);
        spawn_scripted_exchange(
            server,
            b"OK MPD 0.23.5\n",
            b"volume: -1\nstate: stop\nOK\n",
        );
        let mut conn = handshake_for_exchange(client).await;

        let s = conn.status().await.unwrap();
        assert_eq!(s.state, PlayState::Stopped);
        assert_eq!(s.volume, None);
        assert_eq!(s.song_position, None);
    }

    #[tokio::test]
    async fn status_reports_pause_state() {
        let (server, client) = duplex(4096);
        spawn_scripted_exchange(
            server,
            b"OK MPD 0.23.5\n",
            b"state: pause\nsong: 0\nOK\n",
        );
        let mut conn = handshake_for_exchange(client).await;

        let s = conn.status().await.unwrap();
        assert_eq!(s.state, PlayState::Paused);
    }

    #[tokio::test]
    async fn status_errors_on_unknown_play_state() {
        let (server, client) = duplex(4096);
        spawn_scripted_exchange(
            server,
            b"OK MPD 0.23.5\n",
            b"state: wibbling\nOK\n",
        );
        let mut conn = handshake_for_exchange(client).await;

        let err = conn.status().await.unwrap_err();
        assert!(matches!(
            err,
            MpdError::Protocol(ProtocolError::UnknownPlayState(_))
        ));
    }

    #[tokio::test]
    async fn status_errors_when_state_field_missing() {
        let (server, client) = duplex(4096);
        spawn_scripted_exchange(
            server,
            b"OK MPD 0.23.5\n",
            b"volume: 50\nsong: 3\nOK\n",
        );
        let mut conn = handshake_for_exchange(client).await;

        let err = conn.status().await.unwrap_err();
        assert!(matches!(
            err,
            MpdError::Protocol(ProtocolError::MissingField { field: "state" })
        ));
    }

    #[tokio::test]
    async fn status_surfaces_ack_as_mpderror_ack() {
        let (server, client) = duplex(4096);
        spawn_scripted_exchange(
            server,
            b"OK MPD 0.23.5\n",
            b"ACK [2@0] {status} Bad argument\n",
        );
        let mut conn = handshake_for_exchange(client).await;

        let err = conn.status().await.unwrap_err();
        match err {
            MpdError::Ack {
                code,
                command,
                message,
                ..
            } => {
                assert_eq!(code, 2);
                assert_eq!(command, "status");
                assert_eq!(message, "Bad argument");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    // ----- current_song dispatch -----

    #[tokio::test]
    async fn current_song_populated_returns_some() {
        let (server, client) = duplex(4096);
        spawn_scripted_exchange(
            server,
            b"OK MPD 0.23.5\n",
            b"file: INTERNAL/Artist/Album/track.flac\nTitle: Track One\nArtist: An Artist\nAlbum: An Album\nduration: 242.5\nOK\n",
        );
        let mut conn = handshake_for_exchange(client).await;

        let s = conn.current_song().await.unwrap().unwrap();
        assert_eq!(s.file_path, "INTERNAL/Artist/Album/track.flac");
        assert_eq!(s.title.as_deref(), Some("Track One"));
        assert_eq!(s.artist.as_deref(), Some("An Artist"));
        assert_eq!(s.album.as_deref(), Some("An Album"));
        assert_eq!(s.duration, Some(Duration::from_millis(242_500)));
    }

    #[tokio::test]
    async fn current_song_empty_response_returns_none() {
        let (server, client) = duplex(4096);
        spawn_scripted_exchange(server, b"OK MPD 0.23.5\n", b"OK\n");
        let mut conn = handshake_for_exchange(client).await;

        let s = conn.current_song().await.unwrap();
        assert!(s.is_none());
    }

    #[tokio::test]
    async fn current_song_uses_time_as_duration_fallback_on_old_mpd() {
        let (server, client) = duplex(4096);
        spawn_scripted_exchange(
            server,
            b"OK MPD 0.21.0\n",
            b"file: x.flac\nTitle: t\nTime: 300\nOK\n",
        );
        let mut conn = handshake_for_exchange(client).await;

        let s = conn.current_song().await.unwrap().unwrap();
        assert_eq!(s.duration, Some(Duration::from_secs(300)));
    }

    // ----- transport: wire-byte assertions -----

    #[tokio::test]
    async fn play_sends_bare_play_on_wire() {
        let (server, client) = duplex(4096);
        let rx = spawn_capturing_exchange(server, b"OK MPD 0.23.5\n", b"OK\n");
        let mut conn = handshake_for_exchange(client).await;
        conn.play().await.unwrap();
        let captured = rx.await.unwrap();
        assert_eq!(captured, b"play\n");
    }

    #[tokio::test]
    async fn play_position_sends_quoted_position() {
        let (server, client) = duplex(4096);
        let rx = spawn_capturing_exchange(server, b"OK MPD 0.23.5\n", b"OK\n");
        let mut conn = handshake_for_exchange(client).await;
        conn.play_position(3).await.unwrap();
        let captured = rx.await.unwrap();
        assert_eq!(captured, b"play \"3\"\n");
    }

    #[tokio::test]
    async fn pause_true_sends_one() {
        let (server, client) = duplex(4096);
        let rx = spawn_capturing_exchange(server, b"OK MPD 0.23.5\n", b"OK\n");
        let mut conn = handshake_for_exchange(client).await;
        conn.pause(true).await.unwrap();
        let captured = rx.await.unwrap();
        assert_eq!(captured, b"pause \"1\"\n");
    }

    #[tokio::test]
    async fn pause_false_sends_zero() {
        let (server, client) = duplex(4096);
        let rx = spawn_capturing_exchange(server, b"OK MPD 0.23.5\n", b"OK\n");
        let mut conn = handshake_for_exchange(client).await;
        conn.pause(false).await.unwrap();
        let captured = rx.await.unwrap();
        assert_eq!(captured, b"pause \"0\"\n");
    }

    #[tokio::test]
    async fn stop_sends_bare_stop() {
        let (server, client) = duplex(4096);
        let rx = spawn_capturing_exchange(server, b"OK MPD 0.23.5\n", b"OK\n");
        let mut conn = handshake_for_exchange(client).await;
        conn.stop().await.unwrap();
        let captured = rx.await.unwrap();
        assert_eq!(captured, b"stop\n");
    }

    #[tokio::test]
    async fn next_sends_bare_next() {
        let (server, client) = duplex(4096);
        let rx = spawn_capturing_exchange(server, b"OK MPD 0.23.5\n", b"OK\n");
        let mut conn = handshake_for_exchange(client).await;
        conn.next().await.unwrap();
        let captured = rx.await.unwrap();
        assert_eq!(captured, b"next\n");
    }

    #[tokio::test]
    async fn previous_sends_bare_previous() {
        let (server, client) = duplex(4096);
        let rx = spawn_capturing_exchange(server, b"OK MPD 0.23.5\n", b"OK\n");
        let mut conn = handshake_for_exchange(client).await;
        conn.previous().await.unwrap();
        let captured = rx.await.unwrap();
        assert_eq!(captured, b"previous\n");
    }

    #[tokio::test]
    async fn seek_uses_seekcur_with_three_decimal_seconds() {
        let (server, client) = duplex(4096);
        let rx = spawn_capturing_exchange(server, b"OK MPD 0.23.5\n", b"OK\n");
        let mut conn = handshake_for_exchange(client).await;
        conn.seek(Duration::from_millis(12_500)).await.unwrap();
        let captured = rx.await.unwrap();
        assert_eq!(captured, b"seekcur \"12.500\"\n");
    }

    #[tokio::test]
    async fn seek_whole_seconds_has_three_decimal_places() {
        let (server, client) = duplex(4096);
        let rx = spawn_capturing_exchange(server, b"OK MPD 0.23.5\n", b"OK\n");
        let mut conn = handshake_for_exchange(client).await;
        conn.seek(Duration::from_secs(12)).await.unwrap();
        let captured = rx.await.unwrap();
        assert_eq!(captured, b"seekcur \"12.000\"\n");
    }

    #[tokio::test]
    async fn seek_relative_positive_uses_plus_prefix() {
        let (server, client) = duplex(4096);
        let rx = spawn_capturing_exchange(server, b"OK MPD 0.23.5\n", b"OK\n");
        let mut conn = handshake_for_exchange(client).await;
        conn.seek_relative(15_000).await.unwrap();
        let captured = rx.await.unwrap();
        assert_eq!(captured, b"seekcur \"+15.000\"\n");
    }

    #[tokio::test]
    async fn seek_relative_negative_uses_minus_prefix() {
        let (server, client) = duplex(4096);
        let rx = spawn_capturing_exchange(server, b"OK MPD 0.23.5\n", b"OK\n");
        let mut conn = handshake_for_exchange(client).await;
        conn.seek_relative(-5_000).await.unwrap();
        let captured = rx.await.unwrap();
        assert_eq!(captured, b"seekcur \"-5.000\"\n");
    }

    #[tokio::test]
    async fn seek_relative_sub_second_carries_millis_precision() {
        let (server, client) = duplex(4096);
        let rx = spawn_capturing_exchange(server, b"OK MPD 0.23.5\n", b"OK\n");
        let mut conn = handshake_for_exchange(client).await;
        conn.seek_relative(250).await.unwrap();
        let captured = rx.await.unwrap();
        assert_eq!(captured, b"seekcur \"+0.250\"\n");
    }

    #[tokio::test]
    async fn seek_relative_zero_is_no_op_on_wire() {
        let (server, client) = duplex(4096);
        let rx = spawn_capturing_exchange(server, b"OK MPD 0.23.5\n", b"OK\n");
        let mut conn = handshake_for_exchange(client).await;
        conn.seek_relative(0).await.unwrap();
        let captured = rx.await.unwrap();
        // MPD ACKs +0.000 cleanly; the wrapper still sends it so
        // the caller's intent is faithfully relayed.
        assert_eq!(captured, b"seekcur \"+0.000\"\n");
    }

    #[tokio::test]
    async fn set_volume_sends_setvol_with_quoted_value() {
        let (server, client) = duplex(4096);
        let rx = spawn_capturing_exchange(server, b"OK MPD 0.23.5\n", b"OK\n");
        let mut conn = handshake_for_exchange(client).await;
        conn.set_volume(50).await.unwrap();
        let captured = rx.await.unwrap();
        assert_eq!(captured, b"setvol \"50\"\n");
    }

    #[tokio::test]
    async fn ping_sends_bare_ping() {
        let (server, client) = duplex(4096);
        let rx = spawn_capturing_exchange(server, b"OK MPD 0.23.5\n", b"OK\n");
        let mut conn = handshake_for_exchange(client).await;
        conn.ping().await.unwrap();
        let captured = rx.await.unwrap();
        assert_eq!(captured, b"ping\n");
    }

    #[tokio::test]
    async fn set_crossfade_sends_crossfade_with_quoted_seconds() {
        let (server, client) = duplex(4096);
        let rx = spawn_capturing_exchange(server, b"OK MPD 0.23.5\n", b"OK\n");
        let mut conn = handshake_for_exchange(client).await;
        conn.set_crossfade(5).await.unwrap();
        let captured = rx.await.unwrap();
        assert_eq!(captured, b"crossfade \"5\"\n");
    }

    #[tokio::test]
    async fn set_crossfade_zero_disables() {
        let (server, client) = duplex(4096);
        let rx = spawn_capturing_exchange(server, b"OK MPD 0.23.5\n", b"OK\n");
        let mut conn = handshake_for_exchange(client).await;
        conn.set_crossfade(0).await.unwrap();
        let captured = rx.await.unwrap();
        assert_eq!(captured, b"crossfade \"0\"\n");
    }

    #[tokio::test]
    async fn set_single_true_engages_one() {
        let (server, client) = duplex(4096);
        let rx = spawn_capturing_exchange(server, b"OK MPD 0.23.5\n", b"OK\n");
        let mut conn = handshake_for_exchange(client).await;
        conn.set_single(true).await.unwrap();
        let captured = rx.await.unwrap();
        assert_eq!(captured, b"single \"1\"\n");
    }

    #[tokio::test]
    async fn set_single_false_engages_zero() {
        let (server, client) = duplex(4096);
        let rx = spawn_capturing_exchange(server, b"OK MPD 0.23.5\n", b"OK\n");
        let mut conn = handshake_for_exchange(client).await;
        conn.set_single(false).await.unwrap();
        let captured = rx.await.unwrap();
        assert_eq!(captured, b"single \"0\"\n");
    }

    #[tokio::test]
    async fn set_repeat_true_sends_repeat_one() {
        let (server, client) = duplex(4096);
        let rx = spawn_capturing_exchange(server, b"OK MPD 0.23.5\n", b"OK\n");
        let mut conn = handshake_for_exchange(client).await;
        conn.set_repeat(true).await.unwrap();
        let captured = rx.await.unwrap();
        assert_eq!(captured, b"repeat \"1\"\n");
    }

    #[tokio::test]
    async fn set_repeat_false_sends_repeat_zero() {
        let (server, client) = duplex(4096);
        let rx = spawn_capturing_exchange(server, b"OK MPD 0.23.5\n", b"OK\n");
        let mut conn = handshake_for_exchange(client).await;
        conn.set_repeat(false).await.unwrap();
        let captured = rx.await.unwrap();
        assert_eq!(captured, b"repeat \"0\"\n");
    }

    #[tokio::test]
    async fn set_random_true_sends_random_one() {
        let (server, client) = duplex(4096);
        let rx = spawn_capturing_exchange(server, b"OK MPD 0.23.5\n", b"OK\n");
        let mut conn = handshake_for_exchange(client).await;
        conn.set_random(true).await.unwrap();
        let captured = rx.await.unwrap();
        assert_eq!(captured, b"random \"1\"\n");
    }

    #[tokio::test]
    async fn set_consume_true_sends_consume_one() {
        let (server, client) = duplex(4096);
        let rx = spawn_capturing_exchange(server, b"OK MPD 0.23.5\n", b"OK\n");
        let mut conn = handshake_for_exchange(client).await;
        conn.set_consume(true).await.unwrap();
        let captured = rx.await.unwrap();
        assert_eq!(captured, b"consume \"1\"\n");
    }

    #[tokio::test]
    async fn status_parses_transport_modes() {
        // The emit_test_tone diagnostic captures these fields
        // before play + restores them after. Drift between
        // the parser + the wire format MPD emits would silently
        // leak diagnostic-only modes into the operator's
        // normal playback. Pin them.
        let (server, client) = duplex(4096);
        spawn_scripted_exchange(
            server,
            b"OK MPD 0.23.5\n",
            b"state: stop\nrepeat: 1\nrandom: 1\nsingle: 1\nconsume: 1\nxfade: 7\nOK\n",
        );
        let mut conn = handshake_for_exchange(client).await;
        let s = conn.status().await.unwrap();
        assert!(s.repeat, "repeat: 1 must parse to true");
        assert!(s.random, "random: 1 must parse to true");
        assert!(s.single, "single: 1 must parse to true");
        assert!(s.consume, "consume: 1 must parse to true");
        assert_eq!(s.crossfade_seconds, 7);
    }

    #[tokio::test]
    async fn status_defaults_transport_modes_to_off_when_absent() {
        // When MPD omits the fields, the diagnostic must capture
        // them as `false` / `0` so the restore path doesn't
        // accidentally engage them.
        let (server, client) = duplex(4096);
        spawn_scripted_exchange(
            server,
            b"OK MPD 0.23.5\n",
            b"state: stop\nOK\n",
        );
        let mut conn = handshake_for_exchange(client).await;
        let s = conn.status().await.unwrap();
        assert!(!s.repeat);
        assert!(!s.random);
        assert!(!s.single);
        assert!(!s.consume);
        assert_eq!(s.crossfade_seconds, 0);
    }

    #[tokio::test]
    async fn status_single_oneshot_parses_as_true() {
        // MPD 0.21+ `single: oneshot` is a binary-collapse case
        // for the diagnostic's restore path. The parser maps
        // any non-zero value to `single = true`; the restore
        // re-engages `single 1` (subset of oneshot semantics).
        let (server, client) = duplex(4096);
        spawn_scripted_exchange(
            server,
            b"OK MPD 0.23.5\n",
            b"state: stop\nsingle: oneshot\nOK\n",
        );
        let mut conn = handshake_for_exchange(client).await;
        let s = conn.status().await.unwrap();
        assert!(
            s.single,
            "single: oneshot must collapse to true for the \
             diagnostic's restore-binary capture"
        );
    }

    // ----- transport: ACK handling -----

    #[tokio::test]
    async fn transport_command_surfaces_ack_as_mpderror_ack() {
        let (server, client) = duplex(4096);
        spawn_scripted_exchange(
            server,
            b"OK MPD 0.23.5\n",
            b"ACK [2@0] {play} Bad song index\n",
        );
        let mut conn = handshake_for_exchange(client).await;

        let err = conn.play_position(999).await.unwrap_err();
        match err {
            MpdError::Ack {
                code,
                command,
                message,
                ..
            } => {
                assert_eq!(code, 2);
                assert_eq!(command, "play");
                assert_eq!(message, "Bad song index");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_volume_out_of_range_surfaces_ack_not_clamp() {
        // Caller passed a u8 above MPD's 0..=100 range. The layer
        // passes through rather than clamping; MPD's ACK is the
        // truthful failure surface.
        let (server, client) = duplex(4096);
        spawn_scripted_exchange(
            server,
            b"OK MPD 0.23.5\n",
            b"ACK [2@0] {setvol} Bad volume value\n",
        );
        let mut conn = handshake_for_exchange(client).await;

        let err = conn.set_volume(200).await.unwrap_err();
        assert!(matches!(err, MpdError::Ack { code: 2, .. }));
    }

    // ----- idle -----

    #[tokio::test]
    async fn idle_with_empty_subsystems_sends_bare_idle() {
        let (server, client) = duplex(4096);
        let rx = spawn_capturing_exchange(
            server,
            b"OK MPD 0.23.5\n",
            b"changed: player\nOK\n",
        );
        let mut conn = handshake_for_exchange(client).await;
        let _ = conn.idle(&[], Duration::from_millis(500)).await.unwrap();
        let captured = rx.await.unwrap();
        assert_eq!(captured, b"idle\n");
    }

    #[tokio::test]
    async fn idle_with_subsystems_sends_quoted_names() {
        let (server, client) = duplex(4096);
        let rx = spawn_capturing_exchange(
            server,
            b"OK MPD 0.23.5\n",
            b"changed: player\nOK\n",
        );
        let mut conn = handshake_for_exchange(client).await;
        let _ = conn
            .idle(
                &[IdleSubsystem::Player, IdleSubsystem::Mixer],
                Duration::from_millis(500),
            )
            .await
            .unwrap();
        let captured = rx.await.unwrap();
        assert_eq!(captured, b"idle \"player\" \"mixer\"\n");
    }

    #[tokio::test]
    async fn idle_returns_single_changed_subsystem() {
        let (server, client) = duplex(4096);
        spawn_scripted_exchange(
            server,
            b"OK MPD 0.23.5\n",
            b"changed: player\nOK\n",
        );
        let mut conn = handshake_for_exchange(client).await;
        let changed = conn.idle(&[], Duration::from_millis(500)).await.unwrap();
        assert_eq!(changed, vec![IdleSubsystem::Player]);
    }

    #[tokio::test]
    async fn idle_returns_multiple_changed_subsystems_in_order() {
        let (server, client) = duplex(4096);
        spawn_scripted_exchange(
            server,
            b"OK MPD 0.23.5\n",
            b"changed: player\nchanged: mixer\nchanged: playlist\nOK\n",
        );
        let mut conn = handshake_for_exchange(client).await;
        let changed = conn.idle(&[], Duration::from_millis(500)).await.unwrap();
        assert_eq!(
            changed,
            vec![
                IdleSubsystem::Player,
                IdleSubsystem::Mixer,
                IdleSubsystem::Playlist,
            ]
        );
    }

    #[tokio::test]
    async fn idle_immediate_ok_returns_empty_vec() {
        // MPD responded OK with no body. This happens after a
        // noidle cancellation from another connection; the idle
        // method surfaces it as "no changes observed" rather than
        // an error.
        let (server, client) = duplex(4096);
        spawn_scripted_exchange(server, b"OK MPD 0.23.5\n", b"OK\n");
        let mut conn = handshake_for_exchange(client).await;
        let changed = conn.idle(&[], Duration::from_millis(500)).await.unwrap();
        assert!(changed.is_empty());
    }

    #[tokio::test]
    async fn idle_preserves_unknown_subsystem_as_other_variant() {
        let (server, client) = duplex(4096);
        spawn_scripted_exchange(
            server,
            b"OK MPD 0.23.5\n",
            b"changed: future_thing\nOK\n",
        );
        let mut conn = handshake_for_exchange(client).await;
        let changed = conn.idle(&[], Duration::from_millis(500)).await.unwrap();
        assert_eq!(
            changed,
            vec![IdleSubsystem::Other("future_thing".to_string())]
        );
    }

    #[tokio::test]
    async fn idle_times_out_when_mpd_never_responds() {
        let (mut server, client) = duplex(4096);
        // Welcome arrives, then nothing. The server task holds the
        // connection open for the duration of the test, so there is
        // no EOF masquerade.
        let _hold = tokio::spawn(async move {
            server.write_all(b"OK MPD 0.23.5\n").await.unwrap();
            server.flush().await.unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let mut conn = handshake_for_exchange(client).await;

        let budget = Duration::from_millis(50);
        let err = conn.idle(&[], budget).await.unwrap_err();
        match err {
            MpdError::Timeout { operation, elapsed } => {
                // idle() re-wraps internal read timeouts so the
                // caller sees the idle-level operation name and a
                // wall-clock elapsed measured from idle's entry.
                assert_eq!(operation, "idle");
                // Wide bounds: the budget is 50ms, but the elapsed
                // value is wall-clock from entry and subject to
                // normal scheduler jitter. We check only that it is
                // roughly in range.
                assert!(
                    elapsed >= Duration::from_millis(30),
                    "idle returned too quickly: {elapsed:?}"
                );
                assert!(
                    elapsed < Duration::from_secs(1),
                    "idle waited far longer than budget: {elapsed:?}"
                );
            }
            other => panic!("expected idle timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn idle_surfaces_ack_as_mpderror_ack() {
        let (server, client) = duplex(4096);
        spawn_scripted_exchange(
            server,
            b"OK MPD 0.23.5\n",
            b"ACK [5@0] {idle} unknown subsystem\n",
        );
        let mut conn = handshake_for_exchange(client).await;

        let err = conn
            .idle(
                &[IdleSubsystem::Other("bogus".to_string())],
                Duration::from_millis(500),
            )
            .await
            .unwrap_err();
        match err {
            MpdError::Ack { code, command, .. } => {
                assert_eq!(code, 5);
                assert_eq!(command, "idle");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    // ----- real-transport integration -----

    #[tokio::test]
    async fn connect_works_over_real_tcp() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.write_all(b"OK MPD 0.23.5\n").await.unwrap();
            stream.flush().await.unwrap();
            // Keep open briefly for the handshake to complete.
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        let endpoint =
            MpdEndpoint::tcp(addr.ip().to_string(), addr.port()).unwrap();
        let conn =
            MpdConnection::connect_with_timeouts(endpoint, short_timeouts())
                .await
                .unwrap();
        assert_eq!(conn.version(), MpdVersion::new(0, 23, 5));
    }

    #[tokio::test]
    async fn connect_works_over_real_unix_socket() {
        let dir = std::env::temp_dir();
        let path: PathBuf =
            dir.join(format!("evo-mpd-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let listener = UnixListener::bind(&path).unwrap();
        let path_for_endpoint = path.clone();
        let path_for_cleanup = path.clone();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.write_all(b"OK MPD 0.23.5\n").await.unwrap();
            stream.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        let endpoint = MpdEndpoint::unix(path_for_endpoint).unwrap();
        let conn =
            MpdConnection::connect_with_timeouts(endpoint, short_timeouts())
                .await;
        let _ = server.await;
        let _ = std::fs::remove_file(&path_for_cleanup);

        let conn = conn.unwrap();
        assert_eq!(conn.version(), MpdVersion::new(0, 23, 5));
    }

    #[tokio::test]
    async fn connect_times_out_when_welcome_never_arrives() {
        let (server, client) = duplex(1024);
        // Hold server open, never write anything.
        let _hold = tokio::spawn(async move {
            let _keep = server;
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let (r, w) = tokio::io::split(client);

        let tight = ConnectTimeouts {
            connect: Duration::from_millis(500),
            welcome: Duration::from_millis(50),
            command: Duration::from_millis(500),
        };

        let err = handshake(Box::new(r), Box::new(w), fake_endpoint(), tight)
            .await
            .unwrap_err();
        match err {
            MpdError::Timeout { operation, .. } => {
                assert_eq!(operation, "welcome");
            }
            other => panic!("expected welcome timeout, got {other:?}"),
        }
    }

    // ----- field-projection unit tests -----

    #[test]
    fn parse_status_requires_state() {
        let fields = vec![Field {
            key: "volume".into(),
            value: "50".into(),
        }];
        let err = parse_status(&fields).unwrap_err();
        assert!(matches!(
            err,
            MpdError::Protocol(ProtocolError::MissingField { field: "state" })
        ));
    }

    #[test]
    fn parse_status_ignores_unknown_fields() {
        let fields = vec![
            Field {
                key: "state".into(),
                value: "play".into(),
            },
            Field {
                key: "unknown_field".into(),
                value: "value".into(),
            },
            Field {
                key: "xfade".into(),
                value: "2".into(),
            },
        ];
        let s = parse_status(&fields).unwrap();
        assert_eq!(s.state, PlayState::Playing);
    }

    #[test]
    fn parse_stats_extracts_songs_and_db_update() {
        let fields = vec![
            Field {
                key: "artists".into(),
                value: "62".into(),
            },
            Field {
                key: "albums".into(),
                value: "86".into(),
            },
            Field {
                key: "songs".into(),
                value: "1134".into(),
            },
            Field {
                key: "db_update".into(),
                value: "1717527921".into(),
            },
            Field {
                key: "uptime".into(),
                value: "3600".into(),
            },
        ];
        let s = parse_stats(&fields).unwrap();
        assert_eq!(s.artists, 62);
        assert_eq!(s.albums, 86);
        assert_eq!(s.songs, 1134);
        assert_eq!(s.db_update_unix_s, Some(1_717_527_921));
    }

    #[test]
    fn parse_stats_db_update_absent_yields_none() {
        let fields = vec![Field {
            key: "songs".into(),
            value: "0".into(),
        }];
        let s = parse_stats(&fields).unwrap();
        assert_eq!(s.songs, 0);
        assert_eq!(s.db_update_unix_s, None);
    }

    #[test]
    fn parse_stats_unparseable_songs_returns_protocol_error() {
        let fields = vec![Field {
            key: "songs".into(),
            value: "not-a-number".into(),
        }];
        let err = parse_stats(&fields).unwrap_err();
        assert!(
            matches!(err, MpdError::Protocol(ProtocolError::UnparseableField { field, .. }) if field == "songs"),
            "got: {err:?}"
        );
    }

    #[test]
    fn parse_current_song_missing_file_returns_none() {
        let fields = vec![Field {
            key: "Title".into(),
            value: "Something".into(),
        }];
        let s = parse_current_song(&fields).unwrap();
        assert!(s.is_none());
    }

    // ----- queue / playlist / sticker / library parser tests -----

    fn field(key: &str, value: &str) -> Field {
        Field {
            key: key.into(),
            value: value.into(),
        }
    }

    #[test]
    fn parse_queue_items_collects_each_item_by_pos_and_id() {
        let fields = vec![
            field("file", "INTERNAL/a.flac"),
            field("Title", "Track A"),
            field("Artist", "Someone"),
            field("Album", "Album A"),
            field("Time", "180"),
            field("duration", "180.500"),
            field("Pos", "0"),
            field("Id", "7"),
            field("file", "INTERNAL/b.flac"),
            field("Title", "Track B"),
            field("Pos", "1"),
            field("Id", "8"),
        ];
        let items = parse_queue_items(&fields);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, 7);
        assert_eq!(items[0].position, 0);
        assert_eq!(items[0].file_path, "INTERNAL/a.flac");
        assert_eq!(items[0].title.as_deref(), Some("Track A"));
        assert_eq!(items[0].duration, Some(Duration::from_millis(180_500)));
        assert_eq!(items[1].id, 8);
        assert_eq!(items[1].position, 1);
    }

    #[test]
    fn parse_queue_items_drops_items_missing_pos_or_id() {
        // Item without Pos/Id (defensive — should not break).
        let fields = vec![
            field("file", "INTERNAL/a.flac"),
            field("Title", "incomplete"),
        ];
        let items = parse_queue_items(&fields);
        assert!(items.is_empty());
    }

    #[test]
    fn parse_playlist_summaries_handles_optional_last_modified() {
        let fields = vec![
            field("playlist", "rock"),
            field("Last-Modified", "2025-01-02T03:04:05Z"),
            field("playlist", "jazz"),
        ];
        let summaries = parse_playlist_summaries(&fields);
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].name, "rock");
        assert_eq!(
            summaries[0].last_modified.as_deref(),
            Some("2025-01-02T03:04:05Z")
        );
        assert_eq!(summaries[1].name, "jazz");
        assert!(summaries[1].last_modified.is_none());
    }

    #[test]
    fn parse_playlist_entries_assigns_positions_by_parse_order() {
        let fields = vec![
            field("file", "INTERNAL/a.flac"),
            field("Title", "A"),
            field("file", "INTERNAL/b.flac"),
            field("Title", "B"),
            field("file", "INTERNAL/c.flac"),
        ];
        let entries = parse_playlist_entries(&fields);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].position, 0);
        assert_eq!(entries[0].title.as_deref(), Some("A"));
        assert_eq!(entries[1].position, 1);
        assert_eq!(entries[2].position, 2);
        assert!(entries[2].title.is_none());
    }

    #[test]
    fn sticker_parse_value_strips_named_prefix() {
        let v = sticker_parse_value("evo:available=1", "evo:available");
        assert_eq!(v.as_deref(), Some("1"));
    }

    #[test]
    fn sticker_parse_value_returns_none_on_mismatched_name() {
        let v = sticker_parse_value("rating=4", "evo:available");
        assert!(v.is_none());
    }

    #[test]
    fn sticker_split_pair_splits_on_first_equals() {
        let p = sticker_split_pair("rating=4=stars").unwrap();
        assert_eq!(p.0, "rating");
        assert_eq!(p.1, "4=stars");
    }

    #[test]
    fn parse_sticker_list_collects_every_sticker_line() {
        let fields = vec![
            field("sticker", "evo:available=1"),
            field("sticker", "rating=4"),
        ];
        let stickers = parse_sticker_list(&fields);
        assert_eq!(stickers.len(), 2);
        assert_eq!(stickers[0].name, "evo:available");
        assert_eq!(stickers[0].value, "1");
        assert_eq!(stickers[1].name, "rating");
    }

    #[test]
    fn parse_sticker_find_attaches_each_sticker_to_its_file() {
        let fields = vec![
            field("file", "INTERNAL/a.flac"),
            field("sticker", "evo:available=1"),
            field("file", "INTERNAL/b.flac"),
            field("sticker", "evo:available=0"),
        ];
        let matches = parse_sticker_find(&fields);
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].file_path, "INTERNAL/a.flac");
        assert_eq!(matches[0].sticker.value, "1");
        assert_eq!(matches[1].file_path, "INTERNAL/b.flac");
        assert_eq!(matches[1].sticker.value, "0");
    }

    #[test]
    fn parse_library_entries_distinguishes_kind_per_block() {
        let fields = vec![
            field("directory", "Artist A"),
            field("Last-Modified", "2025-01-01T00:00:00Z"),
            field("file", "Artist A/Album 1/01 Track.flac"),
            field("Title", "Opener"),
            field("duration", "210.000"),
            field("playlist", "Favourites"),
            field("Last-Modified", "2025-02-02T02:00:00Z"),
        ];
        let entries = parse_library_entries(&fields);
        assert_eq!(entries.len(), 3);
        match &entries[0] {
            MpdLibraryEntry::Directory {
                path,
                last_modified,
            } => {
                assert_eq!(path, "Artist A");
                assert!(last_modified.is_some());
            }
            other => panic!("expected directory, got {other:?}"),
        }
        match &entries[1] {
            MpdLibraryEntry::File {
                path,
                title,
                duration,
                ..
            } => {
                assert_eq!(path, "Artist A/Album 1/01 Track.flac");
                assert_eq!(title.as_deref(), Some("Opener"));
                assert_eq!(*duration, Some(Duration::from_secs(210)));
            }
            other => panic!("expected file, got {other:?}"),
        }
        match &entries[2] {
            MpdLibraryEntry::Playlist {
                path,
                last_modified,
            } => {
                assert_eq!(path, "Favourites");
                assert!(last_modified.is_some());
            }
            other => panic!("expected playlist, got {other:?}"),
        }
    }

    #[test]
    fn parse_mounts_pairs_each_alias_with_storage() {
        let fields = vec![
            field("mount", ""),
            field("storage", ""),
            field("mount", "nas"),
            field("storage", "smb://nas.local/music"),
        ];
        let mounts = parse_mounts(&fields);
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0].name, "");
        assert_eq!(mounts[0].storage, "");
        assert_eq!(mounts[1].name, "nas");
        assert_eq!(mounts[1].storage, "smb://nas.local/music");
    }

    #[test]
    fn parse_neighbors_collects_uri_and_name_pairs() {
        let fields = vec![
            field("neighbor", "smb://NAS"),
            field("name", "Home NAS"),
            field("neighbor", "upnp://uuid:abc.../"),
            field("name", "Living Room TV"),
        ];
        let neighbours = parse_neighbors(&fields);
        assert_eq!(neighbours.len(), 2);
        assert_eq!(neighbours[0].uri, "smb://NAS");
        assert_eq!(neighbours[0].name, "Home NAS");
        assert_eq!(neighbours[1].uri, "upnp://uuid:abc.../");
        assert_eq!(neighbours[1].name, "Living Room TV");
    }

    #[test]
    fn mpd_search_field_protocol_tokens_cover_every_variant() {
        let pairs = [
            (MpdSearchField::Any, "any"),
            (MpdSearchField::Artist, "artist"),
            (MpdSearchField::AlbumArtist, "albumartist"),
            (MpdSearchField::Album, "album"),
            (MpdSearchField::Title, "title"),
            (MpdSearchField::Genre, "genre"),
            (MpdSearchField::Composer, "composer"),
            (MpdSearchField::File, "file"),
            (MpdSearchField::Base, "base"),
            (MpdSearchField::Date, "date"),
        ];
        for (f, token) in pairs {
            assert_eq!(f.as_protocol_str(), token);
        }
    }
}
