# Fuji RAW-conversion / desktop-USB PTP surface — protocol evidence



call-site inspection); none of these opcodes have been wire-captured against a
camera yet. Treat as leads pending golden fixtures.

evidence level, logging leads, follow-up plan) lives on the operator tree at

This file extracts the protocol-spec subset for the manifest pipeline.

> ⚠️ This is the **desktop USB** RAW path. It is NOT the mobile Wi-Fi PTP/IP path
> the `../fw0230.yaml` manifest models. Do not collapse the two flows (see
> "Desktop USB vs mobile Wi-Fi" below).

## Transport



`ICCameraDeviceDelegate`, `ICTransportTypeUSB`,
`requestSendPTPCommand:outData:sendCommandDelegate:didSendCommandSelector:contextInfo:`,
and `cameraDevice:didReceivePTPEvent:`. No raw-USB entitlement; a consumer would
start from public ImageCaptureCore rather than IOKit/ExternalAccessory.

## Low-level PTP surface


`OpenSession`, `GetStorageIDs`, `GetObjectHandles`, `GetObjectInfo`, `GetObject`,
`GetThumb`, `InitiateCapture`, `GetDevicePropDesc_{Int,Str}`,
`Get/Set/ResetDevicePropValue`, `InitiateOpenCapture`, `TerminateOpenCapture`,

lens, and backup workflows on top of `FTL_PTP_VendorExtensionOperation`.

`FTL_PTP_DATA_DIR`: `1` = camera→host (inbound), `2` = host→camera (outbound).

## Opcode leads (static call sites)

| Operation | Dir | Params | Payload | Static caller |
|---|---:|---:|---:|---|
| `0x1015` GetDevicePropValue | 1 | 1 | varies | RAW settings, buffer capacity, image/property reads |
| `0x1016` SetDevicePropValue | 2 | 1 | varies | RAW settings + property writes |
| `0x900c` vendor object info | 2 | 3 | `0x72` bytes (RAW-from-PC) | `CCameraCommandSendRAWFromPC::SendObjectInfo` |
| `0x900d` vendor object data | 2 | 0 | caller image/update buffer | `CCameraCommandSendRAWFromPC::SendObject` |
| `0x100c` SendObjectInfo-shaped backup write | 2 | 2 | `0x434` bytes | `CCameraCommandBackupSettings::ExecSetBackupSettingsInfo` |
| `0x100d` SendObject-shaped backup write | 2 | 1 | caller backup buffer | `CCameraCommandBackupSettings::ExecSetBackupSettings` |
| `0x1008` GetObjectInfo-shaped backup read | 1 | 1 | `0x434` bytes | `CCameraCommandBackupSettings::ExecGetBackupSettingsInfo` |
| `0x1009` GetObject-shaped backup read | 1 | 1 | caller backup buffer | `CCameraCommandBackupSettings::ExecGetBackupSettings` |

## RAW Settings property `0xd185`

- `CCameraCommandRAWSettings::ExecGetRAWSettings`: allocates a `0x275`-byte
  buffer, sets Param1 = `0xd185`, calls `VendorExtensionOperation(opcode=0x1015,
  dir=1, 1 param)`.
- `CCameraCommandRAWSettings::ExecSetRAWSettings`: builds a `0x275`-byte buffer,
  Param1 = `0xd185`, `VendorExtensionOperation(opcode=0x1016, dir=2, 1 param)`.
- The set path writes an initial `uint16` `0x001d`, stores an `IOPCode` string in
  PTP-string form, then copies many 32-bit RAW-conversion parameters into fixed
  offsets. **Exact field map needs a live capture / deeper structure recovery
  before it should be modeled.**

## Vendor object transfer `0x900c` / `0x900d` (RAW-from-PC)

- `SendObjectInfo`: builds a `0x72`-byte object-info payload, writes filename
  `UP_FILE.dat` (PTP string), copies the outbound object size in, calls
  `VendorExtensionOperation(opcode=0x900c, dir=2, 3 params)` (params zero-init in
  this call site). If a model condition returns `0x1381`, the object-format field
  is set to `0xf802`.
- `SendObject`: sends the caller's RAW image buffer via
  `VendorExtensionOperation(opcode=0x900d, dir=2, 0 params)`.
- Firmware/lens update classes reuse the same `0x900c`/`0x900d` shape; static
  inspection saw firmware object-info format variants around `0xb802`/`0xc802` —
  needs live validation.

## Desktop USB vs mobile Wi-Fi (do not collapse)

| Flow | Transport | Obj-info op | Obj-data op | Payload shape |
|---|---|---:|---:|---|
| reference app mobile firmware upload | Wi-Fi PTP/IP | `0x9040` | `0x9042` | capture-backed `839`-byte object info + chunked DAT |


The client application mobile firmware workflow keeps using the reference app-derived `0x9040` /
`0x9042` evidence for Wi-Fi PTP/IP transfer until a camera capture proves otherwise.
