# Durable memory for disposable agents

Shared memory and coordination for fleets of agents, built so that the record outlives every agent, harness, model, and vendor it was written with. This repository holds the reference implementation and the plan for building it.

The design is the paper in [docs/](docs/durable-memory-for-disposable-agents.md). The plan, including the first milestone, is [PLAN.md](PLAN.md). The stack this grows into is [docs/reference-stack.md](docs/reference-stack.md).

## This is a proof of concept

What is proven here is the paper's contracts, not a system that scales. The entry
format and its frontmatter, the ledger's rules (one holder, leases, dedup by origin and
upstream event, recursive invalidation), the MCP surface, trust tiers and review clocks,
summaries first, the hook protocol, the egress as the only way out, and the containment
set (canaries, an independent checker, reversible writes) are built and exercised end to
end, by real harnesses on real models, and the conformance suite in `crates/memory/tests`
is the statement of them. Beyond the read path, so are the paper's other commitments:
trust tiers that move only by a person's approval, policy as memory with scoped
exceptions, the write gates and the quarantine, the task fabric's rubric, the
librarian's curation and its own telemetry, prefetch and the read metrics, vendor-drift
probes, transcripts kept as sources, spend limits at the egress, and frozen-context
replay. Every store behind those contracts is the simplest thing that
could hold them on one laptop: the record as files under one directory, the index, the
ledger and the metering each a SQLite file, the read log a JSONL file, one process holding
every lock, one host. That is deliberate. It let the contracts be tried against ten
harnesses in two weeks, and it is also why none of it is the production shape.

### What the paper asks for that is not built

- **Replay, in part.** Frozen-context replay is built, `amem replay`, on decision
  contexts the egress records, and so is live re-execution, `amem reenact`: a prompted
  session run again on its prompt at its commit, its memory calls answered from a tape
  of what they returned until the run diverges. An interactive session has no prompt to
  run again, a task run's tape does not yet carry its task's prompt, and a login run
  through a tunnel, which the egress cannot see into, has no contexts, so its replay
  falls back to the harness's own transcript, a self-report, and says so. Re-enactment
  has run on Codex through the gateway (demo act 8), not yet on the other harnesses.
- **The platform as a domain, in part.** The grants may name a platform domain whose
  entries carry artifacts, fenced `platform <kind> <name>` blocks: an agent definition
  and a template that open a task's prompt, an eval suite `amem golden --platform` scores,
  and a harness's default model. Each loads only from an active entry a person verified,
  as a policy rule is enforced only once verified (`amem platform list` says why the rest
  do not). Not yet: harness settings beyond the model, decomposition templates for the
  board, authoring agent definitions, and the loop that proposes changes to them from
  cost and telemetry.
- **Identity.** One principal, the owner of a root. Other people appear only as a class
  of claimant (`human`) and as approvers by convention; nobody authenticates.
- **Federation.** No cross-domain request tasks, interface summaries between domains, or
  portfolio orchestration; a cross-domain dependency is seen, nothing more.
- **The event log, in part.** Every write of an entry goes through its domain's
  append-only log first, ids and digests and never bodies, one writer per domain, and
  one materializer per domain builds the index from the log and the record and replays
  the ledger's own events (`amem log`). The ledger keeps its events in its own table
  rather than the domain's log, the index is not yet fed from the log as it grows, only
  rebuilt from it, and there is no freshness fast lane with a published bound.
- **Drift over many runs.** The detectors read answers as well as metadata: fingerprint
  digests, flip rate, formats and refusals. What is missing is logprob divergence and
  change-point detection over many runs; the probes run each curation cycle with
  `--drift`, against fixed thresholds.
- **Costing, in part.** Tokens, calls, each call's time, and each run's wall clock, CPU
  time and peak memory are measured and priced (`amem cost`, `amem usage`, the
  evaluation). Not yet: a task's own duration from claim to completion, accelerator time,
  which only the gateway can count, bytes stored, and CPU or memory limits on the
  sandbox.
- **Smaller pieces.** The search tool has met only a fake of Brave's API, never the live
  provider.
- **Platforms and CI.** Confinement on macOS is dry-coded: a Seatbelt profile made from
  the plan, unit-tested on Linux and not yet run on a Mac. The bridge to Gofannon is
  built against its REST surface and a fake of it, not yet a live deployment, and on its
  session cookie, the only credential Gofannon issues; the model calls a Gofannon agent
  makes are its own, outside the egress, and so not metered. CI has not run since a
  billing failure; every change is gated locally by the pre-commit hook, and `make
  verify` run by hand.

