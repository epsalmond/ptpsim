#!/usr/bin/env python3
"""Parse a libgphoto2 config dump (e.g. camlibs/ptp2/cameras/fuji-gfx100-ii.txt) into
structured value-label facts — a CURATION AID for naming/labelling manifest properties.

Source: `gphoto2 --list-all-config` style dumps. These are facts (public, redistributable
mappings), used to seed human-readable names + value labels for camera-config properties.

IMPORTANT — how to use the output (it is NOT an auto-ingest by code):
  * `Choice: <index> <label>` — the <index> is gphoto2's OWN enum index, which is NOT
    guaranteed to equal our raw PTP wire value. Treat labels as candidates to reconcile
    against the probe's observed raw value set, not as direct value->label truth.
  * Named props live under semantic paths (/capturesettings/shutterspeed), not hex codes;
    only /main/other/<hex> carries a raw PTP code (usually unnamed). So mapping gphoto2's
    named props onto our hex codes is a human/curation step.
  * Units leak usefully here: e.g. /main/other/500d "Exposure Time" Current 9843 (= ~1/100s)
    confirms 0x500D is microseconds — handy for value-display transforms.

Output: JSONL, one property per line:
  {"path","label","readonly","type","current","hexCode"?,"choices":[{"index","label"}],
   "range":{"bottom","top","step"}?}
"""
import json
import re
import sys


def parse(text):
    """Yield one dict per property block."""
    prop = None
    hex_re = re.compile(r"/([0-9a-fA-F]{4})$")
    for raw in text.splitlines():
        line = raw.rstrip("\n")
        if line.startswith("/"):
            if prop:
                yield _finish(prop)
            prop = {"path": line, "choices": []}
            m = hex_re.search(line)
            if m:
                prop["hexCode"] = "0x" + m.group(1).lower()
        elif prop is None:
            continue
        elif line == "END":
            yield _finish(prop)
            prop = None
        elif ":" in line:
            key, _, val = line.partition(":")
            key, val = key.strip(), val.strip()
            if key == "Label":
                prop["label"] = val
            elif key == "Readonly":
                prop["readonly"] = val == "1"
            elif key == "Type":
                prop["type"] = val
            elif key == "Current":
                prop["current"] = val
            elif key == "Choice":
                idx, _, lbl = val.partition(" ")
                try:
                    prop["choices"].append({"index": int(idx), "label": lbl})
                except ValueError:
                    pass
            elif key in ("Bottom", "Top", "Step"):
                prop.setdefault("range", {})[key.lower()] = val
    if prop:
        yield _finish(prop)


def _finish(prop):
    if not prop["choices"]:
        prop.pop("choices", None)
    return prop


def main(argv):
    if len(argv) != 2:
        print("usage: gphoto2_labels.py <gphoto2-config-dump.txt>", file=sys.stderr)
        return 2
    with open(argv[1], encoding="utf-8", errors="replace") as f:
        text = f.read()
    for prop in parse(text):
        print(json.dumps(prop))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
