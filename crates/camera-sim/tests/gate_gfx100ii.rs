//! DESIGN gates #3 (ImageImport) + #4 (LiveView): the simulator runs the GFX100 II
//! choreography PURELY from the consolidated manifest — including the vendor-op
//! "chords" — and enumerates believably. Engine-level (the service smoke test covers
//! the TCP path). This is the vcam-replacement believability gate.

use camera_config::index::ResolvedManufacturerIndex;
use camera_config::model::ReopenSession;
use camera_config::{
    ActionArgument, ActionInvocationRequest, ActionRole, ActionVerb, AwaitSource, AwaitUntil,
    CameraManifest, Leaf, ModeEntry, ModeEntryExecution, Predicate, Step, StepParam,
};
use camera_media_store::{fmt, ByteSource, MediaStore, ObjectQuery};
use camera_sim::{
    walk_establishment, walk_ptpip, walk_ptpip_in, BleResponder, Engine, Fault, Phase, Reply,
    StateOverlay, StreamCompletion,
};
use ptp_core::{DeviceInfo, ObjectInfo, OperationRequest, Reader};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn data(rel: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data")
        .join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn consolidated() -> CameraManifest {
    CameraManifest::from_yaml(&data("fuji/gfx100ii/gfx100ii.consolidated.yaml"))
        .unwrap_or_else(|e| panic!("consolidated loads: {e}"))
}

fn without_decode_retry() -> CameraManifest {
    let yaml = data("fuji/gfx100ii/gfx100ii.yaml")
        .replace("                whenFailureClasses: [\"decode\"]\n", "");
    CameraManifest::from_yaml(&yaml)
        .unwrap_or_else(|error| panic!("manifest without decode retry loads: {error}"))
}

fn entry_steps(entry: &ModeEntry) -> &[Step] {
    entry.ptp_steps().expect("PTP mode entry")
}

fn engine_with_manifest(manifest: CameraManifest) -> Engine {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("ptpsim-gate-{nanos}"));
    let dir = root.join("DCIM/100_FUJI");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("DSCF0001.JPG"), b"\xFF\xD8HELLOJPEG\xFF\xD9").unwrap();
    let mut store = MediaStore::open(&root).unwrap();
    store.scan().unwrap();
    Engine::new(manifest, store)
}

fn engine() -> Engine {
    engine_with_manifest(consolidated())
}

fn image_import_ready_with_manifest(manifest: CameraManifest) -> (Engine, camera_config::Action) {
    let app = &manifest.connections["app"];
    let cold = app
        .entries
        .iter()
        .find(|entry| entry.to == "image-transfer" && entry.from.is_none())
        .expect("cold image-transfer entry");
    let cold_steps = entry_steps(cold).to_vec();
    let enumerate = app
        .actions
        .get(&ActionVerb::EnumerateObjects)
        .expect("enumerateObjects action")
        .clone();
    let mut engine = engine_with_manifest(manifest);
    walk_ptpip_in(&mut engine, &cold_steps, &BTreeMap::new(), Some("app"))
        .expect("cold image-transfer entry succeeds");
    (engine, enumerate)
}

fn image_import_ready() -> (Engine, camera_config::Action) {
    image_import_ready_with_manifest(consolidated())
}

fn engine_with_two_jpegs() -> Engine {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("ptpsim-reserved-{nanos}"));
    let dir = root.join("DCIM/100_FUJI");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("DSCF0001.JPG"), b"\xFF\xD8FIRST-JPEG\xFF\xD9").unwrap();
    std::fs::write(dir.join("DSCF0002.JPG"), b"\xFF\xD8SECOND-JPEG\xFF\xD9").unwrap();
    let mut store = MediaStore::open(&root).unwrap();
    store.scan().unwrap();
    let mut engine = Engine::new(consolidated(), store);
    activate_camera_initiated_transfer(&mut engine);
    engine
}

fn engine_with_non_aliasing_reserved_head() -> Engine {
    Engine::new(consolidated(), non_aliasing_reserved_store())
}

fn non_aliasing_reserved_store() -> MediaStore {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("ptpsim-reserved-alias-{nanos}"));
    let dir = root.join("DCIM/100_FUJI");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("AAAA0001.MOV"), b"NOT-A-RESERVED-PHOTO").unwrap();
    std::fs::write(dir.join("DSCF0001.JPG"), b"\xFF\xD8RESERVED-JPEG\xFF\xD9").unwrap();
    let mut store = MediaStore::open(&root).unwrap();
    store.scan().unwrap();
    store
}

fn engine_with_sparse_mov(size: u64) -> (Engine, u32) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("ptpsim-large-mov-{nanos}"));
    let path = root.join("DCIM/100_FUJI/DSCF8476.MOV");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(size).unwrap();
    drop(file);

    let mut store = MediaStore::open(&root).unwrap();
    store.scan().unwrap();
    let handle = store.handles(ObjectQuery {
        parent: None,
        format: Some(fmt::MOV),
    })[0];
    (Engine::new(consolidated(), store), handle)
}

fn req(code: u16, tid: u32, params: Vec<u32>) -> OperationRequest {
    OperationRequest {
        data_phase_info: 1,
        code,
        transaction_id: tid,
        params,
    }
}

fn activate_camera_initiated_transfer(engine: &mut Engine) {
    let overlay: StateOverlay = serde_json::from_value(serde_json::json!({
        "camera_initiated_transfer_active": true
    }))
    .unwrap();
    engine.apply_state_overlay(&overlay).unwrap();
}

fn assert_ok(reply: &Reply) {
    match reply {
        Reply::Response(r) => assert_eq!(r.code, 0x2001, "expected OK"),
        Reply::Data { response, .. } => assert_eq!(response.code, 0x2001, "expected OK"),
        Reply::DataStream { response, .. } => assert_eq!(response.code, 0x2001, "expected OK"),
        Reply::NoResponse => panic!("unexpected NoResponse"),
        Reply::Close => panic!("unexpected Close"),
    }
}

fn assert_no_response(reply: Reply) {
    assert!(
        matches!(reply, Reply::NoResponse),
        "expected NoResponse, got {reply:?}"
    );
}

fn data_of(reply: Reply) -> Vec<u8> {
    match reply {
        Reply::Data { data, response } => {
            assert_eq!(response.code, 0x2001, "OK expected");
            data
        }
        Reply::DataStream {
            source, response, ..
        } => {
            assert_eq!(response.code, 0x2001, "OK expected");
            source.read().expect("realize stream")
        }
        other => panic!("expected Data, got {other:?}"),
    }
}

fn write_u16(e: &mut Engine, tid: u32, code: u16, value: u16) {
    assert_ok(&e.on_operation(
        &req(0x1016, tid, vec![code as u32]),
        Some(&value.to_le_bytes()),
    ));
}

fn write_u32(e: &mut Engine, tid: u32, code: u16, value: u32) {
    assert_ok(&e.on_operation(
        &req(0x1016, tid, vec![code as u32]),
        Some(&value.to_le_bytes()),
    ));
}

