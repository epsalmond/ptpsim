//! The generic responder engine. It dispatches PTP operations against the
//! manifest + media store with **no** manufacturer-specific branches: which ops
//! exist, which properties have which forms, and which workflow they belong to
//! are all manifest data. The handlers here are generic PTP semantics.

use std::collections::BTreeSet;

use camera_config::model::{Action, ScalarEncoding, Step, StepParam};
use camera_config::{parse_hex_code, ActionVerb, CameraManifest};
use camera_media_store::{ByteSource, MediaStore, ObjectQuery, SIZE_CEILING};
use ptp_core::codes::{op, resp};
use ptp_core::dataset::PropValue;
use ptp_core::{DeviceInfo, OperationRequest, OperationResponse, Reader, Writer};

use crate::fault::{Fault, FaultSet};
use crate::state::{
    build_prop_desc, datatype_of, CameraState, Phase, DF01_IMAGE_IMPORT, DF01_LIVE_VIEW, PROP_DF01,
};
use crate::state_overlay::{AppliedStateOverlay, StateOverlay};

const STORAGE_ID: u32 = 0x0001_0001;

#[derive(Debug, Clone, PartialEq, Eq)]
struct GateSequence {
    name: String,
    steps: Vec<GateMatcher>,
}

#[derive(Debug, Clone)]
struct TransferQueue {
    handles: Vec<u32>,
    available: BTreeSet<u32>,
    next_index: usize,
    enqueue_per_shutter: u32,
    shutter_sequence: Option<Vec<GateMatcher>>,
    shutter_progress: usize,
}

#[derive(Debug, Clone)]
struct CameraInitiatedQueue {
    handles: Vec<u32>,
    head: usize,
    generation: u64,
    delivered: Vec<(u64, u64)>,
}

impl CameraInitiatedQueue {
    fn new(handles: Vec<u32>) -> Self {
        Self {
            handles,
            head: 0,
            generation: 0,
            delivered: Vec::new(),
        }
    }

    fn head(&self) -> Option<u32> {
        self.handles.get(self.head).copied()
    }

    fn remaining(&self) -> usize {
        self.handles.len().saturating_sub(self.head)
    }