### What is built but works only at demo scale

These do what the paper says, on one laptop, and each names what replaces it.

- **The record** is files under one directory on one disk. Production: an object store
  with versioning, through OpenDAL, as the reference stack says.
- **The index** is one SQLite FTS5 file, BM25 in process. Production: a search engine
  sharded by domain, with the read filter compiled into its query by OPA.
- **The ledger** is SQLite by default, one writer under a mutex per process; on
  PostgreSQL it has a schema per domain and a `SKIP LOCKED` claim, but one connection per
  process and no pool. Production: the Postgres shape, pooled, as a projection of the log.
- **The logs**, reads, writes, decisions and the egress's metering, are JSONL and SQLite
  files under `state/`, and everything computed from them, the librarian's retrieval
  signals and telemetry, taint, the read metrics, the policy rates, the relay and
  write-rate alarms, prefetch's co-retrieval, reads them whole. Production: the same
  events as OpenTelemetry spans, already emitted, into a collector and analytics tables.
- **The event log** is a JSONL file per domain under `state/log`, behind a file lock.
  Production: a partitioned log, Kafka in the reference stack, a partition per domain
  and the materializer a consumer of it.
- **Curation** surveys a domain by reading every entry, hunts contradictions pair by
  pair through search, and runs as a cycle someone starts. Production: event-driven, per
  write, on a schedule with a budget per domain.
- **The rubric's rescore** reads a domain's whole queue each cycle and settles its edges
  by repeated passes. Fine for hundreds of tasks; production ranks incrementally.
- **The policy** is Rego in process through regorus, compiled again whenever a policy
  entry changes, with record rules read from files at open. Production: OPA as a sidecar,
  partial evaluation for the read filter, decision logs to telemetry.
- **The quarantine** is files under `state/`, reviewed by a model on request, released
  by the owner by hand.
- **Transcripts' citations** are found by scanning a domain's entries on every lookup,
  and so are the citations that keep a tool's source past its clock.
- **Tools** fetch and search from the launcher's own machine, one compiled component per
  call, connecting to the address the policy checked, or from a tool server
  (`amem tool-server`) that runs the same components for launchers elsewhere, or fetch
  natively when no component is installed, behind the same gate; called through the
  egress like a model call: held to the tool's ceiling, metered and timed under the
  tool's name, the server's bearer put on there. Every source says where it was fetched
  from. A held answer is released by the owner by hand
  (`amem source release`). Production: the tool servers registered in the Foundation's
  gateway, found by name rather than by URL.
- **Cost** is priced from a table the owner keeps by hand, and a task's share is its
  session's spend, wall clock and CPU divided evenly among the tasks it completed. A
  run's CPU and memory are counted when it is reaped (`wait4`), which sees a process tree
  only as far as each level reaps its children, and peak memory is the largest process's,
  not the tree's sum. Production: the gateway's
  per-key spend, with each call tagged with its task and trace.
- **The evaluation** is four questions graded by regular expression, under four
  conditions (with this memory, with an empty one, the bare harness, and the bare harness
  with its own tools off), on any number of harnesses but one model per run.
- **The web launcher** listens on loopback for the one owner.

None of this is hidden in the code: the conformance suite states the contracts, the
reference stack in [docs/reference-stack.md](docs/reference-stack.md) names each
production backend, and the register in [PLAN.md](PLAN.md) the order. Production
backends go in behind the same crates and must pass the same suite, and the evaluation
harness and `amem savings` give the numbers for each swap, so that nothing in the move is
argued from taste.

## What it is

Several parts around one record, and no one of them is the safety boundary on its own.
The **memory server** is what an agent harness reaches over MCP: it serves the record by
summaries first, holds the task ledger, and checks every read and write against the
policy. The **launcher** runs a harness in a **sandbox**, with no network except the
**egress**, which forwards its model and tool calls to a provider, a gateway or a tool
server, holds each to its destination's ceiling, meters them, holds the credentials,
enforces spend limits and records each call's decision context. The memory server and the
egress enforce the same grants; the sandbox makes the egress the only way out. An agent
starts empty, loads a small index called T0, works a task it
claimed, and writes a handoff when it stops; the next agent continues from the handoff,
not a transcript. Around them, the **librarian** curates the record on a budget, the
**perimeter** decides what may be read, written and admitted, and a person promotes what
the system may rely on.

