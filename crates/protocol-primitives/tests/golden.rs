//! Golden-packet round-trip test. Loads the labeled fixtures under
//! `packages/protocol-spec/golden/` (extracted from real captures by
//! `tools/golden/extract_golden.py`) and asserts they decode to the documented
//! op and re-encode byte-for-byte. This makes the golden packets both
//! documentation and a regression guard on the framing codecs.

use std::path::PathBuf;

use ptp_core::{PtpCodec, PtpIpPacket};
use serde::Deserialize;

#[derive(Deserialize)]
struct Golden {
    label: String,
    framing: String,
    bytes_hex: String,
    decoded: Decoded,
}

#[derive(Deserialize)]
struct Decoded {
    op: Option<String>,
}

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../packages/protocol-spec/golden")
}

fn hex_to_bytes(s: &str) -> Vec<u8> {
    let clean: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    (0..clean.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&clean[i..i + 2], 16).unwrap())
        .collect()
}

fn op_of(pkt: &PtpIpPacket) -> Option<String> {
    match pkt {
        PtpIpPacket::OperationRequest(r) => Some(format!("0x{:04x}", r.code)),
        PtpIpPacket::OperationResponse(r) => Some(format!("0x{:04x}", r.code)),
        _ => None,
    }
}

#[test]
fn golden_packets_decode_and_round_trip() {
    let dir = golden_dir();
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("golden dir exists") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        let g: Golden =
            serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).expect("parse golden");
        let bytes = hex_to_bytes(&g.bytes_hex);

        let (pkt, reencoded) = match g.framing.as_str() {
            "fuji-compressed" => {
                let p = protocol_primitives::fuji_framing::decode(&bytes)
                    .unwrap_or_else(|e| panic!("{}: decode failed: {e}", g.label));
                let b = protocol_primitives::fuji_framing::encode(&p).unwrap();
                (p, b)
            }
            "ptpip-standard" => {
                let p = PtpIpPacket::decode(&bytes)
                    .unwrap_or_else(|e| panic!("{}: decode failed: {e}", g.label));
                (p.clone(), ptp_core::encode(&p).unwrap())
            }
            other => panic!("{}: unknown framing {other}", g.label),
        };

        // Documented op matches the decoded frame.
        if let Some(expected) = &g.decoded.op {
            assert_eq!(op_of(&pkt).as_deref(), Some(expected.as_str()), "{}: op mismatch", g.label);
        }
        // Re-encode is byte-identical to the captured bytes (the codec is faithful
        // to real wire data).
        assert_eq!(reencoded, bytes, "{}: re-encode must match captured bytes", g.label);
        checked += 1;
    }
    assert!(checked >= 1, "expected at least one golden packet in {}", dir.display());
}
