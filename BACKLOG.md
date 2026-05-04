# Backlog

This backlog tracks known work, open defects, and investigation threads. Commit
messages should stay short and imperative; detailed context belongs here, in
`README.md`, `AGENTS.md`, or targeted design docs.

## Bugs

### BUG-001: Camera registration completes but camera-side host name is blank

Status: understood; macOS workaround still open

Observed: 2026-05-02

Summary:

The Fuji GFX100 II reports `PAIRING HAS COMPLETED` and BLE registration reaches `gps_sync_ready`, but the camera's registered-device list displays an empty host name.

Evidence:

- `rce/sessions/laptop_ble_gps_20260502T071758Z`: app-level name `eric’s MacBook Pro (2)`; camera name displayed empty.
- `rce/sessions/laptop_ble_gps_20260502T073029Z`: fresh registration with ASCII app-level name `eric's MacBook Pro (2)`; camera name displayed empty.
- `rce/sessions/laptop_ble_gps_20260502T075852Z`: fresh registration with reference app-shaped app-level name `mbp-7274`; camera name displayed empty.
- `rce/sessions/laptop_ble_gps_20260502T081250Z`: fresh registration with macOS `ComputerName` and `LocalHostName` temporarily set to `mbp-7274`, and app-level name `mbp-7274`; camera name displayed empty.
- `rce/sessions/laptop_ble_gps_20260502T100213Z`: fresh registration using connect-on-detection plus macOS Local Name advertiser `mbp-7274`; pairing was fast and registration succeeded, but camera name displayed empty.

Current facts:

- Pairing/registration completes.
- `CONNECTED_DEVICE_NAME_STRING` writes succeed.
- Registration id reads and ack writes succeed.
- Location/date sync reaches `0100`.
- Camera UI display name remains empty even when the OS-level macOS pairing identity and Fuji app-level name match.
- Static GFX100 II firmware inspection indicates the Bluetooth device-list UI reads the displayed name from the persisted ThreadX `PairingInfo` slot. It does not synthesize a name from the BD_ADDR at render time.
- The `PairingInfo` display-name slot is populated by bond-time peer GAP/local-name data for BLE pairing, or by PTP-IP `InitiatorFriendlyName` for Wi-Fi tethering. It is not populated by our later `CONNECTED_DEVICE_NAME_STRING` write.
- macOS public CoreBluetooth allows Local Name advertising from the helper, but rejects publishing reserved GAP service `0x1800` / Device Name `0x2A00`.

Falsified hypotheses:

- The blank name is caused only by a Unicode smart apostrophe.
- The blank name is caused only by spaces/punctuation in the app-level name.
- The blank name is fixed by an reference app-shaped `host-####` app-level name.
- The camera UI simply displays macOS `ComputerName` / `LocalHostName`.
- The blank name is fixed by a parallel macOS Local Name advertisement alone.

Implications:

- Treat this as a display-name bug only; it is not currently blocking GPS writes when state is `gps_sync_ready`.
- On Linux/BlueZ, set and verify the adapter alias before pairing; firmware analysis indicates this is the source for the peer GAP Device Name / advertisement Local Name path.
- On macOS/CoreBluetooth, changing `ComputerName` and `LocalHostName` was not enough, and advertising Local Name alone was not enough. A real fix likely needs a peer-readable GAP Device Name during bonding, if macOS permits that through private APIs or an external BLE adapter.

Next investigation:

- Compare Android bond metadata and BLE pairing identity against macOS CoreBluetooth behavior.
- Determine whether macOS can expose a peer-readable GAP `0x2A00` Device Name while our client performs the central-side camera registration flow.
- For Linux support, add a pre-pairing alias check around `bluetoothctl show` / `bluetoothctl system-alias`.

### BUG-002: PTP/IP init GUID appears registration-bound

Status: in progress

Observed: 2026-05-02

Summary:

The camera accepts the exact captured reference app PTP/IP init payload at
`rce/reference/ptp_decoded/liveview_payload_00000061.bin`, returns
`InitCommandAck`, accepts `OpenSession`, and responds to
`GetDevicePropValue 0xD212`. A generated packet using the accepted reference app GUID,
the laptop friendly name `mbp-7274`, and the liveview tail also succeeds through
`GetDevicePropValue 0xD212`. The default generated packet with a fresh random
GUID timed out. A generated packet using the accepted reference app friendly name
`Pixel-6-9405` with fresh deterministic GUID `00112233445566778899aabbccddeeff`
also timed out at init.

