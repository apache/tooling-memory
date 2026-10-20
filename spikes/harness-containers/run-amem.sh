#!/bin/sh
# The baseline: our own Linux backend, `amem launch --harness command`, running the check
# as the workload. The working tree keeps its host path inside, so the check is told
# where it is; everything else is at the fixed layout. AMEM names the binary to use.
here=$(cd "$(dirname "$0")" && pwd)
amem=${AMEM:-$here/../../target/debug/amem}
. "$here/fixture.sh"
trap fixture_down EXIT
set +e
"$amem" --root "$FIX/root" init --owner human:spike:me --domain dom-spike >/dev/null
OPENAI_API_KEY=sk-placeholder "$amem" --root "$FIX/root" launch --harness command --auth key \
    --workdir "$FIX/work" -- \
    sh -c "CHECK_WORK='$FIX/work' CHECK_HOST_MARKER='$CHECK_HOST_MARKER' python3 -I '$FIX/work/contract-check.py'" \
    </dev/null
echo "exit=$?"
