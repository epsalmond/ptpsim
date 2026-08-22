#!/usr/bin/env python3
"""Minimal fake plugin proving discovery, panels, actions, and shutdown.

Serves:
  GET /manifest -> plugin manifest matching schema v1
  GET /panels   -> rows/spans payload with priority
  POST /do      -> proxied operator action

Both spawned and attached modes use the same HTTP surface; the TUI discovers
via GET /manifest and then poles GET /panels and proxies POST /plugins/{id}/actions/{id}.
"""

import argparse
import json
import http.server
import socketserver
import urllib.parse

PANEL_ROWS = [[{"text": "fake plugin panel", "style": "info"}, {"text": " rows ok", "style": "plain"}]]

ACTION_COUNT = 0

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/manifest" or self.path.startswith("/manifest?"):
            manifest = {
                "protocolVersion": "1.0.0",
                "id": "fake-tui-plugin",
                "version": "0.1.0",
                "displayName": "Fake TUI Plugin",
                "description": "In-repo fake plugin for discovery/panel/action/shutdown",
                "panels": [
                    {
                        "id": "fake-panel",
                        "title": "Fake Panel",
                        "priority": 50,
                        "rows": [PANEL_ROWS[0]]
                    }
                ],
                "actions": [
                    {
                        "id": "fake-action",
                        "label": "Fake Action",
                        "path": "/do",
                        "method": "POST",
                        "hotkey": "f"
                    }
                ],
                "endpoint": f"http://127.0.0.1:{self.server.server_address[1]}"
            }
            body = json.dumps(manifest).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        elif self.path == "/panels" or self.path.startswith("/panels?"):
            body = json.dumps({"panels": [
                {"id": "fake-panel", "rows": PANEL_ROWS}
            ]}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        else:
            self.send_response(404)
            self.end_headers()

    def do_POST(self):
        if self.path == "/do":
            global ACTION_COUNT
            ACTION_COUNT += 1
            length = int(self.headers.get("Content-Length", "0") or "0")
            if length:
                self.rfile.read(length)
            body = json.dumps({"ok": True, "count": ACTION_COUNT}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        else:
            self.send_response(404)
            self.end_headers()

    def log_message(self, format, *args):
        # Silence access log unless debug
        return

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=0, help="port to listen on (0 for ephemeral)")
    ap.add_argument("--endpoint", type=str, default=None, help="full endpoint url, overrides port")
    args = ap.parse_args()
    port = args.port
    if args.endpoint:
        parsed = urllib.parse.urlparse(args.endpoint)
        port = parsed.port or 0
    with socketserver.TCPServer(("127.0.0.1", port), Handler, bind_and_activate=True) as httpd:
        # If endpoint was provided, we already bound to that port; otherwise ephemeral
        addr = httpd.server_address
        print(f"fake plugin listening on http://127.0.0.1:{addr[1]}", flush=True)
        # Also handle case where we need to bind to specific port from endpoint but TCPServer already does
        try:
            httpd.serve_forever()
        except KeyboardInterrupt:
            pass

if __name__ == "__main__":
    main()
