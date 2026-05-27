# Golden packet extraction

`extract_golden.py` is the **canonical** way to turn a capture into a public
fixture. It cherry-picks one frame, redacts it, and writes a small **labeled**
artifact under `packages/camera-config-data/golden/`. The label + decoded summary make
each golden self-documenting — you reference the golden, not the original pcap.

## The discipline (why this tool exists)

Raw captures never enter this (public) repo. They live outside it

golden packets are committed. A golden records its source by **name only** — not
the path, not the bytes of anything but the one frame it documents.

- **frida** blobs are already cherry-picked at runtime (one frame per blob), so
  extraction is just "decode and pick by op/type".
- **pcap** extraction *requires* `--host`: only the conversation with that single
  address is touched; nothing else in the file is read. The host is recorded in
  the golden's provenance.
- **raw** is a single `.bin` already holding one frame.

Redaction replaces known device-identifying byte sequences (the GFX100 II
GUID; add more with `--redact <hex>`) with equal-length zeros so frame offsets
are preserved. Always eyeball a new golden's `bytes_hex` for residual identifiers
before committing — especially `Init*` frames (GUIDs) and any data-phase frame
that carries a serial.

## Usage

```sh
# List the decodable frames in a frida blob dir, then pick one:


python3 tools/golden/extract_golden.py frida --blobs <dir> --select op:0x1002 \
    --label open-session-request --transport app --firmware 2.30 \
    --description "OpenSession on the reference app command channel."

# tcpdump/pcap — note the mandatory --host (only that address is copied):
python3 tools/golden/extract_golden.py pcap --file walk.pcapng --host 192.168.0.1 \
    --port 55740 --select op:0x1001 --label get-device-info-request --transport pcss

python3 tools/golden/extract_golden.py raw --file request.bin --label some-frame \
    --transport usb


# The bulk data transfer (a 100 MB+ RAW) is skipped — only the tiny op/response/
# event containers are touched.
python3 tools/golden/extract_golden.py usbscan --file xraw_capture.pcap
python3 tools/golden/extract_golden.py usbmon --file xraw_capture.pcap --select op:0x1001 \
    --label usb-get-device-info-request --transport usb
```

Framings understood: `ptpip-standard` (PTP/IP), `fuji-compressed` (reference app command
channel), `usb-ptp` (PIMA 15740 USB containers). The matching decode/encode lives
in `ptp-core` / `protocol-primitives`, and the golden round-trip test exercises
each.

## Golden packet format

```yaml
label: open-session-request
description: >-
  PTP/IP OpenSession on the reference app command channel (Fuji compressed framing), fw 2.30.
source:
  capture: <name only>        # never a path or the source bytes
  kind: frida                 # frida | pcap | raw
  address: null               # for pcap: the single host this was copied from
  selector: op:0x1002
  extracted: "2026-05-25"
transport: app
firmware: "2.30"
framing: fuji-compressed      # or ptpip-standard
decoded: { type: "OperationRequest", op: "0x1002", tid: 1 }
redactions: []
bytes_hex: "10000000010002100100000001000000"
```

## Verification

`crates/protocol-primitives/tests/golden.rs` loads every golden, decodes it with
the matching framing, asserts the decoded op equals `decoded.op`, and asserts a
re-encode is byte-identical to `bytes_hex`. So goldens are both documentation and
a regression guard that the codecs stay faithful to real wire data.