```
  person ──▶ amem: web launcher, CLI, profiles
               │ launch / work (a claimed task)
               ▼
  ┌─ sandbox: harness, no network ───────┐
  │ Claude Code, Codex, Gemini CLI, ...  │──MCP──▶ memory server ──▶ record/   entries, transcripts, contexts, tapes
  └──────────────┬───────────────────────┘          │  policy:  ├──▶ index    SQLite FTS5, BM25
                 │ model calls                      │  Rego     ├──▶ ledger   SQLite, or PostgreSQL
                 ▼                                  │           └──▶ logs     the event log; reads, writes, decisions
              egress ──▶ provider or gateway        │
       metered, spend-limited, contexts recorded    └── librarian: curation, checker, canaries, drift
```

Everything lives under one root, `~/memory` unless `--root` or `MEMORY_ROOT` says
otherwise, and the process touches nothing outside it:

| Path | What |
|---|---|
| `grants.toml` | The perimeter's data: domains and their labels, destinations and their ceilings, `[gates]`, `[spend.<destination>]`, `[taps.<name>]` for taps and tools (`hosted` for one a tool server serves), `platform = "<domain>"`, and `[ledger]` for PostgreSQL |
| `credentials.env` | API keys, `LLMAO_KEY`, `GH_TOKEN`, as `NAME=value`, mode 600; the environment wins |
| `profiles.toml` | Named launches: harness, auth, model, subagents |
| `prices.toml` | USD per million tokens per model, and per CPU-hour and GiB-hour under `[resources]`, for `amem cost` |
| `policy/*.rego` | Optional: replaces the built-in policy |
| `record/<domain>/` | The entries, Markdown with frontmatter, never edited in place; `.history`, `.erasures`, `.transcripts`, `.contexts` and `.tapes` (re-enactment's, the owner's alone) beside them |
| `state/` | Operational, and rebuildable but for the event log: `log/<domain>.jsonl`, the event log, the order of record the index is rebuilt from; `index.db`, `ledger.db`, `telemetry.db`; the read, write and decision logs as JSONL; the quarantine; drift, replay and re-enactment runs |

## Build and run

Rust stable, then:

```sh
cargo install --path crates/memory --locked   # or `make install`; puts `amem` on the PATH
amem init --domain dom-me                     # a root at ~/memory
amem doctor                                   # what this host can run, and what is missing
amem                                          # the web launcher, opened in the browser
```

`amem` with no command starts the web launcher over the root, or finds the one already
running, and opens the browser on a link that pairs it: a code good once, for two
minutes, which the page trades for a key of its own. Run it from the project sessions
should start in, or give `--workdir`. The server listens on loopback only and answers
nothing past its static pages without a key. `amem doctor` names which credentials are
found where, never their values. `LLMAO_KEY` alone points at a local LiteLLM on port 4000;
a PAT for the Foundation's gateway wants `LLMAO_URL=https://llm.apache.org` beside it.

The page has a companion: a browser extension in `crates/web/companion` (Chrome, load it
unpacked from `chrome://extensions`) that fetches for a session with the browser's own
logins. A `[taps.fetch]` grant with `browser = true` sends that tool's fetches there, so a
page you are signed in to answers the session as it would answer you, and no credential
goes into a grant. The policy still decides every URL before the browser is asked and
every URL that answered before anything is kept, and the extension holds its requests to
the session's hosts with the browser's own request rules, so a redirect elsewhere is
stopped before it is made. A fetch with `render` opens the page in a tab instead and
reads it as its scripts built it, for a page that is empty without them; the page shows
whether the companion is connected. It works only while `amem` runs with the page open in
that browser.

A profile names the flags of a launch, so the name is the whole command. Every harness
is one, `amem claude`, and `profiles.toml` adds more:

```toml
[nightly]
harness = "codex"
auth = "gateway"
model = "qwen3.8-27b"
subagents = "board"
```

