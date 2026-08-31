---
description: Operating camera-initiator, the headless real-camera PTP/IP probe lane — inputs, commands, and trace capture against real bodies.
status: reference
read-when: Driving a real camera over PTP/IP, capturing traces, or debugging manifest behavior against hardware.
---

# Driving a real camera over PTP/IP

`camera-initiator` is ptpsim's headless real-camera probe lane. It is deliberately
thin: the binary loads the same camera manifests as consumers, calls the shipping
`camera-protocol-ffi` entry/action executors, and uses the shipping `ptp-core` and
`protocol-primitives` codecs. Editing YAML and rerunning the command therefore
exercises the same protocol program that an app receives through FFI.

The tool sends commands to a real camera. Use a camera and media card whose
state may safely change. Traces can contain device identity and media bytes;
inspect and redact them before attaching them to a public issue.

## Inputs

Every command selects a body manifest, optional manufacturer defaults and
ordered overlays. Runtime values use decimal or `0x` hexadecimal notation.
String-valued identity inputs remain strings until the manifest's init policy
resolves them. `--camera` is required for direct-address connections such as
reference app, but may be omitted when the selected PCSS manifest declares subnet
broadcast as its default discovery target.

For Fuji reference app, `terminalName` must be exactly the same name registered during
BLE pairing. A mismatch can make the camera silently discard the PTP/IP init.

```sh
cargo run -p camera-initiator -- \
  --camera CAMERA_IP \
  --manifest packages/camera-config-data/fuji/gfx100ii/gfx100ii.yaml \
  --manufacturer packages/camera-config-data/fuji/fuji.yaml \
  --connection app \
  --param terminalName='probe-host' \
  entry --to shooting/stills
```

`--overlay` may be repeated; overlays apply in command-line order. The default
observation destination is `camera-observation.jsonl` in canonical
`camera-observation/v1` JSON Lines format. `--observation FILE` selects another
bundle path. The trace defaults to standard output, remains a bounded operator
projection, and is not generator input.

## Retained session plans

The `session` command runs mode entries, mode switches, and named actions from
one strict YAML plan. It prepares the complete plan before opening a transport
or creating an artifact. Preparation resolves every mode edge and action
through the selected manifest. It also checks mode progression, action mode,
initiator role, parameters, output policy, and expectations. A rejected plan
does not create the output directory, trace, observation bundle, report, or
payload files.

```sh
cargo run -p camera-initiator -- \
  --camera CAMERA_IP \
  --manifest packages/camera-config-data/fuji/gfx100ii/gfx100ii.yaml \
  --manufacturer packages/camera-config-data/fuji/fuji.yaml \
  --connection app \
  --param terminalName='probe-host' \
  --trace /tmp/session-trace.jsonl \
  --observation /tmp/session-observation.jsonl \
  session --plan /tmp/session.yaml --output-dir /tmp/session-output
```

The plan schema is `camera-initiator-session/v1`. Unknown fields are rejected
at every level. `steps` must contain at least one item. Each `id` is unique and
matches `[A-Za-z0-9][A-Za-z0-9_-]*`. Step order is execution order.

```yaml
schema: camera-initiator-session/v1
steps:
  - id: enter-stills
    kind: entry
    to: shooting/stills
    expect:
      stepsRun: 7
      outputCount: 0
  - id: focus-lock
    kind: action
    action: autofocusLock
    parameters:
      afArea: 1402507338
    expect:
      stepsRun: 2
  - id: focus-release
    kind: action
    action: autofocusRelease
  - id: transfer
    kind: switch
    from: shooting/stills
    to: image-transfer
    expect:
      exit:
        stepsRun: 2
      targetEntry:
        stepsRun: 9
```

An `entry` step has required `to`, optional `from`, and optional `expect`.
Cold entry omits `from` and is valid only when no mode is retained. An
in-session entry must name the retained mode in `from`. Its declared edge must
use the PTP execution variant.

A `switch` step has required `from` and `to`. Its declared edge must use the
re-establishment execution variant. A first switch cold-enters its source. A
later switch reuses retained source state when `from` matches the active mode.
Any other source mode is rejected during preparation. Its optional `expect`
object has only `sourceEntry`, `exit`, and `targetEntry`. `sourceEntry` is valid
only when the runner must cold-enter the source.

