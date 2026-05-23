# GFX100 II — Master Property Catalog (PTP / PTP-IP remote control)

Single source of truth for our remote-control app: for every controllable or
readable camera property, the wire DPC, name, datatype, R/W, **how it is set**,
value encoding, and the source(s) it came from.

**Body / firmware**: GFX100 II, fw 02.30 (live-confirmed facts marked **[live]**).
**Date**: 2026-05-23.
**Companion data**: `2026-05-23-property-sweep-live.tsv` — a live read-sweep of all 159 DPCs
(every one returns a value over the wire; only 6 — `0x5007`, `0x500A`, `0x500E`, `0x5010`,
`0xD001`, `0xD028` — expose a rich `GetDevicePropDesc`, so wire datatypes/enums for the rest
come from `property_code_def.py`/stub, and *current values* from that TSV).

---

## Legend

### Datatype codes (from `property_code_def.py` `KEY_NAME_DATA_TYPE`)

| Code | Meaning | Wire width |
|---|---|---|
| `0x02` | UINT8 | 1 byte LE |
| `0x03` | INT16 | 2 bytes LE, signed |
| `0x04` | UINT16 | 2 bytes LE |
| `0x05` | INT32 | 4 bytes LE, signed |
| `0x06` | UINT32 | 4 bytes LE |
| `0xFFFF` | string / array | length-prefixed; usually a comma-joined ASCII list (e.g. `"1,2,"`) |
| `0x0000` | blob / opaque array | bulk binary (histogram, waveform, focus map) — read-only telemetry |

`form_flag`: `0x00`=none, `0x01`=range (min/max/step), `0x02`=enum.

### Get/Set (from `KEY_NAME_GET_SET`)

`RW` = `get_set==0x01` (gettable + settable). `RO` = `get_set==0x00` (read-only / status).
Note: RW in the catalog only means the property *can be written*; on this body some RW
exposure props are **ACK'd-but-ignored** unless set via the relative-step path (see ring-pick).

### Control method (from reference app RE — `APP_LIVEVIEW_CODE_MAP` + `big3-control-reference`)

* **list-pick (absolute)** — set via `SetDevicePropValue(0xPPPP, value)`. Pick an exact value
  from the enum. Used for ISO (`0xD02A`), WB (`0x5005`), FilmSim (`0xD001`), Flash (`0x500C`),
  Timer (`0x5012`) and the great majority of `0xD0xx`/`0xD1xx`/`0xD2xx` settings.
* **ring-pick (relative step)** — set via a Fuji-vendor relative-step opcode with a direction
  param (`1`=up/open, `0` or `0xFFFFFFFF`=down/close); the camera advances to the next legal
  value for the current mode. Direct `SetDevicePropValue` to the standard prop is
  **ACK'd (`0x2001`) but ignored** on this body. Used for **shutter** (`0x902C` `StepShutterspeed`),
  **aperture** (`0x902D` `StepFnumber`), **exposure-comp** (`StepExposureBias`, sibling
  vendor op via `Java_SDK_StepExposureBias`). Zoom/focus drive have their own
  start/step/stop opcodes.
* **read-only / status** — poll only (RO props, bulk telemetry blobs).
* **function-mode / session** — `0xDF00`/`0xDF01`/`0xDF2A` session-setup writes (not in the
  159-entry property catalog; listed in the Connection/function-mode section).

### Universal preconditions

1. **Session bring-up** (mandatory, non-skippable):
   `OpenSession(0x1002)` → `SetDevicePropValue(0xDF00, 6)` (SDK_MODE_NEUTRAL20) →
   `SetDevicePropValue(0xDF01, 22)` (SDK_MODE_IMAGE_LIVE_VIEW) →
   `GetDevicePropValue(0xDF2A)` then `SetDevicePropValue(0xDF2A, min(max,4))` (live-view
   version negotiate) → `InitiateOpenCapture(0x101C, 0, 0)`.
2. **`InitiateOpenCapture (0x101C)` precondition** — **writes only apply after live view
   is running.** Before `0x101C` the camera ACKs prop writes (`0x2001`) but silently ignores
   them. **[live]** (ISO 80→400 only after `0x101C`).
3. **`0xD212` poll-to-read-back rule** — fw 2.30 does **not** push `DevicePropChanged` events
   for `0xD2xx`. After any set/step, **poll the `0xD212` bundle** (~1–3.6 Hz) and read the
   sub-property back; `0x2001` OK on a write does **not** prove the value applied. The
   `0xD212` bundle carries the Big-3: `0x5007` (aperture), `0xD02A`/`0xD02B` (ISO still/movie),
   `0xD240` (shutter), among ~19 TLV sub-properties. **[live]**
4. **PausePolling / ResumePolling** must bracket `StartZoom` and still-capture, or the
   camera replies BUSY (`0x2019` / SDK 4102).

### Sources




* **Big3** = `wire/docs/2026-05-23-big3-control-reference.md` (live-confirmed Big-3 encodings + `0x101C`/`0xD212` rules).
* **SDK** = `SDK13410/HEADERS/GFX100II.h`, `XAPI.H`, `XAPIOpt.H` (enum names, API codes).


