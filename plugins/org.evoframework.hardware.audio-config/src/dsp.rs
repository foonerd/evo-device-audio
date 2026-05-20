//! DSP capability resolver — three-layer merge producing the
//! operator-facing per-DAC DSP control set.
//!
//! The resolver joins:
//!
//! 1. The active DAC's catalog `dsp_options[]` (list of ALSA mixer-
//!    control names the catalog declares for this hardware).
//! 2. The curated DSP control pool (operator-facing schema per
//!    control: type, value-domain, human label, recommended
//!    default, apply-semantics, description).
//! 3. Live amixer introspection (current value + runtime-narrowed
//!    range; abstracted via the [`AmixerReader`] trait so the
//!    resolver is testable without a real ALSA card).
//!
//! Each layer's absence is observable in the published surface; no
//! layer is silently skipped. Specifically:
//!
//! * Control declared in catalog but not in pool → surfaces with
//!   `unbound_pool_entry = true` and the raw ALSA name as the
//!   human label.
//! * Control declared in catalog (and possibly pool) but not
//!   exposed by amixer on the bound card → surfaces with
//!   `bound = false` and an operator-readable `unbound_reason`.
//! * No active DAC selected → resolver returns an empty
//!   [`DspCapabilitySet`] plus a top-level diagnostic
//!   [`ResolverDiagnostic::NoActiveDac`].

use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

use crate::dsp_pool::{ApplySemantics, ControlType, DspControlPool};
use crate::evo_catalog::EvoCatalog;

/// Live amixer-introspection abstraction. The resolver invokes
/// [`read_control`] once per control declared in the catalog
/// `dsp_options[]`; the production implementation shells into
/// `amixer -c <card> cget name='<control>'` and parses the output.
/// Tests inject stub implementations to exercise every
/// resolver branch without depending on an ALSA card.
pub trait AmixerReader: Send + Sync {
    /// Read the live state of one ALSA mixer control on the given
    /// card. Returns [`AmixerReadOutcome::Found`] when the control
    /// exists on the card, [`AmixerReadOutcome::NotPresent`] when
    /// amixer enumerates the card but does not expose the requested
    /// control, [`AmixerReadOutcome::CardUnknown`] when no card
    /// matches the supplied hint (bound card not resolvable),
    /// [`AmixerReadOutcome::IntrospectionFailed`] for any other
    /// failure (subprocess error, parse error, …). The variant is
    /// the load-bearing diagnostic the resolver surfaces back to
    /// the operator via `unbound_reason`.
    fn read_control<'a>(
        &'a self,
        card_hint: &'a str,
        control_name: &'a str,
    ) -> Pin<Box<dyn Future<Output = AmixerReadOutcome> + Send + 'a>>;
}

/// Outcome variant returned by [`AmixerReader::read_control`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AmixerReadOutcome {
    /// Control exists on the card. Carries the live runtime state.
    Found(LiveControlState),
    /// Amixer enumerates the card but does not expose this control.
    /// The catalog declared it, but the active overlay's kernel
    /// driver did not register the mixer for it (operator likely
    /// has a mismatch between the catalog row's overlay and what's
    /// actually loaded).
    NotPresent {
        /// Operator-readable diagnostic (e.g. "control not in
        /// amixer for card 'sndrpihifiberry'").
        reason: String,
    },
    /// No ALSA card matches the supplied hint on this host. The
    /// hardware-audio active_config subject's `alsa_card_hint` is
    /// stale or the operator has not yet selected a DAC.
    CardUnknown {
        /// Operator-readable diagnostic.
        reason: String,
    },
    /// Subprocess failed, parse failed, or some other introspection
    /// path errored. The variant carries the underlying diagnostic
    /// verbatim so the operator-facing surface preserves the
    /// debugging detail.
    IntrospectionFailed {
        /// Underlying error string.
        reason: String,
    },
}

/// Runtime control state amixer reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveControlState {
    /// The control's runtime-reported type. For enum controls,
    /// pair this with [`enum_values`] to know the legal set; for
    /// integer / db_scale controls, pair with [`integer_min`] /
    /// [`integer_max`] for the range.
    pub control_type: ControlType,
    /// Current value as a JSON-typed payload (string for enum,
    /// integer for integer / db_scale, bool for boolean). Keeps
    /// the wire surface uniform across types.
    pub current_value: serde_json::Value,
    /// For enum: the runtime-reported value set on the bound card.
    /// May be narrower than the curated pool's `known_values`.
    /// Empty when the control type is not enum.
    pub enum_values: Vec<String>,
    /// For integer / db_scale: the runtime-reported minimum. `None`
    /// when amixer did not report a bound.
    pub integer_min: Option<i64>,
    /// For integer / db_scale: the runtime-reported maximum.
    pub integer_max: Option<i64>,
}

