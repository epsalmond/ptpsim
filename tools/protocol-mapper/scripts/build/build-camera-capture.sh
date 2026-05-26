#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/build/build-camera-capture.sh [--force] [--app-path]

Builds the macOS AVFoundation Continuity Camera capture helper and prints the
binary path, or the app bundle path with --app-path.
USAGE
}

force=0
print_app_path=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --force)
      force=1
      shift
      ;;
    --app-path)
      print_app_path=1
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
src="$repo_root/macos/camera_capture/CameraCapture.swift"
plist="$repo_root/macos/camera_capture/Info.plist"
build_dir="$repo_root/build/camera_capture"
module_cache="$repo_root/.build/swift-module-cache"
app="$build_dir/FujiCameraCapture.app"
contents="$app/Contents"
macos_dir="$contents/MacOS"
resources_dir="$contents/Resources"
binary="$macos_dir/FujiCameraCapture"

if ! command -v xcrun >/dev/null 2>&1; then
  echo "xcrun is required to build the macOS capture helper" >&2
  exit 1
fi

mkdir -p "$build_dir" "$module_cache" "$macos_dir" "$resources_dir"
if [[ "$force" == "1" || ! -x "$binary" || "$src" -nt "$binary" || "$plist" -nt "$binary" ]]; then
  cp "$plist" "$contents/Info.plist"
  printf 'APPL????' > "$contents/PkgInfo"
  echo "+ xcrun swiftc -module-cache-path $module_cache $src -o $binary" >&2
  xcrun swiftc \
    -module-cache-path "$module_cache" \
    "$src" \
    -o "$binary" \
    -framework AVFoundation \
    -framework AppKit \
    -framework CoreImage
  codesign --force --deep --sign - "$app" >/dev/null
fi

if [[ "$print_app_path" == "1" ]]; then
  echo "$app"
else
  echo "$binary"
fi
