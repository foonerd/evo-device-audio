// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Five verbs on one output, leaving the others as they are.
//!
//! `delivery.enable_output`, `delivery.disable_output`,
//! `delivery.set_output_volume`, `delivery.play_output` and
//! `delivery.pause_output` each name one output — one of the
//! rows `delivery.list_outputs` already names, by its `hw:N,M`
//! id — and touch that output only. An unknown output is
//! refused.
//!
//! ## What "playing" means here
//!
//! This plugin owns the chain definition, not the bytes flowing
//! through it. The chain has one terminus, so an output is
//! *playing* when the chain terminates at it. With no output
//! verb spoken the chain follows the operator's `output_device`
//! ([`OutputBinding::Follow`]), and that output is the one
//! playing. `play_output` binds the chain to the named output;
//! `pause_output` on the playing output renders a `null`
//! terminus ([`OutputBinding::Silent`]), so the paused output
//! goes quiet and no other output starts in its place. The
//! device cannot play two outputs at once, so a second play is
//! refused and the first keeps playing.
//!
//! The binding is not persisted: it is what the chain is doing
//! now, and the plugin releases it at unload. Enabled flags and
//! last-set volumes are persisted under the plugin's state
//! directory so a disabled output stays disabled across a
//! restart.
//!
//! ## What "volume" means here
//!
//! An output's volume is its card's default mixer control, set
//! with `amixer`. The global volume is the chain's mixer. When
//! the chain's hardware mixer *is* this output's control, the
//! two are one knob, so the per-output verb is refused rather
//! than moving the global volume by the back door.
//!
//! The pure policy lives here with no I/O; the handlers in
//! `lib.rs` run its effects (one `amixer` call, one drop-in
//! render) and persist its state.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::options_render::{
    parse_card_device, parse_card_name, MixerType, OptionsSettings,
    OutputBinding,
};
use crate::output_enumeration::ResolvedAlsaOutput;

/// Payload version of the five verbs and of the state file.
pub(crate) const OUTPUTS_STATE_VERSION: u32 = 1;

/// How many outputs the chain can play at once. The chain has
/// one terminus.
pub(crate) const OUTPUTS_PLAYING_AT_ONCE: usize = 1;

/// Why a verb was refused. Each names the output it is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The id is not one `delivery.list_outputs` names.
    UnknownOutput(String),
    /// The output is disabled and so cannot play.
    Disabled(String),
    /// Another output is playing; the chain plays one at a time.
    Busy {
        /// The output that keeps playing.
        playing: String,
        /// The output that was asked to play.
        asked: String,
    },
    /// Volume is a percentage, 0 to 100.
    VolumeOutOfRange(u8),
    /// The output has no mixer control to set.
    NoMixerControl(String),
    /// The output's mixer control is the chain's hardware mixer:
    /// that is the global volume, not this output's.
    GlobalMixer {
        /// The output asked about.
        output: String,
        /// The control both would move.
        control: String,
    },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::UnknownOutput(id) => {
                write!(f, "unknown output {id:?}: not one delivery.list_outputs names")
            }
            Refusal::Disabled(id) => {
                write!(
                    f,
                    "output {id:?} is disabled; enable it before playing it"
                )
            }
            Refusal::Busy { playing, asked } => write!(
                f,
                "output {asked:?} cannot play while {playing:?} is playing; \
                 the chain plays {OUTPUTS_PLAYING_AT_ONCE} output at a time \
                 and {playing:?} keeps playing"
            ),
            Refusal::VolumeOutOfRange(v) => {
                write!(f, "volume {v} is out of range; 0 to 100")
            }
            Refusal::NoMixerControl(id) => {
                write!(f, "output {id:?} has no mixer control to set")
            }
            Refusal::GlobalMixer { output, control } => write!(
                f,
                "output {output:?}'s control {control:?} is the chain's \
                 hardware mixer, which is the global volume; set that instead"
            ),
        }
    }
}

/// What a verb asks the handler to do after the policy agreed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Effect {
    /// Nothing beyond recording the state.
    None,
    /// The chain's terminus moved: render the drop-in again.
    Rebind,
}

