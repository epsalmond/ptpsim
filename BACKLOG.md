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
- Generated init with accepted reference app GUID plus laptop friendly name succeeds through standard `GetObject` for handle `0x0000000c`; the complete JPEG payload is preserved at `rce/sessions/ptpip_probe_20260504T003935Z/get_object_payload.jpg`, but the final PTP OK response timed out after data transfer.
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
- Investigate why the decoded ObjectInfo `object_compressed_size` is `167936` while the full `GetObject` JPEG payload is `2456203` bytes for `_DSF8109.JPG`.
- Use the init comparator output to choose the next live generated-identity candidates.

### PROTO-002: Implement the next observed reference app PTP sequence

Status: open

Summary:

Reference captures under `rce/reference/ptp_decoded/` include liveview and
property/action traffic. Continue implementing the observed sequence after
successful init/open-session/property-read.

Completed:

- Promoted the live `GetObject` probe into `scripts/ptpip_export_object.sh` and `scripts/camera_ap_download_object.sh`; exports are accepted only when the JPEG payload independently validates as complete.

Next candidates:

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

### PROTO-005: Determine how to enter active FF80 USB transport mode

Status: in progress

Summary:

The FF80 reference project from `eric@nas.local:~/git/fffw/ff80` is copied into
`rce/reference/ff80` and patched locally so the target USB product can be
overridden. Normal camera USB mode enumerates as PTP product `04cb:02fe`; active
FF80 mode enumerates as `04cb:ff80`.

Current facts:

- `gphoto2 --auto-detect` sees `Fuji Fujifilm GFX100 II` on `usb:000,001`.
- `system_profiler SPUSBDataType` reports Fuji vendor `0x04cb`, product
  `0x02fe`, version `2.40`, and serial `593537303632230829053020110C3E`.
- `gphoto2 --summary` succeeds only after winning the race with macOS PTP
  helper processes.
- `rce/reference/ff80/ff80.py --product-id 0x02fe --trace ping` opens the
  device and claims interface 0, then stalls on FF80 `open_session`.
- FF80 USB request recipients `other`, `interface`, `device`, and `endpoint`
  all fail the same way with `LIBUSB_ERROR_PIPE`.
- In active FF80 mode, `scripts/poll_fuji_usb_devices.sh --once` sees
  `04cb:ff80`.
- `rce/reference/ff80/ff80.py --trace ping` succeeds: `open_session`, `ping`,
  `nop`, and `close_session` all return expected short replies.
- `rce/reference/ff80/ff80.py --trace info` returns `FUJIFILM`, `GFX100 II`,
  firmware `2.55`, serial `593537303632230829053020110C3E`, framework `1.00`.
- `cfgdata read 0x100 -s 0x60` confirms config fields including USB vendor
  `0x04cb`, normal PTP product `0x02fe`, jig product `0xff80`, camera name
  `GFX100 II`, serial, default directory `_FUJI`, and file prefix `DSCF`.
- `ff80.py dummy` returns 65,536 bytes and passes the expected hash check.
- `ram dump 0` timed out on the first read, left a zero-byte file, and the FF80
  command transport then timed out on `ping` until camera reboot/re-entry.
- After reboot/re-entry, `ram read 0x80003ff0 -s 0x10` succeeded and returned 16
  zero bytes.
- After reboot/re-entry, `ram dump 0x80000000 -s 0x4000` succeeded, wrote
  16,384 bytes, ended at `0x80004000`, and produced SHA256
  `a65ac6e7f228ada4702706181d0dad464d5aaa5e6785a588ba5d8f5ded3a68a0`.
- `scripts/ff80_dump_priority_ranges.sh` captures read-only priority ranges
  with pre/post `ping` checks and a 16-byte probe read before each bounded dump.
- `rce/sessions/ff80_priority_dumps_20260504T230211Z` captured all default
  priority ranges successfully: `0x80000000`, the `amp_shared`/`rpmsg_shared`
  heads, six message-pool windows, and two static-offset probe windows.
- `rce/sessions/ff80_priority_dumps_20260504T230326Z` captured the low ThreadX
  runtime windows successfully: scheduler globals, task records, and task
  record pointers. The task-record dump demonstrated that upstream
  `ff80.py ram dump` reads in `0x100` chunks, so non-aligned requests can write
  through the next chunk boundary.
- Initial dump triage found `syslog Ver 3.0` in the first three message-pool
  windows, `uiMPL001`/`uiMPL002` near scheduler globals, sparse shared-memory
  heads, and no obvious `FF80`, `JASM`, `GYRO_GAIN`, or ThreadX strings in the
  static-offset probes.
- After a wedged FF80 `ping` timeout, cold reboot/re-entry restored the
  transport. `rce/sessions/ff80_priority_dumps_20260504T232251Z` captured
  `0x000e1000..0x000e3fff`, `0x00057000..0x00058fff`,
  `0x000ea000..0x000eafff`, and `0x000ed000..0x000eefff`.
