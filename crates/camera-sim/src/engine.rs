//! The generic responder engine. It dispatches PTP operations against the
//! manifest + media store with **no** manufacturer-specific branches: which ops
//! exist, which properties have which forms, and which workflow they belong to
//! are all manifest data. The handlers here are generic PTP semantics.

use std::collections::{BTreeMap, BTreeSet};

use camera_config::model::{Action, ScalarEncoding, SetPropValue, Step, StepParam};
use camera_config::{
    parse_hex_code, ActionArgumentValue, ActionVerb, CameraInitiatedMetadataPhase, CameraManifest,
    PropertyTransitionTerminal, ResponderMutation,
};
use camera_media_store::{ByteSource, MediaStore, ObjectQuery, SIZE_CEILING};
use ptp_core::codes::{op, resp};
use ptp_core::dataset::PropValue;
use ptp_core::{DeviceInfo, OperationRequest, OperationResponse, Reader, Writer};
use serde::Serialize;

use crate::fault::{
    AppliedFault, FaultApplication, FaultMutation, FaultSet, FaultSpec, FaultStage, FaultView,
};
use crate::state::{build_prop_desc, datatype_of, typed_descriptor_value, CameraState, Phase};
use crate::state_overlay::{AppliedStateOverlay, StateOverlay};

const STORAGE_ID: u32 = 0x0001_0001;
// GFX100 II firmware 2.30 returns this vendor response when PCSS live-view
// arming is blocked by a pending object queue or an unterminated prior stream.
const LIVE_VIEW_ARMING_BLOCKED: u16 = 0xa002;

#[derive(Debug, Clone, PartialEq, Eq)]
struct GateSequence {
    name: String,
    steps: Vec<GateMatcher>,
}

