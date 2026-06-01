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
use crate::model::{CameraManifest, ManufacturerDefaults, ValuePolicy};
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
}

impl ConfigStore {
    /// A store over a single body manifest, no manufacturer defaults.
    pub fn new(manifest: CameraManifest) -> Self {
        ConfigStore {
            manifest,
            manufacturer: None,
            index: None,
            bodies: BTreeMap::new(),
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
        Ok(Arc::new(ConfigStore {
            manifest,
            manufacturer: None,
            index: Some(index),
            bodies,
        }))
    }

    /// Look up a model body by id (only useful after
    /// [`Self::from_manufacturer_index`]; single-body loads have an empty
    /// `bodies` map).
    pub fn body(&self, model_id: &str) -> Option<&CameraManifest> {
        self.bodies.get(model_id)
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
