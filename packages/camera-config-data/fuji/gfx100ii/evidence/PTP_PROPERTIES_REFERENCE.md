# Fuji PTP-IP reference for client application iOS

**Status:** downstream consolidated reference for everything PTP-related decoded against reference app 2.7.3 + GFX100 II fw 2.30.
**Scope:** What it is and how to use it. Forensics (how it was derived, capture-by-capture
evidence) lives upstream in `operators/fuji/mobile/docs/` (see §"Where the forensics live").
**Last updated:** 2026-05-23

> **Reconciled 2026-05-26 with the client application app copy.** Folded in: the 2026-05-23
> Big-3 catalog annotations (property descriptions retained, app-exposure caveats
> appended); the corrected capture model (live view owns ONE open-capture session,
> `InitiateCapture` fires inside it — NOT a 3-op open/terminate per shutter; see
> §6, consistent with `INITIATE_CAPTURE_FW0230`); aperture direct-set is
> ACK-but-ignored on this body (see §3.3/§4.4, consistent with
> `APERTURE_PROTOCOL_FW0230`); the corrected `0x902C/0x902D/0x902E` step-opcode
> assignments; the §8/§11 image-enumeration sequence (`0x9054/0x9055/0x9050/0x9053`
> → `0xD620/0xD621`); and §3.9/§4.1.1 (the XLV live-view stream-config DPCs).

findings. For exposed client application camera controls, the current app contract is the
client application app's `docs/PTP_CAMERA_CONTROLS.md`, backed by the latest
`~/git/fuji-remote/PROPERTY_CATALOG.md`. As of the 2026-05-23 Big-3 work,
aperture/shutter/exposure-bias are ring-pick controls, ISO is a direct list-pick
write, focus/shooting mode are readbacks, and `0xd246` remains ambiguous rather
than an exposed still/movie or drive control.

---

## How to read this doc

This doc covers everything the iOS client needs to talk PTP-IP to a Fuji GFX100 II (and
likely most other recent Fuji bodies — see §"Other bodies" caveats below). It is organized
top-down by what an implementer does:

1. **Wire framing primer** — packet format, ports, the InitCommandRequest handshake
2. **Session lifecycle** — OpenSession, Function Mode handshake, version negotiation, close
3. **Properties catalog** — every Fuji propcode we know about (standard PTP + Fuji vendor)
4. **Live view** — entering it, the JPEG stream, the event channel, the polling loop
5. **AF tap** — phone-side tap-to-AF over the wire
6. **Still capture** ("Take") — the 3-op sequence + event lifecycle
8. **Image transfer** — enumerating and downloading images from the card
9. **Errors and retries** — what to retry, what to give up on
10. **iOS implementation cheat sheets** — provisional code for each major flow
11. **What's not decoded** — open questions, known unknowns
12. **Where the forensics live** — pointers back to the upstream evidence

---

## 1. Wire framing primer

### 1.1 Ports

