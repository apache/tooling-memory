"""A domain's event log, from `amem log show` on stdin, as a few lines the room can
read: how many events of each kind, and the first event of each kind."""
import collections
import json
import sys

events = json.load(sys.stdin)
kinds = collections.Counter(e["kind"] for e in events)
print(f"  {len(events)} events: " + ", ".join(f"{n} {k}" for k, n in sorted(kinds.items())))


def names(e):
    kind = e["kind"]
    if kind == "read":
        ids = e["ids"]
        more = f" and {len(ids) - 2} more" if len(ids) > 2 else ""
        return ", ".join(ids[:2]) + more
    if kind == "send":
        return f"to {e['to']}  {e['digest'][:16]}"
    if kind == "ledger":
        return f"{e['op']} {e['task']}"
    if kind == "quarantined":
        return f"{e['id']}  refused by {e['gate']}, write event {e['of']}"
    return f"{e['id']}  {e.get('digest', '')[:16]}"


seen = set()
for e in events:
    if e["kind"] in seen:
        continue
    seen.add(e["kind"])
    label = e.get("label", "")
    print("  %6d  %-11s %-10s %s" % (e["seq"], e["kind"], label, names(e)))
