---
description: PTP/USB, XLV (HTTP/HTTPS), and wireless-tether transports as manifest-driven adapters over the one generic engine. The initiator-side USB model is specified in MANIFEST_SCHEMA §11.29; the USB responder, XLV, and wireless tether remain paper design.
status: plan
read-when: Planning or implementing a new transport, or checking the transport-adapter invariant before touching the engine/transport boundary.
---

# ptpsim Transport Designs

Covers PTP/USB, XLV (HTTP/HTTPS), and wireless tether, for the Fuji GFX100 II
first. These extend the design in [`../DESIGN.md`](../DESIGN.md); read its
"Transport And Mode Matrix" and "Architecture: generic engine" sections first.
NB: the YAML sketches below predate the `connections` schema that superseded
the draft `transports:` block (#162); on implementation, this material lands
as `connections.*` entries.

USB captures are in progress. The initiator-side USB schema has landed
([`MANIFEST_SCHEMA.md`](MANIFEST_SCHEMA.md) §11.29); the responder-side design
below remains the target the captures and manifests land against.

## The transport-adapter invariant

Every transport is an **adapter** that moves protocol units between a wire (or a
USB PHY) and the **one generic engine's shared state** (`CameraState`,
`camera-media-store`, the property engine). The rules that keep this from
becoming "vcam per transport":

1. **Descriptors, ports, security, and mode gating are manifest data**
   (`connections.*`), never per-firmware/per-brand code.
2. **The core is untouched.** `ptp-core`, the `camera-manifest` model shape, and
   `camera-sim`'s state model do not change to add a transport.
3. New code is allowed only for: a new **wire framing** (→ `ptp-core` /
   `protocol-primitives`), a new **protocol family** dispatcher (e.g. HTTP), or a
   **runtime capability** (TLS, a USB PHY). Each lands as a shared, concern-
   organized peer — never a per-model branch.
4. **Two dispatch shapes, one state model.** PTP transports call
   `engine.on_operation(req, data)`. XLV calls a parallel
   `engine.on_http(route, req)`. Both resolve behavior from the manifest and
   mutate the same `CameraState`. "Add a camera/firmware/transport variant" stays
   a data PR.

Illustrative boundary (names not final):

```rust
/// Each transport owns its socket/PHY and pumps protocol units into the shared
/// engine. The engine is `Arc<Mutex<Engine>>`, exactly as the service wires the
/// command/liveview/event sockets today.
pub trait TransportAdapter {
    async fn run(self, engine: Arc<Mutex<Engine>>, shutdown: Shutdown);
}
```

---

## 1. PTP/USB transport

USB is a single link carrying mutually-exclusive **modes**. PTP containers ride
bulk IN/OUT; events ride an interrupt IN endpoint (USB Still-Image-Capture
class). The container payloads are the **same `ptp-core` types** already used by
PTP/IP; only the framing/transport differ, so the engine is unchanged.

### Initiator (implemented)

The initiator-side model is manifest schema, specified in
[`MANIFEST_SCHEMA.md`](MANIFEST_SCHEMA.md) §11.29. Two connection kinds:

- `usb` (raw): the initiator owns the device, interface claim, endpoints,
  session, and transaction ids. Establishment plans live in
  `families.<fam>.usb.establishments` and use the USB verbs (`usbClaim`,
  `usbBulkOut`, `usbBulkIn`, `usbAwaitInterrupt`). The host implements the
  foreign `UsbExecutorTransport` trait; the executor entry point is
  `run_usb_establishment`.
- `usb-passthrough`: a platform daemon owns framing, session, and transaction
  ids. The host speaks typed transactions through the foreign
  `PtpTransactionTransport` trait; mode entries and actions run through
  `run_mode_entry_txn` and `run_initiator_action_txn`.

Per-connection trait fields (`session.ownership`, `events.delivery`) tell the
consumer which behavior to select, following the #81 pattern. Platform
availability of each kind stays manifest data through `connections(platform)`.

### Responder (paper: simulating USB to a host)

A normal machine is a USB host; presenting a *device* needs a PHY:

- **Preferred:** Linux USB gadget on an SBC (Pi Zero 2 W / Pi 4 in peripheral
  mode) via **FunctionFS**: the PTP responder runs in userspace and bridges bulk
  transfers to `camera-sim`. ~$15, all logic stays in our software.
- **Alternative:** Facedancer-class hardware (Cynthion / GreatFET): device logic
  on a full PC, board is just the PHY.
- The macOS app compiles as a macOS host app to get USB3 host control/support
  "for free"; that is the **host/probe** side, not the responder.

The adapter is a `UsbResponder` that reads a bulk frame → decodes (PTP-over-USB
container framing, a `protocol-primitives` entry) → `engine.on_operation` →
encodes the reply → bulk write. Same `Reply` enum as every other transport.
The responder side remains paper design.

### Modes

`usb/image`, `usb/raw-conversion`, `usb/backup-restore`, `usb/webcam`. Each is a
`(transport, mode)` availability set in the manifest; the documented cross-mode
bleed (ops legal in a mode where they "make no sense") is captured as data, not
special-cased.

### Responder descriptor data (paper)

