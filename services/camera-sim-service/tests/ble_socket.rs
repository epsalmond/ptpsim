//! BLE socket listener (`ptpsim-ble/1`, docs/TRANSPORTS.md "BLE socket"): the
//! golden transcript, the scenario advert against the manifest signature, the
//! manifest pair plan and AP handoff walked over TCP, the cross-transport link
//! gates, the per-accept responder rebuild, and the single-client guard.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use camera_config::index::eval::{self, BleAdvertFacts};
use camera_config::index::{
    CccdMode, Encoding, FamilyBleBlock, ModelView, ResolvedManufacturerIndex, Signature, Step,
    StepValue,
};
use camera_sim::{BleEvent, BleResponder, SharedLink};
use camera_sim_service::ble_socket::{BleAdvert, BleScenario, BleSocket, SimulatedPeripheral};
use camera_sim_service::{Config, Server};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};

const IO_TIMEOUT: Duration = Duration::from_secs(5);
const PERIPHERAL: &str = "6F1C2A4E-3B7D-4C59-8E21-0A9B4D7C3E15";
const TERMINAL_NAME: &str = "iphone";

fn repo(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

fn scenario_path() -> PathBuf {
    repo("packages/fixtures/ble/gfx100ii-pair-handoff.yaml")
}

fn gfx100ii() -> ModelView {
    let text = std::fs::read_to_string(repo("packages/camera-config-data/fuji/index.yaml"))
        .expect("read fuji index");
    ResolvedManufacturerIndex::from_yaml(&text)
        .expect("fuji index loads")
        .models
        .into_iter()
        .find(|m| m.id == "gfx100ii")
        .expect("gfx100ii present")
}

fn uuid(ble: &FamilyBleBlock, name: &str) -> String {
    ble.gatt
        .get(name)
        .unwrap_or_else(|| panic!("gatt name {name} in catalog"))
        .clone()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
        .collect()
}

// ---------------------------------------------------------------------------
// Golden transcript
// ---------------------------------------------------------------------------

const FFF1: &str = "0000FFF1-0000-1000-8000-00805F9B34FB";
const FFF2: &str = "0000FFF2-0000-1000-8000-00805F9B34FB";
const FFF3: &str = "0000FFF3-0000-1000-8000-00805F9B34FB";

/// The synthetic scenario the transcript was written against: 0xFFFF is the
/// Bluetooth SIG company ID reserved for testing.
fn golden_socket() -> BleSocket {
    let peripheral = SimulatedPeripheral {
        id: "6E2B0C3A-5D7F-4E21-9A8B-1C2D3E4F5A6B".into(),
        advert: BleAdvert {
            service_uuids: vec!["0000FFF0-0000-1000-8000-00805F9B34FB".into()],
            manufacturer_data: vec![0xff, 0xff, 0x01, 0x02],
        },
    };
    BleSocket::new(BleScenario::new(vec![peripheral], 247, None, || {
        BleResponder::new([FFF1, FFF2, FFF3].map(String::from))
            .serve_read(FFF1, &[0x0a, 0x0b])
            .queue_notification(FFF2, &[0xee])
            .queue_notification_after_fenced_write(FFF2, FFF3, 1, &[0x02, 0x80])
            .queue_notification_after_fenced_write(FFF2, FFF3, 1, &[0x01, 0x80])
    }))
}

#[tokio::test]
async fn golden_transcript_replays_through_the_connection_handler() {
    let transcript = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/ble-socket-v1.transcript.ndjson"),
    )
    .expect("read transcript");
    let socket = golden_socket();
    let (client, server) = tokio::io::duplex(64 * 1024);
    let serve = tokio::spawn({
        let socket = socket.clone();
        async move { socket.serve(server).await }
    });
    let (read, mut write) = tokio::io::split(client);
    let mut lines = BufReader::new(read).lines();

    for (n, line) in transcript.lines().enumerate() {
        let entry: Value = serde_json::from_str(line).expect("transcript line is JSON");
        let msg = &entry["msg"];
        match entry["dir"].as_str() {
            Some("c2s") => {
                let mut bytes = serde_json::to_vec(msg).unwrap();
                bytes.push(b'\n');
                write.write_all(&bytes).await.unwrap();
            }
            Some("s2c") => {
                let got = timeout(IO_TIMEOUT, lines.next_line())
                    .await
                    .unwrap_or_else(|_| panic!("line {}: no server message", n + 1))
                    .unwrap()
                    .unwrap_or_else(|| panic!("line {}: server closed early", n + 1));
                let got: Value = serde_json::from_str(&got).expect("server line is JSON");
                assert_eq!(&got, msg, "transcript line {}", n + 1);
            }
            other => panic!("line {}: bad dir {other:?}", n + 1),
        }
    }

    // Nothing follows the last scripted message: the server only answers EOF
    // by ending the connection.
    write.shutdown().await.unwrap();
    let tail = timeout(IO_TIMEOUT, lines.next_line())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tail, None, "no message after the transcript");
    timeout(IO_TIMEOUT, serve).await.unwrap().unwrap().unwrap();

    assert_eq!(
        socket.log(),
        vec![
            BleEvent::Connect,
            BleEvent::DiscoverServices,
            BleEvent::Read { uuid: FFF1.into() },
            BleEvent::Subscribe {
                uuid: FFF2.into(),
                mode: CccdMode::Notify
            },
            BleEvent::Write {
                uuid: FFF3.into(),
                value: vec![0x01]
            },
            BleEvent::NotificationFence { uuid: FFF2.into() },
            BleEvent::Write {
                uuid: FFF3.into(),
                value: vec![0xaa, 0x55]
            },
        ],
        "the finished responder's log moves to the service history"
    );
}

