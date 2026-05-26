"""Probe-plan registry, keyed by (manufacturer, transport, mode, name).

Plans WRAP the proven scripts in `scripts/` — no rewrite of working logic. Each runner sets the
bundle context (so facts are stamped with transport/mode), opens the bundle if requested, invokes
the script's `main(argv)`, and closes the bundle. Risk tier is declared per plan and enforced by
the CLI before anything state-changing runs.
"""
from __future__ import annotations

import importlib
import os
import sys
import time
from dataclasses import dataclass
from typing import Callable

from . import bundle
from .risk import RiskClass

_REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
_SCRIPTS = os.path.join(_REPO, "scripts")
if _SCRIPTS not in sys.path:
    sys.path.insert(0, _SCRIPTS)


def _load(modname: str):
    return importlib.import_module(modname)


def _sessiondir(name: str) -> str:
    d = os.path.join("/tmp", "protocol-mapper", f"{name}_{time.strftime('%Y%m%dT%H%M%S')}")
    os.makedirs(d, exist_ok=True)
    return d


@dataclass(frozen=True)
class Plan:
    key: str            # fuji/<transport>/<mode>/<name>
    risk: RiskClass
    transport: str
    mode: str
    summary: str
    run: Callable       # run(camera_ip, *, out, dry_run, **kw) -> int


def _wrap(transport, mode, build_argv, module, attr="main"):
    """Build a runner that stamps bundle context, opens the bundle, and calls module.main(argv)."""
    def run(camera_ip, *, out=None, dry_run=False, **kw) -> int:
        argv = build_argv(camera_ip, dry_run=dry_run, **kw)
        if dry_run:
            print(f"[dry-run] would run {module}.{attr} {argv}")
            return 0
        bundle.set_context(transport=transport, mode=mode, state="probing")
        if out:
            bundle.open_bundle(out)
        try:
            return int(getattr(_load(module), attr)(argv) or 0)
        finally:
            if out:
                facts = bundle._SINK.count if bundle._SINK else 0
                bundle.close_bundle()
                print(f"[bundle] wrote {facts} facts -> {out}")
    return run


def _argv_sweep(camera_ip, *, dry_run=False, **kw):
    a = [camera_ip, "--session", _sessiondir("settings-sweep")]
    if kw.get("props"):
        a += ["--props", kw["props"]]
    return a


def _argv_partial(camera_ip, *, dry_run=False, **kw):
    return [camera_ip, "--bytes", str(kw.get("bytes", 4096))]


def _argv_pull(camera_ip, *, dry_run=False, **kw):
    a = [camera_ip]
    if kw.get("check_band", True):
        a.append("--check-band")
    return a


REGISTRY: dict[str, Plan] = {}


def _reg(key, risk, transport, mode, summary, run):
    REGISTRY[key] = Plan(key, risk, transport, mode, summary, run)


_reg("fuji/pcss/auto/settings-sweep", RiskClass.SETTINGS_WRITE, "pcss", "auto",
     "Drift-immune before→after property sweep (0xD17F denylisted); pins .dat byte offsets.",
     _wrap("pcss", "auto", _argv_sweep, "backup_sweep"))
_reg("fuji/pcss/auto/partial-header", RiskClass.SAFE, "pcss", "auto",
     "Enumerate handles + GetPartialObject header-only read (linchpin transport check).",
     _wrap("pcss", "auto", _argv_partial, "probe_partial_header"))
_reg("fuji/pcss/auto/pull-backup", RiskClass.SAFE, "pcss", "auto",
     "Pull the 69500B settings .dat; --check-band reports 5GHz/2.4GHz.",
     _wrap("pcss", "auto", _argv_pull, "pull_backup"))
_reg("fuji/app/import/image-import", RiskClass.SAFE, "app", "import",
     "reference app AP image-import (DF01=20) browse/enumerate — needs BLE→AP launched first.",
     _wrap("app", "import",
           lambda ip, *, dry_run=False, **kw: ["--host", ip, "--session-dir", _sessiondir("image-import"),
                                               "--auto-receive", str(kw.get("secs", 5))],
           "probe_iso_liveview"))
_reg("fuji/app/liveview/stream", RiskClass.SAFE, "app", "liveview",
     "reference app AP live-view stream (DF01=22 + 0x101C) — needs BLE→AP launched first.",
     _wrap("app", "liveview",
           lambda ip, *, dry_run=False, **kw: ["--host", ip, "--session-dir", _sessiondir("liveview"),
                                               "--stream-frames", str(kw.get("frames", 30))],
           "probe_iso_liveview"))


def get(key: str) -> Plan:
    if key not in REGISTRY:
        raise KeyError(key)
    return REGISTRY[key]
