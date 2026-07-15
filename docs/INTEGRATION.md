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
| `ConnectionInfo.command_listener_volatile` | whether closing the active PTP/IP transport may remove the command listener, so a consumer must not assume it can immediately redial the same endpoint as generic recovery. A manifest-authored outer connection re-establishment may create a new listener. |
| `connection_establishment(connection)` | how to bring a connection up (PCSS knock ports, BLE→Wi-Fi handover) **as data — you drive the I/O** *(renamed from `establishment(connection)` — the bare name now belongs to the pull-model flow §9)* |
| `pcss_rendezvous(connection)` + `build_pcss_discovery` / `parse_pcss_notify` / `build_pcss_callback_ack` / `build_pcss_init` | typed callback/knock policy and byte-exact PCSS packet codecs. `NOTIFY` supplies the camera address and command port; neither is inferred from a captured default. |
| `run_pcss_auto_establishment` / `run_pcss_known_address_establishment` | Rust-owned PCSS discovery and establishment. Auto-discovery broadcasts only to identify an address, then converges on the same unicast rendezvous used for saved or manually supplied addresses. The host implements raw socket I/O through `PcssExecutorTransport`. |
| `port_for_role(connection, role)` / `socket_bindings(connection)` | the port to bind for a socket role (`command` / `event` / `liveView`) — bind by role, not by the Fuji command port + `+1`/`+2` offsets. `None` = the connection has no such socket (e.g. poll-based `wireless-tether` has no event socket) |
| `camera_initiated_transfer(model)` | BLE trigger states, optional/cached handoff, resolved endpoint, reserved count/head, metadata/data operations, chunk limit, and completion policy for the camera-controlled pull queue. Requires a manufacturer-index store so symbolic GATT names resolve. |
| `transport_close(connection)` | the manifest-resolved frame plus its declared `when` context (Fuji `app`: the 8-byte sentinel before image-transfer re-establishment), `None` when absent; malformed sentinel data is an error. Use it only in the declared context; sending it does not by itself guarantee that the endpoint is immediately redialable. |
| `modes(connection)` / `capabilities(connection, mode)` | the modes + what they can do |
| `detect_mode(connection, observed)` | which mode the camera is in, from props you read |
| `mode_entry(connection, from, to)` | a closed execution plan: PTP wire steps, a manual instruction, or an outer connection re-establishment that exits the old session and reuses the target mode's cold entry |
| `action(connection, verb)` | the parameterized recipe for a verb (e.g. `shutter`, `getObject`) — `Action.params` names runtime slots to bind, `Action.steps` is the wire sequence, `Action.triggers` declares post-conditions (e.g. `objectsAvailable { min, max }` for PCSS shutter queue growth). A `PtpU32Array` capture binds a count-prefixed property reply as a collection; `ForEach.collection` names that captured slot and performs no additional read. See `docs/plans/action-verbs.md` and schema §11.22. |
| `selected_object_transfer(connection)` | typed lazy-gallery projection of the canonical `importObjects` per-handle preparation plus the existing chunk-read action; exposes the preparation-step index whose response is ObjectInfo and manifest-owned u64 transfer-size/u32 chunk-size slots without requiring consumers to inspect nested action ASTs; returns a contract error when a connection declares the actions with an invalid shape |
| `object_transfer_contract(connection)` | transfer strategy (`chunked` or `wholeObject`), resume policy, read/completion actions, completion timing, and per-format confidence. A completion action is eligible only after the host has atomically committed the object locally. |
| `operation_available(connection, mode, op, observed)` | `Available / WrongMode / WrongConnection / Blocked / Unavailable` |
| `control_for(connection, mode, prop)` | the set-mechanism (absolute vs vendor-step — differs by connection) |
| `control_surface(connection, mode)` | semantic control roles mapped to manifest-owned properties, write effects/evidence state, effective owner, and the existing set/readback mechanism. `descriptorOnly` and other non-confirmed effects must be presented as experimental and verified by readback. |
| `property_value_width(prop)` | the manifest property's generic scalar encoder width (`u8`, `u16`, `u32`, `i16`, or `i32`), or `None` for non-scalar/unknown types |
| `value(key)` / `value_label(prop, value)` / `decode_property(prop, raw)` / `encode_property(prop, label)` | value-policy resolution, human labels, and manifest-backed property label↔wire-byte encoding |
| `encode_property_text(prop, value)` / `encode_structured_integer_property(prop, values)` | PTP `STR` encoding. The structured form validates manifest-declared field count and separators without inventing model-specific limits. |

`PropertyValueInfo.evidence` preserves provenance per enum row. Consumers can
therefore distinguish directly exercised values from accepted reference-defined
rows even when both belong to one semantic property.

