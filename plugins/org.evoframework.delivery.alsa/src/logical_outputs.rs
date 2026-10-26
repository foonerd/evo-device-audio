// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Add, update and remove a named output.
//!
//! A logical output is a name the operator gives to one physical
//! `hw:N,M` output — one `delivery.list_outputs` names. It has a
//! stable id the plugin mints (`logical:<n>`), a name, and the
//! physical output it points at. `delivery.add_logical_output`
//! makes one, `delivery.update_logical_output` renames it or
//! points it elsewhere without changing its id, and
//! `delivery.remove_logical_output` takes it away.
//!
//! A name is trimmed, non-empty, at most [`NAME_MAX_CHARS`]
//! characters, and unique among logical outputs case-
//! insensitively. The physical output must be one the plugin
//! knows; a logical id is never accepted where a physical id is
//! asked for, so the five output verbs of G1 are untouched. Two
//! names may point at one physical output: a name is a label,
//! not a claim.
//!
//! Names survive a restart in `logical-outputs.toml` under the
//! plugin's state directory. No subject is published and
//! `delivery.list_outputs` is unchanged.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::output_enumeration::ResolvedAlsaOutput;

/// State-file version.
pub(crate) const LOGICAL_OUTPUTS_VERSION: u32 = 1;

/// Longest name accepted, in characters.
pub(crate) const NAME_MAX_CHARS: usize = 64;

/// Prefix of every logical output id.
pub(crate) const ID_PREFIX: &str = "logical:";

/// Why a verb was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The physical output is not one `delivery.list_outputs` names.
    UnknownOutput(String),
    /// No logical output has this id.
    UnknownLogicalOutput(String),
    /// The name is empty once trimmed.
    EmptyName,
    /// The name is longer than [`NAME_MAX_CHARS`].
    NameTooLong(usize),
    /// Another logical output already carries this name,
    /// case-insensitively.
    NameTaken {
        /// The name asked for.
        name: String,
        /// The logical output that has it.
        held_by: String,
    },
    /// An update that names neither a new name nor a new output.
    NothingToUpdate,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::UnknownOutput(id) => write!(
                f,
                "unknown output {id:?}: not one delivery.list_outputs names"
            ),
            Refusal::UnknownLogicalOutput(id) => {
                write!(f, "unknown logical output {id:?}")
            }
            Refusal::EmptyName => write!(f, "a logical output needs a name"),
            Refusal::NameTooLong(n) => {
                write!(f, "name is {n} characters; at most {NAME_MAX_CHARS}")
            }
            Refusal::NameTaken { name, held_by } => {
                write!(f, "name {name:?} is already carried by {held_by:?}")
            }
            Refusal::NothingToUpdate => {
                write!(f, "update names neither a new name nor a new output")
            }
        }
    }
}

/// One logical output, as persisted and as answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LogicalOutput {
    /// Stable id the plugin minted: `logical:<n>`.
    pub(crate) id: String,
    /// The operator's name for it.
    pub(crate) name: String,
    /// The physical `hw:N,M` output it points at.
    pub(crate) output_id: String,
}

/// The persisted registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PersistedLogicalOutputs {
    /// State-file version.
    #[serde(default = "state_version")]
    pub(crate) v: u32,
    /// The next number to mint; never reused after a remove, so
    /// a stale id from before a remove cannot land on a newer
    /// logical output.
    #[serde(default = "first_id")]
    pub(crate) next_id: u64,
    /// Every logical output, in creation order.
    #[serde(default)]
    pub(crate) outputs: Vec<LogicalOutput>,
}

fn state_version() -> u32 {
    LOGICAL_OUTPUTS_VERSION
}

fn first_id() -> u64 {
    1
}

impl Default for PersistedLogicalOutputs {
    fn default() -> Self {
        Self {
            v: LOGICAL_OUTPUTS_VERSION,
            next_id: first_id(),
            outputs: Vec::new(),
        }
    }
}

/// The registry. Pure: every method takes the known physical
/// outputs and returns the row the verb answers with.
#[derive(Debug, Clone, Default)]
pub(crate) struct LogicalOutputs {
    state: PersistedLogicalOutputs,
}

