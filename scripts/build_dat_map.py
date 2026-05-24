#!/usr/bin/env python3
"""Assemble the comprehensive GFX100 II camera-settings .dat map from all sources:
  1. a backup_sweep report.json  (offset <-> PTP DPC, drift-immune before/after diffs)
  2. body_diff_findings.tsv       (settings with no PTP prop, pinned by body-toggle diffs)
  3. PROPERTY_CATALOG.md          (DPC -> human name)
  4. BACKUP_SETTINGS_SCHEMA.tsv   (coarse structural regions / header)
Emits one offset-sorted markdown map. Props that set no .dat byte are listed as transient.

Usage: python scripts/build_dat_map.py <sweep_report.json> [--body TSV] [--out FILE]
"""
import argparse
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from label_backup import load_names  # noqa: E402

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

SCHEMA = os.path.expanduser("~/fuji/analysis/BACKUP_SETTINGS_SCHEMA.tsv")


def main(argv=None) -> int:
    p = argparse.ArgumentParser(prog="build-dat-map")
    p.add_argument("report")
    p.add_argument("--body", default=os.path.join(SETTINGS, "body_diff_findings.tsv"))
    p.add_argument("--out", default=None)
    args = p.parse_args(argv)

    names = load_names()
    rows = {}          # offset -> (len, source, label, change)
    transient = []
    for r in json.load(open(args.report)):
        prop = int(r["prop"], 16)
        if r.get("error"):
            continue
        if not r.get("diff_runs"):
            transient.append(prop)
            continue
        nm = names.get(prop, "(unnamed DPC)")
        for run in r["diff_runs"]:
            off = int(run["off"], 16)
            rows[off] = (run["len"], f"PTP 0x{prop:04x}", nm,
                         f"{r.get('old')}→{r.get('new')}")

    if os.path.exists(args.body):
        for line in open(args.body):
            parts = line.rstrip("\n").split("\t")
            if len(parts) >= 3 and parts[0].startswith("0x"):
                off = int(parts[0], 16)
                rows[off] = (1, "body-diff", parts[1], parts[2])


                                   "2026-05-23-backup-dat-FULL-map.md")
    lines = ["# GFX100 II camera-settings .dat — comprehensive label map",
             "",
             f"Auto-assembled from {os.path.basename(args.report)} + body-diff findings + "
             "PROPERTY_CATALOG names. Offset-sorted. `.dat` = 69500B `FUJIFILMX-BACKUP`.",
             "",
             f"**{len(rows)} byte offsets pinned to settings.**",
             "",
             "| offset | len | source | setting | change/encoding |",
             "|---|---|---|---|---|"]
    for off in sorted(rows):
        ln, src, label, change = rows[off]
        lines.append(f"| `0x{off:04X}` | {ln} | {src} | {label} | {change} |")
    if transient:
        lines += ["", f"**Writable PTP props with no .dat byte** ({len(transient)} — live-view/"
                  "transient, not persisted): " + ", ".join(f"`0x{t:04x}`" for t in transient)]

    # append coarse structural regions for orientation
    if os.path.exists(SCHEMA):
        lines += ["", "## Structural regions (from BACKUP_SETTINGS_SCHEMA.tsv)", "",
                  "| offset | type | name | notes |", "|---|---|---|---|"]
        for i, line in enumerate(open(SCHEMA)):
            if i == 0:
                continue
            c = line.rstrip("\n").split("\t")
            if len(c) >= 6:
                lines.append(f"| {c[0]} | {c[2]} | {c[3]} | {c[5][:80]} |")

    with open(out, "w") as f:
        f.write("\n".join(lines) + "\n")
    print(f"[map] {len(rows)} offsets -> {out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
