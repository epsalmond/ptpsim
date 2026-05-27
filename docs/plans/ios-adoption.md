# Plan: Adopt ptpsim as client application iOS's PTP Protocol Layer (sans-io FFI)

Status: drafted 2026-05-25, iterating. Sibling to the ptpsim core-build plan.

## Intent (what this refactor is FOR)
This is an **app refactor** that extracts camera quirks and control codes out of
app source into **configuration** (the `camera-config` manifest). The point is to
make adding/fixing camera support a **data change** — reviewable, and eventually
OTA-able — instead of an app-release-gated code change. That is the payoff: faster
iteration and faster support rollout. The value accrues **incrementally** — every
quirk/opcode/mode moved into the manifest is one more thing that is, from then on, a
config change, not a Swift edit. So this is not all-or-nothing; partial extraction
already pays.

## Resolved decisions (this revision)
- **Scope: Phase A fully, Phase B gated.** Plan and ship Phase A (extract the codec
  + gating into the manifest/FFI, prove parity, delete the Swift codec). The A→B
  boundary is a decision gate decided *after* A lands.
- **Iterate by parity UNIT TESTS, not a runtime feature flag.** Stand the
  manifest/FFI path up *beside* the legacy Swift codec and assert they produce
  identical bytes/decisions — table-driven over the golden corpus + the full option
  lists. The dev loop is `cargo test`/`swift test`, not a deploy + field A/B (too
  coarse, too slow). Delete the legacy path when the table is green. A flag, if it
  survives at all, is just the final cutover toggle — **not** the iteration mechanism.
  *Final confidence* is a separate, integration-level check (connect/live-view/
  download against the simulator + a real camera) — still no coarse runtime flag.
- **One-time directory reorg, up front.** The tree was organized around protocol
  living *in* the app; that assumption is now inverted. Reorganize to reflect the new
  boundary: (a) the generated FFI bindings module, (b) the I/O/transport layer that
  STAYS Swift (sockets/USB/BLE/Wi-Fi), (c) orchestration, (d) the legacy codec,
  quarantined so its deletion is clean. The app agent owns the layout; deliberate
  early step, not a rename-everything detour.
- **The app agent owns the client application-side FFI tooling.** Generating bindings, building/
  packaging the xcframework, AND wiring it into the client application build are theirs. ptpsim
  provides the crate + the `uniffi-bindgen` binary + `docs/INTEGRATION.md`.
- **Init packet: manifest data.** The Fuji reference app `InitCommandRequest`'s 26-byte
  name field + 28-byte `liveViewInitTail` live in the camera manifest
  (`transports.init`: `friendlyNameLength`, `tailHex`), NOT a hardcoded Rust
  constant. Works whether or not it varies across firmwares; differing cameras
  carry different data. → closes open decision #4.

## Context

ptpsim is two artifacts; **this plan is about the LIBRARY**: `ptp-core` +
`camera-manifest` + `protocol-primitives`, shipped embedded in the app via a
uniffi FFI boundary. The SIMULATOR service (`camera-sim-service`) is a separate
optional CI/test target (could later replace `FixtureCameraBackend`) — out of
scope here.

**Hard constraint — sans-io.** ptpsim does no I/O: no sockets, no CoreBluetooth,
no WiFi. The app owns every byte on the wire and every OS API. ptpsim only
(a) intents→outbound bytes, (b) inbound bytes→decoded events/property updates,
(c) capability queries against the manifest.

## Three findings from the code that shape everything
1. **The app reaches around the seam.** `FixtureDemoViewModel` holds a concrete
   `RealCameraPTPIPSession` (line ~625) and calls methods NOT on
   `CameraAPISession`: `stepAperture/stepShutterSpeed/stepExposureBias`
   (delta, return `FujiCameraPropertySnapshot`), `nextLiveViewFrameResult()`,
   rich `autofocus`. → Keep `RealCameraPTPIPSession`'s concrete signatures;
   refactor its guts, don't reroute through the narrow protocol.
2. **Compressed command framing already matches Rust byte-for-byte** — golden
   `open-session-request` (`10000000010002100100000001000000`) proves it. Flip
   this first.
3. **The 82-byte reference app `InitCommandRequest` does NOT match `ptp-core`** — Swift
   emits fixed 26-byte name + 28-byte tail; ptp-core has variable init. This is
   gap G1, the parity blocker, flipped last behind its own sub-flag.

## What ptpsim has vs must be built
Has (verified): `ptp-core` (standard PTP/IP framing, all containers, datasets,
PTP UTF-16 strings, registries; decode∘encode identity); `fuji_framing`
(compressed channel, byte-exact OpenSession); `camera-manifest`
(`supports_operation`, `control_for` intent→mechanism, `value_label`); golden
harness + extractor + corpus.

Must build:
- **G1 — Fuji reference app init variant** in `protocol-primitives` (`fuji_app_init`):
  fixed-82-byte init driven by manifest `transports.init` data
  (`friendlyNameLength=26`, `tailHex`); `validateInitCommandAck`.
