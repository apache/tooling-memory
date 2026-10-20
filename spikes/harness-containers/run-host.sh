#!/bin/sh
# The control: the check run unconfined on the host, where it must fail on point 1 and on
# the marker. A check that passes here tests nothing.
here=$(cd "$(dirname "$0")" && pwd)
. "$here/fixture.sh"
trap fixture_down EXIT
set +e
CHECK_HOME="$FIX/home" CHECK_WORK="$FIX/work" CHECK_MEMORY_BIN="$FIX/opt/memory/memory" \
    CHECK_PROJECTION="$FIX/projection" CHECK_SOCKET="$FIX/run/egress.sock" \
    python3 -I "$FIX/work/contract-check.py"
echo "exit=$?"
