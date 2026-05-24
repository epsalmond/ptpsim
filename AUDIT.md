# camera-probe — script & doc audit (2026-05-24)

Lightweight audit done as part of the fuji-remote → camera-probe promotion.

> **Conclusion (revised after scope clarification): KEEP EVERYTHING — nothing is removed.**
> camera-probe is the **end-user-shipped, cross-platform crowdsourcing probe**: end-users run it on
> *their own* machines (macOS today, Windows planned) to contribute observation bundles for cameras we
> don't own. That makes the macOS BLE bring-up, permission-grant, and diagnostic scripts **load-bearing
> platform support**, not learning-curve cruft — a non-technical end-user needs exactly those
> permission/diagnostic helpers. The audit below is therefore a categorized inventory + *consolidation*
> notes only; the original "prune superseded" framing was wrong.

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

## C. KEEP — macOS platform support (end-user-shipped; Windows planned next)
**Not superseded — this IS the product surface for end-users on Macs.** The Linux path
(`launch_ap_linux.py` + `register_launch_linux.py`) is the *operator/dev* path; the macOS scripts are
the *end-user* path. Permission-grant + diagnostic helpers are essential for non-technical contributors.
Keep all; a future `windows/` peer is expected.

- `diagnose_macos_bluetooth_state.sh`, `install_macos_dependencies.sh`
- `request_macos_bluetooth_permission.sh`, `request_macos_camera_permission.sh`
- `forget_bluetooth_device_via_system_settings.sh`, `delete_local_ble_pairing.sh`
- `macos_pairing_identity.sh`, `macos_ble_identity_advertiser.sh`
- `live_ble_camera_test.sh`, `live_ble_with_identity_advertiser.sh`
- `restore_wifi_internet.sh` (macOS Wi-Fi restore helper)
- dirs: `macos/ble_identity_advertiser/` (Swift), `bluetooth-wrapper/` (Obj-C BLE wrapper + `tools/`),
  `macos/camera_capture/` (Swift AVFoundation capture — superseded by PTP live-view/import)

## D. KEEP — camera-LCD vision / screen-state cluster
Reads/classifies the camera's on-screen state via image recognition. Useful for validating camera state
independently of the wire (and for end-user setups where wire-state is ambiguous). Keep:

- `detect_camera_lcd_box.sh`, `read_camera_screen_state.sh`, `reclassify_camera_screen_state.sh`
- `identify_unknown_elements.sh`, `capture_continuity_camera_frame.sh`
- `evaluate_connection_state.sh`, `reset_connection_state.sh`
- (+ the `rce/screen_captures/` template assets these depend on)

## Docs
- Refresh `README.md` + `AGENTS.md` to lead with camera-probe (transports / plans / risk / bundle).
- Stale-doc candidates: any BLE-OSX bring-up walkthroughs that no longer reflect the Linux path.
- Keep the wire-protocol docs (`TETHER_STATE_MACHINE.md`, `MODES_AND_CONTROL.md`,
  `CONNECTION_STATES.md`, `PROPERTY_CATALOG.md`) — these are evidence the bundle/manifest cite.