/// The one `amixer` call `set_output_volume` makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MixerTarget {
    /// ALSA card index (`N` in `hw:N,M`).
    pub(crate) card_idx: u32,
    /// Simple mixer control name on that card.
    pub(crate) control: String,
    /// Percentage to set.
    pub(crate) percent: u8,
}

/// The chain's hardware mixer as the operator's settings name it,
/// when they name one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GlobalMixer {
    /// Card token from the operator's `output_device`: either a
    /// card index or a card name.
    pub(crate) card: String,
    /// The hardware mixer control.
    pub(crate) control: String,
}

impl GlobalMixer {
    /// The chain's hardware mixer, from the operator's settings.
    /// `None` when the chain has no hardware mixer (software or
    /// no mixer), since then the card's own control is not the
    /// global volume.
    pub(crate) fn from_settings(settings: &OptionsSettings) -> Option<Self> {
        match (settings.mixer_type, settings.mixer_control.as_deref()) {
            (Some(MixerType::Hardware), Some(control))
                if !control.is_empty() =>
            {
                let device = settings
                    .output_device
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(crate::options_render::DEFAULT_OUTPUT_DEVICE);
                Some(Self {
                    card: parse_card_name(device).to_string(),
                    control: control.to_string(),
                })
            }
            _ => None,
        }
    }

    fn is_control_of(&self, output: &ResolvedAlsaOutput) -> bool {
        output.default_mixer_control.as_deref() == Some(self.control.as_str())
            && (self.card == output.card_idx.to_string()
                || self.card == output.card_name)
    }
}

/// One output's state as the verbs answer it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct OutputSnapshot {
    /// The `hw:N,M` id.
    pub(crate) output_id: String,
    /// Whether the output may play.
    pub(crate) enabled: bool,
    /// Whether the chain terminates at this output.
    pub(crate) playing: bool,
    /// The last volume set through `set_output_volume`, if any.
    pub(crate) volume: Option<u8>,
}

/// What survives a restart: enabled flags and last-set volumes.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub(crate) struct PersistedOutputs {
    /// State-file version.
    #[serde(default = "state_version")]
    pub(crate) v: u32,
    /// One row per output the verbs have touched.
    #[serde(default)]
    pub(crate) outputs: Vec<PersistedOutput>,
}

fn state_version() -> u32 {
    OUTPUTS_STATE_VERSION
}

/// One persisted output row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PersistedOutput {
    /// The `hw:N,M` id.
    pub(crate) output_id: String,
    /// Whether the output may play.
    pub(crate) enabled: bool,
    /// The last volume set, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) volume: Option<u8>,
}

/// Per-output state and the chain binding. Pure: every method
/// takes the known outputs as an argument and returns what the
/// handler must do.
#[derive(Debug, Clone, Default)]
pub(crate) struct OutputPolicy {
    /// Outputs explicitly disabled. An output not named here is
    /// enabled.
    disabled: BTreeMap<String, ()>,
    /// Last volume set per output.
    volume: BTreeMap<String, u8>,
    /// Where the chain terminates now.
    binding: OutputBinding,
}

/// Resolve the operator's `output_device` descriptor to the
/// `hw:N,M` id of a known output, when it names one. Accepts the
/// `hw:N,M`, `hw:CARD=name,DEV=M` and bare-card forms the options
/// renderer accepts.
pub(crate) fn resolve_output_device<'a>(
    outputs: &'a [ResolvedAlsaOutput],
    descriptor: &str,
) -> Option<&'a ResolvedAlsaOutput> {
    if let Some(exact) = outputs.iter().find(|o| o.alsa_id == descriptor) {
        return Some(exact);
    }
    let card = parse_card_name(descriptor);
    let device = parse_card_device(descriptor);
    outputs.iter().find(|o| {
        o.device_idx == device
            && (o.card_name == card || o.card_idx.to_string() == card)
    })
}

fn known<'a>(
    outputs: &'a [ResolvedAlsaOutput],
    id: &str,
) -> Result<&'a ResolvedAlsaOutput, Refusal> {
    outputs
        .iter()
        .find(|o| o.alsa_id == id)
        .ok_or_else(|| Refusal::UnknownOutput(id.to_string()))
}

