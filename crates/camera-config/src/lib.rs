//! `camera-config` — the manifest schema, loader, validation, compatibility
//! queries, and the canonical bundle→proposal generator.
//!
//! Manifests are the reviewed source of truth for camera behavior. `evidence:`
//! references are provenance only: a manifest loads and runs with them
//! unresolved (they become lints, never load errors), which is what lets the
//! public engine run manifests whose evidence lives in a private repo.

pub mod action;
pub mod activity;
pub mod error;
pub mod generate;
pub mod index;
pub mod model;
pub mod observation;
pub mod predicate;
pub mod query;
pub mod recorder;
pub mod std_names;
pub mod store;
pub mod trace;
pub mod version;

pub use action::{
    ActionArgument, ActionArgumentValue, ActionAvailability, ActionCatalog, ActionCatalogEntry,
    ActionCatalogParameter, ActionCatalogParameterKind, ActionInvocationRequest,
    ActionResolutionError, ActionRoleParameters, ResolvedActionInvocation,
};
pub use activity::{
    ConnectionActivityBinding, ConnectionActivityDescriptor, ConnectionActivityDisplayRole,
    ConnectionActivityExecutorSpan, ConnectionActivityHostCheckpoint,
    ConnectionActivityHostEstablishment, ConnectionActivitySequence, ExecutorSpanBinding,
    HostCheckpointBinding, HostEstablishmentBinding, NetworkIdentityExactBinding,
    RetainedSessionOpenBinding,
};
pub use error::{ConfigError, Lint, ManifestError, Severity};
pub use generate::{
    apply_review, proposal_json, propose, validate_bundles, validation_report_json,
    CandidateAssertion, GenerationError, Proposal, ProposalCandidate, ProposalRecordDisposition,
    ProposalRecordStatus, ProposalReview, RecordDisposition, ReviewDisposition,
    ValidatedObservations, ValidationReport, ValidationStatus, PROPOSAL_SCHEMA, REVIEW_SCHEMA,
    VALIDATION_REPORT_SCHEMA,
};
pub use model::{
    parse_hex_bytes, parse_hex_code, parse_hex_u32, Action, ActionEffect, ActionInitiator,
    ActionInitiatorParameter, ActionInitiatorParameterDeclaration, ActionInitiatorParameterKind,
    ActionParameter, ActionParameterKind, ActionResponder, ActionVerb, AvailableWhen, AwaitSource,
    AwaitUntil, BleLiteralWrite, BleStateTrigger, CameraIdentity, CameraInitiatedData,
    CameraInitiatedHandoff, CameraInitiatedMetadata, CameraInitiatedMetadataPhase,
    CameraInitiatedMonitorRecovery, CameraInitiatedReceive, CameraInitiatedTransfer,
    CameraInitiatedTrigger, CameraManifest, CaptureSource, CloseSession, ComputedValue, Connection,
    ConnectionTransition, Control, ControlOwner, ControlReadSource, ControlRole,
    ControlSurfaceEntry, Descriptor, DescriptorValue, GateFailure, GateRequirement, InitIdentity,
    InitRetries, InitShape, LiveViewDelivery, LiveViewDeliveryKind, LiveViewStream, Loop,
    ManufacturerDefaults, Media, MediaFormat, MissingRuntimeValue, Mode, ModeEntry,
    ModeEntryExecution, ObjectTransferCompletionPolicy, ObjectTransferCompletionTiming,
    ObjectTransferContract, ObjectTransferFormatSupport, ObjectTransferResumePolicy,
    ObjectTransferStrategy, ObjectsAvailable, ObservedScope, OpEffect, Operation, OperationHandler,
    OperationKind, Payload, PayloadForm, PcssDiscoveryTarget, PcssDiscoveryTargets, PcssKnock,
    PostviewEvent, Property, PropertyAccess, PropertyKind, PropertySemanticAssertions,
    PropertyTransitionTerminal, PropertyValueEncoding, PropertyValueProfile,
    PropertyValueProfileRow, PropertyValueRow, ProvenancedName, ProvenancedPropertyValueProfile,
    ProvenancedPropertyValueRow, RecordLayout, RecordMember, RecordMemberDetail, RecordMemberRef,
    RecordValueEncoding, RecordValueLiteral, ReestablishConnection, ResponderMutation,
    RetryFailureClass, RuntimeSetPropValue, SemanticAssertionLedger, SentinelFrame, SentinelMask,
    SequenceGate, SetPropValue, ShutterRecipe, SocketAvailability, SocketBinding,
    SocketBindingDescriptor, SocketBindings, SocketRole, Step, StepParam, StepRetry,
    StructuredTextField, StructuredTextLayout, StructuredTextScalar, TransferCompletion,
    TransportClose, TriggerMatch, ValuePolicy, ValueSource, VersionCond, WireFraming, Workflow,
};
pub use observation::*;
pub use predicate::{Leaf, Predicate, PropView};
pub use query::{Availability, Support};
pub use recorder::{
    direct_epistemic, no_loss, payload_metadata, ObservationRecorder, PayloadMetadataBuilder,
    RecorderError,
};
pub use store::{
    ConfigStore, ResolvedBleLiteralWrite, ResolvedBleStateTrigger, ResolvedCameraInitiatedTransfer,
};
pub use trace::{LeafEval, PredicateOutcome, ResolutionTrace};
pub use version::VersionScheme;

/// The manifest schema version this build understands.
pub const SCHEMA_VERSION: &str = "camera-config/v1";

/// Deep-merge two YAML values, `overlay` winning. Mappings merge recursively;
/// everything else (scalars, sequences) is replaced by `overlay`. The basis of
/// firmware-tier overlays — see [`CameraManifest::from_tiers`].
fn merge_yaml(base: serde_yaml::Value, overlay: serde_yaml::Value) -> serde_yaml::Value {
    use serde_yaml::Value::Mapping;
    match (base, overlay) {
        (Mapping(mut b), Mapping(o)) => {
            for (k, ov) in o {
                let merged = match b.remove(&k) {
                    Some(bv) => merge_yaml(bv, ov),
                    None => ov,
                };
                b.insert(k, merged);
            }
            Mapping(b)
        }
        (_, overlay) => overlay,
    }
}

impl CameraManifest {
    /// Parse a manifest from YAML text. Does not fail on unresolved evidence —
    /// call [`CameraManifest::validate`] for those lints.
    pub fn from_yaml(text: &str) -> Result<Self, ManifestError> {
        let m: CameraManifest = serde_yaml::from_str(text)?;
        m.require_valid_mode_entries()?;
        Ok(m)
    }

    pub fn from_file(path: impl AsRef<std::path::Path>) -> Result<Self, ManifestError> {
        Self::from_yaml(&std::fs::read_to_string(path)?)
    }

    /// Load a body manifest with firmware-tier overlays deep-merged on top, most-
    /// specific last (`from_tiers(body, &[fw240])`). The merge is **field-level**
    /// at the YAML level — an overlay overrides just the keys it names (e.g. only
    /// `connections.xlv.transport`), so a fw delta need not restate whole records.
    /// Maps merge recursively; scalars and sequences are replaced by the overlay.
    pub fn from_tiers(base_yaml: &str, overlays: &[&str]) -> Result<Self, ManifestError> {
        let mut merged: serde_yaml::Value = serde_yaml::from_str(base_yaml)?;
        for ov in overlays {
            let ov: serde_yaml::Value = serde_yaml::from_str(ov)?;
            merged = merge_yaml(merged, ov);
        }
        let manifest: CameraManifest = serde_yaml::from_value(merged)?;
        manifest.require_valid_mode_entries()?;
        Ok(manifest)
    }

    /// Serialize back to YAML (used by the generator to write proposals).
    pub fn to_yaml(&self) -> Result<String, ManifestError> {
        Ok(serde_yaml::to_string(self)?)
    }

