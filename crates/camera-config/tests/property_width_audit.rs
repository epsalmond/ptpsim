//! #53 — audit curated property `type:` widths against the probe-derived
//! GetDevicePropDesc descriptors (`evidence/probe/*.jsonl`, the wire-captured
//! ground truth). A curated width that disagrees with the probe corrupts
//! encode/parse for every consumer, so this pins them and guards future drift.
//!
//! Scope is **width and signedness**: the codec now models signed datatypes
//! (`i16`/`i32`), so a curated `u16` where the probe declares `i16` is a genuine
//! mismatch (#88) — not normalized away. Variable-width types (`u8a`, `str`,
//! `undef`) are one "non-fixed" class. The probe wins on a genuine
//! width/sign disagreement unless an `OVERRIDES` entry documents why the curated
//! value is correct.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use camera_config::CameraManifest;

/// Codes where the curated width intentionally differs from the probe
/// descriptor, with the wire evidence that justifies keeping the curated value.
const OVERRIDES: &[(&str, &str)] = &[
    // GetDevicePropDesc declares these `str`, but the wire-proven working write
    // is uint16 `00 00` (evidence/IMAGE_TRANSFER_FW230.md:138); the setProp
    // path at gfx100ii.yaml:97-98/124-125 encodes them as u16. The actual write
    // overrides the descriptor's declared datatype.
    (
        "0xd226",
        "wire-proven uint16 write despite str descriptor (IMAGE_TRANSFER_FW230.md)",
    ),
    (
        "0xd227",
        "wire-proven uint16 write despite str descriptor (IMAGE_TRANSFER_FW230.md)",
    ),
    // fw2.30 returns a degenerate single-value u16 stub descriptor for ISO/shutter,
    // but the real datatype is u32: 0xD02A literal-ISO / 0x80000000|ceiling and
    // 0xD240 0x80000000|denom*1000 (1/60 = 0x8000EA60). Proven by client application's UINT32
    // DevicePropDesc parser + the v6 ISO-write capture / FUJI_PTP_PROP_REFERENCE.md
    // [live] 0xD212 readback. The actual wire values override the stub descriptor (#100).
    (
        "0xd02a",
        "u32 literal/auto ISO despite degenerate u16 stub descriptor (client application UINT32 parser, v6 ISO writes)",
    ),
    (
        "0xd240",
        "u32 0x80000000|denom*1000 shutter despite degenerate u16 stub descriptor (client application UINT32 parser, live 0xD212)",
    ),
];

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum WidthClass {
    Fixed { bytes: u8, signed: bool }, // byte width + signedness
    Variable,                          // u8a / str / undef — non-fixed
}

/// Map a `type:` token to its width class, including signedness (the codec models
/// `i16`/`i32`, so signed and unsigned of the same width are distinct classes).
fn width_class(t: &str) -> Option<WidthClass> {
    let fixed = |bytes, signed| Some(WidthClass::Fixed { bytes, signed });
    match t.trim().to_ascii_lowercase().as_str() {
        "u8" => fixed(1, false),
        "i8" => fixed(1, true),
        "u16" => fixed(2, false),
        "i16" => fixed(2, true),
        "u32" => fixed(4, false),
        "i32" => fixed(4, true),
        "u64" => fixed(8, false),
        "i64" => fixed(8, true),
        "u8a" | "str" | "undef" | "" => Some(WidthClass::Variable),
        _ => None, // unknown token — surfaced as a failure below
    }
}

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../packages/camera-config-data")
}

/// Curated `code -> type` from the GFX100 II manifest (lowercased hex codes).
fn curated_types() -> BTreeMap<String, String> {
    let yaml = std::fs::read_to_string(data_dir().join("fuji/gfx100ii/gfx100ii.yaml")).unwrap();
    let m = CameraManifest::from_yaml(&yaml).expect("gfx100ii.yaml loads");
    m.properties
        .iter()
        .filter_map(|(code, p)| {
            p.ptype
                .as_ref()
                .map(|t| (code.to_ascii_lowercase(), t.clone()))
        })
        .collect()
}

/// Probe `code -> set(type)` across all `evidence/probe/*.jsonl` files.
fn probe_types() -> BTreeMap<String, BTreeSet<String>> {
    #[derive(serde::Deserialize)]
    struct Rec {
        kind: String,
        code: Option<String>,
        #[serde(rename = "type")]
        ptype: Option<String>,
    }
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let probe_dir = data_dir().join("fuji/gfx100ii/evidence/probe");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&probe_dir)
        .expect("probe dir present")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "expected probe evidence files in {probe_dir:?}"
    );
    for f in files {
        for line in std::fs::read_to_string(&f).unwrap().lines() {
            if line.trim().is_empty() {
                continue;
            }
            let r: Rec = serde_json::from_str(line).unwrap_or_else(|e| panic!("{f:?}: {e}"));
            if r.kind == "property" {
                if let (Some(code), Some(t)) = (r.code, r.ptype) {
                    out.entry(code.to_ascii_lowercase()).or_default().insert(t);
                }
            }
        }
    }
    out
}

#[test]
fn curated_property_widths_match_probe_descriptors() {
    let curated = curated_types();
    let probe = probe_types();
    let overrides: BTreeMap<&str, &str> = OVERRIDES.iter().copied().collect();

    let mut drift: Vec<String> = Vec::new();
    let mut overrides_hit: BTreeSet<String> = BTreeSet::new();

    for (code, ctype) in &curated {
        let Some(ptypes) = probe.get(code) else {
            continue; // no probe descriptor for this code — nothing to audit
        };
        let Some(cclass) = width_class(ctype) else {
            drift.push(format!(
                "{code}: curated type `{ctype}` is an unknown width token"
            ));
            continue;
        };
        // The curated width must match at least one probe descriptor's width.
        let pclasses: BTreeSet<Option<WidthClass>> =
            ptypes.iter().map(|t| width_class(t)).collect();
        if pclasses.contains(&Some(cclass)) {
            continue; // agrees
        }
        match overrides.get(code.as_str()) {
            Some(reason) => {
                overrides_hit.insert(code.clone());
                let _ = reason; // documented at the OVERRIDES table
            }
            None => drift.push(format!(
                "{code}: curated `{ctype}` ({cclass:?}) vs probe {ptypes:?} — probe wins; \
                 fix the curated width or add a documented OVERRIDES entry"
            )),
        }
    }

    // Keep the override table honest: a code that no longer drifts must be removed.
    let stale: Vec<&str> = overrides
        .keys()
        .copied()
        .filter(|c| !overrides_hit.contains(*c))
        .collect();

    assert!(
        drift.is_empty() && stale.is_empty(),
        "property width audit failed.\n\nDrift (probe descriptor wins):\n  {}\n\nStale overrides (no longer drift, remove them):\n  {}\n",
        if drift.is_empty() { "(none)".into() } else { drift.join("\n  ") },
        if stale.is_empty() { "(none)".to_string() } else { stale.join(", ") },
    );
}