Byte codecs (build/parse PTP packets, decode datasets, encode values) are exported
functions — the **G1–G3 set is complete** (see §6).

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

# 2. Build the host-only bindgen tool ONCE. It is a separate package so its
#    CLI dependencies are never cross-compiled into an iOS/Android target.
cargo build -p ptpsim-uniffi-bindgen --bin uniffi-bindgen
BINDGEN=target/debug/uniffi-bindgen

# 3. Pick the .a (or .so on Linux) and generate.
LIB=target/release/libcamera_protocol_ffi.a   # .so on Linux; .a is canonical on macOS

# Swift (iOS / macOS)
"$BINDGEN" generate -l swift  -o generated/swift  "$LIB"

# Kotlin (Android)
"$BINDGEN" generate -l kotlin -o generated/kotlin "$LIB"

# Python (Linux — consumed by the standalone camera-protocol-mapper repo)
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
| `cargo run --bin uniffi-bindgen` hangs over non-interactive SSH | Cargo's stdin forwarding to the child process misbehaves over headless ssh. Build the binary once (`cargo build -p ptpsim-uniffi-bindgen --bin uniffi-bindgen`) and invoke `target/debug/uniffi-bindgen` directly. |

## 4. Build + package the native library (per platform)

`camera-protocol-ffi` is `crate-type = ["lib", "staticlib", "cdylib"]`. Standard
per-platform packaging:

- **iOS:** build the **staticlib** (`.a`) for device arm64 plus arm64 and
  x86_64 simulators, `lipo`-combine the simulator archives, then
  `xcodebuild -create-xcframework`. **Verified end-to-end recipe in
  `docs/plans/ios-rewrite-p0-p1-ble-mvp.md` §11.11** — that's the recipe
  the explicit Woodpecker promotion workflow ships; reuse it for local builds.
  Each promoted release
  publishes `CameraProtocolFFI-<sha8>.xcframework.zip` + a `.checksum`
  sibling for SPM `binaryTarget(url:, checksum:)`; consumer-side wiring
  is **`docs/SPM_INTEGRATION.md`** (one-line render via
  `ci/spm-snippet.sh <sha-tag>`). See `docs/APPLE_FFI_RELEASES.md` for the
  supported-slice and pinning policy. macOS is not a current release target.
- **Android:** build the **cdylib** (`.so`) per ABI (`aarch64-linux-android`,
  `armv7-linux-androideabi`, `x86_64-linux-android`), compile the uniffi `.kt`
  to a `classes.jar`, and wrap both (+ `AndroidManifest.xml` + `jni/<abi>/*.so`)
  as a real Android Archive (`CameraProtocolFFI-<sha8>.aar`) the consumer adds as
  a Gradle file dependency (`implementation(files("libs/…​.aar"))`) plus the JNA
  `@aar`. **Consumer-side: `docs/ANDROID_INTEGRATION.md`.** Built by
  `ci/build-android.sh` on the `ci-android` image (`kotlinc` + JNA; `android.jar`
  from the cimg Android image).
- **Linux / Python:** the `.so` + the generated `camera_protocol_ffi.py`. Consumed
  across a repo boundary by the standalone `epsalmond/camera-protocol-mapper` (probe
  tooling), which pulls the generated binding — it is no longer built, tested, or
  shipped from ptpsim CI.

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
   here. Bring it up: `connection_establishment(connection)` returns the recipe (knock
   ports, GATT char UUIDs). BLE/Wi-Fi association remains host-owned; Rust-owned
   executors drive BLE and PCSS protocol sequencing over foreign raw-I/O traits. Bind sockets by
   role with `port_for_role(connection, role)` (`ConnectionInfo.command_framing` /
   `event_framing` tell you which codec framing each channel uses).

   PCSS has two entry paths. `run_pcss_known_address_establishment` sends
   `DISCOVERY` directly to a saved or manually supplied camera IPv4.
   `run_pcss_auto_establishment` first sends the same payload to the
   route-selected subnet broadcast address, acknowledges a recognized callback,
   then invokes that same known-address unicast path. Broadcast is address
   discovery, not a prerequisite for PCSS. Both paths bind the callback listener
   before sending, parse `DSC`, `CAMERANAME`, `DSCPORT`, and `SERVICE`, acknowledge
   the callback, and connect to the advertised `DSC:DSCPORT`. The executor retries
   a typed Device Busy InitFail with an identical request on the same command
   socket; other InitFail reasons remain fatal. Captured application delays are
   not protocol constants.
