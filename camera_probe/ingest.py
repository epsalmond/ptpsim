"""Ingest prior captures into observation-bundle facts (the seam), without re-probing.

DESIGN lists "capture import from traces" as a camera-probe responsibility. v1 ingests the XLV
HTTP cap/get sweep CSVs (code,status,body_len,body_snippet where body_snippet is base64 JSON
{property_code_value_list:[{property_code,value}], processing_result}) into bundle facts on the
`http`/xlv transport — so April's ad-hoc XLV sweeps become part of the bundle without touching a camera.

Usage: python3 -m camera_probe.ingest xlv-sweep <sweep.csv> --out bundle.jsonl [--names <catalog.md>]
"""
from __future__ import annotations

import argparse
import base64
import json
import os
import re
import sys

from . import bundle

_REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def _b64json(s: str):
    return json.loads(base64.b64decode(s + "=" * (-len(s) % 4)))


def load_names(catalog: str) -> dict:
    names: dict[int, str] = {}
    if catalog and os.path.exists(catalog):
        for line in open(catalog, errors="ignore"):
            m = re.match(r"\|\s*`?0x([0-9A-Fa-f]{4})`?\s*\|\s*([^|]+?)\s*\|", line)
            if m:
                names.setdefault(int(m.group(1), 16), m.group(2).strip()[:48])
    return names


def ingest_xlv_sweep(csv_path: str, out: str, catalog: str) -> int:
    names = load_names(catalog)
    bundle.set_context(transport="http", mode="xlv", state="cap-sweep",
                       evidence=os.path.basename(csv_path))
    bundle.open_bundle(out)
    n_resp = 0
    for line in open(csv_path, errors="ignore"):
        p = line.rstrip("\n").split(",", 3)
        if len(p) < 4 or p[1] != "200":
            continue
        code = int(p[0], 16)
        try:
            body = _b64json(p[3])
            pv = body.get("property_code_value_list", [])
            val = pv[0]["value"] if pv else None
        except Exception:
            val = None
        # represent the XLV /get as a GetDevicePropValue fact (op 0x1015, prop=code)
        data = json.dumps({"value": val, "name": names.get(code, "(unnamed)")}).encode()
        bundle.observe(0x1015, [code], data, 0x2001)
        n_resp += 1
    facts = bundle._SINK.count if bundle._SINK else 0
    bundle.close_bundle()
    print(f"[ingest] {n_resp} XLV responders -> {facts} facts -> {out}")
    return 0


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(prog="camera-probe ingest")
    sub = ap.add_subparsers(dest="kind", required=True)
    x = sub.add_parser("xlv-sweep", help="ingest an XLV cap/get sweep CSV")
    x.add_argument("csv")
    x.add_argument("--out", required=True)
    x.add_argument("--names", default=os.path.join(_REPO, "PROPERTY_CATALOG.md"))
    args = ap.parse_args(argv)
    if args.kind == "xlv-sweep":
        return ingest_xlv_sweep(args.csv, args.out, args.names)
    return 2


if __name__ == "__main__":
    sys.exit(main())