    fn acknowledge(&mut self, offset: u64, len: u64, object_size: u64) -> bool {
        let end = offset.saturating_add(len).min(object_size);
        if end > offset {
            self.delivered.push((offset, end));
            self.delivered.sort_unstable();
            let mut merged: Vec<(u64, u64)> = Vec::with_capacity(self.delivered.len());
            for (start, end) in self.delivered.drain(..) {
                if let Some((_, previous_end)) = merged.last_mut() {
                    if start <= *previous_end {
                        *previous_end = (*previous_end).max(end);
                        continue;
                    }
                }
                merged.push((start, end));
            }
            self.delivered = merged;
        }
        let complete = object_size == 0
            || self
                .delivered
                .first()
                .is_some_and(|(start, end)| *start == 0 && *end >= object_size);
        if complete {
            self.head += 1;
            self.generation = self.generation.wrapping_add(1);
            self.delivered.clear();
        }
        complete
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamCompletion {
    generation: u64,
    handle: u32,
    offset: u64,
    len: u64,
    object_size: u64,
}

enum CameraQueueTarget {
    None,
    Head { handle: u32 },
    Invalid,
}

impl TransferQueue {
    fn startup_seeded(handles: Vec<u32>) -> Self {
        let available = handles.iter().copied().collect();
        TransferQueue {
            next_index: handles.len(),
            handles,
            available,
            enqueue_per_shutter: 0,
            shutter_sequence: None,
            shutter_progress: 0,
        }
    }

    fn shutter_seeded(
        handles: Vec<u32>,
        enqueue_per_shutter: u32,
        shutter_sequence: Vec<GateMatcher>,
    ) -> Self {
        TransferQueue {
            handles,
            available: BTreeSet::new(),
            next_index: 0,
            enqueue_per_shutter,
            shutter_sequence: Some(shutter_sequence),
            shutter_progress: 0,
        }
    }

    fn handles(&self) -> Vec<u32> {
        self.handles
            .iter()
            .copied()
            .filter(|handle| self.available.contains(handle))
            .collect()
    }

    fn contains(&self, handle: u32) -> bool {
        self.available.contains(&handle)
    }

    fn drain(&mut self, handle: u32) -> bool {
        self.available.remove(&handle)
    }

    fn enqueue_next(&mut self) {
        for _ in 0..self.enqueue_per_shutter {
            let Some(handle) = self.handles.get(self.next_index).copied() else {
                break;
            };
            self.available.insert(handle);
            self.next_index += 1;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GateMatcher {
    SetProp { prop: u16, value: Option<i64> },
    GetProp { prop: u16 },
    SendOp { op: u16, params: Vec<u32> },
}

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
        completion: Option<StreamCompletion>,
    },
    /// Write nothing and keep the command socket open, matching a camera-side
    /// no-response / timeout. Distinct from `Close`, which drops the socket.
    NoResponse,
    Close,
}

pub struct Engine {
    manifest: CameraManifest,
    gate_sequences: Vec<GateSequence>,
    store: MediaStore,
    state: CameraState,
    connection: String,
    transfer_queue: Option<TransferQueue>,
    camera_initiated_queue: Option<CameraInitiatedQueue>,
    faults: FaultSet,
    /// Cross-transport arming link (#102): the BLE `IMAGE_TRANSFER_SETTING` write
    /// arms the session that function-launch brings up. Default armed (standalone).
    link: crate::link::SharedLink,
}

impl Engine {
    pub const DEFAULT_CONNECTION: &'static str = "app";

    pub fn new(manifest: CameraManifest, store: MediaStore) -> Self {
        let state = CameraState::from_manifest(&manifest);
        let gate_sequences = compile_gate_sequences(&manifest);
        let mut engine = Engine {
            manifest,
            gate_sequences,
            store,
            state,
            connection: Self::DEFAULT_CONNECTION.to_string(),
            transfer_queue: None,
            camera_initiated_queue: None,
            faults: FaultSet::default(),
            link: crate::link::SharedLink::default(),
        };
        let handles = engine.camera_initiated_media_handles();
        if engine.manifest.camera_initiated_transfer.is_some() {
            engine.camera_initiated_queue = Some(CameraInitiatedQueue::new(handles));
        }
        engine.sync_camera_initiated_counts();
        engine
    }

    /// Bind the connection context for manifest-scoped behavior such as
    /// connection/mode-specific value profiles. The default standalone engine
    /// context is the app PTP/IP command channel.
    pub fn bind_connection(&mut self, connection: &str) {
        self.connection.clear();
        self.connection.push_str(connection);
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

    pub fn apply_state_overlay(
        &mut self,
        overlay: &StateOverlay,
    ) -> Result<AppliedStateOverlay, String> {
        crate::state_overlay::apply_overlay(&self.manifest, &mut self.state, overlay)
    }

    /// Enable standard PTP object-queue behavior for a connection whose manifest
    /// enumerates with `0x1007`. `shutter_enqueue_count == 0` seeds transferable
    /// non-movie media at startup; nonzero starts empty and enqueues after the
    /// manifest shutter action's literal wire sequence completes.
    pub fn configure_standard_object_queue(
        &mut self,
        connection_id: &str,
        shutter_enqueue_count: u32,
    ) -> Result<(), String> {
        let uses_standard_enumeration = {
            let Some(connection) = self.manifest.connections.get(connection_id) else {
                return Err(format!("connection '{connection_id}' is not present"));
            };
            connection
                .actions
                .get(&ActionVerb::EnumerateObjects)
                .is_some_and(|action| action_sends_op(action, op::GET_OBJECT_HANDLES))
        };
        if !uses_standard_enumeration {
            if shutter_enqueue_count == 0 {
                return Ok(());
            }
            return Err(format!(
                "selected connection '{connection_id}' does not enumerate objects with 0x1007"
            ));
        }

        let handles = self.standard_object_queue_handles();
        self.transfer_queue = Some(if shutter_enqueue_count == 0 {
            TransferQueue::startup_seeded(handles)
        } else {
            let (max, steps) = {
                let connection = self
                    .manifest
                    .connections
                    .get(connection_id)
                    .expect("connection checked above");
                let shutter = connection
                    .actions
                    .get(&ActionVerb::Shutter)
                    .ok_or_else(|| {
                        format!("selected connection '{connection_id}' has no shutter action")
                    })?;
                let max = shutter
                    .triggers
                    .iter()
                    .filter_map(|effect| effect.objects_available)
                    .map(|images| images.max)
                    .max()
                    .ok_or_else(|| {
                        format!(
                            "selected connection '{connection_id}' shutter action has no objectsAvailable trigger"
                        )
                    })?;
                (max, shutter.steps.clone())
            };
            if shutter_enqueue_count > max {
                return Err(format!(
                    "--pcss-shutter-enqueue-count {shutter_enqueue_count} exceeds selected connection '{connection_id}' objectsAvailable max {max}"
                ));
            }
            let shutter_sequence = matcher_sequence_for_steps(&steps).ok_or_else(|| {
                format!(
                    "selected connection '{connection_id}' shutter action cannot be matched as literal setProp/sendOp steps"
                )
            })?;
            TransferQueue::shutter_seeded(handles, shutter_enqueue_count, shutter_sequence)
        });
        Ok(())
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
        Self::data_stream_with_params(tid, source, vec![])
    }

    fn data_stream_with_params(tid: u32, source: ByteSource, params: Vec<u32>) -> Reply {
        Reply::DataStream {
            source,
            response: OperationResponse {
                code: resp::OK,
                transaction_id: tid,
                params,
            },
            completion: None,
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

        if let Some(reply) = self.operation_gate_reply(req.code) {
            return reply;
        }

        let reply = match req.code {
            op::OPEN_SESSION => {
                self.state.reset_gates();
                self.state.active_mode = None;
                self.state.session_open = true;
                self.state.phase = Phase::SessionOpen;
                Self::ok(tid)
            }
            op::CLOSE_SESSION => {
                self.state.reset_gates();
                self.state.active_mode = None;
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
                let handles = self.enumerated_object_handles();
                let mut w = Writer::new();
                w.ptp_array(&handles, |w, v| w.u32(*v));
                Self::data(tid, w.into_vec())
            }
            op::GET_OBJECT_INFO => {
                let handle = match self.camera_initiated_metadata_target(req.code, p(0)) {
                    CameraQueueTarget::Head { handle } => handle,
                    CameraQueueTarget::Invalid => {
                        return Self::err(tid, resp::INVALID_OBJECT_HANDLE);
                    }
                    CameraQueueTarget::None if self.object_handle_available(p(0)) => p(0),
                    CameraQueueTarget::None => {
                        return Self::err(tid, resp::INVALID_OBJECT_HANDLE);
                    }
                };
                match self.store.object_info(handle) {
                    Ok(oi) => {
                        let mut w = Writer::new();
                        if oi.encode(&mut w).is_err() {
                            return Self::err(tid, resp::GENERAL_ERROR);
                        }
                        Self::data(tid, w.into_vec())
                    }
                    Err(_) => Self::err(tid, resp::INVALID_OBJECT_HANDLE),
                }
            }
            op::GET_THUMB => {
                if !self.object_handle_available(p(0)) {
                    Self::err(tid, resp::INVALID_OBJECT_HANDLE)
                } else {
                    match self.store.thumbnail(p(0)) {
                        Ok(source) => Self::data_stream(tid, source),
                        Err(_) => Self::err(tid, resp::INVALID_OBJECT_HANDLE),
                    }
                }
            }
            op::GET_PARTIAL_OBJECT => {
                let offset = (p(1) as u64) | ((p(3) as u64) << 32);
                let (handle, completion_context) =
                    match self.camera_initiated_data_target(req.code, p(0)) {
                        CameraQueueTarget::Head { handle } => (handle, true),
                        CameraQueueTarget::Invalid => {
                            return Self::err(tid, resp::INVALID_OBJECT_HANDLE);
                        }
                        CameraQueueTarget::None => (p(0), false),
                    };
                match self.store.read_range(handle, offset, p(2)) {
                    Ok(source) => {
                        let returned = source.len().min(u32::MAX as u64) as u32;
                        let completion = completion_context.then_some(()).and_then(|()| {
                            let queue = self.camera_initiated_queue.as_ref()?;
                            Some(StreamCompletion {
                                generation: queue.generation,
                                handle,
                                offset,
                                len: source.len(),
                                object_size: self.store.object_size(handle).ok()?,
                            })
                        });
                        Reply::DataStream {
                            source,
                            response: OperationResponse {
                                code: resp::OK,
                                transaction_id: tid,
                                params: vec![returned],
                            },
                            completion,
                        }
                    }
                    Err(_) => Self::err(tid, resp::INVALID_OBJECT_HANDLE),
                }
            }
            op::GET_OBJECT => {
                if !self.object_handle_available(p(0)) {
                    return Self::err(tid, resp::INVALID_OBJECT_HANDLE);
                }
                // The PTP `ObjectInfo` size field is 32-bit (`SIZE_CEILING`);
                // this whole-object op is 32-bit-sized, while extension partial
                // reads can use a separate true-size path. Clamp here so the
                // request never allocates a multi-GB buffer.
                let size = self.store.object_size(p(0)).unwrap_or(0);
                let len = size.min(SIZE_CEILING) as u32;
                match self.store.read_range(p(0), 0, len) {
                    Ok(source) => Self::data_stream(tid, source),
                    Err(_) => Self::err(tid, resp::INVALID_OBJECT_HANDLE),
                }
            }
            op::DELETE_OBJECT => {
                if let Some(queue) = &mut self.transfer_queue {
                    if queue.drain(p(0)) {
                        Self::ok(tid)
                    } else {
                        Self::err(tid, resp::INVALID_OBJECT_HANDLE)
                    }
                } else if self.store.object_info(p(0)).is_ok() {
                    Self::ok(tid)
                } else {
                    Self::err(tid, resp::INVALID_OBJECT_HANDLE)
                }
            }
            op::GET_DEVICE_PROP_DESC => {
                let code = p(0) as u16;
                if let Some(reply) = self.property_gate_reply(code) {
                    return reply;
                }
                match build_prop_desc(&self.manifest, &self.state, code) {
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
                if self.transfer_queue.is_some() {
                    if code == 0xd621 {
                        let mut w = Writer::new();
                        w.ptp_array(&self.enumerated_object_handles(), |w, v| w.u32(*v));
                        return Self::data(tid, w.into_vec());
                    }
                    if code == 0xd620 {
                        let mut w = Writer::new();
                        w.u32(self.enumerated_object_handles().len() as u32);
                        return Self::data(tid, w.into_vec());
                    }
                }
                if let Some(reply) = self.property_gate_reply(code) {
                    return reply;
                }
                // A deferred op-effect transition settles on its scheduled poll.
                self.state.resolve_pending(code);
                // Reading a composite observes its members, so their pending
                // transitions tick too — consumers that reach a member through
                // its payload container (client application polls 0xd209 via 0xd212) must
                // see the same settle behavior as a direct member read (#185).
                let member_codes: Vec<u16> = self
                    .manifest
                    .property(code)
                    .and_then(|prop| prop.payload.as_ref())
                    .map(|payload| {
                        payload
                            .members
                            .iter()
                            .filter_map(|m| parse_hex_code(m))
                            .collect()
                    })
                    .unwrap_or_default();
                for member in member_codes {
                    self.state.resolve_pending(member);
                }
                // Composite properties are computed record streams assembled
                // from current member state, never opaque stored byte arrays.
                if self
                    .manifest
                    .property(code)
                    .and_then(|prop| prop.payload.as_ref())
                    .is_some()
                {
                    match self.record_stream_property(code) {
                        Ok(bytes) => Self::data(tid, bytes),
                        // A manifest/codec width disagreement must be visible on
                        // the wire, not served as misframed bytes (#161).
                        Err(_) => Self::err(tid, resp::GENERAL_ERROR),
                    }
                } else if code == 0xd621 {
                    // The Fuji object-handle list property (#46): the manifest's
                    // enumerate/import actions read it as a u32 array. Serve the
                    // real handles (same encoding as GetObjectHandles 0x1007) so
                    // the property-driven enumeration is believable, not a default.
                    let mut w = Writer::new();
                    w.ptp_array(&self.store_file_handles(), |w, v| w.u32(*v));
                    Self::data(tid, w.into_vec())
                } else if code == 0xd620 {
                    // The object count that sizes the 0xd621 list (declared u32).
                    let mut w = Writer::new();
                    w.u32(self.store_file_handles().len() as u32);
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
            self.advance_sequence_gates(req, data_in);
            self.apply_op_effects(req.code, &req.params);
            self.apply_op_emits(req.code);
            self.advance_transfer_queue(req, data_in);
        }
        reply
    }

    /// Arm the manifest-declared [`OpEffect`]s of operation `code` against state.
    /// No-op for ops without effects (the common case).
    fn operation_gate_reply(&self, code: u16) -> Option<Reply> {
        let req = self.manifest.operation(code)?.requires_gate.as_ref()?;
        (!self.state.gate_satisfied(&req.name)).then_some(Reply::NoResponse)
    }

    fn property_gate_reply(&self, code: u16) -> Option<Reply> {
        let req = self.manifest.property(code)?.requires_gate.as_ref()?;
        (!self.state.gate_satisfied(&req.name)).then_some(Reply::NoResponse)
    }

    fn advance_sequence_gates(&mut self, req: &OperationRequest, data_in: Option<&[u8]>) {
        for sequence_index in 0..self.gate_sequences.len() {
            let sequence = self.gate_sequences[sequence_index].clone();
            if self.state.gate_satisfied(&sequence.name) {
                continue;
            }
            let progress = self.state.gate_progress(&sequence.name, sequence_index);
            if progress > 0 {
                if sequence
                    .steps
                    .get(progress)
                    .is_some_and(|matcher| self.gate_match(matcher, req, data_in))
                {
                    self.advance_gate_progress(&sequence, sequence_index, progress + 1);
                    continue;
                }
                self.state
                    .set_gate_progress(&sequence.name, sequence_index, 0);
            }

            if sequence
                .steps
                .first()
                .is_some_and(|matcher| self.gate_match(matcher, req, data_in))
            {
                self.state.clear_gate(&sequence.name);
                self.advance_gate_progress(&sequence, sequence_index, 1);
            }
        }
    }

    fn advance_gate_progress(
        &mut self,
        sequence: &GateSequence,
        sequence_index: usize,
        progress: usize,
    ) {
        if progress >= sequence.steps.len() {
            self.state.satisfy_gate(&sequence.name);
            self.state
                .set_gate_progress(&sequence.name, sequence_index, 0);
        } else {
            self.state
                .set_gate_progress(&sequence.name, sequence_index, progress);
        }
    }

    fn gate_match(
        &self,
        matcher: &GateMatcher,
        req: &OperationRequest,
        data_in: Option<&[u8]>,
    ) -> bool {
        gate_match_for_manifest(&self.manifest, matcher, req, data_in)
    }

    fn advance_transfer_queue(&mut self, req: &OperationRequest, data_in: Option<&[u8]>) {
        let Some(queue) = &mut self.transfer_queue else {
            return;
        };
        let Some(sequence) = &queue.shutter_sequence else {
            return;
        };
        let progress_match = sequence
            .get(queue.shutter_progress)
            .is_some_and(|matcher| gate_match_for_manifest(&self.manifest, matcher, req, data_in));
        let first_match = sequence
            .first()
            .is_some_and(|matcher| gate_match_for_manifest(&self.manifest, matcher, req, data_in));
        if progress_match {
            queue.shutter_progress += 1;
        } else {
            queue.shutter_progress = usize::from(first_match);
        }
        if queue.shutter_progress >= sequence.len() {
            queue.shutter_progress = 0;
            queue.enqueue_next();
        }
    }

    fn camera_initiated_metadata_target(&self, operation: u16, index: u32) -> CameraQueueTarget {
        let Some(transfer) = self.manifest.camera_initiated_transfer.as_ref() else {
            return CameraQueueTarget::None;
        };
        if self.state.phase == Phase::QueuedReceive {
            if parse_hex_code(&transfer.receive.metadata.operation) != Some(operation) {
                return CameraQueueTarget::None;
            }
            if transfer.receive.head_index != index {
                return CameraQueueTarget::Invalid;
            }
            return self.camera_queue_head();
        }

        if self.state.phase != Phase::SessionOpen {
            return CameraQueueTarget::None;
        }
        if transfer.handoff.connection == self.connection
            && transfer.receive.metadata.before_mode_entry
            && parse_hex_code(&transfer.receive.metadata.operation) == Some(operation)
            && transfer.receive.head_index == index
        {
            self.camera_queue_head()
        } else {
            CameraQueueTarget::None
        }
    }

    fn camera_initiated_data_target(&self, operation: u16, index: u32) -> CameraQueueTarget {
        if self.state.phase != Phase::QueuedReceive {
            return CameraQueueTarget::None;
        }
        let Some(transfer) = self.manifest.camera_initiated_transfer.as_ref() else {
            return CameraQueueTarget::Invalid;
        };
        if parse_hex_code(&transfer.receive.data.operation) != Some(operation) {
            return CameraQueueTarget::None;
        }
        if transfer.receive.head_index != index {
            return CameraQueueTarget::Invalid;
        }
        self.camera_queue_head()
    }

    fn camera_queue_head(&self) -> CameraQueueTarget {
        self.camera_initiated_queue
            .as_ref()
            .and_then(CameraInitiatedQueue::head)
            .map_or(CameraQueueTarget::Invalid, |handle| {
                CameraQueueTarget::Head { handle }
            })
    }

    /// Acknowledge a streamed range after the transport has written the entire
    /// data phase and final OK response. Returns true only when the queue head
    /// advanced; stale or duplicate tokens are ignored.
    pub fn complete_stream(&mut self, completion: StreamCompletion) -> bool {
        let Some(queue) = self.camera_initiated_queue.as_mut() else {
            return false;
        };
        if queue.generation != completion.generation || queue.head() != Some(completion.handle) {
            return false;
        }
        let advanced = queue.acknowledge(completion.offset, completion.len, completion.object_size);
        if advanced {
            self.sync_camera_initiated_counts();
        }
        advanced
    }

    fn sync_camera_initiated_counts(&mut self) {
        if let Some((code, count)) =
            self.manifest
                .camera_initiated_transfer
                .as_ref()
                .and_then(|transfer| {
                    let code = parse_hex_code(&transfer.receive.count.member)?;
                    let count = u32::try_from(self.camera_initiated_queue.as_ref()?.remaining())
                        .unwrap_or(u32::MAX);
                    Some((code, count))
                })
        {
            let datatype = datatype_of(
                self.manifest
                    .property(code)
                    .and_then(|property| property.ptype.as_deref()),
            );
            self.state
                .props
                .insert(code, crate::state::typed(datatype, count as i64));
        }
    }

    fn camera_initiated_media_handles(&self) -> Vec<u32> {
        use ptp_core::codes::format::ASSOCIATION;
        self.store
            .handles(ObjectQuery::default())
            .into_iter()
            .filter(|handle| {
                let Ok(info) = self.store.object_info(*handle) else {
                    return false;
                };
                if info.object_format == ASSOCIATION {
                    return false;
                }
                self.manifest
                    .media
                    .as_ref()
                    .and_then(|media| {
                        media.formats.iter().find(|(code, _)| {
                            parse_hex_code(code.as_str()) == Some(info.object_format)
                        })
                    })
                    .is_some_and(|(_, format)| format.is_photos_compatible && !format.is_movie)
            })
            .collect()
    }

    fn refresh_detected_mode(&mut self) {
        let observed: camera_config::PropView = self
            .state
            .props
            .iter()
            .filter_map(|(code, value)| value_to_i64(value).map(|value| (*code, value)))
            .collect();
        let detected = self.manifest.detect_mode(&observed).map(str::to_string);
        self.state.active_mode = detected.clone();
        let transfer_active = detected.as_deref().is_some_and(|mode| {
            self.manifest
                .camera_initiated_transfer
                .as_ref()
                .is_some_and(|transfer| {
                    transfer.handoff.connection == self.connection && transfer.receive.mode == mode
                })
        });
        if transfer_active {
            self.state.phase = Phase::QueuedReceive;
        } else {
            if self.state.phase == Phase::QueuedReceive {
                self.state.phase = Phase::SessionOpen;
            }
        }
    }

    fn object_handle_available(&self, handle: u32) -> bool {
        self.transfer_queue
            .as_ref()
            .map(|queue| queue.contains(handle))
            .unwrap_or(true)
    }

    fn enumerated_object_handles(&self) -> Vec<u32> {
        self.transfer_queue
            .as_ref()
            .map(TransferQueue::handles)
            .unwrap_or_else(|| self.store_file_handles())
    }

    fn apply_op_effects(&mut self, code: u16, params: &[u32]) {
        let Some(opdef) = self.manifest.operation(code) else {
            return;
        };
        if opdef.effects.is_empty() {
            return;
        }
        // Snapshot (target code, value, settle) under the immutable manifest
        // borrow, then mutate state. A `fromParam` effect derives its value from
        // this op's request params (§5.5: 0x9026's packed AF-area -> 0xD17C); a
        // missing param index drops just that effect.
        let armed: Vec<(u16, i64, u32)> = opdef
            .effects
            .iter()
            .filter_map(|e| {
                let target = parse_hex_code(&e.set_prop)?;
                let value = match &e.from_param {
                    Some(src) => {
                        let raw = *params.get(src.index)? as u64;
                        ((raw >> src.shift) & src.mask.unwrap_or(u64::MAX)) as i64
                    }
                    None => e.value,
                };
                Some((target, value, e.settle_after_polls))
            })
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
            Some("object.size") => self.object_size_op(tid, opdef, params),
            _ => Self::ok(tid),
        }
    }

    fn object_size_op(
        &self,
        tid: u32,
        opdef: &camera_config::model::Operation,
        params: &[u32],
    ) -> Reply {
        let Some(handler) = &opdef.object_size else {
            return Self::err(tid, resp::GENERAL_ERROR);
        };
        if handler
            .required_params
            .iter()
            .any(|p| params.get(p.index).copied() != Some(p.equals))
        {
            return Self::err(tid, resp::INVALID_PARAMETER);
        }
        let Some(handle) = params.get(handler.handle_param).copied() else {
            return Self::err(tid, resp::INVALID_PARAMETER);
        };
        let Ok(size) = self.store.object_size(handle) else {
            return Self::err(tid, resp::INVALID_OBJECT_HANDLE);
        };
        let data = match handler.encoding {
            ScalarEncoding::U32Le => (size as u32).to_le_bytes().to_vec(),
            ScalarEncoding::U64Le => size.to_le_bytes().to_vec(),
        };
        Self::data(tid, data)
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
                if n as u32 != DF01_IMAGE_IMPORT {
                    self.state.reset_gates();
                }
            }
        }
        let value = self.normalized_property_write(code, prop, datatype, value);
        self.state.props.insert(code, value);
        self.refresh_detected_mode();
        Self::ok(tid)
    }

    fn normalized_property_write(
        &self,
        code: u16,
        prop: &camera_config::model::Property,
        datatype: u16,
        value: PropValue,
    ) -> PropValue {
        let Some(raw) = value_to_i64(&value) else {
            return value;
        };
        let Some(profile) = self.manifest.value_profile_for(
            code,
            &self.connection,
            self.state.manifest_mode_path(),
        ) else {
            return value;
        };
        let Some(row) = prop.profile_row_for_write(profile, raw) else {
            return value;
        };
        let store_raw = row.write_store_raw.unwrap_or(row.raw);
        crate::state::typed(datatype, store_raw)
    }

    fn store_file_handles(&self) -> Vec<u32> {
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

    fn standard_object_queue_handles(&self) -> Vec<u32> {
        self.store_file_handles()
            .into_iter()
            .filter(|h| {
                self.store
                    .object_info(*h)
                    .map(|oi| !self.object_format_is_movie(oi.object_format))
                    .unwrap_or(false)
            })
            .collect()
    }

    fn object_format_is_movie(&self, format_code: u16) -> bool {
        self.manifest
            .media
            .as_ref()
            .and_then(|media| {
                media
                    .formats
                    .iter()
                    .find(|(code, _)| parse_hex_code(code.as_str()) == Some(format_code))
                    .map(|(_, format)| format.is_movie)
            })
            .unwrap_or(false)
    }

    /// Assemble a manifest-declared record-stream property from current member
    /// state. Field widths and members are data; no property code is special.
    fn record_stream_property(
        &self,
        property: u16,
    ) -> Result<Vec<u8>, protocol_primitives::quirk::RecordStreamError> {
        use protocol_primitives::quirk::{record_stream, RecordStreamLayout};
        let Some(payload) = self
            .manifest
            .property(property)
            .and_then(|p| p.payload.as_ref())
        else {
            return record_stream(&[], &RecordStreamLayout::D212);
        };
        let (count_w, code_w, value_w) = payload.record_widths();
        let layout = RecordStreamLayout::new(count_w, code_w, value_w)?;
        let records: Vec<(u16, u32)> = payload
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
            .collect();
        record_stream(&records, &layout)
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
            serial_number: self
                .manifest
                .camera
                .identities
                .get("serialNumber")
                .cloned()
                .unwrap_or_default(),
            operations_supported: ops,
            device_properties_supported: props,
            ..Default::default()
        };
        let mut w = Writer::new();
        let _ = di.encode(&mut w);
        w.into_vec()
    }
}

fn compile_gate_sequences(manifest: &CameraManifest) -> Vec<GateSequence> {
    let mut out = Vec::new();
    for connection in manifest.connections.values() {
        for entry in &connection.entries {
            collect_gate_sequences(&entry.steps, &mut out);
        }
        for action in connection.actions.values() {
            collect_gate_sequences(&action.steps, &mut out);
        }
    }
    out
}

fn collect_gate_sequences(steps: &[Step], out: &mut Vec<GateSequence>) {
    let mut active: std::collections::BTreeMap<String, Option<Vec<GateMatcher>>> =
        std::collections::BTreeMap::new();
    for step in steps {
        let matcher = matcher_for_step(step);
        let starts = step.starts_gate.clone();
        for (gate, sequence) in active.iter_mut() {
            if Some(gate) == starts.as_ref() {
                continue;
            }
            match (sequence.as_mut(), matcher.clone()) {
                (Some(sequence), Some(matcher)) => sequence.push(matcher),
                (Some(_), None) => *sequence = None,
                (None, _) => {}
            }
        }
        if let Some(gate) = starts {
            active.insert(gate, matcher.clone().map(|matcher| vec![matcher]));
        }
        if let Some(gate) = &step.completes_gate {
            if let Some(Some(sequence)) = active.remove(gate) {
                if !sequence.is_empty() {
                    let compiled = GateSequence {
                        name: gate.clone(),
                        steps: sequence,
                    };
                    if !out.contains(&compiled) {
                        out.push(compiled);
                    }
                }
            }
        }
    }
}

fn matcher_for_step(step: &Step) -> Option<GateMatcher> {
    if !step.is_sequence_gate_matchable() {
        return None;
    }
    if let Some(prop) = &step.set_prop {
        return Some(GateMatcher::SetProp {
            prop: parse_hex_code(prop)?,
            value: step.value,
        });
    }
    if let Some(prop) = &step.get_prop {
        return Some(GateMatcher::GetProp {
            prop: parse_hex_code(prop)?,
        });
    }
    if let Some(op) = &step.send_op {
        return Some(GateMatcher::SendOp {
            op: parse_hex_code(op)?,
            params: literal_params(&step.params)?,
        });
    }
    None
}

fn matcher_sequence_for_steps(steps: &[Step]) -> Option<Vec<GateMatcher>> {
    let sequence: Option<Vec<_>> = steps.iter().map(matcher_for_step).collect();
    sequence.filter(|sequence| !sequence.is_empty())
}

fn action_sends_op(action: &Action, code: u16) -> bool {
    action
        .steps
        .iter()
        .any(|step| step.send_op.as_deref().and_then(parse_hex_code) == Some(code))
}

fn gate_match_for_manifest(
    manifest: &CameraManifest,
    matcher: &GateMatcher,
    req: &OperationRequest,
    data_in: Option<&[u8]>,
) -> bool {
    match matcher {
        GateMatcher::GetProp { prop } => {
            req.code == op::GET_DEVICE_PROP_VALUE && req.params.first() == Some(&(*prop as u32))
        }
        GateMatcher::SetProp { prop, value } => {
            if req.code != op::SET_DEVICE_PROP_VALUE || req.params.first() != Some(&(*prop as u32))
            {
                return false;
            }
            let Some(bytes) = data_in else {
                return false;
            };
            let Some(expected) = value else {
                return true;
            };
            let datatype = datatype_of(manifest.property(*prop).and_then(|p| p.ptype.as_deref()));
            let mut r = Reader::new(bytes);
            PropValue::decode(&mut r, datatype)
                .ok()
                .and_then(|v| value_to_i64(&v))
                == Some(*expected)
        }
        GateMatcher::SendOp { op, params } => req.code == *op && req.params == *params,
    }
}

/// Whether a reply carries an OK response code (effects arm only on success).
fn reply_is_ok(reply: &Reply) -> bool {
    match reply {
        Reply::Response(r)
        | Reply::Data { response: r, .. }
        | Reply::DataStream { response: r, .. } => r.code == resp::OK,
        Reply::NoResponse | Reply::Close => false,
    }
}

fn literal_params(params: &[StepParam]) -> Option<Vec<u32>> {
    params
        .iter()
        .map(|p| match p {
            StepParam::Literal(v) => Some(*v),
            StepParam::Runtime { .. } => None,
        })
        .collect()
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

    /// Read a GetDevicePropValue reply back to an i64, decoding at `dt` width.
    fn poll_dt(engine: &mut Engine, code: u16, tid: u32, dt: u16) -> i64 {
        let reply = engine.on_operation(
            &req(op::GET_DEVICE_PROP_VALUE, tid, vec![code as u32]),
            None,
        );
        let Reply::Data { data, .. } = reply else {
            panic!("expected data reply for {code:#06x}");
        };
        let mut r = ptp_core::Reader::new(&data);
        value_to_i64(&PropValue::decode(&mut r, dt).unwrap()).unwrap()
    }

    /// Read a GetDevicePropValue reply back to an i64 (u16 width).
    fn poll(engine: &mut Engine, code: u16, tid: u32) -> i64 {
        poll_dt(engine, code, tid, ptp_core::codes::datatype_code::UINT16)
    }

    /// Same, decoding a u32 property (e.g. 0xD17C S1LockAreaState).
    fn poll_u32(engine: &mut Engine, code: u16, tid: u32) -> i64 {
        poll_dt(engine, code, tid, ptp_core::codes::datatype_code::UINT32)
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

    /// #96: a `fromParam` effect copies the op's request param into the target
    /// prop — 0x9026's packed AF-area (params[0]) into 0xD17C S1LockAreaState,
    /// immediately (settleAfterPolls defaults 0). This is the real consumer that
    /// exercises the param-derived value the §5.5 stub previously left unmodeled.
    #[test]
    fn from_param_effect_copies_packed_request_param() {
        let manifest = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
operations:
  "0x9026":
    name: LockS1Lock
    effects:
      - { setProp: "0xd17c", fromParam: { index: 0 } }
properties:
  "0xd17c": { name: s1Lock, type: u32, access: readOnly }
"#,
        )
        .unwrap();
        let mut e = Engine::new(manifest, empty_store());
        e.on_operation(&req(op::OPEN_SESSION, 1, vec![1]), None);
        // Packed AF-area 0x04030504 (aspW·aspH·col·row) from the tap request.
        assert!(reply_is_ok(
            &e.on_operation(&req(0x9026, 2, vec![0x0403_0504]), None)
        ));
        assert_eq!(
            poll_u32(&mut e, 0xd17c, 3),
            0x0403_0504,
            "0xD17C mirrors the packed request param"
        );
    }

    /// #96: the fromParam bit-slice (shift then mask) pulls a packed sub-field.
    /// From 0x04030504, `>> 8 & 0xFF` selects the col byte (0x05). Grammar-level —
    /// 0x9026 itself uses the identity form, but the slice spec must work.
    #[test]
    fn from_param_effect_applies_shift_and_mask() {
        let manifest = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
operations:
  "0x9026":
    name: LockS1Lock
    effects:
      - { setProp: "0xd17c", fromParam: { index: 0, shift: 8, mask: 0xff } }
properties:
  "0xd17c": { name: s1Lock, type: u32, access: readOnly }
"#,
        )
        .unwrap();
        let mut e = Engine::new(manifest, empty_store());
        e.on_operation(&req(op::OPEN_SESSION, 1, vec![1]), None);
        e.on_operation(&req(0x9026, 2, vec![0x0403_0504]), None);
        assert_eq!(
            poll_u32(&mut e, 0xd17c, 3),
            0x05,
            "shift+mask extracts the col byte"
        );
    }

    /// #96: a fromParam effect whose index is out of range drops just that effect
    /// (no panic); a sibling fixed effect still applies.
    #[test]
    fn from_param_effect_skips_when_param_absent() {
        let manifest = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
operations:
  "0x9026":
    name: LockS1Lock
    effects:
      - { setProp: "0xd17c", fromParam: { index: 3 } }
      - { setProp: "0xd209", value: 1 }
properties:
  "0xd17c": { name: s1Lock, type: u32, access: readOnly }
  "0xd209": { name: s1LockColor, type: u16, access: readOnly }
"#,
        )
        .unwrap();
        let mut e = Engine::new(manifest, empty_store());
        e.on_operation(&req(op::OPEN_SESSION, 1, vec![1]), None);
        // No params → index 3 is absent: the 0xd17c effect is skipped, but the
        // sibling fixed 0xd209 effect still fires.
        e.on_operation(&req(0x9026, 2, vec![]), None);
        assert_eq!(
            poll(&mut e, 0xd209, 3),
            1,
            "sibling fixed effect unaffected"
        );
        assert_eq!(
            poll_u32(&mut e, 0xd17c, 4),
            0,
            "absent-param effect skipped (stays default 0)"
        );
    }
}
