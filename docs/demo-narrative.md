# The demo, narrated

Cues for the presenter, and a walkthrough to hand out afterwards. Step numbers match the
headings the demo prints. Figures are from a run on 12 October 2026. Counts and token
figures vary a little between runs. Anything a model decides can vary more: the
checker's verdicts, what the harness sessions do, the replay in 5.5, the answers in act
6 and the re-enactments in act 8.

```sh
make demo-ready HARNESS=claude RESET=1
make demo-all HARNESS=claude TRANSCRIPT=demo.txt
```

`HARNESS` picks the harness that works on your login, in acts 1 and 4. These notes
assume Claude Code, `HARNESS=claude`. Codex on qwen3.8-27b always does the work in acts
2 and 6 and the delegated tasks in act 4; in act 7 it runs on the model the platform
names, and in act 8 on gemma4-26b as well.

The run takes about twenty minutes: about a minute each for acts 1 to 3, about nine for
act 4, five or six for act 5, three for act 6, a minute for act 7, half a minute for act
8, and seconds each for acts 9 to 11. Most acts call models, through the gateway or
Claude Code's own login, and act 9 reads GitHub's public API, so the demo wants the
network; only acts 10 and 11 run offline.

## Who's on stage

- **You**, the owner of the memory. You file tasks, approve what only a person may, and
  erase what shouldn't be there.
- **Claude Code** (`claude` on screen), a real session on your own login, locked inside
  a container. It runs on your login's model, claude-opus-5-5 in this run.
- **Codex** (`codex` on screen) on qwen3.8-27b, a real session through the Foundation's
  LLM gateway, llm.apache.org. It does a curation task in act 2, the delegated work in
  act 4, both conditions in act 6, the task in act 7, and the re-runs in act 8, one of
  them on gemma4-26b.
- **qwen3.8-27b on its own**, called directly by the system: to answer a question in
  1.5, and as the model probed in 5.3 and replayed against in 5.5.
- **gemma4-26b**, the checker. It reads summaries in act 2 and held writes in act 3. In
  5.3 it plays a different model swapped in behind qwen3.8-27b's name, and in act 8 it
  is the other model a session is re-enacted on.
- **Scripted agents**: `agent:rogue`, `agent:release`, `agent:reader`, `agent:a`,
  `agent:librarian` and `agent:architect`. These are the demo script using the same
  tools an agent would. No model is behind them; say so.
- **GitHub**, in act 9: its public API, read by a tap with no credential.
- **The system**: the memory server, the ledger, the policy and the egress (below), and
  the launcher, which runs and measures each harness.

## Words you'll hear

- **Record**: the memory itself, a few hundred Markdown entries.
- **T0**: the short summary every session loads first.
- **Handoff**: the note a session leaves for the next one.
- **Ledger**: the shared task list. "The board" is its open tasks.
- **Trust tiers**: raw (anyone wrote it), curated (an agent tidied it under a curation
  task), verified (a person signed off).
- **Quarantine**: where a write is held, out of search, until you decide. "Held" always
  means this: a write that looks suspect, or that needs a person's approval.
- **Policy**: the rules every write, read and outside request is checked against.
  The rules are entries too.
- **Egress**: the only way out to a model. Every model call goes through it, a
  harness's or the system's own, on to the model's provider or the gateway. It checks,
  logs and meters each one. Web fetches don't go through it; they leave through the
  memory server's own fetch tool (3.9).
- **Harness**: the agent program a model works in, such as Claude Code or Codex.
- **Gateway**: llm.apache.org, the Foundation's LLM gateway. It serves the open models
  in this demo.
- **The paper**: "Durable Memory for Disposable Agents", the design this demo builds,
  in `docs/durable-memory-for-disposable-agents.md`.
- **Platform domain**: where the fleet's own configuration lives as entries: agent
  definitions, eval suites, default models. Each is an *artifact*, loaded only once a
  person verifies it (act 7).
- **Tape**: a session's prompt, harness and model, and what each of its memory calls
  returned. Sessions leave one; one started with a prompt can be run again (act 8).
- **Tap**: a connector to an outside system, run as a WebAssembly component that can
  reach only what its grant names: the fetch tool in 3.9 is one, the GitHub tap in act 9
  another.
- **Event log**: every write and erasure in order, by id and digest, never a body; the
  index can be rebuilt from it and the record (act 10).
- **Bundle**: entries exported at or below a label, as Markdown and a manifest, for
  another record to import (act 10).

## Act 1 · The read path

On stage: you, Claude Code, the memory server and the egress; qwen3.8-27b in 1.5.

