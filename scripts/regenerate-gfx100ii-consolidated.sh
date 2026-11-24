#!/bin/sh
# Regenerate packages/camera-config-data/fuji/gfx100ii/gfx100ii.consolidated.yaml
# from the base manifest and the reviewed proposal chain.
#
#     scripts/regenerate-gfx100ii-consolidated.sh           # rewrite the file
#     scripts/regenerate-gfx100ii-consolidated.sh --check   # fail if it drifted
#
# The chain is defined once, in crates/camera-config/tests/gfx100ii_slice.rs
# (`reviewed_chain`), which the drift test also uses. This script runs that
# file's ignored `regenerate_consolidated_manifest` test, which writes the file
# exactly as `camera-config-generate apply` would, and then replaces or
# byte-compares the committed copy. Never hand-edit the consolidated manifest:
# edit the base manifest or a review, then run this script.
set -eu

repo_root=$(CDPATH="" cd "$(dirname "$0")/.." && pwd)
committed="$repo_root/packages/camera-config-data/fuji/gfx100ii/gfx100ii.consolidated.yaml"

case "${1:-}" in
"") mode="write" ;;
--check) mode="check" ;;
*)
    echo "usage: scripts/regenerate-gfx100ii-consolidated.sh [--check]" >&2
    exit 2
    ;;
esac

# Keep the scratch copy beside the build output, not on a tmpfs. Resolve a
# relative CARGO_TARGET_DIR against the starting directory and export it, so
# cargo, the test (which runs in the crate directory), and the comparison all
# use the same absolute path.
target_dir="${CARGO_TARGET_DIR:-$repo_root/target}"
case "$target_dir" in
/*) ;;
*) target_dir="$PWD/$target_dir" ;;
esac
mkdir -p "$target_dir"
target_dir=$(CDPATH="" cd "$target_dir" && pwd)
CARGO_TARGET_DIR="$target_dir"
export CARGO_TARGET_DIR
regenerated="$target_dir/gfx100ii.consolidated.regenerated.yaml"
trap 'rm -f "$regenerated"' EXIT HUP INT TERM

cd "$repo_root"
PTPSIM_REGENERATED_CONSOLIDATED="$regenerated" cargo test --quiet -p camera-config \
    --test gfx100ii_slice -- --ignored --exact regenerate_consolidated_manifest >&2

if [ "$mode" = check ]; then
    if cmp -s "$regenerated" "$committed"; then
        echo "gfx100ii consolidated manifest: up to date"
        exit 0
    fi
    diff -u "$committed" "$regenerated" | head -40 >&2 || true
    echo "gfx100ii consolidated manifest drifted; run scripts/regenerate-gfx100ii-consolidated.sh" >&2
    exit 1
fi

cp "$regenerated" "$committed"
echo "regenerated $committed"
