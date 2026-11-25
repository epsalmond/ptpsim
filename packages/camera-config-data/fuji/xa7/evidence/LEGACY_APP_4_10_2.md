# X-A7 legacy vendor mobile app 4.10.2 reduction

## Scope

This document records how the legacy vendor mobile app, version 4.10.2,
connects to an X-A7. It comes from analysis of the app's code. Statements
describe what the app does, not what the camera requires. No capture of this
app with an X-A7 exists. Where the analysis does not state something, this
document says it is unknown.

## Discovery

- The app accepts adverts carrying the camera-information service
  `117C4142-EDD4-4C77-8696-DD18EEBB770A` and a device name longer than four
  characters. The first four characters are the short serial.
- Manufacturer byte 0 is a bit field. Bit 1 means the next four bytes are the
  pairing key, which the app copies as advertised.
- A pairing scan uses only keyed adverts. A reconnect scan uses only keyless
  adverts.

## Connection setup

The app connects, waits 600 ms, requests MTU 515, then discovers services. It
does not call the platform bonding API; registration is application GATT
traffic.

## Registration queue

Fresh pairing and saved-key reconnect run the same queue:

1. Write the four-byte pairing key to `ABA356EB-9633-4E60-B73F-F52516DBD671`.
2. Write the phone name followed by one NUL byte to
   `85B9163E-62D1-49FF-A6F5-054B4630D4A1`.
3. If present, write protocol version `02 00` to
   `EB4166B0-9CCA-445E-A4E4-75B3817FD57A`.
4. Read, in order: GAP device name `2A00`, serial `2A25`, SSID, reservation
   setting, paired-device count, firmware `2A26`, lens product, lens firmware,
   wake mode, startup value, and disconnect reason. Then read card state, movie
   state, and camera BLE protocol version only when discovered.
5. When the `2A00` read completes with status 0, the app issues a second,
   byte-identical name write outside the queue. It races the next queued read
   (`2A25`), so either can reach the wire first.
6. Enable CCCDs, in order, for SSID, AP state, transfer state, date sync,
   location sync, and firmware state (only when discovered). SSID, AP state,
   transfer state, and firmware state use indication (`02 00`). Date sync and
   location sync use notification (`01 00`).
7. When the last descriptor callback finds the CCCD queue empty, the app marks
   the link ready. In parallel it reads AP state, then transfer state. Ready
   does not wait for those two reads.

Reservation setting feeds only the app's analytics. Wake mode and startup value
have no consumer in the app.

## Failure handling

- A failed pairing-key write disconnects. That covers a missing key
  characteristic, a key write that cannot be issued, and an error status in the
  key write callback.
- A nonzero service-discovery status disconnects.
- Any name or protocol-version write callback advances the queue, whatever its
  status. A write that cannot be issued, or has no callback, stalls the queue.
- A read error callback advances the queue. A read that cannot be issued
  abandons the remaining reads and moves on to CCCD registration. A read with no
  callback stalls the queue. No read failure disconnects.
- A missing CCCD characteristic or descriptor, or a CCCD write that cannot be
  issued, is skipped. Any descriptor callback advances to the next entry,
  whatever its status. A CCCD write with no callback stalls the queue. If it
  is the last one, the app can still mark the link ready but does not issue the
  AP-state and transfer-state reads.
- The app makes up to 3 attempts to issue each GATT operation, waiting up to
  5 s for each callback. There is no overall registration deadline.

## Access-point launch

Function launch is a `u16-le` write to `600655E6-3637-42F1-8FB2-44EFC5C63B13`:
`01 00` photo receiver, `02 00` GPS assist, `03 00` photo viewer, `04 00`
remote shooting, `05 00` firmware update.

- The write is not gated on AP state, on a read, or on registration completion.
- The app writes on the open registration link. Launch adds no subscribe and no
  read; it relies on the AP-state CCCD enabled during registration.
- After a successful write, the app waits for an AP-state indication on
  `A68E3F66-0FCC-4395-8D4C-AA980B5877FA` with no timeout.

The app reads AP state as a `u16-le`:

