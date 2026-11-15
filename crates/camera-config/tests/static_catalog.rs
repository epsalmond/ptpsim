use std::collections::BTreeMap;

use camera_config::{
    annotate_static_candidates, apply_review, propose_static_candidates, CameraManifest,
    CandidateAssertion, PropertyKind, ProposalReview, ReviewDisposition,
    StaticCandidateDisposition, StaticCandidateDocument, REVIEW_SCHEMA,
};

fn document(plugin: &str) -> StaticCandidateDocument {
    let text = match plugin {
        "FF0018" => include_str!(
            "../../../packages/camera-config-data/fuji/catalog/tether-app-ff0018.candidates.json"
        ),
        "FF0021" => include_str!(
            "../../../packages/camera-config-data/fuji/catalog/tether-app-ff0021.candidates.json"
        ),
        _ => unreachable!(),
    };
    serde_json::from_str(text).unwrap()
}

fn base() -> CameraManifest {
    CameraManifest::from_yaml(
        r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: '2.30' }
properties:
  '0xd232': { name: raw_0xd232, type: u16, access: readOnly }
  '0xd20c': { name: simultaneousCardRecording, type: u16, access: readWrite }
"#,
    )
    .unwrap()
}

fn reviewed(doc: &StaticCandidateDocument) -> (camera_config::Proposal, ProposalReview) {
    let proposal = propose_static_candidates(doc).unwrap();
    let review = ProposalReview {
        schema: REVIEW_SCHEMA.into(),
        proposal_digest: proposal.digest.clone(),
        decisions: proposal
            .candidates
            .iter()
            .map(|row| (row.id.clone(), ReviewDisposition::Accept))
            .collect(),
    };
    (proposal, review)
}

#[test]
fn reduced_ledger_census_preserves_model_scope_and_full_code_identity() {
    for (plugin, vendor_count, total_count) in [("FF0018", 236, 246), ("FF0021", 282, 288)] {
        let doc = document(plugin);
        doc.validate().unwrap();
        let props = doc
            .candidates
            .iter()
            .filter(|row| row.kind == camera_config::StaticCandidateKind::Property)
            .collect::<Vec<_>>();
        assert_eq!(props.len(), total_count);
        assert_eq!(
            props
                .iter()
                .filter(|row| row.code.as_ref().unwrap().starts_with("0xd"))
                .count(),
            vendor_count
        );
        for (code, identity) in [
            ("0xd145", "PasswordInput"),
            ("0xd232", "AFCCustom"),
            ("0xd273", "MovieMonochromaticColor"),
        ] {
            assert!(props
                .iter()
                .find(|row| row.code.as_deref() == Some(code))
                .unwrap()
                .native_names
                .iter()
                .any(|name| name.contains(identity)));
        }
        if plugin == "FF0021" {
            assert!(doc.model.is_none());
            assert!(props
                .iter()
                .all(|row| row.disposition == StaticCandidateDisposition::UnknownModel));
        }
    }
}

#[test]
fn static_apply_adds_identity_without_fabricating_descriptor_or_write_claims() {
    let mut doc = document("FF0018");
    doc.candidates.retain(|row| {
        [Some("0xd15c"), Some("0xd232"), Some("0xd20c")].contains(&row.code.as_deref())
    });
    let (proposal, review) = reviewed(&doc);
    let manifest = apply_review(&base(), &proposal, &review).unwrap();
    let added = &manifest.properties["0xd15c"];
    assert_eq!(added.name, "videoMode");
    assert_eq!(added.kind, PropertyKind::CatalogOnly);
    assert!(added.ptype.is_none());
    assert!(added.access.is_none());
    assert!(added.initial_value.is_none());
    assert!(added.descriptor.is_none());
    assert!(added.controls.is_empty());
    assert!(added.labels.is_empty());
    assert!(added.value_rows.is_empty());
    assert!(added.value_profiles.is_empty());
    assert!(added.observed_scopes.is_empty());
    assert_eq!(manifest.properties["0xd232"].ptype.as_deref(), Some("u16"));
    assert_ne!(manifest.properties["0xd232"].name, "raw_0xd232");
    assert_eq!(
        manifest.properties["0xd20c"].name,
        "simultaneousCardRecording"
    );
    let yaml = annotate_static_candidates(&manifest, &manifest.to_yaml().unwrap());
    assert_eq!(yaml.matches("# unverified:").count(), 2);
    CameraManifest::from_yaml(&yaml).unwrap();
}

