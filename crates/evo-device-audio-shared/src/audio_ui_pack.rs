//! Tier 2 audio-reference shelves + widget kinds.
//!
//! The framework owns Tier 1 universals (`evo.prompt.*`,
//! `evo.theme.picker`, `evo.status.badge`, `evo.wizard.*`,
//! operator-tile entries) in the framework SDK's Tier 1 module. The audio
//! reference device adds Tier 2 — concrete audio-domain shelves
//! the operator interacts with (transport, queue, metering, browse,
//! search, signal path) plus the widget kinds that paint on them.
//!
//! These are typed declarations only: every `ShelfContract` +
//! `WidgetKindEnvelope` value matches the SDK shape, and the
//! distribution's `AdmissionSetup` closure registers them on the
//! framework's `ShelfRegistry` + `WidgetKindRegistry` BEFORE any
//! plugin admission so the gate validates `[[ui.stocks]]`
//! declarations against the combined Tier 1 + Tier 2 set.
//!
//! The widget kinds are framework-tier RECORDS, not renderer
//! components — the renderer (the UI shell that consumes the
//! framework's HTTPS substrate) maps each kind id to a Preact /
//! native component. The kinds declared here are the contract;
//! the components live in `evo-device-audio-ui` (the device-tier
//! UI repository).

use evo_plugin_sdk::ui::{
    AcceptedWidgets, ShelfCardinality, ShelfContract, ShelfLayout, ShelfOrder,
    UiAspect, UiMode, UiSize, WidgetKindEnvelope,
};
use std::collections::BTreeMap;

/// Stable shelf id for the transport bar.
pub const SHELF_PLAYBACK_TRANSPORT: &str = "audio.playback.transport";
/// Stable shelf id for the queue surface.
pub const SHELF_QUEUE: &str = "audio.queue";
/// Stable shelf id for the per-channel metering panel.
pub const SHELF_METERING: &str = "audio.metering";
/// Stable shelf id for the library browse surface.
pub const SHELF_BROWSE: &str = "audio.browse";
/// Stable shelf id for the unified search bar.
pub const SHELF_SEARCH: &str = "audio.search";
/// Stable shelf id for the signal-path inspection panel.
pub const SHELF_SIGNAL_PATH: &str = "audio.signal_path";

/// Stable widget-kind id for the transport-controls renderer.
pub const KIND_PLAYER_TRANSPORT: &str = "audio.player.transport";
/// Stable widget-kind id for the queue-list renderer.
pub const KIND_QUEUE_LIST: &str = "audio.queue.list";
/// Stable widget-kind id for the peak-meter renderer.
pub const KIND_METERING_PEAK: &str = "audio.metering.peak";
/// Stable widget-kind id for the browse-tree-entry renderer.
pub const KIND_BROWSE_TREE_ENTRY: &str = "audio.browse.tree.entry";
/// Stable widget-kind id for the unified-search renderer.
pub const KIND_SEARCH_UNIFIED: &str = "audio.search.unified";
/// Stable widget-kind id for the signal-path renderer.
pub const KIND_SIGNAL_PATH: &str = "audio.signal_path";

