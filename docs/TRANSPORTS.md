---
description: PTP/USB, XLV (HTTP/HTTPS), wireless-tether, and BLE socket transports as manifest-driven adapters over the one generic engine. The initiator-side USB model is specified in MANIFEST_SCHEMA §11.29 and the BLE socket listener is implemented; the USB responder, XLV, and wireless tether remain paper design.
status: plan
read-when: Planning or implementing a new transport, or checking the transport-adapter invariant before touching the engine/transport boundary.
---

# ptpsim Transport Designs

Covers PTP/USB, XLV (HTTP/HTTPS), wireless tether, and the BLE socket, for the
Fuji GFX100 II first. These extend the design in [`../DESIGN.md`](../DESIGN.md); read its
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
   organized peer, never a per-model branch.
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
USB attachment callbacks enter the same recognition surface as BLE and PCSS.
The host supplies platform plus raw vendor and product IDs. Connection
discovery data selects the automatic route. A vendor-level match requires
parsed DeviceInfo confirmation before the model becomes durable identity.

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

XLV is the camera's HTTP web service, a **second protocol family**: HTTP routes
→ handlers (the HTTP analog of the PTP op table), with live view delivered as
chunked / `multipart/x-mixed-replace` MJPEG. It is a **sibling dispatcher that
shares engine state**, not a separate camera; reads/writes hit the same property
engine and media store. XLV is plausibly where the live-view-size properties
that no-op over PTP become real.

### Firmware-keyed transport security (the headline)

- fw **2.30**: XLV over plain **HTTP**.
- fw **2.40**: XLV requires **HTTPS**, and the client must **trust the camera's
  self-signed cert**.

This is pure manifest data: a `tls:` block that differs between the two firmware
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
  path params, chunked streaming), still framework-light.
- **TLS termination** via `rustls`; the self-signed cert is minted per instance
  and surfaced (e.g. a `/cert` control endpoint or a file) so the app/probe can
  trust it. The "client must trust the cert" is the *client's* job; the simulator
  just presents one.
- `engine.on_http(route, req) -> HttpReply`, parallel to `on_operation`, over
  the same `CameraState`.

### Probe

`camera-probe` gains an `xlv`/`http` plan and a **trust-self-signed-cert** knob
so it can talk to a real fw2.40 camera (authorized probing of your own device)
and capture routes/props into bundles.

---

## 3. Wireless tether transport

Likely **PTP/IP-shaped** (confirm via probe), i.e. another transport feeding the
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

## 4. BLE socket (implemented, opt-in)

`camera-sim-service` can serve the in-memory GATT responder
(`camera_sim::BleResponder`) over a loopback TCP socket. A client application
in an environment with no Bluetooth radio (for example an app in the iOS
Simulator) implements its BLE transport seam over this socket and runs its
discovery, pairing and AP-handoff flows against the simulator. The PTP/IP
session that follows uses the service's ordinary listeners.

The listener is off unless both flags are given:

```sh
camera-sim-service ... --ble-bind 127.0.0.1:55750 \
  --ble-scenario packages/fixtures/ble/gfx100ii-pair-handoff.yaml
```

`./run` passes `PTPSIM_BLE_BIND` and `PTPSIM_BLE_SCENARIO` through when both
are set. It starts no BLE listener by default. `127.0.0.1:55750` is the
suggested local address. The protocol has no authentication and its writes
arm the PTP/IP init gate, so the service refuses a non-loopback `--ble-bind`
unless `--ble-allow-non-loopback` is also given.

### Protocol `ptpsim-ble/1`

- Newline-delimited JSON: one compact UTF-8 object per line, ended by `\n`.
  A line longer than 64 KiB (65536 bytes) gets a `bad_request` response with
  `"id": null`, and the server closes the connection.
- Wire field names are `snake_case`. Bytes are lowercase hex strings: pairs of
  hex digits, with no sign, prefix or separator (the server also accepts
  uppercase digits). UUIDs are uppercase canonical strings; the server
  uppercases UUID input before lookup.
- Request (client to server): `{"id": <u64>, "op": "<name>", ...args}`. `id`
  must be a non-negative integer. Client ids start at 1 and increase by 1. A
  line that is not JSON, or has no such `id`, gets a `bad_request` response
  with `"id": null`.
