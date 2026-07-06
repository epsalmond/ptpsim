//! DESIGN gates #3 (ImageImport) + #4 (LiveView): the simulator runs the GFX100 II
//! choreography PURELY from the consolidated manifest — including the vendor-op
//! "chords" — and enumerates believably. Engine-level (the service smoke test covers
//! the TCP path). This is the vcam-replacement believability gate.

use camera_config::model::ReopenSession;
use camera_config::{
    ActionVerb, AwaitSource, AwaitUntil, CameraManifest, Leaf, Predicate, Step, StepParam,
};
use camera_media_store::MediaStore;
use camera_sim::{walk_ptpip, walk_ptpip_in, Engine, Reply};
use ptp_core::{DeviceInfo, OperationRequest, Reader};
use std::collections::BTreeMap;
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

#[test]
fn d212_live_status_emits_member_record_stream_from_the_descriptor() {
    let mut e = engine();
    assert_ok(&e.on_operation(&req(0x1002, 1, vec![1]), None)); // OpenSession

    // The 0xD212 readback is the manifest-descriptor-driven record stream, not a
    // hand-coded blob — its members come from the payload descriptor (#51).
    let bytes = data_of(e.on_operation(&req(0x1015, 2, vec![0xd212]), None));
    let records = decode_record_stream(&bytes);
    assert_eq!(records.len(), 26, "all 26 descriptor members emitted");
    // The named sub-fields the bundle carries survive into the stream.
    for code in [0x5007u16, 0xd17c, 0xd209, 0xd02a] {
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
fn reopen_session_is_refused_over_a_volatile_listener_connection() {
    // #103 negative oracle: the GFX100 II `app` Wi-Fi-AP path tears down the :55740
    // command-port listener on the transport-close, so a reopenSession's reconnect
    // is refused — the reference executor must error, matching the device.
    let reopen = vec![Step {
        reopen_session: Some(ReopenSession {}),
        ..Default::default()
    }];

    // Over `app` (commandListenerVolatile: true) → refused.
    let mut e = engine();
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
fn live_view_to_image_transfer_switches_in_session() {
    // #103 fix: the live-view → image-transfer transition runs over the existing
    // :55740 socket with NO reopenSession — the only flow the camera accepts. If a
    // reopen were (re-)added, the oracle above would refuse it over the `app`
    // connection and this end-to-end walk would fail.
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
    assert!(
        xfer.steps.iter().all(|s| s.reopen_session.is_none()),
        "the transition must switch in-session, not reopen (#103)"
    );

    // Run live-view bring-up then the transition as one in-session walk over `app`.
    let mut steps = live.steps.clone();
    steps.extend(xfer.steps.clone());
    let mut e = engine();
    let params = BTreeMap::from([("openCaptureTxId".to_string(), "1".to_string())]);
    walk_ptpip_in(&mut e, &steps, &params, Some("app"))
        .expect("in-session live-view → image-transfer flow runs end-to-end");
}

/// An engine whose card holds `count` small JPGs (each one 12 MiB chunk).
fn engine_with_jpegs(count: usize) -> Engine {
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
    Engine::new(consolidated(), store)
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
    let outcome = walk_ptpip_in(&mut e, &action.steps, &BTreeMap::new(), Some("app"))
        .expect("importObjects walks the armed enumerate→forEach→chunk path");
    assert_eq!(
        outcome.loop_iterations,
        vec![1, 1, 1, 3],
        "one chunk per handle, then forEach visited all three handles",
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
    let outcome = walk_ptpip_in(&mut e, &action.steps, &BTreeMap::new(), Some("app"))
        .expect("importObjects walks even with nothing to transfer");
    assert_eq!(
        outcome.loop_iterations,
        vec![0],
        "forEach over an empty card is a no-op",
    );
}