/// Top-level resolver output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DspCapabilitySet {
    /// Catalog id of the DAC the resolver targeted. `None` when
    /// no active DAC was passed.
    pub dac_id: Option<String>,
    /// ALSA card hint resolved from the catalog for this DAC.
    /// `None` when the catalog row had no `alsa_card_hint` set
    /// (Sparky-class boards without ALSA-card bindings).
    pub alsa_card_hint: Option<String>,
    /// Whether the operator (and any modder gestures) can apply
    /// advanced-settings against this DAC. Copied verbatim from
    /// the catalog row's `advanced_settings_enabled` field; the
    /// wire-op layer enforces.
    pub advanced_settings_enabled: bool,
    /// Per-control resolved state. Empty Vec when the DAC has no
    /// catalog `dsp_options[]` (most DACs without an onboard DSP
    /// chip).
    pub controls: Vec<DspControlState>,
    /// Top-level diagnostics — non-fatal contextual notes the UI
    /// surfaces alongside the controls list (e.g. "no active DAC
    /// selected; select_dac to populate this view").
    pub diagnostics: Vec<ResolverDiagnostic>,
}

/// Per-control resolved state surfaced on the
/// `dsp_capabilities` subject. UI plugins iterate this list to
/// render the operator-facing DSP form.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DspControlState {
    /// Verbatim ALSA mixer-control name. Operator gestures
    /// reference this string.
    pub name: String,
    /// Operator-readable label. Resolved from the curated pool
    /// when the control has a pool entry; falls back to `name`
    /// otherwise.
    pub human_label: String,
    /// Control type (enum / integer / boolean / db_scale). Resolved
    /// from the curated pool when present; from amixer otherwise.
    /// When neither layer surfaces a type, defaults to
    /// `ControlType::Enum` with empty value set.
    pub control_type: ControlType,
    /// Value domain (legal set for enum; range for integer /
    /// db_scale; ignored for boolean). The merge picks the
    /// narrower of pool + amixer when both are present.
    pub value_domain: ValueDomain,
    /// Current value as reported by amixer. `None` when amixer
    /// could not read the control (the `bound = false` case).
    pub current_value: Option<serde_json::Value>,
    /// Recommended default from the curated pool. `None` when the
    /// control has no pool entry (`unbound_pool_entry = true`).
    pub recommended_default: Option<serde_json::Value>,
    /// Apply semantics: hot_apply or requires_restart. Resolved
    /// from the curated pool; defaults to `hot_apply` when no
    /// pool entry exists (the safer assumption: most ALSA mixer
    /// controls hot-apply; UI can confirm via gesture-test).
    pub apply_semantics: ApplySemantics,
    /// Operator-facing description. From the curated pool's
    /// `description` field; empty when no pool entry.
    pub description: String,
    /// `true` when amixer exposed the control on the bound card;
    /// `false` when amixer could not read it (catalog declares
    /// but card does not expose — operator likely has overlay /
    /// driver mismatch).
    pub bound: bool,
    /// Operator-readable diagnostic when `bound = false`. Empty
    /// when `bound = true`.
    pub unbound_reason: String,
    /// `true` when the control name was declared in the catalog
    /// `dsp_options[]` but did NOT have a curated pool entry.
    /// Operator sees the raw mixer-control name as the label;
    /// description is empty; recommended_default is None.
    pub unbound_pool_entry: bool,
}

/// Value domain discriminator. Mirrors the curated pool's
/// `ControlType` but carries the resolved runtime data inline so
/// the published subject is self-contained.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ValueDomain {
    /// Enum: a finite set of string values.
    Enum {
        /// Resolved value set (intersection of pool + amixer when
        /// both present; pool only when amixer absent; amixer only
        /// when pool absent).
        values: Vec<String>,
    },
    /// Integer: a numeric range.
    Integer {
        /// Resolved minimum (narrower of pool + amixer when both
        /// present). `None` when no layer supplied one.
        min: Option<i64>,
        /// Resolved maximum (narrower of pool + amixer when both
        /// present). `None` when no layer supplied one.
        max: Option<i64>,
    },
    /// dB-scale integer: same shape as Integer, distinct
    /// discriminator so UI renders a different widget.
    DbScale {
        /// Resolved minimum (dB).
        min: Option<i64>,
        /// Resolved maximum (dB).
        max: Option<i64>,
    },
    /// Boolean: no value domain beyond {true, false}.
    Boolean,
}

