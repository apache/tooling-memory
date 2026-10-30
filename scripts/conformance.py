#!/usr/bin/env python3
"""The seven M0 steps against any server exposing the memory surface.

A harness that works against the laptop server works against any serving layer, if the
layer exposes the same surface: the same tools, taking the same arguments, answering in
the same shapes, with the same meaning. This script is that claim made checkable. It
speaks MCP and nothing else, so it says nothing about how the server is built, and it
writes into the record it is pointed at, so point it at a disposable one.

    scripts/conformance.py --stdio 'amem --root /tmp/conf serve'
    scripts/conformance.py --url http://127.0.0.1:8080/mcp

Steps 1 to 5 run over the surface alone: the handshake, the tools and the T0 resource; a
session writing a decision and a handoff; a fresh session loading T0 and finding the
handoff; search ranking summaries and get returning one body; the task fabric, with a
lease held, renewed, lost at expiry and reclaimed. Steps 6 and 7 are the operator's,
export and import and the rebuild from the record, and run when --operator names a
command that administers the server:

    --operator 'amem --root /tmp/conf'

The command is run with `export <dir>`, `import <dir>` and `reindex` appended, and its
exit status is its only word: the effects are checked on the bundle it wrote and over the
surface. A serving layer with other tools wraps them in a script that takes those three
verbs. --drop-index names a command that loses the derived state, so step 7 can show the
rebuild rather than take it on trust; without it, step 7 shows that reindex keeps the
surface's answers. A stdio server opens its state per session, so the loss shows at the
next one; a server that stays up sees it only if --drop-index restarts it.

Exit status 0 when every step that ran passed, 1 on a failure, 2 on a usage error. Only
the standard library is used.
"""

import argparse
import hashlib
import json
import os
import queue
import secrets
import shlex
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

PROTOCOL = "2025-06-18"

# The tools the seven steps drive, with the arguments the surface requires of each. A
# server that requires more breaks a harness that follows the surface; one that requires
# less has a tool with another meaning.
DRIVEN = {
    "search": ["query"],
    "get": ["id"],
    "write": ["body"],
    "list": [],
    "recall": ["question"],
    "t0": [],
    "create_task": ["title", "body"],
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
# The rest of the surface, listed but not driven here.
NOT_DRIVEN = ["fan_out", "task_tree", "retry_task"]

BUNDLE_FORMAT = "durable-memory/bundle/0"
LABELS = ["public", "internal", "restricted"]


class Fail(Exception):
    """A step's check that did not hold."""


class NotRun(Exception):
    """A step that could not run, and why. One left out by choice, the operator steps
    with no --operator, is `optional`; any other is a run that could not finish."""

    def __init__(self, why, optional=False):
        super().__init__(why)
        self.optional = optional


class ToolError(Exception):
    """A tool call the server refused, in the server's words."""


def check(cond, what):
    if not cond:
        raise Fail(what)


# ---- transports -------------------------------------------------------------------------


class Session:
    """One MCP session: the handshake done, requests numbered, answers matched by id."""

    def __init__(self, timeout):
        self.timeout = timeout
        self.next_id = 0
        self.info = None

    def _send(self, message):
        raise NotImplementedError

    def request(self, method, params=None):
        self.next_id += 1
        rid = self.next_id
        msg = {"jsonrpc": "2.0", "id": rid, "method": method}
        if params is not None:
            msg["params"] = params
        reply = self._send(msg)
        if reply is None:
            raise Fail(f"{method}: no reply within {self.timeout}s")
        if reply.get("id") != rid:
            raise Fail(f"{method}: reply carries id {reply.get('id')!r}, not {rid}")
        if "error" in reply:
            err = reply["error"]
            raise ToolError(f"{err.get('message', 'error')} ({err.get('code')})")
        return reply.get("result")

    def notify(self, method, params=None):
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        self._send(msg)

    def open(self):
        self.info = self.request(
            "initialize",
            {
                "protocolVersion": PROTOCOL,
                "capabilities": {},
                "clientInfo": {"name": "conformance", "version": "0"},
            },
        )
        check(isinstance(self.info, dict), "initialize: the result is not an object")
        self.notify("notifications/initialized")
        return self

    def tools(self):
        return self.request("tools/list", {}).get("tools", [])

    def resources(self):
        return self.request("resources/list", {}).get("resources", [])

    def read_resource(self, uri):
        return self.request("resources/read", {"uri": uri}).get("contents", [])

    def call(self, name, **arguments):
        """A tool's payload: its JSON when it answers JSON, else its text."""
        result = self.request("tools/call", {"name": name, "arguments": arguments})
        check(isinstance(result, dict), f"{name}: the result is not an object")
        blocks = result.get("content") or []
        texts = [b.get("text", "") for b in blocks if b.get("type") == "text"]
        if result.get("isError"):
            raise ToolError(texts[0] if texts else "refused without a word")
        if "structuredContent" in result:
            return result["structuredContent"]
        check(texts, f"{name}: no text block in the answer")
        try:
            return json.loads(texts[0])
        except ValueError:
            return texts[0]

    def refused(self, name, **arguments):
        """The server's words for a call it must refuse; a Fail when it did not."""
        try:
            got = self.call(name, **arguments)
        except ToolError as e:
            return str(e)
        raise Fail(f"{name} {json.dumps(arguments)} was answered, not refused: {short(got)}")

    def close(self):
        pass


class Stdio(Session):
    """A server spawned per session, spoken to on its standard input and output."""

    def __init__(self, command, timeout):
        super().__init__(timeout)
        self.argv = shlex.split(command)
        try:
            self.proc = subprocess.Popen(
                self.argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True
            )
        except OSError as e:
            raise NotRun(f"cannot start {command!r}: {e}") from None
        self.lines = queue.Queue()
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self):
        for line in self.proc.stdout:
            self.lines.put(line)
        self.lines.put(None)

    def _send(self, message):
        try:
            self.proc.stdin.write(json.dumps(message) + "\n")
            self.proc.stdin.flush()
        except (BrokenPipeError, OSError):
            raise Fail(f"the server went away (exit {self.proc.poll()})") from None
        if "id" not in message:
            return None
        deadline = time.monotonic() + self.timeout
        while True:
            left = deadline - time.monotonic()
            if left <= 0:
                return None
            try:
                line = self.lines.get(timeout=left)
            except queue.Empty:
                return None
            if line is None:
                raise Fail(f"the server closed its output (exit {self.proc.poll()})")
            try:
                reply = json.loads(line)
            except ValueError:
                raise Fail(f"not JSON on the server's output: {line.rstrip()!r}") from None
            # a request or notification of the server's own carries a method, and may
            # carry an id of its own numbering; only a reply to what was asked is one
            if isinstance(reply, dict) and "method" not in reply and reply.get("id") == message["id"]:
                return reply

    def close(self):
        try:
            self.proc.stdin.close()
        except OSError:
            pass
        try:
            self.proc.wait(timeout=self.timeout)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()