- `rce/sessions/ff80_cfgdata_20260504T234744Z` captured a full read-only
  cfgdata dump: 17,956,864 bytes, SHA256
  `9b91c39b3b35ca2af06348df40bc724dbf37a928bea2ab829eb12745ed48127a`.
  It contains early camera identity/config strings including `GFX100 II`,
  `_FUJI`, `DSCF`, `FUJIFILM`, USB vendor `0x04cb`, normal product `0x02fe`,
  and jig product `0xff80`.
- `rce/sessions/ff80_priority_dumps_20260504T235319Z` captured the combined
  `--next-targets` set in ascending RAM address order: the earlier
  task-slot/dispatch windows plus backlog ranges `0x00044000`, `0x00059000`,
  `0x0005e000`, `0x000a9000`, `0x004c7000`, and `0x004e7000`.
- `rce/sessions/ff80_priority_dumps_20260505T000131Z` captured the `--gap-targets`
  set in ascending RAM address order. It filled code/runtime gaps at
  `0x00040000`, `0x0005c000`, `0x00060000`, `0x000a1000`, `0x000ad000`,
  `0x000e0000`, `0x000e4000`, widened globals at `0x004c0000` and
  `0x004e0000`, and continued message-pool capture at `0x005c8000`.
- `rce/sessions/ff80_priority_dumps_20260505T000917Z` ran the low-watermark
  probe. `0x00030000`, `0x00020000`, `0x00010000`, `0x00008000`, and
  `0x00004000` each returned 16 bytes with post-read FF80 ping still healthy.
  `0x00002000` timed out with `LIBUSB_ERROR_TIMEOUT`, and a follow-up active
  FF80 `ping` also timed out. Treat `0x00004000` as the lowest verified readable
  address from this boot, and cold boot before any further FF80 commands.
- `scripts/ff80_analyze_dumps.sh` writes repeatable offline summaries for dump
  sessions. The current combined analysis is
  `rce/sessions/ff80_priority_dumps_20260505T000131Z/analysis.json`.
- The analyzer reports two ThreadX byte pools (`uiMPL001` at `0x000a0d60` and
  `uiMPL002` at `0x000a0df8`), `syslog Ver 3.0` in the first three
  message-pool windows, 194 nonempty `0x230` task-record slots out of 300 in
  the captured task-record range, a populated `0x57000` window, and zeroed
  `0xea000`/`0xed000` windows in this boot.
- The captured `0x57000` window starts with AArch64-looking instruction bytes
  and only a few pointer-like qwords near `0x582b8`; do not treat it as a
  decoded function-pointer table until the table/code mapping is reconciled
  against static xrefs.

Next investigation:

- Record the exact button/menu/service sequence that makes the camera enumerate
  as `04cb:ff80`.
- Add a scripted safe FF80 inventory command that captures poll, ping, info,
  small config-window read, and dummy bulk-read into `rce/sessions/`.
- Extend FF80 dump analysis beyond summaries: decode Fuji message-pool record
  fields, reconcile `0x57000` with the static indirect-branch xrefs, and map
  the dense token-like task slot table at `0x000e1000`.
- Either align future requested FF80 dump sizes to `0x100` or keep recording
  actual byte counts so every dump summary reflects the upstream chunking
  behavior.
- Keep FF80 command testing read-only until the transport and command safety are
  understood.

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



Status: Done

Summary: These ranges were appended to `scripts/ff80_dump_priority_ranges.sh
--next-targets` without dropping the existing `--next-targets` ranges, then
captured in ascending RAM address order in
`rce/sessions/ff80_priority_dumps_20260504T235319Z`.

Requested backlog ranges:

- 0x4E7000 + 0x1000 — top ADRP target, biggest data globals
- 0xA9000 + 0x4000 — covers 0xA9000–0xAD000 runtime data
- 0x44000 + 0x14000 — fills in code at 0x44550–0x57000 (BL targets)
- 0x59000 + 0x3000 — extends dispatch_table_57000 to 0x5C000
- 0x4C7000 + 0x1000 — secondary globals
- 0x5E000 + 0x2000 — adjacent code/data for 0x57000

Execution order:

- 0x00044000 + 0x14000
- 0x00057000 + 0x2000
- 0x00059000 + 0x3000
- 0x0005e000 + 0x2000
- 0x000a9000 + 0x4000
- 0x000e1000 + 0x3000
- 0x000ea000 + 0x1000
- 0x000ed000 + 0x2000
- 0x004c7000 + 0x1000
- 0x004e7000 + 0x1000

### RAM-002: Fill next low-memory and global gaps

Status: Done

Summary: Captured in ascending RAM address order with
`scripts/ff80_dump_priority_ranges.sh --gap-targets` in
`rce/sessions/ff80_priority_dumps_20260505T000131Z`.

Execution order:

- 0x00040000 + 0x4000
- 0x0005c000 + 0x2000
- 0x00060000 + 0x4000
- 0x000a1000 + 0x8000
- 0x000ad000 + 0xa000
- 0x000e0000 + 0x1000
- 0x000e4000 + 0xa000
- 0x004c0000 + 0x10000
- 0x004e0000 + 0x10000
- 0x005c8000 + 0x40000

Analysis notes:

- `0x00040000..0x00044000` and `0x0005c000..0x0005e000` are populated and look
  worth decoding with the adjacent `0x00044000..0x0005c000` code/data region.
- `0x005c8000..0x00608000` is mostly zero but has 2324 nonempty message-pool
  stride records.
- The scheduler/task/global gap fills are mostly sparse; preserve them as
  context, but prioritize decoding the populated code windows and message-pool
  records next.

### RAM-003: Find low RAM readable boundary

Status: Done

Summary: Added and ran `scripts/ff80_dump_priority_ranges.sh --low-watermark`
as a probe-only workflow. It performs a 16-byte read at each candidate address,
pings before and after each read, skips `0x00000000`, and stops on the first
failed probe. Session:
`rce/sessions/ff80_priority_dumps_20260505T000917Z`.

Results:

- `0x00030000` readable
- `0x00020000` readable
- `0x00010000` readable
- `0x00008000` readable
- `0x00004000` readable
- `0x00002000` timed out with `LIBUSB_ERROR_TIMEOUT`

Follow-up active FF80 `ping` also timed out after the `0x00002000` failure, so
the camera needs a cold boot before the next FF80 command. Do not probe below
`0x00004000` unless intentionally testing a known wedge boundary.

### RAM-004: Safe-fill uncovered RAM gaps above the hazardous low window

Status: Done

Summary: Added and ran `scripts/ff80_dump_priority_ranges.sh --safe-fill-gaps`.
The workflow uses the same read-only probe/dump/ping guards as the priority
dump wrapper, chunks larger gaps into `0x10000` reads, and deliberately skips
the hazardous `0x00002000..0x00040000` interval.

Session: `rce/sessions/ff80_priority_dumps_20260505T001807Z`

Execution order:

- 0x00064000..0x0009e000
- 0x000b7000..0x000b7400
- 0x000ef000..0x004c0000
- 0x004d0000..0x004e0000
- 0x004f0000..0x00508000

Results:

- All requested safe-fill ranges completed successfully.
- Analysis written to
  `rce/sessions/ff80_priority_dumps_20260505T001807Z/analysis.json`.
- `0x00064000..0x0009e000` contains sparse non-zero data and should be kept with
  the adjacent low-runtime/code windows for decoding.
- `0x000b7000..0x000b7400`, `0x000ef000..0x004c0000`,
  `0x004d0000..0x004e0000`, and `0x004f0000..0x00500000` were zero-filled in
  this boot.
- `0x00500000..0x00508000` is mostly zero but includes `syslog Ver 3.0` near
  `0x00507000`.

### RAM-005: Probe high addresses for likely 512 MiB DDR window

Status: Done

Summary: Added and ran `scripts/ff80_dump_priority_ranges.sh
--ram-size-probes`. The workflow is read-only and probe-only: it pings before
and after each 16-byte read, then records probe bytes under the session
directory.

Session: `rce/sessions/ff80_priority_dumps_20260505T003239Z`

Successful probes:

- `0x29b00000`
- `0x39a00000`
- `0x39b00000`
- `0x3f000000`
- `0x3ff00000`
- `0x3ffff000`

Notes:

- Post-probe FF80 ping stayed healthy.
- `0x3ffff000` is in the last page below `0x40000000`, which supports the
  hypothesis that a `0x20000000..0x40000000` 512 MiB DDR window is addressable.
- The last two probes returned all `ff` bytes, so this is sparse addressability
  evidence, not conclusive physical RAM-size proof. A real RAM-size register,
  boot memory map, or more systematic bounded reads are still needed.

### RAM-006: Test board-level 16 GB RAM hypothesis

Status: Done

Summary: Board inspection found two Micron MT53E2G32D4DE-046 WT:C LPDDR4
packages. Each package is 64 Gbit, so two packages imply 128 Gbit total, or
16 GB nominal RAM. Added and ran `scripts/ff80_dump_priority_ranges.sh
--ram-16gb-probes`.

Session: `rce/sessions/ff80_priority_dumps_20260505T003935Z`

Important limitation:

- The current FF80 RAM read API encodes the address in a 32-bit field
  (`params[8:12]` in `debug_read_ram`), so this probe cannot directly address
  RAM above 4 GB. It tests visible 32-bit aperture boundaries only.

Successful probes:

- `0x3ffff000` returned 16 bytes, all `ff`
- `0x40000000` returned 16 bytes, all zero
- `0x7ffff000` returned 16 bytes, all `ff`
- `0x80000000` returned 16 bytes, all zero
- `0xbffff000` returned 16 bytes, all `ff`

Failed probe:

- `0xfffff000` timed out with `LIBUSB_ERROR_TIMEOUT`, and follow-up FF80 ping
  also timed out. USB enumeration still showed `04cb:ff80`, but FF80 command
  transport needs a cold boot before more commands.

Follow-up:

- `scripts/ff80_dump_priority_ranges.sh --ram-16gb-probes` now skips
  `0xfffff000` by default.
- To prove 16 GB through FF80, find a RAM-size register, a boot memory map, or a
  64-bit/banked debug-read path. Sparse 32-bit aperture probes are not enough.
