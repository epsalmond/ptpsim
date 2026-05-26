#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/build/build-all.sh [--force]

Builds all local native helper binaries used by the project.
USAGE
}

force=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --force)
      force=1
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

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

if [[ "$force" == "1" ]]; then
  scripts/build/build-camera-capture.sh --force
  scripts/build/build-ble-identity-advertiser.sh --force
  scripts/build/build-bluetooth-wrapper.sh --force
else
  scripts/build/build-camera-capture.sh
  scripts/build/build-ble-identity-advertiser.sh
  scripts/build/build-bluetooth-wrapper.sh
fi