- Response (server to client): exactly one per request, in request order:
  `{"id": n, "ok": true, ...result}` or
  `{"id": n, "ok": false, "error": {"code": "...", "detail": "..."}}`.
- Event (server to client): `{"event": "<name>", ...}`, with no `id`.

The server handles one request at a time per connection. After it writes a
response it pushes the events that request made eligible, then reads the next
request. Request-driven events follow the response to the request that
produced them. Notification fencing depends on this order. An active transfer
scenario can also push notifications in response to a control-state change.

#### Connection lifecycle

- The service serves one client at a time. On accept the server waits up to
  500 ms for an earlier connection to finish. A connection that gets the slot
  receives `{"event":"hello","protocol":"ptpsim-ble","version":1}` as its first
  line; a client fails the connection on any other version. A connection that
  does not get the slot receives `{"event":"error","code":"busy"}` instead of
  `hello`, and is closed. `hello` therefore always means the connection holds
  the slot.
- Each connection that gets the slot starts a new session with a new
  responder built from the scenario and an unspent notification script.
  On the engine's `CameraLink`, the session start clears only the prep flag:
  - The prep flag is cleared, so a launch arms the engine only if this
    connection wrote the prep characteristic before it.
  - The armed state carries over as the last launch left it. A connection
    that opens and closes without a launch changes nothing, so an unarmed
    engine stays unarmed.
  - The registered device name carries over until a connection writes a new
    one.

  These are simulator modelling choices, not observed camera behavior. The
  per-session prep flag follows the `ble-establish-wifi-ap` plan, which writes
  the prep characteristic immediately before every launch; clearing it makes
  a client that skips that write in a later session fail. The armed state is
  latched at launch, as the in-process link model and `ble_arms_init.rs`
  already define. Keeping the registered name is a simulator convention: the
  manifest's `ble-reconnect` plan rewrites the name on every reconnect, and no
  manifest data or test establishes whether a camera keeps it across BLE
  connections.
- `disconnect` answers, then the server closes the TCP connection. When
  checking whether the port can be rebound after a restart, use `SO_REUSEADDR`
  as the listener does. The server end of a closed connection can be in
  `TIME_WAIT`.
- A scripted peer disconnect ends the GATT link and leaves the TCP connection
  open. The client can scan again and reconnect on the same TCP connection.
  A new GATT link has fresh discovery, subscriptions, read and notification
  scripts, and prep flag. The remaining peer-disconnect steps carry over
  until the TCP connection ends.
- A connection holds the slot for as long as it is open, however quiet, since
  a client may hold its BLE link idle for a whole session. The server enables
  TCP keepalive (30 s idle, 10 s interval), so a peer that vanishes without
  closing ends with a read error. A refused client is logged at `warn` with
  the peer that holds the slot and for how long, so a client that opens two
  connections is visible in the service log.

| op | args | result | responder call |
|---|---|---|---|
| `scan_start` | none | none | none; then one `advert` event per scenario peripheral |
| `scan_stop` | none | none | none |
| `connect` | `peripheral` | `mtu`, `peripheral_name` (nullable) | `connect()`. A repeated `connect` to the connected peripheral returns the same result and leaves discovery and subscriptions intact |
| `discover` | none | `characteristics: [uuid]` | `discover_services()` |
| `read` | `uuid` | `value` | `read(uuid)` |
| `write` | `uuid`, `value` | none | `write(uuid, value)` |
| `write_fenced` | `uuid`, `value`, `notify_uuid` | none | `write_with_notification_fence(uuid, value, notify_uuid)` |
| `subscribe` | `uuid` | none | `subscribe(uuid, notify)`; adds `uuid` to the connection's subscribed set |
| `await_disconnect` | none | none | `await_disconnect()` when the next scripted peer disconnect is `awaitDisconnect` |
| `disconnect` | none | none | none; the server then closes the connection |

`discover`, `read`, `write`, `write_fenced` and `subscribe` before `connect`
answer `not_connected`. Every characteristic in the scenario's catalog accepts
`subscribe`; one with no scripted notification stays quiet.

`mtu` is the scenario's `mtu` value, reported as the link's ATT MTU. No MTU
negotiation is modeled. `peripheral_name` is the scenario's `peripheralName`,
or `null`.

