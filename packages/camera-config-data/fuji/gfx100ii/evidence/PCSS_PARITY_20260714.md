# PCSS parity evidence — 2026-07-14

This public summary records the protocol facts used by the GFX100 II manifest.
Addresses in examples use the documentation-only `192.0.2.0/24` range; endpoint
values are always parsed from the callback.

## Discovery and establishment

- Auto discovery sends `DISCOVERY * HTTP/1.1` to the subnet broadcast address on
  UDP 51562. A known camera address accepts the same request by direct unicast.
- The camera opens a TCP callback to the host on 51560 and sends one CRLF-delimited
  `NOTIFY` containing `DSC`, `CAMERANAME`, `DSCPORT`, `MX`, and `SERVICE`.
- The callback is acknowledged with the exact 18 bytes
  `HTTP/1.1 200 OK\0`. The advertised `DSC` and `DSCPORT` select the command
  endpoint; neither is a fixed protocol constant.
- A callback may precede command-endpoint readiness. Repeating the unicast
  rendezvous is reliable and preserves a single establishment path for auto,
  saved, and manually entered addresses.
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
