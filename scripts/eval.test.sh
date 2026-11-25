#!/bin/sh
# What scripts/eval.py does with the arms of a plan, shown with a stub in the place of
# `amem`: no build, no harness and no model. The stub records each launch, and it leaves
# a transcript with a fixed usage, as the record keeps one for a session.
#
#   sh scripts/eval.test.sh
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/root" "$work/out"

# The stub. Each launch gets the next session id, and the stub records when the launch
# started. The first run takes more than one second, so a snapshot that is taken again
# for a later run is a different snapshot. Two runs are damaged on purpose. The
# no-memory run at seed 0 of dr19-decision, the first run, leaves no transcript, so it
# has no usage. The t0-only run at seed 1 of dr19-spike prints no session id; its
# transcript is in the harness's home in the sandbox, where the script also looks, so it
# has a usage and no session.
cat >"$work/amem" <<'STUB'
#!/bin/sh
work=$(dirname "$0")
echo "$*" >>"$work/calls.log"
date -u +%Y-%m-%dT%H:%M:%SZ >>"$work/times.log"
n=$(wc -l <"$work/calls.log" | tr -d ' ')
session=sess-stub$n
arm= task= seed=
while [ $# -gt 0 ]; do
  case $1 in
    --arm) arm=$2 ;;
    --eval-task) task=$2 ;;
    --eval-seed) seed=$2 ;;
  esac
  shift
done
kept=$work/root/record/dom-demo/.transcripts
[ "$arm $task $seed" = "t0-only dr19-spike 1" ] && kept=$work/root/state/sandbox/claude/home
if [ "$arm $task $seed" != "no-memory dr19-decision 0" ]; then
  mkdir -p "$kept"
  echo '{"type": "assistant", "message": {"id": "m1", "model": "stub", "usage": {"input_tokens": 10, "output_tokens": 2, "cache_read_input_tokens": 3, "cache_creation_input_tokens": 1}}}' \
    >"$kept/$session.jsonl"
fi
[ "$arm $task $seed" = "t0-only dr19-spike 1" ] || echo "amem: session $session" >&2
[ "$arm $task $seed" = "no-memory dr19-decision 0" ] && sleep 2
# only the full arm knows the spike
[ "$arm" = full ] && echo "The S2 spike settles it." || echo "I do not know."
STUB
chmod +x "$work/amem"

# the set by a relative path: the launcher starts in a different directory
run() { cd "$here" && AMEM="$work/amem" python3 scripts/eval.py evals/demo.toml --root "$work/root" --harness claude --out "$work/out" "$@"; }

run --conditions no-memory,t0-only,full --only dr19-decision,dr19-spike >"$work/stdout" 2>"$work/stderr" \
  || { cat "$work/stderr" >&2; echo "scripts/eval.py: the run of the arms stopped with an error" >&2; exit 1; }
cp "$work/calls.log" "$work/arms.log"
cp "$work/times.log" "$work/arms-times.log"
# the next run can have the same stamp in its file names, so keep the outcomes now
sed -n 's/^outcomes //p' "$work/stdout" | while read -r path; do cp "$path" "$work/outcomes.json"; done
: >"$work/calls.log"
run --conditions full --only dr19-spike --seeds 3 >"$work/stdout3" 2>/dev/null
cp "$work/calls.log" "$work/seeds3.log"
: >"$work/calls.log"
run --conditions memory --only dr19-spike >"$work/stdout-memory" 2>/dev/null
cp "$work/calls.log" "$work/memory.log"
: >"$work/calls.log"
two=0
run --harness codex,claude --conditions no-memory,full --only dr19-spike >/dev/null 2>"$work/stderr-two" || two=$?
echo "$two" >"$work/two.exit"
cp "$work/calls.log" "$work/two.log"

python3 - "$work" "$here" <<'PY'
import json, pathlib, re, sys
work, here = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
sys.path.insert(0, str(here / "scripts"))
import eval as runner

fail = []
def check(what, ok):
    if not ok:
        fail.append(what)

# the snapshot is five seconds before the start, as RFC 3339 in UTC: a fixed time, not
# a reading of the clock
check("the snapshot of a start at 1000000000 is 2001-09-09T01:46:35Z", runner.snapshot(1000000000) == "2001-09-09T01:46:35Z")
check("the cost of a run is every token: input, output, cache read, cache write",
      runner.cost_of({"input": 1, "output": 20, "cache_read": 300, "cache_write": 4000}) == 4321)