An `action` step has required `action`, optional `parameters`, optional
`expectedBytes`, and optional `expect`. `action` is the exact camelCase action
name from the manifest catalog. `parameters` is local to that invocation. Each
value is a YAML unsigned integer or string and must match the manifest's exact
parameter declaration. Quote numeric-looking strings. Required parameters
must be present and extra parameters are rejected. Top-level `--param` values
remain available only to transport identity and mode-entry execution.
`expectedBytes` is valid only for the connection's whole-object streaming read
action.

Entry and action expectations use this exact subset schema:

```yaml
expect:
  stepsRun: 3
  scope:
    numericValue: 7
    textValue: "ready"
  collections:
    exactHandles: [1, 2, 3]
  outputCount: 1
  outputs:
    - index: 0
      payloadBytes: 4096
      responseParams: [1, 2]
```

Every field is optional. Present fields compare exactly. `scope` preserves the
YAML scalar type. `collections` values and `responseParams` are exact ordered
unsigned-integer arrays. `outputs` selects results by zero-based `index`.
Output checks accept only `index`, `payloadBytes`, and `responseParams`.
Expectations do not accept operation codes, manifest step names, arbitrary
field paths, or a separate assertion step.

The runner keeps one native transport, trace writer, and observation recorder.
It opens one PTP session lazily. Compatible entries merge their captured scope,
while action scope stays local to the invocation. Transaction identifiers are
transport-owned and increase across retained steps. A re-establishment clears
the retained scope, closes the old session, records `externalEstablishment`,
waits for endpoint replacement, and opens the target as a fresh PTP session.
The replacement session starts its transaction sequence again. The observation
bundle keeps one run identity and records a distinct session identity for each
physical session.

The output directory must not exist. The runner creates it with these reserved
paths:

```text
session-output/
  session-report.json
  payloads/
    0001-focus-lock/
      0000_steps_1__sendOp_tid0000000d.bin
```

Each action receives `payloads/{step-index}-{step-id}/`, using a zero-padded
four-digit step index. Ordinary outputs are written with create-new semantics.
Their names include output order, sanitized manifest step path, and transaction
identifier. Whole-object reads use the existing streaming sink in that step
directory. A failed stream retains its `.partial` file. Import and other
multi-output actions also use the step directory. Preparation rejects path
collisions with `session-report.json` and `payloads`.

The report schema is `camera-initiator-session-report/v1`. The fixed path is
`session-report.json`. It contains `planSchema`, `runId`, `connection`, overall
`status`, and ordered attempted `steps`. Each step records `id`, `kind`,
`status`, session index, deduplicated transaction identifiers, and a normalized
outcome. Payload records contain the relative path, byte length, SHA-256,
manifest step path, transaction identifier, and response parameters. Switch
records include a performed source entry when applicable, exit, checkpoint,
target entry, and the session indexes before and after replacement.
Expectation failures record the expected and actual values. The report also
records the terminal error, cleanup warning, and artifact references.
An action failure retains payload records created before the error. A
best-effort `stopLiveView` attempt is recorded on that failed step with its
status, transaction identifiers, outcome, payloads, and error. Cleanup
transactions are excluded from the failed action's transaction identifiers.

This is the exact report shape. Nullable fields remain present. `switch` is
non-null only for a switch step. Its `checkpoint` is null when execution stops
before the external handoff.

```json
{
  "schema": "camera-initiator-session-report/v1",
  "planSchema": "camera-initiator-session/v1",
  "runId": "local-initiator",
  "connection": "app",
  "status": "succeeded",
  "steps": [
    {
      "id": "focus-release",
      "kind": "action",
      "status": "succeeded",
      "sessionIndex": 1,
      "transactionIds": [13],
      "outcome": {
        "stepsRun": 1,
        "scope": {},
        "collections": {},
        "outputCount": 0,
        "outputs": []
      },
      "payloads": [],
      "cleanupAttempt": null,
      "switch": null,
      "expectationMismatch": null,
      "error": null
    }
  ],
  "terminalError": null,
  "cleanupWarning": null,
  "artifacts": {
    "report": "session-report.json",
    "trace": "/tmp/session-trace.jsonl",
    "observation": "/tmp/session-observation.jsonl",
    "payloads": "payloads"
  }
}
```