`amem nightly` launches it, `amem work nightly` works the ledger with it, and a flag
typed after the name wins over the file. The launcher runs ten harnesses, Claude Code,
Codex, Gemini CLI, Qwen Code, Grok Build, Copilot CLI, OpenCode, Antigravity, Muse and
pi, in native Linux namespaces from the harness image once `make unpack` has put it in
place (every one but Antigravity is in it), under WSL containers, or, not yet run on a
Mac, under Seatbelt on macOS; auth is
`key`, `gateway`, or `subscription`, the harness's own login, which the egress passes
through and meters as plan quota. [local/README.md](local/README.md) is the full guide.

Or point a harness at the memory server alone over stdio, unconfined:

```sh
claude mcp add memory -- /path/to/amem --root ~/memory serve --author "agent:claude-code"
```

The agent gets `t0`, `search`, `get`, `write`, `list`, `recall`, `promote`, the ledger's tools
and `transcript_turn`, and its writes land at raw trust under the author given.
Unconfined means only that: the record's rules hold for what the harness reads and writes
over MCP, but no sandbox or egress stands around the rest of what it does, and it keeps
its own network and credentials.

## The parts, and their commands

Most parts are described in [local/README.md](local/README.md) under their own heading;
re-enactment, the event log, the platform domain and the Gofannon bridge so far only in
[PLAN.md](PLAN.md)'s register.

| Part | What it does | Commands |
|---|---|---|
| The read path | T0, BM25 search with summaries first, one body on `get`, recall as a cited brief or a pack, prefetch on claim and at fetch | `t0`, `search`, `get`, `list`, `recall`, `write` |
| The task fabric | Tasks with leases and handoffs; a rubric from the L1 strategy ranks along the ledger's edges at admission and in the triage cycle, and workers only claim; subagents filed on the board as children with a continuation | `task create, admit, claim-next, complete, release, tree, retry, rescore, stats`, `work` |
| Trust tiers | Raw, curated, verified; verified only by a person who did not propose it; review clocks and decay | `promote`, `approve` |
| The perimeter | Rego over `grants.toml` and policy entries in the record (L0 to L3, enforced once verified, with scoped exceptions); admission, the read filter, the write scan and quarantine, blast-radius gates; every decision logged | `policy show, eval, list, lint`, `quarantine`, `taint` |
| The librarian | A survey of what needs curation, an independent checker, contradiction hunting, canaries, its own telemetry and alarms, a cycle on a budget | `librarian`, `curate`, `check`, `canary` |
| The egress | Model calls forwarded, metered and timed, spend limits per destination, decision contexts recorded; each run's wall clock, CPU and peak memory kept beside its tokens | `launch`, `usage`, `cost` |
| Sources and replay | Transcripts kept in the record and reached by citation, swept on a clock; frozen-context replay on the recorded contexts; re-enactment of a session at its commit against the tape of its memory calls, the tapes on the transcripts' clock unless pinned | `transcript`, `replay`, `reenact run, report, tapes, show, pin, unpin` |
| The event log | Every entry write through its domain's append-only log, ids and digests only; a materializer builds the index from it and replays the ledger's events | `log show`, `log materialize` |
| The platform | Agent definitions, templates, eval suites and harness defaults as artifacts in a platform domain, loaded only once a person verified them | `platform list, show`, `golden --platform` |
| Vendor drift | A pinned probe set per model, checked against a baseline, detector by detector | `drift probe`, `drift check` |
| Measurement | The read-path claims on this record, a golden recall suite, and the evaluation with memory against without | `savings`, `golden`, `scripts/eval.py` |
| Interchange | Bundles at or below a label, erasure with a stub and a receipt, taps as WASI components | `export`, `import`, `erase`, `tap` |
| Tools | Fetch and search as taps an agent calls mid-turn: every request decided by the policy at the session's label, answers scanned and kept as sources; or hosted, the same components served by a tool server another launcher calls through its egress, which reaches only the hosts the calling session may and says where it went, kept on the source | `tool`, `source`, `tool-server`; MCP `fetch`, `web_search` |
| Gofannon | Its deployed agents as a tool tap on the same terms: listed, run, read back; and a worker that runs one on the ledger's tasks, its result filed as the handoff | `gofannon agents`, `gofannon work`; MCP `gofannon` |
| Traces | OpenTelemetry spans for model calls, reads, policy decisions and task transitions, to any OTLP collector | `OTEL_EXPORTER_OTLP_ENDPOINT` |

From the command line:

