//! The generic responder engine. It dispatches PTP operations against the
//! manifest + media store with **no** manufacturer-specific branches: which ops
//! exist, which properties have which forms, and which workflow they belong to
//! are all manifest data. The handlers here are generic PTP semantics.

use camera_config::{parse_hex_code, CameraManifest};
use camera_media_store::{MediaStore, ObjectQuery};
use ptp_core::codes::{op, resp};
use ptp_core::dataset::PropValue;
use ptp_core::{DeviceInfo, OperationRequest, OperationResponse, Reader, Writer};

use crate::fault::{Fault, FaultSet};
use crate::state::{
    build_prop_desc, datatype_of, CameraState, Phase, DF01_IMAGE_IMPORT, DF01_LIVE_VIEW, PROP_DF01,
};

const STORAGE_ID: u32 = 0x0001_0001;

/// The engine's answer to one operation: a bare response, a data phase plus
/// response, or a directive to close the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Response(OperationResponse),
    Data {
        data: Vec<u8>,
        response: OperationResponse,
    },
    Close,
}

pub struct Engine {
    manifest: CameraManifest,
    store: MediaStore,
    state: CameraState,
    faults: FaultSet,
}

impl Engine {
    pub fn new(manifest: CameraManifest, store: MediaStore) -> Self {
        let state = CameraState::from_manifest(&manifest);
        Engine {
            manifest,
            store,
            state,
            faults: FaultSet::default(),
        }
    }

    pub fn state(&self) -> &CameraState {
        &self.state
    }

    /// Install a fault (control API `/faults`). Checked before normal dispatch.
    pub fn install_fault(&mut self, fault: Fault) {
        self.faults.install(fault);
    }

    pub fn clear_faults(&mut self) {
        self.faults.clear();
    }

    pub fn manifest(&self) -> &CameraManifest {
        &self.manifest
    }

    pub fn store(&self) -> &MediaStore {
        &self.store
    }

    fn ok(tid: u32) -> Reply {
        Reply::Response(OperationResponse {
            code: resp::OK,
            transaction_id: tid,
            params: vec![],
        })
    }

    fn err(tid: u32, code: u16) -> Reply {
        Reply::Response(OperationResponse {
            code,
            transaction_id: tid,
            params: vec![],
        })
    }

    fn data(tid: u32, data: Vec<u8>) -> Reply {
        Reply::Data {
            data,
            response: OperationResponse {
                code: resp::OK,
                transaction_id: tid,
                params: vec![],
            },
        }
    }