    /// Structural lints. Currently: schema mismatch is surfaced as an error via
    /// [`CameraManifest::require_supported_schema`]; everything here is a
    /// non-fatal warning, notably evidence ids referenced but not defined.
    pub fn validate(&self) -> Vec<Lint> {
        let mut lints = Vec::new();
        let defined: std::collections::BTreeSet<&str> =
            self.evidence.keys().map(|s| s.as_str()).collect();

        let check = |ids: &[String], ctx: &str, lints: &mut Vec<Lint>| {
            for id in ids {
                if !defined.contains(id.as_str()) {
                    lints.push(Lint::warn(format!(
                        "{ctx} references evidence id '{id}' which is not defined in this manifest \
                         (provenance may live in a private repo)"
                    )));
                }
            }
        };
        let defined_gates: std::collections::BTreeSet<&str> =
            self.sequence_gates.keys().map(|s| s.as_str()).collect();

        if let Some(transfer) = &self.camera_initiated_transfer {
            let ctx = "cameraInitiatedTransfer";
            check(&transfer.evidence, ctx, &mut lints);

            let connection = self.connections.get(&transfer.handoff.connection);
            if connection.is_none() {
                lints.push(Lint::warn(format!(
                    "{ctx} references unknown connection '{}'",
                    transfer.handoff.connection
                )));
            }
            if !self.modes.contains_key(&transfer.receive.mode) {
                lints.push(Lint::warn(format!(
                    "{ctx} references unknown receive mode '{}'",
                    transfer.receive.mode
                )));
            } else if connection.is_some_and(|c| !c.modes.contains(&transfer.receive.mode)) {
                lints.push(Lint::warn(format!(
                    "{ctx} receive mode '{}' is not available on connection '{}'",
                    transfer.receive.mode, transfer.handoff.connection
                )));
            }
            if connection
                .and_then(|c| c.bindings.as_ref())
                .and_then(|b| b.port_for(transfer.handoff.socket_role))
                .is_none()
            {
                lints.push(Lint::warn(format!(
                    "{ctx} socket role '{:?}' is not bound on connection '{}'",
                    transfer.handoff.socket_role, transfer.handoff.connection
                )));
            }

            if transfer.receive.head_index == 0 {
                lints.push(Lint::warn(format!("{ctx} headIndex must be non-zero")));
            }
            if transfer.receive.metadata.phases.is_empty() {
                lints.push(Lint::warn(format!(
                    "{ctx} metadata must declare at least one phase"
                )));
            }
            let unique_metadata_phases: std::collections::BTreeSet<_> =
                transfer.receive.metadata.phases.iter().collect();
            if unique_metadata_phases.len() != transfer.receive.metadata.phases.len() {
                lints.push(Lint::warn(format!(
                    "{ctx} metadata phases must not contain duplicates"
                )));
            }
            validate_transfer_hex_values(transfer, ctx, &mut lints);

            let count_property = parse_hex_code(&transfer.receive.count.property);
            let count_member = parse_hex_code(&transfer.receive.count.member);
            match count_property.and_then(|code| self.property(code)) {
                Some(prop) => {
                    let contains_member = count_member.is_some_and(|member| {
                        prop.payload.as_ref().is_some_and(|payload| {
                            payload
                                .members
                                .iter()
                                .any(|candidate| parse_hex_code(candidate.code()) == Some(member))
                        })
                    });
                    if !contains_member {
                        lints.push(Lint::warn(format!(
                            "{ctx} count member '{}' is absent from property '{}' record stream",
                            transfer.receive.count.member, transfer.receive.count.property
                        )));
                    }
                }
                None => lints.push(Lint::warn(format!(
                    "{ctx} references unknown count property '{}'",
                    transfer.receive.count.property
                ))),
            }
            if count_member.and_then(|code| self.property(code)).is_none() {
                lints.push(Lint::warn(format!(
                    "{ctx} references unknown count member property '{}'",
                    transfer.receive.count.member
                )));
            }
            for (label, code) in [
                ("metadata operation", &transfer.receive.metadata.operation),
                ("data operation", &transfer.receive.data.operation),
            ] {
                if parse_hex_code(code)
                    .and_then(|code| self.operation(code))
                    .is_none()
                {
                    lints.push(Lint::warn(format!(
                        "{ctx} references unknown {label} '{code}'"
                    )));
                }
            }
            if parse_hex_code(&transfer.receive.data.chunk_limit_property)
                .and_then(|code| self.property(code))
                .is_none()
            {
                lints.push(Lint::warn(format!(
                    "{ctx} references unknown chunk-limit property '{}'",
                    transfer.receive.data.chunk_limit_property
                )));
            }
        }

        for (code, op) in &self.operations {
            check(&op.evidence, &format!("operation {code}"), &mut lints);
            check_gate_requirement(
                op.requires_gate.as_ref(),
                &format!("operation {code}"),
                &defined_gates,
                &mut lints,
            );
        }
        for (id, gate) in &self.sequence_gates {
            check(&gate.evidence, &format!("sequenceGate {id}"), &mut lints);
        }
        for (id, sentinel) in &self.sentinels {
            check(&sentinel.evidence, &format!("sentinel {id}"), &mut lints);
            if parse_hex_bytes(&sentinel.bytes).is_none() {
                lints.push(Lint::warn(format!(
                    "sentinel {id} has invalid hex bytes; transport-close resolution will fail"
                )));
            }
        }
        for (code, prop) in &self.properties {
            check(&prop.evidence, &format!("property {code}"), &mut lints);
            check_gate_requirement(
                prop.requires_gate.as_ref(),
                &format!("property {code}"),
                &defined_gates,
                &mut lints,
            );
            if let Some(payload) = &prop.payload {
                for m in &payload.members {
                    let member = m.code();
                    if matches!(m, RecordMember::Code(_)) && !self.properties.contains_key(member) {
                        lints.push(Lint::warn(format!(
                            "property {code} payload member '{member}' is not a defined property; \
                             use a detailed member encoding when the global type is unavailable"
                        )));
                    }
                }
            }
        }
        for (id, wf) in &self.workflows {
            check(&wf.evidence, &format!("workflow {id}"), &mut lints);
        }
        for (id, conn) in &self.connections {
            if let Some(init) = &conn.init {
                check(&init.evidence, &format!("connection {id} init"), &mut lints);
            }
            if let Some(tc) = &conn.transport_close {
                if !self.sentinels.contains_key(&tc.sentinel) {
                    lints.push(Lint::warn(format!(
                        "connection {id} transportClose references sentinel '{}' which is not \
                         defined in this manifest",
                        tc.sentinel
                    )));
                }
            }
            for (i, entry) in conn.entries.iter().enumerate() {
                let ctx = format!("connection {id} entry {i}");
                match &entry.execution {
                    ModeEntryExecution::Ptp { steps } => {
                        check_gate_steps(steps, &ctx, &defined_gates, &mut lints);
                    }
                    ModeEntryExecution::ReestablishConnection(reestablish) => {
                        check_gate_steps(
                            &reestablish.exit_steps,
                            &format!("{ctx} exit"),
                            &defined_gates,
                            &mut lints,
                        );
                    }
                    ModeEntryExecution::UserInstruction { .. } => {}
                }
            }
            for (verb, action) in &conn.actions {
                if let Some(initiator) = &action.initiator {
                    check_gate_steps(
                        &initiator.steps,
                        &format!("connection {id} action {verb:?}"),
                        &defined_gates,
                        &mut lints,
                    );
                }
            }
        }
        lints
    }

