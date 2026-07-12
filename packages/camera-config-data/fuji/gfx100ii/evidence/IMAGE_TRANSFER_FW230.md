# Image transfer (download) wire protocol — reference app 2.7.3 vs fw 2.30, GFX100 II

**Date:** 2026-05-19
**Capture:** v6 second session 11:53:40–11:54:38 (image-import flow within the v6 run dir)
**Companions:** `INITIATE_CAPTURE_FW0230_2026-05-18.md`, `LIVEVIEW_JPEG_STREAM_FW0230_2026-05-18.md`,
`../PTP_PROPERTIES_REFERENCE.md`

## TL;DR

reference app's "Get" / Photo-View / image-import flow uses **standard PTP 1.1 opcodes** over the existing
55740 control socket, after switching to **inner function mode 20** (image-import) with version 3
(`Fpcsh_VersionRemotePhotoViewEx = 0xDF28`). Per image, reference app pairs a `GetObjectInfo (0x1008)` →
`GetThumb (0x100A)` to enumerate. To actually download a selected image, it uses
`GetPartialObject (0x101B)` in **12 MB chunks**.

In v6 we captured:
- 122× `GetObjectInfo` + 118× `GetThumb` (enumerating 122 images; 4 had no JPEG thumb)
- 1× small-file `GetPartialObject` (1.17 MB single chunk)
- 30× chunked `GetPartialObject` for a 356 MB file (likely RAW or movie)


