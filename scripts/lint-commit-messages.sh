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

self_test() {
    if check_message 'Update one thing
with a body' multiline >/dev/null 2>&1; then
        echo "lint-commit-messages self-test: multiline fixture passed" >&2
        exit 1
    fi
    overlength='Update the camera protocol implementation with an intentionally excessive subject length'
    if check_message "$overlength" overlength >/dev/null 2>&1; then
        echo "lint-commit-messages self-test: overlength fixture passed" >&2
        exit 1
    fi
    trailer='Update protocol data Co-'"authored-by: Example <example@example.com>"
    if check_message "$trailer" trailer >/dev/null 2>&1; then
        echo "lint-commit-messages self-test: trailer fixture passed" >&2
        exit 1
    fi
    closing_fixture='Fix'
    closing_fixture="${closing_fixture}es #12"
    if check_message "$closing_fixture" closing >/dev/null 2>&1; then
        echo "lint-commit-messages self-test: closing fixture passed" >&2
        exit 1
    fi
    check_message 'Update protocol data Refs #12' valid >/dev/null
    echo "lint-commit-messages self-test: ok"
}

if [ "${1:-}" = "--self-test" ]; then
    self_test
    exit 0
fi
if [ "$#" -ne 1 ]; then
    echo "usage: $0 <revision|revision-range> | --self-test" >&2
    exit 2
fi

range=$1
failed=0
# Merge commits carry GitHub's generated message, so only authored commits
# are linted. A bare revision lints exactly that commit, not its ancestry.
case "$range" in
    *..*) commits=$(git rev-list --no-merges "$range") ;;
    *) commits=$(git rev-list --no-merges "$range^!") ;;
esac
for commit in $commits; do
    message=$(git show -s --format=%B "$commit")
    if ! check_message "$message" "$commit"; then
        failed=1
    fi
done
if [ "$failed" -ne 0 ]; then
    exit 1
fi

echo "lint-commit-messages: ok"
