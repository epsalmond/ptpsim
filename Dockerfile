# ptpsim camera-sim-service — the deployable PTP/IP responder image. The

# workspace, no native deps.

# --- builder (native, cross-compiles) ---------------------------------------
# The builder runs on the NATIVE build platform (`--platform=$BUILDPLATFORM`)
# and CROSS-compiles to the target arch with cargo-zigbuild (zig as the
# cross-linker). This avoids QEMU emulation of the Rust compiler — emulating the
# arm64 compile is what made this step ~30 min (the arm64 `cargo chef cook` ran
# ~1670s emulated vs ~278s native amd64). cargo-chef still splits the dependency
# cook (cached, keyed on recipe.json) from the workspace compile, so a code-only
# change recompiles workspace crates only and a docs/data change rebuilds nothing.
# Base image + glibc floor (2.36) are pinned to match the bookworm runtime and
# rust-toolchain.toml.
FROM --platform=$BUILDPLATFORM rust:1.96.0-slim AS chef
SHELL ["/bin/bash", "-o", "pipefail", "-c"]
ARG ZIG_VERSION=0.14.1
# hadolint ignore=DL3008
RUN apt-get update \
 && apt-get install -y --no-install-recommends curl xz-utils ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu \
 && cargo install --locked cargo-chef cargo-zigbuild \
 && curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/zig-x86_64-linux-${ZIG_VERSION}.tar.xz" \
      | tar -xJ -C /opt \
 && ln -s "/opt/zig-x86_64-linux-${ZIG_VERSION}/zig" /usr/local/bin/zig
WORKDIR /build

FROM chef AS planner
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY services ./services
COPY packages ./packages
COPY tools/camera-simctl ./tools/camera-simctl
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
# buildx provides TARGETARCH (amd64|arm64); map it to the rust gnu triple. The
# `.2.36` glibc floor (debian:bookworm-slim ships glibc 2.36) is a zigbuild
# link-time hint; the artifacts still land under target/<triple>/.
ARG TARGETARCH
RUN case "$TARGETARCH" in \
      amd64) echo "x86_64-unknown-linux-gnu"  > /tmp/rust-target ;; \
      arm64) echo "aarch64-unknown-linux-gnu" > /tmp/rust-target ;; \
      *) echo "FATAL: unsupported TARGETARCH=$TARGETARCH" >&2; exit 1 ;; \
    esac
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY --from=planner /build/recipe.json .
# Cook deps for the target arch (cross-compiled via zig) — the cached layer.
RUN cargo chef cook --release --zigbuild \
      --target "$(cat /tmp/rust-target).2.36" --recipe-path recipe.json

# Only the workspace inputs the service needs.
COPY crates ./crates
COPY services ./services
COPY packages ./packages
# tools/camera-simctl is a workspace member — cargo needs it present even when
# building -p camera-sim-service. Tiny (~20K), keeps the workspace evaluable.
COPY tools/camera-simctl ./tools/camera-simctl

# No explicit strip: [profile.release] strip = true already covers it. Copy the
# cross-built binary to a stable arch-independent path for the runtime COPY.
RUN cargo zigbuild --release -p camera-sim-service --bin camera-sim-service \
      --target "$(cat /tmp/rust-target).2.36" \
 && cp "target/$(cat /tmp/rust-target)/release/camera-sim-service" /build/camera-sim-service

# --- runtime (per-arch) -----------------------------------------------------
FROM debian:bookworm-slim AS runtime

# curl for HEALTHCHECK; ca-certificates for any future HTTPS reads.
# DL3008 (pin apt versions) intentionally skipped via the directive below:
# ca-certificates and curl are stable Debian base packages that receive monthly
# security updates; pinning would force unpredictable rebuild breakage with no
# security benefit.
# hadolint ignore=DL3008
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 65532 --gid nogroup --no-create-home --shell /usr/sbin/nologin ptpsim \
 && mkdir -p /etc/ptpsim /var/lib/ptpsim/media-root \
 && chown -R ptpsim:nogroup /var/lib/ptpsim

COPY --from=builder /build/camera-sim-service /usr/local/bin/camera-sim-service
COPY packages/camera-config-data/fuji/gfx100ii/gfx100ii.consolidated.yaml /etc/ptpsim/gfx100ii.consolidated.yaml
# Default live-view corpus: 30 looped JPEG frames (~880 KB), JFIF-only (no EXIF).
# Operators can override with --liveview-dir /path/to/other/frames.
COPY packages/fixtures/liveview/640x480 /etc/ptpsim/liveview/640x480

USER ptpsim:nogroup
WORKDIR /var/lib/ptpsim

# PTP/IP listeners (55740 command / 55741 event / 55742 live-view) + control HTTP.
# All bind addresses are CLI-overridable; control defaults to 127.0.0.1:8080.
EXPOSE 55740/tcp 55741/tcp 55742/tcp 8080/tcp

# Healthcheck hits /healthz on the default control bind. If you override
# --control-bind, override this too (or pass --health-cmd= at docker run).
HEALTHCHECK --interval=10s --timeout=2s --start-period=5s --retries=3 \
  CMD curl -fsS http://127.0.0.1:8080/healthz || exit 1

ENTRYPOINT ["camera-sim-service"]
CMD ["--manifest", "/etc/ptpsim/gfx100ii.consolidated.yaml", \
     "--media-root", "/var/lib/ptpsim/media-root", \
     "--profile", "fuji/gfx100ii", \
     "--liveview-dir", "/etc/ptpsim/liveview/640x480"]
