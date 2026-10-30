#!/bin/sh
# The Markdown the demo reads, checked as the demo reads it. scripts/docs-only.sh lets a
# change to docs/ or examples/decisions/ skip the build and the tests, but those files are
# the demo's record: this seeds a scratch record from them as `make demo-ready` does, and
# fails when what reads that record would break.
#
#   - every golden question in golden/docs.toml names an entry the record holds, and its
#     keywords find that entry in the top k;
#   - every task in evals/demo.toml names, in `answers`, an entry the record holds;
#   - act 11 finds both of its lists at the top of README.md, and neither is empty.
#
#   scripts/corpus-check.sh target/debug/amem
set -eu
amem=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
here=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
root=$work/record
cd "$here"

"$amem" --root "$root" init --owner human:demo:check --domain dom-demo >/dev/null
python3 scripts/seed-corpus.py --root "$root" --memory "$amem" examples/decisions docs | tail -n 1

"$amem" --root "$root" golden golden/docs.toml --json >"$work/golden.json"
python3 - evals/demo.toml "$work/evals.toml" <<'PY'
import json, sys, tomllib
tasks = tomllib.load(open(sys.argv[1], "rb"))["task"]
bad = [t["id"] for t in tasks if not t.get("answers")]
if bad:
    sys.exit(f"evals/demo.toml: tasks without `answers`: {', '.join(bad)}")
with open(sys.argv[2], "w") as out:
    out.write('domain = "dom-demo"\n')
    for t in tasks:
        out.write(f"\n[[question]]\nask = {json.dumps(t['id'])}\nanswers = {json.dumps(t['answers'])}\n")
PY
"$amem" --root "$root" golden "$work/evals.toml" --json >"$work/evals.json"
python3 - "$work/golden.json" "$work/evals.json" <<'PY'
import json, sys
golden, evals = (json.load(open(p)) for p in sys.argv[1:3])
fail = [f"golden/docs.toml: no entry is titled as the answer to: {q}" for q in golden["unresolved"]]
fail += [
    f"golden/docs.toml: its keywords do not find the answer in the top {golden['k']}: {r['ask']}"
    for r in golden["rows"]
    if r["strategy"] == "keywords" and r["rank"] is None
]
fail += [f"evals/demo.toml: no entry is titled as the answer to task {t}" for t in evals["unresolved"]]
for s in golden["scores"]:
    print(f"  golden {s['strategy']:<9} {s['hits']} of {s['asked']}, MRR {s['mrr']:.2f}")
print(f"  evals: {evals['questions'] - len(evals['unresolved'])} of {evals['questions']} tasks name an entry")
if fail:
    sys.exit("\n".join(fail))
PY

AMEM=$amem sh scripts/demo/11-limits.sh "$root" >"$work/act11.txt"
python3 - "$work/act11.txt" <<'PY'
import re, sys
text = open(sys.argv[1]).read()
parts = re.split(r"^11\.\d  ", text, flags=re.M)[1:]
counts = [len(re.findall(r"^  - ", p, re.M)) for p in parts]
print(f"  act 11: {' and '.join(map(str, counts))} items")
if len(counts) != 2 or 0 in counts:
    sys.exit(f"act 11: README.md does not give it two lists with items:\n{text}")
PY