3. **Enter a mode.** `mode_entry(connection, from, to)` returns one
   `ModeEntryExecution`: execute `Ptp.steps` with `run_mode_entry` over the
   current transport, surface `UserInstruction`, or orchestrate
   `ReestablishConnection` as described below. Each PTP step may be `tolerant` (a
   non-OK PTP *response* is advisory — log + continue; only a transport failure
   aborts). A `retry` step reruns its complete nested sequence only for the exact
   manifest-declared PTP response codes; transport failures and unselected
   responses escape immediately. `sendOp` `params` are literals **or** a named runtime slot
   (`EntryParam.Runtime { slot }`, e.g. `openCaptureTxId`) that **you** bind from
   your session state — ptpsim names which runtime value goes there; it never
   computes it.

   `ReestablishConnection` is an outer lifecycle, not a PTP step. Execute its
   `exitSteps` on the old session, including any orderly transport close; release
   the current network association; obtain `establishment(model, connection, ...)`;
   walk `postExitReadiness` and then `steps` with the supplied
   `establishmentParams`; associate to the resulting network; open a fresh PTP/IP
   session with reset transaction state; then execute the cold
   `mode_entry(connection, None, to)`. Do not substitute an immediate redial when
   `commandListenerVolatile` is true. A `CloseSession { transportClose: true }`
   closes auxiliary socket roles normally, sends the resolved transport-close
   frame on the command socket after the PTP CloseSession response, flushes it,
   and closes the command socket cleanly.

   Store construction fails closed when a re-establishment has no connection
   establishment mechanism, no cold PTP entry, an incomplete UniFFI step mirror,
   or (for manufacturer-index stores) parameter keys that do not exactly match
   the resolved establishment plan. A consumer never receives a destructive
   partial plan.
4. **Drive controls, gated.** Before any op: `operation_available(...)`. To set a
   value: `control_for(...)` tells you the mechanism; the codec encodes the bytes.
5. **Detect state.** Feed observed prop values to `detect_mode` / `operation_available`
   (the predicate `requires`) — you read them off the wire; the engine evaluates.

For a camera-initiated transfer, retain the latest values for every returned BLE
trigger state and act only when its match rule holds. Open the resolved endpoint,
read the declared count, and execute the returned mode entry. Run the metadata
probe immediately after the count read when `metadata_phases` contains
`afterCountBeforeModeEntry`, and after the mode entry when it contains
`afterModeEntry`; both may be declared. Then read the chunk limit and pull the
fixed queue head. A head completes only after the full data phase and its final
OK response arrive; neither socket close nor a transport sentinel is a
queue-completion signal.

The trigger is descriptive data, not a simulated Bluetooth peripheral. The
current service exposes the PTP pull queue seeded from its media inputs; a
consumer or test fixture supplies BLE/app-state transitions by patching
`camera_initiated_transfer_active` through the generic state-overlay control
surface. Reserved routing is disabled until that state is true, so an ordinary
count read cannot redirect public object lookup. A generic BLE adapter remains
tracked by issue #164.

## 6. Status — what's ready vs pending

