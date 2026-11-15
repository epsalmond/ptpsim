---
description: Read-only candidate capability probes with retained wire evidence.
status: reference
read-when: Preparing a static candidate ledger for simulator or camera qualification.
---

# Candidate capability verification

`camera-initiator verify-capabilities` consumes a
`camera-static-candidates/v1` JSON ledger. The ledger is static client evidence.
Every input row and output result keeps `status: unverified`. A successful request
establishes only the observed response in that run. It does not establish a
semantic effect, a firmware range, or support on another body. Simulator runs
validate the probe path and cannot qualify a physical camera.

The default command reads DeviceInfo, then GetDevicePropDesc (0x1014) and
GetDevicePropValue (0x1015) for each eligible property row. It includes explicitly
listed codes omitted from DeviceInfo. It never derives writes from accessor
names or descriptor write flags. The camera descriptor controls value decoding;
a conflicting client datatype hint becomes a reported conflict.

```sh
cargo run -p camera-initiator -- \
  --manifest packages/camera-config-data/fuji/gfx100ii/gfx100ii.consolidated.yaml \
  --connection wireless-tether \
  --camera CAMERA_IP \
  --trace camera-capabilities.trace.jsonl \
  --observation camera-capabilities.observation.jsonl \
  verify-capabilities \
  --candidates packages/camera-config-data/fuji/catalog/tether-app-ff0018.candidates.json \
  --output-dir camera-capabilities
```

Use a fresh output directory and observation path for every run. The directory
contains `capability-report.json`, a copy of the input ledger, DeviceInfo, and
individual raw descriptor/value payloads. Reports retain responses, transaction
IDs, payload hashes, code advertisement, decoding errors, and scoped identity.
The native transport records transactions in the canonical observation bundle.
A transport error stops further requests and leaves explicit skipped rows in
the report. A property rejection or malformed descriptor does not truncate the
remaining census.

The ledger model must match the selected manifest and observed DeviceInfo.
Unknown-model ledgers produce coverage rows by default. `--allow-unknown-model`
permits an exploratory run on the selected body. Its results retain the unknown
source model and remain unverified. It never converts FF0021 into GFX100 II
evidence. The manifest firmware and raw DeviceInfo version string are recorded separately.
DeviceInfo version is not automatically interpreted as camera firmware.
Connection identity is recorded for the individual run;
static plugin scope does not imply a firmware qualification.

## Optional requests

A separate strict `camera-capability-plan/v1` YAML plan can select known request
shapes with `--plan FILE`. Each entry references a ledger row, states its mode
precondition, records the source of the complete request shape, and declares
bounded response expectations. The plan also names the required connection. A `read` entry may request camera data with a
known layout. `action` and `reversibleWrite` entries additionally require
`--allow-control`. An explicit `--scope-mode MODE` records the operator's mode
claim. The tool does not silently enter a mode or establish its precondition.
Use the retained-session command when a reviewed manifest entry sequence is
needed before an action in the same session.

The checked-in [FF0018 plan](../tools/camera-initiator/plans/tether-app-ff0018.yaml)
contains the complete static 0x9018 read shape and the two-parameter 0x100e
capture shape. Add these arguments to the command above after establishing the
shooting/stills precondition:

```sh
--plan tools/camera-initiator/plans/tether-app-ff0018.yaml \
--scope-mode shooting/stills
```

This runs the known read request and skips capture. Add `--allow-control` only
when the stated capture effect and cleanup limitation are acceptable. The read
plan checks the response and retains image bytes; it does not assert an image
format or a valid live-view workflow. For a reviewed initialization in the same
session, use a retained-session plan with `entry`, `startLiveView`, and
`pollLiveView` from the existing manifest before inspecting its observations.

Playback 0x9008 has one dynamic parameter and 0x901b has four. Their parameter
recipes and source model remain unresolved, so no concrete playback requests
are supplied. Legacy 0x101c/0x1018 likewise remain reasoned coverage entries.
The ledger preserves these gaps without filling arguments with guessed zeros.

Unknown request layouts remain coverage rows with their missing facts. Firmware
update codes 0x900c and 0x900d are excluded from this lane even with control
permission. SDK poller wakeups 0xc006 and 0x4002 are internal client facts and
must not become camera-event expectations.

A reversible write declares baseline, set, verify, restore, and verify-restored
requests. Restoration sends the observed baseline bytes, including after set or
verification failure. A restore failure is reported separately. It is never
reported as a successful write test. Actions without a reversible baseline must
state the expected state change and cleanup limitation. A successful response
alone leaves the semantic effect unverified.

Existing `camera-initiator-pcss-probe/v1` plans support bounded raw operations and
reversible write cycles. Existing `camera-initiator-session/v1` plans support
reviewed manifest entries and named actions. Their contracts and examples are in
[REAL_CAMERA_PTPIP.md](REAL_CAMERA_PTPIP.md). Use exact `(0, 0)` capture parameters
for 0x100e where established. Preserve the existing meanings of 0xd208
(`pcssCaptureFunction`) and 0xd20c (simultaneous card recording).

## Comparing simulator and camera

Start a local simulator using the same manifest and connection:

```sh
mkdir -p capability-media
cargo run -p camera-sim-service -- \
  --manifest packages/camera-config-data/fuji/gfx100ii/gfx100ii.consolidated.yaml \
  --connection wireless-tether --media-root capability-media \
  --command-bind 127.0.0.1:15740 --knock-bind 127.0.0.1:51562
```

Run the initiator command above with `--camera 127.0.0.1` and distinct
`simulator-` artifact paths. This exercises the PCSS rendezvous on loopback. Run it
against the intended camera with distinct `camera-` paths after the camera and
mode have been selected. No hardware run is part of implementing this command.

Compare row `id`, `outcome`, advertisement, descriptor datatype, response codes,
and payload hashes in the two reports. A compact comparison is:

```sh
jq -S '[.rows[] | {id, outcome, advertised, descriptorType, responses: [.requests[].response]}]' \
  simulator-capabilities/capability-report.json > simulator-rows.json
jq -S '[.rows[] | {id, outcome, advertised, descriptorType, responses: [.requests[].response]}]' \
  camera-capabilities/capability-report.json > camera-rows.json
diff -u simulator-rows.json camera-rows.json
```

Inspect raw bytes when a descriptor is
empty, rejected, or undecodable. Report equality does not qualify behavior. Use
the camera observation bundle as input to canonical observation intake, then
proposal, digest-bound review, and apply before promoting manifest assertions.
Keep captures scoped to the observed model, firmware, connection, and mode.