class Http(Session):
    """MCP's streamable HTTP transport: every message a POST, the session in a header."""

    def __init__(self, url, timeout):
        super().__init__(timeout)
        self.url = url
        self.session_id = None

    def _send(self, message):
        body = json.dumps(message).encode()
        headers = {
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
        }
        if self.session_id:
            headers["mcp-session-id"] = self.session_id
        req = urllib.request.Request(self.url, data=body, headers=headers, method="POST")
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                sid = resp.headers.get("mcp-session-id")
                if sid:
                    self.session_id = sid
                ctype = resp.headers.get("content-type", "")
                text = resp.read().decode("utf-8", "replace")
        except urllib.error.HTTPError as e:
            text = e.read().decode("utf-8", "replace")
            raise Fail(f"{message['method']}: HTTP {e.code}: {text[:200]}") from None
        except (urllib.error.URLError, OSError) as e:
            raise NotRun(f"{self.url}: {e}") from None
        if "id" not in message:
            return None
        if "text/event-stream" in ctype:
            replies = []
            for event in text.replace("\r\n", "\n").split("\n\n"):
                data = "\n".join(l[5:].strip() for l in event.split("\n") if l.startswith("data:"))
                if data:
                    try:
                        replies.append(json.loads(data))
                    except ValueError:
                        pass
        else:
            try:
                replies = [json.loads(text)] if text.strip() else []
            except ValueError:
                raise Fail(f"{message['method']}: not JSON: {text[:200]!r}") from None
        for reply in replies:
            if isinstance(reply, dict) and "method" not in reply and reply.get("id") == message["id"]:
                return reply
        return None

    def close(self):
        if not self.session_id:
            return
        req = urllib.request.Request(
            self.url, headers={"mcp-session-id": self.session_id}, method="DELETE"
        )
        try:
            urllib.request.urlopen(req, timeout=self.timeout).close()
        except (urllib.error.URLError, OSError):
            pass


