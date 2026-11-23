#!/bin/sh
# Act 7: the platform. The fleet's configuration is memory too: agent definitions, eval
# suites and harness defaults are entries in a platform domain, and the launcher loads
# one only once a person has verified it, so an agent cannot write the instructions the
# next agent runs under. Then a task worked under the verified definition, the suite
# scored, and what the run cost in every resource the host can count.
. "$(dirname "$0")/lib.sh"
act 7 "The platform" "The fleet's own configuration held as memory, under the same trust tiers as what it knows: agent definitions, eval suites and harness defaults in a platform domain, loaded only once a person verifies them. A task worked under a verified definition, the suite scored, and the run's cost in tokens, time, CPU and memory." "agent:architect, played by the demo script, proposing a definition, a suite and a default model; you, who verify them; $WORKER_HARNESS on the model the platform names, working a task under the definition; agent:worker, agent:evaluator and agent:reviewer, played by the demo script, for the autonomy level; and the launcher and the egress, measuring the run."

say "A platform domain" "The grants name one domain the platform's. Its entries carry artifacts, fenced blocks the launcher reads by kind and name."
if ! grep -q '^platform = ' "$root/grants.toml"; then
  # a top-level key, before the first table
  sed -i 's/^handling = \(.*\)$/handling = \1\nplatform = "dom-platform"/' "$root/grants.toml"
  printf '\n[domains.dom-platform]\ndefault_classification = "internal"\nadmit_own = true\n' >> "$root/grants.toml"
fi
echo "  grants.toml: platform = \"dom-platform\", and [domains.dom-platform]"

say "An agent proposes a definition" "agent:architect writes an agent definition for $WORKER_HARNESS, a recall suite, and the model $WORKER_HARNESS should run on. Written by an agent, they are raw, and raw does not load."
definition=$(agent agent:architect write "$(python3 - "$WORKER_HARNESS" <<'PY'
import json, sys
h = sys.argv[1]
print(json.dumps({"title": f"How a {h} worker answers a question", "domain": "dom-platform",
                  "entry_type": "method",
                  "body": f"```platform agent {h}\nSearch the record for the decision your task names and read it before you answer. End your handoff with this line, exactly: Checked against the platform definition.\n```\n"}))
PY
)" | j "d['id']")
suite=$(agent agent:architect write "$(python3 - "$WORKER_HARNESS" "$WORKER_MODEL" "$here/golden/platform.toml" <<'PY'
import json, sys
h, model = sys.argv[1], sys.argv[2]
suite = open(sys.argv[3]).read()
print(json.dumps({"title": "The recall suite, and the worker's model", "domain": "dom-platform",
                  "entry_type": "method",
                  "body": f"```platform eval recall\n{suite}```\n\n```platform harness {h}\n{{\"model\": \"{model}\"}}\n```\n"}))
PY
)" | j "d['id']")
listing() { a platform list | j "'\n'.join(f\"  {x['kind']:<8} {x['name']:<8} {x['trust_tier']:<9} {'loaded' if x['loaded'] else 'not loaded: ' + (x.get('why_not') or '')}\" for x in d)"; }
listing

say "Verified, it loads" "The agent proposes both for verified; you approve, as in 2.4. Now they load, and an agent's rewrite of the definition is held, not written."
for e in "$definition" "$suite"; do
  t=$(agent agent:architect promote "{\"id\": \"$e\", \"to\": \"verified\", \"why\": \"the definition and defaults the workers should run under\"}" | j "d['task']['id']")
  a approve "$t" >/dev/null
done
listing
echo "  an agent rewriting the verified definition:"
agent agent:architect write "{\"title\": \"A looser definition\", \"domain\": \"dom-platform\", \"entry_type\": \"method\", \"supersedes\": \"$definition\", \"body\": \"Answer however you like.\"}" | sed 's/^/  /' || true

if gateway; then
  say "A task under the definition" "A question filed for $WORKER_HARNESS, launched with no model named: the platform's default model is used, and the verified definition is added to the task's prompt."
  task=$(a task create "What did DR-22 decide about the format memory entries travel in?" --domain dom-demo --priority 30 --body "Answer from the memory record, in two or three sentences, and file the answer as your handoff." | j "d['id']")
  a task admit "$task" >/dev/null
  ( cd "$(mktemp -d)" && a work --harness "$WORKER_HARNESS" --auth gateway --model "$WORKER_MODEL" --backend namespaces --lease-seconds 900 --task "$task" 2>&1 | grep -E '^amem: (claimed|task|.*confined)' ) || true
  handoff=$(a task get "$task" | j "d.get('handoff') or ''")
  if [ -n "$handoff" ]; then
    a get "$handoff" 2>/dev/null | grep -q "Checked against the platform definition" \
      && echo "  its handoff ends as the definition asks: \"Checked against the platform definition.\"" \
      || echo "  its handoff does not end with the definition's line: the model did not follow it this time"
  fi
fi

