use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Result};
use async_trait::async_trait;
use camera_config::{
    direct_epistemic, no_loss, BundleHeader, CameraContext, CaptureClock, CaptureContext,
    CaptureInterface, CaptureInterfaceType, ClientContext, ClockType, ClockUnit,
    ObservationRecorder, PtpTransport,
};
use camera_initiator::usb::{
    run_usb_inventory, select_usb_interface, validate_usb_artifact_paths, UsbEndpointDescriptor,
    UsbInterfaceDescriptor, UsbInventoryIo, UsbInventoryPlan, UsbInventoryTransport, UsbSelection,
    UsbTransferType,
};
use camera_initiator::{TraceFormat, TraceWriter};
use camera_protocol_ffi::{ConfigStore, KeyValue};
use ptp_core::{DeviceInfo, OperationResponse, PtpIpPacket, Writer};
use serde_json::Value;

const GET_DEVICE_INFO_READS: &str = "            - usbBulkOut: { data: { literal: '0c0000000100011001000000' } }\n            - usbBulkIn: { maxLength: 65536, encoding: bytes, captureAs: deviceInfo }\n            - usbBulkIn: { maxLength: 512, encoding: bytes, captureAs: deviceInfoResponse }";

fn store(ownership: &str, operation: &str, establishment_inventory: bool) -> Arc<ConfigStore> {
    if establishment_inventory {
        store_with_establishment(ownership, operation, GET_DEVICE_INFO_READS, 6)
    } else {
        store_with_establishment(ownership, operation, "", 3)
    }
}

/// `extra` follows the claim and OpenSession steps verbatim.
fn store_with_establishment(
    ownership: &str,
    operation: &str,
    extra: &str,
    step_count: usize,
) -> Arc<ConfigStore> {
    let index = format!(
        r#"
manufacturer: TESTCO
families:
  test:
    usb:
      interfaces:
        stillImage: {{ class: 6, subclass: 1, protocol: 1 }}
      establishments:
        usb-claim-open:
          mechanism: usb-claim-open
          activities:
            - id: camera.test.usb-inventory-open
              version: 1
              displayRole: openingSession
              defaultExpectedDurationMs: 5
              interactionRequired: false
              executorSpan: {{ sequence: steps, startStep: 0, endStepExclusive: {step_count} }}
          steps:
            - usbClaim: {{ interface: stillImage }}
            - usbBulkOut: {{ data: {{ literal: '10000000010002100000000001000000' }} }}
            - usbBulkIn: {{ maxLength: 512, encoding: bytes, captureAs: opened }}
{extra}
models:
  - id: tu1
    displayName: Test USB
    inherits: [test]
    manifest: tu1.yaml
"#
    );
    let body = format!(
        r#"
schema: camera-config/v1
camera: {{ manufacturer: TESTCO, model: TU1 }}
connections:
  usb:
    kind: usb
    platforms: [linux, macos]
    establishment: usb-claim-open
    commandFraming: usb
    session: {{ ownership: {ownership} }}
    events: {{ delivery: reliable }}
    actions:
      readDeviceInfo:
        mode: ''
        initiator:
          steps:
            - {{ sendOp: '{operation}', params: [] }}
"#
    );
    ConfigStore::from_manufacturer_index(
        index,
        vec![KeyValue {
            key: "tu1".into(),
            value: body,
        }],
    )
    .unwrap()
}

fn descriptors() -> Vec<UsbInterfaceDescriptor> {
    vec![UsbInterfaceDescriptor {
        number: 7,
        alternate: 0,
        class: 6,
        subclass: 1,
        protocol: 1,
        endpoints: vec![
            UsbEndpointDescriptor {
                address: 0x86,
                transfer_type: UsbTransferType::Bulk,
                max_packet_size: 512,
            },
            UsbEndpointDescriptor {
                address: 0x04,
                transfer_type: UsbTransferType::Bulk,
                max_packet_size: 512,
            },
            UsbEndpointDescriptor {
                address: 0x89,
                transfer_type: UsbTransferType::Interrupt,
                max_packet_size: 64,
            },
        ],
    }]
}