# ---- helpers ----------------------------------------------------------------------------


def short(value, n=160):
    text = value if isinstance(value, str) else json.dumps(value)
    return text if len(text) <= n else text[: n - 3] + "..."


def ids(hits):
    return [h.get("id") for h in hits]


def task_of(payload, what):
    check(isinstance(payload, dict) and "task" in payload, f"{what}: no `task` in {short(payload)}")
    return payload["task"]


def parse_manifest(text):
    """The subset of YAML a manifest is written in: scalars at the top, and under
    `entries`, a list of maps. The format is the contract, so the shape is known."""
    top = {}
    entries = []
    in_entries = False
    for raw in text.splitlines():
        if not raw.strip() or raw.lstrip().startswith("#"):
            continue
        if not raw.startswith(" ") and not raw.startswith("-"):
            key, _, value = raw.partition(":")
            in_entries = key.strip() == "entries"
            if not in_entries:
                top[key.strip()] = value.strip().strip("'\"")
            continue
        if not in_entries:
            continue
        line = raw.lstrip()
        if line.startswith("- "):
            entries.append({})
            line = line[2:]
        key, _, value = line.partition(":")
        check(entries, f"manifest: a field before the first entry: {raw!r}")
        entries[-1][key.strip()] = value.strip().strip("'\"")
    return top, entries


def plain_name(path):
    """A file name and nothing more: no separator, not hidden, as import reads one."""
    return bool(path) and "/" not in path and "\\" not in path and not path.startswith(".")


def frontmatter(text):
    """The frontmatter block of an entry, between its two `---` lines."""
    if not text.startswith("---\n"):
        return ""
    end = text.find("\n---", 4)
    return text[4:end] if end != -1 else ""


def when(stamp):
    """An RFC 3339 time as seconds since the epoch, fractions and offsets allowed."""
    import datetime

    try:
        return datetime.datetime.fromisoformat(stamp.replace("Z", "+00:00")).timestamp()
    except (ValueError, AttributeError):
        return None


def run_operator(prefix, args, what, timeout):
    argv = shlex.split(prefix) + args
    try:
        done = subprocess.run(argv, capture_output=True, text=True, check=False, timeout=timeout)
    except OSError as e:
        raise NotRun(f"cannot run {prefix!r}: {e}") from None
    except subprocess.TimeoutExpired:
        raise Fail(f"{what}: {' '.join(argv)} did not finish within {timeout:g}s") from None
    return done


# ---- the steps --------------------------------------------------------------------------


