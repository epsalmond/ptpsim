#!/usr/bin/env python3
"""Generate the synthetic pushFrame observation fixture.

The fixture is packages/camera-config-data/observations/fixtures/positive/
push-frames-multi-stream.jsonl. It describes synthetic packets only: the bodies
are minimal structurally valid JPEGs built below, not camera images, and the
bundle names the made-up camera EXAMPLE PUSH CAMERA. Every hash in the fixture
is computed here from those bytes, so the fixture can be verified from the repo.

Regenerate after changing this script:

    python3 scripts/generate-push-frame-fixture.py

Verify that the committed fixture matches this script's output (CI runs this):

    python3 scripts/generate-push-frame-fixture.py --check

Output is deterministic: no clocks, randomness, or host paths are read.
"""

import hashlib
import json
import pathlib
import struct
import sys

FIXTURE = (
    pathlib.Path(__file__).resolve().parent.parent
    / "packages/camera-config-data/observations/fixtures/positive"
    / "push-frames-multi-stream.jsonl"
)
RUN = "fixture-push-frames"
EPISTEMIC = {"class": "syntheticFixture", "confidence": "exact", "alternatives": [], "unknowns": []}
# The shared live-view framing from crates/protocol-primitives/src/liveview.rs:
# an 18-byte little-endian header (u32 inclusive length, zero u32, u32 frame
# counter, zero u32 body offset adjustment, reserved u16) before the body.
FRAMING = "fuji-liveview-v1"
# Two connection instances, each with one live-view stream, and the first-read
# time in milliseconds of each frame on the synthetic "mono" clock.
STREAMS = [
    ("fixture-connection-a", [100, 116, 133]),
    ("fixture-connection-b", [500, 507, 525]),
]


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def jpeg(marker):
    """A 640x480, 8-bit, one-component JPEG skeleton with one varying scan byte."""
    return bytes(
        [0xFF, 0xD8, 0xFF, 0xC0, 0, 11, 8, 1, 224, 2, 128, 1, 1, 0x11, 0]
        + [0xFF, 0xDA, 0, 8, 1, 1, 0, 0, 63, 0, marker, 2, 3, 0xFF, 0xD9]
    )


def header_line():
    return {
        "kind": "bundleHeader",
        "schema": "camera-observation/v1",
        "runId": RUN,
        "recordId": "header",
        "ordinal": 0,
        "camera": {
            "manufacturer": "EXAMPLE",
            "model": "PUSH CAMERA",
            "bodyId": "body-push",
            "firmware": "1.0",
        },
        "client": {"artifact": "camera-initiator", "version": "fixture", "platform": "test"},
        "capture": {
            "interfaces": [{"id": "push", "interfaceType": "synthetic", "role": "push-stream"}],
            "clocks": [{"id": "mono", "clockType": "monotonic", "unit": "milliseconds"}],
            "clockMappings": [],
            "loss": {"droppedRecords": 0, "droppedBytes": 0, "truncatedPayloads": 0},
            "redactions": [],
            "toolVersions": {"camera-initiator": "fixture"},
            "artifacts": [],
        },
        "epistemic": EPISTEMIC,
    }


def frame_line(ordinal, stream_index, instance, sequence, first_read):
    body = jpeg(16 * stream_index + sequence + 1)
    envelope = struct.pack("<IIIIH", 18 + len(body), 0, sequence, 0, 0)
    last_read = first_read + 1
    return {
        "kind": "pushFrame",
        "schema": "camera-observation/v1",
        "runId": RUN,
        "recordId": f"frame-{'ab'[stream_index]}-{sequence}",
        "ordinal": ordinal,
        "context": {"connection": "app", "mode": "shooting/stills", "state": "pushFrame"},
        "time": {"clock": "mono", "value": last_read},
        "physicalContext": {},
        "artifactRanges": [],
        "epistemic": EPISTEMIC,
        "connectionInstance": instance,
        "stream": "liveView",
        "sequence": sequence,
        "frameCounter": sequence,
        "hostRead": {
            "firstRead": {"clock": "mono", "value": first_read},
            "lastRead": {"clock": "mono", "value": last_read},
        },
        "envelope": {
            "framing": FRAMING,
            "declaredLength": len(envelope) + len(body),
            "bodyOffset": len(envelope),
            "header": {
                "length": len(envelope),
                "sha256": sha256(envelope),
                "inlineHex": envelope.hex(),
                "streamRanges": [],
                "ranges": [],
            },
        },
        "body": {
            "mediaType": "image/jpeg",
            "payload": {
                "length": len(body),
                "sha256": sha256(body),
                "inlineHex": None,
                "streamRanges": [{"offset": 0, "length": len(body), "sha256": sha256(body)}],
                "ranges": [],
            },
            "image": {"width": 640, "height": 480, "precision": 8, "components": 1},
        },
    }


def render():
    lines = [header_line()]
    for stream_index, (instance, first_reads) in enumerate(STREAMS):
        for sequence, first_read in enumerate(first_reads):
            lines.append(frame_line(len(lines), stream_index, instance, sequence, first_read))
    return "".join(json.dumps(line, separators=(",", ":")) + "\n" for line in lines)


def main(argv):
    expected = render()
    if argv == ["--check"]:
        if FIXTURE.read_text(encoding="utf-8") != expected:
            print(f"{FIXTURE.name} differs from the generator output; regenerate it", file=sys.stderr)
            return 1
        print(f"{FIXTURE.name}: ok")
        return 0
    if argv:
        print("usage: generate-push-frame-fixture.py [--check]", file=sys.stderr)
        return 2
    FIXTURE.write_text(expected, encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
