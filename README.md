# Fuji BLE GPS

Laptop-side prototype for pairing/registering with a Fuji GFX100 II over BLE and writing the same GPS payload observed from FUJIFILM reference app.

The first backend targets macOS through Bleak/CoreBluetooth. Dry-run payload commands and tests do not require BLE hardware.

## Setup

```sh
python3 -m venv .venv
.venv/bin/python -m pip install -e '.[test,usb]'
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

macOS system dependencies are tracked in `Brewfile`. `blueutil` is required for scripted Bluetooth unpair/forget workflows; without it, macOS requires manual removal through Bluetooth Settings. `libusb` and the Python `usb` extra support fast USB/FF80 evidence polling.
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

### USB And FF80 Evidence

Use the fast USB poller while trying camera button/menu combinations that may
briefly expose Fuji service USB modes:

```sh
scripts/poll_fuji_usb_devices.sh
scripts/poll_fuji_usb_devices.sh --product-id 0xff80 --exit-on-match
scripts/poll_fuji_usb_devices.sh --product-id 0xff80 --timeout 3 --exit-on-match
scripts/poll_fuji_usb_devices.sh --product-id 0xff80 --interval 0
```

The poller uses libusb enumeration only. It does not claim the device or send
commands. Normal USB PTP mode for the GFX100 II has been observed as
`04cb:02fe`; active FF80 is expected as `04cb:ff80`.

Once the camera is confirmed in active FF80 mode, collect the current read-only
priority RAM ranges with:

```sh
scripts/ff80_ping.sh
scripts/ff80_dump_priority_ranges.sh
scripts/ff80_dump_priority_ranges.sh --only-risky-low
scripts/ff80_dump_priority_ranges.sh --next-targets
scripts/ff80_dump_priority_ranges.sh --gap-targets
scripts/ff80_dump_priority_ranges.sh --safe-fill-gaps
scripts/ff80_dump_priority_ranges.sh --low-watermark
scripts/ff80_dump_priority_ranges.sh --ram-size-probes
scripts/ff80_dump_priority_ranges.sh --ram-16gb-probes
scripts/ff80_dump_priority_ranges.sh --bootrom-recon-probes
scripts/ff80_dump_cfgdata.sh
```

Use `scripts/ff80_ping.sh` as the single-purpose active command-service check
after camera cold boots or suspected wedges. It writes
`rce/sessions/ff80_ping_<timestamp>/`, records passive USB enumeration as
advisory evidence, and treats timeout/stall text as failure.

The dump wrapper writes `rce/sessions/ff80_priority_dumps_<timestamp>/`, probes
each range before dumping, pings before and after each operation, and records
SHA256 plus actual byte counts. It skips low ThreadX runtime ranges by default
because a previous read from `0x00000000` wedged FF80 until camera reboot.
Use `--next-targets` for the combined follow-up set. It keeps the earlier
ThreadX slot/dispatch-table targets and appends backlog code/data/global ranges,
then executes them in ascending RAM address order:
`0x00044000`, `0x00057000`, `0x00059000`, `0x0005e000`, `0x000a9000`,
`0x000e1000`, `0x000ea000`, `0x000ed000`, `0x004c7000`, and `0x004e7000`.
Use `--gap-targets` for the next gap-fill pass around populated code/runtime
windows, widened globals, and message-pool continuation:
`0x00040000`, `0x0005c000`, `0x00060000`, `0x000a1000`, `0x000ad000`,
`0x000e0000`, `0x000e4000`, `0x004c0000`, `0x004e0000`, and `0x005c8000`.
Use `--safe-fill-gaps` to fill the remaining known uncovered low-map gaps above
the currently hazardous low window. It deliberately avoids
`0x00002000..0x00040000` and captures selected chunked safe ranges within
`0x00064000..0x00508000`.
Use `--low-watermark` only as a probe-only boundary finder. It reads 16 bytes
per address, pings before and after every read, skips `0x00000000`, and stops
on the first failed probe.
Use `--ram-size-probes` for sparse high-address evidence around the likely DDR
window. It only performs 16-byte reads and currently probes known high mapped
regions plus `0x3f000000`, `0x3ff00000`, and `0x3ffff000`. A successful probe
is addressability evidence; it is not a substitute for a full RAM-size register
or a complete contiguous dump.
Use `--ram-16gb-probes` for the board-level 16 GB hypothesis. Two 64 Gbit
LPDDR4 parts imply 128 Gbit total, or 16 GB nominal. The current FF80 RAM read
API only encodes a 32-bit address, so this mode probes visible 32-bit aperture
boundaries and cannot directly prove RAM above 4 GB. It skips known-wedging
`0xfffff000` by default; only include that address intentionally with
`--include-wedging-fffff000` after a cold boot is acceptable.
Use `--bootrom-recon-probes` after a cold boot to probe likely high-zone
bootrom addresses from `rce/reference/BOOTROM_RECON.md`. It reads 16 bytes,
pings, and only dumps a bounded chunk when the probe is neither all zero nor all
`ff`. The `0xffff0000` candidate is limited to `0xf000` bytes by default so it
does not read into known-wedging `0xfffff000`. The `0xfffc0000` candidate is
also skipped by default after a live 16-byte read there timed out and wedged
FF80 ping until cold boot. The later `0xfff00000` probe behaved the same way,
so it is also skipped by default.
The cfgdata wrapper is also read-only; it gates on active FF80 `ping`, records
passive USB polling as advisory evidence, dumps `cfgdata.bin`, and writes JSON
plus text analysis under `rce/sessions/ff80_cfgdata_<timestamp>/`.

Analyze one or more dump sessions offline with:

```sh
scripts/ff80_analyze_dumps.sh --session-dir rce/sessions/ff80_priority_dumps_<timestamp> --output-json rce/sessions/ff80_analysis_<timestamp>/analysis.json
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