- **G2 — Fuji value codecs:** `encode_aperture`(×100), `encode_iso`,
  `encode_shutter_speed`(`0x80000000|denom*1000`), `encode_exposure_bias`, ISO
  `0x7fffffff` manual-flag normalization, labels. None exist in Rust today.
- **G3 — parse helpers/quirks:** `parse_live_status` (0xd212 bundle → quirk),
  `parse_object_handle_list` (0xd621), Fuji `parse_object_info` (52-prefix
  variant), `parse_event`; RAF preview parse → `camera-media-store`.
- **G4 — `camera-protocol-ffi` uniffi crate** (today a placeholder with no
  uniffi dep): the whole FFI surface + Swift binding generation + xcframework.
- **G5 — GFX100 II manifest** carrying ops/props/labels/quirks the app gates on
  (import quirk 0xd620/0xd621 ≠ GetObjectHandles 0x1007; >4GB ceiling;
  transports.init data per the resolved decision).
- **G6 — manifest→Swift codegen** for static tables (labels, code constants,
  capability gating).

## FFI surface

### Phase A — codec-level (pure functions, no session state)
`build_command/build_data/build_app_init_request(G1)/parse_response/
parse_data_payload/parse_event`; `parse_device_prop_desc/parse_object_info/
parse_object_handle_list/parse_live_status`; value codecs (G2);
`supports_operation/control_for` (manifest-backed). App keeps ALL orchestration
and I/O; each `FujiPTPIP.buildX/parseX` call site swaps to FFI behind the flag.

uniffi VERIFY-before-relying: Vec<u8>↔Data copy cost for 12 MiB chunks (keep
chunked); `interface`/Arc object + interior Mutex for Phase B; prefer sync
`poll()` over callbacks; async support of installed uniffi version; xcframework
build wired into XcodeGen `project.yml` (no precedent).

### Phase B — sans-io session driver (gated, decided after A)
Stateful uniffi `PtpSession` owning the PTP **transaction-ordering** state
machine (NOT the iOS BLE/WiFi parts): `start/set_aperture/step_aperture/
trigger_shutter/autofocus/begin_download/feed(bytes)/poll()->[Action]` where
Action = SendOnCommand|FrameReady|PropertyUpdate|DownloadChunk|Done|Error. App's
`send`/`receivePacket` become the I/O pump. NOTE: DESIGN forbids sharing the
*responder* (`camera-sim`); a Phase B *initiator* driver is a NEW component
(propose `camera-initiator`) — open decision #1.

## Generated Swift tables vs runtime FFI
- **Static (G6):** value labels, editable-property catalog metadata, capability
  gating keyed by (model, fw, workflow), CameraFeatureMode mode values
  (0400/0300/1600/1400). No opcodes the app branches on.
- **Runtime FFI:** anything touching bytes (build/parse), value *encoding*
  (kept in Rust to avoid drift), `control_for`/`supports_operation`.

## Migration order (parity-unit-test-driven; buildable each step)
Pre-work (ptpsim, no app change): G1–G6 + golden fixtures for every Swift packet test.
App Phase A — the iteration loop is **parity unit tests**, not a runtime flag:
0. **Directory reorg** (above): carve out the FFI bindings module, the Swift I/O
   layer that stays, orchestration, and a quarantined legacy-codec target.
1. **FFI tooling (app side):** generate Swift bindings + build/package the xcframework
   + wire it into the client application build (XcodeGen `project.yml`, no precedent). Embed the
   `camera-config-data` bundle. (`docs/INTEGRATION.md` §3–4.)
2. **Two implementations, side by side:** keep the legacy Swift codec; add the
   manifest/FFI path behind a `PTPCodec` Swift protocol (`LegacyFujiPTPIPCodec` vs
   `FfiCodec`). This is the test harness, not a runtime toggle.
3. **Drive convergence by table-driven parity tests** over the golden corpus + the
   full option lists: assert legacy and FFI paths produce identical bytes/decisions.
   Iterate in `swift test`/`cargo test`. Work family by family (compressed build/parse,
   value encoders, dataset parsers, then init G1) — each "done" = its parity table green.
4. **Delete the legacy codec** (`FujiPTPIP.swift ~459–1670`) once every parity entry
   passes; shrink `FujiPTPIPTests` to FFI-parity tests. → anti-vcam gate.
5. **Integration validation** (final confidence, separate from the loop): full
   connect/live-view/download against `camera-sim-service` + a real GFX100 II on the
   FFI path. No coarse runtime A/B flag required — you just run the new path.
App Phase B (optional, after A): replace RealCameraPTPIPSession transaction
sequencing with the PtpSession feed/poll pump; keep concrete public methods as thin
wrappers so FixtureDemoViewModel is untouched.

## Dev iteration loop (fast config iteration)
The app always resolves **locally** (embedded FFI, sans-io) — never remotely — so the
loop tunes the exact path that ships. Two pieces:
- **Hot-reload the bundle from the existing TUI** (the terminal state injector for
  simulator builds) on demand — re-run `from_bundle` on the fetched/edited manifest, no
  rebuild. Edit config → TUI-reload → retry, in seconds.