The camera (at `192.168.0.1` once you've joined its AP) listens on three TCP ports during a
live PTP-IP session:

| Camera port | What flows | Notes |
|---|---|---|
| **55740** | PTP-IP control channel — opcodes, property reads/writes, OpenSession etc. | bidirectional; always open during a session |
| **55741** | **Event channel** — small async event packets from camera to phone | inbound-only from phone's view |
| **55742** | **Through-picture (live-view JPEG) channel** — large JPEG frames | inbound-only from phone's view |

**Important:** reference app's source enum (`SOCKET_PORT_NO`) labels 55741 as `PORT_THROUGH_SOCK` and
55742 as `PORT_EVENT_SOCK` — those names are **backwards** from the wire reality. The Fuji
developer either swapped the names or the camera firmware swapped its assignment. Trust the
bytes, not the names. The table above is what actually happens.

### 1.2 Fuji's compressed PTP-IP framing (control + event channels)

reference app uses a non-standard compressed PTP-IP framing on 55740 and 55741. The standard PTP-IP
spec puts a 4-byte `packet_type` field after `length`; Fuji collapses this to 2 bytes and
moves the PTP opcode into the same word.

```
offset 0x00  uint32 LE   length        (inclusive of these 4 bytes)
offset 0x04  uint16 LE   packet_type
offset 0x06  uint16 LE   opcode / response_code / event_code
offset 0x08  uint32 LE   transaction_id (tid)
offset 0x0c  variable    params (uint32 LE each) or data payload
```

**Packet type values:**

| `packet_type` | Direction | Meaning |
|---|---|---|
| `1` | out | OperationRequest (phone sending an opcode + params) |
| `2` | out / in | Data phase (request payload, e.g. value for SetDevicePropValue; or response data e.g. ObjectInfo bytes) |
| `3` | in | OperationResponse (camera reporting result code) |
| `4` | in | Event (on 55741 only) |

**The one exception: `InitCommandRequest`** (the very first packet on 55740 of each session)
uses **standard** PTP-IP framing with a full 4-byte `packet_type=1` and a 16-byte initiator
GUID. See §2.1.

### 1.3 Live-view through-picture framing (port 55742)

Different from the compressed PTP-IP framing — JPEG frames have their own simple format:

```
offset 0x00  uint32 LE   total_length    (inclusive of these 4 bytes)
offset 0x04  uint32 LE   reserved        (always 0 in fw 2.30)
offset 0x08  uint32 LE   frame_counter   (monotonic per through-picture session, resets to 0 on reopen)
offset 0x0c  uint32 LE   jpeg_offset_adjust  (reference app READS this; JPEG starts at byte 0x12 + value)
offset 0x10  uint16 LE   reserved        (always 0 in fw 2.30)
offset 0x12+ JPEG body                   (starts FF D8 FF C4, ends FF D9; no JFIF/EXIF wrapper)
```

**JPEG decoder note:** the body uses SOI directly followed by a Huffman table marker (no
JFIF or EXIF APP blocks). Apple `ImageIO` / `UIImage(data:)`, libjpeg-turbo, and Android
`BitmapFactory` all accept this. Strict decoders that require JFIF may not.

**Skip distance:** **do not hardcode skip=14 bytes.** Read the uint32 LE at full-frame
offset `0x0c` and skip `(value + 14)` bytes. On fw 2.30 that value is always 0 so effective
skip is 14, but the field exists for newer firmware to insert per-frame metadata between
byte 18 and the JPEG SOI.

**Source rate:** ~60 fps (median 16 ms inter-frame). reference app's UI redraws at 30 fps and drops
intermediate frames.

**Natural stream gaps:** the camera pauses the stream for **~1.2 seconds every ~13 seconds**
on fw 2.30 over Wi-Fi. Not events, not errors. Your receive code must tolerate at least
1.5 s of inactivity (use ≥ 3 s for safety) without canceling the connection.

### 1.4 Response codes you'll see

Common values returned in `packet_type=3` packets at offset `0x06`:

```
0x2001  OK_NoData       (operation succeeded, no data phase)
0x2002  SessionAlreadyOpen
0x2003  Transaction Cancelled
0x200A  AccessDenied
0x2019  DeviceBusy / 4102 SDK_ERRCODE_BUSY    ← retry sentinel
0x201F  TransactionCancelled
```

reference app's higher-level error code translation (visible in `ControlFFIR.java:70-79`):

```
SDK_ERRCODE_NOERR             = 0
SDK_ERRCODE_BUSY              = 4102  (0x1006)  ← retry pattern, see §9
SDK_ERRCODE_COMMUNICATION     = 8193  (0x2001)
SDK_ERRCODE_TIMEOUT           = 8194  (0x2002)
SDK_ERRCODE_SYSTEMERROR       = 12289 (0x3001)
SDK_ERRCODE_STORE_FULL        = 12290 (0x3002)
SDK_ERRCODE_PROTECT_CARD      = 12291 (0x3003)
SDK_ERRCODE_STORE_NOT_AVAILABLE = 12293 (0x3005)
SDK_ERRCODE_INTERNAL          = 36865 (0x9001)
SDK_ERROR                     = -1
SDK_ERROR_OPEN_SOCKET         = -2
SDK_PTP_ACCESS_DENIED         = 49    (0x0031)
SDK_PTP_STORE_NOT_AVAILABLE   = 53    (0x0035)
SDK_PTP_DEVICE_BUSY           = 59    (0x003B)
```

---

## 2. Session lifecycle

### 2.1 `InitCommandRequest` (82-byte handshake — opens the session)

The very first packet on a freshly-connected 55740 socket. **Standard** PTP-IP framing
(4-byte `packet_type` not compressed):

```
offset 0x00  uint32 LE  length = 82 (0x52)
offset 0x04  uint32 LE  type = 1 (InitCommandRequest)
offset 0x08  16 bytes   initiator GUID (reference app Android hardcodes f2 e4 53 8f ad a5 48 5d 87 b2 7f 0b d3 d5 de d0)
offset 0x18  4 bytes    reserved (always 0)
offset 0x1c  variable   friendly name (UTF-16LE NUL-terminated, e.g. "Pixel-6-4976")
offset 0x36..0x51  ~28 bytes — **don't matter to the camera.** Zeros work.
                  (reference app leaks process memory here due to an OOB read bug — see
                   INIT_COMMAND_REQUEST_FW0230_STACK_LEAK_2026-05-18.md)
```

**For client application iOS:** use any 16-byte initiator GUID (or a fresh per-install one for
fingerprinting friendliness; the camera doesn't validate it). Friendly name = the iPhone's
user-visible device name, UTF-16LE NUL-terminated. Pad the tail with zeros — do NOT replicate
reference app's stack leak.

Camera responds with `InitCommandAck` (64 bytes; payload includes the camera's GUID).

### 2.2 OpenSession + Function Mode handshake

After `InitCommandAck`, reference app opens a PTP session and switches to a specific Function Mode for
the operation it wants to perform. Pattern is always:

```
1.  OpenSession (0x1002) with SessionID=1
       out: 10 00 00 00  01 00  02 10  01 00 00 00  01 00 00 00

2.  SetCameraFunctionMode (outer mode) — always called as the first step
       The JNI side calls SDK_SetCameraFunctionMode(N); on the wire this is realized as
       SetDevicePropValue(0xDF00, N) — N=6 (SDK_MODE_NEUTRAL20) for every observed path.
       Retried on SDK_ERRCODE_BUSY (4102).

3.  SetFunctionMode (inner mode) — chosen per operation
       Wire equivalent: SetDevicePropValue(0xDF01, mode_id)
       Retried on BUSY.

       Operation       Inner mode (SDK_MODE_*)
       ──────────────  ────────────────────────────────────────────
       Live view +     22  SDK_MODE_IMAGE_LIVE_VIEW
         remote take

       Image import    20  (no constant; matches Fpcsh_VersionRemotePhotoViewEx pairing)

       Auto image      21  (no constant; matches FPCSH_VERSION_RESERVED_PHOTO_RECEIVED_EX)
         receive

       FW update       19  SDK_MODE_FIRMWARE_DATA_TRANSFER

       Other constants (defined but not always exercised):
         1   SDK_MODE_IMAGE_RECEIVE
         5   SDK_MODE_REMOTE

4.  GetFunctionVersion(propcode) — read camera's max supported version for this feature
       The propcode is one of the Fpcsh_* values (§3.5).
       For live view:  0xDF2A  Fpcsh_VersionRemoteEx
       For import:     0xDF28  Fpcsh_VersionRemotePhotoViewEx
       For fw update:  0xDF27  Fpcsh_VersionFirmwareDataTransfer
       Camera returns uint16 (or uint8) value = camera's max version.

5.  SetFunctionVersion(propcode, min(camera_max, app_max)) — commit a version we both support
       For live view, reference app's max is 4 (VERSION_MODE_REMOTE_EX).
```

All four steps must succeed before the operation-specific opcodes will work. Any of the
retry-able setters can return 4102 BUSY; retry up to `RETRY_COUNT` (reference app uses ~5) with
~1 second between attempts.

### 2.3 Session teardown

```
TerminateOpenCapture (0x1018) — if a capture session is open (param = tid of original Open)
CloseSession (0x1003) — final cleanup before disconnect
```

If the phone disconnects the TCP socket without sending CloseSession, the camera assumes the
session is dead and cleans up internally. Polite shutdown reduces "session already open"
errors on the next connect.

---

## 3. Properties catalog

### 3.1 Standard PTP 1.1 prop codes (`0x5000–0x501F`)


`GetStringDevicePropCode` for log formatting). These are the canonical PTP-spec names;
reference app uses some by different (sometimes mis-typed) names in its UI.

| Wire | PTP name | reference app `CameraProperty.*` alias |
|---|---|---|
| `0x5000` | Undefined | — |
| `0x5001` | BatteryLevel | `PROPERTY_BATTERY` |
| `0x5002` | FunctionMode | (see §2.2 — used as session mode selector at 0xDF00/0xDF01) |
| `0x5003` | ImageSize | `PROPERTY_IMAGE_SIZE` |
| `0x5004` | CompressionSetting | — |
| `0x5005` | WhiteBalance | `PROPERTY_WHITE_BALANCE` |
| `0x5006` | RGB Gain | — |
| `0x5007` | F-Number | `PROPERTY_EXPOSURE` ← yes, reference app calls aperture "EXPOSURE" |
| `0x5008` | FocalLength | — |
| `0x5009` | FocusDistance | — |
| `0x500A` | FocusMode | `PROPERTY_FORCUS_MODE` ← reference app typo for FOCUS |
| `0x500B` | ExposureMeteringMode | — |
| `0x500C` | FlashMode | `PROPERTY_FLASH` |
| `0x500D` | ExposureTime | `PROPERTY_SHUTTER_SPEED` |
| `0x500E` | ExposureProgramMode | `PROPERTY_SHOOTING_MODE` — **value `0x8003` = movie mode** |
| `0x500F` | ExposureIndex | — |
| `0x5010` | ExposureBiasCompensation | `PROPERTY_EXPOSURE_CORRECTION` |
| `0x5011` | DateTime | — |
| `0x5012` | CaptureDelay | `PROPERTY_TIMER` (self-timer) |
| `0x5013` | StillCaptureMode | — |
| `0x5014` | Contrast | — |
| `0x5015` | Sharpness | — |
| `0x5016` | DigitalZoom | — |
| `0x5017` | EffectMode | — |
| `0x5018` | BurstNumber | — |
| `0x5019` | BurstInterval | — |
| `0x501A` | TimelapseNumber | — |
| `0x501B` | TimelapseInterval | — |
| `0x501C` | FocusMeteringMode | — |
| `0x501D` | UploadURL | — |
| `0x501E` | Artist | — |
| `0x501F` | CopyrightInfo | — |

### 3.2 Fuji vendor properties reference app explicitly names (`0xD000–0xD6FF`)

From `CameraControlActivity.CameraProperty`:

| Wire | reference app name | What it controls |
|---|---|---|
| `0xD001` | `PROPERTY_FILM_SUMILATION` (sic) | Film-simulation profile (Provia / Velvia / Classic Neg / etc.) |
| `0xD018` | `PROPERTY_CCD_MODE` | Sensor / CCD readout mode (older reference app name; not in the latest master catalog, not exposed as a client application control) |
| `0xD019` | `PROPERTY_RECMODE_ENABLE` | Whether recording controls are enabled |
| `0xD01D` | `PROPERTY_MACRO` | Macro focus mode (older reference app name; not in the latest master catalog, not exposed as a client application control) |
| `0xD028` | `PROPERTY_EXPOSURE_COLOR` / `PROPERTY_SHUTTER_SPEED_COLOR` | UI color tag |
| `0xD02A` | `PROPERTY_ISO` | ISO (still mode) |
| `0xD02B` | `PROPERTY_MOVIE_ISO` | ISO (movie mode) |
| `0xD170` | `PROPERTY_ZOOM` | Power-zoom position |
| `0xD17C` | `PROPERTY_S1_LOCK` | AF area state (also encodes aspect ratio in high bytes — see §5.1) |
| `0xD209` | `PROPERTY_S1_LOCK_COLOR` | AF box color (0=white/none, 1=green/locked, 2=red/failed) |
| `0xD21B` | `PROPERTY_DEVICE_ERROR` (also: composite container code inside `0xD212`) | Error/status code |
| `0xD212` | — | **Composite live-view bundle** — see §3.4 |
| `0xD229` | `PROPERTY_REMAINING_SHEET` | Still shots remaining on card |
| `0xD22A` | `PROPERTY_REMAINING_TIME` | Movie record time remaining |
| `0xD240` | `PROPERTY_SHUTTER_SPEED_EX` | Extended-range shutter speed |
| `0xD241` | `PROPERTY_PIXEL_SIZE` | Earlier reference app gloss: pixel-size encoding for resolution. Latest catalog maps nearby movie/shutter fields differently, so do not expose as an app control without live confirmation |
| `0xD242` | `PROPERTY_BATTERY_MULTI_STEPS` | Earlier reference app gloss: finer-grained battery vs `0x5001`. Latest master catalog maps this as a movie ISO field, distinct from client application's exposed live movie ISO `0xd02b` |
| `0xD243` | `PROPERTY_CURRENT_SHUTTER_TYPE` | Earlier reference app gloss: mechanical / electronic / silent. Latest catalog treats this neighborhood as movie exposure fields, so do not expose without live confirmation |
| `0xD245` | `PROPERTY_AF_SELECT_MODE` | AF area selection mode (older reference app name; not exposed in client application until live-confirmed) |
| `0xD246` | `PROPERTY_DRIVE_MODE` | Ambiguous: earlier reference app gloss was drive mode (single / cont / bracket / timer), but latest PCD/SDK catalog says movie resolution; not exposed until live-confirmed |
| `0xD2B7` | `PROPERTY_MOVIE_TRANSPARENTE_FRAME_INFO` (sic) | Movie-mode overlay frame info |

### 3.3 F-Number encoding (`0x5007` and inside `0xD212`)

Aperture values are encoded as **F × 100 as a 32-bit unsigned little-endian integer**:

```
F1.4  → 140  (0x8C)
F2.0  → 200  (0xC8)
F2.8  → 280  (0x118)
F4.0  → 400  (0x190)
F5.6  → 560  (0x230)
F8.0  → 800  (0x320)
F11   → 1100 (0x44C)
F16   → 1600 (0x640)
F22   → 2200 (0x898)
```

Do not use the generic camera-controls surface to direct-set aperture on real
hardware. Newer live work showed `SetDevicePropValue(0x5007, F*100)` can ACK
while being ignored; client application uses reference app's camera-managed vendor opcode
`0x902D StepFnumber` and then reads back `0xd212`.

### 3.4 The composite property `0xD212` (live-view bundle)

reference app polls this at ~3.6 Hz (median 222 ms cadence) during live view. One request = 16 bytes
out, one response = 130 bytes in. The response wraps an outer container code `0xD21B` and
contains **19 sub-properties** as TLV entries `<uint16 propcode><uint32 LE value>`.

```
Request shape (16 bytes):
   10 00 00 00      length = 16
   01 00            type = 1 (OpRequest)
   15 10            opcode = 0x1015 (GetDevicePropValue)
   <4 byte tid>
   12 d2 00 00      param = 0xD212

Response shape (130 bytes):
   82 00 00 00      length = 130
   02 00            type = 2 (Data)
   15 10            opcode echo
   <4 byte tid>
   14 00 1b d2      outer-header: <length=20><container propcode=0xD21B>
   00 00 00 00      4 zero bytes
   then 19 × 6-byte TLV entries: <uint16 propcode> <uint32 LE value>
```

The 19 entries are always the same set on fw 2.30:

```
Standard PTP (6 entries):
   0x5005 WhiteBalance
   0x5007 F-Number              ← aperture value F × 100
   0x500A FocusMode
   0x500C FlashMode
   0x500E ExposureProgramMode   ← 0x8003 means movie mode
   0x5012 CaptureDelay

Fuji vendor (13 entries):
   0xD001 FILM_SUMILATION
   0xD018 CCD_MODE              ← older reference app label; not an exposed client application control
   0xD028 EXPOSURE_COLOR
   0xD02A ISO
   0xD02B MOVIE_ISO
   0xD17C S1_LOCK               ← bytes 3,2 = (aspect_w, aspect_h); bytes 1,0 = (col, row) of last AF tap
   0xD209 S1_LOCK_COLOR         ← 0=white, 1=green/locked, 2=red/failed
   0xD229 REMAINING_SHEET
   0xD22A REMAINING_TIME        ← decrements during movie record
   0xD240 SHUTTER_SPEED_EX
   0xD241 PIXEL_SIZE            ← older reference app label; latest catalog treats this as movie shutter speed
   0xD242 BATTERY_MULTI_STEPS   ← older reference app label; latest catalog maps this as movie ISO
   0xD246 DRIVE_MODE            ← ambiguous; latest catalog says movie resolution
```

**Why polling instead of subscriptions?** `SDK_SetCameraEvent` exists but reference app does NOT use
it for live-view properties. The camera does not push `DevicePropChanged (0x4006)` events
for the `0xD2xx` range on fw 2.30 — none observed in any of v6/v7/v8/v9 captures. **Polling
is the only way** to get current property values.

**Polling rate guidance for iOS:** reference app polls at 3.6 Hz. iOS can poll at 1 Hz with negligible
user-visible lag for most overlay state, or off if not displaying live camera state. Do not
try to replace with event subscriptions on fw 2.30.

### 3.5 Function-mode pseudo-properties (`0xDF00–0xDF31`)

Used by the §2.2 handshake. These behave like properties on the wire but are session-mode /
feature-version negotiators, not on-screen values.

| Wire | reference app constant | Decimal | Role |
|---|---|---|---|
| `0xDF00` | (unnamed) | — | Outer mode select (`SetCameraFunctionMode`) |
| `0xDF01` | (unnamed) | — | Inner mode select (`SetFunctionMode`) |
| `0xDF21` | `Fpcsh_VersionPhotoReceive` | 57121 | Photo-receive feature version |
| `0xDF22` | `Fpcsh_VersionPhotoView` | 57122 | Photo-view feature version |
| `0xDF24` | `Fpcsh_VersionRemote` | 57124 | Basic remote feature version |
| `0xDF25` | `Fpcsh_Version_RemotePhotoView` | 57125 | Remote photo view feature version |
| `0xDF27` | `Fpcsh_VersionFirmwareDataTransfer` | 57127 | Firmware-data-transfer feature version |
| `0xDF28` | `Fpcsh_VersionRemotePhotoViewEx` | 57128 | Extended remote photo view (image import) feature version |
| `0xDF29` | `FPCSH_VERSION_RESERVED_PHOTO_RECEIVED_EX` | 57129 | Reserved photo-receive (auto-import) feature version |
| `0xDF2A` | `Fpcsh_VersionRemoteEx` | 57130 | **Extended remote shooting (live view + take) feature version** |
| `0xDF31` | `Fpcsh_VersionGPSSet` | 57137 | GPS-set feature version |

**Version-mode values** (the integer written to a `Fpcsh_*` propcode):

```
VERSION_MODE_FIRMWARE_DATA_TRANSFER  = 1   // fw update uses version 1
VERSION_MODE_REMOTE_PHOTE_RAW        = 3   // RAW-capable photo viewer at version 3
VERSION_MODE_RESERVED_PHOTO_RECEIVE  = 3   // auto-import at version 3
VERSION_MODE_REMOTE_EX               = 4   // reference app's max-supported live-view version
```

### 3.6 Fuji vendor properties reference app queries but does NOT name

Observed on the wire but without a `CameraProperty.*` alias. Reachable via standard
`GetDevicePropValue` / `SetDevicePropValue`:

| Wire | Best guess | Notes |
|---|---|---|
| `0xD226` | image filter / sort mode | Written = 0 during image-import setup |
| `0xD227` | image filter / sort mode | Written = 0 during image-import setup |
| `0xD22B`–`0xD22E` | more remaining-* counters? | Sandwiched between REMAINING_SHEET/TIME (0xD229, 0xD22A) |
| `0xD235` | unknown | Observed 2× |
| `0xD244` | shutter-related sub-setting? | Between CURRENT_SHUTTER_TYPE (0xD243) and AF_SELECT_MODE (0xD245) |
| `0xD620` | GFX-specific (high range; not in X-T/X-H strings) | Observed 2× |
| `0xD621` | GFX-specific | Observed 2× |

### 3.7 Search-mode property family (`0xD600–0xD605`)

For the "search images on card" feature (date / folder / format / rating filtering):

```
FPCSH_SEACH_MODE_TIMEZONE      = 0xD600  ← reference app typo: SEACH → SEARCH
FPCSH_SEACH_MODE_DATE          = 0xD601
FPCSH_SEACH_MODE_FOLDER        = 0xD602
FPCSH_SEACH_MODE_RATING        = 0xD603
FPCSH_SEACH_MODE_OBJECT_FORMAT = 0xD604
FPCSH_SEACH_MODE_SUBJECT       = 0xD605
```

### 3.8 Other shorthand

```
FPCSH_CURRENT_BODY_FIRMWARE_FILENAME = 0xD22F  ← read camera-side current fw filename
```

### 3.9 Live-view stream-config properties (not used by reference app; tuned by XLV Linux backend)

Five DPCs control the body's JPEG live-view stream output. reference app never reads or writes these
— the camera's own XLV Linux backend (running on the camera's Linux co-SoC) sets them at
boot from `xlv_settings_org.yaml`, and once the stream is open they govern everything you
receive on port 55742 (frame size, quality preset, aspect handling, and which command path
the body uses to produce frames). The host (client application / a tether app) MAY override them
between `OpenSession` and the through-channel open if it wants a different stream than
reference app's defaults.

| Wire | Fuji name | R/W | Type | Form | Role |
|---|---|---|---|---|---|
| `0xD01B` | `Fpcsh_LiveZoom` | RW | UINT16 | enum {1..4} | live-view image zoom factor |
| `0xD173` | `Fpcsh_LiveImage_Quality` | RW | UINT16 | enum {1,2,3} | JPEG quality preset |
| `0xD174` | `Fpcsh_LiveViewImageSize` | RW | UINT16 | enum {1,2,3} | JPEG resolution class |
| `0xD1BC` | `Fpcsh_LiveViewMode` | **W-only** | UINT16 | enum {1,2} | command-path selector |
| `0xD23C` | `Fpcsh_LiveViewImageRatio` | RW | UINT16 | enum {1,2} | aspect-ratio handling |

Full value semantics + recipe in §4.1.1.

---

## 4. Live view (RemoteShooting mode)

### 4.1 Entering live view

Per §2.2, with operation-specific values:

```
1. InitCommandRequest                                       // 55740 connect + handshake
2. OpenSession(0x1002)
3. SetDevicePropValue(0xDF00, 6)       // outer = SDK_MODE_NEUTRAL20
4. SetDevicePropValue(0xDF01, 22)      // inner = SDK_MODE_IMAGE_LIVE_VIEW
5. GetDevicePropValue(0xDF2A)          // GetFunctionVersion(Fpcsh_VersionRemoteEx)
                                       //   camera returns its max supported version
6. SetDevicePropValue(0xDF2A, min(camera_max, 4))  // SetFunctionVersion to negotiated value
7. Open TCP 55741 (event channel) — passive listen
8. Open TCP 55742 (through-picture channel) — passive listen
9. Begin polling 0xD212 on 55740 at ~1-3 Hz (your choice; reference app does 3.6 Hz)
   Begin reading frames on 55742 (60 fps source)
   Begin reading events on 55741 (sparse)
```

The BLE prerequisite — writing `FUNCTION_LAUNCH_REQUEST = 0x0004` (RemoteShooting) and
waiting for `AP_STATE = 0x8001 Launched` — is documented in
`BLE_WIFI_READ_THIS_FIRST.md` §8 and is out of scope for this doc.

### 4.1.1 Tuning the live-view stream (quality / size / mode / ratio)

Optional. The body has sensible defaults baked in via `xlv_settings_org.yaml`, so a tether
app that doesn't `SetDevicePropValue` any of these still gets a working 640×480 stream at
"new command" mode with variable aspect ratio. Set them only if you need to change the
trade-off between bandwidth, latency, and resolution. **Best placement: between step 6
(SetFunctionVersion) and step 7 (open 55741) in §4.1.** Setting after the through-channel
opens may or may not take effect on the next frame — untested.

#### `0xD173 Fpcsh_LiveImage_Quality` — JPEG quality preset

3-value enum (UINT16). Read-write.

| Value | Meaning | Effect |
|---|---|---|
| `0x0001` | **FINE** | Highest JPEG quality; largest per-frame bytes (~18-22 KB at 640×480). Higher peak bandwidth, lower compression artifacts. |
| `0x0002` | (NORMAL — implied between FINE and BASIC; not labeled in source YAML) | Mid-quality preset. |
| `0x0003` | **BASIC** | Lowest JPEG quality; smallest per-frame bytes (~8-12 KB at 640×480). Lower bandwidth, more compression artifacts. |

- XLV's per-run default: `live_view_default_image_quality: 0x0003` (BASIC).
- Observed in 2026-04-25 wire `/get`: `value = 3 (BASIC)`.
- Set with `SetDevicePropValue(0xD173, <1|2|3>)`.

#### `0xD174 Fpcsh_LiveViewImageSize` — JPEG resolution class

3-value enum (UINT16). Read-write.

| Value | Meaning | Approx pixel dimension |
|---|---|---|
| `0x0001` | **L** | 1024-class (probably 1024×?? at the body's preferred aspect) |
| `0x0002` | **M** | 640-class |
| `0x0003` | **S** | 320-class |

- XLV's per-priority preset picks one of two values:
  - `live_view_image_size_image_quality_priority: 0x0001` (use L when prioritizing quality)
  - `live_view_image_size_real_time_priority: 0x0002` (use M when prioritizing latency)
- XLV's stub default: `0x0003` (S/320). Observed in 2026-04-25 wire `/get`: `value = 1 (L)`.
- Set with `SetDevicePropValue(0xD174, <1|2|3>)`.
- Exact pixel-dimension verification (1024×768 vs 1024×684 etc.) is **not yet captured on
  the wire** — only the size-class labels. Read the first JPEG frame and inspect SOI dims to
  confirm.

#### `0xD1BC Fpcsh_LiveViewMode` — command-path selector (write-only)

2-value enum (UINT16). **Write-only on fw 2.30** — `/get` returns response code `cpr=8194`
(`Operation_Not_Supported`) in all observed sweeps; `/set` is accepted.

| Value | Meaning |
|---|---|
| `0x0001` | Legacy command path (旧コマンド) — older X-T3/X-T30 / X-H1 era frame-generation flow |
| `0x0002` | New command path (新コマンド) — reference app 2.7.3 and XLV use this on GFX100 II fw 2.30 |

- XLV's runtime default: `live_view_mode: 0x0002` (new command).
- Set with `SetDevicePropValue(0xD1BC, 0x0002)` before opening port 55742. Don't bother
  reading it back — the camera will return an error.
- Probably what `Fpcsh_VersionRemoteEx` (§3.5 `0xDF2A`) implicitly negotiates via the
  version handshake; setting `0xD1BC` explicitly is belt-and-suspenders.

#### `0xD23C Fpcsh_LiveViewImageRatio` — aspect-ratio handling

2-value enum (UINT16). Read-write.

| Value | Meaning |
|---|---|
| `0x0001` | **Fixed ratio with black padding** (比率固定 — 差分は黒埋め) — frame always matches the sensor's native aspect; padded with black bars if the requested size has a different aspect. |
| `0x0002` | **Variable ratio** (比率可変 — サイズで変化) — frame aspect varies based on the selected size class; no padding. |

- XLV's runtime default: `live_view_ratio: 0x0002` (variable).
- Set with `SetDevicePropValue(0xD23C, <1|2>)`.

#### `0xD01B Fpcsh_LiveZoom` — live-view image zoom factor

4-value enum (UINT16). Read-write. Range `{0x0001, 0x0002, 0x0003, 0x0004}`. Used for
host-driven pinch-zoom during live view. Default = 0x0001 (no zoom).

#### Recipe: low-bandwidth tether (e.g. unstable 2.4 GHz link)

```
SetDevicePropValue(0xD1BC, 0x0002)   // new command path
SetDevicePropValue(0xD173, 0x0003)   // BASIC quality
SetDevicePropValue(0xD174, 0x0003)   // S (320-class) size
SetDevicePropValue(0xD23C, 0x0002)   // variable ratio (no black bars)
// then open 55741, 55742, begin polling 0xD212
```

Expected per-frame size: ~3-5 KB at 320×240 BASIC, sustained bandwidth ~200-300 KB/s.

#### Recipe: high-quality tether (5 GHz link, latency tolerant)

```
SetDevicePropValue(0xD1BC, 0x0002)   // new command path
SetDevicePropValue(0xD173, 0x0001)   // FINE quality
SetDevicePropValue(0xD174, 0x0001)   // L (1024-class) size
SetDevicePropValue(0xD23C, 0x0001)   // fixed ratio (matches sensor aspect)
// then open 55741, 55742, begin polling 0xD212
```

Expected per-frame size: ~40-80 KB at ~1024×768 FINE, sustained bandwidth ~2-5 MB/s.

#### Two settings NOT exposed via PTP DPC (yet)

The camera firmware has two more PCSH-namespace live-view settings that don't have PTP DPC
representations:

- `UI_SETP_PCSH_LIVEVIEW_TYPE` — values `{FF, NORMAL}` — likely "full-frame vs normal"
  rendering type.
- `UI_SETP_PCSH_LIVEVIEW_DRAWBLACK` — values `{OFF, ON}` — likely "draw black bars when
  cropping" toggle.

Both are firmware-internal `UI_SETP` IDs (2727 and 2728 in `ffun_enums.json`), not in any
XLV catalog, and the full D000-D3FF brute sweep found zero unmapped responding opcodes
(every responding propcode in that range is already in `property_code_def.py`). The
realistic wire path for these two is the `SaveSettings` → edit byte → `RestoreSettings` flow
(see §"BackupSettings .dat" — TODO if client application needs them; same approach as the Wi-Fi band
byte at .dat offset 0x052d).

### 4.2 Live-view JPEG stream (port 55742)

See §1.3 for the 14-byte stream header + JPEG body layout. Key facts:

- **Resolution:** 640×480 baseline JPEG, ~13.5 KB median (range 8.8–22.5 KB)
- **Source rate:** ~60 fps; reference app displays at 30 fps
- **Bandwidth:** ~810 KB/s sustained = ~6.5 Mbit/s (about half a 2.4 GHz link)
- **Natural gaps:** ~1.2 s pause every ~13 s; not events; tolerate ≥ 3 s receive inactivity
- **Frame counter at offset 0x08 is NOT consumed by reference app** — diagnostic only; safe to ignore
- **Field at offset 0x0c IS consumed** as a JPEG-body-offset adjust; always 0 on fw 2.30
  but reserved for future per-frame metadata insertion

iOS receive sketch:

```swift
func readLiveViewStream(socket: NWConnection) async throws -> AsyncStream<UIImage> {
    AsyncStream { continuation in
        Task {
            while !Task.isCancelled {
                let prefix = try await socket.read(exactly: 4)
                let totalLen = UInt32(littleEndian: prefix.load(as: UInt32.self))
                let payload = try await socket.read(exactly: Int(totalLen) - 4)
                let offsetAdjust = UInt32(littleEndian:
                    payload.load(fromByteOffset: 8, as: UInt32.self))
                let jpegStart = 14 + Int(offsetAdjust)
                guard payload.count > jpegStart else { continue }
                let jpegData = payload[jpegStart...]
                if let img = UIImage(data: jpegData) { continuation.yield(img) }
            }
            continuation.finish()
        }
    }
}
```

### 4.3 Event channel (port 55741)

Sparse — typically 0-2 events per minute of normal live view, more during capture cycles.
Wire format = 4-byte length prefix `1c 00 00 00` followed by 24-byte event body:

```
offset 0x00  uint16 LE  packet_type     (always 0x0004 Event)
offset 0x02  uint16 LE  event_code
offset 0x04  uint32 LE  tid             (typically 1)
offset 0x08  uint32 LE  param1
offset 0x0c  uint32 LE  param2
offset 0x10  uint32 LE  param3
offset 0x14  uint32 LE  param4
```




| event_code | Internal kind | Meaning | Params semantics |
|---|---|---|---|
| `0x4002` | (inline in PropList) | ObjectAdded | (objectHandle, …) — fires after capture |
| `0x4008` | 41 (`DEVICEINFOCHANGED`) | DeviceInfoChanged | (no useful params) |
| `0x400D` | 31 (`CAPTURE_COMPLETE`) | CaptureComplete | (savedObjectHandle, savedObjectHandle echo, 0, 0) |
| `0xC001` | 32 (`POSTVIEW_COMPLETE`) | Fuji vendor — object commit | (objectHandle, objectHandle echo, **fileSizeBytes**, 0) |
| `0xC004` | 30 (`CAPTURE_START`) | Fuji vendor — capture pending | (txnId, txnId echo, 0, 0) |
| `0xC005` | 43 (`AFCAPTUER`, sic) | **Phone-initiated AF complete** — see §5 | (afJobId, afJobId echo, 0, 0) |
| `0xC006` | (inline in PropList) | Fuji vendor — prop-list change | — |

Everything else (standard PTP `0x4003` ObjectRemoved, `0x4006` DevicePropChanged, `0x4007`
ObjectInfoChanged, `0x400A` StoreFull, `0x400B` DeviceReset, `0x400C` StorageInfoChanged,
`0x400E` UnreportedStatus, etc.) is logged in reference app but **not dispatched to handlers** — fw
2.30 simply doesn't emit them.

**Important:** `0xC005 AFCAPTUER` fires only in response to **phone-initiated** AF (your
`0x9026 LockS1Lock`, see §5). Physical half-press of the camera body's shutter button does
**NOT** generate any event on 55741. The camera does not signal body-side AF activity to
the app at all.

### 4.4 Aperture control during live view

Two ways to change F-number:

| Method | Wire | When to use |
|---|---|---|
| **Direct set** | `SetDevicePropValue(0x5007, F*100)` | Historical/static path only; newer live work showed ACK-but-ignored behavior on real hardware |
| **Step (camera-managed)** | Vendor `0x902D StepFnumber` with a `0`/`1` direction enum | client application live-view HUD arrows, matching reference app's captured aperture path |

Companion step opcodes (same shape, different property):

```
0x902C  StepShutterspeed    (param1 = direction in the live-view HUD path)
0x902D  StepFnumber         (param1 = direction)
0x902E  StepExposureBias    (param1 = direction)
```

Reading the current F-number: it's in the `0xD212` poll response (entry `0x5007`). No need
for a separate request.

### 4.5 Aperture range / valid values

`GetDevicePropDesc(0x1014, 0x5007)` returns the per-lens valid F-number list, but reference app does
not call this for the live-view UI — it relies on the camera's StepFnumber to advance only
through valid values. iOS can either:
1. Use Step + read back via 0xD212 (simple, ~250 ms per step)
2. Request `GetDevicePropDesc(0x5007)` as evidence only; do not expose it as a
   direct-set picker until a future body/run proves direct writes apply.

### 4.6 Exiting live view

```
TerminateOpenCapture(0x1018) — if any capture session is open (param = its tid)
CloseSession(0x1003)
Close TCP 55742 and 55741
Close TCP 55740
```

On the BLE side, you may also want to write `FUNCTION_LAUNCH_REQUEST = 0x0000 None` to tell
the camera to tear down the AP.

---

## 5. AF tap (LockS1Lock) — wire protocol

When the user taps inside the live-view view to "tap to AF". reference app issues a vendor PTP-IP
opcode and the camera reports the AF result through two channels: an async event on 55741
**and** a property-value flip visible in the next 0xD212 poll. **The AF box color the user
sees is driven by the property poll, not the event.**

### 5.1 The vendor opcode

| Wire | Name | Param | Effect |
|---|---|---|---|
| `0x9026` | LockS1Lock (tap-to-AF) | 4 bytes: encoded AF area | Camera moves AF area, begins focusing, fires `0xC005 AFCAPTUER` when done |
| `0x9027` | UnlockS1Lock (likely; release AF) | TBD | Camera clears AF lock, resets `0xD209 S1_LOCK_COLOR` to 0 |

### 5.2 AF-area param byte layout

reference app's Java builds an 8-char hex string `"WWHHIIJJ"` from `(mAspWidth, mAspHeight, col, row)`
in `FinderView.execS1Lock`, parses it as a hex integer, passes to native, which writes the
lower 32 bits as little-endian uint32. So the wire bytes (LE order) unpack as:

```
offset 12 (J): row of grid cell, 1-indexed
offset 13 (I): col of grid cell, 1-indexed
offset 14 (H): aspect-ratio height numerator (e.g. 3 for 4:3)
offset 15 (W): aspect-ratio width numerator  (e.g. 4 for 4:3)
```

`(mAspWidth, mAspHeight)` come from the high bytes of property `0xD17C S1_LOCK`. Grid
dimensions (`mSplitWidth, mSplitHeight`) come from the second arg passed to `setS1Value`.
For GFX100 II in 4:3 mode we observed AF coordinates up to `col=9, row=6` — fine grid.

### 5.3 Full timeline

```
T+0ms     0xD17C S1_LOCK       = (old coords cached from prior tap)
          0xD209 S1_LOCK_COLOR = 0           (no AF in progress)

T+~280ms  User taps screen at new location
          Phone sends 0x9026 LockS1Lock with param 0x WW HH II JJ on 55740
          UI: draw AF box at tap location, color WHITE = "focusing"

T+~290ms  Next 0xD212 poll: 0xD17C updated to new coords; 0xD209 still 0

T+~1000ms Camera sends 0xC005 AFCAPTUER on 55741, params=(afJobId, afJobId, 0, 0)
          → "AF done"
          reference app's handler: state-machine cleanup (clears "tap-to-AF mode" flag).
            Does NOT change the box color directly.

T+~1010ms Next 0xD212 poll: 0xD209 S1_LOCK_COLOR flipped 0 → 1 (or 2 on failure)
          UI: change AF box color GREEN (1 = locked OK) or RED (2 = failed)
```

`setForcusColor()` mapping (`FinderView.java:402`):

```
COLOR == 0 → white (no AF active)
COLOR == 1 → green (LOCK, AF succeeded)
COLOR == 2 → red   (LOCK, AF failed)
```

### 5.4 Implementation note for iOS

The `0xC005` event params `(N, N, 0, 0)` are **camera-internal sequential AF-job IDs**, not
the AF coordinates echoed back. They are useful only as job-correlation tokens if you fire
multiple back-to-back `0x9026` requests; they do not tell you whether AF succeeded. For
success/fail, read `0xD209 S1_LOCK_COLOR` in the next property poll.

If your poll rate is slower than reference app's 3.6 Hz, the color change will lag — at 1 Hz, up to
~1 second between AFCAPTUER arrival and the box turning green. Users tolerate this fine.

### 5.5 Camera-side simulator stub

```
On receipt of opcode 0x9026 with 4-byte AF-area param:
  1. Bump internal AF transaction counter (sequential int starting ~1)
  2. Update 0xD17C S1_LOCK property to the new (asp_w, asp_h, col, row)
  3. Send OperationResponse (type=3) on 55740 with response_code=0x2001 OK
  4. After 500–1200 ms (simulating AF time):
       a. Update 0xD209 S1_LOCK_COLOR to 1 (success) or 2 (fail)
       b. Send Event packet on 55741:
            [4-byte length=0x1c][type=0x04][code=0xC005][tid=0x01]
            [counter LE][counter LE][8 zero bytes]
```

Observed AF response time across our 3 captured taps: 464 ms, 712 ms, 1239 ms — scene-difficulty dependent.

---

## 6. Still capture ("Take")

Live view opens one capture session with `InitiateOpenCapture(0x101C)`.
Each shutter press fires `InitiateCapture(0x100E)` inside that existing
session. `TerminateOpenCapture(0x1018)` releases the session when leaving
live view; it is not sent after every shutter press.

### 6.1 The live-view capture sequence

```
1. Live-view startup:
   InitiateOpenCapture (0x101C) tid=N   params=(StorageID=0, ObjectFormatCode=0)
   → "lock into remote-shutter mode" and keep 55741/55742 open

2. Each shutter press:
   InitiateCapture (0x100E) tid=M       params=(StorageID=0, ObjectFormatCode=0)
   → "fire the shutter now"
   M is independent of N.

3. Live-view teardown:
   TerminateOpenCapture (0x1018) tid=K  param=N
   → "release the lock"
   The param is the TID of step 1 — that's how the camera identifies which open-capture
   session to close.
```

All three are standard PTP opcodes. Wire shape examples:

```
1. InitiateOpenCapture request (20 bytes):
   14 00 00 00 01 00 1c 10 <tid LE> 00 00 00 00 00 00 00 00

2. InitiateCapture request (20 bytes):
   14 00 00 00 01 00 0e 10 <tid LE> 00 00 00 00 00 00 00 00

3. TerminateOpenCapture request (16 bytes):
   10 00 00 00 01 00 18 10 <tid LE> <openCapture_tid LE>
```

`StorageID=0` and `ObjectFormatCode=0` are observed in every reference app Take — they mean "camera
decides which slot and what format based on its current settings."

### 6.2 Event lifecycle on 55741

Each Take generates exactly 3 events in ~800 ms:

| T+ | event_code | params | Meaning |
|---|---|---|---|
| 0 | `0xC004` CAPTURE_START | (baseHandle, baseHandle, 0, 0) | Shutter triggered |
| ~400 ms | `0xC001` POSTVIEW_COMPLETE | (baseHandle, baseHandle, **fileSizeBytes**, 0) | JPEG written to card |
| ~800 ms | `0x400D` CaptureComplete | (savedHandle, savedHandle, 0, 0) | Saved with ObjectHandle = savedHandle |

The `savedHandle` (in `0x400D`) is typically `baseHandle + 3` — the camera creates intermediate
objects (postview / RAW / JPEG slots) during capture and the saved handle is one of them.

`fileSizeBytes` in `0xC001`'s param[2] is the size of the saved image file (e.g., 9895 bytes
for a small JPEG in our captures). Useful for showing "image saved (10 KB)" toast.

### 6.3 Pacing

Observed inter-Take spacing in our v6 capture:

```
Open  →  Capture       : ~1.5 s (reference app UX delay)
Capture → Terminate    : ~5.0 s (reference app waits out the camera's processing + event lifecycle)
End → next Open        : depends on user
```

The reference app delays are UX/user choices, not camera requirements. Do not model a shutter press as
Open → Capture → Terminate → Reopen. The v6 capture shows multiple `InitiateCapture` requests
inside one open-capture session, and fw 2.30 resets replacement 55741/55742 sockets if client application
terminates/reopens them after each shutter.

### 6.4 Sibling step opcodes (no Take, just adjust)

For non-take adjustments visible to the user (turn the aperture ring, etc.):

| Wire | Name | Params |
|---|---|---|
| `0x902C` | StepShutterspeed | direction |
| `0x902D` | StepFnumber | direction enum (`0` down / `1` up in client application, read back via `0xd212`) |
| `0x902E` | StepExposureBias | direction |

Read back the new value via the next `0xD212` poll (entries `0x5007` for F-number,
`0xD240` for shutter, `0x5010` for bias).

### 6.5 iOS Take sketch

```swift
let tidOpen = nextTid()
let tidTake = nextTid()

try await control.send(opRequest(opcode: 0x101C, tid: tidOpen,
                                 params: [UInt32(0), UInt32(0)]))    // InitiateOpenCapture
try await waitForResponse(tid: tidOpen)

try await control.send(opRequest(opcode: 0x100E, tid: tidTake,
                                 params: [UInt32(0), UInt32(0)]))    // InitiateCapture
try await waitForResponse(tid: tidTake)

// Wait for 0x400D CaptureComplete on 55741
let savedHandle = await event.waitFor(eventCode: 0x400D).param0

// savedHandle is now usable for GetObjectInfo / GetThumb / GetObject (§8)
// Keep tidOpen active and keep 55741/55742 connected until live-view teardown.
```

---


The Java/JNI side is decoded; the wire opcode and event traffic are NOT.

### 7.1 Mode detection

`PROPERTY_SHOOTING_MODE` (`0x500E ExposureProgramMode`) with value **`0x8003`** means
the camera is in movie/video mode. reference app uses this as a read-only sentinel to show the
record button.

Whether iOS can FLIP `0x500E` to `0x8003` remotely to switch modes is unverified. Most
likely the user must switch the camera body's mode dial physically.

### 7.2 Start / stop API (no params on Java side)

```



Java_SDK_TerminateMovieCapture(hCamera) → similar chain
```

UI flow at `CameraControlActivity.onClickMovieButton` (line 2183) toggles `mMovieShooting`
flag and fires the appropriate `RemoteAPICall` (`METHOD.START_MOVE` / `STOP_MOVE`).

### 7.3 What we don't know

- The PTP opcode emitted by `InitiateMovieCapture` — best guess one of the unobserved
  `0x90xx` vendor opcodes (could be `0x9028`, `0x9029`, `0x902A`, or a `0x905x` range)
- Whether the camera fires `0x400D CaptureComplete` (or a similar event) at end of record
- Whether the 55742 through-picture stream pauses, changes bitrate, or continues unchanged
  during record
- The exact role of `0xD2B7 PROPERTY_MOVIE_TRANSPARENTE_FRAME_INFO` (returned by
  `SDK_GetMovieTransparentFrameInfo`) — probably per-frame overlay metadata
- Whether the camera body's record button (physical) emits any wire signal (the
  `CHARACTERISTIC_FF_CAMERA_MOVIE_BUTTON` BLE characteristic exists but **is dead code in
  reference app 2.7.3** — never read or written)

