#!/bin/sh
# Act 5: watching it. Traces to a collector: model calls, reads, policy decisions and
# task transitions; the paper's read metrics; vendor-drift probes; and transcripts kept
# as sources, reached only by citation. Each part runs when this build has it. The last
# step is the freeze: a drift window, and what it stops. It needs no model.
. "$(dirname "$0")/lib.sh"
act 5 "Watching it" "What the system shows about itself: traces of every model call, read, decision and task transition; the paper's read metrics; probes for a vendor changing a model under its name; transcripts kept as sources; and replay of a session's decisions on another model." "the system itself: a loopback OpenTelemetry sink counting spans; the drift probes, run against $WORKER_MODEL and then $AUDITOR_MODEL; the egress, whose recorded decision contexts are replayed; and $WORKER_MODEL, replaying a session another model ran."
has() { a "$1" --help >/dev/null 2>&1; }
# the open drift windows, one `destination model` per line
open_windows() { a drift window list | j "'\n'.join(w['destination'] + ' ' + w['model'] for w in d if not w['until'])"; }
# the windows that were open before this act: the act leaves them as they are
has drift && windows_before=$(open_windows) || windows_before=

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
  # whole names only: an attribute such as task.created_by is not a task.created span. The
  # sink keeps raw protobuf, where a name can run straight into the next field's tag byte,
  # often a digit, so -w would miss it: only a letter, `_` or `.` continues a name.
  re=$(printf '%s' "$name" | sed 's/\./\\./g')
  printf '  %-18s %s\n' "$name" "$(grep -a -o -P "(?<![A-Za-z_.])$re(?![A-Za-z_.])" "$spans" | wc -l | tr -d ' ')"
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
  echo "  a repoint, as a vendor swapping the model behind a name would look: $AUDITOR_MODEL against $WORKER_MODEL's baseline"
  a drift probe --auth gateway --model "$AUDITOR_MODEL" >/dev/null
  # --file: confirmed drift is filed as a task for the owner, and it opens a drift window
  a drift check --provider llmao --model "$AUDITOR_MODEL" --baseline-model "$WORKER_MODEL" --file --domain dom-demo | python3 -c "import json,sys; d=json.load(sys.stdin); $show_drift"
fi

if has transcript; then
  say "Transcripts are sources" "A session's transcript is kept in the record on a clock, never searched and never in T0, and reached only through an entry that cites one of its turns."
  a transcript list | j "'\n'.join(f\"  {t['session']}  {t['harness']:<8} {t['turn_count']:>3} turns  {t['classification']:<9} {t['state']:<7} until {t['retain_until'][:10]}\" for t in d) or '  (none kept yet)'"
  # act 4's delegating session: of this harness's live sessions, the latest kept with a
  # cited turn. A run that leaves no handoff of its own gets the hook's session-end note,
  # which cites its turns, and here that is the run that delegated and stopped. Session ids
  # are random, so neither the list's order nor its last entry says which
  s=$(a transcript list | j "' '.join(t['session'] for t in sorted(d, key=lambda t: t['kept_at'], reverse=True) if t['state']=='live' and t['harness']=='${HARNESS:-}')")
  # `continue` rather than a bare test: with no cited turn anywhere, a trailing failed
  # test makes the substitution exit 1, and `set -e` then ends the whole demo here with
  # nothing printed -- which is what happens whenever an earlier act left no session
  # with a cited turn
  s=$(for c in $s; do
        [ -n "$(a transcript show "$c" | j "next(iter(d['cited']), '')")" ] || continue
        echo "$c"; break
      done)
  [ -n "$s" ] || s=$(a transcript list | j "next((t['session'] for t in sorted(d, key=lambda t: t['kept_at'], reverse=True) if t['state']=='live'), '')")
  if [ -n "$s" ]; then
    turn=$(a transcript show "$s" | j "next(iter(d['cited']), '')")
    if [ -n "$turn" ]; then
      echo "  $s turn $turn, cited by $(a transcript show "$s" | j "', '.join(d['cited']['$turn'])"):"
      a transcript show "$s" --turn "$turn" | j "'    ' + d['role'] + ': ' + d['text'][:160].replace(chr(10), ' ')"
    fi
    echo "  search never reaches a transcript: searching for its session id finds $(a search "$s" | j 'len(d)') hits"
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

if has drift; then
  say "The freeze" "Confirmed drift opens a drift window for the destination and the model. Nothing that an agent wrote inside a window is promoted, also after the window closes: the change of the model may be in it. The agent writes the entry again, and that one is promoted."
  # one promotion to curated by agent:curator: the outcome, or the refusal
  promote() {
    got=$(agent agent:curator promote "{\"id\": \"$1\", \"to\": \"curated\", \"why\": \"read against its source\"}" 2>&1) || true
    case $got in
      refused:*) echo "$got" | fold -s -w 70 | sed 's/^/      /' ;;
      *) echo "$got" | j "f\"      {d['outcome']}: {d['id']} is {d['tier']}\"" ;;
    esac
  }
  # the windows this act opened: a window that was open before the act stays as it is
  mine() { open_windows | while read -r destination model; do [ -n "$destination" ] || continue; echo "$windows_before" | grep -qxF "$destination $model" || echo "$destination $model"; done; }
  if [ -z "$(mine)" ]; then
    a drift window open --provider llmao --model "$WORKER_MODEL" --why "The demo opens a window by hand: no check confirmed drift in this run." >/dev/null
    echo "  no check opened a window in this run, so the owner opens one by hand:"
  else
    echo "  the repoint check confirmed drift, and that opened a window:"
  fi
  a drift window list | j "'\n'.join(f\"    {w['destination']}/{w['model']} from {w['from'][:19]}Z, {'open' if not w['until'] else 'closed ' + w['until'][:19] + 'Z'}; detectors: {', '.join(w['detectors']) or 'none, opened by hand'}\" for w in d if not w['until'])"
  # the wall clock of a host can step back by a part of a second; the pauses keep each
  # write and each close on its own side of the window's times
  sleep 2
  inside=$(agent agent:a write '{"title": "The mirror list, read inside the window", "domain": "dom-demo", "body": "Three mirrors carry the release."}' | j "d['id']")
  echo "  agent:a writes $inside inside the window; agent:curator promotes it to curated:"
  promote "$inside"
  sleep 2
  # close what this act opened, by a check or by hand; a window that stays open would
  # also refuse the approvals of act 7
  mine | while read -r destination model; do
    a drift window close --provider "$destination" --model "$model" --why "The demo closes the window: the change is understood." >/dev/null
    echo "  the owner closes the window of $destination/$model"
  done
  echo "  the same promotion, after the close:"
  promote "$inside"
  sleep 2
  after=$(agent agent:a write '{"title": "The mirror list, read after the window", "domain": "dom-demo", "body": "Three mirrors carry the release. Read again after the window closed."}' | j "d['id']")
  echo "  agent:a writes the entry again as $after; agent:curator promotes that one:"
  promote "$after"
fi