- **Capture the engine's resolution-trace into telemetry** (`camera-config.md` §5b):
  an error and the manifest's answer that produced it surface together, so you see "what
  the manifest delivered" and edit accordingly.
Automated mutate→observe→converge iteration is a protocol-mapper concern (a gRPC resolver
wrapping the sans-io engine over the simulator), NOT an app path — avoids dev/prod skew.

## Parity harness
Single source of truth = `packages/protocol-spec/golden/*.yaml`. Extend with
Swift test vectors (the pinned 82-byte init hex, OpenSession, GetPartialObject,
SetDevicePropValue, DevicePropDesc enum/range, ObjectInfo, 0xd621 list, 0xd212
bundle, events). Rust: extend `golden.rs`. Swift: new `GoldenParityTests.swift`
reads the SAME corpus, asserts both legacy and FFI codecs match `bytes_hex`/
decode. Encoder parity: table-driven over the full option lists. Gate: deletion
blocked until every entry passes in both languages.

## Acceptance gate (anti-vcam)
After Phase A deletion: grep for PTP hex literals (0x9.., 0x10.., 0x5.., 0xd..,
mode 1600/1400/0400/0300) in `Sources/` returns nothing the app branches on;
adding a model/fw is a ptpsim manifest+capture+golden change with ZERO Swift
edits. Encode as a CI lint + an "add-a-camera" runbook touching only ptpsim.

## Out of scope
camera-sim-service replacing FixtureCameraBackend; CoreBluetooth/WiFi/socket I/O
(the radio + Network.framework execution stays per-platform);
UI/telemetry/image library/runtime clients; iOS-orchestration parts of BLE/WiFi
state machines; BLE-as-ptpsim-transport. NOTE refinement: the BLE GATT
UUIDs/handshake *values* in FujiBLERegistration are manifest data (establishment
section, shared cross-platform) — only their *execution* is out of scope here.

## Manifest schema requirements (folded in 2026-05-26)
The manifest is declarative data; a finite-vocabulary engine resolves it (see
`docs/plans/manifest-system.md` for the cross-cutting architecture). Two schema
features this refactor needs:
- **Value-resolution-policy** — a property value can be `{type: fixed, value}`,
  `{type: generated, scheme, persist}`, or `{type: from-pairing, source}`. This
  is how the init identity, init tail, and per-pairing values are all expressed
  uniformly. → supersedes "init identity stays an app constant": the initiator
  GUID/name is a **manufacturer-tier `fixed` value-policy**, not Swift code.
- **Establishment section** — values + a direction-neutral `workflow` for
  connection establishment (BLE GATT UUIDs/handshake, PTP-over-HTTP, port-knock).
  The values/workflow are manifest data (shared cross-platform); only the radio/
  socket I/O stays per-platform. ptpsim's PTP engine never reads this section —
  it's for clients + future establishment drivers.

## Open decisions remaining
1. **Phase B initiator driver placement** — new `camera-initiator`
   crate/module vs keep transaction SM in Swift, stop at A. (Recommend: ship A,
   decide after.)
2. **G2 value codecs / G3 quirk parsers home** — `protocol-primitives` (quirk
   registry, per DESIGN) vs a new `fuji`/`camera-value-codec` module. RAF preview
   → `camera-media-store` (settled).
3. **uniffi version & async support** — verify before callback-vs-poll for B.
4. ~~Init packet as manifest data vs constant~~ — RESOLVED: manifest data
   (value-policy `fixed` at the model tier; identity at the manufacturer tier).
5. **Buffer strategy for 12 MiB chunks** over uniffi — measure; keep chunked.
6. **Codegen home (G6)** — `camera-manifest` bin target vs `tools/` generator;
   how generated `.swift` is checked in vs built.
7. **Manifest lives in its own repo** (Apache-2.0 data) with the resolution
   engine extracted as a standalone MIT lib that ptpsim AND clients consume —
   see `manifest-system.md`. Affects where `camera-manifest` lands and the FFI's
   data-loading path. (Direction set; structural details pending.)

## Critical files
- client application `apps/apple/Sources/client applicationCore/FujiPTPIP.swift` (codec to replace:
  constants ~945-1054, encoders ~676-733, build/parse ~1056-1670)
- client application `apps/apple/Sources/client applicationApp/FujiCameraAPISession.swift`
  (orchestration + I/O: intent methods ~1662-1751, startup ~1919-2129,
  send/receivePacket ~3103/3122)
- ptpsim `crates/camera-protocol-ffi/src/lib.rs` (uniffi boundary to build)
- ptpsim `crates/protocol-primitives/src/fuji_framing.rs` (add reference app init + codecs)
- client application `apps/apple/Tests/client applicationCoreTests/FujiPTPIPTests.swift` (golden
  vectors → parity harness)
