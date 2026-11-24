#!/bin/sh
# Act 2: curation and the trust tiers. A curation cycle on an auditor model; a curator
# run on a cheap model working what it filed, its rewrite landing curated; an agent
# proposing an entry for verified and a person approving it; and the gate on the tier.
# Then the duties that need no model: a demotion behind a tombstone, the convention
# checks, a correction, and the mode of the auditor. Steps 2.1 to 2.3 and the
# qualification in 2.8 need the gateway; the other steps run without it.
. "$(dirname "$0")/lib.sh"
act 2 "Curation, and the trust tiers" "Memory that is never tended rots. A curation cycle on an auditor model surveys the record and files what needs doing; a cheap worker model does the top task through the same tools any agent has, and what it writes lands curated. Then the one step only a person can take: approving an entry for verified. Then the duties of a curator and of the auditor: dead material demoted behind a tombstone, the convention checks, a correction, and the mode the auditor runs in." "the curation cycle, with $AUDITOR_MODEL on the gateway as its auditor; the curator, $WORKER_HARNESS on $WORKER_MODEL (agent:$WORKER_HARNESS), a real session doing the task; agent:curator, played by the demo script through the same tools, proposing an entry for verified; and you, the person who approves it."
if gateway; then

say "One curation cycle" "The survey looks at every entry and files a task for each one that needs work; the auditor model reads a sample of summaries against their bodies; contradictions are hunted, canaries checked, and the queue rescored. All of it under a token budget."
a canary plant --domain dom-demo --count 2 >/dev/null 2>&1 || true
a curate --auth gateway --model "$AUDITOR_MODEL" --sample 6 --contradictions --budget-tasks 8 \
  | j "f\"  proposals {d['proposals']}, filed {d['filed']}; checked {d['checked']}, summaries rejected {d['rejected']}, contradictions {d['contradictions']}; quarantine reviewed {d['reviewed']}; canaries tripped {d['canaries_tripped']}; alarms {d['alarms']}; rescored {d['rescored']}\n  the auditor ({d['auditor_model']}) made {d['calls']} calls, {d['tokens_in']} tokens in and {d['tokens_out']} out, under a budget of {d['budget_tokens']}\""

say "What it filed, as the rubric ranks it" "Curation does not edit the record itself: it files tasks on the same ledger every agent works from, ranked by the rubric."
a task list --domain dom-demo --status open \
  | j "'\n'.join(f\"  {t['priority']:>3}  {t['id']}  {t['title']}\" for t in d if (t.get('origin') or '').startswith(('curator:','auditor:')))"

say "A curator run" "$WORKER_HARNESS on the gateway's $WORKER_MODEL claims the top curation task and does it. An entry rewritten while holding a curator's task lands one tier up, curated, and never further on an agent's word."
top=$(a task list --domain dom-demo --status open | j "next((t['id'] for t in d if (t.get('origin') or '').startswith(('curator:','auditor:'))), '')")
if [ -n "$top" ]; then
  ( cd "$(mktemp -d)" && a work --harness "$WORKER_HARNESS" --auth gateway --model "$WORKER_MODEL" --backend namespaces --lease-seconds 900 --task "$top" 2>&1 | grep -E '^amem: (claimed|task|prefetched)' ) || true
else
  echo "  no curation task is admitted: the cycle filed none, or the domain does not admit its own filings (a record from an older demo; make demo-ready RESET=1)"
fi
echo "  what the curator wrote lands curated, because it replaced an entry while holding a curator's task:"
a list --limit 50 | j "'\n'.join(f\"  {h['id']}  {h['trust_tier']}  {h['title']}\" for h in d if h['trust_tier']=='curated') or '  (none this run)'"

else
  echo "skipped: steps 2.1 to 2.3, the cycle with its auditor and the curator run, need the gateway (local/.virtual-key)"
  # the steps keep their numbers: the next one is 2.4
  DEMO_STEP=3
fi

say "Verified: proposed by one, approved by another" "An agent may propose an entry for verified; only a person who did not propose it may approve. Once verified, an agent's rewrite of it is held, not written."
pick=$(a list --kind decision --limit 1 | j "d[0]['id']")
echo "  an agent proposes the decision $pick for verified:"
task=$(agent agent:curator promote "{\"id\": \"$pick\", \"to\": \"verified\", \"why\": \"read against the decision record and its sources\"}" | j "d['task']['id']")
echo "  proposal $task filed, requiring a human; the entry is still $(a get "$pick" | sed -n 's/^trust_tier: //p')"
echo "  the agent cannot approve its own proposal: $(echo "{\"action\":\"promote\",\"principal\":{\"id\":\"agent:curator\",\"class\":\"agent\"},\"domain\":\"dom-demo\",\"promote\":{\"phase\":\"approve\",\"to\":\"verified\",\"author\":\"x\",\"proposer\":\"agent:curator\"}}" | a policy eval | j "d['reason']")"
a approve "$task" | j "f\"  the owner approves: {d['id']} is {d['tier']}\""
echo "  and the gate now guards it: an agent rewriting a verified entry is held, not written:"
agent agent:curator write "{\"title\": \"A rewrite\", \"body\": \"Changed.\", \"entry_type\": \"decision\", \"supersedes\": \"$pick\"}" | sed 's/^/  /' || true

say "The curators' own telemetry" "How much each curator did, computed from the record and the ledger rather than reported by the curators, with alarms when a curator writes too much or changes bodies it was asked to summarise."
a curator --telemetry | j "'\n'.join(f\"  {who}: {s['curations']} curations ({s['summary_only']} summary-only, {s['body_changed']} changed the body), {s['tasks_completed']} tasks, {s['writes']} writes\" for who, s in d['by_author'].items()) + f\"\n  alarms: {len(d['alarms'])}\""

