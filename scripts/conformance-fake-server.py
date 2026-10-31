#!/usr/bin/env python3
"""A second implementation of the memory surface, in memory, for conformance.test.sh.

It serves MCP on stdin and stdout, keeps its record in the JSON file FAKE_STATE names so
a fresh session sees what the last one wrote, and takes the operator's three verbs as
`operator export <dir>`, `operator import <dir>`, `operator reindex`, plus `operator
drop-index`. FLAW names one defect to switch on, each the kind of drift a serving layer
could have, so the test can show which step of conformance.py catches it:

    missing-tool        heartbeat is not listed
    write-ignores-label a write lands at the domain's default label, whatever was asked
    t0-forgets          T0 names no handoffs
    search-everything   search returns every entry whatever the query
    search-bodies       search at summary depth carries bodies
    second-claim        claim_next hands out a task somebody holds
    lease-forever       a lapsed lease is still renewed, and the task not returned
    heartbeat-noop      heartbeat answers but does not extend the lease
    heartbeat-lies      heartbeat reports a renewed lease_until but does not extend the lease
    heartbeat-short     heartbeat renews for half of what was asked
    heartbeat-lies-short heartbeat reports what was asked but renews for a third of it
    release-lies        release answers admitted and leaves the task claimed
    complete-anyone     complete works on a task nobody holds
    no-holder           a claim records no holder
    import-trusts       import does not check digests
    highest-wrong       the manifest's highest_label is below its lines
    reindex-forgets     reindex drops the ledger
    silent              nothing is ever answered

Not part of the product: it exists to be imperfect on request.
"""

import hashlib
import json
import os
import sys
import tempfile
import time

FLAW = os.environ.get("FLAW", "")
STATE = os.environ.get("FAKE_STATE") or os.path.join(tempfile.gettempdir(), "conformance-fake-state.json")
DOMAIN = "dom-fake"
LABELS = ["public", "internal", "restricted"]

REQUIRED = {
    "search": ["query"],
    "get": ["id"],
    "write": ["body"],
    "list": [],
    "recall": ["question"],
    "t0": [],
    "create_task": ["title", "body"],
    "fan_out": ["parent", "items"],
    "task_tree": ["id"],
    "retry_task": ["id"],
    "admit_task": ["id"],
    "claim_next": [],
    "claim": ["id"],
    "heartbeat": ["id"],
    "complete": ["id", "handoff"],
    "release": ["id"],
    "invalidate_task": ["id", "reason"],
    "get_task": ["id"],
    "list_tasks": [],
}


def load():
    try:
        with open(STATE, encoding="utf-8") as f:
            return json.load(f)
    except (OSError, ValueError):
        return {"entries": [], "tasks": [], "seq": 0, "index_lost": False}


def save(state):
    tmp = STATE + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(state, f)
    os.replace(tmp, STATE)


class Refused(Exception):
    pass


def render(e):
    """An entry as a bundle file: frontmatter, then the body."""
    front = "\n".join(
        [
            "---",
            f"id: {e['id']}",
            f"domain: {e['domain']}",
            f"type: {e['type']}",
            f"author: {e['author']}",
            f"created: {e['created']}",
            "trust_tier: raw",
            f"classification: {e['classification']}",
            f"title: {e['title']}",
            f"summary: {e['summary']}",
            "---",
        ]
    )
    return front + "\n\n" + e["body"]


