#!/bin/sh
# The close: what the demo did not show, and what it showed only at demo scale, read
# from the README's own lists so the two never disagree.
. "$(dirname "$0")/lib.sh"
act "What this is not, yet"
python3 - "$here/README.md" <<'PY'
import re, sys
text = open(sys.argv[1]).read()
for head in ("What the paper asks for that is not built", "What is built but works only at demo scale"):
    part = text.split("### " + head, 1)[1].split("\n#", 1)[0]
    names = re.findall(r"^- \*\*(.+?)\*\*", part, re.M)
    print("\n== " + head.lower())
    for n in names:
        print("  - " + n.rstrip("."))
print("\n  The README's first section says what replaces each, and why the shape is deliberate.")
PY
