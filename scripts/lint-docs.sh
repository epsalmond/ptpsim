#!/bin/sh
# Docs lint: frontmatter, index completeness, relative-link resolution, and em
# dashes in Markdown prose.
# Run from the repo root; used by scripts/git-hooks/pre-commit and CI.
#
# usage: lint-docs.sh [--base <revision>] | --self-test
#
# The em dash check reads every line of every tracked Markdown file, except the
# paths in em_dash_excluded below. --base is accepted for compatibility with
# the CI workflow and has no effect.
set -eu

# U+2014 as UTF-8 bytes, so this file contains no literal em dash.
em_dash=$(printf '\342\200\224')

# Path prefixes the em dash check skips, separated by spaces. .agents/plans/
# holds dated agent plan and handoff records that are kept as written.
em_dash_excluded='.agents/plans/'

usage() {
    echo "usage: $0 [--base <revision>] | --self-test" >&2
    exit 2
}

self_test() {
    script=$(cd "$(dirname "$0")" && pwd)/$(basename "$0")
    fixture_dir=$(mktemp -d "${TMPDIR:-/tmp}/lint-docs.XXXXXX")
    trap 'rm -rf "$fixture_dir"' EXIT HUP INT TERM
    repo="$fixture_dir/repo"
    git init -q -b main "$repo"
    cd "$repo"
    git config user.name 'Docs lint test'
    git config user.email 'docs-lint@example.invalid'
    git config core.hooksPath /dev/null
    git config commit.gpgsign false
    git config core.autocrlf false

    printf '%s\n' '# Guide' '' 'Plain prose.' >guide.md
    printf '%s\n' 'echo unchanged' >tool.sh
    git add -A
    git commit -q -m 'Seed repository'
    base=$(git rev-parse HEAD)
    st_failed=0

    # Each case edits the working tree, runs the lint, then restores the seed.
    # An expected failure must report the em dash, or the optional third
    # argument when given.
    run_case() {
        expect=$1
        name=$2
        want=${3:-em dash}
        git add -A
        if output=$("$script" 2>&1); then status=0; else status=$?; fi
        case "$expect:$status" in
        pass:0) ;;
        fail:0)
            echo "lint-docs self-test: $name passed unexpectedly" >&2
            st_failed=1
            ;;
        pass:*)
            echo "lint-docs self-test: $name failed unexpectedly: $output" >&2
            st_failed=1
            ;;
        fail:*)
            if ! printf '%s\n' "$output" | grep -q -F "$want"; then
                echo "lint-docs self-test: $name failed for another reason: $output" >&2
                st_failed=1
            fi
            ;;
        esac
        git reset -q --hard "$base"
        git clean -fdq
    }

    run_case pass 'clean tree'

    printf '%s\n' "Added prose $em_dash line." >>guide.md
    run_case fail 'added Markdown line' 'guide.md:4: em dash'

    mkdir -p notes/nested
    printf '%s\n' '# New' '' "New file $em_dash prose." >notes/nested/new.md
    run_case fail 'new Markdown file' 'notes/nested/new.md:3: em dash'

    # A committed em dash fails too: the check is not limited to edited lines.
    printf '%s\n' "Committed prose $em_dash line." >>guide.md
    git commit -q -a -m 'Commit an em dash'
    run_case fail 'committed line in an unedited file' 'guide.md:4: em dash'

    printf '%s\n' "echo '$em_dash'" >>tool.sh
    printf '%s\n' "Plain text $em_dash note." >notes.txt
    run_case pass 'em dash outside Markdown'

    {
        printf '%s\n' "Inline \`a $em_dash b\` and \`\`c \` $em_dash d\`\` spans."
        printf '%s\n' '```text' "Fenced $em_dash block." '```'
        printf '%s\n' '  ~~~~' "Tilde $em_dash block." '  ~~~' "Still $em_dash fenced." '~~~~'
    } >>guide.md
    run_case pass 'em dash in code spans or fenced blocks'

    printf '%s\n' '```' 'code' '```' "After the fence $em_dash prose." >>guide.md
    run_case fail 'prose after a closed fence'

    printf '%s\n' "Unclosed \`span $em_dash prose." >>guide.md
    run_case fail 'unmatched backtick'

    # shellcheck disable=SC2016 # literal backticks are fixture Markdown
    printf '```\r\ncode\r\n```\r\nAfter a CRLF fence %s prose.\r\n' "$em_dash" >>guide.md
    run_case fail 'prose after a closed CRLF fence'

    # shellcheck disable=SC2016 # literal backticks are fixture Markdown
    printf '%s\n' '```x```' "After inline code $em_dash prose." >>guide.md
    run_case fail 'prose after a one-line backtick span'

    mkdir -p .agents/plans
    printf '%s\n' "Dated plan $em_dash prose." >.agents/plans/2026-01-01-plan.md
    run_case pass 'excluded agent plan record'

    # The exclusion is a path prefix from the repository root.
    mkdir -p notes/.agents/plans
    printf '%s\n' "Nested plan $em_dash prose." >notes/.agents/plans/plan.md
    run_case fail 'exclusion name below the root' 'notes/.agents/plans/plan.md:1: em dash'

    # Git quotes this name in ls-files output.
    printf '%s\n' "Quoted path $em_dash prose." >'q"uo\te.md'
    run_case fail 'quoted path' 'q"uo\te.md:1: em dash'

    tab_name=$(printf 't\tab.md')
    printf '%s\n' "Escaped path $em_dash prose." >"$tab_name"
    run_case fail 'path with a control character' "$tab_name:1: em dash"

    printf '%s\n' "Spaced quoted path $em_dash prose." >'sp "q.md'
    run_case fail 'quoted path with a space' 'sp "q.md:1: em dash'

    tab_space_name=$(printf 't\tab sp.md')
    printf '%s\n' "Tab space path $em_dash prose." >"$tab_space_name"
    run_case fail 'path with a tab plus a space' "$tab_space_name:1: em dash"

    if ! output=$("$script" --base refs/heads/missing 2>&1); then
        echo "lint-docs self-test: --base was not accepted: $output" >&2
        st_failed=1
    fi

    cd /
    rm -rf "$fixture_dir"
    trap - EXIT HUP INT TERM
    if [ "$st_failed" -ne 0 ]; then
        exit 1
    fi
    echo "lint-docs self-test: ok"
}

