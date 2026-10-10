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
is the statement of them. Every store behind those contracts is the simplest thing that
could hold them on one laptop: the record as files under one directory, the index, the
ledger and the metering each a SQLite file, the read log a JSONL file, one process holding
every lock, one host. That is deliberate. It let the contracts be tried against ten
harnesses in two weeks, and it is also why none of it is the production shape.

Where it stops. The ledger is one writer under one mutex; ten thousand committers filing
tasks and agents claiming around the clock need per-domain ledgers in a database that
takes a claim as one row-locked statement. The index is SQLite FTS5; a fleet needs a
search engine sharded by domain. The metering and the read log are private tables that
only this binary writes and reads; a fleet needs OpenTelemetry from the egress, the
server and the ledger into a collector, and the librarian's signals computed over that
stream rather than by scanning the record. The curation cycle surveys a domain by
reading every entry; at scale it is event-driven. The perimeter, admission and the gates
on writes, is Rego already, run in process over the grants file; at scale the same rules
run in OPA and its partial evaluation pushes the read filter into the index. Shared memory
is an injection surface and a channel between agents, and what is here against that, the
write scan and quarantine, the blast-radius and verified-tier gates, the decision and
write logs, taint and the relay alarm, is the start of the paper's answer, not the end. The targets for each are named in
[docs/reference-stack.md](docs/reference-stack.md) and the order in the register in
[PLAN.md](PLAN.md): the production backends go in behind the same crates and must pass
the same conformance suite, and the evaluation harness and `amem savings` give the
numbers for each swap, so that nothing in the move is argued from taste.

## What you get

A per-session memory server, `amem`, that an agent harness reaches over MCP. It holds the record as Markdown files with frontmatter, an index and a task ledger in one SQLite file, and a grants file the owner edits. An agent starts empty, loads a small index called T0, writes a handoff when it stops, and the next agent continues from the handoff rather than a transcript.

```
  harness (Claude Code, Codex, ...) ──MCP──▶ amem ──▶ record/   Markdown, versioned
                                            ├──▶ index.db  SQLite FTS5 + ledger
                                            └──▶ grants.toml
```

## Build and run

Rust stable, then:

```sh
cargo install --path crates/memory --locked   # or `make install`; puts `amem` on the PATH
amem init --domain dom-me                     # a record at ~/memory; --root or MEMORY_ROOT for elsewhere
amem doctor                                   # what this host can run, and what is missing
amem                                          # the web launcher, opened in the browser
```

`amem` with no command starts the web launcher over the record, or finds the one already
running, and opens the browser on a link that pairs it: a code good once, for two
minutes, which the page trades for a key of its own. Run it from the project sessions
should start in, or give `--workdir`. The server listens on loopback only, answers
nothing past its static pages without a key, and keeps its token in
`~/memory/state/web.json`, owner-only; a paired browser's key lasts as long as the server.
Credentials the launcher needs, the API keys, `LLMAO_KEY` for the
gateway, `GH_TOKEN` for Copilot, go in `~/memory/credentials.env` as `NAME=value`, mode
600; the environment wins over the file, and a `gh auth login` stands in for `GH_TOKEN`.
`amem doctor` names which are found where, never their values. `LLMAO_KEY` alone points at the local LiteLLM on port 4000; a PAT for the Foundation's gateway wants `LLMAO_URL=https://llm.apache.org` beside it, which `amem doctor` says when the local one does not answer.

A profile names the flags of a launch, so the name is the whole command. Every harness
is one, `amem claude`, and `~/memory/profiles.toml` adds more:

```toml
[nightly]
harness = "codex"
auth = "gateway"
model = "qwen3.8-27b"
subagents = "board"
```

`amem nightly` launches it, `amem work nightly` works the ledger with it, and a flag
typed after the name wins over the file; `amem profiles` lists them, and the web form
offers them first.

To run a harness confined, with the memory server attached and every model call metered,
interactively, on a prompt, or on the next task from the ledger,
see [local/README.md](local/README.md): `make launch` runs Claude Code, Codex, Gemini CLI,
Qwen Code, Grok Build, Copilot CLI or OpenCode in native namespaces with no network except
the egress, from the harness image once `make unpack` has put it in place, and Antigravity
CLI from the host's own install, since the image does not carry it yet. The gateway half of that page runs LiteLLM
locally so the Foundation's shape can be tried end to end.

Or point a harness at the memory server alone over stdio. For Claude Code:

```sh
claude mcp add memory -- /path/to/amem --root ~/memory serve --author "agent:claude-code"
```

The agent gets `t0`, `search`, `get`, `write`, `list`, the ledger tools, and a
`memory://t0/dom-me` resource. Its writes land at raw trust under the author you gave.

From the command line, the same operations:

```sh
echo "Stopped at step 3. Next: digests." | amem write --kind handoff --title "Import, step 3"
amem search digests
amem get mem-me-xxxxxxxx
amem task create "Finish the import" --priority 5
amem task admit task-me-xxxxxxxx
amem task claim-next
amem recall "how does the import retry?"          # the pack, synthesis owed to the caller
amem recall "how does the import retry?" --provider anthropic   # the brief, a key or the gateway
amem export ../bundle --max-label internal
amem erase mem-me-xxxxxxxx --reason requested      # the owner only; a stub and a receipt remain
amem tap install github-issues taps/target/wasm32-wasip2/release/tap_github_issues.wasm
amem tap run github-issues --config '{"repo":"owner/name"}'
amem                # the web launcher and the browser reader, opened in the browser
amem doctor         # logins, keys, harnesses, backend and gateway, checked
amem claude         # a profile: every harness is one, profiles.toml adds more
amem reindex
amem savings        # the paper's read-path claims, measured on this record
amem golden golden/docs.toml   # recall scored on a question suite
```

`crates/memory/tests/cli.rs` runs the seven steps PLAN.md calls M0, end to end, against the
built binary. `scripts/conformance.py` runs the same seven over MCP against any server
exposing the surface, this one included (`make conformance`); a serving layer that passes
it serves the harnesses this one serves.

## Layout

| Path | What |
|---|---|
| `crates/core` | Entries, frontmatter, bundles, digests, labels. No I/O |
| `crates/store` | The record over OpenDAL |
| `crates/index` | SQLite FTS5 index, summaries first |
| `crates/ledger` | Tasks, leases, origin dedup |
| `crates/policy` | The grants file |
| `crates/server` | The MCP surface |
| `crates/client` | A typed MCP client for the ledger and the record; what the launcher's task mode and an orchestrator use |
| `crates/wslc-api` | The WSL containers C API, bound by hand, Windows only |
| `crates/web` | The web terminal: sessions in pseudo-terminals, a reading pane over the record |
| `crates/memory` | The binary, `amem` |
| `images/harness` | The harness image: seven harnesses, Node, Python, and `memory`, pinned |
| `docs/` | The paper, its case study, the stack |

## Status

M0 code complete as of 2 October 2026, plus the confined launcher: Claude Code, Codex,
Gemini CLI, Copilot CLI and OpenCode run in native Linux namespaces with the memory
server attached over MCP and model calls forwarded and metered through the egress, on
your own logins or on keys the egress holds. First cuts landed 3 and 4 October: `recall`
in two tiers with a golden suite; erasure with a stub and a receipt; taps as WASI
components, GitHub issues in and handoffs posted back as comments; the browser reader
over an exported bundle, searched with the server's own index compiled to WebAssembly;
and the projection, the record as read-only files beside the working tree with a
pre-read hook that makes every open a `get`. See [PLAN.md](PLAN.md).