#[derive(Default, Debug)]
struct Script {
    calls: Vec<String>,
    writes: Vec<Vec<u8>>,
    reads: VecDeque<Vec<u8>>,
    fail_claim: bool,
    short_write: bool,
    pending_read: bool,
}
struct ScriptedIo {
    script: Arc<Mutex<Script>>,
    descriptors: Vec<UsbInterfaceDescriptor>,
}
#[async_trait]
impl UsbInventoryIo for ScriptedIo {
    fn descriptors(&self) -> &[UsbInterfaceDescriptor] {
        &self.descriptors
    }
    async fn claim(&mut self, selection: &UsbSelection) -> Result<()> {
        let mut script = self.script.lock().unwrap();
        script.calls.push(format!("claim:{}", selection.interface));
        if script.fail_claim {
            bail!("interface belongs to another owner")
        }
        Ok(())
    }
    async fn write(&mut self, endpoint: u8, bytes: &[u8]) -> Result<usize> {
        assert_eq!(endpoint, 0x04);
        let mut script = self.script.lock().unwrap();
        let PtpIpPacket::OperationRequest(command) =
            protocol_primitives::usb_ptp::decode(bytes).unwrap()
        else {
            panic!()
        };
        script.calls.push(format!(
            "out:{:04x}:{}",
            command.code, command.transaction_id
        ));
        script.writes.push(bytes.to_vec());
        Ok(bytes.len() - usize::from(script.short_write))
    }
    async fn read(&mut self, endpoint: u8, max_length: usize) -> Result<Vec<u8>> {
        assert_eq!(endpoint, 0x86);
        assert!(max_length <= 65536);
        let pending = {
            let mut script = self.script.lock().unwrap();
            script.calls.push(format!("in:{max_length}"));
            script.pending_read
        };
        if pending {
            return std::future::pending().await;
        }
        let mut script = self.script.lock().unwrap();
        let bytes = script.reads.pop_front().expect("script has a USB read");
        if bytes.len() > max_length {
            script.reads.push_front(bytes[max_length..].to_vec());
        }
        Ok(bytes[..bytes.len().min(max_length)].to_vec())
    }
    async fn release(&mut self) -> Result<()> {
        self.script.lock().unwrap().calls.push("release".into());
        Ok(())
    }
}

struct FinalizationFailureIo {
    inner: ScriptedIo,
    observation: PathBuf,
    fail_release: bool,
    block_payload: Option<PathBuf>,
}

#[async_trait]
impl UsbInventoryIo for FinalizationFailureIo {
    fn descriptors(&self) -> &[UsbInterfaceDescriptor] {
        self.inner.descriptors()
    }
    async fn claim(&mut self, selection: &UsbSelection) -> Result<()> {
        self.inner.claim(selection).await
    }
    async fn write(&mut self, endpoint: u8, bytes: &[u8]) -> Result<usize> {
        self.inner.write(endpoint, bytes).await
    }
    async fn read(&mut self, endpoint: u8, max_length: usize) -> Result<Vec<u8>> {
        if let Some(path) = self.block_payload.take() {
            std::fs::create_dir(path).unwrap();
        }
        self.inner.read(endpoint, max_length).await
    }
    async fn release(&mut self) -> Result<()> {
        self.inner.release().await?;
        // Replace the observation destination only after inventory and retention finish.
        std::fs::rename(
            &self.observation,
            self.observation.with_extension("retained"),
        )
        .unwrap();
        std::fs::create_dir(&self.observation).unwrap();
        if self.fail_release {
            bail!("injected USB release failure")
        }
        Ok(())
    }
}

/// A directory that belongs to one test. `tempfile` creates it exclusively
/// under a random name and removes it when the guard drops.
struct TempDir(tempfile::TempDir);
impl TempDir {
    fn new() -> Self {
        Self(
            tempfile::Builder::new()
                .prefix("ptpsim-usb-")
                .tempdir()
                .unwrap(),
        )
    }
    fn path(&self) -> &Path {
        self.0.path()
    }
    fn output(&self) -> PathBuf {
        self.path().join("inventory")
    }
}

