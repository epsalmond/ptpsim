#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/capture_continuity_camera_frame.sh --device-name iPhone --output PATH [--timeout SEC] [--warmup SEC] [--zoom FACTOR]
  scripts/capture_continuity_camera_frame.sh --check-permission
  scripts/capture_continuity_camera_frame.sh --list-devices

Builds and runs the macOS AVFoundation Continuity Camera capture helper.
The helper writes a lossless PNG frame. Default warmup: 2 seconds.
Set FUJI_CAMERA_ZOOM or pass --zoom to request output center-crop zoom.
USAGE
}

device_name="iPhone"
output=""
timeout="10"
warmup="2"
zoom="${FUJI_CAMERA_ZOOM:-}"
check_permission=0
list_devices=0

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
    --check-permission)
      check_permission=1
      shift
      ;;
    --list-devices)
      list_devices=1
      shift
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

if [[ -z "$output" && "$check_permission" != "1" && "$list_devices" != "1" ]]; then
  echo "--output is required" >&2
  usage >&2
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ -n "$output" && "$output" != /* ]]; then
  output="$(pwd)/$output"
fi
binary="$("$repo_root/scripts/build/build-camera-capture.sh")"
app="$("$repo_root/scripts/build/build-camera-capture.sh" --app-path)"
lock_dir="${TMPDIR:-/tmp}/fuji-camera-capture.lock"

acquire_lock() {
  local waited=0
  while ! mkdir "$lock_dir" 2>/dev/null; do
    if (( waited >= 30 )); then
      echo "timed out waiting for camera capture lock: $lock_dir" >&2
      return 1
    fi
    sleep 1
    waited=$((waited + 1))
  done
  trap 'rmdir "$lock_dir" 2>/dev/null || true' EXIT
}

run_capture_app() {
  open -W -n "$app" --args "$@"
}

if [[ "$check_permission" == "1" ]]; then
  permission_output="/private/tmp/fuji-camera-capture-permission-check-$$.png"
  permission_args=(--device-name "$device_name" --output "$permission_output" --timeout 10 --warmup 0)
  if [[ -n "$zoom" ]]; then
    permission_args+=(--zoom "$zoom")
  fi
  acquire_lock
  if run_capture_app "${permission_args[@]}" \
    && [[ -s "$permission_output" ]]; then
    echo "camera_authorization=authorized"
    echo "permission_check_capture=$permission_output"
    exit 0
  fi
  echo "camera_authorization=unavailable_via_app_bundle"
  echo "app=$app"
  "$binary" --check-permission || true
  exit 1
fi

if [[ "$list_devices" == "1" ]]; then
  exec "$binary" --list-devices
fi

capture_args=(--device-name "$device_name" --output "$output" --timeout "$timeout" --warmup "$warmup")
if [[ -n "$zoom" ]]; then
  capture_args+=(--zoom "$zoom")
fi
acquire_lock
if ! run_capture_app "${capture_args[@]}"; then
  echo "camera capture app failed to launch or exited with an error" >&2
  echo "app=$app" >&2
  exit 1
fi

if [[ ! -s "$output" ]]; then
  echo "camera capture app did not produce output: $output" >&2
  "$binary" --check-permission >&2 || true
  exit 1
fi

echo "captured_device=$device_name"
if [[ -n "$zoom" ]]; then
  echo "requested_zoom=$zoom"
fi
echo "output=$output"
