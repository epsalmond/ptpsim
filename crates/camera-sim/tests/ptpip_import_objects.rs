//! #46 acceptance: the reference PTP-IP executor walks the FULL image-transfer
//! choreography — arm → enumerate → for-each handle { getObjectInfo (sizes the
//! object) → chunk-download until exhausted } — entirely from the manifest loop
//! primitives, with the executor owning the chunk offset cursor.
//!
//! These build the loop steps programmatically with a TINY chunk window so the
//! chunk arithmetic (short last window, exact multiple, empty, the deterministic
//! cap) is exercised cheaply; the keystone in `gate_gfx100ii.rs` walks the REAL
//! consolidated action. `loop_iterations` is the cursor oracle: the per-handle
//! chunk count is exactly `ceil(size / window)`, which uniquely pins the cursor
//! to cover `[0, size)` with no over-read (`read_range` clamps, so a successful
//! walk alone would not catch an over-read — the exact count does).

use camera_config::model::{ChunkSize, Loop, Step, StepParam};
use camera_config::CameraManifest;
use camera_media_store::MediaStore;
use camera_sim::{walk_ptpip_in, Engine};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn consolidated() -> CameraManifest {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data/fuji/gfx100ii/gfx100ii.consolidated.yaml");
    CameraManifest::from_yaml(&std::fs::read_to_string(&p).unwrap())
        .unwrap_or_else(|e| panic!("consolidated loads: {e}"))
}

fn loop_mechanics_manifest() -> CameraManifest {
    let mut manifest = consolidated();
    for prop in ["0xd620", "0xd621"] {
        manifest
            .properties
            .get_mut(prop)
            .unwrap_or_else(|| panic!("{prop} property exists"))
            .requires_gate = None;
    }
    manifest
}

/// A JPEG of exactly `size` bytes (SOI … EOI, index-filled) so MediaStore scans
/// it as a transferable image whose ObjectCompressedSize == `size`.
fn jpeg(size: usize) -> Vec<u8> {
    assert!(size >= 4 || size == 0, "a JPEG needs SOI+EOI or be empty");
    if size == 0 {
        return Vec::new();
    }
    let mut v = vec![0u8; size];
    v[0] = 0xFF;
    v[1] = 0xD8;
    for (i, b) in v.iter_mut().enumerate().take(size - 2).skip(2) {
        *b = (i % 251) as u8;
    }
    v[size - 2] = 0xFF;
    v[size - 1] = 0xD9;
    v
}

/// An engine whose card holds `files` (name, size-in-bytes). Each file's size
/// drives the chunk math.
fn engine_with(files: &[(&str, usize)]) -> Engine {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("ptpsim-import-{nanos}"));
    let dir = root.join("DCIM/100_FUJI");
    std::fs::create_dir_all(&dir).unwrap();
    for (name, size) in files {
        std::fs::write(dir.join(name), jpeg(*size)).unwrap();
    }
    let mut store = MediaStore::open(&root).unwrap();
    store.scan().unwrap();
    Engine::new(loop_mechanics_manifest(), store)
}

fn rt(slot: &str) -> StepParam {
    StepParam::Runtime {
        runtime: slot.into(),
        shift: 0,
        mask: None,
    }
}

fn send_op(op: &str, params: Vec<StepParam>) -> Step {
    Step {
        send_op: Some(op.into()),
        params,
        ..Default::default()
    }
}

fn loop_step(lp: Loop, tolerant: bool) -> Step {
    Step {
        r#loop: Some(lp),
        tolerant,
        ..Default::default()
    }
}