> several entries (`0x500D`, `0x500F`) return empty descriptors on the live fw-2.30 body, and
> the real ISO/shutter data lives on `0xD02A`/`0xD240`. Where live capture contradicts the
> catalog, the live fact wins and is marked **[live]**. Datatype/enum columns without a **[live]**
> tag are from the static catalog and should be re-validated against `GetDevicePropDesc` per session.

---

## Exposure

| Wire DPC | Name | Datatype | Get/Set | Control method | Value encoding / enum notes | Source(s) |
|---|---|---|---|---|---|---|
| `0x5007` | Aperture / F-Number (`ExposureTime`'s sibling `F-Number`) | UINT16 | RW | **ring-pick** (vendor `0x902D StepFnumber`, dir 1=open / 0xFFFFFFFF=close). Direct `SetDevicePropValue(0x5007)` is **ACK'd-but-ignored** **[live]** | F-number ×100 (`0x118`=280=F2.8). Enum is mode/lens-dependent; live descriptor showed `[350,530,710,1000,1100,1600,0xFFFF=AUTO]`. STUB sample `{100,200,300,3200}`. `0xFFFF`=AUTO. Read back via `0xD212`/`0x5007`. | PCD, STUB, reference app, Big3 |
| `0x500D` | Shutter speed (`ExposureTime`) | UINT32 | RW (catalog) | **ring-pick** (vendor `0x902C StepShutterspeed`, dir ±1). **`0x500D` returns empty descriptor on fw2.30 → not directly settable** **[live]** | Legacy. Read back the live shutter via `0xD212`→`0xD240`. STUB enum `{15, 244, 0x7A120000, 65504, -1, 65535}` (encoded shutter; -1/65535 = BULB/AUTO sentinels). | PCD, STUB, Big3 |
| `0x500E` | Exposure program mode (PASM) | UINT16 | RW | list-pick (but **dial-driven**: read-only from reference app — value mirrors body PASM dial) | `1`=M, `2`=P, `3`=A, `4`=S, `6`=? ; high-bit `0x8001..0x8008` = movie-mode positions (`0x8003`=dial-on-Movie). **[live]** M→1, A→3 confirmed. | PCD, STUB, reference app, Big3 |
| `0x500F` | ISO sensitivity (`ExposureIndex`) | INT32 | RW (catalog) | list-pick **per SDK** (`SetDevicePropValue(0x500F, int32)`), but **`0x500F` returns empty descriptor on fw2.30** → real wire ISO is `0xD02A` (below) **[live]** | Signed: manual = +literal ISO; AUTO = negative sentinel (`-1`..`-4`=AUTO presets, `-10`=AUTO). STUB enum lists 50–102400 + `-1..-4` + `0x8001..0x8005`. SDK `XSDK_SetSensitivity` writes this code/INT32 (FF0018API.so `0x95d70`). | PCD, STUB, Big3, SDK |
| `0x5010` | Exposure compensation (EV bias) | INT16 | RW | **ring-pick** (vendor `StepExposureBias`, dir ±1, via `Java_SDK_StepExposureBias`). Direct write ACK'd-but-ignored | EV ×1000 (millistops): `0`=0EV, `0x190`=400=+0.4? — STUB step grid runs −5000..+5000 in 1/3-stop increments (`-1388`=−5.0EV … `1388`=+5.0EV). | PCD, STUB, reference app |
| `0xD02A` | **ISO (still) — live wire prop** | UINT32 | RW | **list-pick** `SetDevicePropValue(0xD02A, v)` (after `0x101C`) **[live]** | Manual `v` = literal ISO (`400`=`0x190`, `6400`=`0x1900`). **AUTO** `v` = `0x80000000 | ceiling` (Auto 6400=`0x80001900`, Auto 12800=`0x80003200`). `GetDevicePropDesc(0xD02A)` enum is **`count=0` on this fw regardless of mode** (live-checked in manual ISO 80, manual 320, A-mode auto) — the valid-ISO list is NOT exposed on the wire; use the SDK canonical table (50…102400 + AUTO sentinels). *(No ISO dial on this PASM body — the earlier "dial in C" idea is dead; the write applies once `0x101C` live view is running.)* Read back via `0xD212`. **[live]** 80→400. | reference app, Big3 (not in PCD) |
| `0xD02B` | ISO (movie) — live wire prop | UINT32 | RW | list-pick `SetDevicePropValue(0xD02B, v)` (movie mode only) | Same encoding as `0xD02A`; movie-ISO list differs. | reference app, Big3 (not in PCD) |
| `0xD039` | Capture-function select (UINT32 extended) | UINT32 | RW | list-pick | Bitfield enum `{0x10000, 0x1, 0x20000, 0x2, …, 0x30003, 0x20003, 0x3}` — drive/AF/capture-mode flag combo. Extended form of `0xD208`. | PCD, STUB |
| `0xD208` | Capture-function select | UINT16 | RW | list-pick | enum `{0x100,0x104,0x200,0x004,0x300,0x304,0x400,0x500,0x008,0x00C,0x8000,0xA000,0x006,0x9000,0x002,0x9100,0x001,0x9300,0x005,0x00E,0x9200,0x040,0x804,0x080,0x4000}` — single/CL/CH/bracket/AE-BKT/etc. | PCD, STUB |
| `0xD230` | Force mode switch | UINT16 | RW | list-pick | enum `{1,2}`. Forces a mode transition. | PCD, STUB |
| `0xD272` | Movie metering mode | UINT16 | RW | list-pick | enum `{1,2,3,4}` (multi / spot / center-weighted / average). | PCD, STUB |
| `0x500C` | Flash mode | UINT16 | RW | **list-pick** `SetDevicePropValue(0x500C, v)` | Standard PTP flash-mode enum (auto/on/off/red-eye/slow-sync). Enum is body-reported. | reference app (not in PCD) |
| `0x5012` | Self-timer | UINT16 | RW | **list-pick** `SetDevicePropValue(0x5012, v)` | Timer seconds enum (off / 2s / 10s). reference app gates capture on this (`PROPERTY_TIMER`). | reference app (not in PCD) |
| `0x5001` | Battery level | UINT8 | RO | poll | Standard PTP battery %. Footer icon. | reference app (not in PCD) |

## Focus

| Wire DPC | Name | Datatype | Get/Set | Control method | Value encoding / enum notes | Source(s) |
|---|---|---|---|---|---|---|
| `0x500A` | Focus mode | UINT16 | RW (catalog) | list-pick per catalog, but **read-only from reference app** (AF-mode probe was a no-op on the wire) | enum `{0x0001=Manual? , 0x8001=AFS, 0x8002=AFC}` (cf. SDK `FOCUSMODE_MANUAL/AFS/AFC`). STUB current `0x0001`. | PCD, STUB, reference app, SDK |
| `0x501C` | AF area / metering mode (current) | UINT16 | RW (catalog) | list-pick per catalog; **polled but not written by reference app** | enum `{0x8001,0x8002,0x8003,0x8004}` = single-point / zone / wide-tracking / all. | PCD, STUB, reference app |
| `0xD112` | AF assist illuminator | UINT16 | RW | list-pick | enum `{1=off?,2=on}`. | PCD, STUB |
| `0xD171` | Focus position | INT16 | RW | list-pick / range | enum `{1..6}` (discrete focus steps). | PCD, STUB |
| `0xD1BA` | Start focus drive (`Fpcsh_StartFocus`) | UINT32 | RW | **action / step** (write to start continuous focus drive) | form=range; param = drive direction/speed. Pair with `0xD1BB` stop. | PCD |
| `0xD1BB` | Stop focus drive (`Fpcsh_StopFocus`) | UINT16 | RW | action | write to halt focus drive started by `0xD1BA`. | PCD |
| `0xD1BD` | Subject-detection AF mode | UINT16 | RW | list-pick | enum `{1..7}` (off / animal / bird / car / motorcycle / airplane / train — SDK subject classes). | PCD, STUB |
| `0xD1C5` | Tracking-AF frame info | string/array | RO | poll | comma-joined frame coords; camera→app. | PCD |
| `0xD209` | AF status (`Fpcsh_AfStatus`) | UINT16 | RO | poll | enum `{1,2,3,4}` (idle / searching / locked / failed). | PCD, STUB |
| `0xD225` | Focus-limiter position | string/array | RW | list-pick (string enum) | enum `{"1,2,","2,2,"}` — limiter near/far pair. | PCD, STUB |
| `0xD226` | Focus-limiter distance index | string/array | RO | poll | comma list. | PCD, STUB |
| `0xD227` | Focus-limiter search range | string/array | RO | poll | enum `{"0,0,","1,1,","2,2,"}`. | PCD, STUB |
| `0xD228` | Focus-limiter registered no. | UINT16 | RW | list-pick | enum `{1,2,3,4}`. | PCD, STUB |
| `0xD34A` | One-push AF behaviour (`InstantAfSetting`) | UINT16 | RW | list-pick | enum `{1=AF-S,2=AF-C}`. | PCD, STUB |
| `0xD34E` | AE/AF-LOCK mode | UINT16 | RW | list-pick | enum `{1,2}` (AE&AF / AE only). | PCD, STUB |
| `0xD323` | Full-time manual focus on/off | UINT16 | RW | list-pick | enum `{1=on,2=off}`. | PCD, STUB |
| `0xD35F` | Focus-scale (distance) unit | UINT16 | RW | list-pick | enum `{1=m,2=ft}`. | PCD, STUB |
| `0xD395` | Focus-area current | string/array | RW | list-pick (string) | `"x,y,size"` comma triple; tap-to-focus area. | PCD, STUB |
| `0x9026` | (AF-lock action — vendor opcode, not a property) | — | — | **vendor `LockS1Lock(area)` → `0x9026`** | Tap-to-focus on live-view surface. `UnlockS1Lock` → `0x9027`. No half-press; single-action. | reference app |

## Image / Film

| Wire DPC | Name | Datatype | Get/Set | Control method | Value encoding / enum notes | Source(s) |
|---|---|---|---|---|---|---|
| `0xD001` | Film simulation (`PresetMode`) | UINT16 | RW | **list-pick** `SetDevicePropValue(0xD001, v)` | enum `{0x01..0x13, 0x8000}`; SDK order: 0x01 Provia/Std, Velvia, Astia, ClassicChrome, ProNegHi, ProNegStd, Monochrome(+Y/R/G), Sepia, Acros(+Y/R/G), Eterna, ClassicNeg, BleachBypass, NostalgicNeg, RealaAce; `0x8000` = (custom/last). | PCD, STUB, reference app, SDK |
| `0xD007` | Dynamic range (`DrangMode`) | UINT16 | RW | list-pick | enum `{0xFFFF=AUTO, 100, 200, 400, 800}` (DR%). | PCD, STUB |
| `0xD00B` | WB shift R-Cy | INT16 | RW | list-pick / range | range `[-9..9]` step 1. | PCD, STUB |
| `0xD00C` | WB shift B-Ye | INT16 | RW | list-pick / range | range `[-9..9]` step 1. | PCD, STUB |
| `0xD017` | WB color temperature (Kelvin) | UINT16 | RW | list-pick | enum of Kelvin ×1 `{2500,2510,…,10000}` (STUB sample). SDK `WB_COLORTEMP_2500..10000`. | PCD, STUB, SDK |
| `0x5005` | White balance (preset) | UINT16 | RW | **list-pick** `SetDevicePropValue(0x5005, v)` | SDK WB enum: Auto / AutoWhitePri / AutoAmbience / Daylight / Incandescent / UnderWater / Fluorescent1-3 / Shade / ColorTemp / Custom1-3. (cf. movie-WB `0xD26C` enum values.) | reference app, SDK (not in PCD) |
| `0xD028` | DOF scale | UINT16 | RW | list-pick | enum `{1,2}` (px / film-format basis). | PCD, STUB |
| `0xD02F` | T-number (cine lens) | UINT16 | RW | list-pick | enum `{0,1}` (F vs T display). | PCD, STUB |
| `0xD239` | Aperture unit (cine lens) | UINT16 | RW | list-pick | enum `{1=F,2=T}`. | PCD, STUB |
| `0xD1B4` | WB gain | array/blob | RW | list-pick (blob) | sample current `1`, enum `{1..5}`; gain channels packed. | PCD, STUB |
| `0xD18B` | Custom WB info | string/array | RW | list-pick (string) | `"512,512,4,1,"` (R,B,?,slot). | PCD, STUB |
| `0xD20A` | Custom WB result status | UINT16 | RO | poll | enum `{1,2,3}` (ok / warn / fail). | PCD, STUB |

## Drive / Shutter-type

| Wire DPC | Name | Datatype | Get/Set | Control method | Value encoding / enum notes | Source(s) |
|---|---|---|---|---|---|---|
| `0xD208` / `0xD039` | Capture-function (drive incl. CL/CH/BKT) | UINT16 / UINT32 | RW | list-pick | See Exposure section enums. SDK `DRIVE_MODE_S=single`, `CL=0x1000`, `CH=0x10F0`, `BKT=0x4000`, `MOVIE`. | PCD, STUB, SDK |
| — | Burst / drive mode (Single/CL/CH/Bracket) | DriveModeInfo struct | RW | **vendor `Java_SDK_SetDriveMode(status)`** (RemoteAPICall SEND_DRIVE_MODE case 22) — distinct vendor opcode, **not** `SetDevicePropValue` | `GetDriveMode` returns struct; the body's continuous/single/bracket selector. | reference app |
| `0x5003` | Image size | UINT16 | RW (poll in reference app) | list-pick? | Header readout; SetDevicePropValue path unconfirmed (SA). | reference app (not in PCD) |
| `0xD23F` | Media status extended | UINT32 | RO | poll | range `[0..0xFFFF]`; slot/card flags. | PCD, STUB |
| `0xD211` | Media status | UINT16 | RO | poll | range `[0..0xFFFF]`; current `0x0101`. | PCD, STUB |
| `0xD224` | Release status (`ReleaseStatus`) | UINT16 | RO | poll | range `[0..0xEF00]`; shutter/release state machine. | PCD, STUB |
| `0xD255` | Media-eject warning | UINT16 | RO | poll | range `[0..7]` flag bits. | PCD, STUB |
| `0xD23B` | Playback media slot | UINT16 | RW | list-pick | enum `{1,2,3}` (slot1 / slot2 / both). | PCD, STUB |

## Live-view

| Wire DPC | Name | Datatype | Get/Set | Control method | Value encoding / enum notes | Source(s) |
|---|---|---|---|---|---|---|
| `0xD173` | Live-view image quality (`LiveImage_Quality`) | UINT16 | RW | list-pick | enum `{1,2,3}` (low/med/high). | PCD, STUB |
| `0xD174` | Live-view image size (`LiveViewImageSize`) | UINT16 | RW | list-pick | enum `{1,2,3}`. | PCD, STUB |
| `0xD23C` | Live-view image aspect ratio | UINT16 | RW | list-pick | enum `{1,2}`. | PCD, STUB |
| `0xD01B` | Live-view zoom (digital magnify) | UINT16 | RW | list-pick | enum `{1..0x0C, 0x11}` zoom steps. | PCD, STUB |
| `0xD18A` | Face-frame info | string/array | RW(catalog)/poll | poll | comma list of 14 face-rect fields; camera→app overlay. | PCD, STUB |
| `0xD22F` | Histogram data | blob | RO | poll (bulk) | 1024-float luminance histogram (4 KB on wire; pure telemetry — no band/state info). | PCD, STUB |
| `0xD2A0` | Focus map data | blob | RO | poll (bulk) | focus-peaking map. | PCD |
| `0xD2A2` | Waveform monitor data | blob | RO | poll (bulk) | — | PCD |
| `0xD2A3` | Vectorscope data | blob | RO | poll (bulk) | — | PCD |
| `0xD2A8` | Parade data | blob | RO | poll (bulk) | RGB parade. | PCD |
| `0xD2A1` | Waveform/vectorscope settings | UINT16 | RW | list-pick | enum unknown (not in STUB). | PCD |
| `0xD170` | Lens zoom position (zoom steps) | UINT16 | RW | list-pick / range | range `[1..3]` step 1 (STUB; lens-dependent). reference app drives zoom via step/start/stop vendor ops, not this prop directly. | PCD, STUB, reference app |
| `0xD1B8` | Start zoom drive (`StartZoom`) | UINT32 | RW | **action** (write to start continuous zoom) | range `[0x01..0x108]`; param = direction|speed. | PCD, STUB |
| `0xD1B9` | Stop zoom drive (`StopZoom`) | UINT16 | RW | action | enum `{1}`. | PCD, STUB |
| `0xD1BF` | Lens zoom speed | UINT16 | RW | list-pick / range | range `[10..80]` step 10. | PCD, STUB |
| — | Zoom (reference app) | — | — | **vendor `StepZoom`/`StartZoom`/`StopZoom`** (`Java_SDK_StepZoom`/`StartZoom`/`StopZoom`) | three distinct vendor opcodes; bracket with PausePolling/ResumePolling. | reference app |
| `0xD1B7` | Fan operation mode | UINT16 | RW | list-pick | enum `{1..5}`. | PCD, STUB |
| `0xD28E`? | (see Movie/audio) | | | | | |

## Movie

| Wire DPC | Name | Datatype | Get/Set | Control method | Value encoding / enum notes | Source(s) |
|---|---|---|---|---|---|---|
| `0xD240` | Movie/primary shutter (`MovieF-Number` per PCD comment; **carries shutter readback `SHUTTER_SPEED_EX`**) | UINT16 | RW | **ring-pick** for shutter (`0x902C`); read back here | **[live]** S-mode: `0x80004DA0`→`0x80009C40` after `0x902C` step. STUB enum `{0x64,0x78,0xC8,0x12C,0xC80}`. (PCD JP comment labels it MovieF-Number; reference app/live use it as primary SS_EX readback.) | PCD, STUB, reference app, Big3 |
| `0xD241` | Movie shutter speed | string/array | RW | list-pick (string fraction) | enum `{"1,8000","1,6400","1,24","65504,1","65535,1"}` = "num,den" or AUTO/BULB sentinels. | PCD, STUB |
| `0xD242` | Movie ISO | INT32 | RW | list-pick | enum `{50,60,80,100,-10=AUTO}` (STUB sample). | PCD, STUB |
| `0xD243` | Movie exposure compensation | INT16 | RW | list-pick / step | EV ×1000 grid `−5000..+5000`. | PCD, STUB |
| `0xD229` | Movie AF mode | UINT16 | RW | list-pick | enum `{1,2}`. | PCD, STUB |
| `0xD22A` | Movie focus-area X | INT16 | RW | list-pick / range | range `[-12..12]`. | PCD, STUB |
| `0xD22B` | Movie focus-area Y | INT16 | RW | list-pick / range | range `[-8..8]`. | PCD, STUB |
| `0xD22C` | Movie focus-area size | UINT16 | RW | list-pick / range | range `[1..6]`. | PCD, STUB |
| `0xD246` | Movie resolution (`MovieResolution`)*¹ | UINT16 | RW | list-pick | enum `{1..9}` (resolutions). *¹ reference app's `PROPERTY_DRIVE_MODE`=`0xd246` field name is misleading — reference app uses it as still/movie toggle (0/1); PCD/SDK define it as movie resolution. **Validate live before relying on a meaning.** | PCD, STUB, reference app |
| `0xD247` | Movie frame rate | UINT16 | RW | list-pick | enum `{1..6}`. | PCD, STUB |
| `0xD248` | Movie bitrate | UINT16 | RW | list-pick | enum `{1..6}`. | PCD, STUB |
| `0xD249` | Movie file format | UINT16 | RW | list-pick | enum `{1..0x0A}`. | PCD, STUB |
| `0xD24A` | High-speed rec mode | UINT16 | RW | list-pick | enum `{1,2,3}`. | PCD, STUB |
| `0xD24B` | High-speed rec resolution | UINT16 | RW | list-pick | enum `{1..9}`. | PCD, STUB |
| `0xD24C` | High-speed rec frame rate | UINT16 | RW | list-pick | enum `{1..4}`. | PCD, STUB |
| `0xD24D` | High-speed rec playback frame rate | UINT16 | RW | list-pick | enum `{1..6}`. | PCD, STUB |
| `0xD24E` | Movie media record dest | UINT16 | RW | list-pick | enum `{1..7}`. | PCD, STUB |
| `0xD250` | Movie ProRes proxy setting | UINT16 | RW | list-pick | enum `{1,2,3}`. | PCD, STUB |
| `0xD251` | Movie HDMI output RAW | UINT16 | RW | list-pick | enum `{1,2,3}`. | PCD, STUB |
| `0xD252` | Movie HDMI RAW resolution | UINT16 | RW | list-pick | enum `{1,2}`. | PCD, STUB |
| `0xD253` | Movie HDMI RAW frame rate | UINT16 | RW | list-pick | enum `{1..0x0D}`. | PCD, STUB |
| `0xD254` | F-Log recording | UINT16 | RW | list-pick | enum `{1..8}` (Off/F-Log/F-Log2/HLG/…). | PCD, STUB |
| `0xD256` | Movie crop magnification fix mode | UINT16 | RW | list-pick | enum `{1,2}`. | PCD, STUB |
| `0xD257` | Movie HDMI info display | UINT16 | RW | list-pick | enum `{1,2}`. | PCD, STUB |
| `0xD258` | Movie HDMI rec control | UINT16 | RW | list-pick | enum `{1,2}`. | PCD, STUB |
| `0xD259` | Movie IS mode | UINT16 | RW | list-pick | enum `{1..4}`. | PCD, STUB |
| `0xD25A` | Movie IS mode boost | UINT16 | RW | list-pick | enum `{1,2}`. | PCD, STUB |
| `0xD25E` | Movie tally light | UINT16 | RW | list-pick | enum `{1..7}`. | PCD, STUB |
| `0xD264` | Movie AF-C custom param | string/array | RW | list-pick / range (string) | range `["0,0".."10,10"]`. | PCD, STUB |
| `0xD265` | Movie high-freq flickerless | UINT16 | RW | list-pick | enum `{1,2}`. | PCD, STUB |
| `0xD26C` | Movie white balance | UINT16 | RW | list-pick | enum `{0x2=Auto,0x4=Daylight,0x6=?,0x8=?,0x8001..0x8021}` (maps to SDK WB names). | PCD, STUB, SDK |
| `0xD26D` | Movie WB shift R-Cy | INT16 | RW | list-pick / range | range `[-9..9]`. | PCD, STUB |
| `0xD26E` | Movie WB shift B-Ye | INT16 | RW | list-pick / range | range `[-9..9]`. | PCD, STUB |
| `0xD26F` | Movie WB color temp (K) | UINT16 | RW | list-pick | Kelvin enum `{2500..10000}`. | PCD, STUB |
| `0xD270` | Movie film simulation | UINT16 | RW | list-pick | enum `{0x01..0x13,0x8000}` (same as `0xD001`). | PCD, STUB |
| `0xD271` | Movie dynamic range | UINT16 | RW | list-pick | enum `{0xFFFF=AUTO,100,200,400,800}`. | PCD, STUB |
| `0xD273` | Movie monochrome WC (warm/cool) | INT16 | RW | list-pick / range | range `[-180..180]` step 10. | PCD, STUB |
| `0xD274` | Movie monochrome MG | INT16 | RW | list-pick / range | range `[-180..180]` step 10. | PCD, STUB |
| `0xD276` | Movie highlight tone | INT16 | RW | list-pick / range | range `[-20..40]` step 5. | PCD, STUB |
| `0xD277` | Movie shadow tone | INT16 | RW | list-pick / range | range `[-20..40]` step 5. | PCD, STUB |
| `0xD278` | Movie color | INT16 | RW | list-pick / range | range `[-40..40]` step 10. | PCD, STUB |
| `0xD279` | Movie sharpness | INT16 | RW | list-pick / range | range `[-40..40]` step 10. | PCD, STUB |
| `0xD27A` | Movie noise reduction | UINT16 | RW | list-pick | enum `{0,0x1000,…,0x8000}` (NR strength). | PCD, STUB |
| `0xD27B` | Movie inter-frame NR | UINT16 | RW | list-pick | enum `{1,2}`. | PCD, STUB |
| `0xD27C` | Movie peripheral light correction | UINT16 | RW | list-pick | enum `{1,2}`. | PCD, STUB |
| `0xD27D` | Movie face-detect mode | UINT16 | RW | list-pick | enum `{1,2}`. | PCD, STUB |
| `0xD27E` | Movie eye-AF | UINT16 | RW | list-pick | enum `{1..4}`. | PCD, STUB |
| `0xD27F` | Movie subject-detection AF | UINT16 | RW | list-pick | enum `{1..7}`. | PCD, STUB |
| `0xD281` | Movie MF assist | UINT16 | RW | list-pick | enum `{1..0x0C,0x1000..0x100B}`. | PCD, STUB |
| `0xD282` | Movie focus check | UINT16 | RW | list-pick | enum `{1,2}`. | PCD, STUB |
| `0xD283` | Movie focus-check lock | UINT16 | RW | list-pick | enum `{1,2}`. | PCD, STUB |
| `0xD294` | Movie full-time manual on/off | UINT16 | RW | list-pick | enum `{1,2}`. | PCD, STUB |
| `0xD295` | Movie crop zoom | UINT16 | RW | list-pick | enum `{1,2}`. | PCD, STUB |
| `0xD296` | Movie crop zoom range | blob | RO | poll | current `100`. | PCD, STUB |
| `0xD298` | Movie crop mode | UINT16 | RW | list-pick | enum unknown (not in STUB). | PCD |
| `0xD293` | Movie crop magnification value | UINT16 | RO | poll | range `[1000..1380]` step 10 (×1000). | PCD, STUB |
| `0xD256`/`0xD2A5` | Movie self-timer (`MovieCaptureDelay`) | UINT16 | RW | list-pick | `0xD2A5` enum unknown (not in STUB). | PCD |
| `0xD2A6` | Anamorphic desqueeze display | UINT16 | RW | list-pick | enum unknown. | PCD |
| `0xD2A7` | Anamorphic lens magnification | UINT16 | RW | list-pick | enum unknown. | PCD |
| `0xD2B1` | Rolling-speed priority | UINT16 | RW | list-pick | enum unknown. | PCD |
| `0xD22D` | Movie recording status | UINT16 | RO | poll | enum `{1,2,4,8}` (idle/rec/paused/…). | PCD, STUB |
| `0xD284` | Movie recording time | string/array | RO | poll | `"HH:MM:SS"`. | PCD, STUB |
| `0xD285` | Movie remaining time | string/array | RO | poll | `"HH:MM:SS"`. | PCD, STUB |
| `0xD286` | Movie focus-guide state | string/array | RO | poll | range `[0..200]` (focus-guide indicator). | PCD, STUB |
| — | Record start / stop | — | — | **vendor `InitiateMovieCapture` / `TerminateMovieCapture`** (`Java_SDK_*`) | likely Fuji-vendor `0x902X`; not `SetDevicePropValue`. | reference app |

### Movie — Timecode

| Wire DPC | Name | Datatype | Get/Set | Control method | Value encoding / enum notes | Source(s) |
|---|---|---|---|---|---|---|
| `0xD266` | Timecode | string/array | RW | list-pick (string) | `"HH:MM:SS.SS"`. | PCD, STUB |
| `0xD267` | Timecode display | UINT16 | RW | list-pick | enum `{1,2}`. | PCD, STUB |
| `0xD268` | Timecode start setting | UINT16 | RW | list-pick | enum `{1,2}`. | PCD, STUB |
| `0xD269` | Timecode count-up | UINT16 | RW | list-pick | enum `{1,2}` (rec-run / free-run). | PCD, STUB |
| `0xD26A` | Timecode drop-frame | UINT16 | RW | list-pick | enum `{1,2}` (DF / NDF). | PCD, STUB |
| `0xD26B` | HDMI timecode output | UINT16 | RW | list-pick | enum `{1,2}`. | PCD, STUB |
| `0xD292` | Timecode current value | string/array | RO | poll | `"00:00:00.00"`. | PCD, STUB |
| `0xD2B3` | Timecode status | UINT16 | RO | poll | enum unknown. | PCD |
| `0xD2C9` | Timecode sync setting | UINT16 | RW | list-pick | enum unknown. | PCD |
| `0xD2B2` | ATOMOS AirGlu connection | UINT16 | RW | list-pick | enum unknown. | PCD |

### Movie — Audio

| Wire DPC | Name | Datatype | Get/Set | Control method | Value encoding / enum notes | Source(s) |
|---|---|---|---|---|---|---|
| `0xD287` | Internal mic level setting | UINT16 | RW | list-pick | enum `{1,2,3}` (auto/manual/off). | PCD, STUB |
| `0xD288` | Internal mic level (manual) | INT16 | RW | list-pick | enum `{-300..+60}` step 15 (dB ×10). | PCD, STUB |
| `0xD289` | External mic level setting | UINT16 | RW | list-pick | enum `{1,2,3}`. | PCD, STUB |
| `0xD28A` | External mic level (manual) | INT16 | RW | list-pick | enum `{-300..+60}` step 15. | PCD, STUB |
| `0xD28B` | Mic level limiter | UINT16 | RW | list-pick | enum `{1,2}`. | PCD, STUB |
| `0xD28C` | Wind filter | UINT16 | RW | list-pick | enum `{1,2}`. | PCD, STUB |
| `0xD28D` | Low-cut filter | UINT16 | RW | list-pick | enum `{1,2}`. | PCD, STUB |
| `0xD28E` | Headphone volume | INT16 | RW | list-pick / range | range `[0..100]` step 10. | PCD, STUB |
| `0xD28F` | XLR adapter mic input source | UINT16 | RW | list-pick | enum `{1,2}`. | PCD, STUB |
| `0xD290` | XLR adapter monitoring source | UINT16 | RW | list-pick | enum `{1,2}`. | PCD, STUB |
| `0xD291` | XLR adapter HDMI output source | UINT16 | RW | list-pick | enum `{1,2}`. | PCD, STUB |
| `0xD305` | Mic/line input switch | UINT16 | RW | list-pick | enum `{1,2}`. | PCD, STUB |
| `0xD280` | Mic level indicator | string/array | RO | poll | comma list of L/R levels. | PCD, STUB |

## Connection / function-mode / session

| Wire DPC | Name | Datatype | Get/Set | Control method | Value encoding / enum notes | Source(s) |
|---|---|---|---|---|---|---|
| `0xDF00` | Camera function mode (outer) | UINT16 | set | **function-mode write** | `SetDevicePropValue(0xDF00, 6)` = SDK_MODE_NEUTRAL20. First handshake step. | Big3, reference app |
| `0xDF01` | Function mode (inner) | UINT16 | set | **function-mode write** | `SetDevicePropValue(0xDF01, 22)` = SDK_MODE_IMAGE_LIVE_VIEW. Second step. | Big3, reference app |
| `0xDF2A` | Live-view version (`VersionRemoteEx`=57130) | UINT16 | RW | function-mode negotiate | `GetDevicePropValue(0xDF2A)` then `SetDevicePropValue(0xDF2A, min(max,4))` (reference app uses 2). | Big3, reference app |
| `0x101C` | InitiateOpenCapture (PTP op, not a prop) | — | op | **session op** `InitiateOpenCapture(0,0)` | Starts live view; opens THROUGH (55741) + EVENT (55742) sockets. **Required before any setting write applies.** | Big3, reference app |
| `0xD136` | Function lock | UINT16 | RW | list-pick | enum `{1,2,3}`. | PCD, STUB |
| `0xD23E` | Camera operation lock | UINT16 | RW | list-pick | enum `{1,2}` (lock body controls during remote). | PCD, STUB |
| `0xD244` | Browser-remote user ID | string/array | RO | poll | sample `"test"`. | PCD, STUB |
| `0xD245` | Browser-remote password | string/array | RO | poll | sample `"test"`. | PCD, STUB |
| `0xD24F` | Browser-remote communication method | UINT16 | RO | poll | enum `{1,2}`. | PCD, STUB |
| `0xD15A` | Language | UINT16 | RW | list-pick | enum `{0x00..0x22}` (35 languages). | PCD, STUB |
| `0xD352` | Date/time format | UINT16 | RW | list-pick | enum `{1,2,3}` (Y-M-D / M-D-Y / D-M-Y). | PCD, STUB |

## Status / read-only

| Wire DPC | Name | Datatype | Get/Set | Control method | Value encoding / enum notes | Source(s) |
|---|---|---|---|---|---|---|
| `0xD22E` | Camera status | UINT16 | RO | poll | enum `{1,2,3,4}`. | PCD, STUB |
| `0xD1C6` | Body temperature warning | UINT16 | RO | poll | overheat warning level. | PCD |
| `0xD36A` | Battery info | UINT32 | RO | poll | range `[0..0xFFFFFFFF]`; packed battery struct (sample `0x070809`). | PCD, STUB |
| `0xD212` | **Big-3 / multi-prop bundle** (read-back) | TLV blob | RO | **poll** `GetDevicePropValue(0xD212)` | ~19 TLV sub-properties incl. `0x5007`,`0xD02A`,`0xD02B`,`0xD240`,`0x500E`. Poll after every set/step to confirm. **[live]** | Big3 (not in PCD) |
| `0xD292` | Timecode current value | string | RO | poll | (see Timecode). | PCD, STUB |
| `0xD38C` | Lens zoom pos query index | UINT16 | RW | list-pick / range | range `[1..0xFFFF]`; selects which `0xD38D`/`0xD38E` entry to read. | PCD, STUB |
| `0xD38E` | Per-zoom-pos focal-length table | string/array | RO | poll | comma table of focal lengths. | PCD, STUB |

## Misc

| Wire DPC | Name | Datatype | Get/Set | Control method | Value encoding / enum notes | Source(s) |
|---|---|---|---|---|---|---|
| `0xD037` | Shooting mode (`Fpcsh_Mode`) | UINT16 | RW | list-pick | enum `{0x01..0x08, 0x101..0x108, 0x81, 0xB1, 0xF0}` — Fuji internal scene/shoot modes. | PCD, STUB |

---

## Notes on coverage and confidence

* **All 159 PCD entries are represented.** Datatype / get-set / form-flag columns are
  mechanically derived from `property_code_def.py`; enum/range columns from
  `property_stub_value_def.py` (142 of the 159 have stub entries — the rest are bulk
  blobs or props the stub didn't sample, marked "enum unknown").
* **Supplemental rows** (`0x5005`,`0x500C`,`0x5012`,`0x5001`,`0x5003`,`0xD02A`,`0xD02B`,

  are the codes the reference app / live-wire path actually uses for the Big-3, WB, flash, timer,
  battery, session bring-up, and AF-lock. They are included because the app must use them.
* **Ring-pick is inferred** for shutter (`0x902C`), aperture (`0x902D`), exp-comp
  (`StepExposureBias` sibling) and live-confirmed for shutter/aperture (Big3). Everything
  else defaults to list-pick (absolute `SetDevicePropValue`) per the reference app control model.
* **`0xD246` ambiguity is unresolved**: PCD/SDK = movie resolution; reference app's `PROPERTY_DRIVE_MODE`
  field = still/movie toggle (0/1). Validate on the live body before relying on either meaning.
* **STUB enum values are samples**, not the live legal list. `GetDevicePropDesc(0xPPPP)` on the
  connected body returns the authoritative, mode/lens/state-dependent enum/range. Re-validate
  per session — especially ISO (`0xD02A` enum is `count=0` unless the ISO dial is in `C`).
