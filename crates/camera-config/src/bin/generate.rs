//! `camera-config-generate <evidence.jsonl>...` — concatenate `camera-config-evidence/v1`
//! files (protocol-mapper output) and emit a reviewable manifest **proposal** (YAML) to
//! stdout. A starting point for review + merge with curated sequences/establishment — NOT
//! a drop-in manifest. See docs/plans/camera-config.md and the generator's module docs.
use std::io::Read;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: camera-config-generate <evidence.jsonl>...");
        std::process::exit(2);
    }
    let mut jsonl = String::new();
    for path in &args {
        let mut s = String::new();
        if let Err(e) = std::fs::File::open(path).and_then(|mut f| f.read_to_string(&mut s)) {
            eprintln!("read {path}: {e}");
            std::process::exit(1);
        }
        jsonl.push_str(&s);
        jsonl.push('\n');
    }
    let proposal = camera_config::generate_proposal(&jsonl);
    match proposal.to_yaml() {
        Ok(y) => print!("{y}"),
        Err(e) => {
            eprintln!("serialize: {e}");
            std::process::exit(1);
        }
    }
}
