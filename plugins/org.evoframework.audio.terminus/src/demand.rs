// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Spectrum-demand control plane — subject + verb + producer feed.
//!
//! # Purpose
//!
//! The operator's `ui.visualizer.{enabled,bin_count,channel_mode}`
//! settings drive the terminus producer via the `audio_playback_
//! spectrum_demand` subject. The subject carries only production-
//! affecting fields (renderer choices like preset/palette never
//! appear here); every seat on the same device reads the same
//! demand, so the producer serves ONE FFT shape regardless of
//! seat count.
//!
//! The framework's `audio.spectrum.set_demand` verb writes the
//! demand: evo-ui-runtime's settings-patch bridge derives the
//! demand payload from the operator's settings-store write and
//! calls this verb. Terminus receives, validates, applies to the
//! shared store, republishes the subject, and broadcasts the
//! change on a watch channel the capture loop consumes as its
//! outer-gate feed.
//!
//! # Wire shape (v1)
//!
//! ```text
//! {
//!   "v":              1,
//!   "enabled":        bool,
//!   "bins":           u32,   // one of 32, 64, 128, 256
//!   "channels":       u32,   // one of 1, 2
//!   "rate_hz_target": u32,   // producer emit throttle; typical 30
//!   "updated_at_ms":  u64
//! }
//! ```
//!
//! # Invariants
//!
//! - `enabled` is the single production-truth field. `preset=off`
//!   folds to `enabled=false` at the settings layer; the framework
//!   never sees `preset`.
//! - `bins ∈ {32, 64, 128, 256}` — refused with Permanent otherwise.
//! - `channels ∈ {1, 2}` — refused with Permanent otherwise.
//! - `rate_hz_target` clamped to `[1, 60]`; the capture loop's
//!   emit throttle honours the clamped value.
//! - `frequency_scale ∈ {log, mel, linear}` accepts every
//!   documented `bins` value. Honesty at high log bin counts is
//!   the analyser's job (FFT_WINDOW=16384 + anti-clone banking),
//!   not a product refuse/clamp on the wire.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use evo_plugin_sdk::contract::{
    ExternalAddressing, PluginError, Request, Response, SubjectAnnouncement,
    SubjectAnnouncer, SubjectQuerier, SubjectStateSubscriber,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::watch;

const PLUGIN_NAME: &str = "org.evoframework.audio.terminus";

const SUBJECT_TYPE: &str = "audio_playback_spectrum_demand";
const ADDRESSING_SCHEME: &str = "evo.audio.playback";
const ADDRESSING_VALUE: &str = "spectrum_demand";

pub(crate) const VERB_SET_DEMAND: &str = "audio.spectrum.set_demand";
pub(crate) const DEMAND_PAYLOAD_VERSION: u32 = 1;

/// Frequency-bin spacing across the analyser's `[20, 20000]` Hz
/// window. Determines how the operator's `bins` output slots are
/// distributed across the audible band on the wire.
///
/// - `Log`: equal log-ratio spacing. Music-analyser default;
///   allocates ~37% of display width to 20–250 Hz — matches
///   music-genre energy distribution and industry meters.
/// - `Mel`: equal mel-scale spacing. Perceptual-loudness bank
///   the plugin originally shipped; allocates ~8% of display
///   width to 20–250 Hz — better than linear but noticeably
///   left-parked for music-heavy content.
/// - `Linear`: equal Hz spacing. Raw-FFT / diagnostics shape;
///   allocates ~1% of display width to 20–250 Hz — mostly
///   useful for engineering visualisation of raw spectrum.
///
/// Wire form is lowercase snake (`"log"` / `"mel"` / `"linear"`)
/// via serde's `rename_all = "lowercase"`; enum-validated at the
/// verb parse stage.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum FrequencyScale {
    /// Equal log-ratio spacing. Default; matches music-analyser
    /// convention.
    #[default]
    Log,
    /// Equal mel-scale spacing. Prior default (perceptual bank).
    Mel,
    /// Equal Hz spacing. Diagnostics only.
    Linear,
}

