#!/bin/sh
# Exit 0 when every path on stdin, one per line, is a Markdown file, and 1 otherwise.
# The pre-commit hook and CI ask this before the expensive checks: a change that touches
# only Markdown cannot change what builds or what the tests see, since no crate embeds a
# `.md` file (scripts/docs-only.test.sh holds that). An empty list is not docs-only, so a
# caller that failed to list its changes runs everything.
#
#   git diff --cached --name-only --no-renames | scripts/docs-only.sh
#
# Pass `--no-renames`, so a file renamed from `.rs` to `.md` shows both of its names.
set -eu

seen=0
while IFS= read -r path || [ -n "$path" ]; do
    [ -n "$path" ] || continue
    case $path in
        *.md) seen=1 ;;
        *) exit 1 ;;
    esac
done
[ "$seen" = 1 ]