> **Lifecycle correction (2026-07-11, ptpsim #243/#244).** The earlier
> PTP-only reading incorrectly inferred that reference app kept the same camera-Wi-Fi
> association and merely redialed `:55740`. The complete v6 sidecar shows an
> orderly PTP/IP close, disassociation from the camera AP, a later BLE-side image
> import launch, camera-AP re-association and DHCP, then the fresh command
> session documented below. The PTP bootstrap, vendor-prime payloads, enumeration
> results, and transport teardown remain valid. The current manifest's
> Take-to-Get edge is being reconciled separately in #244.

## Captured cold-session enumeration and corrected outer lifecycle

The original "Open questions" below (how reference app learns the handle range; whether
`0x9054` is required) are now **resolved** by re-parsing the v6 outbound opcode
stream and cross-checking against on-device behavior (client application on a real GFX100 II
fw2.30 over the camera AP). The image-import PTP session is fresh: its
`InitCommandRequest` is byte-identical to the earlier session, transaction ids
restart at 1, and the cold mode-20 bootstrap follows `OpenSession`.

The full sidecar chronology resolves what the PTP stream alone could not. reference app
finished the old transport teardown, left the camera network, later ran its
BLE-to-Wi-Fi image-import establishment path, rejoined the camera AP, acquired
network readiness, and only then opened the new command socket. The v6 BLE payload
hooks did not capture the launch write itself; #244 records the supporting app
control-flow evidence and that remaining validation gap. Directly redialing after
the PTP close is therefore not a supported generic substitute for re-establishment.

**The old PTP/IP transport still closes orderly, including an 8-byte transport-close
packet.** The v6 capture's exact teardown on the old socket is:
`TerminateOpenCapture → CloseSession(0x1003)` → (read OK) → **`08 00 00 00 ff ff ff ff`**
(an 8-byte PTP-IP transport-close control packet: length=8, payload=0xffffffff).
The auxiliary 55741/55742 sockets close before the sentinel, which is sent only on
55740. These are transport teardown facts, not proof that the AP association or
command listener remains continuously reusable afterward.

reference app sends the sentinel, then performs `shutdown(SHUT_RDWR)` → `close()`
(plain FIN, no SO_LINGER/RST). On iOS, `NWConnection.cancel()` alone can emit a
RST and race the sentinel flush, which the camera reads as abnormal. A caller
reproducing this teardown must flush the final message and FIN before cancellation.
The camera does not reply to the sentinel and reference app does not read after it, so no
response drain is required.

The captured setup + enumeration sequence (on reference app's fresh session) is:

```
(prior session) TerminateOpenCapture(0x1018) → CloseSession(0x1003)
(new) InitCommandRequest (82-byte) → then:
tid=1   OpenSession(0x1002)            params=[1]
tid=2   GetDevicePropValue(0xD212)
tid=3   SetDevicePropValue(0xDF01)=20  inner mode = image import
tid=4   GetDevicePropValue(0xDF28)     READ before write (matters)
tid=5   SetDevicePropValue(0xDF28)=3   Fpcsh_VersionRemotePhotoViewEx version 3
tid=6   SetDevicePropValue(0xD226)=0
tid=7   SetDevicePropValue(0xD227)=0
tid=8   GetDevicePropValue(0xD244)
tid=9   0x9054(param=0x10000001)        current-object metadata/bootstrap
tid=10  0x9055(param=0x10000001)        current-object thumbnail/bootstrap
tid=11  0x9050()                        begin enumeration
tid=12  GetDevicePropValue(0xD212)
tid=13  GetDevicePropValue(0xD22B)
tid=14  0x9053(params=[0x0, 0x7530])    page params: offset=0, count=30000
tid=15  GetDevicePropValue(0xD212)
tid=16  GetDevicePropValue(0xD620)  →   OBJECT COUNT  (uint32 LE; v6: 0x83 = 131)
tid=17  GetDevicePropValue(0xD621)  →   HANDLE LIST   (see format below)
tid=18  GetObjectInfo(0x84) ...          enumerate per handle, newest-first
```

**`0xD620` (object count) response:** 4-byte uint32 LE. v6 returned `0x83` (131).

**`0xD621` (handle list) response:** uint32 LE count prefix, then `count`×uint32 LE
object handles, **descending / newest-first**. v6: `83000000 84000000 83000000
82000000 81000000 80000000 7f000000 ...` → count 0x83, handles 0x84,0x83,...,0x01
(4 + 131×4 = 528 bytes).

**On-device app note (2026-05-22):** client application on the real GFX100 II reaches
`imageImportModeEntered`, then `0xD620`/`0xD621` returns a valid library
(`reportedCount: 217, parsedHandles: 217`). The apparent "Transfer connected but
nothing loads" failure was in the app layer: refresh waited for a full
`GetObjectInfo` sweep before publishing any handles, and the camera later closed
the command socket during that sweep (`shortRead ... GetObjectInfo 0x1008 first
length`). The UI must publish the handle list immediately and fetch
`GetObjectInfo` lazily for the selected object/download path.

**Gate the Photos save on ObjectFormat.** Only save objects whose `ObjectFormat` is a
still image (`0x3801` EXIF JPEG, `0x3808`/`0x380B`/`0x3800`, `0xB103` RAF, `0xB105` HEIF)
or a movie to the Photos library. Handing a non-image object (e.g. format `0x3004`) to
`PHAssetCreationRequest(.photo)` fails with `PHPhotosError 3302 (invalidResource)`. The
client checks `FujiPTPIPObjectInfo.isPhotosCompatible` before saving; non-media objects
stay in-app only. (Surfaced via the vcam dev harness, whose first object is format
`0x3004`.)

**Setup-prop rejections must be tolerated, not fatal.** `0xDF28`, `0xD226`, `0xD227` are
advisory feature-version/filter negotiators. Some responders/bodies don't implement them
(the `~/git/vcam` responder returns `0x200A DeviceProp_Not_Supported` for `0xDF28` and
resets if the client then aborts). The client sends these best-effort: a property
rejection (operationRejected, response code) is logged and skipped — the response packet
is consumed so transactions stay in sync — and only a transport failure aborts. The real
GFX100 II fw2.30 accepts `0xDF28`; vcam does not. Verified in the simulator-vs-vcam dev
harness (FUJI_KAGE_DEV_CAMERA_DIRECT_HOST).

**SetDevicePropValue value widths (from the capture's data-phase bytes):**
`0xDF01` = uint16 `14 00` (=20); `0xDF28` = **uint32 `03 00 00 00`** (=3, NOT a single
byte); `0xD226` / `0xD227` = uint16 `00 00`. A wrong-width write (e.g. 1-byte `0xDF28`)
is accepted but poisons the session so the later `0x9054` select-storage is rejected
(echoed back as response code `0x9054`) — observed on device even on a clean
InCameraViewIng AP session.

**`0x9054`/`0x9055`/`0x9050`/`0x9053` ARE required on the real camera before `0xD620`.**
(Earlier notes here said to skip them — that was wrong, based on vcam not needing them.)
On a real GFX100 II fw2.30 cold InCameraViewIng session, `GetDevicePropValue(0xD620)`
**times out with no response** unless this vendor block runs first, in this order:
`0x9054(0x10000001)` (current-object metadata) → `0x9055(0x10000001)` (current-object
thumbnail) → `0x9050()` (folder/begin enumeration) → `0x9053(0, 0x7530)` (page params).
`~/git/fuji-remote` encodes the same
chain (`SDCARD_OBJECT_HANDLES_STEPS`): the handle list (`0xD620`/`0xD621`) is gated behind
"folder-and-dates" (`0x9050`/`0x9053`). The `0x9054` rejection seen earlier was specific to
the SWAP path (repurposing a live-view session); on a fresh InCameraViewIng session the
block is accepted. Send all four (tolerant of rejection so older bodies don't abort).
vcam doesn't implement them and answers `0xD620` directly, so it still works there — but
the real camera needs them. Setup is therefore: `0xDF01=20`, the `0xDF28`/`0xD226`/`0xD227`
writes, the `0x9054/0x9055/0x9050/0x9053` block, then `0xD620`/`0xD621` + GetObjectInfo/GetThumb.

vcam also names the props the capture docs left "Unknown": `0xDF00` = CameraState,
`0xDF01` = ClientState, `0xD620` = ObjectCount, `0xD621` = object handle list. Its
`ClientStates` enum gives `FUJI_MODE_REMOTE_IMG_VIEW_XAPP = 20`, confirming `0xDF01=20`.

**Critical corrections to earlier guesses:**

- reference app **never** sends `GetObjectHandles (0x1007)`. On fw2.30 the camera **rejects
  `0x1007` with `0x2005 Operation_Not_Supported`** (verified on device). Use
  `0xD620`/`0xD621` to get the count and handle list.
- `0x9054` is **required** but must be sent **in this exact sequence** — preceded by
  the `0xDF01`/`0xDF28`/`0xD226`/`0xD227` writes and the `0xD244` read, and **paired
  with `0x9055`** (both `param=0x10000001`), with `0x9050` after. In `~/git/fuji-remote`
  these are current-object metadata/thumbnail bootstrap calls, not storage-select calls.
  Sending `0x9054`
  standalone makes the camera **reject it (echoes `0x9054` as the response code)**,
  which then poisons the transaction sequence (`0x2009 Invalid_TransactionID` on the
  next op). Verified on device.
- The `GetObjectInfo` **response** struct uses **length-prefixed UTF-16LE** strings
  (PTP-spec), filename at offset 52. The fixed 1076-byte ASCII `FTL_PTP_OBJECT_INFO`

  direction only** (`0x100C SendObjectInfo`, restore flow) — do not use it to parse
  GetObjectInfo responses.

Source: re-parse of `frida_messages.jsonl` outbound stream in the v6 run dir (see
Companion artifacts); on-device evidence via the app's `/telemetry` connection events.


## Session setup — `Fpcsh_VersionRemotePhotoViewEx` handshake

reference app tears down any open-capture / live-view session, sends a fresh `InitCommandRequest`, and
brings up image-import mode in 8 PTP operations:

```
out 11:53:40.159  InitCommandRequest 82-byte handshake (same bytes as live-view session start)
out 11:53:40.497  OpenSession(0x1002)         tid=1   params=[0x00000001]
out 11:53:40.728  SetDevicePropValue(0x1016)  tid=3   prop=0xDF01 value=0x0014  ← inner mode = 20
out 11:53:41.063  SetDevicePropValue(0x1016)  tid=5   prop=0xDF28 value=0x03    ← Fpcsh_VersionRemotePhotoViewEx = version 3
out 11:53:41.521  SetDevicePropValue(0x1016)  tid=6   prop=0xD226 value=0       ← (unnamed; possibly storage filter)
out 11:53:41.534  SetDevicePropValue(0x1016)  tid=7   prop=0xD227 value=0       ← (unnamed; possibly sort/filter mode)
out 11:53:41.853  vendor 0x9054               tid=9   param=0x10000001          ← likely "select storage" (StorageID for SD slot 1)
                  [GetObjectInfo loop starts here ~900 ms later]
```

This matches `CameraConnectRepository.java:155` and `ImportImageModel.java:4409`:
```kotlin
CameraConnectModel.INSTANCE.setFunctionMode(
    /* functionMode */    20,                              // SDK_MODE for image import
    /* functionVersion */ Fpcsh_VersionRemotePhotoViewEx,   // 0xDF28
    /* versionApp */      3                                 // VERSION_MODE_REMOTE_PHOTE_RAW
)
```

The `0x9054` vendor opcode with param `0x10000001` is unconfirmed but the value pattern is
PTP-standard StorageID format (`0x10000001` = first SD slot). It only appears once per
import session, consistent with a "select storage to enumerate" call.

The exact byte layout for each setup write follows the Fuji compressed PTP-IP framing
(`PTP_PROPERTIES_REFERENCE.md` §"How to read this doc"):

```
out: SetDevicePropValue request (16 bytes)
     10 00 00 00      length = 16
     01 00            type = 1 (OpRequest)
     16 10            opcode = 0x1016 (SetDevicePropValue)
     <4 byte tid>
     <2 byte propcode LE> 00 00

out: SetDevicePropValue data phase (14 bytes)
     0e 00 00 00      length = 14
     02 00            type = 2 (Data)
     16 10            opcode echo
     <4 byte tid>
     <2 byte value LE>   ← the value being written
```

## Enumeration loop — `GetObjectInfo` + `GetThumb` per image

After setup, reference app walks the camera's image library in reverse-chronological order. For each
ObjectHandle (starting from the newest and decrementing), it issues a `GetObjectInfo` then
a `GetThumb`. Both are standard PTP opcodes with a single `ObjectHandle` parameter.

### Request shapes

```
GetObjectInfo request (16 bytes):
     10 00 00 00 01 00 08 10 <4 byte tid> <4 byte ObjectHandle LE>

GetThumb request (16 bytes):
     10 00 00 00 01 00 0a 10 <4 byte tid> <4 byte ObjectHandle LE>
```

### Response shape (GetObjectInfo)

Data-phase response containing a standard-PTP ObjectInfo struct. Observed size 150 bytes:

```
in: data response (150 bytes)
     02 00            type = 2 (Data)
     08 10            opcode echo (GetObjectInfo)
     <4 byte tid>
     <142 bytes ObjectInfo struct>:
        offset 0   uint32 LE    StorageID         (e.g., 0x10000001)
        offset 4   uint16 LE    ObjectFormat      (0x3801 = EXIF JPEG)
        offset 6   uint16 LE    ProtectionStatus  (0 = unprotected)
        offset 8   uint32 LE    ObjectCompressedSize  (bytes of full image)
        offset 12  uint16 LE    ThumbFormat       (0xB901 = JPEG/EXIF thumb)
        offset 14  uint32 LE    ThumbCompressedSize    (bytes of thumb)
        offset 18  uint32 LE    ThumbPixWidth     (e.g., 640)
        offset 22  uint32 LE    ThumbPixHeight    (e.g., 480)
        offset 26  uint32 LE    ImagePixWidth     (e.g., 11648 — GFX100 II native)
        offset 30  uint32 LE    ImagePixHeight    (e.g., 8736)
        offset 34  uint32 LE    ImageBitDepth
        offset 38  uint32 LE    ParentObject
        offset 42  uint16 LE    AssociationType
        offset 44  uint32 LE    AssociationDesc
        offset 48  uint32 LE    SequenceNumber
        offset 52  variable-len Filename (length-prefixed UTF-16LE)
        offset ... variable-len CaptureDate (length-prefixed)
        offset ... variable-len ModificationDate
        offset ... variable-len Keywords

in: response packet (12 bytes)
     0c 00 00 00      length = 12
     03 00            type = 3 (OpResponse)
     01 20            response_code = 0x2001 (OK)
     <4 byte tid>
```

### Empirical field-value tally (81 decoded ObjectInfo responses from v6)

Added 2026-05-25 after a full decode pass over the v6 capture's
GetObjectInfo data-phase payloads. This is what the gallery-enumeration
metadata channel ACTUALLY carries in practice (vs. what the struct
*could* carry per spec).

**ObjectFormat codes observed:**

| Code | Format | Count in v6 |
|---|---|---|
| `0x3801` | EXIF JPEG | ~70+ (most files; 167,936 B for S-FINE, 1,227,053 B for one larger JPEG) |
| `0xB103` | Fuji RAF (raw) | ~10 (110-117 MB each) |
| `0x300D` | Apple QuickTime / MOV (movie) | 1 (373 MB) |

**ThumbFormat codes observed:**

| Code | Format |
|---|---|
| `0xB901` | JPEG thumbnail (8-37 KB, 640×480) — all stills + RAFs |
| `0x3808` | Vendor "movie thumbnail" — only the MOV file (3.4 KB, 2912×2184) |

**ImagePixWidth × ImagePixHeight distinct values:**

| Width × Height | Notes |
|---|---|
| `11648 × 8736` | GFX100 II native (102 MP, full-sensor JPEG and RAF) |
| `2912 × 2184` | Small JPEGs (the S-size 1.2 MB JPEG) + MOV frame dims |

**ImageBitDepth:** always `0` across all 81 responses. Camera does not
populate.

**`ModificationDate` is ALWAYS EMPTY** (zero-length u16le string) across
all 81 responses. Fuji firmware fw 2.30 does not populate this field
even when stat'd files on the SD card would have a mtime. iOS/Android
clients should NOT rely on ModificationDate — only CaptureDate.

**`Keywords` field surprise — it's used for EXIF Orientation, not user tags:**

Across all 81 files, the `Keywords` field is ONLY ever one of three values:

| Value | What it means | Count in v6 |
|---|---|---|
| `Orientation:1` | normal / landscape (no rotation) | ~75 |
| `Orientation:6` | 90° clockwise (portrait orientation) | 1 |
| `''` (empty) | no orientation hint | 1 (the MOV file) |

So the `Keywords` field is **a structured EXIF Orientation hint**, NOT
user-supplied keywords or tags. The thumbnail JPEG is delivered
unrotated; the client uses `Orientation:N` to decide how to display it
in the gallery grid. EXIF Orientation values 1-8 follow the standard
EXIF spec (1 = top-left, 6 = right-top / rotate 90° CW, etc.).

**No Orientation value other than 1 or 6** was observed in this single
capture, but the full EXIF range is presumably possible (`Orientation:3`
= 180°, `Orientation:8` = 270° CW). A wider lab capture would confirm.

**`CaptureDate` format**: ISO-8601 short form with `T` separator and no
delimiters elsewhere. Example: `'20260518T115308'` = 2026-05-18 11:53:08
(the photo's actual capture time, not the gallery-enumeration time).
Always present, non-empty.

### What metadata is NOT delivered at enumeration

The gallery-enumeration step `GetObjectInfo` returns **only** the
fields above. NONE of the following are delivered at this step:

- **Camera settings**: aperture (F-number), shutter speed, ISO,
  exposure compensation, white balance, film simulation, focal length.
  All embedded in the JPEG/RAF EXIF block, only readable AFTER a full
  download via GetPartialObject.
- **GPS metadata**: lat/lon, altitude, GPS timestamp. EXIF-embedded
  (provided the camera has GPS data — the GFX100 II receives this
  via BLE from the phone if location-sync is enabled).
- **Star rating / picks / rejects / labels**. Fuji firmware supports
  in-camera rating but it is not surfaced in ObjectInfo. Might be
  accessible via a separate PTP DPC; not yet decoded.
- **Lens info**: lens model name, max aperture, IS state — EXIF-only.
- **Copyright / artist** — EXIF-only.
- **Movie duration, codec, bitrate, audio format** for MOV files —
  not in this struct.
- **Drive mode** (single shot vs burst), **file group** (e.g. bracket
  triplets, panorama), **face-detection metadata** — none of these.

**Implication for a client building a gallery UI**: thumbnails + capture
date + orientation + file size + dimensions + filename is the WHOLE
metadata budget at enumeration time. If the user wants to filter by
F-number, see settings, see GPS markers on a map, etc. — the client
must either (a) full-download the file and EXIF-parse, or (b) ask
Fuji-vendor opcodes that reference app's image-import flow does NOT use
(notably the `0x9054 GetExtensionObjectInfo` vendor variant, which
per the vcam-side RE returns a 52-byte fixed header + 4×256-byte ASCII
slots — those slots MAY carry settings/meta strings but no reference app wire
capture has been observed using 0x9054 for image-import).

### Concrete decoded example

First photo enumerated by reference app at v6 timestamp 11:53:42.766
(payload_00007238.bin):

```
StorageID            = 0x10000001  (SD slot 1)
ObjectFormat         = 0x3801      (EXIF JPEG)
ProtectionStatus     = 0           (unprotected)
ObjectCompressedSize = 167,936     (~164 KB; S-FINE JPEG)
ThumbFormat          = 0xB901      (JPEG thumb)
ThumbCompressedSize  = 8898
ThumbPix             = 640 × 480
ImagePix             = 11648 × 8736  (GFX100 II native)
ImageBitDepth        = 0
ParentObject         = 0
AssociationType      = 0
AssociationDesc      = 0
SequenceNumber       = 0
Filename             = "DSCF8225.JPG"
CaptureDate          = "20260518T115308"  (2026-05-18 11:53:08 UTC offset unknown)
ModificationDate     = ""  (empty)
Keywords             = "Orientation:1"  (no rotation)
```

The first MOV file enumerated (payload_00008303.bin):

```
StorageID            = 0x10000001
ObjectFormat         = 0x300D      (QuickTime MOV)
ObjectCompressedSize = 373,819,904  (~356 MB)
ThumbFormat          = 0x3808      (vendor movie-thumb)
ThumbCompressedSize  = 3388
ThumbPix             = 640 × 480
ImagePix             = 2912 × 2184
Filename             = "DSCF8103.MOV"
CaptureDate          = "20260425T111102"
Keywords             = ""  (no orientation hint)
```

### Response shape (GetThumb)

Data-phase response containing the JPEG thumbnail body verbatim:

```
in: data response (~8-11 KB depending on image)
     02 00            type = 2 (Data)
     0a 10            opcode echo (GetThumb)
     <4 byte tid>
     <JPEG thumb bytes — starts FFD8, ends FFD9>

in: response packet (12 bytes; same OK format as above)
```

### Enumeration timing & rate

| Metric | v6 observed |
|---|---|
| Images enumerated | 122 |
| Time elapsed | 11:53:42.766 → 11:54:09 = ~26 s |
| Rate | ~4.7 images/sec |
| Per-image round-trip | ~210 ms (GetObjectInfo response ~100 ms + GetThumb response ~110 ms) |
| Thumb size range | ~8.9 KB to ~11 KB (640×480 JPEG) |
| ObjectHandles seen | decrement from `0x84` (132) down — camera enumerates newest-first |

The 4 missing thumbs (122 GetObjectInfo, 118 GetThumb) are likely RAW-only files where the
camera reports no JPEG thumbnail available. reference app skips `GetThumb` if `ThumbCompressedSize == 0`.

## Download — `GetPartialObject` chunked

When the user selects an image to actually download, reference app uses `GetPartialObject (0x101B)`. The
request takes three uint32 LE params: ObjectHandle, Offset, MaxBytes.

### Request shape (24 bytes)

```
out: GetPartialObject request
     18 00 00 00          length = 24
     01 00                type = 1 (OpRequest)
     1b 10                opcode = 0x101B
     <4 byte tid>
     <4 byte ObjectHandle LE>
     <4 byte Offset LE>
     <4 byte MaxBytes LE>
```

### Two download patterns observed in v6

#### Pattern A: small file (single chunk)

```
out 11:54:10.223  tid=193  ObjectHandle=0x7c  Offset=0  MaxBytes=1227053
                          ↑ one call, requests the full file (~1.2 MB)
```

Used when the file size (from the prior `GetObjectInfo` ObjectCompressedSize) fits in one
request — likely the standard JPEG path for smaller files.

#### Pattern B: large file (chunked, 12 MB at a time)

```
out 11:54:11.013  tid=198  handle=0x04  offset=         0  MaxBytes=12582880
out 11:54:12.331  tid=199  handle=0x04  offset=  12582880  MaxBytes=12582880
out 11:54:13.204  tid=200  handle=0x04  offset=  25165760  MaxBytes=12582880
...
out 11:54:38.170  tid=227  handle=0x04  offset= 364903520  MaxBytes= 8916384   ← final chunk, partial
```

Total: 30 chunks. Sum: 364,903,520 + 8,916,384 = **373,819,904 bytes ≈ 356 MB**. That's a RAW
(GFX100 II RAF is typically 200-220 MB) or a movie file (4K/8K can easily be 300+ MB for
short clips).

Chunk size: `0xC00020 = 12,582,880 bytes ≈ 12 MiB`. **reference app hardcodes this as the chunk size**
for large-file downloads — this is the natural cap built into the import flow. The final
chunk requests only `(remaining_bytes)` instead of the full 12 MB.

Throughput: ~700 ms per 12 MB chunk = effective **~18 MB/s** over Wi-Fi. Matches GFX100 II AP
2.4 GHz Wi-Fi throughput (the camera's AP doesn't max out 5 GHz).

### Why the 12 MB chunk size?

We have not located a `CHUNK_SIZE = 12582880` constant in the reference app source, but the consistency
across 30 chunks confirms it's a fixed parameter, not a streaming negotiation. iOS clients
implementing import should use the same value to mirror reference app's behavior, but could probably
use any value the camera accepts; standard PTP doesn't constrain this.

## Session teardown

```
out 11:53:14.604  TerminateOpenCapture (0x1018)  tid=179  param=0x09  ← refs an earlier InitiateOpenCapture
out 11:53:15.996  CloseSession (0x1003)          tid=181
```

(Note: in v6 the teardown of the live-view session preceded the image-import setup. A clean
image-import-only session would skip the `TerminateOpenCapture` step.)

## What reference app does NOT use for image transfer

For completeness, these BLE characteristics are defined in `BTConstansKt.java` but are NOT
involved in the Wi-Fi PTP-IP image-import path observed here:

| BLE characteristic | UUID | Used for |
|---|---|---|
| `CHARACTERISTIC_FF_FILE_INFORMATION` | `C922AC69-…` | BLE-only auto-import (small image push over BLE) |
| `CHARACTERISTIC_FF_FILE_PARTIAL_DATA` | `AC0C799A-…` | BLE auto-import chunks (max 120 bytes per `FILE_PARTIAL_SIZE`) |
| `CHARACTERISTIC_FF_FILE_PARTIAL_SIZE` | `7F3400FE-…` | BLE auto-import |
| `CHARACTERISTIC_FF_FILE_TRANSFER_INDEX` | `051DD980-…` | BLE auto-import |
| `CHARACTERISTIC_FF_FILE_TRANSFER_RESULT` | `68052E8A-…` | BLE auto-import |
| `CHARACTERISTIC_FF_IMAGE_TRANSFER_SETTING` | `CAEDB497-…` | BLE config: which auto-transfer mode is selected |
| `CHARACTERISTIC_FF_IMAGE_TRANSFER_SETTING_EX` | `98934B2C-…` | BLE config: extended setting |
| `CHARACTERISTIC_FF_TRANSFER_STATE` | `BD17BA04-…` | BLE notify of import-state changes |

The BLE-only path uses 120-byte chunks (`FILE_PARTIAL_SIZE`) — workable for small preview JPEGs
but impractical for large files. The Wi-Fi PTP-IP path (this doc) is reference app's normal "Get" flow
when the user manually picks images.

## Image-transfer setting flags

Two related properties on the BLE side affect whether/how images are auto-pushed:

- `CHARACTERISTIC_FF_IMAGE_TRANSFER_SETTING` (UUID `CAEDB497-…`) — a single byte. Set to `0x01`
  before AP launch (this is the write at `CameraControlActivity.java:2390` and similar
  pre-AP-handover sites). Likely "enable Wi-Fi image transfer".
- `CHARACTERISTIC_FF_IMAGE_TRANSFER_SETTING_EX` (UUID `98934B2C-…`) — also a single byte.
  Capability-gated (reference app only writes it if `bImageTransferSettingEx` flag was set during
  `analyzeServices`). Newer-firmware variant.
- `HEIF_TRANSFER_NOT_SETTING = 0` and `RAF_TRANSFER_SETTING = 1` constants (BTConstansKt
  lines 102, 109) — values for these properties. `RAF_TRANSFER_SETTING=1` likely means
  "include RAW files in transferable set".

These are pre-conditions for the Wi-Fi PTP-IP image-import path, not byte-level details of
the transfer itself.

## iOS replay sketch

```swift
// Assumes BLE pair complete + Wi-Fi handover to camera AP done
// + TCP 55740 open and InitCommandRequest acked

// 1. Open PTP session
control.send(opRequest(opcode: 0x1002, tid: nextTid(), params: [UInt32(1)]))

// 2. Switch to image-import function mode
control.sendSetDevicePropValue(prop: 0xDF01, value: UInt16(20))            // inner mode = 20
control.sendSetDevicePropValue(prop: 0xDF28, value: UInt8(3))              // version 3
control.sendSetDevicePropValue(prop: 0xD226, value: UInt8(0))              // unknown filter
control.sendSetDevicePropValue(prop: 0xD227, value: UInt8(0))              // unknown filter
control.sendVendorOp(opcode: 0x9054, tid: nextTid(), params: [UInt32(0x10000001)])  // storage select

// 3. Enumerate library (camera assigns ObjectHandles; usually newest = highest)
//    reference app's pattern is to start from "the highest known handle" and decrement.
//    Alternative: send GetObjectHandles (0x1007) to enumerate first, then loop.
var handle: UInt32 = highestHandle
while handle > lowestHandle {
    let objInfo = control.sendGetObjectInfo(handle: handle)               // 0x1008
    let thumbJpeg = objInfo.thumbSize > 0
        ? control.sendGetThumb(handle: handle)                            // 0x100A
        : nil
    ui.appendThumbnail(handle: handle, info: objInfo, thumb: thumbJpeg)
    handle -= 1
}

// 4. User taps a specific image to download
let info = library[selectedHandle]
let totalBytes = info.objectCompressedSize
let chunkSize: UInt32 = 12_582_880   // reference app's chunk size; any value the camera accepts will work
var offset: UInt32 = 0
var imageData = Data()
while offset < totalBytes {
    let want = min(chunkSize, totalBytes - offset)
    let chunk = control.sendGetPartialObject(handle: selectedHandle, offset: offset, maxBytes: want)
    imageData.append(chunk)
    offset += UInt32(chunk.count)
    ui.updateProgress(offset, of: totalBytes)
}

// 5. Save to camera roll / app sandbox; done.
```

## Companion artifacts

- Run dir: `~/fuji/lab/captures/app_real_run_fw0230_wirelevel_v6_20260518T114931Z/`
- Per-event payload blobs: `frida_payload_blobs/payload_NNNNNNNN.bin`


## Open questions


- `0xD226` / `0xD227` properties — set to 0 during image-import setup. Likely filter / sort
  mode but unnamed in reference app source.
- `GetObjectHandles (0x1007)` — reference app did NOT use this in v6; it knew the handle range
  somehow without enumerating. Possibly via a vendor `GetNumObjects`-equivalent. Worth a
  fresh capture starting from cold (no cached handle range) to see if reference app asks first.
- Smaller chunk size handling — if a camera AP has lower throughput, does reference app fall back
  to smaller chunks? We saw a single 12 MB chunk size used throughout.
- `0xC001 POSTVIEW_COMPLETE` event with file-size param — fires on Take cycle's image-write
  events. Whether it ALSO fires when a downloaded transfer completes is uncertain (we did
  not see any 55741 events during the v6 download window).

---

## Update 2026-05-26: RAF vs JPEG vs MOV ObjectInfo decode (1441 samples from app_real_run_20260526T031004Z)

The 2026-05-25 capture session ran reference app through a 317-image gallery with all three file types present, producing **1,441 unique `0x1008 GetObjectInfo` responses**: 875 JPEG / 433 RAF / 91 MOV. Format distribution settles the per-file metadata question.

### Side-by-side comparison

| Field             | JPEG (0x3801)         | RAF (0xb103)          | MOV (0x300d)         |
|-------------------|-----------------------|-----------------------|----------------------|
| `declared_len`    | **150 B**             | **150 B**             | **122 B**            |
| `object_format`   | 0x3801                | 0xb103                | 0x300d               |
| `object_size`     | varies (kB-MB)        | ~100 MB (native compressed RAF) | varies (MB-GB) |
| `image_pix`       | 11648×8736 (101.8 MP) | 11648×8736            | 11648×8736           |
| `thumb_format`    | 0xb901 (JPEG thumb)   | 0xb901 (JPEG thumb)   | **0x3808 (TIFF thumb)** |
| `thumb_pix`       | 640×480               | 640×480               | **160×120**          |
| `filename`        | `DSCFnnnn.JPG`        | `DSCFnnnn.RAF`        | `DSCFnnnn.MOV`       |
| `capture_date`    | `YYYYMMDDTHHmmSS`     | `YYYYMMDDTHHmmSS`     | `YYYYMMDDTHHmmSS`    |
| `mod_date`        | (empty)               | (empty)               | (empty)              |
| `keywords`        | `Orientation:N`       | `Orientation:N`       | **(empty)**          |
| trailing bytes    | 0                     | 0                     | 0                    |

### Implementer takeaways

1. **One ObjectInfo parser handles all three formats.** RAF and JPEG share IDENTICAL 150-byte struct layout. MOV uses the 122-byte variant (no keywords field present after the fixed header), differs only in thumb metadata.

2. **The keywords field carries `Orientation:N` for stills, empty for movies.** No rating, subject, voice-memo, GPS, or color-profile data in any ObjectInfo response (verified across 1441 samples).

3. **No vendor-specific extension** trailing the standard struct. All 1441 samples had 0 bytes after the parsed strings — meaning Fuji does NOT extend the standard PTP ObjectInfo struct on this firmware. Any vendor metadata comes from a separate opcode/channel, not appended to ObjectInfo.

4. **`object_format` field IS the only reliable file-type indicator.** Don't rely on filename extension parsing — the format field is authoritative and 100% populated.

### 0x9054 GetExtensionObjectInfo and 0x9055 in actual usage

reference app also calls `0x9054 GetExtensionObjectInfo` and `0x9055`, but only sparingly — 5 calls each in the full 1.89M-line trace (vs 1441 standard 0x1008). Observed behavior:

- **0x9054** returns the same ObjectInfo struct prefixed with a 4-byte object handle (i.e., adds the missing handle field that standard 0x1008 omits). Used for handle-resolution scenarios.
- **0x9055** (named `getExtensionObjectSize` in reference app source `BTConstansKt.java` / `ControlFFIR.java`) actually returns **JPEG image bytes** (response body starts with `ff d8` SOI marker). The "size" name is misleading — this delivers preview/thumb JPEG data inline. Likely the path used for the in-gallery thumbnail rendering when 0x100A GetThumb isn't sufficient.

For a client application iOS impl: stick with 0x1008 + 0x100A + 0x101B for the main enumeration + thumb + download path. 0x9054/0x9055 can be ignored for v1.

### Cross-reference: where Subjects/Ratings data ACTUALLY lives

Confirmed in `GALLERY_FILTER_PROTOCOL_FW0230_2026-05-25.md`: Subjects (AI categories) and Ratings (star count) are stored in the camera-side `ffdb` sqlite and surfaced ONLY via the `0x9051/0x9052/0x9053` server-side filter triplet (the camera computes histograms; phone never sees per-file metadata). Voice memos (the body-attached audio annotation) are similarly invisible in ObjectInfo; their existence would need to be inferred from a separate associated-object lookup.
