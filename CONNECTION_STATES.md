# Fuji BLE Connection States

Directive: No guessing. This is an application that does not rely on superstition to determine state or next steps.

This document names observable host/camera connection states and defines deterministic workflows for each one. A state label is only valid when its required evidence is present. Camera screen text is useful context, but it is not enough by itself to select a workflow.

## Evidence Sources

Use the diagnostic script before choosing a workflow:

```sh
scripts/diagnose_macos_bluetooth_state.sh --scan --timeout 20
```

Primary fields:

- `macos_known_device_plist`: whether macOS has a persisted Bluetooth device entry in the Bluetooth plist.
- `macos_ioreg_device`: whether macOS currently exposes the camera in the Bluetooth I/O Registry.
- `system_profiler_device`: whether Bluetooth Settings/system_profiler lists the camera, and whether it is connected or not connected.
- `ble_advertisement_scan`: whether CoreBluetooth/Bleak can currently discover the camera advertisement.

Session artifacts to preserve:

- `rce/sessions/macos_bluetooth_state_*/`
- `rce/sessions/laptop_ble_gps_*/`
- `writes.jsonl`, `reads.jsonl`, `session.log`, `services.json`

## Evidence Scripts And Statefile

The evidence layer stores observations in:

```sh
rce/state/connection_state.json
```

Each focused script updates one evidence key and then re-evaluates the current state label. Run these scripts sequentially when they target the same statefile.

Host-side Bluetooth evidence:

```sh
scripts/evidence/macos_known_device_plist.sh
scripts/evidence/macos_ioreg_device.sh
scripts/evidence/system_profiler_device.sh
scripts/evidence/blueutil_paired_device.sh
scripts/evidence/blueutil_connected_device.sh
scripts/evidence/ble_advertisement_scan.sh --timeout 20
```

Camera/USB evidence:

```sh
scripts/evidence/camera_usb_probe.sh
scripts/evidence/camera_screen_capture.sh --image /path/to/screen.jpg

# Calibrate LCD geometry. Rerun only when the phone/camera framing changes.
scripts/detect_camera_lcd_box.sh --device-name iPhone --warmup 5 --zoom 2

# Classify the current camera LCD with the saved calibration.
scripts/read_camera_screen_state.sh --device-name iPhone --warmup 5 --zoom 2

# Re-run classifier rules against an existing capture without the camera.
scripts/reclassify_camera_screen_state.sh --capture rce/screen_captures/<timestamp>/capture.json

# Label unknown crops and then reclassify with --write.
scripts/identify_unknown_elements.sh --capture rce/screen_captures/<timestamp>/capture.json
scripts/reclassify_camera_screen_state.sh --capture rce/screen_captures/<timestamp>/capture.json --write
```

`camera_screen_capture.sh` records a screen image artifact only. It does not yet interpret the screen. Interpretation must come from OCR/vision in a later step or from a manual evidence script.
Camera-screen vision is camera-side context, not host-side protocol proof. Prefer deterministic host evidence when deciding BLE/Wi-Fi workflows, and collect more evidence if host evidence conflicts with screen OCR.
`detect_camera_lcd_box.sh` performs LCD box detection and writes the reusable calibration at `rce/state/camera_lcd_box.json`. Run it again only when the iPhone or camera LCD moves. Use `--image rce/screen_captures/<timestamp>/raw.png` to calibrate from a saved frame and `--no-save` to test detection without replacing the calibration. LCD detection has multiple fallback methods, including edge geometry, bright/dark LCD color, and known-glyph geometry based on stable Fuji UI anchors such as the exposure scale and AF touch glyph.
`read_camera_screen_state.sh` performs local capture, uses the saved LCD box calibration for screen normalization, then runs OCR/symbol detection and conservative state classification. It writes local-time artifacts under `rce/screen_captures/<timestamp>/`: lossless `raw.png`, normalized `screen.png`, and parsable `capture.json`. It only records actionable `camera_screen_state` evidence when confidence is high enough. Continuity Camera capture defaults to a two-second warmup; use `--warmup 5 --zoom 2` when the camera screen is dark or autofocus needs more time, then verify the capture artifact.
`reclassify_camera_screen_state.sh` reads an existing `capture.json`, loads its saved `screen.png`, and reruns classification without a live camera round trip. Use it while refining OCR/classification rules and for regression checks against preserved artifacts. Add `--write` to update the existing capture JSON after rule or label changes.
`identify_unknown_elements.sh` records labels in `rce/screen_captures/screen_element_labels.json` and copies accepted template crops to `rce/screen_captures/screen_element_templates/`, so future reclassification does not depend on mutable `unknown/` crops.
Known camera-screen state labels include `registration_mode` for the Fuji pairing/ready-to-pair screen, `device_not_found_continue_search`, `waiting_for_connected`, `connection_lost`, `app_function_not_found_retry`, `ready_to_take_photo`, and `ready_to_shoot_video`.

