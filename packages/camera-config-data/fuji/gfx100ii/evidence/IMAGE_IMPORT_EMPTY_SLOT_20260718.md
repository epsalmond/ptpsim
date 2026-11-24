# Image import with memory card slot 1 empty: 2026-07-18

This public reduction records what a GFX100 II returned to the image-import
operation `0x9050` while memory card slot 1 held no card. It backs the
`memoryCardSlot1Empty` simulator state in `../gfx100ii.yaml`. The source is the
field report in ptpsim issue #352. This reduction contains no capture location,
device identity, or consumer identifier.

## Configuration

One GFX100 II body was observed in one field session on 2026-07-18:

- Memory card slot 1 held no card.
- Memory card slot 2 held a card.
- The connection was the smartphone-app PTP/IP persona (`app`), reached by a
  BLE launch of the camera's Wi-Fi access point.
- The camera was in image-transfer mode.

## Observed behavior

- BLE launch, Wi-Fi association, PTP/IP initialization, `OpenSession`, and
  image-transfer mode entry succeeded.
- Every image-enumeration attempt failed at its first operation. Operation
  `0x9050` answered PTP response `0x2019` (Device Busy).
- The enumeration prime retries `0x9050` up to five times, 100 ms apart, when it
  answers `0x2019`. All five attempts answered `0x2019`.
- The failure repeated over five enumeration attempts, including after a fresh
  PTP/IP connection.
- No later enumeration step ran. The `0xD620` object count and the `0xD621`
  handle list were never read.

After a card was inserted into slot 1, the same image-transfer flow listed files.

## Uncertainty

- This is one field session on one body.
- The body's firmware version was not recorded. The state is declared in the
  base GFX100 II manifest (firmware 2.30), so the firmware 2.40 tier inherits
  it. Neither version is specifically established.
- A person confirmed the positive control (card in slot 1, files listed) on the
  body. It produced no stored telemetry row.
- An earlier field observation on a GFX100 II with a card present saw `0x9050`
  answer `0x2019` transiently during a slow cold image-transfer entry. A later
  enumeration in that session succeeded. The same response code therefore also
  occurs briefly while the camera settles. Only persistence across attempts and
  connections separates the two cases.
- Response parameters and any data phase on the `0x2019` reply were not
  recorded.

## Not established

This observation does not establish:

- the responses of the tolerated mode-entry steps in this configuration;
- what `0x9053`, `0xD620`, `0xD621`, `GetStorageIDs`, `GetStorageInfo`, or
  `GetObjectHandles` return in this configuration;
- the BLE card-state characteristic or the `0xD212` members in this
  configuration;
- behavior with both slots empty, or with slot 1 loaded and slot 2 empty;
- whether selecting a different import slot changes which slot is checked;
- behavior on the PC tether persona, over USB, or in shooting modes;
- how long the transient settling case lasts.

## Simulator consequence

The simulator reproduces only the observed reply. While the state is active, on
the `app` connection with `image-transfer` detected, `0x9050` answers `0x2019`.
The reply carries no parameters and no data phase because none were recorded.
Every other operation, connection, and mode behaves as the base manifest
describes.