```yaml
transports:
  usb:
    kind: ptpip-usb        # PTP containers over USB bulk
    descriptor:
      idVendor: "0x04cb"   # FUJIFILM
      idProduct: "0x0000"  # from capture
      bcdDevice: "0x0001"
      manufacturer: FUJIFILM
      product: GFX100 II
      serialPattern: "xxxxxxxxxx"
      class: still-image-capture
      endpoints: { bulkIn: 0x81, bulkOut: 0x02, interruptIn: 0x83 }
    modes:
      image:          { operations: [/* … */] }
      raw-conversion: { operations: [/* … */], note: "allows some image ops too (bleed)" }
      backup-restore: { operations: [/* … */] }
      webcam:         { operations: [/* … */] }
```

This descriptor block is what the gadget presents to a host. On implementation
it lands as `connections.*` and family `usb` data per §11.29.

### Probe

`camera-probe` over USB is an ordinary host (libusb/nusb); **no PHY hardware
needed** to learn USB behavior. Plans keyed `fuji/usb/<mode>/…` capture each
mode's operation set + cross-mode bleed into bundles → manifest. (This is what
the in-progress USB captures feed.)

---

## 2. XLV / HTTP(S) transport

XLV is the camera's HTTP web service — a **second protocol family**: HTTP routes
→ handlers (the HTTP analog of the PTP op table), with live view delivered as
chunked / `multipart/x-mixed-replace` MJPEG. It is a **sibling dispatcher that
shares engine state**, not a separate camera; reads/writes hit the same property
engine and media store. XLV is plausibly where the live-view-size properties
that no-op over PTP become real.

### Firmware-keyed transport security (the headline)

- fw **2.30**: XLV over plain **HTTP**.
- fw **2.40**: XLV requires **HTTPS**, and the client must **trust the camera's
  self-signed cert**.

This is pure manifest data — a `tls:` block that differs between the two firmware
manifests. No code knows about the firmware difference.

### Manifest block

```yaml
transports:
  xlv:
    kind: http-xlv
    bind: { http: 80 }            # or 443 when tls is on
    tls:
      mode: none                  # fw 2.30
      # fw 2.40 manifest instead:
      # mode: self-signed
      # cert: minted-per-instance # exposed so app/probe can trust/pin it
    routes:
      "GET /info":          { handler: device.info }
      "GET /liveview":      { handler: liveview.mjpeg }     # chunked/multipart
      "GET /props/{code}":  { handler: property.get }
      "PUT /props/{code}":  { handler: property.set }
```

### Runtime impact

- An **HTTP router** + handler set in the service. The existing control HTTP is
  hand-rolled and minimal; XLV likely warrants a small real router (routes,
  path params, chunked streaming) — still framework-light.
- **TLS termination** via `rustls`; the self-signed cert is minted per instance
  and surfaced (e.g. a `/cert` control endpoint or a file) so the app/probe can
  trust it. The "client must trust the cert" is the *client's* job; the simulator
  just presents one.
- `engine.on_http(route, req) -> HttpReply` — parallel to `on_operation`, over
  the same `CameraState`.

### Probe

`camera-probe` gains an `xlv`/`http` plan and a **trust-self-signed-cert** knob
so it can talk to a real fw2.40 camera (authorized probing of your own device)
and capture routes/props into bundles.

---

## 3. Wireless tether transport

Likely **PTP/IP-shaped** (confirm via probe) — i.e. another transport feeding the
**existing PTP engine**, which makes it the cheapest of the three. If it uses a
knock/announce establishment (like PCSS), that establishment is a
`protocol-primitives` strategy referenced by the manifest; the command session
itself is ordinary PTP/IP.

```yaml
transports:
  wirelessTether:
    kind: ptpip-tether
    bind: { command: 15740 }
    establishment: tether-knock-v1   # protocol-primitives strategy id, if needed
    status: planned
```

If the probe shows it is *not* PTP/IP-shaped, it becomes a third protocol family
like XLV; assume PTP/IP until a capture says otherwise.

---

## Scorecard (honest impact)

| Transport | New core? | New runtime? | New data? |
|---|---|---|---|
| PTP/USB | none | USB-bulk framing primitive + a PHY adapter (gadget/Facedancer) | descriptors + modes |
| XLV/HTTP | none | HTTP router + `rustls` + `on_http` dispatch | routes + firmware-keyed `tls` |
| Wireless tether | none | maybe an establishment strategy | transport block (cheapest) |

What stays invariant: `ptp-core`, the `camera-manifest` model shape, and
`camera-sim`'s state model. Schema additions (`kind: ptpip-usb|http-xlv|ptpip-tether`,
`tls`, `modes`) are all optional fields — `camera-manifest/v1` stays compatible.

## Build order when these land

1. Probe each transport into bundles (USB needs no PHY; XLV-2.40 needs the cert
   trust knob) → reviewed manifests under `packages/protocol-spec`.
2. Wireless tether first if PTP/IP-shaped (reuses the engine; smallest runtime).
3. XLV next: HTTP router + `rustls` + `on_http`; high value (web control surface,
   possibly the real live-view-size props).
4. USB last on the responder side (needs a PHY); USB *probing* can land anytime.
