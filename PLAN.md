# Plan

Written 1 October 2026. The first target is a developer weekend on 10 and 11 October 2026: working code that a few people can run, read, and extend.

## What this is

The reference implementation of [Durable Memory for Disposable Agents](docs/durable-memory-for-disposable-agents.md). The paper designs shared memory and coordination for fleets of agents that outlive every component they are built from. This repository builds the smallest complete deployment of it, the one the paper's §14 asks for: one person's memory, one process, runnable on a laptop, and then grows it toward the stack in [docs/reference-stack.md](docs/reference-stack.md) without changing the surface.

The one piece of code that matters most is the **wrapper**: a per-session memory server that sits beside an agent harness, holds every credential and every policy check, and is the only thing the agent talks to. On a laptop it is a native binary that Claude Code or Codex reaches over MCP. In an institution the same binary runs as a sidecar the platform launches per workload. Same surface, same stores, different process boundary.

## Decisions already made

| Decision | Choice | Why |
|---|---|---|
| Language | Rust, one workspace | One core library compiles native for Windows and Linux, to wasm for a browser reader, and to WASI for sandboxed plugins. The Apache libraries involved (OpenDAL, DataFusion, Arrow, Iceberg) are Rust-native |
| The record | Markdown files with frontmatter, versioned, never edited in place | The paper's first principle: data outlives tooling, and a person with a text editor can read it |
| The engine | Not chosen. Local files through OpenDAL now; S3-compatible later | The paper names no engine, only seven guarantees (§4) |
| Partition key | The domain, everywhere | Record prefix, index shard, log partition, ledger schema. One writer per domain falls out of it |
| Local index and ledger | SQLite with FTS5, one file | Nothing to operate; BM25 ranking built in; rebuildable from the record |
| Agent binding | MCP over stdio first, HTTP second | Every harness speaks it; the surface in the paper's Appendix A is the contract |
| Policy | A grants file the owner edits; one principal | Identity is the serving layer's concern and a local server authenticates nobody (§5) |
| Interchange | A directory of entries plus a manifest with digests | Aligns with where the Open Memory Protocol work converges |
| Containment | The harness runs with no network. Its only egress is the wrapper, over a unix socket relayed to loopback inside. The wrapper forwards model calls (to a gateway or a provider), checks the session label against the destination's ceiling first, reads token usage out of every response, and serves memory over MCP on the same socket. Linux: native namespaces, unprivileged. macOS: Seatbelt, not yet. Windows: WSL2 | Containment is the launcher's job and does not depend on how the harness authenticates. Metering and the model-class check come for free from being in the path, which is what a subscription login would otherwise lose |
| Handling by egress | A session's handling is capped at the ceiling of the destination its model calls go to, set at launch | A session whose calls go to a public-ceiling destination reads only public entries, so nothing it reads can leave above that ceiling |
| Credentials | The wrapper holds and injects them; the harness gets a placeholder. The one exception is a subscription login, which the harness must hold itself and the wrapper passes through | Anthropic requires the subscription be used by Claude Code itself. Codex with a ChatGPT login is not supported yet, since Codex routes that to chatgpt.com |
| The wrapper's reach | A confined process. It reads and writes only what is mounted into it: the record, its own state, its grants file. Nothing else on the host. Store and index roots are the mount points, and no name may become a path outside them | The agent beside it holds no credentials and the wrapper holds no ambient access. What is mounted is deliberate, as a container's mounts are, whether or not a container runtime is doing the confining |

## M0: the developer weekend, 10 and 11 October 2026

**What "working" means.** Someone clones this, builds it, points Claude Code at it, and the following happens:

1. `memory init` creates a domain with a grants file.
2. A session, through MCP, writes a handoff entry and a decision entry.
3. A **fresh** session starts, loads T0, finds the handoff, and continues the work without the transcript. This is the paper's first exit criterion (§14, M1).
4. `memory search` ranks results by BM25 and returns summaries first; `memory get` returns one body.
5. A task is created, claimed with a lease, heartbeated, and completed; a second claim while the lease holds is refused; an expired lease returns the task to the pool.
6. `memory export` writes a bundle; `memory import` lands it at raw and refuses to import the same entry twice.
7. Deleting the SQLite file and running `memory reindex` rebuilds index and ledger projection from the record alone.

Items 1 through 4 are the demo. Items 5 through 7 are what makes it the paper's design rather than a notes folder.

