"""An erasure receipt, from `amem erase` on stdin, as a few lines the room can read."""
import json
import sys

r = json.load(sys.stdin)
when = r["erased_at"][:19]
print(f"  the receipt: {r['id']} erased by {r['erased_by']}, reason {r['reason']}, at {when}Z")
for v in r["removed"]:
    print(f"    removed {v['version']}: digest {v['digest'][:16]}..., kept so a copy coming back is known")
print(f"    entries flagged for review: {', '.join(r['flagged']) or 'none'}")
