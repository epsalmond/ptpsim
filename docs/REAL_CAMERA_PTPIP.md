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

When `GetDeviceInfo.OperationsSupported` advertises Nikon operation `0x9439`,
the standard startup path performs SnapBridge's post-session vendor-operation
discovery with selector `9` and decodes the returned exact little-endian `u32`
array. If the operation is not advertised, startup skips it; if it is advertised
but fails, establishment fails rather than silently using an incomplete action
catalog.

The bundled Nikon bodies use the direct-camera defaults from SnapBridge 2.13.3:
both socket roles target TCP 15740, the initiator GUID is
`00112233445566778899aabbccddeeff`, and the friendly name is `Android Device`.
Use `packages/camera-config-data/nikon/d850/d850.yaml` together with
`packages/camera-config-data/nikon/nikon.yaml`. The D850 body is deliberately
provisional: these values and the family flow are static application facts, not
a successful D850 registration or interoperability claim.

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
