#!/usr/bin/env python3
"""Active-probe enumeration -> manifest-ingestion evidence (read-only).

B2: GetDeviceInfo -> DeviceVersion (manifest version key), OperationsSupported, DeviceProps count, Events.
B1: GetDevicePropDesc (0x1014) per supported prop -> dataType, getSet, FormFlag, default, current, enum/range.

This is an *active* fact source: the mapper sends the op and decodes the reply in-process, so it emits
ingestion-ready `camera-config-evidence/v1` fragments directly (no separate pcap/synthesis stage), each
scoped by (firmware, connection, mode) and carrying inline raw bytes as `kind: wire-capture` evidence.
Wire-observable fields only (type/access/descriptor/observed) — names/labels/controls stay curated downstream.

  --format {jsonl,tsv,both}   jsonl = ingestion fragments; tsv = human; both (default) = tsv to stderr
  --mode / --connection       scope tags for this run (operator declares the camera state)
  --run                       run id (default: UTC timestamp)
"""
import argparse
import datetime
import json
import struct
import sys

from ptp_usb import PTPUSB

GET_PROP_DESC = 0x1014

# PTP DataType -> (manifest-type token, struct fmt). 0xFFFF = PTP string; 0x0000 = opaque/undef.
DT = {
    0x0000: ("undef", None), 0x0001: ("i8", "<b"), 0x0002: ("u8", "<B"),
    0x0003: ("i16", "<h"), 0x0004: ("u16", "<H"), 0x0005: ("i32", "<i"),
    0x0006: ("u32", "<I"), 0x0007: ("i64", "<q"), 0x0008: ("u64", "<Q"),
    0xFFFF: ("str", None),
}


def type_name(dt):
    if dt in DT:
        return DT[dt][0]
    if 0x4001 <= dt <= 0x4008:
        return "u8a"          # PTP array datatypes -> opaque array
    return "0x%04x" % dt


def access_name(gs):
    return {0: "readOnly", 1: "readWrite"}.get(gs, "gs%d" % gs)


def rd_str(b, o):
    n = b[o]
    o += 1
    if n == 0:
        return "", o
    s = b[o:o + 2 * n].decode("utf-16-le", "replace").rstrip("\x00")
    return s, o + 2 * n


def rd_arr16(b, o):
    (cnt,) = struct.unpack_from("<I", b, o)
    o += 4
    vals = list(struct.unpack_from("<%dH" % cnt, b, o))
    return vals, o + 2 * cnt


def parse_deviceinfo(b):
    o = 2 + 4 + 2
    _vdesc, o = rd_str(b, o)
    o += 2
    ops, o = rd_arr16(b, o)
    evs, o = rd_arr16(b, o)
    props, o = rd_arr16(b, o)
    _capf, o = rd_arr16(b, o)
    _imgf, o = rd_arr16(b, o)
    manu, o = rd_str(b, o)
    model, o = rd_str(b, o)
    devver, o = rd_str(b, o)
    serial, o = rd_str(b, o)
    return dict(ops=ops, evs=evs, props=props, manu=manu, model=model, devver=devver, serial=serial)


def rd_val(b, o, dt):
    if dt == 0xFFFF:
        return rd_str(b, o)
    fmt = DT.get(dt, (None, None))[1]
    if fmt:
        return struct.unpack_from(fmt, b, o)[0], o + struct.calcsize(fmt)
    raise ValueError("unhandled datatype 0x%04x" % dt)


def parse_propdesc(b):
    code, dt, getset = struct.unpack_from("<HHB", b, 0)
    o = 5
    if dt == 0x0000:                       # Undefined datatype: opaque, no typed value/form
        return dict(code=code, dt=dt, getset=getset, default=None, current=None, form="none", detail=None)
    deflt, o = rd_val(b, o, dt)
    cur, o = rd_val(b, o, dt)
    form = b[o] if o < len(b) else 0
    o += 1
    detail = None
    fname = "none"
    if form == 1:
        mn, o = rd_val(b, o, dt)
        mx, o = rd_val(b, o, dt)
        st, o = rd_val(b, o, dt)
        fname = "range"
        detail = dict(min=mn, max=mx, step=st)
    elif form == 2:
        (cnt,) = struct.unpack_from("<H", b, o)
        o += 2
        vals = []
        for _ in range(cnt):
            v, o = rd_val(b, o, dt)
            vals.append(v)
        fname = "enum"
        detail = vals
    return dict(code=code, dt=dt, getset=getset, default=deflt, current=cur, form=fname, detail=detail)