### 7.4 iOS implementation scaffold (provisional, fill in after capture)

```swift
// Pre-condition: already in live view (mode 22)
guard await control.getDevicePropValue(0x500E) == 0x8003 else {
    return .notInMovieMode  // user must flip camera body dial
}

let rcStart = await control.sendInitiateMovieCapture()  // wire opcode TBD
guard rcStart == 0 else { return .startFailed }
ui.setRecIndicator(.recording)
// 0xD22A REMAINING_TIME decrements during recording — poll for countdown UI

let rcStop = await control.sendTerminateMovieCapture()
guard rcStop == 0 else { return .stopFailed }
ui.setRecIndicator(.idle)

// Probably: await CaptureComplete-like event with new ObjectHandle of the movie file
// Then use Image-Transfer (§8) to download it
```

---

## 8. Image transfer (download)

The user-initiated "Get" path. Different function mode from live view; uses standard PTP
opcodes.

### 8.1 Entering image-import mode

```
1. InitCommandRequest (if not already open) + OpenSession
2. SetDevicePropValue(0xDF01, 20)         // inner mode = 20 (image import)
3. SetDevicePropValue(0xDF28, 3)          // Fpcsh_VersionRemotePhotoViewEx version 3
4. SetDevicePropValue(0xD226, 0)          // unnamed; sort/filter mode reset
5. SetDevicePropValue(0xD227, 0)          // unnamed; sort/filter mode reset
6. Vendor 0x9054 with param 0x10000001    // current-object metadata/bootstrap
7. Vendor 0x9055 with param 0x10000001    // current-object thumbnail/bootstrap
8. Vendor 0x9050                         // folder/begin listing
9. Vendor 0x9053 with params 0, 0x7530    // date/page listing
10. GetDevicePropValue(0xD620)            // object count
11. GetDevicePropValue(0xD621)            // newest-first object handles
```

