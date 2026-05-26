# GFX100 II — mapping settings → offsets in the 69500-byte BackupSettings image

Goal: know which byte(s) of the "download camera settings" image (`.dat`, 69500 B) hold
which camera setting, by diffing dumps taken at different settings. Extends the prior

counters) with **setting semantics**. Tool: `fuji-remote/scripts/settings_map.py diff A B [--ignore-known]`.

## How "download settings" works on the wire

SDK `SDK_GetBackupSettings` → `CCameraCommandBackupSettings::ExecGetBackupSettings` →
`CPTPCommand::VendorExtensionOperation(code=0x1009 GetObject, …)` — i.e. the backup blob is
pulled as a **GetObject (0x1009)** of the backup object (the same flow that produced the
existing `tether-backup-*.dat`). Camera-side parser/materialiser is in ThreadX firmware
(per `BACKUP_SETTINGS_FINDINGS.md`); the host just transports bytes. The 8 existing dumps
were desktop-tether (X-Acquire/USB); whether the backup function-mode is reachable over our
BLE→AP→PTP-IP path is **[to verify]** for the live loop (else use the tether path).

## Bootstrap map from the 8 existing dumps (offline, 2026-05-23)

**Noise floor** (baseline vs restore-baseline, *identical* settings → ignore these): `0x00E8`
(checksum/save-counter), `0x0607`, `0x06EC`, `0x08A7`, `0xF724` (counters/timestamps; matches
the structural map's counter offsets).

**Exposure (ISO/shutter)** — these change together (auto-metering cascade; not cleanly
isolated by these auto-mode dumps): `0x00C0` (u16; tracked 390/177/476 across ISO+SS changes),
`0x07C3`, `0x07C8`. **Clean isolation requires single-variable changes in Manual mode.**

**White balance** (wb-auto → wb-incandescent) — large region, ~35 runs: `0x00D4`, `0x00D8`,
`0x00DC`, `0x00E4`, `0x00F4`, `0x00FC`, `0x0114`, **`0x0138`** (u16 5000↔16000), **`0x013C`**
(u16 — looks like color-temp/Kelvin: 12800↔200), **`0x0144`** (7362↔33017), `0x0148`, `0x0154`,
`0x0158`, `0x0180`, `0x0458`, `0x045B`, `0x0474`, `0x0854`, `0x08BB`, `0x08BE`, `0x08C2`,
`0x08C9`, `0x08CC`, `0x08CF`, `0x08FE`, `0xF6C0`, `0xF6C2` (WB gain/temp/shift block — many
sub-fields). WB is multi-field; needs per-WB-preset dumps to fully resolve.

## Plan for the full settings→offset map (scripted live loop)

`settings_map.py` will gain a `live-map` loop:
1. Connect (BLE→AP→PTP-IP), enter live view (`0x101C`), put camera in **Manual exposure +
   Manual ISO** (single-variable control; avoids auto-metering cascade).
2. For each setting in the catalog: set value A → download `.dat` → set value B → download →
   `diff --ignore-known` → record the offset(s) that changed for that setting.
3. Settable now: ISO (`0xD02A` list), shutter (`0x902C` step), aperture (`0x902D` step), WB
   (`0x5005` list), film-sim (`0xD001` list), drive/timer/flash, etc. (per the catalog).
4. Append confirmed `setting → offset (size, encoding)` rows here.

**Prereq to script the loop end-to-end:** confirm `GetBackupSettings`/backup-object GetObject
works over the PTP-IP session (or fall back to a desktop-tether download between set steps).
This will take a while (one set+download cycle per value, BLE re-launch per session if backup
needs its own function mode).