```sh
echo "Stopped at step 3. Next: digests." | amem write --kind handoff --title "Import, step 3"
amem search digests
amem get mem-me-xxxxxxxx
amem task create "Finish the import" --priority 5
amem task admit task-me-xxxxxxxx
amem work --harness claude                 # claim the top task, run the harness on it
amem recall "how does the import retry?" --provider anthropic
amem curate --auth gateway --contradictions  # one curation cycle
amem quarantine list
amem taint mem-me-xxxxxxxx                 # where a bad entry may have reached
amem replay <session> --model gemma4-26b   # the same decisions, another model
amem reenact run <session> --auth gateway --model qwen3.8-27b --runs 3   # the same task again
amem reenact report <session>              # each harness and model against the tape
amem cost                                  # spend, wall clock, CPU and memory per session and task
amem savings                               # the paper's read-path claims, measured
amem savings --judge-provider anthropic   # with a small model judging what was "used"
```

`crates/memory/tests/cli.rs` runs the paper's first milestone end to end against the
built binary, and `scripts/conformance.py` runs the same steps over MCP against any
server exposing the surface (`make conformance`): a serving layer that passes it serves
the harnesses this one serves.

## The demo

`make demo-all HARNESS=claude` runs the whole demo in acts on one record, `DEMO_ROOT`,
`~/memory-demo` by default, seeded from the Markdown directories in `DEMO_FROM`. The
acts that need a model use `local/.virtual-key` on llm.apache.org, so they want the
network, and act 9 reads api.github.com; `ACTS="1 4"` runs a subset. Act 7 gives the
record a second domain, the platform's, so run the acts in order on a fresh record. Each
act and each step opens with a rule, a number and a sentence on what it shows;
`TRANSCRIPT=demo.txt` also writes the whole run there as plain text, to read back with
[docs/demo-narrative.md](docs/demo-narrative.md), the presenter's cues and a handout
keyed to the step numbers. `DEMO_FROM` is `examples/decisions docs` by default: a small
fictional decision register, which the demo and the evaluation set ask about, and the papers.

`make demo-ready HARNESS=claude` gets it to a state it can run first: it seeds the
record when there is none (`RESET=1` starts a demo record over), builds the harness
image at its pins when the one unpacked is not and unpacks it into the record, checks
that the gateway answers with the key, and shows the logins `amem doctor` finds. It
says what is missing and how to fix it, and ends with the command to run. The demo
needs no podman where `wslc.exe` builds the image; the local stack does, and
`make local-up` now says how to fix a shell whose `/run/user/<uid>` is missing.

1. **The read path.** A few hundred entries seeded, a harness working a real task from T0 and a handoff, and the savings measured.
2. **Curation, and the trust tiers.** A curation cycle, a librarian run whose rewrite lands curated, and a person approving an entry for verified.
3. **The perimeter.** A write that speaks to agents held in quarantine, taint, policy as memory with a scoped exception, the read filter, and a spend limit refusing a call.
4. **The task fabric.** The rubric compiled from a strategy, a rescore along the edges, and subagents filed on the board, their reports brought back by a follow-up task whose completion closes the task that delegated.
5. **Watching it.** Traces in a collector, the read metrics, drift probes, transcripts by citation, and replay.
6. **Memory against no memory.** The same questions to a harness with memory and with an empty one, graded, timed and counted in tokens, CPU and memory.
7. **The platform.** An agent definition, an eval suite and a default model as entries in a platform domain, loaded only once a person verifies them; a task worked under the definition, the suite scored, and the run's cost in every resource the host counts.
8. **Run it again.** A session from act 6 re-enacted from its tape, on its own model and on another, scored on how far each kept to the original.
9. **From outside.** A real Apache repository's open issues filed as proposed tasks by the GitHub tap, under a grant that reaches one host; filed once, admitted by a person.
10. **The record moves.** The event log, the index rebuilt from it, an export at a label, an import under another owner, and an erasure that refuses the text when the bundle comes again.
11. **What this is not, yet.** The two lists at the top of this page, read from it.

## Checks

`make init` installs the pre-commit hook, which runs `make check` (only the guard's
tests when a commit stages Markdown alone): formatting, clippy
with `-D warnings`, the workspace's tests including the browser reader's under a Chrome
from `CHROME` or Playwright's image, the conformance script's own test against a fake
server with one defect switched on per run, the guard and docs-only scripts, unsafe-code and taps checks. `make verify` adds
`cargo deny`, `machete`, coverage with a 90% floor on lines, and the WebAssembly build.
CI has not run since a billing failure, so the hook is the gate, and `make verify` is
run by hand.

