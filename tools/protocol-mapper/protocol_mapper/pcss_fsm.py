"""Host-side mirror of the camera's PCSS state machine.

The camera's PCSS daemon is a 6-state FSM, statically reverse-engineered on Fuji
firmware 2.30:

  pcss_state_set_and_notify @ 0x03216534  writes the state byte to [0x0449d6d4]

  0 IDLE         no PCSS activity
  1 LISTENING    waiting for a DISCOVERY knock on UDP :51562
  2 DISCOVERED   DISCOVERY accepted; about to dial back to HOST: <ip>
  3 HANDSHAKING  TCP up to PC :51560; NOTIFY sent, awaiting PC's 200 OK + PTP-IP init
  4 PAIRED       Init_Command_Ack received; session live on DSCPORT (15740)
  5 ERROR        terminal-for-this-attempt; needs remediation (not just a retry)

The transitions modelled here are the HOST-side mirror — i.e. what the host can
infer about the camera's state by watching its own observable events (sends, recvs,
ICMP, RST). Mirroring lets the host orchestrator decide WHEN to give up vs.
re-knock, surface meaningful errors instead of looping silently, and emit a
bundleable transition log.

Event names recognised by `step`:
  - "knock_sent"        host sent the DISCOVERY UDP datagram
  - "icmp_unreachable"  ICMP type 3 / code 3 on the knock port (camera spent)
  - "callback_accepted" inbound TCP on PC :51560 from the camera
  - "notify_received"   a NOTIFY frame parsed off the callback socket
  - "ok_sent"           PC sent "HTTP/1.1 200 OK" on the callback socket
  - "ptpip_connected"   TCP connect() to camera:<DSCPORT> succeeded
  - "init_ack"          Init_Command_Ack (raw_type == 2)
  - "init_fail"         Init_Fail (raw_type == 5); frame.headers may carry reason
  - "rst"               TCP RST mid-session
  - "session_closed"    clean close — go back to IDLE
"""
from __future__ import annotations

import time
from dataclasses import dataclass, field
from enum import IntEnum
from typing import Optional

from .pcss_frame import PCSSFrame


class PCSSState(IntEnum):
    IDLE = 0
    LISTENING = 1
    DISCOVERED = 2
    HANDSHAKING = 3
    PAIRED = 4
    ERROR = 5


# Distinguished Init_Fail reason — "device busy, please retry on the same socket"
# (matches the wire capture). Any other Init_Fail reason is treated as terminal.
PTP_RESPONSE_DEVICE_BUSY = 0x2019


@dataclass
class FSMTransition:
    ts: float
    prev: PCSSState
    new: PCSSState
    event: str
    reason: Optional[str] = None

    def to_dict(self) -> dict:
        return {
            "kind": "pcss.fsm.transition",
            "ts": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(self.ts)),
            "prev_state": self.prev.name,
            "new_state": self.new.name,
            "event": self.event,
            "reason": self.reason,
        }


@dataclass
class PCSSFSM:
    """Host-side mirror of the camera PCSS state machine.

    `step(event, frame=None, *, reason=None)` consumes one host-observable event
    and returns the resulting state. The full transition is appended to `history`
    so callers can emit one bundle fact per transition.
    """
    current: PCSSState = PCSSState.LISTENING
    history: list[FSMTransition] = field(default_factory=list)

    def _go(self, new: PCSSState, event: str, reason: Optional[str] = None) -> PCSSState:
        prev = self.current
        self.current = new
        self.history.append(FSMTransition(time.time(), prev, new, event, reason))
        return new

    def step(self, event: str, frame: Optional[PCSSFrame] = None,
             *, reason: Optional[str] = None) -> PCSSState:
        s = self.current

        if event == "knock_sent":
            # benign — stay in LISTENING (or IDLE if we're orchestrating manually)
            if s == PCSSState.IDLE:
                return self._go(PCSSState.LISTENING, event)
            return self.current

        if event == "icmp_unreachable":
            # "stop knocking — different remediation"; mid-session this is fatal.
            if s in (PCSSState.LISTENING, PCSSState.IDLE):
                return self._go(PCSSState.IDLE, event,
                                reason or "knock refused — camera spent (power-cycle)")
            return self._go(PCSSState.ERROR, event,
                            reason or "ICMP port-unreachable mid-session")

        if event == "callback_accepted":
            return self._go(PCSSState.DISCOVERED, event)

        if event == "notify_received":
            # camera dialled us, sent NOTIFY → we're in HANDSHAKING
            r = (f"verb={frame.verb} camera={frame.camera_name} dscport={frame.dsc_port}"
                 if frame else None)
            return self._go(PCSSState.HANDSHAKING, event, r)

        if event == "ok_sent":
            # stay in HANDSHAKING until PTP-IP Init_Command_Ack lands
            return self.current

        if event == "ptpip_connected":
            return self.current  # still HANDSHAKING; ack is the gate

        if event == "init_ack":
            return self._go(PCSSState.PAIRED, event)

        if event == "init_fail":
            # Reason can come from the caller (already decoded) or a frame
            # carrying a "REASON" header. Device_Busy = transient → stay.
            code = _extract_fail_reason(reason, frame)
            if code == PTP_RESPONSE_DEVICE_BUSY:
                return self._go(self.current, event,
                                f"Init_Fail 0x{code:04x} Device_Busy (resend on same socket)")
            human = f"Init_Fail 0x{code:04x}" if code is not None else "Init_Fail"
            return self._go(PCSSState.ERROR, event,
                            f"{human} — stop retrying, different remediation needed")

        if event == "rst":
            # Mid-session RST → IDLE (cleanly drop; orchestrator decides whether to re-knock)
            return self._go(PCSSState.IDLE, event, reason or "TCP RST mid-session")

        if event == "session_closed":
            return self._go(PCSSState.IDLE, event)

        # Unknown event — don't transition, just record it.
        self.history.append(FSMTransition(time.time(), s, s, event,
                                          reason or "unknown event (no transition)"))
        return self.current


def _extract_fail_reason(reason: Optional[str], frame: Optional[PCSSFrame]) -> Optional[int]:
    """Best-effort: pull the numeric Init_Fail reason out of either a textual caller
    hint (e.g. "0x2019") or a frame whose headers carry it."""
    if reason:
        try:
            return int(reason, 0)
        except ValueError:
            pass
    if frame is not None:
        v = frame.headers.get("REASON") or frame.headers.get("FAIL_REASON")
        if v:
            try:
                return int(v, 0)
            except ValueError:
                return None
    return None
