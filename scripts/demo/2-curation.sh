#!/bin/sh
# Act 2: curation and the trust tiers. A curation cycle on a checker model; a librarian
# run on a cheap model working what it filed, its rewrite landing curated; an agent
# proposing an entry for verified and a person approving it; and the gate on the tier.
. "$(dirname "$0")/lib.sh"
act 2 "Curation, and the trust tiers" "Memory that is never tended rots. A curation cycle on a checker model surveys the record and files what needs doing; a cheap worker model does the top task through the same tools any agent has, and what it writes lands curated. Then the one step only a person can take: approving an entry for verified." "the curation cycle, with $CHECKER_MODEL on the gateway as its checker; the librarian, $WORKER_HARNESS on $WORKER_MODEL (agent:$WORKER_HARNESS), a real session doing the task; agent:librarian, played by the demo script through the same tools, proposing an entry for verified; and you, the person who approves it."
gateway || { echo "skipped: the checker and the librarian run need the gateway (local/.virtual-key)"; exit 0; }

say "One curation cycle" "The survey looks at every entry and files a task for each one that needs work; the checker model reads a sample of summaries against their bodies; contradictions are hunted, canaries checked, and the queue rescored. All of it under a token budget."
a canary plant --domain dom-demo --count 2 >/dev/null 2>&1 || true
a curate --auth gateway --model "$CHECKER_MODEL" --sample 6 --contradictions --budget-tasks 8 \
  | j "f\"  proposals {d['proposals']}, filed {d['filed']}; checked {d['checked']}, summaries rejected {d['rejected']}, contradictions {d['contradictions']}; quarantine reviewed {d['reviewed']}; canaries tripped {d['canaries_tripped']}; alarms {d['alarms']}; rescored {d['rescored']}\n  the checker ({d['checker_model']}) made {d['calls']} calls, {d['tokens_in']} tokens in and {d['tokens_out']} out, under a budget of {d['budget_tokens']}\""

say "What it filed, as the rubric ranks it" "Curation does not edit the record itself: it files tasks on the same ledger every agent works from, ranked by the rubric."
a task list --domain dom-demo --status admitted \
  | j "'\n'.join(f\"  {t['priority']:>3}  {t['id']}  {t['title']}\" for t in d if (t.get('origin') or '').startswith(('librarian:','checker:')))"

say "A librarian run" "$WORKER_HARNESS on the gateway's $WORKER_MODEL claims the top curation task and does it. An entry rewritten while holding a librarian's task lands one tier up, curated, and never further on an agent's word."
top=$(a task list --domain dom-demo --status admitted | j "next((t['id'] for t in d if (t.get('origin') or '').startswith(('librarian:','checker:'))), '')")
if [ -n "$top" ]; then
  ( cd "$(mktemp -d)" && a work --harness "$WORKER_HARNESS" --auth gateway --model "$WORKER_MODEL" --backend namespaces --lease-seconds 900 --task "$top" 2>&1 | grep -E '^memory: (claimed|task|prefetched)' ) || true
else
  echo "  no curation task is admitted: the cycle filed none, or the domain does not admit its own filings (a record from an older demo; make demo-ready RESET=1)"
fi
echo "  what the librarian wrote lands curated, because it replaced an entry while holding a librarian's task:"
a list --limit 50 | j "'\n'.join(f\"  {h['id']}  {h['trust_tier']}  {h['title']}\" for h in d if h['trust_tier']=='curated') or '  (none this run)'"

say "Verified: proposed by one, approved by another" "An agent may propose an entry for verified; only a person who did not propose it may approve. Once verified, an agent's rewrite of it is held, not written."
pick=$(a list --kind decision --limit 1 | j "d[0]['id']")
echo "  an agent proposes the decision $pick for verified:"
task=$(agent agent:librarian promote "{\"id\": \"$pick\", \"to\": \"verified\", \"why\": \"read against the decision record and its sources\"}" | j "d['task']['id']")
echo "  proposal $task filed, requiring a human; the entry is still $(a get "$pick" | sed -n 's/^trust_tier: //p')"
echo "  the agent cannot approve its own proposal: $(echo "{\"action\":\"promote\",\"principal\":{\"id\":\"agent:librarian\",\"class\":\"agent\"},\"domain\":\"dom-demo\",\"promote\":{\"phase\":\"approve\",\"to\":\"verified\",\"author\":\"x\",\"proposer\":\"agent:librarian\"}}" | a policy eval | j "d['reason']")"
a approve "$task" | j "f\"  the owner approves: {d['id']} is {d['tier']}\""
echo "  and the gate now guards it: an agent rewriting a verified entry is held, not written:"
agent agent:librarian write "{\"title\": \"A rewrite\", \"body\": \"Changed.\", \"entry_type\": \"decision\", \"supersedes\": \"$pick\"}" | sed 's/^/  /' || true

say "The librarians' own telemetry" "How much each curator did, computed from the record and the ledger rather than reported by the curators, with alarms when a curator writes too much or changes bodies it was asked to summarise."
a librarian --telemetry | j "'\n'.join(f\"  {who}: {s['curations']} curations ({s['summary_only']} summary-only, {s['body_changed']} changed the body), {s['tasks_completed']} tasks, {s['writes']} writes\" for who, s in d['by_author'].items()) + f\"\n  alarms: {len(d['alarms'])}\""
