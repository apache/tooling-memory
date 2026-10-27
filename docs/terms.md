# Terms

The words this repository's documents use for its parts and ideas. Each term has one
meaning, and each meaning has one term. Use the term in the first column. Do not use the
words in the last column for that meaning; a word there may still name a different thing,
as its note in brackets says. A design term that is not here is in the paper's
[glossary](durable-memory-for-disposable-agents.md#appendix-f-glossary). Where the paper
and this list differ, the documents written in ASD-STE100 use this list.

This list applies to the documents that AGENTS.md says are written in ASD-STE100. Add a
term here in the same commit as the first document that needs it.

## The parts

| Term | Meaning | Do not use |
|---|---|---|
| `amem` | The binary. It holds the launcher, the memory server, the egress, the web launcher and the command line | memory (for the binary), memoryd, the CLI (for the binary) |
| memory server | The part that serves the record over MCP: T0, search, get, write, the ledger's memory tools, and the tools | the server (alone), MCP server (for this part) |
| memory tool | An MCP tool of the memory server: `t0`, `search`, `get`, `write`, `recall`, `claim_next` and the others | tool (alone), operation |
| launcher | The part of `amem` that starts a harness in a sandbox, with an egress | wrapper |
| launch | One `amem launch` or `amem work`, or a profile: one sandbox and one egress, with one session, or one session for each task with `--loop` | run (for a launch) |
| session | One run of a harness in a launch, from its start to its exit, with an id, a session label and a transcript | conversation |
| sandbox | The confinement around a harness | jail |
| sandbox backend | One way to make a sandbox, the value of `--backend`: `namespaces`, `image` (namespaces in the harness image), `wslc`, `seatbelt`, or `none` (no sandbox). `auto` selects one for the platform | runtime (for a sandbox backend) |
| harness image | The OCI image that holds the harnesses, Node, Python and `amem`, for the `image` and `wslc` sandbox backends | the image (alone) |
| working tree | The directory that the harness may read and write, `--workdir` | workspace, project directory |
| egress | The one network path out of a sandbox. It carries MCP to the harness, and forwards or tunnels model calls | proxy (for the egress) |
| gateway | A model gateway outside `amem` that the egress forwards to, such as LiteLLM or llm.apache.org | proxy (for a gateway) |
| relay | The process in the sandbox that connects a loopback port to the egress socket | |
| web launcher | The loopback web server that `amem` with no command, or `amem web`, starts, and the pages it serves | web terminal, the UI, dashboard |
| librarian | `amem librarian`: a survey of the record, with no model, that files curation tasks for `amem work` to do | curator (for this part) |
| harness | A program that runs a model as an agent: Claude Code, Codex, and the others | client (for a harness) |
| agent | A model that a harness runs, in one session | harness |
| tool server | `amem tool-server`: it runs tools for launchers on other hosts | |

## The record and its rules

| Term | Meaning | Do not use |
|---|---|---|
| root | The directory that holds the grants, the credentials, the record and `state/`. `~/memory` unless `--root` or `MEMORY_ROOT` names another | home (for the root), data directory |
| record | Everything under `record/`: the entries and their earlier versions, and beside them the transcripts, sources, decision contexts and tapes. Search serves only the entries | store (for the record), database (for the record) |
| entry | One Markdown file in the record, with frontmatter | document (for an entry), memory (for an entry) |
| handoff | An entry of type `handoff`: what a session leaves for the next session | note (for a handoff) |
| domain | An area of work with an owner. The record, the ledger and the grants are divided by domain | namespace, project (for a domain) |
| task | One unit of work in the ledger: proposed, admitted, claimed, completed or invalidated | job, ticket |
| ledger | The tasks of each domain, with their leases and origins, on SQLite under `state/` or on PostgreSQL | queue, task list |
| grants | The file `grants.toml`: the owner, handling, the domains, destinations, taps, gates, spend limits and the ledger | permissions (for the grants), config (for the grants) |
| grant | One table in the grants: `[domains.<name>]`, `[destinations.<name>]` or `[taps.<name>]` | permission |
| policy | The Rego rules that decide each action: write, read, erase, promote, file a task, spend and fetch. The root's `policy/*.rego` replaces the built-in rules | rules engine |
| perimeter | The policy, the grants and the parts that enforce them, as one part of the design | |
| gates | The `[gates]` rules on a write: how many entries one session may supersede, and whether a verified entry needs an approver | |
| scan | The check of a write and of a tool's answer for instructions, secrets, hidden text and the like. The scan finds; the policy decides | |
| label | `public`, `internal` or `restricted`, in that order | level, tier (for a label), sensitivity |
| classification | The label of an entry | |
| handling | The highest label that a session may read | clearance |
| session label | The highest label that a session has read. It goes up and does not go down | taint level |
| ceiling | The highest label that a destination or a tap may get | limit (for a ceiling), cap (for a ceiling) |
| destination | A place that the grants name for model calls: a model provider or a gateway | target (for a destination), endpoint (for a destination) |
| trust tier | `raw`, `curated` or `verified`: how far an entry has been checked | label (for a tier) |
| quarantine | `state/quarantine`, where the policy holds a write that it refused, for the owner to release or drop | |
| owner | The person who holds the root and its grants | user (for the owner), admin |
| projection | The read-only copy of the record's entries at `/memory` in the sandbox | |

## Calls and tools

| Term | Meaning | Do not use |
|---|---|---|
| model call | A request to a model through the egress, from a harness or from the memory server | API call, LLM call |
| forward | The egress reads a model call and sends it on. It can check, meter and record the call | pass (for forward), proxy (as a verb) |
| tunnel | For a `CONNECT`, the egress opens a TCP connection to an allowed host, and TLS runs through it end to end. The egress checks the server name in the TLS hello and the ceiling when the tunnel opens, and sees nothing after that | |
| auth | How a launch pays for model calls: `key`, `gateway` or `subscription` | mode (alone) |
| subscription login | The harness's own login to its provider. The egress passes it through on a forwarded call, or tunnels the call | |
| credential | A key or token that a destination or a tap needs | secret (for a credential), password |
| usage | The tokens, calls and time that the egress counts for each model call it forwards | telemetry (for usage) |
| spend limit | The most tokens that one destination's model calls may use in one session, and across the root in one day, from `[spend.<destination>]` | budget (for a spend limit) |
| decision context | The record of what a model call sent and got back, kept for replay | trace (for a decision context), transcript |
| replay | Send a recorded decision context to a model again | |
| tape | The memory tool calls of one session, with their results | |
| re-enactment | Run a session again on its prompt, and answer its memory tools from its tape | |
| recall | The memory tool that answers a question with a cited brief, or with a pack of entries when no model is behind it | |
| deputy | The small model that the memory server calls through the egress for recall | |
| tap | A component that connects an outside system to `amem`. A tap with `tool = true` is a tool. Any other tap runs on a schedule, such as `github-issues` | connector, plugin (for a tap) |
| tool | A tap that an agent calls in a turn: `fetch`, `search` and the others. A tool server can also run `fetch` as native code | |
| hosted tool | A tool that a tool server runs, called through the egress | remote tool |
| source | What a tool brought in, kept under `.sources/` and cited as `source:<id>`. Transcripts are sources too | |
| transcript | The turns of one session, kept in the record | log (for a transcript) |
| trace | An OpenTelemetry trace | |
