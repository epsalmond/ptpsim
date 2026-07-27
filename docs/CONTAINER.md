---
description: The camera-sim-service container image — supported platforms, runtime port contract, and publication guidance.
status: reference
read-when: Building, publishing, or running the simulator container, or wiring it into integration tests.
---

# Container image

`camera-sim-service` is the deployable PTP/IP responder. The project image
supports `linux/amd64` and `linux/arm64`; the arm64 binary is cross-compiled,
not compiled under CPU emulation.

The repository does not designate a registry or publication policy. Image
publishers should build an exact source revision, preserve its full commit in
the `org.opencontainers.image.revision` label, and give consumers an immutable
reference such as a digest.

## Runtime contract

The default command starts the baked GFX100 II app-connection profile. The
image exposes the following ports; a listener is active only when the selected
manifest connection and command-line options require it.

| Port | Transport | Role |
| --- | --- | --- |
| `55740` | TCP | App PTP/IP command |
| `55741` | TCP | App PTP/IP events |
| `55742` | TCP | App live view |
| `15740` | TCP | PCSS PTP/IP command |
| `51562` | UDP | Optional PCSS discovery knock |
| `8080` | TCP | Control HTTP |

The process runs as numeric uid/gid `65532`. `GET /healthz` returns HTTP 200
after the service is ready. The image healthcheck calls that endpoint on its
default loopback control bind.

The default control bind is container-local. Publish the port and override the
bind when the host or another container must reach it:

```sh
docker run --rm \
  -p 127.0.0.1:55740:55740 \
  -p 127.0.0.1:8080:8080 \
  ptpsim:local \
    --manifest /etc/ptpsim/gfx100ii.consolidated.yaml \
    --media-root /var/lib/ptpsim/media-root \
    --profile fuji/gfx100ii \
    --connection app \
    --command-bind '[::]:55740' \
    --control-bind '0.0.0.0:8080' \
    --liveview-dir /etc/ptpsim/liveview/640x480
```

## Startup state

`--startup-state` accepts a read-only bind-mounted YAML or JSON state overlay.
The file is applied before any listener begins serving traffic. The public
schema is `ptpsim-startup-state/v1`; the repository fixture at
`packages/fixtures/startup-state/gfx100ii-iso-2000.yaml` is a runnable example.

```sh
docker run --rm \
  -v "$PWD/packages/fixtures/startup-state/gfx100ii-iso-2000.yaml:/fixtures/startup.yaml:ro" \
  ptpsim:local \
    --manifest /etc/ptpsim/gfx100ii.consolidated.yaml \
    --media-root /var/lib/ptpsim/media-root \
    --profile fuji/gfx100ii \
    --connection app \
    --startup-state /fixtures/startup.yaml \
    --liveview-dir /etc/ptpsim/liveview/640x480
```

## Local build

The Dockerfile remains self-contained for public contributors:

```sh
docker build -t ptpsim:local .
```

Publishers may supply a compatible prebuilt toolchain image through the
`BUILDER_IMAGE` build argument, but that optimization is not required for a
local build.
