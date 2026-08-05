//! `camera-sim` — the generic responder engine. One engine runs every camera
//! from manifest data; there are no per-manufacturer branches. The Fuji-specific
//! bits it needs (compressed framing, live-view packetization, computed quirks)
//! come from `protocol-primitives`, referenced by manifest id.

pub mod ble;
pub mod engine;
pub mod fault;
pub mod framesource;
pub mod link;
pub mod ptpip;
pub mod state;
pub mod state_overlay;
pub mod usb;

pub use ble::{
    walk_establishment, BleError, BleEvent, BleResponder, EstablishmentConfirmOutcome,
    EstablishmentWalkSummary, WalkError, WalkOutcome,
};
pub use engine::{
    Engine, PreparedPropertyTransition, PreparedResponderMutation, QueueStats, Reply,
    StreamCompletion, TransferQueueStats,
};
pub use fault::{
    AppliedFault, DataOrResponse, FaultApplication, FaultMutation, FaultSelector, FaultSet,
    FaultSpec, FaultStage, FaultView, WirePlan,
};
pub use framesource::{FrameSource, LoopingFrameSource, StaticFrameSource};
pub use link::{CameraLink, SharedLink};
pub use ptpip::{walk_ptpip, walk_ptpip_in, PtpIpError, PtpIpOutcome};
pub use state::{CameraState, Phase};
pub use state_overlay::{AppliedStateOverlay, StateOverlay};
pub use usb::{UsbError, UsbEvent, UsbResponder, UsbTxnReply};

#[cfg(test)]
mod tests {
    use super::*;
    use camera_config::CameraManifest;
    use camera_media_store::MediaStore;
    use ptp_core::dataset::PropValue;
    use ptp_core::{OperationRequest, Reader, Writer};
    use std::fs::File;
    use std::io::Write;
    use std::path::{Path, PathBuf};

    const RAF_THUMBNAIL: &[u8] = b"\xff\xd8TINY-THUMB\xff\xd9";

    const MANIFEST: &str = r#"
schema: camera-config/v1
camera:
  manufacturer: FUJIFILM
  model: GFX100 II
  firmware: "2.30"
operations:
  "0x1002": { name: OpenSession, owner: standard-ptp }
  "0x9054": { name: GetCurrentObjectMeta, owner: fuji-vendor, workflows: [imageImport] }
  "0x101c": { name: InitiateOpenCapture, owner: standard-ptp, workflows: [liveView] }
  "0x902d":
    name: StepFNumber
    owner: fuji-vendor
    workflows: [liveView]
    handler: property.step
    property: "0x5007"
properties:
  "0xdf01": { name: functionMode, type: u16, access: readWrite }
  "0xd02a":
    name: stillIso
    type: u32
    access: readWrite
    descriptor: { form: range, values: [100, 12800, 1] }
  "0x5007":
    name: aperture
    type: u16
    access: readWrite
    descriptor: { form: enum, values: [280, 400, 560, 800, 1600] }
    controls:
      liveView: { setMethod: vendorStep, operation: "0x902d", readback: "0xd212" }
"#;

    const ISO_SLAM_MANIFEST: &str = r#"
schema: camera-config/v1
camera:
  manufacturer: FUJIFILM
  model: GFX100 II
  firmware: "2.30"
connections:
  app: { kind: ptpip-app }
operations:
  "0x1002": { name: OpenSession, owner: standard-ptp }
  "0x1015": { name: GetDevicePropValue, owner: standard-ptp }
  "0x1016": { name: SetDevicePropValue, owner: standard-ptp }
properties:
  "0xd02a":
    name: stillIso
    type: u32
    access: readWrite
    descriptor: { form: enum, values: [32769] }
    valueProfiles:
      - connection: app
        mode: shooting/stills
        rows:
          - { label: "80", raw: 80, legal: true }
          - { label: "2000", raw: 2000, legal: true }
          - { label: "50", raw: 50, legal: false, writeStoreRaw: 80 }
"#;

