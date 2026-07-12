#!/bin/bash
set -euo pipefail

: "${IMAGE_REF:?IMAGE_REF is required}"
: "${EXPECTED_ARCH:?EXPECTED_ARCH is required}"
: "${EXPECTED_REVISION:?EXPECTED_REVISION is required}"

config="$(regctl image inspect "$IMAGE_REF")"
jq -e --arg arch "$EXPECTED_ARCH" --arg revision "$EXPECTED_REVISION" '
  .architecture == $arch
  and .os == "linux"
  and .config.Labels["org.opencontainers.image.revision"] == $revision
  and .config.Labels["org.opencontainers.image.source"]
    == "https://github.com/epsalmond/ptpsim"
  and .config.User == "65532:65532"
  and .config.Entrypoint == ["camera-sim-service"]
  and (.config.ExposedPorts | has("55740/tcp"))
  and (.config.ExposedPorts | has("8080/tcp"))
  and .config.Healthcheck.Test == ["CMD", "/usr/local/bin/container-healthcheck"]
' <<<"$config" >/dev/null

tmpdir="$(mktemp -d)"
binary="$tmpdir/camera-sim-service"
trap 'rm -rf "$tmpdir"' EXIT
regctl image get-file "$IMAGE_REF" /usr/local/bin/camera-sim-service "$binary"
magic="$(od -An -tx1 -N4 "$binary" | tr -d ' \n')"
read -r machine_low machine_high < <(od -An -tu1 -j18 -N2 "$binary")
machine="$machine_low $machine_high"
printf 'ELF header: magic=%s e_machine=%s\n' "$magic" "$machine"
[[ "$magic" == 7f454c46 ]]
case "$EXPECTED_ARCH" in
  amd64) [[ "$machine" == "62 0" ]] ;;
  arm64) [[ "$machine" == "183 0" ]] ;;
  *) printf 'unsupported ELF architecture %s\n' "$EXPECTED_ARCH" >&2; exit 1 ;;
esac

printf 'verified %s as linux/%s with ELF e_machine %s\n' \
  "$IMAGE_REF" "$EXPECTED_ARCH" "$machine"
