#!/bin/sh
# The whole demo, act by act, on one record: the read path (scripts/demo.sh), then
# curation and the trust tiers, the perimeter, the task fabric, observability, and the
# evaluation, each in scripts/demo/. ACTS names which to run, by number; all by default.
#
#   scripts/demo-all.sh <demo-root> <markdown-dir>...
#
# HARNESS names the harness for the sessions that need one (act 1's task, act 4's
# subagents); the gateway acts use local/.virtual-key and llm.apache.org.
#
# DEMO_TRANSCRIPT=<file> also writes everything shown to that file, as plain text.
set -eu
root=${1:?demo root}; shift
here=$(cd "$(dirname "$0")/.." && pwd)
acts=${ACTS:-1 2 3 4 5 6 7}
if [ -n "${DEMO_TRANSCRIPT:-}" ] && [ -z "${DEMO_TEED:-}" ]; then
  # run again with everything going through tee, and keep the run's own exit code
  status=$(mktemp)
  { DEMO_TEED=1 sh "$0" "$root" "$@" 2>&1; echo $? >"$status"; } | tee "$DEMO_TRANSCRIPT"
  code=$(cat "$status"); rm -f "$status"
  echo "transcript: $DEMO_TRANSCRIPT"
  exit "$code"
fi
. "$here/scripts/demo/show.sh"
_rule '#'
printf 'Durable memory for disposable agents: the demo\n'
_wrap 0 "Shared memory and coordination for fleets of agents, built so the record outlives every agent, harness, model and vendor it was written with. One record, seeded from real decisions and docs; seven acts, each a part of the paper shown working."
printf '\n  %-9s %s\n  %-9s %s\n  %-9s %s\n  %-9s %s\n  %-9s %s\n' \
  when "$(date -u +%Y-%m-%dT%H:%MZ)" \
  commit "$(git -C "$here" rev-parse --short HEAD 2>/dev/null || echo unknown)" \
  harness "${HARNESS:-none}" \
  record "$root" \
  acts "$acts"
_rule '#'
for n in $acts; do
  case $n in
    1) sh "$here/scripts/demo.sh" "$root" "$@" ;;
    *) for f in "$here"/scripts/demo/"$n"-*.sh; do
         [ -e "$f" ] || { echo "no act $n" >&2; continue; }
         sh "$f" "$root"
       done ;;
  esac
done
