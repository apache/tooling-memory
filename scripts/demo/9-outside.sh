#!/bin/sh
# Act 9: from outside. Work arrives from trackers as well as from people: a tap is a
# connector run as a WASI component with exactly the access its grant allows, and what
# it brings is proposed, never admitted on its own say.
. "$(dirname "$0")/lib.sh"
act 9 "From outside" "Open issues of a real Apache repository arrive as tasks through a tap: a WebAssembly component that may reach the one host its grant names and nothing else. They arrive proposed, for a person to admit, and arriving twice files nothing twice." "the GitHub issues tap, run by the launcher under its grant; GitHub's public API, read without a credential; and you, who admit what is worth doing."
repo=${TAP_REPO:-apache/tooling-trusted-releases}
wasm="$here/taps/target/wasm32-wasip2/release/tap_github_issues.wasm"
[ -f "$wasm" ] || cargo build -q --manifest-path "$here/taps/Cargo.toml" -p tap-github-issues \
  --target wasm32-wasip2 --release 2>/dev/null || true
if [ ! -f "$wasm" ]; then
  echo "  the tap is not built, and could not be: make taps-build"
  exit 0
fi

say "The grant" "The tap may reach api.github.com and nothing else, files into dom-demo, and holds no credential: one would be put on its requests by the host, and here there is none, so it reads what anyone may."
a tap install github-issues "$wasm" >/dev/null
grep -q '^\[taps.github-issues\]' "$root/grants.toml" || cat >> "$root/grants.toml" <<'GRANT'

[taps.github-issues]
domain = "dom-demo"
hosts = ["api.github.com"]
ceiling = "public"
credential = "GH_TOKEN"
GRANT
echo "  grants.toml: [taps.github-issues] domain dom-demo, hosts api.github.com, ceiling public"

say "Issues become tasks" "The open issues of $repo, each a task citing its issue as origin; pull requests are skipped."
GH_TOKEN= a tap run github-issues --config "{\"repo\": \"$repo\"}" 2>/dev/null \
  | j "f\"  filed {d.get('filed', 0)}, already there {d.get('deduplicated', 0)}, refused {len(d.get('refused', []))}: {d.get('note', '')}\""
a task list --domain dom-demo --status proposed | j "'\n'.join(f\"  {t['id']}  {t['status']:<9} {t['title'][:70]}\" for t in (d if isinstance(d, list) else d.get('tasks', [])) if (t.get('origin') or '').startswith('github:'))" | head -5
echo "  ..."

say "Again" "The same run again files nothing: the ledger knows each issue by its origin and last update."
GH_TOKEN= a tap run github-issues --config "{\"repo\": \"$repo\"}" 2>/dev/null \
  | j "f\"  filed {d.get('filed', 0)}, already there {d.get('deduplicated', 0)}\""

say "A person admits" "What a tap brings is proposed: no worker claims it until someone admits it. You admit one."
first=$(a task list --domain dom-demo --status proposed | j "next((t['id'] for t in (d if isinstance(d, list) else d.get('tasks', [])) if (t.get('origin') or '').startswith('github:')), '')")
if [ -n "$first" ]; then
  a task admit "$first" | j "f\"  {d['id']} is {d['status']}: {d['title'][:70]}\""
fi

say "Raw reach" "An issue is text from a stranger. agent:a works the admitted task and leaves a note. Until someone curates the note, it reaches the task chain and you, and no other agent."
if [ -z "$first" ]; then
  echo "  skipped: the tap brought no task"
  exit 0
fi
marker="harbour$(date +%s)"
hits() { j "f\"  $1: {len(d['hits'])} hit(s)\" + ''.join(f\", {h['id']} at {h['trust_tier']}, reach {h.get('reach', 'fleet')}\" for h in d['hits'])"; }
note=$(agent agent:a batch "[{\"tool\": \"get_task\", \"arguments\": {\"id\": \"$first\"}}, {\"tool\": \"write\", \"arguments\": {\"entry_type\": \"decision\", \"title\": \"What the issue asks, $marker\", \"body\": \"The issue asks for one change. Marker: $marker.\"}}]" "$first" \
  | j "d['id'] + ' ' + d.get('reach', 'fleet') + ' ' + d.get('task', '-')")
echo "  agent:a wrote ${note%% *}: reach $(echo "$note" | cut -d' ' -f2), task $(echo "$note" | cut -d' ' -f3)"
note=${note%% *}
agent agent:a search "{\"query\": \"$marker\"}" "$first" | hits "a new session of agent:a on the same task"
agent agent:reader search "{\"query\": \"$marker\"}" | hits "agent:reader, on no task"
echo "  agent:reader asks for it by id: $(agent agent:reader get "{\"id\": \"$note\"}" 2>&1 | cut -c1-90 || true)"
a search "$marker" | j "f\"  you: {len(d)} hit(s)\" + ''.join(f\", {h['id']} at {h['trust_tier']}, reach {h.get('reach', 'fleet')}\" for h in d)"
a promote "$note" --to curated --why "read against the issue: it says what the issue asks, and no more" >/dev/null
echo "  you promoted $note to curated"
agent agent:reader search "{\"query\": \"$marker\"}" | hits "agent:reader, on no task"
a log outside --domain dom-demo | j "f\"  the log names {len(d)} session(s) that read outside content\" + ''.join(f\"; {r['by']}, session {r['session']}: {len(r['writes'])} write(s), {len(r['sends'])} send(s) after the read\" for r in d if '$note' in r['writes'])"