    /// Fail closed on destructive outer transitions. A consumer must be able to
    /// establish the connection and resolve a non-recursive cold PTP entry before
    /// it executes the old session's exit steps.
    pub fn require_valid_mode_entries(&self) -> Result<(), ManifestError> {
        let mut activity_metadata = std::collections::BTreeMap::new();
        for code in self.properties.keys() {
            if parse_hex_code(code).is_none() {
                return Err(ManifestError::Contract(format!(
                    "properties map key '{code}' is not a hex property code"
                )));
            }
        }
        for code in self.operations.keys() {
            if parse_hex_code(code).is_none() {
                return Err(ManifestError::Contract(format!(
                    "operations map key '{code}' is not a hex operation code"
                )));
            }
        }
        for (code, operation) in &self.operations {
            require_valid_operation_handler(operation, code)?;
        }
        for (code, property) in &self.properties {
            require_valid_descriptor(property, code)?;
            require_valid_structured_text(property, code)?;
            require_valid_payload(self, property, code)?;
        }
        for (connection_id, connection) in &self.connections {
            require_valid_host_activities(connection, connection_id)?;
            require_valid_socket_bindings(self, connection, connection_id)?;
            require_valid_init_shape(connection, connection_id)?;
            require_valid_connection_transitions(self, connection, connection_id)?;
            require_valid_pcss_rendezvous(connection, connection_id)?;
            require_valid_object_transfer(self, connection, connection_id)?;
            require_valid_control_surfaces(self, connection, connection_id)?;
            for descriptor in &connection.activities {
                let key = (descriptor.id.clone(), descriptor.version);
                let value = (
                    descriptor.display_role.clone(),
                    descriptor.default_expected_duration_ms,
                    descriptor.interaction_required,
                    descriptor.optional,
                    descriptor.identity(),
                );
                if let Some(previous) = activity_metadata.insert(key, value.clone()) {
                    if previous != value {
                        return Err(ManifestError::Contract(format!(
                            "connections.{connection_id}.activities activity '{}@{}' metadata differs or binding identity differs from another descriptor",
                            descriptor.id, descriptor.version
                        )));
                    }
                }
            }
            for (index, entry) in connection.entries.iter().enumerate() {
                let path = format!("connections.{connection_id}.entries[{index}]");
                match &entry.execution {
                    ModeEntryExecution::Ptp { steps } => {
                        require_valid_ptp_steps(steps, &path)?;
                        require_no_runtime_set_props(steps, &path)?;
                        require_valid_open_channels(
                            steps,
                            connection.bindings.as_ref(),
                            &path,
                            true,
                        )?;
                        require_valid_executor_activities(
                            &entry.activities,
                            steps.len(),
                            &format!("{path}.activities"),
                        )?;
                    }
                    ModeEntryExecution::ReestablishConnection(reestablish) => {
                        require_valid_ptp_steps(
                            &reestablish.exit_steps,
                            &format!("{path}.reestablishConnection.exitSteps"),
                        )?;
                        require_no_runtime_set_props(
                            &reestablish.exit_steps,
                            &format!("{path}.reestablishConnection.exitSteps"),
                        )?;
                        require_valid_open_channels(
                            &reestablish.exit_steps,
                            connection.bindings.as_ref(),
                            &format!("{path}.reestablishConnection.exitSteps"),
                            false,
                        )?;
                        require_valid_executor_activities(
                            &entry.activities,
                            reestablish.exit_steps.len(),
                            &format!("{path}.activities"),
                        )?;
                    }
                    ModeEntryExecution::UserInstruction { .. } => {
                        if !entry.activities.is_empty() {
                            return Err(ManifestError::Contract(format!(
                                "{path}.activities cannot target a userInstruction"
                            )));
                        }
                    }
                }
                for descriptor in &entry.activities {
                    require_consistent_activity_metadata(
                        &mut activity_metadata,
                        descriptor,
                        &format!("{path}.activities"),
                    )?;
                }
                if !matches!(
                    entry.execution,
                    ModeEntryExecution::ReestablishConnection(_)
                ) {
                    continue;
                }
                if connection.establishment.is_none() {
                    return Err(ManifestError::Contract(format!(
                        "{path} reestablishes a connection with no establishment mechanism"
                    )));
                }
                let cold = connection
                    .entries
                    .iter()
                    .find(|candidate| candidate.to == entry.to && candidate.from.is_none());
                if !matches!(
                    cold.map(|candidate| &candidate.execution),
                    Some(ModeEntryExecution::Ptp { .. })
                ) {
                    return Err(ManifestError::Contract(format!(
                        "{path} requires a non-recursive cold PTP entry for '{}'",
                        entry.to
                    )));
                }
            }
            for (verb, action) in &connection.actions {
                let path = format!("connections.{connection_id}.actions.{verb:?}");
                require_valid_action_roles(self, action, &path)?;
                if let Some(initiator) = &action.initiator {
                    require_valid_ptp_steps(&initiator.steps, &path)?;
                    require_valid_action_runtime_values(self, initiator, &path)?;
                    require_valid_open_channels(
                        &initiator.steps,
                        connection.bindings.as_ref(),
                        &path,
                        true,
                    )?;
                    require_valid_executor_activities(
                        &initiator.activities,
                        initiator.steps.len(),
                        &format!("{path}.initiator.activities"),
                    )?;
                    for descriptor in &initiator.activities {
                        require_consistent_activity_metadata(
                            &mut activity_metadata,
                            descriptor,
                            &format!("{path}.initiator.activities"),
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Returns an error if the manifest's schema is not understood by this build.
    pub fn require_supported_schema(&self) -> Result<(), ManifestError> {
        if self.schema != SCHEMA_VERSION {
            return Err(ManifestError::Schema {
                found: self.schema.clone(),
                expected: SCHEMA_VERSION.to_string(),
            });
        }
        Ok(())
    }
}

fn require_valid_operation_handler(operation: &Operation, code: &str) -> Result<(), ManifestError> {
    match operation.handler {
        Some(OperationHandler::PropertyStep) => {
            let property = operation.property.as_deref().ok_or_else(|| {
                ManifestError::Contract(format!(
                    "operation '{code}' handler 'property.step' requires a 'property' code"
                ))
            })?;
            if parse_hex_code(property).is_none() {
                return Err(ManifestError::Contract(format!(
                    "operation '{code}' handler 'property.step' property '{property}' is not a hex property code"
                )));
            }
        }
        Some(OperationHandler::ObjectSize) => {
            if operation.object_size.is_none() {
                return Err(ManifestError::Contract(format!(
                    "operation '{code}' handler 'object.size' requires an 'objectSize' block"
                )));
            }
        }
        None => {
            if operation.object_size.is_some() {
                return Err(ManifestError::Contract(format!(
                    "operation '{code}' declares 'objectSize' without handler 'object.size'"
                )));
            }
        }
    }
    Ok(())
}

fn require_valid_descriptor(property: &Property, code: &str) -> Result<(), ManifestError> {
    let is_string = property.ptype.as_deref() == Some("str");
    match (is_string, &property.initial_value) {
        (true, Some(DescriptorValue::Int(_))) => {
            return Err(ManifestError::Contract(format!(
                "properties.{code}.initialValue must be a quoted string for a property with type str"
            )));
        }
        (false, Some(DescriptorValue::Str(_))) => {
            return Err(ManifestError::Contract(format!(
                "properties.{code}.initialValue must be an integer for a property with a numeric type"
            )));
        }
        _ => {}
    }
    let Some(descriptor) = &property.descriptor else {
        return Ok(());
    };
    let path = format!("properties.{code}.descriptor.values");
    if descriptor.form == "range"
        && descriptor
            .values
            .iter()
            .any(|value| matches!(value, DescriptorValue::Str(_)))
    {
        return Err(ManifestError::Contract(format!(
            "{path} contains a string value but form range requires integer values"
        )));
    }
    if descriptor
        .values
        .iter()
        .any(|value| matches!(value, DescriptorValue::Str(_)))
        && !is_string
    {
        return Err(ManifestError::Contract(format!(
            "{path} contains a string value but property {code} does not have type str"
        )));
    }
    if descriptor
        .values
        .iter()
        .any(|value| matches!(value, DescriptorValue::Int(_)))
        && is_string
    {
        return Err(ManifestError::Contract(format!(
            "{path} contains an integer value but property {code} has type str; string enum values must be quoted in YAML"
        )));
    }
    Ok(())
}

fn require_valid_structured_text(property: &Property, code: &str) -> Result<(), ManifestError> {
    let Some(layout) = &property.structured_text else {
        return Ok(());
    };
    let path = format!("properties.{code}.structuredText");
    if property.ptype.as_deref() != Some("str") {
        return Err(ManifestError::Contract(format!(
            "{path} is valid only for a property with type str"
        )));
    }
    if layout.delimiter.is_empty() || layout.fields.is_empty() {
        return Err(ManifestError::Contract(format!(
            "{path} requires a non-empty delimiter and at least one field"
        )));
    }
    let mut names = std::collections::BTreeSet::new();
    if layout
        .fields
        .iter()
        .any(|field| field.name.trim().is_empty() || !names.insert(&field.name))
    {
        return Err(ManifestError::Contract(format!(
            "{path}.fields must have unique, non-empty names"
        )));
    }
    Ok(())
}

fn require_valid_payload(
    manifest: &CameraManifest,
    property: &Property,
    code: &str,
) -> Result<(), ManifestError> {
    let Some(payload) = &property.payload else {
        return Ok(());
    };
    let path = format!("properties.{code}.payload");
    let (count_width, code_width, default_value_width) = payload.record_widths();
    if !matches!(count_width, 1 | 2 | 4)
        || !matches!(code_width, 1 | 2)
        || !matches!(default_value_width, 1 | 2 | 4)
    {
        return Err(ManifestError::Contract(format!(
            "{path} uses unsupported widths count={count_width} code={code_width} value={default_value_width}"
        )));
    }

    let mut seen = std::collections::BTreeSet::new();
    for (index, member) in payload.members.iter().enumerate() {
        let member_path = format!("{path}.members[{index}]");
        let member_code = parse_hex_code(member.code()).ok_or_else(|| {
            ManifestError::Contract(format!(
                "{member_path} has invalid property code '{}'",
                member.code()
            ))
        })?;
        if code_width == 1 && member_code > u8::MAX as u16 {
            return Err(ManifestError::Contract(format!(
                "{member_path} code {member_code:#06x} does not fit codeWidth 1"
            )));
        }
        if !seen.insert(member_code) {
            return Err(ManifestError::Contract(format!(
                "{path} repeats member code {member_code:#06x}"
            )));
        }

        let encoding = member.encoding(default_value_width);
        let simulator_value = member.simulator_value();
        match encoding {
            RecordValueEncoding::Fixed { width } => {
                if !matches!(width, 1 | 2 | 4) {
                    return Err(ManifestError::Contract(format!(
                        "{member_path} uses unsupported fixed width {width}"
                    )));
                }
                if let Some(value) = simulator_value {
                    let RecordValueLiteral::Unsigned(value) = value else {
                        return Err(ManifestError::Contract(format!(
                            "{member_path}.simulatorValue must be unsigned for a fixed encoding"
                        )));
                    };
                    let max = match width {
                        1 => u8::MAX as u32,
                        2 => u16::MAX as u32,
                        _ => u32::MAX,
                    };
                    if *value > max {
                        return Err(ManifestError::Contract(format!(
                            "{member_path}.simulatorValue {value} does not fit width {width}"
                        )));
                    }
                }
            }
            RecordValueEncoding::Signed { width } => {
                if !matches!(width, 1 | 2 | 4) {
                    return Err(ManifestError::Contract(format!(
                        "{member_path} uses unsupported signed width {width}"
                    )));
                }
                if let Some(value) = simulator_value {
                    let value = match value {
                        RecordValueLiteral::Unsigned(value) => i64::from(*value),
                        RecordValueLiteral::Signed(value) => i64::from(*value),
                        RecordValueLiteral::String(_) => {
                            return Err(ManifestError::Contract(format!(
                                "{member_path}.simulatorValue must be numeric for a signed encoding"
                            )));
                        }
                    };
                    let bits = u32::from(width) * 8;
                    let min = -(1_i64 << (bits - 1));
                    let max = (1_i64 << (bits - 1)) - 1;
                    if !(min..=max).contains(&value) {
                        return Err(ManifestError::Contract(format!(
                            "{member_path}.simulatorValue {value} does not fit signed width {width}"
                        )));
                    }
                }
            }
            RecordValueEncoding::PtpString => {
                if simulator_value
                    .is_some_and(|value| !matches!(value, RecordValueLiteral::String(_)))
                {
                    return Err(ManifestError::Contract(format!(
                        "{member_path}.simulatorValue must be a string for ptpString"
                    )));
                }
                let mutable_string_state = manifest
                    .property(member_code)
                    .is_some_and(|candidate| candidate.ptype.as_deref() == Some("str"));
                if simulator_value.is_none() && !mutable_string_state {
                    return Err(ManifestError::Contract(format!(
                        "{member_path} ptpString requires simulatorValue when the global property is not type str"
                    )));
                }
            }
        }
    }
    Ok(())
}

fn require_valid_init_shape(
    connection: &Connection,
    connection_id: &str,
) -> Result<(), ManifestError> {
    if connection.init_shape.as_deref() == Some("standardPtpIp") {
        let path = format!("connections.{connection_id}.init");
        let init = connection.init.as_ref().ok_or_else(|| {
            ManifestError::Contract(format!("{path} is required for initShape standardPtpIp"))
        })?;
        if init.identity.guid.trim().is_empty() || init.identity.friendly_name.trim().is_empty() {
            return Err(ManifestError::Contract(format!(
                "{path}.identity requires non-empty guid and friendlyName value references"
            )));
        }
        if init.identity.client_ipv4.is_some() || init.expected_responder_guid.is_some() {
            return Err(ManifestError::Contract(format!(
                "{path} standardPtpIp does not use clientIpv4 or expectedResponderGuid"
            )));
        }
        if init.name_field_byte_count != 0 {
            return Err(ManifestError::Contract(format!(
                "{path}.nameFieldByteCount must be omitted for initShape standardPtpIp"
            )));
        }
        if connection.command_framing != Some(WireFraming::Standard) {
            return Err(ManifestError::Contract(format!(
                "connections.{connection_id}.commandFraming must be standard for initShape standardPtpIp"
            )));
        }
        if connection
            .bindings
            .as_ref()
            .and_then(|bindings| bindings.port_for(SocketRole::Event))
            .is_none()
        {
            return Err(ManifestError::Contract(format!(
                "connections.{connection_id}.bindings.event is required for initShape standardPtpIp"
            )));
        }
        if connection.event_framing != Some(WireFraming::Standard) {
            return Err(ManifestError::Contract(format!(
                "connections.{connection_id}.eventFraming must be standard for initShape standardPtpIp"
            )));
        }
        return Ok(());
    }
    if connection.init_shape.as_deref() == Some("app82") {
        let Some(init) = connection.init.as_ref() else {
            // Responder-only synthetic manifests can identify the parser shape
            // without declaring initiator-side identity policy.
            return Ok(());
        };
        let path = format!("connections.{connection_id}.init");
        if init.identity.guid.trim().is_empty() || init.identity.friendly_name.trim().is_empty() {
            return Err(ManifestError::Contract(format!(
                "{path}.identity requires non-empty guid and friendlyName value references"
            )));
        }
        if init.identity.client_ipv4.is_some() || init.expected_responder_guid.is_some() {
            return Err(ManifestError::Contract(format!(
                "{path} app82 does not use clientIpv4 or expectedResponderGuid"
            )));
        }
        if init.name_field_byte_count != 54 {
            return Err(ManifestError::Contract(format!(
                "{path}.nameFieldByteCount must be 54 for initShape app82"
            )));
        }
        return Ok(());
    }
    if connection.init_shape.as_deref() != Some("legacyApp82") {
        return Ok(());
    }
    let path = format!("connections.{connection_id}.init");
    let init = connection.init.as_ref().ok_or_else(|| {
        ManifestError::Contract(format!("{path} is required for initShape legacyApp82"))
    })?;
    if init.identity.guid.trim().is_empty() || init.identity.friendly_name.trim().is_empty() {
        return Err(ManifestError::Contract(format!(
            "{path}.identity requires non-empty guid and friendlyName value references"
        )));
    }
    if init
        .identity
        .client_ipv4
        .as_deref()
        .is_none_or(|value| value.trim().is_empty())
    {
        return Err(ManifestError::Contract(format!(
            "{path}.identity.clientIpv4 is required for initShape legacyApp82"
        )));
    }
    if init.name_field_byte_count != 54 {
        return Err(ManifestError::Contract(format!(
            "{path}.nameFieldByteCount must be 54 for initShape legacyApp82"
        )));
    }
    init.expected_responder_guid
        .as_deref()
        .filter(|key| !key.trim().is_empty())
        .ok_or_else(|| {
            ManifestError::Contract(format!(
                "{path}.expectedResponderGuid is required for initShape legacyApp82"
            ))
        })?;
    if connection.command_framing != Some(WireFraming::Usb) {
        return Err(ManifestError::Contract(format!(
            "connections.{connection_id}.commandFraming must be usb for initShape legacyApp82"
        )));
    }
    Ok(())
}

fn require_valid_socket_bindings(
    manifest: &CameraManifest,
    connection: &Connection,
    connection_id: &str,
) -> Result<(), ManifestError> {
    let Some(bindings) = connection.bindings.as_ref() else {
        return Ok(());
    };
    let command_port = bindings.command.port();
    for role in [SocketRole::Command, SocketRole::Event, SocketRole::LiveView] {
        let Some(binding) = bindings.binding_for(role) else {
            continue;
        };
        let Some(availability) = binding.available_after() else {
            continue;
        };
        let operation = &availability.operation;
        let role_name = match role {
            SocketRole::Command => "command",
            SocketRole::Event => "event",
            SocketRole::LiveView => "liveView",
        };
        let path =
            format!("connections.{connection_id}.bindings.{role_name}.availableAfter.operation");
        if role == SocketRole::Command {
            return Err(ManifestError::Contract(format!(
                "{path} is valid only for event or live-view bindings"
            )));
        }
        if binding.port() == command_port {
            return Err(ManifestError::Contract(format!(
                "{path} cannot gate a listener shared with the command binding"
            )));
        }
        let code = parse_hex_code(operation).ok_or_else(|| {
            ManifestError::Contract(format!("{path} has invalid operation code '{operation}'"))
        })?;
        let declared = manifest.operation(code).ok_or_else(|| {
            ManifestError::Contract(format!(
                "{path} references operation '{operation}' which is not defined in this manifest"
            ))
        })?;
        if !declared.connections.is_empty()
            && !declared
                .connections
                .iter()
                .any(|candidate| candidate == connection_id)
        {
            return Err(ManifestError::Contract(format!(
                "{path} references operation '{operation}' which is unavailable on connection '{connection_id}'"
            )));
        }
    }
    Ok(())
}

fn require_valid_connection_transitions(
    manifest: &CameraManifest,
    connection: &Connection,
    connection_id: &str,
) -> Result<(), ManifestError> {
    let mut seen = std::collections::BTreeSet::new();
    for (index, transition) in connection.enables.iter().enumerate() {
        let path = format!("connections.{connection_id}.enables[{index}]");
        let target = manifest.connections.get(&transition.to).ok_or_else(|| {
            ManifestError::Contract(format!(
                "{path}.to references unknown connection '{}'",
                transition.to
            ))
        })?;
        if transition.mechanism.is_none() && transition.user_instruction.is_none() {
            return Err(ManifestError::Contract(format!(
                "{path} requires mechanism or userInstruction"
            )));
        }
        if !transition.params.is_empty() && transition.mechanism.is_none() {
            return Err(ManifestError::Contract(format!(
                "{path}.params require a mechanism-backed establishment edge"
            )));
        }
        if let Some(mechanism) = &transition.mechanism {
            if target.establishment.as_ref() != Some(mechanism) {
                return Err(ManifestError::Contract(format!(
                    "{path}.mechanism '{mechanism}' does not match target connection '{}' establishment {:?}",
                    transition.to, target.establishment
                )));
            }
        }
        if let Some(mode) = &transition.mode {
            if !target.modes.contains(mode) {
                return Err(ManifestError::Contract(format!(
                    "{path}.mode '{mode}' is not declared by target connection '{}'",
                    transition.to
                )));
            }
        }
        if transition.params.keys().any(|key| key.trim().is_empty()) {
            return Err(ManifestError::Contract(format!(
                "{path}.params keys must not be empty"
            )));
        }
        if !seen.insert((transition.to.as_str(), transition.mode.as_deref())) {
            return Err(ManifestError::Contract(format!(
                "connections.{connection_id}.enables contains a duplicate target/mode edge to '{}' ({:?})",
                transition.to, transition.mode
            )));
        }
    }
    Ok(())
}

fn require_valid_pcss_rendezvous(
    connection: &Connection,
    connection_id: &str,
) -> Result<(), ManifestError> {
    require_valid_init_retries(connection, connection_id)?;
    let Some(knock) = &connection.knock else {
        return Ok(());
    };
    let path = format!("connections.{connection_id}.knock");
    if knock.callback_port == 0 || knock.knock_port == 0 {
        return Err(ManifestError::Contract(format!(
            "{path} callbackPort and knockPort must be non-zero"
        )));
    }
    if knock.protocol.trim().is_empty() {
        return Err(ManifestError::Contract(format!(
            "{path}.protocol must not be empty"
        )));
    }
    if knock.retry_interval_ms == 0 || knock.max_attempts == 0 || knock.connect_timeout_ms == 0 {
        return Err(ManifestError::Contract(format!(
            "{path} retryIntervalMs, maxAttempts, and connectTimeoutMs must be non-zero"
        )));
    }
    let targets = &knock.discovery_targets;
    if targets.supported.is_empty() {
        return Err(ManifestError::Contract(format!(
            "{path}.discoveryTargets.supported must not be empty"
        )));
    }
    if !targets.supported.contains(&targets.default) {
        return Err(ManifestError::Contract(format!(
            "{path}.discoveryTargets.default must be listed in supported"
        )));
    }
    if targets
        .supported
        .iter()
        .enumerate()
        .any(|(index, target)| targets.supported[..index].contains(target))
    {
        return Err(ManifestError::Contract(format!(
            "{path}.discoveryTargets.supported must not contain duplicates"
        )));
    }
    if targets.retry_discovered_unicast
        && (!targets
            .supported
            .contains(&PcssDiscoveryTarget::SubnetBroadcast)
            || !targets
                .supported
                .contains(&PcssDiscoveryTarget::ExplicitUnicast))
    {
        return Err(ManifestError::Contract(format!(
            "{path}.discoveryTargets.retryDiscoveredUnicast requires subnetBroadcast and explicitUnicast support"
        )));
    }
    Ok(())
}

fn require_valid_init_retries(
    connection: &Connection,
    connection_id: &str,
) -> Result<(), ManifestError> {
    let Some(retries) = &connection.init_retries else {
        return Ok(());
    };
    let retry_path = format!("connections.{connection_id}.initRetries");
    let parsed_reasons: Option<Vec<u32>> = retries
        .when_reasons
        .iter()
        .map(|reason| parse_hex_u32(reason))
        .collect();
    let Some(parsed_reasons) = parsed_reasons else {
        return Err(ManifestError::Contract(format!(
            "{retry_path}.whenReasons entries must be 32-bit hexadecimal codes"
        )));
    };
    let incoherent = if retries.max == 0 {
        retries.backoff_ms != 0 || !retries.when_reasons.is_empty()
    } else {
        retries.backoff_ms == 0 || retries.when_reasons.is_empty()
    };
    if incoherent {
        return Err(ManifestError::Contract(format!(
            "{retry_path} requires non-empty whenReasons and non-zero backoffMs exactly when max is non-zero"
        )));
    }
    let reasons: std::collections::BTreeSet<_> = parsed_reasons.iter().collect();
    if reasons.len() != parsed_reasons.len() {
        return Err(ManifestError::Contract(format!(
            "{retry_path}.whenReasons must not contain duplicates"
        )));
    }
    Ok(())
}

fn require_valid_action_roles(
    manifest: &CameraManifest,
    action: &Action,
    path: &str,
) -> Result<(), ManifestError> {
    if action.initiator.is_none() && action.responder.is_none() {
        return Err(ManifestError::Contract(format!(
            "{path} must declare initiator or responder"
        )));
    }
    if let Some(initiator) = &action.initiator {
        let mut names = std::collections::BTreeSet::new();
        if initiator.params.iter().any(|parameter| {
            let name = parameter.name();
            name.is_empty() || !names.insert(name)
        }) {
            return Err(ManifestError::Contract(format!(
                "{path}.initiator.params names must be non-empty and unique"
            )));
        }
    }
    for (index, trigger) in action.triggers.iter().enumerate() {
        if !trigger.is_well_formed() {
            return Err(ManifestError::Contract(format!(
                "{path}.triggers[{index}] must contain exactly one trigger"
            )));
        }
        if trigger
            .objects_available
            .is_some_and(|range| range.min > range.max)
        {
            return Err(ManifestError::Contract(format!(
                "{path}.triggers[{index}].objectsAvailable min exceeds max"
            )));
        }
    }
    let Some(responder) = &action.responder else {
        return Ok(());
    };
    let mut names = std::collections::BTreeSet::new();
    for parameter in &responder.params {
        if parameter.name.is_empty() || !names.insert(parameter.name.as_str()) {
            return Err(ManifestError::Contract(format!(
                "{path}.responder.params names must be non-empty and unique"
            )));
        }
        if parameter
            .min
            .zip(parameter.max)
            .is_some_and(|(min, max)| min > max)
        {
            return Err(ManifestError::Contract(format!(
                "{path}.responder parameter '{}' has min greater than max",
                parameter.name
            )));
        }
        if parameter.default.is_some_and(|default| {
            parameter.min.is_some_and(|min| default < min)
                || parameter.max.is_some_and(|max| default > max)
        }) {
            return Err(ManifestError::Contract(format!(
                "{path}.responder parameter '{}' default is out of range",
                parameter.name
            )));
        }
    }
    match &responder.mutation {
        ResponderMutation::EnqueueObjects { count_param } => {
            let parameter = responder
                .params
                .iter()
                .find(|parameter| parameter.name == *count_param)
                .ok_or_else(|| {
                    ManifestError::Contract(format!(
                        "{path}.responder enqueueObjects references unknown countParam '{count_param}'"
                    ))
                })?;
            let range = action
                .triggers
                .iter()
                .find_map(|effect| effect.objects_available)
                .ok_or_else(|| {
                    ManifestError::Contract(format!(
                        "{path}.responder enqueueObjects requires an objectsAvailable trigger"
                    ))
                })?;
            if parameter.min != Some(range.min) || parameter.max != Some(range.max) {
                return Err(ManifestError::Contract(format!(
                    "{path}.responder count parameter must use the objectsAvailable range"
                )));
            }
        }
        ResponderMutation::PropertyTransition {
            target,
            initial,
            terminal,
            ..
        } => {
            let code = parse_hex_code(target).ok_or_else(|| {
                ManifestError::Contract(format!(
                    "{path}.responder propertyTransition target '{target}' is not a property code"
                ))
            })?;
            let property = manifest.property(code).ok_or_else(|| {
                ManifestError::Contract(format!(
                    "{path}.responder propertyTransition references unknown target '{target}'"
                ))
            })?;
            if property.payload.is_some()
                || numeric_scalar_bounds(property.ptype.as_deref()).is_none()
            {
                return Err(ManifestError::Contract(format!(
                    "{path}.responder propertyTransition target '{target}' must be a numeric scalar property"
                )));
            }
            if initial.is_some_and(|value| !property_scalar_fits(property, value)) {
                return Err(ManifestError::Contract(format!(
                    "{path}.responder propertyTransition initial value is outside target '{target}' encoding"
                )));
            }
            match terminal {
                PropertyTransitionTerminal::Fixed { value } => {
                    if !property_scalar_fits(property, *value) {
                        return Err(ManifestError::Contract(format!(
                            "{path}.responder propertyTransition terminal value is outside target '{target}' encoding"
                        )));
                    }
                }
                PropertyTransitionTerminal::Parameter { parameter } => {
                    let declaration = responder
                        .params
                        .iter()
                        .find(|candidate| candidate.name == *parameter)
                        .ok_or_else(|| {
                            ManifestError::Contract(format!(
                                "{path}.responder propertyTransition references unknown terminal parameter '{parameter}'"
                            ))
                        })?;
                    let (min, max) = match declaration.kind {
                        ActionParameterKind::U32 => (
                            i128::from(declaration.min.unwrap_or(u32::MIN)),
                            i128::from(declaration.max.unwrap_or(u32::MAX)),
                        ),
                    };
                    let (property_min, property_max) =
                        numeric_scalar_bounds(property.ptype.as_deref()).expect("checked above");
                    if min < property_min || max > property_max {
                        return Err(ManifestError::Contract(format!(
                            "{path}.responder propertyTransition terminal parameter '{parameter}' range is outside target '{target}' encoding"
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

fn numeric_scalar_bounds(ptype: Option<&str>) -> Option<(i128, i128)> {
    match ptype {
        Some("u8") => Some((i128::from(u8::MIN), i128::from(u8::MAX))),
        Some("u16") => Some((i128::from(u16::MIN), i128::from(u16::MAX))),
        Some("u32") => Some((i128::from(u32::MIN), i128::from(u32::MAX))),
        Some("u64") => Some((i128::from(u64::MIN), i128::from(u64::MAX))),
        Some("i16") => Some((i128::from(i16::MIN), i128::from(i16::MAX))),
        Some("i32") => Some((i128::from(i32::MIN), i128::from(i32::MAX))),
        _ => None,
    }
}

fn property_scalar_fits(property: &Property, value: i64) -> bool {
    numeric_scalar_bounds(property.ptype.as_deref())
        .is_some_and(|(min, max)| i128::from(value) >= min && i128::from(value) <= max)
}

fn require_valid_action_runtime_values(
    manifest: &CameraManifest,
    initiator: &ActionInitiator,
    path: &str,
) -> Result<(), ManifestError> {
    let declarations = initiator
        .params
        .iter()
        .map(|parameter| {
            let normalized = parameter.normalized();
            (normalized.name.clone(), normalized)
        })
        .collect::<std::collections::BTreeMap<_, _>>();

    fn walk(
        manifest: &CameraManifest,
        declarations: &std::collections::BTreeMap<String, ActionInitiatorParameterDeclaration>,
        steps: &[Step],
        path: &str,
    ) -> Result<(), ManifestError> {
        for (index, step) in steps.iter().enumerate() {
            let step_path = format!("{path}.steps[{index}]");
            if let Some(SetPropValue::Runtime(reference)) = &step.value {
                let declaration = declarations.get(&reference.runtime).ok_or_else(|| {
                    ManifestError::Contract(format!(
                        "{step_path}.value references undeclared initiator parameter '{}'",
                        reference.runtime
                    ))
                })?;
                if reference.if_missing == MissingRuntimeValue::Skip && declaration.required {
                    return Err(ManifestError::Contract(format!(
                        "{step_path}.value ifMissing skip requires optional initiator parameter '{}'",
                        reference.runtime
                    )));
                }
                let prop = step
                    .set_prop
                    .as_deref()
                    .and_then(parse_hex_code)
                    .and_then(|code| manifest.property(code))
                    .ok_or_else(|| {
                        ManifestError::Contract(format!(
                            "{step_path}.value runtime reference requires a declared setProp property"
                        ))
                    })?;
                let compatible = match declaration.kind {
                    ActionInitiatorParameterKind::String => prop.ptype.as_deref() == Some("str"),
                    ActionInitiatorParameterKind::U64 => matches!(
                        prop.ptype.as_deref(),
                        Some("u8" | "u16" | "u32" | "u64" | "i16" | "i32")
                    ),
                };
                if !compatible {
                    return Err(ManifestError::Contract(format!(
                        "{step_path}.value parameter '{}' kind is incompatible with property type {:?}",
                        reference.runtime, prop.ptype
                    )));
                }
            }
            if let Some(await_until) = &step.await_until {
                walk(
                    manifest,
                    declarations,
                    &await_until.on_each,
                    &format!("{step_path}.awaitUntil"),
                )?;
            }
            if let Some(retry) = &step.retry {
                walk(
                    manifest,
                    declarations,
                    &retry.steps,
                    &format!("{step_path}.retry"),
                )?;
            }
            if let Some(loop_step) = &step.r#loop {
                let body = match loop_step {
                    Loop::ForEach { body, .. } | Loop::Chunk { body, .. } => body,
                };
                walk(manifest, declarations, body, &format!("{step_path}.loop"))?;
            }
            if let Some(condition) = &step.if_step {
                walk(
                    manifest,
                    declarations,
                    &condition.then_steps,
                    &format!("{step_path}.if.then"),
                )?;
                walk(
                    manifest,
                    declarations,
                    &condition.else_steps,
                    &format!("{step_path}.if.else"),
                )?;
            }
        }
        Ok(())
    }

    walk(manifest, &declarations, &initiator.steps, path)
}

fn require_no_runtime_set_props(steps: &[Step], path: &str) -> Result<(), ManifestError> {
    for (index, step) in steps.iter().enumerate() {
        let step_path = format!("{path}.steps[{index}]");
        if matches!(step.value, Some(SetPropValue::Runtime(_))) {
            return Err(ManifestError::Contract(format!(
                "{step_path}.value runtime references require an action initiator parameter declaration"
            )));
        }
        if let Some(await_until) = &step.await_until {
            require_no_runtime_set_props(&await_until.on_each, &format!("{step_path}.awaitUntil"))?;
        }
        if let Some(retry) = &step.retry {
            require_no_runtime_set_props(&retry.steps, &format!("{step_path}.retry"))?;
        }
        if let Some(loop_step) = &step.r#loop {
            let body = match loop_step {
                Loop::ForEach { body, .. } | Loop::Chunk { body, .. } => body,
            };
            require_no_runtime_set_props(body, &format!("{step_path}.loop"))?;
        }
        if let Some(condition) = &step.if_step {
            require_no_runtime_set_props(&condition.then_steps, &format!("{step_path}.if.then"))?;
            require_no_runtime_set_props(&condition.else_steps, &format!("{step_path}.if.else"))?;
        }
    }
    Ok(())
}

fn require_valid_object_transfer(
    manifest: &CameraManifest,
    connection: &Connection,
    connection_id: &str,
) -> Result<(), ManifestError> {
    let Some(contract) = &connection.object_transfer else {
        return Ok(());
    };
    let path = format!("connections.{connection_id}.objectTransfer");
    let read = connection
        .actions
        .get(&contract.read_action)
        .ok_or_else(|| {
            ManifestError::Contract(format!(
                "{path}.readAction references a missing connection action"
            ))
        })?;
    if !read.mode.is_empty() && !connection.modes.iter().any(|mode| mode == &read.mode) {
        return Err(ManifestError::Contract(format!(
            "{path}.readAction mode '{}' is unavailable on this connection",
            read.mode
        )));
    }
    let read_params = read
        .initiator
        .as_ref()
        .map(|binding| binding.params.as_slice());
    match contract.strategy {
        ObjectTransferStrategy::WholeObject
            if !read_params.is_some_and(|params| params == ["handle"]) =>
        {
            return Err(ManifestError::Contract(format!(
                "{path} wholeObject readAction must accept exactly [handle]"
            )));
        }
        ObjectTransferStrategy::Chunked
            if !read_params.is_some_and(|params| params == ["handle", "offset", "length"]) =>
        {
            return Err(ManifestError::Contract(format!(
                "{path} chunked readAction must accept [handle, offset, length]"
            )));
        }
        _ => {}
    }
    match (contract.strategy, contract.resume_policy) {
        (ObjectTransferStrategy::WholeObject, ObjectTransferResumePolicy::RestartFromZero)
        | (ObjectTransferStrategy::Chunked, ObjectTransferResumePolicy::ByteOffset) => {}
        _ => {
            return Err(ManifestError::Contract(format!(
                "{path} strategy and resumePolicy are inconsistent"
            )));
        }
    }
    if let Some(completion) = &contract.completion {
        let action = connection.actions.get(&completion.action).ok_or_else(|| {
            ManifestError::Contract(format!(
                "{path}.completion.action references a missing connection action"
            ))
        })?;
        if !action
            .initiator
            .as_ref()
            .is_some_and(|binding| binding.params == ["handle"])
        {
            return Err(ManifestError::Contract(format!(
                "{path}.completion.action must accept exactly [handle]"
            )));
        }
        if action.mode != read.mode {
            return Err(ManifestError::Contract(format!(
                "{path}.completion.action mode '{}' does not match readAction mode '{}'",
                action.mode, read.mode
            )));
        }
        if !action.mode.is_empty() && !connection.modes.iter().any(|mode| mode == &action.mode) {
            return Err(ManifestError::Contract(format!(
                "{path}.completion.action mode '{}' is unavailable on this connection",
                action.mode
            )));
        }
    }
    if contract.formats.is_empty() {
        return Err(ManifestError::Contract(format!(
            "{path}.formats must declare at least one object format"
        )));
    }
    for code in contract.formats.keys() {
        if parse_hex_code(code).is_none() {
            return Err(ManifestError::Contract(format!(
                "{path}.formats contains invalid object-format code '{code}'"
            )));
        }
        if !manifest
            .media
            .as_ref()
            .is_some_and(|media| media.formats.contains_key(code))
        {
            return Err(ManifestError::Contract(format!(
                "{path}.formats references unknown media format '{code}'"
            )));
        }
    }
    Ok(())
}

fn require_valid_control_surfaces(
    manifest: &CameraManifest,
    connection: &Connection,
    connection_id: &str,
) -> Result<(), ManifestError> {
    for (mode, controls) in &connection.control_surfaces {
        let path = format!("connections.{connection_id}.controlSurfaces.{mode}");
        if !connection.modes.iter().any(|candidate| candidate == mode) {
            return Err(ManifestError::Contract(format!(
                "{path} references a mode unavailable on this connection"
            )));
        }
        for (role, surface) in controls {
            if parse_hex_code(&surface.property).is_none() {
                return Err(ManifestError::Contract(format!(
                    "{path}.{role:?} contains invalid property code '{}'",
                    surface.property
                )));
            }
            let property = manifest.properties.get(&surface.property).ok_or_else(|| {
                ManifestError::Contract(format!(
                    "{path}.{role:?} references unknown property '{}'",
                    surface.property
                ))
            })?;
            if !property.controls.contains_key(connection_id)
                && !property.controls.contains_key(mode)
            {
                return Err(ManifestError::Contract(format!(
                    "{path}.{role:?} property '{}' has no control for this connection or mode",
                    surface.property
                )));
            }
            let control = property
                .controls
                .get(connection_id)
                .or_else(|| property.controls.get(mode))
                .expect("checked above");
            if surface.read_source == ControlReadSource::DeclaredReadback
                && control.readback.is_none()
            {
                return Err(ManifestError::Contract(format!(
                    "{path}.{role:?} selects declaredReadback but its control has no readback property"
                )));
            }
        }
    }
    Ok(())
}

type ActivityContract = (
    ConnectionActivityDisplayRole,
    u32,
    bool,
    bool,
    activity::ConnectionActivityIdentity,
);

fn require_consistent_activity_metadata(
    seen: &mut std::collections::BTreeMap<(String, u32), ActivityContract>,
    descriptor: &ConnectionActivityDescriptor,
    path: &str,
) -> Result<(), ManifestError> {
    let key = (descriptor.id.clone(), descriptor.version);
    let value = (
        descriptor.display_role.clone(),
        descriptor.default_expected_duration_ms,
        descriptor.interaction_required,
        descriptor.optional,
        descriptor.identity(),
    );
    if let Some(previous) = seen.insert(key, value.clone()) {
        if previous != value {
            return Err(ManifestError::Contract(format!(
                "{path} activity '{}@{}' metadata differs or binding identity differs from another descriptor",
                descriptor.id, descriptor.version
            )));
        }
    }
    Ok(())
}

fn require_valid_executor_activities(
    activities: &[ConnectionActivityDescriptor],
    step_count: usize,
    path: &str,
) -> Result<(), ManifestError> {
    use activity::{valid_activity_id, ConnectionActivityBinding, ConnectionActivitySequence};

    let mut ids = std::collections::BTreeSet::new();
    let mut next = 0_u32;
    for (index, descriptor) in activities.iter().enumerate() {
        let here = format!("{path}[{index}]");
        if !valid_activity_id(&descriptor.id) || !ids.insert(&descriptor.id) {
            return Err(ManifestError::Contract(format!(
                "{here}.id must be unique with at least two dot-delimited segments"
            )));
        }
        if descriptor.version == 0 || descriptor.default_expected_duration_ms == 0 {
            return Err(ManifestError::Contract(format!(
                "{here} version and defaultExpectedDurationMs must be > 0"
            )));
        }
        let ConnectionActivityBinding::ExecutorSpan(binding) = &descriptor.binding else {
            return Err(ManifestError::Contract(format!(
                "{here} must use executorSpan"
            )));
        };
        let span = &binding.executor_span;
        if span.sequence != ConnectionActivitySequence::Steps {
            return Err(ManifestError::Contract(format!(
                "{here}.executorSpan.sequence must be steps"
            )));
        }
        if span.start_step != next || span.end_step_exclusive <= span.start_step {
            return Err(ManifestError::Contract(format!(
                "{here}.executorSpan must continue ordered coverage at step {next}"
            )));
        }
        next = span.end_step_exclusive;
    }
    if !activities.is_empty() && next as usize != step_count {
        return Err(ManifestError::Contract(format!(
            "{path} covers {next} of {step_count} top-level steps"
        )));
    }
    Ok(())
}

fn require_valid_host_activities(
    connection: &Connection,
    connection_id: &str,
) -> Result<(), ManifestError> {
    use activity::{
        valid_activity_id, ConnectionActivityBinding, ConnectionActivityHostEstablishment,
    };

    let mut ids = std::collections::BTreeSet::new();
    let mut checkpoints = std::collections::BTreeSet::new();
    let mut exact_network_scopes = std::collections::BTreeSet::new();
    let mut retained_session_role = None;
    for (index, descriptor) in connection.activities.iter().enumerate() {
        let path = format!("connections.{connection_id}.activities[{index}]");
        if !valid_activity_id(&descriptor.id) {
            return Err(ManifestError::Contract(format!(
                "{path}.id must contain at least two non-empty dot-delimited segments"
            )));
        }
        if !ids.insert(&descriptor.id) {
            return Err(ManifestError::Contract(format!(
                "{path}.id duplicates activity '{}'",
                descriptor.id
            )));
        }
        if descriptor.version == 0 {
            return Err(ManifestError::Contract(format!(
                "{path}.version must be > 0"
            )));
        }
        if descriptor.default_expected_duration_ms == 0 {
            return Err(ManifestError::Contract(format!(
                "{path}.defaultExpectedDurationMs must be > 0"
            )));
        }
        match &descriptor.binding {
            ConnectionActivityBinding::HostCheckpoint(binding) => {
                if binding.host_checkpoint.name.is_empty() {
                    return Err(ManifestError::Contract(format!(
                        "{path}.hostCheckpoint.name must not be empty"
                    )));
                }
                if !checkpoints.insert(&binding.host_checkpoint.name) {
                    return Err(ManifestError::Contract(format!(
                        "{path}.hostCheckpoint.name duplicates checkpoint '{}'",
                        binding.host_checkpoint.name
                    )));
                }
            }
            ConnectionActivityBinding::HostEstablishment(binding) => {
                match &binding.host_establishment {
                    ConnectionActivityHostEstablishment::NetworkIdentityExact {
                        network_identity_exact,
                    } => {
                        if network_identity_exact.expected_scope.is_empty() {
                            return Err(ManifestError::Contract(format!(
                            "{path}.hostEstablishment.networkIdentityExact.expectedScope must not be empty"
                        )));
                        }
                        if !exact_network_scopes
                            .insert(network_identity_exact.expected_scope.as_str())
                        {
                            return Err(ManifestError::Contract(format!(
                                "{path}.hostEstablishment.networkIdentityExact.expectedScope duplicates exact network gate '{}'",
                                network_identity_exact.expected_scope
                            )));
                        }
                    }
                    ConnectionActivityHostEstablishment::RetainedSessionOpen {
                        retained_session_open,
                    } => {
                        if retained_session_open.socket_role != SocketRole::Command {
                            return Err(ManifestError::Contract(format!(
                            "{path}.hostEstablishment.retainedSessionOpen.socketRole must be command"
                            )));
                        }
                        if retained_session_role
                            .replace(retained_session_open.socket_role)
                            .is_some()
                        {
                            return Err(ManifestError::Contract(format!(
                                "{path}.hostEstablishment.retainedSessionOpen.socketRole duplicates retained session gate '{:?}'",
                                retained_session_open.socket_role
                            )));
                        }
                    }
                }
            }
            ConnectionActivityBinding::ExecutorSpan(_) => {
                return Err(ManifestError::Contract(format!(
                    "{path} must use hostCheckpoint or hostEstablishment"
                )));
            }
        }
    }
    Ok(())
}

fn require_valid_ptp_steps(steps: &[Step], path: &str) -> Result<(), ManifestError> {
    let mut collections = std::collections::BTreeSet::new();
    require_valid_ptp_steps_with_collections(steps, path, &mut collections)
}

fn require_valid_ptp_steps_with_collections(
    steps: &[Step],
    path: &str,
    collections: &mut std::collections::BTreeSet<String>,
) -> Result<(), ManifestError> {
    for (index, step) in steps.iter().enumerate() {
        let step_path = format!("{path}.steps[{index}]");
        if !step.is_well_formed() {
            return Err(ManifestError::Contract(format!(
                "{step_path} must contain exactly one action"
            )));
        }
        if let Some(await_until) = &step.await_until {
            if !step.captures.is_empty() {
                let has_polled_value = matches!(await_until.source, AwaitSource::Poll { .. })
                    || matches!(
                        await_until.source,
                        AwaitSource::Event {
                            then_poll: Some(_),
                            ..
                        }
                    );
                if !has_polled_value {
                    return Err(ManifestError::Contract(format!(
                        "{step_path}.captures require awaitUntil poll or event thenPoll"
                    )));
                }
                if step
                    .captures
                    .iter()
                    .any(|capture| capture.source != CaptureSource::PropValue)
                {
                    return Err(ManifestError::Contract(format!(
                        "{step_path}.captures on awaitUntil support only propValue"
                    )));
                }
            }
        }
        let mut array_binds = std::collections::BTreeSet::new();
        for capture in &step.captures {
            if capture.source == CaptureSource::TransactionId {
                if step.send_op.is_none() {
                    return Err(ManifestError::Contract(format!(
                        "{step_path} transactionId capture requires sendOp"
                    )));
                }
                if step.repeat != 1 {
                    return Err(ManifestError::Contract(format!(
                        "{step_path} transactionId capture requires an unrepeated sendOp"
                    )));
                }
                if step.tolerant {
                    return Err(ManifestError::Contract(format!(
                        "{step_path} transactionId capture must not be tolerant"
                    )));
                }
                if capture.bind.trim().is_empty() {
                    return Err(ManifestError::Contract(format!(
                        "{step_path} transactionId capture bind must not be empty"
                    )));
                }
            }
            if capture.source != CaptureSource::PtpU32Array {
                continue;
            }
            if step.get_prop.is_none() && step.send_op.is_none() {
                return Err(ManifestError::Contract(format!(
                    "{step_path} ptpU32Array capture requires getProp or sendOp"
                )));
            }
            if step.tolerant {
                return Err(ManifestError::Contract(format!(
                    "{step_path} ptpU32Array capture must not be tolerant"
                )));
            }
            if capture.bind.trim().is_empty() {
                return Err(ManifestError::Contract(format!(
                    "{step_path} ptpU32Array capture bind must not be empty"
                )));
            }
            if !array_binds.insert(capture.bind.clone()) {
                return Err(ManifestError::Contract(format!(
                    "{step_path} repeats ptpU32Array capture bind '{}'",
                    capture.bind
                )));
            }
        }
        collections.extend(array_binds);
        if let Some(retry) = &step.retry {
            if retry.steps.is_empty() {
                return Err(ManifestError::Contract(format!(
                    "{step_path}.retry steps must not be empty"
                )));
            }
            if retry.when_response_codes.is_empty() && retry.when_failure_classes.is_empty() {
                return Err(ManifestError::Contract(format!(
                    "{step_path}.retry must select at least one of \
                     whenResponseCodes/whenFailureClasses"
                )));
            }
            if retry.max_attempts == 0 {
                return Err(ManifestError::Contract(format!(
                    "{step_path}.retry maxAttempts must be at least one"
                )));
            }
            if contains_ptp_loop(&retry.steps) {
                return Err(ManifestError::Contract(format!(
                    "{step_path}.retry must not contain loop; put retry inside the per-element body"
                )));
            }
            for code in &retry.when_response_codes {
                if parse_hex_code(code).is_none() {
                    return Err(ManifestError::Contract(format!(
                        "{step_path}.retry has invalid response code '{code}'"
                    )));
                }
            }
            let before_retry = collections.clone();
            let mut after_retry = collections.clone();
            require_valid_ptp_steps_with_collections(
                &retry.steps,
                &format!("{step_path}.retry"),
                &mut after_retry,
            )?;
            if step.tolerant && after_retry != before_retry {
                return Err(ManifestError::Contract(format!(
                    "{step_path}.retry captures a collection and must not be tolerant"
                )));
            }
            *collections = after_retry;
        }
        if let Some(await_until) = &step.await_until {
            let mut nested = collections.clone();
            require_valid_ptp_steps_with_collections(
                &await_until.on_each,
                &format!("{step_path}.awaitUntil"),
                &mut nested,
            )?;
        }
        if let Some(loop_step) = &step.r#loop {
            let body = match loop_step {
                Loop::ForEach {
                    collection, body, ..
                } => {
                    if !collections.contains(collection) {
                        return Err(ManifestError::Contract(format!(
                            "{step_path}.loop forEach collection '{collection}' is not definitely bound"
                        )));
                    }
                    body
                }
                Loop::Chunk { body, .. } => body,
            };
            let mut nested = collections.clone();
            require_valid_ptp_steps_with_collections(
                body,
                &format!("{step_path}.loop"),
                &mut nested,
            )?;
        }
        if let Some(condition) = &step.if_step {
            let before = collections.clone();
            let mut then_collections = before.clone();
            require_valid_ptp_steps_with_collections(
                &condition.then_steps,
                &format!("{step_path}.if.then"),
                &mut then_collections,
            )?;
            let mut else_collections = before;
            require_valid_ptp_steps_with_collections(
                &condition.else_steps,
                &format!("{step_path}.if.else"),
                &mut else_collections,
            )?;
            // A collection is definitely bound after the conditional only if
            // both possible branches bind it.
            then_collections.retain(|collection| else_collections.contains(collection));
            *collections = then_collections;
        }
    }
    Ok(())
}

fn require_valid_open_channels(
    steps: &[Step],
    bindings: Option<&SocketBindings>,
    path: &str,
    top_level_allowed: bool,
) -> Result<(), ManifestError> {
    let mut has_enforceable_prefix = false;
    for (index, step) in steps.iter().enumerate() {
        let step_path = format!("{path}.steps[{index}]");
        if let Some(role) = step.open_channel {
            if !top_level_allowed {
                return Err(ManifestError::Contract(format!(
                    "{step_path}.openChannel is only valid as a top-level mode-entry or action step"
                )));
            }
            if role == SocketRole::Command {
                return Err(ManifestError::Contract(format!(
                    "{step_path}.openChannel cannot open the command channel; it is established before plan execution"
                )));
            }
            if bindings.and_then(|value| value.port_for(role)).is_none() {
                return Err(ManifestError::Contract(format!(
                    "{step_path}.openChannel role '{role:?}' has no socket binding on its connection"
                )));
            }
            if !has_enforceable_prefix {
                return Err(ManifestError::Contract(format!(
                    "{step_path}.openChannel requires a preceding strict wire step so simulators can enforce its causal boundary"
                )));
            }
            continue;
        }
        if let Some(retry) = &step.retry {
            require_valid_open_channels(
                &retry.steps,
                bindings,
                &format!("{step_path}.retry"),
                false,
            )?;
        }
        if let Some(await_until) = &step.await_until {
            require_valid_open_channels(
                &await_until.on_each,
                bindings,
                &format!("{step_path}.awaitUntil"),
                false,
            )?;
        }
        if let Some(loop_step) = &step.r#loop {
            let body = match loop_step {
                Loop::ForEach { body, .. } | Loop::Chunk { body, .. } => body,
            };
            require_valid_open_channels(body, bindings, &format!("{step_path}.loop"), false)?;
        }
        if let Some(condition) = &step.if_step {
            require_valid_open_channels(
                &condition.then_steps,
                bindings,
                &format!("{step_path}.if.then"),
                false,
            )?;
            require_valid_open_channels(
                &condition.else_steps,
                bindings,
                &format!("{step_path}.if.else"),
                false,
            )?;
        }
        has_enforceable_prefix = !step.tolerant && step.is_sequence_gate_matchable();
    }
    Ok(())
}

fn contains_ptp_loop(steps: &[Step]) -> bool {
    steps.iter().any(|step| {
        step.r#loop.is_some()
            || step
                .retry
                .as_ref()
                .is_some_and(|retry| contains_ptp_loop(&retry.steps))
            || step
                .await_until
                .as_ref()
                .is_some_and(|await_until| contains_ptp_loop(&await_until.on_each))
            || step.if_step.as_ref().is_some_and(|condition| {
                contains_ptp_loop(&condition.then_steps) || contains_ptp_loop(&condition.else_steps)
            })
    })
}

fn validate_transfer_hex_values(
    transfer: &CameraInitiatedTransfer,
    ctx: &str,
    lints: &mut Vec<Lint>,
) {
    if transfer.trigger.states.is_empty() {
        lints.push(Lint::warn(format!("{ctx} declares no trigger states")));
    }
    for (i, state) in transfer.trigger.states.iter().enumerate() {
        if state.trigger_values.is_empty() {
            lints.push(Lint::warn(format!(
                "{ctx} trigger state {i} declares no triggerValues"
            )));
        }
        for value in state.trigger_values.iter().chain(&state.baseline_values) {
            if parse_hex_bytes(value).is_none() {
                lints.push(Lint::warn(format!(
                    "{ctx} trigger state {i} has invalid hex value '{value}'"
                )));
            }
        }
    }
    if let Some(launch) = &transfer.handoff.function_launch {
        if parse_hex_bytes(&launch.value).is_none() {
            lints.push(Lint::warn(format!(
                "{ctx} functionLaunch has invalid hex value '{}'",
                launch.value
            )));
        }
    }
}

fn check_gate_requirement(
    req: Option<&GateRequirement>,
    ctx: &str,
    defined_gates: &std::collections::BTreeSet<&str>,
    lints: &mut Vec<Lint>,
) {
    let Some(req) = req else {
        return;
    };
    if !defined_gates.contains(req.name.as_str()) {
        lints.push(Lint::warn(format!(
            "{ctx} requiresGate references gate '{}' which is not defined in this manifest",
            req.name
        )));
    }
}

fn check_gate_steps(
    steps: &[Step],
    ctx: &str,
    defined_gates: &std::collections::BTreeSet<&str>,
    lints: &mut Vec<Lint>,
) {
    let mut active: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for (i, step) in steps.iter().enumerate() {
        if let Some(retry) = &step.retry {
            check_gate_steps(
                &retry.steps,
                &format!("{ctx}.steps[{i}].retry"),
                defined_gates,
                lints,
            );
        }
        if let Some(gate) = &step.starts_gate {
            if !defined_gates.contains(gate.as_str()) {
                lints.push(Lint::warn(format!(
                    "{ctx}.steps[{i}] startsGate references gate '{gate}' which is not defined in this manifest"
                )));
            }
            active.insert(gate.clone(), i);
        }

        for gate in active.keys() {
            if !step.is_sequence_gate_matchable() {
                lints.push(Lint::warn(format!(
                    "{ctx}.steps[{i}] is inside sequence gate '{gate}' but is not a matchable setProp/getProp/sendOp step"
                )));
            }
        }

        if let Some(gate) = &step.completes_gate {
            if !defined_gates.contains(gate.as_str()) {
                lints.push(Lint::warn(format!(
                    "{ctx}.steps[{i}] completesGate references gate '{gate}' which is not defined in this manifest"
                )));
            }
            if active.remove(gate).is_none() {
                lints.push(Lint::warn(format!(
                    "{ctx}.steps[{i}] completesGate '{gate}' has no preceding startsGate in this step sequence"
                )));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
schema: camera-config/v1
camera:
  manufacturer: FUJIFILM
  model: GFX100 II
  firmware: "2.30"
operations:
  "0x101c":
    name: InitiateOpenCapture
    owner: standard-ptp
    workflows: [liveView]
    evidence: [appLiveViewCapture]
properties:
  "0x5007":
    name: aperture
    ptpName: FNumber
    type: u16
    access: readWrite
    descriptor:
      form: enum
      values: [280, 400, 560, 800, 65535]
    controls:
      liveView: { setMethod: vendorStep, operation: "0x902d", readback: "0xd212" }
      tether:   { setMethod: absolute,   operation: "0x1016" }
    labels:
      280: "f/2.8"
      65535: "body"
    evidence: [appLiveViewCapture]
workflows:
  liveView:
    transport: appAp
    states: [sessionOpen, streaming]
evidence:
  appLiveViewCapture:
    kind: wire-capture
    path: docs/APP_LIVEVIEW_CODE_MAP.md
"#;

    #[test]
    fn loads_and_queries() {
        let m = CameraManifest::from_yaml(SAMPLE).unwrap();
        m.require_supported_schema().unwrap();
        assert_eq!(m.camera.model, "GFX100 II");

        // Operation support is workflow-aware.
        assert_eq!(
            m.supports_operation("liveView", 0x101c),
            Support::InWorkflow
        );
        assert_eq!(
            m.supports_operation("imageImport", 0x101c),
            Support::WrongWorkflow
        );
        assert_eq!(
            m.supports_operation("liveView", 0x9999),
            Support::Unsupported
        );

        // Intent -> mechanism resolution differs by mode.
        let lv = m.control_for(0x5007, "liveView").unwrap();
        assert_eq!(lv.set_method.as_deref(), Some("vendorStep"));
        assert_eq!(lv.operation.as_deref(), Some("0x902d"));
        let tether = m.control_for(0x5007, "tether").unwrap();
        assert_eq!(tether.set_method.as_deref(), Some("absolute"));

        // Value labels.
        assert_eq!(m.value_label(0x5007, 280), Some("f/2.8"));
        assert_eq!(m.value_label(0x5007, 65535), Some("body"));
        assert_eq!(m.value_label(0x5007, 999), None);
    }

    #[test]
    fn defined_evidence_produces_no_lint() {
        let m = CameraManifest::from_yaml(SAMPLE).unwrap();
        assert!(m.validate().is_empty());
    }

    #[test]
    fn unresolved_evidence_is_a_warning_not_an_error() {
        // Same manifest but drop the evidence definition.
        let text = SAMPLE.replace(
            "evidence:\n  appLiveViewCapture:\n    kind: wire-capture\n    path: docs/APP_LIVEVIEW_CODE_MAP.md\n",
            "",
        );
        let m = CameraManifest::from_yaml(&text).expect("still loads");
        let lints = m.validate();
        assert!(!lints.is_empty(), "should warn about unresolved evidence");
        assert!(lints.iter().all(|l| l.severity == Severity::Warning));
    }

    #[test]
    fn descriptor_values_must_match_the_property_type() {
        let with_property = |ptype: &str, values: &str| {
            CameraManifest::from_yaml(&format!(
                r#"
schema: camera-config/v1
camera: {{ manufacturer: EXAMPLE, model: MODEL, firmware: "1.0" }}
properties:
  "0xd001":
    name: example
    type: {ptype}
    descriptor: {{ form: enum, values: {values} }}
"#
            ))
        };

        let string_error = with_property("u16", r#"["4000x2664"]"#)
            .unwrap_err()
            .to_string();
        assert!(
            string_error.contains(
                "properties.0xd001.descriptor.values contains a string value but property 0xd001 does not have type str"
            ),
            "got: {string_error}"
        );

        let integer_error = with_property("str", "[1]").unwrap_err().to_string();
        assert!(
            integer_error.contains(
                "properties.0xd001.descriptor.values contains an integer value but property 0xd001 has type str; string enum values must be quoted in YAML"
            ),
            "got: {integer_error}"
        );
    }

    #[test]
    fn string_properties_must_not_have_integer_initial_values() {
        let error = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: EXAMPLE, model: MODEL, firmware: "1.0" }
properties:
  "0xd001": { name: example, type: str, initialValue: 1 }
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains(
                "properties.0xd001.initialValue must be a quoted string for a property with type str"
            ),
            "got: {error}"
        );
    }

    #[test]
    fn string_properties_accept_quoted_string_initial_values() {
        let manifest = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: EXAMPLE, model: MODEL, firmware: "1.0" }
properties:
  "0xd001": { name: example, type: str, initialValue: "4000x2664" }
"#,
        )
        .unwrap();
        assert_eq!(
            manifest.properties["0xd001"].initial_value,
            Some(DescriptorValue::Str("4000x2664".to_string()))
        );
    }

    #[test]
    fn numeric_properties_reject_string_initial_values() {
        let error = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: EXAMPLE, model: MODEL, firmware: "1.0" }
properties:
  "0xd001": { name: example, type: u16, initialValue: "4000x2664" }
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains(
                "properties.0xd001.initialValue must be an integer for a property with a numeric type"
            ),
            "got: {error}"
        );
    }

    #[test]
    fn range_descriptors_reject_string_values_but_allow_empty_string_ranges() {
        let with_values = |values: &str| {
            CameraManifest::from_yaml(&format!(
                r#"
schema: camera-config/v1
camera: {{ manufacturer: EXAMPLE, model: MODEL, firmware: "1.0" }}
properties:
  "0xd001":
    name: example
    type: str
    descriptor: {{ form: range, values: {values} }}
"#
            ))
        };

        let error = with_values(r#"["a", "b"]"#).unwrap_err().to_string();
        assert!(
            error.contains(
                "properties.0xd001.descriptor.values contains a string value but form range requires integer values"
            ),
            "got: {error}"
        );
        with_values("[]").expect("an empty range on a string property remains valid");
    }

    #[test]
    fn property_catalog_keys_must_be_hex_codes() {
        let manifest: CameraManifest = serde_yaml::from_str(
            r#"
schema: camera-config/v1
camera: { manufacturer: EXAMPLE, model: MODEL, firmware: "1.0" }
properties: { "0xzz": { name: invalid } }
"#,
        )
        .unwrap();
        let error = manifest.require_valid_mode_entries().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("properties map key '0xzz' is not a hex property code"),
            "got: {error}"
        );
    }

    #[test]
    fn operation_catalog_keys_must_be_hex_codes() {
        let manifest: CameraManifest = serde_yaml::from_str(
            r#"
schema: camera-config/v1
camera: { manufacturer: EXAMPLE, model: MODEL, firmware: "1.0" }
operations: { "0xzz": { name: invalid } }
"#,
        )
        .unwrap();
        let error = manifest.require_valid_mode_entries().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("operations map key '0xzz' is not a hex operation code"),
            "got: {error}"
        );
    }

    #[test]
    fn init_shape_validation_lints_undefined_evidence() {
        let manifest = |evidence: &str| {
            CameraManifest::from_yaml(&format!(
                r#"{SAMPLE}
connections:
  app:
    init:
      identity:
        guid: initiatorGuid
        friendlyName: initFriendlyName
      nameFieldByteCount: 54
      evidence: [{evidence}]
"#
            ))
            .expect("init-shape manifest loads")
        };

        assert!(
            manifest("appLiveViewCapture").validate().is_empty(),
            "defined init-shape evidence should not produce a lint"
        );

        let lints = manifest("missingInitEvidence").validate();
        assert!(lints.iter().any(|lint| {
            lint.message.contains(
                "connection app init references evidence id 'missingInitEvidence' which is not defined",
            )
        }), "missing init-shape evidence lint; got {lints:?}");
        assert!(lints.iter().all(|lint| lint.severity == Severity::Warning));
    }

    #[test]
    fn sentinel_validation_lints_evidence_reference_and_bytes() {
        let text = format!(
            "{SAMPLE}\n{}",
            r#"
sentinels:
  badFrame: { bytes: "0xz", evidence: [missingEvidence] }
"#
        );
        let m = CameraManifest::from_yaml(&text).expect("sentinel manifest loads");
        let messages: Vec<_> = m.validate().into_iter().map(|l| l.message).collect();
        assert!(
            messages
                .iter()
                .any(|m| m.contains("sentinel badFrame references evidence id 'missingEvidence'")),
            "missing sentinel evidence lint; got {messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|m| m.contains("sentinel badFrame has invalid hex bytes")),
            "missing sentinel bytes lint; got {messages:?}"
        );
    }

    #[test]
    fn transport_close_validation_lints_unknown_sentinel() {
        let text = format!(
            "{SAMPLE}\n{}",
            r#"
connections:
  app:
    transportClose: { sentinel: missingFrame }
"#
        );
        let m = CameraManifest::from_yaml(&text).expect("transport-close manifest loads");
        let messages: Vec<_> = m.validate().into_iter().map(|l| l.message).collect();
        assert!(
            messages.iter().any(|m| {
                m.contains(
                    "connection app transportClose references sentinel 'missingFrame' which is not defined",
                )
            }),
            "missing unknown transport-close sentinel lint; got {messages:?}"
        );
    }

    #[test]
    fn pcss_discovery_targets_are_closed_and_consistent() {
        let valid = format!(
            "{SAMPLE}\n{}",
            r#"
connections:
  wireless-tether:
    knock:
      callbackPort: 51560
      knockPort: 51562
      protocol: PCSS/1.0
      discoveryTargets:
        default: subnetBroadcast
        supported: [subnetBroadcast, explicitUnicast]
        retryDiscoveredUnicast: true
"#
        );
        CameraManifest::from_yaml(&valid).expect("valid PCSS target modes load");

        let missing_default = valid.replace(
            "default: subnetBroadcast\n        supported: [subnetBroadcast, explicitUnicast]",
            "default: explicitUnicast\n        supported: [subnetBroadcast]",
        );
        assert!(CameraManifest::from_yaml(&missing_default)
            .unwrap_err()
            .to_string()
            .contains("default must be listed in supported"));

        let duplicate = valid.replace(
            "supported: [subnetBroadcast, explicitUnicast]",
            "supported: [subnetBroadcast, subnetBroadcast]",
        );
        assert!(CameraManifest::from_yaml(&duplicate)
            .unwrap_err()
            .to_string()
            .contains("must not contain duplicates"));

        let recovery_without_unicast = valid.replace(
            "supported: [subnetBroadcast, explicitUnicast]",
            "supported: [subnetBroadcast]",
        );
        assert!(CameraManifest::from_yaml(&recovery_without_unicast)
            .unwrap_err()
            .to_string()
            .contains("retryDiscoveredUnicast requires subnetBroadcast and explicitUnicast"));
    }

    #[test]
    fn wrong_schema_is_rejected_explicitly() {
        let text = SAMPLE.replace("camera-config/v1", "camera-config/v999");
        let m = CameraManifest::from_yaml(&text).unwrap();
        assert!(m.require_supported_schema().is_err());
    }

    #[test]
    fn unknown_operation_handler_is_a_load_error() {
        // #407: a handler typo must fail closed at load, not dispatch as a
        // successful no-op.
        let text = r#"
schema: camera-config/v1
camera: { manufacturer: TESTCO, model: TM1, firmware: "1" }
operations:
  "0x902c": { name: StepSomething, handler: property.stepp }
"#;
        let err = CameraManifest::from_yaml(text).unwrap_err().to_string();
        assert!(err.contains("handler"), "err: {err}");
    }

    #[test]
    fn property_step_handler_requires_a_property_code() {
        let text = r#"
schema: camera-config/v1
camera: { manufacturer: TESTCO, model: TM1, firmware: "1" }
operations:
  "0x902c": { name: StepSomething, handler: property.step }
"#;
        let err = CameraManifest::from_yaml(text).unwrap_err().to_string();
        assert!(
            err.contains("handler 'property.step' requires a 'property' code"),
            "err: {err}"
        );
    }

    #[test]
    fn object_size_handler_requires_its_block_and_vice_versa() {
        let missing_block = r#"
schema: camera-config/v1
camera: { manufacturer: TESTCO, model: TM1, firmware: "1" }
operations:
  "0x9803": { name: ObjectSize, handler: object.size }
"#;
        let err = CameraManifest::from_yaml(missing_block)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("handler 'object.size' requires an 'objectSize' block"),
            "err: {err}"
        );

        let missing_handler = r#"
schema: camera-config/v1
camera: { manufacturer: TESTCO, model: TM1, firmware: "1" }
operations:
  "0x9803":
    name: ObjectSize
    objectSize: { handleParam: 0, encoding: u64Le }
"#;
        let err = CameraManifest::from_yaml(missing_handler)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("declares 'objectSize' without handler 'object.size'"),
            "err: {err}"
        );
    }

    #[test]
    fn unknown_property_access_is_a_load_error() {
        // #407: access is a closed set; an unknown value must not load.
        let text = r#"
schema: camera-config/v1
camera: { manufacturer: TESTCO, model: TM1, firmware: "1" }
properties:
  "0xd260": { name: mystery, type: u16, access: gs2 }
"#;
        let err = CameraManifest::from_yaml(text).unwrap_err().to_string();
        assert!(err.contains("access"), "err: {err}");
    }
}
