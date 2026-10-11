#!/usr/bin/env python3
"""Memory against no memory, and harness against harness, on a set of tasks.

Each task in the set is a prompt and a regular expression its answer must match. Each
run is one prompted launch; the conditions are:

  memory    the harness through `amem launch --prompt`, with the record attached, so T0,
            search, recall and the hooks are there as in any session;
  empty     the same launch over an empty record (`--empty-memory`): the same sandbox,
            egress and tools, and nothing in memory, so the only difference from
            `memory` is what the record holds;
  bare      the harness as installed, in an empty working directory, nothing attached:
            as people use it today, its web and shell tools included, so it may
            rediscover an answer from sources at the cost of doing so. On a gateway a
            bare harness may not even connect, where the launcher's egress fixes the
            gateway's shape for it;
  isolated  the bare harness with its web, shell and subagent tools off: what it knows
            without memory and without rediscovery. Claude Code only, so far.

Both conditions report the tokens the harness itself counted, read from the transcript
it left, so the two are measured the same way; `amem usage` holds the egress's count
for the memory condition as well. The answer is what the harness printed; the grade is
whether it matches `expect`.

  scripts/eval.py evals/demo.toml --root ~/memory-demo2 --harness claude --auth subscription
  scripts/eval.py evals/demo.toml --root ~/memory-demo2 --harness codex,claude --auth gateway --model qwen3.8-27b
  scripts/eval.py evals/demo.toml --root ~/memory-demo2 --conditions memory,empty

Results go to evals/out/<set>-<stamp>.json and a table to stdout. Bare runs are known
for claude and codex; the memory condition takes every harness the launcher does.
"""
import argparse
import glob
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent.parent
AMEM = HERE / "target" / "release" / "amem"

BARE = {
    "claude": lambda prompt, model: ["claude", "-p", prompt, "--dangerously-skip-permissions"] + (["--model", model] if model else []),
    "codex": lambda prompt, model: ["codex", "exec", "--skip-git-repo-check", "--dangerously-bypass-approvals-and-sandbox", prompt] + (["-m", model] if model else []),
}

# the bare harness with no way to rediscover: no web, no shell, no subagents, and no
# files either, since the sources may well be on the same machine
ISOLATED = {
    "claude": lambda prompt, model: BARE["claude"](prompt, model) + ["--disallowedTools", "WebFetch,WebSearch,Bash,Agent,Task,Workflow,Read,Glob,Grep,Edit,Write,NotebookEdit,Skill"],
}


def newest(patterns, since):
    files = []
    for pat in patterns:
        # the harnesses keep their transcripts under dot-directories, which glob skips
        # unless told otherwise
        files += [p for p in glob.glob(pat, recursive=True, include_hidden=True) if os.path.getmtime(p) >= since]
    return max(files, key=os.path.getmtime) if files else None


def usage_of(path):
    """What the harness counted, from its transcript: Claude Code's per-call usage summed
    once per message id, or Codex's last running total."""
    if not path:
        return None
    seen = set()
    total = {"input": 0, "output": 0, "cache_read": 0, "cache_write": 0}
    codex = None
    model = None
    with open(path, encoding="utf-8", errors="replace") as f:
        for line in f:
            try:
                v = json.loads(line)
            except json.JSONDecodeError:
                continue
            if v.get("type") == "assistant":
                m = v.get("message", {})
                mid = m.get("id")
                if mid and mid in seen:
                    continue
                if mid:
                    seen.add(mid)
                u = m.get("usage")
                if u:
                    model = m.get("model", model)
                    total["input"] += u.get("input_tokens", 0)
                    total["output"] += u.get("output_tokens", 0)
                    total["cache_read"] += u.get("cache_read_input_tokens", 0)
                    total["cache_write"] += u.get("cache_creation_input_tokens", 0)
            elif v.get("type") == "turn_context":
                model = v.get("payload", {}).get("model", model)
            elif v.get("type") == "event_msg" and v.get("payload", {}).get("type") == "token_count":
                t = v["payload"].get("info", {}).get("total_token_usage")
                if t:
                    cached = t.get("cached_input_tokens", 0)
                    codex = {
                        "input": max(t.get("input_tokens", 0) - cached, 0),
                        "output": t.get("output_tokens", 0),
                        "cache_read": cached,
                        "cache_write": t.get("cache_write_input_tokens", 0),
                    }
    u = codex or total
    u["model"] = model
    return u


