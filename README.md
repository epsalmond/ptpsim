# Fuji BLE GPS

Laptop-side prototype for pairing/registering with a Fuji GFX100 II over BLE and writing the same GPS payload observed from FUJIFILM reference app.

The first backend targets macOS through Bleak/CoreBluetooth. Dry-run payload commands and tests do not require BLE hardware.

## Setup

```sh
python3 -m venv .venv
.venv/bin/python -m pip install -e '.[test]'
scripts/install_macos_dependencies.sh
.venv/bin/python -m pytest -q
```

Native helper builds are intentionally scriptable and discoverable:

```sh
scripts/build/build-all.sh
scripts/build/build-camera-capture.sh
scripts/build/rebuild-camera-capture.sh
scripts/build/build-ble-identity-advertiser.sh
scripts/build/build-bluetooth-wrapper.sh
```

The coverage run intentionally omits `tui.py`, which is a thin interactive Textual wrapper around the tested command and camera flows.

macOS system dependencies are tracked in `Brewfile`. `blueutil` is required for scripted Bluetooth unpair/forget workflows; without it, macOS requires manual removal through Bluetooth Settings.
Camera-screen OCR evidence additionally uses the optional Python vision extra:

```sh
.venv/bin/python -m pip install -e '.[test,vision]'
scripts/request_macos_camera_permission.sh
```

Some macOS Bluetooth Settings `My Devices` rows are not exposed through `blueutil --paired`; for those, use the UI automation fallback:

```sh
scripts/delete_local_ble_pairing.sh --name "GFX100 II" --ui-automate
```

The UI automation fallback requires Accessibility permission for the terminal app.

## Terminal Workflows

On macOS, approve Bluetooth access for the terminal app before live testing:

```sh
scripts/request_macos_bluetooth_permission.sh
```

If macOS prompts that the terminal wants to use Bluetooth, approve it in the popup or in System Settings. The live BLE flow will fail until this permission is granted.

The default app-level registration name is an reference app-shaped `host-####` token based on the macOS `LocalHostName` when available, falling back to a camera-safe ASCII version of the macOS Computer Name. Fuji examples use short hyphenated names like `Pixel-6-9405`, and the app-level value must identify this host rather than reuse a phone name from reference captures.

Static GFX100 II firmware analysis indicates the camera's Bluetooth device-list display name is stored in the ThreadX `PairingInfo` slot at bond time. It is populated from peer GAP/local-name data for BLE pairing, or PTP-IP `InitiatorFriendlyName` for Wi-Fi tethering; it is not populated by the Fuji `CONNECTED_DEVICE_NAME_STRING` characteristic we write after connecting.

On macOS, changing `ComputerName` and `LocalHostName` to match the app-level name did not fix the blank camera-side displayed name. A parallel CoreBluetooth Local Name advertiser made pairing faster when combined with connect-on-detection, but still left the camera-side name blank because public CoreBluetooth rejects publishing GAP `0x1800` / Device Name `0x2A00`. Treat this as a non-blocking display-name bug for now; GPS sync has been confirmed to work while the camera-side name is blank.

On future Linux/BlueZ work, verify and set the adapter alias before pairing:

```sh
bluetoothctl show | grep -i alias
bluetoothctl system-alias mbp-7274
```

### Setup And Verification

```sh
python3 -m rce.tools.fuji_ble_gps.cli --help
python3 -m rce.tools.fuji_ble_gps.cli decode-payload 7ed88e16caeffeb62100000000000000ea070501001a0e
python3 -m rce.tools.fuji_ble_gps.cli set-location --lat 37.8460286 --lon -122.4806454 --alt 33 --speed 0 --dry-run
.venv/bin/python -m pytest -q
```

### State And Evidence

Use the statefile before deciding the next live action:

```sh
scripts/reset_connection_state.sh --reason "starting fresh"
scripts/evidence/ble_advertisement_scan.sh --timeout 20
scripts/evidence/ble_direct_connect_probe.sh --address <CoreBluetooth UUID>
scripts/evidence/blueutil_paired_device.sh
scripts/evidence/system_profiler_device.sh
scripts/evidence/macos_ioreg_device.sh
scripts/evaluate_connection_state.sh --verbose
```

Manual camera observations are explicit evidence, not guesses:

