#!/bin/sh
# The Markdown the demo reads, checked as the demo reads it. scripts/docs-only.sh lets a
# change to docs/ or examples/decisions/ skip the build and the tests, but those files are
# the demo's record: this seeds a scratch record from them as `make demo-ready` does, and
# fails when what reads that record would break.
#
#   - every question in the golden suites the demo scores (golden/docs.toml, and
#     golden/platform.toml, which act 7 files) names an entry the record holds, and its
#     keywords find that entry in the top k;
#   - every question an act puts to a harness (golden/acts.toml) names an entry the
#     record holds;
#   - every task in evals/demo.toml names, in `answers`, an entry the record holds, and
#     its `expect` matches the text of one of them, or, for a task with `counts`, the
#     number of lines of one that match `counts`;
#   - act 11 finds both of its lists at the top of README.md, and prints every item of
#     each.
#
#   scripts/corpus-check.sh target/debug/amem [directories to seed from]
set -eu
amem=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
shift
[ $# -gt 0 ] || set -- examples/decisions docs
here=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
trap 'exit 1' INT TERM HUP
root=$work/record
cd "$here"

"$amem" --root "$root" init --owner human:demo:check --domain dom-demo >/dev/null
python3 scripts/seed-corpus.py --root "$root" --memory "$amem" "$@" >"$work/seed.txt"
tail -n 1 "$work/seed.txt"

for suite in docs platform acts; do
  "$amem" --root "$root" golden "golden/$suite.toml" --json >"$work/$suite.json"
done
python3 - evals/demo.toml "$work/evals.toml" <<'PY'
import json, sys, tomllib
tasks = tomllib.load(open(sys.argv[1], "rb"))["task"]
bad = [t["id"] for t in tasks if not t.get("answers")]
if bad:
    sys.exit(f"evals/demo.toml: tasks without `answers`: {', '.join(bad)}")
with open(sys.argv[2], "w") as out:
    out.write('domain = "dom-demo"\n')
    for t in tasks:
        out.write(f"\n[[question]]\nask = {json.dumps(t['id'], ensure_ascii=False)}\n")
        out.write(f"answers = {json.dumps(t['answers'], ensure_ascii=False)}\n")
PY
"$amem" --root "$root" golden "$work/evals.toml" --json >"$work/evals.json"
python3 - "$work" "$root/record/dom-demo" evals/demo.toml <<'PY'
import json, pathlib, re, sys, tomllib
work, record, evals_toml = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2]), sys.argv[3]
fail = []
for suite in ("docs", "platform", "acts"):
    g = json.loads((work / f"{suite}.json").read_text())
    fail += [f"golden/{suite}.toml: no entry is titled as the answer to: {q}" for q in g["unresolved"]]
    if suite != "acts":
        fail += [
            f"golden/{suite}.toml: its keywords do not find the answer in the top {g['k']}: {r['ask']}"
            for r in g["rows"]
            if r["strategy"] == "keywords" and r["rank"] is None
        ]
        for s in g["scores"]:
            print(f"  golden/{suite}.toml {s['strategy']:<9} {s['hits']} of {s['asked']}, MRR {s['mrr']:.2f}")
    else:
        print(f"  golden/acts.toml: {g['questions'] - len(g['unresolved'])} of {g['questions']} questions name an entry")
e = json.loads((work / "evals.json").read_text())
fail += [f"evals/demo.toml: no entry is titled as the answer to task {t}" for t in e["unresolved"]]

# the entries as golden matches them: an answer is a part of the title, in any case
entries = []
for p in record.glob("*.md"):
    _, front, body = p.read_text().split("---\n", 2)
    title = next((l[7:].strip() for l in front.splitlines() if l.startswith("title: ")), "")
    if title[:1] == "'" and title[-1:] == "'":
        title = title[1:-1].replace("''", "'")
    elif title[:1] == '"' and title[-1:] == '"':
        title = json.loads(title)
    entries.append((title.lower(), body))
matched = 0
for t in tomllib.load(open(evals_toml, "rb"))["task"]:
    bodies = [b for title, b in entries if any(a.strip().lower() in title for a in t["answers"])]
    if "counts" in t:
        bodies = [str(len(re.findall(t["counts"], b, re.M))) for b in bodies]
    if bodies and not any(re.search(t["expect"], b, re.I | re.S) for b in bodies):
        fail.append(f"evals/demo.toml: task {t['id']}: its `expect` matches no text of the entries its `answers` name")
    elif bodies:
        matched += 1
print(f"  evals: {e['questions'] - len(e['unresolved'])} of {e['questions']} tasks name an entry, {matched} of them hold the answer")
if fail:
    sys.exit("\n".join(fail))
PY

AMEM=$amem sh scripts/demo/11-limits.sh "$root" >"$work/act11.txt"
python3 - "$work/act11.txt" README.md <<'PY'
import re, sys
text, readme = open(sys.argv[1]).read(), open(sys.argv[2]).read()
parts = re.split(r"^11\.\d  ", text, flags=re.M)[1:]
counts = [len(re.findall(r"^  - ", p, re.M)) for p in parts]
heads = ("What the paper asks for that is not built", "What is built but works only at demo scale")
# the items of the first list under each heading: it ends at the first line that is not
# an item, a line of one, or blank
wanted = []
for head in heads:
    part = readme.split("### " + head, 1)[1].split("\n#", 1)[0] if "### " + head in readme else ""
    items, started = 0, False
    for line in part.splitlines():
        if line.startswith("- "):
            items, started = items + 1, True
        elif started and line and not line.startswith(" "):
            break
    wanted.append(items)
print(f"  act 11: {' and '.join(map(str, counts))} items, of {' and '.join(map(str, wanted))} in README.md")
if len(counts) != 2 or 0 in counts or counts != wanted:
    sys.exit(f"act 11: it does not print every item of the two lists in README.md:\n{text}")
PY
