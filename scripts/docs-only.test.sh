#!/bin/sh
# What docs-only.sh calls a Markdown-only change, and what it must go on sending to the
# full checks. A wrong yes skips the tests on a change that needed them, so the cases
# that must say no are the ones that matter, and the last checks hold the premise: no
# crate or tap embeds a Markdown file or opens one above its own directory, which would
# make an edit to it a change to the build.
#
# Run by `make check`.

set -eu

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
root=$(CDPATH='' cd -- "$here/.." && pwd)
script="$here/docs-only.sh"

failures=0

check() {
    want=$1
    shift
    status=0
    printf "$@" | sh "$script" || status=$?
    if [ "$want" = yes ] && [ "$status" -ne 0 ]; then
        printf 'not docs-only, should be: %s\n' "$(printf "$@" | tr '\n' ' ')" >&2
        failures=$((failures + 1))
    elif [ "$want" = no ] && [ "$status" -eq 0 ]; then
        printf 'docs-only, should not be: %s\n' "$(printf "$@" | tr '\n' ' ')" >&2
        failures=$((failures + 1))
    fi
}

check yes 'README.md\n'
check yes 'local/README.md\nAGENTS.md\ndocs/design/a b.md\n'
check yes 'crates/core/AGENTS.md\n'
check yes 'taps/github-issues/AGENTS.md\n'
# the last line without its newline, as a caller's printf may send it
check yes 'README.md\nPLAN.md'

# nothing listed: the caller could not say what changed
check no ''
check no '\n'
check no 'crates/core/src/lib.rs\n'
check no 'README.md\ncrates/core/src/lib.rs\n'
# a last line without its newline still counts
check no 'README.md\ncrates/core/src/lib.rs'
check no 'crates/core/src/lib.rs\nREADME.md\n'
# embedded by a crate, not documentation
check no 'local/litellm.yaml\n'
check no 'crates/web/static/index.html\n'
check no '.github/workflows/ci.yml\n'
check no 'scripts/docs-only.sh\n'
# Markdown in the code: a fixture, or a README a doc attribute could include
check no 'crates/core/tests/fixtures/entry.md\n'
check no 'crates/web/README.md\n'
check no 'taps/github-issues/README.md\n'
# only the extension counts, and only in lower case
check no 'README.md.orig\n'
check no 'notes.MD\n'
check no 'mdbook\n'
check no 'scripts/build.cmd\n'
# git quotes a name with a control character in it, and the quote ends the name
check no '"docs/a\\nb.md"\n'

# A match fails, and so does a search that could not run: a grep that failed to look and
# reports nothing found is the worst of the three answers.
no_match() {
    what=$1
    shift
    status=0
    grep "$@" >&2 || status=$?
    case $status in
        0) printf '%s, so an edit to it changes the build\n' "$what" >&2 ;;
        1) return 0 ;;
        *) printf 'could not search for whether %s\n' "$what" >&2 ;;
    esac
    failures=$((failures + 1))
}

no_match 'a crate or tap embeds a Markdown file' \
    -rPzl --include='*.rs' 'include_(str|bytes)!\s*\([^)]*\.md"' "$root/crates" "$root/taps"
no_match 'a crate or tap opens a Markdown file above its own directory' \
    -rnE --include='*.rs' '"/?(\.\./)+[^"*?]*\.md"' "$root/crates" "$root/taps"

if [ "$failures" -ne 0 ]; then
    printf '\n%s docs-only case(s) failed\n' "$failures" >&2
    exit 1
fi

printf 'docs-only.sh: every case holds\n'
