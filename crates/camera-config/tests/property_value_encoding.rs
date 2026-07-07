use camera_config::CameraManifest;

const YAML: &str = r#"
schema: camera-config/v1
camera: { manufacturer: Test, model: Body, firmware: "1.0" }
properties:
  "0xd02a":
    name: stillIso
    type: u32
    access: readWrite
    valueRows:
      - { label: "6400", raw: 6400 }
      - { label: "12800", raw: 12800 }
      - { label: "25600", raw: 1073767424 }
    valueEncoding:
      sentinel:
        mask: 2147483648
        meaning: autoCeiling
        labelPrefix: AUTO
      masks:
        - { mask: 1073741824, meaning: extendedSensitivity, labelPrefix: EXT }
"#;

#[test]
fn property_value_rows_and_sentinel_codec_are_manifest_driven() {
    let m = CameraManifest::from_yaml(YAML).expect("manifest loads");
    let p = m.property(0xd02a).expect("property exists");
    assert_eq!(p.value_rows.len(), 3);
    assert_eq!(
        p.value_encoding
            .as_ref()
            .and_then(|enc| enc.sentinel.as_ref())
            .expect("sentinel")
            .meaning
            .as_deref(),
        Some("autoCeiling")
    );

    assert_eq!(m.value_label(0xd02a, 6400), Some("6400"));
    assert_eq!(
        m.decode_property_label(0xd02a, 0x8000_1900).as_deref(),
        Some("AUTO 6400")
    );
    assert_eq!(m.encode_property_raw(0xd02a, "6400"), Some(6400));
    assert_eq!(
        m.encode_property_raw(0xd02a, "AUTO 6400"),
        Some(0x8000_1900)
    );
    assert_eq!(m.value_label(0xd02a, 0x4000_6400), Some("25600"));
    assert_eq!(
        m.decode_property_label(0xd02a, 0x4000_1900).as_deref(),
        Some("EXT 6400")
    );
    assert_eq!(m.encode_property_raw(0xd02a, "EXT 6400"), Some(0x4000_1900));
    assert_eq!(m.encode_property_raw(0xd02a, "AUTO 25600"), None);
}

#[test]
fn scoped_value_profiles_resolve_by_connection_and_mode() {
    let yaml = r#"
schema: camera-config/v1
camera: { manufacturer: Test, model: Body, firmware: "1.0" }
properties:
  "0xd02a":
    name: stillIso
    type: u32
    access: readWrite
    valueProfiles:
      - connection: app
        mode: shooting/stills
        rows:
          - { label: "80", raw: 80 }
          - { label: "50", raw: 50, legal: false, writeStoreRaw: 80 }
          - { label: "25600", raw: 1073767424, aliases: [25600] }
        evidence: [valueCapability]
"#;
    let m = CameraManifest::from_yaml(yaml).expect("manifest loads");
    let profile = m
        .value_profile_for(0xd02a, "app", "shooting/stills/manual")
        .expect("profile covers child stills mode");
    assert_eq!(profile.rows.len(), 3);
    assert!(profile.rows.iter().any(|row| row.raw == 80 && row.legal));
    assert!(profile
        .rows
        .iter()
        .any(|row| row.raw == 50 && !row.legal && row.write_store_raw == Some(80)));
    let extended = m
        .property(0xd02a)
        .unwrap()
        .profile_row_for_write(profile, 25600)
        .expect("alias resolves to flagged canonical row");
    assert_eq!(extended.raw, 0x4000_6400);
}
