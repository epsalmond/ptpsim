# Image transfer wire protocol for GFX100 II firmware 2.30

**Date:** 2026-05-19
**Evidence:** Wire capture from the reference app and direct camera observations
**Related issues:** #243, #244

## Evidence boundary

This document records bytes and behavior observed on the PTP/IP connection. It does not claim how the reference app implements the flow. The BLE launch write was not captured. The transition into image import therefore remains an outer-lifecycle boundary rather than a replayable BLE sequence.

## Captured session lifecycle

The image-import session used a fresh PTP/IP command connection on port 55740. The preceding session closed in order, the camera access-point association ended, and a later association obtained a new address before the next PTP/IP session. Transaction IDs restarted at 1.

The fresh session used this setup order:

```text
OpenSession
SetDevicePropValue(0xDF01, uint16 20)
SetDevicePropValue(0xDF28, uint32 3)
SetDevicePropValue(0xD226, uint32 0)
SetDevicePropValue(0xD227, uint32 0)
GetDevicePropValue(0xD244)
0x9054(0x10000001)
0x9055(0x10000001)
0x9050()
GetDevicePropValue(0xD212)
GetDevicePropValue(0xD22B)
0x9053(0, 0x7530)
GetDevicePropValue(0xD620)
GetDevicePropValue(0xD621)
GetObjectInfo(handle)
GetThumb(handle)
```

On a cold GFX100 II firmware 2.30 session, reading `0xD620` timed out when the `0x9054`, `0x9055`, `0x9050`, `0x9053` block was omitted. Sending `0x9054` alone was rejected and left the next transaction out of sequence. The complete ordered block was accepted.

## Object count and handles

`0xD620` returned a four-byte little-endian object count. The captured session returned `0x83` (131).

`0xD621` returned:

```text
uint32 count
uint32 handles[count]
```

The captured handle list was newest-first. The client then sent `GetObjectInfo (0x1008)` and, when a thumbnail was declared, `GetThumb (0x100A)` for each selected handle.

## Captured enumeration totals

The capture contained:

- 122 `GetObjectInfo` operations.
- 118 `GetThumb` operations.
- One 1.17 MB `GetPartialObject` response.
- Thirty `GetPartialObject` responses for a 356 MB object.

The four objects without `GetThumb` requests declared no compressed thumbnail. Enumeration took about 26 seconds, approximately 4.7 objects per second.

## ObjectInfo fields observed

The fixed PTP ObjectInfo fields were followed by length-prefixed UTF-16LE strings. Captured JPEG and RAF objects used the same fixed-field layout. Captured MOV objects omitted the final keywords field.

Observed fields included:

- Storage ID.
- Object format.
- Compressed size.
- Thumbnail format, size, width, and height.
- Image width, height, and bit depth.
- Parent object and association fields.
- Sequence number.
- Filename.
- Capture date.
- Modification date.
- Keywords when present.

The still-image keywords field carried an `Orientation:N` value. No aperture, shutter speed, ISO, rating, GPS, or subject classification appeared in the captured ObjectInfo responses.

## Selected-object download

The selected object used `GetPartialObject (0x101B)` with:

```text
param1: object handle
param2: byte offset
param3: requested byte count
```

The large captured object used 12 MiB requests until the final shorter response. The next offset equaled the previous offset plus the delivered byte count. The 1.17 MB object completed in one response.

## Teardown

The captured client closed the PTP session and command connection before the camera access-point association ended. A later image-import launch established a new outer connection rather than reusing the prior PTP/IP state.

## Limits of the evidence

The capture does not establish:

- The BLE write that launches image import.
- Whether every firmware requires the same setup-property writes.
- A fallback chunk size for slower links.
- A general multi-object queue advancement rule after download.
- Per-file camera settings beyond the fields listed above.

Implementations should treat rejected advisory property writes as transaction responses that must be consumed. Transport failures remain fatal to the current session.
