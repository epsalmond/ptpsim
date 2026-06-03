#!/usr/bin/env bash
#
# Render a ready-to-paste SPM `binaryTarget(...)` block for a given release
# tag of epsalmond/ptpsim. Fetches the `.checksum` asset (SHA-256 hex of the
# `.xcframework.zip`) and emits the snippet to stdout.
#
# Usage:
#   ./ci/spm-snippet.sh sha-cded3f95
#
# Requirements: `gh` CLI authenticated against the repo (`gh auth status`).
#
# Design notes: docs/SPM_INTEGRATION.md (consumer integration story).

set -euo pipefail

REPO="${REPO:-epsalmond/ptpsim}"
NAME="CameraProtocolFFI"

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <release-tag>" >&2
    echo "       e.g. $0 sha-cded3f95" >&2
    exit 2
fi
TAG="$1"

# Strip the `sha-` prefix to get the 8-char SHA used in the asset filename.
SHA8="${TAG#sha-}"

command -v gh >/dev/null 2>&1 || {
    echo "FATAL: gh CLI not installed; install + authenticate against $REPO" >&2
    exit 1
}

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

# Fetch just the checksum asset; pattern matches both legacy and current naming.
if ! gh release download "$TAG" \
        --repo "$REPO" \
        --pattern "${NAME}-${SHA8}.checksum" \
        --dir "$TMPDIR" 2>/dev/null; then
    echo "FATAL: could not fetch ${NAME}-${SHA8}.checksum from release $TAG of $REPO" >&2
    echo "       (release exists? gh authenticated? asset uploaded?)" >&2
    exit 1
fi

CHECKSUM=$(cat "${TMPDIR}/${NAME}-${SHA8}.checksum")

cat <<EOF
.binaryTarget(
    name: "${NAME}",
    url: "https://github.com/${REPO}/releases/download/${TAG}/${NAME}-${SHA8}.xcframework.zip",
    checksum: "${CHECKSUM}"
)
EOF