impl FrequencyScale {
    /// Lowercase wire form (matches the serde encoding).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Log => "log",
            Self::Mel => "mel",
            Self::Linear => "linear",
        }
    }

    /// Parse a wire-form string. `None` on unknown value — the
    /// rehydrate path treats that as "roll back to
    /// `disabled_default`" (defensive: a corrupted persisted row
    /// cannot resurrect a scale this build does not understand).
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "log" => Some(Self::Log),
            "mel" => Some(Self::Mel),
            "linear" => Some(Self::Linear),
            _ => None,
        }
    }
}

/// Producer-plane demand shape. Every field is production-
/// affecting; renderer-only choices never appear here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SpectrumDemand {
    /// `true` opens the producer's capture path; `false` parks
    /// it identically to `TransportGate::NotPlaying` (ALSA PCM
    /// released, no FFT compute, no emit).
    pub enabled: bool,
    /// Output bin count. Constrained to `{32, 64, 128, 256}` at
    /// parse time; the analyser rebuilds when this changes
    /// mid-play.
    pub bins: u32,
    /// Channel mode. `1` = mono (producer collapses L+R at the
    /// filterbank stage); `2` = stereo (producer emits both
    /// channels).
    pub channels: u32,
    /// Emit throttle target in Hz. Clamped to `[1, 60]`.
    pub rate_hz_target: u32,
    /// Frequency-bin spacing across `[20, 20000]` Hz. Default
    /// `Log`; the analyser rebuilds when this changes mid-play.
    /// Absent on the wire (older UI runtime bridge that has not
    /// been upgraded) parses to `Log` per the spectrum-frequency-
    /// scale ownership audit.
    pub frequency_scale: FrequencyScale,
    /// Wall-clock ms at last apply. Zero before the first apply.
    pub updated_at_ms: u64,
}

impl SpectrumDemand {
    /// Default: disabled, 256 bins, mono, 30 Hz, log spacing.
    /// Reproduces the operator-visible baseline the visualiser
    /// carries when the operator has never opened the settings
    /// surface — no spectrum activity on-device, ready to enable
    /// on first opt-in without a plugin restart. Dense log×256
    /// is the music-analyser product default once the STFT +
    /// anti-clone analyser is live.
    pub fn disabled_default() -> Self {
        Self {
            enabled: false,
            bins: 256,
            channels: 1,
            rate_hz_target: 30,
            frequency_scale: FrequencyScale::Log,
            updated_at_ms: 0,
        }
    }
}

/// The parsed request payload for `audio.spectrum.set_demand`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetDemandRequest {
    #[serde(default = "default_v")]
    v: u32,
    enabled: bool,
    bins: u32,
    channels: u32,
    #[serde(default = "default_rate_hz_target")]
    rate_hz_target: u32,
    /// Frequency-bin spacing. Absent (older UI runtime bridge)
    /// defaults to `Log` per the ownership audit. Unknown enum
    /// value → serde error → `PluginError::Permanent` at the
    /// verb boundary (same class as bad bins).
    #[serde(default)]
    frequency_scale: FrequencyScale,
}

fn default_v() -> u32 {
    DEMAND_PAYLOAD_VERSION
}

fn default_rate_hz_target() -> u32 {
    30
}

/// Wall-clock ms since the UNIX epoch. `SystemTime` cannot fail
/// in practice; the zero fallback preserves the invariant that
/// `updated_at_ms == 0` means "no apply has happened yet".
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Shared demand store. Cloneable — the plugin holds one, the
/// capture loop holds one, verb dispatchers hold one.
///
/// Every mutation goes through [`Self::apply`] which validates,
/// updates the current value, broadcasts on the watch channel,
/// and republishes the subject. Readers use [`Self::current`]
/// for a snapshot or [`Self::watch`] for a change-driven feed.
#[derive(Clone)]
pub struct SpectrumDemandStore {
    tx: watch::Sender<SpectrumDemand>,
    subjects: Arc<dyn SubjectAnnouncer>,
}

