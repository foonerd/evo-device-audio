// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The two audio chains this distribution publishes, and the
//! controller that picks between them.
//!
//! **Direct.** The default: the source writes the ALSA name `evo`
//! and delivery reads it, so MPD plays straight into the DAC's
//! pipeline. Nothing sits between them. This is the chain the
//! distribution has always published, and it is the one in force
//! unless the operator has chosen the EQ.
//!
//! **EqInPath.** The source writes `evo_comp_in`, a pcm on the
//! snd-aloop playback half; the composition stage captures the
//! other half, runs its cascade, and plays the result into `evo`;
//! delivery reads `evo` as before. This chain exists only while
//! the composition mode is `eq_only`, because that is the only
//! mode in which a stage between the source and the DAC does
//! anything, and it costs a hop, a fixed format and an MPD restart
//! that nobody who is not using the EQ should pay.
//!
//! [`ChainController`] follows the composition plugin's
//! `composition.mode_changed` announcements on the happenings bus
//! and republishes the matching chain through the topology store.
//! It publishes nothing until [`ChainController::arm`] is called,
//! which the distribution does once every plugin has admitted; the
//! chain published then is the one for the mode learnt so far, so
//! a device that comes up in `eq_only` comes up with the EQ in the
//! path and one that does not comes up direct, with no flicker
//! between the two.
//!
//! The composition plugin holds no device in any mode but
//! `eq_only`, so the chain going back to Direct needs no message
//! to it: it has already let go.
//!
//! The EqInPath chain needs `pcm.evo_comp_in` in the ALSA
//! configuration, which this distribution installs with the binary. A
//! box where only the binary was updated has no such pcm, and MPD
//! pointed at a name that does not exist plays nothing. The controller
//! therefore checks the configuration before it publishes that chain
//! and, when the pcm is missing, keeps the direct chain and says why:
//! the EQ is not heard, but the music is.

use std::path::PathBuf;
use std::sync::Arc;

use evo::happenings::Happening;
use evo::server::{
    ActiveAudioTopology, ActiveChainStage, ScoreBreakdown, VolumeMode,
};
use evo_plugin_sdk::audio::{AudioFormat, PcmCodec};
use evo_plugin_sdk::contract::audio_routing::EndpointKind;
use tokio::sync::{broadcast, Mutex};

use crate::audio_topology::AudioTopologyStore;

/// The one target this distribution publishes a chain for.
pub const DEFAULT_TARGET_KEY: &str = "evo-device-audio:default";

/// The source plugin.
pub const SOURCE_PLUGIN: &str = "org.evoframework.playback.mpd";
/// The composition plugin.
pub const COMPOSITION_PLUGIN: &str = "org.evoframework.composition.alsa";
/// The delivery plugin.
pub const DELIVERY_PLUGIN: &str = "org.evoframework.delivery.alsa";

/// The composition mode in which the EQ is in the path.
pub const MODE_EQ_ONLY: &str = "eq_only";

/// The plugin event the composition plugin announces its mode with.
pub const MODE_EVENT: &str = "composition.mode_changed";

/// The pcm the source writes while the EQ is in the path. Defined
/// in `asound.conf` as a fixed-format front on a snd-aloop
/// playback subdevice.
pub const COMP_IN_PCM: &str = "evo_comp_in";

/// The snd-aloop capture subdevice the composition stage reads.
/// It pairs with the playback subdevice behind [`COMP_IN_PCM`].
/// Subdevice 3: the terminus tap holds 7 and multi-room source mode
/// holds 0, so neither is in the way.
pub const COMP_CAPTURE_PCM: &str = "hw:Loopback,1,3";

/// The pcm both chains end on: the one that reaches the DAC.
pub const OUTPUT_PCM: &str = "evo";

/// Where the ALSA configuration lives, for the check that the EQ
/// chain's pcm exists.
#[derive(Debug, Clone)]
pub struct AsoundPaths {
    /// The top-level configuration file.
    pub conf: PathBuf,
    /// The directory of drop-ins it includes.
    pub dropin_dir: PathBuf,
}

