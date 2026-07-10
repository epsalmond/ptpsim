//! `ConfigStore` — a resolved camera: a body manifest plus its manufacturer-tier
//! defaults. For the current corpus (one body) this is the whole store; the
//! multi-body tier-tree loader + identification funnel are deferred until a second
//! body lands (generic infra written once, not per-brand code).
//!
//! Its job today: resolve the version-ordering scheme from manufacturer defaults
//! and merge named values (body overrides manufacturer). The orthogonal-axis
//! queries live on [`CameraManifest`] (they need only the body); `ConfigStore`
//! adds the manufacturer-tier resolution on top.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::error::ConfigError;
use crate::index::ResolvedManufacturerIndex;
use crate::model::{
    parse_hex_bytes, parse_hex_code, CameraManifest, ManufacturerDefaults, SocketRole,
    TransferCompletion, TriggerMatch, ValuePolicy,
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
    pub function_launch: Option<ResolvedBleLiteralWrite>,
    pub mode: String,
    pub count_property: u16,
    pub count_member: u16,
    pub head_index: u32,
    pub metadata_operation: u16,
    pub metadata_before_mode_entry: bool,
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
            bodies.insert(id.clone(), body);
        }
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
        function_launch,
        mode: transfer.receive.mode.clone(),
        count_property: parse_code(&transfer.receive.count.property, "receive.count.property")?,
        count_member: parse_code(&transfer.receive.count.member, "receive.count.member")?,
        head_index: transfer.receive.head_index,
        metadata_operation: parse_code(
            &transfer.receive.metadata.operation,
            "receive.metadata.operation",
        )?,
        metadata_before_mode_entry: transfer.receive.metadata.before_mode_entry,
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
}
