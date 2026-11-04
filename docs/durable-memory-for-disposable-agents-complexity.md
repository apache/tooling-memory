# Appendix H: How Much System This Is

Appendix H of [*Durable Memory for Disposable Agents*](durable-memory-for-disposable-agents.md), kept in its own file because its counts change with every revision of the paper. The counts below are from the paper at commit `ab52e91` (4 November 2026), the revision before this appendix was added, produced by the inventory script in [WRITING.md](../WRITING.md). The sentence that points at this appendix adds 28 words to §1.

## Why count

**Why.** The paper's rule for a fleet's policy applies to the paper: rules accrete, each one reasonable, until the sum stops the work ([§7](durable-memory-for-disposable-agents.md#7-the-policy-stack)). A reader who weighs building this needs three things the paper's sections cannot give one at a time: how many parts there are, what each part buys, and what falls away when one of the paper's premises does not hold. This appendix gives those three. It makes the trade-offs visible and recommends no cut. How a change to the paper is judged against these counts is a later step.

## The inventory

The script counts what the paper names. A role is a row of the [§2](durable-memory-for-disposable-agents.md#roles) table. A rule is a list item in [Appendix C](durable-memory-for-disposable-agents.md#appendix-c-ledger-semantics) or [Appendix E](durable-memory-for-disposable-agents.md#appendix-e-rules-in-detail). A glossary term is a word a reader has to learn.

| Kind | Count |
|---|---|
| Roles (§2 table) | 16 |
| Principles (§2) | 11 |
| Entry types | 7 |
| Labels (§4 four questions) | 4 |
| Trust tiers | 3 |
| Policy layers (§7) | 4 |
| Perimeter limits (§12) | 5 |
| Interface operations (Appendix A) | 20 |
| Task states (Appendix C) | 6 |
| Metrics (§9) | 21 |
| Evaluation arms (§11) | 5 |
| Failure modes (Appendix D) | 38 |
| Rules (Appendix C) | 59 |
| Rules (Appendix E) | 35 |
| Glossary terms (Appendix F) | 44 |
| Diagrams | 9 |
| Evidence rows (Appendix G) | 157 |
| Evidence rows at Argued only | 65 |

Sixteen roles, twenty operations and forty-four terms fit on a page. Ninety-four rules and thirty-eight failure modes do not. The parts are few; the rules are where the system is large.

## Where the weight sits

| Section | Words | List items | Table rows |
|---|---|---|---|
| 1. Why | 781 | 12 | 0 |
| 2. The Design at a Glance | 1052 | 11 | 16 |
| 3. Memory | 1669 | 17 | 17 |
| 4. Trust and Handling | 1783 | 21 | 8 |
| 5. The Curator | 1233 | 26 | 0 |
| 6. Retrieval and Serving | 1573 | 8 | 0 |
| 7. The Policy Stack | 755 | 7 | 0 |
| 8. Coordination | 3145 | 27 | 17 |
| 9. Measurement | 917 | 9 | 21 |
| 10. Evolving the System | 984 | 8 | 5 |
| 11. Evaluating Change | 1017 | 5 | 11 |
| 12. Surviving Long Timescales | 1787 | 14 | 8 |
| Appendix C: Ledger Semantics | 1760 | 59 | 6 |
| Appendix E: Rules in Detail | 1190 | 35 | 0 |

Two places carry the weight. Coordination ([§8](durable-memory-for-disposable-agents.md#8-coordination) with Appendix C) is about 4,900 words and 86 list items, more than twice the next largest section. Trust and handling with the curator ([§4](durable-memory-for-disposable-agents.md#4-trust-and-handling) and [§5](durable-memory-for-disposable-agents.md#5-the-curator)) is about 3,000 words and 47 list items.

The two differ in kind. The coordination rules are the invariants any task ledger with leases needs: a claim with a lease, a fencing generation, an origin for dedup, an append-only log with the task table as its projection. The code holds them (`crates/ledger/src/lib.rs`). The trust rules are the paper's own contribution, and most of their evidence is single studies and incidents ([Appendix G](durable-memory-for-disposable-agents-appendix.md)).

Every other section is under 1,800 words. The parts in the [§2](durable-memory-for-disposable-agents.md#2-the-design-at-a-glance) diagram are each simple: the subsections that introduce the record, the historian, search and access control are each under 500 words.

## What each part is for

**Why.** Every part of the design is a remedy for a failure, and every failure follows from a premise. [§1](durable-memory-for-disposable-agents.md#1-why) states three constraints. Six premises split them finer, and each is one to three of the paper's [principles](durable-memory-for-disposable-agents.md#principles). Sorting the parts under them shows what each premise costs and what it buys, and Beer's Viable System Model ([§13](durable-memory-for-disposable-agents.md#13-related-work)) shows which function of a viable organisation each premise serves.

| Premise | Principles | VSM system | Parts | Rules | Failure modes |
|---|---|---|---|---|---|
| Model output is untrusted input | 7 | 3* | The four labels and three tiers, the screen and quarantine, the promotion gate, lineage and erasure, containing the curator, the auditors, the guards and `declassify`, the principal chain | 19 (Appendix E: access freshness, labels, raw reach, screening) and 6 (Appendix C: delegation) | 11 |
| Agents are disposable | 2, 3, 4 | 1, 2 | The handoff, T0, the ledger with leases and fencing, waiting as a ledger state, task definitions and matching, positions, the recall deputy | 35 (Appendix C: storage, claims, ranking, endings, positions, outages) | 5 |
| Ownership limits conflict | 5 | 1, at each level | Domains, local responsibility, coordination across domains, taps, the platform as a domain | 14 (Appendix C: across domains, taps) | 1 |
| The system must stay viable for years | 6, 8, 9 | 3, 3*, 4, 5 | The policy stack and its rule lifecycle, the ceiling and the reset, exceptions and approvals, the five limits, autonomy levels, the rubric compiled from strategy, the loop and its gates, the curator as permanent work, the horizon scanner | 4 (Appendix C: approvals) | 13 |
| Evaluation is built in | 11 | 3 | The 21 metrics, the evaluator, the five arms, two replays, golden suites, canary entries, vendor-drift probes | 11 (Appendix E: replay state, replay traps, vendor drift) | 3 |
| The institution outlives its parts | 1, 10 | 5 | The plain-text record, stable identifiers and the registry, personal domains, recovery bundles, common components | 5 (Appendix E: recovery) | 5 |

The failure-mode column sorts the 38 rows of [Appendix D](durable-memory-for-disposable-agents.md#appendix-d-failure-modes), each under one premise. The rules column sorts all 94 list items of Appendices C and E by subsection.

**Viability is where the argued design lives.** The fourth premise has the most failure modes and the fewest rules.

Appendix G grades 65 of its 157 rows at Argued, the level for a property that rests on the paper's reasoning and no source, and by its own count most rows of [§7](durable-memory-for-disposable-agents.md#7-the-policy-stack) and [§12](durable-memory-for-disposable-agents.md#12-surviving-long-timescales), where this premise's parts live, are Argued. Beer's claim is that a system stays viable only with all five of its systems, and 3, 3*, 4 and 5 are the ones this premise supplies, so the premise is not optional. The open choice in each row of the [§13](durable-memory-for-disposable-agents.md#13-related-work) table is whether the function is a mechanism or a person.

**Chokepoints are a class of their own.** One record, one ledger, one memory server per session ([§6](durable-memory-for-disposable-agents.md#one-server-per-session)) and one way out through taps and tool servers ([§4](durable-memory-for-disposable-agents.md#labels-and-flow)) serve several premises at once. The one server applies the labels once, checks the principal chain once, observes every call and enforces the limits in one place; the paper's reason is that enforcement split across servers leaves gaps. Each chokepoint is complexity spent to remove enforcement from every harness, tool and tap, and principle 4 is its statement.

## Built and described

**Why.** A reader should know which parts are code and which are a plan, because the two carry different kinds of evidence. The code at the same commit implements a first cut of most mechanisms and none of the standing roles. The evidence for each cell is a module's header comment or a type definition at commit `ab52e91`.

| Premise | In the code | Described only |
|---|---|---|
| Model output is untrusted input | Trust tiers and the read filter (`crates/core/src/lib.rs`, `crates/core/src/filter.rs`); the injection scan and quarantine (`crates/policy/src/scan.rs`, `crates/memory/src/review.rs`); erasure and tombstones (`crates/core/src/erasure.rs`); the checker as the audit agent (`crates/memory/src/checker.rs`); the per-domain event log (`crates/server/src/log.rs`); the promotion gate as a proposal and an approval by two identities, and lineage derived from the read and write logs (`crates/server/src/lib.rs`) | Lineage edges the historian stores, the guards and `declassify`, the principal chain |
| Agents are disposable | The ledger with leases, handoffs and origin dedup (`crates/ledger/src/lib.rs`); T0 (`crates/server/src/t0.rs`); the recall deputy (`crates/server/src/recall.rs`); matching a task's requirements against a session's manifest (`crates/ledger/src/lib.rs`) | Positions, task definitions, waiting as a ledger state |
| Ownership limits conflict | Domains in the read filter; taps (`crates/taps`, `crates/server/src/taps.rs`); the platform as a domain (`crates/server/src/platform.rs`); bundles out and in (`crates/server/src/bundle.rs`) | Coordination across domains |
| The system must stay viable for years | Policy as layered entries with Rego modules, exceptions that expire, enforcement only once verified (`crates/policy/src/record.rs`); the curation cycle with its telemetry (`crates/memory/src/curation.rs`); spend limits at the egress (`crates/egress/src/lib.rs`); the rubric compiled from strategy (`crates/server/src/fabric.rs`) | The rule ceiling and the reset, autonomy levels, approvals with a scope and an expiry that the perimeter checks again when the action runs, the other four limits, the horizon scanner |
| Evaluation is built in | Read and write telemetry (`crates/server/src/reads.rs`); golden suites (`crates/memory/src/golden.rs`); frozen-context replay and re-enactment (`crates/memory/src/replay.rs`, `crates/memory/src/reenact.rs`); canary entries (`crates/memory/src/canary.rs`); vendor-drift probes (`crates/memory/src/drift.rs`); cost (`crates/memory/src/cost.rs`) | The evaluator, the five arms, most of the 21 metrics |
| The institution outlives its parts | Versioned plain-text entries over OpenDAL (`crates/store`); stable meaningless identifiers (`crates/core/src/id.rs`); the bundle manifest (`crates/core/src/manifest.rs`) | The registry, personal domains, recovery drills |

A module named here holds a first cut, and the paper's rule may be wider than what the module checks.

## Is this much expected

**Why.** The question a reader asks is whether a software factory, a fleet of coding agents run as one production line, has to be this large. Two sources from theory and eleven texts from practice, each read in full, give the answer in two parts.

**From theory.** Beer's test is viability, and the 1984 article ([[76]](durable-memory-for-disposable-agents.md#ref-76)) holds the five systems necessary and sufficient for it. A factory that only does work has System 1 and nothing to keep it viable. Ashby's law ([[77]](durable-memory-for-disposable-agents.md#ref-77)) gives the measure: a regulator needs as much variety as the disturbances it regulates. The 38 failure modes are the disturbances the paper sees, and the rules are the regulator. The table above sets the two side by side per premise; a premise with many rules and few failure modes is where to look first for rules that regulate nothing.

**From practice.** Five running systems and one proposal were measured with the rubric of this appendix, by a reader who listed every named part, rule and failure in the full text and quoted its definition. Gas Town and its successor Wheelhouse, which the paper's [§4](durable-memory-for-disposable-agents.md#model-output-in-particular) and [§7](durable-memory-for-disposable-agents.md#7-the-policy-stack) both call Gas Town, are one operator's fleet of twenty to sixty agents ([[116]](durable-memory-for-disposable-agents.md#ref-116), [[117]](durable-memory-for-disposable-agents.md#ref-117), [[118]](durable-memory-for-disposable-agents.md#ref-118), [[120]](durable-memory-for-disposable-agents.md#ref-120), [[121]](durable-memory-for-disposable-agents.md#ref-121), [[137]](durable-memory-for-disposable-agents.md#ref-137), [[138]](durable-memory-for-disposable-agents.md#ref-138)); the Counting House is the same author's proposal for a clerk pool under a mechanical orchestrator ([[119]](durable-memory-for-disposable-agents.md#ref-119)).

OpenAI's account ([[130]](durable-memory-for-disposable-agents.md#ref-130)) was read with an interview-based report that is paywalled after its third section, and the DoorDash ([[131]](durable-memory-for-disposable-agents.md#ref-131)) and Ramp ([[132]](durable-memory-for-disposable-agents.md#ref-132)) accounts stand alone. A part here is a kind of role or component, so Wheelhouse's 38 named individual seats, instances of one kind, are not in its count. The paper's row counts its roles only; on the practice rubric, which counts tools and stores as parts, its count would be higher. A failure "happened" when the text reports it as an event; the paper's failure modes are guards.

| System | Setting | Kinds of part | Rules stated | Failures named | Of which happened |
|---|---|---|---|---|---|
| This paper | An organisation over years, described | 16 roles | 94 | 38 | none |
| Gas Town (January 2026) | One operator, 20 to 30 agents | 17 | 19 | 19 | 14 |
| Wheelhouse (August to October 2026) | One operator, 50 to 60 agents | 27 | 45 | 47 | 40 |
| The Counting House (proposal) | A company's contact centre | 18 | 25 | 18 | 3 |
| OpenAI | A company, most employees | 38 | 21 | 21 | 13 |
| DoorDash | A company, 130,000 tasks a month | 12 | 6 | 7 | 5 |
| Ramp | A company, engineers and others | 28 | 12 | 11 | 1 |

The counts are a reader's and approximate: two readers of the Gas Town texts disagreed on sixteen rows, mostly on whether a thing is a part or a record.

Three things hold across the disagreements. The paper's sixteen roles sit inside the range of parts the practice sources name, twelve to thirty-eight.

The paper states more rules than any source, and the one source whose rules grew as its fleet ran, Wheelhouse, reports several hundred fences cut to fourteen in one reset, with every new one approved by its operator ([[117]](durable-memory-for-disposable-agents.md#ref-117), [[121]](durable-memory-for-disposable-agents.md#ref-121)). The paper's failure modes are written before the fact; Wheelhouse's forty are written after it. After its reset that fleet reached the paper's remedy for rule growth: a ceiling, with a person signing off each rule above it.

The premises sort the sources as the table in [What falls away](#what-falls-away) predicts. Every practice source measures outcomes, Gas Town's first account excepted. Policy with a lifecycle appears at OpenAI, where a failing documentation rule is promoted into a linter ([[130]](durable-memory-for-disposable-agents.md#ref-130)), and at Wheelhouse, where rules go through one.

Self-improvement appears at OpenAI, at Wheelhouse, and in the Counting House under human sign-off. Many owners appear at Ramp, where each prompt that changes code is attributed to its author ([[132]](durable-memory-for-disposable-agents.md#ref-132)). No deployed source grades its data by how far it has been checked; the Counting House proposal comes closest, by re-checking what a model read against the system of record ([[119]](durable-memory-for-disposable-agents.md#ref-119)). The first premise and its eleven failure modes are the paper's addition, and the practice sources neither confirm nor refute it.

## What falls away

**Why.** A deployment that does not hold one of the premises can leave that premise's parts out, and the failure-mode column says what it then accepts.

| Setting | Premises that hold | Parts it needs | Failure modes it accepts |
|---|---|---|---|
| One operator, trusted inputs, one machine | Agents are disposable; evaluation is built in | The parts of those two premises: the ledger, handoffs, T0, the recall deputy, the metrics and replays among them | The 11 of untrusted input, the 1 of ownership, the 13 of viability, the 5 of long timescales |
| One team on shared infrastructure | The two above, and model output is untrusted input | The above and the parts of the first premise: the labels and tiers, the screen, the gate, lineage, the auditors | The 1 of ownership, the 13 of viability, the 5 of long timescales |
| An organisation over years | All six | The whole design | None by choice |

The first row is one operator's fleet, as Gas Town's author reports his ([[116]](durable-memory-for-disposable-agents.md#ref-116), [[117]](durable-memory-for-disposable-agents.md#ref-117)). The paper describes the third row and says so in its title: the agents are disposable, and the institution is what endures.

## The budget

**Why.** Growth in the inventory is invisible from inside a revision, as growth in a domain's rules is invisible from inside a task. The counts above, produced by the inventory script in [WRITING.md](../WRITING.md) at every revision and compared with the previous revision's, make growth show. A revision that adds a role, a label, a limit, an operation, a rule subsection or a failure mode changes a count; a section that passes 2,000 words or 30 list items shows in the second table.

This appendix sets no ceiling and names no cut. It gives the numbers a later decision needs.
