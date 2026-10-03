#!/bin/sh
# The weekend demo, end to end, in a record of its own: seed a few hundred entries from
# Markdown, let a harness work a real task over them, then measure what the paper claims.
#
#   scripts/demo.sh <demo-root> <markdown-dir>... 
#
# Needs target/release/memory (make build) and a harness login; HARNESS and AUTH override
# the defaults (claude, subscription). Rerunning reuses the record and adds another task.
set -eu
root=${1:?demo root}; shift
[ $# -ge 1 ] || { echo "give one or more directories or files of Markdown to seed from" >&2; exit 2; }
here=$(cd "$(dirname "$0")/.." && pwd)
memory="$here/target/release/memory"
[ -x "$memory" ] || { echo "build first: make build" >&2; exit 2; }
if [ ! -f "$root/grants.toml" ]; then
  "$memory" --root "$root" init --owner "human:demo:$(id -un)" --domain dom-demo
  python3 "$here/scripts/seed-corpus.py" --root "$root" --memory "$memory" "$@"
fi
question=${QUERY:-what did DR-19 decide about MoinMoin}
show=$(cat <<'PY'
import json, sys
r = json.load(sys.stdin)
by = r["synthesis"]["by"]
head = f"synthesis {by}"
if by == "server":
    head += " by " + r["synthesis"]["model"]
pack = r["pack"]
print(f'{head}; {len(pack["pieces"])} pieces at {pack["depth"]} depth, {pack["tokens"]} tokens, {r["hops"]} hop(s)')
if r.get("brief"):
    print()
    print(r["brief"].strip())
    print()
print("cites: " + ", ".join(r["citations"]))
PY
)
show_recall() { python3 -c "$show"; }
echo
echo "== before: the record, T0 and retrieval, with no session yet"
"$memory" --root "$root" savings --query "$question" | sed -n '1,12p'
echo
echo "== recall with no model behind it: the pack comes back, synthesis owed to the caller"
"$memory" --root "$root" recall "$question" | show_recall || true
echo
title=${TASK_TITLE:-"Answer: what did DR-19 decide about MoinMoin, and why?"}
body=${TASK_BODY:-"Use the memory tools: search the record, open only the entries you need, and answer in a handoff entry: what was decided, the reasons given, and the entry ids you cite. Then complete this task with that handoff. Do not read files from the working tree; the record is the source."}
id=$("$memory" --root "$root" task create "$title" --body "$body" --priority 5 | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])')
"$memory" --root "$root" task admit "$id" >/dev/null
echo "== task $id filed and admitted; a ${HARNESS:-claude} session claims and works it"
work=$(mktemp -d)
( cd "$work" && "$memory" --root "$root" work --harness "${HARNESS:-claude}" --auth "${AUTH:-subscription}" --lease-seconds 300 ) || true
echo
if [ -n "${LLMAO_KEY:-}" ]; then
  echo "== recall through the gateway: the deputy reads, a cited brief comes back"
  "$memory" --root "$root" recall "$question" --provider llmao --auth gateway | show_recall || true
elif [ -n "${ANTHROPIC_API_KEY:-}" ]; then
  echo "== recall with a key: the deputy reads, a cited brief comes back"
  "$memory" --root "$root" recall "$question" --provider anthropic --auth key | show_recall || true
else
  echo "== no LLMAO_KEY or ANTHROPIC_API_KEY in the environment: server-executed recall skipped"
fi
echo
echo "== after: recovery, retrieval, the funnel and recall, with that session in the numbers"
"$memory" --root "$root" savings --query "$question"
echo
echo "The handoff the session wrote:"
h=$("$memory" --root "$root" task get "$id" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("handoff") or "")')
[ -n "$h" ] && "$memory" --root "$root" get "$h" | sed -n '/^---$/,$p' | tail -n +2 | head -40
