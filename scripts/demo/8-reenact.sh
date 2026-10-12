#!/bin/sh
# Act 8: run it again. Every prompted session leaves a tape: its prompt, its harness and
# model, and each memory call with what it returned. Re-enactment runs the session
# again, on the same model or another, with the memory calls answered from the tape
# until the run asks for something the original did not; then it reports how far the
# run kept to the original. Replay (5.5) resends recorded model calls; this runs the
# whole session.
. "$(dirname "$0")/lib.sh"
act 8 "Run it again" "A session recorded in act 6 run again from its tape: once on its own model, as a control, and once on another, each scored on how far it kept to the original's memory calls, beside its answer, tokens and time." "$WORKER_HARNESS, re-running a session from act 6 on $WORKER_MODEL and then on $CHECKER_MODEL; the tape, answering its memory calls; and the report."

say "The tapes" "Each prompted session's prompt, harness, model and memory calls, kept with the transcripts and on their clock unless pinned."
tape=$(a reenact tapes | j "next((t['session'] for t in d if t.get('prompt') and t.get('harness') == '$WORKER_HARNESS' and t.get('model') == '$WORKER_MODEL'), '')")
if [ -z "$tape" ]; then
  echo "  no prompted $WORKER_HARNESS session on $WORKER_MODEL to run again: act 6 runs them"
  exit 0
fi
a reenact tapes | j "f\"  {len([t for t in d if t.get('prompt')])} tapes with a prompt, of {len(d)}\""
a reenact show "$tape" | j "f\"  {d['header']['session']}: {d['header']['harness']} on {d['header']['model']}, {len(d['calls'])} memory calls; its prompt:\n    \" + d['header']['prompt'].split('; it holds what you need. ')[-1][:200]"

if gateway; then
  run() {
    ( cd "$(mktemp -d)" && a reenact run "$tape" --auth gateway --model "$1" --runs 1 2>/dev/null ) \
      | j "f\"  {d['model']}: {'the same tools in the same order as the tape' if d['alignment'] == 1 else 'its tools in line with the tape' + chr(39) + 's: alignment ' + format(d['alignment'], '.0%')}; {'every call asked as on the tape' if not d['diverged'] else 'first asked differently at call ' + str(d['steps_to_divergence'] + 1) + ' of ' + str(d['tape_calls'])}; {d['seconds']:.0f} s, {d['tokens']['input']}+{d['tokens']['cache_read']}c tokens in\n    answer: {d['answer'].strip().splitlines()[0][:150] if d['answer'].strip() else '(none)'}\""
  }
  say "The same model again" "The control: $WORKER_HARNESS on $WORKER_MODEL, as the original ran. A model is not deterministic, so even this may differ."
  run "$WORKER_MODEL" || true
  say "Another model" "$WORKER_HARNESS on $CHECKER_MODEL, answered from the same tape: whether it asks memory the same things, and comes to the same answer."
  run "$CHECKER_MODEL" || true
  say "Side by side" "Every run of the session, each harness and model against the original."
  a reenact report "$tape" | sed 's/^/  /'
fi