impl OutputPolicy {
    /// Where the chain terminates: the bound output, or under
    /// `Follow` the operator's device when it resolves to a known
    /// output.
    pub(crate) fn terminus(
        &self,
        outputs: &[ResolvedAlsaOutput],
        follow_device: &str,
    ) -> Option<String> {
        match &self.binding {
            OutputBinding::Follow => {
                resolve_output_device(outputs, follow_device)
                    .map(|o| o.alsa_id.clone())
            }
            OutputBinding::Output(id) => Some(id.clone()),
            OutputBinding::Silent => None,
        }
    }

    /// The current binding, for the renderer.
    pub(crate) fn binding(&self) -> &OutputBinding {
        &self.binding
    }

    /// Release the binding: the chain follows the operator's
    /// settings again. Returns whether anything changed.
    pub(crate) fn release_binding(&mut self) -> bool {
        if self.binding == OutputBinding::Follow {
            return false;
        }
        self.binding = OutputBinding::Follow;
        true
    }

    fn is_enabled(&self, id: &str) -> bool {
        !self.disabled.contains_key(id)
    }

    /// `delivery.enable_output`.
    pub(crate) fn enable(
        &mut self,
        outputs: &[ResolvedAlsaOutput],
        id: &str,
    ) -> Result<Effect, Refusal> {
        let output = known(outputs, id)?;
        self.disabled.remove(&output.alsa_id);
        Ok(Effect::None)
    }

    /// `delivery.disable_output`. Disabling the playing output
    /// pauses it; no other output starts.
    pub(crate) fn disable(
        &mut self,
        outputs: &[ResolvedAlsaOutput],
        follow_device: &str,
        id: &str,
    ) -> Result<Effect, Refusal> {
        let output = known(outputs, id)?;
        self.disabled.insert(output.alsa_id.clone(), ());
        if self.terminus(outputs, follow_device).as_deref() == Some(id) {
            self.binding = OutputBinding::Silent;
            return Ok(Effect::Rebind);
        }
        Ok(Effect::None)
    }

    /// `delivery.play_output`. Refused when the output is unknown
    /// or disabled, and when another output is playing: the chain
    /// plays one output at a time and the first keeps playing.
    pub(crate) fn play(
        &mut self,
        outputs: &[ResolvedAlsaOutput],
        follow_device: &str,
        id: &str,
    ) -> Result<Effect, Refusal> {
        let output = known(outputs, id)?;
        if !self.is_enabled(id) {
            return Err(Refusal::Disabled(id.to_string()));
        }
        let playing: Vec<String> =
            self.terminus(outputs, follow_device).into_iter().collect();
        if playing.iter().any(|p| p == id) {
            return Ok(Effect::None);
        }
        if playing.len() >= OUTPUTS_PLAYING_AT_ONCE {
            return Err(Refusal::Busy {
                playing: playing[0].clone(),
                asked: id.to_string(),
            });
        }
        self.binding = OutputBinding::Output(output.alsa_id.clone());
        Ok(Effect::Rebind)
    }

    /// `delivery.pause_output`. Pausing the playing output
    /// silences the chain; pausing one that is not playing
    /// changes nothing, and never pauses another.
    pub(crate) fn pause(
        &mut self,
        outputs: &[ResolvedAlsaOutput],
        follow_device: &str,
        id: &str,
    ) -> Result<Effect, Refusal> {
        known(outputs, id)?;
        if self.terminus(outputs, follow_device).as_deref() == Some(id) {
            self.binding = OutputBinding::Silent;
            return Ok(Effect::Rebind);
        }
        Ok(Effect::None)
    }

