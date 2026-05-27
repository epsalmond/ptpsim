#!/usr/bin/env python3
"""Object enumeration + >4 GiB transfer-boundary probe over USB-PTP (card-reader persona). READ-ONLY.

GetObjectHandles -> GetObjectInfo (format / size / name) per handle; flags the oversize object. Then
GetPartialObject (0x101B) small reads at offset 0 and near the 4 GiB boundary to test u32-offset reach
(reads a few bytes only — never a full GetObject/download). Answers: does a >4 GiB MOV report a real size
or the 0xFFFFFFFF ceiling, and can its bytes be addressed past 4 GiB over standard PTP?
"""
import struct

from ptp_usb import PTPUSB

GET_OBJECT_HANDLES = 0x1007
GET_OBJECT_INFO = 0x1008
GET_PARTIAL_OBJECT = 0x101B
U32_MAX = 0xFFFFFFFF
GIB4 = 0x100000000

FMT = {0x3000: "undef", 0x3001: "assoc/folder", 0x3008: "WAV", 0x300D: "MOV",
       0x3801: "JPEG", 0x3808: "MP4?", 0xB103: "RAF", 0xB982: "HEIF?"}


def rd_str(b, o):
    n = b[o]
    o += 1
    if n == 0:
        return "", o
    return b[o:o + 2 * n].decode("utf-16-le", "replace").rstrip("\x00"), o + 2 * n


def parse_oi(b):
    sid, fmt, _prot = struct.unpack_from("<IHH", b, 0)
    size = struct.unpack_from("<I", b, 8)[0]   # ObjectCompressedSize, u32 (the 4 GiB ceiling field)
    name, _ = rd_str(b, 52)                    # fixed ObjectInfo head = 52 bytes, then Filename
    return sid, fmt, size, name


def main():
    p = PTPUSB()
    p.open()
    data, rc = p._txn(GET_OBJECT_HANDLES, (U32_MAX, 0, 0))
    if rc != 0x2001 or len(data) < 4:
        ids, rcs = p._txn(0x1004)              # GetStorageIDs fallback
        sid0 = struct.unpack_from("<I", ids, 4)[0] if len(ids) >= 8 else U32_MAX
        data, rc = p._txn(GET_OBJECT_HANDLES, (sid0, 0, 0))
    (cnt,) = struct.unpack_from("<I", data, 0)
    handles = struct.unpack_from("<%dI" % cnt, data, 4)
    print("handles: %d" % cnt)

    rows = []
    for h in handles:
        oi, rc = p._txn(GET_OBJECT_INFO, (h,))
        if rc != 0x2001 or not oi:
            continue
        sid, fmt, size, name = parse_oi(oi)
        rows.append((h, fmt, size, name))

    rows.sort(key=lambda r: r[2], reverse=True)
    print("handle      fmt              size         name")
    for h, fmt, size, name in rows[:25]:
        sz = "0xFFFFFFFF*ceiling" if size == U32_MAX else "%d (%.2f GiB)" % (size, size / GIB4)
        print("0x%08X  %-15s  %-22s %s" % (h, FMT.get(fmt, "0x%04x" % fmt), sz, name))

    # Pick the oversize / largest object (the >4 GiB MOV) and probe the addressing boundary.
    target = next((r for r in rows if r[2] == U32_MAX), rows[0] if rows else None)
    if not target:
        return
    h, fmt, size, name = target
    print("\n== boundary probe on 0x%08X (%s, reported %s) ==" % (
        h, name, "0xFFFFFFFF ceiling" if size == U32_MAX else str(size)))
    for off in (0, 0x10000000, 0xF0000000, 0xFFFFF000):    # 0, 256MiB, ~3.75GiB, ~4GiB-4KiB
        d, rc = p._txn(GET_PARTIAL_OBJECT, (h, off, 16))
        print("  GetPartialObject off=0x%08X cnt=16 -> resp=0x%04x got=%dB" % (off, (rc or 0), len(d)))
    print("  note: GetPartialObject offset is u32 (max 0x%08X = 4 GiB-1); no 64-bit variant (0x95C1) "
          "advertised -> bytes beyond 4 GiB are unaddressable over standard PTP here." % U32_MAX)


if __name__ == "__main__":
    main()