/// Top-level resolver diagnostic. Surfaced in
/// [`DspCapabilitySet::diagnostics`] alongside the controls list.
/// Non-fatal — the resolver always returns a [`DspCapabilitySet`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ResolverDiagnostic {
    /// The supplied `dac_id` was `None` — no active DAC. UI
    /// surfaces "select a DAC to populate this view."
    NoActiveDac,
    /// The supplied `dac_id` did not match any entry in the
    /// catalog for the given profile. Carries the offending id
    /// for operator-readable diagnostic.
    DacNotInCatalog {
        /// The id the resolver was asked to look up.
        dac_id: String,
    },
    /// The active DAC catalog entry has no `alsa_card_hint`.
    /// Resolver cannot invoke amixer; every control surfaces
    /// with `bound = false` + a hint-missing `unbound_reason`.
    NoAlsaCardHint {
        /// The DAC id with no card hint.
        dac_id: String,
    },
    /// The active DAC catalog entry has no `dsp_options[]`.
    /// Resolver returns an empty controls list — operator-
    /// expected state for most DACs without onboard DSP chips.
    NoDspOptionsDeclared {
        /// The DAC id with no DSP options.
        dac_id: String,
    },
}

/// Resolve the DSP capability set for the given DAC.
///
/// Pure function (modulo the `AmixerReader::read_control`
/// futures): given the catalog + pool + amixer-reader + the
/// active DAC's identity, returns the merged
/// [`DspCapabilitySet`]. Deterministic given identical inputs.
pub async fn resolve_dsp_capabilities(
    catalog: &EvoCatalog,
    profile: &str,
    dac_id: Option<&str>,
    pool: &DspControlPool,
    amixer: &dyn AmixerReader,
) -> DspCapabilitySet {
    let Some(dac_id) = dac_id else {
        return DspCapabilitySet {
            dac_id: None,
            alsa_card_hint: None,
            advanced_settings_enabled: false,
            controls: Vec::new(),
            diagnostics: vec![ResolverDiagnostic::NoActiveDac],
        };
    };

    let Some(dac) = catalog.find_dac(profile, dac_id) else {
        return DspCapabilitySet {
            dac_id: Some(dac_id.to_string()),
            alsa_card_hint: None,
            advanced_settings_enabled: false,
            controls: Vec::new(),
            diagnostics: vec![ResolverDiagnostic::DacNotInCatalog {
                dac_id: dac_id.to_string(),
            }],
        };
    };

    let alsa_card_hint = if dac.alsa_card_hint.is_empty() {
        None
    } else {
        Some(dac.alsa_card_hint.clone())
    };

    let mut diagnostics: Vec<ResolverDiagnostic> = Vec::new();
    if alsa_card_hint.is_none() {
        diagnostics.push(ResolverDiagnostic::NoAlsaCardHint {
            dac_id: dac_id.to_string(),
        });
    }
    if dac.dsp_options.is_empty() {
        diagnostics.push(ResolverDiagnostic::NoDspOptionsDeclared {
            dac_id: dac_id.to_string(),
        });
        return DspCapabilitySet {
            dac_id: Some(dac.id.clone()),
            alsa_card_hint,
            advanced_settings_enabled: dac.advanced_settings_enabled,
            controls: Vec::new(),
            diagnostics,
        };
    }

    let pool_lookup = pool.build_lookup();
    let mut controls: Vec<DspControlState> =
        Vec::with_capacity(dac.dsp_options.len());
    for control_name in &dac.dsp_options {
        let pool_entry = pool_lookup.get(control_name.as_str());
        let amixer_outcome = match alsa_card_hint.as_ref() {
            Some(card) => amixer.read_control(card, control_name).await,
            None => AmixerReadOutcome::CardUnknown {
                reason: format!(
                    "catalog entry '{}' has no alsa_card_hint; cannot probe amixer",
                    dac.id
                ),
            },
        };
        controls.push(merge_layers(
            control_name,
            pool_entry.copied(),
            amixer_outcome,
        ));
    }

    DspCapabilitySet {
        dac_id: Some(dac.id.clone()),
        alsa_card_hint,
        advanced_settings_enabled: dac.advanced_settings_enabled,
        controls,
        diagnostics,
    }
}

