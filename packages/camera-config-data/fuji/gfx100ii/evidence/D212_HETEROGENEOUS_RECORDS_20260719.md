# App-persona D212 heterogeneous records — 2026-07-19

This public reduction records the byte-level facts used to describe the
GFX100 II firmware 2.30 app-persona `0xD212` payload. It contains no private
capture location or device identity.

## Complete D22F-only snapshot

The camera returned PTP response `0x2001` with this complete seven-byte data
payload:

```text
01 00 2f d2 01 00 00
```

The fields are:

```text
01 00       record count = 1
2f d2       member = 0xD22F
01 00 00    empty PTP string: one UTF-16LE terminator code unit
```

The payload is not a truncated four-byte integer. The PTP string length byte
includes its terminating NUL code unit, so the empty value occupies three
bytes and exactly exhausts the response data.

## Complete mixed snapshot

A preserved successful app-persona transaction returned:

```text
04 00
00 df 12 00 00 00
20 d2 01 00 00 00
41 df 01 00 00 00
2f d2 01 00 00
```

The count is four. Members `0xDF00`, `0xD220`, and `0xDF41` carry four-byte
little-endian unsigned values; `0xD22F` carries the same empty PTP string. The
four records exactly exhaust the 25-byte payload, and `0xDF41` is numeric one.

## Scope and limits

The string encoding belongs to member `0xD22F` inside the app-persona
`0xD212` record stream. It does not establish a global datatype for property
code `0xD22F`, whose meaning is mode/persona-overloaded.

A valid decoded snapshot may omit `0xDF41`. Absence is distinct from numeric
zero and from malformed framing. These observations do not establish a retry
interval, an absent-to-present transition, or session-maintenance behavior.