Current facts:

- AP launch and Wi-Fi association work.
- Ethernet remains the internet route while the camera endpoint routes over Wi-Fi.
- TCP to `192.168.0.1:55740` succeeds when the camera is in the AP/PTP window.
- Replaying the exact captured reference app init succeeds.
- Generated init with accepted reference app GUID plus laptop friendly name succeeds.
- Generated init with accepted reference app GUID plus laptop friendly name succeeds through the corrected SD-card folder/date and object-handle sequences.
- Generated init with accepted reference app GUID plus laptop friendly name succeeds through `GetObjectInfo` and `GetThumb` for a handle returned by `GetDevicePropValue 0xd621`.
- Generated init with a fresh random GUID timed out.
- Generated init with accepted reference app friendly name plus fresh deterministic GUID timed out.
- `scripts/ptpip_inventory_init.sh rce/reference/ptp_decoded rce/sessions` shows accepted reference app reference records use GUID `f2e4538fada5485d87b27f0bd3d5ded0`; successful laptop-name probes also use that GUID.
- The friendly-name field is not the blocker for this camera state; GUID or registration-bound identity is the remaining gate.

Next investigation:

- Determine where the accepted GUID comes from and whether it is persisted in camera registration state.
- Find or create the laptop's own accepted initiator GUID instead of replaying the captured phone GUID.
- Use `scripts/ptpip_inventory_init.sh` on any newly copied decoded traces before changing identity hypotheses.
- Search reference material, app persistent storage, and native code for where reference app stores or derives the initiator GUID.
- Keep route/AP behavior fixed while investigating packet identity.

### BUG-003: Camera-screen classifier does not yet recognize GPS-set or active-Bluetooth icons

Status: in progress

Summary:

The screen classifier can identify the ready screen, the dim trusted-Bluetooth
ready icon, pairing registration, and pairing timeout. It does not yet have
trusted templates for the GPS-set icon or the bright active-Bluetooth icon.

Next investigation:

- Capture LCD frames showing the GPS-set icon.
- Capture LCD frames showing the bright active-Bluetooth icon.
- Label unknown crops with `scripts/identify_unknown_elements.sh`.
- Reclassify saved captures with `scripts/reclassify_camera_screen_state.sh --write`.
- Add tests for the new metadata mapping.

## Protocol And Camera Work

### PROTO-001: Promote successful PTP/IP probe path into a Python client

Status: in progress

Summary:

The current PTP/IP path is mostly shell-script probing. Move the known-good
init/OpenSession/GetDevicePropValue behavior into a tested Python module.

Acceptance criteria:

- Python client can send init, open session, and issue property reads.
- Packet encoder/decoder tests cover accepted captured payloads and generated payloads.
- `scripts/ptpip_probe.sh` calls the Python client for packet/socket work.
- Verbose logs include packet type, transaction id, operation code, response code, byte lengths, and artifact paths.

Remaining work:

- Expose the Python PTP/IP client as a TUI action.
- Decode the full object-transfer operation and decide whether Fuji requires standard `GetObject`, `GetPartialObject`, or an reference app vendor transfer function.
- Build a media-transfer workflow using handles returned by `GetDevicePropValue 0xd621`; latest successful object-handle run returned `0x0000000c`, `0x0000000a`, `0x00000008`, `0x00000006`, `0x00000005`, `0x00000004`, `0x00000003`, `0x00000002`.
- Use the init comparator output to choose the next live generated-identity candidates.

### PROTO-002: Implement the next observed reference app PTP sequence

Status: open

Summary:

Reference captures under `rce/reference/ptp_decoded/` include liveview and
property/action traffic. Continue implementing the observed sequence after
successful init/open-session/property-read.

Next candidates:

- Live-test or implement a dry-run decoder for standard PTP `GetObject` / `GetPartialObject` candidates, using decoded ObjectInfo size `167936` for `_DSF8109.JPG` as the first target.
- Decode reference app action enumeration usage from `rce/reference/APP_ACTION_ENUMERATION.md`.
- Add scripts for one command at a time, each with captured packet evidence.