def fmt_tsv(v):
    if isinstance(v, str):
        return v
    if isinstance(v, int):
        return "0x%x" % v
    return str(v)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--format", choices=["jsonl", "tsv", "both"], default="both")
    ap.add_argument("--mode", default="shooting/stills")
    ap.add_argument("--connection", default="usb")
    ap.add_argument("--run", default=None)
    ap.add_argument("--open-capture", action="store_true",
                    help="InitiateOpenCapture (0x101C) before enumerating to unlock capture-gated props; "
                         "TerminateOpenCapture + CloseSession after")
    args = ap.parse_args()
    run = args.run or datetime.datetime.utcnow().strftime("%Y%m%dT%H%M%SZ")
    ts = datetime.datetime.utcnow().strftime("%Y-%m-%dT%H:%M:%SZ")

    p = PTPUSB()
    p.open()
    d, _ = p.device_info()
    info = parse_deviceinfo(d)

    cap_tid = None
    if args.open_capture:
        _, rc101c = p._txn(0x101C, (0, 0))   # InitiateOpenCapture (StorageID=0, ObjectFormat=0)
        cap_tid = p.tid
        print("[101c InitiateOpenCapture] resp=0x%04x tid=%d" % ((rc101c or 0), cap_tid), file=sys.stderr)
    scope = dict(manufacturer=info["manu"], model=info["model"], firmware=info["devver"],
                 connection=args.connection, mode=args.mode)

    jsonl = args.format in ("jsonl", "both")
    tsv = args.format in ("tsv", "both")
    # In `both`, JSONL is the artifact (stdout) and TSV/summary is human aside (stderr); else pick one.
    jout = sys.stdout
    hout = sys.stderr if args.format == "both" else sys.stdout

    def ev(op):
        return dict(kind="wire-capture", op=op, run=run, ts=ts)

    def emit(obj):
        if jsonl:
            jout.write(json.dumps(obj, separators=(",", ":")) + "\n")

    emit(dict(schema="camera-config-evidence/v1", kind="identity", scope=scope,
              deviceVersion=info["devver"], operationsCount=len(info["ops"]),
              devicePropsCount=len(info["props"]), eventsCount=len(info["evs"]), evidence=ev("0x1001")))
    for op in info["ops"]:
        emit(dict(schema="camera-config-evidence/v1", kind="operation", scope=scope,
                  code="0x%04X" % op, supported=True, evidence=ev("0x1001")))

    if tsv:
        hout.write("## %s | %s | fw %s | run %s\n" % (info["model"], scope["connection"], scope["firmware"], run))
        hout.write("ops(%d): %s\n" % (len(info["ops"]), " ".join("0x%04X" % x for x in info["ops"])))
        hout.write("events=%d  props=%d\n" % (len(info["evs"]), len(info["props"])))
        hout.write("propcode\tresp\ttype\taccess\tform\tdefault\tcurrent\tdetail\n")

    for code in info["props"]:
        data, rc = p._txn(GET_PROP_DESC, (code,))
        avail = rc == 0x2001 and bool(data)
        frag = dict(schema="camera-config-evidence/v1", kind="property", scope=scope,
                    code="0x%04X" % code, supported=True, descriptorAvailable=avail,
                    resp="0x%04x" % (rc or 0), evidence=dict(ev("0x1014"), raw=(data.hex() if data else "")))
        if not avail:
            emit(frag)
            if tsv:
                hout.write("0x%04X\t0x%04x\t-\t-\t-\t-\t-\trefused/empty\n" % (code, (rc or 0)))
            continue
        try:
            pd = parse_propdesc(data)
            desc = {"form": pd["form"]}
            if pd["form"] == "enum":
                desc["values"] = pd["detail"]
            elif pd["form"] == "range":
                desc["range"] = pd["detail"]
            frag.update(type=type_name(pd["dt"]), access=access_name(pd["getset"]),
                        descriptor=desc, observed=dict(default=pd["default"], current=pd["current"]))
            emit(frag)
            if tsv:
                det = ""
                if pd["form"] == "enum":
                    det = "[%s]" % ",".join(fmt_tsv(x) for x in pd["detail"])
                elif pd["form"] == "range":
                    r = pd["detail"]
                    det = "min=%s max=%s step=%s" % (fmt_tsv(r["min"]), fmt_tsv(r["max"]), fmt_tsv(r["step"]))
                hout.write("0x%04X\t0x2001\t%s\t%s\t%s\t%s\t%s\t%s\n" % (
                    code, type_name(pd["dt"]), access_name(pd["getset"]), pd["form"],
                    fmt_tsv(pd["default"]), fmt_tsv(pd["current"]), det))
        except Exception as e:
            frag.update(parseError=str(e))
            emit(frag)
            if tsv:
                hout.write("0x%04X\t0x2001\tPARSE_ERR\t-\t-\t-\t-\t%s\n" % (code, e))

    if args.open_capture and cap_tid is not None:
        p._txn(0x1018, (cap_tid,))   # TerminateOpenCapture(captureTxId) — undo the takeover
        p._txn(0x1003)               # CloseSession


if __name__ == "__main__":
    main()
