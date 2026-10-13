---
description: HTTP, simulator connection, and binary decoder resource limits for untrusted input.
status: reference
read-when: Changing network parsers, connection admission, read deadlines, or binary decoders.
---

# Untrusted input limits

These limits are simulator resource policy. They do not describe camera
firmware limits. Network input must pass length checks before allocation or
slicing. A malformed request closes its connection. Outbound object streaming
does not use the inbound data limit.

## HTTP requests

The service and TUI parsers accept one request per connection. Both limit the
header section to 16 KiB. The service limits the complete request to 64 KiB.
The TUI limits it to 256 KiB and retains its 64 KiB plugin request body limit.

Declared lengths use checked arithmetic. A length that exceeds the complete
request limit is rejected as soon as the headers are available. Duplicate
`Content-Length` fields and every `Transfer-Encoding` field are rejected.
Headers must be UTF-8. Body bytes remain opaque until the endpoint decodes them,
so fragmentation inside a UTF-8 body sequence does not invalidate the request.

The complete request has a five-second read deadline from acceptance. Receiving
another byte does not extend it. Response writes are also bounded. The service
admits at most eight control connections before spawning request tasks. The TUI
handles one connection at a time with the same absolute read deadline.

## Simulator connections

Admission happens before spawning work. The service closes an excess connection
instead of retaining a task that waits for capacity. Each command connection
holds a 4 MiB inbound memory reservation until its task finishes. Sixteen slots
bound that reservation pool to 64 MiB. A standard event connection accepted on
the shared command listener occupies one of those slots without acquiring a
second permit.

| Role | Concurrent tasks | Inbound frame limit |
| --- | ---: | ---: |
| Command or shared standard listener | 16 | 4 KiB handshake, 64 KiB operation, 1 MiB + 12 bytes data |
| Separate event listener | 16 | 4 KiB |
| Liveview listener | 8 | No peer-selected object allocation |
| Control HTTP | 8 | 64 KiB complete request |
| PCSS callback | 8 | Bounded callback acknowledgement |

The command reservation covers simultaneous input frame storage, decoded data,
accumulated data, and retained request or handshake bytes. Other roles have
separate bounded pools and buffers. Trusted fixture data and outbound streaming
are outside this inbound reservation.

An accepted command or event connection must complete its handshake within five
seconds. After establishment, an idle connection may wait indefinitely for the
first byte of a frame. Once that byte arrives, the entire frame must complete
within five seconds. Partial header and body state survive cancellation of an
individual read future, including interruption by an event broadcast.

An initiator-to-responder data phase has a ten-second absolute deadline. The
deadline starts before waiting for its first `StartData` frame. Empty or
fragmented data frames do not extend it. The accumulated payload limit remains
1 MiB. The frame limit includes the twelve-byte data header. An oversized
four-byte length prefix is rejected before allocating its declared body.

PCSS callback admission is bounded independently. Connect, write, and
acknowledgement share one five-second deadline. Completed callback tasks are
reaped. Shutdown cancels and joins retained connection and callback work.
PCSS notification identity checks bind the advertised host to the observed peer.

## Binary decoders

`DeviceInfo`, `ObjectInfo`, `StorageInfo`, and `DevicePropDesc` decode a complete
dataset. Any suffix produces `TrailingBytes`. Incremental property-value decoding
continues to consume values from a caller-owned reader.

Standard PTP/IP, Fuji, and USB decoders reject parameter tails whose size is not
a multiple of four. Fixed-size bodies must consume the complete frame. Array
decoders validate counts against remaining input before reading elements. They
grow storage after successful element reads instead of allocating from an
untrusted count. Existing element-count ceilings still apply.

## Verification

Regression tests cover overflow, duplicate lengths, fragmentation, byte drips,
stalled handshakes, data-phase deadlines, role capacity recovery, partial-frame
interruption, complete datasets, and parameter alignment. Valid 1 MiB inbound
data and large outbound objects remain supported.

The fuzz harness requested in issue #408 remains separate work. Its requested
coverage includes HTTP parsers, service frame and data-phase reads, FFI
execution, PCSS identity checks, binary codecs, initiator framing, Nikon fixed
layouts, and media parsers.
