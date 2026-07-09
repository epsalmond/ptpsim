#!/usr/bin/env python3
"""Run one CI command with a hard limit and publish timing-ratchet alerts."""

from __future__ import annotations

import argparse
import os
import signal
import subprocess
import sys
import time


def stop_process_group(process: subprocess.Popen[bytes]) -> None:
    os.killpg(process.pid, signal.SIGTERM)
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--name", required=True)
    parser.add_argument("--warn-seconds", type=float, required=True)
    parser.add_argument("--hard-seconds", type=float, default=300)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("a command is required after --")
    if args.warn_seconds >= args.hard_seconds:
        parser.error("--warn-seconds must be below --hard-seconds")

    started = time.monotonic()
    process = subprocess.Popen(command, start_new_session=True)
    timed_out = False
    try:
        return_code = process.wait(timeout=args.hard_seconds)
    except subprocess.TimeoutExpired:
        timed_out = True
        stop_process_group(process)
        return_code = 124
    elapsed = time.monotonic() - started
    print(f"timing: {args.name} completed in {elapsed:.1f}s", flush=True)

    if timed_out or elapsed > args.warn_seconds:
        state = "hit the hard limit" if timed_out else "crossed its warning ratchet"
        pipeline_url = os.environ.get("CI_PIPELINE_URL") or os.environ.get(
            "CI_PIPELINE_FORGE_URL", ""
        )
        suffix = f" {pipeline_url}" if pipeline_url else ""
        content = (
            f"ALERT: ptpsim CI step `{args.name}` {state}: {elapsed:.1f}s "
            f"(warn {args.warn_seconds:.0f}s, hard {args.hard_seconds:.0f}s).{suffix}"
        )
        print(content, file=sys.stderr)
        # The dependent NAS workflow owns NATS delivery. A ratchet is a build
        # failure, not a warning that can disappear into logs.
        return return_code or 42

    return return_code


if __name__ == "__main__":
    raise SystemExit(main())
