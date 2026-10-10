#!/bin/sh
# Act 5: watching it. Traces to a collector: model calls, reads, policy decisions and
# task transitions; the paper's read metrics; vendor-drift probes; and transcripts kept
# as sources, reached only by citation. Each part runs when this build has it.
. "$(dirname "$0")/lib.sh"
act "Act 5: observability"
has() { a "$1" --help >/dev/null 2>&1; }

say "traces: every model call, read, policy decision and task transition is a span, sent when a collector is named"
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
  say "the paper's read metrics, from the read and write logs"
  a savings | sed -n '/Metrics/,/^$/p' | sed 's/^/  /'
fi

if has drift && gateway; then
  say "vendor drift: a pinned probe battery against a model, compared with its own baseline"
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
  say "transcripts are sources: kept in the record, never searched, reached only by citation, by turn"
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
      say "frozen-context replay: the decision contexts the egress recorded, sent again to $WORKER_MODEL"
      a replay "$s" --limit 4 --model "$WORKER_MODEL" | j "f\"  agreement {d['agreement']:.0%} with the recorded steps\n\" + '\n'.join(f\"    turn {p['turn']:>2}: recorded {p['recorded_tool'] or 'a reply'}, replayed {p['replayed_tool'] or 'a reply'}  {'agrees' if p['agrees'] else 'differs'}\" for p in d['points'])" || true
      echo "  (the recorded side is the harness's own transcript, a self-report; see the README)"
    fi
  fi
fi