fn read_u32(e: &mut Engine, tid: u32, code: u16) -> u32 {
    let bytes = data_of(e.on_operation(&req(0x1015, tid, vec![code as u32]), None));
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn read_u16(e: &mut Engine, tid: u32, code: u16) -> u16 {
    let bytes = data_of(e.on_operation(&req(0x1015, tid, vec![code as u32]), None));
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn invoke_pcss_autofocus_responder(engine: &mut Engine, action_id: &str, result: Option<u64>) {
    let manifest = consolidated();
    let request = ActionInvocationRequest {
        catalog_revision: manifest.action_catalog().revision,
        action_id: action_id.into(),
        connection: "wireless-tether".into(),
        mode: "shooting/stills".into(),
        role: ActionRole::Responder,
        parameters: result
            .map(|value| ActionArgument {
                name: "result".into(),
                value: value.into(),
            })
            .into_iter()
            .collect(),
    };
    let resolved = manifest
        .resolve_action_invocation(&request)
        .expect("bundled responder invocation resolves");
    let mutation = resolved
        .responder_mutation
        .expect("bundled responder mutation");
    let prepared = engine
        .prepare_responder_mutation(&mutation, &resolved.parameters)
        .expect("responder mutation prepares");
    engine
        .apply_responder_mutation(&prepared)
        .expect("responder mutation applies");
}

fn standard_object_handles(e: &mut Engine, tid: u32) -> Vec<u32> {
    let bytes = data_of(e.on_operation(&req(0x1007, tid, vec![0xffff_ffff, 0]), None));
    Reader::new(&bytes)
        .ptp_array(|reader| reader.u32())
        .unwrap()
}

fn drain_standard_object_queue(e: &mut Engine, first_tid: u32) -> Vec<u32> {
    let handles = standard_object_handles(e, first_tid);
    for (index, handle) in handles.iter().copied().enumerate() {
        let tid = first_tid + 1 + (index as u32 * 3);
        assert_ok(&e.on_operation(&req(0x1008, tid, vec![handle]), None));
        assert!(!data_of(e.on_operation(&req(0x1009, tid + 1, vec![handle]), None)).is_empty());
        assert_ok(&e.on_operation(&req(0x100b, tid + 2, vec![handle]), None));
    }
    assert!(standard_object_handles(e, first_tid + 100).is_empty());
    handles
}

fn stream_of(reply: Reply) -> (ByteSource, Vec<u32>) {
    match reply {
        Reply::DataStream {
            source, response, ..
        } => {
            assert_eq!(response.code, 0x2001, "OK expected");
            (source, response.params)
        }
        other => panic!("expected DataStream, got {other:?}"),
    }
}

fn stream_with_completion(reply: Reply) -> (Vec<u8>, Option<StreamCompletion>) {
    match reply {
        Reply::DataStream {
            source,
            response,
            completion,
        } => {
            assert_eq!(response.code, 0x2001, "OK expected");
            (source.read().expect("realize stream"), completion)
        }
        other => panic!("expected DataStream, got {other:?}"),
    }
}

/// Decode a `0xD212` tight-format record stream: u16 count, then 6-byte
/// `<code u16 LE><value u32 LE>` records.
fn decode_record_stream(b: &[u8]) -> Vec<(u16, u32)> {
    let count = u16::from_le_bytes([b[0], b[1]]) as usize;
    assert_eq!(b.len(), 2 + count * 6, "u16 count + 6-byte records");
    (0..count)
        .map(|i| {
            let o = 2 + i * 6;
            (
                u16::from_le_bytes([b[o], b[o + 1]]),
                u32::from_le_bytes([b[o + 2], b[o + 3], b[o + 4], b[o + 5]]),
            )
        })
        .collect()
}

fn reserved_count(e: &mut Engine, tid: u32) -> u32 {
    let records = decode_record_stream(&data_of(
        e.on_operation(&req(0x1015, tid, vec![0xd212]), None),
    ));
    records
        .into_iter()
        .find_map(|(code, value)| (code == 0xdf41).then_some(value))
        .expect("DF41 is present in D212")
}

#[test]
fn camera_initiated_metadata_uses_reserved_head_in_both_phases() {
    let mut e = engine_with_non_aliasing_reserved_head();
    let public_handle_one = e.store().object_info(1).expect("public handle 1 exists");
    assert_eq!(
        public_handle_one.object_format,
        ptp_core::codes::format::ASSOCIATION,
        "fixture must make public handle 1 differ from the reserved photo head"
    );

    assert_ok(&e.on_operation(&req(0x1002, 1, vec![1]), None));
    assert_eq!(reserved_count(&mut e, 2), 1);
    let ordinary =
        ObjectInfo::decode(&data_of(e.on_operation(&req(0x1008, 3, vec![1]), None))).unwrap();
    assert_eq!(ordinary.object_format, ptp_core::codes::format::ASSOCIATION);

    activate_camera_initiated_transfer(&mut e);
    assert_eq!(reserved_count(&mut e, 4), 1);
    assert!(matches!(
        e.on_operation(&req(0x1008, 5, vec![2]), None),
        Reply::Response(ref response) if response.code == 0x2009
    ));
    assert_eq!(reserved_count(&mut e, 6), 1);
    let before =
        ObjectInfo::decode(&data_of(e.on_operation(&req(0x1008, 7, vec![1]), None))).unwrap();
    assert_eq!(before.filename, "DSCF0001.JPG");

    let after_consumption =
        ObjectInfo::decode(&data_of(e.on_operation(&req(0x1008, 8, vec![1]), None))).unwrap();
    assert_eq!(
        after_consumption.object_format,
        ptp_core::codes::format::ASSOCIATION,
        "the count-read arm is one-shot and must return to public lookup"
    );

    write_u16(&mut e, 9, 0xdf01, 21);
    assert_eq!(read_u32(&mut e, 10, 0xdf29), 0);
    write_u32(&mut e, 11, 0xdf29, 3);
    let after =
        ObjectInfo::decode(&data_of(e.on_operation(&req(0x1008, 12, vec![1]), None))).unwrap();
    assert_eq!(after.filename, before.filename);
}

#[test]
// The single-object drain is wire-confirmed. Reusing index 1 and decrementing
// the count across multiple objects are inferred from reference app's static receive loop.
fn camera_initiated_queue_reuses_head_only_after_acknowledged_eof() {
    let mut e = engine_with_two_jpegs();
    assert_ok(&e.on_operation(&req(0x1002, 1, vec![1]), None));
    assert_eq!(reserved_count(&mut e, 2), 2);

    write_u16(&mut e, 3, 0xdf01, 21);
    assert_eq!(e.phase(), Phase::QueuedReceive);
    assert_eq!(read_u32(&mut e, 4, 0xdf29), 0);
    write_u32(&mut e, 5, 0xdf29, 3);
    let first_info =
        ObjectInfo::decode(&data_of(e.on_operation(&req(0x1008, 6, vec![1]), None))).unwrap();
    assert_eq!(first_info.filename, "DSCF0001.JPG");
    assert_eq!(read_u32(&mut e, 7, 0xd235), 0x00bf_ffe0);

    let first_size = first_info.object_compressed_size;
    let (prefix, prefix_completion) =
        stream_with_completion(e.on_operation(&req(0x101b, 8, vec![1, 0, 4]), None));
    assert_eq!(prefix.len(), 4);
    assert!(!e.complete_stream(prefix_completion.unwrap()));
    assert_eq!(reserved_count(&mut e, 9), 2);

    let (suffix, suffix_completion) =
        stream_with_completion(e.on_operation(&req(0x101b, 10, vec![1, 4, first_size - 4]), None));
    assert_eq!(suffix.len(), (first_size - 4) as usize);
    assert!(e.complete_stream(suffix_completion.unwrap()));
    assert_eq!(reserved_count(&mut e, 11), 1);

    let second_info =
        ObjectInfo::decode(&data_of(e.on_operation(&req(0x1008, 12, vec![1]), None))).unwrap();
    assert_eq!(second_info.filename, "DSCF0002.JPG");
    let second_size = second_info.object_compressed_size;
    let (_, completion) =
        stream_with_completion(e.on_operation(&req(0x101b, 13, vec![1, 0, second_size]), None));
    let duplicate = completion.clone().unwrap();
    assert!(e.complete_stream(completion.unwrap()));
    assert!(!e.complete_stream(duplicate));
    assert_eq!(reserved_count(&mut e, 14), 0);

    assert!(matches!(
        e.on_operation(&req(0x1008, 15, vec![1]), None),
        Reply::Response(ref response) if response.code == 0x2009
    ));
    let public_files = e
        .store()
        .handles(ObjectQuery::default())
        .into_iter()
        .filter(|handle| {
            e.store()
                .object_info(*handle)
                .is_ok_and(|info| info.object_format != ptp_core::codes::format::ASSOCIATION)
        })
        .count();
    assert_eq!(
        public_files, 2,
        "reserved drains do not delete card objects"
    );
    assert_ok(&e.on_operation(&req(0x1003, 16, vec![]), None));
}

#[test]
fn camera_initiated_tail_only_read_does_not_dequeue() {
    let mut e = engine_with_two_jpegs();
    assert_ok(&e.on_operation(&req(0x1002, 1, vec![1]), None));
    write_u16(&mut e, 2, 0xdf01, 21);
    let info =
        ObjectInfo::decode(&data_of(e.on_operation(&req(0x1008, 3, vec![1]), None))).unwrap();
    let size = info.object_compressed_size;
    let (_, completion) =
        stream_with_completion(e.on_operation(&req(0x101b, 4, vec![1, size - 2, 2]), None));
    assert!(!e.complete_stream(completion.unwrap()));
    assert_eq!(reserved_count(&mut e, 5), 2);
}

#[test]
fn failed_count_read_does_not_arm_reserved_metadata() {
    let mut e = engine_with_non_aliasing_reserved_head();
    activate_camera_initiated_transfer(&mut e);
    assert_ok(&e.on_operation(&req(0x1002, 1, vec![1]), None));
    e.install_fault(Fault::FailOperation {
        code: 0x1015,
        response: 0x2002,
    });
    assert!(matches!(
        e.on_operation(&req(0x1015, 2, vec![0xd212]), None),
        Reply::Response(ref response) if response.code == 0x2002
    ));
    e.clear_faults();

    let info =
        ObjectInfo::decode(&data_of(e.on_operation(&req(0x1008, 3, vec![1]), None))).unwrap();
    assert_eq!(info.object_format, ptp_core::codes::format::ASSOCIATION);
}

#[test]
fn camera_initiated_queue_uses_manifest_declared_operations() {
    let mut manifest = consolidated();
    let transfer = manifest.camera_initiated_transfer.as_mut().unwrap();
    transfer.receive.metadata.operation = "0x902b".into();
    transfer.receive.data.operation = "0x902c".into();
    let mut e = Engine::new(manifest, non_aliasing_reserved_store());
    activate_camera_initiated_transfer(&mut e);

    assert_ok(&e.on_operation(&req(0x1002, 1, vec![1]), None));
    assert_eq!(reserved_count(&mut e, 2), 1);
    let info =
        ObjectInfo::decode(&data_of(e.on_operation(&req(0x902b, 3, vec![1]), None))).unwrap();
    assert_eq!(info.filename, "DSCF0001.JPG");

    write_u16(&mut e, 4, 0xdf01, 21);
    assert_eq!(read_u32(&mut e, 5, 0xdf29), 0);
    write_u32(&mut e, 6, 0xdf29, 3);
    let (bytes, completion) =
        stream_with_completion(e.on_operation(&req(0x902c, 7, vec![1, 0, 4]), None));
    assert_eq!(bytes, b"\xFF\xD8RE");
    assert!(completion.is_some());
}

#[test]
fn app_live_controls_start_from_neutral_labeled_values() {
    let manifest = consolidated();
    let expected = [
        ("0xd02a", 200),
        ("0xd240", 0x8001_e848),
        ("0x5007", 400),
        ("0x5010", 0),
    ];
    for (code, value) in expected {
        assert_eq!(
            manifest.properties[code].initial_value,
            Some(value),
            "{code} has an explicit simulator startup value"
        );
    }
    assert!(manifest.properties["0xd02a"]
        .value_profiles
        .iter()
        .filter(|profile| profile.connection.as_deref() == Some("app"))
        .flat_map(|profile| &profile.rows)
        .any(|row| row.raw == 200 && row.legal && row.label == "200"));

    let mut e = engine_with_manifest(manifest);
    assert_ok(&e.on_operation(&req(0x1002, 1, vec![1]), None));
    assert_eq!(read_u32(&mut e, 2, 0xd02a), 200);
    assert_eq!(read_u32(&mut e, 3, 0xd240), 0x8001_e848);
    assert_eq!(read_u16(&mut e, 4, 0x5007), 400);
    assert_eq!(read_u16(&mut e, 5, 0x5010), 0);

    let d212 = data_of(e.on_operation(&req(0x1015, 6, vec![0xd212]), None));
    let records = decode_record_stream(&d212);
    for (code, value) in [(0xd02a, 200), (0xd240, 0x8001_e848), (0x5007, 400)] {
        assert_eq!(
            records.iter().find(|(member, _)| *member == code),
            Some(&(code, value)),
            "D212 member {code:#06x} uses the neutral startup value"
        );
    }
}

#[test]
fn d212_live_status_emits_member_record_stream_from_the_descriptor() {
    let mut e = engine();
    assert_ok(&e.on_operation(&req(0x1002, 1, vec![1]), None)); // OpenSession

    // The 0xD212 readback is the manifest-descriptor-driven record stream, not a
    // hand-coded blob — its members come from the payload descriptor (#51).
    let bytes = data_of(e.on_operation(&req(0x1015, 2, vec![0xd212]), None));
    let records = decode_record_stream(&bytes);
    assert_eq!(records.len(), 27, "all 27 descriptor members emitted");
    // The named sub-fields the bundle carries survive into the stream.
    for code in [0x5007u16, 0xd17c, 0xd209, 0xd02a, 0xdf41] {
        assert!(
            records.iter().any(|(c, _)| *c == code),
            "member {code:#06x} present in the stream"
        );
    }

    // The bundle reflects live state: a member's record == its own scalar read.
    let scalar = data_of(e.on_operation(&req(0x1015, 3, vec![0x5007]), None));
    let aperture = u16::from_le_bytes([scalar[0], scalar[1]]) as u32;
    assert_eq!(
        records.iter().find(|(c, _)| *c == 0x5007).map(|(_, v)| *v),
        Some(aperture),
        "aperture in the bundle matches its individual GetDevicePropValue"
    );
}

#[test]
fn still_iso_setprop_uses_scoped_value_profile_for_camera_readback() {
    let mut e = engine();
    assert_ok(&e.on_operation(&req(0x1002, 1, vec![1]), None));
    write_u16(&mut e, 2, 0xdf01, 0x16);
    assert_ok(&e.on_operation(&req(0x101c, 3, vec![]), None));

    for (tid, sent, expected) in [
        (10, 6400, 6400),
        (20, 25600, 0x4000_6400),
        (30, 0x4001_9000, 0x4001_9000),
        (40, 50, 80),
        (50, 0x8000_00a0, 80),
        (60, 0x8000_6400, 80),
    ] {
        write_u32(&mut e, tid, 0xd02a, sent);
        assert_eq!(
            read_u32(&mut e, tid + 1, 0xd02a),
            expected,
            "scalar readback after writing {sent:#010x}"
        );
        let d212 = data_of(e.on_operation(&req(0x1015, tid + 2, vec![0xd212]), None));
        let records = decode_record_stream(&d212);
        assert_eq!(
            records.iter().find(|(c, _)| *c == 0xd02a).map(|(_, v)| *v),
            Some(expected),
            "D212 readback after writing {sent:#010x}"
        );
    }
}

#[test]
fn neighboring_iso_property_without_value_profile_still_stores_verbatim() {
    let mut e = engine();
    assert_ok(&e.on_operation(&req(0x1002, 1, vec![1]), None));
    write_u16(&mut e, 2, 0xdf01, 0x16);
    assert_ok(&e.on_operation(&req(0x101c, 3, vec![]), None));

    write_u32(&mut e, 4, 0xd02b, 25600);
    assert_eq!(read_u32(&mut e, 5, 0xd02b), 25600);
}

#[test]
fn still_iso_profile_applies_after_open_session_before_live_view_selector() {
    let mut e = engine();
    assert_ok(&e.on_operation(&req(0x1002, 1, vec![1]), None));

    write_u32(&mut e, 2, 0xd02a, 50);
    assert_eq!(read_u32(&mut e, 3, 0xd02a), 80);
}

#[test]
fn default_ptpip_walk_rebinds_app_connection_for_value_profiles() {
    let mut e = engine();
    e.bind_connection("wireless-tether");
    let steps = vec![
        Step {
            set_prop: Some("0xd02a".into()),
            value: Some(50.into()),
            ..Default::default()
        },
        Step {
            get_prop: Some("0xd02a".into()),
            ..Default::default()
        },
    ];

    let out = walk_ptpip(&mut e, &steps, &BTreeMap::new()).expect("walk ok");
    assert_eq!(out.observed.get(0xd02a), Some(80));
}

#[test]
fn wireless_tether_drains_captured_objects_while_live_view_stays_open() {
    let manifest = consolidated();
    let start_live_view = manifest
        .action("wireless-tether", ActionVerb::StartLiveView)
        .expect("wireless-tether startLiveView")
        .initiator()
        .expect("startLiveView initiator")
        .steps
        .clone();
    let shutter = manifest
        .action("wireless-tether", ActionVerb::Shutter)
        .expect("wireless-tether shutter")
        .initiator()
        .expect("shutter initiator")
        .steps
        .clone();
    let mut e = engine_with_manifest(manifest);
    e.bind_connection("wireless-tether");
    e.configure_standard_object_queue("wireless-tether", 1)
        .expect("shutter-driven queue");
    e.preseed_shutter_busy_responses("wireless-tether", 3)
        .expect("field-observed mid-capture busy responses");
    assert_ok(&e.on_operation(&req(0x1002, 1, vec![1]), None));
    e.apply_state_overlay(
        &serde_json::from_value(serde_json::json!({ "phase": "liveView" })).unwrap(),
    )
    .unwrap();

    walk_ptpip_in(
        &mut e,
        &start_live_view,
        &BTreeMap::new(),
        Some("wireless-tether"),
    )
    .expect("live view starts with an empty transfer queue");
    assert_eq!(e.phase(), Phase::Streaming);
    let shutter_outcome =
        walk_ptpip_in(&mut e, &shutter, &BTreeMap::new(), Some("wireless-tether"))
            .expect("shutter retries beat 2 while busy and queues one object");
    assert_eq!(shutter_outcome.retry_delays_ms, [100, 100, 100]);
    assert_eq!(e.transfer_queue_stats().standard.unwrap().queued, 1);

    let handles = drain_standard_object_queue(&mut e, 20);
    assert_eq!(handles.len(), 1);
    assert_eq!(e.phase(), Phase::Streaming, "drain does not enter a mode");
    walk_ptpip_in(
        &mut e,
        &start_live_view,
        &BTreeMap::new(),
        Some("wireless-tether"),
    )
    .expect("terminate-first re-arm succeeds after the queue drains");
}

#[test]
fn wireless_tether_pending_queue_blocks_arming_until_preseed_is_drained() {
    let manifest = consolidated();
    let start_live_view = manifest
        .action("wireless-tether", ActionVerb::StartLiveView)
        .expect("wireless-tether startLiveView")
        .initiator()
        .expect("startLiveView initiator")
        .steps
        .clone();
    let mut e = engine_with_manifest(manifest);
    e.bind_connection("wireless-tether");
    e.configure_standard_object_queue("wireless-tether", 1)
        .expect("shutter-driven queue");
    assert_eq!(
        e.preseed_standard_object_queue("wireless-tether", 1)
            .expect("pre-seed stale queue"),
        1
    );
    assert_ok(&e.on_operation(&req(0x1002, 1, vec![1]), None));
    e.apply_state_overlay(
        &serde_json::from_value(serde_json::json!({ "phase": "liveView" })).unwrap(),
    )
    .unwrap();

    assert!(matches!(
        e.on_operation(
            &req(0x1016, 2, vec![0xd1bc]),
            Some(&2u16.to_le_bytes()),
        ),
        Reply::Response(ref response) if response.code == 0xa002
    ));
    assert_eq!(drain_standard_object_queue(&mut e, 10).len(), 1);
    walk_ptpip_in(
        &mut e,
        &start_live_view,
        &BTreeMap::new(),
        Some("wireless-tether"),
    )
    .expect("terminate-first arm succeeds after the pre-seeded queue drains");
}

#[test]
fn wireless_tether_start_live_view_recovers_a_preseeded_stale_stream() {
    let manifest = consolidated();
    let start_live_view = manifest
        .action("wireless-tether", ActionVerb::StartLiveView)
        .expect("wireless-tether startLiveView")
        .initiator()
        .expect("startLiveView initiator")
        .steps
        .clone();
    let mut e = engine_with_manifest(manifest);
    e.bind_connection("wireless-tether");
    assert!(e.preseed_stale_live_view_stream("app").is_err());
    e.preseed_stale_live_view_stream("wireless-tether")
        .expect("pre-seed unterminated PCSS stream");
    assert_ok(&e.on_operation(&req(0x1002, 1, vec![1]), None));
    e.apply_state_overlay(
        &serde_json::from_value(serde_json::json!({ "phase": "liveView" })).unwrap(),
    )
    .unwrap();

    assert!(matches!(
        e.on_operation(
            &req(0x1016, 2, vec![0xd1bc]),
            Some(&2u16.to_le_bytes()),
        ),
        Reply::Response(ref response) if response.code == 0xa002
    ));
    walk_ptpip_in(
        &mut e,
        &start_live_view,
        &BTreeMap::new(),
        Some("wireless-tether"),
    )
    .expect("defensive terminate clears the stale stream before arming");
    assert_eq!(e.phase(), Phase::Streaming);
}

#[test]
fn wireless_tether_unterminated_stream_survives_session_close() {
    let manifest = consolidated();
    let start_live_view = manifest
        .action("wireless-tether", ActionVerb::StartLiveView)
        .expect("wireless-tether startLiveView")
        .initiator()
        .expect("startLiveView initiator")
        .steps
        .clone();
    let mut e = engine_with_manifest(manifest);
    e.bind_connection("wireless-tether");
    assert_ok(&e.on_operation(&req(0x1002, 1, vec![1]), None));
    e.apply_state_overlay(
        &serde_json::from_value(serde_json::json!({ "phase": "liveView" })).unwrap(),
    )
    .unwrap();
    walk_ptpip_in(
        &mut e,
        &start_live_view,
        &BTreeMap::new(),
        Some("wireless-tether"),
    )
    .expect("initial live-view stream starts");
    assert_ok(&e.on_operation(&req(0x1003, 20, vec![]), None));
    assert_ok(&e.on_operation(&req(0x1002, 21, vec![2]), None));
    e.apply_state_overlay(
        &serde_json::from_value(serde_json::json!({ "phase": "liveView" })).unwrap(),
    )
    .unwrap();

    assert!(matches!(
        e.on_operation(
            &req(0x1016, 22, vec![0xd1bc]),
            Some(&2u16.to_le_bytes()),
        ),
        Reply::Response(ref response) if response.code == 0xa002
    ));
    assert_ok(&e.on_operation(&req(0x1018, 23, vec![1]), None));
    write_u16(&mut e, 24, 0xd1bc, 2);
}

#[test]
fn wireless_tether_start_live_view_retries_then_tolerates_busy_terminate() {
    let manifest = consolidated();
    let start_live_view = manifest
        .action("wireless-tether", ActionVerb::StartLiveView)
        .expect("wireless-tether startLiveView")
        .initiator()
        .expect("startLiveView initiator")
        .steps
        .clone();
    let mut e = engine_with_manifest(manifest);
    e.bind_connection("wireless-tether");
    assert_ok(&e.on_operation(&req(0x1002, 1, vec![1]), None));
    assert_ok(&e.on_operation(&req(0x1018, 2, vec![1]), None));
    e.apply_state_overlay(
        &serde_json::from_value(serde_json::json!({ "phase": "liveView" })).unwrap(),
    )
    .unwrap();
    e.install_fault(Fault::FailOperationTimes {
        code: 0x1018,
        response: 0x2019,
        remaining: 10,
    });

    let outcome = walk_ptpip_in(
        &mut e,
        &start_live_view,
        &BTreeMap::new(),
        Some("wireless-tether"),
    )
    .expect("exhausted busy retries remain tolerated by startLiveView");
    assert_eq!(outcome.steps_run, 3);
    assert_eq!(outcome.retry_delays_ms, [300; 9]);
    assert_eq!(e.phase(), Phase::Streaming);
}

#[test]
fn large_mov_wire_path_reports_sentinel_and_serves_true_size_with_high_offset() {
    const TRUE_SIZE: u64 = 0x0000_0001_230c_a400;
    const FINAL_LOW: u32 = 0x22ff_cf80;
    const FINAL_LEN: u32 = 0x000c_d480;
    const FINAL_HIGH: u32 = 0x1;
    let (mut e, handle) = engine_with_sparse_mov(TRUE_SIZE);
    assert_ok(&e.on_operation(&req(0x1002, 1, vec![1]), None));

    let object_info = ptp_core::ObjectInfo::decode(&data_of(
        e.on_operation(&req(0x1008, 2, vec![handle]), None),
    ))
    .unwrap();
    assert_eq!(object_info.object_format, fmt::MOV);
    assert_eq!(object_info.object_compressed_size, 0xffff_ffff);

    let true_size = data_of(e.on_operation(&req(0x9803, 3, vec![handle, 0xdc04]), None));
    assert_eq!(u64::from_le_bytes(true_size.try_into().unwrap()), TRUE_SIZE);

    let chunk_size = data_of(e.on_operation(&req(0x1015, 4, vec![0xd235]), None));
    assert_eq!(
        u32::from_le_bytes(chunk_size.try_into().unwrap()),
        0x00bf_ffe0
    );

    let (source, response_params) = stream_of(e.on_operation(
        &req(0x101b, 5, vec![handle, FINAL_LOW, FINAL_LEN, FINAL_HIGH]),
        None,
    ));
    assert_eq!(response_params, vec![FINAL_LEN]);
    assert_eq!(source.len(), FINAL_LEN as u64);
    match source {
        ByteSource::FileRange { offset, len, .. } => {
            assert_eq!(offset, ((FINAL_HIGH as u64) << 32) | FINAL_LOW as u64);
            assert_eq!(offset + len, TRUE_SIZE);
        }
        other => panic!("expected file range, got {other:?}"),
    }
}

#[test]
fn composite_read_ticks_member_settles_like_a_direct_read() {
    // #185: client application observes 0xd209 through its payload container (0xd212),
    // so a pending member transition must tick on composite reads too — else
    // the AF settle armed by 0x9026 never resolves for container consumers
    // and the awaitUntil re-poll spins to its cap.
    let mut e = engine();
    assert_ok(&e.on_operation(&req(0x1002, 1, vec![1]), None)); // OpenSession
    assert_ok(&e.on_operation(&req(0x9026, 2, vec![0x0906_0403]), None)); // arms settle=2

    let d209 = |records: &[(u16, u32)]| {
        records
            .iter()
            .find(|(c, _)| *c == 0xd209)
            .map(|(_, v)| *v)
            .expect("0xd209 present in the composite")
    };
    let first = decode_record_stream(&data_of(
        e.on_operation(&req(0x1015, 3, vec![0xd212]), None),
    ));
    assert_eq!(d209(&first), 0, "first composite read is pre-settle");
    let second = decode_record_stream(&data_of(
        e.on_operation(&req(0x1015, 4, vec![0xd212]), None),
    ));
    assert_eq!(
        d209(&second),
        1,
        "second composite read resolves the settle"
    );
}

#[test]
fn believable_enumeration_from_the_rich_manifest() {
    let mut e = engine();
    assert_ok(&e.on_operation(&req(0x1002, 1, vec![1]), None)); // OpenSession
    let di = DeviceInfo::decode(&data_of(e.on_operation(&req(0x1001, 2, vec![]), None))).unwrap();
    assert_eq!(di.model, "GFX100 II");
    // The serial flows from the manifest identity to the wire (#152) — the app
    // keys a saved camera on it, so an empty string would break auto-merge.
    assert_eq!(
        di.serial_number,
        consolidated().camera.identities["serialNumber"],
        "DeviceInfo.serial_number comes from camera.identities.serialNumber"
    );
    assert!(!di.serial_number.is_empty());
    // The consolidated carries the full probed surface — a believable camera, not a stub.
    assert!(
        di.operations_supported.len() >= 20,
        "ops: {}",
        di.operations_supported.len()
    );
    assert!(
        di.device_properties_supported.len() >= 50,
        "props: {}",
        di.device_properties_supported.len()
    );
    // A real DevicePropDesc comes back for a probed property.
    assert!(di.device_properties_supported.contains(&0x5007));
    let desc = data_of(e.on_operation(&req(0x1014, 3, vec![0x5007]), None));
    let mut r = Reader::new(&desc);
    assert_eq!(
        r.u16().unwrap(),
        0x5007,
        "DevicePropDesc echoes the prop code"
    );
}

#[test]
fn gate3_image_import_choreography_runs_from_the_manifest() {
    let mut e = engine();
    assert_ok(&e.on_operation(&req(0x1002, 1, vec![1]), None)); // OpenSession
                                                                // The image-import entry sequence (df01=0x14, df28=3, vendor-prime chord) — each
                                                                // step responds believably purely from the manifest (vendor ops are no-op OK).
    assert_ok(&e.on_operation(&req(0x1016, 2, vec![0xdf01]), Some(&0x14u16.to_le_bytes())));
    assert_ok(&e.on_operation(&req(0x1016, 3, vec![0xdf28]), Some(&3u32.to_le_bytes())));
    for (i, (op, params)) in [
        (0x9054u16, vec![0x1000_0001u32]),
        (0x9055, vec![0x1000_0001]),
        (0x9050, vec![]),
        (0x9053, vec![0, 0x7530]),
    ]
    .into_iter()
    .enumerate()
    {
        assert_ok(&e.on_operation(&req(op, 10 + i as u32, params), None));
    }
    // Enumerate + download a file (gate #3 proper).
    let handles_bytes = data_of(e.on_operation(&req(0x1007, 20, vec![0x0001_0001, 0, 0]), None));
    let mut r = Reader::new(&handles_bytes);
    let handles = r.ptp_array(|r| r.u32()).unwrap();
    assert_eq!(handles.len(), 1);
    assert_ok(&e.on_operation(&req(0x1008, 21, vec![handles[0]]), None)); // GetObjectInfo
    let part = data_of(e.on_operation(&req(0x101b, 22, vec![handles[0], 0, 7]), None));
    assert_eq!(&part, b"\xFF\xD8HELLO");
}

#[test]
fn image_import_count_and_handles_timeout_before_bootstrap_gate() {
    let mut e = engine();
    assert_ok(&e.on_operation(&req(0x1002, 1, vec![1]), None));
    assert_ok(&e.on_operation(&req(0x1016, 2, vec![0xdf01]), Some(&0x14u16.to_le_bytes())));
    assert_no_response(e.on_operation(&req(0x1015, 3, vec![0xd620]), None));
    assert_no_response(e.on_operation(&req(0x1015, 4, vec![0xd621]), None));
    assert_no_response(e.on_operation(&req(0x1014, 5, vec![0xd620]), None));
    assert_no_response(e.on_operation(&req(0x1014, 6, vec![0xd621]), None));
}

#[test]
fn image_import_gate_requires_the_manifest_declared_bootstrap_sequence() {
    let mut e = engine();
    assert_ok(&e.on_operation(&req(0x1002, 1, vec![1]), None));
    // A suffix that looks like the page tail is not enough: this intentionally
    // omits the earlier manifest-declared prime block.
    assert_ok(&e.on_operation(&req(0x1015, 2, vec![0xd212]), None));
    assert_ok(&e.on_operation(&req(0x1015, 3, vec![0xd22b]), None));
    assert_ok(&e.on_operation(&req(0x9053, 4, vec![0, 0x7530]), None));
    assert_ok(&e.on_operation(&req(0x1015, 5, vec![0xd212]), None));
    assert_no_response(e.on_operation(&req(0x1015, 6, vec![0xd620]), None));
}

#[test]
fn image_import_gate_requires_d22b_in_the_manifest_sequence() {
    let mut e = engine();
    assert_ok(&e.on_operation(&req(0x1002, 1, vec![1]), None));
    assert_ok(&e.on_operation(&req(0x1015, 2, vec![0xd212]), None));
    assert_ok(&e.on_operation(&req(0x1016, 3, vec![0xdf01]), Some(&0x14u16.to_le_bytes())));
    assert_ok(&e.on_operation(&req(0x1015, 4, vec![0xdf28]), None));
    assert_ok(&e.on_operation(&req(0x1016, 5, vec![0xdf28]), Some(&3u32.to_le_bytes())));
    assert_ok(&e.on_operation(&req(0x1016, 6, vec![0xd226]), Some(&0u16.to_le_bytes())));
    assert_ok(&e.on_operation(&req(0x1016, 7, vec![0xd227]), Some(&0u16.to_le_bytes())));
    assert_ok(&e.on_operation(&req(0x1015, 8, vec![0xd244]), None));
    assert_ok(&e.on_operation(&req(0x9054, 9, vec![0x1000_0001]), None));
    assert_ok(&e.on_operation(&req(0x9055, 10, vec![0x1000_0001]), None));
    assert_ok(&e.on_operation(&req(0x9050, 11, vec![]), None));
    assert_ok(&e.on_operation(&req(0x1015, 12, vec![0xd212]), None));
    // Omit D22B: this does not satisfy the manifest-declared bootstrap gate.
    assert_ok(&e.on_operation(&req(0x9053, 13, vec![0, 0x7530]), None));
    assert_ok(&e.on_operation(&req(0x1015, 14, vec![0xd212]), None));
    assert_no_response(e.on_operation(&req(0x1015, 15, vec![0xd620]), None));
}

#[test]
fn image_import_gate_advances_only_on_successful_replies() {
    let m = consolidated();
    let cold = m
        .connections
        .get("app")
        .unwrap()
        .entries
        .iter()
        .find(|e| e.to == "image-transfer" && e.from.is_none())
        .expect("cold image-transfer entry");
    let mut e = engine();
    e.install_fault(Fault::FailOperation {
        code: 0x9054,
        response: 0x2005,
    });
    walk_ptpip_in(&mut e, entry_steps(cold), &BTreeMap::new(), Some("app"))
        .expect("tolerant 0x9054 failure does not abort the entry");
    assert_no_response(e.on_operation(&req(0x1015, 50, vec![0xd620]), None));
}

#[test]
fn image_import_full_bootstrap_unlocks_count_and_handle_properties() {
    let m = consolidated();
    let cold = m
        .connections
        .get("app")
        .unwrap()
        .entries
        .iter()
        .find(|e| e.to == "image-transfer" && e.from.is_none())
        .expect("cold image-transfer entry");
    let mut e = engine();
    walk_ptpip_in(&mut e, entry_steps(cold), &BTreeMap::new(), Some("app"))
        .expect("cold image-transfer entry succeeds");
    let enumerate = m
        .action("app", ActionVerb::EnumerateObjects)
        .expect("enumerateObjects action");
    walk_ptpip_in(
        &mut e,
        &enumerate.initiator().unwrap().steps[..1],
        &BTreeMap::new(),
        Some("app"),
    )
    .expect("enumeration prime succeeds");

    let count = data_of(e.on_operation(&req(0x1015, 50, vec![0xd620]), None));
    let mut r = Reader::new(&count);
    assert_eq!(r.u32().unwrap(), 1);
    let handles_bytes = data_of(e.on_operation(&req(0x1015, 51, vec![0xd621]), None));
    let mut r = Reader::new(&handles_bytes);
    let handles = r.ptp_array(|r| r.u32()).unwrap();
    assert_eq!(handles.len(), 1);

    assert_ok(&e.on_operation(&req(0x1015, 52, vec![0xd212]), None));
    let count = data_of(e.on_operation(&req(0x1015, 53, vec![0xd620]), None));
    let mut r = Reader::new(&count);
    assert_eq!(r.u32().unwrap(), 1);

    assert_ok(&e.on_operation(&req(0x1016, 54, vec![0xdf01]), Some(&0x16u16.to_le_bytes())));
    assert_no_response(e.on_operation(&req(0x1015, 55, vec![0xd620]), None));
}

#[test]
fn image_import_retries_transient_prime_and_count_responses() {
    let (mut engine, enumerate) = image_import_ready();
    engine.install_fault(Fault::FailOperationTimes {
        code: 0x9050,
        response: 0x2019,
        remaining: 1,
    });
    let prime = walk_ptpip_in(
        &mut engine,
        &enumerate.initiator().unwrap().steps[..1],
        &BTreeMap::new(),
        Some("app"),
    )
    .expect("DeviceBusy enumeration prime recovers");
    assert_eq!(prime.retry_delays_ms, [100]);

    engine.clear_faults();
    engine.install_fault(Fault::FailOperationTimes {
        code: 0x1015,
        response: 0x2002,
        remaining: 1,
    });
    let count = walk_ptpip_in(
        &mut engine,
        &enumerate.initiator().unwrap().steps[1..2],
        &BTreeMap::new(),
        Some("app"),
    )
    .expect("transient GeneralError count read recovers");
    assert_eq!(count.retry_delays_ms, [1000]);
    assert_eq!(count.observed.get(0xd620), Some(1));
}

#[test]
fn image_import_prime_retries_selected_decode_failure() {
    let (mut engine, enumerate) = image_import_ready();
    engine.install_fault(Fault::TruncateDataParamsTimes {
        code: 0x1015,
        params: vec![0xd212],
        keep: 4,
        remaining: 1,
    });
    let outcome = walk_ptpip_in(
        &mut engine,
        &enumerate.initiator().unwrap().steps[..1],
        &BTreeMap::new(),
        Some("app"),
    )
    .expect("selected transient decode failure retries");
    assert_eq!(outcome.retry_delays_ms, [100]);
}

#[test]
fn image_import_prime_does_not_retry_unselected_decode_failure() {
    let (mut engine, enumerate) = image_import_ready_with_manifest(without_decode_retry());
    engine.install_fault(Fault::TruncateDataParamsTimes {
        code: 0x1015,
        params: vec![0xd212],
        keep: 4,
        remaining: 1,
    });
    let error = walk_ptpip_in(
        &mut engine,
        &enumerate.initiator().unwrap().steps[..1],
        &BTreeMap::new(),
        Some("app"),
    )
    .expect_err("unselected decode failure escapes without retry");
    assert!(error.message.contains("decode prop 0xd212"));
}

#[test]
fn enumerate_objects_executes_the_captured_handle_collection() {
    let (mut engine, enumerate) = image_import_ready();
    let outcome = walk_ptpip_in(
        &mut engine,
        &enumerate.initiator().unwrap().steps,
        &BTreeMap::new(),
        Some("app"),
    )
    .expect("complete enumerateObjects action succeeds");
    assert_eq!(outcome.observed.get(0xd620), Some(1));
}

#[test]
fn import_objects_recovers_each_shared_enumeration_boundary() {
    let manifest = consolidated();
    let action = manifest
        .action("app", ActionVerb::ImportObjects)
        .expect("importObjects action");
    let cases = [
        (
            Fault::FailOperationTimes {
                code: 0x9050,
                response: 0x2019,
                remaining: 1,
            },
            100,
        ),
        (
            Fault::FailOperationParamsTimes {
                code: 0x1015,
                params: vec![0xd620],
                response: 0x2002,
                remaining: 1,
            },
            1000,
        ),
        (
            Fault::FailOperationParamsTimes {
                code: 0x1015,
                params: vec![0xd621],
                response: 0x2002,
                remaining: 1,
            },
            1000,
        ),
    ];

    for (fault, expected_delay) in cases {
        let mut engine = engine_with_jpegs(1);
        engine.install_fault(fault);
        let outcome = walk_ptpip_in(
            &mut engine,
            &action.initiator().unwrap().steps,
            &BTreeMap::new(),
            Some("app"),
        )
        .expect("shared recovery succeeds without replaying transfer work");
        assert_eq!(outcome.retry_delays_ms, [expected_delay]);
        assert_eq!(outcome.loop_iterations, [1, 1]);
    }
}

#[test]
fn image_import_handle_retry_exhausts_with_typed_response() {
    let (mut engine, enumerate) = image_import_ready();
    engine.install_fault(Fault::FailOperationParamsTimes {
        code: 0x1015,
        params: vec![0xd621],
        response: 0x2002,
        remaining: 3,
    });
    let error = walk_ptpip_in(
        &mut engine,
        &enumerate.initiator().unwrap().steps,
        &BTreeMap::new(),
        Some("app"),
    )
    .expect_err("three GeneralError handle reads exhaust the declared budget");
    assert_eq!(error.response_code, Some(0x2002));
}

#[test]
fn image_import_count_retry_exhausts_with_typed_response() {
    let (mut engine, enumerate) = image_import_ready();
    walk_ptpip_in(
        &mut engine,
        &enumerate.initiator().unwrap().steps[..1],
        &BTreeMap::new(),
        Some("app"),
    )
    .expect("enumeration prime succeeds");
    engine.install_fault(Fault::FailOperationTimes {
        code: 0x1015,
        response: 0x2002,
        remaining: 3,
    });
    let error = walk_ptpip_in(
        &mut engine,
        &enumerate.initiator().unwrap().steps[1..2],
        &BTreeMap::new(),
        Some("app"),
    )
    .expect_err("three GeneralError responses exhaust the declared budget");
    assert_eq!(error.response_code, Some(0x2002));
}

#[test]
fn image_import_count_does_not_retry_unselected_or_transport_failures() {
    for fault in [
        Fault::FailOperationTimes {
            code: 0x1015,
            response: 0x2005,
            remaining: 1,
        },
        Fault::CloseOnOperation { code: 0x1015 },
    ] {
        let (mut engine, enumerate) = image_import_ready();
        walk_ptpip_in(
            &mut engine,
            &enumerate.initiator().unwrap().steps[..1],
            &BTreeMap::new(),
            Some("app"),
        )
        .expect("enumeration prime succeeds");
        engine.install_fault(fault.clone());
        let error = walk_ptpip_in(
            &mut engine,
            &enumerate.initiator().unwrap().steps[1..2],
            &BTreeMap::new(),
            Some("app"),
        )
        .expect_err("unselected failure escapes immediately");
        match fault {
            Fault::FailOperationTimes { .. } => assert_eq!(error.response_code, Some(0x2005)),
            Fault::CloseOnOperation { .. } => assert_eq!(error.response_code, None),
            Fault::FailOperation { .. }
            | Fault::FailOperationParamsTimes { .. }
            | Fault::TruncateDataParamsTimes { .. } => unreachable!(),
        }
    }
}

#[test]
fn gate4_live_view_choreography_runs_from_the_manifest() {
    let mut e = engine();
    assert_ok(&e.on_operation(&req(0x1002, 1, vec![1]), None)); // OpenSession
                                                                // Live-view entry: df00=6, df01=0x16 (→ live-view phase), df2a read-echo, 902B×4.
    assert_ok(&e.on_operation(&req(0x1016, 2, vec![0xdf00]), Some(&6u16.to_le_bytes())));
    assert_ok(&e.on_operation(&req(0x1016, 3, vec![0xdf01]), Some(&0x16u16.to_le_bytes())));
    let df2a = data_of(e.on_operation(&req(0x1015, 4, vec![0xdf2a]), None)); // GetDevicePropValue
    assert_ok(&e.on_operation(&req(0x1016, 5, vec![0xdf2a]), Some(&df2a))); // echo back
    for i in 0..4 {
        assert_ok(&e.on_operation(&req(0x902b, 6 + i, vec![]), None)); // FujiVendor_902B ×4
    }
    // InitiateOpenCapture only succeeds once in the live-view phase → streaming.
    assert_ok(&e.on_operation(&req(0x101c, 10, vec![]), None));
}

#[test]
fn af_lock_round_trips_from_the_consolidated_manifest() {
    let mut e = engine();
    let steps = vec![
        Step {
            send_op: Some("0x9026".into()),
            params: vec![StepParam::Literal(0x0906_0403)],
            ..Default::default()
        },
        Step {
            await_until: Some(AwaitUntil {
                source: AwaitSource::Poll {
                    prop: "0xd209".into(),
                },
                until: Predicate::Leaf(Leaf {
                    prop: "0xd209".into(),
                    mask: None,
                    eq: Some(1),
                    ne: None,
                    lt: None,
                    gt: None,
                }),
                on_each: vec![],
                timeout_ms: 5000,
                interval_ms: 250,
            }),
            ..Default::default()
        },
    ];

    let out = walk_ptpip(&mut e, &steps, &BTreeMap::new())
        .expect("AF lock flow should round-trip via real gfx100ii manifest");
    // 0x9026 settles 0xd209 in 2 polls — the manifest models the measured
    // fw02.30 latency (0xC005 fires before 0xD209 latches; client application#157), and
    // await sources re-poll instead of assuming a single post-event read (#185
    // retired the old "settle ≤1 event-coupling" invariant from #42).
    assert_eq!(out.await_iterations, vec![2]);
    assert_eq!(out.observed.get(0xd209), Some(1));
}

#[test]
fn pcss_autofocus_responder_transitions_use_bundled_invocations() {
    let mut e = engine();
    e.bind_connection("wireless-tether");
    assert_ok(&e.on_operation(&req(0x1002, 1, vec![1]), None));

    invoke_pcss_autofocus_responder(&mut e, "autofocusLock", None);
    assert_eq!(read_u16(&mut e, 2, 0xd209), 1);
    assert_eq!(
        read_u16(&mut e, 3, 0xd209),
        3,
        "default responder result is failure"
    );

    invoke_pcss_autofocus_responder(&mut e, "autofocusLock", Some(2));
    assert_eq!(read_u16(&mut e, 4, 0xd209), 1);
    assert_eq!(
        read_u16(&mut e, 5, 0xd209),
        2,
        "explicit responder result is success"
    );

    invoke_pcss_autofocus_responder(&mut e, "autofocusRelease", None);
    assert_eq!(
        read_u16(&mut e, 6, 0xd209),
        4,
        "release reset is immediate simulator policy"
    );
}

#[test]
fn pcss_autofocus_initiator_encodes_runtime_focus_area_as_ptp_string() {
    let manifest = consolidated();
    let action = manifest
        .action("wireless-tether", ActionVerb::AutofocusLock)
        .expect("bundled PCSS autofocusLock")
        .clone();
    let mut e = engine();
    e.bind_connection("wireless-tether");
    assert_ok(&e.on_operation(&req(0x1002, 1, vec![1]), None));
    invoke_pcss_autofocus_responder(&mut e, "autofocusLock", Some(2));

    walk_ptpip_in(
        &mut e,
        &action.initiator().expect("lock initiator").steps,
        &BTreeMap::from([("focusArea".into(), "-12,34,5".into())]),
        Some("wireless-tether"),
    )
    .expect("runtime focus area executes");

    assert_eq!(
        data_of(e.on_operation(&req(0x1015, 100, vec![0xd395]), None)),
        vec![9, b'-', 0, b'1', 0, b'2', 0, b',', 0, b'3', 0, b'4', 0, b',', 0, b'5', 0, 0, 0,]
    );
}

#[test]
fn pcss_autofocus_initiator_times_out_while_d209_remains_operating() {
    let manifest = consolidated();
    let action = manifest
        .action("wireless-tether", ActionVerb::AutofocusLock)
        .expect("bundled PCSS autofocusLock")
        .clone();
    let mut e = engine();
    e.bind_connection("wireless-tether");
    let overlay: StateOverlay = serde_json::from_value(serde_json::json!({
        "props": { "0xd209": 1 }
    }))
    .unwrap();
    e.apply_state_overlay(&overlay).unwrap();

    let error = walk_ptpip_in(
        &mut e,
        &action.initiator().expect("lock initiator").steps,
        &BTreeMap::new(),
        Some("wireless-tether"),
    )
    .expect_err("D209=1 never satisfies the success/failure predicate");
    assert_eq!(error.step, "steps[4].awaitUntil");
    assert!(
        error.message.contains("not satisfied polling 0xd209"),
        "{error:?}"
    );
    assert_eq!(read_u16(&mut e, 100, 0xd209), 1);
}

#[test]
fn autofocus_transitions_do_not_change_shared_initiate_capture_behavior() {
    let manifest = consolidated();
    assert!(manifest.operations["0x100e"].effects.is_empty());
    assert!(manifest.connections["wireless-tether"]
        .bindings
        .as_ref()
        .is_some_and(|bindings| bindings.event.is_none()));
    let app_shutter = manifest
        .action("app", ActionVerb::Shutter)
        .expect("ordinary app shutter remains modeled");
    assert_eq!(app_shutter.initiator().expect("app shutter").steps.len(), 3);

    let mut e = engine();
    e.bind_connection("wireless-tether");
    assert_ok(&e.on_operation(&req(0x1002, 1, vec![1]), None));
    let overlay: StateOverlay = serde_json::from_value(serde_json::json!({
        "props": { "0xd209": 1 }
    }))
    .unwrap();
    e.apply_state_overlay(&overlay).unwrap();
    assert_ok(&e.on_operation(&req(0x100e, 2, vec![0, 0]), None));
    assert_eq!(
        read_u16(&mut e, 3, 0xd209),
        1,
        "0x100E does not arm an autofocus transition"
    );
}

#[test]
fn reopen_session_is_refused_over_a_volatile_listener_connection() {
    // #103 negative oracle: the GFX100 II `app` Wi-Fi-AP path tears down the :55740
    // command-port listener on a live-view transport-close, so a reopenSession's
    // reconnect from active streaming is refused — the reference executor must
    // error, matching the device.
    let m = consolidated();
    let live = m.connections["app"]
        .entries
        .iter()
        .find(|e| e.to == "shooting/stills" && e.from.is_none())
        .expect("cold live-view entry");
    let reopen = vec![Step {
        reopen_session: Some(ReopenSession {}),
        ..Default::default()
    }];

    // Over `app` (commandListenerVolatile: true) from streaming → refused.
    let mut e = engine();
    walk_ptpip_in(&mut e, entry_steps(live), &BTreeMap::new(), Some("app"))
        .expect("live-view entry reaches streaming");
    assert!(matches!(e.phase(), Phase::Streaming));
    let err = walk_ptpip_in(&mut e, &reopen, &BTreeMap::new(), Some("app")).unwrap_err();
    assert!(
        err.message.contains("refused the reconnect"),
        "expected a reopen refusal, got: {}",
        err.message
    );

    // With no volatile connection bound → reopen still re-opens in place (unchanged).
    let mut e2 = engine();
    walk_ptpip_in(&mut e2, &reopen, &BTreeMap::new(), None)
        .expect("a non-volatile connection still allows an in-place reopen");
}

#[test]
fn live_view_to_image_transfer_reestablishes_then_runs_cold_entry() {
    let m = consolidated();
    let app = &m.connections["app"];
    let live = app
        .entries
        .iter()
        .find(|e| e.to == "shooting/stills" && e.from.is_none())
        .expect("cold live-view entry");
    let xfer = app
        .entries
        .iter()
        .find(|e| e.to == "image-transfer" && e.from.as_deref() == Some("shooting/stills"))
        .expect("live-view → image-transfer transition");
    let reestablish = match &xfer.execution {
        ModeEntryExecution::ReestablishConnection(plan) => plan,
        other => panic!("expected re-establishment, got {other:?}"),
    };
    assert_eq!(
        reestablish.params.get("launchMode").map(String::as_str),
        Some("3")
    );

    // The old session only runs the exit plan; no cold vendor bootstrap is sent
    // until a fresh PTP session exists.
    let mut old = engine();
    let params = BTreeMap::from([("openCaptureTxId".to_string(), "1".to_string())]);
    walk_ptpip_in(&mut old, entry_steps(live), &params, Some("app"))
        .expect("live-view entry reaches streaming");
    walk_ptpip_in(&mut old, &reestablish.exit_steps, &params, Some("app"))
        .expect("old session exits orderly");
    assert!(matches!(old.phase(), Phase::Closed));
    assert!(reestablish
        .exit_steps
        .iter()
        .all(|step| { !matches!(step.send_op.as_deref(), Some("0x9050" | "0x9053")) }));

    // Compose the real manufacturer-index establishment plan before creating
    // the fresh PTP engine. This catches stale parameter names and BLE/AP drift.
    let view = ResolvedManufacturerIndex::from_yaml(&data("fuji/index.yaml"))
        .expect("Fuji index loads")
        .models
        .into_iter()
        .find(|model| model.id == "gfx100ii")
        .expect("GFX100 II model view");
    let ble = view.ble.as_ref().expect("GFX100 II inherits BLE data");
    let establishment = ble
        .establishment("ble-establish-wifi-ap")
        .expect("app establishment plan");
    assert_eq!(
        establishment.params,
        reestablish.params.keys().cloned().collect::<Vec<_>>(),
        "the mode edge binds every establishment parameter exactly"
    );
    let gatt = |name: &str| {
        ble.gatt
            .get(name)
            .unwrap_or_else(|| panic!("GATT catalog contains {name}"))
            .clone()
    };
    let ap_state = gatt("apState");
    let launch = gatt("functionLaunchRequest");
    let ssid = gatt("cameraSSIDNameString");
    let passphrase = gatt("cameraWiFiPassphraseString");
    let mut responder = BleResponder::new(vec![
        ap_state.clone(),
        launch.clone(),
        ssid.clone(),
        passphrase.clone(),
        gatt("imageTransferSetting"),
    ])
    .serve_read_sequence(&ap_state, vec![vec![0x00, 0x80], vec![0x02, 0x80]])
    .queue_notification_after_fenced_write(&ap_state, &launch, 1, &[0x01, 0x80])
    .serve_read(&ssid, b"GFX100II-TEST")
    .serve_read(&passphrase, b"test-passphrase");
    let readiness = walk_establishment(
        &mut responder,
        &establishment.post_exit_readiness,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &reestablish.params,
    )
    .expect("post-exit readiness reaches the relaunchable baseline");
    walk_establishment(
        &mut responder,
        &establishment.steps,
        &readiness.scope,
        &BTreeMap::new(),
        &reestablish.params,
    )
    .expect("image-import BLE/AP establishment completes");
    assert_eq!(responder.written(&launch), &[&[0x03, 0x00][..]]);

    let cold = app
        .entries
        .iter()
        .find(|entry| entry.to == "image-transfer" && entry.from.is_none())
        .expect("cold image-transfer entry");
    let mut fresh = engine();
    walk_ptpip_in(&mut fresh, entry_steps(cold), &BTreeMap::new(), Some("app"))
        .expect("fresh session runs cold image-transfer entry");
    let enumerate = app
        .actions
        .get(&ActionVerb::EnumerateObjects)
        .expect("enumerateObjects action");
    walk_ptpip_in(
        &mut fresh,
        &enumerate.initiator().unwrap().steps[..1],
        &BTreeMap::new(),
        Some("app"),
    )
    .expect("fresh session runs enumeration prime");
    let handles = data_of(fresh.on_operation(&req(0x1015, 1, vec![0xd621]), None));
    let mut reader = Reader::new(&handles);
    assert!(!reader.ptp_array(|r| r.u32()).unwrap().is_empty());
}

#[test]
fn image_transfer_to_live_view_reopens_then_streams() {
    // #180 reverse edge: image-transfer has no open-capture stream, so the
    // reference-app Get→Take path can re-establish the PTP/IP session and then
    // bring live-view back up.
    let m = consolidated();
    let app = &m.connections["app"];
    let xfer = app
        .entries
        .iter()
        .find(|e| e.to == "image-transfer" && e.from.is_none())
        .expect("cold image-transfer entry");
    let live = app
        .entries
        .iter()
        .find(|e| e.to == "shooting/stills" && e.from.as_deref() == Some("image-transfer"))
        .expect("image-transfer → live-view entry");
    assert!(
        entry_steps(live)[0].reopen_session.is_some(),
        "Get→Take begins with the reconnect observed in reference app"
    );

    let mut e = engine();
    walk_ptpip_in(&mut e, entry_steps(xfer), &BTreeMap::new(), Some("app"))
        .expect("cold image-transfer entry runs");
    assert!(matches!(e.phase(), Phase::ImageImport));

    walk_ptpip_in(&mut e, entry_steps(live), &BTreeMap::new(), Some("app"))
        .expect("image-transfer → live-view edge runs");
    assert!(matches!(e.phase(), Phase::Streaming));
}

#[test]
fn d246_stills_video_selector_keeps_live_view_streaming() {
    // #180: the in-shooter stills/video selector is a property write, not a
    // movie-record command, import bootstrap, reconnect, or live-view restart.
    let m = consolidated();
    let app = &m.connections["app"];
    let live = app
        .entries
        .iter()
        .find(|e| e.to == "shooting/stills" && e.from.is_none())
        .expect("cold live-view entry");
    let to_video = app
        .entries
        .iter()
        .find(|e| e.to == "shooting/video" && e.from.as_deref() == Some("shooting/stills"))
        .expect("stills → video entry");
    let to_stills = app
        .entries
        .iter()
        .find(|e| e.to == "shooting/stills" && e.from.as_deref() == Some("shooting/video"))
        .expect("video → stills entry");

    let mut e = engine();
    walk_ptpip_in(&mut e, entry_steps(live), &BTreeMap::new(), Some("app"))
        .expect("live-view entry reaches streaming");
    assert!(matches!(e.phase(), Phase::Streaming));

    walk_ptpip_in(&mut e, entry_steps(to_video), &BTreeMap::new(), Some("app"))
        .expect("D246 stills→video selector runs");
    assert!(matches!(e.phase(), Phase::Streaming));
    assert_eq!(
        data_of(e.on_operation(&req(0x1015, 100, vec![0xd246]), None)),
        vec![1],
        "D246=1 after selecting video"
    );

    walk_ptpip_in(
        &mut e,
        entry_steps(to_stills),
        &BTreeMap::new(),
        Some("app"),
    )
    .expect("D246 video→stills selector runs");
    assert!(matches!(e.phase(), Phase::Streaming));
    assert_eq!(
        data_of(e.on_operation(&req(0x1015, 101, vec![0xd246]), None)),
        vec![0],
        "D246=0 after selecting stills"
    );
}

/// An engine whose card holds `count` small JPGs (each one 12 MiB chunk).
fn engine_with_jpegs(count: usize) -> Engine {
    engine_with_jpegs_and_handles(count).0
}

fn engine_with_jpegs_and_handles(count: usize) -> (Engine, Vec<u32>) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("ptpsim-gate-import-{nanos}"));
    let dir = root.join("DCIM/100_FUJI");
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..count {
        std::fs::write(
            dir.join(format!("DSCF{i:04}.JPG")),
            b"\xFF\xD8HELLOJPEG\xFF\xD9",
        )
        .unwrap();
    }
    let mut store = MediaStore::open(&root).unwrap();
    store.scan().unwrap();
    let handles = store.handles(ObjectQuery {
        format: Some(ptp_core::codes::format::EXIF_JPEG),
        ..Default::default()
    });
    (Engine::new(consolidated(), store), handles)
}

