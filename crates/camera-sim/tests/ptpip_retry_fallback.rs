//! Reference-walker coverage for response-selected retry fallback.

use std::collections::BTreeMap;

use camera_config::{CameraManifest, SetPropValue, Step, StepRetry};
use camera_media_store::MediaStore;
use camera_sim::{walk_ptpip_in, Engine, FaultMutation, FaultSelector, FaultSpec};
use std::sync::atomic::{AtomicU64, Ordering};

fn unique_temp_root(prefix: &str) -> std::path::PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "{prefix}-{nanos}-{count}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ))
}

const PRIMARY_OP: u16 = 0x9001;
const FALLBACK_OP: u16 = 0x9002;
const SELECTED_RESPONSE: u16 = 0x2019;
const FALLBACK_RESPONSE: u16 = 0x2005;
const RETRY_DELAY_MS: u32 = 25;

fn manifest() -> CameraManifest {
    CameraManifest::from_yaml(
        r#"
schema: camera-config/v1
camera: { manufacturer: Example, model: Retry, firmware: "1" }
properties:
  "0xd001":
    name: fallbackMarker
    type: u16
    access: readWrite
    initialValue: 3
connections:
  test:
    kind: ptpip-direct
    modes: [image-transfer]
"#,
    )
    .expect("retry fixture manifest loads")
}

fn engine() -> Engine {
    let root = unique_temp_root("ptpsim-retry-fallback");
    std::fs::create_dir_all(&root).expect("create media root");
    let store = MediaStore::open(&root).expect("open empty media store");
    Engine::new(manifest(), store)
}

fn fault(operation: u16, count: u32, response: u16) -> FaultSpec {
    FaultSpec {
        selector: FaultSelector {
            operation,
            params: None,
            skip: 0,
            count: Some(count),
        },
        mutation: FaultMutation::FailResponse { response },
    }
}

fn send_op(operation: u16) -> Step {
    Step {
        send_op: Some(format!("{operation:#06x}")),
        ..Default::default()
    }
}

fn set_marker(value: i64) -> Step {
    Step {
        set_prop: Some("0xd001".into()),
        value: Some(SetPropValue::Literal(value)),
        ..Default::default()
    }
}

fn marker_read() -> Step {
    Step {
        get_prop: Some("0xd001".into()),
        ..Default::default()
    }
}

fn retry(fallback: Vec<Step>, tolerant: bool, max_attempts: u32) -> Step {
    Step {
        retry: Some(StepRetry {
            steps: vec![send_op(PRIMARY_OP)],
            fallback: Some(fallback),
            when_response_codes: vec![format!("{SELECTED_RESPONSE:#06x}")],
            when_failure_classes: Vec::new(),
            max_attempts,
            retry_delay_ms: RETRY_DELAY_MS,
        }),
        tolerant,
        ..Default::default()
    }
}

#[test]
fn exhausted_selected_retry_runs_fallback_after_primary_delays() {
    let mut engine = engine();
    engine
        .install_fault(fault(PRIMARY_OP, 3, SELECTED_RESPONSE))
        .expect("install selected response fault");

    let outcome = walk_ptpip_in(
        &mut engine,
        &[retry(vec![set_marker(7)], false, 3), marker_read()],
        &BTreeMap::new(),
        Some("test"),
    )
    .expect("the fallback replaces the exhausted selected response");

    assert_eq!(outcome.observed.get(0xd001), Some(7));
    assert_eq!(
        outcome.retry_delays_ms,
        [RETRY_DELAY_MS, RETRY_DELAY_MS],
        "three primary attempts consume two delays and no delay before fallback",
    );
    assert_eq!(
        outcome.steps_run, 3,
        "fallback, outer retry, and the following read each complete once",
    );
}

#[test]
fn fallback_response_failure_obeys_outer_retry_tolerance() {
    let mut tolerant_engine = engine();
    tolerant_engine
        .install_fault(fault(PRIMARY_OP, 2, SELECTED_RESPONSE))
        .expect("install selected response fault");
    tolerant_engine
        .install_fault(fault(FALLBACK_OP, 1, FALLBACK_RESPONSE))
        .expect("install fallback response fault");

    let tolerant = walk_ptpip_in(
        &mut tolerant_engine,
        &[retry(vec![send_op(FALLBACK_OP)], true, 2), marker_read()],
        &BTreeMap::new(),
        Some("test"),
    )
    .expect("outer tolerance accepts the fallback's non-OK response");
    assert_eq!(tolerant.observed.get(0xd001), Some(3));
    assert_eq!(tolerant.retry_delays_ms, [RETRY_DELAY_MS]);

    let mut strict_engine = engine();
    strict_engine
        .install_fault(fault(PRIMARY_OP, 2, SELECTED_RESPONSE))
        .expect("install selected response fault");
    strict_engine
        .install_fault(fault(FALLBACK_OP, 1, FALLBACK_RESPONSE))
        .expect("install fallback response fault");
    let error = walk_ptpip_in(
        &mut strict_engine,
        &[retry(vec![send_op(FALLBACK_OP)], false, 2)],
        &BTreeMap::new(),
        Some("test"),
    )
    .expect_err("a strict retry propagates the fallback's non-OK response");
    assert_eq!(error.response_code, Some(FALLBACK_RESPONSE));
    assert!(error.step.contains(".fallback[0].sendOp"), "{error:?}");
}
