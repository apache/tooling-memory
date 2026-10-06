# Durable memory for disposable agents

Shared memory and coordination for fleets of agents, built so that the record outlives every agent, harness, model, and vendor it was written with. This repository holds the reference implementation and the plan for building it.

The design is the paper in [docs/](docs/durable-memory-for-disposable-agents.md). The plan, including the first milestone, is [PLAN.md](PLAN.md). The stack this grows into is [docs/reference-stack.md](docs/reference-stack.md).

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
running, and opens the browser on it with a link that carries the server's token. The
server listens on loopback only and answers nothing past its static pages without that
token; it lives in `~/memory/state/web.json`, owner-only, and survives a restart, so an
open tab keeps working. Credentials the launcher needs, the API keys, `LLMAO_KEY` for the
gateway, `GH_TOKEN` for Copilot, go in `~/memory/credentials.env` as `NAME=value`, mode
600; the environment wins over the file, and a `gh auth login` stands in for `GH_TOKEN`.
`amem doctor` names which are found where, never their values.

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
amem reindex
amem savings        # the paper's read-path claims, measured on this record
amem golden golden/docs.toml   # recall scored on a question suite
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
