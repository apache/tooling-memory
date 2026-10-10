#!/bin/sh
# The whole demo, act by act, on one record: the read path (scripts/demo.sh), then
# curation and the trust tiers, the perimeter, the task fabric, observability, and the
# evaluation, each in scripts/demo/. ACTS names which to run, by number; all by default.
#
#   scripts/demo-all.sh <demo-root> <markdown-dir>...
#
# HARNESS names the harness for the sessions that need one (act 1's task, act 4's
# subagents); the gateway acts use local/.virtual-key and llm.apache.org.
set -eu
root=${1:?demo root}; shift
here=$(cd "$(dirname "$0")/.." && pwd)
acts=${ACTS:-1 2 3 4 5 6}
for n in $acts; do
  case $n in
    1) sh "$here/scripts/demo.sh" "$root" "$@" ;;
    *) for f in "$here"/scripts/demo/"$n"-*.sh; do
         [ -e "$f" ] || { echo "no act $n" >&2; continue; }
         sh "$f" "$root"
       done ;;
  esac
done
