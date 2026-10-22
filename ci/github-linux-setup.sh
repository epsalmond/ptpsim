#!/usr/bin/env bash
# Install public build dependencies on a hosted Ubuntu runner.
set -euo pipefail

case "${1:-}" in
  rust)
    # Reads the pinned channel, components, and targets from rust-toolchain.toml.
    rustup toolchain install
    ;;
  kotlin)
    tools="${RUNNER_TEMP:?}/ptpsim-kotlin-2.0.21"
    mkdir -p "$tools"
    curl --fail --location --retry 3 \
      https://github.com/JetBrains/kotlin/releases/download/v2.0.21/kotlin-compiler-2.0.21.zip \
      --output "$tools/kotlin.zip"
    unzip -q "$tools/kotlin.zip" -d "$tools"
    curl --fail --location --retry 3 \
      https://repo.maven.apache.org/maven2/net/java/dev/jna/jna/5.14.0/jna-5.14.0.jar \
      --output "$tools/jna.jar"
    printf '%s\n' "$tools/kotlinc/bin" >>"${GITHUB_PATH:?}"
    {
      printf 'JNA_JAR=%s\n' "$tools/jna.jar"
      # Use the coroutine runtime shipped with the pinned compiler archive.
      printf 'KOTLINX_COROUTINES_JAR=%s\n' "$tools/kotlinc/lib/kotlinx-coroutines-core-jvm.jar"
    } >>"${GITHUB_ENV:?}"
    ;;
  android)
    sdkmanager 'ndk;27.2.12479018' 'platforms;android-35'
    printf 'ANDROID_NDK_HOME=%s/ndk/27.2.12479018\n' "${ANDROID_HOME:?}" >>"${GITHUB_ENV:?}"
    cargo install --locked cargo-ndk --version 3.5.4
    ;;
  python)
    python3 -m pip install build==1.2.2.post1 setuptools==80.9.0 wheel==0.45.1
    ;;
  *)
    echo 'usage: ci/github-linux-setup.sh rust|kotlin|android|python' >&2
    exit 2
    ;;
esac
