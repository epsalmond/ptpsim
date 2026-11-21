#!/usr/bin/env bash
# Exercise hosted Android setup without downloading an SDK or compiling tools.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEST_ROOT="$(mktemp -d)"
trap 'rm -rf "$TEST_ROOT"' EXIT
export TEST_ROOT
export ANDROID_HOME="$TEST_ROOT/android sdk"
export GITHUB_ENV="$TEST_ROOT/github-env"
mkdir -p "$TEST_ROOT/bin" "$ANDROID_HOME/cmdline-tools/latest/bin"
ln -s "$(command -v bash)" "$TEST_ROOT/bin/bash"
cat > "$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$@" > "$TEST_ROOT/sdkmanager.calls"
exit "${SDKMANAGER_EXIT:-0}"
MOCK
cat > "$TEST_ROOT/bin/cargo" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$@" > "$TEST_ROOT/cargo.calls"
MOCK
chmod +x "$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager" "$TEST_ROOT/bin/cargo"

# No sdkmanager is on PATH. The SDK path also contains a space.
PATH="$TEST_ROOT/bin" "$BASH" "$ROOT/ci/github-linux-setup.sh" android
printf '%s\n' 'ndk;27.2.12479018' 'platforms;android-35' > "$TEST_ROOT/expected-sdkmanager"
cmp "$TEST_ROOT/expected-sdkmanager" "$TEST_ROOT/sdkmanager.calls"
printf '%s\n' install --locked cargo-ndk --version 3.5.4 > "$TEST_ROOT/expected-cargo"
cmp "$TEST_ROOT/expected-cargo" "$TEST_ROOT/cargo.calls"
printf 'ANDROID_NDK_HOME=%s/ndk/27.2.12479018\n' "$ANDROID_HOME" > "$TEST_ROOT/expected-env"
cmp "$TEST_ROOT/expected-env" "$GITHUB_ENV"

# Failed SDK installation must prevent exports and cargo-ndk installation.
rm "$TEST_ROOT/cargo.calls" "$GITHUB_ENV"
status=0
PATH="$TEST_ROOT/bin" SDKMANAGER_EXIT=42 \
  "$BASH" "$ROOT/ci/github-linux-setup.sh" android || status=$?
[[ "$status" == 42 ]]
[[ ! -e "$TEST_ROOT/cargo.calls" && ! -e "$GITHUB_ENV" ]]

# A missing SDK tool gets an actionable diagnostic before any installation.
rm "$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager"
status=0
PATH="$TEST_ROOT/bin" "$BASH" "$ROOT/ci/github-linux-setup.sh" android \
  > "$TEST_ROOT/missing.log" 2>&1 || status=$?
[[ "$status" == 1 ]]
grep -Fq "Android SDK manager is missing or not executable: $ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager" \
  "$TEST_ROOT/missing.log"
[[ ! -e "$TEST_ROOT/cargo.calls" && ! -e "$GITHUB_ENV" ]]
printf 'Hosted Android SDK setup tests passed\n'
