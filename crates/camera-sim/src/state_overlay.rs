use std::collections::BTreeMap;

use camera_config::{parse_hex_code, CameraManifest};
use ptp_core::codes::datatype_code as dt;
use ptp_core::dataset::PropValue;
use serde::{Deserialize, Serialize};

use crate::state::{datatype_of, CameraState, Phase};

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
    let mut applied = AppliedStateOverlay::default();

    if let Some(phase) = overlay.phase.as_deref() {
        let parsed = Phase::from_state_name(phase)
            .ok_or_else(|| format!("unknown phase '{phase}' in state overlay"))?;
        state.phase = parsed;
        state.reset_gates();
        applied.phase = true;
    }

    if let Some(session_open) = overlay.session_open {
        state.session_open = session_open;
        applied.session_open = true;
    }

    for (code_key, raw) in &overlay.props {
        let code = parse_hex_code(code_key)
            .ok_or_else(|| format!("invalid property code '{code_key}' in state overlay"))?;
        let prop = manifest
            .property(code)
            .ok_or_else(|| format!("property '{code_key}' is not in the loaded manifest"))?;
        let datatype = datatype_of(prop.ptype.as_deref());
        let value = prop_value_from_json(datatype, raw)
            .map_err(|e| format!("property '{code_key}': {e}"))?;
        state.props.insert(code, value);
        applied.props += 1;
    }

    Ok(applied)
}

fn prop_value_from_json(datatype: u16, value: &serde_json::Value) -> Result<PropValue, String> {
    match datatype {
        dt::UINT8 => checked_numeric(value, u8::MAX as i128).map(|v| PropValue::U8(v as u8)),
        dt::UINT16 => checked_numeric(value, u16::MAX as i128).map(|v| PropValue::U16(v as u16)),
        dt::UINT32 => checked_numeric(value, u32::MAX as i128).map(|v| PropValue::U32(v as u32)),
        dt::UINT64 => checked_numeric(value, u64::MAX as i128).map(|v| PropValue::U64(v as u64)),
        dt::STR => value
            .as_str()
            .map(|s| PropValue::Str(s.to_string()))
            .ok_or_else(|| "expected string value".to_string()),
        other => Err(format!("unsupported property datatype 0x{other:04x}")),
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
