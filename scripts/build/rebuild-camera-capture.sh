#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/build/rebuild-camera-capture.sh

Deletes the camera-capture build target and rebuilds it from source.
USAGE
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

if [[ $# -ne 0 ]]; then
  echo "unexpected arguments: $*" >&2
  usage >&2
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
target="$repo_root/build/camera_capture"

echo "+ rm -rf $target" >&2
rm -rf "$target"

exec "$repo_root/scripts/build/build-camera-capture.sh" --force
