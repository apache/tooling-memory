#!/bin/sh
# Act 4: the task fabric. A strategy compiles into the rubric; a rescore ranks the
# queue along its edges and keeps curation's floor; a harness that would spawn
# subagents has them filed on the board instead; the flow is counted; and in an outage
# a session works from the T0 it loaded and its handoff waits on a queue.
. "$(dirname "$0")/lib.sh"
act 4 "The task fabric" "Work is a shared ledger of tasks with leases and handoffs. A strategy compiles into the rubric that ranks it, the ranking moves along dependencies, and a harness that would spawn subagents files them on the board instead, where any worker can take them. In an outage, a session works from the T0 that it loaded, and its handoff waits on a queue." "you, who file the tasks and verify the strategy; the ledger, which ranks and moves them, not a worker; ${HARNESS:-a harness} on your login (agent:${HARNESS:-...}), the session asked to delegate and later the continuation; $WORKER_HARNESS on $WORKER_MODEL, the board's worker for each subagent task; and in the last step agent:worker, played by the demo script, on a scratch record."

say "A strategy that compiles into the rubric" "An L1 policy entry says what matters this period: work a running task filed comes first, curation keeps a floor. A person verifies it, and the rubric the queue is ranked by is compiled from it."
strategy=$(a write --kind policy --title "pol-L1-007 This period: subagent work first" <<'POL' | j "d['id']"
# This period, work filed by a running task comes first

```policy
id = "pol-L1-007"
layer = 1
rule = "Prioritise work a running task filed over new work."
intent = "Finishing what is started beats starting more; curation keeps a fifth of the work."

[rubric]
unblock_weight = 1
maintenance_floor = 0.2

[rubric.origin_weights]
"task:" = 2
"continues:" = 3
"curator:" = -1
```
POL
)
a approve "$(agent agent:curator promote "{\"id\": \"$strategy\", \"to\": \"verified\", \"why\": \"agreed for this period\"}" | j "d['task']['id']")" >/dev/null
echo "  pol-L1-007 is verified and in force"

say "A small queue" "A task blocked on another, and a small unrelated one. On admission the blocker rises above what it blocks: a move along the ledger's edge, made by the ledger, not by a worker."
blocker=$(a task create "Migrate the fixtures" --domain dom-demo --body "Needed before the release." | j "d['id']")
ranked=$(a task create "Cut the release" --domain dom-demo --priority 8 --after "$blocker" --body "Blocked on the fixtures." | j "d['id']")
plain=$(a task create "Tidy the README" --domain dom-demo --priority 1 --body "Small." | j "d['id']")
for t in "$blocker" "$ranked" "$plain"; do a task admit "$t" >/dev/null; done
echo "  admitted: the blocker rose above what it blocks, a move along the ledger's edge, not a worker's"
for st in open blocked; do
  a task list --status $st | j "'\n'.join(f\"  {t['priority']:>3}  {t['status']:<8} {t['title']:<24} {'(blocks Cut the release)' if t['id'] == '$blocker' else ''}\" for t in (d if isinstance(d, list) else d.get('tasks', [])) if t['id'] in ('$blocker', '$ranked', '$plain'))"
done
echo "  Cut the release is blocked, not open: no session can claim it until the fixtures are done"