fn trace(directory: &TempDir) -> (Arc<TraceWriter>, ObservationRecorder) {
    let header = BundleHeader {
        schema: camera_config::OBSERVATION_SCHEMA_VERSION.into(),
        run_id: "usb-test".into(),
        record_id: "header".into(),
        ordinal: 0,
        camera: CameraContext {
            manufacturer: "TESTCO".into(),
            model: "TU1".into(),
            body_id: "body".into(),
            firmware: "unknown".into(),
        },
        client: ClientContext {
            artifact: "camera-initiator-test".into(),
            version: "1".into(),
            platform: "test".into(),
        },
        capture: CaptureContext {
            interfaces: vec![CaptureInterface {
                id: "usb".into(),
                interface_type: CaptureInterfaceType::Usb,
                role: "initiator".into(),
            }],
            clocks: vec![CaptureClock {
                id: "process-monotonic".into(),
                clock_type: ClockType::Monotonic,
                unit: ClockUnit::Milliseconds,
            }],
            clock_mappings: vec![],
            loss: no_loss(),
            redactions: vec![],
            tool_versions: BTreeMap::from([("camera-initiator-test".into(), "1".into())]),
            artifacts: vec![],
        },
        epistemic: direct_epistemic(),
    };
    let recorder =
        ObservationRecorder::open(Some(directory.path().join("observation.jsonl")), header)
            .unwrap();
    (
        Arc::new(
            TraceWriter::with_observations(
                TraceFormat::Jsonl,
                Box::new(Vec::<u8>::new()),
                recorder.clone(),
                "usb".into(),
                "inventory".into(),
            )
            .with_ptp_transport(PtpTransport::Usb),
        ),
        recorder,
    )
}
fn response(code: u16, tid: u32) -> Vec<u8> {
    protocol_primitives::usb_ptp::encode(&PtpIpPacket::OperationResponse(OperationResponse {
        code,
        transaction_id: tid,
        params: vec![],
    }))
    .unwrap()
}
fn payload() -> Vec<u8> {
    let info = DeviceInfo {
        standard_version: 100,
        vendor_extension_id: 42,
        vendor_extension_version: 3,
        vendor_extension_desc: "test extension".into(),
        functional_mode: 0,
        operations_supported: vec![0x1001, 0x1002, 0x9001, 0xffff],
        events_supported: vec![0x4001, 0xc001],
        device_properties_supported: vec![0x5001, 0xd001],
        capture_formats: vec![0x3801],
        image_formats: vec![0xb103],
        manufacturer: "Test".into(),
        model: "TU1".into(),
        device_version: "1.2".into(),
        serial_number: "test-body".into(),
    };
    let mut writer = Writer::new();
    info.encode(&mut writer).unwrap();
    writer.into_vec()
}
fn data(tid: u32, payload: &[u8]) -> Vec<u8> {
    protocol_primitives::usb_ptp::encode_data(0x1001, tid, payload)
}
fn transport(
    plan: &UsbInventoryPlan,
    script: Arc<Mutex<Script>>,
    trace: Arc<TraceWriter>,
    timeout: Duration,
) -> Arc<UsbInventoryTransport> {
    UsbInventoryTransport::new(
        Box::new(ScriptedIo {
            script,
            descriptors: descriptors(),
        }),
        plan,
        timeout,
        65536,
        trace,
    )
    .unwrap()
}

#[test]
fn descriptor_selection_is_explicit_and_unambiguous() {
    let all = descriptors();
    let selected = select_usb_interface(&all, (6, 1, 1)).unwrap();
    assert_eq!(
        selected,
        UsbSelection {
            interface: 7,
            bulk_in: 0x86,
            bulk_out: 0x04,
            interrupt_in: 0x89
        }
    );
    assert!(select_usb_interface(&all, (6, 1, 2)).is_err());
    let mut duplicate = all.clone();
    duplicate.push(all[0].clone());
    assert!(select_usb_interface(&duplicate, (6, 1, 1)).is_err());
    for endpoint in 0..3 {
        let mut missing = all.clone();
        missing[0].endpoints.remove(endpoint);
        assert!(select_usb_interface(&missing, (6, 1, 1)).is_err());
    }
    let mut overlapping = all.clone();
    overlapping[0].endpoints[2].address = overlapping[0].endpoints[0].address;
    assert!(select_usb_interface(&overlapping, (6, 1, 1)).is_err());
    let mut alternate = all;
    alternate[0].alternate = 1;
    assert!(select_usb_interface(&alternate, (6, 1, 1)).is_err());
}