- **Ready:** the §A query surface (above) + `operation_available_explained`
  (`GateExplanation` — the resolution trace for telemetry); per-connection socket
  bindings by role + wire framing (`port_for_role` / `transport_close` /
  `command_framing`, #133/#140); the GFX100 II manifest across **all five connections**
  (`app` WiFi-AP, `ble`, `wireless-tether` PCSS, `usb`, `xlv` HTTP); firmware-tier
  overlays via `ConfigStore.from_tiers(body, manufacturer, fw_overlays)` (e.g.
  `fw2.40.yaml` flips XLV to HTTPS).
- **Codecs (§B) — G1–G3 landed + in the bindings.** G1: `build_app_init` (the 82-byte
  init) + `validate_init_ack`; transport-close frames come from `transport_close`. G2:
  `encode_value(raw, width)` (generic 8-, 16-, and 32-bit scalar
  width encoder) plus `ConfigStore.encode_property(prop, label)` /
  `decode_property(prop, raw)` for manifest-backed property value rows and generic
  sentinel/mask forms. Per-value semantics live in data, not app switch tables. G3: packet
  framing `build_command` / `build_data` / `parse_response` / `parse_data_payload` /
  `parse_data_phase` (standard framing streams `StartData`/`Data`/`EndData`; the Fuji
  compressed and USB channels deliver the whole data phase in one type-2 `Data` frame —
  reconciled byte-exact against the wire, #143) / `parse_event`, plus dataset codecs
  `parse_object_info` / `parse_device_prop_desc` / `parse_live_status` (0xd212) /
  `parse_object_handle_list` (0xd621). Framing is selected per call by
  `PtpFraming { Standard | Compressed | Usb }`, which you **read from the manifest**
  (`ConnectionInfo.command_framing` / `event_framing`) — never a `kind→framing` map in app
  source.
- **Property value capabilities are scoped.** `PropertyInfo.value_profiles` carries
  connection/mode-specific legal rows when a camera path does not expose a useful
  `DevicePropDesc` list, or when a descriptor is too broad for a body/mode. Clients should
  filter UI/write choices from those profiles when present, use each row's `raw` as the
  canonical write value, treat `aliases` as readback/input matches, and avoid sending rows
  marked `legal: false`. `value_encoding.masks` carries additional flag/sentinel forms
  alongside the legacy single `sentinel`.
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
  `[ObjectsAvailable]`) to plan UX side-effects without per-transport knowledge —
  the camera-knowledge that's coming next stays out of your code.
  See `docs/plans/action-verbs.md`.
- **Collection reads and iteration are separate steps.** Execute a
  `GetProp` carrying `CaptureSourceInfo::PtpU32Array` through its declared retry
  policy, store the decoded elements under the capture's `bind`, then let
  `FfiLoopKind::ForEach.collection` iterate that collection. Do not re-read the property
  inside the loop or widen the collection retry around its body; a failed body
  must not replay completed object work.
- **Settings UI filters on `PropertyInfo.kind`.** The typed `PropertyKind`
  resolves omitted manifest classifications to `setting`. Props classified as
  `scaffold` (the wireless-tether `0xD039 / 0xD21C / 0xD207` virtual-shutter +
  keepalives) look writable on the wire but are protocol mechanics, NOT
  user-facing values. Don't surface them in settings UI; a generic
  set-prop-by-name path must skip them.
- **Keepalives are actions.** For a connection that declares
  `ActionVerb::Keepalive`, execute that action as the caller-scheduled session
  maintenance iteration. The manifest names the wire writes; it does not encode
  a fixed cadence.
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
| `reconnectPolicy(model)` | The manifest-authored saved-camera scan window. `None` means the model has no automatic BLE reconnect contract. |
| `reconnectDecision(model, observation, persistedScope)` | Classifies a fresh advert for one saved camera as `Wake{plan, runtimeScope}`, `Ready{plan, runtimeScope}`, or `NoMatch`. The manifest owns advert-state recognition, identity keys, and plan selection; callers must not infer readiness from a cached peripheral. |

`Observation::BleAdvert` carries `{ serviceUuids, manufacturerData?:
{ companyId, payload }, serviceData: [{uuid, payload}], localName?,
txPower?, adRecords: [{adType, payload}] }`. Populate every field your
platform exposes and leave the rest nil/empty — signature predicates over an
absent field evaluate false, never error (plan §11.14). `manufacturerData.payload`
is the bytes AFTER the 2-byte company id: split iOS
`CBAdvertisementDataManufacturerDataKey` into `(companyId LE, payload)`;
Android `getManufacturerSpecificData(id)` is already the payload.
CoreBluetooth cannot supply `adRecords` — leave it empty on iOS.
| `establishment(model, connection, initialScope)` | `EstablishmentPlan { planHandle, mechanism, prerequisite?, postExitReadiness: [Step], steps: [Step], activities: [ConnectionActivityDescriptor] }`. Activities contain the plan's executor spans followed by the selected connection's host checkpoints (§11.23). `initialScope` is typically the `runtimeScope` from a `Candidate`. After an orderly feature exit, walk the optional `postExitReadiness` sequence before replaying `steps`; do not infer readiness by negating a launch predicate. |
| `refineEstablishment(planHandle, firmware, scope, nextStepIndex)` | validates the plan handle and returns `NoChange` or `ReplaceTail{steps, activities}` per §11.5/§11.23; replacement activity spans are relative to the returned tail. Invalid handles/indices are errors. Current manifests return `NoChange` because no establishment overlays exist yet. |
| `connectionEstablishment(connection)` | Single-body connection bring-up, including the connection's host-checkpoint activity descriptors (§11.23). |

Saved-camera reconnect is a fresh-observation loop. For each advert, call
`reconnectDecision` with the scope persisted from pairing. `Wake` means walk the
returned wake plan, expect the peer to disconnect while booting, then resume the
scan. `Ready` means walk the returned reconnect plan. `NoMatch` is ignored. Stop
after `reconnectPolicy.scanTimeoutMs` and surface unavailable/retry UI. Startup and
already-paired signatures can be reconnect-only, so they do not appear in normal
`recognize` results.

### 9.3 Walking plans — the Rust executor

The engine walks its own plans (#246). Implement two small foreign traits and
hand them to the executor entry points; Rust owns plan walking,
capture/transform/predicate evaluation, the retry ladder, wall-clock budgets,
`if`/`replaceTail`, and the per-step telemetry stream:

- `BleExecutorTransport` (a `with_foreign` async trait) — raw I/O only:
  `connect` / `awaitDisconnect` / `requestMtu` / `ensureServicesDiscovered` /
  `read` / `write` / `writeWithNotificationFence` / `subscribe` /
  `nextNotification` / `sleep`.
  `awaitDisconnect` resolves when the connected peer drops the link and may
  pend indefinitely — the executor races it against the step's manifest
  `timeoutMs`.
- `StepObserver` — receives the `StepReport` outcome stream: `Started` plus
  exactly one terminal (`Succeeded` / `Tolerated` / `Failed`) per step at every
  nesting level, with a `stepPath` position path (`steps[3].bleWrite`,
  `steps[5].if.then[0].bleRead`). Reports include optional activity id/version
  correlation. Map them onto your diagnostic telemetry bus.
- `ConnectionActivityObserver` — receives the semantic activity stream from
  manifest-declared executor spans: `Started`, `Retrying`, and one terminal
  `Succeeded`, `Failed`, or `Cancelled`. A retry carries its local attempt
  ordinal/limit plus a typed failure whose context contains only
  manifest-selected, decoded scope values. Terminal events carry an
  activity-wide retry summary so consumers can aggregate retries without
  reconstructing them from local ordinals. Host-checkpoint activities are
  driven by the host and are never emitted by this executor.

| call | walks |
|---|---|
| `runEstablishment(store, planHandle, transport, observer, activityObserver, initialScope, initialEncodings, runtimeParams)` | the plan's `steps`, including §11.5 `acquireFirmware` refinement. `initialScope` / `initialEncodings` are the `Candidate`'s `runtimeScope` / `runtimeScopeEncodings` — thread both verbatim so a `{ captured: … }` write-back re-encodes with the capture's true encoding instead of an app-side guess (#43). |
| `runBleAction(store, model, action, transport, observer, initialScope, runtimeParams)` | a named BLE-native control action (#91) over an already-established link; no refinement. |
| `runPostExitReadiness(store, planHandle, transport, observer, activityObserver, initialScope, initialEncodings, runtimeParams)` | the plan's `postExitReadiness` gate. Run it after an orderly feature exit, before replaying `runEstablishment`. A plan whose establishment declares no gate returns immediately with `stepsRun == 0` and touches no I/O; a handle with no establishment at all is `UnknownPlan`, same as `runEstablishment`. |

Activity display roles are consumer-neutral hints: `connecting`,
`waitingForCamera`, `confirmingPairing`, `preparingConnection`,
`startingNetwork`, `joiningNetwork`, and `openingSession`. Unknown future role
tokens cross the FFI as `Unknown { raw }`. `defaultExpectedDurationMs` is a
curated p75-like initial display seed only; it is a conservative estimate, not
a measured guarantee, and MUST NOT be used as an execution deadline. The
executor emits retry ordinals as total-attempt positions (`2` of `3`), keeps an
activity alive across tolerated failures, and emits exactly one `Cancelled` if
its future is dropped while an activity is active (§11.23).
`ConnectionActivityRetry.ordinal` and `limit` apply to the retry primitive that
emitted the event; they may reset when a later primitive retries within the same
activity. `ConnectionActivityTerminalSummary.retryCount` counts every replay
across the complete activity. Its optional `lastRetry` preserves the exact
failure that triggered the most recent replay, including manifest-curated
context captured before the scope advances on a recovered attempt. A terminal
`Failed` event additionally carries the final failure; non-manifest retry paths
have an empty curated context rather than exposing diagnostic strings as
policy. Session correlation, timestamps, camera/client identity, and build
provenance remain host-owned telemetry-envelope fields rather than executor
events.
Firmware refinement preserves one activity lifecycle across the splice when
the first replacement span repeats the active descriptor's id, version, and
metadata; a different identity or version starts a new lifecycle.

### 9.4 Walking PTP entry and action plans

`PtpExecutorTransport` is the raw PTP/IP seam for mode entries and named
actions. The host retains socket ownership, cached session identity, network
association, and outer transition orchestration. Rust owns the `EntryStep`
grammar, transaction ordering, PTP/IP framing, response tolerance, retries,
captures, predicates, loops, deadlines, and outcome streams.

The transport reserves transaction ids, exchanges complete command-channel
frames, pulls complete event-channel frames by requested event code, opens an
auxiliary channel when the plan reaches `openChannel`, closes or reopens the
command session when a plan says so, and provides `sleep(ms)` as the host clock.
The host MUST NOT eagerly open manifest-managed auxiliary channels: their camera
listeners may be unavailable until the preceding PTP operation succeeds. Event
delivery MUST retain nonmatching frames for their normal
consumers; the executor parses and verifies the returned matching frame. The
executor uses the connection's declared command/event framing, including the
standard PTP/IP `StartData`/`EndData` sequence, and races every pending
transport call against that clock. An ordinary step has a 60-second aggregate
budget in addition to the 10-second per-call backstop; `awaitUntil.timeoutMs`
is its aggregate budget. A deadline drops the losing foreign future;
cancelling the whole exported future does the same. Socket reads may therefore
remain pending indefinitely: consumers must not add a second semantic timeout
or retry policy around them.

| call | walks |
|---|---|
| `runModeEntry(store, connection, from, to, transport, observer, activityObserver, runtimeParams)` | a current-session `ptp` mode entry. `UserInstruction` and `ReestablishConnection` fail with `UnsupportedPlan` so the host cannot accidentally skip their outer lifecycle. |
| `runModeReestablishmentExit(store, connection, from, to, transport, observer, activityObserver, runtimeParams)` | only the old-session `exitSteps` of a `ReestablishConnection` entry. Establishment replay, network association, fresh session creation, and the cold entry remain explicit host orchestration. |
| `runAction(store, connection, action, transport, observer, activityObserver, runtimeParams)` | one named manifest action on the current session. |
| `runSelectedObjectPreparation(store, connection, transport, observer, activityObserver, runtimeParams)` | the selected-object prefix projected from the canonical import action, preserving capture bindings for later chunk reads. |
| `runStreamingAction(store, connection, action, transport, sink, runtimeParams, expectedPayloadBytes)` | one compressed, single-`sendOp` data-in action through bounded raw reads. Rust validates the 12-byte data header, streams the exact body to `PtpStreamingSink` in chunks no larger than 1 MiB, then validates the separate final response. |

`PtpExecutionOutcome` returns scalar scope, captured collections, ordered data
outputs (payload plus final response parameters and transaction id), and the
number of completed steps. Runtime values cross the FFI as unsigned 64-bit
values so object sizes and offsets are not truncated. Collection loop bindings
are lexical: the prior value is restored after each element, and a failure on
one element never replays completed elements. A `retry` may not contain a
`loop`; put response-selected retry inside the per-element body so a later
failure cannot restart earlier elements.

`StepReport` is shared by the BLE and PTP executors. PTP reports add optional
operation, property, response-code, and transaction-id correlation plus the
step's declared tolerance. A composite step reports the transaction that
determined its terminal outcome. Raw transport failures are never tolerated;
only a non-OK PTP response may be swallowed by `tolerant: true`, and `retry`
selects only the response codes declared in the manifest.

Scalar property reads always update predicate scope, whether or not they bind a
named capture. A property with a manifest-declared record-stream payload is
decoded into its allowed member observations, so composite polling remains
manifest-driven and does not move camera-specific parsing into the host.

Mode entries and actions may declare complete, ordered `executorSpan`
activities over their top-level steps (§11.24). When present, the PTP executor
emits them through the same `ConnectionActivityObserver` contract as
establishment walking; absent metadata produces no invented activity or
duration. Cancellation emits exactly one `Cancelled` for the active span.

`PtpStreamingTransport` is intentionally separate from `PtpExecutorTransport`:
its `receiveCommandBytes(maxBytes)` must never return more than requested, which
prevents a length-prefixed whole-object frame from being assembled in host or
Rust memory. Each raw read has a 10-second idle deadline; there is no aggregate
whole-transfer deadline. Once the command write begins, cancellation, malformed
or truncated framing, a deadline, or any sink failure invokes
`invalidateCommandSession` synchronously. The host must cancel and poison that
command session because unread compressed-frame bytes cannot be resynchronized.
A fully consumed non-OK PTP response is different: the session is synchronized
and remains reusable. The sink owns temporary-file durability; run the
manifest's completion action only after an atomic local commit.

`planHandle` has the stable form `<model>:<selector>`. Plans obtained through
`establishment(model, connection, ...)` use the connection id as the selector;
`Wake`/`Ready` plans returned by `reconnectDecision` use their establishment
mechanism (`ble-wake`, `ble-reconnect`). Resolution is connection-first: when
the selector names a declared body connection, that connection must declare an
establishment; only a selector that is not a connection falls back to a direct
mechanism lookup. Consumers treat the handle as opaque and echo it unchanged to
`runEstablishment`, `runPostExitReadiness`, and `refineEstablishment`.

Two transport contracts carry the correctness load:

- **Notification buffering.** `subscribe` succeeds on the CCCD descriptor-write
  ack, and from that moment the transport must buffer every notification on
  that characteristic (per-characteristic FIFO) until consumed via
  `nextNotification`. Acceptance logic (until-predicates, seed-read routing)
  lives in the executor — a payload arriving between subscribe and the first
  `nextNotification` call must not be lost. `nextNotification` may stay pending
  indefinitely; the executor owns every deadline (including a backstop on each
  transport verb, so a silently-stalled transport cannot hang a walk). A
  `bleWrite.notificationFence` uses `writeWithNotificationFence`: atomically
  discard that subscribed characteristic's already-buffered prefix immediately
  before issuing the write. Notifications caused by the write remain buffered;
  implementing this as an executor-side drain followed by a separate write is
  invalid because a notification can race into the gap.
- **The host clock.** `sleep(ms)` resolves after `ms` milliseconds of
  wall-clock time. The executor races I/O futures against it for timeouts and
  retry backoff, so the library carries no async runtime of its own.
  Cancellation propagates both ways: when a deadline lapses — or your app
  cancels the whole walk — the dropped Rust future cancels the corresponding
  foreign task through the generated bindings, so make each transport method
  cancellation-safe.

A fatal step returns `ExecutorError::StepFailed { step, kind, detail, context }`.
`kind` is the stable `ExecutorStepFailureKind`: `DeadlineExceeded` covers
executor-owned verb backstops, step-level notification/poll budgets, and a
transport-reported timeout; `ConditionRejected` identifies a manifest-declared
terminal condition; `Other` covers every remaining failure. `context` contains
only the key/value pairs explicitly selected by the controlling manifest step;
the complete executor scope is never exposed. Consumers may map
`DeadlineExceeded` from a readiness gate to retryable UI while keeping
`detail` for diagnostics only. Never string-match `detail` for control flow.
Tolerated failures remain step reports and do not escape as `ExecutorError`.

For an AP-launch establishment, `ConditionRejected` is a camera refusal over
the still-valid BLE link. Keep that link as the resting home: a connected camera
does not advertise, and a subsequent launch is expected to reuse it. Do not
infer the same disposition from `DeadlineExceeded`, `Other`, the step path, or
the diagnostic text. A transport/link failure is not a refusal; release the
failed BLE link before offering a scan/reconnect path.

### 9.4 The Step grammar (reference; legacy dispatcher)

The executor implements this grammar for you — read this section as the verb
reference. It is also the contract for the legacy alternative, a hand-built
app-side dispatcher, if you cannot adopt the executor path. Each verb carries
`StepOptions { tolerant, retries, retryDelayMs }` — wrap each verb body in one
retry loop and the same code handles all of them.

| verb | what to do |
|---|---|
| `bleConnect` | connect to the peripheral your I/O primitive captured at recognize time. *No parameters* (§11.4 — peripheral binding is app-side). |
| `bleAwaitDisconnect` | wait up to `timeoutMs` for the connected peer to disconnect. A disconnect is success; expiry is a step failure. Used by wake plans where connecting to a startup advertisement triggers camera boot. |
| `bleRequestMtu` | request ATT MTU `mtu` before GATT traffic. If your platform has no request API (CoreBluetooth negotiates on its own), treat as a checkpoint: succeed when the negotiated MTU ≥ `mtu`. |
| `bleDiscoverServices` | explicit service-discovery state transition. If your stack auto-discovers, complete when discovery has completed — don't re-trigger. Discovery timeout is your policy. |
| `bleRead` | read the resolved UUID, apply the `transform` chain to the wire bytes (§11.13 — empty chain = no-op), decode per `encoding`, store in scope under `captureAs`. |
| `bleWrite` | resolve `value` → bytes (see StepValue table), write. Optional `notificationFence` names a subscribed GATT characteristic whose buffered prefix the transport atomically fences immediately before issuing this write; notifications caused by the write remain consumable. |
| `bleWriteChunk` | frame and write one manifest-declared window from a runtime blob, using the captured chunk index, frame fields, size, and sentinel index. |
| `bleSubscribe` | enable CCCD on the resolved UUID (`mode`: notify/indicate — CoreBluetooth maps both to `setNotifyValue(true)`); success on descriptor-write ack — no notification payload is waited for. Use for CCCD-only finalization rounds where the camera advances on the write callback itself. |
| `bleNotify` | subscribe (`mode` as above) AND wait for `until` (Any / Equals / Matches); bind whole payload via `captureAs` and/or extract fields via `capture` (window → transform chain → encoding → scope; a failing capture is skipped, not a step failure). |
| `bleAwaitUntil` | observe `source` (poll a `read` characteristic, or consume its `notify` stream) until `until` (a `Predicate` over scope) holds, up to `timeoutMs`. A notify source may set `seedRead`: subscribe + arm notifications, issue one read through the same captures and predicates, then remain notification-only. `seedRead` cannot be combined with `failWhen`, because callback transports cannot reliably distinguish a read response from a racing notification; use an explicit pre-command read plus a notification-only rejection await. Each observation applies `capture`/`captureAs`; `until` wins, otherwise a matching `failWhen` fails as `ConditionRejected`. When `failureEvidence` is present, its `steps` run inside the await budget and rejection is terminal only if its `when` predicate matches fresh evidence; otherwise observation continues. For a notify source, evidence steps cannot read that same characteristic, including through nested control flow; use a separate evidence characteristic so racing callbacks retain unambiguous provenance. If no rejection is confirmed, run `onEach` and observe again. `intervalMs` is the read-poll cadence (ignored for notify). §11.15 — reference semantics in `camera_sim::ble::run_await_until`. |
| `acquire` | run inner step (`from[0]` — `Vec<Step>` of length 1; uniffi 0.31 doesn't accept `Box<Step>` for recursive enums), bind result to `name`. |
| `acquireFirmware` | read fw via `AcquireSource`, then call `refineEstablishment(...)`. |
| `if` | evaluate `condition` (`Predicate{field, op, value}`) against scope; walk `thenBranch` or `elseBranch`. If `tolerant: true` and the predicate's `field` isn't in scope, evaluate `false` rather than erroring. |
| `retry` | run `steps`; when a failure's stable kind equals `whenFailure`, run `onFailure` in the same scope and evaluate `retryWhen`. Retry only when it is true and `maxAttempts` is not exhausted, sleeping `retryDelayMs` first. Unselected failures escape unchanged. Terminal selected failures include only the named `failureContext` values. Repeated subscriptions to the same GATT characteristic and mode are reused within the walk. |

### 9.5 StepValue resolution

| variant | bytes by |
|---|---|
| `Literal{bytes}` | verbatim |
| `Template{value, transform}` | substitute `{name}` against scope ∪ runtimeParams, then apply the transform chain |
| `Runtime{slot, encoding?, transform}` | look up `slot` in runtimeParams, decode per encoding, apply the transform chain |
| `Captured{name, transform}` | look up `name` in scope, apply the transform chain |

`transform`: a `Vec<Transform>` chain from the closed vocabulary (plan §11.13 —
`bitOr`, `bitAnd`, `slice`, `dropPrefix`, `reverseBytes`, `uuidFromBytes`,
`bits`), applied in order; empty = no transform. Reference semantics live in
`camera_config::index::eval::apply_transforms` — implement your dispatcher to
match its unit tests; a failing chain counts as step failure under §11.6.
Models e.g. the RED `F557D96B` echo (read 4 bytes → `value | 0x20000000` →
write); not iOS-specific (reference app Android does the same OR). The transform lives
in the schema, not in your dispatcher logic — you just honour it.

`runtimeParams` is a separate map you populate at walk start (terminal name, host
IP, anything app-supplied), distinct from `scope` (recognize-seed + step captures).

Saved-camera routes are fail-fast too. The loader rejects a reconnect route
without a family policy, a zero scan timeout, an unknown establishment
mechanism, an empty identity list, or an identity key the signature neither
captures nor places in literal scope.

For Fuji, startup-service routes select `ble-wake` (`bleConnect` followed by
`bleAwaitDisconnect`) and awake-service routes select `ble-reconnect`. Both
phases use the manifest's 60-second scan window. A bonded legacy body's awake
advert carries the file-transfer service UUID and a serial-bearing local name,
but no Fuji manufacturer data; its reconnect identity is the captured
`shortSerial`, while the cached `pairingKeyBytes` still seed the reconnect plan.
The manufacturer-data form remains pairing-mode discovery. Clients do not add a
post-registration power-property gate. See issue #264 for the wire evidence.

### 9.6 Zero camera knowledge in app source

If you find yourself hardcoding a UUID, byte literal, or Fuji-specific behaviour
in app source, something's wrong on the ptpsim side — flag it. The whole point of
the pull model is that the app source is identical with one manifest or fifty.

### 9.7 Out of scope (queued for P2+)

USB / mDNS / TCP / UDP / WiFi-join verbs and their I/O primitives.
`promptableModels()`. The `dev-direct` assertive PTP/IP flow. Full PTP session
layer / live-view socket. The BLE→WiFi-AP handover (`ble-establish-wifi-ap`).
Adding any of them is one schema verb + one dispatcher case — no other layer
changes.
