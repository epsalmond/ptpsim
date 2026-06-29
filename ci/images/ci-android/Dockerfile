# CI toolchain image for the android-libs step: cimg's Android SDK+NDK plus
# the Rust cross-toolchain that step previously installed per-run (~7 min of
# rustup + cargo-ndk on every pipeline). Rebuilt by .woodpecker/ci-images.yml
# when ci/images/** changes, or via a manual pipeline trigger.
FROM cimg/android:2026.03.1-ndk

SHELL ["/bin/bash", "-o", "pipefail", "-c"]

# Run as root DELIBERATELY: kills the uid-3434 friction wholesale — the
# cargo-cache volume mount, workspace git "dubious ownership", and the sudo
# dance the step used to carry. Root in an ephemeral CI step container that
# only builds artifacts is the intended shape here, not an oversight.
# hadolint ignore=DL3002
USER root

# Rust under /usr/local so it's on every user's PATH and survives HOME
# overrides. cargo-ndk lands in /usr/local/cargo/bin via the same CARGO_HOME.
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --profile minimal --default-toolchain stable \
 && rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android \
 && cargo install --locked cargo-ndk --version 3.5.4

# Stable NDK path (cimg buries it under a versioned dir).
RUN ln -s "$(find /home/circleci/android-sdk/ndk -mindepth 1 -maxdepth 1 -type d | sort -V | tail -1)" /opt/ndk
ENV ANDROID_NDK_HOME=/opt/ndk

# Same no-pin stance as ci-rust / the service Dockerfile.
# hadolint ignore=DL3008
RUN curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
      -o /usr/share/keyrings/githubcli-archive-keyring.gpg \
 && echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
      > /etc/apt/sources.list.d/github-cli.list \
 && apt-get update -qq \
 && apt-get install -qq -y --no-install-recommends gh unzip \
 && rm -rf /var/lib/apt/lists/*

# Kotlin compiler + a JNA jar for the .aar wrapping (#74): build-android.sh
# compiles the uniffi Kotlin bindings to classes.jar against JNA (the uniffi
# 0.31 Kotlin backend is JNA-based). android.jar comes from the cimg Android SDK
# already in this base — the script discovers it at build time. Pinned, no-cache.
ARG KOTLIN_VERSION=2.0.21
ARG JNA_VERSION=5.14.0
RUN curl -fsSL "https://github.com/JetBrains/kotlin/releases/download/v${KOTLIN_VERSION}/kotlin-compiler-${KOTLIN_VERSION}.zip" -o /tmp/kotlin.zip \
 && unzip -q /tmp/kotlin.zip -d /opt \
 && rm /tmp/kotlin.zip \
 && curl -fsSL "https://repo1.maven.org/maven2/net/java/dev/jna/jna/${JNA_VERSION}/jna-${JNA_VERSION}.jar" -o /opt/jna.jar
ENV PATH=/opt/kotlinc/bin:$PATH \
    JNA_JAR=/opt/jna.jar
