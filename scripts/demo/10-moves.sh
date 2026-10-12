#!/bin/sh
# Act 10: the record moves. The record outlives every component because it is plain
# entries and an ordered log of them: the log rebuilds the index, a bundle carries the
# entries at or below a label to another record, and an erasure travels as a receipt
# that refuses the text if it comes back.
. "$(dirname "$0")/lib.sh"
act 10 "The record moves" "The event log every write goes through, by id and digest and never a body; the index rebuilt from it; the record exported at a label and imported into another, under another owner; and an erasure that holds when the same bundle is imported again." "the event log and its materializer; you, exporting the record; and a second record, another owner's, importing it."
work=$(mktemp -d)

say "The event log" "Every version an entry is written at, and every erasure, in order, one writer per domain. It holds ids and digests, never bodies, so erasing a body leaves the log as it was."
a log show --domain dom-demo | j "f\"  {len(d)} events: \" + ', '.join(f'{n} {k}' for k, n in sorted(__import__('collections').Counter(e['kind'] for e in d).items())) + '\n' + '\n'.join(f\"  {e['seq']:>5}  {e['kind']:<7} {e['id']}  {(e.get('digest') or '')[:16]}\" for e in [x for x in d if x['kind'] != 'put'][:1] + d[-3:])"

say "The index, rebuilt" "The materializer reads the log and the record and builds the index from them, and replays the ledger's own events; it says what the record holds at another version than the log names."
a log materialize --domain dom-demo | j "f\"  {d['events']} events read, {d['live']} entries live in the index, {d['ledger_events']} ledger events replayed, {len(d['behind'])} behind the log\""

say "Exported at a label" "A bundle is the entries at or below a label, Markdown and a manifest. You mark one note public; a public export carries that note alone, an internal one the whole domain."
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
rm -rf "$work"