The claim: an agent can start from T0 and the last handoff, not from everything that
came before.

**1.1 Before.** 282 entries from this project's real decisions and docs. T0 is 2.3% of
their text, counted in tokens. Search each of 200 entries by its own title, and 193 come
back in the top three, typically in under a millisecond.

**1.2 Recall, no model.** The server has no model here, so it returns eight cited
sections and leaves the writing to the caller. Cheap, and the default.

**1.3 Claude Code works a task.** The demo files a question; Claude Code picks it up,
confined, with memory as its only source. Point at the prefetch line: it was handed five
likely summaries before it asked. The next line shows the record keeping its transcript.
It answers that DR-19, one of the project's decision records, hasn't decided yet, which
is correct: it's still a proposal. The last line shows the harness's own copy of the
transcript removed.

**1.4 Erasure.** You erase a note written by mistake. The receipt keeps a fingerprint,
not the text, so a copy coming back is recognised. Search no longer finds it.

**1.5 Recall through the gateway.** The same question, now answered by qwen3.8-27b. It
reads a lighter pack, summaries rather than sections, and writes a short brief that
cites the entries it used. (With no gateway key it uses an Anthropic API key if one is
set, and otherwise says it is skipped.)

**1.6 After.** Starting from T0 and the handoff costs a little over half of replaying
this session: about 2,600 tokens against 4,700. The session was short; the longer the
session, the bigger the saving. The funnel puts what memory served beside what the model
read.

**1.7 The handoff.** What the next session starts from. Every claim points at an entry.

## Act 2 · Curation and trust tiers

On stage: gemma4-26b checking, Codex doing a curation task, `agent:librarian` (scripted)
nominating, and you approving.

Untended memory goes stale. Here's who tends it, and where an agent's say ends.

**2.1 One cycle.** A survey of the whole record picks out entries that need work.
gemma4-26b checked six summaries against their entries and rejected four, for about
2,700 tokens over seven calls.

**2.2 What it filed.** Curation doesn't edit anything itself. It files tasks on the
ledger like any other work: eight from the survey, four from the checker's rejections.

**2.3 A curation task.** Codex on qwen3.8-27b takes the top task and rewrites the
summary. What it writes lands curated, and no higher.

**2.4 Verified.** `agent:librarian`, the script, not Codex, nominates that entry for
verified. Only a person who didn't nominate it can approve, so you do. Then
`agent:librarian` tries to rewrite the verified entry, and that write is held, not
written. You'll see it again in 3.3.

**2.5 Telemetry.** What each curator did, counted from the record, not from their own
reports.

## Act 3 · The perimeter

On stage: scripted agents writing, reading and fetching; gemma4-26b reading what's held;
the policy deciding; the egress counting tokens; and you.

What gets in, what gets out, and who decides.

**3.1 A rogue write.** `agent:rogue` writes an entry telling other agents what to do.
It's held in quarantine; search never sees it.

**3.2 A false alarm.** `agent:release` writes a harmless release signature. It's held
too, on purpose: a false hold costs little.

**3.3 The checker's verdicts.** gemma4-26b reads each held write and gives a verdict:
one malicious, two benign. One of the benign ones is the rewrite held in 2.4. It only
advises; you decide.

**3.4 Taint.** If an entry turned out to be bad: who read it, and what they wrote after.
Here `agent:reader` read one and wrote a note, so the note is listed.

**3.5 Policy is memory.** A rule written as an entry: decisions are superseded by a
person, not an agent. While it's raw it does nothing, and `agent:a` supersedes a
decision. Once you verify it, the same kind of write is held.

**3.6 An exception.** You approve an exception for one task, until a set date. Every
session sees it in T0.

**3.7 The lint.** How often each rule blocked or allowed something.

**3.8 Spend limits.** For this step, the egress limits the checker to a 400-token budget
at the gateway. The call that crosses it still goes out; the next is refused, with over
550 spent.

**3.9 Fetch.** `agent:reader` fetches web pages through memory. example.com isn't
granted at all. Once it has read internal entries, it can only reach trusted hosts,
since a URL could leak what it read. The repeat fetch comes from the stored copy;
nothing leaves.

**3.10 Reads.** A policy limits agents to public entries. `agent:a` searching finds one
hit; your own search reaches the 50-result cap.

## Act 4 · The task fabric

On stage: you filing tasks, the ledger ranking them, Claude Code delegating and later
picking up the results, and Codex doing each delegated task.

Work lives on the ledger, which outlives any one session.

