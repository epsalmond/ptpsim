use camera_config::{
    CameraManifest, CapabilitySubject, ObservationLine, OperationKind, PropertyKind,
};
use std::collections::BTreeSet;
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
fn consolidated_connection_activities_carry_titles() {
    let manifest = CameraManifest::from_yaml(&data("fuji/gfx100ii/gfx100ii.consolidated.yaml"))
        .expect("consolidated loads");
    let titles = |connection: &str| {
        manifest.connections[connection]
            .activities
            .iter()
            .map(|activity| (activity.id.as_str(), activity.title.as_deref()))
            .collect::<Vec<_>>()
    };

    assert_eq!(
        titles("app"),
        vec![
            (
                "camera.network.join-confirmation",
                Some("Tap Join to connect to the camera")
            ),
            (
                "camera.network.associate",
                Some("Connecting to camera Wi-Fi")
            ),
            ("camera.session.open.ap", Some("Opening camera session")),
        ]
    );
    assert_eq!(
        titles("wireless-tether"),
        vec![
            (
                "camera.network.discover",
                Some("Looking for the camera on the network")
            ),
            (
                "camera.network.callback",
                Some("Camera answered, verifying identity")
            ),
            ("camera.session.init", Some("Negotiating with the camera")),
            ("camera.session.open.direct", Some("Opening camera session")),
        ]
    );
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

    // Census after restoring the app startup 0x902b operation and property slice.
    let manifest =
        CameraManifest::from_yaml(&data("fuji/gfx100ii/gfx100ii.consolidated.yaml")).unwrap();
    assert_eq!(manifest.operations.len(), 47, "operation census");
    assert_eq!(manifest.properties.len(), 330, "property census");
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
fn captured_persona_operation_inventories_survive_consolidation() {
    let manifest =
        CameraManifest::from_yaml(&data("fuji/gfx100ii/gfx100ii.consolidated.yaml")).unwrap();
    let mut inventories = std::collections::BTreeMap::new();
    for (persona, count) in [
        ("usb-stills", 24),
        ("usb-video", 24),
        ("usb-pcpriority", 24),
        ("usb-rawconv-backup", 20),
        ("usb-cardreader", 25),
        ("usb-tether-auto", 25),
        ("wireless-stills", 24),
        ("wireless-video", 24),
    ] {
        let bundle = data(&format!(
            "fuji/gfx100ii/evidence/probe/2026-05-27-ptp-evidence-{persona}.jsonl"
        ));
        let mut expected = BTreeSet::new();
        let mut context = None;
        for line in bundle.lines() {
            let ObservationLine::Capability(record) = serde_json::from_str(line).unwrap() else {
                continue;
            };
            if let CapabilitySubject::Operation {
                code,
                supported: true,
                ..
            } = record.subject
            {
                if let Some(previous) = &context {
                    assert_eq!(previous, &record.common.context);
                }
                context = Some(record.common.context);
                expected.insert(code);
            }
        }
        assert_eq!(expected.len(), count, "captured inventory for {persona}");
        let context = context.expect("operation inventory has an execution context");
        let actual = manifest
            .operations
            .iter()
            .filter(|(_, operation)| {
                operation.observed_scopes.iter().any(|scope| {
                    scope.connection == context.connection
                        && scope.mode == context.mode
                        && scope.state == context.state
                })
            })
            .map(|(code, _)| code.clone())
            .collect::<BTreeSet<_>>();
        assert_eq!(actual, expected, "positive inventory scope for {persona}");
        inventories.insert(persona, expected);
    }
    assert_eq!(inventories["usb-stills"], inventories["usb-video"]);
    assert_eq!(
        inventories["wireless-stills"],
        inventories["wireless-video"]
    );
    for persona in ["usb-cardreader", "usb-tether-auto"] {
        for code in ["0x101b", "0x9801", "0x9802", "0x9803", "0x9805"] {
            assert!(inventories[persona].contains(code), "{persona}: {code}");
        }
    }
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
