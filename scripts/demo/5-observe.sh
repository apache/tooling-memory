#!/bin/sh
# Act 5: watching it. Traces to a collector: model calls, reads, policy decisions and
# task transitions; the paper's read metrics; vendor-drift probes; and transcripts kept
# as sources, reached only by citation. Each part runs when this build has it.
. "$(dirname "$0")/lib.sh"
act 5 "Watching it" "What the system shows about itself: traces of every model call, read, decision and task transition; the paper's read metrics; probes for a vendor changing a model under its name; transcripts kept as sources; and replay of a session's decisions on another model."
has() { a "$1" --help >/dev/null 2>&1; }

say "Traces" "Every model call, read, policy decision and task transition is an OpenTelemetry span, sent when a collector is named. Here a small loopback sink counts them."
spans=$(mktemp)
python3 "$here/scripts/demo/otlp_sink.py" "$spans" 4319 & sink=$!
sleep 1
export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4319
a search "release" >/dev/null
t=$(a task create "A traced task" --domain dom-demo --body "Traced." | j "d['id']")
a task admit "$t" >/dev/null
a task invalidate "$t" --reason "a demo" >/dev/null 2>&1 || true
agent agent:a write '{"title": "Traced", "body": "A traced write."}' >/dev/null || true
if gateway; then a recall "what did DR-19 decide" --provider llmao --auth gateway ${RECALL_MODEL:+--model "$RECALL_MODEL"} >/dev/null 2>&1 || true; fi
unset OTEL_EXPORTER_OTLP_ENDPOINT
sleep 1; kill "$sink" 2>/dev/null || true
for name in memory.read policy.decision task.created task.admitted task.invalidated model.call; do
  printf '  %-18s %s\n' "$name" "$(grep -a -o "$name" "$spans" | wc -l | tr -d ' ')"
done
echo "  (a loopback sink for the demo; the same variable points amem at Jaeger or an OpenTelemetry collector)"

if a savings --help 2>/dev/null | grep -q . && a savings | grep -q "Metrics"; then
  say "The read metrics" "Fetch-through, retrieval depth per task, prefetch precision and wasted reads, computed from the read and write logs."
  a savings | sed -n '/Metrics/,/^$/p' | sed 's/^/  /'
fi

if has drift && gateway; then
  say "Vendor drift" "A pinned battery of probes against a model, compared with its own baseline. Two runs of the same model stay quiet; a different model behind the same name fires the detectors."
  show_drift="print('  verdict: ' + d['verdict'] + ('; our side changed: ' + str(d['attribution']['differences']) if d['attribution']['ours_changed'] else '; our side pinned, unchanged')); print('\\n'.join(f\"    {x['name']:<13} {'FIRED' if x['fired'] else 'quiet':<6} {x['catches']}\" for x in d['detectors']))"
  a drift probe --auth gateway --model "$WORKER_MODEL" >/dev/null
  a drift probe --auth gateway --model "$WORKER_MODEL" >/dev/null
  echo "  $WORKER_MODEL, two runs of the same pinned probes:"
  a drift check --provider llmao --model "$WORKER_MODEL" | python3 -c "import json,sys; d=json.load(sys.stdin); $show_drift"
  echo "  a repoint, as a vendor swapping the model behind a name would look: $CHECKER_MODEL against $WORKER_MODEL's baseline"
  a drift probe --auth gateway --model "$CHECKER_MODEL" >/dev/null
  a drift check --provider llmao --model "$CHECKER_MODEL" --baseline-model "$WORKER_MODEL" | python3 -c "import json,sys; d=json.load(sys.stdin); $show_drift"
fi

if has transcript; then
  say "Transcripts are sources" "A session's transcript is kept in the record on a clock, never searched and never in T0, and reached only through an entry that cites one of its turns."
  a transcript list | j "'\n'.join(f\"  {t['session']}  {t['harness']:<8} {t['turn_count']:>3} turns  {t['classification']:<9} {t['state']:<7} until {t['retain_until'][:10]}\" for t in d) or '  (none kept yet)'"
  s=$(a transcript list | j "next((t['session'] for t in reversed(d) if t['state']=='live'), '')")
  if [ -n "$s" ]; then
    turn=$(a transcript show "$s" | j "next(iter(d['cited']), '')")
    if [ -n "$turn" ]; then
      echo "  $s turn $turn, cited by $(a transcript show "$s" | j "', '.join(d['cited']['$turn'])"):"
      a transcript show "$s" --turn "$turn" | j "'    ' + d['role'] + ': ' + d['text'][:160].replace(chr(10), ' ')"
    fi
    echo "  searching the record for its words finds the entries, never the transcript: $(a search "$s" | j 'len(d)') hits"
    if gateway; then
      say "Frozen-context replay" "The egress recorded what each model call of that session was sent. Sent again with only the model changed, here to $WORKER_MODEL, each decision point is scored against what was done."
      # the session just shown: a harness at work, whose recorded calls go back where
      # they went, or, for a native Claude session, to the gateway, which speaks Messages
      rs=$s
      [ -n "$rs" ] && echo "  session $rs, its recorded calls sent again with only the model changed:"
      [ -n "$rs" ] && a replay "$rs" --limit 4 --model "$WORKER_MODEL" | j "(f\"  recorded at {d['retargeted_from']}, replayed at {d['destination']}: an accuracy, speed and cost comparison across vendors\n\" if d.get('retargeted_from') else '') + f\"  agreement {d['agreement']:.0%} with the recorded steps, observed by the {d['observed_by']}\n\" + '\n'.join(f\"    turn {p['turn']:>2}: recorded {p['recorded_tool'] or 'a reply'}, replayed {p['replayed_tool'] or 'a reply'}  {'agrees' if p['agrees'] else 'differs'}\" for p in d['points']) + ('\n  (the egress saw these calls itself; a run with no contexts would replay the transcript, a self-report, and say so)' if d['observed_by'] == 'egress' else '\n  (no contexts: the recorded side is the transcript, a self-report by the harness)')" || true
    fi
  fi
fi
