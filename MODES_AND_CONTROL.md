# GFX100 II — Control priority, shooting/function modes, and mode transitions

Companion to `2026-05-23-big3-control-reference.md` and `2026-05-23-property-catalog.md`.
Body GFX100 II fw 02.30. Sources: SDK13410 ProgrammingReference §1.7–1.8 + headers,
reference app RE (`APP_LIVEVIEW_CODE_MAP`, `PTP_PROPERTIES_REFERENCE`), and live wire captures
(`app_real_run_fw0230_wirelevel_v6`). `[live]` = confirmed on our body.

## 1. PC control vs Camera control (priority mode)

`XSDK_SetPriorityMode` → on the wire **`SetDevicePropValue(0xD207, v)`**:
`1` = `XSDK_PRIORITY_CAMERA`, `2` = `XSDK_PRIORITY_PC` (values from SDK `LiveView` sample).

| | Camera Priority (default at power-on) | PC Priority |
|---|---|---|
| Exposure settings | adjusted **on the camera** (but see note) | adjusted **remotely** via SDK |
| Shutter release | release button on camera; remote = `XSDK_ReleaseEx` | remote = `XSDK_Release` |
| Image transfer | yes (images flow to host as shot) | yes |
| Status on GFX100 II | what the reference app uses | "legacy / **not needed** on recent models" (SDK §1.7) |

**Key practical finding `[live]`:** on fw 2.30 we do NOT need PC Priority. The reference app never
writes `0xD207` (capture-confirmed) and operates entirely in **Camera Priority**, yet still
sets ISO (`0xD02A`), steps shutter/aperture, and pulls images remotely — the real enabler is
**`InitiateOpenCapture (0x101C)` (live view running)**, not the priority switch. We tried
`0xD207=2` and it did not change write-apply behaviour (the `0x101C` gate did). So:
- **For remote control, stay in Camera Priority + run live view (`0x101C`).**
- `0xD207` (PC Priority) is a writable legacy switch; movie-record controls are documented as
  available **only** in Camera Priority (SDK §1.8). Don't switch to PC Priority unless a
  specific legacy API (e.g. `XSDK_Release` shutter) requires it.

### 1a. Remote live-view VIDEO stream in Camera Priority `[live-confirmed]`

**Yes — you can pull the live-view video remotely while the camera stays in Camera Priority
and the photographer keeps shooting.** It's a pure *read* path; it doesn't take control.

Mechanism (through-picture channel):
1. Command channel (55740): bring-up `OpenSession → 0xDF00=6 → 0xDF01=22 → 0xDF2A → InitiateOpenCapture(0x101C)`.
   (Optionally `SetDevicePropValue(0xD207, 1)` to assert Camera Priority — writes `0x2001`.)
2. Open a **second TCP socket to `192.168.0.1:55742`** (through-picture). The camera **pushes**
   JPEG frames there — no per-frame request. Frame framing: **`<u32 LE total-length (incl. the
   4-byte prefix)> <14-byte header (frame seq# at body+4)> <JPEG FFD8…FFD9>`**; a frame's body
   can span several TCP segments, so read exactly `length` bytes. Some bodies are non-JPEG
   through-picture telemetry (skip if no `FFD8`).
3. **Confirmed:** 40 frames captured, valid **640×480** baseline JPEGs (~15.5 KB each), camera
   in Camera Priority. Tool: `probe_iso_liveview.py --camera-priority --stream-frames N --stream-secs T`.

Notes: live-view JPEG size is `0xD174` (L1024/M640/S320) and quality `0xD173`; observed 640×480
here. The `SDK_GetThroughPicture` API = "read next pushed frame" from 55742 (not a command op).

## 2. Shooting / function modes

Two independent layers:

**(a) PASM exposure mode** — `0x500E` ExposureProgramMode (UINT16, mirrors the physical
mode dial): `1`=M, `2`=P, `3`=A, `4`=S `[live]`; high-bit `0x8001..0x8008` = movie-mode dial
positions (`0x8003`=Movie). This selects which of shutter/aperture/ISO the user controls vs
the camera auto-sets (e.g. in A, shutter is auto; in S, aperture is auto) — relevant because
a step/set is ignored for a parameter the current PASM mode auto-controls.

**(b) FUNCTION mode** — the SDK's two-level function selector, set early in every session:
- **`0xDF00` outer** (`SetCameraFunctionMode`) = **`6` `SDK_MODE_NEUTRAL20`** for all normal paths.
- **`0xDF01` inner** (`SetFunctionMode`) = the operation:

| `0xDF01` | SDK_MODE_* | Function | Feature-version prop | BLE `FUNCTION_LAUNCH` |
|---|---|---|---|---|
| **22** | IMAGE_LIVE_VIEW | live view + remote take (stills/movie) | `0xDF2A` RemoteEx (set 2) | `take` = `0x0004` |
| **20** | (RemotePhotoViewEx) | image import / playback | `0xDF28` (set 3) | `get` = `0x0003` |
| **21** | (ReservedPhotoRcv) | auto image receive | `0xDF29` | — |
| **19** | FIRMWARE_DATA_TRANSFER | firmware update | `0xDF27` (set 1) | `fw` = `0x0005` |
| 1 / 5 | IMAGE_RECEIVE / REMOTE | (defined, not commonly used) | — | — |

Per-function bring-up `[live for 22]`: `OpenSession → 0xDF00=6 → 0xDF01=<mode> →
Get/Set feature-version → (live-view only) InitiateOpenCapture 0x101C`.

## 3. Transitioning stills ↔ video ↔ image-transfer

**How the reference app does it (capture-confirmed `[live]`): full teardown per function.** It never
re-purposes a live PTP-IP session — between functions it does `CloseSession (0x1003)` → drops
the socket → (BLE) re-`FUNCTION_LAUNCH` → new AP → new `OpenSession` → new `0xDF01`. The v6
capture shows 4 independent sessions (live-view `0xDF01=22`+`0x101C` ↔ import `0xDF01=20`+`0xDF28=3`),
each on a fresh socket with a ~24 s AP-rejoin gap. The AP is launched **per-function** by the
BLE `FUNCTION_LAUNCH` value, which is why a different function implies a new AP/session.

**So the supported recipe to switch functions:**
1. `CloseSession(0x1003)`, drop the PTP-IP socket.
2. BLE `FUNCTION_LAUNCH` = the new function's value (`take`/`get`/`fw`), wait `AP_STATE=launched`.
3. Re-join Wi-Fi (same open AP), new `OpenSession`, new `0xDF00`/`0xDF01` for the function.

**Stills ↔ video** is likely the *exception* (no teardown needed): both live under the same
**`take`/live-view function (`0xDF01=22`)**, so switching still↔movie is probably just changing
the camera's still/movie mode (`0x500E` movie position / drive-mode `0xD246`) within the live
session. **Untested — movie was never exercised in any capture.**

**OPEN EXPERIMENTS (live, not yet run):**
- **In-session `0xDF01` switch:** does the camera accept `0xDF01=22 → 20` (or →movie) on the
  *same* open socket without `CloseSession`/AP relaunch? The reference app never tries it; the camera
  may or may not allow it. If it does, function switches need no teardown.
- **Stills↔movie within live-view:** set `0x500E` to a movie position / `0xD246`, confirm the
  live-view stream + movie-ISO (`0xD02B`/`0xD242`) and a record start/stop op, all without
  re-launching.
- **PC Priority effect:** set `0xD207=2`, re-check whether `XSDK_Release`-style shutter / any
  setting behaves differently vs Camera Priority.
