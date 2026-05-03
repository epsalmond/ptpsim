#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/connect_camera_ap_wifi.sh --credentials PATH [options]
  scripts/connect_camera_ap_wifi.sh --ssid SSID --passphrase VALUE [options]

Options:
  --credentials PATH    wifi_credentials.json from camera_ap_prepare.sh.
  --ssid SSID           Camera AP SSID. Prefer --credentials.
  --passphrase VALUE    Camera AP passphrase. Prefer --credentials.
  --bssid VALUE         Camera AP BSSID for evidence only.
  --wifi-iface IFACE    Wi-Fi device. Default: detected from networksetup.
  --target-ip IP        Camera AP endpoint. Default: 192.168.0.1.
  --timeout SEC         Seconds to wait for association/IP. Default: 20.
  --allow-wifi-internet-loss
                        Allow the camera AP to temporarily take over Wi-Fi
                        when no Ethernet internet route is available.
  --skip-ping           Do not attempt non-fatal ping evidence.
  -h, --help            Show this help.

This script connects macOS Wi-Fi to the camera AP while preserving the active
internet route. It records route evidence before and after association and
fails if the default/internet route moves onto Wi-Fi unless
--allow-wifi-internet-loss is explicit. The passphrase is never printed or
written to the script's logs. Success is based on Wi-Fi IP plus camera route
evidence because networksetup can misreport camera AP association.
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python_bin="${PYTHON_BIN:-.venv/bin/python}"
credentials=""
ssid="${FUJI_CAMERA_AP_SSID:-}"
passphrase="${FUJI_CAMERA_AP_PASSPHRASE:-}"
bssid="${FUJI_CAMERA_AP_BSSID:-}"
wifi_iface="${FUJI_WIFI_INTERFACE:-}"
target_ip="${FUJI_CAMERA_AP_TARGET_IP:-192.168.0.1}"
timeout="${FUJI_WIFI_TIMEOUT:-20}"
allow_wifi_internet_loss="${FUJI_ALLOW_WIFI_INTERNET_LOSS:-0}"
skip_ping=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --credentials)
      credentials="$2"
      shift 2
      ;;
    --ssid)
      ssid="$2"
      shift 2
      ;;
    --passphrase)
      passphrase="$2"
      shift 2
      ;;
    --bssid)
      bssid="$2"
      shift 2
      ;;
    --wifi-iface)
      wifi_iface="$2"
      shift 2
      ;;
    --target-ip)
      target_ip="$2"
      shift 2
      ;;
    --timeout)
      timeout="$2"
      shift 2
      ;;
    --allow-wifi-internet-loss)
      allow_wifi_internet_loss=1
      shift
      ;;
    --skip-ping)
      skip_ping=1
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

if [[ -n "$credentials" ]]; then
  if [[ ! -r "$credentials" ]]; then
    echo "credentials file is not readable: $credentials" >&2
    exit 1
  fi
  if [[ ! -x "$python_bin" ]]; then
    echo "missing python executable: $python_bin" >&2
    exit 1
  fi
  credential_field() {
    "$python_bin" -c 'import json, sys
data = json.load(open(sys.argv[1], encoding="utf-8"))
print(data.get(sys.argv[2], ""))
' "$credentials" "$1"
  }
  ssid="${ssid:-$(credential_field ssid)}"
  passphrase="${passphrase:-$(credential_field passphrase)}"
  bssid="${bssid:-$(credential_field bssid)}"
  target_ip="${target_ip:-$(credential_field target_ip)}"
fi

if [[ -z "$ssid" ]]; then
  echo "missing camera AP SSID; pass --credentials or --ssid" >&2
  exit 1
fi

if [[ -z "$passphrase" ]]; then
  echo "missing camera AP passphrase; pass --credentials or --passphrase" >&2
  exit 1
fi

timestamp="$(date -u +%Y%m%dT%H%M%SZ)"

mkdir -p "$session_dir"
connect_log="$session_dir/connect.log"

log() {
  printf '%s\n' "$*" | tee -a "$connect_log" >&2
}

capture() {
  local label="$1"
  shift
  "$@" >"$session_dir/$label.txt" 2>&1 || true
}

route_interface() {
  /sbin/route -n get "$1" 2>/dev/null | awk '/interface:/{print $2; exit}'
}

if [[ -z "$wifi_iface" ]]; then
  capture networksetup_hardware_ports networksetup -listallhardwareports
  wifi_iface="$(
    networksetup -listallhardwareports |
      awk '
        /^Hardware Port: (Wi-Fi|AirPort)$/ {want=1; next}
        want && /^Device: / {print $2; exit}
      '
  )"
fi

if [[ -z "$wifi_iface" ]]; then
  echo "could not detect Wi-Fi interface" >&2
  exit 1
fi

capture route_default_before /sbin/route -n get default
capture route_internet_before /sbin/route -n get 1.1.1.1
capture networksetup_wifi_before networksetup -getairportnetwork "$wifi_iface"
capture ifconfig_wifi_before ifconfig "$wifi_iface"
airport_tool="/System/Library/PrivateFrameworks/Apple80211.framework/Versions/Current/Resources/airport"
if [[ -x "$airport_tool" ]]; then
  capture airport_info_before "$airport_tool" -I
