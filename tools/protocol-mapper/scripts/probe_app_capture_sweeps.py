#!/usr/bin/env python3
"""Capture post-#52 Fuji reference app live-view, AP-launch, and transfer metadata sweeps."""
from __future__ import annotations

from pathlib import Path
import sys

REPO_ROOT = Path(__file__).resolve().parents[1]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from protocol_mapper.app_capture_sweeps import main  # noqa: E402


if __name__ == "__main__":
    raise SystemExit(main())
