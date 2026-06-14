# GFX100 II fw0230 — protocol evidence

Provenance documents backing `../fw0230.yaml`. These are **evidence, not a
load-time dependency** (per `DESIGN.md`: "a manifest must validate against
evidence ... so a public manifest is self-justifying"). The manifest generator
and golden tooling may reference them; the simulator does not load them at runtime.

These docs were relocated here from the client application app repo
(`client application/apps/apple/docs/`) during the 2026-05-26 app-vs-protocol docs split:
protocol/opcode reference material does not belong in the app, and ptpsim is the
home for the spec/manifest side. The ptpsim agent decides how much of this gets
distilled into `fw0230.yaml` (or a richer manifest schema).

## Contents

| File | What it is |
|---|---|
| `PTP_PROPERTIES_REFERENCE.md` | Consolidated PTP-IP reference for GFX100 II fw 2.30 — property/DPC codes, F-number/shutter/ISO encodings, the `0xD212` live-view composite bundle, live-view stream-config DPCs (§3.9/§4.1.1), the still-capture sequence (§6), and the image-enumeration sequence (§8/§11). Reconciled 2026-05-26 with the app's latest readings (one-open-capture-session model; aperture direct-set ACK-but-ignored; corrected `0x902C/D/E` step opcodes; `0x9054/0x9055/0x9050/0x9053 → 0xD620/0xD621` enumeration). Mobile Wi-Fi PTP/IP. |

| `IMAGE_TRANSFER_FW0230.md` | Image-import (download) wire protocol for GFX100 II fw 2.30: mode-20 setup, `GetObjectInfo`/`GetThumb` enumeration, `GetPartialObject` 12 MB-chunk download, the `0xD620`/`0xD621` handle-list path, per-format (RAF/JPEG/MOV) ObjectInfo decode (1441-sample tally), and the fresh-session reconnect/sentinel choreography. Mobile Wi-Fi PTP/IP. The app-side reconnect contract is mirrored in the client application repo at `docs/IMAGE_IMPORT_RECONNECT.md`; narrative twin on nas `mobile/docs/captures/`. |

## Note for the ptpsim agent

`../fw0230.yaml`'s `evidence:` block currently points at two app-repo paths that
**no longer exist** there (relocated 2026-05-26):



Consider repointing those at this `evidence/` directory (for committed material)
or at the operator tree (for the discovery narrative). The discovery narrative


