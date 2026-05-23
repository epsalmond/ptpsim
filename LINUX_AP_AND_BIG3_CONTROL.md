# Linux: BLE → Wi-Fi AP → PTP-IP, and Big-3 (shutter / aperture / ISO) control

Canonical Linux-host recipe for driving a Fujifilm GFX100 II (verified body 0C3E,
fw **02.30**) from BLE pairing through Wi-Fi AP launch to PTP-IP camera control,
plus the exact commands for the "Big 3" exposure settings.

The macOS-first paths (`scripts/connect_camera_ap_wifi.sh`, Continuity Camera) do
not apply on Linux; this doc + the `*_linux.py` scripts are the Linux equivalents.
Full property/encoding detail and the live-capture evidence live in the wire


## Prereqs

- USB BT+Wi-Fi adapter (e.g. MediaTek MT7961). Wi-Fi iface e.g. `wlx00c0cab7f674`.
- `bleak>=0.22` in `.venv` (Linux BlueZ backend). `python -m venv .venv && .venv/bin/pip install bleak`.
- Passwordless sudo for `nmcli` (Wi-Fi join is the only privileged step):
  `/etc/sudoers.d/fuji-nmcli` → `eric ALL=(root) NOPASSWD: /usr/bin/nmcli`.
- Set the adapter GAP name (camera binds/persists it): `bluetoothctl system-alias mbp-7274`.

## One-shot flow

```sh
scripts/run_iso_probe_flow.sh           # BLE register+launch → join AP → Big-3 probe
```

Steps it runs (each also usable standalone):

1. **`scripts/register_launch_linux.py`** — BLE register + AP launch.
   - Fuji "pairing" on fw 2.30 is **GATT-level registration, not an SMP bond** (do
     not `bluetoothctl pair`; a bare `connect` resolves the full GATT).
   - Sequence: `PAIRING_KEY (aba356eb)` = legacy key from adv mfr-data (`0x04D8`,
     `02`+4 bytes) → `CONNECTED_DEVICE_NAME (85b9163e)` = `mbp-7274\0` →
     **`CONNECTED_DEVICE_BLE_PROTOCOL_VERSION (eb4166b0) = 0x0101`** → UTC / sync-cycle
     / image-transfer → camera acks with `LOCATION_SYNC_STATE = 0x0100` → wait.
   - **GOTCHA (the one that blocks AP launch):** the `eb4166b0 = 0x0101` write is
     REQUIRED. Without it, `FUNCTION_LAUNCH` is ignored and `AP_STATE` stays `0x8000`.
     (`camera.py._register_connected` now writes it.)
   - `FUNCTION_LAUNCH (600655e6) = 0x0004` (RemoteShooting / live-view+shutter;
     `0x0003` = image-import). Camera notifies `AP_STATE (a68e3f66)`
     `0x8002` (Launching) → `0x8001` (Launched).
   - fw 2.30 has **no registration-ID ack** (`f557d96b` is RED-only/absent). RED /
     newer firmware: read `f557d96b`, write `id | 0x20000000` before the name write.
2. **Wi-Fi join** — `sudo nmcli con add type wifi … ssid FUJIFILM-GFX100II-0C3E
   ipv4.never-default yes` (open AP on fw 2.30; `never-default` keeps internet on
   Ethernet). Camera = `192.168.0.1`; host gets `192.168.0.0/24`.
3. **`scripts/probe_iso_liveview.py`** — PTP-IP probe (read-only Phase 1).

**One PTP-IP session per AP launch:** the camera tears the AP down on PTP-IP
disconnect, and stops BLE-advertising ~60s after wake. Re-launch (re-wake) per run.

## Live-view control session (PTP-IP)

```
TCP 192.168.0.1:55740 → InitCommandRequest (GUID f2e4538fada5485d87b27f0bd3d5ded0,
                        friendly name mbp-7274) → InitCommandAck → OpenSession(0x1002)
SetDevicePropValue(0xDF00, u16 6)    # outer = SDK_MODE_NEUTRAL20
SetDevicePropValue(0xDF01, u16 22)   # inner = SDK_MODE_IMAGE_LIVE_VIEW
GetDevicePropValue(0xDF2A) ; SetDevicePropValue(0xDF2A, min(camera_max, 4))   # reference app uses 2
InitiateOpenCapture(0x101C, 0, 0)    # START LIVE VIEW — required before setting WRITES apply
```
This handshake is REQUIRED before property queries (else `GetDevicePropDesc` times out),
and **`InitiateOpenCapture(0x101C)` is required before any setting WRITE applies** — the
camera ACKs prop writes (`0x2001`) but ignores them until live view is running (verified:
ISO 80→400 only after `0x101C`).

## Big-3 commands (live-confirmed, fw 2.30)

| Setting | Control | How |
|---|---|---|
| **ISO** | `0xD02A` still / `0xD02B` movie (UINT32 RW) | **After `InitiateOpenCapture(0x101C)`**: `SetDevicePropValue(0xD02A, u32_LE)` — manual=literal ISO (`400`=`0x190`), AUTO-ceiling=`0x80000000\|ceiling` (Auto 6400=`0x80001900`). Live-confirmed (80→400). Read back via `0xD212`. |
| **Aperture** | vendor step **`0x902D StepFnumber(dir)`** | direct `SetDevicePropValue(0x5007,…)` is ACK'd-but-ignored — use the step op (ring-pick). Read back `0x5007` in `0xD212`. |
| **Shutter** | vendor step **`0x902C StepShutterspeed(dir)`** | LIVE-CONFIRMED (S mode, `0xD240` moved). Read back `0xD240` in `0xD212`. |

**Control model:** *list-pick* (ISO `0xD02A`, WB `0x5005`, film, flash, timer) = absolute
`SetDevicePropValue`; *ring-pick* (shutter/aperture/exp-comp) = vendor relative-step ops
`0x902C`-class (param = direction 1=up/0=down). All require `0x101C` live view running first.

Read back by polling the `0xD212` live-view bundle (no `DevicePropChanged` push on
fw 2.30): it carries `0x5007` (aperture), `0xD02A`/`0xD02B` (ISO), `0xD240` (shutter)
in one round-trip.

**ISO ≥ 400 (resolved):** 400/640/… set fine — the earlier "socket-kill at 400 / cap ≤320"
was the **missing `0x101C` live-view gate**, not the value or encoding. With live view
running, `SetDevicePropValue(0xD02A, 0x190)` set ISO 400 cleanly (live-confirmed 80→400).
The `0xD02A` descriptor enumerates no values (`count=0` on this fw); use the SDK canonical
ISO list (50…102400 + AUTO sentinels) — see `PROPERTY_CATALOG.md`.

## Full property catalog

All 159 camera properties (code, name, datatype, get/set, control method, encoding):
see [`PROPERTY_CATALOG.md`](PROPERTY_CATALOG.md) (+ live read-sweep `property-sweep-live.tsv`).
