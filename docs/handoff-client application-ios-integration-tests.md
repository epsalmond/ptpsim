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

This doc is the TEST-TIME use of the same ptpsim image, from XCTest.

WHAT PTPSIM PROVIDES:


  Multi-arch — native arm64 on Apple Silicon Macs (fast), amd64 on Intel Macs.

  woodpecker.md). Reach the registry by being on the tailnet + trusting the
  step-ca root cert (install once per dev Mac and CI runner).
- Baked into the image: the GFX100 II rich manifest at
    /etc/ptpsim/gfx100ii.consolidated.yaml
  (GENERATED — same source of truth the production app's FFI consumes; do NOT
  hand-edit and do NOT fork a test-only copy). Default CMD already references
  it, so 'docker run image' is healthy out of the box.
- Per-scenario run shape (ephemeral ports for parallel tests; loopback works
  on macOS host or simulator since both are local):
    docker run -d --rm \
      --name ptpsim-<test-uuid> \
      -v <per-scenario DCIM root>:/var/lib/ptpsim/media-root:ro \
      -p 127.0.0.1:<cmd>:55740 \
      -p 127.0.0.1:<evt>:55741 \
      -p 127.0.0.1:<lv>:55742 \
      -p 127.0.0.1:<ctl>:8080 \

        --manifest    /etc/ptpsim/gfx100ii.consolidated.yaml \
        --media-root  /var/lib/ptpsim/media-root \
        --profile     fuji/gfx100ii \
        --instance-id <test-uuid> \
        --connection  app \
        --command-bind  '[::]:55740' \
        --event-bind    '[::]:55741' \
        --liveview-bind '[::]:55742' \
        --control-bind  '0.0.0.0:8080'   # REQUIRED — XCTest reaches it from
                                         # outside the container's netns
  --control-bind defaults to 127.0.0.1:8080 (loopback-only) so 'docker run' is
  smoke-testable but unreachable from the host; override to 0.0.0.0 for tests.

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
0. One-time per dev Mac + CI runner: be on the tailnet; install the step-ca
   root cert into the Docker daemon's trust store (per the registry contract


1. Pick the pinned image tag for the test suite (e.g. :sha-<8> recorded in
   the test target as an Xcode build setting or test-resource file). 'docker
   pull <tag>' once at suite start; cache locally.
2. Add a test fixture (XCTestObservation, a custom XCTestCase base class, or
   a TestPlan setup phase) that, per scenario:
   - picks free ephemeral host ports (lsof / SocketKit) for command/event/
     live-view/control
   - 'docker run -d' the pinned tag with -v over the per-scenario DCIM root
     and -p for the four ports (see Run shape above)
   - polls 'GET http://127.0.0.1:<ctl>/healthz' until 'ok:true' (typically
     <500 ms after run) before letting the app try to connect
   - 'docker stop' (or POST /shutdown then SIGTERM) on teardown; --rm removes
     the container; reap the docker-run pid unconditionally
3. Point the app under test at the spawned instance. The app already has a dev
   entry point ('connectToDevCameraHost(...)' per DESIGN.md "Relationship To
   The Fixture TUI") that skips BLE/Wi-Fi and opens PTP/IP directly — that's
   the right seam. Pass in 127.0.0.1:<cmd> (the published port).
4. Stage media fixtures per scenario (committed under the iOS test target or
   produced by an XCTest resource), bind-mounted RO into the container:
     <scenario>/DCIM/100_FUJI/DSCF0001.JPG
     <scenario>/DCIM/100_FUJI/DSCF0002.RAF
     <scenario>/DCIM/100_FUJI/CLIP0001.MOV   # for >4 GB ceiling test
   The sim enumerates whatever's in the media root via camera-media-store.
5. Write tests that exercise the app's WORKFLOWS, not protocol details:
   - connect → device info → live-view-stream-arrives
   - connect → image-import → enumerate handles → download a JPEG → checksum
   - connect → live-view → step ISO/aperture → verify readback
   Assert on app-observable outcomes, not on ptpsim's sequencing.
6. CI: pre-pull the pinned tag in a setup step. Keep the version that tests
   pin against EXPLICIT (the image's :sha-<8> tag + the manifest hash you
   record on test-suite start) — the manifest evolves; tests should pin a
   known surface.

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