// ---------------------------------------------------------------------------
// Scenario advert against the manifest
// ---------------------------------------------------------------------------

#[test]
fn scenario_advert_is_the_first_manifest_signature_match() {
    let raw: serde_yaml::Value =
        serde_yaml::from_str(&std::fs::read_to_string(scenario_path()).unwrap()).unwrap();
    assert_eq!(raw["model"].as_str(), Some("gfx100ii"));
    assert_eq!(
        raw["index"].as_str(),
        Some("../../camera-config-data/fuji/index.yaml")
    );
    let view = gfx100ii();
    let ble = view.ble.as_ref().expect("ble block");

    let scenario = BleScenario::load(&scenario_path(), SharedLink::default()).expect("loads");
    assert_eq!(scenario.peripherals().len(), 1);
    let advert = &scenario.peripherals()[0].advert;
    let (company, payload) = advert.manufacturer_data.split_at(2);
    let facts = BleAdvertFacts {
        service_uuids: advert.service_uuids.clone(),
        manufacturer_data: Some((
            u16::from_le_bytes([company[0], company[1]]),
            payload.to_vec(),
        )),
        ..Default::default()
    };
    assert_eq!(
        facts.manufacturer_data.as_ref().map(|(id, _)| *id),
        ble.advert.manufacturer_company_id,
        "advert company ID is the manifest constant"
    );

    let first = view
        .signatures
        .iter()
        .find_map(|(name, sig)| match sig {
            Signature::BleAdvert(sig) if sig.discoverable && eval::advert_matches(sig, &facts) => {
                Some((name.as_str(), sig))
            }
            _ => None,
        })
        .expect("a discoverable signature matches the scenario advert");
    assert_eq!(first.0, "bleLegacyAdvert");
    let scope: BTreeMap<_, _> = eval::advert_scope(first.1, &facts).into_iter().collect();
    assert_eq!(scope.get("style").map(String::as_str), Some("legacy"));
}

#[test]
fn scenario_with_an_unknown_gatt_name_fails_to_load() {
    let dir = tempfile::tempdir().unwrap();
    let index = repo("packages/camera-config-data/fuji/index.yaml");
    let path = dir.path().join("bad.yaml");
    std::fs::write(
        &path,
        format!(
            "schema: ptpsim-ble-scenario/v1\nprovenance: test-scripted\nindex: {}\nmodel: gfx100ii\nmtu: 247\nperipherals: []\nreads:\n  notACharacteristic: {{ hex: \"00\" }}\n",
            index.display()
        ),
    )
    .unwrap();
    let error = BleScenario::load(&path, SharedLink::default())
        .err()
        .expect("unknown name fails");
    assert!(
        error.to_string().contains("notACharacteristic"),
        "error names the unknown GATT name: {error}"
    );
}

// ---------------------------------------------------------------------------
// Server over TCP
// ---------------------------------------------------------------------------