Each normalized output has `stepPath`, `transactionId`, `payloadBytes`, and
`responseParams`. Each retained payload has `path`, `length`, `sha256`,
`stepPath`, `transactionId`, and `responseParams`. A switch step sets `outcome`
to null and fills `switch` with nullable `sourceEntry`, `exit`, `checkpoint`,
and `targetEntry`, plus `beforeSessionIndex` and `afterSessionIndex`.
`cleanupAttempt` is non-null only when the runner attempts a best-effort
`stopLiveView` after an action failure.

After execution starts, the runner stops at the first executor or expectation
failure. It omits later steps, attempts a safe session close, publishes the
report atomically with create-new semantics, then exits with failure. An
expectation mismatch does not change the successful action invocation record
in the observation bundle. The existing `--trace`, `--observation`, and
`--run-id` defaults and overrides apply to the complete session run.

BLE pairing, Wi-Fi configuration, and host network changes remain external.
At a switch checkpoint, use the same external establishment mechanism required
by the standalone `switch` command. The runner only reports the manifest's
handoff parameters and waits for the replacement endpoint.

## Named actions and payloads

Action names are discovered from the manifest catalog and use their stable
camelCase ids. This command always resolves the `initiator` role. Ordinary
single-output actions can write one payload, while looped or multi-output
actions write one file per step path and transaction.

```sh
cargo run -p camera-initiator -- \
  --camera CAMERA_IP \
  --manifest packages/camera-config-data/fuji/gfx100ii/gfx100ii.yaml \
  --manufacturer packages/camera-config-data/fuji/fuji.yaml \
  --connection app \
  --param terminalName='probe-host' \
  --param handle=0x10000001 \
  --param offset=0 \
  --param length=0x00c00000 \
  action getObject --payload-out object.part
```

For a connection whose object-transfer strategy is `wholeObject`, the declared
read action uses the bounded streaming executor. The destination is first
written as `PATH.partial` and renamed only after the declared payload and final
OK response arrive. A failed run leaves the partial file for diagnosis and
never creates the requested final path. The initiator never follows a read with
a destructive delete or completion action on its own.

## Live view to image transfer

The reference app transition from live view to image transfer owns two sessions and an
external network handoff. Run it as one stateful command:

```sh
cargo run -p camera-initiator -- \
  --camera CAMERA_IP \
  --manifest packages/camera-config-data/fuji/gfx100ii/gfx100ii.yaml \
  --manufacturer packages/camera-config-data/fuji/fuji.yaml \
  --connection app \
  --param terminalName='probe-host' \
  switch --from shooting/stills --to image-transfer
```

The command enters cold live view, confirms the first live-view frame, and
captures the `InitiateOpenCapture` transaction ID from the shipping executor.
It then runs the manifest's old-session exit, closes auxiliary sockets, sends
the declared transport-close sentinel, and prints the re-establishment
parameters (for this edge, `launchMode=3`).

At that checkpoint, use an external BLE/Wi-Fi establishment mechanism. The
initiator prints the required manifest parameters, but it does not pair,
configure Wi-Fi, or alter host network state; without that external mechanism,
this edge cannot complete. It waits for the old endpoint to disappear and
return, retains the first successful replacement TCP connection for init,
opens a fresh PTP/IP session with reset transaction state, and executes the
cold image-transfer entry. The default checkpoint timeout is 120 seconds.

## PCSS wireless tether

The `wireless-tether` connection uses the manifest's `pcssKnock` rendezvous:
the initiator binds the callback port, sends discovery, accepts and acknowledges
the callback, connects to the callback's `DSC` address and `DSCPORT`, applies
only the manifest-selected InitFail retries, and opens the compressed PTP
session. The machine must already be on the same routed network as the camera.
Before acknowledging a callback, the initiator also matches its typed
`CAMERANAME` to the selected body's manifest identity when one is declared.
Supply the host's manifest runtime identity with `--param terminalName=...`; use
the same normalized name as the paired host when the camera enforces identity
continuity.

With the GFX100 II manifest, omitting `--camera` selects the manifest-default
IPv4 subnet-directed broadcast target. This is broadcast, not multicast: the
initiator selects the default-route IPv4 interface (or `--interface NAME`),
derives its directed broadcast from the interface address and netmask, and
learns the camera address from the validated callback. If the first advertised
command endpoint or first Init transport attempt is unavailable, the manifest
permits one new rendezvous by unicast to that learned address.