#[test]
fn import_objects_runs_the_full_transfer_from_the_consolidated() {
    // #46 keystone: the REAL importObjects action (arm → enumerate → forEach
    // handle { getObjectInfo → chunk } → idle) walks end-to-end from the
    // consolidated manifest, so sim and device run the identical path. Each small
    // JPG is one 12 MiB window; three handles → three single-chunk downloads.
    let m = consolidated();
    let action = m
        .action("app", ActionVerb::ImportObjects)
        .expect("app.actions.importObjects in the consolidated");
    let mut e = engine_with_jpegs(3);
    let outcome = walk_ptpip_in(
        &mut e,
        &action.initiator().unwrap().steps,
        &BTreeMap::new(),
        Some("app"),
    )
    .expect("importObjects walks the armed enumerate→forEach→chunk path");
    assert_eq!(
        outcome.loop_iterations,
        vec![1, 1, 1, 3],
        "one chunk per handle, then forEach visited all three handles",
    );
}

#[test]
fn import_objects_never_retries_a_per_handle_body_failure() {
    let m = consolidated();
    let action = m
        .action("app", ActionVerb::ImportObjects)
        .expect("app.actions.importObjects in the consolidated");
    let (mut engine, handles) = engine_with_jpegs_and_handles(3);
    let second_handle = handles[1];
    engine.install_fault(Fault::FailOperationParamsTimes {
        code: 0x101b,
        params: vec![second_handle, 0, 13, 0],
        response: 0x2019,
        remaining: 1,
    });
    let error = walk_ptpip_in(
        &mut engine,
        &action.initiator().unwrap().steps,
        &BTreeMap::new(),
        Some("app"),
    )
    .expect_err("a body failure escapes instead of replaying the collection loop");
    assert_eq!(error.response_code, Some(0x2019));
    assert!(error.step.contains("forEach[1]"));
}