fi

before_default_iface="$(route_interface default)"
before_internet_iface="$(route_interface 1.1.1.1)"

if [[ -z "$before_default_iface" || -z "$before_internet_iface" ]]; then
  echo "could not determine current default/internet route; refusing to change Wi-Fi" >&2
  echo "session=$session_dir" >&2
  exit 1
fi

if [[ "$before_default_iface" == "$wifi_iface" || "$before_internet_iface" == "$wifi_iface" ]] &&
  [[ "$allow_wifi_internet_loss" != "1" ]]; then
  echo "internet route is already on Wi-Fi ($wifi_iface); connect/prioritize Ethernet before camera AP association" >&2
  echo "session=$session_dir" >&2
  exit 1
fi

log "session=$session_dir"
log "wifi_interface=$wifi_iface"
log "ssid=$ssid"
if [[ -n "$bssid" ]]; then
  log "bssid=$bssid"
fi
log "default_route_before=$before_default_iface"
log "internet_route_before=$before_internet_iface"
if [[ "$allow_wifi_internet_loss" == "1" ]]; then
  log "temporary_wifi_internet_loss=allowed"
fi
log "+ networksetup -setairportpower $wifi_iface on"
networksetup -setairportpower "$wifi_iface" on >>"$connect_log" 2>&1

log "+ networksetup -setairportnetwork $wifi_iface $ssid <redacted>"
networksetup -setairportnetwork "$wifi_iface" "$ssid" "$passphrase" >>"$connect_log" 2>&1

associated=0
local_ip=""
for _ in $(seq 1 "$timeout"); do
  local_ip="$(ipconfig getifaddr "$wifi_iface" 2>/dev/null || true)"
  if [[ -n "$local_ip" ]]; then
    associated=1
    break
  fi
  sleep 1
done

capture networksetup_wifi_after networksetup -getairportnetwork "$wifi_iface"
capture ifconfig_wifi_after ifconfig "$wifi_iface"
capture ipconfig_wifi_after ipconfig getifaddr "$wifi_iface"
capture route_default_after /sbin/route -n get default
capture route_internet_after /sbin/route -n get 1.1.1.1
capture route_camera_after /sbin/route -n get "$target_ip"
if [[ -x "$airport_tool" ]]; then
  capture airport_info_after "$airport_tool" -I
fi

after_default_iface="$(route_interface default)"
after_internet_iface="$(route_interface 1.1.1.1)"
camera_route_iface="$(route_interface "$target_ip")"

if [[ "$associated" != "1" ]]; then
  echo "Wi-Fi did not obtain an IP within ${timeout}s after joining $ssid" >&2
  echo "session=$session_dir" >&2
  exit 1
fi

if [[ "$allow_wifi_internet_loss" != "1" && "$after_default_iface" != "$before_default_iface" ]]; then
  echo "default route changed from $before_default_iface to $after_default_iface; refusing to continue" >&2
  echo "session=$session_dir" >&2
  exit 1
fi

if [[ "$allow_wifi_internet_loss" != "1" ]] &&
  [[ "$after_internet_iface" != "$before_internet_iface" || "$after_internet_iface" == "$wifi_iface" ]]; then
  echo "internet route changed from $before_internet_iface to $after_internet_iface; refusing to continue" >&2
  echo "session=$session_dir" >&2
  exit 1
fi

if [[ "$camera_route_iface" != "$wifi_iface" ]]; then
  echo "camera route for $target_ip is $camera_route_iface, expected Wi-Fi $wifi_iface" >&2
  echo "session=$session_dir" >&2
  exit 1
fi

if [[ "$skip_ping" != "1" ]]; then
  ping -c 1 -W 1000 "$target_ip" >"$session_dir/ping_camera.txt" 2>&1 || true
fi

{
  printf 'session=%s\n' "$session_dir"
  printf 'associated=present\n'
  printf 'wifi_interface=%s\n' "$wifi_iface"
  printf 'ssid=%s\n' "$ssid"
  printf 'bssid=%s\n' "$bssid"
  printf 'local_ip=%s\n' "$local_ip"
  printf 'target_ip=%s\n' "$target_ip"
  printf 'default_route=%s\n' "$after_default_iface"
  printf 'internet_route=%s\n' "$after_internet_iface"
  printf 'camera_route=%s\n' "$camera_route_iface"
  if [[ "$allow_wifi_internet_loss" == "1" ]]; then
    printf 'internet_mode=temporary_wifi_takeover\n'
    printf 'wifi_internet_loss_allowed=present\n'
  else
    printf 'internet_mode=ethernet_preserved\n'
    printf 'wifi_internet_loss_allowed=absent\n'
  fi
} >"$session_dir/summary.txt"

log "associated=present"
log "local_ip=$local_ip"
log "target_ip=$target_ip"
log "default_route_after=$after_default_iface"
log "internet_route_after=$after_internet_iface"
log "camera_route_after=$camera_route_iface"
log "summary=$session_dir/summary.txt"
