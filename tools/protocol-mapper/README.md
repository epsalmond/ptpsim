# protocol-mapper

`protocol-mapper` is ptpsim's **probe and observation toolkit**. It drives real
cameras across their transports (BLE, Wi-Fi-AP, PTP/IP, USB-PTP, XLV-HTTP),
records the wire behavior, and emits JSONL evidence bundles that the
`camera-config-generate` binary consumes to produce ptpsim manifests.

ptpsim itself does not need this toolkit at runtime — it consumes the
manifests, not the probes. If you only want to use ptpsim against existing
manifests, you can ignore this directory.

## When to use this

- Adding a new camera body to the manifest set — drive a real device, capture
  its DeviceInfo/property surface, dump it as JSONL, feed it through the
  generator.
- Pinning a wire fact for an existing camera — a one-off probe (a paced
  PTP/IP probe, a settings sweep, a state-machine observer) that records
  what the camera actually does, so the manifest can cite it as evidence.
- Iterating on a new transport (e.g. how the camera negotiates BLE pairing,
  what the reference app prime sequence looks like, how XLV authenticates) — the
  scripts are templates you can branch.

## Setup (macOS)

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

macOS system dependencies are tracked in `Brewfile`:

- `blueutil`: scripted Bluetooth evidence and unpair/forget workflows.
- `ffmpeg`: Continuity Camera frame capture support.
- `tesseract`: OCR support for camera-screen classification.

Linux is partially supported (BLE + PTP/IP works; the screen-classification
helpers are macOS-only).

## Build helpers

Native helpers are scriptable and discoverable:

```sh
scripts/build/build-all.sh
scripts/build/build-camera-capture.sh
scripts/build/rebuild-camera-capture.sh
scripts/build/build-ble-identity-advertiser.sh
scripts/build/build-bluetooth-wrapper.sh
```

## Working principle: no guessing

The probe scripts treat camera state as something you measure, not something
you assume. Before each scripted action (registration, GPS write, AP launch,
PTP/IP control), the recommended flow is:

1. Gather fresh evidence (BLE advertisement, USB descriptor, route table,
   camera screen).
2. Assign exactly one connection-state label from
   [`CONNECTION_STATES.md`](CONNECTION_STATES.md).
3. Run only that state's workflow.
4. Stop and collect more evidence when host evidence and camera-screen
   evidence conflict.

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

## Supported probe paths

### BLE pairing + GPS

Drive a real Fuji body's BLE pairing flow and sync GPS location:

```sh
scripts/request_macos_bluetooth_permission.sh
scripts/live_ble_camera_test.sh \
  --device-name <your-host-name> \
  --skip-location \
  --write-registration-ack \
  --timeout 45
```

Pairing identity is wire-protocol data: legacy adverts carry
`0x02 + 4-byte key`, RED adverts carry `0x01 + 5-byte short serial`. The probe
scripts feed those bytes into the `PAIRING_KEY` write
(`ABA356EB-9633-4E60-B73F-F52516DBD671`). On RED bodies, the registration ACK
is a challenge-response on `CONNECTED_DEVICE_IDENTIFICATION_NUMBER`: read the
camera's 4 bytes, set the little-endian `0x20000000` bit, echo back.

### AP Wi-Fi handoff + PTP/IP

Once paired, the camera can launch its own access point and accept a PTP/IP
session on `:55740`. The probe scripts preserve the host's normal internet
route (default route stays on Ethernet/Wi-Fi-infra; only the camera endpoint
goes over the AP).

```sh
scripts/camera_ap_prepare.sh --device-name <your-host-name> --timeout 45
scripts/connect_camera_ap_wifi.sh \
  --credentials rce/sessions/laptop_ble_gps_<timestamp>/wifi_credentials.json
scripts/ptpip_probe.sh \
  --friendly-name <your-host-name> \
  --guid <initiator-guid> \
  --open-session \
  --app-sequence sdcard-folder-and-dates
```

`<initiator-guid>` is the bytes the camera expects in the PTP/IP
`InitCommandRequest`. The official reference app uses a hardcoded GUID extracted from

manufacturer tier.

### Wireless-tether (PCSS, 15740)

The standalone wireless-tether path uses a UDP knock + TCP callback before
PTP/IP. `connect_wireless_tether.py` drives the full handshake; the paced
probes (`paced_ptp_probe.py`, `paced_vendor_probe.py`) layer a slow PTP
op sequence on top so external observers can correlate per-op behavior.



`usb_settings_sweep.py` enumerates the USB-PTP property surface presented by


### XLV (HTTP API)

`xlv_char.py` and `xlv_capsweep.py` characterise the XLV HTTP API. These
require an XLV bearer token — see their module docstrings for how to obtain
one (camera firmware extraction or running the reference app pairing flow).

### Firmware update

The reference app firmware-update flow is modeled as: BLE writes a 92-byte
`FirmwareUpdateRequestInfo` (`b1307521-7ac5-4199-aaee-9d094781ce69`) +
`FUNCTION_LAUNCH=0500`, then a Wi-Fi AP join, then PTP/IP `OpenSession` +
`FunctionMode=0x0013` + `FirmwareTransferVersion=1` + vendor `0x9040`
ObjectInfo for `FUP_FILE.DAT` + chunked `0x9042` streaming in 1 MiB blocks.

```sh
scripts/firmware_update_prepare.sh \
  --dat <path-to-GXUP....DAT> --claim-version 2.41
scripts/ptpip_firmware_update.sh \
  --dat <path-to-GXUP....DAT>            # dry run
scripts/ptpip_firmware_update.sh \
  --dat <path-to-GXUP....DAT> --execute  # actual upload (destructive)
```

`--execute` refuses to proceed unless the camera endpoint route is on Wi-Fi.

### Camera-screen classification (optional, macOS)

Use the camera-screen classifier as camera-side context, not host-side
protocol proof.

```sh
scripts/detect_camera_lcd_box.sh --device-name <iphone-name> --warmup 5 --zoom 2
scripts/read_camera_screen_state.sh --device-name <iphone-name> --warmup 5 --zoom 2
```

If `read_camera_screen_state.sh` returns `camera_screen_state=unknown`, treat
that as a workflow error — fix the classifier/templates or
camera/iPhone alignment before proceeding.

## Tests

```sh
.venv/bin/python -m pip install -e '.[test]'
.venv/bin/python -m pytest -q
```

Hardware-dependent behavior stays behind mockable boundaries. The TUI module
is intentionally omitted from coverage; `screen_vision.py` is covered by
preserved fixture images.

## License

Dual-licensed under MIT OR Apache-2.0; see top-level
[`LICENSE-MIT`](../../LICENSE-MIT) and [`LICENSE-APACHE`](../../LICENSE-APACHE).
