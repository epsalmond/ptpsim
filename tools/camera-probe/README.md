# Fuji Remote

Laptop-side Fuji camera-control prototype for a Fujifilm GFX100 II.

The current implementation is Python-first on macOS. It can pair/register over BLE, write GPS location updates, launch the camera AP, associate macOS Wi-Fi to that AP while preserving the normal internet route, probe PTP/IP camera-control flows, and model the observed firmware-update transfer. The longer-term target is a robust TUI and eventually native app/library support across macOS, Windows, Linux, Android, and iOS.

FF80 service-mode research has moved to the sibling project:

```text
../fuji-ff80
```

Keep this repository focused on the user-facing remote-control application.

## Setup

```sh
python3 -m venv .venv
.venv/bin/python -m pip install -e '.[test]'
scripts/install_macos_dependencies.sh
.venv/bin/python -m pytest -q
```

Camera-screen classification uses optional vision dependencies:

```sh
.venv/bin/python -m pip install -e '.[test,vision]'
scripts/request_macos_camera_permission.sh
```

macOS system dependencies are tracked in `Brewfile`.

- `blueutil`: scripted Bluetooth evidence and unpair/forget workflows.
- `ffmpeg`: Continuity Camera frame capture support.
- `tesseract`: OCR support for camera-screen classification.

## Build Helpers

Native helper builds are intentionally scriptable and discoverable:

```sh
scripts/build/build-all.sh
scripts/build/build-camera-capture.sh
scripts/build/rebuild-camera-capture.sh
scripts/build/build-ble-identity-advertiser.sh
scripts/build/build-bluetooth-wrapper.sh
```

## Deterministic State

Directive: no guessing. This application does not rely on superstition to determine state or next steps.

Before registration, GPS writes, AP launch, or PTP/IP control:

1. Gather fresh evidence.
2. Assign exactly one state label from `CONNECTION_STATES.md`.
3. Run only that state's workflow.
4. Stop and collect more evidence when host evidence and camera-screen evidence conflict.

Useful evidence commands:

```sh
scripts/evaluate_connection_state.sh --verbose
scripts/evaluate_connection_state.sh --refresh-screen --verbose
scripts/reset_connection_state.sh --reason "starting fresh"
scripts/evidence/ble_advertisement_scan.sh --timeout 20
scripts/evidence/ble_direct_connect_probe.sh --address <CoreBluetooth UUID>
scripts/evidence/blueutil_paired_device.sh
scripts/evidence/blueutil_connected_device.sh
scripts/evidence/system_profiler_device.sh
scripts/evidence/camera_usb_probe.sh
```

If `scripts/read_camera_screen_state.sh` returns `camera_screen_state=unknown`, treat that as a workflow error. Fix the classifier/templates or camera/iPhone alignment before proceeding.

## BLE Pairing And GPS

On macOS, approve Bluetooth access before live BLE testing:

```sh
scripts/request_macos_bluetooth_permission.sh
```

Registration-only flow:

```sh
scripts/live_ble_camera_test.sh --device-name mbp-7274 --skip-location --write-registration-ack --timeout 45
```

GPS flow after the state machine shows the camera is registered and ready:

```sh
scripts/live_ble_camera_test.sh \
  --device-name mbp-7274 \
  --write-registration-ack \
  --lat 37.8460286 \
  --lon -122.4806454 \
  --alt 33 \
  --speed 0 \
  --repeat 2 \
  --interval 5 \
  --timeout 45
```

Confirmed behavior:

- Pairing/registration can complete from the terminal on macOS.
- Current reference app/client application pairing treats advertisement identity as protocol data.
  A first-pair connection candidate needs a known connectable Fuji service UUID
  plus Fujifilm manufacturer data: legacy `0x02 + 4-byte key` or RED
  `0x01 + 5-byte short serial`. Name-only advertisements are display/evidence
  candidates, not enough to open a first-pair BLE link when advertisement
  metadata is available.
- When the pairing identity is available, write `PAIRING_KEY`
  (`ABA356EB-9633-4E60-B73F-F52516DBD671`) before
  `CONNECTED_DEVICE_NAME_STRING`. If `CONNECTED_DEVICE_IDENTIFICATION_NUMBER`
  is present, read the camera-returned 4 bytes and acknowledge those bytes with
  the little-endian `0x20000000` bit set; do not write a fixed constant.
- A fast protected read is not automatically stale trust. It can be the normal
  signature of an already trusted BLE link, so continue Fuji registration and
  post-registration sync unless the follow-on path fails.
- A disconnect while the registration ACK write is in flight can be the accepted
  path on current firmware. Preserve the session, re-evaluate state, and
  reconnect for operational work instead of treating the ACK write completion as
  the only proof of acceptance.
- Saved reconnect is not first-pair. The product app persists the exact
  `PAIRING_KEY` payload from first pair and replays it on saved reconnect before
  connected-device name and operational post-registration sync. This Python
  prototype can use an explicit CoreBluetooth address for warm reconnects, but
  it does not yet maintain a saved-camera registry.
- GPS sync works; two GPS writes five seconds apart showed the camera GPS icon after restart.
- The camera-side registered host display name can remain empty on macOS. Treat that as a non-blocking display-name bug while GPS and control flows continue.

## AP Wi-Fi And PTP/IP

AP Wi-Fi handoff is split into deterministic steps:

```sh
scripts/camera_ap_prepare.sh --device-name mbp-7274 --timeout 45
scripts/connect_camera_ap_wifi.sh --credentials rce/sessions/laptop_ble_gps_<timestamp>/wifi_credentials.json
scripts/evidence/camera_ap_wifi_session.sh --session-dir rce/sessions/camera_ap_wifi_<timestamp>
scripts/ptpip_probe.sh --friendly-name mbp-7274 --guid f2e4538fada5485d87b27f0bd3d5ded0
scripts/evidence/ptpip_probe_session.sh --session-dir rce/sessions/ptpip_probe_<timestamp>
```