Manual camera-menu evidence, used only when host-side probes cannot determine camera-side state:

```sh
scripts/evidence/camera_screen_manual.sh --value registration_mode
scripts/evidence/camera_bluetooth_status_manual.sh --value ready_not_connected
scripts/evidence/camera_pairing_mode_manual.sh --value present
scripts/evidence/camera_registered_manual.sh --value absent
scripts/evidence/camera_registered_name_manual.sh --value empty
scripts/evidence/camera_gps_icon_manual.sh --value absent
scripts/evidence/macos_bluetooth_settings_manual.sh --value not_connected
```

To refresh camera-side LCD context as part of evaluation, run:

```sh
scripts/evaluate_connection_state.sh --refresh-screen --verbose
```

This captures the camera LCD, records high-confidence screen evidence into the statefile, then evaluates the state. Current programmatic icon evidence includes the dim trusted-Bluetooth-ready icon as `camera_bluetooth_status=ready_not_connected`. The GPS-set icon and bright active-Bluetooth icon are not yet labeled templates; until they are added, record them manually only when directly observed.

Session-artifact evidence:

```sh
scripts/evidence/session_pair_trigger_read.sh --session-dir rce/sessions/laptop_ble_gps_<timestamp>
scripts/evidence/session_registration_name_written.sh --session-dir rce/sessions/laptop_ble_gps_<timestamp>
scripts/evidence/session_registration_id_read.sh --session-dir rce/sessions/laptop_ble_gps_<timestamp>
scripts/evidence/session_registration_ack_written.sh --session-dir rce/sessions/laptop_ble_gps_<timestamp>
scripts/evidence/session_disconnect_after_ack.sh --session-dir rce/sessions/laptop_ble_gps_<timestamp>
scripts/evidence/session_gps_sync_ready.sh --session-dir rce/sessions/laptop_ble_gps_<timestamp>
scripts/evidence/session_gps_payload_written.sh --session-dir rce/sessions/laptop_ble_gps_<timestamp>
```

State evaluation:

```sh
scripts/evaluate_connection_state.sh
scripts/evaluate_connection_state.sh --verbose
scripts/reset_connection_state.sh --reason "starting a fresh pairing attempt"
```

Install macOS dependencies first:

```sh
scripts/install_macos_dependencies.sh
```

`blueutil` is required for scripted local Bluetooth unpair/forget workflows.
If Bluetooth Settings still shows a `My Devices` row that `blueutil --paired` does not expose, use the UI automation fallback:

```sh
scripts/delete_local_ble_pairing.sh --name "GFX100 II" --ui-automate
```

This requires Accessibility permission for the terminal app. `sudo` does not solve this class of stale Settings row because the actionable control is in the per-user System Settings UI.

Earlier experiments used this helper to test whether macOS `ComputerName` / `LocalHostName` controls the camera-side Bluetooth device-list name:

```sh
scripts/macos_pairing_identity.sh show
scripts/macos_pairing_identity.sh set --name mbp-7274 --admin-dialog
scripts/macos_pairing_identity.sh restore --admin-dialog
```

Changing this identity modifies macOS `ComputerName` and `LocalHostName`; restore it after any approved experiment. This path has already been tested and did not fix the blank GFX100 II registered-device name.

Evaluation notes:

- Host-side Bluetooth evidence is deterministic when it comes from plist, I/O Registry, system_profiler, or CoreBluetooth scan output.
- USB probing is deterministic only for USB visibility. It does not yet prove Fuji application registration.
- Attached-camera screen capture is deterministic for image collection. Camera-screen vision can classify known Fuji LCD states, but it remains camera-side context and not host-side protocol proof.
- Manual camera-menu evidence is allowed as the last resort and must be recorded explicitly with one of the manual scripts.
- Transient manual screen states such as `waiting_for_connected`, `app_function_not_found_retry`, and `pair_prompt_pending` expire after 120 seconds in the evaluator.
- Treat old camera-screen vision captures as stale for current workflow decisions unless they were just captured or intentionally replayed for classifier regression.
- `gps_sync_ready` expires after 300 seconds in the evaluator. It is evidence for an immediate session, not proof that the camera is currently connectable hours later.
- If host-side evidence cannot determine the camera UI state, use the camera-screen classifier when the iPhone/camera framing is calibrated; otherwise ask the user for exact screen text and record it manually.
- A state label is only actionable when all required evidence for that label is present in the statefile or an immediately preceding session artifact.
- When deleting host-side pairing and starting from scratch, reset the statefile before collecting fresh evidence. Otherwise old session evidence will correctly continue to describe the previous failed attempt.

## State Labels

### `host_unknown_camera_unknown_not_advertising`

Evidence:

- `macos_known_device_plist=absent`
- `macos_ioreg_device=absent`
- `system_profiler_device=absent`
- `ble_advertisement_scan=absent`

Meaning:

Neither macOS nor the app can see a camera entry. The camera is not currently discoverable by CoreBluetooth.

Workflow:

1. Do not run registration or GPS commands.
2. Put the camera into pairing/registration mode.
3. Run `scripts/diagnose_macos_bluetooth_state.sh --scan --timeout 20`.
4. Continue only after `ble_advertisement_scan=present`.

### `camera_registered_empty_name_host_not_connected`

Evidence:

- `host_registered_in_camera_menu=present`
- `camera_registered_name_display=empty`
- `macos_bluetooth_settings=not_connected`

Meaning:

The camera reports that pairing/registration completed and Bluetooth Settings records the camera, but the camera-side registration name is blank. This has now happened after app-level connected-device-name writes for `eric’s MacBook Pro (2)`, `eric's MacBook Pro (2)`, and `mbp-7274`, after macOS `ComputerName` / `LocalHostName` were set to `mbp-7274`, and after a parallel macOS Local Name advertiser exposed `mbp-7274`.

Firmware analysis indicates this displayed name comes from the persisted ThreadX `PairingInfo` slot. BLE pairing populates that slot from peer GAP Device Name / advertisement Local Name data; PTP-IP populates it from `InitiatorFriendlyName`. The later Fuji `CONNECTED_DEVICE_NAME_STRING` write is not the camera-list display source.

Workflow:

1. Preserve the successful session artifacts.
2. Treat the app-level `CONNECTED_DEVICE_NAME_STRING` write as insufficient to explain the displayed camera UI name.
3. Do not repeat the macOS `ComputerName` / `LocalHostName` experiment without new evidence.
4. Do not repeat the macOS Local Name-only advertiser experiment without new evidence; it improved timing but did not populate the display name.
5. For Linux/BlueZ, verify/set the adapter alias with `bluetoothctl show` and `bluetoothctl system-alias <name>` before pairing.
6. For macOS/CoreBluetooth, investigate whether a helper can expose a peer-readable GAP `0x2A00` Device Name during bonding.

### `camera_registered_host_unlisted_not_advertising`

Evidence:

- `host_registered_in_camera_menu=present`
- `session_registration_ack_written=present`
- `session_disconnect_after_ack=absent`
- `blueutil_paired_device=absent`
- `system_profiler_device=absent`
- `macos_ioreg_device=absent`
- `ble_advertisement_scan=absent`

Meaning:

The camera has confirmed Fuji application-level pairing/registration, but macOS does not expose the camera through the host-side Bluetooth evidence probes after disconnect. This is a valid completed-pairing state, not proof of an active BLE connection.

Workflow:

1. Do not delete host-side pairing just because `blueutil` or Bluetooth Settings evidence is absent.
2. Do not run GPS until a current BLE connection is available.
3. Put the camera into the mode that should allow reconnect/location sync.
4. Run `scripts/evidence/ble_advertisement_scan.sh --timeout 20`.
5. If the camera advertises, use the reconnect workflow.
6. If scan remains absent but the camera screen shows a dim Bluetooth icon on the ready-to-shoot screen, run `scripts/evidence/ble_direct_connect_probe.sh --address <last CoreBluetooth UUID>`.
7. If direct connect succeeds, use `camera_registered_host_direct_connectable`. If it fails, record the camera screen/menu state and ask the user for exact text before choosing a workflow.