### 8.2 Enumerate library

Publish the `0xD621` handle list immediately. Fetch `GetObjectInfo` + `GetThumb` lazily
per visible/selected image; on-device client application testing on 2026-05-22 found the camera can
close the command socket during a long all-handles metadata sweep even after returning a
valid 217-handle list.

For each image that needs metadata/preview, paired `GetObjectInfo` + `GetThumb`:

```
GetObjectInfo (0x1008) request — 16 bytes:
   10 00 00 00 01 00 08 10 <tid LE> <ObjectHandle LE>

Response data — 150 bytes with standard PTP ObjectInfo struct following the framing:
   02 00 08 10 <tid LE>                  ← 8 bytes framing
   <142 bytes ObjectInfo>:
     uint32 LE  StorageID                 (e.g., 0x10000001)
     uint16 LE  ObjectFormat              (0x3801 = EXIF JPEG)
     uint16 LE  ProtectionStatus          (0 = unprotected)
     uint32 LE  ObjectCompressedSize      (bytes of full image)
     uint16 LE  ThumbFormat               (0xB901 = JPEG/EXIF thumb)
     uint32 LE  ThumbCompressedSize       (bytes of thumbnail)
     uint32 LE  ThumbPixWidth             (e.g., 640)
     uint32 LE  ThumbPixHeight            (e.g., 480)
     uint32 LE  ImagePixWidth             (e.g., 11648 for GFX100 II native)
     uint32 LE  ImagePixHeight            (e.g., 8736)
     uint32 LE  ImageBitDepth
     uint32 LE  ParentObject
     uint16 LE  AssociationType
     uint32 LE  AssociationDesc
     uint32 LE  SequenceNumber
     variable   Filename (length-prefixed UTF-16LE)
     variable   CaptureDate (length-prefixed)
     variable   ModificationDate
     variable   Keywords

GetThumb (0x100A) request — 16 bytes:
   10 00 00 00 01 00 0a 10 <tid LE> <ObjectHandle LE>

Response — JPEG thumbnail body verbatim (~8-11 KB for 640×480), followed by OpResponse OK.
```