impl SpectrumDemandStore {
    /// Construct on plugin load. Reads the last-persisted demand
    /// from the framework's durable subject-state mirror first;
    /// only falls back to `disabled_default` when no prior state
    /// exists (first-ever boot on this device, or the state row
    /// was cleared). Then announces the subject with whatever
    /// value we ended up seeded to — an idempotent overwrite when
    /// rehydrated (same value already in the registry), a first-
    /// write when we defaulted.
    ///
    /// Operator intent (the `enabled` flag) survives reboot +
    /// terminus reload without the UI having to re-push. UI
    /// reassert stays valuable as drift-detection against a
    /// concurrent operator override or a manual demand reset,
    /// but it is no longer the sole memory of what the operator
    /// asked for.
    pub async fn announce_initial(
        subjects: Arc<dyn SubjectAnnouncer>,
        subscriber: Arc<dyn SubjectStateSubscriber>,
        querier: Arc<dyn SubjectQuerier>,
    ) -> Self {
        let seed = rehydrate_or_default(&*subscriber, &*querier).await;
        let (tx, _rx) = watch::channel(seed);
        let store = Self { tx, subjects };
        store.announce_initial_state().await;
        store
    }

    async fn announce_initial_state(&self) {
        let addressing =
            ExternalAddressing::new(ADDRESSING_SCHEME, ADDRESSING_VALUE);
        let announcement =
            SubjectAnnouncement::new(SUBJECT_TYPE, vec![addressing])
                .with_state(render_envelope(&self.current()));
        if let Err(e) = self.subjects.announce(announcement).await {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "audio_playback_spectrum_demand subject announce failed"
            );
        }
    }

    /// Snapshot the current demand. Cheap; the watch channel's
    /// borrow-with-clone is O(1) for a struct this small.
    pub fn current(&self) -> SpectrumDemand {
        *self.tx.borrow()
    }

    /// A watch::Receiver on the demand. The capture loop consumes
    /// this: `changed().await` unblocks on any demand mutation;
    /// `borrow()` reads the current value.
    #[cfg_attr(not(feature = "alsa-substrate"), allow(dead_code))]
    pub fn watch(&self) -> watch::Receiver<SpectrumDemand> {
        self.tx.subscribe()
    }

    /// Handle the `audio.spectrum.set_demand` verb.
    ///
    /// Parses + validates the payload; refuses out-of-range shapes
    /// with structured Permanent errors so evo-ui-runtime's bridge
    /// surfaces the class synchronously rather than shipping a
    /// silently-clamped value the operator did not choose. On
    /// success, updates the current demand, broadcasts on the
    /// watch channel, and republishes the subject.
    pub async fn handle_set_demand(
        &self,
        req: &Request,
    ) -> Result<Response, PluginError> {
        let parsed: SetDemandRequest = serde_json::from_slice(&req.payload)
            .map_err(|e| {
                PluginError::Permanent(format!(
                    "audio.spectrum.set_demand payload: {e}"
                ))
            })?;
        if parsed.v != DEMAND_PAYLOAD_VERSION {
            return Err(PluginError::Permanent(format!(
                "audio.spectrum.set_demand: unsupported payload version \
                 {} (this build understands {})",
                parsed.v, DEMAND_PAYLOAD_VERSION
            )));
        }
        validate_bins(parsed.bins)?;
        validate_channels(parsed.channels)?;
        let rate_hz_target = parsed.rate_hz_target.clamp(1, 60);
        let next = SpectrumDemand {
            enabled: parsed.enabled,
            bins: parsed.bins,
            channels: parsed.channels,
            rate_hz_target,
            frequency_scale: parsed.frequency_scale,
            updated_at_ms: now_ms(),
        };
        self.apply(next).await;
        let body = serde_json::json!({
            "v":       DEMAND_PAYLOAD_VERSION,
            "applied": render_envelope(&next),
        });
        let bytes = serde_json::to_vec(&body).map_err(|e| {
            PluginError::Permanent(format!(
                "audio.spectrum.set_demand response JSON encode: {e}"
            ))
        })?;
        Ok(Response::for_request(req, bytes))
    }

    /// Apply a validated demand. Broadcasts to every watch
    /// consumer + republishes the subject. `send_replace` is
    /// used so consumers see the transition even when the new
    /// value equals the old (defensive against a scenario where
    /// evo-ui-runtime pushes a duplicate on reconnect).
    async fn apply(&self, next: SpectrumDemand) {
        let _ = self.tx.send_replace(next);
        let addressing =
            ExternalAddressing::new(ADDRESSING_SCHEME, ADDRESSING_VALUE);
        if let Err(e) = self
            .subjects
            .update_state(addressing, render_envelope(&next))
            .await
        {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                error = %e,
                "audio_playback_spectrum_demand update_state failed"
            );
        }
        tracing::info!(
            plugin = PLUGIN_NAME,
            enabled = next.enabled,
            bins = next.bins,
            channels = next.channels,
            rate_hz_target = next.rate_hz_target,
            frequency_scale = next.frequency_scale.as_str(),
            "spectrum demand applied"
        );
    }
}

