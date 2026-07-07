//! `camera-config-generate [--enrich <base.yaml>] <evidence.jsonl>...`
//!
//! Concatenate `camera-config-evidence/v1` files (protocol-mapper output) and emit a
//! reviewable manifest **proposal** (YAML) to stdout. With `--enrich <base.yaml>`, the
//! proposal is merged INTO a curated base (curated structure wins; the probe adds
//! properties/operations and fills missing type/access/descriptor) — the
//! generator→merge→review step. Output is a first-pass for human reconciliation, never
//! a silent drop-in. See the generator module docs + docs/plans/camera-config.md.
use std::io::Read;

fn main() {
    let mut args = std::env::args().skip(1);
    let mut base: Option<String> = None;
    let mut evidence_paths: Vec<String> = Vec::new();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--enrich" => {
                base = Some(args.next().unwrap_or_else(|| {
                    eprintln!("--enrich requires a <base.yaml> path");
                    std::process::exit(2);
                }));
            }
            _ => evidence_paths.push(a),
        }
    }
    if evidence_paths.is_empty() {
        eprintln!("usage: camera-config-generate [--enrich <base.yaml>] <evidence.jsonl>...");
        std::process::exit(2);
    }

    let mut jsonl = String::new();
    for path in &evidence_paths {
        let mut s = String::new();
        if let Err(e) = std::fs::File::open(path).and_then(|mut f| f.read_to_string(&mut s)) {
            eprintln!("read {path}: {e}");
            std::process::exit(1);
        }
        jsonl.push_str(&s);
        jsonl.push('\n');
    }

    let proposal = camera_config::generate_proposal(&jsonl);
    let manifest = match base {
        Some(path) => {
            let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
                eprintln!("read {path}: {e}");
                std::process::exit(1);
            });
            let curated = camera_config::CameraManifest::from_yaml(&text).unwrap_or_else(|e| {
                eprintln!("parse {path}: {e}");
                std::process::exit(1);
            });
            camera_config::enrich(curated, proposal)
        }
        None => proposal,
    };

    match manifest.to_yaml() {
        Ok(y) => {
            print!("{HEADER}");
            print!("{y}");
        }
        Err(e) => {
            eprintln!("serialize: {e}");
            std::process::exit(1);
        }
    }
}

const HEADER: &str = "\
# GENERATED — do NOT hand-edit. The rich GFX100 II manifest the simulator loads
# (camera-sim-service --manifest …/gfx100ii.consolidated.yaml). Reproduce with:
#   cargo run -p camera-config --bin camera-config-generate -- \\
#     --enrich packages/camera-config-data/fuji/gfx100ii/gfx100ii.yaml \\
#     packages/camera-config-data/fuji/gfx100ii/evidence/probe/*.jsonl \\
#     packages/camera-config-data/fuji/gfx100ii/evidence/labels/*.jsonl \\
#     packages/camera-config-data/fuji/gfx100ii/evidence/value-profiles/*.jsonl
#
# = curated gfx100ii.yaml (connections/modes/entries/establishment + curated names,
#   labels, value profiles, gating) ENRICHED with active-probe evidence (props/descriptors/ops).
# Mode-naming convention applied; standard PTP codes auto-named. Remaining curation:
# ~306 vendor raw_0x props carry camera-sourced descriptors but still need names/labels.
";
