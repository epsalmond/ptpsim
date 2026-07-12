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
fn an_out_of_range_index_is_an_error() {
    // 10 bytes, size 4 → full windows are 0..2; index 5 is neither a full window
    // nor the sentinel.
    let blob: Vec<u8> = (0..10).collect();
    let err = write(&blob, 4, "5").expect_err("out-of-range index must fail");
    assert!(err.contains("out of range"), "got: {err}");
}