`detect_camera_lcd_box.sh` writes a timestamped `lcd_box.json`, a normalized preview `screen.png`, and updates `rce/state/camera_lcd_box.json`. Use `--image rce/screen_captures/<timestamp>/raw.png` to calibrate from a saved raw frame, and `--no-save` to test detection without replacing the saved calibration. LCD detection currently tries edge geometry, bright LCD color, dark LCD color, blue LCD color for glare-heavy ready screens, and known Fuji glyph geometry from stable UI anchors such as the exposure scale and AF touch glyph.

`read_camera_screen_state.sh` reuses `rce/state/camera_lcd_box.json`; if that file is missing or stale, run `detect_camera_lcd_box.sh` first. It writes local-time artifacts under `rce/screen_captures/<timestamp>/`: lossless `raw.png`, normalized `screen.png`, and parsable `capture.json`. The command prints simple key/value output such as `camera_screen_state=registration_mode`, `confidence=0.82`, `capture=...`, and metadata like `iso=1600` when present. Current known states include `registration_mode` for the Fuji pairing/ready-to-pair screen, `device_not_found_continue_search`, `waiting_for_connected`, `connection_lost`, `app_function_not_found_retry`, `ready_to_take_photo`, `ready_to_shoot_video`, and `lcd_blank_or_sleep`.

`evaluate_connection_state.sh --refresh-screen --verbose` captures the LCD first and then re-evaluates the shared statefile. The screen classifier records `camera_screen_state`, `camera_bluetooth_status=ready_not_connected` when the dim trusted-Bluetooth icon is detected, and `camera_gps_icon=present` only after the GPS icon template exists and matches. The bright active-Bluetooth icon and GPS-set icon still need labeled templates before they can be trusted as programmatic evidence.

`reclassify_camera_screen_state.sh` skips camera capture and LCD warping. It reads an existing `capture.json`, loads its corresponding `screen.png`, and reruns OCR/symbol/classification rules for fast parser iteration and regression checks. Add `--write` only when you want to update the existing `capture.json`.

Unknown screen elements are labeled with:

```sh
scripts/identify_unknown_elements.sh --capture rce/screen_captures/<timestamp>/capture.json
```

