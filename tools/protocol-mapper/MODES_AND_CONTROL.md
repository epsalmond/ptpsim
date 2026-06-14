# GFX100 II — Control priority, shooting/function modes, and mode transitions

Companion to `2026-05-23-big3-control-reference.md` and `2026-05-23-property-catalog.md`.
Body GFX100 II fw 2.30. Sources: SDK13410 ProgrammingReference §1.7–1.8 + headers,
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

### 1a. Remote live-view VIDEO stream — but it TAKES OVER the camera `[live-confirmed]`

**CORRECTION (2026-05-23, user-observed):** entering live view (`0x101C` / `DF01=22`) puts the
camera into **remote-shooting mode** — the **LCD goes black and the on-body controls are
disabled**, exactly like a normal reference app remote session. The remote becomes the shooter; the
photographer **cannot** shoot on-camera during this. Camera Priority (`0xD207=1`) does **not**
change this — it's the live-view/remote-shooting mode itself (one sensor→display→stream
pipeline). So this stream is for a **remote operator**, NOT a passive spectator alongside an
on-camera photographer.

**For "photographer shoots on-camera + someone views remotely"**, use the **auto-image-receive
/ image-transfer** function instead (`DF01=20`/`21`, `0xDF28`/`0xDF29`): the camera stays normal
and usable, the photographer shoots, and each captured frame transfers to the host. That is a
view of *captured shots* (not a 60 fps live feed), but it leaves the camera in the photographer's
hands. **[to validate]**

The mechanics below remain accurate for the *remote-operator* live-view stream:

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

Notes: `SDK_GetThroughPicture` = "read next pushed frame" from 55742 (not a command op).

### 1b. Live-view feed: frame-rate & quality — measured `[live-confirmed 2026-05-23]`

**The through-picture feed is a FIXED 640×480 @ ~60 fps preview, ~14.3 KB/frame, ~840 kbps
(~0.1 MB/s).** Measured a full size×quality sweep on one held-open 55742 socket (camera
**refuses a 2nd TP connect per `0x101C`** → hold one socket):

| `0xD174` size | `0xD173` quality | dims | fps | KB/frame | kbps |
|---|---|---|---|---|---|
| 1=L, 2=M, 3=S | 1=FINE, 2=NORMAL, 3=BASIC | **640×480 (all 9)** | **~60 (all)** | **~14.3 (all)** | **~840 (all)** |

**Key finding (CONFIRMED both ways):** changing `0xD174` (size) or `0xD173` (quality) has **NO
effect on the through-picture stream — neither mid-stream nor set BEFORE `0x101C`**. All 9
mid-stream combos AND a pre-start `S/BASIC` run streamed identical **640×480/60 fps/~840 kbps**
(writes returned `0x2001` OK). **The 55742 through-picture feed is a hard-fixed 640×480/60 fps
preview** — not configurable. Therefore `0xD173`/`0xD174`/`0xD1BC`/`0xD23C` govern the
**separate on-demand live-view image pulled via `0x9018 GetLiveViewData`**, NOT the push stream.

**Implication for constrained Wi-Fi (two distinct feeds):**
- **Push stream (55742):** fixed 640×480/60 fps/~840 kbps. Lowest latency, real-time. Only
  bandwidth knob is the fixed format itself; client-side decimation cuts *display* rate, not
  bytes. Best for the **model's posing feed** when Wi-Fi is OK.
- **Pull image (`0x9018 GetLiveViewData`):** **NOT validated on this fw/path.** A bare
  `0x9018` op (no params) **times out** and drops the session, and the reference app wire capture shows
  **no native `0x9018` calls** — the app's live-view feed is the through-picture push, not an
  on-demand pull. `SDK_GetLiveViewData` maps to SDK feature `0x3335` and issues `0x9018` via
  `VendorExtensionOperation` with a data-IN direction + output buffers and (likely) params we
  haven't recovered. So there is **no confirmed controllable-size/rate live-view feed** over
  our PTP-IP path; `0xD173`/`0xD174` appear inert for the wire live-view here.

**CONCLUSION — Wi-Fi feed options on this fw/path:** the only working live-view feed is the
**fixed 640×480/60 fps/~840 kbps through-picture push**. There is **no per-frame size/quality
reduction** available over PTP-IP (size/quality DPCs don't affect it; `0x9018` pull isn't
working). `~840 kbps` is already light, so for constrained Wi-Fi the practical lever is
**client-side frame decimation** (drop frames → lower *display* rate; camera bandwidth
unchanged at ~840 kbps). A genuine variable-bitrate feed would require either recovering the
`0x9018` params (deeper FF0018API.so RE) or transcoding host-side. **[open if needed]**

**Frame rate:** no live-view frame-rate DPC exists (the `0xD247`/`0xD24C`/`0xD253` rates are
for *movie recording*). The 60 fps push rate is fixed. **For constrained Wi-Fi:**
- `~840 kbps` is already light (HDMI/UVC would be ~tens of Mbps) — 640×480/60 fps streams fine
  over modest Wi-Fi.
- To cut the *display* update rate: **client-side frame decimation** (consume every Nth frame).
  This does NOT reduce camera→host bandwidth (camera still pushes 60 fps).
- To cut *bandwidth*: needs smaller per-frame size, which requires the pre-start size/quality
  path (untested) — if confirmed, S/BASIC set before `StartLiveView` should drop KB/frame.

**Bottom line for the model's posing feed:** the 640×480/60 fps/~840 kbps through-picture
stream is a viable network alternative to the HDMI cable / XLV — real-time, Wi-Fi-friendly,
camera-agnostic on our PTP-IP path. Caveat (per §1a): it takes over the camera, so the shot
must be triggered remotely.

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