KEYS = ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "OPENAI_API_KEY", "GEMINI_API_KEY", "XAI_API_KEY", "META_API_KEY"]


def bare_env(env, harness, auth):
    """The environment a bare harness runs in, matching the auth mode the memory
    condition uses: a login means no key in sight, since a harness prefers a key it is
    given over its login; a key means that key; the gateway means the harness's own
    base-URL override pointed at it with the PAT as the key."""
    e = dict(env)
    if auth == "subscription":
        for k in KEYS:
            e.pop(k, None)
    elif auth == "gateway":
        url = e.get("LLMAO_URL", "http://127.0.0.1:4000")
        key = e.get("LLMAO_KEY", "")
        for k in KEYS:
            e.pop(k, None)
        if harness == "claude":
            e["ANTHROPIC_BASE_URL"] = url
            e["ANTHROPIC_AUTH_TOKEN"] = key
            e["CLAUDE_CODE_EFFORT_LEVEL"] = e.get("CLAUDE_CODE_EFFORT_LEVEL", "medium")
        elif harness == "codex":
            e["OPENAI_BASE_URL"] = url.rstrip("/") + "/v1"
            e["OPENAI_API_KEY"] = key
    if harness == "codex" and auth != "subscription":
        # a bare codex prefers its ChatGPT login to a key, and a gateway's model is
        # not one that login serves; a home with no login makes it use the key, and
        # a provider in its config is how it is pointed at a base other than OpenAI's
        home = tempfile.mkdtemp(prefix="eval-codex-home-")
        e["CODEX_HOME"] = home
        if auth == "gateway":
            base = e.get("LLMAO_URL", "http://127.0.0.1:4000").rstrip("/") + "/v1"
            Path(home, "config.toml").write_text(
                'model_provider = "gateway"\n\n[model_providers.gateway]\nname = "gateway"\n'
                f'base_url = "{base}"\nenv_key = "OPENAI_API_KEY"\nwire_api = "responses"\n'
            )
    return e


