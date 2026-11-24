//! Simulator states (#164, #352): while a manifest-declared state is active,
//! the engine answers the operations it lists with the declared response. The
//! generic tests use a small synthetic manifest; the GFX100 II test replays the
//! real image-transfer choreography with the state declared in the test.

use camera_config::{parse_hex_code, ActionVerb, CameraManifest, ModeEntryExecution, Step};
use camera_media_store::MediaStore;
use camera_sim::{
    walk_ptpip_in, Engine, FaultMutation, FaultSelector, FaultSpec, Reply, StateOverlay,
};
use ptp_core::dataset::PropValue;
use ptp_core::{OperationRequest, Writer};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

const OPEN_SESSION: u16 = 0x1002;
const CLOSE_SESSION: u16 = 0x1003;
const GET_STORAGE_IDS: u16 = 0x1004;
const GET_DEVICE_PROP_VALUE: u16 = 0x1015;
const SET_DEVICE_PROP_VALUE: u16 = 0x1016;
const OK: u16 = 0x2001;
const GENERAL_ERROR: u16 = 0x2002;
const SESSION_NOT_OPEN: u16 = 0x2003;
const OPERATION_NOT_SUPPORTED: u16 = 0x2005;
const DEVICE_BUSY: u16 = 0x2019;

/// A synthetic body. `0x9001` starts a sequence gate, applies an effect, and
/// queues an event when it succeeds; `slotEmpty` answers it with Device Busy
/// on `app` in `browse`. `0x9003` is silently refused and `0x9004` requires
/// the gate; `otherCondition` answers both.
const MANIFEST: &str = r#"
schema: camera-config/v1
camera: { manufacturer: TESTCO, model: TEST }
evidence:
  docCondition: { kind: doc, path: "evidence/CONDITION.md" }
sequenceGates:
  bootstrap: { evidence: [docCondition] }
modes:
  browse:
    detect: { prop: "0xd001", eq: 1 }
  shoot:
    detect: { prop: "0xd001", eq: 2 }
connections:
  app:
    kind: ptpip-app
    bindings: { command: 55740 }
    actions:
      enumerateObjects:
        mode: browse
        initiator:
          steps:
            - { sendOp: "0x9001", startsGate: bootstrap }
            - { sendOp: "0x9002", completesGate: bootstrap }
  tether:
    kind: ptpip
    bindings: { command: 15740 }
operations:
  "0x9001":
    name: VendorBegin
    modes: [browse]
    connections: [app]
    effects: [{ setProp: "0xd002", value: 1 }]
    emits: ["0xc001"]
  "0x9002": { name: VendorPage, modes: [browse], connections: [app] }
  "0x9003":
    name: VendorRefused
    dispositions: [{ disposition: silentRefusal }]
  "0x9004":
    name: VendorGated
    requiresGate: { name: bootstrap, failure: noResponse }
properties:
  "0xd001": { name: selector, type: u16, access: readWrite }
  "0xd002": { name: marker, type: u16, access: readOnly }
  "0xd003":
    name: gatedCount
    type: u32
    access: readOnly
    initialValue: 7
    requiresGate: { name: bootstrap, failure: noResponse }
states:
  slotEmpty:
    summary: The card slot holds no card. Nothing else is modeled.
    responses:
      - { operation: "0x9001", response: "0x2019", connections: [app], modes: [browse] }
    evidence: [docCondition]
  otherCondition:
    summary: A second condition used to pin precedence.
    responses:
      - { operation: "0x9003", response: "0x2019" }
      - { operation: "0x9004", response: "0x2019" }
    evidence: [docCondition]
"#;

fn store() -> (MediaStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = MediaStore::open(dir.path()).expect("temp store");
    (store, dir)
}

fn engine_for(manifest: CameraManifest) -> (Engine, tempfile::TempDir) {
    let (store, dir) = store();
    (Engine::new(manifest, store), dir)
}

fn engine() -> (Engine, tempfile::TempDir) {
    engine_for(CameraManifest::from_yaml(MANIFEST).expect("manifest loads"))
}

