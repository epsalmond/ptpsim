//! `ConfigStore` — a resolved camera: a body manifest plus its manufacturer-tier
//! defaults. For the current corpus (one body) this is the whole store; the
//! multi-body tier-tree loader + identification funnel are deferred until a second
//! body lands (generic infra written once, not per-brand code).
//!
//! Its job today: resolve the version-ordering scheme from manufacturer defaults
//! and merge named values (body overrides manufacturer). The orthogonal-axis
//! queries live on [`CameraManifest`] (they need only the body); `ConfigStore`
//! adds the manufacturer-tier resolution on top.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::error::ConfigError;
use crate::index::{EstablishmentBlock, ResolvedManufacturerIndex, Signature, Step};
use crate::model::{
    parse_hex_bytes, parse_hex_code, CameraInitiatedMetadataPhase, CameraInitiatedMonitorRecovery,
    CameraManifest, ManufacturerDefaults, ModeEntryExecution, SocketRole, TransferCompletion,
    TriggerMatch, ValuePolicy,
};
use crate::version::VersionScheme;

#[derive(Debug, Clone)]
pub struct ConfigStore {
    pub manifest: CameraManifest,
    pub manufacturer: Option<ManufacturerDefaults>,
    /// Present when loaded via [`ConfigStore::from_manufacturer_index`]: the
    /// resolved manufacturer index (signatures, family-merged BLE blocks,
    /// per-model views). Absent on single-body loads.
    pub index: Option<ResolvedManufacturerIndex>,
    /// All model body manifests keyed by model id. Empty on single-body
    /// loads. On manufacturer-index loads, the primary body in `manifest` is
    /// also present here under its model id.
    pub bodies: BTreeMap<String, CameraManifest>,
    /// Manufacturer-index-resolved camera-initiated transfer by model id. Empty
    /// for single-body stores, which lack a shared GATT catalog.
    pub resolved_camera_initiated_transfer_by_model:
        BTreeMap<String, ResolvedCameraInitiatedTransfer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCameraInitiatedTransfer {
    pub trigger_match: TriggerMatch,
    pub trigger_states: Vec<ResolvedBleStateTrigger>,
    pub connection: String,
    pub socket_role: SocketRole,
    pub endpoint_host: Option<String>,
    pub endpoint_port: u16,
    pub cached_credentials_allowed: bool,
    pub monitor_recovery: Option<CameraInitiatedMonitorRecovery>,
    pub function_launch: Option<ResolvedBleLiteralWrite>,
    pub mode: String,
    pub count_property: u16,
    pub count_member: u16,
    pub head_index: u32,
    pub metadata_operation: u16,
    pub metadata_phases: Vec<CameraInitiatedMetadataPhase>,
    pub data_operation: u16,
    pub chunk_limit_property: u16,
    pub completion: TransferCompletion,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBleStateTrigger {
    pub gatt_uuid: String,
    pub trigger_values: Vec<Vec<u8>>,
    pub baseline_values: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBleLiteralWrite {
    pub gatt_uuid: String,
    pub value: Vec<u8>,
    pub required: bool,
}

impl ConfigStore {
    /// A store over a single body manifest, no manufacturer defaults.
    pub fn new(manifest: CameraManifest) -> Self {
        ConfigStore {
            manifest,
            manufacturer: None,
            index: None,
            bodies: BTreeMap::new(),
            resolved_camera_initiated_transfer_by_model: BTreeMap::new(),
        }
    }

    pub fn with_manufacturer(mut self, defaults: ManufacturerDefaults) -> Self {
        self.manufacturer = Some(defaults);
        self
    }

    /// Load a manufacturer index plus every model body it references
    /// (plan §3.1 / §11.10). Fail-fast: any error aborts the whole load.
    ///
    /// `model_bodies` maps each declared `models[*].id` to the body manifest's
    /// YAML text. Missing entries → [`ConfigError::MissingModelBody`]; extras
    /// not referenced by the index are silently ignored. Returns an
    /// [`Arc<Self>`] so the iOS FFI (uniffi) can share the store across the
    /// async hot path without re-parsing.
    pub fn from_manufacturer_index(
        index_yaml: &str,
        model_bodies: BTreeMap<String, String>,
    ) -> Result<Arc<Self>, ConfigError> {
        let index = ResolvedManufacturerIndex::from_yaml(index_yaml)?;
        let mut bodies: BTreeMap<String, CameraManifest> = BTreeMap::new();
        for model_view in &index.models {
            let id = &model_view.id;
            let body_text = model_bodies
                .get(id)
                .ok_or_else(|| ConfigError::MissingModelBody { id: id.clone() })?;
            let body: CameraManifest =
                serde_yaml::from_str(body_text).map_err(|err| ConfigError::BodyParse {
                    id: id.clone(),
                    err,
                })?;
            body.require_supported_schema()
                .map_err(|err| ConfigError::Validation {
                    path: format!("models.{id}.schema"),
                    message: err.to_string(),
                })?;
            body.require_valid_mode_entries()
                .map_err(|err| ConfigError::Validation {
                    path: format!("models.{id}.connections"),
                    message: err.to_string(),
                })?;
            validate_reestablishment_params(id, &body, model_view)?;
            validate_pcss_camera_names(id, &body, model_view)?;
            validate_host_establishment_scopes(id, &body, model_view)?;
            bodies.insert(id.clone(), body);
        }
        validate_activity_metadata_consistency(&index, &bodies)?;
        validate_merged_activity_ids(&index, &bodies)?;
        // Plan §3.1: "the primary manifest" semantics — the first declared
        // model's body. Single-model indices (the MVP case) trivially get
        // the only body. Callers wanting a different model look up by id
        // via `bodies`.
        let primary_id =
            index
                .models
                .first()
                .map(|m| m.id.clone())
                .ok_or_else(|| ConfigError::Validation {
                    path: "models".to_string(),
                    message: "manufacturer index declares zero models".to_string(),
                })?;
        let manifest = bodies
            .get(&primary_id)
            .cloned()
            .expect("loop above inserted every model id");
        let mut resolved_camera_initiated_transfer_by_model = BTreeMap::new();
        for model_view in &index.models {
            let body = bodies
                .get(&model_view.id)
                .expect("all indexed model bodies were loaded");
            let gatt = model_view
                .ble
                .as_ref()
                .map(|ble| &ble.gatt)
                .cloned()
                .unwrap_or_default();
            if let Some(transfer) = &body.camera_initiated_transfer {
                validate_camera_initiated_monitor_recovery(&model_view.id, transfer, model_view)?;
                let resolved =
                    resolve_camera_initiated_transfer(&model_view.id, transfer, &gatt, body)?;
                resolved_camera_initiated_transfer_by_model.insert(model_view.id.clone(), resolved);
            }
        }
        Ok(Arc::new(ConfigStore {
            manifest,
            manufacturer: None,
            index: Some(index),
            bodies,
            resolved_camera_initiated_transfer_by_model,
        }))
    }

    /// Look up a model body by id (only useful after
    /// [`Self::from_manufacturer_index`]; single-body loads have an empty
    /// `bodies` map).
    pub fn body(&self, model_id: &str) -> Option<&CameraManifest> {
        self.bodies.get(model_id)
    }

    /// Select one indexed model as the direct-query body while retaining the
    /// manufacturer defaults and index context. This prevents APIs that read
    /// `manifest` from silently operating on the index's first declared body.
    pub fn model_store(&self, model_id: &str) -> Option<Self> {
        let manifest = self.bodies.get(model_id)?.clone();
        Some(Self {
            manifest,
            manufacturer: self.manufacturer.clone(),
            index: self.index.clone(),
            bodies: self.bodies.clone(),
            resolved_camera_initiated_transfer_by_model: self
                .resolved_camera_initiated_transfer_by_model
                .clone(),
        })
    }

    pub fn camera_initiated_transfer(
        &self,
        model_id: &str,
    ) -> Option<&ResolvedCameraInitiatedTransfer> {
        self.resolved_camera_initiated_transfer_by_model
            .get(model_id)
    }

    /// The version-ordering scheme this camera uses: the manufacturer's
    /// `versionOrder`, mapped to a known scheme. **Fail-soft:** an absent or
    /// unrecognized name falls back to the default (`dotted-int`).
    pub fn version_scheme(&self) -> VersionScheme {
        match self
            .manufacturer
            .as_ref()
            .and_then(|m| m.version_order.as_deref())
        {
            Some("dotted-int") => VersionScheme::DottedInt,
            _ => VersionScheme::default(),
        }
    }

    /// Resolve a named value by policy, body overriding manufacturer defaults.
    pub fn value(&self, key: &str) -> Option<&ValuePolicy> {
        self.manifest
            .values
            .get(key)
            .or_else(|| self.manufacturer.as_ref().and_then(|m| m.values.get(key)))
    }

    /// Connections available under this camera's firmware, using the resolved
    /// version scheme. Convenience over [`CameraManifest::connections_available`].
    pub fn connections_available(&self) -> Vec<&str> {
        let fw = self.manifest.camera.firmware.clone();
        self.manifest
            .connections
            .iter()
            .filter(|(_, c)| {
                c.available_when
                    .as_ref()
                    .is_none_or(|w| w.matches(&fw, self.version_scheme()))
            })
            .map(|(id, _)| id.as_str())
            .collect()
    }
}

fn validate_pcss_camera_names(
    model_id: &str,
    body: &CameraManifest,
    model_view: &crate::index::ModelView,
) -> Result<(), ConfigError> {
    for (signature_name, signature) in &model_view.signatures {
        let Signature::PcssNotify(signature) = signature else {
            continue;
        };
        let connection_id = &signature.suggests.connection;
        let Some(camera_name) = body
            .connections
            .get(connection_id)
            .and_then(|connection| connection.knock.as_ref())
            .and_then(|knock| knock.camera_name.as_deref())
        else {
            continue;
        };
        if camera_name != signature.require.camera_name {
            return Err(ConfigError::Validation {
                path: format!(
                    "models.{model_id}.connections.{connection_id}.knock.cameraName"
                ),
                message: format!(
                    "cameraName '{camera_name}' does not match resolved PCSS NOTIFY signature '{signature_name}' cameraName '{}'",
                    signature.require.camera_name
                ),
            });
        }
    }
    Ok(())
}

fn validate_activity_metadata_consistency(
    index: &ResolvedManufacturerIndex,
    bodies: &BTreeMap<String, CameraManifest>,
) -> Result<(), ConfigError> {
    use crate::ConnectionActivityDescriptor;

    type ActivityMetadata = (
        crate::ConnectionActivityDisplayRole,
        u32,
        bool,
        bool,
        crate::activity::ConnectionActivityIdentity,
        String,
    );

    let mut seen: BTreeMap<(String, u32), ActivityMetadata> = BTreeMap::new();
    let mut check = |descriptor: &ConnectionActivityDescriptor, path: String| {
        let key = (descriptor.id.clone(), descriptor.version);
        let metadata = (
            descriptor.display_role.clone(),
            descriptor.default_expected_duration_ms,
            descriptor.interaction_required,
            descriptor.optional,
            descriptor.identity(),
        );
        if let Some((role, duration, interaction, optional, identity, first_path)) = seen.get(&key)
        {
            if (&metadata.0, metadata.1, metadata.2, metadata.3, &metadata.4)
                != (role, *duration, *interaction, *optional, identity)
            {
                return Err(ConfigError::Validation {
                    path,
                    message: format!(
                        "activity '{}@{}' metadata differs or binding identity differs from {first_path}",
                        descriptor.id, descriptor.version
                    ),
                });
            }
        } else {
            seen.insert(
                key,
                (
                    metadata.0, metadata.1, metadata.2, metadata.3, metadata.4, path,
                ),
            );
        }
        Ok(())
    };

    for model in &index.models {
        if let Some(ble) = &model.ble {
            for (mechanism, establishment) in &ble.establishments {
                for (activity_index, descriptor) in establishment.activities.iter().enumerate() {
                    check(
                        descriptor,
                        format!(
                            "models.{}.establishments.{mechanism}.activities[{activity_index}]",
                            model.id
                        ),
                    )?;
                }
            }
        }
        if let Some(body) = bodies.get(&model.id) {
            for (connection_id, connection) in &body.connections {
                for (activity_index, descriptor) in connection.activities.iter().enumerate() {
                    check(
                        descriptor,
                        format!(
                            "models.{}.connections.{connection_id}.activities[{activity_index}]",
                            model.id
                        ),
                    )?;
                }
                for (entry_index, entry) in connection.entries.iter().enumerate() {
                    for (activity_index, descriptor) in entry.activities.iter().enumerate() {
                        check(
                            descriptor,
                            format!(
                                "models.{}.connections.{connection_id}.entries[{entry_index}].activities[{activity_index}]",
                                model.id
                            ),
                        )?;
                    }
                }
                for (verb, action) in &connection.actions {
                    if let Some(initiator) = &action.initiator {
                        for (activity_index, descriptor) in initiator.activities.iter().enumerate()
                        {
                            check(
                                descriptor,
                                format!(
                                    "models.{}.connections.{connection_id}.actions.{verb:?}.initiator.activities[{activity_index}]",
                                    model.id
                                ),
                            )?;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn validate_merged_activity_ids(
    index: &ResolvedManufacturerIndex,
    bodies: &BTreeMap<String, CameraManifest>,
) -> Result<(), ConfigError> {
    for model in &index.models {
        let Some(ble) = &model.ble else {
            continue;
        };
        let Some(body) = bodies.get(&model.id) else {
            continue;
        };
        for (connection_id, connection) in &body.connections {
            let Some(mechanism) = &connection.establishment else {
                continue;
            };
            let Some(establishment) = ble.establishment(mechanism) else {
                continue;
            };
            let establishment_ids: BTreeSet<_> = establishment
                .activities
                .iter()
                .map(|activity| activity.id.as_str())
                .collect();
            for (activity_index, activity) in connection.activities.iter().enumerate() {
                if establishment_ids.contains(activity.id.as_str()) {
                    return Err(ConfigError::Validation {
                        path: format!(
                            "models.{}.connections.{connection_id}.activities[{activity_index}].id",
                            model.id
                        ),
                        message: format!(
                            "activity id '{}' duplicates an activity in referenced establishment '{mechanism}'",
                            activity.id
                        ),
                    });
                }
            }
        }
    }
    Ok(())
}

fn validate_reestablishment_params(
    model_id: &str,
    body: &CameraManifest,
    model_view: &crate::index::ModelView,
) -> Result<(), ConfigError> {
    for (connection_id, connection) in &body.connections {
        for (index, transition) in connection.enables.iter().enumerate() {
            if transition.params.is_empty() {
                continue;
            }
            let path =
                format!("models.{model_id}.connections.{connection_id}.enables[{index}].params");
            let mechanism =
                transition
                    .mechanism
                    .as_deref()
                    .ok_or_else(|| ConfigError::Validation {
                        path: path.clone(),
                        message: "parameter bindings require an establishment mechanism".into(),
                    })?;
            let establishment = model_view
                .ble
                .as_ref()
                .and_then(|ble| ble.establishment(mechanism))
                .ok_or_else(|| ConfigError::Validation {
                    path: path.clone(),
                    message: format!("unknown establishment mechanism '{mechanism}'"),
                })?;
            let actual: Vec<&str> = transition.params.keys().map(String::as_str).collect();
            let mut expected: Vec<&str> = establishment.params.iter().map(String::as_str).collect();
            expected.sort_unstable();
            if actual != expected {
                return Err(ConfigError::Validation {
                    path,
                    message: format!(
                        "parameter bindings {actual:?} do not exactly match establishment parameters {expected:?}"
                    ),
                });
            }
        }
        for (index, entry) in connection.entries.iter().enumerate() {
            let ModeEntryExecution::ReestablishConnection(reestablish) = &entry.execution else {
                continue;
            };
            let path = format!(
                "models.{model_id}.connections.{connection_id}.entries[{index}].reestablishConnection.params"
            );
            let mechanism = connection
                .establishment
                .as_deref()
                .expect("manifest contract validation requires establishment");
            let establishment = model_view
                .ble
                .as_ref()
                .and_then(|ble| ble.establishment(mechanism))
                .ok_or_else(|| ConfigError::Validation {
                    path: path.clone(),
                    message: format!("unknown establishment mechanism '{mechanism}'"),
                })?;
            let actual: Vec<&str> = reestablish.params.keys().map(String::as_str).collect();
            let mut expected: Vec<&str> = establishment.params.iter().map(String::as_str).collect();
            expected.sort_unstable();
            if actual != expected {
                return Err(ConfigError::Validation {
                    path,
                    message: format!(
                        "parameter bindings {actual:?} do not exactly match establishment parameters {expected:?}"
                    ),
                });
            }
        }
    }
    Ok(())
}

fn validate_host_establishment_scopes(
    model_id: &str,
    body: &CameraManifest,
    model_view: &crate::index::ModelView,
) -> Result<(), ConfigError> {
    use crate::{ConnectionActivityBinding, ConnectionActivityHostEstablishment};

    for (connection_id, connection) in &body.connections {
        for (activity_index, descriptor) in connection.activities.iter().enumerate() {
            let ConnectionActivityBinding::HostEstablishment(binding) = &descriptor.binding else {
                continue;
            };
            let ConnectionActivityHostEstablishment::NetworkIdentityExact {
                network_identity_exact,
            } = &binding.host_establishment
            else {
                continue;
            };
            let path = format!(
                "models.{model_id}.connections.{connection_id}.activities[{activity_index}].hostEstablishment.networkIdentityExact.expectedScope"
            );
            let mechanism =
                connection
                    .establishment
                    .as_deref()
                    .ok_or_else(|| ConfigError::Validation {
                        path: path.clone(),
                        message: "networkIdentityExact requires a selected establishment"
                            .to_string(),
                    })?;
            let establishment = model_view
                .ble
                .as_ref()
                .and_then(|ble| ble.establishment(mechanism))
                .ok_or_else(|| ConfigError::Validation {
                    path: path.clone(),
                    message: format!("unknown establishment mechanism '{mechanism}'"),
                })?;
            let available = establishment_scope_outputs(establishment);
            if !available.contains(&network_identity_exact.expected_scope) {
                return Err(ConfigError::Validation {
                    path,
                    message: format!(
                        "expected scope '{}' is not persisted or produced by establishment '{mechanism}'",
                        network_identity_exact.expected_scope
                    ),
                });
            }
        }
    }
    Ok(())
}

fn establishment_scope_outputs(establishment: &EstablishmentBlock) -> BTreeSet<String> {
    let mut outputs = establishment.persist.iter().cloned().collect();
    collect_step_scope_outputs(&establishment.post_exit_readiness, &mut outputs);
    collect_step_scope_outputs(&establishment.steps, &mut outputs);
    outputs
}

fn collect_step_scope_outputs(steps: &[Step], outputs: &mut BTreeSet<String>) {
    for step in steps {
        match step {
            Step::BleRead(step) => {
                outputs.insert(step.capture_as.clone());
            }
            Step::BlePeripheralName(step) => {
                outputs.insert(step.capture_as.clone());
            }
            Step::BleNotify(step) => {
                if let Some(name) = &step.capture_as {
                    outputs.insert(name.clone());
                }
                outputs.extend(step.capture.iter().map(|capture| capture.name.clone()));
            }
            Step::BleAwaitUntil(step) => {
                if let Some(name) = &step.capture_as {
                    outputs.insert(name.clone());
                }
                outputs.extend(step.capture.iter().map(|capture| capture.name.clone()));
                collect_step_scope_outputs(&step.on_each, outputs);
            }
            Step::Acquire(step) => {
                outputs.insert(step.name.clone());
                collect_step_scope_outputs(std::slice::from_ref(step.from.as_ref()), outputs);
            }
            Step::AcquireFirmware(_) => {
                outputs.insert("firmware".to_string());
            }
            Step::If(step) => {
                collect_step_scope_outputs(&step.then, outputs);
                collect_step_scope_outputs(&step.else_branch, outputs);
            }
            Step::Retry(step) => {
                collect_step_scope_outputs(&step.steps, outputs);
            }
            Step::NikonLssReadConnectionConfiguration(step) => {
                outputs.extend([
                    step.flags_capture_as.clone(),
                    step.ssid_capture_as.clone(),
                    step.password_capture_as.clone(),
                    step.security_mode_capture_as.clone(),
                ]);
                if let Some(name) = &step.spp_max_length_capture_as {
                    outputs.insert(name.clone());
                }
            }
            Step::BleConnect(_)
            | Step::BleDelay(_)
            | Step::BleAwaitDisconnect(_)
            | Step::BleRequestMtu(_)
            | Step::BleDiscoverServices(_)
            | Step::BleWrite(_)
            | Step::BleSubscribe(_)
            | Step::BleWriteChunk(_)
            | Step::NikonLssAuthenticate(_) => {}
        }
    }
}

fn validate_camera_initiated_monitor_recovery(
    model_id: &str,
    transfer: &crate::model::CameraInitiatedTransfer,
    model_view: &crate::index::ModelView,
) -> Result<(), ConfigError> {
    let Some(CameraInitiatedMonitorRecovery::SavedCameraReconnect) = transfer.monitor_recovery
    else {
        return Ok(());
    };
    let path = format!("models.{model_id}.cameraInitiatedTransfer.monitorRecovery");
    let Some(ble) = model_view.ble.as_ref() else {
        return Err(ConfigError::Validation {
            path,
            message: "savedCameraReconnect requires a BLE reconnect policy".to_string(),
        });
    };
    if ble.reconnect.is_none() {
        return Err(ConfigError::Validation {
            path,
            message: "savedCameraReconnect requires ble.reconnect".to_string(),
        });
    }
    let has_reconnect_route = model_view.signatures.iter().any(|(_, signature)| {
        matches!(signature, Signature::BleAdvert(advert) if advert.reconnect.is_some())
    });
    if !has_reconnect_route {
        return Err(ConfigError::Validation {
            path,
            message: "savedCameraReconnect requires at least one BLE reconnect route".to_string(),
        });
    }
    Ok(())
}

fn resolve_camera_initiated_transfer(
    model_id: &str,
    transfer: &crate::model::CameraInitiatedTransfer,
    gatt: &BTreeMap<String, String>,
    body: &CameraManifest,
) -> Result<ResolvedCameraInitiatedTransfer, ConfigError> {
    let path = format!("models.{model_id}.cameraInitiatedTransfer");
    let resolve_gatt = |name: &str, suffix: &str| {
        crate::index::parse::resolve_one_gatt_name(name, gatt, &format!("{path}.{suffix}"))
    };
    let parse_bytes = |value: &str, suffix: &str| {
        parse_hex_bytes(value).ok_or_else(|| ConfigError::Validation {
            path: format!("{path}.{suffix}"),
            message: format!("invalid hex bytes '{value}'"),
        })
    };
    let parse_code = |value: &str, suffix: &str| {
        parse_hex_code(value).ok_or_else(|| ConfigError::Validation {
            path: format!("{path}.{suffix}"),
            message: format!("invalid PTP code '{value}'"),
        })
    };

    let trigger_states = transfer
        .trigger
        .states
        .iter()
        .enumerate()
        .map(|(i, state)| {
            Ok(ResolvedBleStateTrigger {
                gatt_uuid: resolve_gatt(&state.gatt, &format!("trigger.states[{i}]"))?,
                trigger_values: state
                    .trigger_values
                    .iter()
                    .enumerate()
                    .map(|(j, value)| {
                        parse_bytes(value, &format!("trigger.states[{i}].triggerValues[{j}]"))
                    })
                    .collect::<Result<_, _>>()?,
                baseline_values: state
                    .baseline_values
                    .iter()
                    .enumerate()
                    .map(|(j, value)| {
                        parse_bytes(value, &format!("trigger.states[{i}].baselineValues[{j}]"))
                    })
                    .collect::<Result<_, _>>()?,
            })
        })
        .collect::<Result<Vec<_>, ConfigError>>()?;

    let connection = body
        .connections
        .get(&transfer.handoff.connection)
        .ok_or_else(|| ConfigError::Validation {
            path: format!("{path}.handoff.connection"),
            message: format!("unknown connection '{}'", transfer.handoff.connection),
        })?;
    let bindings = connection
        .bindings
        .as_ref()
        .ok_or_else(|| ConfigError::Validation {
            path: format!("{path}.handoff.socketRole"),
            message: "connection has no socket bindings".to_string(),
        })?;
    let endpoint_port = bindings
        .port_for(transfer.handoff.socket_role)
        .ok_or_else(|| ConfigError::Validation {
            path: format!("{path}.handoff.socketRole"),
            message: format!(
                "connection does not bind role '{:?}'",
                transfer.handoff.socket_role
            ),
        })?;

    let function_launch = transfer
        .handoff
        .function_launch
        .as_ref()
        .map(|launch| {
            Ok(ResolvedBleLiteralWrite {
                gatt_uuid: resolve_gatt(&launch.gatt, "handoff.functionLaunch")?,
                value: parse_bytes(&launch.value, "handoff.functionLaunch.value")?,
                required: launch.required,
            })
        })
        .transpose()?;

    Ok(ResolvedCameraInitiatedTransfer {
        trigger_match: transfer.trigger.match_mode,
        trigger_states,
        connection: transfer.handoff.connection.clone(),
        socket_role: transfer.handoff.socket_role,
        endpoint_host: bindings.host.clone(),
        endpoint_port,
        cached_credentials_allowed: transfer.handoff.cached_credentials_allowed,
        monitor_recovery: transfer.monitor_recovery,
        function_launch,
        mode: transfer.receive.mode.clone(),
        count_property: parse_code(&transfer.receive.count.property, "receive.count.property")?,
        count_member: parse_code(&transfer.receive.count.member, "receive.count.member")?,
        head_index: transfer.receive.head_index,
        metadata_operation: parse_code(
            &transfer.receive.metadata.operation,
            "receive.metadata.operation",
        )?,
        metadata_phases: transfer.receive.metadata.phases.clone(),
        data_operation: parse_code(&transfer.receive.data.operation, "receive.data.operation")?,
        chunk_limit_property: parse_code(
            &transfer.receive.data.chunk_limit_property,
            "receive.data.chunkLimitProperty",
        )?,
        completion: transfer.receive.completion,
        evidence: transfer.evidence.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ManufacturerDefaults;

    const BODY: &str = r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
values:
  sessionId: { type: generated, scheme: uuidv4 }
connections:
  instax-printer: { ref: instax-printer, availableWhen: { firmware: { lt: "2.40" } } }
"#;

    fn fuji() -> ManufacturerDefaults {
        ManufacturerDefaults::from_yaml(
            r#"
manufacturer: FUJIFILM
versionOrder: dotted-int
values:
  initiatorGuid: { type: fixed, value: "f2e4538f-..." }
  sessionId: { type: fixed, value: "should-be-overridden" }
"#,
        )
        .unwrap()
    }

    fn store() -> ConfigStore {
        ConfigStore::new(CameraManifest::from_yaml(BODY).unwrap()).with_manufacturer(fuji())
    }

    #[test]
    fn version_scheme_resolves_with_failsoft_default() {
        assert_eq!(store().version_scheme(), VersionScheme::DottedInt);
        // No manufacturer / unknown name -> default scheme, no panic.
        let bare = ConfigStore::new(CameraManifest::from_yaml(BODY).unwrap());
        assert_eq!(bare.version_scheme(), VersionScheme::DottedInt);
    }

    #[test]
    fn value_body_overrides_manufacturer() {
        let s = store();
        // Body defines sessionId -> body wins over the manufacturer's fixed value.
        assert!(matches!(
            s.value("sessionId"),
            Some(ValuePolicy::Generated { .. })
        ));
        // Manufacturer-only value falls through.
        assert!(matches!(
            s.value("initiatorGuid"),
            Some(ValuePolicy::Fixed { .. })
        ));
        assert!(s.value("nope").is_none());
    }

    #[test]
    fn connections_available_uses_camera_firmware() {
        // firmware "2.30" in the body -> instax available.
        assert!(store().connections_available().contains(&"instax-printer"));
    }

    #[test]
    fn model_store_selects_requested_body_for_direct_queries() {
        let mut indexed = store();
        let second = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: X-T5, firmware: "4.20" }
connections:
  app: { kind: ptpip }
"#,
        )
        .unwrap();
        indexed.bodies.insert("xt5".into(), second);

        let selected = indexed.model_store("xt5").expect("indexed body exists");
        assert_eq!(selected.manifest.camera.model, "X-T5");
        assert!(selected.manifest.connections.contains_key("app"));
        assert!(selected.value("initiatorGuid").is_some());
        assert!(indexed.model_store("missing").is_none());
    }
}