    const SIGNED_PROP_MANIFEST: &str = r#"
schema: camera-config/v1
camera:
  manufacturer: FUJIFILM
  model: GFX100 II
  firmware: "2.30"
properties:
  "0x5010":
    name: exposureBias
    type: i16
    access: readWrite
"#;

    fn op(code: u16, tid: u32, params: Vec<u32>) -> OperationRequest {
        OperationRequest {
            data_phase_info: 1,
            code,
            transaction_id: tid,
            params,
        }
    }

    fn u16_data(v: u16) -> Vec<u8> {
        let mut w = Writer::new();
        PropValue::U16(v).encode(&mut w).unwrap();
        w.into_vec()
    }
    fn u32_data(v: u32) -> Vec<u8> {
        let mut w = Writer::new();
        PropValue::U32(v).encode(&mut w).unwrap();
        w.into_vec()
    }

    fn tmp_card() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("ptpsim-sim-{nanos}"));
        let dir = root.join("DCIM/100_FUJI");
        std::fs::create_dir_all(&dir).unwrap();
        write(&dir.join("DSCF0001.JPG"), b"\xFF\xD8JPEGBODY\xFF\xD9");
        let raf = raf_with_header_preview(&jpeg_with_exif_thumbnail(
            RAF_THUMBNAIL,
            b"FULL-HEADER-PREVIEW",
        ));
        write(&dir.join("DSCF0002.RAF"), &raf);
        root
    }

    fn jpeg_with_exif_thumbnail(thumbnail: &[u8], full_preview_marker: &[u8]) -> Vec<u8> {
        let thumbnail_offset = 44u32;
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II");
        tiff.extend_from_slice(&42u16.to_le_bytes());
        tiff.extend_from_slice(&8u32.to_le_bytes());
        tiff.extend_from_slice(&0u16.to_le_bytes());
        tiff.extend_from_slice(&14u32.to_le_bytes());
        tiff.extend_from_slice(&2u16.to_le_bytes());
        tiff.extend_from_slice(&0x0201u16.to_le_bytes());
        tiff.extend_from_slice(&4u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&thumbnail_offset.to_le_bytes());
        tiff.extend_from_slice(&0x0202u16.to_le_bytes());
        tiff.extend_from_slice(&4u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&(thumbnail.len() as u32).to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(tiff.len(), thumbnail_offset as usize);
        tiff.extend_from_slice(thumbnail);

        let mut app1_data = b"Exif\0\0".to_vec();
        app1_data.extend_from_slice(&tiff);
        let app1_len = (app1_data.len() + 2) as u16;
        let comment_len = (full_preview_marker.len() + 2) as u16;

        let mut jpeg = vec![0xff, 0xd8, 0xff, 0xe1];
        jpeg.extend_from_slice(&app1_len.to_be_bytes());
        jpeg.extend_from_slice(&app1_data);
        jpeg.extend_from_slice(&[0xff, 0xfe]);
        jpeg.extend_from_slice(&comment_len.to_be_bytes());
        jpeg.extend_from_slice(full_preview_marker);
        jpeg.extend_from_slice(&[0xff, 0xd9]);
        jpeg
    }

    fn raf_with_header_preview(preview: &[u8]) -> Vec<u8> {
        let preview_offset = 0x94usize;
        let mut raf = vec![0u8; preview_offset];
        raf[0..b"FUJIFILMCCD-RAW".len()].copy_from_slice(b"FUJIFILMCCD-RAW");
        raf[0x3c..0x40].copy_from_slice(b"0201");
        raf[0x54..0x58].copy_from_slice(&(preview_offset as u32).to_be_bytes());
        raf[0x58..0x5c].copy_from_slice(&(preview.len() as u32).to_be_bytes());
        raf.extend_from_slice(preview);
        raf.extend_from_slice(b"raw sensor data");
        raf
    }

    fn write(p: &Path, b: &[u8]) {
        File::create(p).unwrap().write_all(b).unwrap();
    }

    fn engine(root: &Path) -> Engine {
        let manifest = CameraManifest::from_yaml(MANIFEST).unwrap();
        let mut store = MediaStore::open(root).unwrap();
        store.scan().unwrap();
        Engine::new(manifest, store)
    }

    fn empty_engine(manifest_yaml: &str) -> (Engine, PathBuf) {
        let manifest = CameraManifest::from_yaml(manifest_yaml).unwrap();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("ptpsim-empty-{nanos}"));
        std::fs::create_dir_all(root.join("DCIM/100_FUJI")).unwrap();
        let store = MediaStore::open(&root).unwrap();
        (Engine::new(manifest, store), root)
    }

    fn expect_data(reply: Reply) -> Vec<u8> {
        match reply {
            Reply::Data { data, response } => {
                assert_eq!(response.code, 0x2001, "data reply should carry OK");
                data
            }
            Reply::DataStream {
                source, response, ..
            } => {
                assert_eq!(response.code, 0x2001, "data-stream reply should carry OK");
                source.read().expect("realize stream for assertion")
            }
            other => panic!("expected data reply, got {other:?}"),
        }
    }

    fn expect_ok(reply: Reply) {
        match reply {
            Reply::Response(r) => assert_eq!(r.code, 0x2001, "expected OK response"),
            other => panic!("expected OK response, got {other:?}"),
        }
    }

    /// Gate #3: image-import session completes init -> handles -> thumbnail ->
    /// partial download -> close, driven entirely by the manifest + media store.
    #[test]
    fn gate3_image_import_flow() {
        let root = tmp_card();
        let mut e = engine(&root);

        expect_ok(e.on_operation(&op(0x1002, 1, vec![1]), None)); // OpenSession
        expect_ok(e.on_operation(&op(0x1016, 2, vec![0xdf01]), Some(&u16_data(20)))); // df01=20 -> ImageImport
        assert_eq!(e.state().phase, Phase::ImageImport);

        // Enumerate handles.
        let handles_bytes =
            expect_data(e.on_operation(&op(0x1007, 3, vec![0x00010001, 0, 0]), None));
        let mut r = Reader::new(&handles_bytes);
        let handles = r.ptp_array(|r| r.u32()).unwrap();
        assert_eq!(handles.len(), 2, "two files on the card");

        // ObjectInfo for the first handle has a filename.
        let oi_bytes = expect_data(e.on_operation(&op(0x1008, 4, vec![handles[0]]), None));
        let oi = ptp_core::ObjectInfo::decode(&oi_bytes).unwrap();
        assert!(oi.filename.ends_with(".JPG") || oi.filename.ends_with(".RAF"));

        // Thumbnail of the RAF -> tiny EXIF thumbnail, not the full header preview.
        let raf = *handles
            .iter()
            .find(|h| {
                e.store()
                    .object_info(**h)
                    .unwrap()
                    .filename
                    .ends_with(".RAF")
            })
            .unwrap();
        let thumb = expect_data(e.on_operation(&op(0x100a, 5, vec![raf]), None));
        assert_eq!(thumb, RAF_THUMBNAIL);

        // Partial download of the first 4 bytes of the JPG.
        let jpg = *handles
            .iter()
            .find(|h| {
                e.store()
                    .object_info(**h)
                    .unwrap()
                    .filename
                    .ends_with(".JPG")
            })
            .unwrap();
        let part = expect_data(e.on_operation(&op(0x101b, 6, vec![jpg, 0, 4]), None));
        assert_eq!(part, b"\xFF\xD8JP");

        expect_ok(e.on_operation(&op(0x1003, 7, vec![]), None)); // CloseSession
        assert_eq!(e.state().phase, Phase::Closed);
        std::fs::remove_dir_all(&root).ok();
    }

    /// Gate #4 (engine level): live view opens, frames keep coming, and ISO /
    /// vendor-step / readback all work *while streaming*. The literal
    /// three-socket TCP wiring is exercised at the service level.
    #[test]
    fn gate4_liveview_control_while_streaming() {
        let root = tmp_card();
        let mut e = engine(&root);
        let mut frames = StaticFrameSource::new(vec![0xFF, 0xD8, 0x42, 0xFF, 0xD9]);

        expect_ok(e.on_operation(&op(0x1002, 1, vec![1]), None)); // OpenSession
        expect_ok(e.on_operation(&op(0x1016, 2, vec![0xdf01]), Some(&u16_data(22)))); // df01=22 -> LiveView
        assert_eq!(e.state().phase, Phase::LiveView);
        expect_ok(e.on_operation(&op(0x101c, 3, vec![]), None)); // InitiateOpenCapture
        assert_eq!(e.state().phase, Phase::Streaming);

        // Frames flow.
        assert!(frames.next_frame().is_some());

        // Absolute ISO write while streaming.
        expect_ok(e.on_operation(&op(0x1016, 4, vec![0xd02a]), Some(&u32_data(800))));
        assert_eq!(e.state().props.get(&0xd02a), Some(&PropValue::U32(800)));

        // Vendor step aperture wider (direction=1): 280 -> 400.
        let before = e.state().props.get(&0x5007).cloned();
        assert_eq!(before, Some(PropValue::U16(280)));
        expect_ok(e.on_operation(&op(0x902d, 5, vec![1]), None));
        assert_eq!(e.state().props.get(&0x5007), Some(&PropValue::U16(400)));

        // Readback via GetDevicePropValue reflects the change.
        let v = expect_data(e.on_operation(&op(0x1015, 6, vec![0x5007]), None));
        let mut r = Reader::new(&v);
        assert_eq!(r.u16().unwrap(), 400);

        // Frames still flowing after the control ops.
        assert!(frames.next_frame().is_some());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn state_overlay_seeded_iso_can_diverge_to_manifest_write_store_raw() {
        let (mut e, root) = empty_engine(ISO_SLAM_MANIFEST);
        let overlay: StateOverlay = serde_json::from_value(serde_json::json!({
            "props": { "0xd02a": 2000 }
        }))
        .unwrap();
        let applied = e.apply_state_overlay(&overlay).unwrap();
        assert_eq!(applied.props, 1);
        assert_eq!(e.state().props.get(&0xd02a), Some(&PropValue::U32(2000)));

        expect_ok(e.on_operation(&op(0x1002, 1, vec![1]), None)); // SessionOpen -> shooting/stills.
        expect_ok(e.on_operation(&op(0x1016, 2, vec![0xd02a]), Some(&u32_data(50))));

        let readback = expect_data(e.on_operation(&op(0x1015, 3, vec![0xd02a]), None));
        let mut r = Reader::new(&readback);
        assert_eq!(r.u32().unwrap(), 80);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn invalid_state_overlay_does_not_partially_mutate_state() {
        let (mut e, root) = empty_engine(ISO_SLAM_MANIFEST);
        assert_eq!(e.state().phase, Phase::Disconnected);
        assert_eq!(e.state().props.get(&0xd02a), Some(&PropValue::U32(32769)));

        let overlay: StateOverlay = serde_json::from_value(serde_json::json!({
            "phase": "streaming",
            "session_open": true,
            "props": {
                "0xd02a": 2000,
                "0xffff": 1
            }
        }))
        .unwrap();

        let err = e.apply_state_overlay(&overlay).unwrap_err();
        assert!(
            err.contains("property '0xffff' is not in the loaded manifest"),
            "err: {err}"
        );
        assert_eq!(e.state().phase, Phase::Disconnected);
        assert!(!e.state().session_open);
        assert_eq!(e.state().props.get(&0xd02a), Some(&PropValue::U32(32769)));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn state_overlay_rejects_signed_property_types_explicitly() {
        let (mut e, root) = empty_engine(SIGNED_PROP_MANIFEST);
        let overlay: StateOverlay = serde_json::from_value(serde_json::json!({
            "props": { "0x5010": -333 }
        }))
        .unwrap();

        let err = e.apply_state_overlay(&overlay).unwrap_err();
        assert!(
            err.contains("signed property type 'i16' is not supported"),
            "err: {err}"
        );
        assert!(!e.state().props.contains_key(&0x5010));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn unsupported_op_is_rejected() {
        let root = tmp_card();
        let mut e = engine(&root);
        expect_ok(e.on_operation(&op(0x1002, 1, vec![1]), None));
        match e.on_operation(&op(0x9fff, 2, vec![]), None) {
            Reply::Response(r) => assert_eq!(r.code, 0x2005), // OperationNotSupported
            other => panic!("expected unsupported, got {other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn injected_faults_override_dispatch() {
        let root = tmp_card();
        let mut e = engine(&root);
        expect_ok(e.on_operation(&op(0x1002, 1, vec![1]), None));

        // Fail GetObjectHandles with DeviceBusy.
        e.install_fault(FaultSpec {
            selector: FaultSelector {
                operation: 0x1007,
                params: None,
                skip: 0,
                count: None,
            },
            mutation: FaultMutation::FailResponse { response: 0x2019 },
        })
        .unwrap();
        match e.on_operation(&op(0x1007, 2, vec![]), None) {
            Reply::Response(r) => assert_eq!(r.code, 0x2019),
            other => panic!("expected injected DeviceBusy, got {other:?}"),
        }

        // Close the connection on GetPartialObject.
        e.install_fault(FaultSpec {
            selector: FaultSelector {
                operation: 0x101b,
                params: None,
                skip: 0,
                count: None,
            },
            mutation: FaultMutation::Close {
                stage: FaultStage::Command,
            },
        })
        .unwrap();
        assert_eq!(
            e.on_operation(&op(0x101b, 3, vec![1, 0, 4]), None),
            Reply::Close
        );

        // Clearing faults restores normal behavior.
        e.clear_faults();
        assert!(matches!(
            e.on_operation(&op(0x1007, 4, vec![]), None),
            Reply::Data { .. } | Reply::DataStream { .. }
        ));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn data_faults_mutate_manifest_typed_readback_and_record_wire_plan() {
        let root = tmp_card();
        let mut engine = engine(&root);
        expect_ok(engine.on_operation(&op(0x1002, 1, vec![1]), None));

        engine
            .install_fault(FaultSpec {
                selector: FaultSelector {
                    operation: 0x1015,
                    params: Some(vec![0x5007]),
                    skip: 0,
                    count: Some(1),
                },
                mutation: FaultMutation::ReplaceData {
                    bytes: vec![0xde, 0xad],
                },
            })
            .unwrap();
        match engine.on_operation(&op(0x1015, 2, vec![0x5007]), None) {
            Reply::Data { data, .. } => assert_eq!(data, [0xde, 0xad]),
            other => panic!("expected replaced property data, got {other:?}"),
        }
        assert_eq!(engine.take_applied_fault().unwrap().kind, "replaceData");

        engine.clear_faults();
        engine
            .install_fault(FaultSpec {
                selector: FaultSelector {
                    operation: 0x1015,
                    params: Some(vec![0x5007]),
                    skip: 0,
                    count: Some(1),
                },
                mutation: FaultMutation::PropertyReadback { value: 400 },
            })
            .unwrap();
        match engine.on_operation(&op(0x1015, 3, vec![0x5007]), None) {
            Reply::Data { data, .. } => {
                assert_eq!(Reader::new(&data).u16().unwrap(), 400);
            }
            other => panic!("expected typed property data, got {other:?}"),
        }

        engine.clear_faults();
        engine
            .install_fault(FaultSpec {
                selector: FaultSelector {
                    operation: 0x1015,
                    params: Some(vec![0x5007]),
                    skip: 0,
                    count: Some(1),
                },
                mutation: FaultMutation::DataFraming {
                    framing: camera_config::WireFraming::Standard,
                },
            })
            .unwrap();
        assert!(matches!(
            engine.on_operation(&op(0x1015, 4, vec![0x5007]), None),
            Reply::Data { .. }
        ));
        assert_eq!(
            engine.take_applied_fault().unwrap().wire,
            WirePlan::DataFraming(camera_config::WireFraming::Standard)
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ops_before_session_are_refused() {
        let root = tmp_card();
        let mut e = engine(&root);
        match e.on_operation(&op(0x1007, 1, vec![]), None) {
            Reply::Response(r) => assert_eq!(r.code, 0x2003), // SessionNotOpen
            other => panic!("expected session-not-open, got {other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }
}
