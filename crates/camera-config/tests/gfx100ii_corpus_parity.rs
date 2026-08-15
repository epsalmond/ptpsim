use camera_config::{CameraManifest, OperationKind, PropertyKind};
use std::path::PathBuf;

fn data(rel: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data")
        .join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

#[test]
fn loader_round_trips_consolidated_without_loss() {
    let yaml = data("fuji/gfx100ii/gfx100ii.consolidated.yaml");
    let manifest = CameraManifest::from_yaml(&yaml).expect("consolidated loads");
    let rendered = manifest.to_yaml().unwrap();
    let reparsed = CameraManifest::from_yaml(&rendered).expect("round-trip loads");
    assert_eq!(manifest.operations.len(), reparsed.operations.len());
    assert_eq!(manifest.properties.len(), reparsed.properties.len());
    assert_eq!(manifest.camera.model, reparsed.camera.model);
    // Raw placeholders stay unresolved catalog entries.
    for (code, op) in &reparsed.operations {
        if op.name.starts_with("raw_") {
            assert_eq!(op.kind, OperationKind::AdvertisedOnly, "raw op {code}");
        }
    }
    for (code, prop) in &reparsed.properties {
        if prop.name.starts_with("raw_") {
            assert_eq!(prop.kind, PropertyKind::CatalogOnly, "raw prop {code}");
        }
    }
}

#[test]
fn corpus_parity_is_byte_deterministic() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data/fuji/gfx100ii/evidence");
    let mut bundles = Vec::new();
    for directory in ["probe", "labels", "value-profiles"] {
        let mut paths = std::fs::read_dir(root.join(directory))
            .expect("evidence directory")
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            bundles.push(std::fs::read_to_string(path).unwrap());
        }
    }
    let all_refs: Vec<&str> = bundles.iter().map(String::as_str).collect();
    // The slice test's exact bundle set is 10 bundles: 8 probes + iso legality + usb descriptor? Check migration.
    let proposal = camera_config::propose(&all_refs).unwrap();
    let committed: camera_config::Proposal = serde_json::from_str(
        &std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../packages/camera-config-data/fuji/gfx100ii/evidence/camera-observation-v1.proposal.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(proposal.digest, committed.digest);
    assert_eq!(
        camera_config::proposal_json(&proposal).unwrap(),
        camera_config::proposal_json(&committed).unwrap(),
        "proposal bytes must be deterministic"
    );

    // Census at 4a5b8000: 46 ops, 327 props, 680 candidates.
    let manifest =
        CameraManifest::from_yaml(&data("fuji/gfx100ii/gfx100ii.consolidated.yaml")).unwrap();
    assert_eq!(manifest.operations.len(), 46, "operation census");
    assert_eq!(manifest.properties.len(), 327, "property census");
    assert_eq!(proposal.candidates.len(), 680, "candidate census");

    // Representative value rows survive loader.
    let iso = manifest.properties.get("0xd02a").expect("ISO property");
    assert!(iso.value_rows.iter().any(|r| r.raw == 6400));
    assert!(manifest
        .semantic_assertions
        .properties
        .contains_key("0xd02a"));
}

#[test]
fn usb_attachment_remains_typed_discovery() {
    let manifest = CameraManifest::from_yaml(&data("fuji/gfx100ii/gfx100ii.yaml")).unwrap();
    let usb = manifest.connections.get("usb").expect("usb connection");
    assert_eq!(usb.kind.as_deref(), Some("usb"));
    let discovery = usb.discovery.as_ref().expect("usb discovery");
    assert_eq!(discovery.mechanism, "usb");
    assert_eq!(discovery.vid, Some(0x04cb));
    let passthrough = manifest
        .connections
        .get("usb-passthrough")
        .expect("passthrough");
    assert_eq!(passthrough.kind.as_deref(), Some("usb-passthrough"));
    let discovery = passthrough
        .discovery
        .as_ref()
        .expect("passthrough discovery");
    assert_eq!(discovery.mechanism, "usb");
}
