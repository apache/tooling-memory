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
"librarian:" = -1
```
POL
)
a approve "$(agent agent:librarian promote "{\"id\": \"$strategy\", \"to\": \"verified\", \"why\": \"agreed for this period\"}" | j "d['task']['id']")" >/dev/null
echo "  pol-L1-007 is verified and in force"

say "A small queue" "A task blocked on another, and a small unrelated one. On admission the blocker rises above what it blocks: a move along the ledger's edge, made by the ledger, not by a worker."
blocker=$(a task create "Migrate the fixtures" --domain dom-demo --body "Needed before the release." | j "d['id']")
ranked=$(a task create "Cut the release" --domain dom-demo --priority 8 --after "$blocker" --body "Blocked on the fixtures." | j "d['id']")
plain=$(a task create "Tidy the README" --domain dom-demo --priority 1 --body "Small." | j "d['id']")
for t in "$blocker" "$ranked" "$plain"; do a task admit "$t" >/dev/null; done
echo "  admitted: the blocker rose above what it blocks, a move along the ledger's edge, not a worker's"
a task list --status admitted | j "'\n'.join(f\"  {t['priority']:>3}  {t['title']:<24} {'(blocks Cut the release)' if t['id'] == '$blocker' else ''}\" for t in (d if isinstance(d, list) else d.get('tasks', [])) if t['id'] in ('$blocker', '$ranked', '$plain'))"
say "The rescore" "Each cycle the queue is ranked again; every move is a ledger event with its reason, and curation's share of the work is held to its floor."
a task rescore | j "f\"  rubric {d['rubric']}; curation's share of the completed work {d['maintenance_share']:.0%} against a floor of {d['floor']:.0%}\n\" + '\n'.join(f\"  {c['from']:>3} -> {c['to']:<3} {c['title']:<40} {c['why']}\" for c in d['changed'])"

if [ -n "${HARNESS:-}" ]; then
  say "Subagents on the board" "A $HARNESS session is asked to delegate. Each subagent it would spawn is filed as a task of its own instead, with a continuation that waits for them, so the work survives the session and any worker can do it."
  parent=$(a task create "Compare two decisions" --domain dom-demo --priority 20 --body "Use two subagents, one per decision, to summarise DR-19 and DR-22 from the memory record; then combine their answers into a handoff and complete this task." | j "d['id']")
  a task admit "$parent" >/dev/null
  ( cd "$(mktemp -d)" && a work --harness "$HARNESS" --auth "${AUTH:-subscription}" --lease-seconds 900 --task "$parent" 2>&1 | grep -E '^memory: (claimed|task|board|filed)' ) || true
  tree() { a task tree "$parent" | j "'\n'.join(f\"  {c['id']}  {(c.get('origin') or '').split(':')[0]:<10} {c['status']:<11} {c['title']}\" for c in d.get('children', []))"; }
  tree
  kids=$(a task tree "$parent" | j "' '.join(c['id'] for c in d.get('children', []) if (c.get('origin') or '').startswith('task:') and c['status']=='admitted')")
  if [ -n "$kids" ]; then
    say "The board runs the subagents' tasks" "$WORKER_HARNESS on the gateway's $WORKER_MODEL takes each subagent task, one at a time, and files its report as the handoff."
    for k in $kids; do
      ( cd "$(mktemp -d)" && a work --harness "$WORKER_HARNESS" --auth gateway --model "$WORKER_MODEL" --backend namespaces --lease-seconds 900 --task "$k" 2>&1 | grep -E '^memory: (claimed|task)' ) || true
    done
    cont=$(a task tree "$parent" | j "next((c['id'] for c in d.get('children', []) if (c.get('origin') or '').startswith('continues:') and c['status']=='admitted'), '')")
    open=$(a task tree "$parent" | j "' '.join(c['id'] for c in d.get('children', []) if (c.get('origin') or '').startswith('task:') and c['status']!='completed')")
    if [ -n "$cont" ] && [ -z "$open" ]; then
      say "Fan-in" "The continuation waited for both; it runs now, with their handoffs in its prompt, and completes with a handoff of its own. The parent it resumes went back to the pool when its session ended, and stays there."
      ( cd "$(mktemp -d)" && a work --harness "$HARNESS" --auth "${AUTH:-subscription}" --lease-seconds 900 --task "$cont" 2>&1 | grep -E '^memory: (claimed|task)' ) || true
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
