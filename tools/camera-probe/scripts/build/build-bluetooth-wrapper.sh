#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/build/build-bluetooth-wrapper.sh [--wrapper-only | --probe-only] [--force]

Builds the bluetooth-wrapper binary and/or the bt-local probe helper. Prints the
built binary paths.
USAGE
}

build_wrapper=1
build_probe=1
force=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --wrapper-only)
      build_wrapper=1
      build_probe=0
      shift
      ;;
    --probe-only)
      build_wrapper=0
      build_probe=1
      shift
      ;;
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
wrapper_dir="$repo_root/bluetooth-wrapper"
build_dir="$repo_root/.build/bluetooth-wrapper"
probe_src="$wrapper_dir/tools/bt_probe.m"
probe_binary="$build_dir/bt-probe"

if ! command -v xcrun >/dev/null 2>&1; then
  echo "xcrun is required to build bluetooth-wrapper helpers" >&2
  exit 1
fi

if [[ "$build_wrapper" == "1" ]]; then
  if [[ "$force" == "1" ]]; then
    make -C "$wrapper_dir" clean
  fi
  make -C "$wrapper_dir"
  echo "$wrapper_dir/bluetooth-wrapper"
fi

if [[ "$build_probe" == "1" ]]; then
  mkdir -p "$build_dir"
  if [[ "$force" == "1" || ! -x "$probe_binary" || "$probe_src" -nt "$probe_binary" ]]; then
    echo "+ xcrun clang -fobjc-arc -Wall -Wextra -Werror $probe_src -o $probe_binary" >&2
    xcrun clang \
      -fobjc-arc \
      -Wall \
      -Wextra \
      -Werror \
      -framework Foundation \
      -framework CoreBluetooth \
      -framework IOBluetooth \
      "$probe_src" \
      -o "$probe_binary"
  fi
  echo "$probe_binary"
fi
