# GFX100 II firmware 2.30 PTP/IP wire reference

**Last updated:** 2026-05-23
**Evidence:** Wire captures, direct camera observations, PTP 1.1, and public ptpsim issues

## Evidence boundary

This document records observed packet layouts and camera behavior. It does not claim source-level provenance. A property or opcode not listed here still needs public wire evidence before it becomes a compatibility claim.

## TCP endpoints observed on the camera access point

| Camera port | Observed traffic |
|---|---|
| 55740 | PTP/IP command, data, and response packets |
| 55741 | Asynchronous event packets from the camera |
| 55742 | Live-view JPEG frames from the camera |

The camera address in the captured access-point sessions was `192.168.0.1`.

## Compressed command and event framing

After initialization, command and event packets used this little-endian layout:

```text
offset 0x00  uint32  inclusive packet length
offset 0x04  uint16  packet type
offset 0x06  uint16  operation, response, or event code
offset 0x08  uint32  transaction id
offset 0x0c  bytes   parameters or data
```

Observed packet types:

| Type | Meaning |
|---|---|
| 1 | Operation request |
| 2 | Data phase |
| 3 | Operation response |
| 4 | Event |

The first `InitCommandRequest` on port 55740 used the standard PTP/IP packet header with a 32-bit packet type and a 16-byte initiator GUID. Later command traffic used the compressed layout above.

## Live-view frame layout

Port 55742 used this little-endian frame envelope:

```text
offset 0x00  uint32  inclusive frame length
offset 0x04  uint32  reserved, observed as zero
offset 0x08  uint32  frame counter
offset 0x0c  uint32  JPEG offset adjustment
offset 0x10  uint16  reserved, observed as zero
offset 0x12  bytes   JPEG when offset adjustment is zero
```

The JPEG begins after `18 + jpeg_offset_adjustment` bytes. Firmware 2.30 emitted a zero adjustment in the captured sessions. Frames began with JPEG SOI and ended with EOI. Captured baseline frames were 640 by 480 pixels.

The observed stream was approximately 60 frames per second. Gaps of about 1.2 seconds occurred roughly every 13 seconds without an event or error response. A receiver must not treat those captured gaps as immediate connection failure.

## Common response codes

| Code | Meaning |
|---|---|
| `0x2001` | OK |
| `0x2002` | Session already open |
| `0x2003` | Transaction cancelled |
| `0x2005` | Operation not supported |
| `0x2009` | Invalid transaction ID |
| `0x200A` | Device property or access rejected in observed sessions |
| `0x2019` | Device busy in observed sessions |
| `0x201F` | Transaction cancelled |

## Properties supported by checked evidence

The reviewed control declarations use these user-facing names:

| Code | Control name |
|---|---|
| `0x5005` | White balance |
| `0x500A` | Focus mode |
| `0x500C` | Flash mode |
| `0x5012` | Self timer |
| `0xD001` | Film simulation |

Standard PTP property codes retain their PTP 1.1 meanings. The following Fuji vendor properties were observed in checked wire material used by the current manifest:

| Code | Observed role |
|---|---|
| `0xD018` | CCD or readout mode field in property responses |
| `0xD019` | Focus mode field |
| `0xD01D` | Macro mode field |
| `0xD02A` | Live-view image size configuration |
| `0xD02B` | Live-view movie ISO field |
| `0xD212` | Composite live-view status response |
| `0xD226` | Image-import setup property |
| `0xD227` | Image-import setup property |
| `0xD229` | Remaining still-image count |
| `0xD22A` | Remaining movie time |
| `0xD235` | Partial-object chunk limit |
| `0xD240` | Extended shutter-speed field |
| `0xD244` | Image-import setup read |
| `0xD246` | Mode field with unresolved user-facing meaning |
| `0xD620` | Image-import object count |
| `0xD621` | Image-import object handle list |
| `0xDF01` | Function mode |
| `0xDF28` | Function-version field |

This table states wire roles only. It does not imply that every field is safe to expose as a writable client control.

## Live-view session outline

The restored declaration uses this order:

```text
InitCommandRequest
OpenSession
SetDevicePropValue(0xDF00, 6), tolerated when rejected
SetDevicePropValue(0xDF01, live-view mode)
ReadDevicePropValue(0xDF2A), then echo the value
0x902B four times
InitiateOpenCapture
read frames on 55742
read events on 55741
poll declared status properties on 55740
TerminateOpenCapture
CloseSession
```

The retained public wire capture independently confirms the `0xDF01` mode
write, successful `0x101C`, and the auxiliary channel ordering. It does not
independently reproduce the restored `0xDF00`, `0xDF2A`, or `0x902B` steps.

A direct `SetDevicePropValue` can return OK without changing camera behavior. Callers must confirm a visible or wire-level state transition when the manifest requires confirmation.

## Still capture

The captured still-capture path kept one open-capture session active. `InitiateCapture` ran inside that session. Event and property traffic described capture progress. The session did not open and close around each shutter action.

The exact operation sequence and acknowledgement rules live in checked manifests and their byte-level tests. A camera response must be consumed even when the operation is tolerated, otherwise transaction IDs can lose synchronization.

## Image import

The captured image-import session used function mode 20 and version 3. The cold-session vendor setup block was:

```text
0x9054(0x10000001)
0x9055(0x10000001)
0x9050()
0x9053(0, 0x7530)
```

The session then read `0xD620` and `0xD621`, followed by standard `GetObjectInfo`, `GetThumb`, and `GetPartialObject` operations. See `IMAGE_TRANSFER_FW230.md` for the reduced wire record.

## Retry and failure rules

- Retry a busy response only when the selected manifest declares a retry policy.
- Consume rejected property-write responses before issuing the next transaction.
- Do not retry a transport failure inside the same PTP/IP session unless the connection state is known.
- Treat malformed packet lengths, truncated payloads, and transaction-ID mismatches as protocol errors.
- Preserve unknown property values as unknown. Do not infer a user-facing meaning from a numeric code alone.

## Known limits

The checked evidence does not establish:

- Behavior on every Fujifilm body or firmware.
- A complete vendor property catalog.
- A public meaning for every field inside `0xD212`.
- A multi-object image-download queue contract.
- Source-level names for vendor opcodes or properties.

New claims need a checked reduced fixture, a public specification, a public ptpsim issue, or a wire capture that can be redistributed.
