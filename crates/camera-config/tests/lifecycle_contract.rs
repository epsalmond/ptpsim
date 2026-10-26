use camera_config::{
    generated_connection_lifecycle_json_schema, ConnectionLifecycleRecord,
    ConnectionLifecycleSnapshot,
};
use std::path::PathBuf;

fn data(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

#[test]
fn checked_in_lifecycle_schema_is_generated_from_closed_rust_types() {
    let committed = data("packages/camera-config-data/ptpsim-connection-lifecycle-v1.schema.json");
    assert_eq!(
        committed,
        generated_connection_lifecycle_json_schema().unwrap()
    );
}

#[test]
fn snapshot_round_trips_through_the_versioned_record() {
    let record = ConnectionLifecycleRecord::Snapshot(ConnectionLifecycleSnapshot::default());
    let json = serde_json::to_string(&record).unwrap();
    let decoded: ConnectionLifecycleRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, record);
}
