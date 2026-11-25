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
| launcher | The part of `amem` that starts a harness in a sandbox, with an egress | |
| wrapper | The memory server and the egress of one session together: the one confined process that a harness talks to (PLAN.md's word) | the launcher (for the wrapper) |
| launch | One `amem launch` or `amem work`, or a profile: one sandbox, one egress and one session | run (for a launch) |
| session | The memory server's session for one launch, with an id, a session label and a transcript. The harness runs in it once, or once for each task with `--loop`; each of those is a harness run | conversation |
| sandbox | The confinement around a harness | jail |
| sandbox backend | One way to make a sandbox, the value of `--backend`: `namespaces`, `image` (namespaces in the harness image), `wslc`, `seatbelt`, or `none` (no sandbox). `auto` selects one for the platform | runtime (for a sandbox backend) |
| harness image | The OCI image that holds the harnesses, Node, Python and `amem`, for the `image` and `wslc` sandbox backends | the image (alone) |
| working tree | The directory that the harness may read and write, `--workdir` | workspace, project directory |
| egress | The one network path out of a sandbox. It carries MCP to the harness, and forwards or tunnels model calls | proxy (for the egress) |
| gateway | A model gateway outside `amem` that the egress forwards to, such as LiteLLM or llm.apache.org | proxy (for a gateway) |
| relay | The process in the sandbox that connects a loopback port to the egress socket | |
| web launcher | The loopback web server that `amem` with no command, or `amem web`, starts, and the pages it serves | web terminal, the UI, dashboard |
| curator | An agent that does a curation task. `amem curator` is the survey of the curators: it reads the record, with no model, and files curation tasks for `amem work` to do | librarian |
| auditor | `amem audit`: a model that is not the model of the curators. It judges summaries against their bodies, looks for contradictions and reads held writes. It writes nothing to the record | checker |
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
| policy | The Rego rules that decide each action: write, read, erase, approve, declassify, promote, file a task, spend and fetch. The root's `policy/*.rego` replaces the built-in rules | rules engine |
| perimeter | The policy, the grants and the parts that enforce them, as one part of the design | |
| stopped (session) | A session whose grants changed after it started. Each read, decision, tool call and model call of the session is refused, and a new session starts under the grants as they are | revoked session, stale session |
| policy stamp | `state/policy.stamp` and the digest of the root's `policy/*.rego`: what a session compares to know that the policy changed | |
| rule | One policy entry in the record: an id, a layer, the rule, its intent, an owner and, where a machine can check it, a module | |
| status (of a rule) | Where a rule is in its life: `proposed` (in no decision), `advisory` (reports, refuses nothing), `enforced` (refuses) or `retired` | state (of a rule) |
| ceiling | `[gates] max_enforced_rules`: the largest number of enforced rules in one domain that need no sign-off | cap (for rules) |
| reset | `amem policy reset`: the owner retires each rule of a domain that no person wrote, promoted or approved | purge |
| amendment | A task for the owner of a rule, with evidence, that asks for a change to the rule | |
| gates | The `[gates]` rules on a write: how many entries one session may supersede, and whether a verified entry needs an approver | |
| version (of an entry) | The text of an entry at one time. Each write and each promotion makes a new version. The digest of the text names the version: `<id>@<digest>` cites one version, and an approval names one | revision |
| approval | An event on the ledger: a person approved one action on one version of one entry, for one principal, until a time. The policy checks it when the action runs | permission (for an approval) |
| guard | The person who approves a copy of an entry at a lower label: `guard` in the grant of the domain, or the owner | |
| declassification | A copy of one version of an entry at a lower label, written under an approval of the guard. The copy cites that version, and its origin names the guard. The source does not change | downgrade, lowering (of an entry) |
| sign-off task | A task that the service files when an action has no approval: it asks the approver for one approval, of one action on one version of one entry, for the principal that asked | approval request |
| scan | The check of a write and of a tool's answer for instructions, secrets, hidden text and the like. The scan finds; the policy decides | |
| label | `public`, `internal` or `restricted`, in that order | level, tier (for a label), sensitivity |
| classification | The label of an entry | |
| handling | The highest label that a session may read | clearance |
| session label | The highest label that a session has read. It goes up and does not go down | taint level |
| ceiling | The highest label that a destination or a tap may get | limit (for a ceiling), cap (for a ceiling) |
| destination | A place that the grants name for model calls: a model provider or a gateway | target (for a destination), endpoint (for a destination) |
| trust tier | `raw`, `curated` or `verified`: how far an entry has been checked | label (for a tier) |
| task state | `proposed`, `open`, `blocked`, `claimed`, `done` or `invalidated`: where a task is on the ledger | admitted (for `open`), completed (for `done`), status (for a state in prose) |
| generation | The number of a claim of a task. Each claim gets the next number. A heartbeat, a completion and a release carry it, and the ledger refuses an earlier one | attempt (for a generation), fencing token |
| late handoff | A handoff that a holder leaves on a task after its claim is over | |
| attempt key | A key that a caller makes for one claim or one completion. The same key asked again gives the same result, and the ledger does the operation one time | idempotency key, request id |
| outside event | Something that occurs outside the system and that a task waits on: a check that completes, a reply, a change that lands. A tap or a person reports it by name | signal, trigger |
| external action | A change that a holder makes outside the record under its claim. The holder records it on the task before it makes it | side effect |
| settle | To record what became of an external action: the destination holds it, or does not | reconcile, resolve |
| task definition | The record in the platform domain for one kind of work: its purpose, its tools, its acceptance checks and its outputs. A task names one as `<name>@<version>` | task type, task template |
| position | The record for one standing role: an entry of type `position` with a charter and an authority envelope, named `pos-<name>`. It is in force when a person verified its entry | seat, role (for a position), slot, post |
| charter | The part of a position that gives its standing orders, the task definitions that it covers, and the agent definition that fills it | mandate, job description |
| authority envelope | The part of a position that gives the most that a session in it may do: the tools and the handling. "The envelope" after the first use | scope (for the envelope), permissions |
| agent definition | The name of one kind of agent: the name in the author of a session, `codex` for `agent:codex`. An `agent` artifact of the platform domain has the same name. A charter names one | agent type, persona |
| pin | The field of a task that names a position. Only a session that fills the position claims a pinned task. To pin a task is to file it with a pin | assignment, binding |
| pin reason | Why a task is pinned: `continuity`, `relationship` or `separation` | pin type |
| commitment | A task that is pinned to a position, is admitted and is not closed | obligation, duty |
| handoff cache | The newest handoffs that cite a position, which a session in the position gets first in T0 | handoff history, position memory |
| seconded session | A session that fills a position under an agent definition that the charter does not name. It reads the position and does its tasks, and it does not write the position's history | substitute (for a seconded session), stand-in |
| fill (a position) | What a session does when a launcher puts it in a position: it then claims the position's tasks under the authority envelope | occupy, take, assume |
| wake (a position) | To start a session in a position that has a task that waits and no live session | spawn (for a wake), summon |
| retire (a position) | What the owner does to end a position: its tasks lose their pin, and its history stays in the record | delete (for retire), close (for retire) |
| quarantine | `state/quarantine`, where the policy holds a write that it refused, for the owner to release or drop | |
| owner | The person who holds the root and its grants | user (for the owner), admin |
| event log | The log of a domain, `state/log/<domain>.jsonl`, to which `amem` only appends: each write, version, refusal, erasure, read and send, and a copy of each event of the ledger, in order. An event holds ids and digests, and no text. The paper names it the historian | historian, audit log, journal |
| security role | The owner, and each author that `security` in `grants.toml` lists. Only this role reads the quarantine, the event log of a domain, lineage and the list of withheld entries | security team |
| send event | An event in the event log of a domain for content that left the perimeter in a session: a handoff that a tap posted, an export, or a model call that the egress recorded as answered. It holds the place and a digest, and no text | |
| lineage | The answer to one question about an entry, from the event log of each domain. Back: the entries that its session read before it wrote the entry, and the entries that it cites. Forward: the entries and the send events that came after a session read it | provenance |
| informed-by edge | A step of lineage between two entries: a session read the first entry before it wrote the second. Each read of the session before the write counts | derived-from edge |
| cited edge | A step of lineage between two entries: the second entry names the first in `cites` or in `supersedes` | reference (for a cited edge) |
| read set | The entries that a session read before one of its events: the `read` events of that session that are before the event in the log | context (for a read set) |
| outside content | Content from an author outside the fleet: the answer of a tool, a raw entry with tap reach, or a task that a tap filed. A task that a session filed after it read outside content is outside content too. The event log marks each event that a session appends after it read outside content | untrusted content, tainted content |
| raw reach | Who reads an entry while it is raw. An entry has `reach`: `chain`, `fleet` (the default) or `tap`. A read filter has `raw_reach`: `chain` (only the raw entries of the task chain), `fleet` (those, and each raw entry whose reach is not `tap`) or `all` (each raw entry) | visibility (for raw reach), scope (for raw reach) |
| held task | The task that a session works. The launcher sets it for the session, in `MEMORY_TASK` for a memory server on stdio | current task, active task |
| task chain | The held task of a session, the task that it was filed from, the tasks that were filed from it, and the tasks that it depends on. A session reads each raw entry that names a task of its task chain | task tree (for a task chain), lineage (for a task chain) |
| withheld entry | An entry with `status: withheld`. An erasure reached it by lineage, or its session read the erased entry and wrote it after the erasure. Its body stays in the record, and no search, list, T0, recall, get or bundle gives it. It stays withheld until a new entry replaces it, or the owner releases it or erases it | flagged entry, quarantined entry (for a withheld entry) |
| feed | The step that brings the index up to the event log of a domain: it takes each version that the log names above a cursor from the record, and moves the cursor. A writer does it before its write returns | sync (for the feed) |
| freshness bound | The longest time that a version in the event log of a domain can wait for the feed: 60 seconds. `freshness` measures a domain against it | index lag |
| projection | The read-only copy of the record's entries at `/memory` in the sandbox | |
| session settings | The values that a launcher gives a session one time, before the session reads. They make what the session reads smaller, and the session cannot change them | session config, switches, flags (for session settings) |
| arm | One fixed value of the session settings, for an evaluation: `no-memory`, `transcript-replay`, `t0-only`, `raw-only` or `full` | variant, condition, mode (for an arm) |
| snapshot | The time that a session reads the record at, `--as-of`. The session does not read an entry that was created after it | cutoff, checkpoint (for a snapshot) |
| disclosure depth | How far below T0 a session reads: `t0` (T0 and nothing below it) or `full` | depth (alone, for the session setting) |

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
| replay manifest | The part of a decision context and of a tape header that names what the session could read at that time: for each domain the offset of its event log (the number of the last event that names a version of an entry) and the number of entries in the index, the policy version, the digest of the grants, the session settings, the ranker and the digest of T0. On a decision context, it also names the entries that the session got after the decision context before | starting state (for a replay manifest), checkpoint (for a replay manifest) |
| replay state | What `amem reenact state` gives for a tape: its replay manifest, each part that is different now, the calls that an erasure removed, and `complete` or `incomplete` | |
| drift window | A time span for one destination and one model. It starts at the time that detected drift gives, and it ends when the owner closes it. An entry that an agent wrote in the span is not promoted, also after the owner closed the drift window | freeze (as a noun), quarantine (for a drift window) |
| freeze | The rule of a drift window: nothing that an agent wrote inside the window is promoted, also after the window closes | hold (for the freeze), lock (for the freeze) |
| plan | For an evaluation: the evaluation set file with a `[plan]` table. It fixes the tasks, the arms, the baseline arm, the number of seeds, the margin and the judge before the first run. The plan has a digest, and the tape of each run names that digest | protocol, design (for a plan) |
| seed | For an evaluation: the number of a repeated run of one task in one arm, from 0. Nothing gives the seed to the model | repetition, trial (for a seed) |
| pair | For an evaluation: one task and one seed with a run in an arm and a run in the baseline arm. The scorer compares the two runs of a pair | match (for a pair) |
| margin | The fraction that the success rate of an arm can be below the baseline, and the arm is still no worse. The plan fixes it | tolerance, threshold (for the margin) |
| replay trap | One of five conditions that make the result of an evaluation unsafe: `survivorship`, `leakage`, `noise`, `judge-drift`, `distribution-shift`. An open replay trap makes each verdict `inconclusive` | pitfall, bias (for a replay trap) |
| scorecard | The result of one evaluation: for each arm the paired differences from the baseline arm in success and in cost, the verdict, the five replay traps, and the drift window that the runs were in. The record keeps it as a decision entry in the platform's domain | report, result card |
| recall | The memory tool that answers a question with a cited brief, or with a pack of entries when no model is behind it | |
| deputy | The small model that the memory server calls through the egress for recall | |
| tap | A component that connects an outside system to `amem`. A tap with `tool = true` is a tool. Any other tap runs on a schedule, such as `github-issues` | connector, plugin (for a tap) |
| tool | A tap that an agent calls in a turn: `fetch`, `search` and the others. A tool server can also run `fetch` as native code | |
| hosted tool | A tool that a tool server runs, called through the egress | remote tool |
| source | What a tool brought in, kept under `.sources/` and cited as `source:<id>`. Transcripts are sources too | |
| transcript | The turns of one session, kept in the record | log (for a transcript) |
| trace | An OpenTelemetry trace | |

## Words with a second sense

The documents also use some terms above, and some words from the "Do not use" column, for
a different thing. Name that thing in full where it could be read as the term.

| Word | The other sense | Write |
|---|---|---|
| guard | The hook `.claude/hooks/guard-bash.sh`, which refuses some commands of a Claude Code session | the guard hook |
| budget | The tasks or tokens that one curation cycle may use (`--budget-tasks`, `--budget-tokens`), or LiteLLM's budget for each project | curation budget, LiteLLM budget |
| projection | The ledger built from the event log | the ledger's projection of the log |
| root | The root user, or the `/` directory, or the top of a URL | root user, root directory |
| runtime | A container runtime (podman, docker, wslc), or the Wasm runtime | container runtime, Wasm runtime |
| session | A WSL containers session, which is a VM, or an MCP session | WSL session |
| tool | A tool of a harness or a model API: shell, web, file and the like | the harness's tools |
| workspace | The Cargo workspace, or the workspace of Gemini CLI or Qwen Code | Cargo workspace |
| home | A home directory, such as the private home in the sandbox | private home |
| note | The outbox note that a hook leaves, which the launcher files as a handoff | outbox note |
| proxy | `HTTPS_PROXY`, LiteLLM's proxy, or a product with "proxy" in its name | the name in code font |
| user | A Unix user, or the person who gives an agent a request | Unix user |
| board | The task board: the `/board` page, and `--subagents board` | the board |
| run | One harness process in a session, one evaluation run, or one demo run | harness run, evaluation run |
| telemetry | OpenTelemetry data, or the curator's signals | OpenTelemetry, the curator's telemetry |