say "Dead material, demoted" "An entry that nothing reads and nothing names is dead. An agent demotes one only while it holds a task of a curator or of the auditor, and only after 90 days; a person demotes at once. A demotion deletes nothing: a tombstone stays, the archive keeps the text, and the log of the domain says who demoted it and why, as a code."
note=$(a write --kind fact --domain dom-demo --title "Where the old staging mirror was" --body "The staging mirror was at mirror-old.example.org until 2024." | j "d['id']")
echo "  the script writes a note that nothing reads and nothing names: $note"
a dead --domain dom-demo --days 0 | j "f\"  with a window of 0 days, {len(d['dead'])} entries are dead; the note is one of them: {any(x['id'] == '$note' for x in d['dead'])}\""
echo "  an agent with no task of a curator tries to demote it:"
agent agent:curator demote "{\"id\": \"$note\", \"reason\": \"dead\"}" | sed 's/^/    /' || true
a demote "$note" --reason dead | j "f\"  the owner demotes it: {d['id']}, reason {d['reason']}, by {d['by']}\""
echo "  the tombstone: get answers that the entry was demoted, and list does not hold it"
a get "$note" 2>&1 | sed 's/^/    /' || true
a list --domain dom-demo --kind fact --limit 500 | j "f\"    in the list of facts: {any(h['id'] == '$note' for h in d)}\""
echo "  the log of the domain names who and why:"
a log show --domain dom-demo | j "'\n'.join(f\"    {e['seq']:>6}  {e['kind']}  {e['id']}  by {e['by']}  reason {e['reason']}\" for e in d if e['kind'] == 'demoted' and e['id'] == '$note')"

say "Conventions and corrections" "The convention checks read each entry: a fact has a review date, a convention and a method name an owner, a link leads to an entry that the record serves, and no entry cites a replaced one. Each failure becomes a task that names the person who answers for the entry. Then a correction: a person records what became of the result of a task, and a result that was corrected goes to a curator with one question, whether the method or the knowledge was at fault."
conv=$(a write --kind convention --domain dom-demo --title "A title says what an entry claims" --body "The title of an entry says its claim in one sentence." | j "d['id']")
echo "  the owner writes a convention that names no owner: $conv"
a conventions --domain dom-demo --file \
  | j "f\"  the convention checks over dom-demo: failures {len(d['failures'])}, tasks filed now {len(d['filed'])}, filed before {d['already_filed']}\n\" + '\n'.join(f\"  {x['check']}: {x['why']} It goes to {x['owner']}.\" for x in d['failures'] if x['entry'] == '$conv')"
for st in proposed open; do
  a task list --domain dom-demo --status $st \
    | j "'\n'.join(f\"  the task for the owner: {t['id']}  {t['status']}, requires {', '.join(t['requirements'])}: {t['title']}\" for t in d if t.get('origin') == 'convention:ownership' and '$conv' in t['title'] + t['body'])" | sed '/^$/d'
done
small=$(a task create "Name the port of the staging mirror" --domain dom-demo --priority 60 --body "Small." | j "d['id']")
a task admit "$small" >/dev/null
claimed=$(a task claim-next --domain dom-demo --lease-seconds 600 | j "d['id'] + ' ' + str(d['generation'])")
if [ "${claimed% *}" = "$small" ]; then
  answer=$(a write --kind handoff --domain dom-demo --title "Handoff: the port of the staging mirror" --body "The port of the staging mirror is 8080." | j "d['id']")
  a task complete "$small" "$answer" --generation "${claimed#* }" >/dev/null
  echo "  a small task is filed, claimed and completed: $small, with the answer 8080"
  a task outcome "$small" --kind corrected --note "The port is 8443. 8080 was the port of the old mirror." | j "'\n'.join(f\"  a person records the outcome: {o['outcome']}, by {o['by']}: {o['note']}\" for o in d['outcomes'])"
  filed=$(a corrections --domain dom-demo --file | j "(d['filed'] or [''])[0]")
  if [ -n "$filed" ]; then
    a task get "$filed" | j "f\"  the correction is a task for a curator: {d['id']}  origin {d['origin']}  priority {d['priority']}\n  {d['title']}\n  its question: \" + next((l.strip() for l in d['body'].splitlines() if 'Could the agent' in l), '')"
  else
    echo "  the correction has a task from an earlier run"
  fi
else
  echo "  another task was claimed first (${claimed% *}); the correction is not shown in this run"
fi

say "The auditor's mode" "The auditor runs on a model that is not the model of the curators only when that model qualifies: each model judges ten planted cases, six of them faulty, and the candidate must catch at least as many faults and raise no more false alarms. Without a qualified model, the auditor is a fresh session of the curators' model, and a person reviews more of its verdicts."
if gateway; then
  a auditor qualify --auth gateway --curator-model "$WORKER_MODEL" --model "$AUDITOR_MODEL" \
    | j "'\n'.join(f\"  {who} {s['model']}: caught {s['caught']} of {s['caught'] + s['missed']} planted faults, {s['false_alarms']} false alarms\" for who, s in (('the curators\' model', d['curators']), ('the candidate', d['candidate'])))"
else
  echo "  skipped: the qualification needs the gateway (local/.virtual-key)"
fi
a auditor mode | j "f\"  mode {d['mode']}; qualified: {d['qualified']}; model of the auditor: {d['auditor_model'] or 'the curators\' model, in a fresh session'}; a person reviews {d['human_sample']} verdicts in each cycle\""