say "A task definition" "A kind of work is a record in the platform domain too: what it is for, the tools it may use, and the checks that say it is done. A task names its definition, takes its checks and requires its tools. A definition an agent proposed is not in force until a person verifies it, and a task cannot name one that is not in force."
tdef=$(agent agent:architect write "$(python3 - <<'PY'
import json
block = "```platform task answer-from-record\npurpose = \"Answer a question from the record, with the entries it rests on.\"\ntools = []\nscope = [\"dom-demo\"]\nacceptance = [\"The handoff names each entry the answer rests on.\"]\noutputs = [\"handoff\"]\nautonomy = \"review\"\n```\n"
print(json.dumps({"title": "How a question is answered from the record", "domain": "dom-platform",
                  "entry_type": "convention", "body": "# Answer from the record\n\n" + block}))
PY
)" | j "d['id']")
echo "  proposed by agent:architect, not verified; a task that names it:"
a task create "Which decision set the lease length?" --domain dom-demo --definition answer-from-record 2>&1 | sed 's/^/    /' || true
t=$(agent agent:architect promote "{\"id\": \"$tdef\", \"to\": \"verified\", \"why\": \"the checks say what an answer must show\"}" | j "d['task']['id']")
a approve "$t" >/dev/null
echo "  verified by you; the same task now:"
defined=$(a task create "Which decision set the lease length?" --domain dom-demo --definition answer-from-record)
echo "$defined" | j "f\"    {d['id']} runs under {d['definition']}; done when: {d['acceptance'][0]}\""
a task invalidate "$(echo "$defined" | j "d['id']")" --reason "obsolete: shown" >/dev/null

say "An autonomy level" "A version of a task definition starts at the level person: work that is handed in under it waits, blocked, until a person gives the verdict. The line autonomy = \"review\" in the definition is the highest level that this version can earn, not the level that it has."
if ! grep -q '^held_out = ' "$root/grants.toml"; then
  # two top-level keys, before the first table
  sed -i 's/^platform = \(.*\)$/platform = \1\nheld_out = "dom-held-out"\nevaluator = "agent:evaluator"/' "$root/grants.toml"
  printf '\n[domains.dom-held-out]\ndefault_classification = "internal"\nadmit_own = true\n' >> "$root/grants.toml"
fi
echo "  grants.toml: held_out = \"dom-held-out\", evaluator = \"agent:evaluator\", and [domains.dom-held-out]"
level() { a platform level answer-from-record | j "f\"  answer-from-record is at the level {d['level']}; its text lets it earn {d['ceiling']}\""; }
# a task under the definition, worked by agent:worker over the memory server: sets $worked and $handed
work_one() {
  worked=$(a task create "$1" --domain dom-demo --definition answer-from-record --body "Answer from the record and name the decision." | j "d['id']")
  a task admit "$worked" >/dev/null 2>&1 || true
  gen=$(agent agent:worker claim "{\"id\": \"$worked\"}" | j "d['generation']")
  handed=$(agent agent:worker write "$(python3 -c 'import json, sys; print(json.dumps({"title": "Answer from the record", "domain": "dom-demo", "entry_type": "handoff", "body": sys.argv[1] + "\n"}))' "$2")" | j "d['id']")
  agent agent:worker complete "{\"id\": \"$worked\", \"generation\": $gen, \"handoff\": \"$handed\"}" | j "f\"  agent:worker completes {d['task']['id']}: it is {d['task']['status']}, waiting on {d['task']['waiting_on']}\""
}
work_one "Which decision set the format that entries travel in?" "DR-22 set the format that entries travel in: Markdown with frontmatter, in a bundle."
first=$worked; first_handoff=$handed
level
a task verdict "$first" --accept | j "f\"  you accept the work: {d['task']['id']} is {d['task']['status']}, at the level {d['level']}\""

say "A held-out suite" "A definition earns the next level on a suite that the worker cannot read. The suite is in a domain of its own, and only agent:evaluator and you read it. The evaluator grades handoffs against the suite and takes the step to review. Then another agent gives the verdict, and never the agent that did the work. Work that does not stand costs the level again."
held=$(agent agent:evaluator write "$(python3 - <<'PY'
import json
block = "```platform suite answer-from-record\n[pass]\nperson = 1\nreview = 1\n\n[[scenario]]\nname = \"entry format\"\nask = \"Which decision set the format that entries travel in?\"\nexpect = [\"DR-22\"]\n```\n"
print(json.dumps({"title": "The held-out suite for answers from the record", "domain": "dom-held-out",
                  "entry_type": "convention", "body": "# The held-out suite for answers from the record\n\n" + block}))
PY
)" | j "d['id']")
t=$(agent agent:evaluator promote "{\"id\": \"$held\", \"to\": \"verified\", \"why\": \"the scenario has one answer in the record\"}" | j "d['task']['id']")
a approve "$t" >/dev/null
echo "  agent:evaluator wrote the suite $held, and you verified it; agent:worker asks for it:"
agent agent:worker get "{\"id\": \"$held\"}" 2>&1 | sed 's/^/    /' || true
agent agent:evaluator grade "{\"definition\": \"answer-from-record\", \"level\": \"review\", \"answers\": [{\"scenario\": \"entry format\", \"handoff\": \"$first_handoff\"}]}" | j "f\"  agent:evaluator grades the handoff of $first for the level review: {d['record']['passed']} of {d['record']['of']} scenarios, {d['record']['needed']} needed, pass: {d['pass']}\""
agent agent:evaluator raise_autonomy '{"definition": "answer-from-record"}' | j "f\"  agent:evaluator raises the level to {d['to']}, on the run {d['run']}\""
level
work_one "Which decision put the ledger in a database?" "DR-22 does not say. The record holds no decision that names a database for the ledger."
echo "  agent:worker gives the verdict on its own work:"
agent agent:worker verdict "{\"id\": \"$worked\", \"accept\": true}" 2>&1 | sed 's/^/    /' || true
agent agent:reviewer verdict "{\"id\": \"$worked\", \"accept\": true}" | j "f\"  agent:reviewer accepts it: {d['task']['id']} is {d['task']['status']}, at the level {d['level']}\""
a task outcome "$first" reverted | j "f\"  the work of {d['task']} is reverted later, and you record it: the level goes to {d['to']}\""
level