Notifications:

- Notifications scripted with `afterFencedWrite` are released only by a
  `write_fenced` to the named characteristic. A plain `write` releases
  nothing.
- `write_fenced` requires `notify_uuid` to be in the catalog; it need not be
  subscribed. The fence discards the notifications buffered on `notify_uuid`
  before the write.
- A GATT server sends notifications only while the client has them enabled.
  Notifications that a `write_fenced` releases on a characteristic the
  connection has not subscribed are dropped. A later `subscribe` does not
  deliver them.
- Simulator convention, not a GATT rule or a camera fact: a notification
  scripted with no `afterFencedWrite` is queued when the session's responder
  is built and is delivered after the response to the first `subscribe` to
  its characteristic, whenever that `subscribe` is sent. A `write_fenced`
  whose `notify_uuid` is that characteristic discards it first if it is still
  queued. This keeps the in-process responder's queue model, and it is how a
  scenario models a value the client receives as soon as it enables
  notifications. The golden transcript's first notification is one of these.
- After every request the server pushes the eligible notifications of each
  subscribed characteristic: per characteristic in script order, and across
  characteristics in subscription order.

Events:

- `advert`: `{"event":"advert","peripheral":"<uuid>","service_uuids":[...],"manufacturer_data":"<hex>","local_name":null,"service_data":{},"tx_power":null}`.
  `manufacturer_data` is the raw advertised field: the 2-byte little-endian
  company ID, then the payload. An absent manufacturer field is `""`.
  `local_name` is a string when the scenario supplies it.
- `notification`: `{"event":"notification","uuid":"...","value":"<hex>"}`.
- `disconnected`: `{"event":"disconnected","peripheral":"<uuid>"}`. The
  event immediately follows the successful response to the request that
  triggered the scripted peer disconnect.
- `error`: only `busy`, as the first and only line of a refused connection.

Responder errors map to wire codes:

| Responder error | Wire code | `detail` |
|---|---|---|
| `NotConnected` | `not_connected` | |
| `ServicesNotDiscovered` | `services_not_discovered` | |
| `NotExposed(uuid)` | `not_exposed` | the UUID |
| `UnexpectedWrite`, `ScriptOutOfOrder` | `script_violation` | the error text |

`await_disconnect` answers `not_connected` without an active GATT link, and
`script_violation` if the scenario's next peer-disconnect step is missing or
has a different trigger.

Protocol-level codes are `bad_request` (unknown op, missing or malformed
argument, invalid line), `unknown_peripheral` (`detail` is the requested id)
and `busy`.

The socket does not support failed MTU requests or scripted peripheral-name
failures. A request for an unlisted operation answers `bad_request`.

The protocol carries opaque UUIDs, hex bytes, a peripheral id and an MTU. It
never names a manufacturer, model, characteristic meaning or launch mode.

The golden transcript
`services/camera-sim-service/tests/fixtures/ble-socket-v1.transcript.ndjson`
pins the wire format with synthetic UUIDs and bytes. Each line wraps one
message as `{"dir":"c2s"|"s2c","msg":{...}}`; messages compare by JSON value.
`tests/ble_socket.rs` replays it through the connection handler. A client
implementation can replay the same file against its own fake server.
`services/camera-sim-service/tests/fixtures/ble-socket-v1-peer-disconnect.transcript.ndjson`
demonstrates `await_disconnect` and scanning again on the same TCP connection.
The socket integration test also exercises the `afterRead` trigger.

### Scenario `ptpsim-ble-scenario/v1`

The scenario is YAML in the manifest's `camelCase` convention, not wire
format. Symbolic GATT names resolve through `models[id].ble.gatt` in the named
manufacturer index; an unknown name fails startup.

