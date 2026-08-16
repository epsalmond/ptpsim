# X-A7 legacy mobile app protocol contract

The X-A7 legacy mobile app uses the version 4.10.2 registration and PTP/IP
protocol surface.

- Pairing uses the Fuji camera-information BLE service and a four-byte pairing
  key from the manufacturer-data payload.
- Registration requests MTU 515 without a minimum, then walks the declared
  read and subscription queue in order.
- Wi-Fi launch writes a 16-bit launch mode and polls `apState` by read every
  1000 ms. State 1 is success, state 3 is terminal failure, and state 0 remains
  pending. The polling timeout is 45000 ms.
- The PTP/IP command, event, and live-view ports are 55740, 55741, and 55742.
  Command and event packets use USB framing after the 82-byte legacy
  initialization request.
- Function mode and feature-version properties select photo receipt, GPS
  assist, photo viewing, remote shooting, remote photo viewing, reserved photo
  receipt, and firmware update.

This reduction records only the protocol contract used by the manifest.
