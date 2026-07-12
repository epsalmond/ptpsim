#!/bin/sh
set -eu

: "${IMAGE_REF:?IMAGE_REF is required}"
: "${REGISTRY:?REGISTRY is required}"
: "${REGISTRY_USERNAME:?REGISTRY_USERNAME is required}"
: "${REGISTRY_PASSWORD:?REGISTRY_PASSWORD is required}"
: "${CI_COMMIT_SHA:?CI_COMMIT_SHA is required}"
# An orchestrating repo builds a ptpsim revision that differs from its own
# CI_COMMIT_SHA; it passes the built revision explicitly.
expected_revision="${EXPECTED_REVISION:-$CI_COMMIT_SHA}"
# Fixtures live in the ptpsim checkout, which an orchestrating repo places in
# a subdirectory of its workspace — resolve them from this script's location,
# not from CI_WORKSPACE.
source_root="$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)"

container="ptpsim-smoke-${CI_PIPELINE_NUMBER:-local}"
dockerd_log=/tmp/ptpsim-dockerd.log

cleanup() {
  docker rm -f "$container" >/dev/null 2>&1 || true
  if [ -n "${dockerd_pid:-}" ]; then
    kill "$dockerd_pid" >/dev/null 2>&1 || true
    wait "$dockerd_pid" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT INT TERM

dockerd-entrypoint.sh >"$dockerd_log" 2>&1 &
dockerd_pid=$!
for _ in $(seq 1 60); do
  docker info >/dev/null 2>&1 && break
  sleep 1
done
if ! docker info >/dev/null 2>&1; then
  cat "$dockerd_log" >&2
  exit 1
fi

printf '%s' "$REGISTRY_PASSWORD" | docker login "$REGISTRY" \
  --username "$REGISTRY_USERNAME" --password-stdin
docker pull "$IMAGE_REF"

architecture="$(docker image inspect --format '{{.Architecture}}' "$IMAGE_REF")"
runtime_user="$(docker image inspect --format '{{.Config.User}}' "$IMAGE_REF")"
exposed_ports="$(docker image inspect --format '{{json .Config.ExposedPorts}}' "$IMAGE_REF")"
revision="$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$IMAGE_REF")"
printf 'image metadata: arch=%s user=%s revision=%s ports=%s\n' \
  "$architecture" "$runtime_user" "$revision" "$exposed_ports"
[ "$architecture" = amd64 ]
[ "$runtime_user" = 65532:65532 ]
[ "$revision" = "$expected_revision" ]
case "$exposed_ports" in *'"55740/tcp"'*) ;; *) exit 1 ;; esac
case "$exposed_ports" in *'"8080/tcp"'*) ;; *) exit 1 ;; esac

docker run -d --name "$container" \
  -v "$source_root/packages/fixtures/startup-state/gfx100ii-iso-2000.yaml:/fixtures/startup.yaml:ro" \
  "$IMAGE_REF" \
    --manifest /etc/ptpsim/gfx100ii.consolidated.yaml \
    --media-root /var/lib/ptpsim/media-root \
    --profile fuji/gfx100ii \
    --connection app \
    --startup-state /fixtures/startup.yaml \
    --command-bind '[::]:55740' \
    --control-bind '0.0.0.0:8080' \
    --liveview-dir /etc/ptpsim/liveview/640x480 >/dev/null

status=starting
for _ in $(seq 1 30); do
  status="$(docker inspect --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}missing{{end}}' "$container")"
  [ "$status" = healthy ] && break
  [ "$status" = unhealthy ] && break
  sleep 1
done
if [ "$status" != healthy ]; then
  docker logs "$container" >&2
  exit 1
fi

docker exec "$container" /usr/local/bin/container-healthcheck
docker exec "$container" /usr/local/bin/container-healthcheck \
  127.0.0.1 8080 /state '"0xd02a":2000'
docker exec "$container" /bin/bash -c 'exec 3<>/dev/tcp/127.0.0.1/55740'

docker kill --signal TERM "$container" >/dev/null
[ "$(docker wait "$container")" = 0 ]
