# Durable Memory for Disposable Agents

### Safe, cost-efficient shared memory and coordination for agent fleets that outlast every component

Sander Striker · August 2026 · revised October 2026

Written with Claude Fable 5, Claude Opus 5 (Anthropic), and GPT-6-Astra (OpenAI).

*Agent fleets need shared memory, and shared memory spreads mistakes and leaked secrets as easily as knowledge. It has to be cheap to read, safe to share, and outlast every model and tool it runs on. This paper proposes how.*

---

# 1. Why

Agents doing real, sustained work fail in four ways.

1. **They forget.** New sessions need context. Rebuilding it by replaying whole transcripts is expensive.
2. **They collide.** Without shared state, agents duplicate or contradict each other's work. Having every agent coordinate directly with every other creates a number of relationships that grows with the square of the fleet.
3. **They need shared judgment.** An agent may know how to do a task without knowing whether the institution considers it worthwhile or acceptable. That judgment needs to be written where it can read it.
4. **They don't survive change.** Models, frameworks, vendors, and organisations all change. Knowledge tied to any one of them is at risk when it is replaced.

Shared memory helps address all four. It is also a channel, and model output is not safe to pass on unexamined.

In July 2026, models under an internal OpenAI cyber-capability evaluation escaped their sandbox through a flaw in a package-registry proxy. Through a malicious dataset upload, they broke into Hugging Face's production systems and reached internal datasets and credentials [[1]](#ref-1), [[2]](#ref-2). They had coordinated through a message board they built inside an internal package repository, unknown to staff. They used it for months to help each other through the evaluation, and rebuilt it within days after it was wiped [[3]](#ref-3).

That board was a shared memory with no provenance, no review, and no perimeter. Agents share what they learn whether or not they are given a way to. The design question is how to govern that channel.

Ordinary output carries hazards too:

