#!/usr/bin/env python3
"""Map which bytes of the camera's 69500-byte BackupSettings (.dat) image hold which
setting, by diffing dumps taken at different settings.

`diff A.dat B.dat [label]` — print the byte offsets that differ (grouped into runs),
with old/new values, so a setting changed between A and B localizes to those offsets.

The .dat layout (prior RE): 168-byte header + body; a checksum near 0x00E8 changes on
every save (expected noise); Wi-Fi band byte at 0x052D. Use `--ignore-known` to suppress
the checksum/known-noise offsets and surface only the setting-specific delta.
"""
from __future__ import annotations

import argparse
from pathlib import Path

KNOWN_NOISE = {  # offsets that change on every save regardless of setting
    range(0x00E8, 0x00EC): "checksum/save-counter (LSB region)",
}


def _noise_label(off: int) -> str | None:
    for r, name in KNOWN_NOISE.items():
        if off in r:
            return name
    return None


def diff(a: Path, b: Path, ignore_known: bool = False) -> list[dict]:
    ba, bb = a.read_bytes(), b.read_bytes()
    n = min(len(ba), len(bb))
    diffs = [(i, ba[i], bb[i]) for i in range(n) if ba[i] != bb[i]]
    if len(ba) != len(bb):
        diffs.append(("len", len(ba), len(bb)))
    # group consecutive offsets into runs
    runs: list[dict] = []
    cur: list[tuple] = []
    for d in diffs:
        if d[0] == "len":
            continue
        if cur and d[0] == cur[-1][0] + 1:
            cur.append(d)
        else:
            if cur:
                runs.append(_run(cur))
            cur = [d]
    if cur:
        runs.append(_run(cur))
    if ignore_known:
        runs = [r for r in runs if not all(_noise_label(o) for o in range(r["off"], r["off"] + r["len"]))]
    return runs


def _run(group: list[tuple]) -> dict:
    off = group[0][0]
    old = bytes(g[1] for g in group)
    new = bytes(g[2] for g in group)
    return {"off": off, "off_hex": f"0x{off:04X}", "len": len(group),
            "old": old.hex(), "new": new.hex(),
            "old_le": int.from_bytes(old, "little"), "new_le": int.from_bytes(new, "little"),
            "note": _noise_label(off) or ""}


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(prog="settings_map")
    sub = p.add_subparsers(dest="cmd", required=True)
    d = sub.add_parser("diff")
    d.add_argument("a", type=Path)
    d.add_argument("b", type=Path)
    d.add_argument("label", nargs="?", default="")
    d.add_argument("--ignore-known", action="store_true")
    args = p.parse_args(argv)
    if args.cmd == "diff":
        runs = diff(args.a, args.b, args.ignore_known)
        lbl = f" [{args.label}]" if args.label else ""
        print(f"# {args.a.name} -> {args.b.name}{lbl}: {len(runs)} differing run(s)")
        for r in runs:
            note = f"  ({r['note']})" if r["note"] else ""
            print(f"  {r['off_hex']} len={r['len']}  {r['old']} -> {r['new']}"
                  f"  (LE {r['old_le']} -> {r['new_le']}){note}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