| Value | Meaning in the app | What the app does after a launch write |
| ---: | --- | --- |
| 0 | not launched or failed | ends the launch as failed |
| 1 | launched and connectable | joins the SSID, then disconnects BLE |
| 2 | launching | ignores it |
| 3 | reserved or automatic transfer | ends the launch as not connectable |

The app treats AP state 3 together with transfer state 1 as a camera ready for
reserved (automatic) transfer. It never writes the launch characteristic to
start that transfer. It
reads no passphrase or BSSID.

## Pairing-screen cue

Unknown. Which event ends the camera's pairing screen is camera firmware logic
and cannot be determined from the app. The app sends no ready message on the
wire.

A field run on an X-A7 sent the second name write, the CCCD sequence, and the
AP-state and transfer-state reads. The camera stayed on its pairing screen.
Reading the serial once after 5 s was followed by a BLE disconnect, and the
camera went back to keyed pairing adverts. Reading the serial three times at
5 s intervals kept the link up, but the camera stayed on its pairing screen.
That run sent no time write and made no GATT `2A00` read. See #355.

## Not modelled

The manifest does not perform these app behaviors.

- **Date sync.** When a date-sync-state notification carries value 1, the app
  queues a 7-byte local-time write to `B9BFD37F-CCAD-4D36-A1EE-018E792B3EDF`
  (current-time service `E872B11F-D526-4AE1-9BB4-89A99D48FA59`): `u16-le` year,
  then one byte each for month, day, hour, minute, and second. The analysis
  does not state whether the month is 0-based or 1-based, or the width of the
  date-sync notification. Whether an X-A7 sends this notification is unknown.
- **Location sync.** When a location-sync-state notification carries value 1,
  the app looks for a location fix younger than 2 hours, waiting at most 10 s.
  With a fix it writes 23 bytes to `0F36EC14-29E5-411A-A1B6-64EE8383F090`.
  Without one it writes nothing.
- **Periodic serial read.** While the link is ready, the app reads `2A25` every
  5000 ms, starting about 5 s after the link becomes ready. If the read cannot
  be issued, the app disconnects. A launch write suspends these reads.

Neither the date-sync write nor the location write is a sequenced registration
step in the app.

## Manifest deviations

- **AP-state read poll.** `legacy-app-establish-wifi-ap` reads AP state every
  1000 ms for up to 45000 ms instead of waiting for an indication with no
  timeout. Both values are client policy. The poll exists because two
  indication waits on an X-A7 received nothing while reads of the
  characteristic worked (#412). States 0 and 2 keep the poll waiting, because
  one read cannot tell "not launched yet" from "launch failed". State 3 ends
  the walk. The poll has no recorded success on hardware: its one recorded run
  timed out with no AP-state change (#355). The run that brought the access
  point up used fixed delays and reads (#412).
- **Strict serial and SSID reads.** The app continues past any read failure.
  The manifest fails the walk if the `2A25` or SSID read fails: on iOS the
  `2A25` read starts bonding, both values are persisted, and the SSID drives
  the Wi-Fi join.
- **Tolerance model.** A tolerant manifest step continues past any failure.
  The app's continuation depends on the kind of failure, as listed above.
- **Launch-time reconnect and resubscribe.** `legacy-app-establish-wifi-ap`
  connects, discovers services, and subscribes to AP state before the launch
  write, so it can run on its own. The app launches on the open link and adds
  no subscribe. On an open link the connect and discovery steps are
  checkpoints; the subscribe rewrites the AP-state CCCD.
- **Peripheral name.** On hosts without GAP access, `blePeripheralName` stands
  in for the `2A00` GATT read (schema 11.4b). The manifest issues the second
  name write after that step and before `2A25`, one of the two orders the app
  can produce. The step is strict, while the app continues past a failed
  `2A00` read but then skips the second name write.
- **Registration confirmation.** No step is marked `confirms: registration`,
  so a completed walk does not show that the camera accepted the registration.

## PTP/IP transport

- The command, event, and live-view ports are 55740, 55741, and 55742. The
  event and live-view sockets are plain TCP connects with no init request.
- The command socket starts with the 82-byte legacy initialization request.
  After the acknowledgement it carries USB-framed PTP containers.
- Function mode and feature-version properties select photo receipt, GPS
  assist, photo viewing, remote shooting, remote photo viewing, reserved photo
  receipt, and firmware update.
