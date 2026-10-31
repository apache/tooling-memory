#!/bin/sh
# What conformance.py catches, and what it must go on passing. The fake server beside it
# is a second implementation of the surface, in memory, with one defect switched on per
# run; each defect is the kind of drift a serving layer could have, and the test holds
# which step reports it. A script that only ever passed has not been shown to test
# anything, so the cases that must fail are the ones that matter.
#
# Run by `make check`. Needs python3. The runs go in parallel, each on a record of its
# own, so the whole takes about as long as the slowest one.

set -eu

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
script="$here/conformance.py"
fake="$here/conformance-fake-server.py"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

command -v python3 >/dev/null 2>&1 || { echo "python3 is needed for the conformance test" >&2; exit 1; }

# run <name> <flaw> <args...>: one run of the script against the fake with that flaw, its
# output and exit status kept under its name
run() {
    name=$1
    flaw=$2
    shift 2
    (
        status=0
        FLAW="$flaw" FAKE_STATE="$work/$name.json" \
            python3 "$script" --stdio "python3 $fake" --lease 2 --timeout 5 "$@" \
            >"$work/$name.out" 2>&1 || status=$?
        echo "$status" >"$work/$name.status"
    ) &
}

run whole '' --operator "python3 $fake operator" --drop-index "python3 $fake operator drop-index"
run surface ''
run missing-tool missing-tool --operator "python3 $fake operator"
run write-ignores-label write-ignores-label
run t0-forgets t0-forgets --operator "python3 $fake operator"
run search-everything search-everything --operator "python3 $fake operator"
run search-bodies search-bodies --operator "python3 $fake operator"
run second-claim second-claim
run lease-forever lease-forever
run heartbeat-noop heartbeat-noop
run heartbeat-lies heartbeat-lies
run heartbeat-short heartbeat-short
run release-lies release-lies
run complete-anyone complete-anyone
run no-holder no-holder
run import-trusts import-trusts --operator "python3 $fake operator"
run highest-wrong highest-wrong --operator "python3 $fake operator"
run reindex-forgets reindex-forgets --operator "python3 $fake operator" --drop-index "python3 $fake operator drop-index"
run silent silent
wait

failures=0

# expect <name> <exit status> <last line> [<words>]: what the run must have ended with,
# and words its report must hold, so a failure at the right step is the right failure
expect() {
    name=$1
    want_status=$2
    want_tail=$3
    want_words=${4:-}
    got_status=$(cat "$work/$name.status")
    got_tail=$(tail -n 1 "$work/$name.out" | sed 's/, [0-9]* checks//')
    if [ "$got_status" != "$want_status" ] || [ "$got_tail" != "$want_tail" ] ||
        { [ -n "$want_words" ] && ! grep -qF -- "$want_words" "$work/$name.out"; }; then
        printf '%s: exit %s ending %s; wanted exit %s ending %s holding %s\n' \
            "$name" "$got_status" "$got_tail" "$want_status" "$want_tail" "${want_words:-anything}" >&2
        sed 's/^/    /' "$work/$name.out" >&2
        failures=$((failures + 1))
    fi
}

# a conformant server passes, whole and surface-only
expect whole 0 '7 of 7 steps passed'
expect surface 0 '5 of 7 steps passed; not run: 6, 7'
# and each defect is reported at the step whose claim it breaks, the rest still running
expect missing-tool 1 '6 of 7 steps passed; FAILED: 1' 'tools/list lacks heartbeat'
expect write-ignores-label 1 '1 of 7 steps passed; not run: 6, 7; could not run: 3, 4, 5; FAILED: 2' "write at public came back 'internal'"
expect t0-forgets 1 '5 of 7 steps passed; FAILED: 3, 7' 'T0 does not name the handoff'
expect search-everything 1 '5 of 7 steps passed; FAILED: 4, 7' 'search for a word only the handoff carries found'
expect search-bodies 1 '6 of 7 steps passed; FAILED: 4' 'search at summary depth returned a body'
expect second-claim 1 '4 of 7 steps passed; not run: 6, 7; FAILED: 5' 'a second claim_next while the lease holds returned a task'
expect lease-forever 1 '4 of 7 steps passed; not run: 6, 7; FAILED: 5' 'heartbeat {"id": "task-fake-0004", "lease_seconds": 2} was answered, not refused'
expect heartbeat-noop 1 '4 of 7 steps passed; not run: 6, 7; FAILED: 5' 'heartbeat did not renew it: lease_until went from'
expect heartbeat-short 1 '4 of 7 steps passed; not run: 6, 7; FAILED: 5' 'heartbeat did not renew it: lease_until went from'
expect heartbeat-lies 1 '4 of 7 steps passed; not run: 6, 7; FAILED: 5' 'a renewed lease lapsed where the first one would have'
expect release-lies 1 '4 of 7 steps passed; not run: 6, 7; FAILED: 5' 'expecting the released task back from the pool'
expect complete-anyone 1 '4 of 7 steps passed; not run: 6, 7; FAILED: 5' 'complete {"id": "task-fake-0004", "handoff": "mem-fake-0002"} was answered, not refused'
expect no-holder 1 '4 of 7 steps passed; not run: 6, 7; FAILED: 5' 'the claimed task names no holder'
expect import-trusts 1 '6 of 7 steps passed; FAILED: 6' 'import accepted an entry that does not match its digest'
expect highest-wrong 1 '6 of 7 steps passed; FAILED: 6' 'the lines say'
expect reindex-forgets 1 '6 of 7 steps passed; FAILED: 7' 'after reindex the ledger lost the completed task'
# a server that never answers is reported, not waited on
expect silent 1 '0 of 7 steps passed; not run: 6, 7; could not run: 2, 3, 4, 5; FAILED: 1' 'no reply within'

# a command that cannot start is a report, not a traceback
status=0
out=$(python3 "$script" --stdio "/nonexistent/server" 2>&1) || status=$?
case "$status:$out" in
    1:*"cannot start"*) ;;
    *) printf 'a missing server: exit %s: %s\n' "$status" "$out" >&2; failures=$((failures + 1)) ;;
esac

# the usage is refused with 2, as a shell expects
status=0
python3 "$script" >/dev/null 2>&1 || status=$?
[ "$status" = 2 ] || { printf 'no transport given: exit %s, wanted 2\n' "$status" >&2; failures=$((failures + 1)); }

if [ "$failures" -ne 0 ]; then
    printf '%s conformance test(s) failed\n' "$failures" >&2
    exit 1
fi
echo "conformance.py: ok"
