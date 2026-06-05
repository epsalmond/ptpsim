"""PCSS/1.0 frame parser (Fuji "PC Shoot Service").

Wire-confirmed (2026-05-23) frame shape — HTTP-over-(UDP|TCP), SSDP-style, lifted from


  <VERB> * HTTP/1.1\\r\\n          (or status-line: HTTP/1.1 200 OK\\r\\n)
  HOST: <ip>\\r\\n                 (DISCOVERY: PC's own IP)
  MX: <n>\\r\\n
  SERVICE: PCSS/1.0\\r\\n           (some frames omit the trailing /1.0)
  ...                              (NOTIFY adds DSC, CAMERANAME, DSCPORT)
  \\r\\n                            (blank line; sometimes absent)
  \\x00                             (trailing NUL byte — observed in real captures)

Verbs observed: DISCOVERY (host→camera knock), NOTIFY (camera→host announce),
                "HTTP/1.1 200 OK" / "HTTP/1.1 403 Forbidden" (responses).

This module is the single source of truth for frame parsing; both the passive listener
(`scripts/pcss_listen.py`) and the active tether (`scripts/connect_wireless_tether.py`)
go through `parse_pcss_frame`.
"""
from __future__ import annotations

from dataclasses import dataclass, field
from typing import Optional


@dataclass(frozen=True)
class PCSSFrame:
    """A parsed PCSS/1.0 frame.

    Attributes:
        verb: The request verb ("DISCOVERY", "NOTIFY") or status line ("HTTP/1.1 200 OK").
              For response frames this is the full status line; the numeric status code
              is exposed separately via `status_code`.
        status_code: Numeric status code (e.g. 200, 403) for response frames; None for
                     requests.
        headers: Header name → value, names upper-cased and stripped.
        trailing_nul: True if the on-wire bytes ended with a 0x00 (matches captures).
        raw: The raw frame bytes as received.
    """
    verb: str
    status_code: Optional[int]
    headers: dict = field(default_factory=dict)
    trailing_nul: bool = False
    raw: bytes = b""

    @property
    def is_response(self) -> bool:
        return self.status_code is not None

    @property
    def host(self) -> Optional[str]:
        return self.headers.get("HOST")

    @property
    def mx(self) -> Optional[str]:
        return self.headers.get("MX")

    @property
    def service(self) -> Optional[str]:
        return self.headers.get("SERVICE")

    @property
    def dsc(self) -> Optional[str]:
        return self.headers.get("DSC")

    @property
    def camera_name(self) -> Optional[str]:
        return self.headers.get("CAMERANAME")

    @property
    def dsc_port(self) -> Optional[int]:
        v = self.headers.get("DSCPORT", "")
        return int(v) if v.isdigit() else None

    def to_dict(self) -> dict:
        return {
            "verb": self.verb,
            "status_code": self.status_code,
            "headers": dict(self.headers),
            "trailing_nul": self.trailing_nul,
            "frame_bytes_hex": self.raw.hex(),
        }


def parse_pcss_frame(data: bytes) -> Optional[PCSSFrame]:
    """Parse a PCSS/1.0 frame. Returns None if the input doesn't look like one.

    Tolerates:
      - trailing NUL byte (real captures end with one)
      - missing blank-line terminator (NOTIFY in the capture has no \\r\\n\\r\\n)
      - LF-only line endings (be liberal in what you accept)
      - leading/trailing whitespace around header values
    """
    if not data:
        return None
    trailing_nul = data.endswith(b"\x00")
    body = data[:-1] if trailing_nul else data
    try:
        text = body.decode("latin1")
    except UnicodeDecodeError:
        return None
    # Split on CRLF, fall back to LF; tolerate either.
    if "\r\n" in text:
        lines = text.split("\r\n")
    else:
        lines = text.split("\n")
    if not lines:
        return None
    start_line = lines[0].strip()
    if not start_line:
        return None

    verb: str
    status_code: Optional[int] = None
    if start_line.startswith("HTTP/"):
        # status line e.g. "HTTP/1.1 200 OK"
        parts = start_line.split(None, 2)
        if len(parts) >= 2 and parts[1].isdigit():
            status_code = int(parts[1])
            verb = start_line  # keep full status line as the verb for downstream display
        else:
            return None
    else:
        # request line e.g. "DISCOVERY * HTTP/1.1"
        tokens = start_line.split()
        if len(tokens) < 3 or not tokens[2].startswith("HTTP/"):
            return None
        verb = tokens[0].upper()
        # sanity gate: only accept verbs we know belong to PCSS, plus a relaxed
        # unknown-verb pass-through so we don't drop captures with new tokens
        if not verb.replace("-", "").replace("_", "").isalnum():
            return None

    headers: dict = {}
    for line in lines[1:]:
        line = line.rstrip("\r")
        if not line:
            continue  # blank line — body separator (no body in PCSS)
        if ":" not in line:
            continue
        k, _, v = line.partition(":")
        headers[k.strip().upper()] = v.strip()

    # Require a SERVICE header that mentions PCSS, OR a recognisable verb.
    # (Status-line frames legitimately have no SERVICE header.)
    if status_code is None:
        service = headers.get("SERVICE", "")
        known_verbs = {"DISCOVERY", "NOTIFY", "SEARCH", "M-SEARCH"}
        if "PCSS" not in service.upper() and verb not in known_verbs:
            return None

    return PCSSFrame(
        verb=verb,
        status_code=status_code,
        headers=headers,
        trailing_nul=trailing_nul,
        raw=bytes(data),
    )