fn states(entries: &[(&str, bool)]) -> StateOverlay {
    StateOverlay {
        states: entries
            .iter()
            .map(|(name, on)| (name.to_string(), *on))
            .collect(),
        ..Default::default()
    }
}

fn enter(engine: &mut Engine, name: &str) {
    let applied = engine
        .apply_state_overlay(&states(&[(name, true)]))
        .expect("declared state enters");
    assert_eq!(applied.states, 1);
}

struct Wire {
    tid: u32,
}

impl Wire {
    fn new() -> Self {
        Wire { tid: 0 }
    }

    fn send(&mut self, engine: &mut Engine, code: u16, params: Vec<u32>) -> Reply {
        self.send_with(engine, code, params, None)
    }

    fn send_with(
        &mut self,
        engine: &mut Engine,
        code: u16,
        params: Vec<u32>,
        data: Option<&[u8]>,
    ) -> Reply {
        self.tid += 1;
        let request = OperationRequest {
            data_phase_info: 1,
            code,
            transaction_id: self.tid,
            params,
        };
        engine.on_operation(&request, data)
    }

    fn code(&mut self, engine: &mut Engine, code: u16, params: Vec<u32>) -> Option<u16> {
        response_code(&self.send(engine, code, params))
    }

    fn open(&mut self, engine: &mut Engine) {
        assert_eq!(self.code(engine, OPEN_SESSION, vec![1]), Some(OK));
    }

    fn select(&mut self, engine: &mut Engine, selector: u16) {
        let mut writer = Writer::new();
        PropValue::U16(selector).encode(&mut writer).unwrap();
        let payload = writer.into_vec();
        let reply = self.send_with(engine, SET_DEVICE_PROP_VALUE, vec![0xd001], Some(&payload));
        assert_eq!(response_code(&reply), Some(OK));
    }

    fn read_u16(&mut self, engine: &mut Engine, property: u32) -> u16 {
        match self.send(engine, GET_DEVICE_PROP_VALUE, vec![property]) {
            Reply::Data { data, response } => {
                assert_eq!(response.code, OK);
                u16::from_le_bytes([data[0], data[1]])
            }
            other => panic!("expected a property value, got {other:?}"),
        }
    }
}

fn response_code(reply: &Reply) -> Option<u16> {
    match reply {
        Reply::Response(response) => Some(response.code),
        Reply::Data { response, .. } | Reply::DataStream { response, .. } => Some(response.code),
        Reply::NoResponse | Reply::Close => None,
    }
}

/// Open a session and detect `browse`.
fn browse(engine: &mut Engine, wire: &mut Wire) {
    wire.open(engine);
    wire.select(engine, 1);
}

#[test]
fn inactive_state_leaves_the_operation_and_its_effects_unchanged() {
    let (mut engine, _dir) = engine();
    let mut wire = Wire::new();
    browse(&mut engine, &mut wire);
    assert_eq!(wire.code(&mut engine, 0x9001, vec![]), Some(OK));
    assert_eq!(wire.read_u16(&mut engine, 0xd002), 1, "effect applied");
    assert!(engine.take_event(0xc001), "emit queued");
}

#[test]
fn active_state_answers_the_declared_response_without_side_effects() {
    let (mut engine, _dir) = engine();
    enter(&mut engine, "slotEmpty");
    assert!(engine.active_states().contains("slotEmpty"));
    let mut wire = Wire::new();
    browse(&mut engine, &mut wire);

    match wire.send(&mut engine, 0x9001, vec![]) {
        Reply::Response(response) => {
            assert_eq!(response.code, DEVICE_BUSY);
            assert!(response.params.is_empty(), "no response parameters");
        }
        other => panic!("expected a response with no data phase, got {other:?}"),
    }
    assert_eq!(wire.read_u16(&mut engine, 0xd002), 0, "no effect applied");
    assert!(!engine.take_event(0xc001), "no emit queued");

    // 0x9001 started the gate only if it succeeded; completing the sequence
    // therefore does not satisfy it.
    assert_eq!(wire.code(&mut engine, 0x9002, vec![]), Some(OK));
    assert!(!engine.state().gate_satisfied("bootstrap"));
    assert!(matches!(
        wire.send(&mut engine, GET_DEVICE_PROP_VALUE, vec![0xd003]),
        Reply::NoResponse
    ));
}

