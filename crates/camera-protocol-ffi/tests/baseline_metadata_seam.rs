//! Family-baseline metadata exposed through the public config-store seam.

use camera_protocol_ffi::ConfigStore;

mod common;

#[test]
fn indexed_models_expose_baseline_metadata() {
    let store = common::real_fuji_store();

    assert_eq!(store.is_fallback_model("fuji-generic".into()), Some(true));
    assert_eq!(store.is_fallback_model("gfx100ii".into()), Some(false));
    assert_eq!(store.is_fallback_model("xa7".into()), Some(false));
    assert_eq!(store.is_fallback_model("unknown-model".into()), None);
}

#[test]
fn model_stores_preserve_baseline_metadata() {
    let store = common::real_fuji_store();

    for (model, fallback) in [("fuji-generic", true), ("gfx100ii", false), ("xa7", false)] {
        let model_store = store.model_store(model.into()).expect("indexed model");
        assert_eq!(model_store.is_fallback_model(model.into()), Some(fallback));
        assert_eq!(model_store.is_fallback_model("unknown-model".into()), None);
    }
}

#[test]
fn bundle_store_has_no_baseline_metadata() {
    let store = ConfigStore::from_bundle(
        common::data("fuji/fuji-generic/fuji-generic.yaml"),
        Some(common::data("fuji/fuji.yaml")),
    )
    .expect("body bundle loads");

    assert_eq!(store.is_fallback_model("fuji-generic".into()), None);
    assert_eq!(store.is_fallback_model("unknown-model".into()), None);
}