#[test]
fn digest_review_and_model_gate_prevent_unreviewed_or_wrong_body_changes() {
    let mut doc = document("FF0018");
    doc.candidates
        .retain(|row| row.code.as_deref() == Some("0xd15c"));
    let (mut proposal, review) = reviewed(&doc);
    if let CandidateAssertion::StaticCatalog { entry, .. } = &mut proposal.candidates[0].assertion {
        entry.catalog_name = Some("changedAfterReview".into());
    }
    assert!(apply_review(&base(), &proposal, &review).is_err());
    let (proposal, mut review) = reviewed(&doc);
    review.decisions = BTreeMap::new();
    assert!(apply_review(&base(), &proposal, &review).is_err());
    let (proposal, review) = reviewed(&doc);
    let mut wrong_body = base();
    wrong_body.camera.model = "X-T5".into();
    assert!(apply_review(&wrong_body, &proposal, &review).is_err());
    let (proposal, review) = reviewed(&document("FF0021"));
    let original = base();
    assert_eq!(
        apply_review(&original, &proposal, &review)
            .unwrap()
            .to_yaml()
            .unwrap(),
        original.to_yaml().unwrap()
    );
}

#[test]
fn invalid_code_scope_or_request_is_rejected_before_proposal() {
    let mut doc = document("FF0018");
    doc.candidates.truncate(1);
    doc.candidates[0].code = Some("0x17".into());
    assert!(propose_static_candidates(&doc).is_err());
    let mut doc = document("FF0018");
    doc.model = None;
    assert!(propose_static_candidates(&doc).is_err());
    let mut doc = document("FF0018");
    let request = doc
        .candidates
        .iter_mut()
        .find_map(|row| row.request.as_mut())
        .unwrap();
    request.params = vec![0; 6];
    assert!(propose_static_candidates(&doc).is_err());
}

#[test]
fn operation_and_workflow_dispositions_retain_request_boundaries() {
    let doc = document("FF0018");
    let row = |id: &str| doc.candidates.iter().find(|row| row.id == id).unwrap();
    assert_eq!(
        row("ff0018-operation-100e")
            .request
            .as_ref()
            .unwrap()
            .params,
        vec![0, 0]
    );
    assert!(row("ff0018-operation-9018")
        .request
        .as_ref()
        .unwrap()
        .params
        .is_empty());
    assert!(row("ff0018-operation-900c").request.is_none());
    assert!(row("ff0018-coverage-internal-wakeups").request.is_none());
    let unknown = document("FF0021");
    for code in ["0x9008", "0x901b"] {
        let row = unknown
            .candidates
            .iter()
            .find(|row| row.code.as_deref() == Some(code))
            .unwrap();
        assert_eq!(row.disposition, StaticCandidateDisposition::UnknownModel);
        assert!(row.request.is_none());
    }
}

#[test]
fn static_cli_proposal_review_apply_is_atomic_and_reproducible() {
    use std::process::Command;

    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("candidates.json");
    let proposal_path = directory.path().join("proposal.json");
    let review_path = directory.path().join("review.json");
    let base_path = directory.path().join("base.yaml");
    let output_path = directory.path().join("result.yaml");
    let mut doc = document("FF0018");
    doc.candidates
        .retain(|row| row.code.as_deref() == Some("0xd15c"));
    std::fs::write(&input, serde_json::to_vec(&doc).unwrap()).unwrap();
    std::fs::write(&base_path, base().to_yaml().unwrap()).unwrap();
    let binary = env!("CARGO_BIN_EXE_camera-config-generate");
    assert!(Command::new(binary)
        .args(["propose", "--static-candidates"])
        .arg(&input)
        .arg("--output")
        .arg(&proposal_path)
        .status()
        .unwrap()
        .success());
    let generated: camera_config::Proposal =
        serde_json::from_slice(&std::fs::read(&proposal_path).unwrap()).unwrap();
    let (expected, mut review) = reviewed(&doc);
    assert_eq!(generated, expected);
    std::fs::write(&review_path, serde_json::to_vec(&review).unwrap()).unwrap();
    let apply = || {
        Command::new(binary)
            .arg("apply")
            .arg("--base")
            .arg(&base_path)
            .arg("--proposal")
            .arg(&proposal_path)
            .arg("--review")
            .arg(&review_path)
            .arg("--output")
            .arg(&output_path)
            .output()
            .unwrap()
    };
    assert!(apply().status.success());
    let complete = std::fs::read(&output_path).unwrap();
    assert!(String::from_utf8_lossy(&complete).contains("# unverified:"));
    review.proposal_digest = "stale".into();
    std::fs::write(&review_path, serde_json::to_vec(&review).unwrap()).unwrap();
    assert!(!apply().status.success());
    assert_eq!(std::fs::read(&output_path).unwrap(), complete);

    let doc = document("FF0018");
    let expected = propose_static_candidates(&doc).unwrap();
    let mut reordered = doc;
    reordered.candidates.reverse();
    assert_eq!(expected, propose_static_candidates(&reordered).unwrap());
}
