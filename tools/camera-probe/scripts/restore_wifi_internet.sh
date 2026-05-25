#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/restore_wifi_internet.sh --ssid SSID [options]

Options:
  --ssid SSID       Internet Wi-Fi SSID to restore. Required.
  --iface IFACE     Wi-Fi interface. Default: detected Wi-Fi/AirPort device.
  --timeout SEC     Seconds to wait for verified internet. Default: 90.
  --ping-host HOST  Host used for internet verification. Default: 1.1.1.1.
  --session-dir DIR Write logs into DIR. Default: rce/sessions/wifi_restore_<timestamp>.
  -h, --help        Show this help.

This script is for recovery after temporary camera-AP Wi-Fi takeover. It retries
joining the internet SSID, records evidence, and exits only after ping
reachability is verified or the timeout expires. macOS can report "not
associated" even when IP and ping are already working, so ping is authoritative.
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

ssid=""
iface=""
timeout="${FUJI_RESTORE_WIFI_TIMEOUT:-90}"
ping_host="${FUJI_RESTORE_WIFI_PING_HOST:-1.1.1.1}"
session_dir=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --ssid)
      ssid="$2"
      shift 2
      ;;
    --iface)
      iface="$2"
      shift 2
      ;;
    --timeout)
      timeout="$2"
      shift 2
      ;;
    --ping-host)
      ping_host="$2"
      shift 2
      ;;
    --session-dir)
      session_dir="$2"
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

if [[ -z "$ssid" ]]; then
  echo "missing required --ssid" >&2
  usage >&2
  exit 2
fi

detect_wifi_iface() {
  networksetup -listallhardwareports |
    awk '
      /^Hardware Port: (Wi-Fi|AirPort)$/ {want=1; next}
      want && /^Device: / {print $2; exit}
    '
}

if [[ -z "$iface" ]]; then
  iface="$(detect_wifi_iface)"
fi
if [[ -z "$iface" ]]; then
  echo "could not detect Wi-Fi interface" >&2
  exit 1
fi

if [[ -z "$session_dir" ]]; then
  timestamp="$(date -u +%Y%m%dT%H%M%SZ)"

fi
mkdir -p "$session_dir"

summary="$session_dir/summary.txt"
restore_log="$session_dir/restore_wifi.log"
ping_log="$session_dir/ping_internet.txt"

log() {
  printf '%s\n' "$*" | tee -a "$summary" >&2
}

capture() {
  local label="$1"
  shift
  "$@" >"$session_dir/$label.txt" 2>&1 || true
}

log "session=$session_dir"
log "wifi_interface=$iface"
log "restore_ssid=$ssid"
log "timeout=$timeout"
log "ping_host=$ping_host"

networksetup -setairportpower "$iface" on >>"$restore_log" 2>&1 || true

verified=0
for attempt in $(seq 1 "$timeout"); do
  if [[ "$attempt" == "1" || $((attempt % 5)) == "0" ]]; then
    {
      printf 'restore_attempt=%s\n' "$attempt"
      networksetup -setairportnetwork "$iface" "$ssid"
    } >>"$restore_log" 2>&1 || true
  fi

  current_network="$(networksetup -getairportnetwork "$iface" 2>/dev/null || true)"
  current_ip="$(ipconfig getifaddr "$iface" 2>/dev/null || true)"
  {
    printf 'restore_attempt=%s\n' "$attempt"
    printf 'networksetup_status=%s\n' "$current_network"
    printf 'ip=%s\n' "$current_ip"
  } >>"$restore_log"

  if ping -c 1 -W 1000 "$ping_host" >>"$ping_log" 2>&1; then
    log "restored_wifi_ip=${current_ip:-unknown}"
    log "internet_after_restore=present"
    verified=1
    break
  fi
  sleep 1
done

capture networksetup_wifi_after_restore networksetup -getairportnetwork "$iface"
capture ifconfig_after_restore ifconfig "$iface"
capture route_default_after_restore /sbin/route -n get default
capture route_internet_after_restore /sbin/route -n get "$ping_host"

if [[ "$verified" != "1" ]]; then
  log "internet_after_restore=absent"
  log "restore_wifi_log=$restore_log"
  exit 9
fi

exit 0
