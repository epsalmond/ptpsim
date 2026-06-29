//! The generic responder engine. It dispatches PTP operations against the
//! manifest + media store with **no** manufacturer-specific branches: which ops
//! exist, which properties have which forms, and which workflow they belong to
//! are all manifest data. The handlers here are generic PTP semantics.

use camera_config::{parse_hex_code, CameraManifest};
use camera_media_store::{ByteSource, MediaStore, ObjectQuery, SIZE_CEILING};
use ptp_core::codes::{op, resp};
use ptp_core::dataset::PropValue;
use ptp_core::{DeviceInfo, OperationRequest, OperationResponse, Reader, Writer};

use crate::fault::{Fault, FaultSet};
use crate::state::{
    build_prop_desc, datatype_of, CameraState, Phase, DF01_IMAGE_IMPORT, DF01_LIVE_VIEW, PROP_DF01,
};

const STORAGE_ID: u32 = 0x0001_0001;

/// The engine's answer to one operation: a bare response, a data phase plus
/// response, or a directive to close the connection. `Data` carries a small
/// synthesized payload (device info, prop values); `DataStream` carries an
/// object body the service writes in bounded chunks so a multi-GB file never
/// lands in memory (DESIGN.md: "File downloads use bounded chunk buffers").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Response(OperationResponse),
    Data {
        data: Vec<u8>,
        response: OperationResponse,
    },
    DataStream {
        source: ByteSource,
        response: OperationResponse,
    },
    Close,
}

pub struct Engine {
    manifest: CameraManifest,
    store: MediaStore,
    state: CameraState,
    faults: FaultSet,
    /// Cross-transport arming link (#102): the BLE `IMAGE_TRANSFER_SETTING` write
    /// arms the session that function-launch brings up. Default armed (standalone).
    link: crate::link::SharedLink,
}

impl Engine {
    pub fn new(manifest: CameraManifest, store: MediaStore) -> Self {
        let state = CameraState::from_manifest(&manifest);
        Engine {
            manifest,
            store,
            state,
            faults: FaultSet::default(),
            link: crate::link::SharedLink::default(),
        }
    }

    /// A clone of this engine's arming link (#102), to hand to the BLE responder so
    /// its `IMAGE_TRANSFER_SETTING` / function-launch writes arm THIS engine.
    pub fn link(&self) -> crate::link::SharedLink {
        std::sync::Arc::clone(&self.link)
    }

    /// Whether the PTP/IP `InitCommandRequest` handshake should be answered — false
    /// when a BLE AP handoff launched without the arming prep write (#102). A
    /// standalone camera is armed by default.
    pub fn accepts_init(&self) -> bool {
        self.link.is_armed()
    }

    pub fn state(&self) -> &CameraState {
        &self.state
    }