#[derive(Debug, Clone)]
struct TransferQueue {
    connection: String,
    handles: Vec<u32>,
    available: BTreeSet<u32>,
    next_index: usize,
    enqueue_per_shutter: u32,
    shutter_sequence: Option<Vec<GateMatcher>>,
    shutter_progress: usize,
    shutter_busy_responses_remaining: u32,
    completed: usize,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct QueueStats {
    pub queued: usize,
    pub completed: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TransferQueueStats {
    pub standard: Option<QueueStats>,
    pub camera_initiated: Option<QueueStats>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedPropertyTransition {
    pub target: u16,
    pub initial: Option<i64>,
    pub terminal: i64,
    pub settle_after_polls: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreparedResponderMutation {
    EnqueueObjects { count: u32, affected: usize },
    PropertyTransition(PreparedPropertyTransition),
}

#[derive(Clone, Copy)]
enum CameraQueueTarget {
    None,
    Head { handle: u32 },
    Invalid,
}

impl TransferQueue {
    fn startup_seeded(connection: String, handles: Vec<u32>) -> Self {
        let available = handles.iter().copied().collect();
        TransferQueue {
            connection,
            next_index: handles.len(),
            handles,
            available,
            enqueue_per_shutter: 0,
            shutter_sequence: None,
            shutter_progress: 0,
            shutter_busy_responses_remaining: 0,
            completed: 0,
        }
    }

    fn shutter_seeded(
        connection: String,
        handles: Vec<u32>,
        enqueue_per_shutter: u32,
        shutter_sequence: Vec<GateMatcher>,
    ) -> Self {
        TransferQueue {
            connection,
            handles,
            available: BTreeSet::new(),
            next_index: 0,
            enqueue_per_shutter,
            shutter_sequence: Some(shutter_sequence),
            shutter_progress: 0,
            shutter_busy_responses_remaining: 0,
            completed: 0,
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
        let drained = self.available.remove(&handle);
        if drained {
            self.completed += 1;
        }
        drained
    }

    fn enqueue_count(&mut self, count: u32) -> usize {
        let before = self.available.len();
        for _ in 0..count {
            let Some(handle) = self.handles.get(self.next_index).copied() else {
                break;
            };
            self.available.insert(handle);
            self.next_index += 1;
        }
        self.available.len() - before
    }

    fn enqueue_next(&mut self) {
        self.enqueue_count(self.enqueue_per_shutter);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GateMatcher {
    SetProp { prop: u16, value: Option<i64> },
    GetProp { prop: u16 },
    SendOp { op: u16, params: Vec<u32> },
    SuccessfulOperation { op: u16 },
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
    live_view_stream_connection: Option<String>,
    camera_initiated_queue: Option<CameraInitiatedQueue>,
    camera_initiated_transfer_active: bool,
    camera_initiated_pre_mode_probe_armed: bool,
    faults: FaultSet,
    applied_fault: Option<AppliedFault>,
    /// Cross-transport arming link (#102): the BLE `IMAGE_TRANSFER_SETTING` write
    /// arms the session that function-launch brings up. Default armed (standalone).
    link: crate::link::SharedLink,
}

impl Engine {
    pub const DEFAULT_CONNECTION: &'static str = "app";

    pub fn new(manifest: CameraManifest, store: MediaStore) -> Self {
        let state = CameraState::from_manifest(&manifest);
        let mut gate_sequences = compile_gate_sequences(&manifest);
        gate_sequences.extend(compile_channel_sequences(&manifest));
        let mut engine = Engine {
            manifest,
            gate_sequences,
            store,
            state,
            connection: Self::DEFAULT_CONNECTION.to_string(),
            transfer_queue: None,
            live_view_stream_connection: None,
            camera_initiated_queue: None,
            camera_initiated_transfer_active: false,
            camera_initiated_pre_mode_probe_armed: false,
            faults: FaultSet::default(),
            applied_fault: None,
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

    /// The owning command transport ended without `CloseSession` (client
    /// killed, Wi-Fi dropped, the transport-close the manifest itself models):
    /// a real camera binds the session to its link, so the session state goes
    /// with the transport (#455 review). Camera state (property values, media)
    /// outlives the connection; without this, a shared engine wedges on
    /// `SessionAlreadyOpen` until restart.
    pub fn transport_lost(&mut self) {
        self.clear_session();
    }

    fn clear_session(&mut self) {
        self.state.reset_gates();
        self.state.active_mode = None;
        self.camera_initiated_pre_mode_probe_armed = false;
        self.state.session_open = false;
        self.state.phase = Phase::Closed;
    }

    /// Whether a manifest-authored auxiliary socket has crossed its causal
    /// availability boundary. Connections without an `openChannel` step retain
    /// the historical immediately-available behavior.
    pub fn channel_ready(&self, role: camera_config::SocketRole) -> bool {
        let gate = channel_gate_name(&self.connection, role);
        !self
            .gate_sequences
            .iter()
            .any(|sequence| sequence.name == gate)
            || self.state.gate_satisfied(&gate)
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

    /// Install an occurrence-scoped fault and return its server-assigned id.
    pub fn install_fault(&mut self, fault: FaultSpec) -> Result<u64, String> {
        self.faults.try_insert(fault)
    }

    pub fn remove_fault(&mut self, id: u64) -> bool {
        self.faults.remove(id)
    }

    pub fn clear_faults(&mut self) {
        self.faults.clear();
        self.applied_fault = None;
    }

    pub fn faults(&self) -> Vec<FaultView> {
        self.faults.list()
    }

    pub fn last_applied_fault(&self) -> Option<FaultApplication> {
        self.faults.last_applied()
    }

    pub fn take_applied_fault(&mut self) -> Option<AppliedFault> {
        self.applied_fault.take()
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
        let applied = crate::state_overlay::apply_overlay(
            &self.manifest,
            &mut self.state,
            &mut self.camera_initiated_transfer_active,
            overlay,
        )?;
        if !self.camera_initiated_transfer_active {
            self.camera_initiated_pre_mode_probe_armed = false;
        }
        Ok(applied)
    }

    pub fn camera_initiated_transfer_active(&self) -> bool {
        self.camera_initiated_transfer_active
    }

    pub fn transfer_queue_stats(&self) -> TransferQueueStats {
        TransferQueueStats {
            standard: self.transfer_queue.as_ref().map(|queue| QueueStats {
                queued: queue.available.len(),
                completed: queue.completed,
                total: queue.handles.len(),
            }),
            camera_initiated: self
                .camera_initiated_queue
                .as_ref()
                .map(|queue| QueueStats {
                    queued: queue.remaining(),
                    completed: queue.head,
                    total: queue.handles.len(),
                }),
        }
    }

    /// Resolve one closed responder mutation without changing camera state.
    pub fn prepare_responder_mutation(
        &self,
        mutation: &ResponderMutation,
        parameters: &BTreeMap<String, ActionArgumentValue>,
    ) -> Result<PreparedResponderMutation, String> {
        match mutation {
            ResponderMutation::EnqueueObjects { count_param } => {
                let Some(ActionArgumentValue::U64(count)) = parameters.get(count_param) else {
                    return Err(format!(
                        "resolved responder parameter '{count_param}' is not numeric"
                    ));
                };
                let count = u32::try_from(*count)
                    .map_err(|_| format!("resolved parameter '{count_param}' exceeds u32"))?;
                let affected = self.pending_standard_object_enqueue(count)?;
                Ok(PreparedResponderMutation::EnqueueObjects { count, affected })
            }
            ResponderMutation::PropertyTransition {
                target,
                initial,
                terminal,
                settle_after_polls,
            } => {
                let target = parse_hex_code(target)
                    .ok_or_else(|| format!("invalid property transition target '{target}'"))?;
                let property = self
                    .manifest
                    .property(target)
                    .ok_or_else(|| format!("unknown property transition target {target:#06x}"))?;
                if property.payload.is_some() {
                    return Err(format!(
                        "property transition target {target:#06x} is not a scalar"
                    ));
                }
                let terminal = match terminal {
                    PropertyTransitionTerminal::Fixed { value } => *value,
                    PropertyTransitionTerminal::Parameter { parameter } => {
                        let Some(ActionArgumentValue::U64(value)) = parameters.get(parameter)
                        else {
                            return Err(format!(
                                "resolved terminal parameter '{parameter}' is not numeric"
                            ));
                        };
                        i64::try_from(*value).map_err(|_| {
                            format!("resolved terminal parameter '{parameter}' exceeds i64")
                        })?
                    }
                };
                if initial
                    .is_some_and(|value| !scalar_fits_property(property.ptype.as_deref(), value))
                    || !scalar_fits_property(property.ptype.as_deref(), terminal)
                {
                    return Err(format!(
                        "resolved property transition value does not fit target {target:#06x}"
                    ));
                }
                Ok(PreparedResponderMutation::PropertyTransition(
                    PreparedPropertyTransition {
                        target,
                        initial: *initial,
                        terminal,
                        settle_after_polls: *settle_after_polls,
                    },
                ))
            }
        }
    }

    /// Apply a previously prepared mutation. Callers can record the resolved
    /// action between preparation and this state-changing step.
    pub fn apply_responder_mutation(
        &mut self,
        prepared: &PreparedResponderMutation,
    ) -> Result<(), String> {
        match prepared {
            PreparedResponderMutation::EnqueueObjects { count, affected } => {
                let applied = self.enqueue_standard_objects(*count)?;
                debug_assert_eq!(applied, *affected, "preflighted responder mutation drifted");
            }
            PreparedResponderMutation::PropertyTransition(transition) => {
                let ptype = self
                    .manifest
                    .property(transition.target)
                    .and_then(|property| property.ptype.as_deref());
                if let Some(initial) = transition.initial {
                    self.state.props.insert(
                        transition.target,
                        property_transition_value(ptype, initial)
                            .expect("prepared initial transition value must encode"),
                    );
                }
                self.state.arm_effect(
                    transition.target,
                    property_transition_value(ptype, transition.terminal)
                        .expect("prepared terminal transition value must encode"),
                    transition.settle_after_polls,
                );
            }
        }
        Ok(())
    }

    /// Apply the closed responder action primitive after catalog resolution.
    /// No validation happens here: callers must resolve the action first.
    pub fn enqueue_standard_objects(&mut self, count: u32) -> Result<usize, String> {
        let queue = self
            .transfer_queue
            .as_mut()
            .ok_or_else(|| "standard object queue is not configured".to_string())?;
        Ok(queue.enqueue_count(count))
    }

    /// Return the number of objects an enqueue would make available without
    /// mutating the queue. The control service uses this while holding the
    /// engine lock so it can durably record the resolved action before applying
    /// the closed responder mutation.
    pub fn pending_standard_object_enqueue(&self, count: u32) -> Result<usize, String> {
        let queue = self
            .transfer_queue
            .as_ref()
            .ok_or_else(|| "standard object queue is not configured".to_string())?;
        Ok(usize::try_from(count)
            .unwrap_or(usize::MAX)
            .min(queue.handles.len().saturating_sub(queue.next_index)))
    }

    /// Seed an already configured shutter-driven queue before a session opens,
    /// preserving its normal per-shutter enqueue behavior.
    pub fn preseed_standard_object_queue(
        &mut self,
        connection_id: &str,
        count: u32,
    ) -> Result<usize, String> {
        let queue = self
            .transfer_queue
            .as_mut()
            .ok_or_else(|| "standard object queue is not configured".to_string())?;
        if queue.connection != connection_id {
            return Err(format!(
                "standard object queue is configured for '{}', not '{connection_id}'",
                queue.connection
            ));
        }
        Ok(queue.enqueue_count(count))
    }

    /// Seed field-observed Device Busy replies for repeated InitiateCapture
    /// requests after the first shutter beat has started a capture. The queue
    /// must already be configured for the selected connection.
    pub fn preseed_shutter_busy_responses(
        &mut self,
        connection_id: &str,
        attempts: u32,
    ) -> Result<(), String> {
        let queue = self
            .transfer_queue
            .as_mut()
            .ok_or_else(|| "standard object queue is not configured".to_string())?;
        if queue.connection != connection_id {
            return Err(format!(
                "standard object queue is configured for '{}', not '{connection_id}'",
                queue.connection
            ));
        }
        if queue.shutter_sequence.is_none() {
            return Err(format!(
                "standard object queue for '{connection_id}' is not shutter-driven"
            ));
        }
        queue.shutter_busy_responses_remaining = attempts;
        Ok(())
    }

    /// Simulator policy for the field-observed arming wedge: an uncleanly
    /// exited PCSS session left 0xD1BC rejecting 0xA002 until a session whose
    /// arming began with TerminateOpenCapture succeeded (fw 2.30, 2026-07-18).
    /// The wire-capture audit bounds terminate-before-arm as defensive
    /// ordering, not capture-proven causality; a directed probe for the causal
    /// question is tracked upstream of this repo.
    pub fn preseed_stale_live_view_stream(&mut self, connection_id: &str) -> Result<(), String> {
        if !self.connection_models_live_view_stream(connection_id) {
            return Err(format!(
                "selected connection '{connection_id}' does not model a live-view open-capture stream"
            ));
        }
        self.live_view_stream_connection = Some(connection_id.to_string());
        Ok(())
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
            TransferQueue::startup_seeded(connection_id.to_string(), handles)
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
                let initiator = shutter.initiator().ok_or_else(|| {
                    format!(
                        "selected connection '{connection_id}' shutter action has no initiator binding"
                    )
                })?;
                (max, initiator.steps.clone())
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
            TransferQueue::shutter_seeded(
                connection_id.to_string(),
                handles,
                shutter_enqueue_count,
                shutter_sequence,
            )
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
        let applied = self.faults.apply(req.code, &req.params);
        let applied_mutation = self.faults.take_applied_mutation();
        let mut reply = match applied_mutation.as_ref() {
            Some(FaultMutation::FailResponse { response }) => {
                Self::err(req.transaction_id, *response)
            }
            Some(FaultMutation::Close {
                stage: FaultStage::Command,
            }) => Reply::Close,
            _ => self.dispatch_operation(req, data_in),
        };

        if let Some(mutation) = &applied_mutation {
            match mutation {
                FaultMutation::Suppress {
                    stage: crate::fault::DataOrResponse::Response,
                } if matches!(reply, Reply::Response(_)) => reply = Reply::NoResponse,
                FaultMutation::TruncateData { keep } => truncate_reply_data(&mut reply, *keep),
                FaultMutation::ReplaceData { bytes } => replace_reply_data(&mut reply, bytes),
                FaultMutation::PropertyReadback { value } => {
                    self.replace_property_readback(req, &mut reply, *value)
                }
                FaultMutation::FailResponse { .. }
                | FaultMutation::Close { .. }
                | FaultMutation::Delay { .. }
                | FaultMutation::Suppress { .. }
                | FaultMutation::ReplaceTransactionId { .. }
                | FaultMutation::DataFraming { .. } => {}
            }
        }
        self.applied_fault = applied;
        reply
    }

    fn replace_property_readback(&self, req: &OperationRequest, reply: &mut Reply, value: i64) {
        if req.code != op::GET_DEVICE_PROP_VALUE {
            return;
        }
        let Some(code) = req
            .params
            .first()
            .and_then(|code| u16::try_from(*code).ok())
        else {
            return;
        };
        let Some(property) = self.manifest.property(code) else {
            return;
        };
        let Some(value) = property_transition_value(property.ptype.as_deref(), value) else {
            return;
        };
        let Reply::Data { data, .. } = reply else {
            return;
        };
        let mut writer = Writer::new();
        if value.encode(&mut writer).is_ok() {
            *data = writer.into_vec();
        }
    }

    fn dispatch_operation(&mut self, req: &OperationRequest, data_in: Option<&[u8]>) -> Reply {
        let tid = req.transaction_id;
        let p = |i: usize| req.params.get(i).copied().unwrap_or(0);

        self.prepare_camera_initiated_pre_mode_probe(req);

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

        if let Some(reply) = self.shutter_busy_reply(req) {
            return reply;
        }

        if let Some(reply) = self.camera_initiated_operation_reply(req) {
            if reply_is_ok(&reply) {
                self.apply_successful_operation(req, data_in);
            }
            return reply;
        }

        // Catalog availability gates dispatch (#407): connection, mode, kind,
        // and `requires` resolve before any handler runs, so the sim refuses
        // out-of-context ops like the persona-gated real camera. Session
        // lifecycle and device identity are bootstrap ops a camera answers
        // regardless of catalog context.
        if !matches!(
            req.code,
            op::OPEN_SESSION | op::CLOSE_SESSION | op::GET_DEVICE_INFO
        ) {
            if let Some(response) = self.operation_availability_error(req.code) {
                return Self::err(tid, response);
            }
        }

        let reply = match req.code {
            op::OPEN_SESSION => {
                // A real camera refuses a second open instead of silently
                // resetting the session (#407). PTP only forbids session id 0;
                // non-1 ids are accepted absent wire evidence of refusal.
                if self.state.session_open {
                    return Self::err(tid, resp::SESSION_ALREADY_OPEN);
                }
                if p(0) == 0 {
                    return Self::err(tid, resp::INVALID_PARAMETER);
                }
                self.state.reset_gates();
                self.state.active_mode = None;
                self.camera_initiated_pre_mode_probe_armed = false;
                self.state.session_open = true;
                self.state.phase = Phase::SessionOpen;
                Self::ok(tid)
            }
            op::CLOSE_SESSION => {
                self.clear_session();
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
                if !self.object_handle_available(p(0)) {
                    return Self::err(tid, resp::INVALID_OBJECT_HANDLE);
                }
                match self.store.object_info(p(0)) {
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
                match self.store.read_range(p(0), offset, p(2)) {
                    Ok(source) => {
                        let returned = source.len().min(u32::MAX as u64) as u32;
                        Reply::DataStream {
                            source,
                            response: OperationResponse {
                                code: resp::OK,
                                transaction_id: tid,
                                params: vec![returned],
                            },
                            completion: None,
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
                    // Computed sources serve queue-scoped enumeration while a
                    // camera-initiated transfer queue owns visibility (#407).
                    if let Some(reply) = self.computed_property_reply(tid, code) {
                        return reply;
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
                            .filter_map(|member| parse_hex_code(member.code()))
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
                } else if let Some(reply) = self.computed_property_reply(tid, code) {
                    // Manifest-declared computed source (#407): the manifest
                    // names the engine quantity this property serves, so no
                    // property code is special in the engine.
                    reply
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
                                let v = crate::state::default_prop_value(datatype_of(
                                    prop.ptype.as_deref(),
                                ));
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
            op::TERMINATE_OPEN_CAPTURE
                if self.connection_models_live_view_stream(&self.connection) =>
            {
                if self.live_view_stream_connection.as_deref() == Some(self.connection.as_str()) {
                    self.live_view_stream_connection = None;
                }
                Self::ok(tid)
            }
            op::INITIATE_OPEN_CAPTURE => {
                if matches!(self.state.phase, Phase::LiveView | Phase::Streaming) {
                    self.state.phase = Phase::Streaming;
                    if self.connection_models_live_view_stream(&self.connection) {
                        self.live_view_stream_connection = Some(self.connection.clone());
                    }
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
            self.apply_successful_operation(req, data_in);
        }
        reply
    }

    fn apply_successful_operation(&mut self, req: &OperationRequest, data_in: Option<&[u8]>) {
        if self.is_camera_initiated_count_read(req) {
            self.camera_initiated_pre_mode_probe_armed = true;
        }
        self.advance_sequence_gates(req, data_in);
        self.apply_op_effects(req.code, &req.params);
        self.apply_op_emits(req.code);
        self.advance_transfer_queue(req, data_in);
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

    fn shutter_busy_reply(&mut self, req: &OperationRequest) -> Option<Reply> {
        if req.code != op::INITIATE_CAPTURE {
            return None;
        }
        let queue = self.transfer_queue.as_mut()?;
        if queue.connection != self.connection || queue.shutter_busy_responses_remaining == 0 {
            return None;
        }
        let sequence = queue.shutter_sequence.as_ref()?;
        let capture_in_flight = sequence
            .iter()
            .take(queue.shutter_progress)
            .any(|matcher| matches!(matcher, GateMatcher::SendOp { op: code, .. } if *code == op::INITIATE_CAPTURE));
        if !capture_in_flight {
            return None;
        }
        queue.shutter_busy_responses_remaining -= 1;
        Some(Self::err(req.transaction_id, resp::DEVICE_BUSY))
    }

    fn camera_initiated_operation_reply(&mut self, req: &OperationRequest) -> Option<Reply> {
        let metadata_operation = self
            .manifest
            .camera_initiated_transfer
            .as_ref()
            .and_then(|transfer| parse_hex_code(&transfer.receive.metadata.operation));
        if metadata_operation == Some(req.code) {
            let index = req.params.first().copied().unwrap_or(0);
            let target = self.camera_initiated_metadata_target(req.code, index);
            if !matches!(target, CameraQueueTarget::None) {
                if self.state.phase == Phase::SessionOpen {
                    self.camera_initiated_pre_mode_probe_armed = false;
                }
                return Some(self.camera_initiated_metadata_reply(req.transaction_id, target));
            }
        }

        let data_operation = self
            .manifest
            .camera_initiated_transfer
            .as_ref()
            .and_then(|transfer| parse_hex_code(&transfer.receive.data.operation));
        if data_operation == Some(req.code) {
            let index = req.params.first().copied().unwrap_or(0);
            let target = self.camera_initiated_data_target(req.code, index);
            if !matches!(target, CameraQueueTarget::None) {
                return Some(self.camera_initiated_data_reply(req, target));
            }
        }
        None
    }

    fn camera_initiated_metadata_reply(
        &self,
        transaction_id: u32,
        target: CameraQueueTarget,
    ) -> Reply {
        let CameraQueueTarget::Head { handle } = target else {
            return Self::err(transaction_id, resp::INVALID_OBJECT_HANDLE);
        };
        match self.store.object_info(handle) {
            Ok(info) => {
                let mut writer = Writer::new();
                if info.encode(&mut writer).is_err() {
                    return Self::err(transaction_id, resp::GENERAL_ERROR);
                }
                Self::data(transaction_id, writer.into_vec())
            }
            Err(_) => Self::err(transaction_id, resp::INVALID_OBJECT_HANDLE),
        }
    }

    fn camera_initiated_data_reply(
        &self,
        req: &OperationRequest,
        target: CameraQueueTarget,
    ) -> Reply {
        let CameraQueueTarget::Head { handle } = target else {
            return Self::err(req.transaction_id, resp::INVALID_OBJECT_HANDLE);
        };
        let param = |index: usize| req.params.get(index).copied().unwrap_or(0);
        let offset = (param(1) as u64) | ((param(3) as u64) << 32);
        match self.store.read_range(handle, offset, param(2)) {
            Ok(source) => {
                let returned = source.len().min(u32::MAX as u64) as u32;
                let completion = self.camera_initiated_queue.as_ref().and_then(|queue| {
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
                        transaction_id: req.transaction_id,
                        params: vec![returned],
                    },
                    completion,
                }
            }
            Err(_) => Self::err(req.transaction_id, resp::INVALID_OBJECT_HANDLE),
        }
    }

    fn camera_initiated_metadata_target(&self, operation: u16, index: u32) -> CameraQueueTarget {
        let Some(transfer) = self.manifest.camera_initiated_transfer.as_ref() else {
            return CameraQueueTarget::None;
        };
        if !self.camera_initiated_transfer_active
            || transfer.handoff.connection != self.connection
            || parse_hex_code(&transfer.receive.metadata.operation) != Some(operation)
        {
            return CameraQueueTarget::None;
        }
        let metadata_phase = match self.state.phase {
            Phase::SessionOpen if self.camera_initiated_pre_mode_probe_armed => {
                CameraInitiatedMetadataPhase::AfterCountBeforeModeEntry
            }
            Phase::SessionOpen => return CameraQueueTarget::None,
            Phase::QueuedReceive => CameraInitiatedMetadataPhase::AfterModeEntry,
            _ => return CameraQueueTarget::None,
        };
        if transfer.receive.head_index != index {
            return CameraQueueTarget::Invalid;
        }
        if transfer.receive.metadata.phases.contains(&metadata_phase) {
            self.camera_queue_head()
        } else {
            CameraQueueTarget::Invalid
        }
    }

    fn prepare_camera_initiated_pre_mode_probe(&mut self, req: &OperationRequest) {
        let Some(transfer) = self.manifest.camera_initiated_transfer.as_ref() else {
            self.camera_initiated_pre_mode_probe_armed = false;
            return;
        };
        if !self.camera_initiated_transfer_active
            || transfer.handoff.connection != self.connection
            || self.state.phase != Phase::SessionOpen
            || !transfer
                .receive
                .metadata
                .phases
                .contains(&CameraInitiatedMetadataPhase::AfterCountBeforeModeEntry)
        {
            self.camera_initiated_pre_mode_probe_armed = false;
            return;
        }

        let is_metadata_probe =
            parse_hex_code(&transfer.receive.metadata.operation) == Some(req.code);
        if !is_metadata_probe {
            self.camera_initiated_pre_mode_probe_armed = false;
        }
    }

    fn is_camera_initiated_count_read(&self, req: &OperationRequest) -> bool {
        let Some(transfer) = self.manifest.camera_initiated_transfer.as_ref() else {
            return false;
        };
        self.camera_initiated_transfer_active
            && transfer.handoff.connection == self.connection
            && self.state.phase == Phase::SessionOpen
            && transfer
                .receive
                .metadata
                .phases
                .contains(&CameraInitiatedMetadataPhase::AfterCountBeforeModeEntry)
            && req.code == op::GET_DEVICE_PROP_VALUE
            && parse_hex_code(&transfer.receive.count.property).map(u32::from)
                == req.params.first().copied()
    }

    fn camera_initiated_data_target(&self, operation: u16, index: u32) -> CameraQueueTarget {
        if !self.camera_initiated_transfer_active || self.state.phase != Phase::QueuedReceive {
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
        let previous_mode = self.state.active_mode.clone();
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
        } else if self.state.phase == Phase::QueuedReceive {
            self.state.phase = Phase::SessionOpen;
        }
        if previous_mode == detected {
            return;
        }
        // Mode transition: the new mode's declared phase drives the workflow
        // phase (#407) — the manifest's detect predicate selects the mode, and
        // the engine never branches on a selector property code. Writes that
        // keep the same mode leave the phase alone, so in-session states like
        // Streaming survive them.
        let declared = |path: Option<&str>| -> Option<Phase> {
            path.and_then(|path| self.manifest.modes.get(path))
                .and_then(|mode| mode.phase)
                .map(Phase::from)
        };
        let previous_declared = declared(previous_mode.as_deref());
        let new_declared = declared(detected.as_deref());
        if !transfer_active {
            if let Some(phase) = new_declared {
                self.state.phase = phase;
            }
        }
        // Leaving the import workflow clears bootstrap gate progress. Keyed on
        // the transition out of a mode whose declared phase is imageImport
        // (#455 review): modes that declare no phase leave `state.phase`
        // sticky, so a phase-based check would let earned import gates
        // survive leaving the workflow.
        if previous_declared == Some(Phase::ImageImport) && new_declared != Some(Phase::ImageImport)
        {
            self.state.reset_gates();
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

    /// Serve a manifest-declared computed property from engine state (#407).
    /// `None` when the property declares no computed source.
    fn computed_property_reply(&self, tid: u32, code: u16) -> Option<Reply> {
        let computed = self.manifest.property(code)?.computed?;
        let handles = self.enumerated_object_handles();
        let mut w = Writer::new();
        match computed {
            camera_config::ComputedValue::ObjectCount => w.u32(handles.len() as u32),
            camera_config::ComputedValue::ObjectHandles => w.ptp_array(&handles, |w, v| w.u32(*v)),
        }
        Some(Self::data(tid, w.into_vec()))
    }

    /// The response refusing a cataloged operation that is unavailable in the
    /// current connection/mode/state, or `None` when it may proceed (#407).
    /// Operations absent from the catalog are not gated. The mode axis applies
    /// only once a mode is detected; before that, connection, kind, and
    /// `requires` still gate, but the sim doesn't refuse ops off a guessed
    /// fallback mode (PCSS never flips a selector at all). The three
    /// property-access ops skip the mode axis entirely: the property surface
    /// is modeled per property (`access`, `requiresGate`), and the catalog's
    /// mode rows for those standard ops are transport observations, not
    /// camera refusals.
    fn operation_availability_error(&self, code: u16) -> Option<u16> {
        self.manifest.operation(code)?;
        let observed: camera_config::PropView = self
            .state
            .props
            .iter()
            .filter_map(|(code, value)| value_to_i64(value).map(|value| (*code, value)))
            .collect();
        let property_access_op = matches!(
            code,
            op::GET_DEVICE_PROP_DESC | op::GET_DEVICE_PROP_VALUE | op::SET_DEVICE_PROP_VALUE
        );
        let availability = match self.state.active_mode.as_deref() {
            Some(mode) if !property_access_op => {
                self.manifest
                    .operation_available(&self.connection, mode, code, &observed)
            }
            _ => self
                .manifest
                .operation_available_predetect(&self.connection, code, &observed),
        };
        match availability {
            camera_config::Availability::Available => None,
            // A failed `requires` prerequisite is a runtime refusal, not an
            // out-of-catalog one.
            camera_config::Availability::Blocked => Some(resp::GENERAL_ERROR),
            camera_config::Availability::WrongMode
            | camera_config::Availability::WrongConnection
            | camera_config::Availability::Unavailable => Some(resp::OPERATION_NOT_SUPPORTED),
        }
    }

    fn connection_models_live_view_stream(&self, connection_id: &str) -> bool {
        self.manifest
            .connections
            .get(connection_id)
            .and_then(|connection| connection.actions.get(&ActionVerb::StartLiveView))
            .is_some_and(|action| action_sends_op(action, op::INITIATE_OPEN_CAPTURE))
    }

    fn matches_live_view_arming_write(&self, code: u16, value: i64) -> bool {
        self.manifest
            .connections
            .get(&self.connection)
            .and_then(|connection| connection.actions.get(&ActionVerb::StartLiveView))
            .and_then(Action::initiator)
            .is_some_and(|initiator| {
                initiator.steps.iter().any(|step| {
                    step.set_prop.as_deref().and_then(parse_hex_code) == Some(code)
                        && step.value.as_ref().and_then(SetPropValue::literal) == Some(value)
                })
            })
    }

    fn pending_queue_blocks_live_view_arming(&self) -> bool {
        let Some(queue) = &self.transfer_queue else {
            return false;
        };
        queue.connection == self.connection && !queue.available.is_empty()
    }

    fn stale_stream_blocks_live_view_arming(&self) -> bool {
        self.live_view_stream_connection.as_deref() == Some(self.connection.as_str())
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

    /// Manifest-driven dispatch for non-standard ops: the catalog row's handler
    /// selects the generic behavior; a handler-less executable row is a
    /// successful no-op. Handler values are a closed set validated at manifest
    /// load (#407), so no fallthrough can swallow a typo.
    fn dispatch_manifest_op(&mut self, tid: u32, code: u16, params: &[u32]) -> Reply {
        let Some(opdef) = self.manifest.operation(code) else {
            return Self::err(tid, resp::OPERATION_NOT_SUPPORTED);
        };
        match opdef.handler {
            Some(camera_config::OperationHandler::PropertyStep) => {
                let Some(prop_code) = opdef.property.as_deref().and_then(parse_hex_code) else {
                    return Self::err(tid, resp::GENERAL_ERROR);
                };
                let direction = params.first().copied().unwrap_or(0);
                self.vendor_step(prop_code, direction);
                Self::ok(tid)
            }
            Some(camera_config::OperationHandler::ObjectSize) => {
                self.object_size_op(tid, opdef, params)
            }
            None => Self::ok(tid),
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
            .as_ref()
            .and_then(|current| {
                desc.values.iter().position(|value| {
                    typed_descriptor_value(datatype, value).as_ref() == Some(current)
                })
            })
            .unwrap_or(0);
        let new_idx = if direction != 0 {
            (cur_idx + 1).min(desc.values.len() - 1)
        } else {
            cur_idx.saturating_sub(1)
        };
        let converted = typed_descriptor_value(datatype, &desc.values[new_idx]);
        // Load-time validation rejects unconvertible descriptor values, so a
        // None here means the manifest slipped past it; do not no-op quietly.
        debug_assert!(
            converted.is_some(),
            "vendor_step target value for {prop_code:#06x} does not convert at its declared datatype"
        );
        if let Some(value) = converted {
            self.state.props.insert(prop_code, value);
        }
    }

    fn set_prop(&mut self, tid: u32, code: u16, data_in: Option<&[u8]>) -> Reply {
        let Some(prop) = self.manifest.property(code) else {
            return Self::err(tid, resp::DEVICE_PROP_NOT_SUPPORTED);
        };
        // The manifest's declared access is the write claim; anything else is
        // get-only, matching the DevicePropDesc the camera reports (#407).
        if prop.access != Some(camera_config::PropertyAccess::ReadWrite) {
            return Self::err(tid, resp::ACCESS_DENIED);
        }
        let datatype = datatype_of(prop.ptype.as_deref());
        let Some(bytes) = data_in else {
            return Self::err(tid, resp::INVALID_PARAMETER);
        };
        let mut r = Reader::new(bytes);
        let Ok(value) = PropValue::decode(&mut r, datatype) else {
            return Self::err(tid, resp::INVALID_PARAMETER);
        };
        if value_to_i64(&value).is_some_and(|value| {
            self.matches_live_view_arming_write(code, value)
                && (self.pending_queue_blocks_live_view_arming()
                    || self.stale_stream_blocks_live_view_arming())
        }) {
            return Self::err(tid, LIVE_VIEW_ARMING_BLOCKED);
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
        use protocol_primitives::quirk::{
            typed_record_stream, RecordStreamDescriptor, RecordStreamLayout, RecordValueEncoding,
        };
        let Some(payload) = self
            .manifest
            .property(property)
            .and_then(|p| p.payload.as_ref())
        else {
            let descriptor = RecordStreamDescriptor::new(RecordStreamLayout::D212, [])?;
            return typed_record_stream(&[], &descriptor);
        };
        let (count_w, code_w, value_w) = payload.record_widths();
        let descriptor = RecordStreamDescriptor::new(
            RecordStreamLayout::new(count_w, code_w, value_w)?,
            payload.members.iter().filter_map(|member| {
                let code = parse_hex_code(member.code())?;
                let encoding = match member.encoding(value_w) {
                    camera_config::RecordValueEncoding::Fixed { width } => {
                        RecordValueEncoding::Fixed { width }
                    }
                    camera_config::RecordValueEncoding::Signed { width } => {
                        RecordValueEncoding::Signed { width }
                    }
                    camera_config::RecordValueEncoding::PtpString => RecordValueEncoding::PtpString,
                };
                Some((code, encoding))
            }),
        )?;
        let records: Vec<(u16, PropValue)> = payload
            .members
            .iter()
            .filter_map(|member| {
                let code = parse_hex_code(member.code())?;
                let encoding = match member.encoding(value_w) {
                    camera_config::RecordValueEncoding::Fixed { width } => {
                        RecordValueEncoding::Fixed { width }
                    }
                    camera_config::RecordValueEncoding::Signed { width } => {
                        RecordValueEncoding::Signed { width }
                    }
                    camera_config::RecordValueEncoding::PtpString => RecordValueEncoding::PtpString,
                };
                let value = self
                    .state
                    .props
                    .get(&code)
                    .filter(|value| encoding.accepts_value(value))
                    .cloned()
                    .or_else(|| match member.simulator_value() {
                        Some(camera_config::RecordValueLiteral::Unsigned(value)) => {
                            Some(PropValue::U32(*value))
                        }
                        Some(camera_config::RecordValueLiteral::Signed(value)) => {
                            Some(PropValue::I32(*value))
                        }
                        Some(camera_config::RecordValueLiteral::String(value)) => {
                            Some(PropValue::Str(value.clone()))
                        }
                        None => None,
                    })
                    .filter(|value| encoding.accepts_value(value))
                    .unwrap_or_else(|| record_value_zero(encoding));
                Some((code, value))
            })
            .collect();
        typed_record_stream(&records, &descriptor)
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
            match &entry.execution {
                camera_config::ModeEntryExecution::Ptp { steps } => {
                    collect_gate_sequences(steps, &mut out);
                }
                camera_config::ModeEntryExecution::ReestablishConnection(reestablish) => {
                    collect_gate_sequences(&reestablish.exit_steps, &mut out);
                }
                camera_config::ModeEntryExecution::UserInstruction { .. } => {}
            }
        }
        for action in connection.actions.values() {
            if let Some(initiator) = &action.initiator {
                collect_gate_sequences(&initiator.steps, &mut out);
            }
        }
    }
    out
}

fn channel_gate_name(connection: &str, role: camera_config::SocketRole) -> String {
    format!("channel-ready:{connection}:{role:?}")
}

fn compile_channel_sequences(manifest: &CameraManifest) -> Vec<GateSequence> {
    let mut out = Vec::new();
    for (connection_name, connection) in &manifest.connections {
        if let Some(bindings) = connection.bindings.as_ref() {
            for role in [
                camera_config::SocketRole::Event,
                camera_config::SocketRole::LiveView,
            ] {
                let Some(availability) = bindings.available_after(role) else {
                    continue;
                };
                if let Some(op) = parse_hex_code(&availability.operation) {
                    out.push(GateSequence {
                        name: channel_gate_name(connection_name, role),
                        steps: vec![GateMatcher::SuccessfulOperation { op }],
                    });
                }
            }
        }
        for entry in &connection.entries {
            if let camera_config::ModeEntryExecution::Ptp { steps } = &entry.execution {
                collect_channel_sequences(
                    connection_name,
                    connection.bindings.as_ref(),
                    steps,
                    &mut out,
                );
            }
        }
        for action in connection.actions.values() {
            if let Some(initiator) = &action.initiator {
                collect_channel_sequences(
                    connection_name,
                    connection.bindings.as_ref(),
                    &initiator.steps,
                    &mut out,
                );
            }
        }
    }
    out
}

fn collect_channel_sequences(
    connection: &str,
    bindings: Option<&camera_config::SocketBindings>,
    steps: &[Step],
    out: &mut Vec<GateSequence>,
) {
    let mut causal_prefix = Vec::new();
    for step in steps {
        if let Some(role) = step.open_channel {
            if bindings.is_some_and(|bindings| bindings.available_after(role).is_some()) {
                continue;
            }
            if !causal_prefix.is_empty() {
                let sequence = GateSequence {
                    name: channel_gate_name(connection, role),
                    steps: causal_prefix.clone(),
                };
                if !out.contains(&sequence) {
                    out.push(sequence);
                }
            }
            continue;
        }
        if step.tolerant {
            // The executor advances past a tolerated non-OK response, while the
            // engine advances wire gates only after successful operations. A
            // tolerant step therefore cannot be a mandatory causal prefix.
            causal_prefix.clear();
            continue;
        }
        if let Some(matcher) = matcher_for_step(step) {
            causal_prefix.extend(std::iter::repeat_n(matcher, step.repeat.max(1) as usize));
        } else {
            // A channel boundary is simulator-enforceable only from the latest
            // contiguous wire segment. Camera-side validation of the final
            // operation still guards any earlier preconditions.
            causal_prefix.clear();
        }
    }
}

fn collect_gate_sequences(steps: &[Step], out: &mut Vec<GateSequence>) {
    let mut active: std::collections::BTreeMap<String, Option<Vec<GateMatcher>>> =
        std::collections::BTreeMap::new();
    for step in steps {
        if let Some(retry) = &step.retry {
            collect_gate_sequences(&retry.steps, out);
        }
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
        let value = match step.value.as_ref() {
            None => None,
            Some(SetPropValue::Literal(value)) => Some(*value),
            Some(SetPropValue::Runtime(_)) => return None,
        };
        return Some(GateMatcher::SetProp {
            prop: parse_hex_code(prop)?,
            value,
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
    let mut sequence = Vec::new();
    for step in steps {
        if step.retry.is_some() && (step.set_prop.is_some() || step.send_op.is_some()) {
            // A step carrying both its own op and a retry block has no defined
            // matcher ordering; refuse rather than silently dropping the op.
            return None;
        }
        if let Some(retry) = &step.retry {
            sequence.extend(matcher_sequence_for_steps(&retry.steps)?);
        } else {
            sequence.push(matcher_for_step(step)?);
        }
    }
    (!sequence.is_empty()).then_some(sequence)
}

fn action_sends_op(action: &Action, code: u16) -> bool {
    action
        .initiator()
        .into_iter()
        .flat_map(|binding| &binding.steps)
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
        GateMatcher::SuccessfulOperation { op } => req.code == *op,
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

fn truncate_reply_data(reply: &mut Reply, keep: u64) {
    match reply {
        Reply::Data { data, .. } => data.truncate(keep.min(data.len() as u64) as usize),
        Reply::DataStream {
            source, completion, ..
        } => {
            let len = keep.min(source.len());
            *source = match source.clone() {
                ByteSource::Memory(mut bytes) => {
                    bytes.truncate(len as usize);
                    ByteSource::Memory(bytes)
                }
                ByteSource::FileRange { path, offset, .. } => {
                    ByteSource::FileRange { path, offset, len }
                }
                ByteSource::Generated { seed, .. } => ByteSource::Generated { len, seed },
            };
            *completion = None;
        }
        Reply::Response(_) | Reply::NoResponse | Reply::Close => {}
    }
}

fn replace_reply_data(reply: &mut Reply, bytes: &[u8]) {
    match reply {
        Reply::Data { data, .. } => *data = bytes.to_vec(),
        Reply::DataStream { response, .. } => {
            *reply = Reply::Data {
                data: bytes.to_vec(),
                response: response.clone(),
            };
        }
        Reply::Response(_) | Reply::NoResponse | Reply::Close => {}
    }
}

fn scalar_fits_property(ptype: Option<&str>, value: i64) -> bool {
    match ptype {
        Some("u8") => u8::try_from(value).is_ok(),
        Some("u16") => u16::try_from(value).is_ok(),
        Some("u32") => u32::try_from(value).is_ok(),
        Some("u64") => value >= 0,
        Some("i8") => i8::try_from(value).is_ok(),
        Some("i16") => i16::try_from(value).is_ok(),
        Some("i32") => i32::try_from(value).is_ok(),
        Some("i64") => true,
        _ => false,
    }
}

fn property_transition_value(ptype: Option<&str>, value: i64) -> Option<PropValue> {
    Some(match ptype {
        Some("u8") => PropValue::U8(u8::try_from(value).ok()?),
        Some("u16") => PropValue::U16(u16::try_from(value).ok()?),
        Some("u32") => PropValue::U32(u32::try_from(value).ok()?),
        Some("u64") => PropValue::U64(u64::try_from(value).ok()?),
        Some("i8") => PropValue::I8(i8::try_from(value).ok()?),
        Some("i16") => PropValue::I16(i16::try_from(value).ok()?),
        Some("i32") => PropValue::I32(i32::try_from(value).ok()?),
        Some("i64") => PropValue::I64(value),
        _ => return None,
    })
}

fn value_to_i64(v: &PropValue) -> Option<i64> {
    Some(match v {
        PropValue::I8(x) => *x as i64,
        PropValue::U8(x) => *x as i64,
        PropValue::I16(x) => *x as i64,
        PropValue::U16(x) => *x as i64,
        PropValue::I32(x) => *x as i64,
        PropValue::U32(x) => *x as i64,
        PropValue::I64(x) => *x,
        PropValue::U64(x) => *x as i64,
        PropValue::Str(_) => return None,
    })
}

fn record_value_zero(encoding: protocol_primitives::quirk::RecordValueEncoding) -> PropValue {
    match encoding {
        protocol_primitives::quirk::RecordValueEncoding::Fixed { .. } => PropValue::U32(0),
        protocol_primitives::quirk::RecordValueEncoding::Signed { width: 1 } => PropValue::I8(0),
        protocol_primitives::quirk::RecordValueEncoding::Signed { width: 2 } => PropValue::I16(0),
        protocol_primitives::quirk::RecordValueEncoding::Signed { .. } => PropValue::I32(0),
        protocol_primitives::quirk::RecordValueEncoding::PtpString => PropValue::Str(String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camera_config::{CameraManifest, PropertyTransitionTerminal, ResponderMutation};
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

    fn transition_engine() -> Engine {
        let manifest = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: Test, model: Test, firmware: "1" }
properties:
  "0xd001": { name: result, type: u16, access: readOnly }
"#,
        )
        .unwrap();
        let mut engine = Engine::new(manifest, empty_store());
        engine.on_operation(&req(op::OPEN_SESSION, 1, vec![1]), None);
        engine
    }

    fn transition(terminal: i64, settle_after_polls: u32) -> ResponderMutation {
        ResponderMutation::PropertyTransition {
            target: "0xd001".into(),
            initial: Some(1),
            terminal: PropertyTransitionTerminal::Fixed { value: terminal },
            settle_after_polls,
        }
    }

    #[test]
    fn get_prop_value_for_string_property_without_value_is_empty_string() {
        // #417: the read path must agree with GetDevicePropDesc. A str
        // property with no seeded value reports the empty string, not a
        // fabricated "0" (which also violates structuredText layouts).
        let manifest = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: Test, model: Test, firmware: "1" }
properties:
  "0xd395":
    name: liveViewFocusArea
    type: str
"#,
        )
        .unwrap();
        let mut engine = Engine::new(manifest, empty_store());
        engine.on_operation(&req(op::OPEN_SESSION, 1, vec![1]), None);
        let reply = engine.on_operation(&req(op::GET_DEVICE_PROP_VALUE, 2, vec![0xd395]), None);
        let Reply::Data { data, .. } = reply else {
            panic!("expected data reply for 0xd395, got {reply:?}")
        };
        let mut r = ptp_core::Reader::new(&data);
        assert_eq!(
            PropValue::decode(&mut r, ptp_core::codes::datatype_code::STR).unwrap(),
            PropValue::Str(String::new())
        );
    }

    #[test]
    fn vendor_step_moves_through_string_descriptor_values() {
        let manifest = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: Test, model: Test, firmware: "1" }
properties:
  "0xd001":
    name: example
    type: str
    access: readWrite
    descriptor: { form: enum, values: ["a", "b"] }
"#,
        )
        .unwrap();
        let mut engine = Engine::new(manifest, empty_store());
        assert_eq!(
            engine.state.props.get(&0xd001),
            Some(&PropValue::Str("a".into()))
        );

        engine.vendor_step(0xd001, 1);
        assert_eq!(
            engine.state.props.get(&0xd001),
            Some(&PropValue::Str("b".into()))
        );

        engine.vendor_step(0xd001, 0);
        assert_eq!(
            engine.state.props.get(&0xd001),
            Some(&PropValue::Str("a".into()))
        );
    }

    #[test]
    fn record_stream_defaults_unset_string_state_to_an_empty_ptp_string() {
        let manifest = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: Test, model: Test, firmware: "1" }
properties:
  "0xd212":
    name: status
    type: u8a
    access: readOnly
    payload:
      form: recordStream
      members:
        - { code: "0xd22f", encoding: { kind: ptpString } }
  "0xd22f": { name: text, type: str, access: readWrite }
"#,
        )
        .unwrap();
        let engine = Engine::new(manifest, empty_store());

        let bytes = engine.record_stream_property(0xd212).unwrap();
        let descriptor = protocol_primitives::quirk::RecordStreamDescriptor::new(
            protocol_primitives::quirk::RecordStreamLayout::D212,
            [(
                0xd22f,
                protocol_primitives::quirk::RecordValueEncoding::PtpString,
            )],
        )
        .unwrap();
        assert_eq!(
            protocol_primitives::quirk::parse_typed_record_stream(&bytes, &descriptor)
                .unwrap()
                .records,
            vec![(0xd22f, PropValue::Str(String::new()))]
        );
    }

    #[test]
    fn record_stream_replaces_incompatible_state_with_fallback_or_zero() {
        let manifest = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: Test, model: Test, firmware: "1" }
properties:
  "0xd212":
    name: status
    type: u8a
    access: readOnly
    payload:
      form: recordStream
      members:
        - { code: "0xd100", encoding: { kind: fixed, width: 4 }, simulatorValue: 7 }
        - { code: "0xd101", encoding: { kind: fixed, width: 4 } }
        - { code: "0xd102", encoding: { kind: ptpString }, simulatorValue: fallback }
        - { code: "0xd103", encoding: { kind: ptpString } }
        - { code: "0xd104", encoding: { kind: fixed, width: 4 }, simulatorValue: 7 }
        - { code: "0xd105", encoding: { kind: fixed, width: 4 } }
  "0xd100": { name: fixedFallback, type: str, access: readWrite }
  "0xd101": { name: fixedZero, type: str, access: readWrite }
  "0xd102": { name: stringFallback, type: u32, access: readWrite }
  "0xd103": { name: stringZero, type: str, access: readWrite }
  "0xd104": { name: negativeFallback, type: i32, access: readWrite }
  "0xd105": { name: negativeZero, type: i64, access: readWrite }
"#,
        )
        .unwrap();
        let mut engine = Engine::new(manifest, empty_store());
        engine
            .state
            .props
            .insert(0xd100, PropValue::Str("wrong".into()));
        engine
            .state
            .props
            .insert(0xd101, PropValue::Str("wrong".into()));
        engine.state.props.insert(0xd102, PropValue::U32(99));
        engine.state.props.insert(0xd103, PropValue::U32(99));
        engine.state.props.insert(0xd104, PropValue::I32(-1));
        engine.state.props.insert(0xd105, PropValue::I64(i64::MIN));

        let bytes = engine.record_stream_property(0xd212).unwrap();
        let descriptor = protocol_primitives::quirk::RecordStreamDescriptor::new(
            protocol_primitives::quirk::RecordStreamLayout::D212,
            [
                (
                    0xd100,
                    protocol_primitives::quirk::RecordValueEncoding::Fixed { width: 4 },
                ),
                (
                    0xd101,
                    protocol_primitives::quirk::RecordValueEncoding::Fixed { width: 4 },
                ),
                (
                    0xd102,
                    protocol_primitives::quirk::RecordValueEncoding::PtpString,
                ),
                (
                    0xd103,
                    protocol_primitives::quirk::RecordValueEncoding::PtpString,
                ),
                (
                    0xd104,
                    protocol_primitives::quirk::RecordValueEncoding::Fixed { width: 4 },
                ),
                (
                    0xd105,
                    protocol_primitives::quirk::RecordValueEncoding::Fixed { width: 4 },
                ),
            ],
        )
        .unwrap();
        assert_eq!(
            protocol_primitives::quirk::parse_typed_record_stream(&bytes, &descriptor)
                .unwrap()
                .records,
            vec![
                (0xd100, PropValue::U32(7)),
                (0xd101, PropValue::U32(0)),
                (0xd102, PropValue::Str("fallback".into())),
                (0xd103, PropValue::Str(String::new())),
                (0xd104, PropValue::U32(7)),
                (0xd105, PropValue::U32(0)),
            ]
        );
    }

    #[test]
    fn record_stream_keeps_positive_width_overflow_fail_closed() {
        let manifest = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: Test, model: Test, firmware: "1" }
properties:
  "0xd212":
    name: status
    type: u8a
    access: readOnly
    payload:
      form: recordStream
      members:
        - { code: "0xd100", encoding: { kind: fixed, width: 1 }, simulatorValue: 7 }
  "0xd100": { name: oversized, type: i16, access: readWrite }
"#,
        )
        .unwrap();
        let mut engine = Engine::new(manifest, empty_store());
        engine.state.props.insert(0xd100, PropValue::I16(0x100));
        engine.on_operation(&req(op::OPEN_SESSION, 1, vec![1]), None);

        assert!(matches!(
            engine.on_operation(
                &req(op::GET_DEVICE_PROP_VALUE, 2, vec![u32::from(0xd212u16)]),
                None,
            ),
            Reply::Response(response) if response.code == resp::GENERAL_ERROR
        ));
    }

    #[test]
    fn responder_property_transition_immediate_value_wins_over_initial() {
        let mut engine = transition_engine();
        let prepared = engine
            .prepare_responder_mutation(&transition(2, 0), &BTreeMap::new())
            .unwrap();
        engine.apply_responder_mutation(&prepared).unwrap();
        assert_eq!(poll(&mut engine, 0xd001, 1), 2);
    }

    #[test]
    fn responder_property_transition_settles_on_exact_poll() {
        let mut engine = transition_engine();
        let prepared = engine
            .prepare_responder_mutation(&transition(2, 2), &BTreeMap::new())
            .unwrap();
        engine.apply_responder_mutation(&prepared).unwrap();
        assert_eq!(poll(&mut engine, 0xd001, 1), 1);
        assert_eq!(poll(&mut engine, 0xd001, 2), 2);
        assert_eq!(poll(&mut engine, 0xd001, 3), 2);
    }

    #[test]
    fn responder_property_transition_rearm_replaces_pending_value() {
        let mut engine = transition_engine();
        let first = engine
            .prepare_responder_mutation(&transition(2, 2), &BTreeMap::new())
            .unwrap();
        engine.apply_responder_mutation(&first).unwrap();
        assert_eq!(poll(&mut engine, 0xd001, 1), 1);

        let replacement = engine
            .prepare_responder_mutation(&transition(3, 2), &BTreeMap::new())
            .unwrap();
        engine.apply_responder_mutation(&replacement).unwrap();
        assert_eq!(poll(&mut engine, 0xd001, 2), 1);
        assert_eq!(poll(&mut engine, 0xd001, 3), 3);
    }

    #[test]
    fn responder_property_transition_preserves_signed_target_width() {
        let manifest = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: Test, model: Test, firmware: "1" }
properties:
  "0xd001": { name: result, type: i32, access: readOnly }
"#,
        )
        .unwrap();
        let mut engine = Engine::new(manifest, empty_store());
        engine.on_operation(&req(op::OPEN_SESSION, 1, vec![1]), None);
        let prepared = engine
            .prepare_responder_mutation(
                &ResponderMutation::PropertyTransition {
                    target: "0xd001".into(),
                    initial: None,
                    terminal: PropertyTransitionTerminal::Fixed { value: -1 },
                    settle_after_polls: 0,
                },
                &BTreeMap::new(),
            )
            .unwrap();
        engine.apply_responder_mutation(&prepared).unwrap();
        assert_eq!(poll_u32(&mut engine, 0xd001, 2), i64::from(u32::MAX));
    }

    #[test]
    fn property_transition_supports_i8_and_i64() {
        assert!(scalar_fits_property(Some("i8"), i64::from(i8::MIN)));
        assert!(!scalar_fits_property(Some("i8"), i64::from(i8::MAX) + 1));
        assert_eq!(
            property_transition_value(Some("i8"), -1),
            Some(PropValue::I8(-1))
        );
        assert!(scalar_fits_property(Some("i64"), i64::MIN));
        assert_eq!(
            property_transition_value(Some("i64"), i64::MIN),
            Some(PropValue::I64(i64::MIN))
        );
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

    #[test]
    fn undeclared_auxiliary_channels_follow_manifest_causal_boundary() {
        let manifest = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
connections:
  app:
    kind: ptpip-app
    bindings: { command: 55740, event: 55741, liveView: 55742 }
    entries:
      - to: shooting/stills
        steps:
          - { setProp: "0xdf01", value: 22 }
          - { sendOp: "0x101c" }
          - { openChannel: event }
          - { openChannel: liveView }
modes:
  shooting/stills:
    detect: { prop: "0xdf01", eq: 22 }
    phase: liveView
operations:
  "0x1002": { name: OpenSession, connections: [app] }
properties:
  "0xdf01": { name: functionMode, type: u16, access: readWrite }
"#,
        )
        .unwrap();
        let mut engine = Engine::new(manifest, empty_store());
        assert!(!engine.channel_ready(camera_config::SocketRole::Event));
        assert!(!engine.channel_ready(camera_config::SocketRole::LiveView));

        engine.on_operation(&req(op::OPEN_SESSION, 1, vec![1]), None);
        engine.on_operation(
            &req(op::SET_DEVICE_PROP_VALUE, 2, vec![0xdf01]),
            Some(&22u16.to_le_bytes()),
        );
        assert!(!engine.channel_ready(camera_config::SocketRole::Event));
        assert!(reply_is_ok(
            &engine.on_operation(&req(op::INITIATE_OPEN_CAPTURE, 3, vec![]), None)
        ));
        assert!(engine.channel_ready(camera_config::SocketRole::Event));
        assert!(engine.channel_ready(camera_config::SocketRole::LiveView));
    }

    #[test]
    fn declared_auxiliary_gate_overrides_derived_prefix() {
        let manifest = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: TESTCO, model: TM1, firmware: "1" }
connections:
  app:
    kind: ptpip
    bindings:
      command: 55740
      event:
        port: 55741
        availableAfter: { operation: "0x101c" }
    entries:
      - to: shooting/stills
        steps:
          - { setProp: "0xdf01", value: 22 }
          - { openChannel: event }
          - { sendOp: "0x101c" }
modes:
  shooting/stills:
    detect: { prop: "0xdf01", eq: 22 }
    phase: liveView
operations:
  "0x1002": { name: OpenSession, connections: [app] }
  "0x101c": { name: InitiateOpenCapture, connections: [app] }
properties:
  "0xdf01": { name: functionMode, type: u16, access: readWrite }
"#,
        )
        .unwrap();
        let mut engine = Engine::new(manifest, empty_store());
        engine.on_operation(&req(op::OPEN_SESSION, 1, vec![1]), None);
        engine.on_operation(
            &req(op::SET_DEVICE_PROP_VALUE, 2, vec![0xdf01]),
            Some(&22u16.to_le_bytes()),
        );
        assert!(
            !engine.channel_ready(camera_config::SocketRole::Event),
            "the declared operation overrides the openChannel causal prefix"
        );

        assert!(reply_is_ok(
            &engine.on_operation(&req(op::INITIATE_OPEN_CAPTURE, 3, vec![]), None)
        ));
        assert!(engine.channel_ready(camera_config::SocketRole::Event));
    }

    #[test]
    fn strict_suffix_after_tolerant_step_controls_channel_gate() {
        let manifest = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: TESTCO, model: TM1, firmware: "1" }
connections:
  app:
    kind: ptpip
    bindings: { command: 55740, event: 55741 }
    entries:
      - to: shooting/stills
        steps:
          - { sendOp: "0x9999", tolerant: true }
          - { setProp: "0xdf01", value: 22 }
          - { openChannel: event }
operations:
  "0x1002": { name: OpenSession, connections: [app] }
properties:
  "0xdf01": { name: functionMode, type: u16, access: readWrite }
"#,
        )
        .unwrap();
        let mut engine = Engine::new(manifest, empty_store());
        assert!(!engine.channel_ready(camera_config::SocketRole::Event));

        engine.on_operation(&req(op::OPEN_SESSION, 1, vec![1]), None);
        assert!(!reply_is_ok(
            &engine.on_operation(&req(0x9999, 2, vec![]), None)
        ));
        assert!(!engine.channel_ready(camera_config::SocketRole::Event));

        engine.on_operation(
            &req(op::SET_DEVICE_PROP_VALUE, 3, vec![0xdf01]),
            Some(&22u16.to_le_bytes()),
        );
        assert!(engine.channel_ready(camera_config::SocketRole::Event));
    }

    fn response_code_of(reply: Reply) -> u16 {
        match reply {
            Reply::Response(r) => r.code,
            Reply::Data { response, .. } => response.code,
            Reply::DataStream { response, .. } => response.code,
            other => panic!("expected a coded response, got {other:?}"),
        }
    }

    /// #407: cataloged ops resolve connection × mode × kind × `requires`
    /// before dispatch. The mode axis engages once a mode is detected; before
    /// that, only connection/kind/requires gate.
    #[test]
    fn catalog_gating_refuses_out_of_context_ops() {
        let manifest = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: TESTCO, model: TM1, firmware: "1" }
connections:
  app: { kind: ptpip-app }
modes:
  shooting/stills:
    detect: { prop: "0xdf01", eq: 22 }
  image-transfer:
    detect: { prop: "0xdf01", eq: 20 }
    phase: imageImport
operations:
  "0x1002": { name: OpenSession, connections: [app] }
  "0x9001": { name: UsbOnly, connections: [usb] }
  "0x9002": { name: ImportOnly, modes: [image-transfer] }
  "0x9003": { name: InventoryRow, kind: advertisedOnly }
  "0x9004": { name: NeedsCard, requires: { prop: "0xd001", eq: 1 } }
properties:
  "0xdf01": { name: functionMode, type: u16, access: readWrite }
  "0xd001": { name: cardPresent, type: u16, access: readWrite }
"#,
        )
        .unwrap();
        let mut e = Engine::new(manifest, empty_store());
        assert_ok_open(&mut e);

        // Wrong connection is refused regardless of mode state.
        assert_eq!(
            response_code_of(e.on_operation(&req(0x9001, 2, vec![]), None)),
            resp::OPERATION_NOT_SUPPORTED
        );
        // Advertised-only inventory rows never execute.
        assert_eq!(
            response_code_of(e.on_operation(&req(0x9003, 3, vec![]), None)),
            resp::OPERATION_NOT_SUPPORTED
        );
        // No detected mode yet: the mode axis is open, so the mode-gated op
        // proceeds (a successful no-op here).
        assert!(reply_is_ok(&e.on_operation(&req(0x9002, 4, vec![]), None)));
        // `requires` unmet → runtime refusal.
        assert_eq!(
            response_code_of(e.on_operation(&req(0x9004, 5, vec![]), None)),
            resp::GENERAL_ERROR
        );

        // Enter image-transfer: the mode-gated op is now available.
        e.on_operation(
            &req(op::SET_DEVICE_PROP_VALUE, 6, vec![0xdf01]),
            Some(&20u16.to_le_bytes()),
        );
        assert_eq!(e.state().phase, Phase::ImageImport);
        assert!(reply_is_ok(&e.on_operation(&req(0x9002, 7, vec![]), None)));

        // Switch to a different detected mode: now it is wrong-mode.
        e.on_operation(
            &req(op::SET_DEVICE_PROP_VALUE, 8, vec![0xdf01]),
            Some(&22u16.to_le_bytes()),
        );
        assert_eq!(
            response_code_of(e.on_operation(&req(0x9002, 9, vec![]), None)),
            resp::OPERATION_NOT_SUPPORTED
        );

        // Satisfy the prerequisite → the requires-gated op becomes available.
        e.on_operation(
            &req(op::SET_DEVICE_PROP_VALUE, 10, vec![0xd001]),
            Some(&1u16.to_le_bytes()),
        );
        assert!(reply_is_ok(&e.on_operation(&req(0x9004, 11, vec![]), None)));
    }

    /// #407: SetDevicePropValue honors the declared access; only `readWrite`
    /// properties accept writes.
    #[test]
    fn set_prop_enforces_declared_access() {
        let manifest = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: TESTCO, model: TM1, firmware: "1" }
properties:
  "0x5001": { name: writableProp, type: u16, access: readWrite }
  "0x5002": { name: readOnlyProp, type: u16, access: readOnly }
  "0x5003": { name: undeclaredProp, type: u16 }
"#,
        )
        .unwrap();
        let mut e = Engine::new(manifest, empty_store());
        assert_ok_open(&mut e);

        assert!(reply_is_ok(&e.on_operation(
            &req(op::SET_DEVICE_PROP_VALUE, 2, vec![0x5001]),
            Some(&7u16.to_le_bytes()),
        )));
        assert_eq!(e.state().props.get(&0x5001), Some(&PropValue::U16(7)));

        assert_eq!(
            response_code_of(e.on_operation(
                &req(op::SET_DEVICE_PROP_VALUE, 3, vec![0x5002]),
                Some(&7u16.to_le_bytes()),
            )),
            resp::ACCESS_DENIED
        );
        // No write claim = no write, matching the get-only descriptor served.
        assert_eq!(
            response_code_of(e.on_operation(
                &req(op::SET_DEVICE_PROP_VALUE, 4, vec![0x5003]),
                Some(&7u16.to_le_bytes()),
            )),
            resp::ACCESS_DENIED
        );
        // Reads remain available on all three.
        assert!(matches!(
            e.on_operation(&req(op::GET_DEVICE_PROP_VALUE, 5, vec![0x5002]), None),
            Reply::Data { .. }
        ));
    }

    /// #407: OpenSession refuses session id 0 and a second open instead of
    /// silently resetting the session. PTP only forbids id 0; non-1 ids are
    /// accepted absent wire evidence of refusal (#455 review).
    #[test]
    fn open_session_validates_parameter_and_reentry() {
        let manifest = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: TESTCO, model: TM1, firmware: "1" }
"#,
        )
        .unwrap();
        let mut e = Engine::new(manifest, empty_store());

        assert_eq!(
            response_code_of(e.on_operation(&req(op::OPEN_SESSION, 1, vec![0]), None)),
            resp::INVALID_PARAMETER
        );
        assert!(!e.state().session_open);

        assert!(reply_is_ok(
            &e.on_operation(&req(op::OPEN_SESSION, 2, vec![7]), None)
        ));
        assert_eq!(
            response_code_of(e.on_operation(&req(op::OPEN_SESSION, 3, vec![1]), None)),
            resp::SESSION_ALREADY_OPEN
        );

        assert!(reply_is_ok(
            &e.on_operation(&req(op::CLOSE_SESSION, 4, vec![]), None)
        ));
        assert!(reply_is_ok(
            &e.on_operation(&req(op::OPEN_SESSION, 5, vec![1]), None)
        ));
    }

    /// #455 review: a command transport that ends without CloseSession takes
    /// the session with it, so a reconnecting client can open again.
    #[test]
    fn transport_lost_clears_the_wedged_session() {
        let manifest = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: TESTCO, model: TM1, firmware: "1" }
modes:
  shooting/stills:
    detect: { prop: "0xdf01", eq: 22 }
    phase: liveView
properties:
  "0xdf01": { name: functionMode, type: u16, access: readWrite }
"#,
        )
        .unwrap();
        let mut e = Engine::new(manifest, empty_store());
        assert_ok_open(&mut e);
        e.on_operation(
            &req(op::SET_DEVICE_PROP_VALUE, 2, vec![0xdf01]),
            Some(&22u16.to_le_bytes()),
        );
        assert_eq!(e.state().phase, Phase::LiveView);

        e.transport_lost();
        assert!(!e.state().session_open);
        assert_eq!(e.state().phase, Phase::Closed);
        assert!(reply_is_ok(
            &e.on_operation(&req(op::OPEN_SESSION, 3, vec![1]), None)
        ));
    }

    /// #455 review: leaving the import workflow through a mode that declares
    /// no phase still resets the bootstrap gates — `state.phase` is sticky
    /// there, so keying the reset on the phase alone would skip it.
    #[test]
    fn leaving_import_mode_through_a_phaseless_mode_resets_gates() {
        let manifest = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: TESTCO, model: TM1, firmware: "1" }
modes:
  image-transfer:
    detect: { prop: "0xdf01", eq: 20 }
    phase: imageImport
  card-reader:
    detect: { prop: "0xdf01", eq: 23 }
properties:
  "0xdf01": { name: functionMode, type: u16, access: readWrite }
"#,
        )
        .unwrap();
        let mut e = Engine::new(manifest, empty_store());
        assert_ok_open(&mut e);
        e.on_operation(
            &req(op::SET_DEVICE_PROP_VALUE, 2, vec![0xdf01]),
            Some(&20u16.to_le_bytes()),
        );
        assert_eq!(e.state().phase, Phase::ImageImport);
        e.state.satisfy_gate("imageImportBootstrap");

        // Enter a mode with no declared phase: the phase stays sticky, but
        // the import workflow was left, so earned gates reset.
        e.on_operation(
            &req(op::SET_DEVICE_PROP_VALUE, 3, vec![0xdf01]),
            Some(&23u16.to_le_bytes()),
        );
        assert_eq!(e.state().phase, Phase::ImageImport, "phase is sticky");
        assert!(!e.state().gate_satisfied("imageImportBootstrap"));

        // Re-entering import does not reset: gates earned inside the
        // workflow survive.
        e.on_operation(
            &req(op::SET_DEVICE_PROP_VALUE, 4, vec![0xdf01]),
            Some(&20u16.to_le_bytes()),
        );
        e.state.satisfy_gate("imageImportBootstrap");
        e.on_operation(
            &req(op::SET_DEVICE_PROP_VALUE, 5, vec![0xdf01]),
            Some(&20u16.to_le_bytes()),
        );
        assert!(e.state().gate_satisfied("imageImportBootstrap"));
    }

    /// #407: computed properties are declared by the manifest and served from
    /// engine state — no property code is special in the engine.
    #[test]
    fn computed_properties_serve_object_state() {
        let manifest = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: TESTCO, model: TM1, firmware: "1" }
properties:
  "0xd620": { name: objectCount, type: u32, access: readOnly, computed: objectCount }
  "0xd621": { name: objectHandles, type: u8a, access: readOnly, computed: objectHandles }
"#,
        )
        .unwrap();
        let mut e = Engine::new(manifest, empty_store());
        assert_ok_open(&mut e);

        let Reply::Data { data, .. } =
            e.on_operation(&req(op::GET_DEVICE_PROP_VALUE, 2, vec![0xd620]), None)
        else {
            panic!("expected count data");
        };
        assert_eq!(u32::from_le_bytes(data.try_into().unwrap()), 0);

        let Reply::Data { data, .. } =
            e.on_operation(&req(op::GET_DEVICE_PROP_VALUE, 3, vec![0xd621]), None)
        else {
            panic!("expected handle-list data");
        };
        let mut r = Reader::new(&data);
        let handles = r.ptp_array(|r| r.u32()).unwrap();
        assert!(handles.is_empty(), "empty store enumerates nothing");

        // Computed properties are still readOnly on the wire.
        assert_eq!(
            response_code_of(e.on_operation(
                &req(op::SET_DEVICE_PROP_VALUE, 4, vec![0xd620]),
                Some(&1u32.to_le_bytes()),
            )),
            resp::ACCESS_DENIED
        );
    }

    fn assert_ok_open(e: &mut Engine) {
        assert!(reply_is_ok(
            &e.on_operation(&req(op::OPEN_SESSION, 1, vec![1]), None)
        ));
    }
}
