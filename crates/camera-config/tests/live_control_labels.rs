//! The four live-control families (aperture, ISO, shutter, exposure-bias) carry
//! value→label tables in the consolidated manifest. ISO/shutter labels are
//! curated in the base manifest; aperture and exposure-bias labels come from
//! reviewed evidence at generation time — so these assertions exercise the
//! generated artifact loaded by services and apps.
//!
//! Keys reflect each property's real wire datatype: aperture u16, ISO/shutter u32
//! (0x80000000-flag forms), exposure-bias signed i16 (#88) — so its keys are the
//! signed milliEV the codec decodes, not a client's u16 bit-pattern.

use std::path::PathBuf;

use camera_config::CameraManifest;

fn consolidated() -> CameraManifest {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data/fuji/gfx100ii/gfx100ii.consolidated.yaml");
    let yaml = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    CameraManifest::from_yaml(&yaml).expect("consolidated manifest loads")
}

fn authored() -> CameraManifest {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data/fuji/gfx100ii/gfx100ii.yaml");
    let yaml = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    CameraManifest::from_yaml(&yaml).expect("authored manifest loads")
}

#[test]
fn aperture_labels_resolve_from_the_consolidated_manifest() {
    let m = consolidated();
    // u16 raw = f-stop × 100.
    assert_eq!(m.value_label(0x5007, 280), Some("F2.8"));
    assert_eq!(m.value_label(0x5007, 1100), Some("F11"));
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
fn algorithmic_iso_decode_covers_unlisted_extended_and_auto_values() {
    let m = authored();
    for (raw, expected) in [
        (0x4000_0028, "EXT 40"),
        (0x4000_0140, "EXT 320"),
        (0x8000_00c8, "AUTO 200"),
        (0x8000_3200, "AUTO 12800"),
    ] {
        assert_eq!(
            m.decode_property_label(0xd02a, raw).as_deref(),
            Some(expected),
            "raw 0x{raw:08x}"
        );
    }
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
    assert_eq!(
        m.decode_property_label(0xd02a, 0x4000_6400).as_deref(),
        Some("25600"),
        "an exact authored row wins over the EXT numeric decoder"
    );
    let profile = m
        .value_profile_for(0xd02a, "app", "shooting/stills")
        .expect("still ISO profile");
    assert!(profile.rows.iter().any(|row| row.raw == 50 && !row.legal));
    assert!(profile
        .rows
        .iter()
        .any(|row| row.raw == 0x8000_00a0 && !row.legal && row.write_store_raw == Some(80)));
    assert!(profile
        .rows
        .iter()
        .any(|row| row.raw == 0x8000_6400 && !row.legal && row.write_store_raw == Some(80)));
    assert!(profile
        .rows
        .iter()
        .any(|row| row.raw == 0x4000_6400 && row.aliases.contains(&25600)));
    assert!(
        m.value_profile_for(0xd02b, "app", "shooting/stills")
            .is_none(),
        "movie ISO must not inherit still ISO legality"
    );
    assert!(
        m.value_profile_for(0x500f, "wireless-tether", "shooting/stills")
            .is_none(),
        "PCSS ISO must not inherit still ISO legality"
    );
    assert!(
        m.value_profile_for(0xd242, "app", "shooting/video")
            .is_none(),
        "movie-mode sensitivity must not inherit still ISO legality"
    );
}

#[test]
fn shutter_labels_resolve_from_the_high_bit_u32_form() {
    let m = consolidated();
    // 0xD240 is u32: 0x80000000 | (denom × 1000). 1/60 = 0x8000_EA60.
    assert_eq!(m.value_label(0xd240, 0x8000_EA60), Some("1/60"));
    assert_eq!(m.value_label(0xd240, 0x8000_9C40), Some("1/40"));
}

#[test]
fn algorithmic_shutter_decode_covers_fractional_and_slow_values() {
    let m = authored();
    for (raw, expected) in [
        (0x8000_0000 | 4_000, "1/4"),
        (0x8000_0000 | 250_000, "1/250"),
        (0x8000_0000 | 8_000_000, "1/8000"),
        (1_000, "1\""),
        (2_500, "2.5\""),
        (60_000, "60\""),
    ] {
        assert_eq!(
            m.decode_property_label(0xd240, raw).as_deref(),
            Some(expected),
            "raw 0x{raw:08x}"
        );
    }
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