    /// `delivery.set_output_volume`: the one mixer call to make,
    /// or why not. The volume is recorded by
    /// [`OutputPolicy::record_volume`] once the call succeeded.
    pub(crate) fn volume_target(
        &self,
        outputs: &[ResolvedAlsaOutput],
        global: Option<&GlobalMixer>,
        id: &str,
        percent: u8,
    ) -> Result<MixerTarget, Refusal> {
        let output = known(outputs, id)?;
        if percent > 100 {
            return Err(Refusal::VolumeOutOfRange(percent));
        }
        let control = output
            .default_mixer_control
            .as_deref()
            .filter(|c| !c.is_empty())
            .ok_or_else(|| Refusal::NoMixerControl(id.to_string()))?;
        if let Some(global) = global {
            if global.is_control_of(output) {
                return Err(Refusal::GlobalMixer {
                    output: id.to_string(),
                    control: control.to_string(),
                });
            }
        }
        Ok(MixerTarget {
            card_idx: output.card_idx,
            control: control.to_string(),
            percent,
        })
    }

    /// Record a volume the mixer accepted.
    pub(crate) fn record_volume(&mut self, id: &str, percent: u8) {
        self.volume.insert(id.to_string(), percent);
    }

    /// One output's state.
    pub(crate) fn snapshot(
        &self,
        outputs: &[ResolvedAlsaOutput],
        follow_device: &str,
        id: &str,
    ) -> OutputSnapshot {
        OutputSnapshot {
            output_id: id.to_string(),
            enabled: self.is_enabled(id),
            playing: self.terminus(outputs, follow_device).as_deref()
                == Some(id),
            volume: self.volume.get(id).copied(),
        }
    }

    /// What to persist.
    pub(crate) fn persisted(&self) -> PersistedOutputs {
        let mut ids: Vec<&String> =
            self.disabled.keys().chain(self.volume.keys()).collect();
        ids.sort();
        ids.dedup();
        PersistedOutputs {
            v: OUTPUTS_STATE_VERSION,
            outputs: ids
                .into_iter()
                .map(|id| PersistedOutput {
                    output_id: id.clone(),
                    enabled: self.is_enabled(id),
                    volume: self.volume.get(id).copied(),
                })
                .collect(),
        }
    }

    /// Restore enabled flags and volumes. The binding is not
    /// persisted and stays as it is.
    pub(crate) fn restore(&mut self, persisted: &PersistedOutputs) {
        self.disabled.clear();
        self.volume.clear();
        for row in &persisted.outputs {
            if !row.enabled {
                self.disabled.insert(row.output_id.clone(), ());
            }
            if let Some(v) = row.volume {
                self.volume.insert(row.output_id.clone(), v);
            }
        }
    }
}

/// The state file under the plugin's state directory.
#[derive(Debug, Clone)]
pub(crate) struct OutputStateStore {
    path: PathBuf,
}

