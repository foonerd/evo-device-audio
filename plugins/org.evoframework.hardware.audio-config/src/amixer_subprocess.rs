//! `amixer cget` / `amixer cset` subprocess invocation +
//! response parsing. Shared between board-class providers so the
//! amixer interaction surface is one canonical runtime path.
//!
//! Per-provider amixer impls (PiProvider, future RockchipProvider,
//! …) layer test-state stubbing on top; the production amixer
//! invocation lives here.

use tokio::process::Command;

use crate::dsp::{AmixerListOutcome, AmixerReadOutcome, LiveControlState};
use crate::dsp_pool::ControlType;
use crate::provider::{AmixerWriteOutcome, AmixerWriteValue};

/// Parse `amixer cget` output into a [`LiveControlState`].
///
/// The amixer-cget output format is well-known but multi-line.
/// Example for an enum control:
///
/// ```text
/// numid=12,iface=MIXER,name='DSP Program'
///   ; type=ENUMERATED,access=rw------,values=1,items=4
///   ; Item #0 'None'
///   ; Item #1 'DAC'
///   ; Item #2 'DAC+Headphone'
///   ; Item #3 'Headphone'
///   : values=0
/// ```
///
/// For integer / db_scale controls:
///
/// ```text
/// numid=N,iface=MIXER,name='Clock Missing Period'
///   ; type=INTEGER,access=rw------,values=1,min=0,max=10000,step=0
///   : values=0
/// ```
///
/// For boolean controls:
///
/// ```text
/// numid=N,iface=MIXER,name='Soft Mute'
///   ; type=BOOLEAN,access=rw------,values=1
///   : values=on
/// ```
///
/// Returns `Err` with an operator-readable diagnostic on any
/// parse failure (missing type line, unknown type, malformed
/// items list, etc.).
pub fn parse_amixer_cget(output: &str) -> Result<LiveControlState, String> {
    let mut control_type: Option<ControlType> = None;
    let mut enum_items: Vec<String> = Vec::new();
    let mut integer_min: Option<i64> = None;
    let mut integer_max: Option<i64> = None;
    let mut raw_values: Option<String> = None;

    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("; type=") {
            // Type line: parse type token + (for integer) min/max.
            let mut parts = rest.split(',');
            let type_token = parts
                .next()
                .ok_or_else(|| format!("missing type token in '{trimmed}'"))?;
            control_type = Some(match type_token {
                "ENUMERATED" => ControlType::Enum,
                "INTEGER" => ControlType::Integer,
                "BOOLEAN" => ControlType::Boolean,
                other => {
                    return Err(format!(
                        "unrecognised amixer control type {other:?} in '{trimmed}'"
                    ));
                }
            });
            for part in parts {
                if let Some(min_str) = part.trim().strip_prefix("min=") {
                    integer_min = min_str.parse::<i64>().ok();
                } else if let Some(max_str) = part.trim().strip_prefix("max=") {
                    integer_max = max_str.parse::<i64>().ok();
                }
            }
        } else if let Some(rest) = trimmed.strip_prefix("; Item #") {
            // Enum item line. Format: `N 'label'`.
            if let Some(label_start) = rest.find('\'') {
                if let Some(label_end) = rest.rfind('\'') {
                    if label_end > label_start {
                        let label = &rest[label_start + 1..label_end];
                        enum_items.push(label.to_string());
                    }
                }
            }
        } else if let Some(rest) = trimmed.strip_prefix(": values=") {
            raw_values = Some(rest.to_string());
        }
    }

    let control_type = control_type
        .ok_or_else(|| "amixer cget output missing 'type=' line".to_string())?;
    let raw = raw_values.ok_or_else(|| {
        "amixer cget output missing final ': values=' line".to_string()
    })?;

    let (current_value, enum_values, integer_min, integer_max) =
        match control_type {
            ControlType::Enum => {
                // raw is the zero-based index into items.
                let idx: usize = raw.parse::<usize>().map_err(|e| {
                    format!(
                        "amixer enum 'values=' is not an integer: {raw:?} ({e})"
                    )
                })?;
                let current = enum_items.get(idx).cloned().ok_or_else(|| {
                    format!(
                        "amixer enum 'values={idx}' out of range for items list of length {}",
                        enum_items.len()
                    )
                })?;
                (serde_json::Value::String(current), enum_items, None, None)
            }
            ControlType::Integer | ControlType::DbScale => {
                // raw is the integer value (possibly comma-separated
                // for multi-channel controls; take the first value).
                let first =
                    raw.split(',').next().unwrap_or(raw.as_str()).trim();
                let v: i64 = first.parse::<i64>().map_err(|e| {
                    format!(
                        "amixer integer 'values=' is not an integer: {first:?} ({e})"
                    )
                })?;
                (
                    serde_json::Value::Number(v.into()),
                    Vec::new(),
                    integer_min,
                    integer_max,
                )
            }
            ControlType::Boolean => {
                // raw is "on" or "off" (sometimes "true"/"false").
                // Multi-channel controls emit comma-separated values;
                // take the first.
                let first =
                    raw.split(',').next().unwrap_or(raw.as_str()).trim();
                let v = matches!(
                    first.to_ascii_lowercase().as_str(),
                    "on" | "true" | "1"
                );
                (serde_json::Value::Bool(v), Vec::new(), None, None)
            }
        };

    Ok(LiveControlState {
        control_type,
        current_value,
        enum_values,
        integer_min,
        integer_max,
    })
}