impl AsoundPaths {
    /// The paths on a device this distribution installs.
    pub fn system() -> Self {
        Self {
            conf: PathBuf::from("/etc/asound.conf"),
            dropin_dir: PathBuf::from("/etc/asound.d"),
        }
    }

    /// Whether the configuration defines the pcm `name`, in the
    /// top-level file or any drop-in. A file that cannot be read
    /// counts as not defining it.
    pub fn defines_pcm(&self, name: &str) -> bool {
        let mut files = vec![self.conf.clone()];
        if let Ok(entries) = std::fs::read_dir(&self.dropin_dir) {
            let mut found: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "conf"))
                .collect();
            found.sort();
            files.extend(found);
        }
        files
            .iter()
            .filter_map(|f| std::fs::read_to_string(f).ok())
            .any(|text| text_defines_pcm(&text, name))
    }
}

/// Whether ALSA configuration `text` defines the pcm `name`: a line
/// that opens `pcm.<name>` (or its override form `pcm.!<name>`) and is
/// not a comment. `pcm.<name>_other` is another name and does not
/// count.
pub fn text_defines_pcm(text: &str, name: &str) -> bool {
    text.lines().any(|line| {
        // A comment line cannot start with `pcm.`, so it never matches.
        let line = line.trim_start();
        let Some(rest) = line.strip_prefix("pcm.") else {
            return false;
        };
        let rest = rest.strip_prefix('!').unwrap_or(rest);
        let Some(after) = rest.strip_prefix(name) else {
            return false;
        };
        after
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace() || c == '{' || c == '=')
    })
}

/// Whether boot rehydration should re-publish the stored chain for
/// `target_key`.
///
/// The default chain is the controller's: it publishes the chain the
/// composition mode wants when it arms after admission. A stored EQ
/// chain re-published ahead of that hands the composition plugin
/// endpoints for a chain that may no longer be wanted (the mode was
/// changed, or the pcm it needs is gone), and a plugin is never told
/// that a chain was withdrawn: with `eq_only` recorded it would open
/// the output and hold it against the source that has it back. Every
/// other target is rehydrated as before.
pub fn rehydrates(target_key: &str) -> bool {
    target_key != DEFAULT_TARGET_KEY
}

/// Which chain is in force.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainShape {
    /// Source straight into delivery.
    Direct,
    /// Source into the composition stage into delivery.
    EqInPath,
}

impl ChainShape {
    /// The chain a composition mode wants. Only `eq_only` puts the
    /// EQ in the path; anything else, including a mode this build
    /// does not know, is the direct chain.
    pub fn for_mode(mode: &str) -> Self {
        if mode == MODE_EQ_ONLY {
            ChainShape::EqInPath
        } else {
            ChainShape::Direct
        }
    }
}

fn format() -> AudioFormat {
    AudioFormat::Pcm {
        codec: PcmCodec::PcmS16Le,
        rate_hz: 44_100,
        channels: 2,
    }
}

