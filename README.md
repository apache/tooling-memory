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

## Build

Rust stable. Then:

```
cargo build --release
./target/release/memory init ~/memory --domain dom-me
./target/release/memory serve ~/memory
```

Point your harness's MCP configuration at `memory serve ~/memory` over stdio.

## Layout

| Path | What |
|---|---|
| `crates/core` | Entries, frontmatter, bundles, digests, labels. No I/O |
| `crates/store` | The record over OpenDAL |
| `crates/index` | SQLite FTS5 index, summaries first |
| `crates/ledger` | Tasks, leases, origin dedup |
| `crates/policy` | The grants file |
| `crates/server` | The MCP surface |
| `crates/memory` | The binary |
| `docs/` | The paper, its case study, the stack |

## Status

Pre-M0. See [PLAN.md](PLAN.md) for what M0 is and when.
