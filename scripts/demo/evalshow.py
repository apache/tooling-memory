"""How act 6 of the demo prints what `amem eval` and the scripted agent give as JSON.

    amem eval arms          | python3 evalshow.py arms
    mcp.py ... steps ...    | python3 evalshow.py under <arm>
    amem eval plan <set>    | python3 evalshow.py plan
    amem eval score ...     | python3 evalshow.py scorecard

Each reads stdin and prints lines for a person. No number is made here: each one is
in the JSON.
"""
import json
import sys


def on(b):
    return "on" if b else "off"


def arms(d):
    for x in d:
        s = x["settings"]
        print(f"  {x['arm']:<18} {x['stands_for']}")
        print(f"  {'':<18} memory {on(s['memory'])}, T0 {s['t0']}, prefetch {on(s['prefetch'])}, depth {s['depth']}, "
              f"tier up to {s['max_tier']}, transcripts {on(s['transcripts'])}")


def under(arm, lines):
    """One session under an arm: the `steps` lines of `settings`, `t0` and `search`."""
    steps = {s["tool"]: s for s in map(json.loads, lines)}
    t0 = steps["t0"]
    size = f"T0 is {len(t0['result'])} characters" if t0["ok"] else "T0 is refused: " + t0["refused"]
    search = steps["search"]
    if search["ok"]:
        found = f"{len(search['result']['hits'])} hits"
    else:
        # the refusal, without the prefix and the sentence that every refusal ends with
        found = "refused: " + search["refused"].split(": ", 1)[-1].split(". It is a session setting")[0]
    print(f"  {arm:<18} it reports the arm {steps['settings']['result']['arm']}; {size}; search: {found}")


def plan(d):
    p = d["plan"]
    print(f"  plan {p['name']} in {p['domain']}: baseline {p['baseline']}, {p['seeds']} seeds, margin {p['margin']}, "
          f"accepted cost {p['accepted_cost']} times the baseline, judge {p['judge']}")
    print("  arms: " + ", ".join(p["arms"]))
    for t in p["tasks"]:
        print(f"  task {t['id']:<18} {t['set']:<8} stratum {t['stratum']}")
    print(f"  digest {d['digest'][:16]}, of the frozen tasks {d['frozen_digest'][:16]}")


def name(v):
    """A finding or a verdict: its name, without what a verdict carries."""
    if v is None:
        return "-"
    return v if isinstance(v, str) else next(iter(v))


def scorecard(d):
    c = d["scorecard"]
    print(f"  plan {c['plan']} ({c['plan_digest'][:16]}): {c['harness']} on {c['model']}, judge {c['judge']}, snapshot {c['snapshot']}")
    print(f"  margin {c['margin']}, accepted cost {c['accepted_cost']} times the baseline; runs from {c['started'][:19]}Z to {c['ended'][:19]}Z")
    for a in c["arms"]:
        line = f"  {a['arm']:<18} {a['passed']}/{a['runs']} passed, mean cost {a['mean_cost']:.0f} tokens"
        if a["baseline"]:
            print(line + "; the baseline")
            continue
        s, k = a["success"], a["cost"]
        print(line + f"; {a['pairs']} pairs over {a['seeds']} seeds")
        print(f"  {'':<18} success {s['mean']:+.2f} [{s['low']:+.2f}, {s['high']:+.2f}], "
              f"cost {k['mean']:+.0f} [{k['low']:+.0f}, {k['high']:+.0f}] tokens; "
              f"finding {name(a['finding'])}, verdict {name(a['verdict'])}")
    if c["unpaired"]:
        print(f"  {c['unpaired']} runs have no partner in the baseline, and are in no difference")
    for t in c["traps"]:
        print(f"  trap {t['name']}: {'held' if t['held'] else 'OPEN'}. {t['why']}")
    if c["quarantined"]:
        w = c["quarantined"]
        print(f"  quarantined: the runs spanned the drift window of {w['destination']}/{w['model']}")
    print(f"  kept as {d['entry']}" if d.get("entry") else "  not kept")


if __name__ == "__main__":
    what = sys.argv[1]
    if what == "under":
        under(sys.argv[2], sys.stdin)
    else:
        {"arms": arms, "plan": plan, "scorecard": scorecard}[what](json.load(sys.stdin))