**Status, 1 October 2026.** All seven pass as `crates/memory/tests/cli.rs` against the
built binary, and `memory serve` answers a raw JSON-RPC session: initialize, fifteen
tools, a write under the session's author, the T0 resource. `local/` brings up LiteLLM
and Postgres in containers as the gateway, and `scripts/claude-via-llmao.sh` starts Claude
Code through it with the memory server attached. Left before the weekend: a run with a
real provider key and what happened written down; a one-page demo script; and whatever
that run turns up.

**In scope for M0**

- `core`: entry schema and frontmatter (id, type, domain, author, created, source, confidence, trust_tier, review_by, supersedes, classification), bundle manifest, digests, the label order.
- `store`: the record over OpenDAL's filesystem backend. Current version at `record/<domain>/<id>.md`, prior versions under `record/<domain>/.history/<id>/`. Demotion writes a tombstone; nothing is deleted in M0.
- `index`: SQLite FTS5 over title, summary, and body, plus a metadata table. Summaries-first results. Rebuildable.
- `ledger`: SQLite. `create_task`, `claim_next`, `claim`, `heartbeat`, `complete`, `release`. Leases expire by wall clock. Origin dedup on `(origin, upstream_event)`.
- `policy`: a TOML grants file: which domains the owner reads, each domain's default classification, each destination's ceiling. One principal.
- `server`: MCP over stdio exposing `search`, `get`, `write`, `create_task`, `claim_next`, `heartbeat`, `complete`, `release`, and T0 as a resource.
- `memory` binary: `init`, `write`, `search`, `get`, `t0`, `export`, `import`, `reindex`, `serve`.
- T0 generation: under 2k tokens, from the domain's handoffs, standing decisions, and pointers.

**Out of scope for M0, named so nobody builds it by accident**

- `recall`: needs a model behind it. M1, through an OpenAI-compatible endpoint (llmao locally is LiteLLM).
- Erasure with receipts. M1. Demotion and tombstones are enough for M0.
- Classification enforcement on destinations beyond export. M1.
- HTTP binding, Postgres and Solr backends, Kafka log, WASI tap host, browser reader. M2 and later; see the stack document.

## After the weekend

| Milestone | Adds | Proves |
|---|---|---|
| M1 | `recall` through llmao or any OpenAI-compatible endpoint; erasure with stub and receipt; ceilings checked on every outbound call; HTTP binding | The paper's §5 flow rules hold in one process |
| M2 | Postgres ledger, Solr index, Kafka log as alternative backends behind the same crates; a compose file | The same binary runs as the sidecar in a container stack |
| M3 | WASI host for taps; a GitHub issues tap with a ceiling. Federated tools reached through the gateway's MCP endpoint, exposed under the wrapper's own surface so the one-server rule holds | Contributed connectors run sandboxed with the access the grant allows |
| M3b | Upstream transport: the wrapper talks to hosts (the institution's memory, the gateway's federation) over gRPC where that pays, while the agent-facing surface stays MCP over stdio or HTTP | MCP has no gRPC transport and the harness side should stay standard; server-to-server is where the paper says a shared protocol belongs, and where efficiency matters |
| M4 | Browser reader over an exported bundle, SQLite FTS5 compiled to wasm | Distribution by URL for the person's own memory |
| M5 | More harnesses behind `memory launch`: Gemini CLI, GitHub Copilot CLI, Cursor's agent CLI, then OpenCode, Aider, Goose. Each needs three things from the harness: a base-URL override for its model calls, MCP over HTTP, and a config directory the launcher can own | The launcher's shape is one adapter per harness; the sandbox, egress, and memory do not change |

## What a developer can pick up on the weekend

Each is a crate or a backend with a clear seam, and each can be done without touching the others.

- The `recall` deputy: a cheap model loop over `search` and `get` returning a cited brief, behind an OpenAI-compatible client.
- The HTTP binding of the same surface.
- A second `index` backend: Solr, or Tantivy embedded.
- A second `ledger` backend: Postgres with `SKIP LOCKED`.
- The WASI plugin host, and a first tap.
- The browser reader.
- Conformance tests: a harness-neutral script that runs the seven M0 steps against any server exposing the surface.

## Working agreements

- Entries are the contract. Any change to the frontmatter is a change to `core` and gets a note in this file.
- Every crate has a test that round-trips its data through the record and back.
- The CLI and the MCP server call the same library functions; nothing lives only in the CLI.
- No engine-specific vocabulary in docs or code outside the backend that needs it.
