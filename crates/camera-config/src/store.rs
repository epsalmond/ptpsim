//! `ConfigStore` — a resolved camera: a body manifest plus its manufacturer-tier
//! defaults. For the current corpus (one body) this is the whole store; the
//! multi-body tier-tree loader + identification funnel are deferred until a second
//! body lands (generic infra written once, not per-brand code).
//!
//! Its job today: resolve the version-ordering scheme from manufacturer defaults
//! and merge named values (body overrides manufacturer). The orthogonal-axis
//! queries live on [`CameraManifest`] (they need only the body); `ConfigStore`
//! adds the manufacturer-tier resolution on top.

use crate::model::{CameraManifest, ManufacturerDefaults, ValuePolicy};
use crate::version::VersionScheme;

#[derive(Debug, Clone)]
pub struct ConfigStore {
    pub manifest: CameraManifest,
    pub manufacturer: Option<ManufacturerDefaults>,
}

impl ConfigStore {
    /// A store over a single body manifest, no manufacturer defaults.
    pub fn new(manifest: CameraManifest) -> Self {
        ConfigStore {
            manifest,
            manufacturer: None,
        }
    }

    pub fn with_manufacturer(mut self, defaults: ManufacturerDefaults) -> Self {
        self.manufacturer = Some(defaults);
        self
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
