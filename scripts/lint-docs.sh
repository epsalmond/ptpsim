#!/bin/sh
# Docs lint: frontmatter, index completeness, relative-link resolution, and em
# dashes in new Markdown prose.
# Run from the repo root; used by scripts/git-hooks/pre-commit and CI.
#
# usage: lint-docs.sh [--base <revision>] | --self-test
#
# The em dash check reads only the Markdown lines that the working tree adds or
# edits relative to the base revision, so prose written before the rule passes
# until someone edits it. Without --base, the base is the merge base of HEAD
# with origin/main; when that is unavailable the check is skipped with a notice.
set -eu

# U+2014 as UTF-8 bytes, so this file contains no literal em dash.
em_dash=$(printf '\342\200\224')

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

    printf '%s\n' '# Guide' '' "Older prose $em_dash grandfathered." \
        "Removable prose $em_dash line." 'Plain prose.' >guide.md
    printf '%s\n' '# Moved' '' "Older prose $em_dash in a file that moves." >moved.md
    printf '%s\n' 'echo unchanged' >tool.sh
    git add -A
    git commit -q -m 'Seed repository'
    base=$(git rev-parse HEAD)
    st_failed=0

    # Each case edits the working tree, runs the lint against the seed commit,
    # then restores the seed. Expected failures must name the em dash.
    run_case() {
        expect=$1
        name=$2
        git add -A
        if output=$("$script" --base "$base" 2>&1); then status=0; else status=$?; fi
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
            if ! printf '%s\n' "$output" | grep -q 'em dash'; then
                echo "lint-docs self-test: $name failed for another reason: $output" >&2
                st_failed=1
            fi
            ;;
        esac
        git reset -q --hard "$base"
        git clean -fdq
    }

    printf '%s\n' "Added prose $em_dash line." >>guide.md
    run_case fail 'added Markdown line'

    sed "s/grandfathered/edited/" guide.md >guide.tmp && mv guide.tmp guide.md
    run_case fail 'edited line that keeps its em dash'

    mkdir -p notes/nested
    printf '%s\n' '# New' '' "New file $em_dash prose." >notes/nested/new.md
    run_case fail 'new Markdown file'

    sed "s/Plain prose./Plain prose, edited./" guide.md >guide.tmp && mv guide.tmp guide.md
    run_case pass 'untouched existing em dash in an edited file'

    grep -v Removable guide.md >guide.tmp && mv guide.tmp guide.md
    run_case pass 'removed line'

    git mv -k moved.md renamed.md
    run_case pass 'renamed file with an existing em dash'

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

    if "$script" --base refs/heads/missing >/dev/null 2>&1; then
        echo "lint-docs self-test: unresolvable base passed unexpectedly" >&2
        st_failed=1
    fi

    printf '%s\n' "Added prose $em_dash line." >>guide.md
    git add -A
    if output=$("$script" 2>&1); then
        if ! printf '%s\n' "$output" | grep -q 'em dash check skipped'; then
            echo "lint-docs self-test: missing origin/main gave no skip notice" >&2
            st_failed=1
        fi
    else
        echo "lint-docs self-test: missing origin/main failed: $output" >&2
        st_failed=1
    fi
    git update-ref refs/remotes/origin/main "$base"
    if "$script" >/dev/null 2>&1; then
        echo "lint-docs self-test: origin/main merge base was not used" >&2
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

base=
case "${1:-}" in
--self-test)
    [ "$#" -eq 1 ] || usage
    self_test
    exit 0
    ;;
--base)
    if [ "$#" -ne 2 ] || [ -z "$2" ]; then usage; fi
    base=$2
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

# No em dash on a Markdown line added or edited since the base (AGENTS.md
# "Prose register"). Code spans and fenced code blocks are exempt.
if [ -z "$base" ]; then
    base=$(git merge-base HEAD origin/main 2>/dev/null || true)
    if [ -z "$base" ]; then
        echo "lint-docs: no origin/main merge base; em dash check skipped (pass --base <revision>)" >&2
    fi
fi
if [ -n "$base" ] && ! git rev-parse --verify --quiet "$base^{commit}" >/dev/null; then
    err "base revision '$base' does not resolve to a commit"
    base=
fi
if [ -n "$base" ]; then
    # The diff gives each added line's number in the working-tree file; awk
    # then reads that file to know which lines sit inside a fenced block.
    # Renamed files report only their edited lines.
    dash_report=$(
        git -c core.quotePath=false diff --no-ext-diff --no-color --unified=0 \
            --find-renames --diff-filter=AMR --src-prefix=a/ --dst-prefix=b/ \
            "$base" -- '*.md' |
            LC_ALL=C awk -v dash="$em_dash" '
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
                function scan(path,    text, ln, fenced, fence_char, fence_len, marker, ch, len) {
                    ln = 0
                    fenced = 0
                    while ((getline text < path) > 0) {
                        ln++
                        # A fence is three or more backticks or tildes after at
                        # most three spaces; it closes on a longer-or-equal run
                        # of the same character with nothing after it.
                        if (match(text, /^ ? ? ?(```|~~~)/)) {
                            marker = substr(text, RSTART)
                            sub(/^ +/, "", marker)
                            ch = substr(marker, 1, 1)
                            len = 0
                            while (substr(marker, len + 1, 1) == ch) len++
                            if (!fenced) {
                                fenced = 1
                                fence_char = ch
                                fence_len = len
                                continue
                            }
                            if (ch == fence_char && len >= fence_len &&
                                substr(marker, len + 1) ~ /^[ \t]*$/) {
                                fenced = 0
                                continue
                            }
                        }
                        if (fenced || !((path SUBSEP ln) in added)) continue
                        if (index(strip_code(text), dash)) {
                            print path ":" ln ": em dash (U+2014) in new prose; use a comma, colon, parentheses, or two sentences"
                        }
                    }
                    close(path)
                }
                /^diff --git / { in_hunk = 0; next }
                !in_hunk && /^\+\+\+ b\// {
                    path = substr($0, 7)
                    sub(/\t$/, "", path)
                    files[++count] = path
                    next
                }
                /^@@ / {
                    in_hunk = 1
                    split($3, start, ",")
                    line = substr(start[1], 2) + 0
                    next
                }
                in_hunk && /^\+/ { added[path SUBSEP line] = 1; line++ }
                END { for (i = 1; i <= count; i++) scan(files[i]) }
            '
    )
    if [ -n "$dash_report" ]; then
        printf '%s\n' "$dash_report" | sed 's/^/lint-docs: /' >&2
        fail=1
    fi
fi

if [ "$fail" -ne 0 ]; then
    exit 1
fi
echo "lint-docs: ok"