ObjectHandles enumerate **newest-first** (camera decrements from highest). Observed rate:
~4.7 images/sec; ~210 ms per pair.

If `ObjectInfo.ThumbCompressedSize == 0`, skip the `GetThumb` call (some RAW-only files
have no JPEG thumb).

### 8.3 Download a specific image

`GetPartialObject (0x101B)` with 3 params:

```
GetPartialObject request — 24 bytes:
   18 00 00 00 01 00 1b 10 <tid LE>
   <ObjectHandle LE> <Offset LE> <MaxBytes LE>
```

**Two patterns observed:**

| Pattern | When | Calls | MaxBytes per call |
|---|---|---|---|
| Single chunk | File ≤ ~12 MB | 1 | requested full file size |
| Chunked | File > 12 MB | N | **12,582,880 (0xC00020 ≈ 12 MiB)** for full chunks; final chunk = remainder |

reference app's chunk size is `12,582,880` bytes (~12 MiB). Final chunk = `total_bytes - last_offset`.
Observed throughput: ~18 MB/s for chunked download. The camera does NOT advertise a
preferred chunk size; it accepts any MaxBytes the client passes.

### 8.4 iOS download sketch

```swift
let info = library[selectedHandle]
let chunkSize: UInt32 = 12_582_880
var offset: UInt32 = 0
var imageData = Data()
while offset < info.objectCompressedSize {
    let want = min(chunkSize, info.objectCompressedSize - offset)
    let chunk = await control.sendGetPartialObject(
        handle: selectedHandle, offset: offset, maxBytes: want)
    imageData.append(chunk)
    offset += UInt32(chunk.count)
    ui.updateProgress(offset, of: info.objectCompressedSize)
}
```