#[tokio::test]
async fn inventory_reuses_establishment_action_pipeline_and_usb_provenance() {
    let directory = TempDir::new();
    let (trace, recorder) = trace(&directory);
    let store = store("initiatorOwned", "0x1001", true);
    let plan = UsbInventoryPlan::prepare(&store, "tu1", "usb", 65536).unwrap();
    let payload = payload();
    let open = response(0x2001, 0);
    let mut joined = data(2, &payload);
    joined.extend(response(0x2001, 2));
    let script = Arc::new(Mutex::new(Script {
        reads: VecDeque::from([
            open[..3].to_vec(),
            open[3..].to_vec(),
            data(1, &payload),
            response(0x2001, 1),
            joined,
            response(0x2001, 3),
        ]),
        ..Script::default()
    }));
    let adapter = transport(&plan, script.clone(), trace.clone(), Duration::from_secs(1));
    let report = run_usb_inventory(plan, store, adapter, trace, &directory.output())
        .await
        .unwrap();
    assert_eq!(
        report.operations_supported,
        vec![0x1001, 0x1002, 0x9001, 0xffff]
    );
    assert_eq!(report.events_supported, vec![0x4001, 0xc001]);
    assert_eq!(report.device_properties_supported, vec![0x5001, 0xd001]);
    assert_eq!(report.capture_formats, vec![0x3801]);
    assert_eq!(report.image_formats, vec![0xb103]);
    assert_eq!(
        std::fs::read(directory.output().join("device-info.bin")).unwrap(),
        payload
    );
    assert_eq!(
        std::fs::read(directory.output().join("establishment-device-info-1.bin")).unwrap(),
        payload
    );
    assert!(directory.output().join("report.json").is_file());
    let script = script.lock().unwrap();
    assert_eq!(script.calls.first().unwrap(), "claim:7");
    assert_eq!(script.calls.last().unwrap(), "release");
    let writes = script
        .calls
        .iter()
        .filter(|call| call.starts_with("out:"))
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(
        writes,
        ["out:1002:0", "out:1001:1", "out:1001:2", "out:1003:3"]
    );
    let export: Value = serde_json::from_str(&recorder.export_json(0).unwrap()).unwrap();
    let transactions = export["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|record| record["kind"] == "ptpTransaction")
        .collect::<Vec<_>>();
    assert_eq!(transactions.len(), 4);
    assert!(transactions
        .iter()
        .all(|record| record["transport"] == "usb" && record["request"]["framing"] == "usb"));
    assert!(transactions
        .iter()
        .all(|record| record["context"]["mode"] == "inventory"));
    assert_eq!(transactions[2]["request"]["operation"], "0x1001");
    assert_eq!(
        transactions[2]["response"]["data"]["payload"]["length"],
        payload.len()
    );
    // Export parses through the canonical schema, including the USB header and record provenance.
    let retained = std::fs::read_to_string(directory.path().join("observation.jsonl")).unwrap();
    camera_config::validate_bundles(&[&retained]).unwrap();
    assert!(
        retained.contains("\"type\":\"usb\"") || retained.contains("\"interfaceType\":\"usb\"")
    );
}

#[tokio::test]
async fn malformed_inventory_is_retained_then_closed_and_released() {
    let directory = TempDir::new();
    let (trace, _) = trace(&directory);
    let store = store("initiatorOwned", "0x1001", false);
    let plan = UsbInventoryPlan::prepare(&store, "tu1", "usb", 65536).unwrap();
    let script = Arc::new(Mutex::new(Script {
        reads: VecDeque::from([
            response(0x2001, 0),
            data(1, &[1, 2, 3]),
            response(0x2001, 1),
            response(0x2001, 2),
        ]),
        ..Script::default()
    }));
    let adapter = transport(&plan, script.clone(), trace.clone(), Duration::from_secs(1));
    let error = run_usb_inventory(plan, store, adapter, trace, &directory.output())
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("decode complete USB DeviceInfo"));
    assert_eq!(
        std::fs::read(directory.output().join("device-info.bin")).unwrap(),
        [1, 2, 3]
    );
    let script = script.lock().unwrap();
    assert!(script.calls.contains(&"out:1003:2".into()));
    assert_eq!(script.calls.last().unwrap(), "release");
}

#[tokio::test]
async fn failed_claim_never_writes_or_closes_another_owners_session() {
    let directory = TempDir::new();
    let (trace, _) = trace(&directory);
    let store = store("initiatorOwned", "0x1001", false);
    let plan = UsbInventoryPlan::prepare(&store, "tu1", "usb", 65536).unwrap();
    let script = Arc::new(Mutex::new(Script {
        fail_claim: true,
        ..Script::default()
    }));
    let adapter = transport(&plan, script.clone(), trace.clone(), Duration::from_secs(1));
    assert!(
        run_usb_inventory(plan, store, adapter, trace, &directory.output())
            .await
            .is_err()
    );
    let script = script.lock().unwrap();
    assert!(script.writes.is_empty());
    assert_eq!(script.calls, ["claim:7", "release"]);
}

