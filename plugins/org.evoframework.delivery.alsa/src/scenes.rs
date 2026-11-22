// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! A scene recalls the zones.
//!
//! A scene is a name and a snapshot of every zone — id, name,
//! membership and EQ — as they were when it was saved. It does
//! not hold the playing terminus, the options EQ or the logical
//! outputs. `scene.save` stores the zones as they are under a
//! name, replacing a scene of that name; `scene.list` answers
//! the names; `scene.remove` takes one away; `scene.recall`
//! replaces the live zones with a snapshot, which is the zone
//! registry's work (`Zones::recall`), not this module's.
//!
//! A name is trimmed, non-empty, at most [`NAME_MAX_CHARS`]
//! characters, and unique among scenes without regard to case.
//! At most [`SCENE_CAP`] scenes: a save that would make one more
//! is refused and nothing is written.
//!
//! Scenes survive a restart in `scenes.toml` under the plugin's
//! state directory. A missing file is no scenes; a file that
//! does not parse is no scenes, and the live zones stay.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::zones::Zone;

/// State-file version.
pub(crate) const SCENES_VERSION: u32 = 1;

/// Longest name accepted, in characters.
pub(crate) const NAME_MAX_CHARS: usize = 64;

/// How many scenes may exist.
pub(crate) const SCENE_CAP: usize = 32;

/// Why a verb was refused.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Refusal {
    /// No scene has this name.
    UnknownScene(String),
    /// The name is empty once trimmed.
    EmptyName,
    /// The name is longer than [`NAME_MAX_CHARS`].
    NameTooLong(usize),
    /// A new scene would be one past [`SCENE_CAP`].
    TooManyScenes,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::UnknownScene(name) => write!(f, "unknown scene {name:?}"),
            Refusal::EmptyName => write!(f, "a scene needs a name"),
            Refusal::NameTooLong(n) => {
                write!(f, "name is {n} characters; at most {NAME_MAX_CHARS}")
            }
            Refusal::TooManyScenes => write!(
                f,
                "there are already {SCENE_CAP} scenes; remove one first"
            ),
        }
    }
}

/// One scene, as persisted and as answered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Scene {
    /// The operator's name for it.
    pub(crate) name: String,
    /// Every zone as it was when the scene was saved.
    #[serde(default)]
    pub(crate) zones: Vec<Zone>,
}

/// The persisted registry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PersistedScenes {
    /// State-file version.
    #[serde(default = "state_version")]
    pub(crate) v: u32,
    /// Every scene, in the order first saved.
    #[serde(default)]
    pub(crate) scenes: Vec<Scene>,
}

fn state_version() -> u32 {
    SCENES_VERSION
}

impl Default for PersistedScenes {
    fn default() -> Self {
        Self {
            v: SCENES_VERSION,
            scenes: Vec::new(),
        }
    }
}

/// Trim and bound a name.
fn clean_name(name: &str) -> Result<String, Refusal> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Refusal::EmptyName);
    }
    let n = name.chars().count();
    if n > NAME_MAX_CHARS {
        return Err(Refusal::NameTooLong(n));
    }
    Ok(name.to_string())
}

/// The registry. Pure.
#[derive(Debug, Clone, Default)]
pub(crate) struct Scenes {
    state: PersistedScenes,
}

impl Scenes {
    /// A registry holding what was persisted.
    pub(crate) fn from_persisted(state: PersistedScenes) -> Self {
        Self { state }
    }

    /// What to persist.
    pub(crate) fn persisted(&self) -> &PersistedScenes {
        &self.state
    }

    fn position(&self, name: &str) -> Option<usize> {
        let wanted = name.trim().to_lowercase();
        self.state
            .scenes
            .iter()
            .position(|s| s.name.to_lowercase() == wanted)
    }

    /// `scene.save`: the zones as they are, under this name. A
    /// scene of that name, without regard to case, is replaced
    /// in place and takes the new spelling; otherwise a new scene
    /// is added unless there are already [`SCENE_CAP`].
    pub(crate) fn save(
        &mut self,
        name: &str,
        zones: Vec<Zone>,
    ) -> Result<Scene, Refusal> {
        let name = clean_name(name)?;
        let scene = Scene { name, zones };
        match self.position(&scene.name) {
            Some(at) => {
                self.state.scenes[at] = scene.clone();
            }
            None => {
                if self.state.scenes.len() >= SCENE_CAP {
                    return Err(Refusal::TooManyScenes);
                }
                self.state.scenes.push(scene.clone());
            }
        }
        Ok(scene)
    }

    /// `scene.list`: the names, in the order first saved.
    pub(crate) fn names(&self) -> Vec<String> {
        self.state.scenes.iter().map(|s| s.name.clone()).collect()
    }

    /// The scene of this name, for `scene.recall`.
    pub(crate) fn get(&self, name: &str) -> Result<&Scene, Refusal> {
        self.position(name)
            .map(|at| &self.state.scenes[at])
            .ok_or_else(|| Refusal::UnknownScene(name.trim().to_string()))
    }

    /// `scene.remove`: the scene taken away.
    pub(crate) fn remove(&mut self, name: &str) -> Result<Scene, Refusal> {
        let at = self
            .position(name)
            .ok_or_else(|| Refusal::UnknownScene(name.trim().to_string()))?;
        Ok(self.state.scenes.remove(at))
    }
}

