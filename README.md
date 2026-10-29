# Durable memory for disposable agents

This project gives shared memory and coordination to fleets of agents. Its design aims to
make the record continue to exist after every agent, harness, model and vendor that it was
written with. This repository holds the reference implementation and the plan to build it.

The paper in [docs/](docs/durable-memory-for-disposable-agents.md) gives the design.
[PLAN.md](PLAN.md) gives the plan, with the first milestone.
[docs/reference-stack.md](docs/reference-stack.md) gives the stack that this
implementation grows into.

## This is a proof of concept

This repository proves the paper's contracts. It does not prove a system that scales.
These contracts are built, and real harnesses on real models test them from end to end:

- the entry format and its frontmatter
- the ledger's rules: one holder, leases, dedup by origin and upstream event, and
  recursive invalidation
- the MCP surface
- trust tiers and review clocks
- summaries first
- the hook protocol
- the egress as the only way out
- the containment set: canaries, an independent checker, and reversible writes

The conformance suite in `crates/memory/tests` states these contracts. The paper's other
commitments, beyond the read path, are also built and tested from end to end, by real
harnesses on real models:

- trust tiers that move only when a person approves
- policy as memory, with scoped exceptions
- the write gates and the quarantine
- the task fabric's rubric
- the librarian's curation and its own telemetry
- prefetch and the read metrics
- vendor-drift probes
- transcripts kept as sources
- spend limits at the egress
- frozen-context replay

Each backend behind those contracts is the simplest one that can hold them on one laptop:

- The record is files under one directory.
- The index, the ledger and the metering are each a SQLite file.
- The read log is a JSONL file.
- One process holds every lock.
- There is one host.

This is deliberate. This shape let the project try the contracts against ten harnesses in
two weeks. It is also the reason that none of it is the production shape.

### What the paper asks for that is not built

- **Replay, in part.** Frozen-context replay is built: `amem replay` runs on the decision
  contexts that the egress records. Live re-execution is also built: `amem reenact` runs a
  prompted session again on its prompt, at its commit. A tape of what its memory tool
  calls returned answers those calls, until the run diverges. These limits
  apply:
  - An interactive session has no prompt to run again.
  - The tape of a session that works a task does not yet carry the prompt of that task.
  - A session on a login that runs through a tunnel has no decision contexts, because the
    egress cannot see into the tunnel. Its replay uses the harness's own transcript, which
    is a self-report, and says so.

  Re-enactment ran on Codex through the gateway (demo act 8). It did not run on the other
  harnesses yet.
- **The platform as a domain, in part.** The grants may name a platform domain. Its
  entries carry artifacts in fenced `platform <kind> <name>` blocks:
  - an agent definition and a template, which open the prompt of a task
  - an eval suite, which `amem golden --platform` scores
  - the default model of a harness

  Each artifact loads only from an active entry that a person verified. In the same way,
  the policy enforces a policy rule only after it is verified. `amem platform list` says
  why the other artifacts do not load. These are not built yet:
  - harness settings other than the model
  - decomposition templates for the board
  - the authoring of agent definitions
  - the loop that proposes changes to them from cost and telemetry
- **Identity.** There is one principal: the owner of a root. Other people appear only as
  a class of claimant (`human`), and as approvers by convention. Nobody authenticates.
- **Federation.** There are no cross-domain request tasks, no interface summaries between
  domains, and no portfolio orchestration. A cross-domain dependency is visible, and that
  is all.
- **The event log, in part.** Every write of an entry goes first through the append-only
  log of its domain. The log holds ids and digests, and never bodies. Each domain has one
  writer. One materializer per domain builds the index from the log and the record. It
  also replays the ledger's own events (`amem log`). These are not built:
  - The ledger keeps its events in its own table, not in the log of the domain.
  - `amem` does not yet feed the index from the log as the log grows. It only rebuilds
    the index from the log.
  - There is no freshness fast lane with a published bound.