class Run:
    def __init__(self, opts):
        self.opts = opts
        self.state = {}
        tag = secrets.token_hex(3)
        # one word only this run wrote, one both entries carry: the first ranks, the
        # second lists
        self.only = f"conf{tag}x"
        self.both = f"conf{tag}y"
        self.checks = 0

    def session(self):
        if self.opts.stdio:
            return Stdio(self.opts.stdio, self.opts.timeout).open()
        return Http(self.opts.url, self.opts.timeout).open()

    def check(self, cond, what):
        self.checks += 1
        check(cond, what)

    def need(self, *keys):
        for k in keys:
            if k not in self.state:
                raise NotRun(f"needs {k}, which an earlier step did not produce")

    # 1
    def step_server(self):
        s = self.session()
        try:
            info = s.info
            self.check(
                isinstance(info.get("serverInfo"), dict) and info["serverInfo"].get("name"),
                f"initialize: no serverInfo.name in {short(info)}",
            )
            self.check(
                info.get("protocolVersion"),
                f"initialize: no protocolVersion in {short(info)}",
            )
            caps = info.get("capabilities") or {}
            self.check("tools" in caps, f"initialize: the server does not declare tools: {short(caps)}")
            self.check(
                "resources" in caps,
                f"initialize: the server does not declare resources: {short(caps)}",
            )
            resources = s.resources()
            t0s = [r for r in resources if str(r.get("uri", "")).startswith("memory://t0/")]
            self.check(t0s, f"resources/list names no memory://t0/<domain>: {short(resources)}")
            domain = self.opts.domain or t0s[0]["uri"][len("memory://t0/") :]
            self.check(
                any(r["uri"] == f"memory://t0/{domain}" for r in t0s),
                f"no T0 resource for domain {domain}",
            )
            contents = s.read_resource(f"memory://t0/{domain}")
            self.check(
                contents and isinstance(contents[0].get("text"), str),
                f"resources/read of T0 returned no text: {short(contents)}",
            )
            self.state["domain"] = domain
            tools = {t.get("name"): t for t in s.tools()}
            missing = [n for n in DRIVEN if n not in tools]
            self.check(not missing, f"tools/list lacks {', '.join(missing)}")
            for name, required in DRIVEN.items():
                schema = tools[name].get("inputSchema") or {}
                got = sorted(schema.get("required") or [])
                self.check(
                    got == sorted(required),
                    f"{name} requires {got}, the surface requires {sorted(required)}",
                )
                self.check(tools[name].get("description"), f"{name} has no description")
            quiet = [n for n in NOT_DRIVEN if n not in tools]
            if quiet:
                print(f"   note: not listed, not driven here: {', '.join(quiet)}")
            print(f"   domain {domain}, server {info['serverInfo'].get('name')} {info['serverInfo'].get('version', '')}".rstrip())
        finally:
            s.close()

    # 2
    def step_write(self):
        self.need("domain")
        d = self.state["domain"]
        s = self.session()
        try:
            self.check(
                "body" in s.refused("write", title="No body"),
                "write without a body was refused without naming the body",
            )
            dec = s.call(
                "write",
                entry_type="decision",
                title=f"Use the pilot store {self.both}",
                body=f"Nothing to operate, and the index is built in. Marker {self.both}.\n",
                domain=d,
                classification="public",
            )
            self.check(isinstance(dec, dict) and dec.get("id"), f"write returned no id: {short(dec)}")
            self.check(dec.get("domain") == d, f"write landed in {dec.get('domain')!r}, not {d}")
            self.check(
                dec.get("classification") == "public",
                f"write at public came back {dec.get('classification')!r}",
            )
            hand = s.call(
                "write",
                entry_type="handoff",
                title=f"Stopped at step 3 of the import {self.both}",
                body=f"Steps 1 and 2 done; the manifest parses. Next: digests {self.only}.\n",
                domain=d,
                classification="public",
            )
            self.check(isinstance(hand, dict) and hand.get("id"), f"write returned no id: {short(hand)}")
            self.check(hand["id"] != dec["id"], "two writes share an id")
            got = s.call("get", id=dec["id"])
            self.check(got.get("type") == "decision", f"get: type {got.get('type')!r}, not decision")
            self.check(got.get("trust_tier") == "raw", f"get: a fresh write is {got.get('trust_tier')!r}, not raw")
            self.check(self.both in got.get("body", ""), "get: the body is not what was written")
            self.check(got.get("author"), "get: no author on the entry")
            self.state["decision"] = dec["id"]
            self.state["handoff"] = hand["id"]
            print(f"   decision {dec['id']}, handoff {hand['id']}")
        finally:
            s.close()

    # 3
    def step_t0(self):
        self.need("domain", "handoff", "decision")
        d, h, dec = self.state["domain"], self.state["handoff"], self.state["decision"]
        s = self.session()  # a fresh session: no transcript, nothing carried over
        try:
            t0 = s.call("t0", domain=d)
            self.check(isinstance(t0, str), f"t0 is not text: {short(t0)}")
            self.check(h in t0, f"T0 does not name the handoff {h}")
            self.check(self.only in t0, "T0 names the handoff without its summary")
            self.check(dec in t0, f"T0 does not name the decision {dec}")
            self.check(
                t0.index(h) < t0.index(dec) if h in t0 and dec in t0 else False,
                "T0 lists the standing decision before the latest handoff",
            )
            res = s.read_resource(f"memory://t0/{d}")
            self.check(
                res and res[0].get("text") == t0,
                "the T0 resource and the t0 tool disagree",
            )
        finally:
            s.close()

    # 4
    def step_search(self):
        self.need("domain", "handoff", "decision")
        d, h, dec = self.state["domain"], self.state["handoff"], self.state["decision"]
        s = self.session()
        try:
            r = s.call("search", query=self.only, domain=d)
            hits = r.get("hits") if isinstance(r, dict) else None
            self.check(isinstance(hits, list), f"search: no `hits` list in {short(r)}")
            self.check(ids(hits) == [h], f"search for a word only the handoff carries found {ids(hits)}")
            hit = hits[0]
            for key in ("id", "type", "title", "summary", "trust_tier", "classification"):
                self.check(key in hit, f"search: a hit without `{key}`: {short(hit)}")
            self.check("body" not in hit, "search at summary depth returned a body")
            self.check(self.only in hit["summary"], "search: the summary is not the entry's")
            self.check(hit["type"] == "handoff", f"search: type {hit['type']!r}")
            r = s.call("search", query=self.both, domain=d)
            self.check(
                sorted(ids(r.get("hits", []))) == sorted([h, dec]),
                f"search for a word both carry found {ids(r.get('hits', []))}",
            )
            r = s.call("search", query=self.both, domain=d, limit=1)
            self.check(len(r.get("hits", [])) == 1, "search: limit 1 returned more than one")
            r = s.call("search", query=self.only, domain=d, depth="sections")
            hits = r.get("hits", [])
            self.check(
                hits and self.only in (hits[0].get("body") or ""),
                f"search at sections depth carries no body: {short(r)}",
            )
            got = s.call("get", id=h)
            self.check(got.get("id") == h, f"get returned {got.get('id')!r}")
            self.check(self.only in got.get("body", ""), "get: the body is not the handoff's")
            self.check(got.get("type") == "handoff", f"get: type {got.get('type')!r}")
            s.refused("get", id=f"{h}-nope")
            r = s.call("list", domain=d, kind="handoff")
            entries = r.get("entries") if isinstance(r, dict) else None
            self.check(isinstance(entries, list), f"list: no `entries` list in {short(r)}")
            self.check(h in ids(entries), "list of handoffs lacks the handoff")
            self.check(all(e.get("type") == "handoff" for e in entries), "list of handoffs carries another kind")
            r = s.call("list", domain=d)
            order = ids(r.get("entries", []))
            self.check(
                h in order and dec in order and order.index(h) < order.index(dec),
                f"list is not newest first: {order}",
            )
            r = s.call("recall", question=f"what is next after the manifest {self.only}", domain=d)
            synth = r.get("synthesis", {}) if isinstance(r, dict) else {}
            self.check(synth.get("by") in ("owed", "server"), f"recall: synthesis.by is {synth.get('by')!r}")
            self.check(h in (r.get("citations") or []), f"recall does not cite the handoff: {short(r)}")
            if synth.get("by") == "owed":
                pieces = (r.get("pack") or {}).get("pieces") or []
                self.check(h in ids(pieces), "recall owed: the pack lacks the handoff")
            print(f"   recall synthesis {synth.get('by')}")
        finally:
            s.close()

    def claimed(self, payload, what, lease, expecting="a task"):
        """The task a claim returned, held by someone, under a lease of about `lease`."""
        t = task_of(payload, what)
        self.check(t is not None, f"{what} found nothing claimable, expecting {expecting}")
        self.check(t.get("status") == "claimed", f"{what}: a claimed task is {t.get('status')!r}")
        self.check(t.get("holder"), f"{what}: the claimed task names no holder: {short(t)}")
        until = when(str(payload.get("lease_until", "")))
        self.check(until is not None, f"{what}: lease_until {payload.get('lease_until')!r} is not a time")
        if until is not None:
            ahead = until - time.time()
            self.check(
                0 < ahead <= lease + 5,
                f"{what}: lease_until is {ahead:.0f}s ahead, for a lease of {lease}s",
            )
        return t

    # 5
    def step_tasks(self):
        self.need("domain", "handoff")
        d, h = self.state["domain"], self.state["handoff"]
        lease = self.opts.lease
        if lease < 10:
            print(f"   note: --lease {lease} is below the surface's floor of 10; a server holding the floor fails here")
        s = self.session()
        try:
            c = s.call("create_task", title=f"Finish the import {self.only}", body="The rest of it.", domain=d, priority=5)
            t = task_of(c, "create_task")
            self.check(t.get("id"), f"create_task returned no id: {short(c)}")
            self.check(c.get("deduplicated") is False, f"create_task: no `deduplicated` false in {short(c)}")
            self.check(t.get("status") == "proposed", f"a new task is {t.get('status')!r}, not proposed")
            tid = t["id"]
            s.refused("claim", id=tid, lease_seconds=lease)  # proposed: not yet claimable
            t = task_of(s.call("admit_task", id=tid), "admit_task")
            self.check(t.get("status") == "admitted", f"admit_task left it {t.get('status')!r}")
            t = self.claimed(s.call("claim_next", domain=d, lease_seconds=lease), "claim_next", lease, "the admitted task")
            self.check(
                t.get("id") == tid,
                f"claim_next returned {t.get('id')!r}, not the task filed; run against a record with no other admitted tasks",
            )
            self.check(
                task_of(s.call("claim_next", domain=d, lease_seconds=lease), "claim_next") is None,
                "a second claim_next while the lease holds returned a task",
            )
            s.refused("claim", id=tid, lease_seconds=lease)
            t = task_of(s.call("heartbeat", id=tid, lease_seconds=lease), "heartbeat")
            self.check(t.get("status") == "claimed", f"heartbeat left it {t.get('status')!r}")
            s.refused("complete", id=tid, handoff=f"{h}-nope")
            t = task_of(s.call("complete", id=tid, handoff=h), "complete")
            self.check(t.get("status") == "completed", f"complete left it {t.get('status')!r}")
            self.check(t.get("handoff") == h, f"the completed task carries handoff {t.get('handoff')!r}")
            t = task_of(s.call("get_task", id=tid), "get_task")
            self.check(t.get("status") == "completed", f"get_task says {t.get('status')!r}")
            done = s.call("list_tasks", domain=d, status="completed")
            tasks = done.get("tasks") if isinstance(done, dict) else done
            self.check(isinstance(tasks, list), f"list_tasks: no list in {short(done)}")
            self.check(tid in ids(tasks), "list_tasks completed lacks the task")
            self.state["task"] = tid
            # a second task: its lease renewed past where it would have lapsed, then
            # left to lapse, lost, returned to the pool, and given back with its handoff
            t2 = task_of(s.call("create_task", title=f"Left behind {self.only}", body="Claimed, then abandoned.", domain=d), "create_task")
            tid2 = t2["id"]
            s.call("admit_task", id=tid2)
            got = s.call("claim_next", domain=d, lease_seconds=lease)
            t = self.claimed(got, "claim_next", lease)
            first = when(str(got.get("lease_until", "")))
            self.check(t.get("id") == tid2, "the second task was not claimed")
            print(f"   holding the lease past {lease}s with one heartbeat, then letting it lapse")
            # the heartbeat renews for three leases, so each check stands clear of the ends
            # it sits between, by a quarter of a lease or more: the renewal moves
            # lease_until on by two and a half leases against the one asked for; the claim
            # check falls after the first lease lapsed and two leases before the renewed one
            # does; and the lapse check comes a lease after that. A stalled call, or a
            # server clock that steps by less than those margins, moves no end past a check
            time.sleep(lease * 0.5)
            beat = task_of(s.call("heartbeat", id=tid2, lease_seconds=3 * lease), "heartbeat")
            renewed = when(str(beat.get("lease_until", "")))
            self.check(
                first is not None and renewed is not None and renewed - first >= lease,
                f"heartbeat did not renew it: lease_until went from {first} to {renewed}",
            )
            time.sleep(lease * 0.75)
            self.check(
                task_of(s.call("claim_next", domain=d, lease_seconds=lease), "claim_next") is None,
                "a renewed lease lapsed where the first one would have: heartbeat did not renew it",
            )
            time.sleep(lease * 3.25)
            s.refused("heartbeat", id=tid2, lease_seconds=lease)
            s.refused("complete", id=tid2, handoff=h)
            t = self.claimed(s.call("claim_next", domain=d, lease_seconds=lease), "claim_next", lease, "the task whose lease lapsed")
            self.check(
                t.get("id") == tid2,
                f"an expired lease did not return the task to the pool: {short(t)}",
            )
            t = task_of(s.call("release", id=tid2, handoff=h), "release")
            self.check(t.get("status") == "admitted", f"release left it {t.get('status')!r}")
            t = self.claimed(s.call("claim_next", domain=d, lease_seconds=lease), "claim_next", lease, "the released task back from the pool")
            self.check(t.get("id") == tid2, "a released task did not come back from the pool")
            self.check(t.get("handoff") == h, f"the released task came back with handoff {t.get('handoff')!r}, not the one left")
            s.call("release", id=tid2)
            t = task_of(s.call("invalidate_task", id=tid2, reason="obsolete: conformance run"), "invalidate_task")
            self.check(t.get("status") == "invalidated", f"invalidate_task left it {t.get('status')!r}")
            self.check(
                task_of(s.call("claim_next", domain=d, lease_seconds=lease), "claim_next") is None,
                "an invalidated task was claimed",
            )
        finally:
            s.close()

    # 6
    def step_export_import(self):
        if not self.opts.operator:
            raise NotRun("give --operator to run it", optional=True)
        self.need("domain", "handoff", "decision")
        d, h, dec = self.state["domain"], self.state["handoff"], self.state["decision"]
        work = tempfile.mkdtemp(prefix="conformance-")
        try:
            bundle = os.path.join(work, "bundle")
            done = run_operator(self.opts.operator, ["export", bundle], "export", self.opts.timeout * 10)
            self.check(done.returncode == 0, f"export failed ({done.returncode}): {done.stderr.strip()[-300:]}")
            mpath = os.path.join(bundle, "manifest.yaml")
            self.check(os.path.isfile(mpath), "export wrote no manifest.yaml")
            with open(mpath, encoding="utf-8") as f:
                top, lines = parse_manifest(f.read())
            self.check(top.get("format") == BUNDLE_FORMAT, f"manifest format {top.get('format')!r}")
            self.check(top.get("domain") == d, f"manifest domain {top.get('domain')!r}, not {d}")
            for key in ("exported_at", "exported_by", "max_label", "highest_label"):
                self.check(top.get(key), f"manifest has no {key}")
            self.check(top["max_label"] in LABELS, f"manifest max_label {top['max_label']!r}")
            highest = "public"
            seen = set()
            for m in lines:
                for key in ("id", "path", "digest", "classification"):
                    self.check(m.get(key), f"a manifest line without {key}: {m}")
                self.check(m["id"] not in seen, f"manifest lists {m['id']} twice")
                seen.add(m["id"])
                self.check(
                    plain_name(m["path"]),
                    f"manifest path {m['path']!r} is not a plain file name inside the bundle",
                )
                path = os.path.join(bundle, m["path"])
                self.check(os.path.isfile(path), f"manifest names {m['path']}, which the bundle lacks")
                with open(path, "rb") as f:
                    text = f.read()
                self.check(
                    hashlib.sha256(text).hexdigest() == m["digest"],
                    f"{m['id']}: the digest is not the file's sha256",
                )
                self.check(m["classification"] in LABELS, f"{m['id']}: classification {m['classification']!r}")
                self.check(
                    LABELS.index(m["classification"]) <= LABELS.index(top["max_label"]),
                    f"{m['id']} is {m['classification']}, above max_label {top['max_label']}",
                )
                self.check(
                    f"classification: {m['classification']}" in frontmatter(text.decode("utf-8", "replace")),
                    f"{m['id']}: the manifest's classification is not the entry's",
                )
                if LABELS.index(m["classification"]) > LABELS.index(highest):
                    highest = m["classification"]
            self.check(
                top["highest_label"] == highest,
                f"manifest highest_label {top['highest_label']}, the lines say {highest}",
            )
            self.check(h in seen and dec in seen, "the bundle lacks the entries this run wrote")
            print(f"   bundle of {len(lines)} entries, highest {highest}")

            s = self.session()
            try:
                before = ids(s.call("list", domain=d, limit=100).get("entries", []))
            finally:
                s.close()
            for n in (1, 2):
                done = run_operator(self.opts.operator, ["import", bundle], "import", self.opts.timeout * 10)
                self.check(done.returncode == 0, f"import {n} failed ({done.returncode}): {done.stderr.strip()[-300:]}")
            # a tampered entry: the bundle is refused whole and the record is untouched
            forged = os.path.join(work, "forged")
            shutil.copytree(bundle, forged)
            victim = next(m for m in lines if m["id"] == h)
            with open(os.path.join(forged, victim["path"]), "a", encoding="utf-8") as f:
                f.write("\nAn addition the manifest does not know.\n")
            done = run_operator(self.opts.operator, ["import", forged], "import", self.opts.timeout * 10)
            self.check(done.returncode != 0, "import accepted an entry that does not match its digest")
            s = self.session()
            try:
                after = ids(s.call("list", domain=d, limit=100).get("entries", []))
                self.check(after == before, f"importing the record's own export changed it: {before} -> {after}")
                got = s.call("get", id=h)
                self.check("does not know" not in got.get("body", ""), "the tampered entry landed")
            finally:
                s.close()
        finally:
            shutil.rmtree(work, ignore_errors=True)

    # 7
    def step_reindex(self):
        if not self.opts.operator:
            raise NotRun("give --operator to run it", optional=True)
        self.need("domain", "handoff", "task")
        d, h, tid = self.state["domain"], self.state["handoff"], self.state["task"]
        if self.opts.drop_index:
            done = run_operator(self.opts.drop_index, [], "drop-index", self.opts.timeout * 10)
            self.check(done.returncode == 0, f"--drop-index failed ({done.returncode}): {done.stderr.strip()[-300:]}")
            s = self.session()
            try:
                found = ids(s.call("search", query=self.only, domain=d).get("hits", []))
                self.check(h not in found, "after --drop-index the handoff is still found; the index was not lost")
            finally:
                s.close()
        done = run_operator(self.opts.operator, ["reindex"], "reindex", self.opts.timeout * 10)
        self.check(done.returncode == 0, f"reindex failed ({done.returncode}): {done.stderr.strip()[-300:]}")
        s = self.session()
        try:
            found = ids(s.call("search", query=self.only, domain=d).get("hits", []))
            self.check(found == [h], f"after reindex, search finds {found}")
            t0 = s.call("t0", domain=d)
            self.check(h in t0, "after reindex, T0 lost the handoff")
            try:
                t = task_of(s.call("get_task", id=tid), "get_task")
            except ToolError as e:
                raise Fail(f"after reindex the ledger lost the completed task: {e}") from None
            self.check(t.get("status") == "completed", f"after reindex the task is {t.get('status')!r}")
            done_tasks = s.call("list_tasks", domain=d, status="completed")
            tasks = done_tasks.get("tasks") if isinstance(done_tasks, dict) else done_tasks
            self.check(tid in ids(tasks or []), "after reindex the ledger lost the completed task")
        finally:
            s.close()
        print(
            "   the index rebuilt from the record, the ledger replayed"
            + ("" if self.opts.drop_index else " (no --drop-index: the loss was not shown)")
        )