fn known_output(
    outputs: &[ResolvedAlsaOutput],
    id: &str,
) -> Result<(), Refusal> {
    if outputs.iter().any(|o| o.alsa_id == id) {
        Ok(())
    } else {
        Err(Refusal::UnknownOutput(id.to_string()))
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

impl LogicalOutputs {
    /// A registry holding what was persisted.
    pub(crate) fn from_persisted(state: PersistedLogicalOutputs) -> Self {
        Self { state }
    }

    /// What to persist.
    pub(crate) fn persisted(&self) -> &PersistedLogicalOutputs {
        &self.state
    }

    /// The registry's rows, in creation order.
    #[cfg(test)]
    pub(crate) fn rows(&self) -> &[LogicalOutput] {
        &self.state.outputs
    }

    fn position(&self, id: &str) -> Result<usize, Refusal> {
        self.state
            .outputs
            .iter()
            .position(|l| l.id == id)
            .ok_or_else(|| Refusal::UnknownLogicalOutput(id.to_string()))
    }

    /// Refuse a name another logical output carries, case-
    /// insensitively; `except` is the one being renamed.
    fn name_free(
        &self,
        name: &str,
        except: Option<&str>,
    ) -> Result<(), Refusal> {
        let wanted = name.to_lowercase();
        match self.state.outputs.iter().find(|l| {
            Some(l.id.as_str()) != except && l.name.to_lowercase() == wanted
        }) {
            Some(holder) => Err(Refusal::NameTaken {
                name: name.to_string(),
                held_by: holder.id.clone(),
            }),
            None => Ok(()),
        }
    }

    /// `delivery.add_logical_output`: a new row with a minted id.
    pub(crate) fn add(
        &mut self,
        outputs: &[ResolvedAlsaOutput],
        name: &str,
        output_id: &str,
    ) -> Result<LogicalOutput, Refusal> {
        let name = clean_name(name)?;
        known_output(outputs, output_id)?;
        self.name_free(&name, None)?;
        let id = format!("{ID_PREFIX}{}", self.state.next_id);
        self.state.next_id += 1;
        let row = LogicalOutput {
            id,
            name,
            output_id: output_id.to_string(),
        };
        self.state.outputs.push(row.clone());
        Ok(row)
    }

    /// `delivery.update_logical_output`: a new name, a new
    /// physical output, or both; the id stays.
    pub(crate) fn update(
        &mut self,
        outputs: &[ResolvedAlsaOutput],
        id: &str,
        name: Option<&str>,
        output_id: Option<&str>,
    ) -> Result<LogicalOutput, Refusal> {
        let at = self.position(id)?;
        if name.is_none() && output_id.is_none() {
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
        if let Some(o) = output_id {
            known_output(outputs, o)?;
        }
        let row = &mut self.state.outputs[at];
        if let Some(n) = new_name {
            row.name = n;
        }
        if let Some(o) = output_id {
            row.output_id = o.to_string();
        }
        Ok(row.clone())
    }

    /// `delivery.remove_logical_output`: the row taken away.
    pub(crate) fn remove(
        &mut self,
        id: &str,
    ) -> Result<LogicalOutput, Refusal> {
        let at = self.position(id)?;
        Ok(self.state.outputs.remove(at))
    }
}

/// The state file under the plugin's state directory.
#[derive(Debug, Clone)]
pub(crate) struct LogicalOutputStore {
    path: PathBuf,
}

impl LogicalOutputStore {
    /// File name under the state directory.
    pub(crate) const FILE_NAME: &'static str = "logical-outputs.toml";

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
    pub(crate) async fn load(&self) -> PersistedLogicalOutputs {
        match tokio::fs::read_to_string(&self.path).await {
            Ok(body) => {
                match toml::from_str::<PersistedLogicalOutputs>(&body) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(
                            plugin = crate::PLUGIN_NAME,
                            path = %self.path.display(),
                            error = %e,
                            "logical outputs file is malformed; starting empty"
                        );
                        PersistedLogicalOutputs::default()
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                PersistedLogicalOutputs::default()
            }
            Err(e) => {
                tracing::warn!(
                    plugin = crate::PLUGIN_NAME,
                    path = %self.path.display(),
                    error = %e,
                    "logical outputs file could not be read; starting empty"
                );
                PersistedLogicalOutputs::default()
            }
        }
    }

    /// Write the file atomically (temp file beside it, then
    /// rename).
    pub(crate) async fn save(
        &self,
        persisted: &PersistedLogicalOutputs,
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
    use crate::outputs::tests::output;

    fn two() -> Vec<ResolvedAlsaOutput> {
        vec![
            output(0, "DAC", Some("Digital")),
            output(1, "HDMI", Some("PCM")),
        ]
    }

    /// Add mints `logical:<n>`, keeps the trimmed name, and
    /// points at the physical output; ids never repeat.
    #[test]
    fn add_mints_a_stable_id() {
        let outs = two();
        let mut r = LogicalOutputs::default();
        let a = r.add(&outs, "  Kitchen ", "hw:0,0").unwrap();
        assert_eq!(
            a,
            LogicalOutput {
                id: "logical:1".to_string(),
                name: "Kitchen".to_string(),
                output_id: "hw:0,0".to_string()
            }
        );
        let b = r.add(&outs, "Hall", "hw:0,0").unwrap();
        assert_eq!(b.id, "logical:2", "two names on one output are allowed");
        r.remove("logical:2").unwrap();
        let c = r.add(&outs, "Hall", "hw:1,0").unwrap();
        assert_eq!(c.id, "logical:3", "a removed id is never reused");
        assert_eq!(r.rows().len(), 2);
    }

    /// An output `list_outputs` does not name is refused, on add
    /// and on update, and nothing is minted.
    #[test]
    fn an_unknown_physical_output_is_refused() {
        let outs = two();
        let mut r = LogicalOutputs::default();
        assert_eq!(
            r.add(&outs, "Kitchen", "hw:9,9"),
            Err(Refusal::UnknownOutput("hw:9,9".to_string()))
        );
        assert!(r.rows().is_empty());
        assert_eq!(r.persisted().next_id, 1, "nothing minted");
        let a = r.add(&outs, "Kitchen", "hw:0,0").unwrap();
        assert_eq!(
            r.update(&outs, &a.id, None, Some("logical:1")),
            Err(Refusal::UnknownOutput("logical:1".to_string())),
            "a logical id is not a physical output"
        );
        assert_eq!(r.rows()[0].output_id, "hw:0,0");
    }

    /// Names: empty refused, too long refused, taken refused
    /// case-insensitively; renaming to one's own name is fine.
    #[test]
    fn a_name_is_trimmed_bounded_and_unique() {
        let outs = two();
        let mut r = LogicalOutputs::default();
        assert_eq!(r.add(&outs, "   ", "hw:0,0"), Err(Refusal::EmptyName));
        let long = "x".repeat(NAME_MAX_CHARS + 1);
        assert_eq!(
            r.add(&outs, &long, "hw:0,0"),
            Err(Refusal::NameTooLong(NAME_MAX_CHARS + 1))
        );
        assert!(r.add(&outs, &"y".repeat(NAME_MAX_CHARS), "hw:0,0").is_ok());
        let k = r.add(&outs, "Kitchen", "hw:0,0").unwrap();
        assert_eq!(
            r.add(&outs, "kitchen", "hw:1,0"),
            Err(Refusal::NameTaken {
                name: "kitchen".to_string(),
                held_by: k.id.clone()
            })
        );
        let h = r.add(&outs, "Hall", "hw:1,0").unwrap();
        assert_eq!(
            r.update(&outs, &h.id, Some("KITCHEN"), None),
            Err(Refusal::NameTaken {
                name: "KITCHEN".to_string(),
                held_by: k.id.clone()
            })
        );
        let same = r.update(&outs, &k.id, Some("KITCHEN"), None).unwrap();
        assert_eq!(same.name, "KITCHEN", "its own name, respelled");
        assert_eq!(same.id, k.id);
    }

    /// Update keeps the id, changes what was named, and refuses
    /// an unknown id or an empty update.
    #[test]
    fn update_keeps_the_id_and_changes_only_what_was_named() {
        let outs = two();
        let mut r = LogicalOutputs::default();
        let k = r.add(&outs, "Kitchen", "hw:0,0").unwrap();
        let renamed = r.update(&outs, &k.id, Some("Pantry"), None).unwrap();
        assert_eq!(renamed.id, k.id);
        assert_eq!(renamed.name, "Pantry");
        assert_eq!(renamed.output_id, "hw:0,0");
        let moved = r.update(&outs, &k.id, None, Some("hw:1,0")).unwrap();
        assert_eq!(moved.name, "Pantry");
        assert_eq!(moved.output_id, "hw:1,0");
        assert_eq!(
            r.update(&outs, &k.id, None, None),
            Err(Refusal::NothingToUpdate)
        );
        assert_eq!(
            r.update(&outs, "logical:9", Some("X"), None),
            Err(Refusal::UnknownLogicalOutput("logical:9".to_string()))
        );
        // A refused update changes nothing.
        assert_eq!(
            r.update(&outs, &k.id, Some(""), Some("hw:0,0")),
            Err(Refusal::EmptyName)
        );
        assert_eq!(r.rows()[0], moved);
    }

    /// Remove answers the row taken away and refuses an unknown
    /// id; the other rows stay.
    #[test]
    fn remove_takes_one_row_away() {
        let outs = two();
        let mut r = LogicalOutputs::default();
        let k = r.add(&outs, "Kitchen", "hw:0,0").unwrap();
        let h = r.add(&outs, "Hall", "hw:1,0").unwrap();
        assert_eq!(r.remove(&k.id), Ok(k.clone()));
        assert_eq!(r.rows(), &[h.clone()]);
        assert_eq!(
            r.remove(&k.id),
            Err(Refusal::UnknownLogicalOutput(k.id.clone()))
        );
        assert_eq!(r.rows(), &[h]);
    }

    /// The store round-trips, keeps `next_id`, and tolerates an
    /// absent or malformed file.
    #[tokio::test]
    async fn the_store_round_trips_and_tolerates_a_bad_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = LogicalOutputStore::at(
            dir.path().join("sub").join(LogicalOutputStore::FILE_NAME),
        );
        assert_eq!(store.load().await, PersistedLogicalOutputs::default());
        let outs = two();
        let mut r = LogicalOutputs::default();
        r.add(&outs, "Kitchen", "hw:0,0").unwrap();
        r.add(&outs, "Hall", "hw:1,0").unwrap();
        r.remove("logical:2").unwrap();
        store.save(r.persisted()).await.unwrap();
        let back = store.load().await;
        assert_eq!(&back, r.persisted());
        assert_eq!(back.next_id, 3);
        let again = LogicalOutputs::from_persisted(back);
        assert_eq!(again.rows().len(), 1);
        assert!(!store.path().with_extension("toml.tmp").exists());
        tokio::fs::write(store.path(), "v = \"nope\"\n[[outputs")
            .await
            .unwrap();
        assert_eq!(store.load().await, PersistedLogicalOutputs::default());
    }
}
