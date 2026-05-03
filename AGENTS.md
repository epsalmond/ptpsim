# Fuji Remote Agent Notes

## Project Summary

This repository is building a laptop-side Fuji BLE GPS pairing and location sync prototype for a Fujifilm GFX100 II. The current implementation is Python-first, with macOS as the first live target through Bleak/CoreBluetooth. The longer-term goal is a robust TUI that can pair with the camera, maintain/diagnose connection state, and update GPS location, with very verbose logging.

Target platforms eventually include:

- macOS
- Windows
- Linux
- Android
- iOS

Python scripts are acceptable for early protocol work. Rust is also acceptable later, especially if we need stronger cross-platform packaging or long-running TUI behavior.

## Source Reference Material

Reference material lives on `nas.local` and is accessed by `scp`.

Known source paths:

```sh
scp eric@nas.local:~/fuji/laptop_ble_gps_agent_prompt.md .

```

The user has said all paths listed in those documents are accessible via `scp`. Copy anything needed into the local workspace before using it. Do not assume copied logs are still current if newer source material exists on the NAS.

## Current Local Shape

Core package:

```text
rce/tools/fuji_ble_gps/
```

Important scripts:

```text
scripts/install_macos_dependencies.sh
scripts/live_ble_camera_test.sh
scripts/live_ble_with_identity_advertiser.sh
scripts/macos_ble_identity_advertiser.sh
scripts/request_macos_bluetooth_permission.sh
scripts/delete_local_ble_pairing.sh
scripts/diagnose_macos_bluetooth_state.sh
scripts/evaluate_connection_state.sh
scripts/detect_camera_lcd_box.sh
scripts/read_camera_screen_state.sh
scripts/reclassify_camera_screen_state.sh
scripts/identify_unknown_elements.sh
scripts/evidence/*.sh
```

Important docs:

```text
CONNECTION_STATES.md
rce/notes/laptop_ble_gps.md
rce/reference/GFX100II_PAIRING_NAME_FIRMWARE_NOTES.md
rce/reference/APP_ACTION_ENUMERATION.md
```

Run artifacts:

```text
rce/sessions/laptop_ble_gps_<timestamp>/
rce/sessions/macos_bluetooth_state_<timestamp>/
rce/screen_captures/<timestamp>/
rce/state/connection_state.json
rce/state/camera_lcd_box.json
```

## Current Progress

Working as of 2026-05-02:

- macOS BLE pairing/registration can complete from the terminal.
- Live actions use connect-on-detection. This avoids stale CoreBluetooth identifiers from scan-then-connect behavior.
- Registration with `--write-registration-ack` can persist the host when the camera is in the correct registration state.
- GPS sync works. The confirmed minimal post-restart flow used two GPS writes five seconds apart and showed the camera GPS icon.
- The camera-side registered host name still displays empty. Treat this as a non-blocking display-name defect for GPS.
- The macOS Local Name advertiser experiment did not fix the blank camera-side name. Public CoreBluetooth rejects reserved GAP `0x1800` / Device Name `0x2A00`.
- AP/Wi-Fi handoff is scripted. BLE can read SSID/BSSID/AP state, store the AP passphrase in a `0600` credentials file with redacted logs, launch the AP, and ask macOS to associate while preserving the Ethernet internet route.
- Before AP launch, the BLE flow writes observed reference app setup values when available: `UTC_AND_TIMEZONE` and `IMAGE_TRANSFER_SETTING_EX=01`.
- Live AP launch evidence: `launch_ap=get` wrote `0300` but stayed at `ap_state=0080`; `launch_ap=take` wrote `0400` and reached `ap_state=0180`.
- Live Wi-Fi evidence: macOS associated with local IP `192.168.0.136`, camera endpoint `192.168.0.1` was reachable, camera route used Wi-Fi `en0`, and default/internet routes stayed on Ethernet `en7`.
- PTP/IP probing is implemented. TCP connect to `192.168.0.1:55740` succeeds when the camera route uses Wi-Fi. Generated laptop-identity init packets still time out, but exact captured reference app init payload replay has produced `InitCommandAck`; exact init plus `OpenSession` plus `GetDevicePropValue 0xD212` has succeeded.
- Camera-screen vision is scripted. LCD geometry is calibrated separately, current screens can be classified through the iPhone Continuity Camera, and preserved `capture.json` artifacts can be reclassified without another camera round trip.

Useful successful sessions:

```text
rce/sessions/laptop_ble_gps_20260502T093421Z  two-pass GPS after camera restart
rce/sessions/laptop_ble_gps_20260502T100213Z  connect-on-detection registration with ack
```