/// The topology for a chain shape.
pub fn chain_topology(shape: ChainShape) -> ActiveAudioTopology {
    let f = format();
    let (display_name, chain) = match shape {
        ChainShape::Direct => (
            "Default delivery chain (44.1kHz/16-bit/stereo)",
            vec![
                ActiveChainStage::Source {
                    plugin: SOURCE_PLUGIN.to_string(),
                    format: f.clone(),
                    endpoint_kind: EndpointKind::AlsaPcm,
                    endpoint_path: PathBuf::from(OUTPUT_PCM),
                },
                ActiveChainStage::Delivery {
                    plugin: DELIVERY_PLUGIN.to_string(),
                    format: f,
                    endpoint_kind: EndpointKind::AlsaPcm,
                    endpoint_path: PathBuf::from(OUTPUT_PCM),
                },
            ],
        ),
        ChainShape::EqInPath => (
            "EQ delivery chain (44.1kHz/16-bit/stereo)",
            vec![
                ActiveChainStage::Source {
                    plugin: SOURCE_PLUGIN.to_string(),
                    format: f.clone(),
                    endpoint_kind: EndpointKind::AlsaPcm,
                    endpoint_path: PathBuf::from(COMP_IN_PCM),
                },
                ActiveChainStage::Composition {
                    plugin: COMPOSITION_PLUGIN.to_string(),
                    mode: MODE_EQ_ONLY.to_string(),
                    format_in: f.clone(),
                    format_out: f.clone(),
                    endpoint_in_kind: EndpointKind::AlsaPcm,
                    endpoint_in_path: PathBuf::from(COMP_CAPTURE_PCM),
                    endpoint_out_kind: EndpointKind::AlsaPcm,
                    endpoint_out_path: PathBuf::from(OUTPUT_PCM),
                },
                ActiveChainStage::Delivery {
                    plugin: DELIVERY_PLUGIN.to_string(),
                    format: f,
                    endpoint_kind: EndpointKind::AlsaPcm,
                    endpoint_path: PathBuf::from(OUTPUT_PCM),
                },
            ],
        ),
    };
    ActiveAudioTopology {
        target_key: DEFAULT_TARGET_KEY.to_string(),
        display_name: display_name.to_string(),
        chain,
        volume_mode: VolumeMode::Software,
        volume_position: Some(0.5),
        volume_db: None,
        bit_perfect: false,
        score: ScoreBreakdown::default(),
        implicit_conversions: Vec::new(),
        warnings: Vec::new(),
    }
}

/// The composition mode a happening announces, if it announces one.
///
/// Only the composition plugin's [`MODE_EVENT`] counts. A payload
/// without a string `mode` announces nothing.
pub fn mode_from_happening(h: &Happening) -> Option<String> {
    match h {
        Happening::PluginEvent {
            plugin,
            event_type,
            payload,
            ..
        } if plugin == COMPOSITION_PLUGIN && event_type == MODE_EVENT => {
            payload.get("mode")?.as_str().map(str::to_string)
        }
        _ => None,
    }
}

#[derive(Debug, Default)]
struct State {
    /// The composition mode last announced. `None` until the
    /// plugin has said.
    mode: Option<String>,
    /// Whether the distribution has said every plugin is admitted.
    armed: bool,
    /// The shape the store holds, as far as this controller knows.
    /// `None` until something has been published, and again after a
    /// publish that failed, so the next change tries again.
    published: Option<ChainShape>,
    /// The EQ chain is wanted but its pcm is missing, so the direct
    /// chain stands in.
    degraded: bool,
}

/// Keeps the published chain matching the composition mode.
pub struct ChainController {
    store: Arc<AudioTopologyStore>,
    asound: AsoundPaths,
    state: Mutex<State>,
}

impl ChainController {
    /// A controller over `store`, checking `asound` for the EQ chain's
    /// pcm. It publishes nothing yet.
    pub fn new(store: Arc<AudioTopologyStore>, asound: AsoundPaths) -> Self {
        Self {
            store,
            asound,
            state: Mutex::new(State::default()),
        }
    }

    /// Whether the EQ chain was wanted but is not being published
    /// because its pcm is missing from the ALSA configuration.
    pub async fn degraded(&self) -> bool {
        self.state.lock().await.degraded
    }

    /// The composition plugin announced `mode`.
    pub async fn observe_mode(&self, mode: &str) {
        let mut st = self.state.lock().await;
        st.mode = Some(mode.to_string());
        self.reconcile(&mut st).await;
    }

    /// Every plugin has admitted: publish the chain for the mode
    /// learnt so far (the direct chain when none has been). From
    /// here on a change of mode publishes at once.
    pub async fn arm(&self) {
        let mut st = self.state.lock().await;
        st.armed = true;
        self.reconcile(&mut st).await;
    }

    /// The shape the controller last published, if it has.
    pub async fn published(&self) -> Option<ChainShape> {
        self.state.lock().await.published
    }