```sh
cargo run -p camera-initiator -- \
  --manifest packages/camera-config-data/fuji/gfx100ii/gfx100ii.yaml \
  --manufacturer packages/camera-config-data/fuji/fuji.yaml \
  --connection wireless-tether \
  --param terminalName='probe-host' \
  --trace pcss-broadcast.jsonl \
  action readDeviceInfo --payload-out device-info.bin
```

Supplying `--camera` selects the separately declared explicit-unicast target.
The callback TCP peer and its `DSC` field must both match that address.

```sh
cargo run -p camera-initiator -- \
  --camera CAMERA_IP \
  --manifest packages/camera-config-data/fuji/gfx100ii/gfx100ii.yaml \
  --manufacturer packages/camera-config-data/fuji/fuji.yaml \
  --connection wireless-tether \
  --param terminalName='probe-host' \
  --trace pcss-unicast.jsonl \
  action readDeviceInfo --payload-out device-info.bin
```

PCSS live view is polled rather than delivered on an auxiliary socket, and it
does not use the reference app `DF01` mode flip. A fresh PCSS session must first execute
the manifest's `startLiveView` action, which selects PCSS polled delivery before
opening live view. `pollLiveView` deliberately requests only one frame and uses
only the manifest's bounded transient-response retry; `stopLiveView` closes the
camera's live-view state. `--then`
executes additional named actions before closing the same PTP session. This
acceptance probe starts live view, reads one frame, enumerates transferable
objects while live view remains open, and then stops live view:

```sh
cargo run -p camera-initiator -- \
  --manifest packages/camera-config-data/fuji/gfx100ii/gfx100ii.yaml \
  --manufacturer packages/camera-config-data/fuji/fuji.yaml \
  --connection wireless-tether \
  --param terminalName='probe-host' \
  --trace pcss-live-to-transfer.jsonl \
  action startLiveView \
  --then pollLiveView \
  --then enumerateObjects \
  --then stopLiveView \
  --payload-dir pcss-output
```

Discovery observations record the selected target mode, UDP destination, callback
peer, parsed `DSC`, and dynamic `DSCPORT`, so broadcast and explicit-unicast
runs remain distinguishable without a packet capture.

### Runtime capability and bulk probe plans

The `probe` command accepts a runtime YAML plan for a bounded PCSS capability
census and repeatable whole-object read. The plan is operator input, not camera
manifest data: running it never promotes operation or property claims into the
public catalog. Put its output directory outside a tracked source tree. The
command creates that directory and every payload without overwriting an
existing path, retains all three object reads, and never sends a delete or
transfer-completion action automatically.

Every operation in the plan supplies its numeric code, parameter template, data
direction, accepted and retryable responses, bounded timeout and attempt count,
and output policy. The inventory binds `propertyCode` only from the exact
property list decoded from DeviceInfo. The object walk binds `objectHandle` only
from the complete standard handle catalog and selects exactly one ObjectInfo by
the operator's filename and exact byte size. Reversible writes spell out their
baseline, set, verify, restore, and restored-value verification; once a set is
attempted, restore and restored-value verification run even if the main step
fails.

This synthetic-camera plan illustrates the shape without publishing a camera
or manufacturer catalog:

