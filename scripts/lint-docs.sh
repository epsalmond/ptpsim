#!/bin/sh
# Docs lint: frontmatter, index completeness, and relative-link resolution.
# Run from the repo root; used by scripts/git-hooks/pre-commit and CI.
set -eu

fail=0
err() {
    echo "lint-docs: $1" >&2
    fail=1
}

# docs/consults/ has its own formal frontmatter schema (docs/consults/README.md)
# and is excluded; docs/README.md is the index itself.
docs_files=$(git ls-files 'docs/*.md' 'docs/plans/*.md' | grep -v '^docs/consults/' | grep -v '^docs/README.md$' || true)

for f in $docs_files; do
    if [ "$(head -1 "$f")" != "---" ]; then
        err "$f: missing YAML frontmatter (---)"
        continue
    fi
    header=$(awk 'NR==1{next} /^---$/{exit} {print}' "$f")
    for key in description status read-when; do
        if ! printf '%s\n' "$header" | grep -q "^$key: ..*"; then
            err "$f: frontmatter missing '$key:'"
        fi
    done
    status=$(printf '%s\n' "$header" | sed -n 's/^status: *//p' | head -1)
    case "$status" in
    reference | plan | shipped | historical | generated) ;;
    *) err "$f: invalid status '$status' (reference|plan|shipped|historical|generated)" ;;
    esac
done

# Every doc appears in the docs/README.md index exactly once.
for f in $docs_files; do
    rel=${f#docs/}
    if ! grep -qF "($rel)" docs/README.md; then
        err "$f: not listed in docs/README.md index"
    fi
done

# Relative markdown links resolve on disk (docs/ and root-level docs).
for f in $(git ls-files '*.md' | grep -v '^docs/consults/'); do
    dir=$(dirname "$f")
    # shellcheck disable=SC2013
    for link in $(grep -o '](\([^)#]*\.md\)' "$f" | sed 's/^](//' | grep -v '^http' | grep -v '^~'); do
        if [ ! -f "$dir/$link" ] && [ ! -f "$link" ]; then
            err "$f: broken relative link -> $link"
        fi
    done
done

if [ "$fail" -ne 0 ]; then
    exit 1
fi
echo "lint-docs: ok"
