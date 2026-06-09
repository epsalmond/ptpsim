#!/usr/bin/env bash
# Linux end-to-end: BLE-launch the camera AP -> join its open Wi-Fi (never-default,
# internet stays on Ethernet) -> run the read-only PTP/IP ISO probe (phase 1).
# Designed to run inside the camera's ~60s AP/search window.
#
# Usage: scripts/run_iso_probe_flow.sh [SESSION_DIR]
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO" || exit 1
PY="$REPO/.venv/bin/python"
WIFI_IFACE="${FUJI_WIFI_IFACE:-wlx00c0cab7f674}"
SSID="${FUJI_AP_SSID:-FUJIFILM-GFX100II-XXXX}"
CAM_IP="${FUJI_CAM_IP:-192.168.0.1}"
GUID="${FUJI_PTPIP_GUID:-f2e4538fada5485d87b27f0bd3d5ded0}"
NAME="${FUJI_PTPIP_NAME:-mbp-7274}"
CON_NAME="fuji-cam-ap"

mkdir -p "$SESSION_DIR"

echo "== [1/4] BLE register(+BLE_PROTOCOL_VERSION 0x0101) + launch AP (function=take=0x0004) =="
PYTHONPATH=. "$PY" scripts/register_launch_linux.py --function "${LAUNCH_FUNCTION:-take}" --device-name "$NAME" \
  --status-file "$SESSION_DIR/ap_launch.json" || { echo "register/AP-launch FAILED"; exit 10; }

echo "== [2/4] join open AP $SSID on $WIFI_IFACE (never-default) =="
sudo -n nmcli con delete "$CON_NAME" >/dev/null 2>&1 || true
sudo -n nmcli con add type wifi con-name "$CON_NAME" ifname "$WIFI_IFACE" ssid "$SSID" \
  ipv4.never-default yes ipv6.never-default yes ipv4.route-metric 9999 >/dev/null \
  || { echo "nmcli con add FAILED (need passwordless sudo for nmcli)"; exit 20; }
sudo -n nmcli con up "$CON_NAME" >/dev/null 2>&1 || { echo "nmcli con up FAILED"; exit 21; }

echo "== [3/4] verify route to $CAM_IP uses Wi-Fi, internet stays on Ethernet =="
sleep 2
ROUTE="$(ip route get "$CAM_IP" 2>/dev/null)"
echo "  route: $ROUTE"
echo "  default: $(ip route show default | head -1)"
echo "$ROUTE" | grep -q "$WIFI_IFACE" || { echo "WARN: route to camera not via $WIFI_IFACE"; }
ip route show default | grep -q "$WIFI_IFACE" && { echo "ABORT: default route moved to Wi-Fi"; exit 22; }

echo "== [4/4] PTP/IP Big-3 probe (phase 1 read-only; extra args via PROBE_ARGS) =="
# shellcheck disable=SC2086
PYTHONPATH=. "$PY" scripts/probe_iso_liveview.py \
  --session-dir "$SESSION_DIR/ptpip" --host "$CAM_IP" --guid "$GUID" --friendly-name "$NAME" \
  ${PROBE_ARGS:-}
rc=$?
echo "== done (probe rc=$rc); session: $SESSION_DIR =="
exit $rc
