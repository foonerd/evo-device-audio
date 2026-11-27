// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! What a stream is called, remembered.
//!
//! MPD reads no tag from an HTTP stream. A DLNA track is named only by the
//! tags this plugin puts on its queue row with `addtagid`, taken from the
//! server's own description of it. The name lives on that row and nowhere
//! else: the moment the bare URL is added to the queue again — the last
//! queue coming back, a stored playlist loading, which hold URLs and
//! nothing more — the row is untitled and the glass shows the URL.
//!
//! So the plugin remembers, by URL, the tags a stream was last known by,
//! and puts them back whenever the URL is added. Only what a server or the
//! queue itself said is kept; a title is never made up.
//!
//! One file under the plugin's state directory, written whole and renamed
//! into place, bounded in size. Losing it costs titles, never a queue.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::PLUGIN_NAME;

/// File-format version.
pub(crate) const STREAM_TAGS_VERSION: u32 = 1;

/// Most streams remembered. Past this the longest-unseen go first.
pub(crate) const MAX_STREAMS: usize = 4096;

/// Longest URL kept, and longest tag value, in bytes. A longer one is not
/// remembered rather than cut: a cut URL names nothing.
const MAX_URI_BYTES: usize = 2048;
const MAX_VALUE_BYTES: usize = 1024;

/// How long a change waits before it is written, so an album of a hundred
/// tracks is one write.
const WRITE_DELAY: Duration = Duration::from_secs(2);

/// The tags a stream is known by. Each is what a server or the queue said.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StreamTags {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) genre: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) composer: Option<String>,
}

impl StreamTags {
    /// Nothing is known.
    pub(crate) fn is_empty(&self) -> bool {
        *self == StreamTags::default()
    }

    /// Keep only what is worth and safe to remember: non-empty values of
    /// bounded length.
    fn cleaned(self) -> StreamTags {
        let keep = |v: Option<String>| {
            v.filter(|s| !s.is_empty() && s.len() <= MAX_VALUE_BYTES)
        };
        StreamTags {
            title: keep(self.title),
            artist: keep(self.artist),
            album: keep(self.album),
            genre: keep(self.genre),
            date: keep(self.date),
            composer: keep(self.composer),
        }
    }

    /// `self` with every field `newer` states put over it.
    fn updated_by(&self, newer: &StreamTags) -> StreamTags {
        let pick = |new: &Option<String>, old: &Option<String>| {
            new.clone().or_else(|| old.clone())
        };
        StreamTags {
            title: pick(&newer.title, &self.title),
            artist: pick(&newer.artist, &self.artist),
            album: pick(&newer.album, &self.album),
            genre: pick(&newer.genre, &self.genre),
            date: pick(&newer.date, &self.date),
            composer: pick(&newer.composer, &self.composer),
        }
    }
}

/// Whether `uri` names a stream this plugin should keep tags for: a network
/// URL, not a library path.
pub(crate) fn is_stream(uri: &str) -> bool {
    uri.contains("://") && uri.len() <= MAX_URI_BYTES
}

#[derive(Debug, Serialize, Deserialize)]
struct FileEntry {
    uri: String,
    #[serde(flatten)]
    tags: StreamTags,
}

#[derive(Debug, Serialize, Deserialize)]
struct File {
    v: u32,
    /// Oldest first: the order is the age.
    entries: Vec<FileEntry>,
}

#[derive(Default)]
struct Memory {
    /// URL → (sequence of last sighting, tags).
    streams: HashMap<String, (u64, StreamTags)>,
    seq: u64,
    dirty: bool,
    write_pending: bool,
}

/// The remembered tags. Cloning shares them.
#[derive(Clone, Default)]
pub(crate) struct StreamTagStore {
    path: Option<PathBuf>,
    memory: Arc<Mutex<Memory>>,
}

