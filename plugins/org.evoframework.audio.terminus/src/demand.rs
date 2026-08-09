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

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use evo_plugin_sdk::contract::{
    ExternalAddressing, PluginError, Request, Response, SubjectAnnouncement,
    SubjectAnnouncer,
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

/// Producer-plane demand shape. Every field is production-
/// affecting; renderer-only choices never appear here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SpectrumDemand {
    /// `true` opens the producer's capture path; `false` parks
    /// it identically to `TransportGate::NotPlaying` (ALSA PCM
    /// released, no FFT compute, no emit).
    pub enabled: bool,
    /// Mel-bin count. Constrained to `{32, 64, 128, 256}` at
    /// parse time; the analyser rebuilds when this changes
    /// mid-play.
    pub bins: u32,
    /// Channel mode. `1` = mono (producer collapses L+R at the
    /// mel stage); `2` = stereo (producer emits both channels).
    pub channels: u32,
    /// Emit throttle target in Hz. Clamped to `[1, 60]`.
    pub rate_hz_target: u32,
    /// Wall-clock ms at last apply. Zero before the first apply.
    pub updated_at_ms: u64,
}

impl SpectrumDemand {
    /// Default: disabled, 64 bins, mono, 30 Hz. Reproduces the
    /// operator-visible baseline the visualiser has always
    /// carried when the operator has never opened the settings
    /// surface — no spectrum activity on-device, ready to enable
    /// on first opt-in without a plugin restart.
    pub fn disabled_default() -> Self {
        Self {
            enabled: false,
            bins: 64,
            channels: 1,
            rate_hz_target: 30,
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
    /// Construct with the default disabled state. Announces the
    /// subject inline so consumers connecting during plugin load
    /// see the seeded envelope before the first apply.
    pub async fn announce_initial(subjects: Arc<dyn SubjectAnnouncer>) -> Self {
        let (tx, _rx) = watch::channel(SpectrumDemand::disabled_default());
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
        "v":              DEMAND_PAYLOAD_VERSION,
        "enabled":        d.enabled,
        "bins":           d.bins,
        "channels":       d.channels,
        "rate_hz_target": d.rate_hz_target,
        "updated_at_ms":  d.updated_at_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_default_matches_documented_shape() {
        let d = SpectrumDemand::disabled_default();
        assert!(!d.enabled);
        assert_eq!(d.bins, 64);
        assert_eq!(d.channels, 1);
        assert_eq!(d.rate_hz_target, 30);
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
            updated_at_ms: 1_720_000_000_000,
        };
        let env = render_envelope(&d);
        assert_eq!(env["v"], DEMAND_PAYLOAD_VERSION);
        assert_eq!(env["enabled"], true);
        assert_eq!(env["bins"], 128);
        assert_eq!(env["channels"], 2);
        assert_eq!(env["rate_hz_target"], 30);
        assert_eq!(env["updated_at_ms"], 1_720_000_000_000_u64);
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
}
