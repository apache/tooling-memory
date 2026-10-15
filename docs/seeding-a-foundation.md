# Seeding a Foundation

### The Apache Software Foundation as a test case for [Durable Memory for Disposable Agents](durable-memory-for-disposable-agents.md)

Sander Striker · August 2026 · revised October 2026

Written with Claude Fable 5, Claude Opus 5 (Anthropic), and GPT-6 Astra (OpenAI).

*The Apache Software Foundation is a hard case for shared agent memory: hundreds of projects, decades of public archives, and a front door anyone can walk through. This case study maps the [companion paper](durable-memory-for-disposable-agents.md) onto it and proposes a deployment to test the design.*

This is a design exercise, not a report of an ASF deployment. The memory policies, service tiers, and pilots below are proposals for the responsible ASF bodies to consider. Links to ASF sources identify existing practice; the proposed tooling still needs to earn its place through the pilots.

---

## 1. Why this test

The Foundation is a hard case for this design, which makes it a useful one.

| Stress | Why it is hard |
|---|---|
| **Scale and age** | Hundreds of project communities, including podlings in the Incubator, each with its own code, community, and conventions, and archives going back to the 1990s. No one can distil all of it, so the paper's two speeds (index eligible sources, distil on demand; [paper Appendix B](durable-memory-for-disposable-agents.md#appendix-b-seeding)) are essential |
| **Governance matters as much as code** | The Apache Way (merit, community over code, vendor neutrality, consensus) is what the Foundation exists to protect. A memory that captures code but garbles governance has failed |
| **Public work, private exceptions** | Most project work is public: lists, issues, reviews, and published board minutes. Personnel discussions and undisclosed vulnerabilities need restricted handling. Public material can also contain personal information |
| **Open front door** | Anyone can post to a list, open an issue, or send a patch, so anyone can write into the raw tier |
| **Constant churn** | Projects incubate, graduate, split, merge, and retire. Committers come and go over decades, and employers change while hats stay. The registry and the paper's churn rules ([paper §12](durable-memory-for-disposable-agents.md#12-surviving-long-timescales)) are exercised all the time |
| **Federated direction** | Projects choose their technical direction; the Foundation also has its own strategy and shared services. The policy stack must accommodate both ([§5](#5-a-policy-stack-for-a-federation)) |

Each section below tests one part of [the paper](durable-memory-for-disposable-agents.md). [§10](#10-what-the-foundation-taught-the-design) collects what the Foundation taught the design.

---

## 2. Domains and the project lifecycle

**Why.** Memory, policy, and access are organised by domain ([paper §2](durable-memory-for-disposable-agents.md#2-the-design-at-a-glance)), so the domain map decides everything that follows.

The Foundation already partitions work by project. The proposed map starts with **one domain per top-level project or podling**, keyed by a permanent ID. The responsible committee and the project's name are attributes of the domain, so either can change. Foundation-level domains sit alongside: governance, infrastructure, legal, security, brand, and the Incubator.

Each domain has a steward in the paper's sense ([paper §5](durable-memory-for-disposable-agents.md#5-the-curator)), the party that answers for it and under whose policy curators work. The registry records who that is:

| Domain | Steward |
|---|---|
| A top-level project | Its PMC (project management committee) |
| A podling | Its podling PMC, under the Incubator PMC's oversight |
| Infrastructure, legal, security, brand | The responsible officer and team |
| Governance | The board |

The registry records lifecycle decisions made through ASF governance. It does not make those decisions itself:

```mermaid
stateDiagram-v2
    [*] --> Incubating: approved intake, imports start raw
    Incubating --> Active: graduation, new PMC takes ownership
    Incubating --> Retired: retirement under Incubator oversight
    Active --> Orphaned: registered steward missing or unresponsive
    Orphaned --> Active: accountable stewardship restored
    Orphaned --> Retired: governance decision, Attic process
    Active --> Retired: governance decision, Attic process
    note right of Active
        split and merge are moves between Active domains,
        tombstone redirects, shared entries both cite
    end note
    note right of Retired
        historical memory available for audit,
        retirement notice remains current,
        retention and erasure rules still apply
    end note
```

| Lifecycle event | Becomes |
|---|---|
| **Import** | [IP clearance](https://incubator.apache.org/ip-clearance/) establishes whether donated code may enter the ASF. Memory import adds a separate check: claims from that code and its history arrive raw and earn trust through review ([paper §12](durable-memory-for-disposable-agents.md#identity-when-the-organisation-changes)). Legal clearance does not verify their accuracy |
| **Rename** | Usually during incubation, after the podling name search. The domain ID stays; the new name is a dated assertion ([paper §12](durable-memory-for-disposable-agents.md#12-surviving-long-timescales)) |
| **Graduation** | Record the governance decision and transfer stewardship to the new PMC. Verification can already operate during incubation under an explicit policy authorised by the Incubator PMC; graduation does not promote entries automatically |
| **Split** | [Solr became a separate top-level project in 2021](https://solr.apache.org/whoweare.html). That is a useful example for the paper's domain split ([paper §12](durable-memory-for-disposable-agents.md#identity-when-the-organisation-changes)): curators propose redirects, while both domains can cite shared history without copying it |
| **Merge** | Combine domains only when their responsibilities converge. Shared development alone need not mean shared governance or access |
| **Orphaning** | A missing steward or an overdue response triggers escalation within a published interval. A roster entry alone cannot establish that oversight is healthy. This supplements the board's [project-reporting process](https://www.apache.org/dev/pmc.html) |
| **Retirement** | The [Attic preserves retired projects' resources](https://attic.apache.org/process-howto.html). In memory, demote historical entries to the audit-only archive tier and keep a current retirement notice with source pointers. Give archival custody a named owner. Retired podlings remain under [Incubator oversight](https://attic.apache.org/); retention and erasure still apply |

---

## 3. People and names

**Why.** Decades of provenance are only useful if they still point at the right person. At the Foundation, the names around a person change far more often than the person.

The paper's rule is opaque permanent keys plus dated names ([paper §12](durable-memory-for-disposable-agents.md#12-surviving-long-timescales), after RFC 4151). At the Foundation:

- **Committers are keyed by Apache ID**, as `human:apache.org,<date>:<apache-id>`. The dated authority costs nothing and still holds if the Foundation's naming ever changes.
- **Everyone else starts with a source identity.** List posters, issue reporters, and patch senders may have no Apache ID. Record the service and its stable account ID where available; for mail, record the observed address and date. An address or display name is evidence about an author, not proof of a person's identity. Link it to an Apache ID only with corroborating evidence, and keep uncertain matches separate.
- **Other identities are dated assertions.** Commits carry employer email addresses that lapse when people move. GitHub usernames can be renamed, and an old name can pass to someone else. Each is recorded as "this person used this identity from this date to that".
- **Roles are dated too.** Committer and PMC membership are dated from the records that exist: rosters, account creation, board resolutions. "Release manager for 2.4.0" is a fact distilled from the release vote thread. An employer is a dated affiliation kept apart from the identity, which is how "employers change while hats stay" becomes data.
- **Ordinary changes add history.** A rename, a new employer, or a lapsed role adds an assertion. Correct mistaken identity links explicitly; retention and erasure remain exceptions to preserving old assertions.

---

## 4. Sources to entries

**Why.** Decades of archives can seed far more than any new deployment could write. The question is which sources yield which kind of entry, and which can be cited for decades.

The paper's source table ([Appendix B](durable-memory-for-disposable-agents.md#appendix-b-seeding)) applies row by row. The Foundation's archive discipline supplies a strong starting point: **stable citations**. Record mailing-list permalinks and message IDs, and retain permitted excerpts under the source's access and retention rules. Check links over time; a permalink is a useful identifier, not a guarantee that a host will last twenty years.

The archives stay the original record. The memory holds distilled entries that link back, and its historian ([paper §3](durable-memory-for-disposable-agents.md#3-memory)) records what the fleet did with them.

| Foundation source | Entry type | Notes |
|---|---|---|
| Improvement proposals (KIPs, FLIPs, SPIPs and kin) | Decision candidates | Context, options, and dissent are often already written down; distinguish proposed, accepted, rejected, and superseded outcomes |
| [VOTE] and [DISCUSS] threads | Decisions | Cite the original discussion and the recorded result. Preserve material dissent; a discussion or an unfinished vote is not an adopted decision |
| Published board minutes, bylaws, Apache Way documents | Inputs to policy L0 and L1 | Responsible people author and approve policy entries; importing a document does not enact policy |
| Release, security, and licence policy | Policy L2 | The checkable part compiles to gates, including dependency-licence rules |
| Project style guides, commit and review norms | Policy L3 | Owned by the project domain |
| Commit histories | Decision provenance, possible hazards, files often changed together | Repeated reverts suggest areas worth investigating; they do not by themselves explain why the code changed |
| Reviews | Candidate L3 conventions, evidence of expertise | Recurring comments suggest conventions for project review; frequency alone does not make a rule or grant authority |
| Issues with linked fixes (GitHub, Jira, Bugzilla) | Golden-suite seeds, FAQ entries | Real questions with known resolutions |
| Project sites, docs, wikis | Interface summaries | The breadth-first layer |
| Rosters, release lines, dependency matrices | Facts | Short review clocks; these rot fastest |

Backfill records both when a source was written and when it was imported. A role recorded in 2004 must not become a current permission simply because the importer read it today.

---

## 5. A policy stack for a federation

**Why.** The paper's policy stack ([paper §7](durable-memory-for-disposable-agents.md#7-the-policy-stack)) must represent both Foundation strategy and each project's direction. The [board provides strategic oversight](https://www.apache.org/board/), while [projects control their technical direction](https://www.apache.org/foundation/how-it-works/). A shared strategy layer must preserve that division of responsibility.

| Layer | At the Foundation |
|---|---|
| **L0** | Shared principles and boundaries, including community governance and project independence. Responsible ASF bodies approve how these are expressed for the tooling; a curator cannot turn its interpretation into policy |
| **L1** | Foundation priorities for shared resources and sustainability, alongside each project's priorities developed through its community process and recorded under PMC oversight. Each domain gets its own rubric |
| **L2** | Versioned Foundation and project processes for releases, security handling, branding, and licence checks. Mechanical gates check what they can; required human decisions remain explicit |
| **L3** | Project conventions, owned by each domain, distilled largely from review archives |

**Finding: the stack must allow many L1s under one L0.** Each domain orders its own queue ([paper §8](durable-memory-for-disposable-agents.md#8-coordination)). Shared priorities can rank requests for common resources, but cannot commit another project's volunteers. A cross-project task needs acceptance from each affected owner, a recorded commitment, and an escalation path if it stalls.

The paper's strategy review also needs a local home. Each pilot names a horizon scanner to bring evidence from users and neighbouring projects into the community's discussions, with time reserved for that work. The PMC records resulting changes to the agent-work rubric and a date to assess them. Foundation-wide resource conflicts go to the responsible officer or board. This applies the exchanges described in the paper's [Viable System Model discussion](durable-memory-for-disposable-agents.md#13-related-work); a set of domain queues alone does not demonstrate that they work.

---

## 6. Trust and review

**Why.** Anyone can write to the Foundation, and its reviewers are volunteers. Trust has to hold against anyone writing into the raw tier, and review has to fit the time volunteers actually have.

### Foundation practice informs the gates

These are proposed memory-review rules. Existing ASF voting rules still govern the underlying project decisions; promoting an entry cannot substitute for a release vote or grant new authority.

| Foundation practice | Paper mechanism |
|---|---|
| PMC oversight | The proposal reserves promotion to verified for named human reviewers authorised by domain policy ([paper §4](durable-memory-for-disposable-agents.md#4-trust-and-handling)). A current roster establishes eligibility; the policy specifies scope and required approvals. Podlings need an Incubator-authorised policy |
| Committers review each other's work | Sampled review of routine curation, and review of golden suites |
| Lazy consensus | Announced, reversible, routine curation may proceed after a defined objection period where project policy allows. Silence cannot replace mandatory approval |
| Release votes | Encode the [release policy](https://www.apache.org/legal/release-policy.html): at least three binding +1 votes and more binding +1 than -1 votes. Podling releases also require [Incubator PMC approval](https://incubator.apache.org/guides/releasemanagement.html) |
| Procedural votes and code vetoes | Keep the [different voting rules](https://www.apache.org/foundation/voting.html) distinct. Procedural votes normally use simple majority; a valid code veto needs a technical justification and must be resolved. A release -1 is not a veto |
| IP clearance and software grants | Preserve the clearance evidence, while memory imports still start raw ([paper §4](durable-memory-for-disposable-agents.md#4-trust-and-handling)) |

The PMC remains accountable for its memory policy. Reviewers need not be the same people for every subject, and no agent receives a binding vote.

### An open front door

Anyone can submit material through public channels. After screening, raw material from a tap reaches only the task it started and the authorised curators and auditors ([paper §4](durable-memory-for-disposable-agents.md#4-trust-and-handling)). Bulk backfill stays in an explicitly scoped import area; indexing it does not make it available to every agent. The public memory endpoint serves curated or verified entries. Wider reach requires review, and raw text remains evidence rather than an instruction to the agent.

A planted issue, patch, or list post could steer an agent and contaminate later summaries. Screening and review reduce that risk; they do not eliminate it. Lineage ([paper §3](durable-memory-for-disposable-agents.md#3-memory)) supports containment: flag derived entries, identify recorded reads and sends within the retained history, and re-check affected work. It cannot recall copies already sent outside the system.

### Private lists are the restricted label

The paper's labels ([paper §4](durable-memory-for-disposable-agents.md#4-trust-and-handling)) apply directly. Public archives are public; private and security lists are restricted.

- **Who may read.** Match the source's actual permissions. PMC members have access to their project's private list; [ASF members have broader access](https://www.apache.org/foundation/governance/members.html). A shared `restricted` label does not give one PMC access to another's material. Every retrieval also checks the caller's domain and source grants.
- **Where it may go.** Permission for a person to read a list does not itself authorise sending it to an AI service. [ASF confidentiality guidance](https://www.apache.org/foundation/how-it-works/) explicitly limits sharing with third-party services. Restricted processing needs an approved model deployment and a harness that routes every outbound path through the perimeter, including prompts, logs, tools, and sampling requests.
- **Who guards.** Domain policy names the people who may approve disclosure. For vulnerability material, their approval must follow the [project's coordinated disclosure process](https://www.apache.org/security/committers.html), with the ASF Security Team involved as required.

A summary for a wider audience goes through `declassify`. The guard approves the exact public text and metadata; the source citation and full lineage remain in the restricted audit record. A private thread's subject, participants, or URL may themselves disclose information, so they are omitted from the public view unless separately approved. Posting the approved version is a follow-up task for a fresh session that reads only public material. The session that read the private thread cannot post to a public destination, because its label never falls.

```mermaid
sequenceDiagram
    autonumber
    participant A as Agent, handling restricted
    participant P as Perimeter
    participant M as Memory
    participant G as PMC, guard
    participant X as Worker on the follow-up task
    participant T as List tap, ceiling public

    A->>P: get(entry distilled from private@ thread)
    P->>M: read, current source grant and handling checked
    M-->>P: entry
    P-->>A: entry
    Note over A: current label rises to restricted
    A->>P: post summary to users@ through the list tap
    P-->>A: denied, restricted exceeds ceiling public
    A->>P: declassify(entry, public, "summary for users@")
    P->>G: proposal, held at restricted
    G-->>P: approve exact public text and metadata
    P->>M: public entry; private citation retained in restricted lineage
    Note over A: A's label never falls, so A goes no further
    X->>P: get(public entry)
    P-->>X: entry, label stays public
    X->>P: post summary through the list tap
    P->>T: allowed, public within ceiling public
```

An authorised auditor can ask: "Which public entries descend from material read on private@?" Each must have a recorded declassification approval along its lineage. Public readers receive only the metadata approved for publication.

### People deserve a policy before the pipeline runs

The archives hold decades of individuals arguing, deciding, and occasionally behaving badly, all in public. The paper leaves profiles of people to policy ([Appendix B](durable-memory-for-disposable-agents.md#appendix-b-seeding)). This proposal asks the responsible ASF bodies to approve a boundary before the first distillation pass:

- entries describe roles, process, and norms;
- roles and public records (committer, PMC member, release manager for a version) are facts;
- behavioural profiles of named people, including patterns of persuasion, are prohibited. Review gates and adversarial tests check for them; violations trigger removal and a lineage review.

The rule governs the Foundation's tooling and domains. A committer controls their own domain ([§7](#7-a-committer-owns-a-domain)), but that does not authorise Foundation tooling to build a prohibited profile and export it there.

---

## 7. A committer owns a domain

**Why.** Committers outlast their employers and often their projects. Personal memory they choose to keep should be portable and under their control.

The Foundation retains project work and role history in its domains. Personal preferences and notes that a committer chooses to keep belong in a domain they control ([paper §12](durable-memory-for-disposable-agents.md#12-surviving-long-timescales)). Foundation or employer tooling may propose permitted entries there; the person admits or refuses them, and unanswered proposals expire. Personal-domain ownership is not a reason to collect more information about someone.

At the Foundation this reduces to three rules:

- **Work goes to the project**, under the committer's name, with the agent and model recorded.
- **Personal notes and preferences stay under the person's control.** Project decisions and work records stay with the project, even when useful in a later job.
- **Private lists still bind.** An entry about the committer written while reading a private list carries that label, and leaves only through the list's guard.

When a committer leaves, export of their personal domain still respects source restrictions. Revoke the Foundation's access to it and drop cached views with receipts; their project contributions stay with the project.

---

## 8. Serving and who pays

**Why.** A foundation can subsidise an index for everyone but cannot fund unbounded inference. Who gets what has to follow from roles the Foundation already records.

Apache accounts and project rosters provide a starting point. The proposal uses them to allocate an explicitly funded service; an account or a contributor licence agreement alone grants neither restricted-data access nor an inference budget.

| Principal | `recall` runs | Scope | Why |
|---|---|---|---|
| **Committer or PMC member** | Server-side, on approved models, within an allocated budget and published latency and reliability targets | Subsidised in their project's domains, wider by policy; source grants still apply | They review and maintain the corpus. Subsidised recall can offset that effort, and failed searches help prioritise curation |
| **Contributor on the on-ramp** | A capped allowance, granted by a PMC, expiring | Granted per domain | Mentees, programme participants, release helpers: the people the Foundation wants more of |
| **Public** | On the caller's model through supported sampling or a client-run lookup loop; basic `search` and `get` free to the caller | Public entries, curated or verified | Public access to the index is subsidised; the caller supplies inference for deeper help |

- **Basic lookup is subsidised; inference is metered.** Storage, indexing, bandwidth, operations, and abuse prevention still need a budget. Public lookup has rate and response-size limits; model-assisted search counts as inference.
- **Limits sit in the perimeter.** A committer credential caught in a runaway loop hits its cap and raises an alert automatically.
- **Limits cover spend and throughput** ([paper §12](durable-memory-for-disposable-agents.md#12-surviving-long-timescales)). Commercial models are capped in money and in a share of the Foundation's rate limits with each vendor; Foundation-hosted capacity is capped in time share. Both are shared pools: one committer's burst is another's queue.

**Finding: current rosters inform both review rights and serving tier; explicit policy grants them.** Check roster, policy, source grants, and grant expiry on each operation. Changes invalidate cached filters and views; refuse access when freshness cannot be established, and stop sessions that lose a grant they depend on ([paper §6](durable-memory-for-disposable-agents.md#6-retrieval-and-serving)). Budget changes take effect before another model call.

---

## 9. The seeding plan

**Why.** Seeding is where a wrong or planted fact is cheapest to insert and hardest to find later. The plan keeps it on the ordinary, gated path of [Appendix B](durable-memory-for-disposable-agents.md#appendix-b-seeding).

Start with current domain owners, approved source access, and a review budget. Build live taps, then backfill eligible archives through the same pipeline, oldest first. Keep source dates and stable origin IDs so a replay cannot duplicate tasks or turn an old role into a current grant. Index ahead of distillation, within the raw-access limits in [§6](#6-trust-and-review); failed searches guide what to distil next. Three choices make this concrete for the Foundation.

- **One interface summary per pilot project, first.** What it is, its architecture in a paragraph, governance status, key contracts, where its decisions live. Have the authorised reviewers verify each summary, as [Appendix B](durable-memory-for-disposable-agents.md#appendix-b-seeding) requires, before using it as a trusted starting point. Expand to more domains only after the pilots pass.
- **Golden suites come from users.** Questions asked on user lists and in issues, paired with the resolutions the archives record, become per-project evaluation suites, and committers review them. Questions the community struggled to answer form the stratum of past failures ([paper §11](durable-memory-for-disposable-agents.md#11-evaluating-change)).
- **Old threads need no automatic summary.** Keep eligible archives indexed under their retention policy; distil them only when a real question justifies it. Age alone neither establishes trust nor rules out future usefulness.

### Trackers are the ledger's taps

GitHub issues, Jira, and Bugzilla are where work already arrives, and they stay there ([paper §8](durable-memory-for-disposable-agents.md#8-coordination)).

- The registry maps each repository and tracker project to its domain. An issue becomes a `proposed` task, citing the issue as its stable `origin`; triage decides whether to admit it. Backfill does not reopen closed work automatically.
- A label applied by an authorised committer is evidence triage can weigh. It grants no new tools or permissions, and counts as a required sign-off only when policy explicitly says so.
- Results return as drafts or proposed changes through a governed tool server, under the fleet's identity. Authorised project participants decide what merges or closes. The tool server checks the current claim generation before sending, so an expired worker cannot act. Retries reuse a stable action ID, and uncertain posts are reconciled before reassignment.

### Pilots span the range

Three pilots, deliberately different:

- one large, fast-moving project with a formal proposal culture;
- one mature, stable project with decades of history;
- one podling, to test stewardship during incubation and the eventual handover; any pre-incubation history still enters through the ordinary import path.

Before expanding, each pilot must show that memory pays on its own tasks, in paired runs with and without memory ([paper §11](durable-memory-for-disposable-agents.md#11-evaluating-change)). Fix success criteria and acceptable margins before the runs. Both arms keep ordinary repository and tracker access. Hold out evaluation questions from curation and expose only evidence available at the task's snapshot, so a stored answer cannot pass for useful memory.

The pilot scorecard covers more than model tokens:

| Check | Evidence required before expansion |
|---|---|
| **Useful answers** | Task success and cost per successful task, including curation, infrastructure, and reviewer time; inspect wrong answers and omitted dissent |
| **Volunteer capacity** | Review hours, queue age, and time to respond to escalations stay within limits agreed by the project. Reduce intake or pause promotions when review falls behind |
| **Trust and access** | Test planted instructions, mistaken identity links, expired grants, roster revocation, and private metadata in public summaries; unresolved failures block expansion |
| **Recovery** | Rebuild from a record-and-historian recovery bundle. Recover pending tasks, reject old claims, reconcile uncertain sends, and prove later erasures stay erased |
| **Local direction** | A strategy review can change the agent-work rubric in response to users; cross-project commitments have accepting owners and an escalation path |

Start with public material. Add restricted sources only after their owners approve the processing arrangements and the access and disclosure tests pass. Each pilot needs a named operator, a funded operating period, and a way to stop ingestion and serving while preserving the recoverable record.

---

## 10. What the Foundation taught the design

| Pressure | Finding | Where the design answers |
|---|---|---|
| **Federated strategy** | Many L1s under one L0, with local strategy reviews and accepted cross-project commitments ([§5](#5-a-policy-stack-for-a-federation)) | Per-domain rubrics and local responsibility ([paper §8](durable-memory-for-disposable-agents.md#8-coordination)) |
| **Volunteer time is scarce** | Routine work needs sampled review and batched digests. PMCs set strictness within shared policy, and curation intake must fit the available review time. Measure whether subsidised recall offsets that effort | Gates scale with impact (paper [§5](durable-memory-for-disposable-agents.md#5-the-curator), [§8](durable-memory-for-disposable-agents.md#8-coordination)) |
| **Transparency applies to the memory too** | Publish public policy, approved entries, and safe aggregate metrics. Query text, private-source citations, and individual usage may be sensitive; telemetry needs its own access and retention rules | The read filter applies to all views ([paper §6](durable-memory-for-disposable-agents.md#6-retrieval-and-serving)) |
| **The serving layer may lose funding** | Durable knowledge survives only if someone retains readable recovery bundles and the erasure journal. Generated instruction stubs contain pointers, not a backup. An unfunded service also loses freshness and active review | The record test and recovery drills ([paper §12](durable-memory-for-disposable-agents.md#12-surviving-long-timescales)) |
| **Tooling is contributed** | Assign an operator to each production component and adopt a maintainable version. Generated skill pointers must be checked against approved bundles at load time; ordinary project documentation still has its own authors and review process | The ownership test ([paper §12](durable-memory-for-disposable-agents.md#12-surviving-long-timescales)), [Appendix B](durable-memory-for-disposable-agents.md#appendix-b-seeding) |
| **Names move constantly** | Employers, usernames, and project names change; people and projects persist ([§3](#3-people-and-names)) | Dated identity ([paper §12](durable-memory-for-disposable-agents.md#12-surviving-long-timescales)) |

These requirements fit the proposed components. Whether the resulting system is affordable and workable remains a question for the pilots.

---

## 11. Risks and open questions

These are deployment risks. The pilots are chosen to surface them early.

- **Distilling culture.** Process norms distil from observable behaviour, but telling a norm from a habit takes judgment. Early distillations need heavier PMC review until faithfulness sampling shows they hold.
- **Review capacity at scale.** Even three pilots need volunteers who have agreed to the work. Measure review time and decision delay before estimating wider capacity; sampling must not conceal a growing backlog or repeated mistakes.
- **Privacy beyond policy.** Automated aggregation changes what can be inferred from public archives. Test for prohibited profiling in entries and generated answers, provide a correction and erasure route, and keep sensitive queries out of public telemetry. Gates reduce risk; they cannot guarantee that every violation is caught.
- **Client support for sampling.** [MCP sampling](https://modelcontextprotocol.io/specification/2025-11-25/client/sampling) is a negotiated client capability, with the client controlling model access and approval. Check the capabilities needed for the recall loop; otherwise offer ordinary `search` and `get` for a client-run loop. Measure usability and cost across supported clients.
- **Planted material.** How often planted content reaches a curated tier, and how fast lineage finds what it touched, should be measured from the first pilot.