struct Running {
    ble_addr: SocketAddr,
    link: SharedLink,
    socket: BleSocket,
    shutdown: tokio::sync::oneshot::Sender<()>,
    run: tokio::task::JoinHandle<()>,
    _card: tempfile::TempDir,
}

impl Running {
    async fn stop(self) {
        let _ = self.shutdown.send(());
        timeout(IO_TIMEOUT, self.run).await.unwrap().unwrap();
    }
}

async fn start_server() -> Running {
    let card = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(card.path().join("DCIM/100_FUJI")).unwrap();
    let manifest_yaml = std::fs::read_to_string(repo(
        "packages/camera-config-data/fuji/gfx100ii/gfx100ii.consolidated.yaml",
    ))
    .unwrap();
    let loopback: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let config = Config {
        instance_id: "ble-socket-test".into(),
        profile: "fuji/gfx100ii".into(),
        connection: "app".into(),
        manifest_yaml,
        media_root: card.path().to_path_buf(),
        command_bind: Some(loopback),
        liveview_bind: Some(loopback),
        event_bind: Some(loopback),
        knock_bind: None,
        pcss_init_fails: 0,
        pcss_shutter_enqueue_count: 0,
        control_bind: loopback,
        liveview_dir: None,
        state_callback: None,
    };
    let mut server = Server::bind(config).await.unwrap();
    assert!(
        server.ble_addr_opt().is_none(),
        "no BLE listener by default"
    );
    server
        .enable_ble_socket(loopback, &scenario_path(), false)
        .await
        .expect("BLE socket binds with the committed scenario");
    let ble_addr = server.ble_addr_opt().expect("BLE listener bound");
    let socket = server.ble_socket().expect("BLE socket handle");
    let link = server.camera_link().await;
    let (shutdown, rx) = tokio::sync::oneshot::channel();
    let run = tokio::spawn(server.run(rx));
    Running {
        ble_addr,
        link,
        socket,
        shutdown,
        run,
        _card: card,
    }
}

/// A minimal `ptpsim-ble/1` client. Every received line is kept in order in
/// `received`; events that follow a response are read while waiting for the
/// next response, so a trailing `scan_stop` flushes them.
struct Client {
    lines: tokio::io::Lines<BufReader<OwnedReadHalf>>,
    write: OwnedWriteHalf,
    next_id: u64,
    received: Vec<Value>,
}

impl Client {
    async fn connect(addr: SocketAddr) -> Self {
        let (read, write) = TcpStream::connect(addr).await.unwrap().into_split();
        let mut client = Client {
            lines: BufReader::new(read).lines(),
            write,
            next_id: 1,
            received: Vec::new(),
        };
        let hello = client.next_message().await.expect("hello");
        assert_eq!(
            hello,
            json!({"event": "hello", "protocol": "ptpsim-ble", "version": 1})
        );
        client
    }

    async fn next_message(&mut self) -> Option<Value> {
        let line = timeout(IO_TIMEOUT, self.lines.next_line())
            .await
            .expect("server message within the timeout")
            .unwrap()?;
        Some(serde_json::from_str(&line).expect("server line is JSON"))
    }

    async fn call(&mut self, op: &str, args: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let mut request = json!({"id": id, "op": op});
        for (key, value) in args.as_object().into_iter().flatten() {
            request[key] = value.clone();
        }
        let mut bytes = serde_json::to_vec(&request).unwrap();
        bytes.push(b'\n');
        self.write.write_all(&bytes).await.unwrap();
        loop {
            let message = self.next_message().await.expect("response before EOF");
            self.received.push(message.clone());
            if message["id"] == json!(id) {
                return message;
            }
        }
    }

    async fn ok(&mut self, op: &str, args: Value) -> Value {
        let response = self.call(op, args).await;
        assert_eq!(response["ok"], json!(true), "{op} failed: {response}");
        response
    }

    /// Notification payloads received so far for `uuid`, in order.
    fn notifications(&self, uuid: &str) -> Vec<String> {
        self.received
            .iter()
            .filter(|m| m["event"] == json!("notification") && m["uuid"] == json!(uuid))
            .map(|m| m["value"].as_str().unwrap().to_string())
            .collect()
    }
}

