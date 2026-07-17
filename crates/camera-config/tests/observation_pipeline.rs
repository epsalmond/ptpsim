use std::path::{Path, PathBuf};

use camera_config::{
    generated_json_schema, proposal_json, propose, validate_bundles, CandidateAssertion,
};

fn data(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(path),
    )
    .unwrap()
}

fn fixture_files(kind: &str) -> Vec<PathBuf> {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data/observations/fixtures")
        .join(kind);
    let mut files = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn header_and_capability(input: &str, code: &str) -> String {
    input
        .lines()
        .enumerate()
        .filter(|(index, line)| {
            *index == 0
                || serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("subject")?
                            .get("code")?
                            .as_str()
                            .map(|candidate| candidate == code)
                    })
                    .unwrap_or(false)
        })
        .map(|(_, line)| line)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn positive_fixtures_are_completely_accounted() {
    for path in fixture_files("positive") {
        let input = std::fs::read_to_string(&path).unwrap();
        let expected = input.lines().filter(|line| !line.trim().is_empty()).count();
        let validated = validate_bundles(&[&input])
            .unwrap_or_else(|report| panic!("{} rejected: {report:?}", path.display()));
        assert_eq!(
            validated.report.total_nonblank,
            expected,
            "{}",
            path.display()
        );
        assert_eq!(validated.report.accepted, expected, "{}", path.display());
        assert_eq!(validated.report.rejected, 0, "{}", path.display());
        assert_eq!(
            validated.report.dispositions.len(),
            expected,
            "{}",
            path.display()
        );
    }
}

#[test]
fn every_negative_fixture_blocks_proposal_generation() {
    for path in fixture_files("negative") {
        let input = std::fs::read_to_string(&path).unwrap();
        let expected = input.lines().filter(|line| !line.trim().is_empty()).count();
        let report = validate_bundles(&[&input])
            .expect_err(&format!("{} unexpectedly validated", path.display()));
        assert_eq!(report.total_nonblank, expected, "{}", path.display());
        assert_eq!(
            report.accepted + report.rejected,
            expected,
            "{}",
            path.display()
        );
        assert!(report.rejected > 0, "{}", path.display());
        assert!(propose(&[&input]).is_err(), "{}", path.display());
    }
}

#[test]
fn checked_in_schema_is_generated_from_the_rust_model() {
    let committed = data("packages/camera-config-data/camera-observation-v1.schema.json");
    assert_eq!(committed, generated_json_schema().unwrap());
}

#[test]
fn proposal_bytes_ignore_bundle_and_record_order() {
    let inputs = fixture_files("positive")
        .into_iter()
        .map(|path| std::fs::read_to_string(path).unwrap())
        .collect::<Vec<_>>();
    let refs = inputs.iter().map(String::as_str).collect::<Vec<_>>();
    let forward = proposal_json(&propose(&refs).unwrap()).unwrap();
    let proposal = propose(&refs).unwrap();
    let expected_records = inputs
        .iter()
        .map(|input| input.lines().filter(|line| !line.trim().is_empty()).count())
        .sum::<usize>();
    assert_eq!(proposal.record_dispositions.len(), expected_records);
    assert!(proposal.candidates.iter().all(|candidate| {
        candidate.source_records.iter().all(|source| {
            proposal.record_dispositions.iter().any(|record| {
                record.identity == *source && record.candidate_ids.contains(&candidate.id)
            })
        })
    }));

    let mut reversed_inputs = inputs
        .iter()
        .map(|input| {
            let mut lines = input.lines().collect::<Vec<_>>();
            let header = lines.remove(0);
            lines.reverse();
            std::iter::once(header)
                .chain(lines)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect::<Vec<_>>();
    reversed_inputs.reverse();
    let refs = reversed_inputs
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let reversed = proposal_json(&propose(&refs).unwrap()).unwrap();
    assert_eq!(forward, reversed);
}

#[test]
fn proposal_preserves_scope_tuples_without_a_cartesian_product() {
    let first = header_and_capability(
        &data(
            "packages/camera-config-data/fuji/gfx100ii/evidence/probe/2026-05-27-ptp-evidence-usb-stills.jsonl",
        ),
        "0x1014",
    );
    let second = header_and_capability(
        &data(
            "packages/camera-config-data/fuji/gfx100ii/evidence/probe/2026-05-27-ptp-evidence-wireless-video.jsonl",
        ),
        "0x1014",
    );
    let proposal = propose(&[&first, &second]).unwrap();
    let scopes = proposal
        .candidates
        .iter()
        .find_map(|candidate| match &candidate.assertion {
            CandidateAssertion::Operation { code, scopes, .. } if code == "0x1014" => Some(scopes),
            _ => None,
        })
        .expect("0x1014 candidate");
    assert!(scopes
        .iter()
        .any(|scope| { scope.connection == "usb" && scope.mode == "shooting/stills" }));
    assert!(scopes
        .iter()
        .any(|scope| { scope.connection == "wireless-tether" && scope.mode == "shooting/video" }));
    assert!(!scopes
        .iter()
        .any(|scope| { scope.connection == "usb" && scope.mode == "shooting/video" }));
    assert!(!scopes
        .iter()
        .any(|scope| { scope.connection == "wireless-tether" && scope.mode == "shooting/stills" }));
}
