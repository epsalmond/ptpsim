//! `camera-config` — the manifest schema, loader, validation, compatibility
//! queries, and the canonical bundle→proposal generator.
//!
//! Manifests are the reviewed source of truth for camera behavior. `evidence:`
//! references are provenance only: a manifest loads and runs with them
//! unresolved (they become lints, never load errors), which is what lets the
//! public engine run manifests whose evidence lives in a private repo.

pub mod activity;
pub mod error;
pub mod generate;
pub mod index;
pub mod model;
pub mod predicate;
pub mod query;
pub mod std_names;
pub mod store;
pub mod trace;
pub mod version;

pub use activity::{
    ConnectionActivityBinding, ConnectionActivityDescriptor, ConnectionActivityDisplayRole,
    ConnectionActivityExecutorSpan, ConnectionActivityHostCheckpoint, ConnectionActivitySequence,
    ExecutorSpanBinding, HostCheckpointBinding,
};
pub use error::{ConfigError, Lint, ManifestError, Severity};
pub use generate::{enrich, generate_proposal};
pub use model::{
    parse_hex_bytes, parse_hex_code, Action, ActionEffect, ActionVerb, AvailableWhen, AwaitSource,
    AwaitUntil, BleLiteralWrite, BleStateTrigger, CameraIdentity, CameraInitiatedData,
    CameraInitiatedHandoff, CameraInitiatedMetadata, CameraInitiatedMetadataPhase,
    CameraInitiatedReceive, CameraInitiatedTransfer, CameraInitiatedTrigger, CameraManifest,
    CaptureSource, CloseSession, Connection, ConnectionTransition, Control, ControlOwner,
    ControlReadSource, ControlRole, ControlSurfaceEntry, ControlWriteEffect, Descriptor,
    GateFailure, GateRequirement, InitIdentity, InitRetries, InitShape, LiveViewDelivery,
    LiveViewDeliveryKind, LiveViewStream, Loop, ManufacturerDefaults, Media, MediaFormat, Mode,
    ModeEntry, ModeEntryExecution, ObjectTransferCompletionPolicy, ObjectTransferCompletionTiming,
    ObjectTransferContract, ObjectTransferFormatSupport, ObjectTransferResumePolicy,
    ObjectTransferStrategy, ObjectsAvailable, OpEffect, Operation, Payload, PayloadForm, PcssKnock,
    PostviewEvent, Property, PropertyKind, PropertyValueEncoding, PropertyValueProfile,
    PropertyValueProfileRow, PropertyValueRow, RecordLayout, RecordMemberRef,
    ReestablishConnection, ResponseRetry, SentinelFrame, SentinelMask, SequenceGate, ShutterRecipe,
    SocketBindings, SocketRole, Step, StepParam, StructuredTextField, StructuredTextLayout,
    StructuredTextScalar, TransferCompletion, TransportClose, TriggerMatch, ValuePolicy,
    ValueSource, VersionCond, WireFraming, Workflow,
};
pub use predicate::{Leaf, Predicate, PropView};
pub use query::{Availability, Support};
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
                                .any(|candidate| parse_hex_code(candidate) == Some(member))
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
                    if !self.properties.contains_key(m) {
                        lints.push(Lint::warn(format!(
                            "property {code} payload member '{m}' is not a defined property; \
                             its value width cannot be resolved for decode"
                        )));
                    }
                }
            }
        }
        for (id, wf) in &self.workflows {
            check(&wf.evidence, &format!("workflow {id}"), &mut lints);
        }
        for (id, conn) in &self.connections {
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
                check_gate_steps(
                    &action.steps,
                    &format!("connection {id} action {verb:?}"),
                    &defined_gates,
                    &mut lints,
                );
            }
        }
        lints
    }

    /// Fail closed on destructive outer transitions. A consumer must be able to
    /// establish the connection and resolve a non-recursive cold PTP entry before
    /// it executes the old session's exit steps.
    pub fn require_valid_mode_entries(&self) -> Result<(), ManifestError> {
        let mut activity_metadata = std::collections::BTreeMap::new();
        for (code, property) in &self.properties {
            require_valid_structured_text(property, code)?;
        }
        for (connection_id, connection) in &self.connections {
            require_valid_host_activities(connection, connection_id)?;
            require_valid_pcss_rendezvous(connection, connection_id)?;
            require_valid_object_transfer(self, connection, connection_id)?;
            require_valid_control_surfaces(self, connection, connection_id)?;
            for descriptor in &connection.activities {
                let key = (descriptor.id.clone(), descriptor.version);
                let value = (
                    descriptor.display_role.clone(),
                    descriptor.default_expected_duration_ms,
                    descriptor.interaction_required,
                );
                if let Some(previous) = activity_metadata.insert(key, value.clone()) {
                    if previous != value {
                        return Err(ManifestError::Contract(format!(
                            "connections.{connection_id}.activities activity '{}@{}' metadata differs from another descriptor",
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
                require_valid_ptp_steps(
                    &action.steps,
                    &format!("connections.{connection_id}.actions.{verb:?}"),
                )?;
                require_valid_executor_activities(
                    &action.activities,
                    action.steps.len(),
                    &format!("connections.{connection_id}.actions.{verb:?}.activities"),
                )?;
                for descriptor in &action.activities {
                    require_consistent_activity_metadata(
                        &mut activity_metadata,
                        descriptor,
                        &format!("connections.{connection_id}.actions.{verb:?}.activities"),
                    )?;
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

fn require_valid_pcss_rendezvous(
    connection: &Connection,
    connection_id: &str,
) -> Result<(), ManifestError> {
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
    if let Some(retries) = &connection.init_retries {
        let retry_path = format!("connections.{connection_id}.initRetries");
        let parsed_reasons: Option<Vec<u16>> = retries
            .when_reasons
            .iter()
            .map(|reason| parse_hex_code(reason))
            .collect();
        let Some(parsed_reasons) = parsed_reasons else {
            return Err(ManifestError::Contract(format!(
                "{retry_path}.whenReasons entries must be 16-bit hexadecimal codes"
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
    match contract.strategy {
        ObjectTransferStrategy::WholeObject if read.params.as_slice() != ["handle"] => {
            return Err(ManifestError::Contract(format!(
                "{path} wholeObject readAction must accept exactly [handle]"
            )));
        }
        ObjectTransferStrategy::Chunked
            if read.params.as_slice() != ["handle", "offset", "length"] =>
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
        if action.params.as_slice() != ["handle"] {
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

fn require_consistent_activity_metadata(
    seen: &mut std::collections::BTreeMap<
        (String, u32),
        (ConnectionActivityDisplayRole, u32, bool),
    >,
    descriptor: &ConnectionActivityDescriptor,
    path: &str,
) -> Result<(), ManifestError> {
    let key = (descriptor.id.clone(), descriptor.version);
    let value = (
        descriptor.display_role.clone(),
        descriptor.default_expected_duration_ms,
        descriptor.interaction_required,
    );
    if let Some(previous) = seen.insert(key, value.clone()) {
        if previous != value {
            return Err(ManifestError::Contract(format!(
                "{path} activity '{}@{}' metadata differs from another descriptor",
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
    use activity::{valid_activity_id, ConnectionActivityBinding};

    let mut ids = std::collections::BTreeSet::new();
    let mut checkpoints = std::collections::BTreeSet::new();
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
        let ConnectionActivityBinding::HostCheckpoint(binding) = &descriptor.binding else {
            return Err(ManifestError::Contract(format!(
                "{path} must use hostCheckpoint"
            )));
        };
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
        let mut array_binds = std::collections::BTreeSet::new();
        for capture in &step.captures {
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
            if retry.when_response_codes.is_empty() {
                return Err(ManifestError::Contract(format!(
                    "{step_path}.retry whenResponseCodes must not be empty"
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
            let mut nested = collections.clone();
            require_valid_ptp_steps_with_collections(
                &condition.then_steps,
                &format!("{step_path}.if"),
                &mut nested,
            )?;
        }
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
            || step
                .if_step
                .as_ref()
                .is_some_and(|condition| contains_ptp_loop(&condition.then_steps))
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
    fn wrong_schema_is_rejected_explicitly() {
        let text = SAMPLE.replace("camera-config/v1", "camera-config/v999");
        let m = CameraManifest::from_yaml(&text).unwrap();
        assert!(m.require_supported_schema().is_err());
    }
}
