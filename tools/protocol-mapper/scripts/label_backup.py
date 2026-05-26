#!/usr/bin/env python3
"""Join a backup_sweep report (offset <-> PTP prop) with the PROPERTY_CATALOG names to
produce a labeled byte map of the camera-settings .dat, sorted by offset.

Usage:
  python scripts/label_backup.py <sweep_report.json> [--catalog PROPERTY_CATALOG.md]
Emits a markdown table (offset | len | prop | name | observed change) on stdout.
"""
import argparse
import json
import os
import re

CAT_DEFAULT = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                           "PROPERTY_CATALOG.md")
ROW = re.compile(r"^\|\s*`(0x[0-9a-fA-F]{4})`\s*\|\s*([^|]+?)\s*\|")


def load_names(catalog=CAT_DEFAULT):
    names = {}
    if not os.path.exists(catalog):
        return names
    for line in open(catalog):
        m = ROW.match(line)
        if m:
            code = int(m.group(1), 16)
            name = m.group(2).replace("`", "").replace("**", "").strip()
            names[code] = name
    return names


def main(argv=None) -> int:
    p = argparse.ArgumentParser(prog="label-backup")
    p.add_argument("report")
    p.add_argument("--catalog", default=CAT_DEFAULT)
    args = p.parse_args(argv)

    names = load_names(args.catalog)
    recs = json.load(open(args.report))

    rows = []          # (offset, len, prop, name, change)
    no_delta, errored = [], []
    for r in recs:
        prop = int(r["prop"], 16)
        name = names.get(prop, "—")
        if r.get("error"):
            errored.append((prop, r["error"]))
            continue
        if not r.get("diff_runs"):
            no_delta.append(prop)
            continue
        for run in r["diff_runs"]:
            off = int(run["off"], 16)
            rows.append((off, run["len"], prop, name,
                         f"{r.get('old')}→{r.get('new')} ({run['old']}→{run['new']})"))
    rows.sort()

    print(f"# Labeled camera-settings .dat byte map — {len(rows)} offsets from "
          f"{len(recs)} swept props\n")
    print("| offset | len | PTP prop | setting | observed change |")
    print("|---|---|---|---|---|")
    for off, ln, prop, name, change in rows:
        print(f"| 0x{off:04X} | {ln} | `0x{prop:04x}` | {name} | {change} |")
    if no_delta:
        print(f"\n**Writable but no .dat delta** ({len(no_delta)}; live-view/transient, "
              f"not persisted): " + ", ".join(f"`0x{c:04x}`" for c in no_delta))
    if errored:
        print(f"\n**Desynced/skipped** ({len(errored)}): "
              + ", ".join(f"`0x{c:04x}`" for c, _ in errored))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
