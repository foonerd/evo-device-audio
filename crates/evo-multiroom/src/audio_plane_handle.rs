// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! SDK adapter for the audio plane.
//!
//! Wraps the concrete runtime in the plugin-facing
//! `AudioPlaneHandle` contract. It lives beside the plane because
//! it exists only to dress the plane for plugins: a steward that
//! ships no plane has nothing to wrap, and a distribution that
//! ships one is the only party that can build this.

use std::sync::Arc;

/// Framework-side adapter binding
/// [`crate::audio_plane::AudioPlaneRuntime`] to the plugin SDK's
/// [`evo_plugin_sdk::contract::audio_plane::AudioPlaneHandle`]
/// trait. The admission engine wraps the shared runtime in one
/// of these per plugin whose manifest declares
/// `capabilities.audio_plane = true` and populates
/// `LoadContext::audio_plane` so the plugin can fan audio
/// frames out to multi-room receivers + subscribe to incoming
/// frames from a source-host peer.
pub struct RuntimeAudioPlaneHandle {
    runtime: Arc<crate::audio_plane::AudioPlaneRuntime>,
    /// Group store the audio plane consults when fanning frames
    /// out. Kept as a direct handle on this wrapper so the
    /// SDK-side `upsert_group` method (used by source-host
    /// plugins to instantiate their group from operator config)
    /// reaches the same store the runtime reads from.
    group_store: Arc<evo::groups::GroupStore>,
}

impl RuntimeAudioPlaneHandle {
    /// Construct against a shared [`crate::audio_plane::AudioPlaneRuntime`]
    /// + the framework's [`evo::groups::GroupStore`].
    pub fn new(
        runtime: Arc<crate::audio_plane::AudioPlaneRuntime>,
        group_store: Arc<evo::groups::GroupStore>,
    ) -> Self {
        Self {
            runtime,
            group_store,
        }
    }
}

impl evo_plugin_sdk::contract::audio_plane::AudioPlaneHandle
    for RuntimeAudioPlaneHandle
{
    fn subscribe_audio_frames<'a>(
        &'a self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        evo_plugin_sdk::contract::audio_plane::AudioFrameStream,
                        evo_plugin_sdk::contract::PluginError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let runtime = Arc::clone(&self.runtime);
        Box::pin(async move {
            // The framework's broadcast item type re-exports the
            // SDK's AudioFrameReceived, so the receiver matches
            // the SDK's AudioFrameStream constructor without an
            // adapter step.
            let rx = runtime.subscribe_audio_frames();
            Ok(
                evo_plugin_sdk::contract::audio_plane::AudioFrameStream::new(
                    rx,
                ),
            )
        })
    }

    fn fan_out_audio_frame<'a>(
        &'a self,
        group_id: String,
        frame: evo_plugin_sdk::contract::audio_plane::AudioFrameSeed,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<(), evo_plugin_sdk::contract::PluginError>,
                > + Send
                + 'a,
        >,
    > {
        let runtime = Arc::clone(&self.runtime);
        Box::pin(async move {
            // Map the SDK envelope to the runtime's seed type.
            // The runtime's fan_out_audio_frame consumes its
            // own AudioFrameSeed; the SDK type carries the same
            // fields so the conversion is a trivial struct
            // re-pack.
            let seed = crate::audio_plane::AudioFrameSeed {
                sequence: frame.sequence,
                presentation_time_ms: frame.presentation_time_ms,
                codec: frame.codec,
                rate_hz: frame.rate_hz,
                channels: frame.channels,
                payload_b64: frame.payload_b64,
            };
            runtime.fan_out_audio_frame(&group_id, seed).await;
            Ok(())
        })
    }

    fn upsert_group<'a>(
        &'a self,
        group_id: String,
        display_name: String,
        members: Vec<String>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<(), evo_plugin_sdk::contract::PluginError>,
                > + Send
                + 'a,
        >,
    > {
        let group_store = Arc::clone(&self.group_store);
        Box::pin(async move {
            group_store
                .upsert_with_id(&group_id, &display_name, &members)
                .await
                .map(|_| ())
                .map_err(|e| {
                    evo_plugin_sdk::contract::PluginError::Permanent(format!(
                        "upsert_group({group_id}) failed: {e}"
                    ))
                })?;
            Ok(())
        })
    }

    fn dial_peer<'a>(
        &'a self,
        addr: String,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<(), evo_plugin_sdk::contract::PluginError>,
                > + Send
                + 'a,
        >,
    > {
        let runtime = Arc::clone(&self.runtime);
        Box::pin(async move {
            let sock = addr.parse::<std::net::SocketAddr>().map_err(|e| {
                evo_plugin_sdk::contract::PluginError::Permanent(format!(
                    "dial_peer({addr}) address parse failed: {e}"
                ))
            })?;
            runtime.dial_peer(sock).await.map_err(|e| {
                evo_plugin_sdk::contract::PluginError::Permanent(format!(
                    "dial_peer({addr}) failed: {e}"
                ))
            })
        })
    }

    fn close_outbound_connections<'a>(
        &'a self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<(), evo_plugin_sdk::contract::PluginError>,
                > + Send
                + 'a,
        >,
    > {
        let runtime = Arc::clone(&self.runtime);
        Box::pin(async move {
            runtime.close_outbound_connections().await;
            Ok(())
        })
    }

    fn subscribe_frame_send_events<'a>(
        &'a self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        evo_plugin_sdk::contract::audio_plane::FrameSendEventStream,
                        evo_plugin_sdk::contract::PluginError,
                    >,
                > + Send
                + 'a,
        >,
    >{
        let runtime = Arc::clone(&self.runtime);
        Box::pin(async move {
            let rx = runtime.subscribe_frame_send_events();
            Ok(
                evo_plugin_sdk::contract::audio_plane::FrameSendEventStream::new(
                    rx,
                ),
            )
        })
    }

    fn report_frame_trace<'a>(
        &'a self,
        report: evo_plugin_sdk::contract::audio_plane::ReceiverFrameTraceReport,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<(), evo_plugin_sdk::contract::PluginError>,
                > + Send
                + 'a,
        >,
    > {
        let runtime = Arc::clone(&self.runtime);
        Box::pin(async move {
            runtime.route_frame_trace_report(report).await;
            Ok(())
        })
    }

    fn subscribe_frame_trace_reports<'a>(
        &'a self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        evo_plugin_sdk::contract::audio_plane::FrameTraceReportStream,
                        evo_plugin_sdk::contract::PluginError,
                    >,
                > + Send
                + 'a,
        >,
    >{
        let runtime = Arc::clone(&self.runtime);
        Box::pin(async move {
            let rx = runtime.subscribe_frame_trace_reports();
            Ok(
                evo_plugin_sdk::contract::audio_plane::FrameTraceReportStream::new(
                    rx,
                ),
            )
        })
    }

    fn monotonic_ns(&self) -> u64 {
        self.runtime.monotonic_ns()
    }

    fn local_device_id(&self) -> String {
        self.runtime.local_device_id().to_string()
    }
}