## Determinism Rule

Directive: No guessing. This is an application that does not rely on superstition to determine state or next steps.

Before writing registration or GPS data:

1. Gather evidence using scripts or fresh session artifacts.
2. Assign exactly one state label from `CONNECTION_STATES.md`.
3. Execute only that state's workflow.
4. If evidence conflicts, stop and collect more evidence.

Camera screen text is useful context, but it is not sufficient by itself. Prefer deterministic host-side evidence first:

```sh
scripts/evidence/macos_known_device_plist.sh
scripts/evidence/macos_ioreg_device.sh
scripts/evidence/system_profiler_device.sh
scripts/evidence/blueutil_paired_device.sh
scripts/evidence/blueutil_connected_device.sh
scripts/evidence/ble_advertisement_scan.sh --timeout 20
scripts/evidence/ble_direct_connect_probe.sh --address <CoreBluetooth UUID>
scripts/evaluate_connection_state.sh --verbose
scripts/evaluate_connection_state.sh --refresh-screen --verbose
scripts/reset_connection_state.sh --reason "starting fresh"
```

Transient screen evidence expires after 120 seconds, and `gps_sync_ready` expires after 300 seconds. Do not treat stale manual screen text, old camera-screen vision captures, or old GPS-ready sessions as current connection state.

Camera-screen vision is available for camera-side context, but it is not host-side protocol proof. If host evidence conflicts with screen OCR/classification, stop and collect more evidence before choosing a BLE/Wi-Fi workflow.

Use the camera-screen classifier in distinct steps:

```sh
# Rerun only when the iPhone or camera LCD moves.
scripts/detect_camera_lcd_box.sh --device-name iPhone --warmup 5 --zoom 2

# Capture and classify the current LCD using rce/state/camera_lcd_box.json.
scripts/read_camera_screen_state.sh --device-name iPhone --warmup 5 --zoom 2

# Fast parser loop: rerun classification from an existing normalized capture.
scripts/reclassify_camera_screen_state.sh --capture rce/screen_captures/<timestamp>/capture.json

# Label unknown crops, then reclassify with --write.
scripts/identify_unknown_elements.sh --capture rce/screen_captures/<timestamp>/capture.json
scripts/reclassify_camera_screen_state.sh --capture rce/screen_captures/<timestamp>/capture.json --write
```

Classifier artifacts are local-time directories under `rce/screen_captures/<timestamp>/`. `raw.png` is the captured frame, `screen.png` is the normalized LCD, and `capture.json` is the parsable result. The reusable LCD calibration is `rce/state/camera_lcd_box.json`. The symbol label catalog is `rce/screen_captures/screen_element_labels.json`; stable template crops live under `rce/screen_captures/screen_element_templates/`.

Known camera-screen state labels include `registration_mode` for the Fuji pairing/ready-to-pair screen, `device_not_found_continue_search`, `waiting_for_connected`, `connection_lost`, `app_function_not_found_retry`, `ready_to_take_photo`, and `ready_to_shoot_video`.
The classifier can also record `camera_bluetooth_status=ready_not_connected` for the dim trusted-Bluetooth icon. The GPS-set icon and bright active-Bluetooth icon are not yet labeled templates; do not infer those states from screenshots until labels exist.

Use manual evidence scripts only as a last resort when the state cannot be observed programmatically:

```sh
scripts/evidence/camera_screen_manual.sh --value registration_mode
scripts/evidence/camera_bluetooth_status_manual.sh --value ready_not_connected
scripts/evidence/camera_pairing_mode_manual.sh --value present
scripts/evidence/camera_registered_manual.sh --value absent
scripts/evidence/camera_registered_name_manual.sh --value empty
scripts/evidence/camera_gps_icon_manual.sh --value absent
scripts/evidence/macos_bluetooth_settings_manual.sh --value not_connected
scripts/evidence/session_gps_sync_ready.sh --session-dir rce/sessions/laptop_ble_gps_<timestamp>
scripts/evidence/session_gps_payload_written.sh --session-dir rce/sessions/laptop_ble_gps_<timestamp>
```

After a live registration attempt, collect session evidence:

```sh
scripts/evidence/session_registration_name_written.sh --session-dir rce/sessions/laptop_ble_gps_<timestamp>
scripts/evidence/session_registration_id_read.sh --session-dir rce/sessions/laptop_ble_gps_<timestamp>
scripts/evidence/session_registration_ack_written.sh --session-dir rce/sessions/laptop_ble_gps_<timestamp>
scripts/evidence/session_disconnect_after_ack.sh --session-dir rce/sessions/laptop_ble_gps_<timestamp>
scripts/evidence/session_gps_sync_ready.sh --session-dir rce/sessions/laptop_ble_gps_<timestamp>
```