- **Drift over many runs.** The detectors read answers and also metadata: fingerprint
  digests, flip rate, formats and refusals. Logprob divergence and change-point detection
  over many runs are missing. The probes run in each curation cycle with `--drift`,
  against fixed thresholds.
- **Costing, in part.** `amem` measures and prices these (`amem cost`, `amem usage`, the
  evaluation):
  - tokens and calls
  - the time of each call
  - the wall clock, CPU time and peak memory of each harness run, added up for each
    session

  These are not built yet:
  - the duration of a task, from claim to completion
  - accelerator time, which only the gateway can count
  - bytes stored
  - CPU or memory limits on the sandbox
- **Smaller pieces.** The search tool ran only against a fake of Brave's API. It never
  ran against the live provider.
- **Platforms and CI.** Confinement on macOS is dry-coded. Its Seatbelt profile comes from
  the plan. Unit tests check the profile on Linux, but it did not run on a Mac yet. The
  bridge to Gofannon is built against the Gofannon REST surface and a fake of it. It did
  not connect to a live deployment yet. It uses the Gofannon session cookie, which is the
  only credential that Gofannon issues. A Gofannon agent makes its own model calls,
  outside the egress, so the egress does not meter them. Since a billing failure, CI does
  not run. The pre-commit hook gates every change locally, and `make verify` runs
  only by hand.

### What is built but works only at demo scale

These parts do what the paper says, on one laptop. Each item names what replaces it.

- **The record** is files under one directory on one disk. Production: an object store
  with versioning, through OpenDAL, as the reference stack says.
- **The index** is one SQLite FTS5 file, with BM25 in process. Production: a search
  engine sharded by domain. In production, OPA compiles the read filter into its query.
- **The ledger** is on SQLite by default, with one writer under a mutex per process. On
  PostgreSQL, it has a schema per domain and a `SKIP LOCKED` claim. But it has one
  connection per process, and no pool. Production: the PostgreSQL shape, with a pool, as
  a projection of the log.
- **The logs** are JSONL and SQLite files under `state/`. They hold the reads, the
  writes, the decisions and the metering of the egress. Everything that `amem` computes
  from them reads them whole:
  - the librarian's retrieval signals and telemetry
  - taint
  - the read metrics
  - the policy rates
  - the relay and write-rate alarms
  - the co-retrieval of prefetch

  Production: the same events as OpenTelemetry spans, into a collector and analytics
  tables. `amem` already emits these spans.
- **The event log** is a JSONL file per domain under `state/log`, behind a file lock.
  Production: a partitioned log, with a partition per domain. The reference stack uses
  Kafka. In production, the materializer is a consumer of the log.
- **Curation** surveys a domain: it reads every entry. It looks for contradictions pair
  by pair through search. It runs as a cycle that someone starts. Production:
  event-driven, for each write, on a schedule with a budget per domain.
- **The rubric's rescore** reads every open task of a domain in each cycle. It settles
  the edges with repeated passes. This is sufficient for hundreds of tasks. Production
  ranks incrementally.
- **The policy** is Rego in process, through regorus. `amem` compiles it again when a
  policy entry changes. It reads the record rules from files at open. Production: OPA as
  a sidecar, partial evaluation for the read filter, and decision logs to telemetry.
- **The quarantine** is files under `state/`. A model reviews them on request. The owner
  releases them by hand.
- **Transcripts' citations**: `amem` finds them on every lookup, with a read of all the
  entries of a domain. It finds the citations that keep a tool's source past its clock
  in the same way.
- **Tools** fetch and search in one of these ways:
  - from the launcher's own machine, with one compiled component for each call. The
    component connects to the address that the policy checked.
  - from a tool server (`amem tool-server`), which runs the same components for
    launchers on other hosts.
  - natively, when no component is installed, behind the same gate. This way only
    fetches.

  A call to a tool server goes through the egress like a model call. The egress holds it
  to the tool's ceiling, and meters and times it under the tool's name. The egress also
  puts the server's bearer on it. Every source records where the tool fetched it from. The
  owner releases a held answer by hand (`amem source release`). Production: the tool
  servers, registered in the Foundation's gateway and found by name, not by URL.
