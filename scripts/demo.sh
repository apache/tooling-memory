#!/bin/sh
# The weekend demo, end to end, in a record of its own: seed a few hundred entries from
# Markdown, let a harness work a real task over them, then measure what the paper claims.
#
#   scripts/demo.sh <demo-root> <markdown-dir>... 
#
# Needs target/release/amem (make build) and a harness login. HARNESS names the harness,
# with no default, as everywhere else; AUTH defaults to subscription; MODEL names the
# model for the session, which a gateway run needs and a subscription run must not have;
# RECALL_MODEL pins
# the deputy's model on the gateway, which otherwise takes the smallest one the gateway
# lists. Rerunning reuses the record and adds another task;
# the entry the erasure step writes and erases is new each run, so the corpus is kept.
set -eu
root=${1:?demo root}; shift
harness=${HARNESS:?name the harness: HARNESS=claude, codex, gemini, antigravity, qwen, grok, muse, pi, copilot or opencode}
[ $# -ge 1 ] || [ -f "$root/grants.toml" ] || { echo "give one or more directories or files of Markdown to seed from" >&2; exit 2; }
here=$(cd "$(dirname "$0")/.." && pwd)
memory="$here/target/release/amem"
. "$here/scripts/demo/show.sh"
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
act 1 "The read path" "A few hundred entries of real decisions and docs in a record of their own. A harness session works one real task through the memory tools, starting from T0 and leaving a handoff, and the paper's read-path claims are measured before and after." "you, the owner of the record (human:demo:$(id -un)), who files the task and erases a note; $harness, a real harness session on your own login, confined in the harness image (agent:$harness); the memory server, which serves its tools and keeps its transcript; the egress, the only way out, which meters its model calls; and in 1.5 a small model on the Foundation's gateway, the server's recall deputy."
say "Before: the record, T0 and retrieval" "What a fresh session would load, T0, against the whole corpus, and how well search finds known items, before any session has run."
"$memory" --root "$root" savings --query "$question" | sed -n '1,12p'
say "Recall with no model behind it" "With no model of its own, the server answers a question with a pack of the best sections, cited, and leaves the synthesis to the caller."
"$memory" --root "$root" recall "$question" | show_recall || true
echo
title=${TASK_TITLE:-"Answer: what did DR-19 decide about MoinMoin, and why?"}
body=${TASK_BODY:-"Use the memory tools: search the record, open only the entries you need, and answer in a handoff entry: what was decided, the reasons given, and the entry ids you cite. Then complete this task with that handoff. Do not read files from the working tree; the record is the source."}
id=$("$memory" --root "$root" task create "$title" --body "$body" --priority 5 | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])')
"$memory" --root "$root" task admit "$id" >/dev/null
say "A $harness session works a task" "Task $id is filed and admitted on the ledger. A $harness session, confined, with memory as its only source, claims it, searches, opens what it needs, writes a cited handoff and completes the task."
work=$(mktemp -d)
# A gateway run has to name the model, and pi and OpenCode pick from their providers'
# lists rather than taking a default; a subscription run must not have one forced on it.
[ "${AUTH:-subscription}" = gateway ] && MODEL=${MODEL:-qwen3.8-27b}
( cd "$work" && "$memory" --root "$root" work --harness "$harness" --auth "${AUTH:-subscription}" ${MODEL:+--model "$MODEL"} --lease-seconds 300 --task "$id" ) || true
say "Erasure" "An entry written in error is erased by the owner: the body leaves every version, the index and what sessions were sent; a stub and a receipt remain, and search no longer finds it."
gone=$(echo "A note that should never have been written: the demo passphrase is kestrel-91." \
  | "$memory" --root "$root" write --title "Written in error" | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])')
"$memory" --root "$root" erase "$gone" --reason mistaken | python3 "$here/scripts/demo/receipt.py"
echo "  search for the passphrase after the erasure: $("$memory" --root "$root" search "kestrel-91")"
if [ -n "${LLMAO_KEY:-}" ]; then
  say "Recall through the gateway" "Now with a small model of the server's own on the Foundation's gateway: it reads the pack and a cited brief comes back."
  "$memory" --root "$root" recall "$question" --provider llmao --auth gateway ${RECALL_MODEL:+--model "$RECALL_MODEL"} | show_recall || true
elif [ -n "${ANTHROPIC_API_KEY:-}" ]; then
  say "Recall with a key" "Now with a small model of the server's own: it reads the pack and a cited brief comes back."
  "$memory" --root "$root" recall "$question" --provider anthropic --auth key | show_recall || true
else
  say "Recall by the server, skipped" "No LLMAO_KEY or ANTHROPIC_API_KEY in the environment, so the server has no model of its own."
fi
say "After: the numbers again" "Recovery, retrieval, the funnel and recall, now with the session in them: what memory served it against what its model calls ingested."
"$memory" --root "$root" savings --query "$question"
echo
say "The handoff the session wrote" "What the next session starts from, instead of replaying this one's transcript."
h=$("$memory" --root "$root" task get "$id" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("handoff") or "")')
if [ -n "$h" ]; then
  "$memory" --root "$root" get "$h" | sed -n '/^---$/,$p' | tail -n +2 | head -40
else
  echo "  (the session did not complete the task; its output is on the task as a handoff)"
fi
