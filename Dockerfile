# ptpsim camera-sim-service — the deployable PTP/IP responder image. The

# workspace, no native deps → simple two-stage build.

# --- builder ----------------------------------------------------------------
# cargo-chef splits the build into a dependency layer (cook, keyed on the
# content-addressed recipe.json — survives any code-only change) and the
# workspace compile. Combined with the buildx registry cache
# (cache_images in .woodpecker/linux.yml), a code change only recompiles
# workspace crates; a docs/data change rebuilds nothing.
FROM rust:1-slim AS chef
RUN cargo install --locked cargo-chef
WORKDIR /build

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY services ./services
COPY packages ./packages
COPY tools/camera-simctl ./tools/camera-simctl
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /build/recipe.json .
RUN cargo chef cook --release --recipe-path recipe.json

# Only the workspace inputs the service needs.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY services ./services
COPY packages ./packages
# tools/camera-simctl is a workspace member — cargo needs it present even when
# building -p camera-sim-service. Tiny (~20K), keeps the workspace evaluable.
COPY tools/camera-simctl ./tools/camera-simctl

# No explicit strip: [profile.release] strip = true already covers it.
RUN cargo build --release -p camera-sim-service --bin camera-sim-service

# --- runtime ----------------------------------------------------------------
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

COPY --from=builder /build/target/release/camera-sim-service /usr/local/bin/camera-sim-service
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
