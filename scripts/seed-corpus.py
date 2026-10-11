#!/usr/bin/env python3
"""Seed a memory record from Markdown, one entry per section, for a realistic demo corpus.

Usage: seed-corpus.py --root ROOT [--kind KIND] [--min-words N] PATH...

Each .md file under PATH is split on second-level headings; each section becomes an entry
titled "<file title>: <section heading>". Files under a directory named decisions become
decisions; everything else the kind given (default fact). Sections shorter than --min-words
are folded into the previous one. Writes go through the memory CLI, so they land at raw
trust under the owner, like any other write.
"""
import argparse, os, re, subprocess, sys

# a directory's index and the form new files are written from: neither is knowledge
SKIP = {"README.MD", "TEMPLATE.MD"}

def sections(text):
    text = re.sub(r"^---\n.*?\n---\n", "", text, count=1, flags=re.S)  # frontmatter
    title = None
    for line in text.splitlines():
        if line.startswith("# "):
            title = line[2:].strip(); break
    parts = re.split(r"^## ", text, flags=re.M)
    out = []
    for i, p in enumerate(parts):
        if i == 0:
            body = re.sub(r"^# .*\n", "", p, count=1).strip()
            if body: out.append((None, body))
        else:
            head, _, body = p.partition("\n")
            out.append((head.strip(), body.strip()))
    return title, out

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", required=True); ap.add_argument("--kind", default="fact")
    ap.add_argument("--min-words", type=int, default=40); ap.add_argument("--memory", default="memory")
    ap.add_argument("paths", nargs="+")
    a = ap.parse_args()
    files = []
    for p in a.paths:
        if os.path.isdir(p):
            for d, _, fs in os.walk(p):
                files += [os.path.join(d, f) for f in sorted(fs) if f.endswith(".md") and f.upper() not in SKIP]
        elif p.endswith(".md"): files.append(p)
    written = 0
    for f in files:
        kind = "decision" if "/decisions/" in f or f.startswith("decisions/") else a.kind
        title, secs = sections(open(f, encoding="utf-8").read())
        title = title or os.path.splitext(os.path.basename(f))[0]
        merged = []
        for head, body in secs:
            if merged and len(body.split()) < a.min_words:
                h, b = merged[-1]; merged[-1] = (h, b + "\n\n" + (f"### {head}\n\n" if head else "") + body)
            else:
                merged.append((head, body))
        for head, body in merged:
            if len(body.split()) < 8: continue
            t = f"{title}: {head}" if head else title
            r = subprocess.run([a.memory, "--root", a.root, "write", f"--kind={kind}", f"--title={t[:140]}",
                                f"--body={body[:12000]}"], capture_output=True, text=True)
            if r.returncode != 0:
                print(f"skip {f} [{head}]: {r.stderr.strip()[:120]}", file=sys.stderr); continue
            written += 1
    print(f"wrote {written} entries from {len(files)} files")

if __name__ == "__main__":
    main()
