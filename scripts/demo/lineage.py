"""A lineage answer, from `amem lineage` on stdin, as a few lines the room can read.

    amem lineage <id> [--back] | python3 lineage.py [<id to point out>]

It prints at most five entries, nearest first, and the entry to point out always."""
import json
import sys

l = json.load(sys.stdin)
mark = sys.argv[1] if len(sys.argv) > 1 else None
back = l["direction"] == "back"


def how(e):
    read = "its session read this before" if back else "written after a session read it"
    cited = "it cites this" if back else "cites it"
    return " and ".join(w for w, on in ((read, e["informed"]), (cited, e["cited"])) if on)


def count(n, one, many):
    return f"{n} {one if n == 1 else many}"


entries = l["entries"]
found = count(len(entries), "entry", "entries")
if back:
    head = f"back from {l['of']}: {found}"
else:
    readers = count(len(l["sessions"]), "session", "sessions")
    sends = count(len(l["sends"]), "send", "sends")
    head = f"forward from {l['of']}: {readers} read it; {found} and {sends} after"
if not l["complete"]:
    head += "; the log does not hold the whole answer"
print("  " + head)
shown = entries[:5]
if mark and all(e["id"] != mark for e in shown):
    shown += [e for e in entries if e["id"] == mark]
for e in shown:
    hops = "1 step" if e["hops"] == 1 else f"{e['hops']} steps"
    print(f"    {e['id']}  {hops}  {how(e)}" + ("  <- the note" if e["id"] == mark else ""))
if len(entries) > len(shown):
    print(f"    and {len(entries) - len(shown)} more")
for s in l["sends"][:3]:
    print(f"    sent to {s['to']}, {s['count']} time(s), from event {s['first']} of {s['domain']}")
if mark and all(e["id"] != mark for e in entries):
    print(f"    {mark} is not among them")
