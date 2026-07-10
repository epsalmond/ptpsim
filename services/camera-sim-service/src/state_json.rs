use camera_media_store::ObjectQuery;
use camera_sim::Engine;
use ptp_core::dataset::PropValue;

/// Build the camera-state JSON body. Snake_case + `serde_json::json!` to match
/// `control.rs`'s `Health::json`. Keyed iteration (`BTreeMap`) makes output
/// deterministic for a given state.
pub(crate) fn snapshot_json(engine: &Engine) -> String {
    let state = engine.state();
    let props: serde_json::Map<String, serde_json::Value> = state
        .props
        .iter()
        .map(|(&code, val)| {
            let v = match val {
                PropValue::U8(x) => serde_json::json!(x),
                PropValue::U16(x) => serde_json::json!(x),
                PropValue::U32(x) => serde_json::json!(x),
                PropValue::U64(x) => serde_json::json!(x),
                PropValue::Str(s) => serde_json::json!(s),
            };
            (format!("0x{code:04x}"), v)
        })
        .collect();
    let property_labels: serde_json::Map<String, serde_json::Value> = state
        .props
        .keys()
        .filter_map(|&code| {
            engine
                .manifest()
                .property(code)
                .map(|prop| (format!("0x{code:04x}"), serde_json::json!(prop.name)))
        })
        .collect();
    serde_json::json!({
        "phase": state.phase.state_name(),
        "session_open": state.session_open,
        "camera_initiated_transfer_active": engine.camera_initiated_transfer_active(),
        "props": props,
        "property_labels": property_labels,
        "media": { "objects": engine.store().handles(ObjectQuery::default()).len() },
        "transfer_queues": engine.transfer_queue_stats(),
    })
    .to_string()
}