### `camera_registered_host_direct_connectable`

Evidence:

- `host_registered_in_camera_menu=present`
- `ble_advertisement_scan=absent` or stale absent scan evidence.
- `ble_direct_connect_probe=present`

Meaning:

The camera may not be discoverable by name/service scan, but macOS/CoreBluetooth can still reconnect directly by the known CoreBluetooth identifier. Live evidence showed this when the camera was on the ready-to-shoot screen with a dim Bluetooth icon.

Workflow:

1. Use explicit-address BLE commands instead of scan-first commands.
2. For AP handoff, run `scripts/camera_ap_ptpip_probe_flow.sh --address <CoreBluetooth UUID> --device-name mbp-7274 ...`.
3. Preserve the direct-connect probe session because it proves the identifier is currently usable.

### `camera_registered_host_unlisted_advertising`

Evidence:

- `host_registered_in_camera_menu=present`
- `ble_advertisement_scan=present`
- `blueutil_paired_device=absent`
- `system_profiler_device=absent`
- `macos_ioreg_device=absent`

Meaning:

The camera says this host is registered and CoreBluetooth can currently see the camera, even though macOS host-side listing tools do not expose a persisted device entry.

Workflow:

1. Reconnect with a no-pairing live command.
2. Confirm notifications or a GPS-ready evidence key before writing GPS.
3. Preserve the session artifacts and camera-side observation.

### `host_remembers_camera_camera_unknown_not_advertising`

Evidence:

- `system_profiler_device=not_connected`
- `ble_advertisement_scan=absent`
- `macos_ioreg_device=absent`

Meaning:

macOS has a remembered Bluetooth Settings entry, but the app has no connectable CoreBluetooth device. This is not proof of a completed bond. This is the current observed state after the camera timed out.

Observed example:

```text
target_name:                       GFX100 II
macos_known_device_plist:          absent
macos_ioreg_device:                absent
system_profiler_device:            not_connected
ble_advertisement_scan:            absent
```

Suggested label for discussion:

`host_remembers_camera_camera_unknown_not_pairing`

Workflow:

1. Do not attempt direct registration. There is no app-visible BLE target.
2. Do not treat the `system_profiler` Bluetooth address as a CoreBluetooth reconnect handle; direct connect to `38:7C:76:74:73:21` failed with “Device ... was not found.”
3. If the camera offers “continue to search,” keep it searching and rerun the diagnostic scan.
4. If `ble_advertisement_scan` remains absent, restart camera pairing/registration mode.
5. If this state persists, remove the remembered macOS Bluetooth Settings entry and restart pairing from a clean state.

### `camera_advertising_host_unknown`

Evidence:

- `ble_advertisement_scan=present`
- No prior successful pair-only run in the current attempt.
- Camera is not known to be trusted by macOS.

Meaning:

The camera is discoverable and the app can attempt a first connection. Pairing may be required when protected GATT reads/writes occur.

Workflow:

1. Prefer `scripts/live_ble_camera_test.sh --device-name mbp-7274 --skip-location --write-registration-ack --timeout 45` so Fuji registration and ack happen in one BLE connection.
2. Use `scripts/live_ble_camera_test.sh --pair-only --timeout 30` only when isolating the OS-level pairing prompt as evidence.
3. If macOS shows a numeric comparison prompt, accept it on macOS and on the camera.
4. Run the diagnostic again.
5. Continue to GPS only after camera-side registration is confirmed.

### `host_pair_prompt_pending`

Evidence:

- `scripts/live_ble_camera_test.sh --pair-only` is running.
- macOS displays a numeric comparison prompt.
- The camera displays the corresponding pairing confirmation.

Meaning:

The host and camera are in the OS-level pairing handshake.

Workflow:

1. Verify the numbers match.
2. Accept on macOS.
3. Accept on the camera.
4. Wait for the script to exit.
5. If the command is a normal live registration run, let the same process continue into registration and ack write.
6. If the command is `--pair-only`, expect the connection to close after the protected read and treat it as evidence only.
7. If the camera reports canceled/lost, run the diagnostic before retrying.