## Current Protocol Findings

The app-level connected device name must identify this Mac, not a copied phone name from Fuji/reference app logs. On macOS, prefer the short local host name:

```sh
scutil --get LocalHostName
```

Use the Computer Name only as a fallback source:

```sh
scutil --get ComputerName
```

The code converts names to an reference app-shaped `host-####` token before writing them. The known-good Android/reference app trace wrote `Pixel-6-9405`; do not reuse that literal phone name for laptop tests. The code falls back to the short hostname if macOS names are unavailable.

Firmware inspection says the camera's Bluetooth device-list UI reads the displayed name from a persisted ThreadX `PairingInfo` slot. That slot is populated during bond setup from peer GAP Device Name / Local Name data for BLE pairing, or from PTP-IP `InitiatorFriendlyName` for Wi-Fi tethering. It is not populated by the Fuji app-level `CONNECTED_DEVICE_NAME_STRING` write.

Fresh pairing with app-level `CONNECTED_DEVICE_NAME_STRING=mbp-7274`, plus macOS `ComputerName` and `LocalHostName` set to `mbp-7274`, still produced an empty camera-side displayed host name. A later macOS Local Name advertiser experiment also left the camera-side name blank because public CoreBluetooth refused GAP `0x1800` / Device Name `0x2A00`. Do not repeat those macOS identity experiments unless new evidence shows CoreBluetooth can expose a peer-readable GAP Device Name during bonding.

For future Linux/BlueZ work, verify and set the adapter alias before pairing:

```sh
bluetoothctl show | grep -i alias
bluetoothctl system-alias mbp-7274
```

Observed key characteristics:

```text
CONNECTED_DEVICE_NAME_STRING          85b9163e-62d1-49ff-a6f5-054b4630d4a1
CONNECTED_DEVICE_IDENTIFICATION_NUMBER f557d96b-8284-4667-8793-b971c1deca2a
LOCATION_SYNC_CYCLE                   c95d91ae-b247-4d6d-8661-7dd5d6a0f85b
LOCATION_AND_SPEED                    0f36ec14-29e5-411a-a1b6-64ee8383f090
FUJI_CAMERA_SERVICE                   a9d2b304-e8d6-4902-8336-352b772d7597
CAMERA_SSID_NAME_STRING               bf6dc9cf-3606-4ec9-a4c8-d77576e93ea4
CAMERA_WIFI_PASSPHRASE_STRING         e809256a-915c-4967-92e8-53b7d4cad213
FUNCTION_LAUNCH                       600655e6-3637-42f1-8fb2-44efc5c63b13
AP_STATE                              a68e3f66-0fcc-4395-8d4c-aa980b5877fa
```

Live pairing should use connect-on-detection. The old scan-then-connect flow could chase stale CoreBluetooth identifiers after the camera rotated or stopped advertising. `BleakBackend.find_device()` now connects from the first matching detection callback; `scan()` remains a full-window evidence command.

Registration ack behavior:

- Skipping ack can allow identity reads, but the camera does not persist the host registration.
- Writing the reference app-style ack is needed for persistence.
- Writing ack can still cause immediate disconnect if the camera is not in the correct registration state or if the app is chasing a stale CoreBluetooth identifier.
- Do not proceed to GPS writes until camera-side registration is persisted or an immediately preceding session proves `gps_sync_ready=present`.

GPS payload is 23 bytes:

```text
int32le latitude * 10000000
int32le longitude * 10000000
int32le altitude meters
int32le speed meters/sec * 100
uint16le UTC year
uint8   UTC month
uint8   UTC day
uint8   UTC hour
uint8   UTC minute
uint8   UTC second
```

`UTC_AND_TIMEZONE` is a separate 12-byte setup write observed in reference app traces:

```text
uint16le UTC year
uint8   UTC month
uint8   UTC day
uint8   UTC hour
uint8   UTC minute
uint8   UTC second
int32le standard timezone offset as signed HHMM, e.g. -0800
uint8   daylight-saving flag, 1 when DST is active
```

Example reference app payload `ea070501033708e0fcffff01` decodes to `2026-05-01T03:55:08Z`, standard offset `-0800`, DST active.

## macOS Notes

Install macOS system dependencies before live work:

```sh
scripts/install_macos_dependencies.sh
```

These dependencies are also tracked in `Brewfile`. `blueutil` is a project requirement because it lets us script local Bluetooth unpair/forget workflows. Add similar system dependencies when they remove manual state transitions, make evidence collection deterministic, or otherwise advance the project goals.