#[test]
fn import_objects_uses_extension_true_size_for_sentinel_mov() {
    const TRUE_SIZE: u64 = 0x0000_0001_230c_a400;
    let m = consolidated();
    let action = m
        .action("app", ActionVerb::ImportObjects)
        .expect("app.actions.importObjects in the consolidated");
    let (mut e, _) = engine_with_sparse_mov(TRUE_SIZE);
    let outcome = walk_ptpip_in(
        &mut e,
        &action.initiator().unwrap().steps,
        &BTreeMap::new(),
        Some("app"),
    )
    .expect("large MOV import walks through the true 64-bit size");
    assert_eq!(
        outcome.loop_iterations,
        vec![389, 1],
        "388 full 0x00bfffe0 chunks plus the final short chunk, then one handle",
    );
}

#[test]
fn import_objects_does_not_query_extension_size_below_sentinel() {
    const BOUNDARY_SIZE: u64 = 4_053_173_760;
    let m = consolidated();
    let action = m.action("app", ActionVerb::ImportObjects).unwrap();
    let (mut e, _) = engine_with_sparse_mov(BOUNDARY_SIZE);
    e.install_fault(Fault::FailOperation {
        code: 0x9803,
        response: 0x2002,
    });
    let outcome = walk_ptpip_in(
        &mut e,
        &action.initiator().unwrap().steps,
        &BTreeMap::new(),
        Some("app"),
    )
    .expect("sub-sentinel MOV import must not call the extension-size op");
    assert_eq!(
        outcome.loop_iterations,
        vec![323, 1],
        "sub-sentinel MOV uses ObjectInfo size directly",
    );
}

#[test]
fn import_objects_over_empty_card_downloads_nothing() {
    // The negative oracle: an armed import against a card with no transferable
    // objects enumerates an empty handle list and downloads nothing — the forEach
    // runs zero iterations, no chunk loop fires.
    let m = consolidated();
    let action = m.action("app", ActionVerb::ImportObjects).unwrap();
    let mut e = engine_with_jpegs(0);
    let outcome = walk_ptpip_in(
        &mut e,
        &action.initiator().unwrap().steps,
        &BTreeMap::new(),
        Some("app"),
    )
    .expect("importObjects walks even with nothing to transfer");
    assert_eq!(
        outcome.loop_iterations,
        vec![0],
        "forEach over an empty card is a no-op",
    );
}
