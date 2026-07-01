# G4 — `camera-protocol-ffi` surface design (the iOS/macOS seam)

Status: design sketch for review, 2026-05-26. The artifact whose job is **"design
the seam so adding wireless-tether/USB later is data + app I/O, never a re-port."**
Grounded in the `camera-config` query API (query.rs/store.rs) + the 5-connection
GFX100 II manifest. Consumes `ios-adoption.md` (G4) and `camera-config.md`.

## Decisions baked into this sketch
1. **uniffi via proc-macros** (`#[uniffi::export]` / `#[derive(uniffi::Record|Enum|Error)]`),
   not a `.udl` file. Current-recommended; keeps the surface next to the Rust.
2. **The seam is `(connection, mode)`-keyed.** Every capability/gating query takes a
   `connection` id + a `mode` path. This is the anti-re-port decision: the app asks
   *"is this op valid over `connection` in `mode`?"* — adding a transport is a manifest
   row + the app's own socket/USB code, never a change to this surface.
3. **Sans-io.** Nothing here touches a socket, USB, or CoreBluetooth. Queries are pure
   over manifest data + observed values the app supplies. Codecs turn intents↔bytes.
   The app owns every byte on the wire and every OS API.
4. **One loaded `ConfigStore` object** (Arc interface), built once from bundled manifest
   bytes (+ OTA later), queried many times. Records pass by value.
5. **Sync, not async** — queries and codecs are pure/instant. Async/poll is a Phase-B
   (stateful session driver) concern, deferred.

---

## A. Transport-abstraction query surface (the priority)

```rust
/// Loaded, queryable camera config. Built from bundled YAML (manufacturer + body);
/// OTA bundle loading lands later. Arc interface — construct once, query freely.
#[derive(uniffi::Object)]
pub struct ConfigStore { /* wraps camera_config::ConfigStore */ }

#[uniffi::export]
impl ConfigStore {
    /// Bundled baseline. `manufacturer_yaml` carries fuji.yaml (versionOrder + fixed
    /// initiator identity); `body_yaml` carries gfx100ii.yaml.
    #[uniffi::constructor]
    pub fn from_bundle(body_yaml: String, manufacturer_yaml: Option<String>)
        -> Result<Arc<Self>, ConfigError>;

    // --- WHERE: connections (filtered by the calling platform) ---
    /// Connections valid on `platform` under the camera's firmware. USB/wireless-tether
    /// hidden on iOS; instax filtered out by firmware (availableWhen) — all from data.
    pub fn connections(&self, platform: Platform) -> Vec<ConnectionInfo>;

    /// How to bring `connection` up from where you are (BLE→WiFi-AP handover, PCSS knock).
    /// Returns the establishment plan as DATA; the app drives the actual GATT/UDP/TCP I/O.
    pub fn establishment(&self, connection: String) -> Option<EstablishmentPlan>;

    // --- WHERE: socket binding roles + derived ports (#140) ---
    /// The port to bind for `role` (command/event/live-view) — the app binds by role
    /// instead of hardcoding the Fuji command port + `+1`/`+2` offsets. `None` = no such socket.
    pub fn port_for_role(&self, connection: String, role: SocketRole) -> Option<u16>;
    pub fn socket_bindings(&self, connection: String) -> Vec<SocketBindingInfo>; // command → event → live-view
    /// The transport-close frame (manifest names the sentinel; resolved to bytes here)
    /// sent before reopening an image-transfer session. On the Fuji `app` path = keep-AP sentinel.
    pub fn transport_close(&self, connection: String) -> Option<TransportCloseInfo>;

    // --- WHERE: modes within a connection ---
    pub fn modes(&self, connection: String) -> Vec<ModeInfo>;             // hierarchical paths
    pub fn capabilities(&self, connection: String, mode: String) -> Vec<String>;

    /// Which mode the observed props put us in (evaluates `detect` predicates). None →
    /// app shows a picker over `modes()`.
    pub fn detect_mode(&self, connection: String, observed: Vec<PropObservation>)
        -> Option<String>;

    /// The wire-action plan to enter `to` (optionally from a known mode — the cheaper
    /// teardown-free switch when `from` is set). Steps are a closed vocabulary; the app
    /// executes them over its transport. Or a userInstruction (camera-menu / not app-driven).
    pub fn mode_entry(&self, connection: String, from: Option<String>, to: String)
        -> Option<ModeEntryPlan>;

    // --- gating: the orthogonal intersection + runtime prerequisite ---
    pub fn operation_available(
        &self, connection: String, mode: String, op: u16, observed: Vec<PropObservation>,
    ) -> Availability;

    /// Intent→mechanism: how to set `prop` over this connection/mode (App vendor-step vs
    /// tether absolute — mechanism varies by connection).
    pub fn control_for(&self, connection: String, mode: String, prop: u16)
        -> Option<ControlInfo>;

    /// Value-policy resolution (fixed initiator identity, generated session ids, …).
    pub fn value(&self, key: String) -> Option<ResolvedValue>;

    pub fn value_label(&self, prop: u16, value: i64) -> Option<String>;
}
```

