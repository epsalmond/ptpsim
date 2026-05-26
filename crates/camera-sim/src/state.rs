//! Mutable per-session camera state: open flag, property values, and the
//! current workflow phase. Everything here is generic; what the values *mean*
//! comes from the manifest.

use camera_config::CameraManifest;
use ptp_core::dataset::{DevicePropDesc, PropForm, PropValue};
use ptp_core::codes::datatype_code as dt;
use std::collections::BTreeMap;

/// Fuji function-mode selector properties.
pub const PROP_DF00: u16 = 0xdf00;
pub const PROP_DF01: u16 = 0xdf01;
pub const DF01_IMAGE_IMPORT: u32 = 20;
pub const DF01_LIVE_VIEW: u32 = 22;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Disconnected,
    SessionOpen,
    ImageImport,
    LiveView,
    Streaming,
    Closed,
}

pub struct CameraState {
    pub session_open: bool,
    pub phase: Phase,
    /// Current property values, keyed by property code.
    pub props: BTreeMap<u16, PropValue>,
}

impl CameraState {
    /// Seed property values from manifest descriptors (enum -> first value,
    /// range -> min) so reads have something defined to return.
    pub fn from_manifest(manifest: &CameraManifest) -> Self {
        let mut props = BTreeMap::new();
        for (code_key, prop) in &manifest.properties {
            let Some(code) = camera_config::parse_hex_code(code_key) else { continue };
            let datatype = datatype_of(prop.ptype.as_deref());
            if let Some(desc) = &prop.descriptor {
                if let Some(first) = desc.values.first() {
                    props.insert(code, typed(datatype, *first));
                }
            }
        }
        CameraState { session_open: false, phase: Phase::Disconnected, props }
    }

    /// The manifest control-mode key matching the current phase, used to resolve
    /// "intent -> mechanism" the same way the app does.
    pub fn mode_key(&self) -> &'static str {
        match self.phase {
            Phase::LiveView | Phase::Streaming => "liveView",
            Phase::ImageImport => "imageImport",
            _ => "",
        }
    }
}

pub fn datatype_of(ty: Option<&str>) -> u16 {
    match ty {
        Some("u8") => dt::UINT8,
        Some("u16") => dt::UINT16,
        Some("u32") => dt::UINT32,
        Some("u64") => dt::UINT64,
        Some("str") => dt::STR,
        _ => dt::UINT16,
    }
}

pub fn typed(datatype: u16, v: i64) -> PropValue {
    match datatype {
        dt::UINT8 => PropValue::U8(v as u8),
        dt::UINT16 => PropValue::U16(v as u16),
        dt::UINT32 => PropValue::U32(v as u32),
        dt::UINT64 => PropValue::U64(v as u64),
        _ => PropValue::U16(v as u16),
    }
}

/// Build a `DevicePropDesc` for `code` from the manifest property entry and the
/// current value in state.
pub fn build_prop_desc(
    manifest: &CameraManifest,
    state: &CameraState,
    code: u16,
) -> Option<DevicePropDesc> {
    let prop = manifest.property(code)?;
    let datatype = datatype_of(prop.ptype.as_deref());
    let current = state.props.get(&code).cloned().unwrap_or(typed(datatype, 0));
    let get_set = match prop.access.as_deref() {
        Some("readWrite") => 1,
        _ => 0,
    };
    let form = match &prop.descriptor {
        Some(d) if d.form == "enum" => {
            PropForm::Enum(d.values.iter().map(|v| typed(datatype, *v)).collect())
        }
        Some(d) if d.form == "range" && d.values.len() == 3 => PropForm::Range {
            min: typed(datatype, d.values[0]),
            max: typed(datatype, d.values[1]),
            step: typed(datatype, d.values[2]),
        },
        _ => PropForm::None,
    };
    Some(DevicePropDesc {
        code,
        datatype,
        get_set,
        factory_default: current.clone(),
        current,
        form,
    })
}
