"""Probe risk tiers + the destructive-opcode denylist.

Every plan declares a `RiskClass`; the CLI refuses to run a plan whose tier exceeds what the
caller explicitly allowed. Escalating (each tier a louder opt-in):

  safe              read-mostly: DeviceInfo, descriptor sweeps, enumeration, handshake capture
  settings-write    writes documented props / settings blobs (reversible; checksums catch errors)
  ram               volatile RAM read/write (recoverable by power cycle)
  firmware-supported firmware update via the vendor's own path, safety checks intact
  firmware-unlocked  firmware writes bypassing vendor safety checks (can brick)

The factory-reset opcode (0xD17F) is denylisted so a blind sweep can never fire it by accident;
the probe knows it precisely in order to AVOID it. (It is exposed deliberately elsewhere as a
named feature, never via a sweep.)
"""
from __future__ import annotations

import re
from enum import IntEnum


class RiskClass(IntEnum):
    SAFE = 0
    SETTINGS_WRITE = 1
    RAM = 2
    FIRMWARE_SUPPORTED = 3
    FIRMWARE_UNLOCKED = 4


_NAMES = {
    "safe": RiskClass.SAFE,
    "settings-write": RiskClass.SETTINGS_WRITE,
    "ram": RiskClass.RAM,
    "firmware-supported": RiskClass.FIRMWARE_SUPPORTED,
    "firmware-unlocked": RiskClass.FIRMWARE_UNLOCKED,
}

# Opcodes a sweep must never write, regardless of tier.
DENYLIST = {0xD17F}  # Shooting-Menu / settings reset
# Property/operation names that smell destructive — screened out of any blind sweep.
DANGER_NAME = re.compile(r"reset|format|erase|delete|initiali[sz]e|wipe|factory", re.I)


def parse(name: str) -> RiskClass:
    if name not in _NAMES:
        raise ValueError(f"unknown risk class {name!r}; choose from {', '.join(_NAMES)}")
    return _NAMES[name]


def enforce(plan_risk: RiskClass, allowed: RiskClass) -> None:
    """Raise if a plan's risk exceeds what the caller opted into."""
    if plan_risk > allowed:
        raise PermissionError(
            f"plan requires risk tier '{plan_risk.name.lower()}' but caller allowed only "
            f"'{allowed.name.lower()}' — re-run with --risk {plan_risk.name.lower().replace('_','-')}"
        )