    /// Publish the chain the learnt mode wants if it is not the one
    /// already published. Does nothing before [`Self::arm`].
    async fn reconcile(&self, st: &mut State) {
        if !st.armed {
            return;
        }
        let mut want = ChainShape::for_mode(st.mode.as_deref().unwrap_or(""));
        // The EQ chain points MPD at `evo_comp_in`; with no such pcm MPD
        // opens nothing and the listener hears silence. Check each time
        // it would be published, so a box fixed afterwards gets the
        // chain at the next announcement.
        let was_degraded = st.degraded;
        st.degraded = false;
        if want == ChainShape::EqInPath && !self.asound.defines_pcm(COMP_IN_PCM)
        {
            st.degraded = true;
            if !was_degraded {
                tracing::warn!(
                    pcm = COMP_IN_PCM,
                    conf = %self.asound.conf.display(),
                    "audio chain: eq_only is chosen but the ALSA configuration \
                     does not define the EQ chain's pcm; staying on the direct \
                     chain so the music plays. LOGGING.md §2: the EQ is not \
                     heard until the distribution's asound.conf is installed \
                     with this binary and eq_only is chosen again."
                );
            }
            want = ChainShape::Direct;
        }
        if st.published == Some(want) {
            return;
        }
        tracing::info!(
            shape = ?want,
            mode = st.mode.as_deref().unwrap_or("(not announced)"),
            "audio chain: publishing the chain the composition mode wants"
        );
        match self
            .store
            .publish(chain_topology(want), "evo-device-audio:chain-controller")
            .await
        {
            Ok(_) => st.published = Some(want),
            Err(e) => {
                // Leave `published` empty so the next announcement
                // tries again rather than assuming the store took it.
                st.published = None;
                tracing::warn!(
                    error = %e,
                    shape = ?want,
                    "audio chain: publish failed; the previous chain stays \
                     in force. LOGGING.md §2: the chain may not match the \
                     composition mode until the next announcement."
                );
            }
        }
    }