Combined flow:

```sh
scripts/camera_ap_ptpip_probe_flow.sh \
  --device-name mbp-7274 \
  --ptpip-guid f2e4538fada5485d87b27f0bd3d5ded0 \
  --ptpip-open-session \
  --ptpip-get-prop 0xd212
```

Browse and download flows:

```sh
scripts/ptpip_probe.sh \
  --friendly-name mbp-7274 \
  --guid f2e4538fada5485d87b27f0bd3d5ded0 \
  --open-session \
  --app-sequence sdcard-folder-and-dates

scripts/ptpip_probe.sh \
  --friendly-name mbp-7274 \
  --guid f2e4538fada5485d87b27f0bd3d5ded0 \
  --open-session \
  --app-sequence sdcard-object-handles

scripts/camera_ap_download_object.sh --handle 0x0000000c
scripts/ptpip_export_object.sh --session-dir rce/sessions/ptpip_probe_<timestamp> --output-dir rce/downloads/manual_export
```

The Wi-Fi scripts preserve the laptop's normal internet route. When Ethernet is
present, default/internet routes must stay on Ethernet while the camera endpoint
route uses Wi-Fi.

Current AP handoff assumptions, matching client application's hardware path:

- Normal handoff reads the camera-provided SSID/passphrase and does not rewrite
  the SSID/rename characteristic by default.
- AP launch rewrites `IMAGE_TRANSFER_SETTING_EX = 01` when available, writes the
  mode-specific `FUNCTION_LAUNCH_REQUEST`, then waits for `AP_STATE = 0180`.
  A previous `0180` observation does not let a later workflow skip the function
  launch write because AP state does not encode the requested function mode.
- Write completion is not AP readiness. A BLE disconnect after the launch write
  is a failed AP launch unless `AP_STATE = 0180` was already observed.
- Keep `--hold-ble` / `--hold-after-launch` at `0` for normal attempts. Holding
  BLE open after AP launch is diagnostic only; prior live evidence showed it can
  interfere with finding the camera AP.
- Route evidence owns Wi-Fi success. On macOS that means local Wi-Fi IP plus a
  camera route over Wi-Fi while internet/default routes stay where expected. On
  iOS/client application, hidden SSID status is not enough; the first real PTP/IP command
  socket is the route proof.

Known-good PTP/IP identity:

```text
guid=f2e4538fada5485d87b27f0bd3d5ded0
friendly-name=mbp-7274
```

Generated init with that GUID and laptop friendly name succeeded through `GetDevicePropValue 0xD212`. A generated fresh GUID timed out at init. The remaining identity question is how to obtain or register a laptop-owned accepted initiator GUID.

PTP/IP browse and object transfer have been live-tested. The app has successfully listed SD-card folders/dates, listed object handles, fetched ObjectInfo/thumbnail data, and downloaded a complete 4000x3000 JPEG through `GetObject`. Preserve PTP/IP session artifacts because they are the evidence source for export and parser fixes.

## Firmware Update Model

The successful 2026-05-08 reference app firmware-update capture is stored under:

```text
rce/reference/firmware_update_20260508/
```

The modeled flow is:

1. BLE writes a 92-byte `FirmwareUpdateRequestInfo` to `b1307521-7ac5-4199-aaee-9d094781ce69`.
2. BLE writes `FUNCTION_LAUNCH=0500` to launch firmware transfer mode.
3. Wi-Fi associates to the camera AP.
4. PTP/IP opens session, sets FunctionMode `0xdf01=0x0013`, sets firmware transfer version `0xdf27=1`, sends vendor `0x9040` object info for `FUP_FILE.DAT`, then streams the DAT through vendor `0x9042` in 1 MiB chunks.

Build the BLE request/AP handoff:

```sh
scripts/firmware_update_prepare.sh --dat /path/to/GXUP0006.DAT --claim-version 2.41
```

Build a dry-run PTP/IP upload plan:

```sh
scripts/ptpip_firmware_update.sh --dat /path/to/GXUP0006.DAT
```

Actually uploading firmware bytes is destructive and requires an explicit flag:

```sh
scripts/ptpip_firmware_update.sh --dat /path/to/GXUP0006.DAT --execute
```

The PTP upload script records a session, route evidence, SHA-256, chunk plan, the generated 839-byte `SendObjectInfo` payload, and first/last chunk command/response artifacts. `--execute` refuses to proceed unless the camera endpoint route uses Wi-Fi.

## Screen Classification

Use the camera-screen classifier as camera-side context, not host-side protocol proof.

```sh
scripts/detect_camera_lcd_box.sh --device-name iPhone --warmup 5 --zoom 2
scripts/read_camera_screen_state.sh --device-name iPhone --warmup 5 --zoom 2
scripts/reclassify_camera_screen_state.sh --capture rce/screen_captures/<timestamp>/capture.json
scripts/identify_unknown_elements.sh --capture rce/screen_captures/<timestamp>/capture.json
```

Known labels include:

```text
registration_mode
device_not_found_continue_search
waiting_for_connected
connection_lost
app_function_not_found_retry
ready_to_take_photo
ready_to_shoot_video
```

The classifier can also record `camera_bluetooth_status=ready_not_connected` for the dim trusted-Bluetooth icon.

## Tests

Aim for full coverage on the trusted Python package surface:

```sh
.venv/bin/python -m pytest -q
```

Hardware-dependent behavior should stay behind mockable boundaries. The TUI module is intentionally omitted from coverage for now; `screen_vision.py` is covered by preserved fixture images.
