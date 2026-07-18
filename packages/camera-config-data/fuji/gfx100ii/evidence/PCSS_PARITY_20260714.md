# PCSS parity evidence — 2026-07-14

This public summary records the protocol facts used by the GFX100 II manifest.
Addresses in examples use the documentation-only `192.0.2.0/24` range; endpoint
values are always parsed from the callback.

Updated 2026-07-16 with live acceptance evidence for direct use of a valid
broadcast callback versus an unnecessary second rendezvous.

## Discovery and establishment

- Auto discovery sends `DISCOVERY * HTTP/1.1` to the subnet broadcast address on
  UDP 51562. A known camera address accepts the same request by direct unicast.
- The camera opens a TCP callback to the host on 51560 and sends one CRLF-delimited
  `NOTIFY` containing `DSC`, `CAMERANAME`, `DSCPORT`, `MX`, and `SERVICE`.
- The callback is acknowledged with the exact 18 bytes
  `HTTP/1.1 200 OK\r\n\0`. The advertised `DSC` and `DSCPORT` select the command
  endpoint; neither is a fixed protocol constant.
- A valid broadcast callback may advertise a command endpoint that is already
  ready; connect to that endpoint before sending another discovery datagram.
  If the endpoint or first Init transport attempt is unavailable, one fresh
  unicast rendezvous to the learned `DSC` can refresh it. An unconditional
  second rendezvous can invalidate an otherwise ready command session.
- PTP/IP Init may answer with typed `InitFail` reason `0x2019`. Replaying the
  byte-identical request after roughly 500 ms on the same TCP socket succeeded
  in both cold-start and stable-lease observations.

## Property 0xD20C

The device descriptor identifies `0xD20C` as read/write `UINT16` with enum values
1 through 4. Controlled writes directly exercised values 1 (RAW plus processed)
and 2 (RAW only). Vendor SDK definitions supply values 3 (processed only) and 4
(no simultaneous card recording); those two rows deliberately retain separate
provenance.

The host preference for which queued formats to transfer produced no distinct
camera property write. It is an object-selection and queue-disposition policy,
not another device property.

## Property 0xD395

The captured Live View “Move Focus Area” path uses standard
`SetDevicePropValue` for read/write property `0xD395`, whose datatype is PTP
`STR`. Its semantic value is a comma-separated triple `x,y,size`; signed decimal
coordinates were observed. The manifest describes that generic structured-text
shape and the typed encoder validates it, while deliberately leaving coordinate
bounds unspecified. Other click-mode labels are not evidence for their wire
behavior and remain a focused capture gap.

## Autofocus and live-view magnification

The GFX100 II firmware 2.30 PCSS autofocus sequence optionally writes `0xD395`
as the exact PTP string `x,y,size`, then writes `0xD230 = 1` and
`0xD208 = 0xA000`, sends `InitiateCapture (0x100E)` with parameters `(0, 0)`,
and polls `0xD209`. The focus-area write is placement, not the autofocus
trigger. Captured omission reused prior camera state; clean-session omission
behavior is not established.

All five captured attempts changed `0xD209` from 1 to 3 after 177, 192, 189,
210, and 192 polls. Static SDK labels define 1 as operating, 2 as success, 3 as
failure, and 4 as no operation; only 1 and 3 appeared in these attempts.

| Item | Wire-captured fact | Static SDK label | Simulator policy |
| --- | --- | --- | --- |
| `0xD01B` | Writes of 1, 2, and 4 | `Fpcsh_LiveZoom`: 1 = x1.0, 2 = x2.5, 4 = x4.0 | No additional behavior is inferred |
| `0xD230` | Lock writes 1; cleanup writes 1 and tolerates response `0xA002` | `Fpcsh_ForceMode`: 1 = shoot mode | None |
| `0xD208` | Lock writes `0xA000`; cleanup writes `0x0006` | `Fpcsh_CaptureFunction`: `0xA000` = INSTANTAF, `0x0006` = AEOFF\|S1OFF | None |
| `0xD209` | Five attempts changed 1 to 3 | 1 = operating, 2 = success, 3 = failure, 4 = no operation | Lock defaults to 3, accepts only 2 or 3, and settles after two polls; release resets immediately to 4 |
| `0xD395` | Optional exact PTP `STR` containing three signed comma-delimited integers | None | Omission skips the write |

Release is cleanup and belongs in a finally-style path after either autofocus
outcome: write `0xD230 = 1` while tolerating response `0xA002`, write
`0xD21C = 0`, write `0xD208 = 0x0006`, then send `0x100E(0, 0)`; the latter
three operations returned `0x2001`. `0xD21C = 0` remains opaque
cleanup/keepalive and is not labeled as autofocus release. The simulator's
two-poll lock transition and immediate release reset are deterministic policy,
not captured timing or wire evidence; no shared `0x100E` operation effect is
implied.
