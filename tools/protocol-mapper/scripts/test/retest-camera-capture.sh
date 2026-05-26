#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/test/retest-camera-capture.sh [options]

Options:
  --device-name NAME   Camera device name. Default: iPhone.
  --output PATH        Test output PNG. Default: /private/tmp/fuji-camera-capture-retest.png.
  --timeout SEC        Capture timeout. Default: 8.
  --warmup SEC         Capture warmup. Default: 2.
  --zoom FACTOR        Requested output center-crop zoom factor.
  -h, --help           Show this help.

Deletes the previous test image, runs the camera capture helper with the
specified args, and prints file metadata for the new output.
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
device_name="iPhone"
output="/private/tmp/fuji-camera-capture-retest.png"
timeout="8"
warmup="2"
zoom="${FUJI_CAMERA_ZOOM:-}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --device-name)
      device_name="$2"
      shift 2
      ;;
    --output)
      output="$2"
      shift 2
      ;;
    --timeout)
      timeout="$2"
      shift 2
      ;;
    --warmup)
      warmup="$2"
      shift 2
      ;;
    --zoom)
      zoom="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

echo "+ rm -f $output" >&2
rm -f "$output"

capture_args=(
  --device-name "$device_name" \
  --output "$output" \
  --timeout "$timeout" \
  --warmup "$warmup"
)
if [[ -n "$zoom" ]]; then
  capture_args+=(--zoom "$zoom")
fi

echo "+ scripts/capture_continuity_camera_frame.sh ${capture_args[*]}" >&2
"$repo_root/scripts/capture_continuity_camera_frame.sh" "${capture_args[@]}"

echo "+ file $output" >&2
file "$output"
