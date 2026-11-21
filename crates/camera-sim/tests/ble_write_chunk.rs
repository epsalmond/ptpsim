//! #112: focused window-math + frame-assembly tests for the `bleWriteChunk`
//! verb (the settings-restore upload primitive). Each case pre-seeds the chunk
//! index in scope and supplies a tiny blob, so the slice math (full window,
//! short/full sentinel remainder, sub-window blob, out-of-range, empty) is
//! exercised cheaply. The verb's integration into the notify-driven
//! settings-restore loop lives in `ble_actions.rs`.

use std::collections::BTreeMap;

use camera_config::index::{
    BleConnectStep, BleDiscoverServicesStep, BleWriteChunkStep, ChunkField, ChunkFrameField,
    Encoding, Step, StepOptions,
};
use camera_sim::{walk_establishment, BleResponder};

const GATT: &str = "0000FFFF-0000-0000-0000-00000000DA7A";

fn chunk_step(size: u32) -> Vec<Step> {
    vec![
        Step::BleConnect(BleConnectStep::default()),
        Step::BleDiscoverServices(BleDiscoverServicesStep::default()),
        Step::BleWriteChunk(BleWriteChunkStep {
            source: "blob".into(),
            index: "idx".into(),
            size,
            gatt: GATT.into(),
            frame: vec![
                ChunkFrameField {
                    field: ChunkField::Index,
                    encoding: Encoding::U16Le,
                },
                ChunkFrameField {
                    field: ChunkField::Length,
                    encoding: Encoding::U32Le,
                },
            ],
            sentinel_index: 65535,
            checksum: None,
            opts: StepOptions::default(),
        }),
    ]
}

/// Walk a single `bleWriteChunk` with the chunk index pre-seeded in scope and
/// `blob` supplied as a bytes-raw hex param; return the writes, or the error.
fn write(blob: &[u8], size: u32, idx: &str) -> Result<Vec<Vec<u8>>, String> {
    let mut r = BleResponder::new([GATT.to_string()]);
    let scope = BTreeMap::from([("idx".to_string(), idx.to_string())]);
    let hex: String = blob.iter().map(|b| format!("{b:02x}")).collect();
    let params = BTreeMap::from([("blob".to_string(), hex)]);
    walk_establishment(&mut r, &chunk_step(size), &scope, &BTreeMap::new(), &params)
        .map(|_| r.written(GATT).iter().map(|s| s.to_vec()).collect())
        .map_err(|e| e.to_string())
}

/// `[idx u16-le][len u32-le][payload]` — the declared Fuji frame.
fn frame(idx: u16, payload: &[u8]) -> Vec<u8> {
    let mut f = idx.to_le_bytes().to_vec();
    f.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    f.extend_from_slice(payload);
    f
}

#[test]
fn frames_an_interior_full_window() {
    // 10 bytes, size 4 → full windows idx 0 (0..4) and idx 1 (4..8).
    let blob: Vec<u8> = (0..10).collect();
    assert_eq!(write(&blob, 4, "0").unwrap(), vec![frame(0, &blob[0..4])]);
    assert_eq!(write(&blob, 4, "1").unwrap(), vec![frame(1, &blob[4..8])]);
}

#[test]
fn sentinel_index_selects_the_short_remainder_window() {
    // 10 bytes, size 4 → the final remainder is blob[8..10] (2 bytes), idx 0xffff.
    let blob: Vec<u8> = (0..10).collect();
    assert_eq!(
        write(&blob, 4, "65535").unwrap(),
        vec![frame(0xffff, &blob[8..10])],
        "the sentinel window carries the short last bytes",
    );
}

#[test]
fn exact_multiple_sentinel_carries_a_full_last_window() {
    // 8 bytes, size 4 → idx 0 is full; the final window (idx 0xffff) is blob[4..8],
    // a FULL 4-byte window (not an empty trailing frame).
    let blob: Vec<u8> = (0..8).collect();
    assert_eq!(
        write(&blob, 4, "65535").unwrap(),
        vec![frame(0xffff, &blob[4..8])],
    );
}

