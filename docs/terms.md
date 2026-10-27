# Terms

The words this repository's documents use for its parts and ideas. Each term has one
meaning, and each meaning has one term. Use the term in the first column. Do not use the
words in the last column for the same thing. A design term that is not here is in the
paper's [glossary](durable-memory-for-disposable-agents.md#appendix-f-glossary).

This list applies to the documents written in ASD-STE100 (see AGENTS.md). Add a term
here in the same commit as the first document that needs it.

## The parts

| Term | Meaning | Do not use |
|---|---|---|
| `amem` | The binary. It holds the launcher, the memory server, the egress, the web launcher and the command line | memoryd, the CLI (for the binary) |
| memory server | The part that serves the record over MCP: T0, search, get, write, the ledger, tools | the server (alone), MCP server (for this part) |
| launcher | The part of `amem` that starts a harness in a sandbox, with an egress | wrapper |
| sandbox | The confinement around a harness: Linux namespaces, a WSL container, or Seatbelt | jail, container (for the sandbox in general) |
| backend | One way to make a sandbox: `namespaces`, `wslc`, `seatbelt`. `none` is no sandbox | runtime |
| egress | The one network path out of a sandbox. It carries MCP to the harness and forwards or tunnels model calls | proxy (alone), gateway |
| gateway | A model gateway outside `amem` that the egress forwards to, such as LiteLLM or llm.apache.org | proxy |
| relay | The process in the sandbox that forwards from a loopback port to the egress socket | |
| web launcher | The browser page that `amem` with no command serves on loopback | the UI, dashboard |
| librarian | The agents that curate the record on a budget | curator (for the part in this code) |
| harness | A program that runs a model as an agent: Claude Code, Codex, and the others | agent (for the program), client |
| agent | A model that a harness runs, for one session | harness |
| tool server | `amem tool-server`: it runs tool taps for launchers on other hosts | |

## The record and its rules

| Term | Meaning | Do not use |
|---|---|---|
| root | The directory that holds the grants, the credentials, the record and the state | home, data directory |
| record | The entries, as versioned Markdown under `record/` | store, database, memory (for the files) |
| entry | One Markdown file in the record, with frontmatter | note, document, memory (for one file) |
| domain | An area of work with an owner. The record, the ledger and the grants are split by domain | namespace, project (for a domain) |
| grants | The file `grants.toml`: domains, labels, destinations and their ceilings, taps, spend limits | permissions, config |
| policy | The rules, in Rego, over the grants and the record's policy entries, that decide each read, write, call and fetch | rules engine |
| perimeter | The policy and everything that enforces it, as one part of the design | |
| label | `public`, `internal` or `restricted`, in that order | level, tier (for a label), sensitivity |
| classification | The label of an entry | |
| handling | The highest label a session may read | clearance |
| session label | The highest label a session has read so far. It goes up and does not go down | taint level |
| ceiling | The highest label that a destination may get | limit, cap (for a ceiling) |
| destination | A place the grants name that a call may go to: a model provider, a gateway, a tool server | target, endpoint |
| trust tier | `raw`, `curated` or `verified`: how far an entry has been checked | label (for a tier) |
| quarantine | Where a write that the scan stops is held for the owner | |
| owner | The person who holds the root and its grants | user, admin |

## Calls and tools

| Term | Meaning | Do not use |
|---|---|---|
| model call | A request from a harness to a model, through the egress | API call, LLM call |
| forward | The egress sends a model call on in the clear: it can check, meter and record it | proxy (as a verb), pass |
| tunnel | The egress opens a TLS connection to a named host and cannot see into it. It checks the ceiling only when the tunnel opens | |
| auth | How a launch pays for model calls: `key`, `gateway` or `subscription` | mode (alone) |
| subscription login | The harness's own login to its provider. The egress passes it through | |
| credential | A key or token that a destination or a tap needs | secret (for a credential), password |
| spend limit | The most a destination may cost in a period, from `[spend.<destination>]` | budget |
| decision context | The record of what a model call sent and got back, kept for replay | trace (for this), transcript |
| tap | A connector to an outside system. A ledger tap, such as `github-issues`, brings work in. A tool tap is a tool | connector, plugin |
| tool | A tool tap that an agent calls in a turn: `fetch`, `search` and the others | |
| hosted tool | A tool that a tool server runs, called through the egress | remote tool |
| source | What a tool brought in, kept under `.sources/` and cited as `source:<id>` | |
| transcript | The turns of one harness session, kept in the record | log (for a transcript) |
