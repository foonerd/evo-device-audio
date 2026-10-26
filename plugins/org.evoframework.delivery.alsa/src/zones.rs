// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! A zone is a name over one or more logical outputs.
//!
//! A zone is not an ALSA device and not an EQ. It has a stable id
//! the plugin mints (`zone:<n>`, never reused), a name, and one or
//! more logical outputs — the `logical:<n>` ids
//! `delivery.list_logical_outputs` returns. `zone.add` makes one,
//! `zone.update` renames it or changes its outputs without
//! changing its id, `zone.remove` takes it away, and `zone.list`
//! reads them all back in id order.
//!
//! A name is trimmed, non-empty, at most [`NAME_MAX_CHARS`]
//! characters, and unique among zones case-insensitively. A zone
//! holds at least one logical output; a list with none is
//! refused. Every id in the list must be a logical output the
//! plugin has, named once, and not already in another zone: a
//! logical output sits in one zone at most. Removing a zone
//! leaves its logical outputs and the physical outputs as they
//! are.
//!
//! Zones survive a restart in `zones.toml` under the plugin's
//! state directory. No subject is published. No EQ is attached
//! to a zone and no scene is stored.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// State-file version.
pub(crate) const ZONES_VERSION: u32 = 1;

/// Longest name accepted, in characters.
pub(crate) const NAME_MAX_CHARS: usize = 64;

/// Prefix of every zone id.
pub(crate) const ID_PREFIX: &str = "zone:";

/// Why a verb was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// No zone has this id.
    UnknownZone(String),
    /// The name is empty once trimmed.
    EmptyName,
    /// The name is longer than [`NAME_MAX_CHARS`].
    NameTooLong(usize),
    /// Another zone already carries this name, case-insensitively.
    NameTaken {
        /// The name asked for.
        name: String,
        /// The zone that has it.
        held_by: String,
    },
    /// A zone holds at least one logical output.
    NoLogicalOutputs,
    /// The id is not a logical output the plugin has.
    UnknownLogicalOutput(String),
    /// The same logical output named twice in one list.
    DuplicateLogicalOutput(String),
    /// The logical output already sits in another zone.
    AlreadyZoned {
        /// The logical output asked for.
        logical_output_id: String,
        /// The zone it sits in.
        zone: String,
    },
    /// An update that names neither a new name nor new outputs.
    NothingToUpdate,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::UnknownZone(id) => write!(f, "unknown zone {id:?}"),
            Refusal::EmptyName => write!(f, "a zone needs a name"),
            Refusal::NameTooLong(n) => {
                write!(f, "name is {n} characters; at most {NAME_MAX_CHARS}")
            }
            Refusal::NameTaken { name, held_by } => {
                write!(f, "name {name:?} is already carried by {held_by:?}")
            }
            Refusal::NoLogicalOutputs => {
                write!(f, "a zone holds at least one logical output")
            }
            Refusal::UnknownLogicalOutput(id) => write!(
                f,
                "unknown logical output {id:?}: not one \
                 delivery.list_logical_outputs returns"
            ),
            Refusal::DuplicateLogicalOutput(id) => {
                write!(f, "logical output {id:?} is named twice")
            }
            Refusal::AlreadyZoned {
                logical_output_id,
                zone,
            } => write!(
                f,
                "logical output {logical_output_id:?} already sits in \
                 zone {zone:?}; a logical output is in one zone at most"
            ),
            Refusal::NothingToUpdate => write!(
                f,
                "update names neither a new name nor new logical outputs"
            ),
        }
    }
}

/// One zone, as persisted and as answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Zone {
    /// Stable id the plugin minted: `zone:<n>`.
    pub(crate) id: String,
    /// The operator's name for it.
    pub(crate) name: String,
    /// The logical outputs it holds, in the order given.
    pub(crate) logical_output_ids: Vec<String>,
}

/// The persisted registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PersistedZones {
    /// State-file version.
    #[serde(default = "state_version")]
    pub(crate) v: u32,
    /// The next number to mint; never reused after a remove.
    #[serde(default = "first_id")]
    pub(crate) next_id: u64,
    /// Every zone, in creation order.
    #[serde(default)]
    pub(crate) zones: Vec<Zone>,
}

fn state_version() -> u32 {
    ZONES_VERSION
}

fn first_id() -> u64 {
    1
}

