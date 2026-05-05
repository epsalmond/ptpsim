# Fuji Remote Agent Notes

## Project Summary

This repository is building a laptop-side Fuji camera remote-control application for a Fujifilm GFX100 II. The current implementation is Python-first, with macOS as the first live target through Bleak/CoreBluetooth.

Working areas in this repository:

- BLE pairing and registration
- BLE GPS location sync
- deterministic connection-state evidence
- camera-screen classification through Continuity Camera
- camera AP Wi-Fi launch and macOS association
- PTP/IP camera-control probes and download workflows
- eventual TUI and cross-platform application/library extraction

FF80 service-mode research, RAM dumps, bootloader work, and code-execution experiments have moved to:

```text
../fuji-ff80
```

Do not add new FF80 docs, scripts, or artifacts back to this repository.

## Source Reference Material

Reference material lives on `nas.local` and is accessed by `scp`.

Known app-protocol source paths:

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
scripts/live_ble_camera_test.sh
scripts/live_ble_with_identity_advertiser.sh
scripts/macos_ble_identity_advertiser.sh
scripts/request_macos_bluetooth_permission.sh
scripts/request_macos_camera_permission.sh
scripts/delete_local_ble_pairing.sh
scripts/diagnose_macos_bluetooth_state.sh
scripts/evaluate_connection_state.sh
scripts/detect_camera_lcd_box.sh
scripts/read_camera_screen_state.sh
scripts/reclassify_camera_screen_state.sh
scripts/identify_unknown_elements.sh
scripts/camera_ap_prepare.sh
scripts/connect_camera_ap_wifi.sh
scripts/camera_ap_ptpip_probe_flow.sh
scripts/ptpip_probe.sh
scripts/ptpip_compare_init.sh
scripts/ptpip_inventory_init.sh
scripts/ptpip_export_object.sh
scripts/camera_ap_download_object.sh
scripts/evidence/*.sh
```

Important docs:

```text
README.md
BACKLOG.md
CONNECTION_STATES.md
rce/notes/laptop_ble_gps.md
rce/reference/GFX100II_PAIRING_NAME_FIRMWARE_NOTES.md
rce/reference/APP_ACTION_ENUMERATION.md
rce/reference/APP_LIVE_HANDOFF.md
```

Run artifacts:

```text
rce/sessions/laptop_ble_gps_<timestamp>/
rce/sessions/camera_ap_wifi_<timestamp>/
rce/sessions/ptpip_probe_<timestamp>/
rce/screen_captures/<timestamp>/
rce/state/connection_state.json
rce/state/camera_lcd_box.json
```

## Current Progress

Working as of 2026-05-05:

- macOS BLE pairing/registration can complete from the terminal.
- Live actions use connect-on-detection to avoid stale CoreBluetooth identifiers.
- Registration with `--write-registration-ack` can persist the host when the camera is in the correct registration state.
- GPS sync works. The confirmed minimal post-restart flow used two GPS writes five seconds apart and showed the camera GPS icon.
- The camera-side registered host name still displays empty on macOS. Treat this as a non-blocking display-name defect for GPS/control work.
- AP/Wi-Fi handoff is scripted. BLE can read SSID/BSSID/AP state, store the AP passphrase in a `0600` credentials file with redacted logs, launch the AP, and ask macOS to associate while preserving the Ethernet internet route.
- PTP/IP probing is implemented. Generated init using accepted reference app GUID `f2e4538fada5485d87b27f0bd3d5ded0`, laptop friendly name `mbp-7274`, and the liveview tail succeeded through `GetDevicePropValue 0xD212`.
- SD-card browse and object download flows are live-tested. The scripts can list folders/dates, list object handles, fetch ObjectInfo/thumbnail data, download a complete JPEG, and export it with a sidecar manifest.
- Camera-screen vision is scripted. LCD geometry is calibrated separately, current screens can be classified through the iPhone Continuity Camera, and preserved `capture.json` artifacts can be reclassified without another camera round trip.

## Determinism Rule

Directive: no guessing. This is an application that does not rely on superstition to determine state or next steps.

Before writing registration or GPS data:

1. Gather evidence using scripts or fresh session artifacts.
2. Assign exactly one state label from `CONNECTION_STATES.md`.
3. Execute only that state's workflow.
4. If evidence conflicts, stop and collect more evidence.

Statefile evidence writes use `rce/state/connection_state.json.lock` around load/modify/save. Prefer sequential evidence collection in live workflows unless parallel collection is explicitly needed.

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

If `scripts/read_camera_screen_state.sh` returns `camera_screen_state=unknown`, treat that as a workflow error and stop. Do not manually interpret `screen.png` as protocol evidence; fix the classifier/templates or LCD/iPhone alignment, then rerun the script until it returns a named state.

## Current Protocol Findings

The app-level connected device name must identify this Mac, not a copied phone name from Fuji/reference app logs. On macOS, prefer:

```sh
scutil --get LocalHostName
```

Use Computer Name only as a fallback:

```sh
scutil --get ComputerName
```

The code converts names to an reference app-shaped `host-####` token before writing them. The known-good Android/reference app trace wrote `Pixel-6-9405`; do not reuse that literal phone name for laptop tests.

Firmware inspection says the camera's Bluetooth device-list UI reads the displayed name from a persisted ThreadX `PairingInfo` slot populated during bond setup from peer GAP Device Name / Local Name data, or from PTP-IP `InitiatorFriendlyName` for Wi-Fi tethering. It is not populated by the Fuji app-level `CONNECTED_DEVICE_NAME_STRING` write.

Fresh pairing with app-level `CONNECTED_DEVICE_NAME_STRING=mbp-7274`, plus macOS `ComputerName` and `LocalHostName` set to `mbp-7274`, still produced an empty camera-side displayed host name. A later macOS Local Name advertiser experiment also left the camera-side name blank because public CoreBluetooth refused GAP `0x1800` / Device Name `0x2A00`. Do not repeat those macOS identity experiments unless new evidence shows CoreBluetooth can expose a peer-readable GAP Device Name during bonding.

For future Linux/BlueZ work, verify and set the adapter alias before pairing:

```sh
bluetoothctl show | grep -i alias
bluetoothctl system-alias mbp-7274
```

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

Known-good example-capture coordinates:

```text
lat=37.8460286
lon=-122.4806454
alt=33
speed=0
```

## macOS Notes

Install macOS system dependencies before live work:

```sh
scripts/install_macos_dependencies.sh
```

`blueutil` is a project requirement because it lets us script local Bluetooth unpair/forget workflows. Add similar system dependencies when they remove manual state transitions, make evidence collection deterministic, or otherwise advance the remote-control app goals.

The terminal app running BLE commands needs macOS Bluetooth permission:

```sh
scripts/request_macos_bluetooth_permission.sh
```

macOS may show a numeric Bluetooth pairing prompt. The user must confirm matching numbers on both macOS and the camera.

Some Bluetooth Settings `My Devices` rows are not exposed through `blueutil --paired`. For those:

```sh
scripts/delete_local_ble_pairing.sh --name "GFX100 II" --ui-automate
```

This uses System Settings UI automation and requires Accessibility permission for the terminal app.

## Test And Coverage Rules

Aim for full test coverage:

```sh
.venv/bin/python -m pytest -q
```

When adding behavior:

- Add tests with the implementation.
- Keep hardware-dependent behavior behind mockable boundaries.
- Do not reduce coverage.
- Prefer deterministic parser/state-machine tests for connection-state work.
- `screen_vision.py` is covered with preserved fixture images; only the thin `tui.py` wrapper is omitted from coverage.

## Live Testing Rules

Use registration-only before GPS:

```sh
scripts/live_ble_camera_test.sh --device-name mbp-7274 --skip-location --write-registration-ack --timeout 45
```

Only run GPS writes after the state machine says registration is persisted and the camera is ready:

```sh
scripts/live_ble_camera_test.sh --device-name mbp-7274 --write-registration-ack --lat <lat> --lon <lon> --alt <meters> --speed <mps> --repeat 2 --interval 5 --timeout 45
```

AP Wi-Fi handoff is split into deterministic steps:

```sh
scripts/camera_ap_prepare.sh --device-name mbp-7274 --timeout 45
scripts/connect_camera_ap_wifi.sh --credentials rce/sessions/laptop_ble_gps_<timestamp>/wifi_credentials.json
scripts/evidence/camera_ap_wifi_session.sh --session-dir rce/sessions/camera_ap_wifi_<timestamp>
scripts/ptpip_probe.sh --friendly-name mbp-7274 --guid f2e4538fada5485d87b27f0bd3d5ded0
scripts/evidence/ptpip_probe_session.sh --session-dir rce/sessions/ptpip_probe_<timestamp>
scripts/camera_ap_ptpip_probe_flow.sh --device-name mbp-7274 --ptpip-guid f2e4538fada5485d87b27f0bd3d5ded0
scripts/ptpip_probe.sh --friendly-name mbp-7274 --guid f2e4538fada5485d87b27f0bd3d5ded0 --open-session --app-sequence sdcard-folder-and-dates
scripts/ptpip_probe.sh --friendly-name mbp-7274 --guid f2e4538fada5485d87b27f0bd3d5ded0 --open-session --app-sequence sdcard-object-handles
scripts/camera_ap_download_object.sh --handle 0x0000000c
scripts/ptpip_export_object.sh --session-dir rce/sessions/ptpip_probe_<timestamp> --output-dir rce/downloads/manual_export
```

Preserve session artifacts after every live attempt. They are evidence.

## Engineering Direction

Short term:

- Expand the evidence/state-machine layer.
- Add workflows for every state in `CONNECTION_STATES.md`.
- Improve recovery from partial pairing/registration.
- Determine how to obtain or register a laptop-owned accepted PTP/IP initiator GUID.
- Expose PTP/IP client actions and init comparison through the TUI.
- Expand browse/download workflows from PTP/IP probes into tested application actions.

Medium term:

- Build a TUI that shows state, evidence, next allowed action, logs, and GPS write status.
- Add deterministic statefile locking or merge semantics if evidence collection becomes parallel.
- Expand screen classifier regression coverage from preserved `capture.json` artifacts.
- Promote observed reference app PTP sequences into tested client actions.
- Distill protocol logic into a Rust library when interfaces stabilize.

Long term:

- Cross-platform BLE backends for macOS, Windows, Linux, Android, and iOS.
- Stable packaging for non-developer users.
- Continuous location sync with robust reconnection.
- Clear separation between Fuji protocol logic, platform transports, evidence collection, and UI.
