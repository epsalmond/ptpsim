"""camera-probe CLI: list plans, run a probe plan, emit an observation bundle.

  camera-probe list-plans
  camera-probe probe --plan fuji/pcss/auto/partial-header --risk safe --out bundle.jsonl
  camera-probe probe --plan fuji/pcss/auto/settings-sweep --risk settings-write --out sweep.jsonl
"""
from __future__ import annotations

import argparse
import sys

from . import plans, transports
from .risk import enforce, parse


def _cmd_list(_args) -> int:
    print(f"{'PLAN':40s} {'RISK':16s} TRANSPORT  SUMMARY")
    for key in sorted(plans.REGISTRY):
        p = plans.REGISTRY[key]
        t = transports.REGISTRY.get(p.transport)
        tstat = f"{p.transport}({t.status})" if t else p.transport
        print(f"{key:40s} {p.risk.name.lower():16s} {tstat:10s} {p.summary}")
    return 0


def _cmd_probe(args) -> int:
    try:
        plan = plans.get(args.plan)
    except KeyError:
        print(f"[err] unknown plan {args.plan!r}; see `camera-probe list-plans`")
        return 2
    try:
        enforce(plan.risk, parse(args.risk))
    except (PermissionError, ValueError) as exc:
        print(f"[err] {exc}")
        return 2
    return plan.run(args.camera_ip, out=args.out, dry_run=args.dry_run)


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(prog="camera-probe")
    sub = ap.add_subparsers(dest="cmd", required=True)

    sub.add_parser("list-plans", help="list available probe plans").set_defaults(func=_cmd_list)

    pp = sub.add_parser("probe", help="run a probe plan against a camera")
    pp.add_argument("--plan", required=True, help="fuji/<transport>/<mode>/<name>")
    pp.add_argument("camera_ip")
    pp.add_argument("--risk", default="safe",
                    help="max risk tier to allow: safe|settings-write|ram|firmware-supported|firmware-unlocked")
    pp.add_argument("--out", default=None, help="observation-bundle JSONL output path")
    pp.add_argument("--dry-run", action="store_true")
    pp.set_defaults(func=_cmd_probe)

    args = ap.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