def digest(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def lease_for(t, seconds, now):
    """Hold `t` for `seconds` from `now` on the monotonic clock; `lease_until` is the
    same moment on the wall clock, for the caller to read."""
    t["_deadline"] = now + seconds
    t["lease_until"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(time.time() + seconds))


class Fake:
    def __init__(self):
        self.state = load()

    # ---- entries
    def entries(self, kind=None):
        es = [e for e in self.state["entries"] if kind is None or e["type"] == kind]
        return sorted(es, key=lambda e: e["created"], reverse=True)

    def hit(self, e, body=False):
        h = {k: e[k] for k in ("id", "domain", "type", "title", "summary", "classification")}
        h["trust_tier"] = "raw"
        if body or FLAW == "search-bodies":
            h["body"] = e["body"]
        return h

    def search(self, query, depth=None, limit=10, any_word=False):
        if self.state.get("index_lost"):
            return []
        words = query.lower().split()
        need = any if any_word else all

        def score(e):
            text = (e["title"] + " " + e["summary"]).lower()
            if not need(w in text or w in e["body"].lower() for w in words):
                return None
            return sum(text.count(w) for w in words) * 2 + sum(e["body"].lower().count(w) for w in words)

        scored = [(score(e), e) for e in self.state["entries"]]
        if FLAW == "search-everything":
            scored = [(1, e) for e in self.state["entries"]]
        scored = [(s, e) for s, e in scored if s is not None]
        scored.sort(key=lambda p: -p[0])
        return [self.hit(e, depth == "sections") for _, e in scored[:limit]]

    def t0(self):
        out = [f"# T0 for {DOMAIN}\n\n## Now\n\n"]
        hands = [] if FLAW == "t0-forgets" else self.entries("handoff")
        if not hands:
            out.append("No open handoffs. Start from the ledger.\n")
        for e in hands[:5]:
            out.append(f"- `{e['id']}` {e['title']}: {e['summary']} (raw)\n")
        out.append("\n## Standing decisions (full text via get)\n\n")
        for e in self.entries("decision")[:10]:
            out.append(f"- `{e['id']}` {e['title']}: {e['summary']} (raw)\n")
        out.append("\n## Pointers\n\n- when you stop, write a `handoff` entry\n")
        return "".join(out)

    def write(self, a):
        if "body" not in a:
            raise Refused("body is required")
        self.state["seq"] += 1
        body = a["body"]
        first = next((l for l in body.splitlines() if l.strip()), "")
        e = {
            "id": f"mem-fake-{self.state['seq']:04d}",
            "domain": a.get("domain") or DOMAIN,
            "type": a.get("entry_type") or "fact",
            "title": a.get("title") or first.lstrip("# "),
            "summary": a.get("summary") or first,
            "body": body,
            "author": "human:fake:owner",
            "created": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()) + f"{self.state['seq']:04d}",
            "classification": "internal" if FLAW == "write-ignores-label" else a.get("classification") or "internal",
        }
        self.state["entries"].append(e)
        return {"id": e["id"], "domain": e["domain"], "classification": e["classification"]}

    def get(self, eid):
        for e in self.state["entries"]:
            if e["id"] == eid:
                g = dict(e)
                g["trust_tier"] = "raw"
                return g
        raise Refused(f"{eid} not found")

    # ---- tasks
    def task(self, tid):
        for t in self.state["tasks"]:
            if t["id"] == tid:
                return t
        raise Refused(f"task {tid} not found")

    def claimable(self, t, now):
        if t["status"] == "admitted":
            return True
        if t["status"] == "claimed" and FLAW == "second-claim":
            return True
        if t["status"] == "claimed" and FLAW != "lease-forever":
            return t["_deadline"] < now
        return False

    def take(self, t, lease, now):
        t["status"] = "claimed"
        t["holder"] = None if FLAW == "no-holder" else "human:fake:owner"
        lease_for(t, lease, now)
        t["attempts"] += 1
        return {"task": t, "lease_until": t["lease_until"]}

    def tool(self, name, a):
        # leases are timed on the monotonic clock: a wall clock that steps back (WSL2's
        # does when it resyncs) would otherwise keep a lapsed lease held
        now = time.monotonic()
        if name == "search":
            return {"hits": self.search(a["query"], a.get("depth"), a.get("limit") or 10)}
        if name == "get":
            return self.get(a["id"])
        if name == "write":
            return self.write(a)
        if name == "list":
            return {"entries": [self.hit(e) for e in self.entries(a.get("kind"))[: a.get("limit") or 10]]}
        if name == "recall":
            hits = self.search(a["question"], "sections", a.get("limit") or 8, any_word=True)
            return {
                "synthesis": {"by": "owed"},
                "pack": {"depth": "sections", "pieces": hits, "tokens": 0},
                "citations": [h["id"] for h in hits],
                "hops": 1,
            }
        if name == "t0":
            return self.t0()
        if name == "create_task":
            self.state["seq"] += 1
            t = {
                "id": f"task-fake-{self.state['seq']:04d}",
                "spec": {"title": a["title"], "body": a["body"], "domain": a.get("domain") or DOMAIN,
                         "priority": a.get("priority") or 0},
                "status": "proposed",
                "holder": None,
                "lease_until": None,
                "_deadline": None,
                "attempts": 0,
                "handoff": None,
            }
            self.state["tasks"].append(t)
            return {"task": t, "deduplicated": False}
        if name == "admit_task":
            t = self.task(a["id"])
            if t["status"] != "proposed":
                raise Refused(f"task is {t['status']}, not proposed")
            t["status"] = "admitted"
            return {"task": t}
        if name == "claim_next":
            lease = a.get("lease_seconds") or 1800
            open_tasks = [t for t in self.state["tasks"] if self.claimable(t, now)]
            open_tasks.sort(key=lambda t: -t["spec"]["priority"])
            if not open_tasks:
                return {"task": None}
            return self.take(open_tasks[0], lease, now)
        if name == "claim":
            t = self.task(a["id"])
            if not self.claimable(t, now):
                raise Refused(f"task is {t['status']} and not claimable")
            return self.take(t, a.get("lease_seconds") or 1800, now)
        if name == "heartbeat":
            t = self.task(a["id"])
            if t["status"] != "claimed" or (t["_deadline"] < now and FLAW != "lease-forever"):
                raise Refused("the lease is not held: it expired or was never yours")
            if FLAW == "heartbeat-lies":
                seconds = a.get("lease_seconds") or 1800
                t["lease_until"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(time.time() + seconds))
            elif FLAW == "heartbeat-lies-short":
                seconds = a.get("lease_seconds") or 1800
                lease_for(t, seconds / 3, now)
                t["lease_until"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(time.time() + seconds))
            elif FLAW == "heartbeat-short":
                lease_for(t, (a.get("lease_seconds") or 1800) * 0.55, now)
            elif FLAW != "heartbeat-noop":
                lease_for(t, a.get("lease_seconds") or 1800, now)
            return {"task": t}
        if name == "complete":
            t = self.task(a["id"])
            self.get(a["handoff"])
            held = t["status"] == "claimed" and t["_deadline"] >= now
            if not held and FLAW != "complete-anyone":
                raise Refused(f"task is {t['status']} and the lease is not held")
            t["status"] = "completed"
            t["handoff"] = a["handoff"]
            return {"task": t}
        if name == "release":
            t = self.task(a["id"])
            if a.get("handoff"):
                self.get(a["handoff"])
                t["handoff"] = a["handoff"]
            if FLAW == "release-lies":
                return {"task": dict(t, status="admitted")}
            t["status"] = "admitted"
            t["holder"] = None
            t["lease_until"] = None
            t["_deadline"] = None
            return {"task": t}
        if name == "invalidate_task":
            t = self.task(a["id"])
            t["status"] = "invalidated"
            t["reason"] = a["reason"]
            return {"task": t}
        if name == "get_task":
            return {"task": self.task(a["id"])}
        if name == "list_tasks":
            status = a.get("status") or "admitted"
            return {"tasks": [t for t in self.state["tasks"] if t["status"] == status]}
        if name in ("fan_out", "task_tree", "retry_task"):
            raise Refused(f"{name} is not implemented by the fake")
        raise Refused(f"no tool {name}")

    # ---- the protocol
    def handle(self, msg):
        method = msg.get("method")
        params = msg.get("params") or {}
        if method == "initialize":
            return {
                "protocolVersion": params.get("protocolVersion", "2025-06-18"),
                "capabilities": {"tools": {}, "resources": {}},
                "serverInfo": {"name": "conformance-fake", "version": "0"},
            }
        if method == "tools/list":
            tools = []
            for name, req in REQUIRED.items():
                if FLAW == "missing-tool" and name == "heartbeat":
                    continue
                props = {k: {"type": "string"} for k in req}
                tools.append({
                    "name": name,
                    "description": f"The fake's {name}.",
                    "inputSchema": {"type": "object", "properties": props, "required": req},
                })
            return {"tools": tools}
        if method == "resources/list":
            return {"resources": [{"uri": f"memory://t0/{DOMAIN}", "name": f"T0 for {DOMAIN}", "mimeType": "text/markdown"}]}
        if method == "resources/read":
            uri = params.get("uri", "")
            if uri != f"memory://t0/{DOMAIN}":
                raise Refused(f"no resource {uri}")
            return {"contents": [{"uri": uri, "mimeType": "text/markdown", "text": self.t0()}]}
        if method == "tools/call":
            try:
                payload = self.tool(params.get("name"), params.get("arguments") or {})
            except Refused as e:
                return {"content": [{"type": "text", "text": str(e)}], "isError": True}
            save(self.state)
            text = payload if isinstance(payload, str) else json.dumps(payload)
            return {"content": [{"type": "text", "text": text}], "isError": False}
        if method == "ping":
            return {}
        raise Refused(f"no method {method}")

    def serve(self):
        for line in sys.stdin:
            line = line.strip()
            if not line:
                continue
            msg = json.loads(line)
            if "id" not in msg:
                continue
            if FLAW == "silent":
                continue
            try:
                reply = {"jsonrpc": "2.0", "id": msg["id"], "result": self.handle(msg)}
            except Refused as e:
                reply = {"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32601, "message": str(e)}}
            sys.stdout.write(json.dumps(reply) + "\n")
            sys.stdout.flush()

    # ---- the operator's verbs
    def export(self, dest):
        os.makedirs(dest, exist_ok=True)
        lines = []
        highest = "public"
        for e in self.state["entries"]:
            if e["classification"] != "public":
                continue
            text = render(e)
            path = f"{e['id']}.md"
            with open(os.path.join(dest, path), "w", encoding="utf-8") as f:
                f.write(text)
            lines.append((e["id"], path, digest(text), e["classification"]))
            if LABELS.index(e["classification"]) > LABELS.index(highest):
                highest = e["classification"]
        if FLAW == "highest-wrong":
            highest = "public" if highest != "public" else "internal"
        out = [
            "format: durable-memory/bundle/0",
            f"domain: {DOMAIN}",
            "exported_at: 2026-10-08T00:00:00Z",
            "exported_by: human:fake:owner",
            "max_label: public",
            f"highest_label: {highest}",
            "entries:",
        ]
        for eid, path, dig, label in lines:
            out += [f"- id: {eid}", f"  path: {path}", f"  digest: {dig}", f"  classification: {label}"]
        with open(os.path.join(dest, "manifest.yaml"), "w", encoding="utf-8") as f:
            f.write("\n".join(out) + "\n")

    def import_(self, src):
        with open(os.path.join(src, "manifest.yaml"), encoding="utf-8") as f:
            lines = [l for l in f.read().splitlines() if l.startswith("- id:") or l.startswith("  ")]
        items = []
        for l in lines:
            if l.startswith("- id:"):
                items.append({"id": l[5:].strip()})
            else:
                k, _, v = l.strip().partition(":")
                items[-1][k] = v.strip()
        known = {e["id"] for e in self.state["entries"]}
        for m in items:
            with open(os.path.join(src, m["path"]), encoding="utf-8") as f:
                text = f.read()
            if digest(text) != m["digest"] and FLAW != "import-trusts":
                sys.exit(f"{m['id']} does not match its manifest digest")
            if m["id"] in known:
                continue
            front, _, body = text.partition("\n\n")
            fields = dict(l.partition(": ")[::2] for l in front.splitlines() if ": " in l)
            self.state["entries"].append({
                "id": m["id"], "domain": DOMAIN, "type": fields.get("type", "fact"),
                "title": fields.get("title", ""), "summary": fields.get("summary", ""),
                "body": body, "author": fields.get("author", "?"), "created": fields.get("created", ""),
                "classification": m["classification"],
            })
        save(self.state)

    def operator(self, argv):
        verb = argv[0] if argv else ""
        if verb == "export":
            self.export(argv[1])
        elif verb == "import":
            self.import_(argv[1])
        elif verb == "reindex":
            self.state["index_lost"] = False
            if FLAW == "reindex-forgets":
                self.state["tasks"] = []
            save(self.state)
        elif verb == "drop-index":
            self.state["index_lost"] = True
            save(self.state)
        else:
            sys.exit(f"operator: unknown verb {verb!r}")


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "operator":
        Fake().operator(sys.argv[2:])
    else:
        Fake().serve()