### `host_pair_only_complete_camera_not_registered`

Evidence:

- Pair-only command exits cleanly.
- `session.log` contains a successful protected read, for example `pair-trigger read uuid=f557...`.
- Camera-side registration state has not yet been checked.

Meaning:

macOS could pair/connect enough to access protected GATT, but the Fuji application-level registration has not been persisted.

Workflow:

1. Check whether the camera lists this computer as a registered connection.
2. Record that with `scripts/evidence/camera_registered_manual.sh --value present|absent`.
3. If absent, keep the camera in registration mode and run `scripts/live_ble_camera_test.sh --device-name mbp-7274 --skip-location --write-registration-ack --timeout 45`.
4. Re-evaluate the state.

### `host_orphaned_gatt_access_camera_not_registered`

Evidence:

- Pair-only command exits cleanly.
- `session.log` contains a successful protected read, for example `pair-trigger read uuid=f557...`.
- `host_registered_in_camera_menu=absent`.

Meaning:

The host can access protected GATT, but the camera did not persist trust. This is an orphaned connection state. It is not a successful pairing and it is not GPS-ready.

This is the state that captures “the host already thinks it can talk to the camera” when `blueutil --paired`, `system_profiler`, and Bluetooth plists do not list a paired camera. The proof is not the macOS paired-device list; the proof is a successful protected GATT read from the camera.

Workflow:

1. Keep the camera in registration/pairing mode if possible.
2. Run `scripts/live_ble_camera_test.sh --device-name mbp-7274 --skip-location --write-registration-ack --timeout 45`.
3. Watch for camera-side persistence, not just host-side identity reads.
4. If the camera times out or says connection lost, run the diagnostic and classify the new state.

### `host_connected_registration_name_written_ack_skipped`

Evidence:

- `writes.jsonl` contains `CONNECTED_DEVICE_NAME_STRING`.
- `reads.jsonl` contains a registration id from `CONNECTED_DEVICE_IDENTIFICATION_NUMBER`.
- No ack write to `CONNECTED_DEVICE_IDENTIFICATION_NUMBER`.
- Identity reads may succeed.
- The camera does not persist the computer in its connection list.

Meaning:

This is a diagnostic-only success. The app can read camera data, but camera-side trust/registration is incomplete.

Workflow:

1. Do not call this a successful pairing.
2. Preserve the session artifacts.
3. Retry with `--write-registration-ack` while the camera is explicitly in registration mode.

### `host_connected_registration_ack_written_camera_disconnects`

Evidence:

- `writes.jsonl` contains `CONNECTED_DEVICE_NAME_STRING`.
- `reads.jsonl` contains a 4-byte registration id.
- `writes.jsonl` contains the registration id ORed with `0x20000000`.
- The camera disconnects immediately after the ack write.

Meaning:

The app reached the application-level ack step, but the camera rejected or closed the connection. This may be expected if the camera is not in the right mode, or it may mean the ack sequence is incomplete/wrong.

Workflow:

1. Record whether the camera was actively in registration mode at the moment of ack.
2. If not in registration mode, return to `camera_advertising_host_unknown` or `host_pair_only_complete_camera_not_registered` and retry cleanly.
3. If in registration mode, compare the write order and payloads against the known-good phone trace.
4. Do not proceed to GPS writes until camera-side registration is persisted.

### `host_trusted_camera_unknown_not_pairing`

Evidence:

- Host has a deterministic trusted/bonded signal, such as a future `blueutil --paired` entry or another confirmed bond source.
- Camera does not list the host as registered.
- `ble_advertisement_scan=absent` or the camera is not in registration mode.

Meaning:

The host may trust the camera, but the camera has not accepted or retained the host. This label should not be used for `system_profiler_device=not_connected` alone.

Workflow:

1. Do not assume recoverability from host trust alone.
2. Put the camera into registration mode.
3. Run the diagnostic until `ble_advertisement_scan=present`.
4. Run registration with ack enabled.
5. Confirm persistence on the camera before GPS.

### `host_and_camera_registered_not_connected`

Evidence:

- macOS has a deterministic trusted/bonded entry.
- The camera lists the computer in its connection list.
- `system_profiler_device=not_connected`.
- `ble_advertisement_scan` may be absent until the camera enters a connectable mode.

Meaning:

Both sides remember the relationship, but no active BLE session exists.

