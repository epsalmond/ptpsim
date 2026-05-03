#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/install_macos_dependencies.sh

Installs macOS system dependencies that make Fuji BLE and camera-screen
evidence workflows scriptable.
Requires Homebrew.
USAGE
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

if ! command -v brew >/dev/null 2>&1; then
  cat >&2 <<'MSG'
Homebrew is required to install macOS system dependencies.
Install Homebrew first: https://brew.sh/
MSG
  exit 1
fi

for formula in blueutil ffmpeg tesseract; do
  if ! command -v "$formula" >/dev/null 2>&1; then
    echo "+ brew install $formula" >&2
    brew install "$formula"
  else
    echo "$formula already installed: $(command -v "$formula")" >&2
  fi
done

echo "+ blueutil --version" >&2
blueutil --version

echo "+ ffmpeg -version" >&2
ffmpeg -version | head -n 1

echo "+ tesseract --version" >&2
tesseract --version | head -n 1