impl StreamTagStore {
    /// A store kept in `path`, with what is there read now. A file that
    /// cannot be read or parsed is a store that knows nothing.
    pub(crate) fn at(path: PathBuf) -> Self {
        let store = Self {
            path: Some(path.clone()),
            memory: Arc::default(),
        };
        let body = match std::fs::read_to_string(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return store;
            }
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    path = %path.display(),
                    error = %e,
                    "stream tags unreadable; starting with none"
                );
                return store;
            }
        };
        match toml::from_str::<File>(&body) {
            Ok(file) if file.v == STREAM_TAGS_VERSION => {
                let mut m = store.lock();
                for entry in file.entries.into_iter().take(MAX_STREAMS) {
                    if !is_stream(&entry.uri) {
                        continue;
                    }
                    m.seq += 1;
                    let seq = m.seq;
                    m.streams.insert(entry.uri, (seq, entry.tags.cleaned()));
                }
            }
            Ok(file) => tracing::warn!(
                plugin = PLUGIN_NAME,
                version = file.v,
                "stream tags are from another format version; starting \
                 with none"
            ),
            Err(e) => tracing::warn!(
                plugin = PLUGIN_NAME,
                path = %path.display(),
                error = %e,
                "stream tags do not parse; starting with none"
            ),
        }
        store
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Memory> {
        self.memory.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What `uri` is known by, if anything.
    pub(crate) fn get(&self, uri: &str) -> Option<StreamTags> {
        if !is_stream(uri) {
            return None;
        }
        self.lock()
            .streams
            .get(uri)
            .map(|(_, tags)| tags.clone())
            .filter(|tags| !tags.is_empty())
    }

    /// Remember what `uri` is known by. Fields `tags` states replace what
    /// was kept; fields it does not state are left as they were.
    pub(crate) fn remember(&self, uri: &str, tags: StreamTags) {
        if !is_stream(uri) {
            return;
        }
        let tags = tags.cleaned();
        if tags.is_empty() {
            return;
        }
        {
            let mut m = self.lock();
            m.seq += 1;
            let seq = m.seq;
            let merged = match m.streams.get(uri) {
                Some((_, old)) => old.updated_by(&tags),
                None => tags,
            };
            let changed =
                m.streams.get(uri).map(|(_, old)| old) != Some(&merged);
            m.streams.insert(uri.to_string(), (seq, merged));
            if changed {
                m.dirty = true;
            }
            if m.streams.len() > MAX_STREAMS {
                let keep = MAX_STREAMS - MAX_STREAMS / 10;
                let mut by_age: Vec<(u64, String)> = m
                    .streams
                    .iter()
                    .map(|(u, (s, _))| (*s, u.clone()))
                    .collect();
                by_age.sort_unstable();
                let drop = by_age.len() - keep;
                for (_, uri) in by_age.into_iter().take(drop) {
                    m.streams.remove(&uri);
                }
                m.dirty = true;
            }
        }
        self.schedule_write();
    }

    fn schedule_write(&self) {
        if self.path.is_none() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        {
            let mut m = self.lock();
            if !m.dirty || m.write_pending {
                return;
            }
            m.write_pending = true;
        }
        let store = self.clone();
        runtime.spawn(async move {
            tokio::time::sleep(WRITE_DELAY).await;
            store.lock().write_pending = false;
            store.flush().await;
        });
    }

    /// Write what changed, now. Called when the plugin unloads, so a change
    /// is not lost to the delay.
    pub(crate) async fn flush(&self) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        let body = {
            let mut m = self.lock();
            if !m.dirty {
                return;
            }
            let mut by_age: Vec<(&u64, &String, &StreamTags)> =
                m.streams.iter().map(|(u, (s, t))| (s, u, t)).collect();
            by_age.sort_unstable_by_key(|(s, _, _)| **s);
            let file = File {
                v: STREAM_TAGS_VERSION,
                entries: by_age
                    .into_iter()
                    .map(|(_, u, t)| FileEntry {
                        uri: u.clone(),
                        tags: t.clone(),
                    })
                    .collect(),
            };
            let body = match toml::to_string(&file) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(plugin = PLUGIN_NAME, error = %e, "stream tags not serialised");
                    return;
                }
            };
            m.dirty = false;
            body
        };
        let written = async {
            let parent = path.parent().ok_or("no parent directory")?;
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| e.to_string())?;
            let staging = parent.join(".stream-tags.toml.tmp");
            tokio::fs::write(&staging, body)
                .await
                .map_err(|e| e.to_string())?;
            tokio::fs::rename(&staging, path)
                .await
                .map_err(|e| e.to_string())
        }
        .await;
        if let Err(e) = written {
            self.lock().dirty = true;
            tracing::warn!(
                plugin = PLUGIN_NAME,
                path = %path.display(),
                error = %e,
                "stream tags not written; they are kept in memory"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "http://192.0.2.9:32469/object/aaa/file.mp3";

    fn tags(title: &str) -> StreamTags {
        StreamTags {
            title: Some(title.to_string()),
            ..StreamTags::default()
        }
    }

    #[test]
    fn what_is_remembered_is_returned_for_that_url_only() {
        let s = StreamTagStore::default();
        s.remember(URL, tags("Song"));
        assert_eq!(s.get(URL), Some(tags("Song")));
        assert_eq!(s.get("http://192.0.2.9:32469/object/other"), None);
    }

    #[test]
    fn a_library_path_is_never_remembered() {
        let s = StreamTagStore::default();
        s.remember("INTERNAL/Adele/a.flac", tags("Hello"));
        assert_eq!(s.get("INTERNAL/Adele/a.flac"), None);
    }

    #[test]
    fn nothing_known_is_not_remembered_and_nothing_is_made_up() {
        let s = StreamTagStore::default();
        s.remember(URL, StreamTags::default());
        s.remember(URL, tags(""));
        assert_eq!(s.get(URL), None);
        assert!(
            s.lock().streams.is_empty() && !s.lock().dirty,
            "an empty statement takes no room and costs no write"
        );
    }

    #[test]
    fn a_newer_statement_replaces_what_it_says_and_keeps_the_rest() {
        let s = StreamTagStore::default();
        s.remember(
            URL,
            StreamTags {
                title: Some("Old".into()),
                artist: Some("Band".into()),
                ..StreamTags::default()
            },
        );
        s.remember(URL, tags("New"));
        let got = s.get(URL).unwrap();
        assert_eq!(got.title.as_deref(), Some("New"));
        assert_eq!(got.artist.as_deref(), Some("Band"));
    }

    #[test]
    fn oversized_values_and_urls_are_not_kept() {
        let s = StreamTagStore::default();
        s.remember(URL, tags(&"x".repeat(MAX_VALUE_BYTES + 1)));
        assert_eq!(s.get(URL), None, "a cut title is not a title");
        let long = format!("http://h/{}", "a".repeat(MAX_URI_BYTES));
        s.remember(&long, tags("T"));
        assert_eq!(s.get(&long), None);
    }

    #[test]
    fn the_longest_unseen_go_first_when_the_memory_is_full() {
        let s = StreamTagStore::default();
        for n in 0..=MAX_STREAMS {
            s.remember(&format!("http://h/{n}"), tags("t"));
        }
        assert!(s.lock().streams.len() <= MAX_STREAMS);
        assert_eq!(s.get("http://h/0"), None, "the oldest went");
        assert!(s.get(&format!("http://h/{MAX_STREAMS}")).is_some());
    }

    #[tokio::test]
    async fn it_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stream-tags.toml");
        let s = StreamTagStore::at(path.clone());
        s.remember(
            URL,
            StreamTags {
                title: Some("Kept".into()),
                album: Some("Disc".into()),
                genre: Some("Rock".into()),
                ..StreamTags::default()
            },
        );
        s.flush().await;
        assert!(
            !dir.path().join(".stream-tags.toml.tmp").exists(),
            "renamed into place"
        );
        let again = StreamTagStore::at(path);
        let got = again.get(URL).expect("remembered across a restart");
        assert_eq!(got.title.as_deref(), Some("Kept"));
        assert_eq!(got.album.as_deref(), Some("Disc"));
        assert_eq!(got.genre.as_deref(), Some("Rock"));
    }

    #[tokio::test]
    async fn a_change_is_written_after_a_short_delay_without_a_flush() {
        tokio::time::pause();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stream-tags.toml");
        let s = StreamTagStore::at(path.clone());
        s.remember(URL, tags("Soon"));
        assert!(!path.exists(), "not written the instant it changes");
        tokio::time::sleep(WRITE_DELAY + Duration::from_millis(50)).await;
        for _ in 0..50 {
            if path.exists() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(path.exists(), "written once the delay has passed");
    }

    #[tokio::test]
    async fn a_bad_file_is_a_store_that_knows_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stream-tags.toml");
        std::fs::write(&path, "this is not toml = [").unwrap();
        assert_eq!(StreamTagStore::at(path.clone()).get(URL), None);
        std::fs::write(&path, "v = 99\nentries = []\n").unwrap();
        assert_eq!(StreamTagStore::at(path).get(URL), None);
    }

    #[tokio::test]
    async fn nothing_is_written_when_nothing_changed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stream-tags.toml");
        let s = StreamTagStore::at(path.clone());
        s.flush().await;
        assert!(!path.exists());
    }
}
