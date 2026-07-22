// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Minimal MPD client for library enumeration.
//!
//! Two commands used by `browse_by_recording_type`:
//!
//! - `list "albumartist"` — distinct album-artists.
//! - `list "album" "albumartist" "<name>"` — distinct albums
//!   for that artist.
//!
//! Deliberately minimal — the full-featured MPD client lives
//! in `playback.mpd`. Duplicating it here would be net-negative;
//! this plugin only needs the two list commands. Line-oriented
//! text protocol is trivial to speak.
//!
//! Connection is short-lived — one connect + list per browse
//! call. MPD's connection cost on the loopback socket is
//! sub-millisecond, dominated by the `list album` scan time.

use std::io;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// Default MPD endpoint. Matches the standard MPD config that
/// ships with the audio reference distribution.
pub(crate) const DEFAULT_MPD_ADDR: &str = "127.0.0.1:6600";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// One live MPD connection.
pub(crate) struct MinimalMpd {
    reader: BufReader<TcpStream>,
}

impl MinimalMpd {
    /// Connect to MPD, consume the `OK MPD <version>` welcome
    /// line, and return the ready client.
    pub(crate) async fn connect(addr: &str) -> io::Result<Self> {
        let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "MPD connect timeout")
            })??;
        let mut reader = BufReader::new(stream);
        let mut welcome = String::new();
        timeout(CONNECT_TIMEOUT, reader.read_line(&mut welcome))
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "MPD welcome timeout")
            })??;
        if !welcome.starts_with("OK MPD") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected MPD welcome: {welcome:?}"),
            ));
        }
        Ok(Self { reader })
    }

    /// Send a line-oriented command and collect response lines
    /// until `OK` or `ACK` terminator.
    async fn command(&mut self, command_line: &str) -> io::Result<Vec<String>> {
        let line = format!("{command_line}\n");
        timeout(
            COMMAND_TIMEOUT,
            self.reader.get_mut().write_all(line.as_bytes()),
        )
        .await
        .map_err(|_| {
            io::Error::new(io::ErrorKind::TimedOut, "MPD write timeout")
        })??;
        let mut out: Vec<String> = Vec::new();
        loop {
            let mut buf = String::new();
            let n = timeout(COMMAND_TIMEOUT, self.reader.read_line(&mut buf))
                .await
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::TimedOut, "MPD read timeout")
                })??;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "MPD closed connection mid-response",
                ));
            }
            let line = buf.trim_end_matches(['\r', '\n']).to_string();
            if line == "OK" {
                break;
            }
            if line.starts_with("ACK") {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("MPD error: {line}"),
                ));
            }
            out.push(line);
        }
        Ok(out)
    }

    /// `list "<tag>"` — distinct values.
    async fn list_tag_lines(&mut self, tag: &str) -> io::Result<Vec<String>> {
        self.command(&format!("list \"{tag}\"")).await
    }

    /// `list "album" "albumartist" "<artist>"` — filtered.
    async fn list_album_for_artist_lines(
        &mut self,
        artist: &str,
    ) -> io::Result<Vec<String>> {
        // MPD's protocol requires escape of `"` and `\` inside
        // quoted arguments. Very rare in real album-artist
        // names but the escape is mandatory when it occurs
        // (a name like `AC\DC` or `The "Fifth Beatle"`).
        let escaped = artist.replace('\\', "\\\\").replace('"', "\\\"");
        self.command(&format!("list \"album\" \"albumartist\" \"{escaped}\""))
            .await
    }

    /// Enumerate every `(albumartist, album)` pair in the
    /// library. One connection, one traversal.
    ///
    /// Skips entries with empty artist or album (MPD's
    /// tag-less bucket surfaces as `<key>: <blank>` — dropping
    /// these here matches the `browse_by_artist` verb's
    /// discipline).
    pub(crate) async fn enumerate_albums(
        &mut self,
    ) -> io::Result<Vec<(String, String)>> {
        let artist_lines = self.list_tag_lines("albumartist").await?;
        let artists: Vec<String> = artist_lines
            .iter()
            .filter_map(|line| parse_field(line, "AlbumArtist"))
            .filter(|s| !s.is_empty())
            .collect();
        let mut pairs: Vec<(String, String)> = Vec::new();
        for artist in &artists {
            let album_lines = self.list_album_for_artist_lines(artist).await?;
            for line in &album_lines {
                if let Some(album) = parse_field(line, "Album") {
                    let trimmed = album.trim();
                    if !trimmed.is_empty() {
                        pairs.push((artist.clone(), trimmed.to_string()));
                    }
                }
            }
        }
        pairs.sort();
        pairs.dedup();
        Ok(pairs)
    }
}

/// Extract the value from a `Key: value` MPD response line
/// when `Key` matches. Case-preserving (MPD emits mixed-case
/// keys: `AlbumArtist`, `Album`, `Artist`).
fn parse_field(line: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}: ");
    line.strip_prefix(&prefix).map(|v| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_field_extracts_value() {
        assert_eq!(
            parse_field("AlbumArtist: Radiohead", "AlbumArtist"),
            Some("Radiohead".to_string())
        );
        assert_eq!(
            parse_field("Album: OK Computer", "Album"),
            Some("OK Computer".to_string())
        );
        assert_eq!(parse_field("Artist: Radiohead", "AlbumArtist"), None);
    }

    #[test]
    fn parse_field_handles_empty_value() {
        assert_eq!(
            parse_field("AlbumArtist: ", "AlbumArtist"),
            Some("".to_string())
        );
    }
}