Two of those tests reach outside the workspace, and a host can leave either of them
failing for reasons that have nothing to do with the change in hand.

The PostgreSQL half of the conformance suite starts an embedded server, which on some
hosts arrives without the `UTC` zone and stops with `invalid value for parameter
"TimeZone": "UTC"`. `MEMORY_PG_URL` names a server to borrow instead, and each test
takes a database of its own on it and drops it afterwards:

```sh
docker run -d --name pg-test -e POSTGRES_PASSWORD=pg -p 55432:5432 postgres:17
until docker exec pg-test pg_isready -q; do sleep 1; done
export MEMORY_PG_URL='postgres://postgres:pg@127.0.0.1:55432/postgres'
```

Pick a port nothing else holds. A host with PostgreSQL clusters of its own has 5432
taken and often 5433 as well, and the container then fails to bind, sits in `Created`,
and logs nothing — while the port still answers, so the test reports a server that will
not authenticate rather than one that is not there.

The companion test loads the extension unpacked, which branded Google Chrome refuses
(`--load-extension is not allowed in Google Chrome, ignoring`). Chrome is first in the
search and starts, so it is chosen, and the test then waits out its two minutes rather
than failing at once. Point `CHROME` at a Chromium; Playwright's will do:

```sh
npx playwright install chromium        # if there is none already
export CHROME="$(ls -d ~/.cache/ms-playwright/chromium-*/chrome-linux64/chrome | head -1)"
```

## Layout

| Path | What |
|---|---|
| `crates/core` | Entries, frontmatter, bundles, digests, labels. No I/O |
| `crates/store` | The record over OpenDAL |
| `crates/index` | SQLite FTS5 index, summaries first |
| `crates/ledger` | Tasks, leases, origin dedup, the rubric; SQLite and PostgreSQL |
| `crates/policy` | The grants file, the Rego policy through regorus, the write scan and the gates |
| `crates/server` | The MCP surface, prefetch, transcripts and contexts, the event log, tapes, the platform and sources |
| `crates/egress` | The way out: forwarding, metering, spend limits, recorded contexts |
| `crates/sandbox` | Confinement in Linux namespaces, and a Seatbelt profile for macOS |
| `crates/mount` | The projection as a live mount: the record as files the memory server itself serves, every open of an entry a `get`, where the host has FUSE |
| `crates/client` | A typed MCP client for the ledger and the record, used by the launcher and an orchestrator |
| `crates/otel` | OpenTelemetry traces over OTLP |
| `crates/taps` | Taps as WASI components, with exactly the access their grant allows; the native fetch behind the same gate |
| `crates/fetch` | What fetch does with a page, shared by its component and the native fetch: the URL split, redirects resolved, HTML made text, the answer shaped |
| `crates/reader` | The core crate for the browser reader, compiled to WebAssembly |
| `crates/web` | The web launcher: sessions in pseudo-terminals, a reading pane, the board, and the companion extension that fetches with the browser's logins |
| `crates/wslc-api` | The WSL containers C API, bound by hand, Windows only |
| `crates/memory` | The binary, `amem`: the launcher, the librarian, the CLI |
| `taps/` | The taps: GitHub issues and comments, fetch, search and Gofannon |
| `probes/drift.toml` | The pinned vendor-drift probe set |
| `images/harness` | The harness image: the harnesses, Node, Python and `amem`, pinned by version, Muse by its release channel |
| `scripts/` | The demo and its acts, the evaluation, conformance |
| `docs/` | The paper, the reference stack, the design book, seeding a foundation, proposals for llm.apache.org, the demo's narrative |

## Status

As of 12 October 2026, the paper's contracts are built and exercised end to end on one
laptop, and the demo runs them live against real harnesses on the Foundation's gateway
and a Claude login. A few parts have met only stand-ins so far: the Gofannon bridge and
the search tool fakes of their services, and Seatbelt no Mac; the first list above says
so with the rest of what is not built, and the list after it is what works only at demo
scale. [PLAN.md](PLAN.md) holds the decisions and the register, and the order production
backends go in.

What we propose to the Foundation's gateway, llmao at llm.apache.org, and why, from one
trace across the wrapper and the gateway to the tool servers the gateway hosts, is in
[docs/llm-apache-org.md](docs/llm-apache-org.md).
