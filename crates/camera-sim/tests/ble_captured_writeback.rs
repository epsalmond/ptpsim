//! `{ captured: … }` write-back encoding coverage (#43). A recognition-seeded
//! advert capture written back to the wire must re-encode with the capture's
//! ORIGINAL encoding — threaded via `walk_establishment`'s `initial_encodings`
//! — not the scope-string heuristic, which silently hex-decodes an even-length
//! all-hex-digit ASCII value (e.g. "ABCD" → [0xAB, 0xCD] instead of b"ABCD").
//! Latent in shipped data only because the one live ASCII capture
//! (`pairingKeyBytes`, RED) is odd-length; this pins the even-length case.

use std::collections::BTreeMap;

use camera_config::index::{Encoding, ResolvedManufacturerIndex, Step};
use camera_sim::{walk_establishment, BleResponder};

const KEY_CHAR: &str = "0000CC01-0000-1000-8000-00805F9B34FB";

/// `bleConnect`, then write the recognition-seeded `pairingKeyBytes` back to
/// the wire — the shipped Fuji shape (`value: { captured: … }`, no inline
/// encoding, so the value's bytes come from the seeded capture encoding).
fn write_back_steps() -> Vec<Step> {
    let yaml = format!(
        r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt:
        keyChar: "{KEY_CHAR}"
      advert: {{ manufacturerCompanyId: 1 }}
      establishments:
        test:
          mechanism: test
          steps:
            - bleConnect: {{}}
            - bleWrite: {{ gatt: keyChar, value: {{ captured: pairingKeyBytes }} }}
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
"#
    );
    ResolvedManufacturerIndex::from_yaml(&yaml).expect("synthetic index loads").models[0]
        .ble
        .as_ref()
        .unwrap()
        .establishment("test")
        .unwrap()
        .steps
        .clone()
}

/// Walk the write-back plan with `pairingKeyBytes` seeded as an even-length,
/// all-hex-digit ASCII value and return the bytes written to the wire.
fn write_back(initial_encodings: &BTreeMap<String, Encoding>) -> Vec<u8> {
    let scope = BTreeMap::from([("pairingKeyBytes".to_string(), "ABCD".to_string())]);
    let mut responder = BleResponder::new([KEY_CHAR.to_string()]);
    walk_establishment(
        &mut responder,
        &write_back_steps(),
        &scope,
        initial_encodings,
        &BTreeMap::new(),
    )
    .expect("the write-back plan walks to completion");
    responder.written(KEY_CHAR)[0].to_vec()
}

#[test]
fn captured_writeback_uses_the_seeded_capture_encoding() {
    // The fix: with the ascii capture encoding threaded in, "ABCD" re-encodes
    // as its ASCII bytes rather than hex-decoding.
    let encodings = BTreeMap::from([("pairingKeyBytes".to_string(), Encoding::Ascii)]);
    assert_eq!(
        write_back(&encodings),
        b"ABCD".to_vec(),
        "an ascii capture must write its bytes, not hex-decode"
    );
}

#[test]
fn captured_writeback_without_encoding_hex_decodes() {
    // The pre-#43 corruption: without a seeded encoding (advert captures never
    // reached `ctx.encodings`), the scope-string heuristic hex-decodes an
    // even-length all-hex value. Pinned to show exactly what the fix prevents.
    assert_eq!(
        write_back(&BTreeMap::new()),
        vec![0xAB, 0xCD],
        "the heuristic hex-decodes an even-length all-hex value when the encoding is lost"
    );
}