### Records & enums for the query surface
```rust
#[derive(uniffi::Enum)]
pub enum Platform { Ios, Macos, Android, Linux }

#[derive(uniffi::Record)]
pub struct ConnectionInfo {
    pub id: String,            // "app" | "ble" | "usb" | "wireless-tether" | "xlv"
    pub kind: String,          // "ptpip-app" | "ble" | "usb-ptp" | "ptpip-direct" | "http-xlv"
    pub discovery: String,     // "ble" | "usb" | "pcss-knock" | "http-probe"
    pub auto_discoverable: bool,
    // + per-connection traits (#81): init_shape, live_view_delivery, shutter_recipe;
    //   wire framing (#133): command_framing, event_framing (Option<PtpFraming>);
    //   and, via the accessors above, socket bindings (#140).
}

#[derive(uniffi::Enum)]  // #140 — bind by role, not by hardcoded Fuji offsets
pub enum SocketRole { Command, Event, LiveView }

#[derive(uniffi::Record)]
pub struct SocketBindingInfo { pub role: SocketRole, pub port: u16 }

#[derive(uniffi::Record)]
pub struct TransportCloseInfo { pub packet: Vec<u8>, pub when: Option<String> } // named sentinel → bytes

#[derive(uniffi::Record)]
pub struct ModeInfo { pub path: String, pub capabilities: Vec<String> }

#[derive(uniffi::Enum)]
pub enum Availability { Available, WrongMode, WrongConnection, Blocked, Unavailable }

#[derive(uniffi::Record)]
pub struct PropObservation { pub code: u16, pub value: i64 }  // the app's observed values

#[derive(uniffi::Record)]
pub struct ControlInfo {
    pub set_method: Option<String>,   // "absolute" | "vendorStep"
    pub operation: Option<u16>,       // 0x1016 vs 0x902d
    pub readback: Option<u16>,
}

#[derive(uniffi::Record)]
pub struct ModeEntryPlan {
    pub to: String,
    pub from: Option<String>,
    pub steps: Vec<EntryStep>,             // wire actions, in order
    pub user_instruction: Option<String>,  // when not app-driven (USB menu, connection switch)
}

#[derive(uniffi::Enum)]
pub enum EntryStep {
    SetProp { prop: u16, value: i64 },
    GetProp { prop: u16 },
    ReadEcho { prop: u16 },        // read then write the same value back (df2a)
    SendOp { op: u16, repeat: u32 }, // repeat covers 902B×4
}

#[derive(uniffi::Record)]
pub struct EstablishmentPlan {
    pub target_connection: String,         // e.g. "app" (the BLE→WiFi-AP handover)
    pub mechanism: Option<String>,         // "ble-establish-wifi-ap" | "pcss-knock"
    pub user_instruction: Option<String>,
    pub params: Vec<KeyValue>,             // knock ports, GATT char uuids, etc. (app drives I/O)
}

#[derive(uniffi::Record)]
pub struct KeyValue { pub key: String, pub value: String }

#[derive(uniffi::Enum)]
pub enum ResolvedValue {
    Fixed { value: String },
    Generated { scheme: String, persist: bool },
    FromPairing { source: String },
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum ConfigError {
    #[error("manifest parse error: {0}")]
    Parse(String),
    #[error("unsupported schema: {0}")]
    Schema(String),
}
```

---

## B. Codec functions (G1–G3 workstream — pure intents↔bytes)

Sans-io: they return/consume `Vec<u8>`, the app does the socket/USB write. **G1–G3 landed
(#133).** Codec errors surface as `CodecError { Encode | Decode }`. Framing is selected per
call by `PtpFraming { Standard | FujiCompressed | Usb }`, which the consumer **reads from the
manifest** — `ConnectionInfo.command_framing` / `event_framing` — so the connection→framing
choice is data, never a `kind`-mapping in the app's own code (no manufacturer knowledge left in
Swift). Command vs event can differ: the Fuji `app` command channel is `FujiCompressed` while its
event socket is the PIMA type-4 container (`Usb`). All three share the `ptp-core` container
payloads; only the header differs.