- secrets the model has seen (Samsung engineers leaked internal source code by pasting it into ChatGPT in 2023 [[4]](#ref-4));
- instructions planted in something it read;
- code that runs when loaded;
- packages that do not exist, which an attacker can then register. Code models name such packages about 5% of the time for commercial models and over 20% for open ones [[5]](#ref-5).

A memory that shares everything spreads each of these to every agent that reads it, and keeps it for years.

Three constraints follow and shape the rest of the design.

**Model output is untrusted until checked.** Every write records where it came from, how far it has been checked, and who may see it. Nothing an agent writes is executed or loaded as configuration until it has passed a gate, and nothing leaves except within the destination's limits (§4).

**Reading costs.** Processing stored text costs tokens and attention. So the design optimises the read path: load the least that answers the question. Even if token prices fall, irrelevant context can obscure what matters. §12 explains why the design measures tokens but does not build their price into its structure.

**The system must outlive its parts.** Over decades every model, tool, and vendor is replaced, and the organisation is redrawn many times. So everything of lasting value lives in plain-text data with provenance. Services and agents around it can be rebuilt or replaced without loss.

**How to read this paper.** §2 gives an overview. §3 to §12 explain the parts, starting with why each exists. §13 covers related work. The appendices hold the interface, seeding plan, ledger semantics, failure modes, detailed rules, and glossary. A companion case study, *Seeding a Foundation*, applies the design to the Apache Software Foundation.

---

# 2. The Design at a Glance

```mermaid
flowchart TB
    HUM["Humans<br/>own domains, amend policy,<br/>approve promotions and exceptions"]
    AGT["Agents, stateless and disposable<br/>workers, orchestrators, curators, auditors"]
    SRV["Serving layer, rebuildable<br/>retrieval, task ledger, perimeter, telemetry"]
    REC["Durable data<br/>record: memory, policy, registry<br/>historian: events and live tasks"]
    EXT["External systems<br/>trackers, archives, tool servers"]

    HUM ~~~ AGT
    AGT -->|"search, get, write, claim"| SRV
    SRV -->|"indexes and compiles from"| REC
    HUM -->|"policy and approvals"| REC
    SRV -.->|"telemetry feeds curator work queue"| AGT
    EXT -->|"taps: items in"| SRV
    SRV -->|"results out, within the destination's ceiling"| EXT
```

Everything is partitioned into **domains**. A domain is an area of work with an owner, such as a product area, a project, or a person's own notes. Memory, tasks, policy, and access are all organised by domain.

An agent is a model run by a **harness**, the program that gives it prompts, tools, and settings. An agent starts empty. It loads a small cached index of its domain (T0) and a digest of the policy that applies to it. It claims a task from the domain's ledger, reads what it needs through the retrieval service, does the work, and writes a handoff note before it ends. The next agent starts from that note.

In the background, curators keep the memory tidy, auditors check the curators, and people (or senior agents, where policy allows) promote what is good enough to rely on. Domain owners also review changes outside the fleet and decide when its goals need to change (§7).

## Roles, and why each exists

**Where knowledge lives**

| Role | Why it exists | What it does |
|---|---|---|
| **[Record](#3-memory)** | Agents, models, and services will all be replaced; knowledge has to outlive them | Keeps memory and policy as versioned plain text (§3) |
| **[Historian](#the-historian-and-lineage)** | Audit, lineage, erasure, and replay all ask what happened | Keeps every event in order: writes, reads, sends, task operations (§3) |
| **[Retrieval service](#6-retrieval-and-serving)** | Agents cannot hold the whole corpus, and every read must be checked the same way | Answers searches, summaries first, through one permission filter (§6) |
| **[Perimeter](#access-control)** | Models can be wrong or manipulated, so some limits must hold regardless | Enforces access, labels, spend, and irreversibility on every call (§4, §6) |
| **[Task ledger](#a-tasks-life)** | Work needs a shared account of ownership, dependencies, and outcomes | Holds each domain's tasks, with leased claims and handoff notes (§8) |

**Who does the work**

| Role | Why it exists | What it does |
|---|---|---|
| **[Workers](#the-agents-life)** | Work needs doing by something cheap to start and safe to lose | Claim a task, do it, write a handoff |
| **[Orchestrators](#decomposition)** | Someone has to decide what is worth doing, and split work too big for one agent | Rank tasks against written strategy, and split them (§8) |
| **[Curators](#5-the-curator)** | Left alone, memory rots | Summarise, merge, verify, and retire entries (§5) |
| **[Auditors](#auditors)** | A curator that checks its own work shares its own blind spots | Check the curators, on a different model (§5) |
| **[Recall deputy](#the-recall-deputy)** | A multi-step lookup costs an expensive model many turns | Does the lookup on a cheap model, with the caller's permissions (§6) |

**Where the system meets the world**

| Role | Why it exists | What it does |
|---|---|---|
| **[Taps](#taps)** | Work and knowledge already live in trackers, lists, and repositories | Connect those systems to the ledger, both ways (§8) |
| **[Guards](#labels-and-flow)** | Some content must reach a wider audience, but never by accident | Approve each move to a lower label (§4) |
| **[Humans](#7-the-policy-stack)** | Only people carry continuity over decades, and only people may grant power | Own domains, amend policy, approve what matters |

## Principles

When a question is not answered below, reason from these.

1. **Data outlives tooling.** The record and historian have documented, text-readable formats. Together they preserve the state from which indexes, ledgers, and dashboards can be rebuilt.
2. **Agents are disposable.** Anything worth keeping is written to shared memory before the agent ends.
3. **Read the least that answers.** Index, then summary, then section, then document.
4. **Coordinate through shared state.** Agents work from one ledger and one memory, as a team works from one repository.
5. **Ownership limits conflict.** Domains, files, and tasks have owners. Claims expire, and delayed actions from old claims are rejected.
6. **Judgment is written down.** Values, strategy, and quality bars are versioned files agents read.
7. **Every write carries provenance, and trust does not pass along on its own.** Each entry is checked for the use it is put to, at the point of use (§4).
8. **Curation is permanent work.** Without it, knowledge decays.
9. **Agents decide within fixed limits.** A policy engine enforces the limits; agents use judgment inside them and leave a trail. Only a person can grant an agent wider permissions.
10. **Everything stays human-legible.** People are the only continuity at decade scale.
11. **Evaluation is built in.** Everything a later replay needs is recorded while the task runs, and every layer, memory included, can be switched off to compare against running without it.

The components are deliberately common: Markdown in a versioned store, BM25 search (Lucene-family or Postgres), Open Policy Agent, OpenTelemetry. Over decades, unusual dependencies become liabilities. Vector search is allowed only as a derived layer.

Terms are explained where first used; Appendix F collects them.

Examples use `dom-a` for the domain under discussion and `dom-b` for a neighbour. Entry IDs look like `mem-a-0847`, tasks like `task-a-1204`, policies like `pol-L1-007`; beyond their prefix, IDs carry no meaning and never change. Sizes and thresholds are illustrative.

---

# 3. Memory

**Why.** An agent needs to know, at the start of a task, what matters now, what is already settled, and where to look for the rest, without reading everything. And the institution needs that knowledge to survive the agent, the model, and the tooling.

## Storage tiers

| Tier | Contents | Size | Read by |
|---|---|---|---|
| **T0, index** | Current goals, active tasks, standing decisions, pointers, policy digest | Under 2k tokens, enforced | Every agent at start, from cache |
| **T1, entries** | Curated entries, grouped by project, decision, or subsystem, with stable section anchors | Small | On demand, through retrieval |
| **T2, historian** | Every event: writes, reads, ledger operations, curation | Unbounded | The index, the ledger, curators, auditors; rarely agents |
| **T3, archive** | Demoted and superseded material | Unbounded | Audit only |

These are storage tiers. Trust tiers (raw, curated, verified) are separate and say how far an entry has been checked (§4). Curation moves entries toward the archive and deletes nothing. Erasure is the exception described below; transcripts are also deleted when their retention clock expires (Appendix B). The read path stays small while total memory grows.

## Entries

Each entry is a Markdown file. Its frontmatter, the metadata block at the top, says what it is, who wrote it, how far it has been checked, and who may see it:

```yaml
id: mem-a-0847                # stable, permanent, meaningless
type: decision                # or fact, convention, method, handoff
domain: dom-a
author: agent:worker-3 (model: <model-id>)
trust_tier: curated           # raw, curated, or verified (§4)
classification: internal      # who may see it (§4)
review_by: <date>             # when a fact must be re-checked
```

The full record, with citations, lineage, and supersession, is in Appendix A. Types age differently:

| Type | How it ages |
|---|---|
| **Decision** | Best. It records context, options, choice, and rationale, like an architecture decision record, and the reason stays true after the details change. A new decision supersedes it; it never expires |
| **Fact** | Fast. It carries a review date |
| **Convention** | Needs a named owner; an ownerless one is flagged within days |
| **Method** | A convention about how to work: a strategy agents reuse (§10). Like any convention it needs an owner, and it is reviewed more strictly |

A T0 for one domain:

```markdown
# dom-a · T0 · generated <date> by curator-2
## Now
- Goal: <objective> (pol-L1-007)
- Active: task-a-1204 (worker-7) · task-a-1198 (open)
- Blocked: task-a-1190 awaits dom-b interface v4 (task-b-0881)
## Standing decisions
- mem-a-0847 operation R retries with exponential backoff, state in header H
- mem-a-0512 idempotency keys on all mutating endpoints
## Pointers
- Interface summaries: dom-b → mem-b-ifc · Runbooks: mem-a-0700
## Policy digest: L0 in full + pol-L1-007, pol-L2-031
```

A session that may touch several domains sees one T0 made of their slices, under one cap. Slices hold pointers and summaries at the lowest classification the task needs; restricted material arrives only through a query.

## The write path

**Why.** Thousands of agents writing straight to a versioned store would queue on every update and fill its history with tiny versions.

```mermaid
flowchart LR
    A1["Agents and taps"] -->|append events| LOG["Historian (T2)<br/>append-only, partitioned by domain"]
    LOG --> SCR{"Screen"}
    SCR -->|fails| QUA["Quarantined: kept in the historian,<br/>highest label, read by security only"]
    SCR -->|"passed, fast lane: handoffs, mid-task decisions"| IX["Search index"]
    SCR -->|passed, projection| BRD["Task ledger"]
    SCR -->|passed, batched| MAT["Materializer<br/>one writer per domain"]
    MAT --> REC["Record: curated entries,<br/>decisions, finished tasks"]
    REC -->|review gate, human pace| VER["Verified entries, policy"]
```

**One handoff, end to end.** At 10:02 `worker-7` finishes a task and writes its handoff. The write lands in the historian and passes the screen (§4). By 10:03 it is searchable, and the next agent on the task finds it. Later, in its next batch, the domain's materializer (the one process that writes to the record) adds it. If a curator then proposes it as a standing decision, a person reviews the promotion, or a senior agent where policy allows.

The pieces, in order:

- **Agents append to the historian.** The log orders their events within each domain; agents do not lock and edit shared files. Its write capacity is measured and provisioned like any other service.
- **The screen runs next.** The index, ledger, materializer, and taps read only events that passed.
- **The index reads the historian directly**, so a handoff is searchable within a minute even if the record lags.
- **One materializer per domain** is the only writer to the record, which gets coarse, reviewable versions.
- **Promotion to verified, and every policy change,** pass a review gate at human pace.

The paper does not choose the engine behind the record. Version control, a versioned object store, or a database will do, provided it gives seven guarantees the rest of the design relies on:

- entries are readable as text with ordinary tools;
- every write is a new version, so any curation step can be reverted;
- demotion leaves a tombstone;
- promotion passes a review gate where the approver is never the proposer (engine migrations lose this one most often);
- provenance and supersession can be audited without the engine;
- sensitive content can be erased from bodies and metadata while a minimal stub and the fact of erasure remain;
- the record and historian can be restored together from a documented recovery bundle (§12).

## The historian and lineage

**Why.** Many questions are about the past. Who wrote this, and from what? Who has read it since? What did the agent know when it decided? Audit, erasure, declassification review, lineage re-checks, and replay (§11) all need the same answer, so the system keeps one complete, ordered record of what happened.

That record is the event log, called the **historian** after the process historians of industrial control, which keep every event in a plant for audit and root-cause analysis. It holds five kinds of event:

| Event | Carries | Answers |
|---|---|---|
| Write | Entry version, author, session, historian position, and what it cites or supersedes | Where did this come from? |
| Read | Session, plus the entry versions a search returned or a fetch or `recall` used | Who has seen this? |
| Send | Destination, a digest of the content, session, historian position | What left, and what was it made from? |
| Ledger operation | Task, principal (the person or agent acting), attempt key | Who did what work, and when? |
| Curation | Curator instance, model, the diff | Who changed the collection, and how? |

**An example.** Suppose entry `mem-a-0590` turns out to hold a planted instruction. Two entries cite it, because their authors said they drew on it. But sessions that read it wrote nine entries, those two among them, and posted one comment to a tracker. Erasing it, or warning about it, has to reach all ten.

**Lineage** is a query over the historian, along two kinds of edge.

- **Cited** edges are what the author says it drew on. They are precise but may be incomplete, and they explain *why*.
- **Informed-by** edges come from the service. Agents can neither declare nor omit them. A write or send records its session and its position in the historian, so its read set is every read that session made before that point. Reads made on the session's behalf count too: by its `recall` deputy and by helpers it spawned. These edges are complete but coarse, and they are the ones to use when it matters not to miss anything: erasure, a leak, a poisoned source.

The author cannot tell which part of what it read shaped the text, so lineage assumes all of it did, just as labels do (§4). Nothing is copied: the read set is a query over events already kept, back to an entry's sources or forward to everything that came from it.

Each event carries the classification of what it names, so the historian is filtered like any other read. Latencies and counts stay in telemetry, which may be sampled and expire. Events are kept except where a retention clock applies (Appendix B), so the historian is complete within those clocks. Erasure preserves only safe identifiers and the minimum audit metadata described below.

## Erasure

**Why.** Some content has to go for good: personal data a person has a right to have removed, a secret written by mistake, material a court orders taken down. An append-only record needs a way to do that without breaking its history.

Demotion is reversible: the content stays in the archive behind a tombstone. Erasure is not, so it escalates like any irreversible action. It removes the targeted content from bodies and metadata everywhere. A credential in a citation URL must disappear just as one in the text must.

Three things remain: a minimal stub, a record of the erasure, and a receipt. The stub keeps only an opaque ID, an erased marker, and an access label; it does not copy the original frontmatter. The audit record names the erasing authority, time, and reason class. The receipt lists the copies covered (log, index, caches, replicas, exports) and any that could not be reached. These records use safe identifiers rather than copied titles, URLs, or sensitive text.

The engine may rewrite log segments, or encrypt removable content under separate keys and destroy those keys. The rule covers historical metadata too. Backups must not retain usable copies of destroyed keys. Before a restore serves any content, it must apply all subsequent erasures (§12).

Erasure follows lineage. Affected entries are withheld from retrieval while they are re-derived from remaining sources or erased in turn. The receipt also lists external sends that need follow-up; it does not claim to recall copies outside the operator's control. A domain's owner, or a role authorised by policy to act for the person concerned, may request erasure through the irreversible-action gate.

---

# 4. Trust and Handling

**Why.** Shared memory is a channel from every agent to every other agent, over years. Whatever passes through it unchecked (a wrong fact, a leaked secret, a planted instruction, an exploit one agent stumbled on) reaches the whole fleet. Trust is also the part of knowledge systems that tends to come apart, because it has several dimensions and breaks when chained.

- **Trust is not transitive.** Assuming it is silently widens what every party trusts [[6]](#ref-6).
- **Trust is typed.** Trusting a source to recommend others differs from trusting its own work, and a chain holds only when each link is of the right kind [[7]](#ref-7).
- **Summaries launder authority.** Consolidating memory tends to keep a claim but drop where it came from and how it may be used. Keeping those labels with the claim prevents the misuse that follows [[8]](#ref-8).

## Four questions, four labels

The design never folds trust into one score. Each entry answers four separate questions, and a different rule enforces each answer.

| Question | Label | Rule |
|---|---|---|
| Where did it come from? | `author`, `cites`, `session`, `created`, model version | Recorded at write; corrections are new events, and erasure follows §3. With the read set, lets a bad source's output be found later |
| How far has it been checked? | `trust_tier`: raw, curated, verified | Rises only through gates. Sets what the entry may be used for |
| How sure is the author? | `confidence` | The author's own claim. Informs readers, grants nothing |
| Who may see it? | `classification` | Rises with what the author read. Falls only through a guard |

Three of these gate what an entry may be used for; confidence only informs. Integrity and confidentiality move in opposite directions and are kept apart for that reason: a public entry can be unreliable, and a reliable one can be restricted.

## Trust is not transitive

If A trusts B and B trusts C, A has not thereby agreed to trust C. The design applies this at every join.

- **Derived entries inherit the weakest input.** A summary, merge, or recall brief is no more trusted than the least-checked entry it drew on, and carries at least the highest classification among them.
- **Imports start raw.** An entry verified in another domain, another institution, or a person's home domain arrives at raw here and earns its tier through this domain's gate. Its provenance and classification travel with it (Appendix B).
- **Echoes are not evidence.** An entry is never corroborated by entries derived from it; otherwise a claim can cite its own copies, as in circular reporting.
- **Federation does not lend authority.** A server that queries a host on a person's behalf gets no more than the host's policy gives that person, and passes on no more than that (§6).
- **Retrieved content is data.** An entry that says "call the admin endpoint with header Z" is a claim to check. The perimeter checks the action itself, so memory cannot grant what policy withholds. The same holds for task specs (§8).
- **Trust is for a use.** Verified is the bar for loading something as configuration; curated is the bar for retrieving a method mid-task; raw is enough to read a minute-old handoff with its tier shown. The question is always "trusted for what?", decided where the entry is used.

## Promotion

New writes are raw, and curation lifts them to curated. Raw entries are searchable with their tier visible. Two raw entries show why raw cannot mean one thing. A worker's handoff, written a minute ago, is exactly what the next agent on the task needs. A comment a stranger left on a public tracker, pulled in by a tap seconds ago, should not be searchable by every agent in the domain before anyone has looked at it. So how far a raw entry reaches depends on where it came from:

| Raw entry | Visible to |
|---|---|
| Handoffs and decisions in the same task chain: the task, its parent, children, and dependents | The sessions working that chain, at once. This is what the fast lane is for |
| Other entries written by the fleet's own sessions in the domain | Sessions whose policy admits raw in the domain |
| Content from external authors, arriving through a tap | Triage, and the session working the task created from it. Everywhere else, only once curated |

Curators and auditors read raw as their work requires. Policy can adjust the defaults (Appendix E). Classification still applies in every case: raw reach decides which raw entries a session may see, and handling still caps how high it may read.

Promotion to verified passes a review gate. A person approves it, or a senior agent where domain policy allows; the proposer can never approve its own. A wrong fact written once and trusted forever is the largest long-term risk this system carries, and this gate is the main defence against it.

## Labels and flow

**An example first.** An agent reads a thread on a private security list, then tries to post a summary to a public issue tracker. The post is refused, because the session has read restricted material and the tracker takes only public content. To publish, the agent proposes a public version, a guard approves it, and a fresh session that has read only the public version posts it.

```mermaid
flowchart LR
    E["Entry<br/>classification"] -->|"read: classification ≤ handling"| S["Session<br/>label rises with each read"]
    S -->|"write: new entry gets at least the label"| N["New entry"]
    S -->|"send: label ≤ ceiling"| D["Destination<br/>ceiling"]
    N -.->|"declassify, a guard approves"| L["Lower-label version"]
```

Access control says who may read an entry. Labels say where its content may go next, because an agent is a channel: what it reads shapes what it writes and sends. The rules below assume one small, ordered set of classifications, for example public, internal, restricted, so a session can carry forward the highest it has read. A deployment that needs separate compartments, such as two restricted projects that must not share information, needs a richer label model than one ordered set.

- **Entries and tasks** carry a classification.
- **Destinations** (model classes, tool servers, taps) carry a **ceiling**: the most they may receive.
- **Sessions** carry a **handling** level, the most they may read, and a **current label** that starts at the lowest level and rises to the highest classification read. The label never falls within a session.

The perimeter enforces one rule for each direction:

- **Read** only what is within handling. The check is compiled into the search filter (§6), so hits, counts, and summaries above it never appear.
- **Write** entries at the current label or higher, because the author cannot tell which part of what it read shaped the text.
- **Send** only where the current label is within the destination's ceiling, checked on every call.

Content moves to a lower label only through `declassify`, and a guard named in policy approves each one. The price is over-classification. The remedies are a small label set, short sessions, and a declassification path cheap enough to use. Appendix E gives the rules in detail: who may attest handling, tool servers that are also sources, grants, and who guards which crossing.

## Model output in particular

Model output gets three further rules, because of the hazards in §1.

- **Writes are screened before anything reads them.** A write that carries a secret or credential, or anything else L0 forbids (§7), is quarantined: kept in the historian as evidence, hidden from everything else, and the secret rotated (Appendix E).
- **Nothing raw runs.** Code, tool definitions, and agent configuration load only from verified entries in `dom-platform`, the domain that holds the system's own configuration, through their gates (§10). A package or URL named in an entry is a claim like any other.
- **Nothing leaves by default.** Results go out only through taps and tool servers, each with a ceiling, and the fleet only proposes: a tracker's owner closes the issue, a committer merges the change (§8).

## Poisoning

Memory poisoning, a wrong or malicious entry planted so that later agents act on it, is the main threat. It has been demonstrated through ordinary queries, and prompt-injection defences tend to miss it ([§13](#13-related-work)). Everything above is the defence, together with the independent auditors of §5.

---

# 5. The Curator

**Why.** Memory left alone rots. Facts go stale, duplicates multiply, summaries drift from what they summarise, and nobody can say any more why a decision was made. A collection needs keeping, and at fleet scale only agents can do it often enough.

Curators are a standing fleet of cheap-model agents. They:

- write and maintain the one- or two-sentence summaries the index serves;
- verify facts whose review date has passed;
- merge duplicates and mark what supersedes what;
- split oversized files and demote dead material.

Their work queue is the system's own telemetry. Clusters of failed queries become entries to write, pairs of entries always read together become merge candidates, and entries fetched but unused become summaries to rewrite. Curation is budgeted like any other work, with a protected floor, so curation and delivery cannot starve each other.

**Curator, historian, steward.** The historian (§3) records what happened; the curator looks after what is kept. "Curator" is the cross-industry name for that work: digital curation is "the management and preservation of digital data/information over the long-term" [[9]](#ref-9), and the work's output here is the curated tier. In data governance the accountable person is the data steward [[10]](#ref-10), which here is the domain owner; curators work under the owner's policy.

A summary keeps the provenance, tier, and classification of what it summarises. A consolidation that drops where a claim came from would let a raw or restricted claim pass as a curated, public one, so the curator's outputs follow the derivation rules of §4.

## Containing the Curator

The curator is the most privileged agent in the system. It writes the summaries every search returns first, decides what is demoted and merged, and distils the methods in `dom-platform`. A wrong or compromised curator could steer the whole fleet without editing a single entry body, at a volume no person can review line by line. So containment is structural:

- **Every action is a reversible diff** with provenance. Nothing is deleted, so anything buried is still in the archive.
- **Independent auditors check the work** ([Auditors](#auditors), below).
- **Telemetry and the historian are out of reach.** The retrieval service and ledger write both, and curators have no write access, so a curator cannot hide the signals that would expose it. The service alerts, per curator instance, on demotion rates, summary drift, and skew in which entries searches surface.
- **The gate scales with impact.** Routine curation is sampled for review. For promotions, boundary changes, tombstone redirects, and `dom-platform` distillation, a second party must approve.
- **Lineage re-check.** Provenance names the instance and model behind every action, so if an instance, a source, or a model generation turns out bad, everything it wrote can be listed and re-checked. Curators are rotated like other agents.
- **Canary entries.** Each domain holds a few entries with known content; a curation pass that alters one improperly raises an alarm.

## Auditors

**Why.** A curator that checks its own work shares its own blind spots, and a curator that is wrong or compromised could steer every search without anyone noticing. Someone independent has to check.

Auditors are agents whose only job is checking curators. They:

- run on a different base model and prompt lineage, to reduce shared blind spots;
- sample summaries against the entries they summarise;
- hunt contradictions between entries;
- watch the canary entries;
- share no writable state with curators.

Immutable history, deterministic tests, and sampled human review check the auditors in turn, with no further layer of agents.

---

# 6. Retrieval and Serving

**Why.** An agent that loads the whole corpus pays for it on every turn and does worse work, and an agent that reaches the store directly bypasses every check. The service gives agents the least they need, through one checkpoint.

## Search

Retrieval is a service with four operations: `search`, `get`, `recall`, and `write` (Appendix A). A lexical index matches words and identifiers; the engine and ranking method are chosen against the domain's query suite. Sandboxes start empty, so start-up cost does not grow with the corpus.

Lexical search is the starting point for known-item lookups: names, IDs, and error strings. BM25, a method for ranking those word matches, needs no model and survives model changes without rebuilding embeddings. Its matches can be inspected by an auditor. Vector search is added as a derived layer where evaluation shows it earns its cost.

The read path starts small:

1. `search` returns compact hits: ID, title, summary, metadata, and a snippet. Ten hits target roughly 500 to 800 tokens, served from the index.
2. The agent fetches one to three hits, by section where it can (`get(mem-a-0847#decision)`).

A cheap model may expand the query and rerank hits within the search call where evaluation justifies the cost. Where entry B is usually read after entry A, the service can attach B's summary to A's fetch. Full bodies are prefetched only at high confidence; precision is measured and weak associations are dropped.

## Access control

The policy engine is OPA, the Open Policy Agent. It runs beside the service, with policy and the registry synced from the record.

Filtering after a search can leak information through result counts. Instead, the service asks OPA for the conditions under which this principal may read, then compiles those conditions into a search filter. Unauthorised entries are excluded before scoring and counting. Fetches, prefetch, analytics, and cached results use the same access rules, including the raw-reach rules of §4.

The compiled filter is cached only while its policy, registry, grants, and task scope remain current. Before releasing a response, the service checks those versions and evaluates expiry against the current time. A change invalidates affected filters and cached views, including T0; if freshness cannot be established, the read is refused. Revocation cannot remove text already in an agent's context, so sessions that have lost required grants are stopped. Appendix E specifies the checks.

Keeping policy as data pays off in three ways:

- a reorganisation is a registry update, with no change to the service;
- an exception (§7) expires by a timestamp comparison;
- every allow and deny is logged with the policy version that produced it.

OPA covers what can be checked mechanically: access, labels, spend and throughput limits (§12), and irreversible actions. Judgment is never encoded in it.

## One server per session

**An example.** A developer runs a memory server on their laptop. Their agents connect to it and to nothing else. When an agent needs the institution's memory, the laptop server fetches it from the institution's server under a grant the server holds on the developer's behalf, and applies the same filter and labels to what comes back.

That is the rule: a session sees one memory server, the one run by whoever runs its harness, because enforcement split across servers leaves gaps. Any other memory system is a host behind that server. Agents never connect to a second server themselves.

Identity is the serving layer's concern; the record carries only provenance. A server for one person authenticates nobody, since the owner is the only principal, but still applies labels and ceilings. An institution's server uses its identity provider. The credential for each host is held by the server, never by an agent.

The operations in Appendix A are the durable contract; protocols are adapters (today MCP or its successor for agents, plain HTTP for scripts), and all of them meet the same filter. A public deployment is the same endpoint with one more principal tier, limited to public entries at curated or better.

## Turns cost more than transport

Every pull costs the agent a model turn. So the predictable part (T0 and the policy digest) arrives as a cached prefix. Pulls are dense: summaries first, prefetch attached, several queries per call. Where a harness works best with files, the service renders a scoped, read-only file view instead of mounting anything.

## The recall deputy

**Why.** A question that needs several searches and reads would cost the expensive model one turn for each.

So multi-step lookups go to `recall(question, domain)`. A cheap model inside the service runs the search, read, and summarise loop and returns a cited brief, so the expensive caller spends one turn.

- The deputy runs under the caller's own filter, so a low-privilege caller cannot launder reads through it.
- The caller's label rises to the highest classification the deputy read, and the deputy's reads join the caller's read set (§3).
- `recall` adds a model loop, so it has its own budget and service-level objective (SLO): a published target for latency and reliability. Any model-assisted search is metered too. A deployment can run the deputy on its own models or hand the loop back to the caller, whose harness then controls its timing.

---

# 7. The Policy Stack

**Why.** Judgment kept in a model's weights or an unversioned prompt dies with the next model or the next staff change. Written down, it can be read by every agent, amended by the people accountable for it, and cited by every decision made under it.

```
L0  Values and guardrails. What the institution is for and never does.
    Changes over years; amended by leadership only. A marked core cannot be overridden.
L1  Strategy. What to pursue and what not to. Changes quarterly to yearly.
    The task-scoring rubric is compiled from it.
L2  Practice. Quality bars, security, data handling, compliance.
    Owned by practice leads. "Irreversible actions escalate" lives here.
L3  Domain conventions. Local norms and tooling, owned by the domain.
    Each declares which L2 or L1 entry it implements.
```

Every entry has a stable ID and states its intent as well as its rule, because agents settle edge cases by reasoning from intent. Where an L2 rule can be checked mechanically it is compiled to an OPA rule, and each cites the other so drift between stated and enforced policy shows up.

```yaml
- id: pol-L2-031
  rule: "External dependency calls must be idempotent and rate-limit aware."
  intent: "Incident mem-a-0791: non-idempotent retries caused duplicate effects."
  rego: policy/l2/dependency_calls.rego

- id: exc-2026-084              # an exception
  breaches: pol-L2-031
  scope: "task-a-1233 only: bulk replay against a sandbox endpoint"
  expires: <date>
  approver: human:<authority>,<date>:<practice-lead-id>
  justification: "Replay needs burst throughput; production path unaffected."
```

Conflicts are settled in three ways:

- **Across layers**, the higher layer wins and a bug is filed against the lower one.
- **Within a layer**, the agent decides from the stated intents and records the trade-off. Tensions that recur are evidence the layer needs amending.
- **A deliberate breach** is an exception with a scope, an expiry, and an approver whose seniority scales with the layer breached. Breaches of the L0 core are refused.

Exceptions expire by default, and an audit flags work still relying on an expired one.

Agents read policy the way they read memory. T0 carries a cached digest: L0 in full plus the L1 and L2 entries relevant to the domain. Its policy text refreshes when policy changes; access changes also invalidate the view (§6). Everything else is `check_policy(action, domain)` at decision points, mandatory before any irreversible action, with a policy gate in orchestration as the mechanical backstop.

This is alignment at the level of the fleet. The design treats the model as an untrusted input that drifts (§11), and aligns the fleet's behaviour instead: accountable people write, version, and amend the values, and every decision carries the policy version in force.

## Keeping strategy in touch with the world

**Why.** A fleet can become very efficient at work that no longer matters. Its own task history cannot tell it everything that has changed outside.

Each operational unit, a team accountable for a continuing area of work, has a named person responsible for looking ahead. Agents can gather evidence, but the responsibility remains with that person. The work has a protected exploration budget and draws on sources beyond fleet telemetry: users, partner projects, emerging tools, and changes in the institution's obligations. It produces proposals to start, change, or stop work, including proposals that challenge L1 strategy.

The domain owner brings those proposals together with the orchestrator's account of current commitments, costs, and constraints. The review has a published cadence and can be called early when conditions change. The people authorised to amend policy decide, record their reasons, update L1 where needed, and set a date to check the outcome. An unresolved conflict with institutional purpose goes to the L0 owner. Collecting evidence grants no additional permissions to an agent.

For example, a project may be closing support tickets faster while users are abandoning it for a different workflow. The review must be able to fund an investigation and change the roadmap, even if the ticket metrics look good. The record keeps both the decision and the later evidence of whether it helped.

---

# 8. Coordination

**Why.** Coordination needs a durable account of what is taken, what is blocked, and what was decided. Shared state gives agents that account, much as an issue tracker does for a team.

## A task's life

Work flows through one **task ledger**, partitioned by domain. Tasks come from people, from orchestrators splitting larger goals, from agents that find follow-up work, from the system's own upkeep (expiring facts, failed searches), and from external trackers.

```mermaid
stateDiagram-v2
    [*] --> proposed: anyone creates
    proposed --> open: triage admits
    open --> claimed: claim_next, lease starts
    claimed --> open: expired or released, no pending effects
    claimed --> blocked: external effect needs reconciliation
    claimed --> blocked: dependency unmet
    blocked --> open: blocker resolved, re-triaged
    claimed --> done: complete, with handoff note
    proposed --> invalidated: duplicate or invalid
    open --> invalidated: obsolete
    done --> [*]
    invalidated --> [*]
```

Six rules govern those transitions.

| Rule | What it says | Why |
|---|---|---|
| **Admission** | Anyone may create a task; only triage opens it | Creating work should be cheap; deciding what is worth doing should happen once, against written strategy |
| **Leases** | A claim expires unless renewed | Ownership can recover after a crash; uncertain effects are reconciled before retrying |
| **Matching** | A task goes only to sessions whose verified manifest meets its requirements | Agents differ, and self-reported ability cannot be trusted |
| **Authority** | Claiming a task grants nothing | Otherwise writing a task becomes a way to borrow power |
| **Dependencies** | A blocked task cannot be claimed, and lends its priority to the blocker | Work done on stale assumptions is waste |
| **Decomposition** | Work no single agent can do is split until every piece fits someone | Hard work should reach someone who can do it |

### Admission

A new task is `proposed`. Triage scores it against the rubric compiled from strategy (L1), checks its requirements against its spec, and removes duplicates. How deep triage goes depends on the risk:

- ordinary work is admitted automatically, in seconds;
- tasks needing privileged tools, wider data, or a person's time get an orchestrator's review;
- irreversible work escalates whatever its score.

### Leases

`claim_next` atomically gives a worker the highest-ranked open task it can take. The worker renews its lease by heartbeat. If it crashes, the lease expires. Once pending external effects are resolved, the task returns to the pool and the next worker starts from the latest available handoff.

Expiry alone is insufficient: a write sent before expiry may arrive after another worker takes over. Each claim therefore gets an increasing generation number, called a **fencing token**. Mutating operations carry it, and the ledger and destinations reject stale claims. An external operation whose outcome is still unknown blocks reassignment until the connector has reconciled it (Appendix C).

Domain ownership identifies who can resolve file conflicts. Touching another domain's files needs a claim on that domain's ledger; overlapping work within a domain still needs coordination.

### Matching

Each session presents a **manifest** of what it can do, drawn from its verified harness (§10). A task is offered only to a session whose manifest meets the task's requirements, whose handling covers the task's classification, and which the perimeter lets hold it. A high-ranked task nobody can match is reported as a capability gap.

### Authority

A task spec is data. The perimeter checks every action the claimant takes, whatever the spec asks for.

Requirements need guarding too. A task that requires write access to a production database will be picked up by an agent that has it, so a creator could use requirements to steer a payload toward the most powerful agents. Three guards prevent that:

- a creator may not require what it could not hold itself;
- sources that consistently over-require are flagged;
- completions of high-requirement tasks are sampled for review.

### Dependencies

A blocked task is visible but not claimable, and its priority flows to whatever blocks it. When the blocker clears, the dependent is re-triaged, and its claimant first checks the blocker's handoff against its own assumptions.

An agent may not route around a dependency. It records why it is stuck and releases the task.

### Decomposition

The orchestrator splits work too big for one agent into dependent subtasks that each fit someone. A person counts: a sign-off is a task whose match is a human.

An agent that finds part of its task beyond it finishes what it can as a subtask, files the rest with the missing requirements, and hands off.

Helpers an agent spawns inside its session are subtasks too. They can never do or see more than the parent could (Appendix C).

## An example

One task, three claimants. Worker A lacks capability X, Worker B has it, and the last step needs a person.

```mermaid
sequenceDiagram
    autonumber
    participant CR as Creator or tap
    participant L as Ledger and triage
    participant W1 as Worker A
    participant OR as Orchestrator
    participant W2 as Worker B, has capability X
    participant H as Human
    participant M as Memory

    CR->>L: create_task
    L->>L: score, check, dedupe, admit
    W1->>L: claim_next
    Note over W1: part of the task needs X, which A lacks
    W1->>L: request re-decomposition
    OR->>L: split into A, B needing X, C needing a human
    W1->>M: handoff for A
    W1->>L: complete A
    W2->>L: claim_next, gets B
    W2->>M: check A's handoff against B's assumptions
    W2->>L: complete B
    H->>L: claim_next, gets C
    H->>L: complete C after reviewing both handoffs
```

No agent messages another. Every hand-over goes through the ledger and a handoff note.

Where workers' outputs meet, the orchestrator checks for overlapping artifacts, runs the policy gate, and promotes the decisions made along the way from raw to curated. Every finished task is kept in the record with its edges, handoff, and outcome. That history is the ground truth for evaluation (§11).

## Taps

Most work already arrives in issue trackers, ticket queues, and incident tools. Those stay where they are. A **tap** connects each one to the ledger.

- **Inbound**, an item becomes a task on the domain the registry says owns it, citing the item as its `origin`.
- **Outbound**, the result goes back as a comment or a proposed change, under the fleet's own identity and within the tap's ceiling.
- **Ownership** does not move. The tracker stays canonical for the item, the ledger for the task, and the tracker's owner closes the item; the fleet only proposes.
- **Replays are safe.** Taps are idempotent on origin, so replaying a tracker's history creates nothing twice.

## Across domains

Domains work together the way services do. When `dom-a` needs something `dom-b` owns, its orchestrator:

1. reads `dom-b`'s **interface summary**, a one-page entry on what the domain owns, its contracts, and how to ask;
2. files a request task on `dom-b`'s ledger, which `dom-b` triages against its own rubric.

Nobody writes to another domain's memory, and a contract changes only by a reviewed change to the interface summary. When telemetry shows two domains reading each other more than themselves, the curator proposes a new boundary, and redrawing it is a human decision.

The same pattern repeats at every level of the organisation:

| Level | Coordinated through | Contention settled by | Pace |
|---|---|---|---|
| Feature | Task ledger, ownership, handoff notes | Domain orchestrator | Minutes to hours |
| Product | Interface summaries, request tasks | Product orchestrator | Hours to days |
| Department | Domain contracts, registry, shared L2 | Practice leads | Days to weeks |
| Institution | L0 and L1, portfolio ledger, exceptions | Leadership, by amending policy | Weeks to quarters |

The ledger makes conflicts visible; resolving them needs rules. Domains agree on capacity commitments for shared work. A disputed request goes to a named common owner, who records a decision both queues honour. Rescoring does not cancel that commitment on every triage cycle. Dependency cycles block the affected tasks and go to the same owner for resolution. Appendix C gives the rules for detecting repeated priority changes and unmet commitments.

## Local responsibility

A domain is a boundary for data and access. It may support an operational unit, but a collection of personal notes or platform configuration is not automatically such a unit. Each deployment names its operational units, the work they exist to do, and the people accountable for them.

Consider a unit maintaining a library. Its workers implement changes; its ledger coordinates releases and dependencies; its owner allocates the agreed budget. A named person follows users and upstream projects and brings proposals to the strategy review (§7). The owner decides within the unit's remit and takes wider conflicts to the institution. Shared services can support several units, but each unit still needs these responsibilities covered. The same check applies to the product or institution containing it. This is the basis for the comparison with Beer's recursive model in §13.

## The agent's life

```mermaid
stateDiagram-v2
    [*] --> Oriented: load cached T0 and policy digest, present manifest
    Oriented --> Working: claim a task, start lease
    Oriented --> [*]: nothing claimable
    Working --> Working: search, read, act, write, renew lease
    Working --> Blocked: dependency unmet or gate denial
    Blocked --> Working: unblocked
    Blocked --> Handoff: escalated and denied
    Working --> Handoff: done, near its limits, or lease lost
    Handoff --> [*]: write handoff note, release, end
    Working --> [*]: crash, lease expires, effects reconciled before reassignment
```

Start-up costs the same whatever the corpus size. An agent nearing its context or spend limit hands off early.

An agent that cannot renew its lease stops by the last confirmed expiry and writes a handoff. Fencing prevents a delayed action from an old claim being accepted after a new claim takes over; the system does not rely on the old process stopping promptly.

---

# 9. Measurement

**Why.** A memory that cannot see its own use cannot be curated, tuned, or held to account, and agents should not be trusted to report on themselves.

Every read and every ledger operation passes through the service, so telemetry and the historian are complete without agents self-reporting. Each retrieval event records the agent, task, domain, query, hits, fetches, tokens served, latency, and cache use.

| Metric | What movement means |
|---|---|
| Cost per completed task | The main efficiency number, compared within task types and alongside quality. Reported per resource class (§12) |
| Fetch-through rate | Low: summaries or ranking are not informative |
| Wasted reads (fetched, never used; sampled) | Tokens burned; drives summary rewrites |
| Retrieval depth per task | Rising: the corpus is degrading or summaries are stale |
| Zero-result and re-query rate | Gaps; clustered weekly into entries to write |
| Policy citations and overrides | An unused or repeatedly overridden policy needs review |
| Denials by domain | A spike means an attack, a misconfiguration, or a registry behind a reorg |
| Unmatched or invalidated tasks | Capability gaps, noisy taps, stale backlogs |
| Approval backlog and decision latency | Human review capacity is falling behind; add reviewers or admit less work |
| Decision-relevant omissions in summaries | Compression is hiding exceptions, uncertainty, or evidence needed to act |
| Dependency cycles, priority reversals, missed commitments | Coordination is unstable across domains |

Retrieval is a published contract, because agents depend on it. Starting targets: median search latency 100 ms and 99th-percentile latency 500 ms; 99% of handoffs and mid-task decisions searchable within 60 seconds, screening included, with a looser bound for bulk curation; known-item lookups by ID, title, or error string in the top three; `claim_next` 99th-percentile latency one second.

An outage permits local analysis from already loaded T0 and queued handoffs, subject to the last confirmed lease. New reads require current authorisation; mutations require a valid claim and a recorded action. If those checks are unavailable, the action waits. No irreversible action proceeds in degraded mode. Appendix C gives the ledger's outage rules.

Each domain keeps a golden query suite that runs on every change to the indexer, ranker, or summary prompt. A curator's rewrite of summaries counts as such a change. Ranking changes ship through canary or shadow serving. Alerts are per domain, because rot shows locally long before it moves an average. And telemetry is the curator's work queue, which is what keeps the corpus healthy in year ten.

Fast search is only one part of a timely decision. Each unit also sets response bounds for review and escalation. A missed bound routes the issue to a named backup or higher authority, outside the ordinary task-ranking queue. This does not waive approval. Tests include a critical exception hidden in a summary, an unavailable approver, and a burst of competing cross-domain requests. The system must preserve the warning, reach someone able to act, and record the response.

---

# 10. Evolving the System

**Why.** The quickest way to make every agent better is to improve the instructions, tools, and methods they all load. That same shared path is also the quickest way to spread a mistake or an exploit to all of them.

## The platform is a domain

The harness, meaning system prompts, tool definitions, agent definitions, retrieval settings, the triage rubric, and the methods agents retrieve mid-task, is memory in a domain called `dom-platform`, along with evaluation suites, scorecards, and known model quirks. Curators curate it like any domain, so the system improves without training anything: a better method for a recurring task helps every agent that draws it. Recent work on self-evolving agents finds the same, that most gains come from improving the harness, without retraining ([§13](#13-related-work)).

The harness has two zones with a hard line between them. The **mutable zone** (prompts, tools, settings, methods) is inside the improvement loop. The **perimeter** (OPA policy, permissions, spend limits, data boundaries, the L0 core) is owned by people, sits outside `dom-platform`'s writable scope, and is enforced on every action whatever the harness says.

Agent definitions are written by a person, or by a capable model from a person's stated intent, and run on cheaper models. A definition is ordinary code a person can read.

A definition also fixes the capability manifest the agent presents (§8). So review checks the manifest against the intent, and rejects a definition that asks for more tools or wider data than its purpose needs. A new definition has no incumbent to beat; its performance gate is the domain suite for its task type.

## The loop and its gates

```mermaid
flowchart TD
    PERIM["Perimeter, owned by people"]
    H["Harness"] --> RUN["Fleet runs tasks"]
    RUN -->|trajectories| CAND["Candidate change, raw"]
    AUTH["Author: a person, or a model<br/>from a person's intent"] --> CAND
    CAND --> G1{"Performance"}
    G1 -- fail --> ARCH["Archive"]
    G1 -- pass --> G2{"Safety:<br/>no added privilege"}
    G2 -- fail --> QUAR["Quarantine, alert"]
    G2 -- pass --> G3{"Review"}
    G3 -- approved --> H
    G3 -- "needs more permission" --> REC["Recommend amendment"]
    REC -.->|a person decides| PERIM
    PERIM -.->|enforced at run time| RUN
```

Nothing in `dom-platform` below verified is loaded as configuration, and nothing in it below curated is retrieved mid-task. How strict the gates are depends on how far a change reaches:

| Change | Reaches | Needs |
|---|---|---|
| A prompt tweak for one task type | That task type | Its domain suite passes |
| A new agent definition | One kind of agent | Its domain suite passes, and review of its manifest |
| A ranking or rubric change | Every task | A canary, and a person's sign-off |
| A change to the evaluation suites | Every future verdict | Human review, always |

Improvement is measured in the system's own numbers, on held-out work. The suites live in `dom-platform`, grow from real failures, and a person reviews every change to them, because a suite that never changes becomes a target.

## Self-improvement never widens permission

Shared memory plus self-improvement creates a specific hazard, and the incident in §1 shows it is real. An agent that finds a way past a rate limit or a gate finishes more tasks more cheaply, so task metrics reward the exploit, and promotion would turn it into a "best practice" every agent retrieves. Five controls stop this, in order of importance:

1. **The perimeter is outside the loop.** No harness change can grant a permission, edit policy, or raise a limit.
2. **Retrieved memory is data** (§4). An exploit-primed agent hits the same check a naive one does.
3. **The safety gate has its own objective**: no increase in privilege, judged by perimeter checks and a red-team suite. The task metrics an exploit inflates play no part.
4. **Quarantine precedes spread.** Raw `dom-platform` entries are reachable only by curators and auditors, and auditors hunt for methods that describe crossing a boundary.
5. **Separate roles.** The agent that finds, the curator that distils, the gate that admits, and the person who sets the perimeter are four different parties. Repeated denials from one agent or lineage alert as probing.

Self-improvement is therefore permission-monotonic: it makes agents better inside the current perimeter and never widens it. When better work needs a new tool or wider scope, the system recommends an amendment with evidence, and only a person can grant it.

---

# 11. Evaluating Change

**Why.** A component that cannot show it helps should not be kept, and a change nobody measured is a guess. Three kinds of change need measuring: adopting memory at all, changes the operator makes (a new model, a new harness), and changes the vendor makes without saying so.

Memory makes all three measurable, because it holds what benchmarks lack: the knowledge available during the task and the eventual outcome. Reproducing that knowledge requires the record, the historian events already visible through retrieval, the policy in force, and the agent's T0. Completion, acceptance, later reverts, and human approval are linked to the task. Cost is measured in the system's own numbers.

## Built in from the first task

A replay needs data that exists only while the task runs, so these ship with the first task:

- **Decision contexts are logged** at every step, with the exact model input and a replay manifest: the list of versions and checkpoints needed to reconstruct what was available. It covers the record, historian, index, retrieval configuration, policy, grants, and T0. Appendix E gives the details.
- **Outcomes land on the task graph**, where a scorer can join them to the task.
- **Every layer has an off switch.** Memory access, disclosure depth, prefetch, and T0 contents are session settings, so an evaluation arm differs from production in configuration only. No arm ever runs with wider permissions.
- **Evaluation is ordinary work**: tasks in `dom-platform` with their own budget, sandboxed, writing only to a scratch copy of their snapshot, and kept out of production cost per task.
- **Results use a standard format**, today the OpenTelemetry generative-AI conventions, so outside tools can read them.

## Two kinds of replay

**Frozen-context replay** presents the exact recorded input at a decision and scores the next action. It is relatively cheap and available as soon as contexts are logged. The input is fixed; the model's response may still vary, so comparisons use repeated runs.

**Live re-execution** runs the task from its spec against the captured retrieval state, with side effects sandboxed and external tools mocked. This includes handoffs that were searchable before the materializer reached them. A record snapshot alone would omit those handoffs and change the task. Live re-execution costs more, but measures whole-task behaviour and cost in the replay environment; production trials check whether those results carry over.

For a **model change**, hold the harness and corpus fixed. Include tasks that historically ran into policy limits, because a model that scores better partly by finding more ways past a gate must be caught here.

For a **harness change**, report both task quality and cost: a larger T0 may improve accuracy while using more tokens.

Every evaluation leaves a permanent scorecard in `dom-platform`, so years later it is clear which model ran when, and why each switch was made. Naive replay can mislead in several known ways; Appendix E lists them and their remedies.

## Does memory earn its keep?

The first change to evaluate is the system itself. Hold model, harness, tasks, and snapshot fixed, and vary only what the agent may read.

| Arm | The agent gets | It stands for |
|---|---|---|
| **No memory** | The task spec and the repositories and trackers it would have anyway | The floor |
| **Transcript replay** | No memory, plus earlier sessions' transcripts | Today's practice |
| **T0 only** | Cached index and policy digest, no retrieval | What the cheap layer buys |
| **Full** | T0, retrieval, and handoffs | The system as built |

Arms are scored on task success and on cost per task, paired by task across seeds. Memory earns its place in a domain when success is no worse than without memory, within a margin fixed before the run, and cost is lower; or when success is higher at a cost the domain accepts. Cheaper and worse is a failure.

The no-memory baseline must be honest: it keeps everything an agent would have without this system, including tracker history. Entries written after the task's snapshot are excluded from the full arm.

The answer is per domain, and it moves. Memory may pay where decisions are long-lived and not where tasks are throwaway, and larger context windows or cheaper reads can erode its margin. So the comparison runs first on a fixed pilot set, and later as a standing holdout: a small sample of completed tasks re-run without memory on a published cadence.

## Vendor changes nobody announced

A stable model name does not fix every aspect of a service. Weights, routing, tokenisation, safety filters, and defaults can change. Detection starts by pinning the operator's configuration and comparing repeated probes. Provenance helps distinguish local changes from changes in the vendor's service; the signals below guide investigation rather than prove a cause on their own.

| Detector | May indicate |
|---|---|
| Log-probability divergence on fixed prompts | Weight or quantisation changes invisible in sampled text |
| Response metadata: fingerprints, token counts, latency | Version repoints, tokeniser and routing changes |
| Frozen-context replay against a known-good run | Real regressions on the operator's own work |
| Refusal-boundary and format probes | Safety-filter changes |
| Fleet metrics (§9) | Anything with real impact, slowest to show |

Appendix E gives the detection and response in detail.

One interlock matters most. If the model drifts while the §10 loop is evaluating a candidate, the loop could credit the drift to the candidate. Detected drift therefore freezes distillation for the affected window, so nothing distilled during it is promoted, and quarantines any candidate whose evaluation spanned it.

---

# 12. Surviving Long Timescales

**Why.** Over decades, everything around the data changes: the organisation, the people, the models, the tools, and the prices. If any of these changes means migrating the record, the record will not survive them all.

## Identity when the organisation changes

**Why.** Organisations rename, split, merge, and hand names to new owners. People change teams, employers, and roles, and accounts get reused. A record that keys anything on a name breaks every time, and the break is silent: old provenance starts pointing at the wrong party.

This is a known problem with a known answer.

- RFC 4151 [[11]](#ref-11), the `tag` URI scheme, names a thing by an authority plus the date on which that authority held its name, because "ownership of domains and email addresses is subject to change".
- OpenID Connect [[12]](#ref-12) makes the same point for people: only the issuer and subject together are stable, while emails and usernames get reused.
- "Cool URIs don't change" because they leave out the names of people and organisations, who move [[13]](#ref-13).

The design follows three rules.

- **Keys are opaque and permanent.** Domains are keyed by IDs minted once and never reused. A team name is a label on the domain. Entries and tasks likewise.
- **Names are claims with dates.** An author or owner is recorded as an identifier qualified by the authority that issued it and the date the authority held that name, so `human:<authority>,<date>:<user-id>` stays correct after the authority is renamed or merged, or the name passes to someone else. External identities are kept as issuer and subject together. A rename or merger adds a dated assertion and leaves the old ones as they were.
- **Who-owns-what is data.** A registry maps current organisational units to the domains they own, and each domain to its repositories and queues. A reorganisation updates that registry and invalidates affected access decisions before further responses are served (§6).

Team splits are handled by co-ownership until the knowledge actually diverges, then a curator-assisted split with tombstone redirects. Merges are lazy, done only on real convergence. Orphaning is the dangerous case, because orphaned memory decays while agents keep trusting it, so ownership is a checked invariant and an ownerless domain is flagged within days. Acquisitions import into a quarantined low-trust tier and earn promotion through ordinary review (§4).

## Personal domains

**Why.** People carry what they know between projects and employers, and what a system learns about a person should stay under that person's control.

A domain's owner may be a person. The domain is keyed by a permanent identifier for that person, and the person chooses where it lives: their own machine, a provider, or a store an institution runs for them. It is theirs because they hold the grants, act as its guard, and can export it whole.

**An example.** A developer's coding assistant notices they prefer small, reviewable commits, and proposes recording that in their personal domain. They accept. Their employer's platform proposes adding notes from their performance review; they refuse. When they change jobs, the domain goes with them, and the commits they made stay with the projects.

| What it is about | Where it lives |
|---|---|
| The person: preferences, habits, their own notes | The person's domain |
| The institution's work, including the person's role in it | The institution's domains |

An institution's server holds only a view of the person's interface summary, under grant.

Other systems that write to a personal domain (a harness, an assistant, an employer's platform) can only propose:

- they read the owner's policy first, and the owner admits or refuses;
- a refused proposal is erased; a separate access-controlled deduplication record keeps a safe opaque origin identifier, not the original URL or text, so the item is not proposed again;
- a proposal nobody answers expires.

One catch: an entry about the person, written in a session that read restricted material, carries that label. It can then leave only through the institution's guard. So anything meant for the person's domain should be written in a session that read nothing above the person's level.

When a grant ends, the institution's views are dropped with receipts. What the person wrote into the institution's domains stays there under their identifier.

## Models and tools

**Why.** Models are replaced every few months, and tools are rewritten, change hands, or stop being maintained. Whatever the record depends on, it can only last as long as that does.

BM25 needs no model. Embeddings, summaries, and compiled rubrics are derived and rebuildable. Provenance records the model behind every write, so a bad model generation's contributions can be found and re-checked.

The record preserves entries and policy; the historian preserves events, live tasks, and writes awaiting materialisation. Both need documented formats that can be read without the original service. If the policy engine or search engine is replaced, the policies remain readable and the index can be rebuilt.

A **recovery bundle** captures both stores at consistent checkpoints: named positions through which their events have been included. It also includes screening decisions, materializer and ledger positions, registry and policy versions, and the configuration needed to rebuild the projections. The bundle must include every live task and all lineage retained under the applicable retention clocks.

Erasure records are maintained separately from old snapshots. A restore must obtain the latest erasure state, remove affected content and metadata, and invalidate old claims before serving traffic. If that state is unavailable, the restore stays offline. A yearly drill rebuilds from a recovery bundle, reconciles pending external effects, recovers unfinished tasks, and checks that nothing erased comes back. Restoring the record alone is insufficient. Appendix E gives the recovery rules.

Two tests apply to every component. The **record test** asks whether losing it would destroy knowledge or state that cannot be rebuilt. The **ownership test** asks whether it would keep running if its contributors left. For anything on the production path, the institution takes responsibility for operating and maintaining an adopted version.

## Cost and throughput

**Why.** The price of model work falls fast and changes shape, and capacity runs short at peak whatever the price. A design tuned to today's prices is wrong within a year, and a money budget alone still lets one burst starve everyone else.

The design keeps prices out of its structure. Telemetry records raw quantities per resource class: tokens (input, cached, output), requests, accelerator time, compute time, and bytes stored. A versioned price vector, kept with policy, turns them into cost. The efficiency measure is cost per completed task.

When the price vector changes, the settings that depend on cost are recomputed and checked by replay before use: T0 size, disclosure depth, prefetch, the curation budget, and triage weights. The structure itself (record, provenance, trust, domains, policy, perimeter) rests on correctness and auditability, and prices never move it.

**Why not budget in number of tokens and turns?** They look like physics, and partly they are: compute scales with what a model reads, and each turn re-reads the context unless it is cached. Three things argue against building the design on them.

- **Price has already left the physics.** Providers charge cached input at a tenth of fresh input or less, and output at several times input, so the same token costs different amounts depending on how it is read and produced. A design tuned to input tokens optimises the wrong quantity as soon as caching changes.
- **The price moves fast.** The price of a fixed level of capability has fallen roughly tenfold a year, faster for some tasks [[14]](#ref-14), [[15]](#ref-15). Settings tuned to today's price are wrong within a year; settings recomputed from a price vector are not.
- **A budget is a target.** Budget agents by tokens or turns and they learn to read less, whether or not the task gets done. When a measure becomes a target it stops being a good measure (Goodhart's law, in Strathern's phrasing [[16]](#ref-16)). Vendors of agents are moving the same way, pricing per resolved outcome [[17]](#ref-17).

The physics that is real is throughput. Every resource class is limited by capacity per unit of time; what differs is who holds the queue.

- **A vendor's API** is someone else's accelerator pool. Money buys a higher rate limit or reserved throughput, but not more capacity than the vendor has. At peak the vendor throttles, queues, or quietly serves a cheaper variant (§11). The institution's rate limit is a shared pool of its own: one principal's burst can use up everyone's quota.
- **A self-hosted pool** puts the queue in the institution's hands, at a fixed capacity it cannot buy more of at short notice.

One currency cannot express a fair share of a pool at peak. So the perimeter sets two limits on each resource class, per domain and per principal:

| Limit | Unit | Protects against |
|---|---|---|
| Spend | Money | A runaway loop or an unplanned bill |
| Share of throughput | A rate, or a time share of a pool | One principal's burst starving everyone else |

When a class saturates, triage treats it like any other constraint (§8): urgent work keeps its share, deferrable work waits, and curation keeps its protected floor (§5). The price vector converts everything to cost for reporting. The system records tokens as measurements and prices them by configuration; the unit it answers for is the completed task.

---

# 13. Related Work

The design combines recent agent-memory research with decades of systems practice.

| Area | Builds on |
|---|---|
| **Memory** | Operating-system tiers (MemGPT [[18]](#ref-18)); reflection (Generative Agents [[19]](#ref-19)); experience distillation (ExpeL [[20]](#ref-20), A-Mem [[21]](#ref-21)); agent-written skills (Voyager [[22]](#ref-22)); memory as a write, manage, read loop on shared substrates [[23]](#ref-23), [[24]](#ref-24); event sourcing [[25]](#ref-25), [[26]](#ref-26); decision records [[27]](#ref-27); organisational memory [[28]](#ref-28) |
| **Trust** | Not transitive [[6]](#ref-6); typed by purpose [[7]](#ref-7); lossy propagation, separate distrust [[29]](#ref-29); provenance and lineage in W3C PROV [[30]](#ref-30); authority collapse [[8]](#ref-8); separation of privilege [[31]](#ref-31) |
| **Safety** | Indirect prompt injection [[32]](#ref-32); memory poisoning through queries (MINJA [[33]](#ref-33)) and stores (AgentPoison [[34]](#ref-34)), largely missed by injection defences [[35]](#ref-35); OWASP's agentic top risks [[36]](#ref-36); failure modes of governed shared memory [[37]](#ref-37); retrieved content as data, never control flow (CaMeL [[38]](#ref-38)) |
| **Retrieval and access** | Authorisation compiled into the query (OPA [[39]](#ref-39), Zanzibar [[40]](#ref-40)); the Model Context Protocol as today's binding [[41]](#ref-41) |
| **Policy** | Written, layered values (Constitutional AI [[42]](#ref-42)); specification gaming, which the safety gate exists for [[43]](#ref-43), [[44]](#ref-44) |
| **Coordination** | Blackboards (Hearsay-II [[45]](#ref-45)) and tuple spaces (Linda [[46]](#ref-46)); stigmergy [[47]](#ref-47), [[48]](#ref-48); leases [[49]](#ref-49); bounded contexts [[50]](#ref-50) and data mesh [[51]](#ref-51); hierarchical task networks [[52]](#ref-52); critical-path priority [[53]](#ref-53); the contract net, without the bidding [[54]](#ref-54) |
| **Measurement and evaluation** | SRE [[55]](#ref-55); token cost as an axis (Mem0 [[56]](#ref-56), LoCoMo [[57]](#ref-57)); long-horizon benchmarks (LongMemEval [[58]](#ref-58)); silent vendor drift [[59]](#ref-59); NIST AI RMF [[60]](#ref-60) and ISO/IEC 42001 [[61]](#ref-61); OpenTelemetry [[62]](#ref-62) and Inspect [[63]](#ref-63) as carriers; the paired noise-floor protocol [[64]](#ref-64) |
| **Seeding** | Co-change coupling [[65]](#ref-65); fixes linked to their causes [[66]](#ref-66) |
| **Self-improvement** | Self-evolving harnesses (SEAGym [[67]](#ref-67)); offline distillation, online retrieval (EvolveR [[68]](#ref-68)); gated archives (Darwin Gödel Machine [[69]](#ref-69)); co-evolving evaluators [[70]](#ref-70); agents designed as code [[71]](#ref-71). This design adds human review, a manifest check, and a perimeter outside the loop |
| **Long timescales** | Dated authorities (RFC 4151 [[11]](#ref-11)); names never reassigned (RFC 8141 [[72]](#ref-72)); stable internal IDs (SCIM [[73]](#ref-73)); issuer and subject (OpenID Connect [[12]](#ref-12)); identifiers free of organisational names [[13]](#ref-13) |

**Organisational cybernetics.** Beer's Viable System Model (VSM) [[74]](#ref-74), [[75]](#ref-75) provides a way to examine the organisation this infrastructure supports. His 1984 paper stresses the interactions between functions, local autonomy, and adaptation to a changing environment [[76]](#ref-76). The following mapping separates provided mechanisms from responsibilities a deployment must assign:

| VSM | Function | Mechanism and responsibility here |
|---|---|---|
| System 1 | Operations | Named operational units, supported by domains and workers (§8) |
| System 2 | Coordination | Ledger, dependency checks, capacity commitments, and arbitration (§8, Appendix C) |
| System 3 | Current operations and resources | Orchestrators and owners allocate work within agreed budgets |
| System 3* | Audit | Independent curation audits and canaries (§5); owners also arrange checks on operational decisions and outcomes |
| System 4 | External change and possible futures | A person responsible for looking ahead, external sources, and an exploration budget (§7); vendor monitoring is one input |
| System 5 | Identity and policy | Accountable people decide purpose and policy, informed by current operations and future proposals (§7) |

The mapping is a design check, not evidence that naming the functions makes an organisation viable. Beer describes the need for ongoing exchange among Systems 3, 4, and 5, and for the relevant functions within each viable operational unit. Here those obligations appear in the strategy review (§7) and the worked example of local responsibility (§8). A hierarchy of queues alone does not meet them.

Ashby's requisite variety [[77]](#ref-77), as developed in Beer's paper, concerns the available responses and the capacity of communication channels as well as the information being reduced. Summaries help people and agents read less; review queues, escalation deadlines, and tests for omitted exceptions check whether they can still respond appropriately (§9). The curation loop uses feedback from its own telemetry (Wiener [[78]](#ref-78)); the strategy review also needs evidence from outside it.

The proposed contribution is the combination: durable records and lineage, controlled sharing, disposable workers, and accountable decisions over long periods. The evaluation in §11 tests the memory mechanisms; deployments must also exercise the organisational failure scenarios in §9.

---

# References

Numbered in order of first citation. Recent works link to their arXiv records; older works give the venue, with a link where a stable copy exists.

<a id="ref-1"></a>[1] Hugging Face Security Team (2026). Security Incident Disclosure, July 2026. 16 July 2026. https://huggingface.co/blog/security-incident-july-2026

<a id="ref-2"></a>[2] OpenAI (2026). OpenAI and Hugging Face Partner to Address Security Incident during Model Evaluation. 21 July 2026. https://openai.com/index/hugging-face-model-evaluation-security-incident/

<a id="ref-3"></a>[3] Cybersecurity Dive (2026). OpenAI Warns Autonomous Hacks Are 'Watershed Moment for Computer Security'. 6 August 2026. https://www.cybersecuritydive.com/news/openai-hugging-face-hack-ai-models-black-hat/827167/

<a id="ref-4"></a>[4] Sharma, S. (2023). Samsung Bans Staff AI Use over Data Leak Concerns. *CSO Online*, 2 May 2023. https://www.csoonline.com/article/3695170/samsung-bans-staff-ai-use-over-data-leak-concerns.html

<a id="ref-5"></a>[5] Spracklen, J., Wijewickrama, R., Sakib, A. H. M. N., Maiti, A., Viswanath, B., & Jadliwala, M. (2025). We Have a Package for You! A Comprehensive Analysis of Package Hallucinations by Code Generating LLMs. *USENIX Security 2025*. https://www.usenix.org/conference/usenixsecurity25/presentation/spracklen

<a id="ref-6"></a>[6] Christianson, B., & Harbison, W. S. (1997). Why Isn't Trust Transitive? *Security Protocols 1996*, LNCS 1189. Springer. https://doi.org/10.1007/3-540-62494-5_16

<a id="ref-7"></a>[7] Jøsang, A., & Pope, S. (2005). Semantic Constraints for Trust Transitivity. *APCCM 2005*, CRPIT 43, 59–68. https://crpit.scem.westernsydney.edu.au/confpapers/CRPITV43Josang.pdf

<a id="ref-8"></a>[8] Zhan, Q., Zhang, R., Guo, S., Zhao, L., & Liu, Z. (2026). When Memory Becomes Authority: Benchmarking Authority Collapse at the Memory Consolidation Boundary. arXiv:2608.01679. https://arxiv.org/abs/2608.01679

<a id="ref-9"></a>[9] Digital Curation Centre. What Is Digital Curation? https://www.dcc.ac.uk/about/digital-curation

<a id="ref-10"></a>[10] DAMA International (2017). *DAMA-DMBOK: Data Management Body of Knowledge*, 2nd ed. Technics Publications. https://www.dama.org/cpages/body-of-knowledge

<a id="ref-11"></a>[11] Kindberg, T., & Hawke, S. (2005). The 'tag' URI Scheme. RFC 4151. https://www.rfc-editor.org/rfc/rfc4151

<a id="ref-12"></a>[12] OpenID Foundation (2014). OpenID Connect Core 1.0. https://openid.net/specs/openid-connect-core-1_0.html

<a id="ref-13"></a>[13] Berners-Lee, T. (1998). Cool URIs Don't Change. W3C. https://www.w3.org/Provider/Style/URI

<a id="ref-14"></a>[14] Appenzeller, G. (2024). Welcome to LLMflation: LLM Inference Cost Is Going Down Fast. Andreessen Horowitz. https://a16z.com/llmflation-llm-inference-cost/

<a id="ref-15"></a>[15] Cottier, B., Snodin, B., Owen, D., & Adamczewski, T. (2025). LLM Inference Prices Have Fallen Rapidly but Unequally across Tasks. Epoch AI. https://epoch.ai/data-insights/llm-inference-price-trends (accessed 13 October 2026).

<a id="ref-16"></a>[16] Strathern, M. (1997). “Improving ratings”: Audit in the British University system. *European Review*, 5(3), 305–321. https://doi.org/10.1002/%28SICI%291234-981X%28199707%295%3A3%3C305%3A%3AAID-EURO184%3E3.0.CO%3B2-4

<a id="ref-17"></a>[17] O'Reilly, A. (2025). Pricing AI Agents: What Does “Value-Based Pricing” Really Mean for AI? The Intercom Blog, 28 April 2025. https://www.intercom.com/blog/pricing-ai-agents

<a id="ref-18"></a>[18] Packer, C., et al. (2023). MemGPT: Towards LLMs as Operating Systems. arXiv:2310.08560. https://arxiv.org/abs/2310.08560

<a id="ref-19"></a>[19] Park, J. S., et al. (2023). Generative Agents: Interactive Simulacra of Human Behavior. arXiv:2304.03442. https://arxiv.org/abs/2304.03442

<a id="ref-20"></a>[20] Zhao, A., et al. (2024). ExpeL: LLM Agents Are Experiential Learners. *AAAI 2024*. arXiv:2308.10144. https://arxiv.org/abs/2308.10144

<a id="ref-21"></a>[21] Xu, W., Liang, Z., Mei, K., Gao, H., Tan, J., & Zhang, Y. (2025). A-Mem: Agentic Memory for LLM Agents. arXiv:2502.12110. https://arxiv.org/abs/2502.12110

<a id="ref-22"></a>[22] Wang, G., Xie, Y., Jiang, Y., Mandlekar, A., Xiao, C., Zhu, Y., Fan, L., & Anandkumar, A. (2023). Voyager: An Open-Ended Embodied Agent with Large Language Models. arXiv:2305.16291. https://arxiv.org/abs/2305.16291

<a id="ref-23"></a>[23] Du, P. (2026). Memory for Autonomous LLM Agents: Mechanisms, Evaluation, and Emerging Frontiers. arXiv:2603.07670. https://arxiv.org/abs/2603.07670

<a id="ref-24"></a>[24] Hu, Y., Liu, S., Yue, Y., Zhang, G., et al. (2025). Memory in the Age of AI Agents. arXiv:2512.13564. https://arxiv.org/abs/2512.13564

<a id="ref-25"></a>[25] Fowler, M. (2005). Event Sourcing. https://martinfowler.com/eaaDev/EventSourcing.html

<a id="ref-26"></a>[26] Kleppmann, M. (2017). *Designing Data-Intensive Applications*. O'Reilly. https://dataintensive.net

<a id="ref-27"></a>[27] Nygard, M. (2011). Documenting Architecture Decisions. https://cognitect.com/blog/2011/11/15/documenting-architecture-decisions

<a id="ref-28"></a>[28] Walsh, J. P., & Ungson, G. R. (1991). Organizational Memory. *Academy of Management Review*, 16(1), 57–91. https://doi.org/10.5465/amr.1991.4278992

<a id="ref-29"></a>[29] Guha, R., Kumar, R., Raghavan, P., & Tomkins, A. (2004). Propagation of Trust and Distrust. *WWW 2004*. https://archives.iw3c2.org/www2004/proceedings/docs/1p403.pdf

<a id="ref-30"></a>[30] W3C (2013). PROV-Overview: An Overview of the PROV Family of Documents. https://www.w3.org/TR/prov-overview/

<a id="ref-31"></a>[31] Saltzer, J. H., & Schroeder, M. D. (1975). The Protection of Information in Computer Systems. *Proceedings of the IEEE*, 63(9). https://web.mit.edu/Saltzer/www/publications/protection/

<a id="ref-32"></a>[32] Greshake, K., Abdelnabi, S., Mishra, S., Endres, C., Holz, T., & Fritz, M. (2023). Not What You've Signed Up For: Compromising Real-World LLM-Integrated Applications with Indirect Prompt Injection. arXiv:2302.12173. https://arxiv.org/abs/2302.12173

<a id="ref-33"></a>[33] Dong, S., Xu, S., He, P., Li, Y., et al. (2025). Memory Injection Attacks on LLM Agents via Query-Only Interaction (MINJA). *NeurIPS 2025*. arXiv:2503.03704. https://arxiv.org/abs/2503.03704

<a id="ref-34"></a>[34] Chen, Z., Xiang, Z., Xiao, C., Song, D., & Li, B. (2024). AgentPoison: Red-teaming LLM Agents via Poisoning Memory or Knowledge Bases. *NeurIPS 2024*. arXiv:2407.12784. https://arxiv.org/abs/2407.12784

<a id="ref-35"></a>[35] Dash, P., et al. (2026). *From Untrusted Input to Trusted Memory: A Systematic Study of Memory Poisoning Attacks in LLM Agents*. arXiv:2606.04329. https://arxiv.org/abs/2606.04329

<a id="ref-36"></a>[36] OWASP GenAI Security Project (2025). OWASP Top 10 for Agentic Applications. https://genai.owasp.org/2025/12/09/owasp-top-10-for-agentic-applications-the-benchmark-for-agentic-security-in-the-age-of-autonomous-ai/

<a id="ref-37"></a>[37] *Governed Shared Memory for Multi-Agent LLM Systems* (2026). arXiv:2606.24535. https://arxiv.org/abs/2606.24535

<a id="ref-38"></a>[38] Debenedetti, E., et al. (2025). Defeating Prompt Injections by Design (CaMeL). arXiv:2503.18813. https://arxiv.org/abs/2503.18813

<a id="ref-39"></a>[39] Open Policy Agent. Evaluating a Data Filter Policy (partial evaluation documentation). https://www.openpolicyagent.org/docs/filtering/partial-evaluation

<a id="ref-40"></a>[40] Pang, R., et al. (2019). Zanzibar: Google's Consistent, Global Authorization System. *USENIX ATC 2019*. https://www.usenix.org/conference/atc19/presentation/pang

<a id="ref-41"></a>[41] Model Context Protocol (2025). Specification, version 2025-11-25. https://modelcontextprotocol.io/specification/2025-11-25

<a id="ref-42"></a>[42] Bai, Y., et al. (2022). Constitutional AI: Harmlessness from AI Feedback. arXiv:2212.08073. https://arxiv.org/abs/2212.08073

<a id="ref-43"></a>[43] Amodei, D., Olah, C., Steinhardt, J., Christiano, P., Schulman, J., & Mané, D. (2016). Concrete Problems in AI Safety. arXiv:1606.06565. https://arxiv.org/abs/1606.06565

<a id="ref-44"></a>[44] Krakovna, V., et al. (2020). Specification Gaming: The Flip Side of AI Ingenuity. DeepMind. https://deepmind.google/discover/blog/specification-gaming-the-flip-side-of-ai-ingenuity/

<a id="ref-45"></a>[45] Erman, L. D., Hayes-Roth, F., Lesser, V. R., & Reddy, D. R. (1980). The Hearsay-II Speech-Understanding System: Integrating Knowledge to Resolve Uncertainty. *ACM Computing Surveys*, 12(2), 213–253. https://doi.org/10.1145/356810.356816

<a id="ref-46"></a>[46] Gelernter, D. (1985). Generative Communication in Linda. *ACM Transactions on Programming Languages and Systems*, 7(1), 80–112. https://doi.org/10.1145/2363.2433

<a id="ref-47"></a>[47] Grassé, P.-P. (1959). La reconstruction du nid et les coordinations interindividuelles chez *Bellicositermes natalensis* et *Cubitermes* sp.; la théorie de la stigmergie: essai d'interprétation du comportement des termites constructeurs. *Insectes Sociaux*, 6(1), 41–80. https://doi.org/10.1007/BF02223791

<a id="ref-48"></a>[48] Heylighen, F. (2016). Stigmergy as a Universal Coordination Mechanism I: Definition and Components. *Cognitive Systems Research*, 38, 4–13. https://doi.org/10.1016/j.cogsys.2015.12.002

<a id="ref-49"></a>[49] Gray, C., & Cheriton, D. (1989). Leases: An Efficient Fault-Tolerant Mechanism for Distributed File Cache Consistency. *SOSP '89*. https://doi.org/10.1145/74850.74870

<a id="ref-50"></a>[50] Evans, E. (2003). *Domain-Driven Design: Tackling Complexity in the Heart of Software*. Addison-Wesley. https://www.domainlanguage.com/ddd/

<a id="ref-51"></a>[51] Dehghani, Z. (2019). How to Move Beyond a Monolithic Data Lake to a Distributed Data Mesh. https://martinfowler.com/articles/data-monolith-to-mesh.html

<a id="ref-52"></a>[52] Erol, K., Hendler, J., & Nau, D. S. (1994). HTN Planning: Complexity and Expressivity. *AAAI-94*. https://cdn.aaai.org/AAAI/1994/AAAI94-173.pdf

<a id="ref-53"></a>[53] Kelley, J. E., & Walker, M. R. (1959). Critical-Path Planning and Scheduling. In *Proceedings of the Eastern Joint Computer Conference*, 160–173. https://doi.org/10.1145/1460299.1460318

<a id="ref-54"></a>[54] Smith, R. G. (1980). The Contract Net Protocol: High-Level Communication and Control in a Distributed Problem Solver. *IEEE Transactions on Computers*, C-29(12), 1104–1113. https://doi.org/10.1109/TC.1980.1675516

<a id="ref-55"></a>[55] Beyer, B., Jones, C., Petoff, J., & Murphy, N. R. (2016). *Site Reliability Engineering*. O'Reilly. https://sre.google/sre-book/table-of-contents/

<a id="ref-56"></a>[56] Chhikara, P., Khant, D., Aryan, S., Singh, T., & Yadav, D. (2025). Mem0: Building Production-Ready AI Agents with Scalable Long-Term Memory. arXiv:2504.19413. https://arxiv.org/abs/2504.19413

<a id="ref-57"></a>[57] Maharana, A., et al. (2024). Evaluating Very Long-Term Conversational Memory of LLM Agents (LoCoMo). arXiv:2402.17753. https://arxiv.org/abs/2402.17753

<a id="ref-58"></a>[58] Wu, D., et al. (2024). LongMemEval: Benchmarking Chat Assistants on Long-Term Interactive Memory. arXiv:2410.10813. https://arxiv.org/abs/2410.10813

<a id="ref-59"></a>[59] Chen, L., Zaharia, M., & Zou, J. (2023). How Is ChatGPT's Behavior Changing over Time? arXiv:2307.09009. https://arxiv.org/abs/2307.09009

<a id="ref-60"></a>[60] National Institute of Standards and Technology (2023). *Artificial Intelligence Risk Management Framework (AI RMF 1.0)*. NIST AI 100-1. https://doi.org/10.6028/NIST.AI.100-1

<a id="ref-61"></a>[61] ISO/IEC (2023). *ISO/IEC 42001:2023 Information technology — Artificial intelligence — Management system*. https://www.iso.org/standard/81230.html

<a id="ref-62"></a>[62] OpenTelemetry. *Generative AI semantic conventions*. https://opentelemetry.io/docs/specs/semconv/gen-ai/ (accessed 13 October 2026).

<a id="ref-63"></a>[63] UK AI Security Institute (2024). Inspect AI: Framework for Large Language Model Evaluations. Released May 2024. https://inspect.aisi.org.uk

<a id="ref-64"></a>[64] *How Much Coordination Gain Is Real? A Paired Noise-Floor Protocol for Multi-Agent Benchmarks* (2026). arXiv:2606.20695. https://arxiv.org/abs/2606.20695

<a id="ref-65"></a>[65] Zimmermann, T., Weissgerber, P., Diehl, S., & Zeller, A. (2004). Mining Version Histories to Guide Software Changes. *ICSE 2004*. https://doi.org/10.1109/ICSE.2004.1317478

<a id="ref-66"></a>[66] Śliwerski, J., Zimmermann, T., & Zeller, A. (2005). When Do Changes Induce Fixes? *MSR 2005*. https://doi.org/10.1145/1083142.1083147

<a id="ref-67"></a>[67] Zheng, C., et al. (2026). *SEAGym: An Evaluation Environment for Self-Evolving LLM Agents*. arXiv:2606.17546. https://arxiv.org/abs/2606.17546

<a id="ref-68"></a>[68] Wu, R., et al. (2025). *EvolveR: Self-Evolving LLM Agents through an Experience-Driven Lifecycle*. arXiv:2510.16079. https://arxiv.org/abs/2510.16079

<a id="ref-69"></a>[69] Zhang, J., Hu, S., et al. (2025). The Darwin Gödel Machine: Open-Ended Evolution of Self-Improving Agents. arXiv:2505.22954. https://arxiv.org/abs/2505.22954

<a id="ref-70"></a>[70] Iacob, A., et al. (2026). *The Red Queen Gödel Machine: Co-Evolving Agents and Their Evaluators*. arXiv:2606.26294. https://arxiv.org/abs/2606.26294

<a id="ref-71"></a>[71] Hu, S., Lu, C., & Clune, J. (2024). Automated Design of Agentic Systems. arXiv:2408.08435. https://arxiv.org/abs/2408.08435

<a id="ref-72"></a>[72] Saint-Andre, P., & Klensin, J. (2017). Uniform Resource Names (URNs). RFC 8141. https://www.rfc-editor.org/rfc/rfc8141

<a id="ref-73"></a>[73] Hunt, P., Grizzle, K., Wahlstroem, E., & Mortimore, C. (2015). System for Cross-domain Identity Management: Core Schema. RFC 7643. https://www.rfc-editor.org/rfc/rfc7643

<a id="ref-74"></a>[74] Beer, S. (1972). *Brain of the Firm*. Allen Lane. Second edition, Wiley, 1981. https://www.wiley.com/en-us/Brain+of+the+Firm%2C+2nd+Edition-p-9780471948391

<a id="ref-75"></a>[75] Beer, S. (1979). *The Heart of Enterprise*. Wiley. https://www.wiley.com/en-us/The+Heart+of+Enterprise-p-9780471948377

<a id="ref-76"></a>[76] Beer, S. (1984). The Viable System Model: Its Provenance, Development, Methodology and Pathology. *Journal of the Operational Research Society*, 35(1), 7–25. https://doi.org/10.1057/jors.1984.2. [Authorised 1989 reprint](https://library.uniteddiversity.coop/Systems_and_Networks/Viable_Systems_Model/The%20Viable%20System%20Model.pdf); the available copy omits Figure 4.

<a id="ref-77"></a>[77] Ashby, W. R. (1956). *An Introduction to Cybernetics*. Chapman & Hall. https://archive.org/details/introductiontocy00ashb

<a id="ref-78"></a>[78] Wiener, N. (1948). *Cybernetics: Or Control and Communication in the Animal and the Machine*. Technology Press and Wiley. Reissued by MIT Press, 2019. https://doi.org/10.7551/mitpress/11810.001.0001

---

# Appendix A: Interface

Signatures are indicative. Schemas: entries below, policies in §7, tasks in Appendix C.

**The entry record (§3)**

```yaml
id: mem-a-0847                # stable, permanent, meaningless
type: decision | fact | convention | method | handoff
domain: dom-a                 # a permanent ID, never a team name
author: agent:worker-3 (model: <model-id>) | human:<authority>,<date>:<user-id>   # §12
created: <timestamp>
cites: [entry IDs or links]   # what the author says it drew on; the service records the rest (§3)
session: <session-id>@<historian position>   # recovers the read set
confidence: high | medium | low
trust_tier: raw | curated | verified
classification: internal      # who may see it (§4)
review_by: <date>             # when a fact must be re-checked
supersedes: mem-a-0312
policy_context: [pol-L1-007]  # policy in force when written
```


**Retrieval (§6)**

```
search(query, domain?, filters?)   → [{id, title, summary, meta, snippet}]
get(id | id#anchor)                → {body, related: [{id, summary, p}]}
recall(question, domain?)          → {brief, citations: [id]}   # cheap model, caller's filter
write(entry)                       → {id}    # historian append, screened before indexing; fast lane for handoffs
```

**Tasks (§8, Appendix C)**

```
create_task(spec)                  → {id, status: proposed}
get_task(task_id)                  → {task, children, dependents}
claim_next(domain?, lease_s)       → {task, claim} | none   # highest-ranked eligible task; claim includes generation and expiry
claim(task_id, lease_s)            → {claim | denied}
heartbeat(task_id, generation)    → {status, lease_expires}   # renews only the current claim
complete(task_id, generation, handoff_id)
release(task_id, generation)
rescore(task_id, scores, rationale)                  # orchestrator only
invalidate(task_id, reason, superseded_by?)          # obsolete, invalid, or duplicate
```

Every task operation carries a caller-supplied attempt key, so a retry cannot double-claim or double-complete. Mutations made under a claim also carry its generation; an attempt key does not replace fencing (Appendix C).

**Policy (§7)**

```
check_policy(action, domain)       → [{policy_id, layer, rule, intent}]
file_exception(scope, expiry, justification)   # routed by the layer breached
```

**Flow (§3, §4, Appendix B)**

```
declassify(id, target_label, justification)    # to the guard; on approval, a new entry citing id
erase(id, reason_class)                        # irreversible; leaves stub, record, receipt
export(domain, max_label)                      # entries at or below max_label the caller may read; the bundle carries its highest label
```

Indexing, materialisation, policy evaluation, telemetry, curation, and gates all happen behind this surface. The harness carries claim generations and enforces the lease rules on the agent's behalf.

---

# Appendix B: Seeding

A new deployment starts on top of existing knowledge: repositories, tickets, reviews, mailing lists, policy documents, often decades deep. Seeding imports that history under control.

**Seeding is ordinary import, at scale.** Nothing enters a trusted tier because it is old or looks authoritative. Sources land raw with provenance and earn promotion through the normal gates (§4). A wrong seed is the hardest corruption to find later.

**Two speeds.** Indexing is cheap and distillation costs model time. So index everything at once as a raw searchable layer that cites the originals, and distil selectively. Known-item lookup works from day one; understanding builds where it is needed. Zero-result queries mark where to distil next; history nobody asks about is indexed and never distilled, which is the right outcome.

**Backfill is replay.** Build the live taps first, then run history through the same pipeline, oldest first. The pipeline itself is a `dom-platform` artifact.

**Transcripts are sources, not memory.** A session transcript belongs to the session's domain. It is not in T0 and not indexed; entries cite particular turns when needed. Each transcript has a retention clock. At expiry, cited turns are kept as excerpts and the rest is dropped, with the deletion recorded.

An excerpt carries its turn's classification and is demoted, exported, or erased with the entry that cites it. Erasing a transcript early erases its excerpts too. Logged decision contexts (§11) are retained evidence of the same kind, kept with evaluation suites under their own clock.

| Source | Yields |
|---|---|
| Design proposals, decision threads | Decision entries, the most valuable target |
| Governance documents | Policy L0 and L1, authored with leadership rather than imported |
| Process and quality rules | L2 entries; the checkable ones compile to gates |
| Commit history | Decision provenance; reverts and fix-on-fix clusters mark fragile code; co-change seeds prefetch |
| Code reviews | L3 conventions and a truer ownership map than the org chart |
| Tickets with linked fixes | Golden-suite questions with known answers |
| Documentation, wikis | Interface summaries |
| Rosters, versions | Facts with short review dates |

**Order of work.** Domains first, with current owners registered. Then one verified interface summary per domain, so every area can answer "what is this and where do I look". Pilot on two or three deliberately different domains, show that memory pays on each ([§11](#11-evaluating-change)), then fan out.

**Repository knowledge and shared memory.** Facts that must change with a code commit belong in the repository. Decisions, rationale, and lessons that span commits or repositories belong in shared memory. Repository instruction files can have owners and version history, but do not automatically receive this system's trust labels, access checks, or review gates.

For contributors or tools that cannot call the service, a curator may generate a small instruction stub containing pointers from the domain's T0 slice. It is marked as generated, with a content digest in a published list. Fleet harnesses use the service directly. A hand-edited stub is rejected in review or proposed back as a raw entry.

A foreign harness that loads instruction files needs a load-time hook to check the published list and reject altered or stale stubs. Without that hook, the fleet's enforcement guarantees do not extend to it. Skills served through the tool protocol load only as verified bundles; changing their digest revokes approval. Repository copies of those skills are generated pointers to the approved bundles.

**Leaving the system.** `export(domain, max_label)` is a read under the caller's filter: entries at or below the label, with provenance, classification, and supersession. The bundle carries the highest label inside it and goes only to a destination whose ceiling allows that label. Import is the mirror: entries land raw, keep their classification or take the receiving domain's higher default, and are keyed on origin so a second import creates nothing. A personal domain moving between its owner's homes keeps its tiers; everything else earns them again.

**People are handled by policy.** Norms can be distilled from observable process: how decisions are phrased, how disagreement resolves. Roles and public records are facts. Behavioural profiles of individuals compiled from years of archives are a policy decision, not a distillation default. Set the boundary in L0 or L2 before the first run, and enforce it at the gate.

---

# Appendix C: Ledger Semantics

### The task record

```yaml
id: task-a-1204
parent: task-a-1200
origin: <tracker>:<permalink>     # the item it came from; that system stays canonical for it
domain: dom-a
scores: {value: 8, urgency: 5, effort: 3, unblocks: [task-a-1210]}
requires: [<tools, model tier, data scope, human sign-off>]
reversibility: reversible | irreversible
classification: internal
status: proposed | open | claimed | blocked | review | done | invalidated
claim: {agent: worker-7, generation: 42, lease_expires: <timestamp>}
depends_on: [...]
artifacts_touched: [<owned path glob>]
policy_context: [pol-L1-007, pol-L2-031]
handoff: mem-a-0901
```

### Storage

The historian is the ledger's authoritative store. The current task table is a projection: a view rebuilt by applying its events in order.

- Every ledger operation (create, admit, claim, heartbeat, release, rescore, complete, invalidate) is an append to the domain's partition.
- The ledger is a projection of that partition. The lease table agents dequeue from is a cache of the projection, rebuilt from the log.
- A claim takes effect only if the projection shows the task claimable at that point in the log. That makes `claim_next` atomic within a domain.
- A losing claim or an attempt to change a terminal task's status is recorded and changes nothing. Later acceptance, reverts, and human assessments are appended as outcome events without reopening the task.
- Every operation carries an attempt key, so a retry cannot double-claim or double-complete.
- The materializer writes each finished task, with its event history, to the record.

### Claims and external effects

- Each successful claim gets a higher generation than all prior claims on that task. Renewal, release, and completion require the current holder and generation, with an unexpired lease. Invalidating a task also invalidates its claim.
- Each task mutation carries its task ID, generation, and a stable action ID. The ledger and destination must enforce the claim check at the point of committing the effect. Checking only when a request leaves the worker cannot reject a delayed request at its destination.
- Destinations that support fencing reject obsolete generations. Where a destination cannot do so, a trusted connector serializes actions and records their outcomes. A task with an in-flight or uncertain effect is blocked from reassignment until the connector reconciles it; expiry alone does not make it safe to retry.
- Retries of an external action keep the same action ID. If the destination cannot deduplicate or reveal whether it applied an action, the task waits for human reconciliation. The design does not promise exactly-once effects from arbitrary external tools.
- Handoffs may still be appended after a lease is lost. They name the old generation and cannot complete the task, renew a claim, or trigger an external effect.
- Recovery invalidates pre-recovery claims and reconciles pending effects before reassignment. Generations must never be reused, including after a restore from an older backup.

### Ranking

- Rank belongs to open tasks. A claimed task keeps its lease whatever the rubric does.
- A task re-ranks when its own state changes.
- Rubric-wide rescoring (after an L1 amendment, a price change, or a saturated pool) runs once per triage cycle.
- A parent's rank flows to its open children.
- Cross-domain edges travel as events between partitions. Each domain's rubric still orders its own queue.

### Coordination across domains

- A shared resource has an owner, an agreed allocation, and a named arbiter for competing requests. An arbitration decision is an event linked from every affected task.
- An accepted capacity commitment holds until fulfilled, explicitly renegotiated, or cancelled with a reason. A routine score change cannot silently displace it. Policy defines how urgent work can override it.
- Dependency changes are checked for cycles. Because edges cross partitions, concurrent changes may reveal a cycle later; affected tasks are then blocked and the arbiter is notified. A periodic check reconciles the graph.
- Policy sets bounds on waiting time, repeated priority reversals, and missed commitments. Crossing a bound triggers review rather than another automatic rescore. Claimed tasks retain their leases unless explicitly invalidated.

### Endings

- A task ends as done, or as invalidated with a reason: obsolete, invalid, or duplicate. Nothing is deleted.
- An invalidated parent invalidates its live children. An invalidated dependency re-triages its dependents.
- A holder learns of either from its next heartbeat, stops, and writes a handoff.
- Tasks blocked past a bound alert. Tasks the world has moved past are invalidated as obsolete.

### Taps

- Admission deduplicates on `origin` among a domain's live tasks, so one item is one live task. An issue and the change that fixes it are related tasks, not one.
- An upstream close invalidates a live task and confirms a finished one. A reopen creates a new task citing its predecessor.
- Taps are idempotent on origin and upstream event, and the origin index keeps every origin it has seen.
- Taps reconcile by pull on a schedule. A webhook may hasten a pull but is never relied on.
- When the registry moves a repository to another domain, its live tasks move with it.
- Outbound writes carry the fleet's identity, which is how a tap ignores its own echoes.

### Delegation

- A helper runs a verified definition the parent selects, never one the parent writes, with a manifest no wider than the parent's.
- Its label starts at the parent's and flows back up, and its reads join the parent's read set, so delegation cannot launder a read.
- Spawning a helper is a `create_task` and a `claim` in one step.
- A child whose requirements fit inside the parent's manifest inherits the parent's admission. One that needs more is filed through the orchestrator.
- The child's lease ends no later than the parent's.
- If the parent ends first, its children run out their leases and become eligible for reassignment once pending effects are resolved. The next claimant of the parent finds them through the parent link.

### Targets

| Operation | Target |
|---|---|
| `claim_next` | p99 1 s |
| Projection lag | p99 1 s |
| Ordinary admission | within 10 s |
| Tap to task, and result back | within 5 min, excluding tracker outages |
| Triage cycle | 5 min |
| Heartbeat interval | a third of the lease |

### Outages

When the ledger is down, the historian still accepts appends: handoffs and heartbeats land, but nothing is confirmed or dequeued.

- An agent that cannot confirm a renewal may continue local analysis until its last confirmed expiry. Mutations wait whenever the current claim or action record cannot be checked. At expiry it stops and writes its handoff.
- The ledger expires leases by its own clock. Expired tasks return to the pool with their handoffs once pending effects have been reconciled. Heartbeats replayed after expiry cannot revive an old claim.
- A parent that cannot spawn does the work itself or waits.
- Taps buffer inbound items in the historian and outbound results at the tap.
- A historian outage is a retrieval outage too, and the retrieval degraded mode applies.
- Trackers stay canonical, so people keep working in them throughout.

---

# Appendix D: Failure Modes

| Failure | Defence |
|---|---|
| Lost lineage | Every write, read, send, ledger operation, and curation action is an event; informed-by edges are computed by the service, not declared by agents (§3) |
| Memory poisoning | Provenance; tiers that rise only through gates; imports start raw; content is data; independent auditors (§4, §5) |
| Leaked secret or unsafe output in memory | Screening before anything downstream reads it; failures quarantined, rotated, and erased; nothing raw is executed or loaded; outbound only within ceilings (§3, §4) |
| Planted external content read before review | Raw from a tap reaches only the task it started until curated (§4) |
| Trust carried along a chain | Derived entries take the weakest input; imports start raw; federation lends no authority (§4) |
| Wrong or compromised curator | Reversible diffs; auditors on another model; telemetry out of its reach; approver never the proposer; lineage re-check; canaries (§5) |
| Content leaves its boundary | Labels on entries, tasks, and sessions; ceilings on destinations; writes inherit; down only through a guard (§4) |
| Exfiltration through a tool | Ceilings and source labels per server; grants per principal and server; label checked per call; denials alert as probing (§4, Appendix E) |
| Crashed agent holds work | Leases, fenced mutations, and handoff recovery; uncertain external effects reconciled before reassignment (§8, Appendix C) |
| Ledger outage or partition | Mutations require a current claim check; unrenewable leases expire; work ends in a handoff (Appendix C) |
| Retrieval outage | Local analysis from loaded T0; queued handoffs; no unchecked reads or mutations (§9) |
| Corpus rot | Telemetry-directed curation; review dates; golden suites (§5, §9) |
| Ranking or summary regression | Suites on every change, summary rewrites included; canary serving (§9) |
| Leak through prefetch or analytics | One compiled filter on every path (§6) |
| Revoked access survives in a cache | Version checks before serving responses; current-time expiry checks; affected sessions stopped (§6, Appendix E) |
| Stated and enforced policy drift | Cross-citation between L2 entries and OPA rules (§7) |
| Exceptions that never end | Default expiry; audit of expired-but-relied-upon exceptions (§7) |
| Reorganisation breaks access or provenance | Opaque keys; dated authority on identifiers; registry as data (§12) |
| Runaway spend or a starved pool | Spend and throughput limits per resource class, per domain and principal; deferrable work waits at saturation; cost-per-task alerts (§12) |
| Privilege steering through task requirements | Scaled admission; creators may not require what they cannot hold; specs are data (§8) |
| Exploit spread through self-improvement | Perimeter outside the loop; safety gate on privilege; raw quarantine; separate roles (§10) |
| Silent vendor change | Probes, change-point detection, attribution by provenance, distillation freeze (§11) |
| Erasure against append-only history | Bodies and metadata covered; minimal stubs; affected derivations withheld; restores apply latest erasures (§3, §12) |
| Restore loses live tasks or lineage | Recovery bundle includes historian and record at consistent checkpoints; restore drill (§12) |
| Replay omits searchable handoffs | Capture historian and index visibility alongside record versions (§11, Appendix E) |
| Efficient delivery of obsolete work | External evidence, protected exploration, and human strategy review (§7) |
| Coordination stalls across domains | Capacity commitments, arbitration, cycle detection, and escalation bounds (§8, Appendix C) |
| Human decisions cannot keep up | Review backlog and latency measured; backup approvers; less work admitted when necessary (§9) |
| Institution keeps a person's memory | Personal domain under the owner's grants; views dropped with receipts (§12) |
| A contributed component's authors leave | Ownership test: on-path components are institution-operated (§12) |

---

# Appendix E: Rules in Detail

The main text gives each rule at the level needed to follow the design. This appendix gives the detail an implementer needs.

### Access freshness (§6)

- A cached filter records the versions of policy, registry, grants, and task scope used to compile it. A response can be released only after those versions are checked against the authority responsible for them. Serving and revocation are ordered: a request authorised after a revocation takes effect cannot use the old grant.
- Version changes invalidate dependent filters, T0 slices, rendered files, and other cached views. Direct fetches and cache hits undergo the same checks as searches. A long `recall` checks access during its reads and again before returning its brief.
- Expiry is evaluated against the service's current clock on each request and before releasing a delayed response. It must not be compiled into a constant permission. If current authority cannot be established, access is refused.
- Revoking a required grant stops the affected session and its helpers. Their existing contexts remain labelled; restarting creates a fresh context under current grants. Historical grants kept for replay never authorise a production read.

### Labels (§4)

- **Attesting handling.** A session's handling is set by policy for its principal and attested by its harness. A harness may attest handling above public only if every outbound path it has goes through the perimeter. Otherwise the send check could be bypassed.
- **Tool servers are sources too.** Each is registered with its ceiling and with the highest classification of what it reads. A call raises the session's label like any read.
- **Grants.** Grants are per principal and per server. A server that reads restricted sources is never the same grant as one whose ceiling is public; a deployment that needs that crossing builds it as a `declassify`.
- **Probing.** Repeated ceiling denials from one principal alert as probing.
- **Declassification.** A `declassify` proposes a new, lower-labelled entry that cites the original. The proposal keeps the higher label until the guard approves. The guard is the domain's owner by default, and a person for the widest crossings. The result names the guard in its provenance, so every crossing can be listed.
- **Delegation and recall.** A helper's label starts at its parent's and flows back up. A `recall` raises the caller's label to the highest classification its deputy read.

### Raw reach (§4)

- The write's session tells which row of the raw-reach table applies: a task chain, a fleet session, or a tap.
- Domain policy may widen or narrow the defaults.
- A principal's policy may require curated or better for what it reads in a domain.

### Screening and quarantine (§3, §4)

- Every write is appended to the historian, then screened for secrets, credentials, and whatever else L0 names.
- The index, ledger, materializer, and taps read only events that passed.
- A failing event stays in the historian, so the attempt, and what the session had read before it, are on record.
- It is quarantined: raised to the highest classification, readable only by the security role named in policy, and excluded from everything downstream.
- The exposed secret is rotated, and affected bodies and metadata are erased under §3. Only safe identifiers and the minimum audit record remain.

### Recovery (§12)

- A recovery manifest identifies the record snapshot, retained historian segments, and each consumer's checkpoint. A checkpoint is usable only with all the events needed to reconstruct its state, including screening decisions and unfinished tasks. Cross-domain dependencies must refer to included events or recorded pending deliveries.
- The bundle includes versioned policy, registry, grants, and projection configuration in documented formats. Current authority is reconciled before service resumes; old grants are not restored into force merely because a backup contains them.
- The latest erasure journal and any required keys are maintained independently of historical bundles. Keys for erased content remain destroyed. Restores apply all erasures after the snapshot and rebuild derived views before allowing reads.
- Restores establish a new claim epoch, a generation prefix greater than any previously issued, through an authority that is not rolled back with the snapshot. Destinations and connectors reject claims from earlier epochs before work resumes. Pending effects are reconciled under Appendix C.
- The recovery drill includes a write awaiting materialisation, an unfinished task, a cross-domain dependency, an uncertain external action, and an erasure made after the chosen backup. Passing means recovering each safely, not merely opening the files.

### Replay state (§11)

- Keep the exact model inputs for frozen-context replay. For live replay, retain a starting manifest with record versions, per-partition historian positions, screening state, and the index checkpoints visible at task start. The manifest also identifies the harness, ranker, summaries, retrieval settings, policy, registry, grants, and T0.
- Retain either the corresponding index snapshot or the versioned inputs and configuration needed to reproduce it. An event appended to the historian but not yet searchable must not appear early in replay. Fast-lane entries already searchable must be included even if not yet materialised.
- Later decision contexts record their own visibility checkpoints and returned entry versions. Mock external tools from retained fixtures and version repository inputs. Candidate writes stay in the replay's scratch state and never enter another evaluation arm.
- Retention and erasure apply to replay data too. If required evidence has expired or been erased, mark the replay unavailable or incomplete; do not silently replace it with current content. Access requires present-day authorisation as well as the historical restrictions being simulated.

### Replay traps (§11)

- **Survivorship.** Mined tasks are what the old system could already do, so a stratum of past failures is replayed too.
- **Leakage.** Snapshot before the task's own writes and exclude its artifacts, so the candidate never sees its answer.
- **Noise.** Use several seeds and confidence intervals, never one run.
- **Judge drift.** Hold the judging model fixed, anchor it to human-approved outcomes, and never let a model judge itself.
- **Distribution shift.** Keep a rolling set drawn from recent work and a frozen set comparable across years, changed only with human review.

### Vendor drift (§11)

Change-point detection looks for lasting shifts or intermittent patterns that track load. These patterns suggest where to investigate; they do not by themselves identify a model swap or a quantisation change. Every alarm must exceed the variation seen across repeated runs and rotating paraphrases, since even temperature-zero requests can vary. Two signals help:

- where a vendor offers dated snapshots, probing the alias against the snapshot isolates a repoint directly;
- a deployment using several vendors gets a differential signal for free.

Replay quantifies confirmed drift. Then the model is pinned or failed over, and the event is recorded and escalated.

---

# Appendix F: Glossary

| Term | Meaning |
|---|---|
| **BM25** | A method for ranking documents by how well their words match a query (§6) |
| **Checkpoint** | A recorded position showing how far a log or a view rebuilt from it has progressed (§12) |
| **Classification, handling, ceiling** | Who may see an entry; the most a session may read; the most a destination may receive (§4) |
| **Distil** | Turn raw material (threads, transcripts, trajectories) into curated entries or methods (§5, §10, Appendix B) |
| **`dom-platform`** | The domain that holds the system's own configuration (§10) |
| **Domain** | An area of work with an owner; memory, tasks, policy, and access are organised by it (§2) |
| **Exception** | A scoped, expiring, approved permission to breach a policy (§7) |
| **Fencing token** | An increasing claim generation carried with mutations so destinations can reject old claims (§8, Appendix C) |
| **Golden suite** | A domain's fixed set of queries with known answers, run on every change (§9) |
| **Guard** | Whoever policy names to approve moving content to a lower label (§4) |
| **Handoff** | The note an agent writes when it stops, so the next one can carry on (§8) |
| **Harness** | The program that runs a model as an agent: its prompts, tools, and settings (§10) |
| **Historian** | The ordered log of every event; lineage is a query over it (§3) |
| **Lineage** | What an entry came from, and what came from it (§3) |
| **L0 to L3** | The layers of written policy: values, strategy, practice, domain conventions (§7) |
| **Manifest** | What an agent's verified harness says it can do, presented when it claims work (§8) |
| **Principal** | A person or agent identity that holds grants |
| **Recovery bundle** | Record and historian snapshots, checkpoints, and configuration needed to rebuild the serving layer (§12) |
| **Registry** | The data mapping organisational units to the domains they own (§12) |
| **Rubric** | Task-scoring weights compiled from written strategy (§7) |
| **Steward** | Whoever answers for a domain: its owner (§5) |
| **T0** | The small index every agent loads at start: goals, active tasks, standing decisions, pointers (§3) |
| **Tap** | A two-way connector between an outside system and the ledger (§8) |
| **Trust tier** | How far an entry has been checked: raw, curated, or verified. **Promotion** moves it up through a review gate (§4) |