```sh
scripts/evidence/camera_pairing_mode_manual.sh --value present
scripts/evidence/camera_registered_manual.sh --value present
scripts/evidence/camera_registered_name_manual.sh --value empty
scripts/evidence/camera_gps_icon_manual.sh --value present
```

Camera-screen vision is available as camera-side context. It is useful for reading the Fuji LCD and creating artifacts, but it is not a substitute for host-side BLE/Wi-Fi evidence when choosing protocol workflows. If screen evidence conflicts with deterministic host evidence, collect more evidence before acting.

```sh
scripts/evidence/camera_screen_manual.sh --value waiting_for_connected --note "camera screen: WAITING FOR CONNECTED"
scripts/evidence/camera_bluetooth_status_manual.sh --value ready_not_connected --note "dim Bluetooth icon"
```

The classifier has three distinct loops:

```sh
# 1. Calibrate LCD geometry. Rerun only when the iPhone or camera LCD moves.
scripts/detect_camera_lcd_box.sh --device-name iPhone --warmup 5 --zoom 2

# 2. Capture and classify the current camera screen using the saved LCD box.
scripts/read_camera_screen_state.sh --device-name iPhone --warmup 5 --zoom 2

# 3. Refine classification rules against an existing normalized capture without the camera.
scripts/reclassify_camera_screen_state.sh --capture rce/screen_captures/<timestamp>/capture.json
```

`detect_camera_lcd_box.sh` writes a timestamped `lcd_box.json`, a normalized preview `screen.png`, and updates `rce/state/camera_lcd_box.json`. Use `--image rce/screen_captures/<timestamp>/raw.png` to calibrate from a saved raw frame, and `--no-save` to test detection without replacing the saved calibration. LCD detection currently tries edge geometry, bright LCD color, dark LCD color, and known Fuji glyph geometry from stable UI anchors such as the exposure scale and AF touch glyph.

`read_camera_screen_state.sh` reuses `rce/state/camera_lcd_box.json`; if that file is missing or stale, run `detect_camera_lcd_box.sh` first. It writes local-time artifacts under `rce/screen_captures/<timestamp>/`: lossless `raw.png`, normalized `screen.png`, and parsable `capture.json`. The command prints simple key/value output such as `camera_screen_state=registration_mode`, `confidence=0.82`, `capture=...`, and metadata like `iso=1600` when present. Current known states include `registration_mode` for the Fuji pairing/ready-to-pair screen, `device_not_found_continue_search`, `waiting_for_connected`, `connection_lost`, `app_function_not_found_retry`, `ready_to_take_photo`, and `ready_to_shoot_video`.

`evaluate_connection_state.sh --refresh-screen --verbose` captures the LCD first and then re-evaluates the shared statefile. The screen classifier records `camera_screen_state`, `camera_bluetooth_status=ready_not_connected` when the dim trusted-Bluetooth icon is detected, and `camera_gps_icon=present` only after the GPS icon template exists and matches. The bright active-Bluetooth icon and GPS-set icon still need labeled templates before they can be trusted as programmatic evidence.

`reclassify_camera_screen_state.sh` skips camera capture and LCD warping. It reads an existing `capture.json`, loads its corresponding `screen.png`, and reruns OCR/symbol/classification rules for fast parser iteration and regression checks. Add `--write` only when you want to update the existing `capture.json`.

Unknown screen elements are labeled with:

```sh
scripts/identify_unknown_elements.sh --capture rce/screen_captures/<timestamp>/capture.json
```

The label catalog lives at `rce/screen_captures/screen_element_labels.json`, and accepted template crops are copied into `rce/screen_captures/screen_element_templates/` so reclassifying a capture does not overwrite template images. Re-run `scripts/reclassify_camera_screen_state.sh --capture ... --write` after adding labels.

The Continuity Camera capture path waits two seconds by default before saving a frame. macOS AVFoundation exposes Continuity Camera as a camera device, but not as separate iPhone 2x/3x lens devices in this helper. Use `--zoom` for deterministic output center-crop zoom and verify the saved artifact:

```sh
scripts/capture_continuity_camera_frame.sh --list-devices
scripts/detect_camera_lcd_box.sh --device-name iPhone --warmup 5 --zoom 2
scripts/read_camera_screen_state.sh --device-name iPhone --warmup 5 --zoom 2
scripts/test/retest-camera-capture.sh --warmup 5 --zoom 2
```

