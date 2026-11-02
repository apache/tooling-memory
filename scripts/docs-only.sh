#!/bin/sh
# Exit 0 when every path on stdin, one per line, is a Markdown file outside the code, and
# 1 otherwise. The pre-commit hook and CI ask this before the expensive checks: a change
# that touches only such files cannot change what builds or what the tests see. Under
# `crates/` and `taps/` a `.md` file counts as code, since a fixture or a crate's README is
# what a test or a doc attribute would read; an `AGENTS.md` there is instructions, not
# input. No crate or tap embeds a Markdown file from anywhere else or reads one
# (scripts/docs-only.test.sh holds that). Some scripts do: scripts/seed-corpus.py seeds
# the demo record from docs/ and examples/decisions/, which the suites in golden/ and
# evals/demo.toml score against, and act 11 of the demo prints the lists at the top of
# README.md. scripts/corpus-check.sh checks those readers, and CI runs it on every
# change, this kind included. An empty list is not docs-only, so a caller that failed to
# list its changes runs everything.
#
#   git diff --cached --name-only --no-renames | scripts/docs-only.sh
#
# Pass `--no-renames`, so a file renamed from `.rs` to `.md` shows both of its names.
set -eu

seen=0
while IFS= read -r path || [ -n "$path" ]; do
    [ -n "$path" ] || continue
    case $path in
        crates/*/AGENTS.md | taps/*/AGENTS.md) seen=1 ;;
        crates/* | taps/*) exit 1 ;;
        *.md) seen=1 ;;
        *) exit 1 ;;
    esac
done
[ "$seen" = 1 ]