    /// Follow the bus until it closes, applying every mode the
    /// composition plugin announces.
    pub async fn follow(
        self: Arc<Self>,
        mut rx: broadcast::Receiver<Happening>,
    ) {
        loop {
            match rx.recv().await {
                Ok(h) => {
                    if let Some(mode) = mode_from_happening(&h) {
                        self.observe_mode(&mode).await;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::warn!(
                        missed,
                        "audio chain: the happenings feed lagged; waiting for \
                         the next mode announcement"
                    );
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_routing::{AudioRoutingRuntime, PluginAudioRole};
    use crate::audio_topology::topology_validate;
    use evo::persistence::{MemoryPersistenceStore, PersistenceStore};
    use serde_json::json;
    use std::time::SystemTime;

    fn event(
        plugin: &str,
        event_type: &str,
        payload: serde_json::Value,
    ) -> Happening {
        Happening::PluginEvent {
            plugin: plugin.to_string(),
            event_type: event_type.to_string(),
            payload,
            at: SystemTime::now(),
        }
    }

    struct Rig {
        controller: Arc<ChainController>,
        store: Arc<AudioTopologyStore>,
        routing: Arc<AudioRoutingRuntime>,
        asound: AsoundPaths,
        _dir: tempfile::TempDir,
    }

    const STANZA: &str = "pcm.evo_comp_in {\n    type plug\n    slave.pcm \"hw:Loopback,0,3\"\n}\n";

    /// A rig whose ALSA configuration defines the EQ chain's pcm, as an
    /// installed device does.
    fn rig() -> Rig {
        rig_with(Some(STANZA))
    }

    /// A rig whose top-level ALSA configuration holds `conf`, or does not
    /// exist when `None`.
    fn rig_with(conf: Option<&str>) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let asound = AsoundPaths {
            conf: dir.path().join("asound.conf"),
            dropin_dir: dir.path().join("asound.d"),
        };
        std::fs::create_dir_all(&asound.dropin_dir).unwrap();
        if let Some(c) = conf {
            std::fs::write(&asound.conf, c).unwrap();
        }
        let persistence: Arc<dyn PersistenceStore> =
            Arc::new(MemoryPersistenceStore::new());
        let routing = Arc::new(AudioRoutingRuntime::new());
        let bus = Arc::new(evo::happenings::HappeningBus::new());
        let store = Arc::new(AudioTopologyStore::new(
            persistence,
            Arc::clone(&routing),
            bus,
        ));
        Rig {
            controller: Arc::new(ChainController::new(
                Arc::clone(&store),
                asound.clone(),
            )),
            store,
            routing,
            asound,
            _dir: dir,
        }
    }

    async fn stored_stage_count(r: &Rig) -> Option<usize> {
        r.store
            .get(DEFAULT_TARGET_KEY)
            .await
            .unwrap()
            .map(|t| t.chain.len())
    }

    #[test]
    fn only_eq_only_puts_the_eq_in_the_path() {
        assert_eq!(ChainShape::for_mode("eq_only"), ChainShape::EqInPath);
        assert_eq!(ChainShape::for_mode("passthrough"), ChainShape::Direct);
        assert_eq!(ChainShape::for_mode(""), ChainShape::Direct);
        assert_eq!(
            ChainShape::for_mode("some_future_mode"),
            ChainShape::Direct
        );
    }

    #[test]
    fn the_direct_chain_is_the_default_this_distribution_has_always_published()
    {
        let t = chain_topology(ChainShape::Direct);
        assert_eq!(t.target_key, "evo-device-audio:default");
        assert_eq!(
            t.display_name,
            "Default delivery chain (44.1kHz/16-bit/stereo)"
        );
        assert_eq!(t.chain.len(), 2);
        match (&t.chain[0], &t.chain[1]) {
            (
                ActiveChainStage::Source {
                    plugin: sp,
                    endpoint_path: sep,
                    ..
                },
                ActiveChainStage::Delivery {
                    plugin: dp,
                    endpoint_path: dep,
                    ..
                },
            ) => {
                assert_eq!(sp, "org.evoframework.playback.mpd");
                assert_eq!(dp, "org.evoframework.delivery.alsa");
                assert_eq!(sep, &PathBuf::from("evo"));
                assert_eq!(dep, &PathBuf::from("evo"));
            }
            other => panic!("unexpected chain: {other:?}"),
        }
        assert!(matches!(t.volume_mode, VolumeMode::Software));
        assert_eq!(t.volume_position, Some(0.5));
        topology_validate(&t).unwrap();
    }

    #[test]
    fn the_eq_chain_puts_composition_between_the_source_and_the_dac() {
        let t = chain_topology(ChainShape::EqInPath);
        assert_eq!(t.target_key, DEFAULT_TARGET_KEY, "replaces the default");
        assert_eq!(t.chain.len(), 3);
        match (&t.chain[0], &t.chain[1], &t.chain[2]) {
            (
                ActiveChainStage::Source {
                    endpoint_path: src_out,
                    ..
                },
                ActiveChainStage::Composition {
                    plugin,
                    mode,
                    endpoint_in_path,
                    endpoint_out_path,
                    endpoint_in_kind,
                    endpoint_out_kind,
                    ..
                },
                ActiveChainStage::Delivery {
                    endpoint_path: dlv_in,
                    ..
                },
            ) => {
                assert_eq!(src_out, &PathBuf::from("evo_comp_in"));
                assert_eq!(plugin, "org.evoframework.composition.alsa");
                assert_eq!(mode, "eq_only");
                assert_eq!(endpoint_in_path, &PathBuf::from("hw:Loopback,1,3"));
                assert_eq!(endpoint_out_path, &PathBuf::from("evo"));
                assert_eq!(*endpoint_in_kind, EndpointKind::AlsaPcm);
                assert_eq!(*endpoint_out_kind, EndpointKind::AlsaPcm);
                assert_eq!(
                    dlv_in,
                    &PathBuf::from("evo"),
                    "delivery still reads the DAC's pcm"
                );
            }
            other => panic!("unexpected chain: {other:?}"),
        }
        topology_validate(&t).unwrap();
    }

    #[test]
    fn the_loopback_subdevice_is_not_one_another_stage_holds() {
        // The terminus tap holds subdevice 7 and multi-room source mode
        // holds 0 (see asound.conf and the terminus plugin).
        assert!(COMP_CAPTURE_PCM.ends_with(",3"));
        assert!(!COMP_CAPTURE_PCM.ends_with(",7"));
        assert!(!COMP_CAPTURE_PCM.ends_with(",0"));
    }

    #[test]
    fn only_the_composition_plugins_mode_event_announces_a_mode() {
        let ok = event(
            COMPOSITION_PLUGIN,
            "composition.mode_changed",
            json!({ "v": 1, "mode": "eq_only" }),
        );
        assert_eq!(mode_from_happening(&ok).as_deref(), Some("eq_only"));

        let other_plugin = event(
            "org.evoframework.delivery.alsa",
            "composition.mode_changed",
            json!({ "mode": "eq_only" }),
        );
        assert_eq!(mode_from_happening(&other_plugin), None);

        let other_event = event(
            COMPOSITION_PLUGIN,
            "something.else",
            json!({ "mode": "eq_only" }),
        );
        assert_eq!(mode_from_happening(&other_event), None);

        let no_mode = event(
            COMPOSITION_PLUGIN,
            "composition.mode_changed",
            json!({ "v": 1 }),
        );
        assert_eq!(mode_from_happening(&no_mode), None);

        let not_a_string = event(
            COMPOSITION_PLUGIN,
            "composition.mode_changed",
            json!({ "mode": 3 }),
        );
        assert_eq!(mode_from_happening(&not_a_string), None);
    }

    #[tokio::test]
    async fn nothing_is_published_before_the_distribution_arms_it() {
        let r = rig();
        r.controller.observe_mode("eq_only").await;
        assert_eq!(stored_stage_count(&r).await, None);
        assert_eq!(r.controller.published().await, None);
    }

    #[tokio::test]
    async fn arming_with_no_mode_learnt_publishes_the_direct_chain() {
        let r = rig();
        r.controller.arm().await;
        assert_eq!(stored_stage_count(&r).await, Some(2));
        assert_eq!(r.controller.published().await, Some(ChainShape::Direct));
    }

    #[tokio::test]
    async fn a_device_that_comes_up_in_eq_only_is_armed_with_the_eq_in_the_path(
    ) {
        let r = rig();
        // The composition plugin announced its restored mode while the
        // other plugins were still admitting.
        r.controller.observe_mode("eq_only").await;
        r.controller.arm().await;
        assert_eq!(stored_stage_count(&r).await, Some(3));
        assert_eq!(r.controller.published().await, Some(ChainShape::EqInPath));
    }

    #[tokio::test]
    async fn the_chain_follows_the_mode_both_ways() {
        let r = rig();
        r.controller.arm().await;
        assert_eq!(stored_stage_count(&r).await, Some(2));
        r.controller.observe_mode("eq_only").await;
        assert_eq!(stored_stage_count(&r).await, Some(3));
        r.controller.observe_mode("passthrough").await;
        assert_eq!(stored_stage_count(&r).await, Some(2));
        assert_eq!(r.controller.published().await, Some(ChainShape::Direct));
    }

    #[tokio::test]
    async fn the_composition_stage_is_given_endpoints_only_in_the_eq_chain() {
        let r = rig();
        let composition = r.routing.handle_for_plugin(
            COMPOSITION_PLUGIN,
            PluginAudioRole::Composition,
        );
        let source = r
            .routing
            .handle_for_plugin(SOURCE_PLUGIN, PluginAudioRole::Source);
        r.controller.arm().await;
        assert!(
            composition.composition_endpoints().is_err(),
            "none in the direct chain"
        );
        assert_eq!(source.write_endpoint().unwrap().path, PathBuf::from("evo"));

        r.controller.observe_mode("eq_only").await;
        let ep = composition
            .composition_endpoints()
            .expect("given in the eq chain");
        assert_eq!(ep.input.path, PathBuf::from("hw:Loopback,1,3"));
        assert_eq!(ep.output.path, PathBuf::from("evo"));
        assert_eq!(
            source.write_endpoint().unwrap().path,
            PathBuf::from("evo_comp_in"),
            "the source is pointed at the composition input"
        );

        r.controller.observe_mode("passthrough").await;
        assert_eq!(
            source.write_endpoint().unwrap().path,
            PathBuf::from("evo"),
            "the source is pointed back at the DAC's pcm"
        );
    }

    #[tokio::test]
    async fn an_announcement_that_changes_nothing_publishes_nothing() {
        let r = rig();
        let source = r
            .routing
            .handle_for_plugin(SOURCE_PLUGIN, PluginAudioRole::Source);
        let fired = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let f = Arc::clone(&fired);
        source.on_route_change(Some(Arc::new(move |_| {
            f.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })));
        r.controller.arm().await;
        let after_arm = fired.load(std::sync::atomic::Ordering::SeqCst);
        // The same mode again, and a mode that is also the direct chain:
        // neither must restart the source's output.
        r.controller.observe_mode("passthrough").await;
        r.controller.observe_mode("passthrough").await;
        r.controller.observe_mode("some_future_mode").await;
        assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), after_arm);

        r.controller.observe_mode("eq_only").await;
        let after_eq = fired.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(after_eq, after_arm + 1);
        r.controller.observe_mode("eq_only").await;
        assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), after_eq);
    }