```yaml
schema: ptpsim-ble-scenario/v1
provenance: test-scripted          # the only accepted value today
index: ../../camera-config-data/fuji/index.yaml   # relative to the scenario file
model: gfx100ii
mtu: 247
peripheralName: null               # optional
arming: { prep: imageTransferSetting, launch: functionLaunchRequest, deviceName: deviceNameString }
peripherals:
  - id: "<RFC 4122 UUID>"
    advert: { serviceUuids: ["<uuid>"], manufacturerData: "<hex>", localName: "<name>" }
    advertAfterPeerDisconnects:
      1: { serviceUuids: ["<startup service uuid>"], manufacturerData: "<hex>" }
reads:
  <gattName>: { hex: "<hex>" }     # or { utf8: "<text>" }
notifications:
  - { gatt: <gattName>, afterFencedWrite: { gatt: <gattName>, ordinal: 1 }, hex: "<hex>" }
  - { gatt: <gattName>, utf8: "<text>" }   # no afterFencedWrite: delivered on subscribe (simulator convention)
peerDisconnects:
  - { afterRead: <gattName> }
  - { awaitDisconnect: true }
cameraInitiatedTransfer: true    # optional, serves the loaded manifest's trigger state
```

The catalog served is the model's whole GATT map, with every UUID in
uppercase canonical form; a malformed UUID fails startup. `arming` wires the
responder's prep, launch and device-name writes to the engine's `CameraLink`,
so the PTP/IP init gates apply to a handoff that arrives over the socket.

`peerDisconnects` is optional. Its steps are consumed in order once per TCP
connection. Each step declares either a successful read of the named GATT
characteristic or the client's `await_disconnect` request. This ordering
lets a test script link loss after a baseline read and then exercise a wake
plan whose next step is to wait for the camera to drop the link.

An optional `advertAfterPeerDisconnects` map selects the advert served after
the indicated number of completed steps. Without an entry for the current
count, scans serve the peripheral's base `advert`.

Set `cameraInitiatedTransfer: true` to serve the running manifest's declared
transfer trigger characteristics. The scenario uses the first baseline and
trigger value declared for each characteristic. A successful state overlay
sets which values reads return. A live change also pushes notifications on
the currently subscribed trigger characteristics. These control-driven
notifications can arrive between requests and report the latest state.
Other notifications keep their request ordering. Baseline uses the
manifest's inactive values, and the trigger uses its separate active values.

`packages/fixtures/ble/gfx100ii-wake-transfer.yaml` scripts a lost baseline
link, a startup advertisement, and an awake advertisement. After the wake
plan and reconnect, trigger the manifest's transfer state with
`camera-simctl state patch --overlay '{"camera_initiated_transfer_active":true}'`.
Set the same field to `false` to restore baseline. The PTP/IP transfer queue
is populated from the configured media root. The fixture also serves the
ordinary fenced AP handoff for clients that request it after the trigger.

`packages/fixtures/ble/gfx100ii-pair-handoff.yaml` serves the GFX100 II legacy
pairing advert, the `ble-pair` plan and the `ble-establish-wifi-ap` handoff.
Each value carries a comment naming its source: manifest data where the
manifest describes it, otherwise a value already used by a ptpsim test.
`tests/ble_socket.rs` asserts that its advert is the first discoverable
signature match in the manifest.

Limits:

- Advert fields are literal scenario values. A scenario can select them by
  peer-disconnect stage.
- The responder answers immediately. AP-launch timing is not modeled.
- Within one connection the prep flag is never cleared, so after a prep write
  every later launch on that connection arms the engine. A new connection
  clears it.
- One client per service.

---

## Scorecard (honest impact)

| Transport | New core? | New runtime? | New data? |
|---|---|---|---|
| PTP/USB | none | USB-bulk framing primitive + a PHY adapter (gadget/Facedancer) | descriptors + modes |
| XLV/HTTP | none | HTTP router + `rustls` + `on_http` dispatch | routes + firmware-keyed `tls` |
| Wireless tether | none | maybe an establishment strategy | transport block (cheapest) |

What stays invariant: `ptp-core`, the `camera-manifest` model shape, and
`camera-sim`'s state model. Schema additions (`kind: ptpip-usb|http-xlv|ptpip-tether`,
`tls`, `modes`) are all optional fields; `camera-manifest/v1` stays compatible.

## Build order when these land

1. Probe each transport into bundles (USB needs no PHY; XLV-2.40 needs the cert
   trust knob) → reviewed manifests under `packages/protocol-spec`.
2. Wireless tether first if PTP/IP-shaped (reuses the engine; smallest runtime).
3. XLV next: HTTP router + `rustls` + `on_http`; high value (web control surface,
   possibly the real live-view-size props).
4. USB last on the responder side (needs a PHY); USB *probing* can land anytime.