calls = (work / "arms.log").read_text().splitlines()
# three arms, two tasks, the plan's two seeds
check(f"an arm runs each task once per seed of the plan: 12 launches, got {len(calls)}", len(calls) == 12)
flag = lambda c, name: (re.search(rf"{name} (\S+)", c) or [None, None])[1]
check("every arm run has one snapshot", len({flag(c, "--as-of") for c in calls}) == 1 and flag(calls[0], "--as-of"))
# the times are RFC 3339 in UTC to the second, so their order is the order of the text
times = (work / "arms-times.log").read_text().splitlines()
check(f"the snapshot is before the first launch: {flag(calls[0], '--as-of')}, {times[:1]}",
      len(times) == 12 and (flag(calls[0], "--as-of") or "~") < times[0])
check(f"the launches span more than one second, so one snapshot is not one reading per run: {times[0]}, {times[-1]}",
      len(times) == 12 and times[0] < times[-1])
check("the snapshot reads as RFC 3339 in UTC", re.fullmatch(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ", flag(calls[0], "--as-of") or ""))
check("an arm run names the plan, by an absolute path", all(flag(c, "--eval-plan") == str(here / "evals" / "demo.toml") for c in calls))
check("an arm run takes its prompt from the plan: no --prompt", not any("--prompt" in c for c in calls))
check("an arm run leaves no handoff", all("--no-handoff" in c for c in calls))
runs = sorted((flag(c, "--arm"), flag(c, "--eval-task"), flag(c, "--eval-seed")) for c in calls)
want = sorted((a, t, str(n)) for a in ("no-memory", "t0-only", "full") for t in ("dr19-decision", "dr19-spike") for n in (0, 1))
check(f"each arm, task and seed is one run: {runs}", runs == want)

stdout = (work / "stdout").read_text()
stderr = (work / "stderr").read_text()
m = re.search(r"^outcomes (.+)$", stdout, re.M)
check("the script prints the path of the outcomes on a line `outcomes <path>`", m is not None)
outcomes = json.loads((work / "outcomes.json").read_text()) if m else []
check(f"a run with no session id and a run with no usage are left out: 10 outcomes, got {len(outcomes)}", len(outcomes) == 10)
check("each outcome is {session, success, cost, judge}", all(sorted(o) == ["cost", "judge", "session", "success"] for o in outcomes))
check("each outcome names a session", all(re.fullmatch(r"sess-stub\d+", o["session"] or "") for o in outcomes))
check("the cost of an outcome is 16: 10 in, 2 out, 3 cache read, 1 cache write", all(o["cost"] == 16 for o in outcomes))
check("the judge of an outcome is the regular expression", all(o["judge"] == "regex" for o in outcomes))
# full passes dr19-spike at both seeds; nothing else matches an `expect`
check(f"success is the grade of the answer: 2 passes, got {sum(o['success'] for o in outcomes)}", sum(o["success"] for o in outcomes) == 2)
check("the run with no session id is named on stderr", "left out of the outcomes: t0-only dr19-spike seed 1: no session id" in stderr)
check("the run with no usage is named on stderr", "left out of the outcomes: no-memory dr19-decision seed 0: no usage" in stderr)
check("the table has the seed count", "| harness | condition | seeds | passed |" in stdout and "| claude | full | 2 | 2/4 |" in stdout)

check("--seeds gives the count of runs of a task", len((work / "seeds3.log").read_text().splitlines()) == 3)
memory = (work / "memory.log").read_text().splitlines()
check("the condition `memory` runs a task once, with --prompt and no arm",
      len(memory) == 1 and "--prompt" in memory[0] and "--arm" not in memory[0] and "--eval-plan" not in memory[0])
check("arms on two harnesses are refused before a launch",
      (work / "two.exit").read_text().strip() == "1" and (work / "two.log").read_text() == ""
      and "an arm run takes one harness" in (work / "stderr-two").read_text())
check("a run with no arm writes no outcomes", "outcomes " not in (work / "stdout-memory").read_text())
if fail:
    sys.exit("scripts/eval.py:\n" + "\n".join("  FAIL " + f for f in fail))
print("eval.test.sh: every check passed")
PY