### 8.5 BLE auto-import path is different

The BLE characteristics `FILE_INFORMATION`, `FILE_PARTIAL_DATA`, `FILE_PARTIAL_SIZE`,
`FILE_TRANSFER_INDEX`, `FILE_TRANSFER_RESULT` etc. are for a separate **auto-import-over-BLE**
path that sends small previews in 120-byte chunks. Wi-Fi PTP-IP image-import (this section)
does NOT touch them. Different code path entirely.

---

## 9. Errors and retries

### 9.1 The retry sentinel: `4102 = SDK_ERRCODE_BUSY`

Any of `SetCameraFunctionMode`, `SetFunctionMode`, `GetFunctionVersion`, `SetFunctionVersion`
can return `4102 (0x1006)` when the camera is busy. reference app retries up to `RETRY_COUNT` (~5)
times with `RETRY_INTERVAL` (~1 s) between attempts. **iOS clients must mirror this** — without
retries, mode-set will fail on cold connects more often than not.

### 9.2 What NOT to retry

```
SDK_ERRCODE_TIMEOUT (8194)             — retry once; persistent timeout = teardown
SDK_ERRCODE_STORE_FULL (12290)         — surface "card full" to user; do not retry
SDK_ERRCODE_PROTECT_CARD (12291)       — surface "card protected" to user
SDK_ERRCODE_STORE_NOT_AVAILABLE (12293) — likely card ejected; reconnect
SDK_PTP_ACCESS_DENIED (49 / 0x0031)    — wrong mode; check function-mode handshake
```

