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
| M5b, done 2 October | Three ways to run a harness, one launcher. Interactive, as now. Non-interactive: `--prompt` or `--prompt-file`, and the adapter knows each harness's headless form (`claude -p`, `codex exec`, `gemini -p`, `copilot -p`, `opencode run`), so the caller never does; output and exit code come back, and the run is refused if no handoff was written unless `--no-handoff` says that is fine. Task mode: `memory work` claims the next admitted task the session's manifest matches, builds the prompt from T0, the task entry, and the handoffs of what it depends on, runs the harness non-interactively with the task id in hand, and holds the lease itself, heartbeating while the harness runs; on exit, a completed task stays complete, anything else is released back to the pool with the harness's output filed as a handoff so the next claimant starts where this one stopped. `--loop` keeps claiming. The launcher reaches the ledger as an MCP client of the memory server (`crates/client`), the same tool surface the harness uses, over an in-process pipe locally; the ledger's rules stay on the server, and a launcher that is behind still speaks a stable tool contract. | One invocation, one task, is the unit Gofannon will call; the lease belonging to the launcher and not the harness is what makes a crashed or wedged harness harmless to the ledger. The weekend demo is two terminals: one files tasks, the other runs `memory work --loop` |
| M5c, first cut done 2 October | Handoffs the harness did not write. Two sources, in order. First, hooks: Claude Code fires a hook before it compacts context and another at session end, with the transcript's path in hand; Codex and Gemini CLI expose the same moments. The launcher owns each harness's config directory, so it installs the hook itself, pointing at the `memory` binary inside the sandbox, which files a handoff from the transcript's tail. Second, the transcript: it lives in the private home the launcher owns, so after a run the launcher keeps it beside the session and can mine it for entries, raw tier, low confidence, source pointing at the transcript, for a person to promote or drop. Mining is a model call: locally a small model behind the egress as its own destination; under Gofannon, the gateway's configured model and policy. The hook comes first, cheap and deterministic; keeping the transcript second, no model; mining last. Done: Claude Code's PreCompact and SessionEnd hooks, installed by the launcher in the private home, leave notes in an outbox the launcher files as handoffs; a pre-compaction note always, a session-end note only when the run left no handoff. Transcripts persist under the private home. Not yet: Codex and Gemini hooks, mining | A handoff is the record's unit of continuity, and the moments it is most needed, context about to be compacted and a session ending, are exactly the moments a harness is least likely to write one unprompted |
| M6, first cut done 2 October | A Windows backend on WSL containers (`wslc`, GA 29 September 2026, WSL 2.9.3+). The same shape as the Linux backend, with the container as the boundary instead of our namespaces: an egress container runs the Linux `memory` with the record mounted in over virtiofs and the only outside route; a harness container runs with `--network none`, the working tree mounted in, and the egress socket reached through a shared `guest` volume, with the same `memory relay` inside. Tested here on WSL 3.0.1: `--network none` leaves only loopback, a unix socket in a shared volume crosses between the two containers, a WSL distro path mounts by UNC name, and the seccomp filter refuses our inner user namespaces, which is why the container is the boundary there. The image is ours, built from a Dockerfile with `wslc build` and pinned by digest, so every machine runs the same harness versions, the same Node, the same tools: the environment stops depending on how someone's WSL happens to be configured. One image per harness or one for all five is a packaging choice, not a design one. The same rootfs can later serve the Linux backend too, pivoting into the image instead of mounting the host's `/usr`, which would make Linux and Windows runs identical rather than merely alike. The Windows build of `memory` orchestrates, first over the CLI, then over the C API in `wslcsdk.dll`, which is what Microsoft supports for production; no Rust binding exists yet, so that is a small FFI of ours. `wslc network create --internal` is the alternative if a network is ever preferred to a socket. Done: the image, `--backend wslc` for launch, prompted and task mode, proven here with Claude Code on a Max login answering a prompt and completing a ledger task from inside the image; the image workflow with daily pin checks and a weekly rebuild. Also done: the Linux backend pivots into the unpacked image (`memory image unpack`, then the default), so a Linux run and a WSL-containers run are the same environment. Not yet: the C API, the gateway mode through the VM | Windows without asking anyone to open WSL by hand, and a homogeneous environment: the harness runs in an image we control rather than in whatever a developer's WSL contains. Nothing of ours runs on the Windows side except orchestration, since the egress and the memory server stay Linux processes in the session VM |
| M0b, done 3 October | `memory savings`: the paper's read-path claims measured on the record itself. Recovery from T0 and a handoff against replaying the transcripts the sessions here left; a page of BM25 hits against the bodies and the corpus; known items by title in the top three; and the funnel, every read the server answered logged with its tokens (`state/reads.jsonl`) beside what each session's model calls ingested. Shown, not asserted: the number comes from this machine's own sessions | The paper says ingestion dominates and summaries-first retrieval pays; a demonstration has to put a measured number next to the alternative it replaced, on real sessions, or it is a slide |
| M5d, to decide | What a handoff does to the task it was written for. Today a task the harness did not complete goes back to the pool unchanged with the handoff attached, and the next claimant reconciles the two. The alternative under discussion: a handoff spawns a continuation task whose body is what remains, pointing at the original with `continues`; the original leaves the pool as `continued`, neither claimable nor closed; completing the last link completes the chain, so the original is marked complete with the final handoff. When a chain dead-ends, after a bounded number of attempts or an explicit invalidation, the original is re-admitted with the chain's handoffs attached as lessons: its spec untouched, so the intent survives, and the attempts visible, so the next claimant starts from what was learned rather than from zero. Restoring it unchanged would throw the attempts away; amending its body would lose the original ask. Open: the attempt bound, whether a continuation inherits the original's requirements and priority, and how the ledger shows a chain | A task is a promise and a handoff is a report; keeping them distinct objects keeps both the intent and the history legible, which is what the record is for |
| M4 | Browser reader over an exported bundle, SQLite FTS5 compiled to wasm | Distribution by URL for the person's own memory |
| M4a, first cut done 2 October | A web terminal on the launcher: `memory web` serves xterm.js and, per browser tab, allocates a pseudo-terminal, runs `memory launch` in it, and streams it over a websocket. One tab is one session with its own memory server; the M4 reader sits beside it. Three requirements. Scrollback works: the server keeps each session's output in a ring buffer and replays it on reconnect, so a page refresh, a dropped websocket or a second browser lands in the same place; the harnesses we run draw into the normal screen, so the browser's scrollback is the terminal's, and resize is relayed to the PTY. A reading pane beside the terminal renders Markdown with Mermaid diagrams, for files in the working tree and for memory entries, which are Markdown already; a path or `mem-` id printed in the terminal is a link that opens there. The harness shows something by writing it: an entry of kind `artifact` through the memory server, which the pane opens as it lands. Artifacts are then in the record, versioned, labelled and exportable like a handoff, rather than loose files; a file watch on the working tree is the fallback for harnesses that write files anyway. The whole thing feels like a Claude Code session, not a raw shell. Loopback only at first, reached over SSH when remote; identity in front of it comes with llmao, not here. Portable PTY so it holds on Linux, macOS and Windows. Done: `memory web` with sessions in pseudo-terminals, the server-side ring replayed on reconnect, resize relayed, a reading pane rendering Markdown and Mermaid for entries as they land, `mem-` ids as links, and starting interactive, prompted and task-mode sessions from the page. Not yet: artifacts as an entry kind the harness pushes, vendored front-end libraries so the page works offline, file watch on the working tree | Where the launcher runs stops mattering: laptop or shared host, the browser is the same, and the sandbox, egress and record stay on the machine that runs the harness. This is the piece Gofannon orchestrates later |
| M5, done 2 October | Five harnesses behind `memory launch`: Claude Code, Codex, Gemini CLI, Copilot CLI, OpenCode, plus Grok as a destination through `--provider xai`. Copilot has no base-URL override and goes through a CONNECT tunnel to an allowlist, which is pinned egress without metering. Codex's ChatGPT login and Gemini's Google login have their own destinations, `chatgpt` and `google`, because each talks to a backend that is not the public API; both are forwarded and metered, with a tunnel only for token refresh. Next: Cursor's agent CLI, Aider, Goose | The launcher's shape held: one adapter per harness; the sandbox, egress, and memory did not change |
| M3c | Taps as WASI components talk to the wrapper through the component model's typed interfaces, in process; no gRPC at that boundary. Where a tap reaches out (GitHub, a tracker) it uses WASI HTTP under the grant's ceiling. gRPC stays for the wrapper's upstream hosts (M3b) | A component boundary is a function call with capabilities, not a network hop; adding a wire protocol there would add a process and a socket for nothing |

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
