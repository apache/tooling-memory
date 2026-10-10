#!/bin/sh
# Act 2: curation and the trust tiers. A curation cycle on a checker model; a librarian
# run on a cheap model working what it filed, its rewrite landing curated; an agent
# proposing an entry for verified and a person approving it; and the gate on the tier.
. "$(dirname "$0")/lib.sh"
act "Act 2: curation, and the trust tiers"
gateway || { echo "skipped: the checker and the librarian run need the gateway (local/.virtual-key)"; exit 0; }

say "one curation cycle: survey filed, checker on a sample, contradictions hunted, canaries, telemetry, queue rescored"
a canary plant --domain dom-demo --count 2 >/dev/null 2>&1 || true
a curate --auth gateway --model "$CHECKER_MODEL" --sample 6 --contradictions --budget-tasks 8 \
  | j "f\"  proposals {d['proposals']}, filed {d['filed']}; checked {d['checked']}, summaries rejected {d['rejected']}, contradictions {d['contradictions']}; quarantine reviewed {d['reviewed']}; canaries tripped {d['canaries_tripped']}; alarms {d['alarms']}; rescored {d['rescored']}\n  the checker ({d['checker_model']}) made {d['calls']} calls, {d['tokens_in']} tokens in and {d['tokens_out']} out, under a budget of {d['budget_tokens']}\""

say "what it filed, as the rubric ranks it"
a task list --domain dom-demo --status admitted \
  | j "'\n'.join(f\"  {t['priority']:>3}  {t['id']}  {t['title']}\" for t in d if (t.get('origin') or '').startswith(('librarian:','checker:')))"

say "a librarian run: $WORKER_HARNESS on the gateway's $WORKER_MODEL claims the top curation task and does it"
top=$(a task list --domain dom-demo --status admitted | j "next(t['id'] for t in d if (t.get('origin') or '').startswith(('librarian:','checker:')))")
( cd "$(mktemp -d)" && a work --harness "$WORKER_HARNESS" --auth gateway --model "$WORKER_MODEL" --backend namespaces --lease-seconds 900 --task "$top" 2>&1 | grep -E '^memory: (claimed|task|prefetched)' ) || true
echo "  what the librarian wrote lands curated, because it replaced an entry while holding a librarian's task:"
a list --limit 50 | j "'\n'.join(f\"  {h['id']}  {h['trust_tier']}  {h['title']}\" for h in d if h['trust_tier']=='curated') or '  (none this run)'"

say "verified is proposed by one principal and approved by another, a person"
pick=$(a list --kind decision --limit 1 | j "d[0]['id']")
echo "  an agent proposes the decision $pick for verified:"
task=$(agent agent:librarian promote "{\"id\": \"$pick\", \"to\": \"verified\", \"why\": \"read against the decision record and its sources\"}" | j "d['task']['id']")
echo "  proposal $task filed, requiring a human; the entry is still $(a get "$pick" | sed -n 's/^trust_tier: //p')"
echo "  the agent cannot approve its own proposal: $(echo "{\"action\":\"promote\",\"principal\":{\"id\":\"agent:librarian\",\"class\":\"agent\"},\"domain\":\"dom-demo\",\"promote\":{\"phase\":\"approve\",\"to\":\"verified\",\"author\":\"x\",\"proposer\":\"agent:librarian\"}}" | a policy eval | j "d['reason']")"
a approve "$task" | j "f\"  the owner approves: {d['id']} is {d['tier']}\""
echo "  and the gate now guards it: an agent rewriting a verified entry is held, not written:"
agent agent:librarian write "{\"title\": \"A rewrite\", \"body\": \"Changed.\", \"entry_type\": \"decision\", \"supersedes\": \"$pick\"}" | sed 's/^/  /' || true

say "the librarians' own telemetry, computed from the record and the ledger, not written by them"
a librarian --telemetry | j "'\n'.join(f\"  {who}: {s['curations']} curations ({s['summary_only']} summary-only, {s['body_changed']} changed the body), {s['tasks_completed']} tasks, {s['writes']} writes\" for who, s in d['by_author'].items()) + f\"\n  alarms: {len(d['alarms'])}\""