/// The six Tier 2 shelves the audio reference device declares.
///
/// Each cardinality / layout / order picks the most natural
/// behaviour for the surface — the transport bar admits exactly
/// one widget (the live transport controls); the queue / browse
/// / metering surfaces admit zero or more (vendor distributions
/// extending the reference can drop additional widgets onto the
/// same shelf alongside the framework default).
pub fn audio_shelves() -> Vec<ShelfContract> {
    vec![
        ShelfContract {
            id: SHELF_PLAYBACK_TRANSPORT.into(),
            label: Some("Playback".into()),
            cardinality: ShelfCardinality::ExactlyOne,
            accepts_widgets: AcceptedWidgets::Allowed(vec![
                KIND_PLAYER_TRANSPORT.into(),
            ]),
            accepts_sizes: vec![UiSize::Full],
            layout: ShelfLayout::Single,
            order_by: ShelfOrder::ManifestDeclaration,
            default_widget: Some(KIND_PLAYER_TRANSPORT.into()),
            schema_version: 1,
            min_compatible_version: None,
        },
        ShelfContract {
            id: SHELF_QUEUE.into(),
            label: Some("Queue".into()),
            cardinality: ShelfCardinality::AtMostOne,
            accepts_widgets: AcceptedWidgets::Allowed(vec![
                KIND_QUEUE_LIST.into()
            ]),
            accepts_sizes: vec![UiSize::Half, UiSize::Full],
            layout: ShelfLayout::List,
            order_by: ShelfOrder::ManifestDeclaration,
            default_widget: Some(KIND_QUEUE_LIST.into()),
            schema_version: 1,
            min_compatible_version: None,
        },
        ShelfContract {
            id: SHELF_METERING.into(),
            label: Some("Metering".into()),
            cardinality: ShelfCardinality::AnyToMany,
            accepts_widgets: AcceptedWidgets::Allowed(vec![
                KIND_METERING_PEAK.into()
            ]),
            accepts_sizes: vec![UiSize::Quarter, UiSize::Third, UiSize::Half],
            layout: ShelfLayout::GridResponsive,
            order_by: ShelfOrder::ManifestDeclaration,
            default_widget: Some(KIND_METERING_PEAK.into()),
            schema_version: 1,
            min_compatible_version: None,
        },
        ShelfContract {
            id: SHELF_BROWSE.into(),
            label: Some("Browse".into()),
            cardinality: ShelfCardinality::AnyToMany,
            accepts_widgets: AcceptedWidgets::Allowed(vec![
                KIND_BROWSE_TREE_ENTRY.into(),
            ]),
            accepts_sizes: vec![UiSize::Third, UiSize::Half, UiSize::Full],
            layout: ShelfLayout::List,
            order_by: ShelfOrder::Alphabetical,
            default_widget: Some(KIND_BROWSE_TREE_ENTRY.into()),
            schema_version: 1,
            min_compatible_version: None,
        },
        ShelfContract {
            id: SHELF_SEARCH.into(),
            label: Some("Search".into()),
            cardinality: ShelfCardinality::AtMostOne,
            accepts_widgets: AcceptedWidgets::Allowed(vec![
                KIND_SEARCH_UNIFIED.into(),
            ]),
            accepts_sizes: vec![UiSize::Half, UiSize::Full],
            layout: ShelfLayout::Single,
            order_by: ShelfOrder::ManifestDeclaration,
            default_widget: Some(KIND_SEARCH_UNIFIED.into()),
            schema_version: 1,
            min_compatible_version: None,
        },
        ShelfContract {
            id: SHELF_SIGNAL_PATH.into(),
            label: Some("Signal path".into()),
            cardinality: ShelfCardinality::AtMostOne,
            accepts_widgets: AcceptedWidgets::Allowed(vec![
                KIND_SIGNAL_PATH.into()
            ]),
            accepts_sizes: vec![UiSize::Half, UiSize::Full],
            layout: ShelfLayout::Single,
            order_by: ShelfOrder::ManifestDeclaration,
            default_widget: Some(KIND_SIGNAL_PATH.into()),
            schema_version: 1,
            min_compatible_version: None,
        },
    ]
}