impl Default for PersistedZones {
    fn default() -> Self {
        Self {
            v: ZONES_VERSION,
            next_id: first_id(),
            zones: Vec::new(),
        }
    }
}

/// The registry. Pure: every method takes the logical outputs the
/// plugin has and returns the zone the verb answers with.
#[derive(Debug, Clone, Default)]
pub(crate) struct Zones {
    state: PersistedZones,
}

/// The number in a `zone:<n>` id; an id without one sorts last,
/// by its text.
fn id_number(id: &str) -> (u64, String) {
    match id
        .strip_prefix(ID_PREFIX)
        .and_then(|n| n.parse::<u64>().ok())
    {
        Some(n) => (n, String::new()),
        None => (u64::MAX, id.to_string()),
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

impl Zones {
    /// A registry holding what was persisted.
    pub(crate) fn from_persisted(state: PersistedZones) -> Self {
        Self { state }
    }

    /// What to persist.
    pub(crate) fn persisted(&self) -> &PersistedZones {
        &self.state
    }

    /// The registry's rows, in creation order.
    #[cfg(test)]
    pub(crate) fn rows(&self) -> &[Zone] {
        &self.state.zones
    }

    /// `zone.list`: every zone, in id order. Reads only.
    pub(crate) fn list(&self) -> Vec<Zone> {
        let mut rows = self.state.zones.clone();
        rows.sort_by_key(|z| id_number(&z.id));
        rows
    }

    fn position(&self, id: &str) -> Result<usize, Refusal> {
        self.state
            .zones
            .iter()
            .position(|z| z.id == id)
            .ok_or_else(|| Refusal::UnknownZone(id.to_string()))
    }

    /// Refuse a name another zone carries, case-insensitively;
    /// `except` is the zone being renamed.
    fn name_free(
        &self,
        name: &str,
        except: Option<&str>,
    ) -> Result<(), Refusal> {
        let wanted = name.to_lowercase();
        match self.state.zones.iter().find(|z| {
            Some(z.id.as_str()) != except && z.name.to_lowercase() == wanted
        }) {
            Some(holder) => Err(Refusal::NameTaken {
                name: name.to_string(),
                held_by: holder.id.clone(),
            }),
            None => Ok(()),
        }
    }

    /// Check a list of logical outputs for a zone: non-empty, each
    /// one the plugin has, named once, and in no other zone than
    /// `except`.
    fn outputs_free(
        &self,
        known_logical: &[String],
        ids: &[String],
        except: Option<&str>,
    ) -> Result<(), Refusal> {
        if ids.is_empty() {
            return Err(Refusal::NoLogicalOutputs);
        }
        for (i, id) in ids.iter().enumerate() {
            if !known_logical.iter().any(|k| k == id) {
                return Err(Refusal::UnknownLogicalOutput(id.clone()));
            }
            if ids[..i].contains(id) {
                return Err(Refusal::DuplicateLogicalOutput(id.clone()));
            }
            if let Some(holder) = self.state.zones.iter().find(|z| {
                Some(z.id.as_str()) != except
                    && z.logical_output_ids.contains(id)
            }) {
                return Err(Refusal::AlreadyZoned {
                    logical_output_id: id.clone(),
                    zone: holder.id.clone(),
                });
            }
        }
        Ok(())
    }

    /// `zone.add`: a new zone with a minted id.
    pub(crate) fn add(
        &mut self,
        known_logical: &[String],
        name: &str,
        logical_output_ids: &[String],
    ) -> Result<Zone, Refusal> {
        let name = clean_name(name)?;
        self.name_free(&name, None)?;
        self.outputs_free(known_logical, logical_output_ids, None)?;
        let id = format!("{ID_PREFIX}{}", self.state.next_id);
        self.state.next_id += 1;
        let zone = Zone {
            id,
            name,
            logical_output_ids: logical_output_ids.to_vec(),
        };
        self.state.zones.push(zone.clone());
        Ok(zone)
    }

    /// `zone.update`: a new name, a new list of logical outputs,
    /// or both; the id stays. A refused update changes nothing.
    pub(crate) fn update(
        &mut self,
        known_logical: &[String],
        id: &str,
        name: Option<&str>,
        logical_output_ids: Option<&[String]>,
    ) -> Result<Zone, Refusal> {
        let at = self.position(id)?;
        if name.is_none() && logical_output_ids.is_none() {
            return Err(Refusal::NothingToUpdate);
        }
        let new_name = match name {
            Some(n) => {
                let n = clean_name(n)?;
                self.name_free(&n, Some(id))?;
                Some(n)
            }
            None => None,
        };
        if let Some(ids) = logical_output_ids {
            self.outputs_free(known_logical, ids, Some(id))?;
        }
        let zone = &mut self.state.zones[at];
        if let Some(n) = new_name {
            zone.name = n;
        }
        if let Some(ids) = logical_output_ids {
            zone.logical_output_ids = ids.to_vec();
        }
        Ok(zone.clone())
    }

    /// The zone holding this logical output, if any. Reads only;
    /// `delivery.remove_logical_output` consults it to refuse.
    pub(crate) fn holder_of(&self, logical_output_id: &str) -> Option<&Zone> {
        self.state.zones.iter().find(|z| {
            z.logical_output_ids.iter().any(|l| l == logical_output_id)
        })
    }

    /// `zone.remove`: the zone taken away. Its logical outputs are
    /// not touched; they are simply in no zone now.
    pub(crate) fn remove(&mut self, id: &str) -> Result<Zone, Refusal> {
        let at = self.position(id)?;
        Ok(self.state.zones.remove(at))
    }
}

/// The state file under the plugin's state directory.
#[derive(Debug, Clone)]
pub(crate) struct ZoneStore {
    path: PathBuf,
}

impl ZoneStore {
    /// File name under the state directory.
    pub(crate) const FILE_NAME: &'static str = "zones.toml";

    /// A store at this path.
    pub(crate) fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// The path.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Read the file. Absent is empty; unreadable or malformed is
    /// empty with a warning, since a bad state file must not keep
    /// the plugin from loading.
    pub(crate) async fn load(&self) -> PersistedZones {
        match tokio::fs::read_to_string(&self.path).await {
            Ok(body) => match toml::from_str::<PersistedZones>(&body) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        plugin = crate::PLUGIN_NAME,
                        path = %self.path.display(),
                        error = %e,
                        "zones file is malformed; starting empty"
                    );
                    PersistedZones::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                PersistedZones::default()
            }
            Err(e) => {
                tracing::warn!(
                    plugin = crate::PLUGIN_NAME,
                    path = %self.path.display(),
                    error = %e,
                    "zones file could not be read; starting empty"
                );
                PersistedZones::default()
            }
        }
    }

    /// Write the file atomically (temp file beside it, then
    /// rename).
    pub(crate) async fn save(
        &self,
        persisted: &PersistedZones,
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

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn known() -> Vec<String> {
        s(&["logical:1", "logical:2", "logical:3"])
    }

    /// Add mints `zone:<n>`, keeps the trimmed name and the list
    /// as given; ids never repeat.
    #[test]
    fn add_mints_a_stable_id() {
        let mut z = Zones::default();
        let a = z
            .add(&known(), "  Downstairs ", &s(&["logical:2", "logical:1"]))
            .unwrap();
        assert_eq!(
            a,
            Zone {
                id: "zone:1".to_string(),
                name: "Downstairs".to_string(),
                logical_output_ids: s(&["logical:2", "logical:1"]),
            }
        );
        let b = z.add(&known(), "Up", &s(&["logical:3"])).unwrap();
        assert_eq!(b.id, "zone:2");
        z.remove("zone:2").unwrap();
        let c = z.add(&known(), "Up", &s(&["logical:3"])).unwrap();
        assert_eq!(c.id, "zone:3", "a removed id is never reused");
    }

    /// A zone with no logical output is refused, on add and on
    /// update, and nothing is minted or changed.
    #[test]
    fn a_zone_with_no_logical_output_is_refused() {
        let mut z = Zones::default();
        assert_eq!(
            z.add(&known(), "Empty", &[]),
            Err(Refusal::NoLogicalOutputs)
        );
        assert!(z.rows().is_empty());
        assert_eq!(z.persisted().next_id, 1, "nothing minted");
        let a = z.add(&known(), "Down", &s(&["logical:1"])).unwrap();
        assert_eq!(
            z.update(&known(), &a.id, None, Some(&[])),
            Err(Refusal::NoLogicalOutputs)
        );
        assert_eq!(z.rows()[0].logical_output_ids, s(&["logical:1"]));
    }

    /// A logical output sits in one zone at most: a second zone
    /// naming it is refused on add and on update; moving it by
    /// updating its own zone is fine.
    #[test]
    fn a_logical_output_sits_in_one_zone_at_most() {
        let mut z = Zones::default();
        let a = z
            .add(&known(), "Down", &s(&["logical:1", "logical:2"]))
            .unwrap();
        assert_eq!(
            z.add(&known(), "Up", &s(&["logical:3", "logical:2"])),
            Err(Refusal::AlreadyZoned {
                logical_output_id: "logical:2".to_string(),
                zone: a.id.clone()
            })
        );
        assert_eq!(z.rows().len(), 1, "the refused zone was not kept");
        let b = z.add(&known(), "Up", &s(&["logical:3"])).unwrap();
        assert_eq!(
            z.update(
                &known(),
                &b.id,
                None,
                Some(&s(&["logical:3", "logical:1"]))
            ),
            Err(Refusal::AlreadyZoned {
                logical_output_id: "logical:1".to_string(),
                zone: a.id.clone()
            })
        );
        assert_eq!(z.rows()[1].logical_output_ids, s(&["logical:3"]));
        // Its own outputs, reordered: not "another zone".
        let same = z
            .update(
                &known(),
                &a.id,
                None,
                Some(&s(&["logical:2", "logical:1"])),
            )
            .unwrap();
        assert_eq!(same.logical_output_ids, s(&["logical:2", "logical:1"]));
        // Named twice in one list.
        assert_eq!(
            z.update(
                &known(),
                &a.id,
                None,
                Some(&s(&["logical:1", "logical:1"]))
            ),
            Err(Refusal::DuplicateLogicalOutput("logical:1".to_string()))
        );
    }

    /// An id that is not a logical output the plugin has is
    /// refused, on add and on update.
    #[test]
    fn an_unknown_logical_output_is_refused() {
        let mut z = Zones::default();
        assert_eq!(
            z.add(&known(), "Down", &s(&["logical:1", "logical:9"])),
            Err(Refusal::UnknownLogicalOutput("logical:9".to_string()))
        );
        assert!(z.rows().is_empty());
        let a = z.add(&known(), "Down", &s(&["logical:1"])).unwrap();
        assert_eq!(
            z.update(&known(), &a.id, None, Some(&s(&["hw:0,0"]))),
            Err(Refusal::UnknownLogicalOutput("hw:0,0".to_string())),
            "a physical id is not a logical output"
        );
        assert_eq!(z.rows()[0].logical_output_ids, s(&["logical:1"]));
    }

    /// Names: empty, too long and taken are refused; respelling
    /// one's own name is allowed.
    #[test]
    fn a_name_is_trimmed_bounded_and_unique() {
        let mut z = Zones::default();
        assert_eq!(
            z.add(&known(), "  ", &s(&["logical:1"])),
            Err(Refusal::EmptyName)
        );
        let long = "x".repeat(NAME_MAX_CHARS + 1);
        assert_eq!(
            z.add(&known(), &long, &s(&["logical:1"])),
            Err(Refusal::NameTooLong(NAME_MAX_CHARS + 1))
        );
        let d = z.add(&known(), "Down", &s(&["logical:1"])).unwrap();
        assert_eq!(
            z.add(&known(), "down", &s(&["logical:2"])),
            Err(Refusal::NameTaken {
                name: "down".to_string(),
                held_by: d.id.clone()
            })
        );
        let u = z.add(&known(), "Up", &s(&["logical:2"])).unwrap();
        assert_eq!(
            z.update(&known(), &u.id, Some("DOWN"), None),
            Err(Refusal::NameTaken {
                name: "DOWN".to_string(),
                held_by: d.id.clone()
            })
        );
        let same = z.update(&known(), &d.id, Some("DOWN"), None).unwrap();
        assert_eq!(same.name, "DOWN");
        assert_eq!(same.id, d.id);
    }

    /// Update keeps the id, changes only what was named, and
    /// refuses an unknown id or an empty update; a refused update
    /// changes nothing.
    #[test]
    fn update_keeps_the_id_and_changes_only_what_was_named() {
        let mut z = Zones::default();
        let d = z.add(&known(), "Down", &s(&["logical:1"])).unwrap();
        let renamed = z.update(&known(), &d.id, Some("Ground"), None).unwrap();
        assert_eq!(renamed.id, d.id);
        assert_eq!(renamed.name, "Ground");
        assert_eq!(renamed.logical_output_ids, s(&["logical:1"]));
        let moved = z
            .update(
                &known(),
                &d.id,
                None,
                Some(&s(&["logical:2", "logical:3"])),
            )
            .unwrap();
        assert_eq!(moved.name, "Ground");
        assert_eq!(moved.logical_output_ids, s(&["logical:2", "logical:3"]));
        assert_eq!(
            z.update(&known(), &d.id, None, None),
            Err(Refusal::NothingToUpdate)
        );
        assert_eq!(
            z.update(&known(), "zone:9", Some("X"), None),
            Err(Refusal::UnknownZone("zone:9".to_string()))
        );
        assert_eq!(
            z.update(&known(), &d.id, Some(""), Some(&s(&["logical:1"]))),
            Err(Refusal::EmptyName)
        );
        assert_eq!(z.rows()[0], moved);
    }

    /// Remove answers the zone taken away, refuses an unknown id,
    /// and frees its logical outputs for another zone.
    #[test]
    fn remove_takes_the_zone_away_and_frees_its_outputs() {
        let mut z = Zones::default();
        let d = z
            .add(&known(), "Down", &s(&["logical:1", "logical:2"]))
            .unwrap();
        let u = z.add(&known(), "Up", &s(&["logical:3"])).unwrap();
        assert_eq!(z.remove(&d.id), Ok(d.clone()));
        assert_eq!(z.rows(), &[u.clone()]);
        assert_eq!(z.remove(&d.id), Err(Refusal::UnknownZone(d.id.clone())));
        let again = z.add(&known(), "Down again", &s(&["logical:1"])).unwrap();
        assert_eq!(again.id, "zone:3");
        assert_eq!(z.rows(), &[u, again]);
    }

    /// The list is every zone in id order, whatever order the
    /// state file held them in, and reading changes nothing.
    #[test]
    fn list_is_every_zone_in_id_order_and_reads_only() {
        let mut z = Zones::default();
        assert!(z.list().is_empty());
        let d = z.add(&known(), "Down", &s(&["logical:1"])).unwrap();
        let u = z.add(&known(), "Up", &s(&["logical:2"])).unwrap();
        assert_eq!(z.list(), vec![d.clone(), u.clone()]);
        z.remove(&d.id).unwrap();
        assert_eq!(z.list(), vec![u.clone()]);
        let mut persisted = z.persisted().clone();
        let p = Zone {
            id: "zone:1".to_string(),
            name: "Porch".to_string(),
            logical_output_ids: s(&["logical:3"]),
        };
        persisted.zones.push(p.clone());
        let z = Zones::from_persisted(persisted.clone());
        assert_eq!(z.rows(), &[u.clone(), p.clone()]);
        assert_eq!(z.list(), vec![p, u]);
        assert_eq!(z.persisted(), &persisted);
    }

    /// The holder of a logical output is the one zone listing it,
    /// found by id and not by name; an unzoned id has none.
    #[test]
    fn holder_of_finds_the_zone_by_logical_id() {
        let mut z = Zones::default();
        let d = z
            .add(&known(), "logical:3", &s(&["logical:1", "logical:2"]))
            .unwrap();
        assert_eq!(
            z.holder_of("logical:2").map(|h| h.id.as_str()),
            Some(d.id.as_str())
        );
        assert_eq!(
            z.holder_of("logical:3"),
            None,
            "a zone's name is not a membership"
        );
        z.remove(&d.id).unwrap();
        assert_eq!(z.holder_of("logical:2"), None);
    }

    /// The store round-trips, keeps `next_id`, and tolerates an
    /// absent or malformed file.
    #[tokio::test]
    async fn the_store_round_trips_and_tolerates_a_bad_file() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            ZoneStore::at(dir.path().join("sub").join(ZoneStore::FILE_NAME));
        assert_eq!(store.load().await, PersistedZones::default());
        let mut z = Zones::default();
        z.add(&known(), "Down", &s(&["logical:1"])).unwrap();
        z.add(&known(), "Up", &s(&["logical:2"])).unwrap();
        z.remove("zone:2").unwrap();
        store.save(z.persisted()).await.unwrap();
        let back = store.load().await;
        assert_eq!(&back, z.persisted());
        assert_eq!(back.next_id, 3);
        assert!(!store.path().with_extension("toml.tmp").exists());
        tokio::fs::write(store.path(), "v = \"nope\"\n[[zones")
            .await
            .unwrap();
        assert_eq!(store.load().await, PersistedZones::default());
    }
}