Run camera capture checks sequentially. Continuity Camera exposes one camera stream, and parallel permission/capture tests can race each other.

After a live run, record session evidence:

```sh
scripts/evidence/session_registration_name_written.sh --session-dir rce/sessions/laptop_ble_gps_<timestamp>
scripts/evidence/session_registration_id_read.sh --session-dir rce/sessions/laptop_ble_gps_<timestamp>
scripts/evidence/session_registration_ack_written.sh --session-dir rce/sessions/laptop_ble_gps_<timestamp>
scripts/evidence/session_disconnect_after_ack.sh --session-dir rce/sessions/laptop_ble_gps_<timestamp>
scripts/evidence/session_gps_sync_ready.sh --session-dir rce/sessions/laptop_ble_gps_<timestamp>
scripts/evidence/session_gps_payload_written.sh --session-dir rce/sessions/laptop_ble_gps_<timestamp>
```

### Pairing And Registration

Remove host-side pairing when starting from scratch, and also remove the camera-side registration from the camera menu:

```sh
scripts/delete_local_ble_pairing.sh --name "GFX100 II"
scripts/delete_local_ble_pairing.sh --name "GFX100 II" --ui-automate
```

Register without GPS:

```sh
scripts/live_ble_camera_test.sh --device-name mbp-7274 --skip-location --write-registration-ack --timeout 45
```

The live path now uses connect-on-detection: it connects to the first matching Fuji advertisement callback instead of selecting a possibly stale CoreBluetooth identifier after a long scan.

The Local Name advertiser wrapper is available for host-name experiments, but it does not fix the blank camera-side name on macOS:

```sh
scripts/live_ble_with_identity_advertiser.sh --identity-name mbp-7274 -- --skip-location --write-registration-ack --timeout 45
```

### GPS Sync

The currently confirmed minimal GPS refresh is two writes, five seconds apart:

```sh
scripts/live_ble_camera_test.sh --device-name mbp-7274 --write-registration-ack --lat 37.8460286 --lon -122.4806454 --alt 33 --speed 0 --repeat 2 --interval 5 --timeout 45
```

Use the example captures' GPS values through environment variables if preferred:

```sh
FUJI_GPS_LAT=37.8460286 FUJI_GPS_LON=-122.4806454 FUJI_GPS_ALT=33 FUJI_GPS_SPEED=0 scripts/live_ble_camera_test.sh --device-name mbp-7274 --write-registration-ack --repeat 2 --interval 5 --timeout 45
```

### Camera AP Wi-Fi

Prepare the camera AP over BLE. This reads SSID/BSSID/AP state, reads the sensitive passphrase into a `0600` credentials file, and launches the AP with the observed reference app `Take` function value:

```sh
scripts/camera_ap_prepare.sh --device-name mbp-7274 --timeout 45
```

Before AP launch, the BLE path now writes observed reference app setup values when the characteristics are available:

```text
UTC_AND_TIMEZONE: uint16le UTC year, UTC month/day/hour/minute/second, int32le standard timezone HHMM, uint8 DST flag
IMAGE_TRANSFER_SETTING_EX: 01
```

The timezone bytes are based on exact reference app payload examples such as `ea070501033708e0fcffff01`, which decodes to `2026-05-01T03:55:08Z`, standard offset `-0800`, DST active.

Then connect macOS Wi-Fi to the camera AP using the generated credentials file:

```sh
scripts/connect_camera_ap_wifi.sh --credentials rce/sessions/laptop_ble_gps_<timestamp>/wifi_credentials.json
```

The Wi-Fi script assumes this laptop's internet route is already on Ethernet. It records route evidence before and after association, refuses to continue if the default or internet route moves onto Wi-Fi, and requires the camera route to `192.168.0.1` to use the Wi-Fi interface. On the current macOS setup, `networksetup -getairportnetwork` can still report "not associated" while IP, route, and ping evidence prove the camera AP is reachable; treat route/IP evidence as authoritative. The passphrase is not printed or written to the Wi-Fi script logs.

Probe the camera PTP/IP socket during the camera's search window:

```sh
scripts/ptpip_probe.sh --friendly-name mbp-7274
```