- **Cost** comes from a price table that the owner keeps by hand. The share of a task is
  the spend, wall clock and CPU of its session, divided equally among the tasks that the
  session completed. `amem` counts the CPU and memory of a harness process when it reaps
  that process (`wait4`). One session can have several such processes. `wait4` sees a
  process tree only as far as each level reaps its children. Peak memory is that of the
  largest process, not the sum of the tree. Production: the per-key spend of the gateway,
  with each call tagged with its task and trace.
- **The evaluation** is four questions, graded by regular expression, under four
  conditions:
  - with this memory
  - with an empty one
  - the bare harness
  - the bare harness with its own tools off

  It runs on any number of harnesses, but on one model per run.
- **The web launcher** listens on loopback for the one owner.

The code hides none of this:

- The conformance suite states the contracts.
- The reference stack in [docs/reference-stack.md](docs/reference-stack.md) names each
  production backend.
- The register in [PLAN.md](PLAN.md) gives the order.

Production backends go in behind the same crates, and they must pass the same suite. The
evaluation harness and `amem savings` give the numbers for each swap. The purpose is that
no part of the move depends on personal preference.

## What it is

The design is several parts around one record. No one part is the safety boundary alone.

- The **memory server** is the part that a harness connects to over MCP. It serves the
  record with summaries first. It holds the ledger. It checks every read and write
  against the policy. In a launch, it gives the harness nothing above the **ceiling** of
  the destination of its model. The ceiling is the highest label that may go to that
  destination.
- The **launcher** runs a harness in a **sandbox**. The sandbox has no network. The
  harness can reach only the egress.
- The **egress** carries the MCP of the memory server to the harness. It forwards the
  model calls of the harness to a provider or a gateway. It holds each call to the
  ceiling of its destination. When the egress forwards a call, it also meters the call,
  holds it to its spend limit, and records it with its decision context. The egress
  tunnels some calls, as for Copilot, for the ChatGPT login of Codex, and for the Grok
  Build and Antigravity logins. For a call that it tunnels, the egress checks the
  ceiling when the tunnel opens, and does not meter the call. The egress holds the
  credentials, unless the harness runs on its own subscription login.
- The **tools** are fetch, search and the other tool taps. They run outside the sandbox:
  in the `amem` process, on a tool server through the egress, or in the owner's browser.
  Each tool decides every request against the session label.
- The **perimeter** is the policy over `grants.toml` and the record's policy entries. The
  memory server, the egress and the tools all enforce it.

An agent starts empty. It loads a small index called T0, works a task that it claimed,
and writes a handoff when it stops. The next agent continues from the handoff, not from a
transcript. In addition, the **librarian** curates the record on a budget. A person
promotes what the system may rely on.

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

The diagram shows the MCP arrow straight to the memory server. But MCP also goes through
the egress, which is the one way out of the harness.

Everything is under one root. The root is `~/memory` unless `--root` or `MEMORY_ROOT`
names another. The process touches nothing outside the root:

| Path | What |
|---|---|
| `grants.toml` | The perimeter's data: domains and their labels, destinations and their ceilings, `[gates]`, `[spend.<destination>]`, `[taps.<name>]` for taps and tools (`hosted` for a tool that a tool server serves), `platform = "<domain>"`, and `[ledger]` for PostgreSQL |
| `credentials.env` | API keys, `LLMAO_KEY`, `GH_TOKEN`, as `NAME=value`, mode 600. The environment overrides this file |
| `profiles.toml` | Named launches: harness, auth, model, subagents |
| `prices.toml` | USD per million tokens per model, and per CPU-hour and GiB-hour under `[resources]`, for `amem cost` |
| `policy/*.rego` | Optional: replaces the built-in policy |
| `record/<domain>/` | The entries, as Markdown with frontmatter. Nothing edits an entry in place. Beside them: `.history`, `.erasures`, `.transcripts`, `.contexts` and `.tapes` (re-enactment's tapes, which are the owner's alone) |
| `state/` | Operational data. All of it is rebuildable, except the event log. It holds: `log/<domain>.jsonl`, the event log, which is the order of record that the index is rebuilt from; `index.db`, `ledger.db`, `telemetry.db`; the read, write and decision logs as JSONL; the quarantine; drift, replay and re-enactment runs |

## Build and run

Get Rust stable. Then:

```sh
cargo install --path crates/memory --locked   # or `make install`; puts `amem` on the PATH
amem init --domain dom-me                     # a root at ~/memory
amem doctor                                   # what this host can run, and what is missing
amem                                          # the web launcher, opened in the browser
```

`amem` with no command starts the web launcher over the root, or finds the web launcher
that already runs. Then it opens the browser on a link for pairing. The link holds a
code that is good for one use, for two minutes. The page exchanges the code for a key of
its own. Run `amem` from the directory that you want as the working tree, or give
`--workdir`.
The web launcher listens on loopback only. Without a key, it serves its static pages and
answers nothing else. `amem doctor` names each credential that it finds, and where it
found it. It never shows their values. `LLMAO_KEY` alone points at a local LiteLLM on
port 4000. For the Foundation's gateway, put the PAT in `LLMAO_KEY`, and set
`LLMAO_URL=https://llm.apache.org` beside it. A launch uses the gateway only with
`--auth gateway` (`AUTH=gateway` with `make`). The default is `--auth subscription`.

The page has a companion. The companion is a browser extension in `crates/web/companion`.
It fetches for a session with the browser's own logins. It is for Chrome. Load it unpacked
from `chrome://extensions`. A `[taps.fetch]` grant with `browser = true` sends the fetches
of that tool to the companion. Thus a page that you are signed in to answers the session
as it answers you, and no credential goes into a grant. The policy still decides every URL
before the browser gets the request. It also decides every URL that answered, before
`amem` keeps anything. The extension holds its requests to the session's hosts with the
browser's own request rules. Thus the browser stops a redirect to a host outside the
session's hosts before the redirect occurs. A fetch with `render` opens the page in a tab
instead. It reads the page as the page's scripts built it. This is for a page that is
empty without its scripts. The page shows whether the companion is connected. The
companion works only while `amem` runs and the page is open in that browser.

A profile names the flags of a launch, so the name is the whole command. Each harness is
a profile, for example `amem claude`. `profiles.toml` adds more profiles:

```toml
[nightly]
harness = "codex"
auth = "gateway"
model = "qwen3.8-27b"
subagents = "board"
```

`amem nightly` launches it. `amem work nightly` works the ledger with it. A flag that you
type after the name overrides the file. The launcher runs ten harnesses: Claude Code,
Codex, Gemini CLI, Qwen Code, Grok Build, Copilot CLI, OpenCode, Antigravity, Muse and pi.
It runs them in one of these:

- in native Linux namespaces from the harness image, after `make unpack` puts the image
  in place. Every harness except Antigravity is in the harness image.
- under WSL containers
- under Seatbelt on macOS. This did not run on a Mac yet.

Auth is `key`, `gateway`, or `subscription`. `subscription` is the harness's own login,
which the egress passes through. Where the egress forwards and does not tunnel, it meters
the login as plan quota. [local/README.md](local/README.md) is the full guide.

Or point a harness at the memory server alone, over stdio, with no confinement:

```sh
claude mcp add memory -- /path/to/amem --root ~/memory serve --author "agent:claude-code"
```