```rust
// G1 — Fuji reference app 82-byte init (identity from value-policy, tail from manifest)
#[uniffi::export] fn build_app_init(guid: Vec<u8>, friendly_name: String, tail: Vec<u8>) -> Result<Vec<u8>, CodecError>;
#[uniffi::export] fn validate_init_ack(packet: Vec<u8>) -> Result<(), CodecError>;
#[uniffi::export] fn keep_ap_sentinel() -> Vec<u8>;  // 8-byte 0xffffffff keep-AP frame (#82)

// G2 — value codec (per-value semantics are manifest data; this writes the bytes)
#[uniffi::export] fn encode_value(value: i64, width: ValueWidth) -> Result<Vec<u8>, CodecError>;

// G3 — PTP/IP packet framing (all framings via PtpFraming)
#[uniffi::export] fn build_command(framing: PtpFraming, op: u16, txn: u32, params: Vec<u32>) -> Result<Vec<u8>, CodecError>;
#[uniffi::export] fn build_data(framing: PtpFraming, op: u16, txn: u32, payload: Vec<u8>) -> Result<Vec<u8>, CodecError>;
#[uniffi::export] fn parse_response(framing: PtpFraming, packet: Vec<u8>) -> Result<ResponseFrame, CodecError>;
#[uniffi::export] fn parse_data_payload(framing: PtpFraming, packet: Vec<u8>) -> Result<Vec<u8>, CodecError>;
#[uniffi::export] fn parse_data_phase(framing: PtpFraming, packet: Vec<u8>) -> Result<DataPhaseFrame, CodecError>;  // Standard streams Start/Data/End; Fuji/USB deliver one Data frame
#[uniffi::export] fn parse_event(framing: PtpFraming, packet: Vec<u8>) -> Result<Option<CameraEvent>, CodecError>;

// G3 — dataset codecs (framing-independent payloads)
#[uniffi::export] fn parse_object_info(payload: Vec<u8>) -> Result<PtpObjectInfo, CodecError>;       // generic ISO-15740 fields
#[uniffi::export] fn parse_device_prop_desc(payload: Vec<u8>) -> Result<PtpDevicePropDesc, CodecError>; // PtpValue / PtpPropForm
#[uniffi::export] fn parse_live_status(payload: Vec<u8>) -> Result<LiveStatus, CodecError>;          // 0xd212 record stream
#[uniffi::export] fn parse_object_handle_list(payload: Vec<u8>) -> Result<Vec<u32>, CodecError>;     // 0xd621 quirk / GetObjectHandles
```

Records/enums: `ResponseFrame { response_code, txn, params }`, `CameraEvent { code, txn, params }`,
`DataPhaseFrame { kind: DataPhaseKind (Start|Data|End), txn, total_length?, payload }` (total_length
set only on `Start`; the Fuji compressed and USB channels use a single `Data` frame — see the
byte-exact reconciliation in `fuji_framing`),
`PtpObjectInfo` (the generic `ObjectInfo` fields; media classification is `media_format()` + #136),
`PtpDevicePropDesc { code, datatype, get_set, factory_default, current, form }` over
`PtpValue { U8|U16|U32|U64|Str }` and `PtpPropForm { None | Range | Enum }`, `LiveStatus { records:
Vec<PropObservation> }`. Notes: the Fuji compressed data phase is one length-prefixed type-2 frame
whose code echoes the opcode (hence `build_data` takes `op`); standard framing's `Data` carries no
opcode, and USB data phases are a bulk-transfer concern that is not re-emittable, so
`build_data(Usb, …)` errors (decode still works).
`parse_event` on the Fuji compressed command channel rejects event frames (events ride a separate
socket). The G2 sketch's per-value `encode_aperture/iso/shutter` was superseded by the single
manifest-driven `encode_value`.

---

## How the app uses it — the multi-transport seam (Swift)
```swift
let store = try ConfigStore.fromBundle(bodyYaml: gfx, manufacturerYaml: fuji)

// "What can I connect over, here on macOS?"  → app, usb, wireless-tether, xlv (NOT instax/ble-hidden by fw/platform)
for c in store.connections(platform: .macos) { … }

// Bring up the App connection from BLE — app drives GATT using the returned params.
if let plan = store.establishment(connection: "app") { /* app does the BLE/WiFi I/O */ }

// Enter live-view: execute the wire steps over whatever transport is active.
if let entry = store.modeEntry(connection: "app", from: nil, to: "Shooting/Stills") {
    for step in entry.steps { /* app sends bytes via build_command etc. */ }
}

// Gate a control — SAME call whether App or wireless-tether; mechanism differs in data.
switch store.operationAvailable(connection: conn, mode: "Shooting/Stills", op: 0x902d, observed: obs) {
case .available: …
case .wrongConnection, .wrongMode, .blocked, .unavailable: …
}
let ctl = store.controlFor(connection: conn, mode: "Shooting/Stills", prop: 0x5007) // absolute vs vendorStep
```
Adding wireless-tether/USB to the app = the app writes the socket/USB I/O + calls the
**same** `connections/establishment/modeEntry/operationAvailable/controlFor` surface. No
seam change. That is the property we're buying.

---

## Open items (verify during build, per ios-adoption #3/#5)
- **xcframework build** wired into XcodeGen `project.yml` (no precedent in the repo).
- **Vec<u8>↔Data copy cost** for 12 MiB image chunks — measure; keep chunked downloads.
- **Async/poll** only matters for Phase B (stateful `PtpSession` feed/poll pump) — out of
  scope here; this surface is sync.
- **Establishment byte-builders** (PCSS knock payload, BLE writes) — likely small codec
  fns like `build_app_init`; added when wireless-tether/USB I/O lands in the app.
- **Private overlay**: XLV bearer token / BLE access-gate are NOT in the bundled manifest;
  the app supplies them out-of-band (the surface never carries the secret).

## Out of scope
Phase B stateful session driver (`camera-initiator`); the actual socket/USB/BLE I/O; the
fw-tier merge loader (XLV fw2.40 HTTPS override still inline). All deferred, none block G4.
