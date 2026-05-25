#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/build/build-ble-identity-advertiser.sh [--force]

Builds the macOS CoreBluetooth Local Name advertiser helper and prints the
binary path.
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
src="$repo_root/macos/ble_identity_advertiser/IdentityAdvertiser.swift"
build_dir="$repo_root/.build"
module_cache="$repo_root/.build/swift-module-cache"
binary="$build_dir/macos_ble_identity_advertiser"

if ! command -v xcrun >/dev/null 2>&1; then
  echo "xcrun is required to build the BLE identity advertiser helper" >&2
  exit 1
fi

mkdir -p "$build_dir" "$module_cache"
if [[ "$force" == "1" || ! -x "$binary" || "$src" -nt "$binary" ]]; then
  echo "+ xcrun swiftc -module-cache-path $module_cache -framework CoreBluetooth -framework Foundation $src -o $binary" >&2
  xcrun swiftc \
    -module-cache-path "$module_cache" \
    -framework CoreBluetooth \
    -framework Foundation \
    "$src" \
    -o "$binary"
fi

echo "$binary"
