#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/macos_pairing_identity.sh show
  scripts/macos_pairing_identity.sh save [--backup PATH]
  scripts/macos_pairing_identity.sh set --name NAME [--backup PATH] [--admin-dialog]
  scripts/macos_pairing_identity.sh restore [--backup PATH] [--admin-dialog]

Temporarily changes the macOS ComputerName and LocalHostName used as the
Bluetooth-visible identity during pairing. This changes local system settings
and should be restored after the camera creates the desired registration entry.

NAME must contain only ASCII letters, digits, and hyphens.
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

backup="rce/state/macos_pairing_identity.env"
command="${1:-}"
if [[ $# -gt 0 ]]; then
  shift
fi
name=""
admin_dialog=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --backup)
      backup="$2"
      shift 2
      ;;
    --name)
      name="$2"
      shift 2
      ;;
    --admin-dialog)
      admin_dialog=1
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

get_scutil_value() {
  scutil --get "$1" 2>/dev/null || true
}

encode_b64() {
  python3 -c 'import base64,sys; print(base64.b64encode(sys.argv[1].encode()).decode())' "$1"
}

decode_b64() {
  python3 -c 'import base64,sys; print(base64.b64decode(sys.argv[1]).decode())' "$1"
}

shell_quote() {
  python3 -c 'import shlex,sys; print(shlex.quote(sys.argv[1]))' "$1"
}

applescript_quote() {
  python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$1"
}

run_scutil_set() {
  local key="$1"
  local value="$2"
  if [[ "$admin_dialog" == "1" ]]; then
    local shell_script
    shell_script="scutil --set $(shell_quote "$key") $(shell_quote "$value")"
    osascript -e "do shell script $(applescript_quote "$shell_script") with administrator privileges"
  else
    sudo scutil --set "$key" "$value"
  fi
}

save_backup() {
  mkdir -p "$(dirname "$backup")"
  local computer_name local_host_name host_name
  computer_name="$(get_scutil_value ComputerName)"
  local_host_name="$(get_scutil_value LocalHostName)"
  host_name="$(get_scutil_value HostName)"
  {
    printf 'COMPUTER_NAME_B64=%s\n' "$(encode_b64 "$computer_name")"
    printf 'LOCAL_HOST_NAME_B64=%s\n' "$(encode_b64 "$local_host_name")"
    printf 'HOST_NAME_B64=%s\n' "$(encode_b64 "$host_name")"
  } > "$backup"
  echo "saved backup=$backup"
}

show_identity() {
  printf 'ComputerName=%s\n' "$(get_scutil_value ComputerName)"
  printf 'LocalHostName=%s\n' "$(get_scutil_value LocalHostName)"
  printf 'HostName=%s\n' "$(get_scutil_value HostName)"
}

set_pairing_name() {
  if [[ -z "$name" ]]; then
    echo "--name is required" >&2
    exit 2
  fi
  if [[ ! "$name" =~ ^[A-Za-z0-9-]+$ ]]; then
    echo "invalid name: use only ASCII letters, digits, and hyphens" >&2
    exit 2
  fi
  if [[ ${#name} -gt 63 ]]; then
    echo "invalid name: must be 63 characters or fewer" >&2
    exit 2
  fi
  if [[ ! -f "$backup" ]]; then
    save_backup
  fi
  run_scutil_set ComputerName "$name"
  run_scutil_set LocalHostName "$name"
  echo "set ComputerName and LocalHostName to $name"
}

restore_identity() {
  if [[ ! -f "$backup" ]]; then
    echo "missing backup: $backup" >&2
    exit 1
  fi
  # shellcheck disable=SC1090
  source "$backup"
  local computer_name local_host_name host_name
  computer_name="$(decode_b64 "${COMPUTER_NAME_B64:-}")"
  local_host_name="$(decode_b64 "${LOCAL_HOST_NAME_B64:-}")"
  host_name="$(decode_b64 "${HOST_NAME_B64:-}")"
  if [[ -n "$computer_name" ]]; then
    run_scutil_set ComputerName "$computer_name"
  fi
  if [[ -n "$local_host_name" ]]; then
    run_scutil_set LocalHostName "$local_host_name"
  fi
  if [[ -n "$host_name" ]]; then
    run_scutil_set HostName "$host_name"
  fi
  echo "restored identity from $backup"
}

case "$command" in
  show)
    show_identity
    ;;
  save)
    save_backup
    ;;
  set)
    set_pairing_name
    ;;
  restore)
    restore_identity
    ;;
  -h|--help|"")
    usage
    ;;
  *)
    echo "unknown command: $command" >&2
    usage >&2
    exit 2
    ;;
esac