    /// Handle one operation. `data_in` carries an initiator data phase (e.g. the
    /// value for `SetDevicePropValue`).
    pub fn on_operation(&mut self, req: &OperationRequest, data_in: Option<&[u8]>) -> Reply {
        let tid = req.transaction_id;
        let p = |i: usize| req.params.get(i).copied().unwrap_or(0);

        // Injected faults take precedence over normal handling.
        if let Some(fault) = self.faults.match_op(req.code) {
            return match fault {
                Fault::FailOperation { response, .. } => Self::err(tid, *response),
                Fault::CloseOnOperation { .. } => Reply::Close,
            };
        }

        // OpenSession is the only thing allowed before a session exists.
        if !self.state.session_open
            && req.code != op::OPEN_SESSION
            && req.code != op::GET_DEVICE_INFO
        {
            return Self::err(tid, resp::SESSION_NOT_OPEN);
        }

        match req.code {
            op::OPEN_SESSION => {
                self.state.session_open = true;
                self.state.phase = Phase::SessionOpen;
                Self::ok(tid)
            }
            op::CLOSE_SESSION => {
                self.state.session_open = false;
                self.state.phase = Phase::Closed;
                Self::ok(tid)
            }
            op::GET_DEVICE_INFO => Self::data(tid, self.device_info_bytes()),
            op::GET_STORAGE_IDS => {
                let mut w = Writer::new();
                w.ptp_array(&[STORAGE_ID], |w, v| w.u32(*v));
                Self::data(tid, w.into_vec())
            }
            op::GET_OBJECT_HANDLES => {
                let handles = self.file_handles();
                let mut w = Writer::new();
                w.ptp_array(&handles, |w, v| w.u32(*v));
                Self::data(tid, w.into_vec())
            }
            op::GET_OBJECT_INFO => match self.store.object_info(p(0)) {
                Ok(oi) => {
                    let mut w = Writer::new();
                    if oi.encode(&mut w).is_err() {
                        return Self::err(tid, resp::GENERAL_ERROR);
                    }
                    Self::data(tid, w.into_vec())
                }
                Err(_) => Self::err(tid, resp::INVALID_OBJECT_HANDLE),
            },
            op::GET_THUMB => match self.store.thumbnail(p(0)).and_then(|s| s.read()) {
                Ok(bytes) => Self::data(tid, bytes),
                Err(_) => Self::err(tid, resp::INVALID_OBJECT_HANDLE),
            },
            op::GET_PARTIAL_OBJECT => {
                match self
                    .store
                    .read_range(p(0), p(1) as u64, p(2))
                    .and_then(|s| s.read())
                {
                    Ok(bytes) => Self::data(tid, bytes),
                    Err(_) => Self::err(tid, resp::INVALID_OBJECT_HANDLE),
                }
            }
            op::GET_OBJECT => {
                let size = self.store.object_size(p(0)).unwrap_or(0);
                let len = size.min(u32::MAX as u64) as u32;
                match self.store.read_range(p(0), 0, len).and_then(|s| s.read()) {
                    Ok(bytes) => Self::data(tid, bytes),
                    Err(_) => Self::err(tid, resp::INVALID_OBJECT_HANDLE),
                }
            }
            op::GET_DEVICE_PROP_DESC => {
                match build_prop_desc(&self.manifest, &self.state, p(0) as u16) {
                    Some(desc) => {
                        let mut w = Writer::new();
                        let _ = desc.encode(&mut w);
                        Self::data(tid, w.into_vec())
                    }
                    None => Self::err(tid, resp::DEVICE_PROP_NOT_SUPPORTED),
                }
            }
            op::GET_DEVICE_PROP_VALUE => {
                let code = p(0) as u16;
                // 0xd212 is a *computed* live-status bundle, not a stored value:
                // assemble it from current state via the shared quirk primitive.
                if code == 0xd212 {
                    return Self::data(tid, self.status_d212());
                }
                match self.state.props.get(&code) {
                    Some(v) => {
                        let mut w = Writer::new();
                        let _ = v.encode(&mut w);
                        Self::data(tid, w.into_vec())
                    }
                    None => Self::err(tid, resp::DEVICE_PROP_NOT_SUPPORTED),
                }
            }
            op::SET_DEVICE_PROP_VALUE => self.set_prop(tid, p(0) as u16, data_in),
            op::INITIATE_OPEN_CAPTURE => {
                if matches!(self.state.phase, Phase::LiveView | Phase::Streaming) {
                    self.state.phase = Phase::Streaming;
                    Self::ok(tid)
                } else {
                    Self::err(tid, resp::GENERAL_ERROR)
                }
            }
            // Vendor / other ops: resolve from the manifest, not a brand branch.
            other => self.dispatch_manifest_op(tid, other, &req.params),
        }
    }

    /// Manifest-driven dispatch for non-standard ops: a `property.step` handler
    /// runs the generic vendor-step; any other supported op is a successful
    /// no-op; unknown ops are unsupported.
    fn dispatch_manifest_op(&mut self, tid: u32, code: u16, params: &[u32]) -> Reply {
        let Some(opdef) = self.manifest.operation(code) else {
            return Self::err(tid, resp::OPERATION_NOT_SUPPORTED);
        };
        match opdef.handler.as_deref() {
            Some("property.step") => {
                let Some(prop_code) = opdef.property.as_deref().and_then(parse_hex_code) else {
                    return Self::err(tid, resp::GENERAL_ERROR);
                };
                let direction = params.first().copied().unwrap_or(0);
                self.vendor_step(prop_code, direction);
                Self::ok(tid)
            }
            _ => Self::ok(tid),
        }
    }

