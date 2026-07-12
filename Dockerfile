# ptpsim camera-sim-service: a deployable, multi-architecture PTP/IP responder.
# The public default builder is self-contained. CI overrides BUILDER_IMAGE with
# its versioned toolchain image so routine builds do not reinstall Rust tools.
ARG BUILDER_IMAGE=rust:1.97.0-slim
FROM --platform=$BUILDPLATFORM ${BUILDER_IMAGE} AS chef

SHELL ["/bin/bash", "-o", "pipefail", "-c"]
ARG CARGO_CHEF_VERSION=0.1.77
ARG CARGO_ZIGBUILD_VERSION=0.23.0
ARG ZIG_VERSION=0.14.1
ARG ZIG_X86_64_SHA256=24aeeec8af16c381934a6cd7d95c807a8cb2cf7df9fa40d359aa884195c4716c
ARG ZIG_AARCH64_SHA256=f7a654acc967864f7a050ddacfaa778c7504a0eca8d2b678839c21eea47c992b

# Install tools only for the public builder default. The CI builder already
# contains these exact versions and takes the fast branch.
# hadolint ignore=DL3008
RUN if ! command -v cargo-chef >/dev/null \
      || ! command -v cargo-zigbuild >/dev/null \
      || ! command -v zig >/dev/null; then \
      apt-get update \
      && apt-get install -y --no-install-recommends ca-certificates curl xz-utils \
      && rm -rf /var/lib/apt/lists/* \
      && rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu \
      && cargo install --locked cargo-chef --version "$CARGO_CHEF_VERSION" \
      && cargo install --locked cargo-zigbuild --version "$CARGO_ZIGBUILD_VERSION" \
      && case "$(dpkg --print-architecture)" in \
           amd64) zig_arch=x86_64; zig_sha="$ZIG_X86_64_SHA256" ;; \
           arm64) zig_arch=aarch64; zig_sha="$ZIG_AARCH64_SHA256" ;; \
           *) echo "FATAL: unsupported build architecture" >&2; exit 1 ;; \
         esac \
      && curl -fsSL \
           "https://ziglang.org/download/${ZIG_VERSION}/zig-${zig_arch}-linux-${ZIG_VERSION}.tar.xz" \
           -o /tmp/zig.tar.xz \
      && echo "${zig_sha}  /tmp/zig.tar.xz" | sha256sum -c - \
      && tar -xJf /tmp/zig.tar.xz -C /opt \
      && rm /tmp/zig.tar.xz \
      && ln -s "/opt/zig-${zig_arch}-linux-${ZIG_VERSION}/zig" /usr/local/bin/zig; \
    fi

WORKDIR /build

# cargo-chef 0.1.77 can restrict a recipe to one binary's transitive closure.
# The planner still sees every workspace member for valid Cargo metadata, but
# FFI, UniFFI, and TUI dependencies do not enter the service recipe or cache.
FROM chef AS planner
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY services ./services
COPY packages ./packages
COPY tools ./tools
RUN cargo chef prepare --bin camera-sim-service --recipe-path recipe.json

# All compilation executes on BUILDPLATFORM. TARGETARCH only selects the Rust
# target, so arm64 uses cargo-zigbuild cross-compilation rather than QEMU.
FROM chef AS target
ARG TARGETARCH
RUN case "$TARGETARCH" in \
      amd64) echo "x86_64-unknown-linux-gnu" > /tmp/rust-target ;; \
      arm64) echo "aarch64-unknown-linux-gnu" > /tmp/rust-target ;; \
      *) echo "FATAL: unsupported TARGETARCH=$TARGETARCH" >&2; exit 1 ;; \
    esac

FROM target AS dependencies
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY --from=planner /build/recipe.json .
RUN cargo chef cook --release --zigbuild \
      --target "$(cat /tmp/rust-target)" \
      --bin camera-sim-service \
      --recipe-path recipe.json

FROM dependencies AS builder
COPY crates ./crates
COPY services ./services
COPY packages ./packages
RUN cargo zigbuild --release -p camera-sim-service --bin camera-sim-service \
      --target "$(cat /tmp/rust-target).2.36" \
 && cp "target/$(cat /tmp/rust-target)/release/camera-sim-service" \
      /build/camera-sim-service

# The target runtime contains no RUN instruction. Selecting the arm64 Debian
# rootfs and copying cross-built files therefore executes no arm64 code.
FROM debian:bookworm-slim@sha256:60eac759739651111db372c07be67863818726f754804b8707c90979bda511df AS runtime

ARG SOURCE_COMMIT=unknown
LABEL org.opencontainers.image.source="https://github.com/epsalmond/ptpsim" \
      org.opencontainers.image.revision="$SOURCE_COMMIT"

COPY --from=builder /build/camera-sim-service /usr/local/bin/camera-sim-service
COPY --chmod=0555 ci/container-healthcheck /usr/local/bin/container-healthcheck
COPY packages/camera-config-data/fuji/gfx100ii/gfx100ii.consolidated.yaml /etc/ptpsim/gfx100ii.consolidated.yaml
COPY packages/fixtures/liveview/640x480 /etc/ptpsim/liveview/640x480
COPY --chown=65532:65532 packages/fixtures/.gitkeep /var/lib/ptpsim/media-root/.keep

USER 65532:65532
WORKDIR /var/lib/ptpsim

EXPOSE 55740/tcp 55741/tcp 55742/tcp 15740/tcp 51562/udp 8080/tcp

HEALTHCHECK --interval=10s --timeout=2s --start-period=5s --retries=3 \
  CMD ["/usr/local/bin/container-healthcheck"]

ENTRYPOINT ["camera-sim-service"]
CMD ["--manifest", "/etc/ptpsim/gfx100ii.consolidated.yaml", \
     "--media-root", "/var/lib/ptpsim/media-root", \
     "--profile", "fuji/gfx100ii", \
     "--connection", "app", \
     "--liveview-dir", "/etc/ptpsim/liveview/640x480"]