### PROTO-003: Determine registration-bound initiator identity rules

Status: open

Summary:

The camera may bind accepted PTP/IP initiator identity to a previously trusted
mobile-app registration. We need to know which fields are checked and how a
laptop identity can become accepted without replaying a phone identity.

Questions:

- Which GUID bytes are stable across reference app sessions?
- Which friendly-name bytes are displayed or persisted by the camera?
- Does PTP/IP identity depend on BLE registration id, camera-side slot, or Wi-Fi AP launch context?

Useful command:

```sh
scripts/ptpip_inventory_init.sh rce/reference/ptp_decoded rce/sessions
```

### PROTO-004: Preserve and harden AP Wi-Fi workflow

Status: in progress

Current facts:

- BLE AP launch works.
- AP launch can also fail while BLE direct connect and credential reads succeed; session `rce/sessions/laptop_ble_gps_20260503T201641Z` wrote function launch `0400` but AP state stayed `0080/not_launched`.
- Camera AP credentials are read over BLE.
- macOS can associate to the camera AP while Ethernet remains the internet route.
- When Ethernet is unavailable, the combined AP/PTP flow supports explicit temporary Wi-Fi takeover and restores the previous Wi-Fi SSID before exit.
- `networksetup -getairportnetwork` can be misleading; route/IP/ping evidence is more reliable.
- `scripts/evidence/camera_ap_ble_session.sh` records BLE AP launch evidence from `session.log`.

Next work:

- Make route preservation failures more actionable.
- Record AP state and camera endpoint reachability in a single structured artifact.
- Determine whether `0080/not_launched` after function launch means wrong camera screen/function state, stale connected-device state, a missed prerequisite write, or a transient camera refusal.
- Continue promoting `camera_ap_wifi_association` evidence into the TUI workflow.
- Keep all AP scripts deterministic and evidence-driven.

## BLE And Pairing

### BLE-001: Add Linux/BlueZ adapter alias preflight

Status: open

Summary:

Firmware analysis suggests the camera reads peer GAP Device Name or advertisement
Local Name during bond creation. Linux/BlueZ can set adapter alias before pairing.

Acceptance criteria:

- Linux preflight checks `bluetoothctl show`.
- Script can set `bluetoothctl system-alias <name>` when approved.
- Pairing flow records alias before and after registration.
- Tests cover parser behavior for BlueZ alias output.

### BLE-002: Investigate macOS peer-readable GAP Device Name options

Status: open

Summary:

Public CoreBluetooth can advertise Local Name but rejects publishing reserved
GAP service `0x1800` / Device Name `0x2A00`. That likely explains the blank
camera-side host name on macOS.

Possible directions:

- Research private macOS Bluetooth APIs.
- Test an external BLE adapter controlled through BlueZ or another stack.
- Compare Android/reference app bond metadata against macOS bond metadata.

### BLE-003: Improve host-side forget/delete automation

Status: open

Summary:

`blueutil` is a project requirement for scripted macOS Bluetooth operations, but
macOS still exposes some paired-device state only through privileged/private UI
paths.

Next work:

- Keep `blueutil` install/check docs current.
- Identify which delete paths are scriptable and which require System Settings.
- Avoid claiming a device is forgotten unless host evidence proves it.

## State Machine And Evidence

### STATE-001: Build workflows for every connection state

Status: open

Summary:

`CONNECTION_STATES.md` names many states. Each state should eventually have:

- Required evidence.
- Allowed next actions.
- Recovery actions.
- Scripts that collect missing evidence.
- Clear "do not proceed" conditions.

### STATE-002: Add statefile locking or merge semantics

Status: implemented; monitor

Summary:

Evidence writers now take an advisory lock on `rce/state/connection_state.json.lock`
around load/modify/save. This was added after parallel evidence collection lost
a fresh `camera_ap_wifi_association` update while another collector wrote a
newer `camera_ap_ptpip_probe` record.

Remaining work:

- Prefer sequential evidence collection in live workflows unless there is a
  clear need for parallelism.
- Revisit the lock strategy for Windows support, where `fcntl` is unavailable.

### STATE-003: Use screen classification when camera-side state is unknown