/// Scan, then connect to the scenario's one peripheral.
async fn scan_and_connect(client: &mut Client) {
    client.ok("scan_start", json!({})).await;
    client.ok("scan_stop", json!({})).await;
    let adverts: Vec<_> = client
        .received
        .iter()
        .filter(|m| m["event"] == json!("advert"))
        .collect();
    assert_eq!(adverts.len(), 1, "one advert per scenario peripheral");
    assert_eq!(
        adverts[0],
        &json!({
            "event": "advert",
            "peripheral": PERIPHERAL,
            "service_uuids": ["AF854C2E-B214-458E-97E2-912C4ECF2CB8"],
            "manufacturer_data": "d8040244732a80",
            "local_name": null,
            "service_data": {},
            "tx_power": null,
        })
    );
    let connected = client
        .ok("connect", json!({"peripheral": PERIPHERAL.to_lowercase()}))
        .await;
    assert_eq!(connected["mtu"], json!(247));
    assert_eq!(connected["peripheral_name"], Value::Null);
}

/// Walk an establishment plan's steps as socket operations. Covers the verbs
/// the `ble-pair` plan uses; the RED-only `if:` block is skipped for the
/// legacy-style scope this scenario's advert produces.
async fn walk_over_socket(
    client: &mut Client,
    steps: &[Step],
    scope: &BTreeMap<String, String>,
    encodings: &BTreeMap<String, Encoding>,
) {
    for step in steps {
        match step {
            Step::BleConnect(_) => {
                client
                    .ok("connect", json!({"peripheral": PERIPHERAL}))
                    .await;
            }
            Step::BleDiscoverServices(_) => {
                client.ok("discover", json!({})).await;
            }
            Step::BleRead(read) => {
                client.ok("read", json!({"uuid": read.gatt})).await;
            }
            Step::BleWrite(write) => {
                let bytes = match &write.value {
                    StepValue::Captured { captured, .. } => eval::scope_string_to_bytes(
                        &scope[captured],
                        encodings.get(captured).copied(),
                    ),
                    StepValue::Runtime {
                        runtime, encoding, ..
                    } => {
                        assert_eq!(runtime, "terminalName");
                        eval::scope_string_to_bytes(TERMINAL_NAME, *encoding)
                    }
                    other => panic!("value source not used by ble-pair: {other:?}"),
                }
                .expect("value resolves");
                client
                    .ok("write", json!({"uuid": write.gatt, "value": hex(&bytes)}))
                    .await;
            }
            Step::BleSubscribe(subscribe) => {
                client
                    .ok("subscribe", json!({"uuid": subscribe.gatt}))
                    .await;
            }
            Step::If(_) => {
                assert_eq!(scope.get("style").map(String::as_str), Some("legacy"));
            }
            other => panic!("verb not used by ble-pair: {}", other.verb_name()),
        }
    }
}

/// The reference pair order from `ble_pair_roundtrip.rs` `assert_app_order`
/// for a legacy body.
fn expected_pair_log(ble: &FamilyBleBlock) -> Vec<BleEvent> {
    let mut expected = vec![
        BleEvent::Connect,
        BleEvent::DiscoverServices,
        BleEvent::Read {
            uuid: uuid(ble, "protectedSerialString"),
        },
        BleEvent::Write {
            uuid: uuid(ble, "pairingKey"),
            value: vec![0x44, 0x73, 0x2a, 0x80],
        },
        BleEvent::Write {
            uuid: uuid(ble, "deviceNameString"),
            value: TERMINAL_NAME.as_bytes().to_vec(),
        },
    ];
    for name in [
        "apState",
        "transferState",
        "dateSyncState",
        "locationSyncState",
    ] {
        expected.push(BleEvent::Subscribe {
            uuid: uuid(ble, name),
            mode: CccdMode::Notify,
        });
    }
    expected.push(BleEvent::Read {
        uuid: uuid(ble, "transferState"),
    });
    for name in [
        "remoteBootSetting",
        "cameraSSIDNameString",
        "loggingSetting",
        "cameraVitalState",
        "locationSyncSetting",
        "imageResizeRate",
        "imageResizeSetting",
        "iptcSetting",
        "locationSyncCycle",
    ] {
        expected.push(BleEvent::Subscribe {
            uuid: uuid(ble, name),
            mode: CccdMode::Notify,
        });
    }
    expected
}

