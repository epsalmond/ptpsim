---
event: ACTION_RECEIPT
issuer: ptpsim (manifest/engine)
ts: 2026-06-02
status: INGESTED

related_consult: 2026-06-02-client application-request-to-ptpsim-wireless-tether.md
landed_in: "PR <TBD> (this branch)"
durable_facts_lifted_into:
  - packages/camera-config-data/fuji/gfx100ii/gfx100ii.yaml  # ops table, properties, wireless-tether connection block
  - crates/camera-config/src/model.rs                         # Property.kind tag (scaffold marker)
deferred_to:
  - docs/plans/action-verbs.md   # capture + transfer verb-shape decision (option 1/2/3)
---

# 2026-06-02 — ptpsim ingest receipt: D3-wire shoot-download answers

Internal action record for the second D3-wire reply on the wireless-tether
consult (Q1 capture verb + Q2 image-transfer + ISO encoding correction +
0xD039 scaffolding finding). The reply itself lives in the wire repo per the
cross-repo convention in [`README.md`](README.md); this doc records what
ptpsim did with it.

## What landed (data-only, this branch)

### Operations table — `wireless-tether` authorization
- **`0x1007 GetObjectHandles`** (new): `modes: [image-transfer], connections: [wireless-tether]`.
- **`0x1008 GetObjectInfo`**: connections now `[app, usb, wireless-tether]`.
- **`0x1009`**: renamed `GetBackupSettings` → `GetObject` (standard PTP), gated
  to `modes: [image-transfer, raw-conv-backup-restore], connections: [usb,
  wireless-tether]`. Mode-specific payload semantics (image vs backup data)
  noted inline.
- **`0x100a GetThumb`**: connections now `[app, wireless-tether]`.
- **`0x100b DeleteObject`** (new): `modes: [image-transfer], connections: [wireless-tether]`.

### Properties — PCSS ISO + scaffolding markers
- **`0x500f`** (ExposureIndex, the real PCSS ISO): comment clarifies signed
  -1/-2 sentinel encoding (Hyper-Utility wire trace). `0xd02a` (the reference app ISO
  via the `0x80xxxxxx` auto-prefix form) remains `app`-only — they are
  separate prop codes for the same logical setting, gated by transport.
- **`0xd039`**, **`0xd21c`**, **`0xd207`** (new): tagged `kind: scaffold` —
  RW-accepts arbitrary writes but semantic is wire-protocol mechanics
  (virtual-shutter state machine, tethered keepalives), NOT user-facing
  settings. Clients filter on `kind == "scaffold"` to keep them out of
  settings UI.

### Connection block — `wireless-tether`
- **Negative-facts comment updated**: explicitly lists what's wire-confirmed
  SUPPORTED (capture cycle, transfer triad, ISO encoding) vs still NOT
  supported (0x9022, 0x101b, 0xDF01, 0x9050/0x9053/0x9054/0x9055, event
  socket) vs still PENDING (`0xD037` flip transition).
- **`initRetries: { max: 3, backoffMs: 250 }`** added to the knock block —
  Hyper-Utility wire trace shows 0-3 InitFail packets before InitCmdAck.

### Schema — `crates/camera-config/src/model.rs`
- `Property.kind: Option<String>` added with `skip_serializing_if =
  "Option::is_none"` (keeps existing properties' YAML output unchanged).
  Generator updated to set `kind: None` on probe-discovered props.

### Tests
- Extended `wireless_tether_is_wire_confirmed_and_uses_absolute_big3` to
  assert the new ops authorization + the 0x500F-not-0xD02A ISO gating + the
  absence of 0x101b on PCSS.
- Added `scaffold_props_are_tagged_so_clients_can_filter_them_out_of_settings_ui`.

## What's deferred — and to where

The **capture verb** (3-beat `setProp 0xD039` + `0x100E InitiateCapture` cycle)
and the **transfer verbs** (`enumerate / info / getObject / delete`, the last
three parameterized by a runtime `handle`) cannot land as `entries[]` today:
mode-transition entries don't model intra-mode parameterized actions.

The schema-shape decision (pseudo-mode vs verb-on-entry vs new `actions:`
block) is captured in [`docs/plans/action-verbs.md`](../plans/action-verbs.md).
Until that lands, client application will need to bridge the capture sequence in Swift
as a flagged TODO (per the same pattern as `reopenSession`).

## What's still open from the wire side

Lifted verbatim from D3's reply §"What's left open":
1. **Q3** (`0xD037` stills↔video flip transition) — needs a capture with an
   actual flip; this session was stills-only.
2. **`0xD171 / 0xD01B / 0xD026`** semantic decode — low priority; treat as
   opaque pass-through (not surfaced as settings).
3. **Live-view stream content** in the 10,707 `0x9018` responses — orthogonal
   to this consult; LV frame decode work.
4. **`0xA002` vendor errors** (5×) — non-blocking; look if we hit them.

— ptpsim (manifest/engine)
