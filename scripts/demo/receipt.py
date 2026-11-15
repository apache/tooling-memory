"""An erasure receipt, from `amem erase` on stdin, as a few lines the room can read.

    amem erase <id> --reason <class> | python3 receipt.py [<the stub's file in the record>]

With the file of the stub, it also prints which fields the stub keeps."""
import json
import sys

r = json.load(sys.stdin)
when = r["erased_at"][:19]
print(f"  the receipt: {r['id']} erased by {r['erased_by']}, reason {r['reason']}, at {when}Z")
for v in r["removed"]:
    print(f"    removed {v['version']}: digest {v['digest'][:16]}..., kept so a copy coming back is known")
print(f"    entries withheld until they are derived again: {', '.join(r.get('withheld', [])) or 'none'}")
if r.get("further"):
    print(f"    entries further downstream, listed and not withheld: {', '.join(r['further'])}")
for s in r.get("sends", []):
    print(f"    sent after a read of it: to {s['to']}, {s['count']} time(s), from event {s['first']} of {s['domain']}; a copy that the erasure cannot reach")
if not r.get("sends"):
    print("    sends after a read of it: none")
if len(sys.argv) > 1:
    front = open(sys.argv[1], encoding="utf-8").read().split("---\n")[1]
    # the author and the status of a stub are the word `erased`: shown, so nobody reads
    # the field name as the author that wrote the entry
    shown = ("author", "status")
    lines = [line for line in front.splitlines() if line[:1].isalpha()]
    kept = [line if line.split(":")[0] in shown else line.split(":")[0] for line in lines]
    print(f"    the stub in the record keeps: {', '.join(kept)}; and no text")
