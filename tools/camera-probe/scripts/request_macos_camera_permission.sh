#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

tmp_dir="${TMPDIR:-/tmp}/fuji-camera-permission"
mkdir -p "$tmp_dir"

echo "Requesting macOS camera permission through the AVFoundation helper." >&2
echo "Approve camera access for this terminal app if macOS prompts." >&2
scripts/capture_continuity_camera_frame.sh --check-permission

if ! scripts/capture_continuity_camera_frame.sh --device-name iPhone --output "$tmp_dir/permission-check.png" --timeout 5 --warmup 2; then
  cat >&2 <<'MSG'
Camera permission is not authorized for the process running this command.
Open System Settings > Privacy & Security > Camera, then enable camera access
for the terminal or command runner that will execute the capture script.
MSG
  echo "repair_command=open 'x-apple.systempreferences:com.apple.preference.security?Privacy_Camera'"
  exit 1
fi
echo "camera_permission_capture=$tmp_dir/permission-check.png"
