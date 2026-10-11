#!/bin/sh
# Act 6: the evaluation. The same questions, answerable only from the record, put to a
# harness with memory and with an empty one; graded, timed, and counted in tokens.
. "$(dirname "$0")/lib.sh"
act 6 "Memory against no memory" "The same questions, about decisions in this record, put to the same harness and model twice: once with this memory, once with an empty one. Graded, timed and counted." "$WORKER_HARNESS on $WORKER_MODEL, the same harness and model in both runs; the memory server, with this record and then an empty one; and the grader, a regular expression per question, not a model."
gateway || { echo "skipped: the evaluation runs through the gateway (local/.virtual-key)"; exit 0; }
say "$WORKER_HARNESS on $WORKER_MODEL, with and without" "Four questions, each run in the same sandbox with the same tools; the only difference is what is in memory."
python3 "$here/scripts/eval.py" "$here/evals/demo.toml" --root "$root" --harness "$WORKER_HARNESS" \
  --auth gateway --model "$WORKER_MODEL" --conditions memory,empty 2>&1 | tail -15