    #[tokio::test]
    async fn the_follower_applies_what_the_bus_carries_and_ignores_the_rest() {
        let r = rig();
        r.controller.arm().await;
        let bus = Arc::new(evo::happenings::HappeningBus::new());
        let rx = bus.subscribe();
        let task = tokio::spawn(Arc::clone(&r.controller).follow(rx));

        // Wait for the follower to be listening, then feed it.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let _ = bus.emit(event(
            "org.evoframework.delivery.alsa",
            "composition.mode_changed",
            json!({ "mode": "eq_only" }),
        ));
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        assert_eq!(
            r.controller.published().await,
            Some(ChainShape::Direct),
            "wrong plugin ignored"
        );

        let _ = bus.emit(event(
            COMPOSITION_PLUGIN,
            "composition.mode_changed",
            json!({ "mode": "eq_only" }),
        ));
        for _ in 0..50 {
            if r.controller.published().await == Some(ChainShape::EqInPath) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(r.controller.published().await, Some(ChainShape::EqInPath));
        task.abort();
    }

    // ----- the asound pairing check ------------------------------

    #[test]
    fn a_pcm_definition_is_recognised_in_its_real_forms_only() {
        let name = "evo_comp_in";
        for yes in [
            "pcm.evo_comp_in {\n",
            "pcm.evo_comp_in{\n",
            "  pcm.evo_comp_in {\n",
            "pcm.!evo_comp_in {\n",
            "pcm.evo_comp_in = \"x\"\n",
            "pcm.evo_comp_in\n",
            "# header\npcm.evo {\n}\npcm.evo_comp_in {\n}\n",
        ] {
            assert!(text_defines_pcm(yes, name), "{yes:?}");
        }
        for no in [
            "",
            "# pcm.evo_comp_in {\n",
            "   # pcm.evo_comp_in {\n",
            "pcm.evo_comp_in_other {\n",
            "pcm.evo_comp_inx {\n",
            "pcm.evo {\n    slave.pcm \"evo_comp_in\"\n}\n",
            "ctl.evo_comp_in {\n",
            "xpcm.evo_comp_in {\n",
        ] {
            assert!(!text_defines_pcm(no, name), "{no:?}");
        }
    }

    #[test]
    fn the_pcm_is_found_in_the_conf_or_in_a_drop_in_and_not_otherwise() {
        let r = rig_with(Some("pcm.evo {\n}\n"));
        assert!(!r.asound.defines_pcm(COMP_IN_PCM));
        std::fs::write(r.asound.dropin_dir.join("zz-extra.conf"), STANZA)
            .unwrap();
        assert!(r.asound.defines_pcm(COMP_IN_PCM), "a drop-in counts");
        std::fs::write(r.asound.dropin_dir.join("notes.txt"), "pcm.other {\n")
            .unwrap();
        assert!(
            !r.asound.defines_pcm("other"),
            "only .conf drop-ins are read"
        );
        let none = rig_with(None);
        assert!(!none.asound.defines_pcm(COMP_IN_PCM), "no file, no pcm");
    }

    /// A box whose binary is newer than its asound.conf keeps playing:
    /// the EQ chain is not published, the direct one stays, and the
    /// controller says it is degraded.
    #[tokio::test]
    async fn without_the_pcm_the_eq_chain_is_not_published_and_the_music_plays()
    {
        let r = rig_with(Some("pcm.evo {\n}\n"));
        let source = r
            .routing
            .handle_for_plugin(SOURCE_PLUGIN, PluginAudioRole::Source);
        r.controller.arm().await;
        r.controller.observe_mode("eq_only").await;
        assert_eq!(stored_stage_count(&r).await, Some(2), "still direct");
        assert_eq!(r.controller.published().await, Some(ChainShape::Direct));
        assert!(r.controller.degraded().await);
        assert_eq!(
            source.write_endpoint().unwrap().path,
            PathBuf::from("evo"),
            "MPD still writes the DAC's pcm"
        );
    }

    /// A box that comes up in eq_only without the pcm is armed direct,
    /// not silent.
    #[tokio::test]
    async fn arming_in_eq_only_without_the_pcm_is_direct() {
        let r = rig_with(Some("pcm.evo {\n}\n"));
        r.controller.observe_mode("eq_only").await;
        r.controller.arm().await;
        assert_eq!(stored_stage_count(&r).await, Some(2));
        assert!(r.controller.degraded().await);
    }

    /// Once the pcm is installed the next announcement gets the EQ
    /// chain; if it is taken away again the chain goes back to direct.
    #[tokio::test]
    async fn the_chain_follows_the_pcm_as_it_comes_and_goes() {
        let r = rig_with(Some("pcm.evo {\n}\n"));
        r.controller.arm().await;
        r.controller.observe_mode("eq_only").await;
        assert_eq!(stored_stage_count(&r).await, Some(2));
        assert!(r.controller.degraded().await);

        std::fs::write(&r.asound.conf, format!("pcm.evo {{\n}}\n{STANZA}"))
            .unwrap();
        r.controller.observe_mode("eq_only").await;
        assert_eq!(stored_stage_count(&r).await, Some(3), "now the EQ chain");
        assert!(!r.controller.degraded().await);

        std::fs::write(&r.asound.conf, "pcm.evo {\n}\n").unwrap();
        r.controller.observe_mode("eq_only").await;
        assert_eq!(stored_stage_count(&r).await, Some(2), "back to direct");
        assert!(r.controller.degraded().await);
    }

    /// The check applies to the EQ chain only: direct needs nothing.
    #[tokio::test]
    async fn the_direct_chain_needs_no_pcm_and_is_never_degraded() {
        let r = rig_with(None);
        r.controller.arm().await;
        r.controller.observe_mode("passthrough").await;
        assert_eq!(stored_stage_count(&r).await, Some(2));
        assert!(!r.controller.degraded().await);
    }

    #[test]
    fn the_default_chain_is_the_controllers_and_every_other_target_is_rehydrated(
    ) {
        assert!(!rehydrates(DEFAULT_TARGET_KEY));
        assert!(!rehydrates("evo-device-audio:default"));
        assert!(rehydrates("usb:vid=0x21b4,pid=0x0096"));
        assert!(rehydrates("evo-device-audio:default-2"));
        assert!(rehydrates(""));
    }
}