The agent gets `t0`, `search`, `get`, `write`, `list`, `recall`, `promote`, the ledger's
memory tools and `transcript_turn`. Its writes go in at raw trust, under the author that
you give. Such a harness has no sandbox, and its model calls do not go through an egress.
The policy still decides what it reads and writes over MCP. But the harness keeps its own
network and credentials. Nothing meters its model calls. No model's ceiling applies to
what it may read. `amem launch --unconfined` is the middle case. At this time it is only
for Claude Code and Codex. It gives no sandbox, but the model calls still go through the
egress.

## The parts, and their commands

[local/README.md](local/README.md) describes most parts, each under its own heading. At
this time, only the register in [PLAN.md](PLAN.md) describes re-enactment, the event log,
the platform domain and the Gofannon bridge.

| Part | What it does | Commands |
|---|---|---|
| The read path | T0, BM25 search with summaries first, one body on `get`, recall as a cited brief or a pack, prefetch on claim and at fetch | `t0`, `search`, `get`, `list`, `recall`, `write` |
| The task fabric | Tasks with leases and handoffs. A rubric from the L1 strategy ranks along the ledger's edges, at admission and in the triage cycle. Workers only claim. Subagents go on the board as children, with a continuation | `task create, admit, claim-next, complete, release, tree, retry, rescore, stats`, `work` |
| Trust tiers | Raw, curated, verified. Only a person who did not propose an entry can make it verified. Review clocks and decay | `promote`, `approve` |
| The perimeter | Rego over `grants.toml` and policy entries in the record (L0 to L3, enforced after they are verified, with scoped exceptions). Admission, the read filter, the write scan and quarantine, and blast-radius gates. A log of every decision | `policy show, eval, list, lint`, `quarantine`, `taint` |
| The librarian | A survey of what needs curation, an independent checker, a search for contradictions, canaries, its own telemetry and alarms, and a cycle on a budget | `librarian`, `curate`, `check`, `canary` |
| The egress | It forwards, meters and times model calls, holds spend limits per destination, and records decision contexts. It keeps the wall clock, CPU and peak memory of each session beside the session's tokens | `launch`, `usage`, `cost` |
| Sources and replay | Transcripts, kept in the record, reached by citation and swept on a clock. Frozen-context replay on the recorded decision contexts. Re-enactment of a session at its commit, against the tape of its memory tool calls. The tapes use the transcripts' clock unless they are pinned | `transcript`, `replay`, `reenact run, report, tapes, show, pin, unpin` |
| The event log | Every write of an entry goes through the append-only log of its domain, with ids and digests only. A materializer builds the index from the log and replays the ledger's events | `log show`, `log materialize` |
| The platform | Agent definitions, templates, eval suites and harness defaults as artifacts in a platform domain. They load only after a person verifies them | `platform list, show`, `golden --platform` |
| Vendor drift | A pinned probe set per model, checked against a baseline, detector by detector | `drift probe`, `drift check` |
| Measurement | The read-path claims on this record, a golden recall suite, and the evaluation with memory against without memory | `savings`, `golden`, `scripts/eval.py` |
| Interchange | Bundles at or below a label, erasure with a stub and a receipt, and taps as WASI components | `export`, `import`, `erase`, `tap` |
| Tools | Fetch and search as taps that an agent calls in a turn. The policy decides every request at the session label. The answers are scanned and kept as sources. Or hosted: a tool server serves the same components, and another launcher calls it through its egress. This path reaches only the hosts that the calling session may reach, and says where it went. The source keeps that | `tool`, `source`, `tool-server`; MCP `fetch`, `web_search` |
| Gofannon | Its deployed agents as a tool tap on the same terms: listed, run and read back. Also a worker that runs one of them on the ledger's tasks and files its result as the handoff | `gofannon agents`, `gofannon work`; MCP `gofannon` |
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