#[test]
fn blob_smaller_than_window_is_a_single_sentinel_window() {
    let blob: Vec<u8> = (0..3).collect();
    assert_eq!(
        write(&blob, 4, "65535").unwrap(),
        vec![frame(0xffff, &blob)]
    );
}

#[test]
fn empty_blob_writes_an_empty_sentinel_frame() {
    assert_eq!(write(&[], 4, "65535").unwrap(), vec![frame(0xffff, &[])]);
}

#[test]
fn absent_policy_preserves_other_header_layouts() {
    let mut steps = chunk_step(0);
    let Step::BleWriteChunk(step) = &mut steps[2] else {
        unreachable!()
    };
    step.sentinel_index = 255;
    step.frame = vec![
        ChunkFrameField {
            field: ChunkField::Length,
            encoding: Encoding::U16Be,
        },
        ChunkFrameField {
            field: ChunkField::Index,
            encoding: Encoding::U8,
        },
    ];
    assert_eq!(
        step.encode_chunk(&[0x80, 0xff], 0).unwrap(),
        [0, 1, 0, 0x80]
    );
    assert_eq!(
        step.encode_chunk(&[0x80, 0xff], 255).unwrap(),
        [0, 1, 255, 0xff]
    );
    assert_eq!(step.encode_chunk(&[], 255).unwrap(), [0, 0, 255]);
}

#[test]
fn an_out_of_range_index_is_an_error() {
    // 10 bytes, size 4 → full windows are 0..2; index 5 is neither a full window
    // nor the sentinel.
    let blob: Vec<u8> = (0..10).collect();
    let err = write(&blob, 4, "5").expect_err("out-of-range index must fail");
    assert!(err.contains("out of range"), "got: {err}");
}

#[test]
fn checksum_policy_matches_public_restore_vectors() {
    use camera_config::index::ChunkChecksum;
    use protocol_primitives::chunk_frame::{Direction, SourceSum16Decoder};
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../protocol-primitives/tests/fixtures/chunk-frames.json"
    ))
    .unwrap();
    let decode = |hex: &str| -> Vec<u8> {
        hex.as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    };
    for vector in fixture["vectors"].as_array().unwrap() {
        if vector["direction"] != "restore" || !vector["source_hex"].is_string() {
            continue;
        }
        let source = decode(vector["source_hex"].as_str().unwrap());
        let frames = vector["frames_hex"].as_array().unwrap();
        let mut steps = chunk_step(120);
        let Step::BleWriteChunk(step) = &mut steps[2] else {
            unreachable!()
        };
        step.checksum = Some(ChunkChecksum::SourceSum16Le);
        let params =
            BTreeMap::from([("blob".into(), vector["source_hex"].as_str().unwrap().into())]);
        let mut decoder =
            SourceSum16Decoder::new(Direction::Restore, 120, source.len(), Some(source.len()))
                .unwrap();
        for (ordinal, expected) in frames.iter().enumerate() {
            let index = if ordinal + 1 == frames.len() {
                65535
            } else {
                ordinal
            };
            let scope = BTreeMap::from([("idx".into(), index.to_string())]);
            let mut responder = BleResponder::new([GATT.to_string()]);
            walk_establishment(&mut responder, &steps, &scope, &BTreeMap::new(), &params).unwrap();
            let written = responder.written(GATT);
            assert_eq!(
                written,
                [decode(expected.as_str().unwrap())],
                "{}",
                vector["id"]
            );
            decoder.push(written[0]).unwrap();
        }
        assert_eq!(decoder.finish().unwrap(), source);
    }
    let mut steps = chunk_step(120);
    let Step::BleWriteChunk(step) = &mut steps[2] else {
        unreachable!()
    };
    step.checksum = Some(ChunkChecksum::SourceSum16Le);
    assert!(step
        .encode_chunk(&[], 65535)
        .unwrap_err()
        .to_string()
        .contains("empty source"));
}
