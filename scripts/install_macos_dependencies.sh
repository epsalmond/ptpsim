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

if ! brew list --versions libusb >/dev/null 2>&1; then
  echo "+ brew install libusb" >&2
  brew install libusb
else
  echo "libusb already installed: $(brew --prefix libusb)" >&2
fi

echo "+ blueutil --version" >&2
blueutil --version

echo "+ ffmpeg -version" >&2
ffmpeg -version | head -n 1

echo "+ pkg-config --exists libusb-1.0" >&2
if command -v pkg-config >/dev/null 2>&1 && pkg-config --exists libusb-1.0; then
  pkg-config --modversion libusb-1.0
else
  echo "libusb installed; pkg-config metadata unavailable" >&2
fi

echo "+ tesseract --version" >&2
tesseract --version | head -n 1
