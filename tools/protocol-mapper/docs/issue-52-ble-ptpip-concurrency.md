# Issue #52: BLE/PTP-IP Concurrency Capture

## Summary

- Use `eric@rpi4b.local` / `192.168.4.21` as the live capture rig.
- Work from `/home/eric/git/ptpsim-issue-52` on branch
  `issue-52-linux-ble-ptpip-concurrency`, created from `origin/main`.
- Keep code changes scoped to `tools/protocol-mapper`; route ptpsim-wide
  follow-ups into GitHub issues.
- Use `tools/protocol-mapper/captures/issue-52-<UTC>/` as the canonical run
  artifact root. Do not use `rce/sessions/` for new capture output.

## Key Changes

- Add protocol-mapper capture tooling for phases A, B, and C:
  - Phase A: keep PTP/IP live-view active, then fire BLE remote-shutter writes
    `S1=0100`, `S2=0200`, `S0=0000`.
  - Phase B: start from BLE remote-trigger mode, then attempt PTP/IP
    app/live-view bring-up.
  - Phase C: fire PTP `InitiateCapture(0x100E)` while logging BLE
    notifications and HCI traffic.
- Add the BLE remote-shutter UUID constant:
  `7FCF49C6-4FF0-4777-A03D-1A79166AF7A8`.
- Capture raw and decoded evidence: `btmon`/btsnoop, BLE GATT discovery, all
  BLE notifications, PTP/IP 55740 command traffic, 55741 event traffic, 55742
  through-picture traffic, property polls, timeline JSONL, and verdict summary.
- Reuse the existing Linux BLE/AP/PTP-IP flow where possible, especially
  `register_launch_linux.py`, `run_iso_probe_flow.sh`, and the Pi lab model
  from `~/fuji/lab`.

## Test Plan

- Preflight must verify SSH to `rpi4b.local`, BlueZ controller, `btmon`, Wi-Fi
  interface, `bleak`, and capture tools before touching the camera.
- Run `( cd tools/protocol-mapper && python -m pytest -q )`.
- Live-run acceptance: each phase produces `summary.json`, `timeline.jsonl`,
  raw captures, decoded BLE writes/notifications, decoded 55741 events, and
  `VERDICT.md`.
- Post results to issue #52 with paths to the local ignored capture bundle.

## Assumptions

- `rpi4b.local` is the intended rig; verify with
  `avahi-resolve-host-name -4 rpi4b.local` before running.
- The Pi's MediaTek BT/Wi-Fi adapter is sufficient for host-side BLE HCI capture
  and PTP/IP Wi-Fi traffic.
- Sniffle or extra-radio air sniffing is optional for this issue, not required
  for the A/B/C verdict.
- Capture artifacts remain untracked; root `.gitignore` already ignores
  `**/captures/`.

## 2026-06-14 Checkpoint

- Worktree: `/home/eric/git/ptpsim-issue-52`
- Branch: `issue-52-linux-ble-ptpip-concurrency`
- Rig: `eric@rpi4b.local` / `192.168.4.21`
- Camera BLE address observed: `38:7C:76:74:73:21`
- Camera AP identity observed: `FUJIFILM-GFX100II-0C3E`,
  `38-7C-76-74-73-20`, target IP `192.168.0.1`
- Trusted connected-device name: `testhost`
- Remote-shutter characteristic confirmed in GATT:
  `7fcf49c6-4ff0-4777-a03d-1a79166af7a8`, handle `24582`, properties
  `write`

Implemented protocol-mapper-only capture support:

- `protocol_mapper/ble_ptpip_concurrency.py`
- `scripts/probe_ble_ptpip_concurrency.py`
- `tests/test_ble_ptpip_concurrency.py`
- BLE backend fixes for explicit-address targeting and Fujifilm manufacturer
  payloads shaped as `02 <4-byte pairing key> <extra bytes>`.