/// Encode an [`AmixerWriteValue`] into the value string `amixer
/// cset` accepts. Enum labels pass verbatim (amixer accepts the
/// human-readable label); integers as decimal; booleans as on/off.
pub fn encode_amixer_write_value(value: &AmixerWriteValue) -> String {
    match value {
        AmixerWriteValue::EnumLabel(s) => s.clone(),
        AmixerWriteValue::Integer(n) => n.to_string(),
        AmixerWriteValue::Boolean(true) => "on".to_string(),
        AmixerWriteValue::Boolean(false) => "off".to_string(),
    }
}

/// Invoke `amixer -c <card> controls` and parse the resulting
/// control-name list. Returns:
///
/// * [`AmixerListOutcome::Found`] with the discovered control
///   names on success (may be an empty vec for cards without
///   mixer controls).
/// * [`AmixerListOutcome::CardUnknown`] when amixer reports the
///   card hint cannot be matched.
/// * [`AmixerListOutcome::IntrospectionFailed`] for spawn /
///   parse / unrecognised-exit errors.
pub async fn amixer_scontrols_via_subprocess(
    card_hint: &str,
) -> AmixerListOutcome {
    let output = match Command::new("amixer")
        .args(["-c", card_hint, "controls"])
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            return AmixerListOutcome::IntrospectionFailed {
                reason: format!("spawn amixer controls: {e}"),
            };
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let stderr_lc = stderr.to_ascii_lowercase();
        if stderr_lc.contains("no such file") || stderr_lc.contains("card") {
            return AmixerListOutcome::CardUnknown {
                reason: format!("amixer controls refused: {stderr}"),
            };
        }
        return AmixerListOutcome::IntrospectionFailed {
            reason: format!(
                "amixer controls exit {}: {stderr}",
                output.status.code().unwrap_or(-1)
            ),
        };
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let names = parse_amixer_controls(&stdout);
    AmixerListOutcome::Found(names)
}

/// Parse `amixer controls` output into a deduplicated list of
/// `name='<token>'` values, preserving discovery order. Each
/// line is shaped:
///
/// ```text
/// numid=N,iface=MIXER,name='<token>',index=K
/// ```
///
/// Lines that don't carry a `name='...'` field are skipped.
/// Duplicates are dropped (some cards expose the same control
/// twice with different `index=` values; the operator-facing
/// surface treats them as one).
pub fn parse_amixer_controls(output: &str) -> Vec<String> {
    let mut seen: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut names: Vec<String> = Vec::new();
    for line in output.lines() {
        let Some(start) = line.find("name='") else {
            continue;
        };
        let after = &line[start + "name='".len()..];
        let Some(end) = after.find('\'') else {
            continue;
        };
        let name = after[..end].to_string();
        if name.is_empty() {
            continue;
        }
        if seen.insert(name.clone()) {
            names.push(name);
        }
    }
    names
}