`crates/memory/tests/cli.rs` runs the paper's first milestone from end to end against the
built binary. `scripts/conformance.py` runs the same steps over MCP against any server
that exposes the surface (`make conformance`). A serving layer that passes it serves the
harnesses that this one serves.

## The demo

`make demo-all HARNESS=claude` runs the whole demo, in acts, on one record. The record is
`DEMO_ROOT`, `~/memory-demo` by default. The demo seeds it from the Markdown directories
in `DEMO_FROM`. The acts that need a model use `local/.virtual-key` on llm.apache.org, so
they need the network. Act 9 reads api.github.com. `ACTS="1 4"` runs a subset. Act 7 gives
the record a second domain, the platform domain. Thus, run the acts in order on a new
record. Each act and each step starts with a rule, a number and a sentence about what it
shows. `TRANSCRIPT=demo.txt` also writes the whole run to that file as plain text. You
can read it back with [docs/demo-narrative.md](docs/demo-narrative.md), which holds the
presenter's cues and a handout keyed to the step numbers. `DEMO_FROM` is
`examples/decisions docs` by default: a small fictional decision register and the papers.
The demo and the evaluation set ask about the decision register.

Run `make demo-ready HARNESS=claude` first. It makes the demo ready to run:

- It seeds the record when there is no record. `RESET=1` starts a new demo record in place
  of the old one.
- It builds the harness image at its pins when the unpacked image is not at its pins.
  Then it unpacks the image into the record.
- It checks that the gateway answers with the key.
- It shows the logins that `amem doctor` finds.

It says what is missing and how to fix it. At the end, it shows the command to run. Where
`wslc.exe` builds the image, the demo needs no podman. The local stack needs podman.
`make local-up` now says how to fix a shell that has no `/run/user/<uid>`.

1. **The read path.** The demo seeds a few hundred entries. A harness works a real task
   from T0 and a handoff. The demo measures the savings.
2. **Curation, and the trust tiers.** A curation cycle. A librarian run whose rewrite goes
   in at curated. A person approves an entry for verified.
3. **The perimeter.** The policy holds in quarantine a write that speaks to agents. Then
   taint, policy as memory with a scoped exception, and the read filter. A spend limit
   refuses a call.
4. **The task fabric.** The rubric, compiled from a strategy. A rescore along the edges.
   Subagents go on the board. A follow-up task brings back their reports, and its
   completion closes the task that delegated.
5. **Watching it.** Traces in a collector, the read metrics, drift probes, transcripts by
   citation, and replay.
6. **Memory against no memory.** The same questions go to a harness with memory and with
   an empty one. The demo grades and times the answers, and counts them in tokens, CPU and
   memory.
7. **The platform.** An agent definition, an eval suite and a default model as entries in
   a platform domain. They load only after a person verifies them. A harness works a task
   under the definition. The demo scores the suite and gives the cost of the run in every
   resource that the host counts.
8. **Run it again.** The demo re-enacts a session from act 6 from its tape, on its own
   model and on another model. It scores each run on how far it kept to the original.
9. **From outside.** The GitHub tap files the open issues of a real Apache repository as
   proposed tasks. Its grant reaches one host. The tap files each issue once, and a person
   admits it.
10. **The record moves.** The event log, and the index rebuilt from it. An export at a
    label. An import under another owner. An erasure that refuses the text when the bundle
    comes again.
11. **What this is not, yet.** The demo reads the two lists from the top of this page.

## Checks

`make init` installs the pre-commit hook. The hook runs `make check`. When a commit
stages only Markdown, the hook runs only the guard's tests. `make check` runs these:

- formatting
- clippy with `-D warnings`
- the workspace's tests, which include the browser reader's tests under a Chrome from
  `CHROME` or Playwright's image
- the conformance script's own test against a fake server, with one defect switched on
  in each run
- the guard and docs-only scripts
- unsafe-code and taps checks

`make verify` adds `cargo deny`, `machete`, coverage with a minimum of 90% on lines, and
the WebAssembly build. Since a billing failure, CI does not run. Thus the hook is the
gate, and `make verify` runs only by hand.

