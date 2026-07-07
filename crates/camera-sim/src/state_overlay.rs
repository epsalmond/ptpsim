use std::collections::BTreeMap;

use camera_config::{parse_hex_code, CameraManifest};
use ptp_core::dataset::PropValue;
use serde::{Deserialize, Serialize};

use crate::state::{CameraState, Phase};

const STARTUP_STATE_SCHEMA: &str = "ptpsim-startup-state/v1";
const STATE_OVERLAY_SCHEMA: &str = "ptpsim-state-overlay/v1";

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StateOverlay {
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub connection: Option<String>,
    #[serde(default)]
    pub phase: Option<String>,
    #[serde(default, alias = "sessionOpen")]
    pub session_open: Option<bool>,
    #[serde(default)]
    pub props: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct AppliedStateOverlay {
    pub props: usize,
    pub phase: bool,
    pub session_open: bool,
}

impl StateOverlay {
    pub fn validate_context(&self, profile: &str, connection: &str) -> Result<(), String> {
        if let Some(schema) = self.schema.as_deref() {
            if schema != STARTUP_STATE_SCHEMA && schema != STATE_OVERLAY_SCHEMA {
                return Err(format!(
                    "state overlay schema '{schema}' is not supported; expected '{STARTUP_STATE_SCHEMA}' or '{STATE_OVERLAY_SCHEMA}'"
                ));
            }
        }
        if let Some(expected) = self.profile.as_deref() {
            if expected != profile {
                return Err(format!(
                    "startup state profile '{expected}' does not match running profile '{profile}'"
                ));
            }
        }
        if let Some(expected) = self.connection.as_deref() {
            if expected != connection {
                return Err(format!(
                    "startup state connection '{expected}' does not match running connection '{connection}'"
                ));
            }
        }
        Ok(())
    }
}

pub(crate) fn apply_overlay(
    manifest: &CameraManifest,
    state: &mut CameraState,
    overlay: &StateOverlay,
) -> Result<AppliedStateOverlay, String> {
    let staged = StagedOverlay::from_overlay(manifest, overlay)?;

    if let Some(phase) = staged.phase {
        state.phase = phase;
        state.reset_gates();
    }
    if let Some(session_open) = staged.session_open {
        state.session_open = session_open;
    }
    for (code, value) in staged.props {
        state.props.insert(code, value);
    }

    Ok(staged.applied)
}

struct StagedOverlay {
    phase: Option<Phase>,
    session_open: Option<bool>,
    props: Vec<(u16, PropValue)>,
    applied: AppliedStateOverlay,
}

impl StagedOverlay {
    fn from_overlay(manifest: &CameraManifest, overlay: &StateOverlay) -> Result<Self, String> {
        let phase = match overlay.phase.as_deref() {
            Some(phase) => Some(
                Phase::from_state_name(phase)
                    .ok_or_else(|| format!("unknown phase '{phase}' in state overlay"))?,
            ),
            None => None,
        };
        let mut props = Vec::with_capacity(overlay.props.len());
        for (code_key, raw) in &overlay.props {
            let code = parse_hex_code(code_key)
                .ok_or_else(|| format!("invalid property code '{code_key}' in state overlay"))?;
            let prop = manifest
                .property(code)
                .ok_or_else(|| format!("property '{code_key}' is not in the loaded manifest"))?;
            let value = prop_value_from_json(prop.ptype.as_deref(), raw)
                .map_err(|e| format!("property '{code_key}': {e}"))?;
            props.push((code, value));
        }

        Ok(Self {
            phase,
            session_open: overlay.session_open,
            applied: AppliedStateOverlay {
                props: props.len(),
                phase: phase.is_some(),
                session_open: overlay.session_open.is_some(),
            },
            props,
        })
    }
}

fn prop_value_from_json(
    prop_type: Option<&str>,
    value: &serde_json::Value,
) -> Result<PropValue, String> {
    match prop_type {
        Some(signed @ ("i8" | "i16" | "i32" | "i64")) => Err(format!(
            "signed property type '{signed}' is not supported by simulator state overlays yet"
        )),
        Some("u8") => checked_numeric(value, u8::MAX as i128).map(|v| PropValue::U8(v as u8)),
        Some("u16") | None => {
            checked_numeric(value, u16::MAX as i128).map(|v| PropValue::U16(v as u16))
        }
        Some("u32") => checked_numeric(value, u32::MAX as i128).map(|v| PropValue::U32(v as u32)),
        Some("u64") => checked_numeric(value, u64::MAX as i128).map(|v| PropValue::U64(v as u64)),
        Some("str") => value
            .as_str()
            .map(|s| PropValue::Str(s.to_string()))
            .ok_or_else(|| "expected string value".to_string()),
        Some(other) => Err(format!("unsupported property type '{other}'")),
    }
}

fn checked_numeric(value: &serde_json::Value, max: i128) -> Result<i128, String> {
    let raw = match value {
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(i128::from)
            .or_else(|| n.as_u64().map(i128::from))
            .ok_or_else(|| format!("number {n} is outside supported range"))?,
        serde_json::Value::String(s) => parse_numeric_string(s)?,
        _ => return Err("expected numeric value".to_string()),
    };
    if raw < 0 || raw > max {
        Err(format!("value {raw} is outside 0..={max}"))
    } else {
        Ok(raw)
    }
}

fn parse_numeric_string(value: &str) -> Result<i128, String> {
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        i128::from_str_radix(hex, 16).map_err(|e| format!("invalid hex number '{value}': {e}"))
    } else {
        value
            .parse::<i128>()
            .map_err(|e| format!("invalid number '{value}': {e}"))
    }
}