The label catalog lives at `rce/screen_captures/screen_element_labels.json`, and accepted template crops are copied into `rce/screen_captures/screen_element_templates/` so reclassifying a capture does not overwrite template images. Re-run `scripts/reclassify_camera_screen_state.sh --capture ... --write` after adding labels.

Representative classifier fixtures live in `tests/fixtures/screen_vision/`. They are copied from local captures and include ready, pairing registration, AP retry/error, waiting-for-connected, and glare-heavy raw LCD frames so tests can exercise the classifier without the live iPhone camera.

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
scripts/evidence/camera_ap_wifi_session.sh --session-dir rce/sessions/camera_ap_wifi_<timestamp>
scripts/evaluate_connection_state.sh --verbose
```

The Wi-Fi script assumes this laptop's internet route is already on Ethernet. It records route evidence before and after association, refuses to continue if the default or internet route moves onto Wi-Fi, and requires the camera route to `192.168.0.1` to use the Wi-Fi interface. On the current macOS setup, `networksetup -getairportnetwork` can still report "not associated" while IP, route, and ping evidence prove the camera AP is reachable; treat route/IP evidence as authoritative. The passphrase is not printed or written to the Wi-Fi script logs.
The AP Wi-Fi evidence command parses `summary.txt` and records `camera_ap_wifi_association=present` only when association is present, the camera endpoint route is on Wi-Fi, and default/internet routes remain off Wi-Fi.

Probe the camera PTP/IP socket during the camera's search window:

```sh
scripts/ptpip_probe.sh --friendly-name mbp-7274 --guid f2e4538fada5485d87b27f0bd3d5ded0
scripts/evidence/ptpip_probe_session.sh --session-dir rce/sessions/ptpip_probe_<timestamp>
scripts/evaluate_connection_state.sh --verbose
```

This records route evidence and sends an reference app-shaped 82-byte PTP/IP `Init_Command_Request` to `192.168.0.1:55740`. It uses a fixed 26-byte UTF-16LE friendly-name field plus an observed 28-byte reference app tail. Use `--guid` for deterministic generated identity tests; omit it only when a fresh random initiator GUID is intentional. If the camera screen has already timed out to normal shooting, rerun the AP prepare step or press the camera's retry control before probing.
The script keeps macOS route checks in shell and delegates PTP/IP packet construction, socket exchange, binary artifacts, and `summary.json` generation to the tested Python module `rce.tools.fuji_ble_gps.ptpip`.
The PTP/IP evidence command parses `summary.json` and records `camera_ap_ptpip_probe` as the highest reached milestone, such as `tcp_connected_init_timeout`, `init_ack_present`, `open_session_ok`, or `get_prop_d212_ok`.

Compare a generated init packet against the accepted reference app capture without touching the camera:

```sh
scripts/ptpip_compare_init.sh --friendly-name mbp-7274 --guid f2e4538fada5485d87b27f0bd3d5ded0
```

The compare command decodes the Fuji 82-byte init shape field by field: packet header, initiator GUID, post-GUID bytes, fixed UTF-16LE friendly-name field, and the 28-byte reference app tail. It exits `0` only when every decoded field matches.

Inventory captured init identities before changing PTP/IP identity assumptions:

```sh
scripts/ptpip_inventory_init.sh rce/reference/ptp_decoded rce/sessions
```

The inventory command scans captured `.bin` payloads and decoded `.jsonl` traces for Fuji-shaped 82-byte `Init_Command_Request` records, then prints source, GUID, friendly name, tail profile, and packet length. Current local inventory shows all accepted reference app reference init records use GUID `f2e4538fada5485d87b27f0bd3d5ded0` with friendly name `Pixel-6-9405`; successful laptop-name PTP/IP probes also use that same GUID, while fresh generated GUID probes have timed out at init.

For live testing, prefer the combined flow so AP launch, Wi-Fi association, and PTP/IP probing happen inside one camera search window:

```sh
scripts/camera_ap_ptpip_probe_flow.sh --device-name mbp-7274 --ptpip-guid f2e4538fada5485d87b27f0bd3d5ded0
```

The combined flow reads the camera LCD at transition points by default: initial state, after AP launch, after Wi-Fi association, and after the PTP/IP probe. If classification returns `camera_screen_state=unknown`, the flow stops so the classifier or iPhone/LCD alignment can be fixed. Use `--no-screen-read` only for a targeted diagnostic where camera-side screen context is deliberately unavailable.
The combined flow does not keep the BLE connection open after AP launch by default. `--hold-ble SEC` is diagnostic only; live testing showed that keeping BLE open could prevent macOS from finding the camera AP.

When Ethernet is unavailable, use explicit temporary Wi-Fi takeover mode:

```sh
scripts/camera_ap_ptpip_probe_flow.sh --device-name mbp-7274 --ptpip-guid f2e4538fada5485d87b27f0bd3d5ded0 --temporary-wifi-internet
```

This allows Wi-Fi to leave the internet network for the camera AP, runs the local PTP/IP probe, and then reconnects the previous Wi-Fi SSID before the script returns. Pass `--restore-wifi-ssid SSID` if macOS cannot detect the current SSID before switching. The restore step delegates to `scripts/restore_wifi_internet.sh`, verifies internet ping reachability, and exits `9` if the SSID cannot be restored within the timeout. The lower-level `scripts/connect_camera_ap_wifi.sh --allow-wifi-internet-loss` is only the association step; prefer the combined flow when internet must be restored automatically after PTP.

To recover Wi-Fi explicitly after a failed or interrupted AP run:

```sh
scripts/restore_wifi_internet.sh --ssid EthicalDeviancy
```

If the BLE AP-launch step exits before Wi-Fi association, record BLE-side launch evidence from its laptop session:

```sh
scripts/evidence/camera_ap_ble_session.sh --session-dir rce/sessions/laptop_ble_gps_<timestamp>
```

This records `camera_ap_ble_launch=launched`, `not_launched`, `not_requested`, `unknown`, or `unavailable` from `session.log`. `not_launched` means the launch characteristic was written but AP state never reached `0180/launched` inside the polling window. The combined `scripts/camera_ap_ptpip_probe_flow.sh` now records this evidence automatically when the BLE prepare step exits or times out before Wi-Fi association.

Current PTP/IP status: TCP connect to `192.168.0.1:55740` succeeds when the camera route is on Wi-Fi. Replaying exact captured reference app init payload `rce/reference/ptp_decoded/liveview_payload_00000061.bin` produced a 68-byte `InitCommandAck`, and exact init plus `OpenSession` plus `GetDevicePropValue 0xD212` has succeeded. A generated 82-byte init using the accepted reference app GUID, laptop friendly name `mbp-7274`, and the liveview tail also succeeded through `GetDevicePropValue 0xD212` in session `rce/sessions/ptpip_probe_20260503T064901Z`. A generated init using reference app friendly name `Pixel-6-9405` with fresh deterministic GUID `00112233445566778899aabbccddeeff` timed out at init in `rce/sessions/ptpip_probe_20260503T081432Z`, so the next identity blocker is GUID/registration binding rather than the friendly-name field. Do not infer camera UI state from a timeout; ask the user for current camera-screen text or record screen evidence.

To isolate GUID behavior, keep BLE/app identity as `mbp-7274` while changing only the PTP/IP friendly name and GUID:

```sh
scripts/camera_ap_ptpip_probe_flow.sh --device-name mbp-7274 --ptpip-friendly-name Pixel-6-9405 --ptpip-guid 00112233445566778899aabbccddeeff --ptpip-open-session --ptpip-get-prop 0xd212
```

When scan evidence is absent but the camera screen shows a dim Bluetooth icon on the ready-to-shoot screen, probe the last known CoreBluetooth identifier directly:

```sh
scripts/evidence/ble_direct_connect_probe.sh --address 2B403BE3-8075-4865-D0F8-827BA4076BFF
```

Live evidence showed direct reconnect can succeed even when name-based scan evidence is absent. The combined AP/PTP flow can use the same explicit address:

```sh
scripts/camera_ap_ptpip_probe_flow.sh --address 2B403BE3-8075-4865-D0F8-827BA4076BFF --device-name mbp-7274 --ptpip-init-payload rce/reference/ptp_decoded/liveview_payload_00000061.bin --ptpip-open-session
```

The Python PTP/IP generator validates that `liveview`, `get`, and `zeros` tail profiles are 28 bytes, producing an 82-byte packet like the reference app captures. The combined flow can replay exact captured init packets:

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

The next observed SD-card browse/import bootstrap can be run as a named sequence:

```sh
scripts/ptpip_probe.sh --friendly-name mbp-7274 --guid f2e4538fada5485d87b27f0bd3d5ded0 --app-sequence sdcard-browse-bootstrap
scripts/camera_ap_ptpip_probe_flow.sh --address 2B403BE3-8075-4865-D0F8-827BA4076BFF --device-name mbp-7274 --ptpip-guid f2e4538fada5485d87b27f0bd3d5ded0 --ptpip-app-sequence sdcard-browse-bootstrap
```

`sdcard-browse-bootstrap` follows the observed reference app sequence: `GetDevicePropValue 0xd212`, `SetDevicePropValue 0xdf01=1400`, `GetDevicePropValue 0xdf28`, `SetDevicePropValue 0xdf28=03000000`, `SetDevicePropValue 0xd226=0000`, `SetDevicePropValue 0xd227=0000`, and `GetDevicePropValue 0xd244`.
`sdcard-current-object-info` extends that sequence with `FujiVendor_9054` parameter `0x10000001`, which reference app used to read current-object metadata such as `20260425T095812,DSCF8101.MOV`. `sdcard-current-object-thumbnail` extends it again with `FujiVendor_9055` parameter `0x10000001`, which reference app used to read the current object's JPEG thumbnail.
`sdcard-folder-and-dates` continues with `FujiVendor_9050` and `FujiVendor_9053`; the reference capture sends `9053` with parameters `0x00000000,0x00007530` and labels the result as the capture-date list. `sdcard-object-handles` continues again with standard `GetDevicePropValue 0xd620` and `GetDevicePropValue 0xd621`, which the reference labels as object count and visible object handles.
Live laptop evidence reached `sdcard-folder-and-dates`, but the earlier parameterless `FujiVendor_9053` probe returned a large fixed-size payload whose only decoded UTF-16 text was `140_FUJI`, then the earlier vendor-opcode `D620` probe timed out. Treat this as a corrected sequence-model bug, not as a Wi-Fi failure.
Once a handle is known, standard PTP metadata, full-object, and thumbnail probes can be run with `--get-object-info <handle>`, `--get-object <handle>`, and `--get-thumb <handle>`, for example:

```sh
scripts/ptpip_probe.sh --friendly-name mbp-7274 --guid f2e4538fada5485d87b27f0bd3d5ded0 --app-sequence sdcard-object-handles --get-object-info 0x0c --get-object 0x0c
scripts/ptpip_export_object.sh --session-dir rce/sessions/ptpip_probe_<timestamp> --output-dir rce/downloads/manual_export
```

The export command validates JPEG SOI/EOI bytes before writing the file and writes a JSON manifest next to the exported image. A live single-handle download workflow is also available:

```sh
scripts/camera_ap_download_object.sh --address 2B403BE3-8075-4865-D0F8-827BA4076BFF --device-name mbp-7274 --handle 0x0000000c
```

This wraps the combined AP/PTP flow, runs `sdcard-object-handles`, requests ObjectInfo and GetObject for the requested handle, then exports the complete JPEG into `rce/downloads/camera_ap_download_<timestamp>/`. If the AP/PTP flow fails, export is skipped and the preserved session artifacts remain the evidence source.
When Continuity Camera focus is unstable, use `--screen-warmup 5` on the combined flow; the default remains two seconds.

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

The project can read camera AP credentials over BLE, write the observed UTC/timezone and image-transfer setup values, launch the camera AP with the `take` launch value, associate macOS Wi-Fi with the AP while preserving the Ethernet internet route, run PTP/IP init/session/property probes, list observed object handles, and download/export a known object handle as a validated JPEG. The accepted reference app GUID currently works with the laptop friendly name; the remaining PTP/IP identity work is to obtain or register a laptop-owned accepted initiator GUID.
