#!/usr/bin/env bash
#
# Rewrite the repo-root Package.swift to point at a specific release's
# CameraProtocolFFI.xcframework.zip + checksum. Driven by the
# bump-package-swift step of .woodpecker/xcframework.yml AFTER the release
# upload, and runnable locally for one-off bumps.
#
# Usage:
#   ./ci/update-package-swift.sh sha-cded3f95
#
# Requires: `gh` CLI authenticated against epsalmond/ptpsim (the release must
# have its `*.checksum` asset already uploaded). Mutates Package.swift in
# place; the caller is responsible for committing the change.
#
# Design notes: docs/SPM_INTEGRATION.md (consumer flow + chicken-and-egg).

set -euo pipefail

REPO="${REPO:-epsalmond/ptpsim}"
NAME="CameraProtocolFFI"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PKG="${ROOT}/Package.swift"

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <release-tag>" >&2
    echo "       e.g. $0 sha-cded3f95" >&2
    exit 2
fi
TAG="$1"
SHA8="${TAG#sha-}"

command -v gh >/dev/null 2>&1 || {
    echo "FATAL: gh CLI not installed; install + authenticate against $REPO" >&2
    exit 1
}

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

if ! gh release download "$TAG" \
        --repo "$REPO" \
        --pattern "${NAME}-${SHA8}.checksum" \
        --dir "$TMPDIR" 2>/dev/null; then
    echo "FATAL: could not fetch ${NAME}-${SHA8}.checksum from $TAG of $REPO" >&2
    exit 1
fi

CHECKSUM=$(cat "${TMPDIR}/${NAME}-${SHA8}.checksum")
URL="https://github.com/${REPO}/releases/download/${TAG}/${NAME}-${SHA8}.xcframework.zip"

# Validate the checksum looks like 64 hex chars before we rewrite the file —
# don't corrupt Package.swift with garbage.
if [[ ! "$CHECKSUM" =~ ^[0-9a-f]{64}$ ]]; then
    echo "FATAL: checksum is not 64 hex chars: $CHECKSUM" >&2
    exit 1
fi

# Rewrite the url: and checksum: lines in the .binaryTarget block. The pattern
# matches the URL line ending in ".xcframework.zip\"," and the checksum line
# ending in a 64-char-hex string. Anchored to whitespace + quotes so we don't
# accidentally rewrite anything else.
sed -i.bak \
    -e "s|url: \"https://github.com/${REPO}/releases/download/[^\"]*\"|url: \"${URL}\"|" \
    -e "s|checksum: \"[0-9a-f]\{64\}\"|checksum: \"${CHECKSUM}\"|" \
    "$PKG"
rm -f "${PKG}.bak"

echo "Package.swift updated:"
echo "  tag:      ${TAG}"
echo "  url:      ${URL}"
echo "  checksum: ${CHECKSUM}"
