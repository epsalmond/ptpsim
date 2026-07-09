#!/usr/bin/env python3


from __future__ import annotations

import argparse
import json
import os
import socket
from urllib.parse import unquote, urlsplit


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("message")
    parser.add_argument("--subject", default="notify.alert.ptpsim_ci")
    args = parser.parse_args()

    parsed = urlsplit(os.environ.get("NATS_NOTIFY_URL", ""))
    if parsed.scheme != "nats" or not parsed.hostname:
        parser.error("NATS_NOTIFY_URL must be a nats:// URL")
    connect = {"name": "ptpsim-ci-alert", "verbose": False, "pedantic": False}
    if parsed.username:
        connect["user"] = unquote(parsed.username)
    if parsed.password:
        connect["pass"] = unquote(parsed.password)
    payload = json.dumps({"content": args.message, "username": "ptpsim-ci"}).encode()

    with socket.create_connection((parsed.hostname, parsed.port or 4222), timeout=10) as conn:
        if not conn.recv(4096).startswith(b"INFO "):
            raise RuntimeError("NATS server did not send INFO")
        conn.sendall(
            b"CONNECT " + json.dumps(connect, separators=(",", ":")).encode() + b"\r\n"
            + f"PUB {args.subject} {len(payload)}\r\n".encode()
            + payload
            + b"\r\nPING\r\n"
        )
        response = conn.recv(4096)
        if b"-ERR" in response or b"PONG" not in response:
            raise RuntimeError("NATS server did not acknowledge the alert")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
