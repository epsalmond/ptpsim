# Handoff prompt — client application iOS: integration testing against ptpsim


> Goal: drive the app's XCTest / dev-loop runs against a local ptpsim instance
> instead of needing a real GFX100 II on the bench. Companion docs:
> [`handoff-ios-agent.md`](handoff-ios-agent.md) (production app adopting the
> manifest/FFI — the *runtime* path) and
> [`handoff-client application-vcam-agent.md`](handoff-client application-vcam-agent.md) (backend


```
You're wiring ptpsim into the client application Apple app's integration-test loop, in

PTP/IP responder as a binary; your scope is the XCTest harness that spawns it,
points the app at it, and tears it down — NOT camera-protocol code.

THIS IS DISTINCT FROM TWO OTHER TRACKS:
- `handoff-ios-agent.md`            — production app adopting the manifest+FFI
                                       (initiator-side refactor; runtime).
- `handoff-client application-vcam-agent.md`  — backend cutover that runs ptpsim per lease

This doc is the TEST-TIME use of the same ptpsim binary, from XCTest.

WHAT PTPSIM PROVIDES (~/git/ptpsim):
- Binary: `camera-sim-service`
    cargo build -p camera-sim-service --release
  Manifest-driven, sans-IO from the iOS perspective (it owns its own sockets).
- Manifest: packages/camera-config-data/fuji/gfx100ii/gfx100ii.consolidated.yaml
  (GENERATED — same source of truth the production app's FFI consumes; do not
  hand-edit and do not fork a test-only copy).
- Run shape for tests (loopback, no NAT64 needed; pick free ports per test):
    camera-sim-service \
      --manifest <path>/gfx100ii.consolidated.yaml \
      --media-root <per-scenario DCIM root> \
      --profile fuji/gfx100ii --instance-id <test-uuid> \
      --command-bind  '127.0.0.1:<cmd>' \
      --event-bind    '127.0.0.1:<evt>' \
      --liveview-bind '127.0.0.1:<lv>' \
      --control-bind  '127.0.0.1:<ctl>'
  Default ports (55740 command / 55741 event / 55742 live-view) only work for
  one instance at a time — pick ephemeral ports per test for parallel runs.
  Mac simulator + macOS host both work over IPv4 loopback; switch to '[::1]'
  for IPv6 parity with App Review.

CONTROL PLANE (test setup/teardown):
- GET  /healthz → 200 {"ok":true,instance_id,profile,bind,sessions,media_root}.
  Use as the readiness gate after spawn.
- POST /shutdown → graceful drain. SIGTERM also drains.
- That's the WHOLE current contract. /scenario/load, /faults etc. are designed
  but NOT implemented — today the only way to vary behavior between tests is
  RESTART-PER-SCENARIO with a different --media-root and/or manifest. Plan
  scenarios as fresh-instance setups, not as live mutations. If you need live
  scenario/fault control, request it upstream.

VALIDATED BEHAVIOR (the believable surface your tests can lean on):
- DESIGN gates #3 ImageImport, #4 LiveView, #5 black-box smoke pass:
  - GetDeviceInfo enumerates ~31 ops / ~324 properties from the manifest.
  - GetDevicePropDesc returns real descriptors for modeled properties.
  - The full ImageImport choreography runs from the manifest, including the
    wire-discovered vendor-prime "chord" (0x9054/0x9055/0x9050/0x9053).
  - LiveView entry (df00=6, df01=0x16, df2a read-echo, 902B×4, 0x101c) lands
    the engine in Streaming.
- Unmodeled ops return PTP NOT_SUPPORTED (0x2005). Treat that as a manifest
  gap to file upstream, not a test workaround.

TASK (iOS side):
1. Add a test fixture (XCTestObservation, a custom XCTestCase base class, or a
   TestPlan setup phase) that:
   - locates the prebuilt `camera-sim-service` (vendored binary, or a build
     phase that runs `cargo build --release` against the local ptpsim checkout)
   - spawns it with per-test ephemeral ports + per-test --media-root
   - polls `GET http://127.0.0.1:<ctl>/healthz` until `ok:true` (typically
     <500 ms) before letting the app try to connect
   - POSTs `/shutdown` in teardown; falls back to SIGTERM if /shutdown didn't
     return within a deadline; reaps the child unconditionally
2. Point the app under test at the spawned instance. The app already has a dev
   entry point (`connectToDevCameraHost(...)` per DESIGN.md "Relationship To
   The Fixture TUI") that skips BLE/Wi-Fi and opens PTP/IP directly — that's
   the right seam. Pass in the chosen command-bind host:port.
3. Stage media fixtures per scenario (committed under the iOS test target or
   produced by an XCTest resource):
     <scenario>/DCIM/100_FUJI/DSCF0001.JPG
     <scenario>/DCIM/100_FUJI/DSCF0002.RAF
     <scenario>/DCIM/100_FUJI/CLIP0001.MOV   # for >4 GB ceiling test
   The sim enumerates whatever's in the media root via camera-media-store.
4. Write tests that exercise the app's WORKFLOWS, not protocol details:
   - connect → device info → live-view-stream-arrives
   - connect → image-import → enumerate handles → download a JPEG → checksum
   - connect → live-view → step ISO/aperture → verify readback
   Assert on app-observable outcomes, not on ptpsim's sequencing.
5. CI: build the binary in a setup step (or commit a prebuilt binary keyed by
   ptpsim git sha). Keep the ptpsim version that tests pin against EXPLICIT
   (a manifest hash + binary sha you record on test-suite start) — the
   manifest evolves; tests should pin a known surface.

HARD CONSTRAINTS:
- The sim is the RESPONDER; your tests are the INITIATORS — they own every
  socket / connection / sequencing decision. ptpsim never reaches into iOS.
- The MANIFEST is shared with the production app (the FFI loads the same YAML).
  Do not fork a test-only manifest. If a scenario needs a wider/narrower
  surface, either curate it upstream (manifest PR) or build the per-test DCIM
  tree to exercise the surface that's already there.
- Restart-per-scenario is the contract today. Don't build a Swift-side
  "scenario switcher" that pokes ptpsim internals — it has no such surface.

ANTI-PATTERNS:
- Asserting on hex opcodes/prop codes in Swift test code: that's the same
  anti-vcam pattern handoff-ios-agent.md warns about. Assert on the
  manifest's vocabulary (intent + capability), not on PTP bytes.
- Sharing one instance across unrelated tests for "speed": you lose isolation
  and state contamination becomes flaky-test debugging. Spawn per scenario.
- Reimplementing protocol expectations in Swift mocks: if a real camera and
  the sim disagree, the manifest is the contract — fix the manifest.

BACKGROUND:


  manifests, not state machines.

  seam and `connectToDevCameraHost()` (the test redirection point).

  consumes the FFI (same manifest data your tests target).
```