case "${1:-}" in
--self-test)
    [ "$#" -eq 1 ] || usage
    self_test
    exit 0
    ;;
--base)
    # Kept so existing callers keep working; the check no longer uses a base.
    if [ "$#" -ne 2 ] || [ -z "$2" ]; then usage; fi
    ;;
*)
    [ "$#" -eq 0 ] || usage
    ;;
esac

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

# No em dash in Markdown prose (AGENTS.md "Prose register"). Code spans and
# fenced code blocks are exempt.
dash_report=$(
    git -c core.quotePath=false ls-files --cached -- '*.md' |
        LC_ALL=C awk -v dash="$em_dash" -v excluded="$em_dash_excluded" '
            # Remove inline code spans: a run of N backticks up to the next
            # run of exactly N backticks. An unmatched run stays as text.
            function strip_code(s,    out, i, n, run, rest, pos, k, m, close_at) {
                out = ""
                while ((i = index(s, "`")) > 0) {
                    out = out substr(s, 1, i - 1)
                    s = substr(s, i)
                    n = 0
                    while (substr(s, n + 1, 1) == "`") n++
                    run = substr(s, 1, n)
                    rest = substr(s, n + 1)
                    close_at = 0
                    pos = 0
                    while ((k = index(substr(rest, pos + 1), run)) > 0) {
                        m = 0
                        while (substr(rest, pos + k + m, 1) == "`") m++
                        if (m == n) { close_at = pos + k; break }
                        pos = pos + k + m - 1
                    }
                    if (close_at) {
                        s = substr(rest, close_at + n)
                    } else {
                        out = out run
                        s = rest
                    }
                }
                return out s
            }
            # Undo the C-style quoting git applies to unusual file names.
            function unquote(s,    out, i, c) {
                out = ""
                for (i = 2; i < length(s); i++) {
                    c = substr(s, i, 1)
                    if (c == "\\") {
                        c = substr(s, ++i, 1)
                        if (c ~ /[0-7]/) {
                            c = sprintf("%c", c * 64 + substr(s, i + 1, 1) * 8 + substr(s, i + 2, 1))
                            i += 2
                        } else if (c == "a") c = "\007"
                        else if (c == "b") c = "\010"
                        else if (c == "t") c = "\011"
                        else if (c == "n") c = "\012"
                        else if (c == "v") c = "\013"
                        else if (c == "f") c = "\014"
                        else if (c == "r") c = "\015"
                    }
                    out = out c
                }
                return out
            }
            function scan(path,    text, ln, fenced, fence_char, fence_len, marker, ch, len) {
                ln = 0
                fenced = 0
                while ((getline text < path) > 0) {
                    ln++
                    sub(/\r$/, "", text)
                    # A fence is three or more backticks or tildes after at
                    # most three spaces; it closes on a longer-or-equal run
                    # of the same character with nothing after it. A
                    # backtick fence cannot open on a line with another
                    # backtick after the run (that line is inline code).
                    # Not handled, so flagged as prose: indented code
                    # blocks, fences inside blockquotes, and fences
                    # indented four or more spaces in list items. Move the
                    # block to a fence at the left margin, or reword.
                    if (match(text, /^ ? ? ?(```|~~~)/)) {
                        marker = substr(text, RSTART)
                        sub(/^ +/, "", marker)
                        ch = substr(marker, 1, 1)
                        len = 0
                        while (substr(marker, len + 1, 1) == ch) len++
                        if (!fenced) {
                            if (!(ch == "`" && index(substr(marker, len + 1), "`"))) {
                                fenced = 1
                                fence_char = ch
                                fence_len = len
                                continue
                            }
                        } else if (ch == fence_char && len >= fence_len &&
                            substr(marker, len + 1) ~ /^[ \t]*$/) {
                            fenced = 0
                            continue
                        }
                    }
                    if (fenced) continue
                    if (index(strip_code(text), dash)) {
                        print path ":" ln ": em dash (U+2014) in Markdown prose; use a comma, colon, parentheses, or two sentences"
                    }
                }
                close(path)
            }
            BEGIN { excluded_count = split(excluded, prefixes, " ") }
            {
                path = $0
                if (substr(path, 1, 1) == "\"") path = unquote(path)
                for (i = 1; i <= excluded_count; i++) {
                    if (index(path, prefixes[i]) == 1) next
                }
                scan(path)
            }
        '
)
if [ -n "$dash_report" ]; then
    printf '%s\n' "$dash_report" | sed 's/^/lint-docs: /' >&2
    fail=1
fi

if [ "$fail" -ne 0 ]; then
    exit 1
fi
echo "lint-docs: ok"
