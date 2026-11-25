#!/bin/sh
# Act 10: the record moves. The record outlives every component because it is plain
# entries and an ordered log of them: the log feeds and rebuilds the index, a bundle
# carries the entries at or below a label to another record, an erasure travels as a
# receipt that refuses the text if it comes back, an erasure follows the log to the
# entries written and the sends made after a read of what it erased, and a restore
# drill shows that a recovery bundle brings back a task in flight and keeps an erasure.
. "$(dirname "$0")/lib.sh"
act 10 "The record moves" "The event log every write, read and send goes through, by id and digest and never a body; the index fed and rebuilt from it; the record exported at a label and imported into another, under another owner; an erasure that holds when the same bundle is imported again; an erasure that follows lineage to what was written and sent after; and a restore drill on a recovery bundle, with the ownership registry checked first." "the event log, its feed and its materializer; you, exporting the record; a second record, another owner's, importing it; agent:reader, played by the demo script; and a scratch root that the restore drill makes."
work=$(mktemp -d)

say "The event log" "Every write as it was offered, every version the record took, every write the screen refused, every erasure, every read, every send and every event of the ledger, in order, one writer per domain. It holds ids and digests, never bodies, so erasing a body leaves the log as it was. Each event has a label, and the log is for the owner and the security role."
a log show --domain dom-demo | python3 "$here/scripts/demo/events.py"

say "The index, fed and rebuilt" "A writer feeds the index from the log before its write returns, and the bound on how long a version may wait is published: 60 seconds. The materializer builds the same index again from the log and the record, and replays the ledger's own events; it says what the record holds at another version than the log names."
a log freshness --domain dom-demo | j "f\"  the feed: the index took the log up to event {d['fed_seq']} of {d['log_seq']}; {d['unfed']} versions wait above it; within the bound of {d['bound_seconds']} seconds: {'yes' if d['within_bound'] else 'no'}\""
a log materialize --domain dom-demo | j "f\"  rebuilt: {d['events']} events read, {d['live']} entries live in the index, {d['ledger_events']} ledger events replayed, {len(d['behind'])} behind the log\""

say "Exported at a label" "A bundle is the entries at or below a label, Markdown and a manifest. You write one new note, marked public; a public export carries that note alone, an internal one the whole domain."
a write --domain dom-demo --kind fact --title "The demo record is open to read" --classification public --body "This note is public: it may leave in a bundle at the public label." | j "f\"  wrote {d['id']}, {d['classification']}\""
a export "$work/public" --domain dom-demo --max-label public 2>&1 | sed 's/ to .*, highest/, highest/; s/^/  /'
a export "$work/internal" --domain dom-demo --max-label internal 2>&1 | sed 's/ to .*, highest/, highest/; s/^/  /'
erased=$(a log show --domain dom-demo | j "next((e['id'] for e in d if e['kind'] == 'erased'), '')")
if [ -n "$erased" ]; then
  [ -e "$work/internal/$erased.md" ] && echo "  the entry erased in 1.4, $erased, is in the bundle" \
    || echo "  the entry erased in 1.4, $erased, is not in the bundle"
fi

say "Imported elsewhere" "A second record, another owner and another domain, imports the internal bundle; its search finds what this one knew."
"$memory" --root "$work/elsewhere" init --owner human:demo:elsewhere --domain dom-elsewhere >/dev/null
"$memory" --root "$work/elsewhere" import "$work/internal" --domain dom-elsewhere 2>&1 | sed 's/^/  /'
"$memory" --root "$work/elsewhere" search "DR-19 MoinMoin" | j "f\"  searching there: {len(d)} hits, the first {d[0]['title'][:60]!r}\" if d else '  searching there: nothing'"

say "An erasure holds" "The new owner erases an entry it imported. Importing the same bundle again brings back everything else, and refuses that one: its receipt names the text that was removed."
victim=$("$memory" --root "$work/elsewhere" search "DR-19 MoinMoin" | j "d[0]['id']")
"$memory" --root "$work/elsewhere" erase "$victim" --reason mistaken >/dev/null 2>&1
echo "  erased $victim there"
"$memory" --root "$work/elsewhere" import "$work/internal" --domain dom-elsewhere 2>&1 | sed 's/^/  /'

say "An erasure follows lineage" "You write a note in error. An agent reads it and writes a second note, and the domain is exported. You erase the first note. The erasure follows the log: the second note is withheld from every read until it is derived again or you release it, and the receipt names the export, a copy that no erasure can reach. The agent's session is still open, with the first note in its context. It writes a third note, and that note is withheld as it lands."
wrong=$(a write --domain dom-demo --kind fact --title "The walkthrough moved" --body "Written in error: the walkthrough moved to room 4.12." | j "d['id']")
# what you do while the agent's session is open: the export, and then the erasure
printf '%s\n' \
  "\"$memory\" --root \"$root\" export \"$work/after\" --domain dom-demo --max-label internal > \"$work/export.out\" 2>&1" \
  "\"$memory\" --root \"$root\" erase \"$wrong\" --reason mistaken > \"$work/receipt.json\"" > "$work/erase.sh"
