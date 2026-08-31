# CI toolchain image for the Rust workspace and python-wheel steps.
# The final stage bakes Cargo registry sources and dependency-only artifacts for
# the whole workspace. Consumers must use CARGO_HOME=/ci-cache/cargo-home,
# CARGO_TARGET_DIR=/ci-cache/target, and CARGO_INCREMENTAL=0 to reuse them.
# The chef skeleton copies .cargo/config.toml, so scripts/rustc-wrapper is
# required during cook. The deployment owner's CI builds and publishes this
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

WORKDIR /woodpecker/src/ptpsim

COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY .cargo ./.cargo
COPY crates ./crates
COPY services ./services
COPY packages ./packages
COPY tools ./tools
RUN cargo chef prepare --recipe-path /recipe.json

FROM base

WORKDIR /woodpecker/src/ptpsim

ENV CARGO_HOME=/ci-cache/cargo-home \
    CARGO_TARGET_DIR=/ci-cache/target \
    CARGO_INCREMENTAL=0

COPY --from=planner /recipe.json /recipe.json
COPY scripts/rustc-wrapper scripts/rustc-wrapper
RUN cargo chef cook --all-targets --recipe-path /recipe.json \
 && rm -rf /ci-cache/target/debug/incremental \
 && rm -rf /woodpecker/src/ptpsim/* \
      /woodpecker/src/ptpsim/.[!.]* \
      /woodpecker/src/ptpsim/..?* \
      /recipe.json
