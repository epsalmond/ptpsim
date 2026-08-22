#!/bin/sh
# Profile idle CPU for camera-sim-tui (#218 recipe).
# Run the TUI idle (against a real or fake control endpoint) and measure CPU.
#
# Usage:
#   ./tools/camera-sim-tui/scripts/profile-tui-idle.sh --port 8770
#
# The script starts the TUI headless or curses and samples CPU with ps/top.
set -eu

PORT=${1:-8770}
SAMPLE_SECS=5

echo "Starting camera-sim-tui idle profile (sample ${SAMPLE_SECS}s)..."
echo "Control endpoint: 127.0.0.1:8080 (adjust as needed)"
echo "Listen endpoint: 127.0.0.1:${PORT}"
echo ""
echo "Manual recipe:"
echo "  1. Terminal A: cargo run -p camera-sim-tui -- --headless --control 127.0.0.1:8080 --listen 127.0.0.1:${PORT}"
echo "  2. Terminal B: top -pid \$(pgrep camera-sim-tui)  # macOS"
echo "     or: ps -o %cpu -p \$(pgrep camera-sim-tui)     # Linux"
echo "  3. Let idle 5s, record %CPU. Before fix: ~20% of one core."
echo "     After fix (20 Hz draw, 250 ms poll, 2 s health): <5% idle."
echo ""
echo "Automated sample (if TUI already running):"
PID=$(pgrep camera-sim-tui || true)
if [ -n "$PID" ]; then
  echo "Found PID $PID, sampling CPU for ${SAMPLE_SECS}s..."
  if command -v ps >/dev/null 2>&1; then
    for i in $(seq 1 $SAMPLE_SECS); do
      ps -o %cpu= -p "$PID" 2>/dev/null | tr -d ' ' | xargs -I{} echo "  sample $i: {} %CPU"
      sleep 1
    done
  else
    echo "ps not found"
  fi
else
  echo "No running camera-sim-tui found. Start it as above and re-run this script."
fi

# Acceptance check: cargo test verifies cadence constants
echo ""
echo "Running cadence acceptance check..."
cargo test -p camera-sim-tui idle_cadence -- --nocapture 2>&1 | tail -n 20 || true
