#!/bin/sh
# Act 6: the evaluation. The same questions, answerable only from the record, put to a
# harness with memory and with an empty one; graded, timed, and counted in tokens.
. "$(dirname "$0")/lib.sh"
act "Act 6: memory against no memory"
gateway || { echo "skipped: the evaluation runs through the gateway (local/.virtual-key)"; exit 0; }
say "$WORKER_HARNESS on the gateway's $WORKER_MODEL, with the record and with an empty one"
python3 "$here/scripts/eval.py" "$here/evals/demo.toml" --root "$root" --harness "$WORKER_HARNESS" \
  --auth gateway --model "$WORKER_MODEL" --conditions memory,empty 2>&1 | tail -15
