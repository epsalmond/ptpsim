# Integrating `camera-protocol-ffi` (iOS / macOS / Android / Linux)

How a client app adopts ptpsim as its camera-protocol brain. **Platform-neutral on
purpose** — the seam is identical across languages; only binding generation + native
packaging differ. iOS/macOS get Swift, Android gets Kotlin, from the *same* crate and
the *same* command.

Full surface + design rationale: `docs/plans/ffi-surface.md`. Manifest/engine model:
`docs/plans/camera-config.md`.

## 1. Mental model — ptpsim is the brain, you are the hands (sans-io)

ptpsim does **no I/O**. It never opens a socket, USB endpoint, BLE GATT, or Wi-Fi
join. **Your app owns every byte on the wire and every OS API.** ptpsim:

- answers `(connection, mode)` questions — *which connections exist here, which modes,
  is this operation valid now, how do I set this property, how do I enter this mode,
  how do I bring this connection up* — over manifest **data** + values you observed;
- turns intents ↔ bytes (the codec functions).

The payoff: **adding a transport or a camera is a data change + your own I/O code —
never a change to this binding surface.** That is the whole reason the seam is
`(connection, mode)`-keyed. Corollary (anti-vcam): **no PTP opcodes, prop codes, mode
constants, or ports as literals in app source** — ask the manifest.

## 2. The seam (what you call)

A single `ConfigStore`, built once from the bundled manifest YAML, then queried:

| call | gives you |
|---|---|
| `ConfigStore.from_bundle(body, manufacturer?)` | the loaded store |
| `connections(platform)` | connections valid on *this* platform + firmware (USB/tether hidden on iOS — data-driven) |
| `establishment(connection)` | how to bring a connection up (PCSS knock ports, BLE→Wi-Fi handover) **as data — you drive the I/O** |
| `modes(connection)` / `capabilities(connection, mode)` | the modes + what they can do |
| `detect_mode(connection, observed)` | which mode the camera is in, from props you read |
| `mode_entry(connection, from, to)` | the ordered wire-steps to enter a mode (or a `user_instruction` when it's a camera-menu / manual step) |
| `operation_available(connection, mode, op, observed)` | `Available / WrongMode / WrongConnection / Blocked / Unavailable` |
| `control_for(connection, mode, prop)` | the set-mechanism (absolute vs vendor-step — differs by connection) |
| `value(key)` / `value_label(prop, value)` | value-policy resolution + human labels |

Byte codecs (build/parse PTP packets, encode values) are exported functions —
**partial today** (see §6).

## 3. Generate bindings (library mode — no UDL)

Build the lib, then point the bundled generator at it. **Same step, both languages:**

```bash
cargo build -p camera-protocol-ffi --release          # produces lib<...>.{so,dylib,a}
LIB=target/release/libcamera_protocol_ffi.<so|dylib|a>

# Swift (iOS / macOS)
cargo run -p camera-protocol-ffi --bin uniffi-bindgen -- \
    generate --library "$LIB" --language swift  --out-dir generated/swift

# Kotlin (Android)
cargo run -p camera-protocol-ffi --bin uniffi-bindgen -- \
    generate --library "$LIB" --language kotlin --out-dir generated/kotlin
```

Swift emits `camera_protocol_ffi.swift` + `…FFI.h` + `…FFI.modulemap`; Kotlin emits
`uniffi/camera_protocol_ffi/camera_protocol_ffi.kt`. (Optional: install `swiftformat`/
`ktlint` to auto-format; harmless warning if absent.)

## 4. Build + package the native library (per platform)

uniffi is `crate-type = ["staticlib", "cdylib"]`. Standard per-platform packaging:

- **iOS / macOS:** build the **staticlib** (`.a`) for each target (`aarch64-apple-ios`,
  `aarch64-apple-ios-sim`, `aarch64-apple-darwin`, …), then
  `xcodebuild -create-xcframework` over the `.a`s + the generated header/modulemap.
  Vendor the `.swift` + `.xcframework` into the app (e.g. XcodeGen `project.yml`).
- **Android:** build the **cdylib** (`.so`) per ABI (`aarch64-linux-android`, …) into
  `jniLibs/<abi>/`; ship the `.kt` + the `.so`s (optionally as an `.aar`).
- **Linux:** the `.so` + Python/other bindings if needed.

## 5. Integration pattern

1. **Embed the data bundle.** Ship `camera-config-data/fuji/fuji.yaml` (manufacturer)
   + `…/gfx100ii/gfx100ii.yaml` (body) as app resources; pass their contents to
   `from_bundle`. (OTA bundle loading lands later; bundled baseline for now.)
2. **Pick a connection.** `connections(platform)` → present what's actually available
   here. Bring it up: `establishment(connection)` returns the recipe (knock ports,
   GATT char UUIDs); **your code does the UDP/TCP/BLE/Wi-Fi**.
3. **Enter a mode.** `mode_entry(connection, from, to)` → execute the `steps` (send
   each via the codec functions over your transport) or surface the `user_instruction`.
4. **Drive controls, gated.** Before any op: `operation_available(...)`. To set a
   value: `control_for(...)` tells you the mechanism; the codec encodes the bytes.
5. **Detect state.** Feed observed prop values to `detect_mode` / `operation_available`
   (the predicate `requires`) — you read them off the wire; the engine evaluates.

## 6. Status — what's ready vs pending

- **Ready:** the §A query surface (above), and the GFX100 II manifest across **all five
  connections** (`app` WiFi-AP, `ble`, `wireless-tether` PCSS, `usb`, `xlv` HTTP).
- **Partial — byte codecs (§B / G1–G3):** `ptp-core` framing + `fuji_framing` +
  liveview parse + `usb_ptp` exist; the 82-byte reference app init, Fuji value codecs, and Fuji
  parse helpers are **not built yet**. You can validate the seam against the simulator
  before these land; flag what you need.
- **Sync only.** A stateful session driver (feed/poll) is a later phase; today's
  surface is synchronous pure queries.

## 7. Golden rules

- **Transport = data + your I/O.** Adding wireless-tether/USB/HTTP is: the manifest
  already has it (or add a row) + your platform's socket/USB/BLE code. The binding
  surface does not change. If you find yourself wanting to change the seam to add a
  transport, stop — that's the thing we designed against.
- **No protocol literals in app source.** Opcodes, prop codes, mode values, ports → ask
  the manifest. A CI grep for PTP hex literals in app sources should stay empty.
- **Secrets stay out of the bundle.** Access-gate material (the XLV bearer token, BLE
  pairing secrets) is **not** in `camera-config-data` and never comes over this surface
  — your app supplies it out-of-band (a private overlay). The manifest only says *that*
  a token is required, not how to mint it.

## 8. Validate against the simulator

`services/camera-sim-service` runs the same manifest as a responder (IPv6 + control
HTTP). Point your app at it to exercise connect / live-view / browse / download without
a physical camera, and to A/B the FFI path against your legacy codec before cutover.