fn legacy_scope(
    view: &ModelView,
    scenario: &BleScenario,
) -> (BTreeMap<String, String>, BTreeMap<String, Encoding>) {
    let advert = &scenario.peripherals()[0].advert;
    let facts = BleAdvertFacts {
        service_uuids: advert.service_uuids.clone(),
        manufacturer_data: Some((
            u16::from_le_bytes([advert.manufacturer_data[0], advert.manufacturer_data[1]]),
            advert.manufacturer_data[2..].to_vec(),
        )),
        ..Default::default()
    };
    for (_, sig) in &view.signatures {
        if let Signature::BleAdvert(sig) = sig {
            if sig.discoverable && eval::advert_matches(sig, &facts) {
                return (
                    eval::advert_scope(sig, &facts).into_iter().collect(),
                    eval::advert_capture_encodings(sig).into_iter().collect(),
                );
            }
        }
    }
    panic!("no signature matched the scenario advert");
}

#[tokio::test]
async fn pair_plan_walked_over_the_socket_matches_the_reference_order() {
    let running = start_server().await;
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let scenario = BleScenario::load(&scenario_path(), SharedLink::default()).unwrap();
    let (scope, encodings) = legacy_scope(&view, &scenario);

    let mut client = Client::connect(running.ble_addr).await;
    client.ok("scan_start", json!({})).await;
    client.ok("scan_stop", json!({})).await;
    walk_over_socket(
        &mut client,
        &ble.establishment("ble-pair").unwrap().steps,
        &scope,
        &encodings,
    )
    .await;

    assert_eq!(running.socket.log(), expected_pair_log(ble));
    assert_eq!(
        running.link.device_name().as_deref(),
        Some(TERMINAL_NAME),
        "the pair walk registers the device name on the linked engine"
    );
    drop(client);
    running.stop().await;
}

/// The `ble-establish-wifi-ap` exchange for launch mode 4, by hand: baseline
/// read, subscribe, prep write, fenced launch write, then the credentials.
async fn handoff(client: &mut Client, ble: &FamilyBleBlock) {
    let ap_state = uuid(ble, "apState");
    let baseline = client.ok("read", json!({"uuid": ap_state})).await;
    assert_eq!(baseline["value"], json!("0080"));
    client.ok("subscribe", json!({"uuid": ap_state})).await;
    client
        .ok(
            "write",
            json!({"uuid": uuid(ble, "imageTransferSetting"), "value": "01"}),
        )
        .await;
    let fenced = client
        .ok(
            "write_fenced",
            json!({
                "uuid": uuid(ble, "functionLaunchRequest"),
                "value": "0400",
                "notify_uuid": ap_state,
            }),
        )
        .await;
    let flush = client.ok("scan_stop", json!({})).await;

    // The fenced write's response precedes the notifications it caused.
    let at = |m: &Value| client.received.iter().position(|r| r == m).unwrap();
    let after: Vec<_> = client.received[at(&fenced) + 1..at(&flush)].to_vec();
    assert_eq!(
        after,
        vec![
            json!({"event": "notification", "uuid": ap_state, "value": "0280"}),
            json!({"event": "notification", "uuid": ap_state, "value": "0180"}),
        ]
    );

    let ssid = client
        .ok("read", json!({"uuid": uuid(ble, "cameraSSIDNameString")}))
        .await;
    assert_eq!(unhex(ssid["value"].as_str().unwrap()), b"ptpsim-ap");
    let pass = client
        .ok(
            "read",
            json!({"uuid": uuid(ble, "cameraWiFiPassphraseString")}),
        )
        .await;
    assert_eq!(unhex(pass["value"].as_str().unwrap()), b"ptpsim-pass");
}

#[tokio::test]
async fn handoff_over_the_socket_arms_the_engine_and_keeps_the_name() {
    let running = start_server().await;
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();

    let mut client = Client::connect(running.ble_addr).await;
    scan_and_connect(&mut client).await;
    client.ok("discover", json!({})).await;
    client
        .ok(
            "write",
            json!({"uuid": uuid(ble, "deviceNameString"), "value": hex(TERMINAL_NAME.as_bytes())}),
        )
        .await;
    handoff(&mut client, ble).await;

    assert!(running.link.is_armed(), "prep then launch arms the engine");
    assert_eq!(running.link.device_name().as_deref(), Some(TERMINAL_NAME));
    drop(client);
    running.stop().await;
}

