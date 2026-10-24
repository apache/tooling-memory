#!/bin/sh
# Act 7: the platform. The fleet's configuration is memory too: agent definitions, eval
# suites and harness defaults are entries in a platform domain, and the launcher loads
# one only once a person has verified it, so an agent cannot write the instructions the
# next agent runs under. Then a task worked under the verified definition, the suite
# scored, and what the run cost in every resource the host can count.
. "$(dirname "$0")/lib.sh"
act 7 "The platform" "The fleet's own configuration held as memory, under the same trust tiers as what it knows: agent definitions, eval suites and harness defaults in a platform domain, loaded only once a person verifies them. A task worked under a verified definition, the suite scored, and the run's cost in tokens, time, CPU and memory." "agent:architect, played by the demo script, proposing a definition, a suite and a default model; you, who verify them; $WORKER_HARNESS on the model the platform names, working a task under the definition; and the launcher and the egress, measuring the run."

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
suite=$(agent agent:architect write "$(python3 - "$WORKER_HARNESS" "$WORKER_MODEL" <<'PY'
import json, sys
h, model = sys.argv[1], sys.argv[2]
suite = '''domain = "dom-demo"

[[question]]
ask = "What did DR-19 decide about the wiki staying on MoinMoin?"
answers = ["DR-19"]
keywords = "DR-19 MoinMoin wiki"

[[question]]
ask = "Why is the domain the partition key everywhere?"
answers = ["the domain is the partition key"]
keywords = "domain partition key"

[[question]]
ask = "Who is the promotion gate at a foundation?"
answers = ["Trust, review"]
keywords = "committers promotion gate"
'''
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
  ( cd "$(mktemp -d)" && a work --harness "$WORKER_HARNESS" --auth gateway --model "$WORKER_MODEL" --backend namespaces --lease-seconds 900 --task "$task" 2>&1 | grep -E '^memory: (claimed|task|.*confined)' ) || true
  handoff=$(a task get "$task" | j "d.get('handoff') or ''")
  if [ -n "$handoff" ]; then
    a get "$handoff" 2>/dev/null | grep -q "Checked against the platform definition" \
      && echo "  its handoff ends as the definition asks: \"Checked against the platform definition.\"" \
      || echo "  its handoff does not end with the definition's line: the model did not follow it this time"
  fi
fi

say "The suite" "The recall suite, loaded from the platform rather than a file: each question asked as written, stripped, and by its keywords, scored through the pack with no model."
a golden --platform recall 2>/dev/null | sed 's/^/  /'

say "What it cost" "Tokens are one resource. The launcher keeps each run's wall clock, the CPU time and peak memory of the process tree it confined, and its own CPU, the egress among it; the egress times each call. Here the task's share, and its session's."
if [ -n "${task:-}" ]; then
  a cost --json | j "next((f\"  {t['task']}: {t['wall_ms']/1000:.0f} s of wall clock, {t['cpu_ms']/1000:.1f} s of CPU  {t['title']}\" for t in d['tasks'] if t['task'] == '$task'), '  (the task did not complete)')"
  a cost --json | j "next((f\"  its session {s['session']}: {s['runs']} run, {s['wall_ms']/1000:.0f} s wall, {s['cpu_ms']/1000:.1f} s CPU in the sandbox, {s['launcher_cpu_ms']/1000:.1f} s in the launcher, peak {s['max_rss_kib']//1024} MiB; {s['calls']} model calls on \" + ', '.join(f'{m} ({n} tokens)' for m, n in {**s['unpriced'], **s['plan']}.items()) + f\", {s['call_ms']/1000:.0f} s waiting on them\" for s in d['sessions'] if '$task' in s['tasks']), '')"
  [ -s "$root/prices.toml" ] && grep -q '^\[' "$root/prices.toml" || echo "  nothing is priced: prices.toml under the root is empty, so these are quantities, not money"
fi
a cost --json | j "(lambda r: f\"  every run in this record so far: {len(r)} runs, {sum(s['wall_ms'] for s in r)/60000:.0f} min of wall clock, {sum(s['cpu_ms'] + s['launcher_cpu_ms'] for s in r)/1000:.0f} s of CPU, at most {max(s['max_rss_kib'] for s in r)//1024} MiB in one process; {sum(s['call_ms'] for s in r)/60000:.0f} min of it waiting on model calls\")([s for s in d['sessions'] if s['runs']])"
