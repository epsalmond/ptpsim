//! #100 — the four live-control families (aperture, ISO, shutter, exposure-bias)
//! carry value→label tables in the consolidated manifest, sourced from the
//! client application catalog through the evidence→generator pipeline (app-source
//! provenance). The labels live in the GENERATED consolidated (the manifest the
//! service + app load), not the curated base, so this asserts against it.
//!
//! Keys reflect each property's real wire datatype: aperture u16, ISO/shutter u32
//! (0x80000000-flag forms), exposure-bias signed i16 (#88) — so its keys are the
//! signed milliEV the codec decodes, not client application's u16 bit-pattern.

use std::path::PathBuf;

use camera_config::CameraManifest;

fn consolidated() -> CameraManifest {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data/fuji/gfx100ii/gfx100ii.consolidated.yaml");
    let yaml = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    CameraManifest::from_yaml(&yaml).expect("consolidated manifest loads")
}

#[test]
fn aperture_labels_resolve_from_the_consolidated_manifest() {
    let m = consolidated();
    // u16 raw = f-stop × 100; labels are client application's catalog strings.
    assert_eq!(m.value_label(0x5007, 280), Some("F2.8"));
    assert_eq!(m.value_label(0x5007, 3200), Some("F32"));
    assert_eq!(m.value_label(0x5007, 999), None); // an unlabeled raw value
}

#[test]
fn iso_labels_resolve_as_u32_literals() {
    let m = consolidated();
    // 0xD02A is u32 (override): manual ISO is the literal value.
    assert_eq!(m.value_label(0xd02a, 6400), Some("6400"));
    assert_eq!(m.value_label(0xd02a, 102400), Some("102400")); // overflows u16
}

#[test]
fn movie_iso_uses_the_same_u32_encoding_shape_but_separate_legality() {
    let m = consolidated();
    // 0xD02B uses the same u32 raw encoding shape. Its legal list is not
    // inherited from 0xD02A; issue #195 keeps per-body legality scoped per prop.
    assert_eq!(m.value_label(0xd02b, 6400), Some("6400"));
    assert_eq!(m.value_label(0xd02b, 102400), Some("102400")); // overflows u16
    assert!(
        m.value_profile_for(0xd02b, "app", "shooting/video")
            .is_none(),
        "movie live-view ISO must not inherit still live-view legality"
    );
}

#[test]
fn iso_auto_ceiling_sentinels_label_as_auto_with_the_ceiling() {
    let m = consolidated();
    // 0x80000000 | ceiling is the auto form, distinguished from the manual literal
    // that shares the low bytes (#107). 0x80003200 = auto, ceiling 12800;
    // 0x80001900 = auto, ceiling 6400.
    assert_eq!(m.value_label(0xd02a, 0x8000_3200), Some("AUTO 12800"));
    assert_eq!(m.value_label(0xd02b, 0x8000_1900), Some("AUTO 6400"));
}

#[test]
fn still_iso_exposes_value_rows_and_generic_sentinel_metadata() {
    let m = consolidated();
    let p = m.property(0xd02a).expect("still ISO property exists");
    assert_eq!(p.value_rows[0].label, "6400");
    assert_eq!(p.value_rows[0].raw, 6400);
    let sentinel = p
        .value_encoding
        .as_ref()
        .and_then(|enc| enc.sentinel.as_ref())
        .expect("generic sentinel descriptor");
    assert_eq!(sentinel.mask, 0x8000_0000);
    assert_eq!(sentinel.meaning.as_deref(), Some("autoCeiling"));
    assert_eq!(
        m.decode_property_label(0xd02a, 0x8000_1900).as_deref(),
        Some("AUTO 6400")
    );
    assert_eq!(
        m.encode_property_raw(0xd02a, "AUTO 6400"),
        Some(0x8000_1900)
    );
    assert!(p
        .value_encoding
        .as_ref()
        .expect("encoding")
        .masks
        .iter()
        .any(|mask| mask.mask == 0x4000_0000
            && mask.meaning.as_deref() == Some("extendedSensitivity")));
    assert_eq!(m.value_label(0xd02a, 0x4000_6400), Some("25600"));
    let profile = m
        .value_profile_for(0xd02a, "app", "shooting/stills")
        .expect("still ISO profile");
    assert!(profile.rows.iter().any(|row| row.raw == 50 && !row.legal));
    assert!(profile
        .rows
        .iter()
        .any(|row| row.raw == 0x4000_6400 && row.aliases.contains(&25600)));
}

#[test]
fn shutter_labels_resolve_from_the_high_bit_u32_form() {
    let m = consolidated();
    // 0xD240 is u32: 0x80000000 | (denom × 1000). 1/60 = 0x8000_EA60.
    assert_eq!(m.value_label(0xd240, 0x8000_EA60), Some("1/60"));
    assert_eq!(m.value_label(0xd240, 0x8000_9C40), Some("1/40"));
}

#[test]
fn exposure_bias_labels_key_on_signed_milli_ev() {
    let m = consolidated();
    // 0x5010 is i16 (#88): keys are signed milliEV, not the u16 bit-pattern.
    assert_eq!(m.value_label(0x5010, -333), Some("-0.3"));
    assert_eq!(m.value_label(0x5010, 0), Some("0"));
    assert_eq!(m.value_label(0x5010, 333), Some("+0.3"));
    assert_eq!(m.value_label(0x5010, -5000), Some("-5.0"));
}