STEPS = [
    ("the server, its tools and its T0 resource", "step_server"),
    ("a session writes a decision and a handoff", "step_write"),
    ("a fresh session loads T0 and finds the handoff", "step_t0"),
    ("search ranks summaries; get returns one body", "step_search"),
    ("a task is claimed under a lease, renewed, completed; a lost lease returns it", "step_tasks"),
    ("export writes a bundle; import lands it once and refuses a tampered one", "step_export_import"),
    ("the index and the ledger are rebuilt from the record", "step_reindex"),
]


def main(argv):
    p = argparse.ArgumentParser(description="The seven M0 steps against a memory server.")
    how = p.add_mutually_exclusive_group(required=True)
    how.add_argument("--stdio", metavar="CMD", help="a command that serves MCP on stdin and stdout; run once per session")
    how.add_argument("--url", metavar="URL", help="an MCP endpoint over streamable HTTP")
    p.add_argument("--operator", metavar="CMD", help="a command run with `export <dir>`, `import <dir>` and `reindex` appended")
    p.add_argument("--drop-index", metavar="CMD", help="a command that loses the server's derived state, for step 7")
    p.add_argument("--domain", help="the domain to run in; default: the first with a T0 resource. The operator's verbs act on the server's default domain, so steps 6 and 7 want that one")
    p.add_argument("--lease", type=int, default=10, help="lease seconds for step 5; the surface's floor is 10 (default)")
    p.add_argument("--timeout", type=float, default=30, help="seconds to wait for any one answer (default 30)")
    opts = p.parse_args(argv)
    if opts.lease < 1:
        p.error("--lease must be at least 1")

    run = Run(opts)
    passed, failed, not_run, blocked = 0, [], [], []
    for n, (title, method) in enumerate(STEPS, 1):
        print(f"{n}. {title}")
        try:
            getattr(run, method)()
        except NotRun as e:
            print(f"   not run: {e}")
            (not_run if e.optional else blocked).append(n)
            continue
        except Fail as e:
            print(f"   FAIL: {e}")
            failed.append(n)
            continue
        except ToolError as e:
            print(f"   FAIL: the server refused: {e}")
            failed.append(n)
            continue
        except Exception as e:  # noqa: BLE001  an answer of a shape no check expected
            print(f"   FAIL: an answer of an unexpected shape: {type(e).__name__}: {e}")
            failed.append(n)
            continue
        passed += 1
        print("   ok")
    print()
    word = f"{passed} of {len(STEPS)} steps passed, {run.checks} checks"
    if not_run:
        word += f"; not run: {', '.join(map(str, not_run))}"
    if blocked:
        word += f"; could not run: {', '.join(map(str, blocked))}"
    if failed:
        word += f"; FAILED: {', '.join(map(str, failed))}"
    print(word)
    return 1 if failed or blocked else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
