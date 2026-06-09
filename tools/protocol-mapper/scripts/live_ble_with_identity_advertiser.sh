#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/live_ble_with_identity_advertiser.sh [wrapper options] -- [live-test options]

Wrapper options:
  --identity-name NAME       Name advertised as BLE Local Name.
                             Default: FUJI_BLE_IDENTITY_NAME, then testhost.
  --advertise-duration SEC  Seconds to keep advertiser alive. Default: 180.
  -h, --help                Show this help.

Any arguments after -- are passed to scripts/live_ble_camera_test.sh. If no
--device-name is present, the wrapper adds --device-name <identity-name>.

macOS public CoreBluetooth rejects publishing reserved GAP service 0x1800 /
Device Name 0x2A00 from this helper. This wrapper tests the Local Name fallback
path only. Live tests showed it does not fix the camera-side blank host name.

Example:
  scripts/live_ble_with_identity_advertiser.sh --identity-name testhost -- \
    --skip-location --write-registration-ack --timeout 45
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

identity_name="${FUJI_BLE_IDENTITY_NAME:-testhost}"
advertise_duration="${FUJI_BLE_IDENTITY_DURATION:-180}"
live_args=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --identity-name)
      identity_name="$2"
      shift 2
      ;;
    --advertise-duration)
      advertise_duration="$2"
      shift 2
      ;;
    --)
      shift
      live_args+=("$@")
      break
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      live_args+=("$1")
      shift
      ;;
  esac
done

has_device_name=0
for arg in "${live_args[@]}"; do
  if [[ "$arg" == "--device-name" ]]; then
    has_device_name=1
    break
  fi
done
if [[ "$has_device_name" == "0" ]]; then
  live_args=(--device-name "$identity_name" "${live_args[@]}")
fi

session_id="$(date -u +%Y%m%dT%H%M%SZ)"
log_dir="rce/sessions/macos_ble_identity_advertiser_${session_id}"
log_file="${log_dir}/advertiser.log"
mkdir -p "$log_dir"

echo "advertiser_log=$log_file"
scripts/macos_ble_identity_advertiser.sh --name "$identity_name" --duration "$advertise_duration" >"$log_file" 2>&1 &
advertiser_pid=$!

cleanup() {
  if kill -0 "$advertiser_pid" >/dev/null 2>&1; then
    kill "$advertiser_pid" >/dev/null 2>&1 || true
    wait "$advertiser_pid" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

deadline=$((SECONDS + 20))
while [[ $SECONDS -lt $deadline ]]; do
  if grep -q "advertising_started" "$log_file"; then
    break
  fi
  if ! kill -0 "$advertiser_pid" >/dev/null 2>&1; then
    cat "$log_file" >&2
    echo "identity advertiser exited before advertising started" >&2
    exit 1
  fi
  sleep 0.5
done

if ! grep -q "advertising_started" "$log_file"; then
  cat "$log_file" >&2
  echo "identity advertiser did not start within 20 seconds" >&2
  exit 1
fi

echo "identity_advertiser_started name=$identity_name"
scripts/live_ble_camera_test.sh "${live_args[@]}"

echo "identity_advertiser_log_tail:"
tail -n 40 "$log_file"