say "A claim has a generation" "Every claim of a task gets the next generation, and what a holder does under a claim carries it. A holder whose lease ran out is refused by its generation, even when the session that took the task has the same name. What it learned still reaches the next holder, as a handoff left after its claim."
fenced=$(a task create "Check the mirror list" --domain dom-demo --priority 50 --body "One claim is lost, and one more is made." | j "d['id']")
a task admit "$fenced" >/dev/null
first=$(a task claim-next --domain dom-demo --lease-seconds 1 | j "d['generation']")
sleep 2
second=$(a task claim-next --domain dom-demo --lease-seconds 600 | j "d['generation']")
echo "  claimed at generation $first with a lease of one second; the lease ran out, and the same name claimed it at generation $second"
note=$(a write --kind handoff --domain dom-demo --title "Handoff: the mirror list, first pass" --body "Two mirrors answered; the third timed out. Start with the third." | j "d['id']")
echo "  the first holder completes, under generation $first:"
a task complete "$fenced" "$note" --generation "$first" 2>&1 | sed 's/^/    /' || true
a task late-handoff "$fenced" "$note" --generation "$first" >/dev/null
echo "  refused. Its handoff is left on the task for the holder now: late-handoff, generation $first"
a task release "$fenced" --generation "$second" >/dev/null
a task invalidate "$fenced" --reason "obsolete: shown" >/dev/null
say "Waiting, and an effect nobody has settled" "A session does not wait for an outside event. It gives the task up until the event comes, and the tap's event puts the task back on offer. A holder records an external action before it makes it. When its lease then runs out, nobody knows if the destination holds the effect, so the ledger gives the task to nobody until a person settles it."
# claim the next task and say whether it is $1; anything else goes back to the queue
offer() {
  got=$(a task claim-next --domain dom-demo --lease-seconds "${2:-600}")
  gid=$(printf '%s' "$got" | j "d['id'] if d else ''")
  ggen=$(printf '%s' "$got" | j "d['generation'] if d else ''")
  if [ "$gid" = "$1" ]; then
    echo "  claim-next gave the task, at generation $ggen"
  elif [ -n "$gid" ]; then
    a task release "$gid" --generation "$ggen" >/dev/null
    echo "  claim-next gave another task, not this one, though it ranks first"
  else
    echo "  claim-next gave nothing"
  fi
}
waits=$(a task create "Land the fixture change" --domain dom-demo --priority 60 --reversibility irreversible --touches "fixtures/**" --body "Open the change, then wait for its check." | j "d['id']")
a task admit "$waits" >/dev/null
offer "$waits"
a task wait "$waits" check:fixtures-42 --generation "$ggen" | j "f\"  given up by its holder: {d['status']}, waiting on {d['waiting_on']}\""
offer "$waits"
a task arrived check:fixtures-42 --domain dom-demo | j "f\"  the event check:fixtures-42 came: {len(d)} task is {d[0]['status']} again\""
offer "$waits" 1
a task effect-begin "$waits" push-fixtures-1 --generation "$ggen" | j "f\"  the holder records the action before it makes it: effects {d['effects']}\""
sleep 2
echo "  the lease of one second ran out, and the holder never said what became of the push"
offer "$waits"
a task effect-settle "$waits" push-fixtures-1 not_applied | j "f\"  a person looked at the destination and settled it as not applied: effects {d.get('effects', [])}\""
offer "$waits"
a task release "$waits" --generation "$ggen" >/dev/null
a task invalidate "$waits" --reason "obsolete: shown" >/dev/null
say "A position" "A standing role is a record: a charter, an authority envelope, and the work pinned to it. A person verifies it. A pinned task goes only to a session that fills the position, and a fresh session in the position starts from the position's handoff cache, not from an earlier session's transcript."
position=pos-demo-release
# a tool call as an agent whose session a launcher put in the position
seated() { MEMORY_POSITION=$position python3 "$here/scripts/demo/mcp.py" "$memory" "$root" "$@"; }
charter=$(sed "s/@AGENT@/$WORKER_HARNESS/" <<'POS' | a write --kind position --domain dom-demo --title "The release coordinator" | j "d['id']"
# The release coordinator

```position pos-demo-release
[charter]
orders = "Announce each release to the list after the mirrors carry it. One voice writes the announcements."
covers = []
agent = "@AGENT@"

[envelope]
tools = []
handling = "internal"
```
POS
)
a approve "$(agent agent:curator promote "{\"id\": \"$charter\", \"to\": \"verified\", \"why\": \"the release manager agreed the charter\"}" | j "d['task']['id']")" >/dev/null
echo "  $position is verified and in force; its charter names the agent definition $WORKER_HARNESS"
pinned=$(a task create "Announce release 1.4" --domain dom-demo --priority 60 --body "Tell the list that 1.4 is out, once the three mirrors carry it." --position "$position" --pin-reason continuity | j "d['id']")
a task admit "$pinned" >/dev/null
echo "  filed and admitted: Announce release 1.4, pinned to $position for continuity"
echo "  a session that fills no position asks for the task by name:"
agent agent:worker claim "{\"id\": \"$pinned\", \"lease_seconds\": 600}" 2>&1 | sed 's/^/    /' || true
positions() {
  a position list --domain dom-demo | j "'\n'.join(f\"  {p['name']}  {p['state']:<8} agent definition {p['agent']}; pinned {p['pinned']}, waiting {len(p['waiting'])}, live sessions {len(p['sessions'])}, wake {str(p['wake']).lower()}\" for p in d if p['name'] == '$position' and p['state'] != 'none')"
}
positions
worked=$(seated "agent:$WORKER_HARNESS" steps "[
  {\"tool\": \"claim_next\", \"arguments\": {\"domain\": \"dom-demo\", \"lease_seconds\": 600}},
  {\"tool\": \"write\", \"arguments\": {\"entry_type\": \"handoff\", \"domain\": \"dom-demo\", \"title\": \"Handoff: release 1.4, the mirrors\", \"body\": \"Mirrors 1 and 2 checked; the third is next.\"}}]")
claim=$(printf '%s\n' "$worked" | sed -n 1p)
generation=$(printf '%s\n' "$claim" | j "d['result']['generation']")
note=$(printf '%s\n' "$worked" | sed -n 2p | j "d['result']['id']")
printf '%s\n' "$claim" | j "f\"  a session of agent:$WORKER_HARNESS that fills $position asks for the next task: {d['result']['task']['title']}, generation {d['result']['generation']}\""
seated "agent:$WORKER_HARNESS" release "{\"id\": \"$pinned\", \"generation\": $generation, \"handoff\": \"$note\"}" >/dev/null 2>&1
echo "  it wrote the handoff $note, which the memory server cited to $position, and released the task with it"
echo "  a fresh session fills $position and loads T0; the position's section comes first:"
seated "agent:$WORKER_HARNESS" t0 '{"domain": "dom-demo"}' 2>/dev/null | awk '/^## Position /{on=1; print; next} /^## /{on=0} on' | sed 's/^/    /'

say "Secondment, waking and retirement" "A session under another agent definition than the charter's is seconded: it reads what the position holds and does its task, and it does not write the position's history. A launcher reads which positions wait and starts a session in each. The owner retires a position: its tasks return to matching, and its history stays."
seated agent:substitute steps "[
  {\"tool\": \"position\", \"arguments\": {\"name\": \"$position\", \"domain\": \"dom-demo\"}},
  {\"tool\": \"write\", \"arguments\": {\"domain\": \"dom-demo\", \"title\": \"A note for the coordinator\", \"body\": \"The third mirror answered at noon.\", \"cites\": [\"$position\"]}}]" \
  | python3 -c "
import json, sys
seen, wrote = (json.loads(line) for line in sys.stdin)
view = seen['result']
print(f\"  agent:substitute fills $position: seconded {str(view['seat']['seconded']).lower()}; it reads {len(view['commitments'])} open commitment and {len(view['handoffs'])} handoff in the cache\")
print('  its write that cites the position: ' + ('refused: ' + wrote['refused'] if not wrote['ok'] else 'allowed, which is a defect'))"
positions
if gateway; then
  echo "  the launcher wakes the position: $WORKER_HARNESS on $WORKER_MODEL, under the agent definition the charter names"
  ( cd "$(mktemp -d)" && a work --harness "$WORKER_HARNESS" --auth gateway --model "$WORKER_MODEL" --backend namespaces --wake --lease-seconds 900 2>&1 | grep -E '^amem: (woke|claimed|task|nothing)' ) || true
  a task get "$pinned" | j "f\"  {d['id']}  {d['status']:<8} {d['title']}\""
else
  echo "  skipped: no gateway key, so no session was started by 'amem work --wake'; the task still waits"
fi
a position retire "$position" --domain dom-demo --why "shown" | j "f\"  retired by {d['retired_by']}; the pin was taken off: {', '.join(d['unpinned']) or 'no open task'}\""
a task get "$pinned" | j "f\"  {d['id']}  {d['status']:<8} pinned to {d.get('position') or 'no position'}\""
positions
if [ "$(a task get "$pinned" | j "d['status']")" = open ]; then a task invalidate "$pinned" --reason "obsolete: shown" >/dev/null; fi
say "The rescore" "Each cycle the queue is ranked again; every move is a ledger event with its reason, and curation's share of the work is held to its floor."
a task rescore | j "f\"  rubric {d['rubric']}; curation's share of the completed work {d['maintenance_share']:.0%} against a floor of {d['floor']:.0%}\n\" + '\n'.join(f\"  {c['from']:>3} -> {c['to']:<3} {c['title']:<40} {c['why']}\" for c in d['changed'])"

if [ -n "${HARNESS:-}" ]; then
  say "Subagents on the board" "A $HARNESS session is asked to delegate. Each subagent it would spawn is filed as a task of its own instead, with a continuation that waits for them, so the work survives the session and any worker can do it. The original task goes back on the board but waits: nobody may claim it while its continuation is open."
  parent=$(a task create "Compare two decisions" --domain dom-demo --priority 20 --body "Use two subagents, one per decision, to summarise DR-19 and DR-22 from the memory record; then combine their answers into a handoff and complete this task." | j "d['id']")
  a task admit "$parent" >/dev/null
  ( cd "$(mktemp -d)" && a work --harness "$HARNESS" --auth "${AUTH:-subscription}" $MODEL_ARG --lease-seconds 900 --task "$parent" 2>&1 | grep -E '^amem: (claimed|task|board|filed)' ) || true
  tree() {
    a task tree "$parent" | j "'\n'.join(f\"  {c['id']}  {(c.get('origin') or '').split(':')[0]:<10} {c['status']:<11} {c['title']}\" for c in d.get('children', []))"
    a task get "$parent" | j "f\"  {d['id']}  original   {d['status']:<11} {d['title']}\" + ('  (waits on its continuation)' if d['status'] == 'open' else '  (closed by its continuation, with its handoff)' if d['status'] == 'done' else '')"
  }
  tree
  kids=$(a task tree "$parent" | j "' '.join(c['id'] for c in d.get('children', []) if (c.get('origin') or '').startswith('task:') and c['status']=='open')")
  if [ -n "$kids" ]; then
    say "The board runs the subagents' tasks" "$WORKER_HARNESS on the gateway's $WORKER_MODEL takes each subagent task, one at a time, and files its report as the handoff."
    for k in $kids; do
      ( cd "$(mktemp -d)" && a work --harness "$WORKER_HARNESS" --auth gateway --model "$WORKER_MODEL" --backend namespaces --lease-seconds 900 --task "$k" 2>&1 | grep -E '^amem: (claimed|task)' ) || true
    done
    cont=$(a task tree "$parent" | j "next((c['id'] for c in d.get('children', []) if (c.get('origin') or '').startswith('continues:') and c['status']=='open'), '')")
    open=$(a task tree "$parent" | j "' '.join(c['id'] for c in d.get('children', []) if (c.get('origin') or '').startswith('task:') and c['status']!='done')")
    if [ -n "$cont" ] && [ -z "$open" ]; then
      say "Fan-in" "The continuation waited for both; it runs now, with their handoffs in its prompt, and completes with a handoff of its own. Its completion closes the original task it resumed, with the same handoff."
      ( cd "$(mktemp -d)" && a work --harness "$HARNESS" --auth "${AUTH:-subscription}" $MODEL_ARG --lease-seconds 900 --task "$cont" 2>&1 | grep -E '^amem: (claimed|task)' ) || true
    elif [ -n "$cont" ]; then
      echo "  the continuation $cont still waits for: $open"
    fi
    tree
  else
    echo "  (the session did not delegate this time; nothing went on the board)"
  fi
fi

say "The fabric's telemetry" "Tasks by family and status, why work was dropped, and which requirements no worker could meet."
a task stats | j "'\n'.join(f\"  {fam:<12} \" + ', '.join(f'{k} {v}' for k, v in s.items()) for fam, s in d['by_family'].items()) + f\"\n  unmatched requirements: {d['unmatched_requirements'] or 'none'}\""

say "An outage" "You declare an outage when nothing can check a read or a change: each one waits. A session is not stopped. It works from the T0 that it loaded before the outage, and it writes its handoff, which goes on a queue. When you end the outage, the queue is replayed through the ordinary write, so the gates still decide. This step runs on a scratch record of its own, so the queue and the counts of the demo record stay as they are."
outage=$(mktemp -d)
o() { "$memory" --root "$outage/root" "$@"; }
o init --owner human:demo:outage --domain dom-outage >/dev/null
o write --domain dom-outage --kind fact --title "The mirror list has three mirrors" --body "Two mirrors answer. The third mirror times out." >/dev/null
# what you do while the session of the agent is open: declare the outage, and end it
echo "\"$memory\" --root \"$outage/root\" outage declare --reason \"the host of the ledger moves\" > \"$outage/declared.json\"" > "$outage/declare.sh"
echo "\"$memory\" --root \"$outage/root\" outage end > \"$outage/ended.json\"" > "$outage/end.sh"
python3 "$here/scripts/demo/mcp.py" "$memory" "$outage/root" agent:worker steps "[{\"tool\": \"t0\", \"arguments\": {}}, {\"run\": \"sh $outage/declare.sh\"}, {\"tool\": \"outage_status\", \"arguments\": {}}, {\"tool\": \"t0\", \"arguments\": {}}, {\"tool\": \"search\", \"arguments\": {\"query\": \"mirror\"}}, {\"tool\": \"write\", \"arguments\": {\"entry_type\": \"handoff\", \"title\": \"Handoff: the mirror list\", \"body\": \"Two mirrors answered. Start with the third mirror.\"}}, {\"run\": \"sh $outage/end.sh\"}, {\"tool\": \"search\", \"arguments\": {\"query\": \"mirror\"}}]" > "$outage/steps.jsonl"
step() { sed -n "$1p" "$outage/steps.jsonl"; }
step 1 | j "f\"  agent:worker loaded T0: {len(d['result'].splitlines())} lines\" if d['ok'] else '  T0 was refused: ' + d['refused']"
j "f\"  you declared an outage: {d['reason']}\"" < "$outage/declared.json"
step 2 | j "f\"  outage_status, in the same session: an outage since {d['result']['outage']['since']}; T0 loaded for {', '.join(d['result']['t0_loaded']) or 'no domain'}; \" + ('no claim, so no lease to watch' if d['result']['confirmed_lease'] is None else f\"work stops at {d['result']['confirmed_lease']}\")"
step 3 | j "'  t0 again, the loaded copy: ' + next((l for l in d['result'].splitlines() if l.startswith('> Outage')), 'no outage line') if d['ok'] else '  t0 was refused: ' + d['refused']"
step 4 | j "'  search: ' + (d.get('refused') or 'answered, and it must wait')"
step 5 | j "'  a handoff: ' + (d.get('refused') or 'written, and it must wait')"
j "f\"  you ended the outage: {len(d['handoffs'])} handoff landed in the record ({', '.join(d['handoffs'])}); heartbeats applied {d['heartbeats_applied']}, refused {d['heartbeats_refused']}; failed {len(d['failed'])}\"" < "$outage/ended.json"
step 6 | j "'  the same session searches again: ' + (f\"{len(d['result'] if isinstance(d['result'], list) else d['result'].get('hits', []))} hits\" if d['ok'] else 'refused: ' + d['refused'])"
rm -rf "$outage"