Two of those tests reach outside the workspace. A host can cause either of them to fail
for reasons that are not related to the change that you make.

The PostgreSQL half of the conformance suite starts an embedded server. On some hosts,
this server comes without the `UTC` zone. It then stops with `invalid value for parameter
"TimeZone": "UTC"`. `MEMORY_PG_URL` names a different server to use instead. Each test
gets a database of its own on that server, and drops the database after the test:

```sh
docker run -d --name pg-test -e POSTGRES_PASSWORD=pg -p 55432:5432 postgres:17
until docker exec pg-test pg_isready -q; do sleep 1; done
export MEMORY_PG_URL='postgres://postgres:pg@127.0.0.1:55432/postgres'
```

Select a port that nothing else holds. A host with its own PostgreSQL clusters has 5432
in use, and often 5433 also. Then the container cannot bind. It stays in `Created` and
logs nothing. But the port still answers. Thus the test reports a server that does not
authenticate, not a server that is absent.

The companion test loads the extension unpacked. Branded Google Chrome refuses this
(`--load-extension is not allowed in Google Chrome, ignoring`). Chrome is first in the
search, and it starts, so the test selects it. Then the test waits for its full two
minutes, and does not fail immediately. Point `CHROME` at a Chromium. You can use
Playwright's Chromium:

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
| `crates/server` | The MCP surface, prefetch, transcripts and decision contexts, the event log, tapes, the platform and sources |
| `crates/egress` | The way out: forwarding, metering, spend limits, recorded decision contexts |
| `crates/sandbox` | Confinement in Linux namespaces, and a Seatbelt profile for macOS |
| `crates/mount` | The projection as a live mount, where the host has FUSE. The memory server itself serves the record as files. Every open of an entry is a `get` |
| `crates/client` | A typed MCP client for the ledger and the record, which the launcher and an orchestrator use |
| `crates/otel` | OpenTelemetry traces over OTLP |
| `crates/taps` | Taps as WASI components, with exactly the access that their grant allows. The native fetch, behind the same gate |
| `crates/fetch` | What fetch does with a page, which its component and the native fetch share: the URL split, redirects resolved, HTML made text, the answer shaped |
| `crates/reader` | The core crate for the browser reader, compiled to WebAssembly |
| `crates/web` | The web launcher: sessions in pseudo-terminals, a reading pane, the board, and the companion extension that fetches with the browser's logins |
| `crates/wslc-api` | The WSL containers C API, bound by hand, Windows only |
| `crates/memory` | The binary, `amem`: the launcher, the librarian, the command line |
| `taps/` | The taps: GitHub issues and comments, fetch, search and Gofannon |
| `probes/drift.toml` | The pinned vendor-drift probe set |
| `images/harness` | The harness image: the harnesses, Node, Python and `amem`, pinned by version, and Muse by its release channel |
| `scripts/` | The demo and its acts, the evaluation, conformance |
| `docs/` | The paper, the reference stack, the design book, seeding a foundation, proposals for llm.apache.org, the demo's narrative |

## Status

On 12 October 2026, the paper's contracts are built and tested from end to end on one
laptop. The demo runs them live against real harnesses, on the Foundation's gateway and a
Claude login. Until now, a few parts ran only against stand-ins:

- the Gofannon bridge, against a fake of its service
- the search tool, against a fake of its service
- Seatbelt, which did not run on a Mac

The first list above says this, with the rest of what is not built. The list after it
gives what works only at demo scale. [PLAN.md](PLAN.md) holds the decisions, the register,
and the order in which production backends go in.

[docs/llm-apache-org.md](docs/llm-apache-org.md) gives what we propose to the Foundation's
gateway, llmao at llm.apache.org, and why, from one trace across the wrapper and the
gateway to the tool servers that the gateway hosts.