### 9.3 Connection-loss handling

If the TCP 55740 socket closes unexpectedly:
1. The 55741 / 55742 sockets are about to close too — close them proactively
2. The camera-side session is torn down implicitly
3. Reconnect: TCP 55740 → InitCommandRequest → OpenSession → Function Mode handshake
4. If user was in live view: re-enter (mode 22), re-open 55741 + 55742

Don't try to "resume" — there is no resumption state in fw 2.30's PTP-IP.

---

## 10. iOS implementation cheat sheets

### 10.1 Minimum flow for "show live view + take photo + download"

```
1. BLE pair complete (see BLE_WIFI_READ_THIS_FIRST.md)
2. Write FUNCTION_LAUNCH_REQUEST = 0x0004 (RemoteShooting)
3. Wait for AP_STATE = 0x8001 Launched
4. Join camera Wi-Fi AP
5. TCP connect 192.168.0.1:55740 → InitCommandRequest → OpenSession
6. Function Mode handshake for live view:
     SetDevicePropValue(0xDF00, 6)
     SetDevicePropValue(0xDF01, 22)
     GetDevicePropValue(0xDF2A) → cameraVersion
     SetDevicePropValue(0xDF2A, min(cameraVersion, 4))
7. TCP connect 55741 (event channel) — passive read loop
8. TCP connect 55742 (JPEG stream) — passive read loop; tolerate ≥3s gaps
9. Poll GetDevicePropValue(0xD212) at 1 Hz → update UI overlay
10. User taps shutter:
     a. Reuse the current live-view open-capture TID from live-view startup; do not nest a second 0x101C
     b. opRequest(0x100E) — InitiateCapture
     c. await event 0x400D on 55741 → get savedObjectHandle
     d. Keep the current open-capture session and 55741/55742 sockets open
11. User taps "download":
     a. opRequest(0x1018) — TerminateOpenCapture if still open
     b. CloseSession; reconnect to import mode (or keep session and just switch mode)
     c. SetDevicePropValue(0xDF01, 20) — image-import mode
     d. SetDevicePropValue(0xDF28, 3) + 0xD226/0xD227/0x9054 setup
     e. GetObjectInfo + GetThumb to enumerate / preview
     f. GetPartialObject in 12 MB chunks to download
```