#[tokio::test]
async fn already_open_session_does_not_acquire_or_close_daemon_ownership() {
    let directory = TempDir::new();
    let (trace, _) = trace(&directory);
    let store = store("initiatorOwned", "0x1001", false);
    let plan = UsbInventoryPlan::prepare(&store, "tu1", "usb", 65536).unwrap();
    let script = Arc::new(Mutex::new(Script {
        reads: VecDeque::from([response(0x201e, 0)]),
        ..Script::default()
    }));
    let adapter = transport(&plan, script.clone(), trace.clone(), Duration::from_secs(1));
    assert!(
        run_usb_inventory(plan, store, adapter, trace, &directory.output())
            .await
            .is_err()
    );
    let script = script.lock().unwrap();
    assert_eq!(script.writes.len(), 1);
    assert_eq!(script.calls.last().unwrap(), "release");
}

#[tokio::test]
async fn bounded_read_and_short_write_failures_release_the_interface() {
    for mode in ["oversized", "timeout", "short-write", "wrong-transaction"] {
        let directory = TempDir::new();
        let (trace, _) = trace(&directory);
        let store = store("initiatorOwned", "0x1001", false);
        let plan = UsbInventoryPlan::prepare(&store, "tu1", "usb", 65536).unwrap();
        let mut oversized = vec![0; 12];
        oversized[..4].copy_from_slice(&65537u32.to_le_bytes());
        let reads = if mode == "wrong-transaction" {
            VecDeque::from([response(0x2001, 77)])
        } else {
            VecDeque::from([oversized])
        };
        let script = Arc::new(Mutex::new(Script {
            reads,
            pending_read: mode == "timeout",
            short_write: mode == "short-write",
            ..Script::default()
        }));
        let adapter = transport(
            &plan,
            script.clone(),
            trace.clone(),
            Duration::from_millis(10),
        );
        assert!(
            run_usb_inventory(plan, store, adapter, trace, &directory.output())
                .await
                .is_err(),
            "{mode}"
        );
        let script = script.lock().unwrap();
        assert_eq!(script.writes.len(), 1, "{mode}");
        assert_eq!(script.calls.last().unwrap(), "release", "{mode}");
    }
}

#[test]
fn preflight_rejects_incompatible_scope_before_io() {
    let unsafe_action = store("initiatorOwned", "0x100e", false);
    assert!(UsbInventoryPlan::prepare(&unsafe_action, "tu1", "usb", 65536).is_err());
    let valid = store("initiatorOwned", "0x1001", false);
    assert!(UsbInventoryPlan::prepare(&valid, "tu1", "missing", 65536).is_err());
    assert!(UsbInventoryPlan::prepare(&valid, "tu1", "usb", 0).is_err());
    // Change the connection through the schema's daemon-owned kind, before opening any device.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/camera-config-data/fuji");
    let shipping = camera_initiator::usb::load_usb_store(
        &root.join("gfx100ii/gfx100ii.yaml"),
        &root.join("index.yaml"),
        "gfx100ii",
        Some(&root.join("fuji.yaml")),
        &[],
    )
    .unwrap();
    assert!(UsbInventoryPlan::prepare(&shipping, "gfx100ii", "usb-passthrough", 65536).is_err());
    UsbInventoryPlan::prepare(&shipping, "gfx100ii", "usb", 65536).unwrap();
}

#[test]
fn preflight_rejects_get_device_info_without_its_response_read() {
    // Each USB phase is one bulk IN read. Stopping after the data container
    // would leave the response queued for the next transaction.
    let data_only = store_with_establishment(
        "initiatorOwned",
        "0x1001",
        "            - usbBulkOut: { data: { literal: '0c0000000100011001000000' } }\n            - usbBulkIn: { maxLength: 65536, encoding: bytes, captureAs: deviceInfo }",
        5,
    );
    let error = UsbInventoryPlan::prepare(&data_only, "tu1", "usb", 65536)
        .err()
        .expect("a GetDeviceInfo without its response read is rejected");
    assert!(
        format!("{error:#}").contains("read every phase"),
        "{error:#}"
    );
    UsbInventoryPlan::prepare(
        &store("initiatorOwned", "0x1001", true),
        "tu1",
        "usb",
        65536,
    )
    .unwrap();
}

