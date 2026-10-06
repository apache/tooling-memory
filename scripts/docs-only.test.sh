#!/bin/sh
# What docs-only.sh calls a Markdown-only change, and what it must go on sending to the
# full checks. A wrong yes skips the tests on a change that needed them, so the cases
# that must say no are the ones that matter, and the last check holds the premise: no
# crate embeds a `.md` file, which would make a Markdown edit a change to the build.
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
# the last line without its newline, as a caller's printf may send it
check yes 'README.md\nPLAN.md'

# nothing listed: the caller could not say what changed
check no ''
check no '\n'
check no 'crates/core/src/lib.rs\n'
check no 'README.md\ncrates/core/src/lib.rs\n'
check no 'crates/core/src/lib.rs\nREADME.md\n'
# embedded by a crate, not documentation
check no 'local/litellm.yaml\n'
check no 'crates/web/static/index.html\n'
check no '.github/workflows/ci.yml\n'
check no 'scripts/docs-only.sh\n'
# only the extension counts, and only in lower case
check no 'README.md.orig\n'
check no 'notes.MD\n'
check no 'mdbook\n'
# git quotes a name with a control character in it, and the quote ends the name
check no '"docs/a\\nb.md"\n'

if grep -rEn --include='*.rs' 'include_(str|bytes)!.*\.md"' "$root/crates" >&2; then
    printf 'a crate embeds a Markdown file, so a Markdown edit changes the build\n' >&2
    failures=$((failures + 1))
fi

if [ "$failures" -ne 0 ]; then
    printf '\n%s docs-only case(s) failed\n' "$failures" >&2
    exit 1
fi

printf 'docs-only.sh: every case holds\n'