#[tokio::test]
async fn launch_without_the_prep_write_leaves_the_link_unarmed() {
    let running = start_server().await;
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    assert!(
        running.link.is_armed(),
        "a fresh service is armed by default"
    );

    let mut client = Client::connect(running.ble_addr).await;
    scan_and_connect(&mut client).await;
    client.ok("discover", json!({})).await;
    client
        .ok("subscribe", json!({"uuid": uuid(ble, "apState")}))
        .await;
    client
        .ok(
            "write_fenced",
            json!({
                "uuid": uuid(ble, "functionLaunchRequest"),
                "value": "0400",
                "notify_uuid": uuid(ble, "apState"),
            }),
        )
        .await;

    assert!(
        !running.link.is_armed(),
        "launch without prep refuses the init"
    );
    assert_eq!(running.link.device_name(), None);
    drop(client);
    running.stop().await;
}

#[tokio::test]
async fn device_name_write_over_the_socket_is_recorded() {
    let running = start_server().await;
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();

    let mut client = Client::connect(running.ble_addr).await;
    scan_and_connect(&mut client).await;
    client.ok("discover", json!({})).await;
    client
        .ok(
            "write",
            json!({"uuid": uuid(ble, "deviceNameString"), "value": hex(b"other-host")}),
        )
        .await;

    assert_eq!(
        running.link.device_name().as_deref(),
        Some("other-host"),
        "the init gate compares against the registered name"
    );
    drop(client);
    running.stop().await;
}

#[tokio::test]
async fn each_accept_rebuilds_the_responder_from_the_scenario() {
    let running = start_server().await;
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let ap_state = uuid(ble, "apState");

    for pass in 0..2 {
        let mut client = Client::connect(running.ble_addr).await;
        scan_and_connect(&mut client).await;
        client.ok("discover", json!({})).await;
        handoff(&mut client, ble).await;
        assert_eq!(
            client.notifications(&ap_state),
            vec!["0280", "0180"],
            "pass {pass} sees an unspent notification script"
        );
        client.ok("disconnect", json!({})).await;
        // Closing TCP ends the connection. The next connect opens a new one.
        drop(client);
    }
    let connects = running
        .socket
        .log()
        .iter()
        .filter(|e| **e == BleEvent::Connect)
        .count();
    assert_eq!(connects, 2, "both passes land in the service history");
    running.stop().await;
}

#[tokio::test]
async fn scenario_exposes_every_manifest_characteristic_and_quiet_subscribes_succeed() {
    let running = start_server().await;
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let declared: std::collections::BTreeSet<String> = ble.gatt.values().cloned().collect();

    let mut client = Client::connect(running.ble_addr).await;
    scan_and_connect(&mut client).await;
    let discovered = client.ok("discover", json!({})).await;
    let served: std::collections::BTreeSet<String> =
        serde_json::from_value(discovered["characteristics"].clone()).unwrap();
    assert_eq!(
        served, declared,
        "discover reports exactly the model's manifest GATT catalog"
    );

    // Every declared characteristic accepts a subscription; none of them has
    // a notification scripted before the fenced launch write, so all stay
    // quiet.
    for uuid in &declared {
        client.ok("subscribe", json!({"uuid": uuid})).await;
    }
    client.ok("scan_stop", json!({})).await;
    assert!(
        client
            .received
            .iter()
            .all(|m| m["event"] != json!("notification")),
        "unscripted subscriptions stay quiet"
    );
    drop(client);
    running.stop().await;
}