#[tokio::test]
async fn establishment_reads_one_container_per_bulk_in_when_phases_share_a_transfer() {
    let directory = TempDir::new();
    let (trace, _) = trace(&directory);
    let store = store("initiatorOwned", "0x1001", true);
    let plan = UsbInventoryPlan::prepare(&store, "tu1", "usb", 65536).unwrap();
    let payload = payload();
    let mut establishment = data(1, &payload);
    establishment.extend(response(0x2001, 1));
    let mut action = data(2, &payload);
    action.extend(response(0x2001, 2));
    let script = Arc::new(Mutex::new(Script {
        reads: VecDeque::from([
            response(0x2001, 0),
            establishment,
            action,
            response(0x2001, 3),
        ]),
        ..Script::default()
    }));
    let adapter = transport(&plan, script.clone(), trace.clone(), Duration::from_secs(1));
    run_usb_inventory(plan, store, adapter, trace, &directory.output())
        .await
        .unwrap();
    let script = script.lock().unwrap();
    let calls = script
        .calls
        .iter()
        .filter(|call| call.starts_with("out:") || call.starts_with("in:"))
        .map(String::as_str)
        .collect::<Vec<_>>();
    // The establishment's response read is served from the transfer that also
    // carried the data container, without another bulk IN request.
    assert_eq!(
        &calls[..5],
        [
            "out:1002:0",
            "in:512",
            "out:1001:1",
            "in:65536",
            "out:1001:2"
        ]
    );
}

#[tokio::test]
async fn establishment_ending_inside_a_container_fails_before_the_next_command() {
    let directory = TempDir::new();
    let (trace, _) = trace(&directory);
    // The OpenSession response arrives with the start of another container.
    // The transport reports the unread bytes instead of reading on the plan's behalf.
    let store = store("initiatorOwned", "0x1001", false);
    let plan = UsbInventoryPlan::prepare(&store, "tu1", "usb", 65536).unwrap();
    let mut joined = response(0x2001, 0);
    joined.extend(&response(0x2001, 1)[..3]);
    let script = Arc::new(Mutex::new(Script {
        reads: VecDeque::from([joined]),
        ..Script::default()
    }));
    let adapter = transport(&plan, script.clone(), trace.clone(), Duration::from_secs(1));
    let error = run_usb_inventory(plan, store, adapter, trace, &directory.output())
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("ended before reading every container"),
        "{error:#}"
    );
    let script = script.lock().unwrap();
    assert_eq!(
        script.writes.len(),
        1,
        "no command follows the unsynchronized plan"
    );
    assert_eq!(script.calls.last().unwrap(), "release");
}

#[test]
fn artifact_preflight_rejects_overwrite_and_input_aliases() {
    let directory = TempDir::new();
    let input = directory.path().join("body.yaml");
    std::fs::write(&input, "manifest").unwrap();
    let observation = directory.path().join("observation.jsonl");
    assert!(validate_usb_artifact_paths(
        std::slice::from_ref(&input),
        &directory.output(),
        "-",
        &observation
    )
    .is_ok());
    assert!(validate_usb_artifact_paths(
        std::slice::from_ref(&input),
        &directory.output(),
        input.to_str().unwrap(),
        &observation
    )
    .is_err());
    assert!(validate_usb_artifact_paths(
        &[input],
        &directory.output(),
        observation.to_str().unwrap(),
        &observation
    )
    .is_err());
    assert!(!directory.output().exists());
}

#[test]
fn cli_rejects_daemon_owned_selection_before_creating_artifacts() {
    let directory = TempDir::new();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/camera-config-data/fuji");
    let trace = directory.path().join("trace.jsonl");
    let observation = directory.path().join("observation.jsonl");
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_camera-initiator"))
        .args(["--manifest"])
        .arg(root.join("gfx100ii/gfx100ii.yaml"))
        .arg("--manufacturer")
        .arg(root.join("fuji.yaml"))
        .arg("--manufacturer-index")
        .arg(root.join("index.yaml"))
        .args([
            "--model",
            "gfx100ii",
            "--connection",
            "usb-passthrough",
            "--usb-bus",
            "unavailable",
            "--usb-address",
            "1",
        ])
        .arg("--trace")
        .arg(&trace)
        .arg("--observation")
        .arg(&observation)
        .arg("usb-inventory")
        .arg("--output-dir")
        .arg(directory.output())
        .output()
        .unwrap();
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("initiatorOwned") || stderr.contains("unavailable on this platform"),
        "{stderr}"
    );
    assert!(!trace.exists());
    assert!(!observation.exists());
    assert!(!directory.output().exists());
}

