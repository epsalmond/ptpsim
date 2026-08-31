# CI toolchain image for the Rust workspace and python-wheel steps.
# The final stage bakes Cargo registry sources and dependency-only artifacts for
# the whole workspace. Consumers must use CARGO_HOME=/ci-cache/cargo-home,
# CARGO_TARGET_DIR=/ci-cache/target, and CARGO_INCREMENTAL=0 to reuse them.
# The chef skeleton copies .cargo/config.toml, so scripts/rustc-wrapper is
# required during cook. The cook runs under /opt/ptpsim-cook, not under
# /woodpecker: the CI workspace volume mounts over /woodpecker at step
# runtime, and Kaniko skips mounted paths when it snapshots layers. Cargo
# 1.97 does not hash the workspace directory for registry dependencies, so
# only the /ci-cache paths must match CI. The deployment owner's CI builds and publishes this
# image and supplies it to .woodpecker/linux.yml via RUST_TOOLCHAIN_IMAGE.

FROM rust:1-slim AS base

RUN rustup component add rustfmt clippy

# Tool image tracks Debian's current packages by design (same stance as the
# service Dockerfile); pinning would rot faster than it protects.
# hadolint ignore=DL3008
RUN apt-get update -qq \
 && apt-get install -qq -y --no-install-recommends \
      git curl ca-certificates zip python3 python3-pip python3-venv \
 && curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
      -o /usr/share/keyrings/githubcli-archive-keyring.gpg \
 && echo "deb [signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
      > /etc/apt/sources.list.d/github-cli.list \
 && apt-get update -qq \
 && apt-get install -qq -y --no-install-recommends gh \
 && rm -rf /var/lib/apt/lists/*

# Wheel build deps live in a venv. PEP 668 marks system Python as externally
# managed.
# Steps prepend /opt/wheelvenv/bin to PATH.
RUN python3 -m venv /opt/wheelvenv \
 && /opt/wheelvenv/bin/pip install -q build setuptools wheel

RUN cargo install --locked cargo-chef --version 0.1.77

FROM base AS planner

WORKDIR /opt/ptpsim-cook

COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY .cargo ./.cargo
COPY crates ./crates
COPY services ./services
COPY packages ./packages
COPY tools ./tools
RUN cargo chef prepare --recipe-path /recipe.json

FROM base

WORKDIR /opt/ptpsim-cook

ENV CARGO_HOME=/ci-cache/cargo-home \
    CARGO_TARGET_DIR=/ci-cache/target \
    CARGO_INCREMENTAL=0

COPY --from=planner /recipe.json /recipe.json
COPY scripts/rustc-wrapper scripts/rustc-wrapper
# The pinned channel must drive the cook: rustup reads rust-toolchain.toml
# from the working directory, installs 1.97.0 with its components into
# RUSTUP_HOME, and the cook layer bakes that toolchain for CI. Without it
# the cook compiles on the image default and CI rejects every artifact.
COPY rust-toolchain.toml rust-toolchain.toml
# The plain cook warms cargo build/test dependencies. The clippy cook adds
# the clippy-flavored artifacts for the proc-macro closure, which cargo
# clippy compiles differently and would otherwise rebuild every CI run.
RUN cargo chef cook --all-targets --recipe-path /recipe.json \
 && cargo chef cook --all-targets --clippy --recipe-path /recipe.json \
 && rm -rf /ci-cache/target/debug/incremental \
 && rm -rf /opt/ptpsim-cook /recipe.json

WORKDIR /
