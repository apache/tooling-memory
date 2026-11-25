#!/bin/sh
# Act 1, steps 1.8 to 1.10: the surface of the paper's Appendix A. An address for a
# section and for a version, a search with a filter and a snippet, a recall sized for
# its caller, and `approve` and `export` as memory tools. No step needs a model.
#
# scripts/demo.sh sources this file at its end, so these steps follow 1.7. The file also
# runs alone on a seeded record, without a harness:
#
#   AMEM=$PWD/target/debug/amem sh scripts/demo/1-surface.sh <demo-root>
#
# Alone, it opens act 1 itself, and the steps are numbered 1.1 to 1.3.
#
# Each step writes its own entries, so the corpus and the golden suites stay as they
# are. The entries name a season that is new in each run, and the searches name it too,
# so a run does not find the entries of another run. The last step erases the three
# entries: the curator of act 2 reads the same record and the same read log, and it
# must not propose a merge of entries that the demo wrote, in one run or in two.
if [ -z "${memory:-}" ]; then
  . "$(dirname "$0")/lib.sh"
  act 1 "The read path: the surface of Appendix A" "The steps of act 1 that need no harness and no model: an entry by section and by version, a search with a filter, a recall sized for its caller, and an approval and an export through the memory tools." "you, the owner of the record, at the command line and in a session of your own (amem serve --person); agent:a and agent:curator, played by the demo script through the memory tools."
else
  a() { "$memory" --root "$root" "$@"; }
  j() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
  agent() { python3 "$here/scripts/demo/mcp.py" "$memory" "$root" "$@"; }
fi
# a tool call in a session of the owner in person: `amem serve --person`
person() { python3 "$here/scripts/demo/mcp.py" "$memory" "$root" person "$@"; }
surface_folded() { fold -s -w 96 | sed 's/ *$//; s/^/    /'; }
question=${question:-${QUERY:-what did DR-19 decide about MoinMoin}}
season="s$(date +%s)"

say "By address" "A search takes filters, and each hit has a snippet that the index cut around the words of the query. An address names an entry, one section of it, or one version of it. A citation of a version reads the text that it cited, also after the entry changed. A fetch carries the entries that searches return with the entry, each with a weight."
dec=$(printf 'The heron ferry of season %s needs one quay.\n\n## Context\n\nThe east quay has the deeper water. The west quay has the shorter walk from the car park, and the ferry cannot moor there at low tide.\n\n## Decision\n\nThe heron ferry of season %s leaves from the east quay.\n' "$season" "$season" \
  | a write --kind decision --title "Demo: the quay of the heron ferry" | j "d['id']")
note=$(a write --kind fact --title "Demo: the crossing time of the heron ferry" --body "The heron ferry of season $season takes forty minutes at low tide." | j "d['id']")
echo "  you write the decision $dec, with the sections Context and Decision, and the fact $note."
for n in 1 2 3; do a search "ferry $season" >/dev/null; done
echo "  three searches for \"ferry $season\" return the two entries together."
echo "  a search for the same words with --type decision returns the decision alone:"
a search "ferry $season" --type decision | j "'\n'.join(f\"    {h['id']}  {h['type']}  snippet: {h['snippet']}\" for h in d)"
whole=$(a search "ferry $season" --type decision --depth sections | j "d['tokens']")
echo "  amem get $dec#decision gives the section alone:"
got=$(a get "$dec#decision" --json)
printf '%s' "$got" | j "d['body'].strip()" | surface_folded
part=$(python3 - "$root/state/reads.jsonl" "$dec" <<'PY'
import json, sys
reads = [json.loads(line) for line in open(sys.argv[1])]
print([r["tokens"] for r in reads if r["tool"] == "get" and r["ids"] == [sys.argv[2]]][-1])
PY
)
echo "  the read log counts $part tokens for that section; the body of the entry has $whole."
version=$(printf '%s' "$got" | j "d['version']")
printf '%s' "$got" | j "'  the same fetch names the related entries, each with its weight p:\n' + '\n'.join(f\"    {r['id']}  p {r['p']}  {r['title']}\" for r in d.get('related', []))"
cites=$(a write --kind fact --title "Demo: where the night shift moors" --cites "$dec@$version" --body "The night shift moors at the quay that the cited decision names." | j "d['id']")
echo "  $cites cites the version that it read: $dec@$(printf '%s' "$version" | cut -c1-12)..."
agent agent:curator promote "{\"id\": \"$dec\", \"to\": \"curated\", \"why\": \"checked against the tide table\"}" >/dev/null
echo "  agent:curator promotes the decision to curated, so the entry has a new version: $(a list --kind decision --limit 1 | j "d[0]['id'] + ' is ' + d[0]['trust_tier']")"
echo "  amem get $dec@<version> still gives what the citation named:"
a get "$dec@$version" --json | j "f\"    current: {d['current']}, trust_tier: {d['trust_tier']}, version {d['version'][:12]}..., body: {d['body'].splitlines()[0]}\""

say "A recall sized for its caller" "The caller of a recall can say what kind of answer it needs and how full its context is. The memory server then plans the size of the answer. With no model behind the server, the plan bounds the pack, because the pack goes into the context of the caller."
sized() { j "f\"    {len(d['pack']['pieces'])} pieces, {d['pack']['tokens']} tokens; plan: {json.dumps(d.get('plan'))}\""; }
echo "  amem recall \"$question\":"
a recall "$question" | sized
echo "  the same recall with --answer-kind fact --context-tokens 120000:"
a recall "$question" --answer-kind fact --context-tokens 120000 | sized

say "Approve and export as memory tools" "The memory server has a tool that approves. It approves only in a session that a person started, amem serve --person. A session that a harness reaches is refused, whatever its author. An export through the memory server names no path: the server keeps the bundle under its own state and answers with counts, labels and a digest."
echo "  agent:a asks to erase $note:"
refusal=$(agent agent:a erase "{\"id\": \"$note\", \"reason\": \"mistaken\"}" || true)
printf '%s\n' "$refusal" | surface_folded
task=$(printf '%s' "$refusal" | sed -n 's/.*Task \([^ ]*\) asks.*/\1/p')
echo "  agent:a calls approve on $task itself:"
agent agent:a approve "{\"task\": \"$task\"}" | surface_folded || true
echo "  you call approve in your own session, and approvals lists the approval:"
person approve "{\"task\": \"$task\"}" | j "f\"    {d['outcome']}: {d['approval']['principal']} approved {d['approval']['action']} of version {d['approval']['version'][:12]} for {d['approval']['scope']}\""
person approvals "{\"id\": \"$note\"}" | j "'\n'.join(f\"    {x['action']} of {x['target']} until {x['expires'][:19]}Z, in force: {x['in_force']}\" for x in d['approvals'])"
echo "  agent:a asks again, and the erasure runs:"
agent agent:a erase "{\"id\": \"$note\", \"reason\": \"mistaken\"}" | python3 "$here/scripts/demo/receipt.py"
# the citing entry goes first: an erasure of the decision would withhold it
a erase "$cites" --reason mistaken >/dev/null
a erase "$dec" --reason mistaken >/dev/null
echo "  you erase the other two entries of these steps, so the record is as it was."
echo "  agent:a exports the domain at internal, and gets no path that it can open:"
agent agent:a export "{\"max_label\": \"internal\"}" | j "'\n'.join(f'    {k}: {v}' for k, v in d.items())"
