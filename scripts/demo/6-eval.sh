#!/bin/sh
# Act 6: the evaluation. The five arms of the paper's comparison, each a fixed set of
# session settings; one question under each arm through the scripted agent; the plan,
# which fixes the margin and the seeds before a run; then, on the gateway, the runs of
# the plan and their scorecard, kept in the platform's domain. The steps before the
# gateway check need no model.
. "$(dirname "$0")/lib.sh"
act 6 "The evaluation" "What memory buys, measured: the same questions, answerable only from the record, put to the same harness and model under five arms, from no memory to the system as built. A plan fixes the margin and the seeds before a run. A scorecard pairs each arm with the baseline over tasks and seeds, and says which arms pay, which fail, and which replay traps stay open." "the launcher, which gives each session its settings and never the session itself; agent:a, played by the demo script, asking one question under each arm; $WORKER_HARNESS on $WORKER_MODEL, the same harness and model in every run; the grader, a regular expression per question, not a model; and you, the owner, who scores the runs."
plan=$here/evals/demo.toml

say "The arms" "An arm is a fixed set of session settings: the off switches for memory access, T0 and prefetch, the disclosure depth, the highest tier, and whether the launcher puts kept transcripts in front of the prompt. The launcher sets them once. A session cannot change them."
a eval arms | python3 "$here/scripts/demo/evalshow.py" arms

say "One question under each arm" "agent:a starts a session under each arm, with the record as of now, and asks the same three things: its settings, T0, and a search for DR-19. Writes and the task tools work in every arm."
now=$(date -u +%Y-%m-%dT%H:%M:%SZ)
calls='[{"tool": "settings", "arguments": {}}, {"tool": "t0", "arguments": {"domain": "dom-demo"}}, {"tool": "search", "arguments": {"query": "DR-19 wiki MoinMoin", "domain": "dom-demo"}}]'
errors=$(mktemp)
for arm in $(a eval arms | j "' '.join(x['arm'] for x in d)"); do
  got=$(MEMORY_ARM=$arm MEMORY_AS_OF=$now agent agent:a steps "$calls" 2>"$errors") || true
  if [ -z "$got" ]; then
    # the memory server did not start under this arm: its reason
    printf '  %-18s no session: %s\n' "$arm" "$(sed -n 's/^error: //p' "$errors" | head -n 1)"
    continue
  fi
  printf '%s\n' "$got" | python3 "$here/scripts/demo/evalshow.py" under "$arm"
done
rm -f "$errors"

say "The plan" "The evaluation set is also the plan: the arms, the baseline, the seeds, the margin and the judge, fixed before a run. Each run names the digest of the plan it started under, and the scorer refuses an outcome whose run names another digest."
a eval plan "$plan" | python3 "$here/scripts/demo/evalshow.py" plan

gateway || { echo; echo "skipped: the runs of the plan and their scorecard need the gateway (local/.virtual-key)"; exit 0; }

# The runs come before the platform's domain is granted: with two granted domains a
# prompted launch resolves no domain for its transcript, and keeps none.
say "The runs" "Each task of the plan runs once per seed under each arm: one launch per run, in the same sandbox, with the same tools. Every run reads the record at one snapshot, taken before the first run, so no run reads what another run wrote. Two tasks by default; EVAL_TASKS names others."
arms=$(a eval plan "$plan" | j "','.join(d['plan']['arms'])")
log=$(mktemp)
python3 "$here/scripts/eval.py" "$plan" --root "$root" --harness "$WORKER_HARNESS" --auth gateway \
  --model "$WORKER_MODEL" --conditions "$arms" --only "${EVAL_TASKS:-dr19-decision,dr19-spike}" 2>&1 | tee "$log" | sed 's/^/  /' || true
outcomes=$(sed -n 's/^outcomes //p' "$log")
rm -f "$log"

say "The scorecard" "The owner scores the runs. The tape of each run gives its task, seed, arm, harness and model. Each arm is paired with the baseline over task and seed; the differences have an interval. An open replay trap makes each verdict inconclusive, and the finding from the numbers stays beside it. The scorecard is kept as a decision entry in the platform's domain."
if [ -z "$outcomes" ] || [ "$(j 'len(d)' <"$outcomes")" = 0 ]; then
  echo "  no outcomes: no run left a session id and a token count, so there is nothing to score"
  exit 0
fi
platform_domain
a eval score "$plan" "$outcomes" --keep | python3 "$here/scripts/demo/evalshow.py" scorecard
a eval scorecards | j "'\n'.join(f\"  {k['entry']}  {k['created'][:19]}Z  plan {k['scorecard']['plan']}, {len(k['scorecard']['arms'])} arms, by {k['author']}\" for k in d) or '  (no scorecard is kept)'"
