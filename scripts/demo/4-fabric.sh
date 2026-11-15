#!/bin/sh
# Act 4: the task fabric. A strategy compiles into the rubric; a rescore ranks the
# queue along its edges and keeps curation's floor; a harness that would spawn
# subagents has them filed on the board instead; and the flow is counted.
. "$(dirname "$0")/lib.sh"
act 4 "The task fabric" "Work is a shared ledger of tasks with leases and handoffs. A strategy compiles into the rubric that ranks it, the ranking moves along dependencies, and a harness that would spawn subagents files them on the board instead, where any worker can take them." "you, who file the tasks and verify the strategy; the ledger, which ranks and moves them, not a worker; ${HARNESS:-a harness} on your login (agent:${HARNESS:-...}), the session asked to delegate and later the continuation; and $WORKER_HARNESS on $WORKER_MODEL, the board's worker for each subagent task."

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