### 10.2 Mode-switching summary

| Goal | Inner mode (write to 0xDF01) | Version propcode |
|---|---|---|
| Live view + still capture | **22** | `0xDF2A` Fpcsh_VersionRemoteEx |
| Image import / Get | **20** | `0xDF28` Fpcsh_VersionRemotePhotoViewEx |
| Auto image receive | 21 | `0xDF29` FPCSH_VERSION_RESERVED_PHOTO_RECEIVED_EX |
| Firmware update | 19 | `0xDF27` Fpcsh_VersionFirmwareDataTransfer |

Always preceded by `SetDevicePropValue(0xDF00, 6)` (outer = NEUTRAL20).

---

## 11. What's not decoded

### 11.1 Vendor opcodes seen but not fully analyzed

| Opcode | Best guess from context |
|---|---|
| `0x9020`–`0x9022` | Get-path companions (GetStorageIDs / GetStorageInfo / GetNumObjects equivalents); seen once each in v6 |
| `0x9027` | Likely UnlockS1Lock (companion to `0x9026 LockS1Lock`); seen once in v6 — needs deliberate unlock test |
| `0x9028`–`0x902A` | Unknown; possible InitiateMovieCapture (§7) |
| `0x9050`, `0x9053`, `0x9054`, `0x9055`, `0x9060` | Get-path vendor ops. `0x9054(0x10000001)` returns current-object metadata/bootstrap; `0x9055(0x10000001)` returns current-object thumbnail/bootstrap; `0x9050`/`0x9053(0,0x7530)` gate `0xD620`/`0xD621`. |

### 11.2 Properties without confirmed names

`0xD226`, `0xD227`, `0xD22B`–`0xD22E`, `0xD235`, `0xD244`, `0xD620`–`0xD621` — see §3.6.

### 11.3 Movie record protocol

recording, record stop, download the saved file) would resolve essentially all the
unknowns. Capture plan exists in `MOVIE_RECORD_STATIC_KNOWLEDGE_2026-05-19.md`.

### 11.4 Camera-side activity that emits no wire signal

These camera-body actions are SILENT on the wire (verified on fw 2.30 GFX100 II):

- Physical half-press of the camera's shutter button
- Physical full-press of the camera's shutter button (when not in remote-shooting mode)
- Spinning the camera body's aperture ring / shutter dial / ISO dial
  - (the value changes are visible in the next `0xD212` poll but no event fires)
- Pressing the camera body's record button when reference app is in live view (unverified; capture plan in §7's companion doc)

If iOS needs to react to camera-body activity, **polling `0xD212` is the only way** —
there is no event subscription that delivers these.

### 11.5 Other bodies

Everything in this doc is verified against **GFX100 II firmware 2.30**. Most reference app-supported
bodies should behave similarly (the propcode catalog is hardcoded in reference app and identical
across cameras), but:

- Older bodies (X-T3, X-H1 era — see `BLE_WIFI_READ_THIS_FIRST.md` §4 for the LEGACY/RED
  distinction) may not expose all the `0xD2xx` vendor properties
- Newer firmware may set the `jpeg_offset_adjust` field (§1.3) to a non-zero value, prepending
  per-frame metadata before the JPEG SOI
- Bodies without GFX-large-sensor features won't have `0xD620` / `0xD621`
- Standard PTP propcodes (`0x5000–0x501F`) should work identically on all bodies

---

## 12. Where the forensics live

Detailed evidence trail — wire-byte dumps, capture analysis, per-protocol decode steps —

Specific reads:

| Doc | Covers | Why read it |
|---|---|---|
| `INIT_COMMAND_REQUEST_FW0230_STACK_LEAK_2026-05-18.md` | 82-byte handshake byte-by-byte + the OOB stack-leak bug | If you need to understand why the trailing 28 bytes don't matter |
| `INIT_COMMAND_REQUEST_FW0230_FIELD_PROVENANCE_2026-05-18.md` | Where each byte of InitCommandRequest comes from | For replay rigs / forensics |
| `LIVEVIEW_JPEG_STREAM_FW0230_2026-05-18.md` | Live-view JPEG framing, event channel, 0xD212 polling cost | Full byte-level detail for §4 |
| `INITIATE_CAPTURE_FW0230_2026-05-18.md` | The 3-op Take sequence with byte-perfect packets | Full byte-level detail for §6 |
| `IMAGE_TRANSFER_FW0230_2026-05-19.md` | Image enumerate + download decoded from v6 | Full byte-level detail for §8 |
| `V9_RESULTS_AFCAPTUER_HALFPRESS_2026-05-18.md` | Physical-half-press capture results, natural-gap analysis | Why `0xC005` is phone-initiated AF, not body-side; why iOS stream timeouts must be ≥ 3 s |
| `BLE_WIFI_READ_THIS_FIRST.md` | BLE pairing + Wi-Fi AP handover | Everything before the first PTP-IP packet |
| `PTP_PROPERTIES_REFERENCE.md` (this file's local twin) | Same content as this doc, local copy | Lab-local working copy |

Raw captures live at `~/fuji/lab/captures/` on the lab box:

- `app_real_run_fw0230_wirelevel_v6_20260518T114931Z` — Take cycles + image transfer
- `app_real_run_fw0230_aperture_v8_20260518T141230Z` — 14 aperture steps + live view
- `app_real_run_fw0230_v9_afcaptuer_halfpress_20260518T213623Z` — Physical half-press + tap-to-AF

Pre-extracted MJPEG streams (replayable for simulator/testing) at
`~/fuji/lab/captures/mjpeg_extracts/{v6,v9}/session_NN/` — per-frame JPEGs + concatenated
MJPEG + raw on-wire byte stream + timing CSV.