The terminal app running BLE commands needs macOS Bluetooth permission:

```sh
scripts/request_macos_bluetooth_permission.sh
```

macOS may show a numeric Bluetooth pairing prompt. The user must confirm matching numbers on both macOS and the camera.

macOS does not provide a stable built-in CLI for forgetting one BLE device. `scripts/delete_local_ble_pairing.sh` uses required dependency `blueutil`; if that dependency is missing, it can only open Bluetooth Settings as a manual fallback.

Important nuance: macOS Bluetooth Settings can show a device under `My Devices` even when `blueutil --paired` is empty. For that Settings-only row, use:

```sh
scripts/delete_local_ble_pairing.sh --name "GFX100 II" --ui-automate
```

This uses System Settings UI automation and requires Accessibility permission for the terminal app. `sudo` is not the right model for this; the blocker is per-user UI automation/private Bluetooth state, not Unix file permissions.

## Test And Coverage Rules

Aim for full test coverage. Current expectation is:

```sh
.venv/bin/python -m pytest -q
```

The suite should remain at 100% coverage for the trusted Python package surface. The TUI module and experimental `screen_vision.py` are intentionally omitted from coverage for now in `pyproject.toml`.

When adding behavior:

- Add tests with the implementation.
- Keep hardware-dependent behavior behind mockable boundaries.
- Do not reduce coverage.
- Prefer deterministic parser/state-machine tests for connection-state work.

## Live Testing Rules

Live camera tests should be scripted so the user can approve repeatable command prefixes.

Use registration-only before GPS:

```sh
scripts/live_ble_camera_test.sh --device-name mbp-7274 --skip-location --write-registration-ack --timeout 45
```

Use `scripts/live_ble_camera_test.sh --pair-only --timeout 30` only when isolating the OS-level numeric-pairing prompt as evidence. Pair-only closes after the protected read, so it can leave an orphaned host-accessible/camera-unregistered state.

Only run GPS writes after the state machine says registration is persisted and the camera is ready:

```sh
scripts/live_ble_camera_test.sh --device-name mbp-7274 --write-registration-ack --lat <lat> --lon <lon> --alt <meters> --speed <mps> --repeat 2 --interval 5 --timeout 45
```

Known-good example-capture coordinates:

```text
lat=37.8460286
lon=-122.4806454
alt=33
speed=0
```

The Local Name advertiser wrapper exists for experiments only:

```sh
scripts/live_ble_with_identity_advertiser.sh --identity-name mbp-7274 -- --skip-location --write-registration-ack --timeout 45
```

It improved the live pairing experience when combined with connect-on-detection, but it did not populate the blank camera-side host name.

AP Wi-Fi handoff is split into deterministic steps:

```sh
scripts/camera_ap_prepare.sh --device-name mbp-7274 --timeout 45
scripts/connect_camera_ap_wifi.sh --credentials rce/sessions/laptop_ble_gps_<timestamp>/wifi_credentials.json
scripts/evidence/camera_ap_wifi_session.sh --session-dir rce/sessions/camera_ap_wifi_<timestamp>
scripts/ptpip_probe.sh --friendly-name mbp-7274 --guid f2e4538fada5485d87b27f0bd3d5ded0
scripts/evidence/ptpip_probe_session.sh --session-dir rce/sessions/ptpip_probe_<timestamp>
scripts/ptpip_compare_init.sh --friendly-name mbp-7274 --guid f2e4538fada5485d87b27f0bd3d5ded0
scripts/camera_ap_ptpip_probe_flow.sh --device-name mbp-7274 --ptpip-guid f2e4538fada5485d87b27f0bd3d5ded0
```

