# Durable memory for disposable agents

Shared memory and coordination for fleets of agents, built so that the record outlives every agent, harness, model, and vendor it was written with. This repository holds the reference implementation and the plan for building it.

The design is the paper in [docs/](docs/durable-memory-for-disposable-agents.md). The plan, including the first milestone, is [PLAN.md](PLAN.md). The stack this grows into is [docs/reference-stack.md](docs/reference-stack.md).

## What you get

A per-session memory server, `memory`, that an agent harness reaches over MCP. It holds the record as Markdown files with frontmatter, an index and a task ledger in one SQLite file, and a grants file the owner edits. An agent starts empty, loads a small index called T0, writes a handoff when it stops, and the next agent continues from the handoff rather than a transcript.

```
  harness (Claude Code, Codex, ...) ──MCP──▶ memory ──▶ record/   Markdown, versioned
                                              ├──▶ index.db  SQLite FTS5 + ledger
                                              └──▶ grants.toml
```

## Build and run

Rust stable, then:

```sh
cargo build --release
export MEMORY_ROOT=~/memory
./target/release/memory init --domain dom-me
./target/release/memory t0
```

To run a harness confined, with the memory server attached and every model call metered,
interactively, on a prompt, or on the next task from the ledger,
see [local/README.md](local/README.md): `make launch` runs Claude Code, Codex, Gemini CLI,
Copilot CLI or OpenCode in native namespaces with no network except the egress, from the
harness image once `make unpack` has put it in place. The gateway half of that page runs LiteLLM
locally so the Foundation's shape can be tried end to end.

Or point a harness at the memory server alone over stdio. For Claude Code:

```sh
claude mcp add memory -- /path/to/memory --root ~/memory serve --author "agent:claude-code"
```

The agent gets `t0`, `search`, `get`, `write`, `list`, the ledger tools, and a
`memory://t0/dom-me` resource. Its writes land at raw trust under the author you gave.

From the command line, the same operations:

```sh
echo "Stopped at step 3. Next: digests." | memory write --kind handoff --title "Import, step 3"
memory search digests
memory get mem-me-xxxxxxxx
memory task create "Finish the import" --priority 5
memory task admit task-me-xxxxxxxx
memory task claim-next
memory export ../bundle --max-label internal
memory reindex
```

`crates/memory/tests/cli.rs` runs the seven steps PLAN.md calls M0, end to end, against the
built binary.

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
| `crates/web` | The web terminal: sessions in pseudo-terminals, a reading pane over the record |
| `crates/memory` | The binary |
| `images/harness` | The harness image: five harnesses, Node, and `memory`, pinned |
| `docs/` | The paper, its case study, the stack |

## Status

M0 code complete as of 2 October 2026, plus the confined launcher: Claude Code, Codex,
Gemini CLI, Copilot CLI and OpenCode run in native Linux namespaces with the memory
server attached over MCP and model calls forwarded and metered through the egress, on
your own logins or on keys the egress holds. See [PLAN.md](PLAN.md).