This records route evidence and sends an reference app-shaped 82-byte PTP/IP `Init_Command_Request` to `192.168.0.1:55740`. It uses a fixed 26-byte UTF-16LE friendly-name field plus an observed 28-byte reference app tail. If the camera screen has already timed out to normal shooting, rerun the AP prepare step or press the camera's retry control before probing.

For live testing, prefer the combined flow so AP launch, Wi-Fi association, and PTP/IP probing happen inside one camera search window:

```sh
scripts/camera_ap_ptpip_probe_flow.sh --device-name mbp-7274
```

The combined flow does not keep the BLE connection open after AP launch by default. `--hold-ble SEC` is diagnostic only; live testing showed that keeping BLE open could prevent macOS from finding the camera AP.

Current PTP/IP status: TCP connect to `192.168.0.1:55740` succeeds when the camera route is on Wi-Fi. A generated 82-byte init packet with this laptop's name still timed out. Replaying exact captured reference app init payload `rce/reference/ptp_decoded/liveview_payload_00000061.bin` produced a 68-byte `InitCommandAck` once, which proves the socket can enter PTP/IP. Later exact-init retries timed out while TCP still accepted connections; do not infer camera UI state from that timeout. Ask the user for current camera-screen text or record manual evidence.

When scan evidence is absent but the camera screen shows a dim Bluetooth icon on the ready-to-shoot screen, probe the last known CoreBluetooth identifier directly:

```sh
scripts/evidence/ble_direct_connect_probe.sh --address 2B403BE3-8075-4865-D0F8-827BA4076BFF
```

Live evidence showed direct reconnect can succeed even when name-based scan evidence is absent. The combined AP/PTP flow can use the same explicit address:

```sh
scripts/camera_ap_ptpip_probe_flow.sh --address 2B403BE3-8075-4865-D0F8-827BA4076BFF --device-name mbp-7274 --ptpip-init-payload rce/reference/ptp_decoded/liveview_payload_00000061.bin --ptpip-open-session
```

The PTP/IP generator now validates that `liveview`, `get`, and `zeros` tail profiles are 28 bytes, producing an 82-byte packet like the reference app captures. The combined flow can replay exact captured init packets:

```sh
scripts/camera_ap_ptpip_probe_flow.sh --device-name mbp-7274 --ptpip-init-payload rce/reference/ptp_decoded/liveview_payload_00000061.bin
```

To attempt the next raw PTP step after a successful init ack:

```sh
scripts/camera_ap_ptpip_probe_flow.sh --device-name mbp-7274 --ptpip-init-payload rce/reference/ptp_decoded/liveview_payload_00000061.bin --ptpip-open-session
```

The next observed reference app PTP command can be probed with:

```sh
scripts/ptpip_probe.sh --init-payload rce/reference/ptp_decoded/liveview_payload_00000061.bin --open-session --get-prop 0xd212
```

Live evidence returned a 50-byte data packet for `GetDevicePropValue 0xD212` followed by OK (`0x2001`). One run returned `SessionAlreadyOpen` (`0x201e`) for OpenSession, then still answered the property read.

### Lower-Level CLI

These commands are useful for targeted debugging:

```sh
python3 -m rce.tools.fuji_ble_gps.cli scan
python3 -m rce.tools.fuji_ble_gps.cli pair --name "GFX100 II"
python3 -m rce.tools.fuji_ble_gps.cli discover --name "GFX100 II"
python3 -m rce.tools.fuji_ble_gps.cli register --name "GFX100 II" --pair-trigger-first --write-registration-ack
python3 -m rce.tools.fuji_ble_gps.cli set-location --name "GFX100 II" --lat 37.8460286 --lon -122.4806454 --alt 33
python3 -m rce.tools.fuji_ble_gps.cli wifi-info --name "GFX100 II" --read-passphrase --launch-ap take --write-registration-ack
python3 -m rce.tools.fuji_ble_gps.cli tui
```

## Not Yet Implemented

PTP/IP probing is implemented, but the camera has not yet answered our init packet. The project can read camera AP credentials over BLE, write the observed UTC/timezone and image-transfer setup values, launch the camera AP with the `take` launch value, associate macOS Wi-Fi with the AP while preserving the Ethernet internet route, and open TCP to the camera's PTP/IP port.