#[tokio::test]
async fn a_second_connect_on_one_connection_is_idempotent() {
    let running = start_server().await;
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let ap_state = uuid(ble, "apState");

    let mut client = Client::connect(running.ble_addr).await;
    scan_and_connect(&mut client).await;
    client.ok("discover", json!({})).await;
    client.ok("subscribe", json!({"uuid": ap_state})).await;
    let again = client
        .ok("connect", json!({"peripheral": PERIPHERAL}))
        .await;
    assert_eq!(again["mtu"], json!(247));
    assert_eq!(again["peripheral_name"], Value::Null);

    // Discovery and the subscription survive the repeated connect: a read
    // needs no second discover, and the fenced launch still notifies.
    let read = client.ok("read", json!({"uuid": ap_state})).await;
    assert_eq!(read["value"], json!("0080"));
    client
        .ok(
            "write_fenced",
            json!({
                "uuid": uuid(ble, "functionLaunchRequest"),
                "value": "0400",
                "notify_uuid": ap_state,
            }),
        )
        .await;
    client.ok("scan_stop", json!({})).await;
    assert_eq!(client.notifications(&ap_state), vec!["0280", "0180"]);
    let connects = running
        .socket
        .log()
        .iter()
        .filter(|e| **e == BleEvent::Connect)
        .count();
    assert_eq!(
        connects, 1,
        "the repeated connect does not reach the responder"
    );
    drop(client);
    running.stop().await;
}

#[tokio::test]
async fn a_second_concurrent_client_is_refused_as_busy() {
    let running = start_server().await;
    let mut first = Client::connect(running.ble_addr).await;
    first.ok("scan_start", json!({})).await;

    // A refused client gets `busy` instead of `hello`, so `hello` always
    // means the connection holds the slot.
    let stream = TcpStream::connect(running.ble_addr).await.unwrap();
    let mut second = BufReader::new(stream).lines();
    let first_line = timeout(IO_TIMEOUT, second.next_line())
        .await
        .unwrap()
        .unwrap()
        .expect("busy line");
    assert_eq!(
        serde_json::from_str::<Value>(&first_line).unwrap(),
        json!({"event": "error", "code": "busy"})
    );
    let closed = timeout(IO_TIMEOUT, second.next_line())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(closed, None, "busy client is closed");

    // The first client is unaffected.
    first.ok("scan_stop", json!({})).await;
    drop(first);
    running.stop().await;
}

#[tokio::test]
async fn responder_errors_and_bad_requests_map_to_wire_codes() {
    let running = start_server().await;
    let mut client = Client::connect(running.ble_addr).await;

    for (op, args) in [
        ("discover", json!({})),
        ("read", json!({"uuid": FFF1})),
        ("write", json!({"uuid": FFF1, "value": "01"})),
        (
            "write_fenced",
            json!({"uuid": FFF1, "value": "01", "notify_uuid": FFF2}),
        ),
        ("subscribe", json!({"uuid": FFF1})),
    ] {
        let response = client.call(op, args).await;
        assert_eq!(
            response["error"]["code"],
            json!("not_connected"),
            "{op} before connect: {response}"
        );
    }
    let unknown = client.call("connect", json!({"peripheral": FFF1})).await;
    assert_eq!(unknown["error"]["code"], json!("unknown_peripheral"));
    let bad_op = client.call("frobnicate", json!({})).await;
    assert_eq!(bad_op["error"]["code"], json!("bad_request"));

    scan_and_connect(&mut client).await;
    let undiscovered = client.call("read", json!({"uuid": FFF1})).await;
    assert_eq!(
        undiscovered["error"]["code"],
        json!("services_not_discovered")
    );
    client.ok("discover", json!({})).await;
    let bad_hex = client
        .call("write", json!({"uuid": FFF1, "value": "zz"}))
        .await;
    assert_eq!(bad_hex["error"]["code"], json!("bad_request"));
    let signed_hex = client
        .call("write", json!({"uuid": FFF1, "value": "+1"}))
        .await;
    assert_eq!(
        signed_hex["error"]["code"],
        json!("bad_request"),
        "{signed_hex}"
    );
    let unsupported = client.call("await_disconnect", json!({})).await;
    assert_eq!(
        unsupported["error"]["code"],
        json!("bad_request"),
        "scripted peer disconnect is not part of this version"
    );
    drop(client);
    running.stop().await;
}