Workflow:

1. Put the camera into the mode that accepts location sync.
2. Run a diagnostic scan.
3. If discoverable, run GPS sync.
4. If not discoverable, use camera UI to initiate reconnect/search, then rerun diagnostics.

### `host_and_camera_registered_connected`

Evidence:

- macOS exposes the camera as connected or I/O Registry contains the camera.
- Camera UI shows the computer connected.
- App can read services or characteristics.

Meaning:

This is a valid connected state.

Workflow:

1. Read services and identity.
2. Enable notifications for location/date sync state.
3. Write location sync cycle.
4. Write GPS payloads only after the state remains connected.

### `gps_sync_ready`

Evidence:

- `host_and_camera_registered_connected` evidence is present.
- Registration is persisted on the camera.
- Location sync characteristics are present.
- No immediate disconnect occurs after registration/prepare steps.

Meaning:

The app can safely write GPS payloads.

Workflow:

1. Build the GPS payload from current location/time.
2. Write `LOCATION_AND_SPEED`.
3. Log every payload and response.
4. Continue periodic writes according to the selected sync interval.

### `gps_sync_ready_camera_name_empty`

Evidence:

- `gps_sync_ready=present`
- `host_registered_in_camera_menu=present`
- `camera_registered_name_display=empty`

Meaning:

GPS writes are allowed, but the camera UI displays the registered host with a blank name. This is not a connection blocker; it is a registration-display defect.

Workflow:

1. GPS write workflows may proceed if the user accepts the blank display name.
2. Do not keep changing only `CONNECTED_DEVICE_NAME_STRING`; firmware analysis says it is not the list-display source.
3. Do not rely on macOS Local Name advertising alone; the 2026-05-02 live test still displayed an empty camera-side name.
4. For Linux/BlueZ, set the adapter alias before pairing. For macOS, investigate peer-readable GAP Device Name presentation during bonding.

### `gps_payload_written_camera_icon_present`

Evidence:

- `session_gps_payload_written=present`
- `camera_gps_icon=present`

Meaning:

The host wrote at least one GPS payload and the camera UI shows the GPS indicator.

Workflow:

1. Treat location delivery as visibly active.
2. Continue periodic writes according to the selected sync interval.
3. Verify persisted coordinates by checking GPS EXIF on a new test photo when available.

### `gps_payload_written_camera_icon_absent`

Evidence:

- `session_gps_payload_written=present`
- `camera_gps_icon=absent`

Meaning:

The BLE write completed, but the camera UI did not show the expected GPS indicator. Do not infer that the camera stored coordinates until an independent artifact, such as GPS EXIF on a new photo, confirms it.

Workflow:

1. Record the session directory that contains the GPS write evidence.
2. Record the camera-screen GPS icon observation.
3. Take a new test photo and inspect its EXIF GPS tags.
4. If EXIF is empty, compare the Fuji app's missing setup writes before `LOCATION_AND_SPEED`.

### `gps_payload_written_camera_icon_absent_name_empty`

Evidence:

- `session_gps_payload_written=present`
- `camera_gps_icon=absent`
- `camera_registered_name_display=empty`

Meaning:

The camera accepted registration enough for BLE location writes, but two camera-side UI signals are wrong: the registered host name is blank and the GPS icon is absent.

Workflow:

1. Follow `gps_payload_written_camera_icon_absent`.
2. Keep the blank host-name bug logged separately unless evidence shows it blocks location persistence.

### `camera_ap_credentials_ready`

Evidence:

- `wifi_info_redacted.json` exists in the latest BLE session.
- `wifi_credentials.json` exists with mode `0600`.
- `wifi_info_redacted.json` reports `passphrase_present=true`.
- `ap_state_label=launched` after writing the `take` function-launch value (`0400`).
- If the characteristic is present, `writes.jsonl` contains `UTC_AND_TIMEZONE` before AP launch.
- If the characteristic is present, `writes.jsonl` contains `IMAGE_TRANSFER_SETTING_EX=01` before AP launch.

Meaning:

The BLE side of AP handoff succeeded. The laptop has the camera AP credentials without exposing the passphrase in normal logs, and the camera reports its AP as launched.

Workflow:

1. Run `scripts/connect_camera_ap_wifi.sh --credentials rce/sessions/laptop_ble_gps_<timestamp>/wifi_credentials.json`.
2. Do not print the passphrase in chat, shell traces, or log files.
3. Verify the laptop's default/internet route remains on Ethernet.
4. Verify the route to `192.168.0.1` uses Wi-Fi.

### `camera_ap_wifi_associated_ethernet_default`

Evidence:

- `rce/sessions/camera_ap_wifi_<timestamp>/summary.txt` exists.
- The summary reports `associated=present` in `connect.log`.
- `default_route` and `internet_route` match the pre-association Ethernet interface.
- `camera_route` is the Wi-Fi interface.
- If `networksetup -getairportnetwork` says "not associated" but IP/route/endpoint evidence is present, prefer IP/route/endpoint evidence.
- `scripts/evidence/camera_ap_wifi_session.sh --session-dir rce/sessions/camera_ap_wifi_<timestamp>` records `camera_ap_wifi_association=present`.

Meaning:

macOS is associated with the camera AP while the laptop's internet route remains on Ethernet. This is the correct state for PTP/IP testing from the laptop.

Workflow:

1. Preserve the Wi-Fi association session directory.
2. Record it into the statefile with `scripts/evidence/camera_ap_wifi_session.sh --session-dir rce/sessions/camera_ap_wifi_<timestamp>`.
3. Open PTP/IP against the camera endpoint, expected initially at `192.168.0.1`.
4. If PTP/IP fails, collect endpoint route evidence before changing Wi-Fi state.

### `camera_ap_waiting_for_ptpip_connection`

Evidence:

- Camera screen says `WAITING FOR CONNECTED`.
- AP credentials were prepared in the current attempt.
- Wi-Fi route evidence either already shows `192.168.0.1` routed over Wi-Fi, or Wi-Fi association is the next deterministic step.

Meaning:

The camera is in its app-search window and is waiting for the laptop/app to complete the Wi-Fi-side protocol. This is the critical window for Wi-Fi association and PTP/IP init.

Workflow:

1. Run `scripts/connect_camera_ap_wifi.sh --credentials rce/sessions/laptop_ble_gps_<timestamp>/wifi_credentials.json` if Wi-Fi is not already associated.
2. Run `scripts/ptpip_probe.sh --friendly-name mbp-7274 --tail-profile liveview` while the screen remains in this state.
3. Prefer `scripts/camera_ap_ptpip_probe_flow.sh --device-name mbp-7274 --hold-ble 0` for repeat attempts so AP launch, Wi-Fi association, and PTP/IP probe run in one window.
4. To replay an exact reference app init request, use `scripts/camera_ap_ptpip_probe_flow.sh --device-name mbp-7274 --ptpip-init-payload rce/reference/ptp_decoded/liveview_payload_00000061.bin`.
5. To attempt raw PTP OpenSession after a successful init ack, add `--ptpip-open-session`.
6. To probe the next observed reference app PTP property read, add `--ptpip-get-prop 0xd212`.
7. Do not use BLE hold-open as the default. `--hold-ble SEC` is diagnostic only because live evidence showed it could prevent macOS from finding the camera AP.

### `camera_ap_ptpip_tcp_connected_init_timeout`

Evidence:

- `scripts/ptpip_probe.sh` summary reports `route_check=passed`.
- `tcp_connect=present`.
- `init_sent=true`.
- `response_present=false` with `response_error=timeout`.

Meaning:

The laptop reached the camera's PTP/IP TCP listener, but the camera did not accept or answer the init request. This is not a Wi-Fi problem.

Workflow:

1. Preserve the PTP/IP probe session directory.
2. Compare the init request bytes against the reference app reference payloads in `rce/reference/ptp_decoded/`. Generated liveview/get tails must be 28 bytes and the generated packet should be 82 bytes.
3. Verify the BLE AP-prepare session includes `UTC_AND_TIMEZONE` and `IMAGE_TRANSFER_SETTING_EX=01` when those characteristics are present.
4. Do not infer the camera screen state from the timeout. If the next step depends on the Fuji UI, ask the user for exact screen text.
5. Test the next missing reference app prerequisite, likely the camera-registration-bound initiator GUID/name block, before changing Wi-Fi behavior.

### `camera_ap_ptpip_init_ack_present`

Evidence:

- `scripts/ptpip_probe.sh` summary reports `route_check=passed`.
- `tcp_connect=present`.
- `init_sent=true`.
- `response_present=true`.
- `response_header.packet_type=2`.

Meaning:

The camera accepted PTP/IP Init_Command_Request and returned InitCommandAck. Live evidence has shown this with exact captured reference app init payload `rce/reference/ptp_decoded/liveview_payload_00000061.bin`.

Workflow:

1. Preserve the PTP/IP probe session directory.
2. Send raw PTP OpenSession in the same socket with `scripts/ptpip_probe.sh --init-payload rce/reference/ptp_decoded/liveview_payload_00000061.bin --open-session`.
3. If OpenSession is only attempted after the camera window has changed, do not interpret failure as an OpenSession packet error. Repeat through `scripts/camera_ap_ptpip_probe_flow.sh --device-name mbp-7274 --ptpip-init-payload rce/reference/ptp_decoded/liveview_payload_00000061.bin --ptpip-open-session` while the user confirms the camera screen is in the expected state.

### `camera_ap_ptpip_open_session_ok`

Evidence:

- `response_header.packet_type=2`.
- `open_session_sent=true`.
- `open_session_response_present=true`.
- `open_session_response_header.code=8193` (`0x2001`, OK) or `8222` (`0x201e`, SessionAlreadyOpen).

Meaning:

The camera accepted PTP/IP init and the PTP session is usable. `SessionAlreadyOpen` can still be a usable state if subsequent PTP commands return data/OK.

Workflow:

1. Probe the next observed reference app command with `scripts/ptpip_probe.sh --init-payload rce/reference/ptp_decoded/liveview_payload_00000061.bin --open-session --get-prop 0xd212`.
2. Preserve `open_session_response.bin` and summary JSON.

### `camera_ap_ptpip_get_prop_d212_ok`

Evidence:

- OpenSession was OK or already open.
- `get_prop_sent=true`.
- `get_prop_data_header.code=4117` (`0x1015`, GetDevicePropValue data).
- `get_prop_response_header.code=8193` (`0x2001`, OK).

Meaning:

Normal PTP command/data/response exchange works over the camera AP socket.

Workflow:

1. Preserve the probe session directory.
2. Promote this scripted probe into a tested PTP/IP client module.
3. Continue implementing the next reference app PTP sequence from `rce/reference/ptp_decoded/`.

### `camera_ap_launched_app_function_not_found`

Evidence:

- Camera screen says `NOT FOUND` and `PLEASE CHECK THE APP AND SELECT THE FUNCTION AGAIN`.
- The visible choices are `OK: RETRY` and `BACK: CANCEL`.

Meaning:

The camera AP/search workflow launched, but the camera did not observe the expected app-side PTP/IP/FFIR follow-up before its search window failed. Do not treat this as a Wi-Fi association failure unless route/IP evidence is also absent.

Workflow:

1. Record the screen with `scripts/evidence/camera_screen_manual.sh --value app_function_not_found_retry --note "NOT FOUND / PLEASE CHECK THE APP AND SELECT THE FUNCTION AGAIN"`.
2. Probe `192.168.0.1:55740` with `scripts/ptpip_probe.sh --friendly-name mbp-7274 --tail-profile liveview`.
3. If the camera has already returned to the normal shooting screen, relaunch AP/search and run the PTP/IP probe during the retry window.
4. Prefer `scripts/camera_ap_ptpip_probe_flow.sh --device-name mbp-7274` so AP launch, Wi-Fi association, and PTP/IP probe run back-to-back.

### `camera_ap_wifi_associated_internet_route_changed`

Evidence:

- The Wi-Fi association script exits non-zero because `default_route` or `internet_route` changed to Wi-Fi.

Meaning:

The laptop may route internet/OpenAI traffic over the camera AP. This state is unsafe for continued agent work.

Workflow:

1. Stop camera AP testing.
2. Restore Ethernet priority or disconnect from the camera AP.
3. Re-run route evidence before retrying AP association.

## Required Workflow Rule

Before any command that writes registration or GPS data:

1. Run a state diagnostic or use an immediately preceding session artifact.
2. Assign exactly one state label from this document.
3. Execute only the workflow for that state.
4. If evidence conflicts, stop and collect more evidence.

No workflow may proceed because “it might work.” It proceeds because the required evidence for the next state is present.