fn validate_bins(bins: u32) -> Result<(), PluginError> {
    if matches!(bins, 32 | 64 | 128 | 256) {
        Ok(())
    } else {
        Err(PluginError::Permanent(format!(
            "audio.spectrum.set_demand: bins={bins} outside enum \
             {{32, 64, 128, 256}}"
        )))
    }
}

fn validate_channels(channels: u32) -> Result<(), PluginError> {
    if matches!(channels, 1 | 2) {
        Ok(())
    } else {
        Err(PluginError::Permanent(format!(
            "audio.spectrum.set_demand: channels={channels} outside enum \
             {{1, 2}}"
        )))
    }
}

/// Assemble the wire envelope for the demand subject.
fn render_envelope(d: &SpectrumDemand) -> serde_json::Value {
    json!({
        "v":               DEMAND_PAYLOAD_VERSION,
        "enabled":         d.enabled,
        "bins":            d.bins,
        "channels":        d.channels,
        "rate_hz_target":  d.rate_hz_target,
        "frequency_scale": d.frequency_scale.as_str(),
        "updated_at_ms":   d.updated_at_ms,
    })
}

/// Reverse of [`render_envelope`] — read a `SpectrumDemand` out of
/// a wire envelope. Returns `None` when the envelope is malformed,
/// carries an unsupported version, or fails the same enum
/// validators the set_demand verb enforces (bins ∈ {32, 64, 128,
/// 256}, channels ∈ {1, 2}, frequency_scale ∈ {log, mel, linear}).
/// Defensive: a rehydrated envelope from an older or forward build
/// with an out-of-range field should NOT resurrect an invalid
/// demand — fall through to `disabled_default` instead.
///
/// `frequency_scale` is treated additive-compatibly: an envelope
/// persisted by a build predating the spectrum-frequency-scale
/// audit lacks the field, in which case the rehydrated demand
/// resolves to `FrequencyScale::Log` (the same default a fresh
/// `disabled_default` would carry, and the audit's migration
/// rule). An envelope that carries the field with an unknown
/// value (`"octave"`, mistyped, forward build) fails hard the
/// same way an out-of-range `bins` fails hard.
fn parse_envelope(v: &serde_json::Value) -> Option<SpectrumDemand> {
    let version = v.get("v")?.as_u64()? as u32;
    if version != DEMAND_PAYLOAD_VERSION {
        return None;
    }
    let enabled = v.get("enabled")?.as_bool()?;
    let bins = v.get("bins")?.as_u64()? as u32;
    if validate_bins(bins).is_err() {
        return None;
    }
    let channels = v.get("channels")?.as_u64()? as u32;
    if validate_channels(channels).is_err() {
        return None;
    }
    let rate_hz_target = v.get("rate_hz_target")?.as_u64()? as u32;
    let rate_hz_target = rate_hz_target.clamp(1, 60);
    let frequency_scale = match v.get("frequency_scale") {
        None => FrequencyScale::Log,
        Some(scale_val) => {
            // Present-but-non-string, or present-but-unknown
            // enum value, both fail hard (return None → caller
            // falls back to `disabled_default`). Absent is the
            // only migration-friendly case.
            FrequencyScale::from_wire(scale_val.as_str()?)?
        }
    };
    let updated_at_ms = v.get("updated_at_ms")?.as_u64()?;
    Some(SpectrumDemand {
        enabled,
        bins,
        channels,
        rate_hz_target,
        frequency_scale,
        updated_at_ms,
    })
}