**4.1 A strategy.** A policy entry says what matters this period. Once you verify it, it
becomes the scoring rule that orders the ledger.

**4.2 A small queue.** The blocker rose above the task it blocks. The ledger did that,
not a worker.

**4.3 The rescore.** Every move comes with its reason; higher priority runs first. One
curation task jumped from 0 to 10, because curation had done 17% of the completed work,
under its guaranteed 20%. The curation tasks filed at -1 dropped to -2, because this
period's strategy weights curation down.

**4.4 Delegation.** Claude Code is asked to compare two decisions using subagents. It
calls its Agent tool twice, once per subagent. Each call is caught before it runs: the
subagent's prompt is filed on the ledger as a task, with a follow-up task that resumes
the comparison once that subagent's task is done, and Claude Code is told the task ids
instead. The ledger folded the first follow-up into the second, which waits for both:
that's the "invalidated" line. When Claude Code's session ends, the system hands Claude
Code's original task back to the board (releases it), with what it wrote filed as a
handoff. It waits there: nobody may claim it while its follow-up is open, so the work
isn't done twice.

**4.5 The delegated work.** Codex on qwen3.8-27b takes each task and files a report.

**4.6 Results come back.** A new Claude Code session claims the follow-up, works it with
both reports, and completes it. Completing the follow-up closes the original "Compare
two decisions" too, with the same handoff: the follow-up was the rest of its work. The
last line shows it completed.

**4.7 Telemetry.** Tasks by kind and status. "gate" is the held writes waiting for you;
"promote" is the approvals to verified.

## Act 5 · Watching it

On stage: the system watching itself.

**5.1 Traces.** Every read, decision, task change and model call is logged as a span. A
small local sink counts them; in production it's any OpenTelemetry collector.

**5.2 Read metrics.** The metrics our design paper proposes, measured. Fetch-through, a
search hit later opened, is 0 of 71 in this run. Most of those hits are the demo's own
scripted searches, which open nothing. Of the entries sessions opened, 1 in 33 was never
cited in that session's handoff.

**5.3 Vendor drift.** Probe qwen3.8-27b twice: no alarms. Check gemma4-26b against
qwen's baseline, as if a vendor swapped the model behind the name: four of six detectors
fire, though latency is only reported.

**5.4 Transcripts.** Kept for 30 days, never searched, reached only through an entry
that cites a turn. When a session ends without leaving a handoff of its own, the
launcher files a note of its last exchanges that cites their turns. Act 4's delegating
session is the one here that did, so it's the one shown. The turn shown is the first,
the prompt.

**5.5 Replay.** The system resends that session's first four model calls, exactly as the
egress recorded them, to qwen3.8-27b, and checks whether qwen makes the same choice at
each. In this run it agreed at two of the four: both delegated, and both wrote. Where
Claude Code checked the task tree, qwen wrote; where Claude Code ended with a reply,
qwen released the task itself. Replay measures whether another model would agree, not
which one is right, and the agreement varies from run to run.

## Act 6 · Memory against no memory

On stage: Codex on qwen3.8-27b, twice, and a grader that's a regular expression, not a
model.

**6.1 With and without.** The same four questions, same harness, same model, same
sandbox. With this memory: 4 of 4. With an empty one: 0 of 4; it searches, finds
nothing, and says so or answers wrongly. With memory it finished in 71 seconds against
126 and wrote less than half as much, about 1,600 tokens against 3,600. It read more
fresh input, about 19,000 tokens against 11,000, and less cached input, the `c` figures:
about 147,000 against 256,000. Cached tokens are input the provider has seen before in
the session and charges less for. These are amounts, not cost; whether the memory run
was cheaper depends on the provider's prices. The host's side is measured too: 7.4
seconds of CPU against 10.3, and the same harness peaking at about 166 MiB either way.

## Act 7 · The platform

On stage: `agent:architect` (scripted) proposing, you verifying, Codex working a task
under what you verified, and the launcher and egress measuring it.

The claim: the fleet's own configuration is memory too, under the same rule. Only what a
person verified runs.

**7.1 A platform domain.** The demo's grants gain a second domain, `dom-platform`, named
as the platform's. Its entries carry artifacts: fenced blocks the launcher reads by kind
and name.

**7.2 An agent proposes a definition.** `agent:architect` writes three artifacts: an
agent definition for Codex (read the decision your task names; end the handoff with a
fixed line), a three-question recall suite, and the model Codex should run on. Written
by an agent, they're raw, and raw never loads; the listing says why.

