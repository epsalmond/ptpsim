# camera-probe — script & doc audit (2026-05-24)

Lightweight audit done as part of the fuji-remote → camera-probe promotion. Goal: identify scripts
superseded by the current Linux BLE→AP→PTP/IP + tether path (especially the steep macOS-BLE bring-up
era). **Everything is recoverable from `mbp:~/git/fuji-remote.archived`** (full 109-commit history), so
removals are low-risk. Nothing is deleted until the REMOVE list below is confirmed.

## A. Keep — active probe plans + core libs
`connect_wireless_tether.py` (PCSS transport), `pcss_discover.py` (knock), `pull_backup.py`,
`restore_backup.py`, `backup_sweep.py`, `build_dat_map.py`, `label_backup.py`, `settings_map.py`,
`issue_reset.py`, `set_poweroff.py`, `probe_desync_props.py`, `probe_partial_header.py`,
`probe_iso_liveview.py`, `movie_probe.py`, `tether_view.py`, `launch_ap_linux.py`,
`register_launch_linux.py`, `run_iso_probe_flow.sh`. (`rce/tools/fuji_ble_gps/` shared codec stays.)

## B. Keep — PTP/IP & AP shell helpers (consolidation candidates, not removals)
`ptpip_probe.sh`, `ptpip_export_object.sh`, `ptpip_firmware_update.sh`, `ptpip_compare_init.sh`,
`ptpip_inventory_init.sh`, `ptpip_decode_session_artifacts.sh`, `camera_ap_download_object.sh`,
`camera_ap_prepare.sh`, `camera_ap_ptpip_probe_flow.sh`, `firmware_update_prepare.sh`,
`connect_camera_ap_wifi.sh`. Several overlap; worth folding into `camera_probe` plans over time, but
they encode working flows — keep for now.

## C. RECOMMEND REMOVE — macOS BLE bring-up / permission one-shots (superseded by Linux path)
The BLE launch now runs on Linux (`launch_ap_linux.py` + `register_launch_linux.py`); these macOS-only
helpers are leftovers from the OSX bring-up learning curve and are not used by the Linux probe. The
macOS *app's* BLE lives in client application, not here.

- `diagnose_macos_bluetooth_state.sh`, `install_macos_dependencies.sh`
- `request_macos_bluetooth_permission.sh`, `request_macos_camera_permission.sh`
- `forget_bluetooth_device_via_system_settings.sh`, `delete_local_ble_pairing.sh`
- `macos_pairing_identity.sh`, `macos_ble_identity_advertiser.sh`
- `live_ble_camera_test.sh`, `live_ble_with_identity_advertiser.sh`
- `restore_wifi_internet.sh` (macOS Wi-Fi restore helper)
- dirs: `macos/ble_identity_advertiser/` (Swift), `bluetooth-wrapper/` (Obj-C BLE wrapper + `tools/`),
  `macos/camera_capture/` (Swift AVFoundation capture — superseded by PTP live-view/import)

## D. CONFIRM — camera-LCD vision / screen-state cluster (keep or drop?)
Reads/classifies the camera's on-screen state via image recognition. Predates host-side PTP control;
may be dead now that we read state over the wire, or still used for validation. Need a call:

- `detect_camera_lcd_box.sh`, `read_camera_screen_state.sh`, `reclassify_camera_screen_state.sh`
- `identify_unknown_elements.sh`, `capture_continuity_camera_frame.sh`
- `evaluate_connection_state.sh`, `reset_connection_state.sh`
- (+ the `rce/screen_captures/` template assets these depend on)

## Docs
- Refresh `README.md` + `AGENTS.md` to lead with camera-probe (transports / plans / risk / bundle).
- Stale-doc candidates: any BLE-OSX bring-up walkthroughs that no longer reflect the Linux path.
- Keep the wire-protocol docs (`TETHER_STATE_MACHINE.md`, `MODES_AND_CONTROL.md`,
  `CONNECTION_STATES.md`, `PROPERTY_CATALOG.md`) — these are evidence the bundle/manifest cite.