/// enumerate → forEach handle { getObjectInfo → chunk(window) { getPartialObject } }.
/// No arm block: these tests use a manifest clone with only the enumeration gate
/// removed. The keystone covers the armed path from the real action.
fn import_steps(window: u32, chunk_tolerant: bool) -> Vec<Step> {
    let chunk = loop_step(
        Loop::Chunk {
            total: "objectSize".into(),
            size: ChunkSize::literal(window),
            offset_bind: "offset".into(),
            length_bind: "length".into(),
            body: vec![send_op(
                "0x101b",
                vec![rt("handle"), rt("offset"), rt("length")],
            )],
        },
        chunk_tolerant,
    );
    vec![loop_step(
        Loop::ForEach {
            in_prop: "0xd621".into(),
            bind: "handle".into(),
            body: vec![send_op("0x1008", vec![rt("handle")]), chunk],
        },
        false,
    )]
}

fn walk(engine: &mut Engine, steps: &[Step]) -> Result<Vec<usize>, String> {
    walk_ptpip_in(engine, steps, &BTreeMap::new(), Some("app"))
        .map(|o| o.loop_iterations)
        .map_err(|e| e.to_string())
}

#[test]
fn walks_each_handle_and_chunks_each_object() {
    // Three identical objects, window 4 → each takes ceil(10/4)=3 chunks. Order-
    // independent: the per-handle chunk counts are pushed first (inside forEach),
    // then the forEach element count last.
    let mut e = engine_with(&[("A.JPG", 10), ("B.JPG", 10), ("C.JPG", 10)]);
    let iters = walk(&mut e, &import_steps(4, false)).expect("the import walks");
    assert_eq!(
        iters,
        vec![3, 3, 3, 3],
        "three 3-chunk downloads, then forEach visited 3 handles",
    );
}

#[test]
fn chunk_loop_short_last_window_terminates_exactly_at_size() {
    // 10 bytes in 4-byte windows → 4 + 4 + 2: ceil(10/4) = 3, the last window short.
    let mut e = engine_with(&[("A.JPG", 10)]);
    let iters = walk(&mut e, &import_steps(4, false)).expect("walks");
    assert_eq!(iters, vec![3, 1], "3 chunks (last short), 1 handle");
}

#[test]
fn chunk_loop_exact_multiple_has_no_trailing_empty_window() {
    // 8 bytes in 4-byte windows → exactly 2, never a spurious 3rd zero-byte read.
    let mut e = engine_with(&[("A.JPG", 8)]);
    let iters = walk(&mut e, &import_steps(4, false)).expect("walks");
    assert_eq!(iters, vec![2, 1]);
}

#[test]
fn object_smaller_than_window_is_one_chunk() {
    // 2 bytes, window 4 → a single full-object window (length clamps to 2).
    let mut e = engine_with(&[("A.JPG", 4)]);
    let iters = walk(&mut e, &import_steps(16, false)).expect("walks");
    assert_eq!(iters, vec![1, 1]);
}

#[test]
fn empty_card_downloads_nothing() {
    // No transferable objects → the forEach over an empty 0xd621 is a no-op; no
    // chunk loop runs, nothing is downloaded.
    let mut e = engine_with(&[]);
    let iters = walk(&mut e, &import_steps(4, false)).expect("walks");
    assert_eq!(iters, vec![0], "forEach ran zero iterations");
}

#[test]
fn chunk_cap_is_a_hard_error_when_not_tolerant() {
    // MAX_CHUNK_ITERS = 4096: a 4097-byte object in 1-byte windows exceeds the cap.
    // Non-tolerant → the walk fails deterministically rather than spinning.
    let mut e = engine_with(&[("BIG.JPG", 4097)]);
    let err = walk(&mut e, &import_steps(1, false)).expect_err("over-cap must fail");
    assert!(err.contains("chunk loop exceeded"), "cap error, got: {err}",);
}

#[test]
fn chunk_cap_bails_when_tolerant() {
    // Same object, tolerant chunk → bails at the cap (4096) instead of erroring.
    let mut e = engine_with(&[("BIG.JPG", 4097)]);
    let iters = walk(&mut e, &import_steps(1, true)).expect("tolerant cap bails");
    assert_eq!(
        iters,
        vec![4096, 1],
        "chunk stopped at the cap, forEach completed"
    );
}