    /// Current phase — small Copy enum, safe to read under a brief lock from
    /// the live-view writer to gate emission on Phase::Streaming.
    pub fn phase(&self) -> Phase {
        self.state.phase
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

    fn data_stream(tid: u32, source: ByteSource) -> Reply {
        Reply::DataStream {
            source,
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

        let reply = match req.code {
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
            op::GET_THUMB => match self.store.thumbnail(p(0)) {
                Ok(source) => Self::data_stream(tid, source),
                Err(_) => Self::err(tid, resp::INVALID_OBJECT_HANDLE),
            },
            op::GET_PARTIAL_OBJECT => match self.store.read_range(p(0), p(1) as u64, p(2)) {
                Ok(source) => Self::data_stream(tid, source),
                Err(_) => Self::err(tid, resp::INVALID_OBJECT_HANDLE),
            },
            op::GET_OBJECT => {
                // The PTP `ObjectInfo` size field is 32-bit (`SIZE_CEILING`);
                // objects ≥ 4 GiB are memory-card-only on the wire. We clamp at
                // the ceiling and stream chunks, so the request never allocates
                // a multi-GB buffer even on the boundary case.
                let size = self.store.object_size(p(0)).unwrap_or(0);
                let len = size.min(SIZE_CEILING) as u32;
                match self.store.read_range(p(0), 0, len) {
                    Ok(source) => Self::data_stream(tid, source),
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
                // A deferred op-effect transition settles on its scheduled poll.
                self.state.resolve_pending(code);
                // 0xd212 is a *computed* live-status bundle, not a stored value:
                // assemble it from current state via the shared quirk primitive.
                if code == 0xd212 {
                    Self::data(tid, self.status_d212())
                } else if code == 0xd621 {
                    // The Fuji object-handle list property (#46): the manifest's
                    // enumerate/import actions read it as a u32 array. Serve the
                    // real handles (same encoding as GetObjectHandles 0x1007) so
                    // the property-driven enumeration is believable, not a default.
                    let mut w = Writer::new();
                    w.ptp_array(&self.file_handles(), |w, v| w.u32(*v));
                    Self::data(tid, w.into_vec())
                } else if code == 0xd620 {
                    // The object count that sizes the 0xd621 list (declared u32).
                    let mut w = Writer::new();
                    w.u32(self.file_handles().len() as u32);
                    Self::data(tid, w.into_vec())
                } else {
                    // A manifest-declared prop always returns a value (current, else
                    // a typed default) — a real camera doesn't reject a supported
                    // prop just because nothing set it yet. Only props absent from
                    // the manifest are unsupported.
                    match self.state.props.get(&code) {
                        Some(v) => {
                            let mut w = Writer::new();
                            let _ = v.encode(&mut w);
                            Self::data(tid, w.into_vec())
                        }
                        None => match self.manifest.property(code) {
                            Some(prop) => {
                                let v = crate::state::typed(datatype_of(prop.ptype.as_deref()), 0);
                                let mut w = Writer::new();
                                let _ = v.encode(&mut w);
                                Self::data(tid, w.into_vec())
                            }
                            None => Self::err(tid, resp::DEVICE_PROP_NOT_SUPPORTED),
                        },
                    }
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
        };
        // Apply this op's manifest-declared camera-side effects on success —
        // arming immediate writes or deferred (poll-settled) transitions (§5.5
        // AF stub). Generic: the behavior is manifest data, not a brand branch.
        if reply_is_ok(&reply) {
            self.apply_op_effects(req.code);
            self.apply_op_emits(req.code);
        }
        reply
    }

    /// Arm the manifest-declared [`OpEffect`]s of operation `code` against state.
    /// No-op for ops without effects (the common case).
    fn apply_op_effects(&mut self, code: u16) {
        let Some(opdef) = self.manifest.operation(code) else {
            return;
        };
        if opdef.effects.is_empty() {
            return;
        }
        // Snapshot (target code, value, settle) under the immutable manifest
        // borrow, then mutate state.
        let armed: Vec<(u16, i64, u32)> = opdef
            .effects
            .iter()
            .filter_map(|e| parse_hex_code(&e.set_prop).map(|c| (c, e.value, e.settle_after_polls)))
            .collect();
        for (target, value, settle) in armed {
            let datatype = datatype_of(
                self.manifest
                    .property(target)
                    .and_then(|p| p.ptype.as_deref()),
            );
            self.state
                .arm_effect(target, crate::state::typed(datatype, value), settle);
        }
    }

    /// Queue operation `code`'s manifest-declared completion events (#54). A
    /// sibling to [`apply_op_effects`](Self::apply_op_effects) — kept separate
    /// because an event is a signal, not a value mutation, and an op may emit
    /// without arming any effect. Both fire only on an OK response.
    fn apply_op_emits(&mut self, code: u16) {
        let Some(opdef) = self.manifest.operation(code) else {
            return;
        };
        if opdef.emits.is_empty() {
            return;
        }
        let codes: Vec<u16> = opdef
            .emits
            .iter()
            .filter_map(|c| parse_hex_code(c))
            .collect();
        for c in codes {
            self.state.push_event(c);
        }
    }

    /// Take a queued completion event by code — the reference executor's
    /// event-source `awaitUntil` drains here (see [`CameraState::take_event`]).
    pub fn take_event(&mut self, code: u16) -> bool {
        self.state.take_event(code)
    }

    /// Drain all queued completion events in FIFO order — the event socket
    /// forwards these to connected clients (see [`CameraState::drain_events`]).
    pub fn drain_events(&mut self) -> Vec<u16> {
        self.state.drain_events()
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

    /// Assemble the `0xd212` live-status record stream from current state. The
    /// member set and framing come from the property's payload descriptor
    /// (manifest data, not a Fuji branch); each member's current value is
    /// emitted u32-padded, 0 when unset. See operators `D212_TIGHT_FORMAT`.
    fn status_d212(&self) -> Vec<u8> {
        let records: Vec<(u16, u32)> = self
            .manifest
            .property(0xd212)
            .and_then(|p| p.payload.as_ref())
            .map(|payload| {
                payload
                    .members
                    .iter()
                    .filter_map(|m| parse_hex_code(m))
                    .map(|code| {
                        let value = self
                            .state
                            .props
                            .get(&code)
                            .and_then(value_to_i64)
                            .unwrap_or(0) as u32;
                        (code, value)
                    })
                    .collect()
            })
            .unwrap_or_default();
        protocol_primitives::quirk::record_stream(&records)
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

/// Whether a reply carries an OK response code (effects arm only on success).
fn reply_is_ok(reply: &Reply) -> bool {
    match reply {
        Reply::Response(r)
        | Reply::Data { response: r, .. }
        | Reply::DataStream { response: r, .. } => r.code == resp::OK,
        Reply::Close => false,
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

#[cfg(test)]
mod tests {
    use super::*;
    use camera_config::CameraManifest;
    use camera_media_store::MediaStore;

    fn empty_store() -> MediaStore {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("ptpsim-engine-{nanos}"));
        std::fs::create_dir_all(&root).unwrap();
        MediaStore::open(&root).unwrap()
    }

    fn req(code: u16, tid: u32, params: Vec<u32>) -> OperationRequest {
        OperationRequest {
            data_phase_info: 1,
            code,
            transaction_id: tid,
            params,
        }
    }

    /// Read a GetDevicePropValue reply back to an i64 (u16 width here).
    fn poll(engine: &mut Engine, code: u16, tid: u32) -> i64 {
        let reply = engine.on_operation(
            &req(op::GET_DEVICE_PROP_VALUE, tid, vec![code as u32]),
            None,
        );
        let Reply::Data { data, .. } = reply else {
            panic!("expected data reply for {code:#06x}");
        };
        let mut r = ptp_core::Reader::new(&data);
        value_to_i64(&PropValue::decode(&mut r, ptp_core::codes::datatype_code::UINT16).unwrap())
            .unwrap()
    }

    /// §5.5 AF stub: 0x9026 arms a deferred 0xd209 → 1 transition visible on the
    /// 2nd poll. Models the camera-side effect as op-effects-in-data.
    #[test]
    fn op_effect_flips_prop_after_settle_polls() {
        let manifest = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
operations:
  "0x9026":
    name: LockS1Lock
    effects:
      - { setProp: "0xd209", value: 1, settleAfterPolls: 2 }
properties:
  "0xd209": { name: s1LockColor, type: u16, access: readOnly }
"#,
        )
        .unwrap();
        let mut e = Engine::new(manifest, empty_store());
        e.on_operation(&req(op::OPEN_SESSION, 1, vec![1]), None);
        // Trigger the AF op; the effect arms a deferred transition.
        assert!(reply_is_ok(&e.on_operation(&req(0x9026, 2, vec![0]), None)));
        // settleAfterPolls = 2: still 0 on poll 1, flips to 1 on poll 2.
        assert_eq!(poll(&mut e, 0xd209, 3), 0, "not settled yet (poll 1)");
        assert_eq!(poll(&mut e, 0xd209, 4), 1, "settled (poll 2)");
        assert_eq!(poll(&mut e, 0xd209, 5), 1, "stays settled");
    }

    /// settleAfterPolls = 0 writes immediately (no deferral).
    #[test]
    fn immediate_op_effect_writes_at_once() {
        let manifest = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
operations:
  "0x9026":
    name: LockS1Lock
    effects:
      - { setProp: "0xd209", value: 2 }
properties:
  "0xd209": { name: s1LockColor, type: u16, access: readOnly }
"#,
        )
        .unwrap();
        let mut e = Engine::new(manifest, empty_store());
        e.on_operation(&req(op::OPEN_SESSION, 1, vec![1]), None);
        e.on_operation(&req(0x9026, 2, vec![0]), None);
        assert_eq!(
            poll(&mut e, 0xd209, 3),
            2,
            "immediate effect visible at once"
        );
    }

    /// #54: an op's `emits` codes queue a completion event on an OK response, and
    /// `take_event` drains them by code (order-tolerant).
    #[test]
    fn op_emits_queue_event_on_ok() {
        let manifest = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
operations:
  "0x9026":
    name: LockS1Lock
    emits: ["0xc005"]
"#,
        )
        .unwrap();
        let mut e = Engine::new(manifest, empty_store());
        e.on_operation(&req(op::OPEN_SESSION, 1, vec![1]), None);
        assert!(reply_is_ok(&e.on_operation(&req(0x9026, 2, vec![0]), None)));
        assert!(e.take_event(0xc005), "0xc005 queued after the AF op");
        assert!(!e.take_event(0xc005), "drained — not queued twice");
    }

    /// #54: a non-OK response does NOT queue the event (mirrors effects gating).
    #[test]
    fn op_emits_skip_on_error() {
        let manifest = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
operations:
  "0x9026":
    name: LockS1Lock
    emits: ["0xc005"]
"#,
        )
        .unwrap();
        let mut e = Engine::new(manifest, empty_store());
        // No OpenSession → the op is rejected (not OK), so nothing is emitted.
        let reply = e.on_operation(&req(0x9026, 1, vec![0]), None);
        assert!(!reply_is_ok(&reply), "op rejected without an open session");
        assert!(!e.take_event(0xc005), "no event queued on a non-OK reply");
        // drain_events also sees an empty queue.
        assert!(e.drain_events().is_empty());
    }
}
