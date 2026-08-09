#!/bin/sh
set -eu

op_a='fuji'
op_b='[-_ ]?operators'
mgmt_a='management'
mgmt_b='[-_ ]plane'
rev_a='reverse'
rev_b='[ -]?engineering'
dec_a='decomp'
dec_b='il(e|ed|er|ers|ing|ation|ations)?'
dis_a='disass'
dis_b='embl(e|ed|er|ers|ing|y|ies)'
static_a='static'
static_b='([_ -]r'
static_c='e([^A-Za-z0-9_]|$)|[ -]analysis)'
rce_a='/r'
rce_b='ce/'
current_a='(^|[^A-Za-z0-9_])x'
current_b='([ _-]?app)'
legacy_a='(^|[^A-Za-z0-9_])camera'
legacy_b='[ _-]?remote'
operator_a='operator'
operator_b='[-_ ](reply|doc)'
home_a='(/home/[^/]+|~)'
home_b='/(git/)?(ptpsim|fuji'
home_c='kage|operators|management'
home_d='-plane)(/|$)'
private_a='(/mnt/vm(/|$)|osx-'
private_b='vm(\.local)?|mbp\.local|oci-'
private_c='vcam(-[0-9]+)?|registry\.internal|ci\.psalmond\.com)'
proprietary_a='libF'
proprietary_b='FIR|FTL'
proprietary_c='PTP|XGFX'
proprietary_d='API|CCameraEvent::Thread'
proprietary_e='Proc|X RAW '
proprietary_f='Studio'
private_repo_a='github\.com(/repos)?[:/]epsalmond/fuji'
private_repo_b='kage([/.#?]|$)'

deny_pattern="$op_a$op_b|$mgmt_a$mgmt_b|$rev_a$rev_b|$dec_a$dec_b|$dis_a$dis_b|$static_a$static_b$static_c|$rce_a$rce_b|$current_a$current_b|$legacy_a$legacy_b|$operator_a$operator_b|$home_a$home_b$home_c$home_d|$private_a$private_b$private_c|$proprietary_a$proprietary_b$proprietary_c$proprietary_d$proprietary_e$proprietary_f|$private_repo_a$private_repo_b"

scan_file() {
    file=$1
    if grep -E -i -n "$deny_pattern" "$file"; then
        return 1
    fi
    return 0
}

self_test() {
    temporary=$(mktemp -d)
    trap 'rm -rf "$temporary"' EXIT HUP INT TERM

    printf '%s\n' "${rev_a} ${rev_b#\[ -\]?}" >"$temporary/denied.txt"
    if scan_file "$temporary/denied.txt" >/dev/null 2>&1; then
        echo "lint-public self-test: reverse-method fixture passed unexpectedly" >&2
        exit 1
    fi

    method_fixture='The R'
    method_fixture="${method_fixture}E analysis result is private."
    printf '%s\n' "$method_fixture" >"$temporary/method.txt"
    if ! grep -E -q '\bRE\b.{0,24}\b(analysis|method|derived|work|notes|result)s?\b' "$temporary/method.txt"; then
        echo "lint-public self-test: standalone method-related RE fixture passed unexpectedly" >&2
        exit 1
    fi

    denied_name="$temporary/xa""pp-notes.txt"
    : >"$denied_name"
    if ! printf '%s\n' "$denied_name" | grep -E -i -q "$current_a$current_b"; then
        echo "lint-public self-test: denied filename passed unexpectedly" >&2
        exit 1
    fi

    printf '%s\n' 'A clean-room implementation uses wire capture evidence.' >"$temporary/allowed.txt"
    scan_file "$temporary/allowed.txt" >/dev/null

    printf '%s\n' 'Fujikage is public at https://fujikage.io.' >"$temporary/public-product.txt"
    scan_file "$temporary/public-product.txt" >/dev/null

    private_repo_fixture='https://github.com/epsalmond/fuji'
    private_repo_fixture="${private_repo_fixture}kage/issues/1"
    printf '%s\n' "$private_repo_fixture" >"$temporary/private-repo.txt"
    if scan_file "$temporary/private-repo.txt" >/dev/null 2>&1; then
        echo "lint-public self-test: private repository link passed unexpectedly" >&2
        exit 1
    fi
    echo "lint-public self-test: ok"
}

if [ "${1:-}" = "--self-test" ]; then
    self_test
    exit 0
fi
if [ "$#" -ne 0 ]; then
    echo "usage: $0 [--self-test]" >&2
    exit 2
fi

failed=0
if git ls-files | grep -E -i -n "$deny_pattern"; then
    failed=1
fi
if git grep -I -n -E -i "$deny_pattern" -- .; then
    failed=1
fi


if [ "$failed" -ne 0 ]; then
    echo "lint-public: prohibited public content found" >&2
    exit 1
fi

echo "lint-public: ok"