/// Rehydrate the demand from the framework's durable subject-
/// state mirror. Returns `disabled_default` when there is no
/// prior state (never-applied) or when the read errors — the
/// conservative choice: silence beats a stale-envelope-driven
/// FFT run.
///
/// The framework's boot path runs `rehydrate_states_from` before
/// plugin admission opens, so by the time this function is
/// called the durable state is already in the registry and
/// `current_state` returns whatever the last `apply` persisted.
async fn rehydrate_or_default(
    subscriber: &dyn SubjectStateSubscriber,
    querier: &dyn SubjectQuerier,
) -> SpectrumDemand {
    let addressing =
        ExternalAddressing::new(ADDRESSING_SCHEME, ADDRESSING_VALUE);
    let canonical_id = match querier.resolve_addressing(addressing).await {
        Ok(Some(id)) => id,
        Ok(None) => {
            tracing::info!(
                plugin = PLUGIN_NAME,
                source = "no-prior-state-use-default",
                "spectrum-demand initial state: no prior addressing \
                 in registry (first-ever boot on this device)"
            );
            return SpectrumDemand::disabled_default();
        }
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                source = "no-prior-state-use-default",
                "spectrum-demand initial state: resolve_addressing errored"
            );
            return SpectrumDemand::disabled_default();
        }
    };
    match subscriber.current_state(canonical_id.clone()).await {
        Ok(Some(state)) => match parse_envelope(&state) {
            Some(d) => {
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    source = "rehydrate-from-mirror",
                    enabled = d.enabled,
                    bins = d.bins,
                    channels = d.channels,
                    rate_hz_target = d.rate_hz_target,
                    updated_at_ms = d.updated_at_ms,
                    "spectrum-demand initial state: rehydrated from durable mirror"
                );
                d
            }
            None => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    source = "no-prior-state-use-default",
                    canonical_id = %canonical_id,
                    "spectrum-demand initial state: persisted envelope \
                     did not parse; falling back to disabled default"
                );
                SpectrumDemand::disabled_default()
            }
        },
        Ok(None) => {
            tracing::info!(
                plugin = PLUGIN_NAME,
                source = "no-prior-state-use-default",
                canonical_id = %canonical_id,
                "spectrum-demand initial state: subject known but no \
                 prior state (never applied on this device)"
            );
            SpectrumDemand::disabled_default()
        }
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                source = "no-prior-state-use-default",
                canonical_id = %canonical_id,
                "spectrum-demand initial state: current_state read errored"
            );
            SpectrumDemand::disabled_default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_default_matches_documented_shape() {
        let d = SpectrumDemand::disabled_default();
        assert!(!d.enabled);
        assert_eq!(d.bins, 256);
        assert_eq!(d.channels, 1);
        assert_eq!(d.rate_hz_target, 30);
        // Dense log×256 is the music-analyser product default.
        assert_eq!(d.frequency_scale, FrequencyScale::Log);
        assert_eq!(d.updated_at_ms, 0);
    }

    #[test]
    fn bins_enum_accepts_documented_values_and_refuses_others() {
        for good in [32u32, 64, 128, 256] {
            assert!(validate_bins(good).is_ok(), "{good} should be accepted");
        }
        for bad in [0u32, 1, 16, 100, 512, u32::MAX] {
            assert!(validate_bins(bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn channels_enum_accepts_documented_values_and_refuses_others() {
        for good in [1u32, 2] {
            assert!(validate_channels(good).is_ok());
        }
        for bad in [0u32, 3, 4, 5, 6, u32::MAX] {
            assert!(validate_channels(bad).is_err());
        }
    }

    #[test]
    fn rate_hz_target_default_is_30() {
        assert_eq!(default_rate_hz_target(), 30);
    }

    #[test]
    fn envelope_carries_every_wire_field() {
        let d = SpectrumDemand {
            enabled: true,
            bins: 128,
            channels: 2,
            rate_hz_target: 30,
            frequency_scale: FrequencyScale::Mel,
            updated_at_ms: 1_720_000_000_000,
        };
        let env = render_envelope(&d);
        assert_eq!(env["v"], DEMAND_PAYLOAD_VERSION);
        assert_eq!(env["enabled"], true);
        assert_eq!(env["bins"], 128);
        assert_eq!(env["channels"], 2);
        assert_eq!(env["rate_hz_target"], 30);
        assert_eq!(env["frequency_scale"], "mel");
        assert_eq!(env["updated_at_ms"], 1_720_000_000_000_u64);
    }

    #[test]
    fn envelope_encodes_every_frequency_scale_as_lowercase_wire_form() {
        for (scale, wire) in [
            (FrequencyScale::Log, "log"),
            (FrequencyScale::Mel, "mel"),
            (FrequencyScale::Linear, "linear"),
        ] {
            let d = SpectrumDemand {
                frequency_scale: scale,
                ..SpectrumDemand::disabled_default()
            };
            assert_eq!(render_envelope(&d)["frequency_scale"], wire);
        }
    }

    #[test]
    fn frequency_scale_from_wire_accepts_documented_values_and_refuses_others()
    {
        assert_eq!(FrequencyScale::from_wire("log"), Some(FrequencyScale::Log));
        assert_eq!(FrequencyScale::from_wire("mel"), Some(FrequencyScale::Mel));
        assert_eq!(
            FrequencyScale::from_wire("linear"),
            Some(FrequencyScale::Linear)
        );
        // No aliases per the audit — `octave`, `logarithmic`,
        // `fft` all fail hard.
        for bad in ["octave", "logarithmic", "fft", "LOG", "", " log "] {
            assert!(
                FrequencyScale::from_wire(bad).is_none(),
                "must refuse '{bad}'"
            );
        }
    }

    #[test]
    fn set_demand_request_rejects_unknown_field() {
        // `deny_unknown_fields` on the parsed struct catches
        // typos on the runtime-bridge side synchronously
        // rather than silently discarding the parameter.
        let json = r#"{
            "v": 1,
            "enabled": true,
            "bins": 64,
            "channels": 1,
            "rate_hz_target": 30,
            "preset": "bars"
        }"#;
        let err = serde_json::from_str::<SetDemandRequest>(json)
            .expect_err("unknown field must refuse");
        let msg = format!("{err}");
        assert!(
            msg.contains("preset") || msg.contains("unknown"),
            "refusal must name the unknown field, got: {msg}"
        );
    }

    #[test]
    fn parse_envelope_roundtrips_a_rendered_envelope() {
        // Every apply persists via render_envelope; every load
        // rehydrates via parse_envelope. The invariant that must
        // hold: parse(render(d)) == d for every legal d.
        //
        // Every documented scale × bins combination round-trips,
        // including dense log×256 (analyser honesty, not wire
        // refuse, is the product contract).
        for d in [
            SpectrumDemand::disabled_default(),
            SpectrumDemand {
                enabled: true,
                bins: 256,
                channels: 2,
                rate_hz_target: 45,
                frequency_scale: FrequencyScale::Log,
                updated_at_ms: 1_720_000_000_000,
            },
            SpectrumDemand {
                enabled: false,
                bins: 256,
                channels: 1,
                rate_hz_target: 1,
                frequency_scale: FrequencyScale::Mel,
                updated_at_ms: u64::MAX,
            },
            SpectrumDemand {
                enabled: true,
                bins: 32,
                channels: 2,
                rate_hz_target: 60,
                frequency_scale: FrequencyScale::Linear,
                updated_at_ms: 1_234,
            },
        ] {
            let env = render_envelope(&d);
            let parsed =
                parse_envelope(&env).expect("rendered envelope must parse");
            assert_eq!(parsed, d, "roundtrip must preserve every field");
        }
    }

    #[test]
    fn set_demand_request_accepts_log_at_every_documented_bins() {
        for bins in [32u32, 64, 128, 256] {
            let json = format!(
                r#"{{"v":1,"enabled":true,"bins":{bins},"channels":1,\
                 "rate_hz_target":30,"frequency_scale":"log"}}"#
            );
            let json = json.replace('\\', "");
            let req = serde_json::from_str::<SetDemandRequest>(&json)
                .expect("valid JSON parses");
            validate_bins(req.bins).expect("bins enum must accept");
            assert_eq!(req.frequency_scale, FrequencyScale::Log);
        }
    }

    #[test]
    fn parse_envelope_accepts_persisted_log_mel_linear_at_bins_256() {
        for scale in ["log", "mel", "linear"] {
            let env = json!({
                "v": DEMAND_PAYLOAD_VERSION,
                "enabled": true,
                "bins": 256,
                "channels": 1,
                "rate_hz_target": 30,
                "frequency_scale": scale,
                "updated_at_ms": 1_000,
            });
            let parsed = parse_envelope(&env).unwrap_or_else(|| {
                panic!(
                    "persisted {scale} × 256 must rehydrate at parse boundary"
                )
            });
            assert_eq!(parsed.bins, 256);
        }
    }

    #[test]
    fn parse_envelope_absent_frequency_scale_defaults_to_log() {
        // Migration path: an envelope persisted by a build
        // predating the frequency-scale field must resurrect as
        // Log (the new default) rather than fail. The other
        // fields still populate normally.
        let env = json!({
            "v": DEMAND_PAYLOAD_VERSION,
            "enabled": true,
            "bins": 64,
            "channels": 1,
            "rate_hz_target": 30,
            "updated_at_ms": 1_000,
        });
        let parsed = parse_envelope(&env).expect("absent scale must parse");
        assert_eq!(parsed.frequency_scale, FrequencyScale::Log);
    }

    #[test]
    fn parse_envelope_refuses_unknown_frequency_scale() {
        // Present-but-unknown scale value fails hard, same class
        // as an out-of-range bins. A corrupted or forward-build
        // envelope with "octave" MUST NOT resurrect an invalid
        // demand — the caller falls back to disabled_default.
        for bad_scale in ["octave", "logarithmic", "fft", "", "LOG"] {
            let env = json!({
                "v": DEMAND_PAYLOAD_VERSION,
                "enabled": true,
                "bins": 64,
                "channels": 1,
                "rate_hz_target": 30,
                "frequency_scale": bad_scale,
                "updated_at_ms": 1_000,
            });
            assert!(
                parse_envelope(&env).is_none(),
                "must refuse frequency_scale='{bad_scale}'"
            );
        }
    }

    #[test]
    fn parse_envelope_refuses_non_string_frequency_scale() {
        // A numeric frequency_scale is a forward-build shape
        // (someone changed the wire form). Fail hard rather than
        // silently coerce.
        let env = json!({
            "v": DEMAND_PAYLOAD_VERSION,
            "enabled": true,
            "bins": 64,
            "channels": 1,
            "rate_hz_target": 30,
            "frequency_scale": 0,
            "updated_at_ms": 1_000,
        });
        assert!(parse_envelope(&env).is_none());
    }

    #[test]
    fn set_demand_request_absent_frequency_scale_defaults_to_log() {
        // Wire compatibility: an older UI runtime bridge that
        // has not yet been upgraded still lands enabled/bins/
        // channels/rate. The absent frequency_scale field
        // resolves to Log at the verb boundary — matches the
        // audit's migration rule.
        let json = r#"{
            "v": 1,
            "enabled": true,
            "bins": 64,
            "channels": 1,
            "rate_hz_target": 30
        }"#;
        let req = serde_json::from_str::<SetDemandRequest>(json)
            .expect("absent scale must parse");
        assert_eq!(req.frequency_scale, FrequencyScale::Log);
    }

    #[test]
    fn set_demand_request_accepts_every_scale_and_refuses_unknown() {
        for good in ["log", "mel", "linear"] {
            let json = format!(
                r#"{{"v":1,"enabled":true,"bins":64,"channels":1,\
                 "rate_hz_target":30,"frequency_scale":"{good}"}}"#
            );
            let json = json.replace('\\', "");
            let req = serde_json::from_str::<SetDemandRequest>(&json)
                .unwrap_or_else(|e| panic!("{good} must parse: {e}"));
            assert_eq!(req.frequency_scale.as_str(), good);
        }
        for bad in ["octave", "logarithmic", "fft", "", "LOG"] {
            let json = format!(
                r#"{{"v":1,"enabled":true,"bins":64,"channels":1,\
                 "rate_hz_target":30,"frequency_scale":"{bad}"}}"#
            );
            let json = json.replace('\\', "");
            let err = serde_json::from_str::<SetDemandRequest>(&json)
                .expect_err(&format!("'{bad}' must refuse"));
            let msg = format!("{err}");
            assert!(
                msg.contains("frequency_scale")
                    || msg.contains(bad)
                    || msg.contains("unknown"),
                "refusal must name the field or value, got: {msg}"
            );
        }
    }

    #[test]
    fn parse_envelope_refuses_unsupported_version() {
        // A forward-build envelope with v=2 must NOT resurrect
        // itself on a v=1 build; fall through to disabled_default
        // so the operator does not see a state the current build
        // cannot honour.
        let env = json!({
            "v": 2,
            "enabled": true,
            "bins": 64,
            "channels": 1,
            "rate_hz_target": 30,
            "updated_at_ms": 1_000,
        });
        assert!(parse_envelope(&env).is_none());
    }

    #[test]
    fn parse_envelope_refuses_out_of_range_bins() {
        // A corrupted persisted row with bins=100 must NOT
        // resurrect an invalid demand — the set_demand verb
        // would refuse this shape, and rehydrate must not
        // sneak it in by the back door.
        let env = json!({
            "v": DEMAND_PAYLOAD_VERSION,
            "enabled": true,
            "bins": 100,
            "channels": 1,
            "rate_hz_target": 30,
            "updated_at_ms": 1_000,
        });
        assert!(parse_envelope(&env).is_none());
    }

    #[test]
    fn parse_envelope_refuses_out_of_range_channels() {
        let env = json!({
            "v": DEMAND_PAYLOAD_VERSION,
            "enabled": true,
            "bins": 64,
            "channels": 5,
            "rate_hz_target": 30,
            "updated_at_ms": 1_000,
        });
        assert!(parse_envelope(&env).is_none());
    }

    #[test]
    fn parse_envelope_clamps_rate_hz_target() {
        // Same [1, 60] clamp the set_demand verb applies. A
        // persisted 0 or 500 rate must not resurrect verbatim.
        let low = json!({
            "v": DEMAND_PAYLOAD_VERSION,
            "enabled": true,
            "bins": 64,
            "channels": 1,
            "rate_hz_target": 0,
            "updated_at_ms": 1_000,
        });
        assert_eq!(parse_envelope(&low).unwrap().rate_hz_target, 1);
        let high = json!({
            "v": DEMAND_PAYLOAD_VERSION,
            "enabled": true,
            "bins": 64,
            "channels": 1,
            "rate_hz_target": 500,
            "updated_at_ms": 1_000,
        });
        assert_eq!(parse_envelope(&high).unwrap().rate_hz_target, 60);
    }

    #[test]
    fn parse_envelope_refuses_missing_fields() {
        // Every field is required for a rehydrate. Missing any
        // one → None → fall through to disabled_default.
        let missing_enabled = json!({
            "v": DEMAND_PAYLOAD_VERSION,
            "bins": 64,
            "channels": 1,
            "rate_hz_target": 30,
            "updated_at_ms": 1_000,
        });
        assert!(parse_envelope(&missing_enabled).is_none());
        let missing_updated = json!({
            "v": DEMAND_PAYLOAD_VERSION,
            "enabled": true,
            "bins": 64,
            "channels": 1,
            "rate_hz_target": 30,
        });
        assert!(parse_envelope(&missing_updated).is_none());
    }
}