The Wi-Fi script must preserve the laptop's Ethernet internet route. It records default/internet route evidence before and after association, refuses to proceed if those routes move to Wi-Fi, and requires the camera endpoint route to use Wi-Fi.
The AP Wi-Fi evidence command parses the association `summary.txt` and records `camera_ap_wifi_association=present`; the state machine classifies that as `camera_ap_wifi_associated_ethernet_default`.
On the current macOS setup, `networksetup -getairportnetwork` can incorrectly report "not associated" even when `en0` has a camera-subnet IP and `192.168.0.1` routes and pings over Wi-Fi. Prefer IP, route, and endpoint reachability evidence.
When the camera screen shows a dim Bluetooth icon on the ready-to-shoot screen, BLE name scan can be absent while direct CoreBluetooth reconnect still succeeds. Probe that state with `scripts/evidence/ble_direct_connect_probe.sh --address 2B403BE3-8075-4865-D0F8-827BA4076BFF`. If present, run the combined AP/PTP flow with `--address`.
The combined AP/PTP flow defaults to `--hold-ble 0`. Holding the BLE connection open after AP launch is diagnostic only; live testing showed it could keep macOS from finding the camera AP.
`scripts/ptpip_probe.sh` sends an reference app-shaped 82-byte Init_Command_Request by default: 16-byte GUID, four zero bytes, a fixed 26-byte UTF-16LE friendly-name field, and a 28-byte reference app tail. It keeps macOS route checks in shell and delegates packet construction, socket exchange, artifacts, and `summary.json` to `rce.tools.fuji_ble_gps.ptpip`. It validates generated tail lengths and supports `--guid HEX` for deterministic generated-identity tests and `--init-payload PATH` for replaying exact captured init payloads. It also supports `--open-session`, which sends raw PTP OpenSession transaction 1 after an init ack, and `--get-prop HEX`, which sends PTP GetDevicePropValue after OpenSession. `scripts/camera_ap_ptpip_probe_flow.sh` passes through `--ptpip-tail-profile`, `--ptpip-guid`, `--ptpip-init-payload`, `--ptpip-open-session`, and `--ptpip-get-prop`.
`scripts/evidence/ptpip_probe_session.sh` parses `summary.json` and records `camera_ap_ptpip_probe` as the highest reached milestone. The state machine classifies values including `tcp_connected_init_timeout`, `init_ack_present`, `open_session_ok`, and `get_prop_d212_ok`.
`scripts/ptpip_compare_init.sh` is offline packet evidence. It decodes and compares Fuji-shaped 82-byte Init_Command_Request packets field by field: packet header, initiator GUID, post-GUID bytes, fixed UTF-16LE friendly-name field, and 28-byte reference app tail. Use it before changing live PTP/IP init identity assumptions.
If the camera shows "NOT FOUND / PLEASE CHECK THE APP AND SELECT THE FUNCTION AGAIN", record `camera_screen_state=app_function_not_found_retry`. That means AP launch and Wi-Fi association were not enough; the app-side PTP/IP/FFIR follow-up did not happen inside the camera's search window.
Continuity Camera capture defaults to a two-second warmup. macOS AVFoundation exposes the iPhone as a Continuity Camera device here, not as separate 2x/3x lens devices; use `scripts/capture_continuity_camera_frame.sh --list-devices` to inspect exposed devices and pass `--zoom 2` or `--zoom 3` for deterministic output center-crop zoom when it gives a better screen crop.

Latest live caveat: the generated init packet is now 82 bytes, but the generated laptop-identity init still timed out. Replaying exact captured reference app init payload `rce/reference/ptp_decoded/liveview_payload_00000061.bin` produced a 68-byte `InitCommandAck`, proving the camera can enter PTP/IP over this laptop's AP route. Exact init plus OpenSession succeeded; a later probe of `GetDevicePropValue 0xD212` returned a 50-byte data packet and OK. Do not infer camera UI state from timeouts; ask the user for current screen text when needed.

Preserve session artifacts after every live attempt. They are evidence.

## Engineering Direction

Short term:

- Expand the evidence/state-machine layer.
- Add workflows for every state in `CONNECTION_STATES.md`.
- Improve recovery from partial pairing/registration.
- Expand AP/Wi-Fi/PTP evidence scripts for AP state, Wi-Fi association, route preservation, camera endpoint reachability, and PTP/IP init response shape.
- Determine why the camera accepts the exact captured reference app initiator identity but not the generated laptop identity. Likely candidates are the initiator GUID/name block and camera-side registration binding.
- Expose the PTP/IP client and init comparator through the TUI.
- Determine whether Linux/BlueZ adapter alias fixes the camera-side display name.
- Investigate whether macOS can expose peer-readable GAP `0x2A00` through private APIs or an external BLE adapter.

Medium term:

- Build a TUI that shows state, evidence, next allowed action, logs, and GPS write status.
- Add deterministic statefile locking or merge semantics if evidence collection becomes parallel.
- Expand screen OCR/vision support for camera UI evidence and keep classifier regression checks based on preserved `capture.json` artifacts.
- Add USB probing where the Fuji camera exposes useful state over USB.
- Promote more of the observed reference app PTP sequence into tested client actions after the init identity rules are understood.

Long term:

- Cross-platform BLE backends for macOS, Windows, Linux, Android, and iOS.
- Stable packaging for non-developer users.
- Continuous location sync with robust reconnection.
- Clear separation between Fuji protocol logic, platform BLE transport, evidence collection, and UI.