impl OutputStateStore {
    /// File name under the state directory.
    pub(crate) const FILE_NAME: &'static str = "outputs-state.toml";

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
    pub(crate) async fn load(&self) -> PersistedOutputs {
        match tokio::fs::read_to_string(&self.path).await {
            Ok(body) => match toml::from_str::<PersistedOutputs>(&body) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        plugin = crate::PLUGIN_NAME,
                        path = %self.path.display(),
                        error = %e,
                        "outputs state file is malformed; starting empty"
                    );
                    PersistedOutputs::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                PersistedOutputs::default()
            }
            Err(e) => {
                tracing::warn!(
                    plugin = crate::PLUGIN_NAME,
                    path = %self.path.display(),
                    error = %e,
                    "outputs state file could not be read; starting empty"
                );
                PersistedOutputs::default()
            }
        }
    }

    /// Write the file atomically (temp file beside it, then
    /// rename).
    pub(crate) async fn save(
        &self,
        persisted: &PersistedOutputs,
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
pub(crate) mod tests {
    use super::*;
    use crate::output_enumeration::{CatalogProvenance, OutputClass};

    pub(crate) fn output(
        card_idx: u32,
        card_name: &str,
        control: Option<&str>,
    ) -> ResolvedAlsaOutput {
        ResolvedAlsaOutput {
            card_idx,
            device_idx: 0,
            card_name: card_name.to_string(),
            alsa_id: format!("hw:{card_idx},0"),
            label: card_name.to_string(),
            output_class: OutputClass::Unknown,
            default_mixer_control: control.map(str::to_string),
            catalog_provenance: CatalogProvenance::Unmapped,
            hidden: false,
            ignore_generic_mixer: false,
        }
    }

    fn two() -> Vec<ResolvedAlsaOutput> {
        vec![
            output(0, "DAC", Some("Digital")),
            output(1, "HDMI", Some("PCM")),
        ]
    }

    const FOLLOW: &str = "hw:CARD=DAC,DEV=0";

    /// Every verb refuses an id `list_outputs` does not name, and
    /// changes nothing.
    #[test]
    fn an_unknown_output_is_refused_by_every_verb() {
        let outs = two();
        let mut p = OutputPolicy::default();
        let unknown = Refusal::UnknownOutput("hw:9,9".to_string());
        assert_eq!(p.enable(&outs, "hw:9,9"), Err(unknown.clone()));
        assert_eq!(p.disable(&outs, FOLLOW, "hw:9,9"), Err(unknown.clone()));
        assert_eq!(p.play(&outs, FOLLOW, "hw:9,9"), Err(unknown.clone()));
        assert_eq!(p.pause(&outs, FOLLOW, "hw:9,9"), Err(unknown.clone()));
        assert_eq!(
            p.volume_target(&outs, None, "hw:9,9", 10),
            Err(unknown.clone())
        );
        assert_eq!(p.binding(), &OutputBinding::Follow);
        assert!(p.persisted().outputs.is_empty());
    }

    /// Under `Follow` the operator's device is the playing output.
    #[test]
    fn the_operators_device_plays_until_an_output_verb_speaks() {
        let outs = two();
        let p = OutputPolicy::default();
        assert_eq!(p.terminus(&outs, FOLLOW).as_deref(), Some("hw:0,0"));
        assert_eq!(p.terminus(&outs, "hw:1,0").as_deref(), Some("hw:1,0"));
        assert_eq!(p.terminus(&outs, "hw:CARD=Nope,DEV=0"), None);
        assert!(p.snapshot(&outs, FOLLOW, "hw:0,0").playing);
        assert!(!p.snapshot(&outs, FOLLOW, "hw:1,0").playing);
    }

    /// Disabling, enabling or setting the volume of one output
    /// leaves the other as it was.
    #[test]
    fn changing_one_output_leaves_the_other_alone() {
        let outs = two();
        let mut p = OutputPolicy::default();
        let before_b = p.snapshot(&outs, FOLLOW, "hw:1,0");

        assert_eq!(p.disable(&outs, FOLLOW, "hw:1,0"), Ok(Effect::None));
        let a = p.snapshot(&outs, FOLLOW, "hw:0,0");
        assert!(a.enabled && a.playing, "{a:?}");
        assert!(!p.snapshot(&outs, FOLLOW, "hw:1,0").enabled);

        assert_eq!(p.enable(&outs, "hw:1,0"), Ok(Effect::None));
        assert_eq!(p.snapshot(&outs, FOLLOW, "hw:1,0"), before_b);

        // Both disabled: enabling A leaves B disabled.
        p.disable(&outs, FOLLOW, "hw:0,0").unwrap();
        p.disable(&outs, FOLLOW, "hw:1,0").unwrap();
        assert_eq!(p.enable(&outs, "hw:0,0"), Ok(Effect::None));
        assert!(!p.snapshot(&outs, FOLLOW, "hw:1,0").enabled);
        assert_eq!(p.enable(&outs, "hw:1,0"), Ok(Effect::None));

        let t = p.volume_target(&outs, None, "hw:0,0", 40).unwrap();
        assert_eq!(
            t,
            MixerTarget {
                card_idx: 0,
                control: "Digital".to_string(),
                percent: 40
            }
        );
        p.record_volume("hw:0,0", 40);
        assert_eq!(p.snapshot(&outs, FOLLOW, "hw:0,0").volume, Some(40));
        assert_eq!(p.snapshot(&outs, FOLLOW, "hw:1,0"), before_b);
    }

    /// The chain plays one output at a time: the second play is
    /// refused and the first keeps playing. Pausing the playing
    /// output silences the chain and starts no other.
    #[test]
    fn the_second_play_is_refused_and_the_first_keeps_playing() {
        let outs = two();
        let mut p = OutputPolicy::default();
        // The operator's device is playing; the other must wait.
        assert_eq!(
            p.play(&outs, FOLLOW, "hw:1,0"),
            Err(Refusal::Busy {
                playing: "hw:0,0".to_string(),
                asked: "hw:1,0".to_string()
            })
        );
        assert_eq!(p.binding(), &OutputBinding::Follow);

        // Pause it: nothing plays, the other did not start.
        assert_eq!(p.pause(&outs, FOLLOW, "hw:0,0"), Ok(Effect::Rebind));
        assert_eq!(p.binding(), &OutputBinding::Silent);
        assert!(!p.snapshot(&outs, FOLLOW, "hw:1,0").playing);

        // Now the other may play; the first then may not.
        assert_eq!(p.play(&outs, FOLLOW, "hw:1,0"), Ok(Effect::Rebind));
        assert_eq!(p.binding(), &OutputBinding::Output("hw:1,0".to_string()));
        assert_eq!(
            p.play(&outs, FOLLOW, "hw:0,0"),
            Err(Refusal::Busy {
                playing: "hw:1,0".to_string(),
                asked: "hw:0,0".to_string()
            })
        );
        assert!(p.snapshot(&outs, FOLLOW, "hw:1,0").playing);
        // Playing the playing output again changes nothing.
        assert_eq!(p.play(&outs, FOLLOW, "hw:1,0"), Ok(Effect::None));
        // Pausing the one that is not playing pauses nothing.
        assert_eq!(p.pause(&outs, FOLLOW, "hw:0,0"), Ok(Effect::None));
        assert!(p.snapshot(&outs, FOLLOW, "hw:1,0").playing);
    }

    /// A disabled output cannot play; disabling the playing output
    /// pauses it and starts no other.
    #[test]
    fn a_disabled_output_does_not_play() {
        let outs = two();
        let mut p = OutputPolicy::default();
        assert_eq!(p.pause(&outs, FOLLOW, "hw:0,0"), Ok(Effect::Rebind));
        assert_eq!(p.disable(&outs, FOLLOW, "hw:1,0"), Ok(Effect::None));
        assert_eq!(
            p.play(&outs, FOLLOW, "hw:1,0"),
            Err(Refusal::Disabled("hw:1,0".to_string()))
        );
        assert_eq!(p.binding(), &OutputBinding::Silent);

        assert_eq!(p.enable(&outs, "hw:1,0"), Ok(Effect::None));
        assert_eq!(p.play(&outs, FOLLOW, "hw:1,0"), Ok(Effect::Rebind));
        assert_eq!(p.disable(&outs, FOLLOW, "hw:1,0"), Ok(Effect::Rebind));
        assert_eq!(p.binding(), &OutputBinding::Silent);
        assert!(!p.snapshot(&outs, FOLLOW, "hw:0,0").playing);
    }

    /// Volume on the chain's hardware mixer is the global volume
    /// and is refused; the other card's control is not.
    #[test]
    fn volume_on_the_chains_hardware_mixer_is_refused() {
        let outs = two();
        let p = OutputPolicy::default();
        let settings = OptionsSettings {
            mixer_type: Some(MixerType::Hardware),
            mixer_control: Some("Digital".to_string()),
            output_device: Some(FOLLOW.to_string()),
            ..Default::default()
        };
        let global = GlobalMixer::from_settings(&settings).unwrap();
        assert_eq!(
            p.volume_target(&outs, Some(&global), "hw:0,0", 50),
            Err(Refusal::GlobalMixer {
                output: "hw:0,0".to_string(),
                control: "Digital".to_string()
            })
        );
        assert!(p.volume_target(&outs, Some(&global), "hw:1,0", 50).is_ok());

        // A software mixer is not the card's control.
        let software = OptionsSettings {
            mixer_type: Some(MixerType::Software),
            ..settings.clone()
        };
        assert_eq!(GlobalMixer::from_settings(&software), None);

        // The card index form names the same card.
        let by_index = OptionsSettings {
            output_device: Some("hw:0,0".to_string()),
            ..settings
        };
        let global = GlobalMixer::from_settings(&by_index).unwrap();
        assert!(p.volume_target(&outs, Some(&global), "hw:0,0", 50).is_err());
    }

    /// Out of range and no mixer control are refused.
    #[test]
    fn volume_needs_a_control_and_a_percentage() {
        let outs =
            vec![output(0, "DAC", Some("Digital")), output(2, "Bare", None)];
        let p = OutputPolicy::default();
        assert_eq!(
            p.volume_target(&outs, None, "hw:0,0", 101),
            Err(Refusal::VolumeOutOfRange(101))
        );
        assert!(p.volume_target(&outs, None, "hw:0,0", 100).is_ok());
        assert!(p.volume_target(&outs, None, "hw:0,0", 0).is_ok());
        assert_eq!(
            p.volume_target(&outs, None, "hw:2,0", 10),
            Err(Refusal::NoMixerControl("hw:2,0".to_string()))
        );
    }

    /// Enabled flags and volumes round-trip; the binding does not.
    #[test]
    fn enabled_and_volume_persist_and_the_binding_does_not() {
        let outs = two();
        let mut p = OutputPolicy::default();
        p.disable(&outs, FOLLOW, "hw:1,0").unwrap();
        p.record_volume("hw:0,0", 33);
        p.play(&outs, FOLLOW, "hw:0,0").unwrap();
        let persisted = p.persisted();
        assert_eq!(
            persisted.outputs,
            vec![
                PersistedOutput {
                    output_id: "hw:0,0".to_string(),
                    enabled: true,
                    volume: Some(33)
                },
                PersistedOutput {
                    output_id: "hw:1,0".to_string(),
                    enabled: false,
                    volume: None
                },
            ]
        );
        let body = toml::to_string(&persisted).unwrap();
        assert!(!body.contains("binding"), "{body}");
        let back: PersistedOutputs = toml::from_str(&body).unwrap();
        let mut q = OutputPolicy::default();
        q.restore(&back);
        assert!(!q.snapshot(&outs, FOLLOW, "hw:1,0").enabled);
        assert_eq!(q.snapshot(&outs, FOLLOW, "hw:0,0").volume, Some(33));
        assert_eq!(q.binding(), &OutputBinding::Follow);
    }

    /// The store writes and reads the file; absent and malformed
    /// are empty.
    #[tokio::test]
    async fn the_store_round_trips_and_tolerates_a_bad_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = OutputStateStore::at(
            dir.path().join("sub").join("outputs-state.toml"),
        );
        assert_eq!(store.load().await, PersistedOutputs::default());
        let p = PersistedOutputs {
            v: OUTPUTS_STATE_VERSION,
            outputs: vec![PersistedOutput {
                output_id: "hw:1,0".to_string(),
                enabled: false,
                volume: Some(7),
            }],
        };
        store.save(&p).await.unwrap();
        assert_eq!(store.load().await, p);
        assert!(!store.path().with_extension("toml.tmp").exists());
        tokio::fs::write(store.path(), "v = \"nope\"\n[[outputs")
            .await
            .unwrap();
        assert_eq!(store.load().await, PersistedOutputs::default());
    }

    /// The operator's device descriptor resolves by card name or
    /// index, with the device index.
    #[test]
    fn the_operators_device_resolves_to_a_known_output() {
        let mut outs = two();
        outs.push(ResolvedAlsaOutput {
            device_idx: 1,
            alsa_id: "hw:1,1".to_string(),
            ..output(1, "HDMI", None)
        });
        let id = |d: &str| {
            resolve_output_device(&outs, d).map(|o| o.alsa_id.clone())
        };
        assert_eq!(id("hw:CARD=DAC,DEV=0").as_deref(), Some("hw:0,0"));
        assert_eq!(id("hw:0,0").as_deref(), Some("hw:0,0"));
        assert_eq!(id("hw:CARD=HDMI,DEV=1").as_deref(), Some("hw:1,1"));
        assert_eq!(id("hw:1,1").as_deref(), Some("hw:1,1"));
        assert_eq!(id("hw:CARD=HDMI").as_deref(), Some("hw:1,0"));
        assert_eq!(id("hw:CARD=Nope,DEV=0"), None);
    }
}