say "A candidate that adds a tool" "A change to a definition that is in force reaches every task of that kind. A candidate that an agent wrote passes a safety gate before a person sees it: one that adds a tool, a domain or a higher level is refused, and a person writes such a change. The performance gate asks for a passing run on the held-out suite. An agent that asks again and again is counted, and the telemetry raises an alarm for you."
wider=$(agent agent:architect write "$(python3 - <<'PY'
import json
block = "```platform task answer-from-record\npurpose = \"Answer a question from the record, with the entries it rests on.\"\ntools = [\"tool:fetch\"]\nscope = [\"dom-demo\"]\nacceptance = [\"The handoff names each entry the answer rests on.\"]\noutputs = [\"handoff\"]\nautonomy = \"review\"\n```\n"
print(json.dumps({"title": "How a question is answered, with the web", "domain": "dom-platform",
                  "entry_type": "convention", "body": "# Answer from the record, and from the web\n\n" + block}))
PY
)" | j "d['id']")
ask_wider() { agent agent:architect promote "{\"id\": \"$wider\", \"to\": \"verified\", \"why\": \"an answer is better with the web\"}" 2>&1 || true; }
echo "  agent:architect proposes a version that adds tool:fetch, for verified:"
ask_wider | sed 's/^/    /'
a platform gates "$wider" | j "'\n'.join(f\"  {g:<12}{'needed' if d[g]['needed'] else 'not needed':<11} {'met' if d[g]['met'] else 'not met':<8} {d[g]['why']}\" for g in ('performance', 'safety', 'review'))"
# nine requests in all: five of them are in one clock hour, whenever the act runs
for n in 2 3 4 5 6 7 8 9; do ask_wider >/dev/null; done
echo "  agent:architect asked nine times in all; the curator's telemetry:"
a curator --telemetry --domain dom-demo | j "'\n'.join(f\"    {x['kind']}: {x['why']}\" for x in d['alarms'] if x['kind'] == 'probing') or '    no probing alarm'"

say "The suite" "The recall suite, loaded from the platform rather than a file: each question asked as written, stripped, and by its keywords, scored through the pack with no model."
a golden --platform recall 2>/dev/null | sed 's/^/  /'

say "What it cost" "Tokens are one resource. The launcher keeps each run's wall clock, the CPU time and peak memory of the process tree it confined, and its own CPU, the egress among it; the egress times each call. Here the task's share, and its session's."
if [ -n "${task:-}" ]; then
  a cost --json | j "next((f\"  {t['task']}: {t['wall_ms']/1000:.0f} s of wall clock, {t['cpu_ms']/1000:.1f} s of CPU  {t['title']}\" for t in d['tasks'] if t['task'] == '$task'), '  (the task did not complete)')"
  a cost --json | j "next((f\"  its session {s['session']}: {s['runs']} run, {s['wall_ms']/1000:.0f} s wall, {s['cpu_ms']/1000:.1f} s CPU in the sandbox, {s['launcher_cpu_ms']/1000:.1f} s in the launcher, peak {s['max_rss_kib']//1024} MiB; {s['calls']} model calls on \" + ', '.join(f'{m} ({n} tokens)' for m, n in {**s['unpriced'], **s['plan']}.items()) + f\", {s['call_ms']/1000:.0f} s waiting on them\" for s in d['sessions'] if '$task' in s['tasks']), '')"
  [ -s "$root/prices.toml" ] && grep -q '^\[' "$root/prices.toml" || echo "  nothing is priced: prices.toml under the root is empty, so these are quantities, not money"
fi
a cost --json | j "(lambda r: f\"  every run in this record so far: {len(r)} runs, {sum(s['wall_ms'] for s in r)/60000:.0f} min of wall clock, {sum(s['cpu_ms'] + s['launcher_cpu_ms'] for s in r)/1000:.0f} s of CPU, at most {max(s['max_rss_kib'] for s in r)//1024} MiB in one process; {sum(s['call_ms'] for s in r)/60000:.0f} min of it waiting on model calls\" if r else '  no run is in this record so far')([s for s in d['sessions'] if s['runs']])"