fn merge_layers(
    control_name: &str,
    pool_entry: Option<&crate::dsp_pool::DspControlEntry>,
    amixer_outcome: AmixerReadOutcome,
) -> DspControlState {
    let (
        human_label,
        description,
        recommended_default,
        apply_semantics,
        pool_type,
        pool_enum_values,
    ) = match pool_entry {
        Some(entry) => (
            entry.human_label.clone(),
            entry.description.clone(),
            Some(entry.recommended_default.clone()),
            entry.apply_semantics,
            Some(entry.control_type),
            entry.known_values.clone(),
        ),
        None => (
            control_name.to_string(),
            String::new(),
            None,
            ApplySemantics::HotApply,
            None,
            Vec::new(),
        ),
    };
    let unbound_pool_entry = pool_entry.is_none();

    let (
        bound,
        unbound_reason,
        current_value,
        amixer_type,
        amixer_enum_values,
        amixer_min,
        amixer_max,
    ) = match amixer_outcome {
        AmixerReadOutcome::Found(state) => (
            true,
            String::new(),
            Some(state.current_value),
            Some(state.control_type),
            state.enum_values,
            state.integer_min,
            state.integer_max,
        ),
        AmixerReadOutcome::NotPresent { reason }
        | AmixerReadOutcome::CardUnknown { reason }
        | AmixerReadOutcome::IntrospectionFailed { reason } => {
            (false, reason, None, None, Vec::new(), None, None)
        }
    };

    let control_type = amixer_type.or(pool_type).unwrap_or(ControlType::Enum);

    let value_domain = match control_type {
        ControlType::Enum => {
            let values = if amixer_enum_values.is_empty() {
                pool_enum_values
            } else if pool_enum_values.is_empty() {
                amixer_enum_values
            } else {
                // Intersection: keep only values present in both
                // layers (pool's curated set narrowed to what the
                // bound card actually exposes). Preserves pool's
                // ordering for UI stability.
                pool_enum_values
                    .into_iter()
                    .filter(|v| amixer_enum_values.iter().any(|a| a == v))
                    .collect()
            };
            ValueDomain::Enum { values }
        }
        ControlType::Integer => ValueDomain::Integer {
            min: amixer_min,
            max: amixer_max,
        },
        ControlType::DbScale => ValueDomain::DbScale {
            min: amixer_min,
            max: amixer_max,
        },
        ControlType::Boolean => ValueDomain::Boolean,
    };

    DspControlState {
        name: control_name.to_string(),
        human_label,
        control_type,
        value_domain,
        current_value,
        recommended_default,
        apply_semantics,
        description,
        bound,
        unbound_reason,
        unbound_pool_entry,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp_pool::parse_dsp_control_pool;
    use crate::evo_catalog::parse_evo_catalog;
    use std::collections::HashMap;
    use std::sync::Mutex;

    const EMBEDDED_CATALOG: &str = include_str!("../data/evo-catalog.toml");
    const EMBEDDED_POOL: &str = include_str!("../data/dsp-control-pool.toml");

    /// Stub amixer reader keyed by (card, control_name). Tests
    /// pre-populate the response for each control they want to
    /// exercise; anything not in the map returns NotPresent.
    struct StubAmixer {
        responses: Mutex<HashMap<(String, String), AmixerReadOutcome>>,
    }

    impl StubAmixer {
        fn new() -> Self {
            Self {
                responses: Mutex::new(HashMap::new()),
            }
        }

        fn set(&self, card: &str, control: &str, outcome: AmixerReadOutcome) {
            self.responses
                .lock()
                .unwrap()
                .insert((card.to_string(), control.to_string()), outcome);
        }
    }

    impl AmixerReader for StubAmixer {
        fn read_control<'a>(
            &'a self,
            card_hint: &'a str,
            control_name: &'a str,
        ) -> Pin<Box<dyn Future<Output = AmixerReadOutcome> + Send + 'a>>
        {
            let key = (card_hint.to_string(), control_name.to_string());
            let outcome = self
                .responses
                .lock()
                .unwrap()
                .get(&key)
                .cloned()
                .unwrap_or_else(|| AmixerReadOutcome::NotPresent {
                    reason: format!(
                        "stub: no response set for ({card_hint}, {control_name})"
                    ),
                });
            Box::pin(async move { outcome })
        }
    }

    #[tokio::test]
    async fn no_active_dac_surfaces_diagnostic() {
        let catalog = parse_evo_catalog(EMBEDDED_CATALOG).expect("catalog");
        let pool = parse_dsp_control_pool(EMBEDDED_POOL).expect("pool");
        let amixer = StubAmixer::new();
        let caps = resolve_dsp_capabilities(
            &catalog,
            "Raspberry PI",
            None,
            &pool,
            &amixer,
        )
        .await;
        assert!(caps.dac_id.is_none());
        assert!(caps.controls.is_empty());
        assert_eq!(caps.diagnostics, vec![ResolverDiagnostic::NoActiveDac]);
    }

    #[tokio::test]
    async fn unknown_dac_id_surfaces_diagnostic() {
        let catalog = parse_evo_catalog(EMBEDDED_CATALOG).expect("catalog");
        let pool = parse_dsp_control_pool(EMBEDDED_POOL).expect("pool");
        let amixer = StubAmixer::new();
        let caps = resolve_dsp_capabilities(
            &catalog,
            "Raspberry PI",
            Some("nonexistent-dac"),
            &pool,
            &amixer,
        )
        .await;
        assert!(caps.controls.is_empty());
        assert!(matches!(
            caps.diagnostics.first(),
            Some(ResolverDiagnostic::DacNotInCatalog { .. })
        ));
    }

    #[tokio::test]
    async fn dac_with_no_dsp_options_returns_empty_controls() {
        // adafruit-max98357 has no DSP options in the catalog
        // (just a basic DAC with no onboard DSP). Should return
        // empty controls + the NoDspOptionsDeclared diagnostic.
        let catalog = parse_evo_catalog(EMBEDDED_CATALOG).expect("catalog");
        let pool = parse_dsp_control_pool(EMBEDDED_POOL).expect("pool");
        let amixer = StubAmixer::new();
        let caps = resolve_dsp_capabilities(
            &catalog,
            "Raspberry PI",
            Some("adafruit-max98357"),
            &pool,
            &amixer,
        )
        .await;
        assert!(caps.controls.is_empty());
        assert!(matches!(
            caps.diagnostics.first(),
            Some(ResolverDiagnostic::NoDspOptionsDeclared { .. })
        ));
        assert!(caps.advanced_settings_enabled);
    }

    #[tokio::test]
    async fn hifiberry_dacplus_resolves_two_dsp_options_via_pool_and_amixer() {
        // HiFiBerry DAC Plus has dsp_options = ["DSP Program",
        // "Clock Missing Period"]. Stub amixer to expose both;
        // pool covers both with full schema. Verify the merged
        // surface carries pool's human labels + amixer's current
        // values + intersected enum domain.
        let catalog = parse_evo_catalog(EMBEDDED_CATALOG).expect("catalog");
        let pool = parse_dsp_control_pool(EMBEDDED_POOL).expect("pool");
        let amixer = StubAmixer::new();

        amixer.set(
            "sndrpihifiberry",
            "DSP Program",
            AmixerReadOutcome::Found(LiveControlState {
                control_type: ControlType::Enum,
                current_value: serde_json::Value::String("None".into()),
                enum_values: vec!["None".into(), "DAC".into()],
                integer_min: None,
                integer_max: None,
            }),
        );
        amixer.set(
            "sndrpihifiberry",
            "Clock Missing Period",
            AmixerReadOutcome::Found(LiveControlState {
                control_type: ControlType::Integer,
                current_value: serde_json::Value::Number(0.into()),
                enum_values: vec![],
                integer_min: Some(0),
                integer_max: Some(10000),
            }),
        );

        let caps = resolve_dsp_capabilities(
            &catalog,
            "Raspberry PI",
            Some("hifiberry-dacplus"),
            &pool,
            &amixer,
        )
        .await;
        assert_eq!(caps.controls.len(), 2);
        assert_eq!(caps.dac_id.as_deref(), Some("hifiberry-dacplus"));
        assert_eq!(caps.alsa_card_hint.as_deref(), Some("sndrpihifiberry"));

        let dsp_program = &caps.controls[0];
        assert_eq!(dsp_program.name, "DSP Program");
        assert_eq!(dsp_program.human_label, "DSP Program");
        assert!(dsp_program.bound);
        assert!(!dsp_program.unbound_pool_entry);
        assert!(matches!(dsp_program.control_type, ControlType::Enum));
        // Intersection of pool's known_values ["None", "DAC",
        // "DAC+Headphone", "Headphone", "Mute"] with amixer's
        // ["None", "DAC"] gives ["None", "DAC"] in pool order.
        match &dsp_program.value_domain {
            ValueDomain::Enum { values } => {
                assert_eq!(
                    values,
                    &vec!["None".to_string(), "DAC".to_string()]
                );
            }
            other => panic!("expected Enum value domain, got {other:?}"),
        }
        assert_eq!(
            dsp_program.current_value,
            Some(serde_json::Value::String("None".into()))
        );
        assert!(dsp_program.recommended_default.is_some());

        let clock = &caps.controls[1];
        assert_eq!(clock.name, "Clock Missing Period");
        assert!(clock.bound);
        assert!(matches!(clock.control_type, ControlType::Integer));
        match &clock.value_domain {
            ValueDomain::Integer { min, max } => {
                assert_eq!(*min, Some(0));
                assert_eq!(*max, Some(10000));
            }
            other => panic!("expected Integer value domain, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unbound_control_surfaces_with_reason_not_silent_disappearance() {
        // Pin the ADR-required invariant: a control declared in
        // catalog dsp_options[] but not exposed by amixer surfaces
        // with bound = false + operator-readable unbound_reason.
        // The control does NOT silently disappear from the list.
        let catalog = parse_evo_catalog(EMBEDDED_CATALOG).expect("catalog");
        let pool = parse_dsp_control_pool(EMBEDDED_POOL).expect("pool");
        let amixer = StubAmixer::new();
        // Set only ONE of HiFiBerry DAC Plus's two controls;
        // leave the other unset (StubAmixer returns NotPresent).
        amixer.set(
            "sndrpihifiberry",
            "DSP Program",
            AmixerReadOutcome::Found(LiveControlState {
                control_type: ControlType::Enum,
                current_value: serde_json::Value::String("None".into()),
                enum_values: vec!["None".into()],
                integer_min: None,
                integer_max: None,
            }),
        );
        let caps = resolve_dsp_capabilities(
            &catalog,
            "Raspberry PI",
            Some("hifiberry-dacplus"),
            &pool,
            &amixer,
        )
        .await;
        assert_eq!(caps.controls.len(), 2, "both controls surface");
        let clock = caps
            .controls
            .iter()
            .find(|c| c.name == "Clock Missing Period")
            .expect("Clock Missing Period present in list");
        assert!(!clock.bound);
        assert!(!clock.unbound_reason.is_empty());
        assert!(clock.current_value.is_none());
        // Pool entry still resolved — the human label + recommended
        // default + apply_semantics come from the pool layer even
        // when amixer is unbound.
        assert_eq!(clock.human_label, "Clock Missing Period (s)");
        assert!(clock.recommended_default.is_some());
    }

    #[tokio::test]
    async fn unbound_pool_entry_surfaces_when_catalog_references_unknown_control(
    ) {
        // Catalog declares a control that has no curated pool
        // entry. The control should surface with
        // unbound_pool_entry = true + the raw ALSA name as the
        // human label + recommended_default = None.
        let catalog_toml = r#"
schema_version = 1
[[boards]]
name = "Test Board"
provider = "noop"
[[boards.dacs]]
id = "test-dac"
display_name = "Test DAC"
overlay = "test"
alsa_card_hint = "TestCard"
needs_reboot_on_apply = false
advanced_settings_enabled = true
dsp_options = ["Made Up Control"]
provenance = "test"
"#;
        let catalog = parse_evo_catalog(catalog_toml).expect("catalog");
        let pool = parse_dsp_control_pool(EMBEDDED_POOL).expect("pool");
        let amixer = StubAmixer::new();
        amixer.set(
            "TestCard",
            "Made Up Control",
            AmixerReadOutcome::Found(LiveControlState {
                control_type: ControlType::Boolean,
                current_value: serde_json::Value::Bool(false),
                enum_values: vec![],
                integer_min: None,
                integer_max: None,
            }),
        );
        let caps = resolve_dsp_capabilities(
            &catalog,
            "Test Board",
            Some("test-dac"),
            &pool,
            &amixer,
        )
        .await;
        assert_eq!(caps.controls.len(), 1);
        let entry = &caps.controls[0];
        assert_eq!(entry.name, "Made Up Control");
        assert_eq!(entry.human_label, "Made Up Control");
        assert!(entry.unbound_pool_entry);
        assert!(entry.bound);
        assert!(entry.recommended_default.is_none());
        assert!(entry.description.is_empty());
        // Type comes from amixer when pool is absent.
        assert!(matches!(entry.control_type, ControlType::Boolean));
    }

    #[tokio::test]
    async fn no_alsa_card_hint_surfaces_diagnostic_and_unbinds_every_control() {
        // Sparky / Tinkerboard DACs in the catalog have no
        // alsa_card_hint. The resolver cannot probe amixer; every
        // control surfaces with bound = false + a card-hint-missing
        // unbound_reason; the top-level diagnostic carries
        // NoAlsaCardHint.
        let catalog_toml = r#"
schema_version = 1
[[boards]]
name = "Test Board"
provider = "noop"
[[boards.dacs]]
id = "test-dac"
display_name = "Test DAC"
overlay = ""
alsa_card_hint = ""
needs_reboot_on_apply = false
advanced_settings_enabled = true
dsp_options = ["DSP Program"]
provenance = "test"
"#;
        let catalog = parse_evo_catalog(catalog_toml).expect("catalog");
        let pool = parse_dsp_control_pool(EMBEDDED_POOL).expect("pool");
        let amixer = StubAmixer::new();
        let caps = resolve_dsp_capabilities(
            &catalog,
            "Test Board",
            Some("test-dac"),
            &pool,
            &amixer,
        )
        .await;
        assert_eq!(caps.controls.len(), 1);
        assert!(caps.alsa_card_hint.is_none());
        assert!(caps
            .diagnostics
            .iter()
            .any(|d| matches!(d, ResolverDiagnostic::NoAlsaCardHint { .. })));
        let ctl = &caps.controls[0];
        assert!(!ctl.bound);
        assert!(ctl.unbound_reason.contains("alsa_card_hint"));
    }

    #[tokio::test]
    async fn advanced_settings_enabled_carries_through_from_catalog() {
        // Test that the per-DAC advanced_settings_enabled flag
        // surfaces verbatim on the capability set. Default in the
        // shipped catalog is true; a hand-authored catalog with
        // false MUST surface false.
        let catalog_toml = r#"
schema_version = 1
[[boards]]
name = "Test Board"
provider = "noop"
[[boards.dacs]]
id = "locked-dac"
display_name = "Vendor-Locked DAC"
overlay = "vendor"
alsa_card_hint = "VendorCard"
needs_reboot_on_apply = false
advanced_settings_enabled = false
dsp_options = []
provenance = "test"
"#;
        let catalog = parse_evo_catalog(catalog_toml).expect("catalog");
        let pool = parse_dsp_control_pool(EMBEDDED_POOL).expect("pool");
        let amixer = StubAmixer::new();
        let caps = resolve_dsp_capabilities(
            &catalog,
            "Test Board",
            Some("locked-dac"),
            &pool,
            &amixer,
        )
        .await;
        assert!(!caps.advanced_settings_enabled);
    }

    #[tokio::test]
    async fn introspection_failure_surfaces_reason_verbatim() {
        let catalog_toml = r#"
schema_version = 1
[[boards]]
name = "Test Board"
provider = "noop"
[[boards.dacs]]
id = "test-dac"
display_name = "Test DAC"
overlay = "x"
alsa_card_hint = "TestCard"
needs_reboot_on_apply = false
advanced_settings_enabled = true
dsp_options = ["DSP Program"]
provenance = "test"
"#;
        let catalog = parse_evo_catalog(catalog_toml).expect("catalog");
        let pool = parse_dsp_control_pool(EMBEDDED_POOL).expect("pool");
        let amixer = StubAmixer::new();
        amixer.set(
            "TestCard",
            "DSP Program",
            AmixerReadOutcome::IntrospectionFailed {
                reason: "amixer subprocess returned exit 1: 'card not found'"
                    .into(),
            },
        );
        let caps = resolve_dsp_capabilities(
            &catalog,
            "Test Board",
            Some("test-dac"),
            &pool,
            &amixer,
        )
        .await;
        assert_eq!(caps.controls.len(), 1);
        let ctl = &caps.controls[0];
        assert!(!ctl.bound);
        assert!(ctl.unbound_reason.contains("card not found"));
    }
}