#[tokio::test]
async fn notifications_released_while_unsubscribed_are_dropped() {
    let running = start_server().await;
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let ap_state = uuid(ble, "apState");

    let mut client = Client::connect(running.ble_addr).await;
    scan_and_connect(&mut client).await;
    client.ok("discover", json!({})).await;
    client
        .ok(
            "write",
            json!({"uuid": uuid(ble, "imageTransferSetting"), "value": "01"}),
        )
        .await;
    // The launch releases the scripted AP-state transitions while AP state
    // has no enabled notifications, so a GATT server never sends them.
    client
        .ok(
            "write_fenced",
            json!({
                "uuid": uuid(ble, "functionLaunchRequest"),
                "value": "0400",
                "notify_uuid": ap_state,
            }),
        )
        .await;
    client.ok("subscribe", json!({"uuid": ap_state})).await;
    client.ok("scan_stop", json!({})).await;
    assert_eq!(
        client.notifications(&ap_state),
        Vec::<String>::new(),
        "a late subscribe receives nothing released before it"
    );
    drop(client);
    running.stop().await;
}

#[tokio::test]
async fn disconnect_responds_then_closes_the_connection() {
    let running = start_server().await;
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();

    let mut client = Client::connect(running.ble_addr).await;
    scan_and_connect(&mut client).await;
    client.ok("discover", json!({})).await;
    client.ok("disconnect", json!({})).await;
    // A request sent after the disconnect gets no answer: the server has
    // closed the connection.
    let mut bytes =
        serde_json::to_vec(&json!({"id": 99, "op": "read", "uuid": uuid(ble, "transferState")}))
            .unwrap();
    bytes.push(b'\n');
    let _ = client.write.write_all(&bytes).await;
    assert_eq!(client.next_message().await, None, "connection closed");
    assert!(
        !running
            .socket
            .log()
            .iter()
            .any(|e| matches!(e, BleEvent::Read { .. })),
        "nothing reaches the responder after disconnect"
    );
    running.stop().await;
}

#[tokio::test]
async fn a_new_connection_resets_the_link_handoff_state() {
    let running = start_server().await;
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();

    let mut first = Client::connect(running.ble_addr).await;
    scan_and_connect(&mut first).await;
    first.ok("discover", json!({})).await;
    first
        .ok(
            "write",
            json!({"uuid": uuid(ble, "deviceNameString"), "value": hex(TERMINAL_NAME.as_bytes())}),
        )
        .await;
    handoff(&mut first, ble).await;
    assert!(running.link.is_armed());
    assert_eq!(running.link.device_name().as_deref(), Some(TERMINAL_NAME));
    first.ok("disconnect", json!({})).await;
    drop(first);

    // The second connection skips both the prep write and the name write.
    let mut second = Client::connect(running.ble_addr).await;
    scan_and_connect(&mut second).await;
    second.ok("discover", json!({})).await;
    second
        .ok("subscribe", json!({"uuid": uuid(ble, "apState")}))
        .await;
    second
        .ok(
            "write_fenced",
            json!({
                "uuid": uuid(ble, "functionLaunchRequest"),
                "value": "0400",
                "notify_uuid": uuid(ble, "apState"),
            }),
        )
        .await;
    assert!(
        !running.link.is_armed(),
        "an earlier connection's prep write must not arm this launch"
    );
    assert_eq!(
        running.link.device_name(),
        None,
        "an earlier connection's registered name must not gate this session"
    );
    drop(second);
    running.stop().await;
}

#[tokio::test]
async fn a_non_loopback_ble_bind_is_refused_without_the_opt_in() {
    let card = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(card.path().join("DCIM/100_FUJI")).unwrap();
    let loopback: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let config = Config {
        instance_id: "ble-socket-bind-test".into(),
        profile: "fuji/gfx100ii".into(),
        connection: "app".into(),
        manifest_yaml: std::fs::read_to_string(repo(
            "packages/camera-config-data/fuji/gfx100ii/gfx100ii.consolidated.yaml",
        ))
        .unwrap(),
        media_root: card.path().to_path_buf(),
        command_bind: Some(loopback),
        liveview_bind: Some(loopback),
        event_bind: Some(loopback),
        knock_bind: None,
        pcss_init_fails: 0,
        pcss_shutter_enqueue_count: 0,
        control_bind: loopback,
        liveview_dir: None,
        state_callback: None,
    };
    let mut server = Server::bind(config).await.unwrap();
    for addr in ["0.0.0.0:0", "[::]:0"] {
        let error = server
            .enable_ble_socket(addr.parse().unwrap(), &scenario_path(), false)
            .await
            .err()
            .unwrap_or_else(|| panic!("{addr} is refused"));
        assert!(error.to_string().contains("loopback"), "{error}");
    }
    assert!(server.ble_addr_opt().is_none());
}