    /// Advance a property's current value within its enum descriptor. `direction`
    /// follows the manifest's convention (non-zero = "wider"/up the list).
    fn vendor_step(&mut self, prop_code: u16, direction: u32) {
        let Some(prop) = self.manifest.property(prop_code) else {
            return;
        };
        let Some(desc) = &prop.descriptor else { return };
        if desc.values.is_empty() {
            return;
        }
        let datatype = datatype_of(prop.ptype.as_deref());
        let current = self.state.props.get(&prop_code).cloned();
        let cur_idx = current
            .and_then(|c| value_to_i64(&c))
            .and_then(|cv| desc.values.iter().position(|v| *v == cv))
            .unwrap_or(0);
        let new_idx = if direction != 0 {
            (cur_idx + 1).min(desc.values.len() - 1)
        } else {
            cur_idx.saturating_sub(1)
        };
        self.state.props.insert(
            prop_code,
            crate::state::typed(datatype, desc.values[new_idx]),
        );
    }

    fn set_prop(&mut self, tid: u32, code: u16, data_in: Option<&[u8]>) -> Reply {
        let Some(prop) = self.manifest.property(code) else {
            return Self::err(tid, resp::DEVICE_PROP_NOT_SUPPORTED);
        };
        let datatype = datatype_of(prop.ptype.as_deref());
        let Some(bytes) = data_in else {
            return Self::err(tid, resp::INVALID_PARAMETER);
        };
        let mut r = Reader::new(bytes);
        let Ok(value) = PropValue::decode(&mut r, datatype) else {
            return Self::err(tid, resp::INVALID_PARAMETER);
        };
        // Function-mode selector drives the workflow phase.
        if code == PROP_DF01 {
            if let Some(n) = value_to_i64(&value) {
                self.state.phase = match n as u32 {
                    DF01_IMAGE_IMPORT => Phase::ImageImport,
                    DF01_LIVE_VIEW => Phase::LiveView,
                    _ => self.state.phase,
                };
            }
        }
        self.state.props.insert(code, value);
        Self::ok(tid)
    }

    fn file_handles(&self) -> Vec<u32> {
        use ptp_core::codes::format::ASSOCIATION;
        self.store
            .handles(ObjectQuery::default())
            .into_iter()
            .filter(|h| {
                self.store
                    .object_info(*h)
                    .map(|oi| oi.object_format != ASSOCIATION)
                    .unwrap_or(false)
            })
            .collect()
    }

    /// Assemble the `0xd212` live-status readback from current state using the
    /// shared computed-quirk primitive (concern-organized, not a Fuji branch).
    fn status_d212(&self) -> Vec<u8> {
        let aperture = match self.state.props.get(&0x5007) {
            Some(PropValue::U16(v)) => *v,
            _ => 0,
        };
        let iso = match self.state.props.get(&0xd02a) {
            Some(PropValue::U32(v)) => *v,
            _ => 0,
        };
        protocol_primitives::quirk::status_d212(aperture, iso, false)
    }

    fn device_info_bytes(&self) -> Vec<u8> {
        let ops: Vec<u16> = self
            .manifest
            .operations
            .keys()
            .filter_map(|k| parse_hex_code(k))
            .collect();
        let props: Vec<u16> = self
            .manifest
            .properties
            .keys()
            .filter_map(|k| parse_hex_code(k))
            .collect();
        let di = DeviceInfo {
            standard_version: 100,
            vendor_extension_id: 0x0000_00ff,
            model: self.manifest.camera.model.clone(),
            manufacturer: self.manifest.camera.manufacturer.clone(),
            device_version: self.manifest.camera.firmware.clone(),
            operations_supported: ops,
            device_properties_supported: props,
            ..Default::default()
        };
        let mut w = Writer::new();
        let _ = di.encode(&mut w);
        w.into_vec()
    }
}

fn value_to_i64(v: &PropValue) -> Option<i64> {
    Some(match v {
        PropValue::U8(x) => *x as i64,
        PropValue::U16(x) => *x as i64,
        PropValue::U32(x) => *x as i64,
        PropValue::U64(x) => *x as i64,
        PropValue::Str(_) => return None,
    })
}