**7.3 Verified, it loads.** The agent proposes them for verified and you approve, as in
2.4. Now all three load. The agent then tries to rewrite the verified definition, and
that write is held: an agent can't change the instructions the next agent runs under.

**7.4 A task under the definition.** A question goes to Codex with no model named. It
runs on qwen3.8-27b because the platform says so (7.6 names the model), with the
definition added to its prompt. Its handoff ends with the definition's line, which is
how you can tell it was followed; the step says so if a model doesn't follow it.

**7.5 The suite.** The recall suite comes from the platform, not a file. All three
questions find their entry in the top eight however they're asked. Asked by keywords,
the entry comes first every time (mean reciprocal rank 1.00); asked as written, it ranks
lower (0.67).

**7.6 What it cost.** Tokens are one resource. The task took 61 seconds of wall clock
and 4.0 seconds of CPU: 2.4 in the sandbox and 1.6 in the launcher, which includes the
egress. Its largest process peaked at 158 MiB. It made 6 model calls, about 223,000
tokens counting cached input, and spent 53 of its 61 seconds waiting on them. Nothing is
priced in this record, so these are quantities, not money; `prices.toml` can price
CPU-hours and GiB-hours as well as tokens. The last line totals every run in this record
so far, act 6's empty-memory runs aside since they used records of their own: about 12
minutes of wall clock, 11 of them waiting on models, and under a minute of CPU.

## Act 8 · Run it again

On stage: Codex re-running a session from act 6, on qwen3.8-27b and then on gemma4-26b,
with the tape answering its memory calls.

The claim: a session can be run again later, on another model, and scored on whether it
would have done the same.

**8.1 The tapes.** Sessions leave a tape: its prompt, harness and model, and what
each memory call returned. Those started with a prompt can be run again: act 6's four
questions with memory, a session each, left four. This one asked who the promotion gate
is, and made 2 memory calls.

**8.2 The same model again.** The control, on qwen3.8-27b: the same tools in the same
order as the tape. Its second call was asked differently, a different search say, so
from there the record answered it rather than the tape. The same answer it gave in act
6, where it passed. A model isn't deterministic, so even the control can differ.

**8.3 Another model.** On gemma4-26b: its first call already asked differently, and its
tools line up with the tape's less well (alignment 67%, which counts the tools both runs
used in the same order against all the calls either made). The same answer, shorter.

**8.4 Side by side.** Every run of the session against the original: the comparison a
platform eval would keep. Replay (5.5) resends single model calls; re-enactment runs the
whole session.

## Act 9 · From outside

On stage: the GitHub issues tap, GitHub's public API, and you.

**9.1 The grant.** The tap may reach api.github.com and nothing else, files into
`dom-demo`, and holds no credential. It runs as a WebAssembly component with only that
access.

**9.2 Issues become tasks.** The open issues of apache/tooling-trusted-releases, the
Foundation's release tooling: 59 read, 59 tasks filed, each citing its issue. Pull
requests would be skipped.

**9.3 Again.** The same run again files nothing: all 59 are already there. The ledger
knows each issue by its origin and last update.

**9.4 A person admits.** What a tap brings is proposed: no worker claims it until a
person admits it. You admit one.

## Act 10 · The record moves

On stage: the event log and its materializer, you, and a second record, another owner's.

The claim: the record outlives every component. It can be rebuilt from its log, moved to
another owner, and keeps its erasures when it does.

**10.1 The event log.** 319 events: 318 writes and the erasure from 1.4, by id and
digest, never a body. Erasing a body adds an erasure to the log and changes nothing
already in it.

**10.2 The index, rebuilt.** The materializer rebuilds the index from the log and the
record: 304 entries live: all but the four superseded and the one erased, 172 ledger
events replayed, nothing behind the log.

**10.3 Exported at a label.** You mark one note public. A public export carries that one
entry; an internal export carries all 309 entries of `dom-demo`, superseded ones
included, but not the platform's domain. The entry erased in 1.4 is in neither.

**10.4 Imported elsewhere.** A second record, under another owner and domain, imports
the internal bundle: 309 entries. Searching it for DR-19 finds 10 hits.

**10.5 An erasure holds.** The new owner erases one of them, then imports the same
bundle again: 308 skipped as already there, 1 refused, because its erasure receipt holds
a fingerprint of the removed text, and the bundle's copy matches it.

## Act 11 · What this is not, yet

On stage: you.

**11.1 and 11.2 The two lists.** Read out the two lists in the repo's README.md: what
the paper asks for that isn't built, and what's built but only works at laptop scale.
Everything shown is real; the README says what replaces each part.