```yaml
schema: camera-initiator-pcss-probe/v1
operations:
  deviceInfo: { code: 0x1001, dataPhase: in, acceptedResponses: [0x2001], output: memory }
  propertyDescriptor: { code: 0x1014, params: [propertyCode], dataPhase: in, acceptedResponses: [0x2001], output: memory }
  propertyValue: { code: 0x1015, params: [propertyCode], dataPhase: in, acceptedResponses: [0x2001], output: memory }
  objectCatalog: { code: 0x1007, params: [0xffffffff, 0, 0], dataPhase: in, acceptedResponses: [0x2001], output: memory }
  objectInfo: { code: 0x1008, params: [objectHandle], dataPhase: in, acceptedResponses: [0x2001], output: memory }
  readObject: { code: 0x1009, params: [objectHandle], dataPhase: in, acceptedResponses: [0x2001], output: stream, timeoutMs: 60000 }
  readSyntheticToggle: { code: 0x9001, params: [7], dataPhase: in, acceptedResponses: [0x2001], output: memory }
  writeSyntheticToggle: { code: 0x9002, params: [7], dataPhase: out, acceptedResponses: [0x2001], output: discard }
inventory:
  deviceInfo: deviceInfo
  propertyDescriptor: propertyDescriptor
  propertyValue: propertyValue
objectProbe:
  catalog: objectCatalog
  objectInfo: objectInfo
  readObject: readObject
  filename: SYNTHETIC.BIN
  exactSize: 4
  repetitions: 3
reversibleWrites:
  - name: synthetic-toggle
    baseline: { name: baseline, operation: readSyntheticToggle }
    set: { name: set, operation: writeSyntheticToggle, payloadHex: "0100" }
    verify: { name: verify, operation: readSyntheticToggle }
    restore: { name: restore, operation: writeSyntheticToggle }
    verifyRestored: { name: verify-restored, operation: readSyntheticToggle }
```

Run it through the manifest-selected shipping rendezvous and command transport:

```sh
cargo run -p camera-initiator -- \
  --manifest path/to/body.yaml \
  --manufacturer path/to/manufacturer.yaml \
  --connection wireless-tether \
  --param terminalName='probe-host' \
  --trace /tmp/synthetic-probe-trace.jsonl \
  --observation /tmp/synthetic-probe-observation.jsonl \
  probe --plan /tmp/synthetic-probe.yaml --output-dir /tmp/synthetic-probe-output
```

The output report records the exact advertised property census, descriptor and
current-value response codes and payload hashes, per-property decode errors,
the selected object metadata, and per-repetition byte counts, hashes, command
durations, and end-to-end durations. A rejected property read or an otherwise
well-framed descriptor/value that the generic PTP codec cannot decode is
recorded without truncating the census. Transport failures, mismatched property
codes, non-empty data paired with an error response, duplicate or truncated
object catalogs, ambiguous filename-and-size matches, payload mismatches, and
cleanup failures still make the run fail closed.

## Standard PTP/IP

A connection with `initShape: standardPtpIp` uses the canonical PTP/IP command,
event, operation, data, and probe packets. Its manifest must declare standard
command/event framing and an initiator GUID/friendly-name identity. Command and
event roles may name the same TCP port; the initiator opens two sockets to that
address and the responder distinguishes them by `InitCommandRequest` versus
`InitEventRequest`.

Session startup is fixed: command init, event init using the command ack's
connection number, pre-session `GetDeviceInfo` with transaction 0, then
`OpenSession` with transaction 1. A mismatched event connection number is a
terminal event-socket rejection and does not disturb an unrelated command
session. `NativePtpTransport::probe_event_channel` sends a standard probe and
requires the paired response.


## Reading observations

Each bundle begins with a required header followed by stable-id, ordinal-tagged
records. PTP records correlate commands, data phases, responses, and separately
linked events. Large streamed frames carry a streaming length, whole-payload
SHA-256, and contiguous per-range hashes rather than being held in memory;
optional artifact ranges point to retained capture bytes. Lifecycle and
action-invocation records make outer stalls distinguishable from command stalls.

The live-view socket is the one deliberate exception to full payload logging:
the trace records frame length and first-frame readiness, not high-rate JPEG
bytes. Packet capture remains the right tool when TCP-level timing or live-view
payload inspection is required.

The last PTP transaction and terminal lifecycle record answer the first
debugging questions: which operation was sent, whether a data/reply frame
arrived, which transaction owned it, and whether the stop was a response
failure, EOF, or a deadline. Orderly process-exit frames use the `cleanup`
channel, and the final record is the terminal `outcome` after cleanup.

## Manifest iteration

Copy the body manifest to a temporary path, make one evidence-backed step
change, and point `--manifest` at that copy. The initiator reloads all tiers on
every invocation; no build, app vendoring, install, or re-pair step is involved.
Camera observations flow through `camera-config-generate validate`, `propose`,
and digest-bound `apply` rather than being hand-authored into repository
manifests.

BLE control, operating-system Wi-Fi association, packet capture, and automated
camera-menu interaction are outside this tool's scope.