#[tokio::test]
async fn invalid_native_location_is_rejected_before_usb_enumeration() {
    assert!(camera_initiator::usb::NativeUsbIo::open_selected("", 1)
        .await
        .is_err());
    assert!(
        camera_initiator::usb::NativeUsbIo::open_selected("unavailable", 0)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn non_ok_inventory_response_preserves_primary_failure_and_closes_owned_session() {
    let directory = TempDir::new();
    let (trace, _) = trace(&directory);
    let store = store("initiatorOwned", "0x1001", false);
    let plan = UsbInventoryPlan::prepare(&store, "tu1", "usb", 65536).unwrap();
    let script = Arc::new(Mutex::new(Script {
        reads: VecDeque::from([
            response(0x2001, 0),
            data(1, &payload()),
            response(0x2002, 1),
            response(0x2001, 2),
        ]),
        ..Script::default()
    }));
    let adapter = transport(&plan, script.clone(), trace.clone(), Duration::from_secs(1));
    let error = run_usb_inventory(plan, store, adapter, trace, &directory.output())
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("2002"), "{error:#}");
    assert_eq!(
        std::fs::read(directory.output().join("device-info.bin")).unwrap(),
        payload()
    );
    let script = script.lock().unwrap();
    assert!(script.calls.contains(&"out:1003:2".into()));
    assert_eq!(script.calls.last().unwrap(), "release");
}

#[tokio::test]
async fn final_observation_failure_preserves_inventory_retention_and_cleanup_errors() {
    for failure in ["claim", "retention", "none"] {
        let directory = TempDir::new();
        let (trace, _) = trace(&directory);
        let store = store("initiatorOwned", "0x1001", false);
        let plan = UsbInventoryPlan::prepare(&store, "tu1", "usb", 65536).unwrap();
        let script = Arc::new(Mutex::new(Script {
            fail_claim: failure == "claim",
            reads: VecDeque::from([
                response(0x2001, 0),
                data(1, &payload()),
                response(0x2001, 1),
                response(0x2001, 2),
            ]),
            ..Script::default()
        }));
        let adapter = UsbInventoryTransport::new(
            Box::new(FinalizationFailureIo {
                inner: ScriptedIo {
                    script: script.clone(),
                    descriptors: descriptors(),
                },
                observation: directory.path().join("observation.jsonl"),
                fail_release: failure != "none",
                block_payload: (failure == "retention")
                    .then(|| directory.output().join("device-info.bin")),
            }),
            &plan,
            Duration::from_secs(1),
            65536,
            trace.clone(),
        )
        .unwrap();
        let error = run_usb_inventory(plan, store, adapter, trace, &directory.output())
            .await
            .unwrap_err();
        let detail = format!("{error:#}");
        assert!(
            detail.contains("observation recorder I/O failed"),
            "{detail}"
        );
        match failure {
            "claim" => {
                assert!(
                    detail.contains("interface belongs to another owner"),
                    "{detail}"
                );
                assert!(detail.contains("injected USB release failure"), "{detail}");
                assert!(script.lock().unwrap().writes.is_empty());
            }
            "retention" => {
                assert!(detail.contains("retain DeviceInfo payload"), "{detail}");
                assert!(
                    detail.contains("retain received USB DeviceInfo payload"),
                    "{detail}"
                );
                assert!(detail.contains("injected USB release failure"), "{detail}");
            }
            "none" => {
                assert_eq!(
                    std::fs::read(directory.output().join("device-info.bin")).unwrap(),
                    payload()
                );
                assert!(!detail.contains("USB cleanup"), "{detail}");
            }
            _ => unreachable!(),
        }
        assert_eq!(script.lock().unwrap().calls.last().unwrap(), "release");
        assert!(!directory.output().join("report.json").exists());
    }
}