Live capture bundles currently saved under ignored
`tools/protocol-mapper/captures/`:

- `issue-52-20260614T131122Z`: best baseline. BLE found the camera, wrote the
  pairing key `f6313280`, discovered GATT, confirmed the remote-shutter UUID,
  read identity, and got AP SSID. Phase B reached `ap_state=0180` once, but
  Wi-Fi join failed because `wpa_cli` had no control socket for
  `wlx00c0cab7f674`.
- `issue-52-20260614T132332Z`: after parser fixes, phase B wrote pairing key
  `f6313280` and subscribed to notifications, but the camera stayed at
  `ap_state=0080` and never exposed PTP/IP. Phase C connected, wrote pairing
  key, then disconnected with BlueZ `Not connected`.
- `issue-52-20260614T132548Z`: targeted phase B while the camera was asleep;
  no live advertisement from `38:7C:76:74:73:21`, then direct connect failed
  with `BleakDeviceNotFoundError`.

Operational update: the camera was later found to have gone to sleep. It was
turned back on, auto-power-off was disabled, and follow-up live runs were
performed with the trusted connected-device name `testhost`.

Additional protocol-mapper changes from the live reruns:

- Added `--pairing-key` so the runner can write the known Fuji
  `PAIRING_KEY` payload `f6313280` when current advertisements omit
  manufacturer data.
- Corrected verdict ordering so PTP/IP event/live-view reader summaries are
  attached before per-phase verdicts are calculated.
- Added retry logging for PTP/IP 55740 control-port connection attempts within
  the existing PTP timeout budget.

Key live capture bundles under ignored `tools/protocol-mapper/captures/`:

- `issue-52-20260614T134851Z`: full A/B/C run with `--pairing-key f6313280`.
  Phase C succeeded: BLE launched AP to `0180`, `iw_static` joined the camera
  AP, PTP/IP 55740 opened, 55742 live-view streamed 63 JPEG frames, and PTP
  `InitiateCapture(0x100E)` produced two 55741 events (`CaptureStart` and
  `PostviewComplete`). Phase A blocked at `0080`; phase B timed out before
  registration.
- `issue-52-20260614T135606Z`: targeted phase A succeeded. With PTP/IP
  55740/55741/55742 active and 55742 streaming 498 JPEG frames, BLE remote
  shutter writes to `7fcf49c6-4ff0-4777-a03d-1a79166af7a8` (`0100`, `0200`,
  `0000`) were accepted. No 55741 events were observed from the BLE shutter
  writes during that window.
- `issue-52-20260614T135803Z`: targeted phase B reached AP launch and accepted
  the BLE remote shutter sequence before PTP/IP bring-up, but the subsequent
  55740 command-port connect failed with `ConnectionRefusedError(111)`.
- `issue-52-20260614T140044Z`: targeted phase B after adding 55740 connect
  retry timed out before BLE connection, so it is less useful than
  `issue-52-20260614T135803Z`.

Latest focused checks:

```sh
cd /home/eric/git/ptpsim-issue-52/tools/protocol-mapper
python3 -m pytest -q tests/test_ble_backend.py tests/test_ble_ptpip_concurrency.py
```

Result after the live-run fixes: `11 passed, 13 skipped`; skipped tests are the existing
`pytest-asyncio` environment gap tracked separately in issue #56.

Useful repeat command:

```sh
cd /home/eric/git/ptpsim-issue-52/tools/protocol-mapper
python3 scripts/probe_ble_ptpip_concurrency.py \
  --ble-address 38:7C:76:74:73:21 \
  --pairing-key f6313280 \
  --wifi-join-method iw_static \
  --scan-timeout 60
```

If only phase B is needed:

```sh
python3 scripts/probe_ble_ptpip_concurrency.py \
  --phases b \
  --ble-address 38:7C:76:74:73:21 \
  --pairing-key f6313280 \
  --wifi-join-method iw_static \
  --scan-timeout 60
```