/// The state file under the plugin's state directory.
#[derive(Debug, Clone)]
pub(crate) struct SceneStore {
    path: PathBuf,
}

impl SceneStore {
    /// File name under the state directory.
    pub(crate) const FILE_NAME: &'static str = "scenes.toml";

    /// A store at this path.
    pub(crate) fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// The path.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Read the file. Absent is no scenes; unreadable or malformed
    /// is no scenes with a warning, and nothing else moves.
    pub(crate) async fn load(&self) -> PersistedScenes {
        match tokio::fs::read_to_string(&self.path).await {
            Ok(body) => match toml::from_str::<PersistedScenes>(&body) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        plugin = crate::PLUGIN_NAME,
                        path = %self.path.display(),
                        error = %e,
                        "scenes file is malformed; starting with no scenes"
                    );
                    PersistedScenes::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                PersistedScenes::default()
            }
            Err(e) => {
                tracing::warn!(
                    plugin = crate::PLUGIN_NAME,
                    path = %self.path.display(),
                    error = %e,
                    "scenes file could not be read; starting with no scenes"
                );
                PersistedScenes::default()
            }
        }
    }

    /// Write the file atomically (temp file beside it, then
    /// rename).
    pub(crate) async fn save(
        &self,
        persisted: &PersistedScenes,
    ) -> std::io::Result<()> {
        let body = toml::to_string(persisted).map_err(std::io::Error::other)?;
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut tmp = self.path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        tokio::fs::write(&tmp, body.as_bytes()).await?;
        tokio::fs::rename(&tmp, &self.path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zones::ZoneEq;

    fn zone(id: &str) -> Zone {
        Zone {
            id: id.to_string(),
            name: id.to_string(),
            logical_output_ids: vec!["logical:1".to_string()],
            eq: ZoneEq::default(),
        }
    }

    /// Save keeps the trimmed name and the zones; a second save
    /// of the same name, in any case, replaces in place and takes
    /// the new spelling; the list is in first-saved order.
    #[test]
    fn save_replaces_by_name_without_regard_to_case() {
        let mut s = Scenes::default();
        let a = s.save("  Evening ", vec![zone("zone:1")]).unwrap();
        assert_eq!(a.name, "Evening");
        assert_eq!(a.zones.len(), 1);
        s.save("Morning", vec![]).unwrap();
        let b = s
            .save("EVENING", vec![zone("zone:1"), zone("zone:2")])
            .unwrap();
        assert_eq!(b.zones.len(), 2);
        assert_eq!(
            s.names(),
            vec!["EVENING", "Morning"],
            "replaced in place, new spelling"
        );
        assert_eq!(s.get("evening").unwrap().zones.len(), 2);
    }

    /// Empty and over-long names are refused; the 33rd scene is
    /// refused and nothing changes; replacing at the cap is fine.
    #[test]
    fn names_are_bounded_and_there_are_at_most_thirty_two() {
        let mut s = Scenes::default();
        assert_eq!(s.save("   ", vec![]), Err(Refusal::EmptyName));
        let long = "x".repeat(NAME_MAX_CHARS + 1);
        assert_eq!(
            s.save(&long, vec![]),
            Err(Refusal::NameTooLong(NAME_MAX_CHARS + 1))
        );
        for i in 0..SCENE_CAP {
            s.save(&format!("Scene {i}"), vec![]).unwrap();
        }
        let before = s.persisted().clone();
        assert_eq!(s.save("One more", vec![]), Err(Refusal::TooManyScenes));
        assert_eq!(s.persisted(), &before, "nothing written");
        assert!(
            s.save("scene 3", vec![zone("zone:9")]).is_ok(),
            "a replace at the cap"
        );
        assert_eq!(s.names().len(), SCENE_CAP);
    }

    /// Remove answers the scene and refuses an unknown name; get
    /// refuses an unknown name.
    #[test]
    fn remove_takes_one_scene_away() {
        let mut s = Scenes::default();
        s.save("Evening", vec![zone("zone:1")]).unwrap();
        s.save("Morning", vec![]).unwrap();
        let gone = s.remove(" evening ").unwrap();
        assert_eq!(gone.name, "Evening");
        assert_eq!(s.names(), vec!["Morning"]);
        assert_eq!(
            s.remove("Evening"),
            Err(Refusal::UnknownScene("Evening".to_string()))
        );
        assert_eq!(
            s.get("Night").err(),
            Some(Refusal::UnknownScene("Night".to_string()))
        );
    }

    /// The store round-trips; absent and malformed are no scenes.
    #[tokio::test]
    async fn the_store_round_trips_and_tolerates_a_bad_file() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            SceneStore::at(dir.path().join("sub").join(SceneStore::FILE_NAME));
        assert_eq!(store.load().await, PersistedScenes::default());
        let mut s = Scenes::default();
        s.save("Evening", vec![zone("zone:1")]).unwrap();
        store.save(s.persisted()).await.unwrap();
        assert_eq!(&store.load().await, s.persisted());
        assert!(!store.path().with_extension("toml.tmp").exists());
        tokio::fs::write(store.path(), "v = \"nope\"\n[[scenes")
            .await
            .unwrap();
        assert_eq!(store.load().await, PersistedScenes::default());
    }
}
