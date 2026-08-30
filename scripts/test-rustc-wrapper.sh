#!/bin/sh
set -eu

repo_root=$(CDPATH= cd "$(dirname "$0")/.." && pwd)
wrapper="$repo_root/scripts/rustc-wrapper"
temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT HUP INT TERM

cached_bin="$temporary/cached-bin"
direct_bin="$temporary/direct-bin"
mkdir "$cached_bin" "$direct_bin"
bash_path=$(command -v bash)
ln -s "$bash_path" "$cached_bin/bash"
ln -s "$bash_path" "$direct_bin/bash"

cat >"$cached_bin/sccache" <<'EOF'
#!/bin/sh
printf '%s\n' "$@" >"$RUSTC_WRAPPER_TEST_LOG"
exit "$RUSTC_WRAPPER_TEST_STATUS"
EOF
chmod +x "$cached_bin/sccache"

fake_rustc="$direct_bin/rustc"
cat >"$fake_rustc" <<'EOF'
#!/bin/sh
printf '%s\n' "$@" >"$RUSTC_WRAPPER_TEST_LOG"
exit "$RUSTC_WRAPPER_TEST_STATUS"
EOF
chmod +x "$fake_rustc"

set +e
PATH="$cached_bin" \
    RUSTC_WRAPPER_TEST_LOG="$temporary/cached-args" \
    RUSTC_WRAPPER_TEST_STATUS=23 \
    "$wrapper" "$fake_rustc" --crate-name 'cached crate' '' \
    -C "incremental=$temporary/cached-incremental" \
    -Cdebuginfo=1 \
    "-Cincremental=$temporary/cached-compact-incremental" \
    --cfg 'feature="cached feature"'
cached_status=$?
set -e

if [ "$cached_status" -ne 23 ]; then
    echo "cached path: expected status 23, got $cached_status" >&2
    exit 1
fi
printf '%s\n' \
    "$fake_rustc" --crate-name 'cached crate' '' \
    -Cdebuginfo=1 --cfg 'feature="cached feature"' \
    >"$temporary/cached-expected"
diff -u "$temporary/cached-expected" "$temporary/cached-args"

set +e
PATH="$direct_bin" \
    RUSTC_WRAPPER_TEST_LOG="$temporary/direct-args" \
    RUSTC_WRAPPER_TEST_STATUS=29 \
    "$wrapper" "$fake_rustc" --crate-name 'direct crate' '' \
    -C "incremental=$temporary/direct-incremental" \
    -Cdebuginfo=1 \
    "-Cincremental=$temporary/direct-compact-incremental" \
    --cfg 'feature="direct feature"'
direct_status=$?
set -e

if [ "$direct_status" -ne 29 ]; then
    echo "direct path: expected status 29, got $direct_status" >&2
    exit 1
fi
printf '%s\n' \
    --crate-name 'direct crate' '' \
    -C "incremental=$temporary/direct-incremental" \
    -Cdebuginfo=1 \
    "-Cincremental=$temporary/direct-compact-incremental" \
    --cfg 'feature="direct feature"' \
    >"$temporary/direct-expected"
diff -u "$temporary/direct-expected" "$temporary/direct-args"

echo "rustc-wrapper tests: ok"