/// Invoke `amixer -c <card> cget name='<control>'` and classify
/// the outcome. Returns:
///
/// * [`AmixerReadOutcome::Found`] on success (parse success).
/// * [`AmixerReadOutcome::CardUnknown`] when amixer's stderr
///   matches "no such file" / "card" patterns.
/// * [`AmixerReadOutcome::NotPresent`] when amixer's stderr
///   matches "not found" / "unable to find" / "cannot find"
///   patterns.
/// * [`AmixerReadOutcome::IntrospectionFailed`] for every other
///   error (spawn failure, parse failure, unrecognised exit).
pub async fn amixer_cget_via_subprocess(
    card_hint: &str,
    control_name: &str,
) -> AmixerReadOutcome {
    let name_arg = format!("name='{control_name}'");
    let output = match Command::new("amixer")
        .args(["-c", card_hint, "cget", &name_arg])
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            return AmixerReadOutcome::IntrospectionFailed {
                reason: format!("spawn amixer cget: {e}"),
            };
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let stderr_lc = stderr.to_ascii_lowercase();
        if stderr_lc.contains("no such file") || stderr_lc.contains("card") {
            return AmixerReadOutcome::CardUnknown {
                reason: format!("amixer cget refused: {stderr}"),
            };
        }
        if stderr_lc.contains("not found")
            || stderr_lc.contains("unable to find")
            || stderr_lc.contains("cannot find")
        {
            return AmixerReadOutcome::NotPresent {
                reason: format!(
                    "amixer reports control '{control_name}' not on card '{card_hint}': {stderr}"
                ),
            };
        }
        return AmixerReadOutcome::IntrospectionFailed {
            reason: format!(
                "amixer cget exit {}: {stderr}",
                output.status.code().unwrap_or(-1)
            ),
        };
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    match parse_amixer_cget(&stdout) {
        Ok(state) => AmixerReadOutcome::Found(state),
        Err(e) => AmixerReadOutcome::IntrospectionFailed {
            reason: format!("amixer cget parse failed: {e}"),
        },
    }
}

