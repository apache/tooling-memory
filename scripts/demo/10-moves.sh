#!/bin/sh
# Act 10: the record moves. The record outlives every component because it is plain
# entries and an ordered log of them: the log feeds and rebuilds the index, a bundle
# carries the entries at or below a label to another record, an erasure travels as a
# receipt that refuses the text if it comes back, and an erasure follows the log to the
# entries written and the sends made after a read of what it erased.
. "$(dirname "$0")/lib.sh"
act 10 "The record moves" "The event log every write, read and send goes through, by id and digest and never a body; the index fed and rebuilt from it; the record exported at a label and imported into another, under another owner; an erasure that holds when the same bundle is imported again; and an erasure that follows lineage to what was written and sent after." "the event log, its feed and its materializer; you, exporting the record; a second record, another owner's, importing it; and agent:reader, played by the demo script."
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

say "An erasure follows lineage" "You write a note in error. An agent reads it and writes a second note, and the domain is exported. You erase the first note. The erasure follows the log: the second note is withheld from every read until it is derived again or you release it, and the receipt names the export, a copy that no erasure can reach."
wrong=$(a write --domain dom-demo --kind fact --title "The walkthrough moved" --body "Written in error: the walkthrough moved to room 4.12." | j "d['id']")
derived=$(agent agent:reader batch "[{\"tool\": \"get\", \"arguments\": {\"id\": \"$wrong\"}}, {\"tool\": \"write\", \"arguments\": {\"title\": \"Where visitors go\", \"body\": \"The visitors of the walkthrough go to the new room this week.\"}}]" | j "d['id']")
echo "  you wrote $wrong; agent:reader read it and wrote $derived, which does not name it"
a export "$work/after" --domain dom-demo --max-label internal 2>&1 | sed 's/ to .*, highest/, highest/; s/^/  /'
a erase "$wrong" --reason mistaken | python3 "$here/scripts/demo/receipt.py" "$root/record/dom-demo/$wrong.md"
echo "  a search for the second note: $(a search 'visitors walkthrough' | j 'len(d)') hits"
a withheld list --domain dom-demo | j "'\n'.join(f\"  withheld: {w['id']}, because {w.get('because', 'an erasure')} was erased\" for w in d) or '  nothing is withheld'"
a withheld release "$derived" | j "f\"  you looked at it and it stands: released {d['released']}\""
echo "  the search again: $(a search 'visitors walkthrough' | j 'len(d)') hits"
rm -rf "$work"
