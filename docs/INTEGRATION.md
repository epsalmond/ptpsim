# Integrating `camera-protocol-ffi` (iOS / macOS / Android / Linux)

How a client app adopts ptpsim as its camera-protocol brain. **Platform-neutral on
purpose** — the seam is identical across languages; only binding generation + native
packaging differ. iOS/macOS get Swift, Android gets Kotlin, from the *same* crate and
the *same* command.

Full surface + design rationale: `docs/plans/ffi-surface.md`. Manifest/engine model:
`docs/plans/camera-config.md`. Greenfield iOS rewrite consuming the new pull-model
surface: `docs/plans/ios-rewrite-p0-p1-ble-mvp.md` + the handoff at
`docs/handoff-ios-ble-mvp.md`.

Two seams ship from this crate:

* **Single-body queries** (`(connection, mode)`-keyed, §2–§7 below). The original
  surface; app already knows which body it's talking to. Used by the existing
  adoption path.
* **Manufacturer-index pull model** (§9). Observation in → decision out; the app
  carries zero camera knowledge and the manifest tells it what each scanned advert /
  enumerated device means. Used by the iOS rewrite for BLE pairing and forward.

Both seams coexist on `ConfigStore` — different constructors choose which one you
get.

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
| `ConfigStore.from_bundle(body, manufacturer?)` | the loaded store (single-body) |
| `ConfigStore.from_tiers(body, manufacturer?, fw_overlays)` | as above, with firmware-tier overlays merged onto the body |
| `connections(platform)` | connections valid on *this* platform + firmware (USB/tether hidden on iOS — data-driven) |
| `connection_establishment(connection)` | how to bring a connection up (PCSS knock ports, BLE→Wi-Fi handover) **as data — you drive the I/O** *(renamed from `establishment(connection)` — the bare name now belongs to the pull-model flow §9)* |
| `modes(connection)` / `capabilities(connection, mode)` | the modes + what they can do |
| `detect_mode(connection, observed)` | which mode the camera is in, from props you read |
| `mode_entry(connection, from, to)` | the ordered wire-steps to enter a mode (or a `user_instruction` when it's a camera-menu / manual step) |
| `action(connection, verb)` | the parameterized recipe for a verb (e.g. `shutter`, `getObject`) — `Action.params` names runtime slots to bind, `Action.steps` is the wire sequence, `Action.triggers` declares post-conditions (e.g. `imagesPushed { min, max }` for PCSS shutter — camera auto-pushes images, register receiver first). See `docs/plans/action-verbs.md` |
| `operation_available(connection, mode, op, observed)` | `Available / WrongMode / WrongConnection / Blocked / Unavailable` |
| `control_for(connection, mode, prop)` | the set-mechanism (absolute vs vendor-step — differs by connection) |
| `value(key)` / `value_label(prop, value)` | value-policy resolution + human labels |

Byte codecs (build/parse PTP packets, encode values) are exported functions —
**partial today** (see §6).

## 3. Generate bindings (uniffi 0.31, library mode — no UDL)

Build the lib, then point the bundled generator at it. **One binary, four
languages.** uniffi 0.31 dropped the `--library` flag (auto-detected); the
`-l <lang>` flag selects.

**Run from the workspace root.** Bindgen discovers `uniffi.toml` via cargo
metadata starting at the *current* directory; running from a subdirectory
silently falls back to default snake_case names (`camera_protocol_ffi.swift`
instead of `CameraProtocolFFI.swift`) and may fail to detect library mode
at all ("UDL file does not appear to be inside a crate").

```bash
# 1. Build the library in release mode. Pick the extension your platform produces:
#       macOS  → libcamera_protocol_ffi.a       (staticlib; also dylib for dev)
#       Linux  → libcamera_protocol_ffi.so
#       iOS    → built per-target from ci/build-xcframework.sh
cargo build -p camera-protocol-ffi --release

# 2. Build the bindgen tool ONCE (avoids debug-rebuild on every invocation;
#    also sidesteps the cargo-run-over-non-interactive-ssh hang).
cargo build -p camera-protocol-ffi --release --bin uniffi-bindgen
BINDGEN=target/release/uniffi-bindgen

# 3. Pick the .a (or .so on Linux) and generate.
LIB=target/release/libcamera_protocol_ffi.a   # .so on Linux; .a is canonical on macOS

# Swift (iOS / macOS)
"$BINDGEN" generate -l swift  -o generated/swift  "$LIB"

# Kotlin (Android)
"$BINDGEN" generate -l kotlin -o generated/kotlin "$LIB"

# Python (Linux / protocol-mapper)
"$BINDGEN" generate -l python -o generated/python "$LIB"
```

Swift emits `CameraProtocolFFI.swift` + `CameraProtocolFFIFFI.{h,modulemap}` (the
PascalCase names come from `crates/camera-protocol-ffi/uniffi.toml`). Kotlin emits
`uniffi/camera_protocol_ffi/camera_protocol_ffi.kt`. Python emits
`camera_protocol_ffi.py`. Optional formatters (`swiftformat`/`ktlint`/`ruff`) run
automatically when on PATH; harmless warning if absent.

### Troubleshooting

| symptom | fix |
|---|---|
| `UDL file does not appear to be inside a crate` | You're running bindgen against a path it can't resolve to a crate. Confirm you're in the workspace root (not a subdirectory), the `--release` build actually produced `target/release/libcamera_protocol_ffi.{a,so,dylib}`, and you're passing the staticlib (`.a` on macOS) rather than the rlib. |
| Swift file is `camera_protocol_ffi.swift` (snake_case) instead of `CameraProtocolFFI.swift` | Same root cause as above — bindgen didn't find `crates/camera-protocol-ffi/uniffi.toml`, so the PascalCase override didn't apply. Re-run from the workspace root after `cargo clean -p camera-protocol-ffi`. |
| `Warning: Unable to auto-format … using swiftformat` | Harmless. Install `swiftformat` to suppress, or pass `-n` / `--no-format` to skip the attempt. The `.swift` file is still produced. |
| `cargo run --bin uniffi-bindgen` hangs over non-interactive SSH | Cargo's stdin forwarding to the child process misbehaves over headless ssh. Build the binary once (`cargo build --release --bin uniffi-bindgen`) and invoke `target/release/uniffi-bindgen` directly. |

## 4. Build + package the native library (per platform)

`camera-protocol-ffi` is `crate-type = ["lib", "staticlib", "cdylib"]`. Standard
per-platform packaging:

- **iOS / macOS:** build the **staticlib** (`.a`) for each target
  (`aarch64-apple-ios`, `aarch64-apple-ios-sim`, `aarch64-apple-darwin`,
  `x86_64-apple-darwin`), `lipo`-combine the two macOS arches, then
  `xcodebuild -create-xcframework`. **Verified end-to-end recipe in
  `docs/plans/ios-rewrite-p0-p1-ble-mvp.md` §11.11** — that's the recipe
  Woodpecker ships from CI; reuse it for local builds. Each release
  publishes `CameraProtocolFFI-<sha8>.xcframework.zip` + a `.checksum`
  sibling for SPM `binaryTarget(url:, checksum:)`; consumer-side wiring
  is **`docs/SPM_INTEGRATION.md`** (one-line render via
  `ci/spm-snippet.sh <sha-tag>`).
- **Android:** build the **cdylib** (`.so`) per ABI (`aarch64-linux-android`,
  `armv7-linux-androideabi`, `x86_64-linux-android`) into `jniLibs/<abi>/`; ship
  the `.kt` + the `.so`s as a source-distribution tarball (`CameraProtocolFFI-
  <sha8>-android.tar.gz`) the consumer drops into their Gradle module via two
  `cp -r` commands. **Consumer-side: `docs/ANDROID_INTEGRATION.md`.** Real
  `.aar` wrapping (compiled `classes.jar` + AndroidManifest) is the follow-up
  (#43) — needs `kotlinc` + `android.jar` in CI.
- **Linux / Python:** the `.so` + the generated `camera_protocol_ffi.py`. Used
  by `protocol-mapper` (P2 task — same parent CI job as Android).

## 5. Integration pattern

1. **Embed the data bundle.** Ship `camera-config-data/fuji/fuji.yaml` (manufacturer)
   + `…/gfx100ii/gfx100ii.yaml` (body) as app resources; pass their contents to
   `from_bundle`. Each release attaches a `camera-config-data-<sha8>.tar.gz`
   sibling alongside the xcframework, so the FFI binary and the YAML data come
   from the same commit and can't drift mid-integration; extract it to vendor
   the bundle without cloning the source tree. (Co-shipped pre-data-repo-split;
   the eventual Apache-licensed data repo will publish its own versioned
   releases.) OTA bundle loading lands later; bundled baseline for now.
2. **Pick a connection.** `connections(platform)` → present what's actually available
   here. Bring it up: `establishment(connection)` returns the recipe (knock ports,
   GATT char UUIDs); **your code does the UDP/TCP/BLE/Wi-Fi**.
3. **Enter a mode.** `mode_entry(connection, from, to)` → execute the `steps`
   (`setProp`/`getProp`/`readEcho`/`sendOp`) via the codec functions over your
   transport, or surface the `user_instruction`. Each step may be `tolerant` (a
   non-OK PTP *response* is advisory — log + continue; only a transport failure
   aborts). `sendOp` `params` are literals **or** a named runtime slot
   (`EntryParam.Runtime { slot }`, e.g. `openCaptureTxId`) that **you** bind from
   your session state — ptpsim names which runtime value goes there; it never
   computes it.
4. **Drive controls, gated.** Before any op: `operation_available(...)`. To set a
   value: `control_for(...)` tells you the mechanism; the codec encodes the bytes.
5. **Detect state.** Feed observed prop values to `detect_mode` / `operation_available`
   (the predicate `requires`) — you read them off the wire; the engine evaluates.

## 6. Status — what's ready vs pending

- **Ready:** the §A query surface (above) + `operation_available_explained`
  (`GateExplanation` — the resolution trace for telemetry); the GFX100 II manifest
  across **all five connections** (`app` WiFi-AP, `ble`, `wireless-tether` PCSS, `usb`,
  `xlv` HTTP); firmware-tier overlays via `ConfigStore.from_tiers(body, manufacturer,
  fw_overlays)` (e.g. `fw2.40.yaml` flips XLV to HTTPS).
- **Codecs (§B):** `build_app_init` (G1, the 82-byte init), `validate_init_ack`, and
  `encode_value(raw, width)` (G2 — the generic value encoder; per-value semantics live
  in `descriptor.values`/`labels`) are **landed + in the bindings**. Plus existing
  `fuji_framing` + liveview parse + `usb_ptp`. **Pending — G3 parse helpers**
  (`parse_live_status` 0xd212, `parse_object_handle_list` 0xd621, Fuji `parse_object_info`,
  `parse_event`): not built — they need byte-layout evidence. Flag what you need.
- **Sync only.** A stateful session driver (feed/poll) is a later phase; today's
  surface is synchronous pure queries.

## 7. Golden rules

- **Transport = data + your I/O.** Adding wireless-tether/USB/HTTP is: the manifest
  already has it (or add a row) + your platform's socket/USB/BLE code. The binding
  surface does not change. If you find yourself wanting to change the seam to add a
  transport, stop — that's the thing we designed against.
- **No protocol literals in app source.** Opcodes, prop codes, mode values, ports → ask
  the manifest. A CI grep for PTP hex literals in app sources should stay empty.
- **No shutter or transfer sequences in app source either.** Verbs like
  shutter / enumerateObjects / getObject are connection-specific recipes —
  the PCSS shutter is a 3-beat `0xD039 + 0x100E` dance, the reference app shutter is
  `0x100E + 0x9022` cleanup. Don't hardcode either; ask
  `action(connection, ActionVerb::<verb>)`. Read `.triggers` (e.g.
  `[ImagePushed]`) to plan UX side-effects without per-transport knowledge —
  the camera-knowledge that's coming next stays out of your code.
  See `docs/plans/action-verbs.md`.
- **Settings UI filters on `Property.kind`.** Props tagged `kind: scaffold`
  (the wireless-tether `0xD039 / 0xD21C / 0xD207` virtual-shutter +
  keepalives) look writable on the wire but are protocol mechanics, NOT
  user-facing values. Don't surface them in settings UI; a generic
  set-prop-by-name path must skip them.
- **Secrets stay out of the bundle.** Access-gate material (the XLV bearer token, BLE
  pairing secrets) is **not** in `camera-config-data` and never comes over this surface
  — your app supplies it out-of-band (a private overlay). The manifest only says *that*
  a token is required, not how to mint it.

## 8. Validate against the simulator

`services/camera-sim-service` runs the same manifest as a responder (IPv6 + control
HTTP). Point your app at it to exercise connect / live-view / browse / download without
a physical camera, and to A/B the FFI path against your legacy codec before cutover.

## 9. Pull-model surface — manufacturer index (BLE-MVP)

The seam the **greenfield iOS rewrite** consumes. Same `ConfigStore`, different
constructor + a few new methods. The app pushes observations to the FFI and gets
decisions back — **no UUIDs, byte literals, or model names in app source.**

Authoritative spec: `docs/plans/ios-rewrite-p0-p1-ble-mvp.md` (§11 is the contract
tiebreaker). Handoff for the iOS planning agent: `docs/handoff-ios-ble-mvp.md`.

### 9.1 Load (manufacturer index + every model body it references)

```swift
let store = try ConfigStore.fromManufacturerIndex(
    indexYaml: bundleString("fuji/index.yaml"),
    modelBodies: [
        KeyValue(key: "gfx100ii", value: bundleString("fuji/gfx100ii/gfx100ii.yaml")),
    ]
)
```

Fail-fast: missing body → `MissingModelBody`; unknown family → `UnknownFamily`; bad
YAML at any layer → `IndexParse` / `BodyParse`.

### 9.2 The four pull-model calls

| call | gives you |
|---|---|
| `recognize(observation)` | `Recognition::Candidate{model, connection, confidence, runtimeScope}` / `Disambiguate{family, candidates, runtimeScope, hint}` / `NoMatch`. `runtimeScope` is `Vec<KeyValue>` carrying the signature's derived facts (`style: "legacy"`, `pairingKeyBytes: "44732a80"`, …). |
| `establishment(model, connection, initialScope)` | `EstablishmentPlan { planHandle, mechanism, prerequisite?, steps: [Step] }`. `initialScope` is typically the `runtimeScope` from a `Candidate`. |
| `refineEstablishment(planHandle, firmware, scope, nextStepIndex)` | the *unwalked tail* (steps from `nextStepIndex` onward) with firmware overlays applied — per §11.5. Returns `nil` when no overlay matched; dispatcher keeps existing plan (graceful degrade). MVP stub always returns `nil`. |
| `connectionEstablishment(connection)` | (unchanged renamed §2 method — single-body connection bring-up) |

### 9.3 The 7-verb Step grammar

You build a small dispatcher; the verbs come from the FFI. Each carries
`StepOptions { tolerant, retries, retryDelayMs }` — wrap each verb body in one
retry loop and the same code handles all of them.

| verb | what to do |
|---|---|
| `bleConnect` | connect to the peripheral your I/O primitive captured at recognize time. *No parameters* (§11.4 — peripheral binding is app-side). |
| `bleRead` | read the resolved UUID, decode per `encoding`, store in scope under `captureAs`. |
| `bleWrite` | resolve `value` → bytes (see StepValue table), write. |
| `bleSubscribe` | enable CCCD on the resolved UUID; success on descriptor-write ack — no notification payload is waited for. Use for CCCD-only finalization rounds where the camera advances on the write callback itself. |
| `bleNotify` | subscribe AND wait for `until` (Any / Equals / Matches), optionally `captureAs`. Use when the plan needs to capture or gate on a notification payload. |
| `acquire` | run inner step (`from[0]` — `Vec<Step>` of length 1; uniffi 0.31 doesn't accept `Box<Step>` for recursive enums), bind result to `name`. |
| `acquireFirmware` | read fw via `AcquireSource`, then call `refineEstablishment(...)`. |
| `if` | evaluate `condition` (`Predicate{field, op, value}`) against scope; walk `thenBranch` or `elseBranch`. If `tolerant: true` and the predicate's `field` isn't in scope, evaluate `false` rather than erroring. |

### 9.4 StepValue resolution

| variant | bytes by |
|---|---|
| `Literal{bytes}` | verbatim |
| `Template{value, transform?}` | substitute `{name}` against scope ∪ runtimeParams, then apply transform |
| `Runtime{slot, encoding?, transform?}` | look up `slot` in runtimeParams, decode per encoding, apply transform |
| `Captured{name, transform?}` | look up `name` in scope, apply transform |

`transform`: an allowlisted post-resolution byte transform —
`ValueTransform.bitOr(operand)` or `bitAnd(operand)`. Applied to the assembled
bytes as a u32. Models the RED `F557D96B` echo (read 4 bytes → `value | 0x20000000`
→ write); not iOS-specific (reference app Android does the same OR). The transform lives in
the schema, not in your dispatcher logic — you just honour it.

`runtimeParams` is a separate map you populate at walk start (terminal name, host
IP, anything app-supplied), distinct from `scope` (recognize-seed + step captures).

### 9.5 Zero camera knowledge in app source

If you find yourself hardcoding a UUID, byte literal, or Fuji-specific behaviour
in app source, something's wrong on the ptpsim side — flag it. The whole point of
the pull model is that the app source is identical with one manifest or fifty.

### 9.6 Out of scope (queued for P2+)

USB / mDNS / TCP / UDP / WiFi-join verbs and their I/O primitives.
`promptableModels()`. The `dev-direct` assertive PTP/IP flow. Full PTP session
layer / live-view socket. The BLE→WiFi-AP handover (`fuji-ble-to-wifi-ap-v1`).
Adding any of them is one schema verb + one dispatcher case — no other layer
changes.