Status: in progress

Current facts:

- `scripts/evaluate_connection_state.sh --refresh-screen --verbose` can capture LCD state before evaluation.
- Screen evidence is camera-side context, not host-side BLE/Wi-Fi proof.
- If screen evidence conflicts with host evidence, collect more evidence.

Next work:

- Wire `--refresh-screen` into live workflows only when camera-side context is required.
- Keep manual camera-screen prompts as fallback, not default.
- Add tests for each script that depends on screen-state evidence.

## Screen Vision

### SCREEN-001: Label missing camera icons

Status: open

Missing labels:

- GPS-set icon.
- Bright active-Bluetooth icon.

Current labels:

- Battery percentage indicator.
- External power indicator.
- Autofocus area.
- Roll indicator segment.
- Autofocus touch indicator.
- Dim trusted-Bluetooth ready/not-connected icon.

### SCREEN-002: Detect stale LCD calibration

Status: open

Summary:

`rce/state/camera_lcd_box.json` is valid only while the phone/camera framing is
unchanged. Add a deterministic stale-calibration signal.

Possible evidence:

- Raw capture dimensions mismatch.
- Warp confidence drops.
- Known template anchors move or vanish.
- User explicitly records framing change.

### SCREEN-003: Add screen-capture regression fixtures

Status: done

Summary:

Timestamped live captures are ignored by git. Add a small curated fixture set
for parser/classifier regression tests without committing bulky live artifacts.

Result:

- Added representative fixtures under `tests/fixtures/screen_vision/`.
- Covered ready, registration, waiting-for-connected, app-not-found retry,
  device-not-found retry, and a glare-heavy raw LCD capture.

## Product And TUI

### TUI-001: Build the operator TUI

Status: open

Goal:

A TUI that can pair with the camera, diagnose state, run allowed workflows, write
GPS updates, and show verbose logs/artifacts without making the user guess.

Core views:

- Current state label and freshness.
- Evidence table.
- Next allowed actions.
- Live log stream.
- GPS write status.
- Camera-screen capture/classification status.
- AP/PTP/IP route and session status.

### TUI-002: Keep scripts as stable automation API

Status: in progress

Summary:

The TUI should call the same deterministic scripts/modules used from the
terminal. Terminal workflows remain first-class so live testing can be
auto-approved and reproduced.

## Cross Platform

### PLATFORM-001: Define backend boundaries for macOS, Windows, Linux, Android, and iOS

Status: open

Targets:

- macOS first, using Bleak/CoreBluetooth and Swift helpers where needed.
- Linux with BlueZ and explicit adapter alias control.
- Windows BLE backend and pairing UX.
- Android BLE/GPS/AP behavior, likely closest to reference app traces.
- iOS feasibility assessment, especially BLE central behavior and background/location constraints.

### PLATFORM-002: Decide whether to keep Python or move the long-running core to Rust

Status: open

Summary:

Python is working well for protocol discovery and tests. Rust may be useful for
cross-platform packaging, long-running TUI behavior, stronger typing, and
shipping fewer runtime dependencies.

Decision should wait until the BLE/PTP protocol surface is better understood.

## Testing And Quality

### TEST-001: Maintain full Python coverage

Status: ongoing

Current target:

- `pytest` should remain at 100% coverage for trusted package code.
- New modules need focused unit tests.
- Hardware scripts need parser/unit coverage plus live artifacts when run against the camera.

### TEST-002: Add integration smoke commands for live workflows

Status: open

Summary:

Create non-destructive smoke checks for:

- BLE scan/direct connect.
- State evaluation.
- Screen classification from saved capture.
- AP route preservation.
- PTP/IP packet encoding.

## Dependencies And Setup

### DEPS-001: Keep project requirements explicit

Status: ongoing

Known requirements:

- Python virtualenv with project test extras.
- `blueutil` for macOS Bluetooth scripting.
- macOS camera and Bluetooth permissions for the relevant helper apps/terminal.
- Tesseract/OpenCV stack for screen vision.
- Xcode command line tools for Swift/Objective-C helpers.

### DEPS-002: Add setup verification script

Status: open

Summary:

Create one script that checks host prerequisites and reports exact repair
commands without making state changes unless explicitly requested.
