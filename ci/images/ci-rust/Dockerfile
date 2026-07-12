# CI toolchain image for the rust workspace + python-wheel steps.
# Bakes everything those steps previously installed per-run (rustfmt/clippy
# components, gh, git, python venv with wheel build deps) so a pipeline step
# starts compiling immediately. This repo defines only the portable recipe;
# building and publishing it is the deployment owner's CI, which supplies the
# result to .woodpecker/linux.yml via RUST_TOOLCHAIN_IMAGE.
FROM rust:1-slim

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

# Wheel build deps in a venv (PEP 668 — system python is externally managed).
# Steps prepend /opt/wheelvenv/bin to PATH.
RUN python3 -m venv /opt/wheelvenv \
 && /opt/wheelvenv/bin/pip install -q build setuptools wheel