def run(cmd, cwd, env, timeout):
    started = time.time()
    try:
        p = subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True, timeout=timeout, stdin=subprocess.DEVNULL)
        out, err, code = p.stdout, p.stderr, p.returncode
    except subprocess.TimeoutExpired as e:
        # a timed-out run hands its output back as bytes, whatever `text` said
        text = lambda b: b.decode("utf-8", "replace") if isinstance(b, bytes) else (b or "")
        out, err, code = text(e.stdout), text(e.stderr) + "\n[timed out]", 124
    return out, err, code, time.time() - started


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("set", help="the TOML task set")
    ap.add_argument("--root", required=True, help="the record the memory condition attaches")
    ap.add_argument("--harness", default="claude", help="comma-separated: claude, codex, ...")
    ap.add_argument("--auth", default="subscription")
    ap.add_argument("--model", default=None)
    ap.add_argument("--provider", default=None)
    ap.add_argument("--conditions", default="memory,empty,bare")
    ap.add_argument("--timeout", type=int, default=600)
    ap.add_argument("--only", default=None, help="comma-separated task ids")
    args = ap.parse_args()

    with open(args.set, "rb") as f:
        spec = tomllib.load(f)
    tasks = spec["task"]
    preamble = spec.get("preamble", "")
    for t in tasks:
        t["prompt"] = preamble + t["prompt"]
    if args.only:
        keep = set(args.only.split(","))
        tasks = [t for t in tasks if t["id"] in keep]
    harnesses = [h.strip() for h in args.harness.split(",") if h.strip()]
    conditions = [c.strip() for c in args.conditions.split(",") if c.strip()]
    root = Path(os.path.expanduser(args.root))
    if "memory" in conditions and not AMEM.exists():
        sys.exit("build first: make build")
    env = dict(os.environ)
    cred = root / "credentials.env"
    if cred.exists():
        for line in cred.read_text().splitlines():
            line = line.strip()
            if line and not line.startswith("#") and "=" in line:
                k, v = line.split("=", 1)
                env.setdefault(k.strip(), v.strip().strip("'\""))

    results = []
    for harness in harnesses:
        for cond in conditions:
            if cond == "bare" and harness not in BARE:
                print(f"skip: no bare form known for {harness}", file=sys.stderr)
                continue
            if cond == "isolated" and harness not in ISOLATED:
                print(f"skip: no isolated form known for {harness}", file=sys.stderr)
                continue
            for t in tasks:
                since = time.time() - 1
                if cond in ("memory", "empty"):
                    cmd = [str(AMEM), "--root", str(root), "launch", "--harness", harness, "--auth", args.auth, "--no-handoff", "--prompt", t["prompt"]]
                    if cond == "empty":
                        cmd.append("--empty-memory")
                    if args.model:
                        cmd += ["--model", args.model]
                    if args.provider:
                        cmd += ["--provider", args.provider]
                    cwd = tempfile.mkdtemp(prefix="eval-mem-")
                    out, err, code, secs = run(cmd, cwd, env, args.timeout)
                    session = None
                    m = re.search(r"session (sess-[a-z0-9]+)", err)
                    if m:
                        session = m.group(1)
                    roots = [root]
                    m = re.search(r"empty memory at (\S+)", err)
                    if m:
                        roots.append(Path(m.group(1)))
                    # the record keeps the session's transcript and takes it out of the
                    # harness's home at the session's end: read the record's copy, by session
                    homes = [str(r / "record" / "*" / ".transcripts" / f"{session}*.jsonl") for r in roots if session]
                    homes += [str(r / "state" / "sandbox" / harness / "**" / "*.jsonl") for r in roots]
                    transcript = newest(homes, since)
                    empty_root = re.search(r"empty memory at (\S+)", err)
                else:
                    empty_root = None
                    cwd = tempfile.mkdtemp(prefix=f"eval-{cond}-")
                    cmd = (ISOLATED if cond == "isolated" else BARE)[harness](t["prompt"], args.model)
                    # a bare codex speaks to the gateway through its openai provider, which
                    # reads OPENAI_BASE_URL; it still wants the model on its catalog, which a
                    # gateway's is not, so the warning is expected
                    out, err, code, secs = run(cmd, cwd, bare_env(env, harness, args.auth), args.timeout)
                    home = Path.home()
                    transcript = newest([str(home / ".claude" / "projects" / "**" / "*.jsonl"), str(home / ".codex" / "sessions" / "**" / "*.jsonl")], since)
                    session = None
                shutil.rmtree(cwd, ignore_errors=True)
                answer = out.strip()
                passed = re.search(t["expect"], answer, re.I | re.S) is not None
                usage = usage_of(transcript)
                # an empty record made for one run has served its purpose once read
                if cond == "empty" and empty_root and empty_root.group(1).startswith(tempfile.gettempdir()):
                    shutil.rmtree(empty_root.group(1), ignore_errors=True)
                r = {
                    "task": t["id"], "harness": harness, "condition": cond, "model": args.model,
                    "pass": passed, "exit": code, "seconds": round(secs, 1), "session": session,
                    "usage": usage, "answer": answer[-600:], "transcript": transcript,
                    "stderr": err.strip()[-600:] if code != 0 else "",
                }
                results.append(r)
                tok = f"{usage['input']}+{usage['cache_read']}c in, {usage['output']} out" if usage else "no transcript"
                print(f"{harness:<10} {cond:<7} {t['id']:<18} {'PASS' if passed else 'FAIL'}  {secs:5.0f}s  {tok}", flush=True)

    out_dir = HERE / "evals" / "out"
    out_dir.mkdir(parents=True, exist_ok=True)
    stamp = time.strftime("%Y%m%dT%H%M%S")
    name = Path(args.set).stem
    out_path = out_dir / f"{name}-{stamp}.json"
    out_path.write_text(json.dumps({"set": args.set, "root": str(root), "args": vars(args), "results": results}, indent=1))

    print()
    print("| harness | condition | passed | input tokens | cached | output | seconds |")
    print("|---|---|---|---|---|---|---|")
    for harness in harnesses:
        for cond in conditions:
            rs = [r for r in results if r["harness"] == harness and r["condition"] == cond]
            if not rs:
                continue
            ok = sum(1 for r in rs if r["pass"])
            us = [r["usage"] for r in rs if r["usage"]]
            tin = sum(u["input"] for u in us)
            tc = sum(u["cache_read"] for u in us)
            tout = sum(u["output"] for u in us)
            secs = sum(r["seconds"] for r in rs)
            print(f"| {harness} | {cond} | {ok}/{len(rs)} | {tin} | {tc} | {tout} | {secs:.0f} |")
    print(f"\nwritten {out_path.relative_to(HERE)}")


if __name__ == "__main__":
    main()
