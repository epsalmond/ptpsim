//! DESIGN gates #3 (ImageImport) + #4 (LiveView): the simulator runs the GFX100 II
//! choreography PURELY from the consolidated manifest — including the vendor-op
//! "chords" — and enumerates believably. Engine-level (the service smoke test covers
//! the TCP path). This is the vcam-replacement believability gate.

use camera_config::CameraManifest;
use camera_media_store::MediaStore;
use camera_sim::{Engine, Reply};
use ptp_core::{DeviceInfo, OperationRequest, Reader};
use std::path::PathBuf;

fn consolidated() -> CameraManifest {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data/fuji/gfx100ii/gfx100ii.consolidated.yaml");
    CameraManifest::from_yaml(&std::fs::read_to_string(&p).unwrap())
        .unwrap_or_else(|e| panic!("consolidated loads: {e}"))
}

fn engine() -> Engine {
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
    Engine::new(consolidated(), store)
}

fn req(code: u16, tid: u32, params: Vec<u32>) -> OperationRequest {
    OperationRequest {
        data_phase_info: 1,
        code,
        transaction_id: tid,
        params,
    }
}

fn assert_ok(reply: &Reply) {
    match reply {
        Reply::Response(r) => assert_eq!(r.code, 0x2001, "expected OK"),
        Reply::Data { response, .. } => assert_eq!(response.code, 0x2001, "expected OK"),
        Reply::DataStream { response, .. } => assert_eq!(response.code, 0x2001, "expected OK"),
        Reply::Close => panic!("unexpected Close"),
    }
}

fn data_of(reply: Reply) -> Vec<u8> {
    match reply {
        Reply::Data { data, response } => {
            assert_eq!(response.code, 0x2001, "OK expected");
            data
        }
        Reply::DataStream { source, response } => {
            assert_eq!(response.code, 0x2001, "OK expected");
            source.read().expect("realize stream")
        }
        other => panic!("expected Data, got {other:?}"),
    }
}

#[test]
fn believable_enumeration_from_the_rich_manifest() {
    let mut e = engine();
    assert_ok(&e.on_operation(&req(0x1002, 1, vec![1]), None)); // OpenSession
    let di = DeviceInfo::decode(&data_of(e.on_operation(&req(0x1001, 2, vec![]), None))).unwrap();
    assert_eq!(di.model, "GFX100 II");
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