/// The six Tier 2 widget-kind envelopes the audio reference
/// device declares. Each envelope advertises the size window
/// the renderer can compose at and the render mode the kind
/// was designed for. The renderer's responsive picker uses the
/// envelope's `responsive` table to swap sizes across viewport
/// breakpoints; absent entries fall through to `ideal_size`.
pub fn audio_widget_kinds() -> Vec<WidgetKindEnvelope> {
    let no_responsive = BTreeMap::new();
    vec![
        WidgetKindEnvelope {
            id: KIND_PLAYER_TRANSPORT.into(),
            min_size: UiSize::Half,
            ideal_size: UiSize::Full,
            max_size: UiSize::Full,
            aspect_ratio: UiAspect::Wide,
            responsive: no_responsive.clone(),
            mode: UiMode::Inline,
            schema_version: 1,
        },
        WidgetKindEnvelope {
            id: KIND_QUEUE_LIST.into(),
            min_size: UiSize::Third,
            ideal_size: UiSize::Half,
            max_size: UiSize::Full,
            aspect_ratio: UiAspect::Tall,
            responsive: no_responsive.clone(),
            mode: UiMode::Inline,
            schema_version: 1,
        },
        WidgetKindEnvelope {
            id: KIND_METERING_PEAK.into(),
            min_size: UiSize::Quarter,
            ideal_size: UiSize::Third,
            max_size: UiSize::Half,
            aspect_ratio: UiAspect::Tall,
            responsive: no_responsive.clone(),
            mode: UiMode::Inline,
            schema_version: 1,
        },
        WidgetKindEnvelope {
            id: KIND_BROWSE_TREE_ENTRY.into(),
            min_size: UiSize::Third,
            ideal_size: UiSize::Half,
            max_size: UiSize::Full,
            aspect_ratio: UiAspect::Any,
            responsive: no_responsive.clone(),
            mode: UiMode::Inline,
            schema_version: 1,
        },
        WidgetKindEnvelope {
            id: KIND_SEARCH_UNIFIED.into(),
            min_size: UiSize::Half,
            ideal_size: UiSize::Half,
            max_size: UiSize::Full,
            aspect_ratio: UiAspect::Wide,
            responsive: no_responsive.clone(),
            mode: UiMode::Inline,
            schema_version: 1,
        },
        WidgetKindEnvelope {
            id: KIND_SIGNAL_PATH.into(),
            min_size: UiSize::Half,
            ideal_size: UiSize::Full,
            max_size: UiSize::Full,
            aspect_ratio: UiAspect::Wide,
            responsive: no_responsive,
            mode: UiMode::Inline,
            schema_version: 1,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shelves_and_kinds_are_paired_by_id() {
        let shelves = audio_shelves();
        let kinds = audio_widget_kinds();
        assert_eq!(shelves.len(), 6);
        assert_eq!(kinds.len(), 6);
        // Every shelf's default widget must be present in the
        // widget-kind set the same pack registers.
        let kind_ids: Vec<&str> = kinds.iter().map(|k| k.id.as_str()).collect();
        for shelf in &shelves {
            let default = shelf
                .default_widget
                .as_deref()
                .expect("Tier 2 shelf must declare a default widget");
            assert!(
                kind_ids.contains(&default),
                "shelf {:?} declares default widget {:?} not present in the kind set",
                shelf.id,
                default,
            );
        }
    }

    #[test]
    fn every_default_widget_passes_its_shelf_envelope() {
        let shelves = audio_shelves();
        let kinds = audio_widget_kinds();
        let kind_by_id: std::collections::BTreeMap<&str, &WidgetKindEnvelope> =
            kinds.iter().map(|k| (k.id.as_str(), k)).collect();
        for shelf in &shelves {
            let default_id = shelf.default_widget.as_deref().unwrap();
            let envelope = kind_by_id.get(default_id).unwrap();
            // Every accepted size on the shelf must be inside the
            // widget envelope's min..=max window so the convergence
            // default never produces a stocking the envelope refuses.
            for size in &shelf.accepts_sizes {
                assert!(
                    *size >= envelope.min_size && *size <= envelope.max_size,
                    "shelf {:?} accepts size {:?} but kind {:?} \
                     envelope is {:?}..={:?}",
                    shelf.id,
                    size,
                    envelope.id,
                    envelope.min_size,
                    envelope.max_size,
                );
            }
        }
    }
}