agent agent:reader steps "[{\"tool\": \"get\", \"arguments\": {\"id\": \"$wrong\"}}, {\"tool\": \"write\", \"arguments\": {\"title\": \"Where visitors go\", \"body\": \"The visitors of the walkthrough go to the new room this week.\"}}, {\"run\": \"sh $work/erase.sh\"}, {\"tool\": \"write\", \"arguments\": {\"title\": \"Where visitors wait\", \"body\": \"The visitors of the walkthrough wait outside the new room.\"}}]" > "$work/steps.jsonl"
derived=$(sed -n 2p "$work/steps.jsonl" | j "d['result']['id']")
echo "  you wrote $wrong; agent:reader read it and wrote $derived, which does not name it"
sed 's/ to .*, highest/, highest/; s/^/  /' "$work/export.out"
python3 "$here/scripts/demo/receipt.py" "$root/record/dom-demo/$wrong.md" < "$work/receipt.json"
sed -n 3p "$work/steps.jsonl" | j "f\"  the same session wrote {d['result']['id']} after the erasure: \" + (f\"withheld, because it read {d['result']['withheld']}\" if d['result'].get('withheld') else 'served')"
echo "  a search for the second note: $(a search 'visitors walkthrough' | j 'len(d)') hits"
a withheld list --domain dom-demo | j "'\n'.join(f\"  withheld: {w['id']}, because {w.get('because', 'an erasure')} was erased\" for w in d) or '  nothing is withheld'"
a withheld release "$derived" | j "f\"  you looked at it and it stands: released {d['released']}\""
echo "  the search again: $(a search 'visitors walkthrough' | j 'len(d)') hits"

say "A restore drill" "A backup of the record alone loses the tasks in flight. A recovery bundle holds the record and the ledger at checkpoints that fit together. First the ownership registry says who answers for each domain, and flags a domain that nobody answers for. Then agent:reader claims a task, you make a recovery bundle, and you erase a note. The restore drill restores the bundle into a scratch root, with the erasure journal and the next claim epoch, and reports five cases. The record and the ledger of this root are not written."
owner=$(sed -n 's/^owner = "\(.*\)"$/\1/p' "$root/grants.toml")
domains=$(sed -n 's/^\[domains\.\(.*\)\]$/"\1"/p' "$root/grants.toml" | paste -sd, -)
# the registry names a person by a dated identifier; an assertion says that the
# identifier in the grants names the same person
dated=$(a registry resolve "$owner" | j "d['id'] if d['dated'] else f\"human:{d['authority']},2026-11-01:{d['user_id']}\"")
registry() {
  {
    printf '[units.unit-7f3a]\nname = "The demo unit"\ndomains = [%s]\nhorizon_scanner = "%s"\n' "$domains" "$dated"
    [ "$1" = owned ] && printf '\n[[units.unit-7f3a.owners]]\nid = "%s"\n' "$dated"
    [ "$dated" != "$owner" ] && printf '\n[[assertions]]\nwas = "%s"\nis = "%s"\nsince = 2026-11-01\n' "$owner" "$dated"
    :
  } > "$root/registry.toml"
}
flags="f\"  registry check: {len(d['flags'])} flags\" + ''.join(f\"\\n    {x['flag']}: {x.get('domain') or x.get('unit')}\" for x in d['flags'])"
registry owned
echo "  registry.toml: the unit unit-7f3a answers for $(echo "$domains" | tr -d '"' | sed 's/,/, /g'), and $dated owns it"
a registry resolve "$owner" | j "f\"  {d['id']} is {d['is']} now\" if d['id'] != d['is'] else f\"  {d['id']} is a dated identifier\""
a registry check | j "$flags"
registry ownerless
echo "  the owner is removed from the unit:"
a registry check | j "$flags"
registry owned
echo "  the owner is back: $(a registry check | j "len(d['flags'])") flags"
drilled=$(a task create "Check the restore" --domain dom-demo --body "Held under a claim while the recovery bundle is made." | j "d['id']")
a task admit "$drilled" >/dev/null 2>&1 || true
generation=$(agent agent:reader claim "{\"id\": \"$drilled\"}" | j "d['generation']")
echo "  agent:reader claimed $drilled at generation $generation"
lost=$(a write --domain dom-demo --kind fact --title "The restore runs on Fridays" --body "Written in error: the restore runs on Fridays in room 4.12." | j "d['id']")
a recover bundle "$work/recovery" | j "f\"  recovery bundle: {len(d['domains'])} domains, {len(d['files'])} files, {d['ledger']['events']} events of the ledger, claim epoch {d['ledger']['epoch']}; the index is not in it\""
a erase "$lost" --reason mistaken >/dev/null 2>&1
echo "  after the bundle, you erased $lost: the bundle still holds its text"
a recover drill "$work/recovery" --scratch "$work/drill" > "$work/drill.json"
j "'\n'.join(f\"  {c['name']:<32} {c['outcome']:<10} {c['detail']}\" for c in d['cases'])" < "$work/drill.json"
j "f\"  the restore began claim epoch {d['epoch']}; no case failed: {'yes' if d['passed'] else 'no'}\"" < "$work/drill.json"
j "next(('  on the restored root, the heartbeat of agent:reader under generation $generation is refused: that claim is from an earlier claim epoch' for c in d['cases'] if c['name'] == 'unfinished-task' and c['outcome'] == 'recovered' and 'is refused' in c['detail']), '  the drill did not show a refused heartbeat')" < "$work/drill.json"
echo "  on this root the claim is still in force: $(a task get "$drilled" | j "f\"{d['status']} by {d['holder']} at generation {d['generation']}\"")"
agent agent:reader release "{\"id\": \"$drilled\", \"generation\": $generation}" >/dev/null
a task invalidate "$drilled" --reason "obsolete: shown" >/dev/null
rm -f "$root/registry.toml"
echo "  the task is released and closed, and registry.toml is removed: the root has no registry, as before"
rm -rf "$work"
