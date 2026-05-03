# Backlog

## Bugs

### BUG-001: Camera registration completes but camera-side host name is blank

Status: understood; macOS workaround still open

Observed: 2026-05-02

Summary:

The Fuji GFX100 II reports `PAIRING HAS COMPLETED` and BLE registration reaches `gps_sync_ready`, but the camera's registered-device list displays an empty host name.

Evidence:

- `rce/sessions/laptop_ble_gps_20260502T071758Z`: app-level name `eric’s MacBook Pro (2)`; camera name displayed empty.
- `rce/sessions/laptop_ble_gps_20260502T073029Z`: fresh registration with ASCII app-level name `eric's MacBook Pro (2)`; camera name displayed empty.
- `rce/sessions/laptop_ble_gps_20260502T075852Z`: fresh registration with reference app-shaped app-level name `mbp-7274`; camera name displayed empty.
- `rce/sessions/laptop_ble_gps_20260502T081250Z`: fresh registration with macOS `ComputerName` and `LocalHostName` temporarily set to `mbp-7274`, and app-level name `mbp-7274`; camera name displayed empty.
- `rce/sessions/laptop_ble_gps_20260502T100213Z`: fresh registration using connect-on-detection plus macOS Local Name advertiser `mbp-7274`; pairing was fast and registration succeeded, but camera name displayed empty.

Current facts:

- Pairing/registration completes.
- `CONNECTED_DEVICE_NAME_STRING` writes succeed.
- Registration id reads and ack writes succeed.
- Location/date sync reaches `0100`.
- Camera UI display name remains empty even when the OS-level macOS pairing identity and Fuji app-level name match.
- Static GFX100 II firmware inspection indicates the Bluetooth device-list UI reads the displayed name from the persisted ThreadX `PairingInfo` slot. It does not synthesize a name from the BD_ADDR at render time.
- The `PairingInfo` display-name slot is populated by bond-time peer GAP/local-name data for BLE pairing, or by PTP-IP `InitiatorFriendlyName` for Wi-Fi tethering. It is not populated by our later `CONNECTED_DEVICE_NAME_STRING` write.
- macOS public CoreBluetooth allows Local Name advertising from the helper, but rejects publishing reserved GAP service `0x1800` / Device Name `0x2A00`.

Falsified hypotheses:

- The blank name is caused only by a Unicode smart apostrophe.
- The blank name is caused only by spaces/punctuation in the app-level name.
- The blank name is fixed by an reference app-shaped `host-####` app-level name.
- The camera UI simply displays macOS `ComputerName` / `LocalHostName`.
- The blank name is fixed by a parallel macOS Local Name advertisement alone.

Implications:

- Treat this as a display-name bug only; it is not currently blocking GPS writes when state is `gps_sync_ready`.
- On Linux/BlueZ, set and verify the adapter alias before pairing; firmware analysis indicates this is the source for the peer GAP Device Name / advertisement Local Name path.
- On macOS/CoreBluetooth, changing `ComputerName` and `LocalHostName` was not enough, and advertising Local Name alone was not enough. A real fix likely needs a peer-readable GAP Device Name during bonding, if macOS permits that through private APIs or an external BLE adapter.

Next investigation:

- Compare Android bond metadata and BLE pairing identity against macOS CoreBluetooth behavior.
- Determine whether macOS can expose a peer-readable GAP `0x2A00` Device Name while our client performs the central-side camera registration flow.
- For Linux support, add a pre-pairing alias check around `bluetoothctl show` / `bluetoothctl system-alias`.
