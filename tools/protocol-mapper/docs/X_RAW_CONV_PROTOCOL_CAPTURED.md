


172MB / 20k pkts). This is the complete loop the app needs. Transport: PTP-over-USB (camera `04cb:02fe`,
RAW-CONV mode). Probe: `scripts/ptp_usb.py`.

## Sequence

### 1. Connect / ARM (clears Device_Busy — my minimal open→0x900c failed without this)
- `GetDeviceInfo` ×~9 (poll until ready)
- `OpenSession(1)`
- `GetStorageIDs` → 2 storages (0x10000001, 0x10000002); `GetStorageInfo`+`GetObjectHandles` each → 0 objects
- `GetPropVal 0xd16e`
- **`SetPropVal 0xd040 ← 0x00000001`**   ← arm flag
- `GetObjectHandles(0xffffffff)` → 0
- `GetPropVal 0xd20b, 0xd212` (status), `0xd184` (IOPCode "FA189501,…"), `0xd186`/`0xd187` ("GFX100 II_01")
- `GetPropVal 0xd36a`(=12)/`0xd36b`("100,0,0,…"); read `0xd18c..0xd1a5` cluster; **`SetPropVal 0xd18c ← 1`**
- `GetPropVal 0xd21c` then **`SetPropVal 0xd21c ← 0`** (PC-priority-ish, same as backup prelude)

### 2. Send RAF (host → camera)
- **`0x900c` V_SendObjectInfo**, params (0,0,0), 114-byte ObjectInfo:
  `StorageID=0 @0x00, ObjectFormat=0xf802 @0x04, ProtStatus=0 @0x06, ObjectCompressedSize @0x08,
   Filename PTP-string @0x34, rest 0`  (VALIDATED — camera accepted 0x2001)
- **`0x900d` V_SendObject** + the full RAF bytes (chunk the bulk write; 88–117MB)

### 3. Recipe (0xD185, 629-byte / 0x275 blob)
- `GetPropVal 0xD185` → default recipe (only valid AFTER a RAF is loaded; errors 0x2002 otherwise)
- **`SetPropVal 0xD185 ← <629-byte recipe>`**: header uint16 `0x001d`, byte[0x02]=string-count, IOPCode
  PTP-string ("FA189501…"), then 32-bit param table ~@0x201. Templates: `captures/xraw/d185_{default,
  modified}.bin`. Observed slot **+0x20d: 2→3** = the one param the user changed in this capture (slot→
  param map still needs per-param captures; param SET known from XRFC `Cap*`/`Set*` symbols).

### 4. Convert + retrieve
- **`SetPropVal 0xD183 ← 1`  = CONVERT TRIGGER**
- `GetObjectHandles(0xffffffff)` → now 1 handle (result obj, e.g. 0x00000002)
- `GetObjectInfo` → `GetObject(handle)` = **the developed JPEG/HEIF** (`ffd8ffe1…EXIF…`)
- `DeleteObject(handle)` (free the result)

Model-lock: camera advertises IOPCode `FA189501`/`GFX100 II_01`; only RAFs whose header matches
(`FUJIFILMCCD-RAW 0201FA189501GFX100 II`) convert — that's the `GetRejected*` logic.
