//! Shared fixtures for the FFI integration tests: the real vendored Fuji
//! config data, loaded the way a consuming app loads it. The body list lives
//! HERE ONLY — the loader requires a body per declared model, so adding a
//! model to fuji/index.yaml means extending `real_fuji_bodies()` once instead
//! of editing every test file (that hand-maintenance is how test call sites
//! drifted when a second model landed).
#![allow(dead_code)]

use camera_protocol_ffi::*;
use std::path::PathBuf;
use std::sync::Arc;

/// Read a fixture from `packages/camera-config-data`.
pub fn data(rel: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data")
        .join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// Every body the real Fuji manufacturer index declares, in index order.
pub fn real_fuji_bodies() -> Vec<KeyValue> {
    vec![
        KeyValue {
            key: "gfx100ii".to_string(),
            value: data("fuji/gfx100ii/gfx100ii.yaml"),
        },
        KeyValue {
            key: "fuji-generic".to_string(),
            value: data("fuji/fuji-generic/fuji-generic.yaml"),
        },
    ]
}

/// The real Fuji index + bodies, as a vendored consumer constructs it.
pub fn real_fuji_store() -> Arc<ConfigStore> {
    ConfigStore::from_manufacturer_index(data("fuji/index.yaml"), real_fuji_bodies())
        .expect("manufacturer index loads")
}

/// `real_fuji_bodies()` with one model's body text replaced — for tests that
/// exercise a mutated body against the real index.
pub fn real_fuji_bodies_with(model: &str, body: String) -> Vec<KeyValue> {
    let mut bodies = real_fuji_bodies();
    let slot = bodies
        .iter_mut()
        .find(|kv| kv.key == model)
        .unwrap_or_else(|| panic!("model {model} not in real_fuji_bodies"));
    slot.value = body;
    bodies
}
