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
    valueEncoding:
      sentinel:
        mask: 2147483648
        meaning: autoCeiling
        labelPrefix: AUTO
"#;

#[test]
fn property_value_rows_and_sentinel_codec_are_manifest_driven() {
    let m = CameraManifest::from_yaml(YAML).expect("manifest loads");
    let p = m.property(0xd02a).expect("property exists");
    assert_eq!(p.value_rows.len(), 2);
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
    assert_eq!(m.encode_property_raw(0xd02a, "AUTO 25600"), None);
}
