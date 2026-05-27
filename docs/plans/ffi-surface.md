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
}

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
    pub mechanism: Option<String>,         // "ble-to-wifi-ap-v1" | "pcss-knock-v1"
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

These flow bytes through the same FFI; partial today (`ptp-core` framing + `fuji_framing`
+ liveview parse + `usb_ptp` exist; init variant / value codecs / Fuji parse helpers are
the build). Sans-io: they return/consume `Vec<u8>`, the app does the socket/USB write.

```rust
// G1 — Fuji reference app 82-byte init (identity from value-policy, tail from manifest)
#[uniffi::export] fn build_app_init(guid: Vec<u8>, friendly_name: String, tail_hex: String) -> Vec<u8>;
#[uniffi::export] fn validate_init_ack(packet: Vec<u8>) -> Result<(), ConfigError>;

// PTP-IP command/data framing (compressed reference app channel byte-exact today)
#[uniffi::export] fn build_command(op: u16, txn: u32, params: Vec<u32>) -> Vec<u8>;
#[uniffi::export] fn parse_response(packet: Vec<u8>) -> Result<ResponseFrame, ConfigError>;

// G2 — Fuji value codecs (none exist in Rust yet)
#[uniffi::export] fn encode_aperture(f_number_x100: u16) -> Vec<u8>;      // 280 = f/2.8
#[uniffi::export] fn encode_iso(iso: u32, auto_ceiling: bool) -> Vec<u8>; // 0x80000000|ceiling
#[uniffi::export] fn encode_shutter_speed(numer: u32, denom: u32) -> Vec<u8>;

// G3 — Fuji parse helpers / quirks
#[uniffi::export] fn parse_live_status(payload: Vec<u8>) -> LiveStatus;       // 0xd212 bundle
#[uniffi::export] fn parse_object_handle_list(payload: Vec<u8>) -> Vec<u32>;  // 0xd621 quirk
#[uniffi::export] fn parse_event(packet: Vec<u8>) -> Option<CameraEvent>;
```

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