#[test]
fn active_state_answers_every_attempt() {
    let (mut engine, _dir) = engine();
    enter(&mut engine, "slotEmpty");
    let mut wire = Wire::new();
    browse(&mut engine, &mut wire);
    for _ in 0..5 {
        assert_eq!(wire.code(&mut engine, 0x9001, vec![]), Some(DEVICE_BUSY));
    }
}

/// Replays the same request sequence against an engine with the state active
/// and one without it. Outside the declared scope every reply must match.
fn assert_same_replies(sequence: impl Fn(&mut Engine, &mut Wire) -> Vec<Option<u16>>) {
    let (mut baseline, _a) = engine();
    let (mut stated, _b) = engine();
    enter(&mut stated, "slotEmpty");
    let expected = sequence(&mut baseline, &mut Wire::new());
    let actual = sequence(&mut stated, &mut Wire::new());
    assert_eq!(actual, expected);
}

#[test]
fn state_does_not_apply_before_mode_detection() {
    assert_same_replies(|engine, wire| {
        wire.open(engine);
        vec![wire.code(engine, 0x9001, vec![])]
    });
}

#[test]
fn state_does_not_apply_in_another_mode() {
    assert_same_replies(|engine, wire| {
        wire.open(engine);
        wire.select(engine, 2);
        vec![wire.code(engine, 0x9001, vec![])]
    });
    let (mut engine, _dir) = engine();
    enter(&mut engine, "slotEmpty");
    let mut wire = Wire::new();
    wire.open(&mut engine);
    wire.select(&mut engine, 2);
    assert_eq!(
        wire.code(&mut engine, 0x9001, vec![]),
        Some(OPERATION_NOT_SUPPORTED),
        "catalog availability still refuses the operation outside its modes"
    );
}

#[test]
fn state_does_not_apply_on_another_connection() {
    assert_same_replies(|engine, wire| {
        engine.bind_connection("tether");
        browse(engine, wire);
        vec![wire.code(engine, 0x9001, vec![])]
    });
}

#[test]
fn state_leaves_other_operations_unchanged() {
    assert_same_replies(|engine, wire| {
        browse(engine, wire);
        vec![
            wire.code(engine, 0x9002, vec![]),
            wire.code(engine, GET_STORAGE_IDS, vec![]),
            wire.code(engine, GET_DEVICE_PROP_VALUE, vec![0xd001]),
            wire.code(engine, 0x9003, vec![]),
        ]
    });
}

#[test]
fn state_survives_session_cycles_and_transport_loss() {
    let (mut engine, _dir) = engine();
    enter(&mut engine, "slotEmpty");
    let mut wire = Wire::new();
    browse(&mut engine, &mut wire);
    assert_eq!(wire.code(&mut engine, 0x9001, vec![]), Some(DEVICE_BUSY));

    assert_eq!(wire.code(&mut engine, CLOSE_SESSION, vec![]), Some(OK));
    browse(&mut engine, &mut wire);
    assert_eq!(wire.code(&mut engine, 0x9001, vec![]), Some(DEVICE_BUSY));

    engine.transport_lost();
    browse(&mut engine, &mut wire);
    assert_eq!(wire.code(&mut engine, 0x9001, vec![]), Some(DEVICE_BUSY));

    engine.clear_session_after_transport_end();
    browse(&mut engine, &mut wire);
    assert_eq!(wire.code(&mut engine, 0x9001, vec![]), Some(DEVICE_BUSY));
    assert!(engine.active_states().contains("slotEmpty"));
}

