# Fuji Remote Backlog

This backlog is for the camera-control remote application. FF80 service-mode research has moved to `../fuji-ff80`.

## P0: Complete Deterministic Connection Workflows

Build workflows for every state in `CONNECTION_STATES.md`.

Needed:

- map each state label to allowed next actions
- stop on conflicting host/screen evidence
- keep transient screen evidence expiry enforced
- make recovery from orphaned host-side pairing explicit
- preserve session artifacts as evidence after every live attempt

## P0: Resolve PTP/IP Initiator Identity

Known:

- Accepted reference app GUID: `f2e4538fada5485d87b27f0bd3d5ded0`
- Laptop friendly name `mbp-7274` succeeded when paired with that GUID.
- A fresh deterministic GUID timed out at init.

Question:

- How do we obtain or register a laptop-owned initiator GUID that the camera accepts?

Next steps:

- compare accepted and rejected init sessions field-by-field
- inspect registration bindings between BLE pairing and PTP/IP identity
- preserve all `summary.json` and packet artifacts

## P0: Promote PTP/IP Actions Into App Commands

Current probes support init, open session, `GetDevicePropValue 0xD212`, and reference app-shaped SD-card browse bootstrap sequences.

Needed:

- expose tested commands through `rce.tools.fuji_ble_gps.ptpip`
- keep route checks in shell wrappers
- add parser tests for every response shape
- make download/export flows restartable and idempotent

## P0: Build The TUI Skeleton

Target experience:

- current state label
- evidence list with timestamps
- next allowed actions
- live logs
- GPS write status
- AP/PTP session status
- screen-classifier status

Keep hardware access behind tested command modules so the TUI stays thin.

## P1: Strengthen Camera-Screen Classification

Known labels:

- `registration_mode`
- `device_not_found_continue_search`
- `waiting_for_connected`
- `connection_lost`
- `app_function_not_found_retry`
- `ready_to_take_photo`
- `ready_to_shoot_video`
- `camera_bluetooth_status=ready_not_connected`

Needed labels:

- GPS-set icon
- active Bluetooth icon
- additional AP/PTP transition screens

Rule: if live classification returns `unknown`, stop the workflow and fix classifier/templates or camera/iPhone alignment.

## P1: Improve Pairing Recovery

Known issue: macOS can retain a Bluetooth Settings `My Devices` row even when `blueutil --paired` is empty.

Needed:

- keep `scripts/delete_local_ble_pairing.sh --ui-automate` reliable
- document Accessibility permission failure modes
- add evidence for Settings-only rows where possible
- avoid pair-only flows unless isolating OS numeric-pairing behavior

## P1: Revisit Camera-Side Blank Host Name

Status:

- app-level `CONNECTED_DEVICE_NAME_STRING` does not populate the camera Bluetooth device-list name
- macOS `ComputerName` / `LocalHostName` changes did not help
- CoreBluetooth Local Name advertising did not help because public CoreBluetooth rejects GAP `0x1800` / Device Name `0x2A00`

Next useful work:

- verify Linux/BlueZ adapter alias behavior
- investigate external adapter or alternate BLE stack only if it advances the app
- keep this non-blocking while GPS/control work succeeds

## P1: Add Rust Library Plan

Distill stable protocol logic into a Rust library once the Python flows settle:

- payload encoding/decoding
- UUID/action catalog
- state machine
- PTP/IP packet construction/parsing
- platform transport traits

Keep live hardware workflows in Python until the protocol edges are stable enough to port without churn.

## P2: Cross-Platform Backends

Targets:

- macOS CoreBluetooth
- Windows BLE APIs
- Linux BlueZ
- Android BLE
- iOS CoreBluetooth

Keep platform-specific behavior isolated behind transport interfaces.

## P2: Packaging And User-Facing UX

Needed:

- single command to install dependencies/check permissions
- scripted diagnostics bundle
- safe logging with redacted Wi-Fi passphrases
- TUI packaging
- later native app packaging
