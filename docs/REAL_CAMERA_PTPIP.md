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
trace destination is standard output in JSON Lines format. `--trace FILE` keeps
the wire record separate from progress written to standard error, and
`--trace-format text` selects a compact human-readable form.

## Named actions and payloads

Action names are the exact camelCase manifest verbs. Ordinary single-output
actions can write one payload, while looped or multi-output actions write one
file per step path and transaction.

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

Discovery traces record the selected target mode, UDP destination, callback
peer, parsed `DSC`, and dynamic `DSCPORT`, so broadcast and explicit-unicast
runs remain distinguishable without a packet capture.

## Reading a trace

Each JSONL record has a monotonic elapsed time and a kind. `wire` records add
direction, channel, frame ID, byte length and lowercase hex. Large streamed
frames are split into offset-tagged chunks that remain byte-for-byte
reconstructable. `step` records are emitted as the executor starts and
completes each step, including operation, response and transaction correlation
when available. `checkpoint`, `session` and terminal `outcome` records make
outer lifecycle stalls distinguishable from command stalls.

The live-view socket is the one deliberate exception to full payload logging:
the trace records frame length and first-frame readiness, not high-rate JPEG
bytes. Packet capture remains the right tool when TCP-level timing or live-view
payload inspection is required.

The last `command` wire record and terminal `step` record answer the first
debugging questions: which operation was sent, whether a data/reply frame
arrived, which transaction owned it, and whether the stop was a response
failure, EOF, or a deadline. Orderly process-exit frames use the `cleanup`
channel, and the final record is the terminal `outcome` after cleanup.

## Manifest iteration

Copy the body manifest to a temporary path, make one evidence-backed step
change, and point `--manifest` at that copy. The initiator reloads all tiers on
every invocation; no build, app vendoring, install, or re-pair step is involved.
Bulk camera observations still flow through the generator intake rather than
being hand-authored into repository manifests.

BLE control, operating-system Wi-Fi association, packet capture, and automated
camera-menu interaction are outside this tool's scope.