#[test]
fn false_leaves_the_state() {
    let (mut engine, _dir) = engine();
    enter(&mut engine, "slotEmpty");
    let applied = engine
        .apply_state_overlay(&states(&[("slotEmpty", false)]))
        .expect("declared state leaves");
    assert_eq!(applied.states, 1);
    assert!(engine.active_states().is_empty());
    let mut wire = Wire::new();
    browse(&mut engine, &mut wire);
    assert_eq!(wire.code(&mut engine, 0x9001, vec![]), Some(OK));
}

#[test]
fn unknown_state_name_rejects_the_whole_overlay() {
    let (mut engine, _dir) = engine();
    let selector_before = engine.state().props.get(&0xd001).cloned();
    let overlay = StateOverlay {
        phase: Some("liveView".into()),
        session_open: Some(true),
        props: [("0xd001".to_string(), serde_json::json!(1))]
            .into_iter()
            .collect(),
        states: [
            ("slotEmpty".to_string(), true),
            ("notDeclared".to_string(), true),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let error = engine
        .apply_state_overlay(&overlay)
        .expect_err("unknown state name");
    assert!(error.contains("unknown state 'notDeclared'"), "{error}");
    assert!(
        error.contains("declared states: otherCondition, slotEmpty"),
        "{error}"
    );
    assert!(engine.active_states().is_empty());
    assert!(!engine.state().session_open);
    assert_eq!(engine.state().phase.state_name(), "disconnected");
    assert_eq!(engine.state().props.get(&0xd001).cloned(), selector_before);
}

#[test]
fn unknown_state_name_without_declared_states_says_so() {
    let manifest = CameraManifest::from_yaml(
        "schema: camera-config/v1\ncamera: { manufacturer: TESTCO, model: TEST }\n",
    )
    .unwrap();
    let (mut engine, _dir) = engine_for(manifest);
    let error = engine
        .apply_state_overlay(&states(&[("slotEmpty", true)]))
        .expect_err("no states declared");
    assert!(error.contains("unknown state 'slotEmpty'"), "{error}");
    assert!(error.contains("no states are declared"), "{error}");
}

#[test]
fn state_response_outranks_silent_refusal() {
    let (mut engine, _dir) = engine();
    let mut wire = Wire::new();
    wire.open(&mut engine);
    assert_eq!(wire.code(&mut engine, 0x9003, vec![]), Some(OK));
    enter(&mut engine, "otherCondition");
    assert_eq!(wire.code(&mut engine, 0x9003, vec![]), Some(DEVICE_BUSY));
}

#[test]
fn unsatisfied_gate_session_and_fault_outrank_the_state() {
    let (mut engine, _dir) = engine();
    enter(&mut engine, "slotEmpty");
    enter(&mut engine, "otherCondition");
    let mut wire = Wire::new();

    // No session yet.
    assert_eq!(
        wire.code(&mut engine, 0x9001, vec![]),
        Some(SESSION_NOT_OPEN)
    );

    browse(&mut engine, &mut wire);
    // An unsatisfied requiresGate answers with silence.
    assert!(matches!(
        wire.send(&mut engine, 0x9004, vec![]),
        Reply::NoResponse
    ));

    // An installed fault answers first; the state answers once it is spent.
    engine
        .install_fault(FaultSpec {
            selector: FaultSelector {
                operation: 0x9001,
                params: None,
                skip: 0,
                count: Some(1),
            },
            mutation: FaultMutation::FailResponse {
                response: GENERAL_ERROR,
            },
        })
        .unwrap();
    assert_eq!(wire.code(&mut engine, 0x9001, vec![]), Some(GENERAL_ERROR));
    assert_eq!(wire.code(&mut engine, 0x9001, vec![]), Some(DEVICE_BUSY));
}

// GFX100 II image-transfer choreography against the shipped manifest data.
// These name the shipped state, so renaming it in gfx100ii.yaml fails here.

const SHIPPED_STATE: &str = "memoryCardSlot1Empty";

/// The shipped GFX100 II consolidated manifest, with its declared states.
fn gfx_manifest() -> CameraManifest {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data/fuji/gfx100ii/gfx100ii.consolidated.yaml");
    CameraManifest::from_yaml(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

fn gfx_engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let media = dir.path().join("DCIM/100_FUJI");
    std::fs::create_dir_all(&media).unwrap();
    std::fs::write(media.join("DSCF0001.JPG"), b"\xFF\xD8HELLOJPEG\xFF\xD9").unwrap();
    let mut store = MediaStore::open(dir.path()).unwrap();
    store.scan().unwrap();
    (Engine::new(gfx_manifest(), store), dir)
}

fn gfx_cold_entry_steps(manifest: &CameraManifest) -> Vec<Step> {
    let entry = manifest.connections["app"]
        .entries
        .iter()
        .find(|entry| entry.to == "image-transfer" && entry.from.is_none())
        .expect("cold image-transfer entry");
    match &entry.execution {
        ModeEntryExecution::Ptp { steps } => steps.clone(),
        _ => panic!("cold image-transfer entry is PTP"),
    }
}

fn gfx_enumerate_steps(manifest: &CameraManifest) -> Vec<Step> {
    manifest.connections["app"].actions[&ActionVerb::EnumerateObjects]
        .initiator()
        .expect("enumerateObjects initiator")
        .steps
        .clone()
}

/// Counts 0x9050 requests without changing them: the fault never reaches its
/// skip window, and its `seen` counter records every matching request.
fn count_image_import_begin(engine: &mut Engine) {
    engine
        .install_fault(FaultSpec {
            selector: FaultSelector {
                operation: 0x9050,
                params: None,
                skip: 1000,
                count: Some(1),
            },
            mutation: FaultMutation::FailResponse {
                response: GENERAL_ERROR,
            },
        })
        .unwrap();
}

fn image_import_begin_count(engine: &Engine) -> u32 {
    engine.faults()[0].seen
}

#[test]
fn gfx_enumeration_fails_at_image_import_while_slot_one_is_empty() {
    let (mut engine, _dir) = gfx_engine();
    let manifest = engine.manifest().clone();
    enter(&mut engine, SHIPPED_STATE);
    count_image_import_begin(&mut engine);

    walk_ptpip_in(
        &mut engine,
        &gfx_cold_entry_steps(&manifest),
        &BTreeMap::new(),
        Some("app"),
    )
    .expect("image-transfer mode entry is unchanged");

    let enumerate = gfx_enumerate_steps(&manifest);
    for invocation in 1..=2 {
        let error = walk_ptpip_in(&mut engine, &enumerate, &BTreeMap::new(), Some("app"))
            .expect_err("enumeration fails at the prime");
        assert_eq!(error.response_code, Some(DEVICE_BUSY), "{error}");
        assert!(error.step.starts_with("steps[0].retry"), "{error}");
        assert_eq!(image_import_begin_count(&engine), 5 * invocation);
    }

    // A fresh connection does not clear a physical condition.
    engine.transport_lost();
    walk_ptpip_in(
        &mut engine,
        &gfx_cold_entry_steps(&manifest),
        &BTreeMap::new(),
        Some("app"),
    )
    .expect("image-transfer mode entry after reconnection");
    let error = walk_ptpip_in(&mut engine, &enumerate, &BTreeMap::new(), Some("app"))
        .expect_err("enumeration still fails after reconnection");
    assert_eq!(error.response_code, Some(DEVICE_BUSY));

    // Other image-transfer requests answer exactly as they do without the state.
    let (mut baseline, _baseline_dir) = engine_for(gfx_manifest());
    walk_ptpip_in(
        &mut baseline,
        &gfx_cold_entry_steps(&manifest),
        &BTreeMap::new(),
        Some("app"),
    )
    .expect("baseline image-transfer mode entry");
    let other_requests = [
        (GET_STORAGE_IDS, vec![]),
        (GET_DEVICE_PROP_VALUE, vec![0xd212]),
        (GET_DEVICE_PROP_VALUE, vec![0xd22b]),
        (0x9054, vec![0x1000_0001]),
    ];
    let (mut wire, mut baseline_wire) = (Wire { tid: 900 }, Wire { tid: 900 });
    for (code, params) in other_requests {
        assert_eq!(
            wire.code(&mut engine, code, params.clone()),
            baseline_wire.code(&mut baseline, code, params),
            "{code:#06x}"
        );
    }

    // Leaving the state lets the same enumeration succeed.
    engine
        .apply_state_overlay(&states(&[(SHIPPED_STATE, false)]))
        .unwrap();
    let outcome = walk_ptpip_in(&mut engine, &enumerate, &BTreeMap::new(), Some("app"))
        .expect("enumeration succeeds once slot 1 holds a card");
    assert_eq!(outcome.observed.get(0xd620), Some(1));
}

#[test]
fn gfx_state_does_not_change_other_connections() {
    let (mut engine, _dir) = gfx_engine();
    enter(&mut engine, SHIPPED_STATE);
    engine.bind_connection("wireless-tether");
    let mut wire = Wire::new();
    wire.open(&mut engine);
    let (mut baseline, _dir2) = engine_for(gfx_manifest());
    baseline.bind_connection("wireless-tether");
    let mut baseline_wire = Wire::new();
    baseline_wire.open(&mut baseline);
    assert_eq!(
        wire.code(&mut engine, 0x9050, vec![]),
        baseline_wire.code(&mut baseline, 0x9050, vec![])
    );
}

/// Every operation code the steps can send, including inside retry blocks.
fn sent_operations(steps: &[Step], out: &mut BTreeSet<u16>) {
    for step in steps {
        if let Some(code) = step.send_op.as_deref().and_then(parse_hex_code) {
            out.insert(code);
        }
        if step.get_prop.is_some() {
            out.insert(GET_DEVICE_PROP_VALUE);
        }
        if step.set_prop.is_some() {
            out.insert(SET_DEVICE_PROP_VALUE);
        }
        if let Some(retry) = &step.retry {
            sent_operations(&retry.steps, out);
            if let Some(fallback) = &retry.fallback {
                sent_operations(fallback, out);
            }
        }
    }
}

/// Install a counting fault for each code. The faults never reach their skip
/// window, so replies are unchanged; `seen` records every matching request.
fn count_requests(engine: &mut Engine, codes: &BTreeSet<u16>) {
    for &operation in codes {
        engine
            .install_fault(FaultSpec {
                selector: FaultSelector {
                    operation,
                    params: None,
                    skip: 1000,
                    count: Some(1),
                },
                mutation: FaultMutation::FailResponse {
                    response: GENERAL_ERROR,
                },
            })
            .unwrap();
    }
}

fn request_counts(engine: &Engine) -> BTreeMap<u16, u32> {
    engine
        .faults()
        .into_iter()
        .map(|fault| (fault.selector.operation, fault.seen))
        .collect()
}

fn gfx_image_transfer_engine(state_active: bool) -> (Engine, tempfile::TempDir, Vec<Step>) {
    let (mut engine, dir) = gfx_engine();
    let manifest = engine.manifest().clone();
    assert!(
        manifest.states.contains_key(SHIPPED_STATE),
        "the shipped manifest declares {SHIPPED_STATE}"
    );
    if state_active {
        enter(&mut engine, SHIPPED_STATE);
    }
    walk_ptpip_in(
        &mut engine,
        &gfx_cold_entry_steps(&manifest),
        &BTreeMap::new(),
        Some("app"),
    )
    .expect("image-transfer mode entry");
    let steps = gfx_enumerate_steps(&manifest);
    let mut codes = BTreeSet::new();
    sent_operations(&steps, &mut codes);
    assert!(codes.contains(&0x9050) && codes.len() > 1, "{codes:?}");
    count_requests(&mut engine, &codes);
    (engine, dir, steps)
}

#[test]
fn shipped_state_answers_five_image_import_requests_then_nothing_more() {
    let (mut engine, _dir, steps) = gfx_image_transfer_engine(true);
    let error = walk_ptpip_in(&mut engine, &steps, &BTreeMap::new(), Some("app"))
        .expect_err("enumeration fails at the prime");
    // The state answers every 0x9050 the same way, and the walk ends on the
    // last of them with that response.
    assert_eq!(error.response_code, Some(DEVICE_BUSY), "{error}");
    assert!(error.step.starts_with("steps[0].retry"), "{error}");
    let counts = request_counts(&engine);
    for (code, seen) in &counts {
        let expected = if *code == 0x9050 { 5 } else { 0 };
        assert_eq!(*seen, expected, "requests for {code:#06x}: {counts:?}");
    }
}

#[test]
fn without_the_shipped_state_enumeration_proceeds_past_the_prime() {
    let (mut engine, _dir, steps) = gfx_image_transfer_engine(false);
    let outcome = walk_ptpip_in(&mut engine, &steps, &BTreeMap::new(), Some("app"))
        .expect("enumeration succeeds");
    assert_eq!(outcome.observed.get(0xd620), Some(1));
    let counts = request_counts(&engine);
    assert_eq!(counts[&0x9050], 1, "{counts:?}");
    assert_eq!(counts[&0x9053], 1, "{counts:?}");
    assert!(counts[&GET_DEVICE_PROP_VALUE] >= 2, "{counts:?}");
}

/// Property-access and session operations follow the dispatcher's own
/// availability rule (MANIFEST_SCHEMA §11.32).
const DISPATCH_RULE_MANIFEST: &str = r#"
schema: camera-config/v1
camera: { manufacturer: TESTCO, model: TEST }
evidence:
  docCondition: { kind: doc, path: "evidence/CONDITION.md" }
modes:
  browse:
    detect: { prop: "0xd001", eq: 1 }
  shoot:
    detect: { prop: "0xd001", eq: 2 }
connections:
  app:
    kind: ptpip-app
    bindings: { command: 55740 }
operations:
  "0x1002": { name: OpenSession, connections: [app] }
  "0x1015": { name: GetDevicePropValue, modes: [shoot], connections: [app] }
properties:
  "0xd001": { name: selector, type: u16, access: readWrite }
states:
  readRefused:
    summary: Property reads are refused in browse.
    responses:
      - { operation: "0x1015", response: "0x2019", connections: [app], modes: [browse] }
    evidence: [docCondition]
  cameraOff:
    summary: The camera refuses to open a session.
    responses:
      - { operation: "0x1002", response: "0x2019" }
    evidence: [docCondition]
"#;

#[test]
fn mode_scoped_state_on_a_property_read_answers_in_its_mode() {
    let manifest = CameraManifest::from_yaml(DISPATCH_RULE_MANIFEST).expect("manifest loads");
    let (mut engine, _dir) = engine_for(manifest);
    enter(&mut engine, "readRefused");
    let mut wire = Wire::new();
    wire.open(&mut engine);
    wire.select(&mut engine, 1);
    assert_eq!(
        wire.code(&mut engine, GET_DEVICE_PROP_VALUE, vec![0xd001]),
        Some(DEVICE_BUSY)
    );
    wire.select(&mut engine, 2);
    assert_eq!(
        wire.code(&mut engine, GET_DEVICE_PROP_VALUE, vec![0xd001]),
        Some(OK)
    );
}

#[test]
fn state_on_open_session_applies_before_any_session_exists() {
    let manifest = CameraManifest::from_yaml(DISPATCH_RULE_MANIFEST).expect("manifest loads");
    let (mut engine, _dir) = engine_for(manifest);
    enter(&mut engine, "cameraOff");
    let mut wire = Wire::new();
    assert_eq!(
        wire.code(&mut engine, OPEN_SESSION, vec![1]),
        Some(DEVICE_BUSY)
    );
    assert!(!engine.state().session_open);
    engine
        .apply_state_overlay(&states(&[("cameraOff", false)]))
        .unwrap();
    wire.open(&mut engine);
}
