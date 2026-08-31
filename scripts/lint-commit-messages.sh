#!/bin/sh
set -eu

close_a='(close[sd]?|fix(e[sd])?|resolve[sd]?)'
close_b='[[:space:]]*:?[[:space:]]*'
issue_ref='(#[0-9]+|[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+#[0-9]+|https?://github\.com/[^/[:space:]]+/[^/[:space:]]+/issues/[0-9]+)'
ai_a='Clau'
ai_b='de|Anthro'
ai_c='pic|Co-authored'
ai_d='-by|Generated '
ai_e='with'

check_message() {
    message=$1
    label=$2
    message_failed=0
    line_count=$(printf '%s\n' "$message" | wc -l | tr -d ' ')
    if [ "$line_count" -ne 1 ] || [ -z "$message" ]; then
        echo "$label: commit message must be one non-empty line" >&2
        message_failed=1
    fi
    length=$(printf '%s' "$message" | wc -m | tr -d ' ')
    if [ "$length" -gt 72 ]; then
        echo "$label: commit message exceeds 72 characters" >&2
        message_failed=1
    fi
    if printf '%s\n' "$message" | grep -E -i -q '[[:space:]]and[[:space:]]'; then
        echo "$label: commit message contains prohibited 'and'" >&2
        message_failed=1
    fi
    if printf '%s\n' "$message" | grep -E -i -q "$close_a$close_b$issue_ref"; then
        echo "$label: commit message contains an auto-close keyword" >&2
        message_failed=1
    fi
    if printf '%s\n' "$message" | grep -E -i -q "$ai_a$ai_b$ai_c$ai_d$ai_e"; then
        echo "$label: commit message contains an AI or co-author trailer" >&2
        message_failed=1
    fi
    [ "$message_failed" -eq 0 ]
}

check_message_file() {
    message_file=$1
    if [ ! -f "$message_file" ] || [ ! -r "$message_file" ]; then
        echo "$message_file: commit message file must be a readable regular file" >&2
        return 2
    fi
    message=$(cat -- "$message_file")
    check_message "$message" "$message_file"
}

self_test() {
    fixture_dir=$(mktemp -d "${TMPDIR:-/tmp}/lint-commit-messages.XXXXXX")
    trap 'rm -rf "$fixture_dir"' EXIT HUP INT TERM

    multiline_file="$fixture_dir/multiline"
    printf '%s\n' 'Update one thing' 'with a body' >"$multiline_file"
    overlength='Update the camera protocol implementation with an intentionally excessive subject length'
    overlength_file="$fixture_dir/overlength"
    printf '%s\n' "$overlength" >"$overlength_file"
    conjunction_file="$fixture_dir/conjunction"
    printf '%s\n' 'Update protocol data and fixtures' >"$conjunction_file"
    trailer='Update protocol data Co-'"authored-by: Example <example@example.com>"
    trailer_file="$fixture_dir/trailer"
    printf '%s\n' "$trailer" >"$trailer_file"
    closing_fixture='Fix'
    closing_fixture="${closing_fixture}es #12"
    closing_file="$fixture_dir/closing"
    printf '%s\n' "$closing_fixture" >"$closing_file"
    valid_file="$fixture_dir/valid message"
    printf '%s\n' 'Update protocol data Refs #12' >"$valid_file"

    for fixture in "$multiline_file" "$overlength_file" "$conjunction_file" "$trailer_file" "$closing_file"; do
        if "$0" --message-file "$fixture" >/dev/null 2>&1; then
            echo "lint-commit-messages self-test: invalid file fixture passed: $fixture" >&2
            exit 1
        fi
    done
    "$0" --message-file "$valid_file" >/dev/null

    rm -rf "$fixture_dir"
    trap - EXIT HUP INT TERM
    echo "lint-commit-messages self-test: ok"
}

if [ "${1:-}" = "--self-test" ]; then
    if [ "$#" -ne 1 ]; then
        echo "usage: $0 <revision|revision-range> | --message-file <path> | --self-test" >&2
        exit 2
    fi
    self_test
    exit 0
fi
if [ "${1:-}" = "--message-file" ]; then
    if [ "$#" -ne 2 ]; then
        echo "usage: $0 <revision|revision-range> | --message-file <path> | --self-test" >&2
        exit 2
    fi
    check_message_file "$2"
    echo "lint-commit-messages: ok"
    exit 0
fi
if [ "$#" -ne 1 ]; then
    echo "usage: $0 <revision|revision-range> | --message-file <path> | --self-test" >&2
    exit 2
fi

range=$1
failed=0
# Merge commits carry GitHub's generated message, so only authored commits
# are linted. A bare revision lints exactly that commit, not its ancestry.
# Parents are read from the commit object rather than rev-list --no-merges:
# CI clones are shallow, so a merge at the shallow boundary has no parents
# for traversal purposes but still records them in the object.
case "$range" in
    *..*) commits=$(git rev-list "$range") ;;
    *) commits=$(git rev-list "$range^!") ;;
esac
is_merge_commit() {
    [ "$(git cat-file -p "$1" | sed '/^$/q' | grep -c '^parent ')" -ge 2 ]
}
for commit in $commits; do
    if is_merge_commit "$commit"; then
        continue
    fi
    message=$(git show -s --format=%B "$commit")
    if ! check_message "$message" "$commit"; then
        failed=1
    fi
done
if [ "$failed" -ne 0 ]; then
    exit 1
fi

echo "lint-commit-messages: ok"
