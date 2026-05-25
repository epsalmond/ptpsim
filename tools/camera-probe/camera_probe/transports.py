"""Transport axis — first-class and pluggable.

A camera's behavior is a function of (model, firmware, **transport**, **mode**, state). Each
transport owns its *connection establishment* (how the socket comes into being) and its port
layout. Adding a transport = adding an entry here, not rewriting plans.

v1 wraps what already works:
  - pcss : PCSS/PTP-IP desktop tether — UDP knock :51562 → camera NOTIFY :51560 → PTP/IP :15740
           (connect_wireless_tether.connect / .connect_ptpip)
  - app : reference app AP PTP/IP — BLE→AP launch, then direct connect :55740 (+ event :55741, liveview :55742)
           (probe_iso_liveview.open_control_session)
  - http : the camera's embedded webserver (ex-"XLV") — declared, not yet wired (top exploration target)
  - usb  : PTP over USB with mutually-exclusive modes (raw-conversion/backup-restore/webcam/image) — stub
"""
from __future__ import annotations

from dataclasses import dataclass, field


@dataclass(frozen=True)
class TransportSpec:
    name: str
    kind: str
    establishment: str
    ports: dict = field(default_factory=dict)
    status: str = "ready"
    notes: str = ""


REGISTRY: dict[str, TransportSpec] = {
    "pcss": TransportSpec(
        name="pcss", kind="ptpip-pcss", establishment="udp-knock+callback",
        ports={"callback": 51560, "knock": 51562, "command": 15740}, status="ready",
        notes="Desktop tether. Control + new-capture download; NOT the image library (DF01=20→0x200A).",
    ),
    "app": TransportSpec(
        name="app", kind="ptpip-app", establishment="ble-launch+direct",
        ports={"command": 55740, "event": 55741, "liveview": 55742}, status="ready",
        notes="Mobile-app AP path. Image import (DF01=20) + live view (DF01=22).",
    ),
    "http": TransportSpec(
        name="http", kind="http-xlv", establishment="https-direct",
        ports={"https": 443, "http": 80}, status="exploring",
        notes="Camera webserver (ex-XLV). Photo/video history + configurable live-view size. Top target.",
    ),
    "usb": TransportSpec(
        name="usb", kind="ptp-usb", establishment="usb-enumerate",
        ports={}, status="stub",
        notes="Mutually-exclusive modes: raw-conversion / backup-restore / webcam / image.",
    ),
}


def get(name: str) -> TransportSpec:
    if name not in REGISTRY:
        raise ValueError(f"unknown transport {name!r}; known: {', '.join(REGISTRY)}")
    return REGISTRY[name]