/// Invoke `amixer -c <card> cset name='<control>' <value>` and
/// classify the outcome. Returns:
///
/// * [`AmixerWriteOutcome::Applied`] on success.
/// * [`AmixerWriteOutcome::CardUnknown`] / `NotPresent` /
///   `ValueRejected` / `InvocationFailed` per the standard
///   amixer stderr-pattern mapping.
pub async fn amixer_cset_via_subprocess(
    card_hint: &str,
    control_name: &str,
    value: &AmixerWriteValue,
) -> AmixerWriteOutcome {
    let name_arg = format!("name='{control_name}'");
    let value_str = encode_amixer_write_value(value);
    let output = match Command::new("amixer")
        .args(["-c", card_hint, "cset", &name_arg, &value_str])
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            return AmixerWriteOutcome::InvocationFailed {
                reason: format!("spawn amixer cset: {e}"),
            };
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let stderr_lc = stderr.to_ascii_lowercase();
        if stderr_lc.contains("no such file") || stderr_lc.contains("card") {
            return AmixerWriteOutcome::CardUnknown {
                reason: format!("amixer cset refused: {stderr}"),
            };
        }
        if stderr_lc.contains("not found")
            || stderr_lc.contains("unable to find")
            || stderr_lc.contains("cannot find")
        {
            return AmixerWriteOutcome::NotPresent {
                reason: format!(
                    "amixer reports control '{control_name}' not on card '{card_hint}': {stderr}"
                ),
            };
        }
        if stderr_lc.contains("invalid")
            || stderr_lc.contains("out of range")
            || stderr_lc.contains("bad value")
        {
            return AmixerWriteOutcome::ValueRejected {
                reason: format!(
                    "amixer rejected value '{value_str}': {stderr}"
                ),
            };
        }
        return AmixerWriteOutcome::InvocationFailed {
            reason: format!(
                "amixer cset exit {}: {stderr}",
                output.status.code().unwrap_or(-1)
            ),
        };
    }
    AmixerWriteOutcome::Applied
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_amixer_cget_enum_returns_resolved_label_and_value_set() {
        let raw = "numid=12,iface=MIXER,name='DSP Program'\n  ; type=ENUMERATED,access=rw------,values=1,items=4\n  ; Item #0 'None'\n  ; Item #1 'DAC'\n  ; Item #2 'DAC+Headphone'\n  ; Item #3 'Headphone'\n  : values=2\n";
        let state = parse_amixer_cget(raw).expect("parse ok");
        assert!(matches!(state.control_type, ControlType::Enum));
        assert_eq!(
            state.current_value,
            serde_json::Value::String("DAC+Headphone".into())
        );
        assert_eq!(
            state.enum_values,
            vec!["None", "DAC", "DAC+Headphone", "Headphone"]
        );
    }

    #[test]
    fn parse_amixer_cget_integer_returns_min_max_and_value() {
        let raw = "numid=4,iface=MIXER,name='Clock Missing Period'\n  ; type=INTEGER,access=rw------,values=1,min=0,max=10000,step=0\n  : values=2500\n";
        let state = parse_amixer_cget(raw).expect("parse ok");
        assert!(matches!(state.control_type, ControlType::Integer));
        assert_eq!(state.current_value, serde_json::json!(2500));
        assert_eq!(state.integer_min, Some(0));
        assert_eq!(state.integer_max, Some(10000));
    }

    #[test]
    fn parse_amixer_cget_boolean_returns_bool_value() {
        let raw = "numid=7,iface=MIXER,name='Soft Mute'\n  ; type=BOOLEAN,access=rw------,values=1\n  : values=on\n";
        let state = parse_amixer_cget(raw).expect("parse ok");
        assert!(matches!(state.control_type, ControlType::Boolean));
        assert_eq!(state.current_value, serde_json::Value::Bool(true));
    }

    #[test]
    fn parse_amixer_cget_handles_multi_channel_integer() {
        let raw = "numid=1,iface=MIXER,name='Master Playback Volume'\n  ; type=INTEGER,access=rw------,values=2,min=-80,max=0,step=0\n  : values=-10,-10\n";
        let state = parse_amixer_cget(raw).expect("parse ok");
        assert_eq!(state.current_value, serde_json::json!(-10));
        assert_eq!(state.integer_min, Some(-80));
        assert_eq!(state.integer_max, Some(0));
    }

    #[test]
    fn parse_amixer_cget_refuses_missing_type_line() {
        let raw = "numid=N,iface=MIXER,name='x'\n  : values=0\n";
        assert!(parse_amixer_cget(raw).is_err());
    }

    #[test]
    fn parse_amixer_cget_refuses_missing_values_line() {
        let raw =
            "numid=N,iface=MIXER,name='x'\n  ; type=BOOLEAN,access=rw------,values=1\n";
        assert!(parse_amixer_cget(raw).is_err());
    }

    #[test]
    fn parse_amixer_cget_refuses_unrecognised_type() {
        let raw =
            "numid=N,iface=MIXER,name='x'\n  ; type=BYTES,access=rw------,values=1\n  : values=0\n";
        let err = parse_amixer_cget(raw).unwrap_err();
        assert!(err.contains("BYTES"));
    }

    #[test]
    fn parse_amixer_cget_refuses_enum_index_out_of_range() {
        let raw = "numid=N,iface=MIXER,name='x'\n  ; type=ENUMERATED,access=rw------,values=1,items=2\n  ; Item #0 'a'\n  ; Item #1 'b'\n  : values=5\n";
        assert!(parse_amixer_cget(raw).unwrap_err().contains("out of range"));
    }

    #[test]
    fn encode_amixer_write_value_round_trips() {
        assert_eq!(
            encode_amixer_write_value(&AmixerWriteValue::EnumLabel(
                "Slow Roll-Off".into()
            )),
            "Slow Roll-Off"
        );
        assert_eq!(
            encode_amixer_write_value(&AmixerWriteValue::Integer(-10)),
            "-10"
        );
        assert_eq!(
            encode_amixer_write_value(&AmixerWriteValue::Boolean(true)),
            "on"
        );
        assert_eq!(
            encode_amixer_write_value(&AmixerWriteValue::Boolean(false)),
            "off"
        );
    }
}
