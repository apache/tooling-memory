#!/bin/sh
# The close: what the demo did not show, and what it showed only at demo scale, read
# from the README's own lists so the two never disagree.
. "$(dirname "$0")/lib.sh"
act 7 "What this is not, yet" "A proof of concept: the paper's contracts are built and exercised, the stores behind them are the simplest that hold them on one laptop. The two lists at the top of the README, read from it." "you, reading them out."
python3 - "$here/README.md" <<'PY'
import re, sys
text = open(sys.argv[1]).read()
for head in ("What the paper asks for that is not built", "What is built but works only at demo scale"):
    part = text.split("### " + head, 1)[1].split("\n#", 1)[0]
    names = re.findall(r"^- \*\*(.+?)\*\*", part, re.M)
    print("\n" + "-" * 78 + f"\n7.{1 + (head.startswith('What is built'))}  {head}\n" + "-" * 78)
    for n in names:
        print("  - " + n.rstrip("."))
print("\n  The README's first section says what replaces each, and why the shape is deliberate.")
PY
