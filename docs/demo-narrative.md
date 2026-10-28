# The demo, narrated

This document gives cues for the presenter. It is also a walkthrough to hand out after
the demo. The step numbers match the headings that the demo prints. The figures are from
a run on 12 October 2026. Counts and token figures change a little from run to run.
Anything that a model decides can change more:

- the checker's verdicts
- what the harness sessions do
- the replay in 5.5
- the answers in act 6
- the re-enactments in act 8.

```sh
make demo-ready HARNESS=claude RESET=1
make demo-all HARNESS=claude TRANSCRIPT=demo.txt
```

`HARNESS` selects the harness that works on your login, in acts 1 and 4. These notes
assume Claude Code, `HARNESS=claude`. Codex on qwen3.8-27b always does the work in acts
2 and 6, and the delegated tasks in act 4. In act 7, Codex runs on the model that the
platform names. In act 8, it also runs on gemma4-26b.

The run takes about twenty minutes. Each act takes about this long:

- acts 1 to 3: a minute each
- act 4: nine minutes
- act 5: five or six minutes
- act 6: three minutes
- act 7: a minute
- act 8: half a minute
- acts 9 to 11: seconds each.

Most acts call models, through the gateway or through Claude Code's own login. Act 9
reads GitHub's public API. Thus the demo needs the network. Only acts 10 and 11 run
offline.

## Who's on stage

- **You**, the owner of the memory. You file tasks, approve what only a person may
  approve, and erase what should not be there.
- **Claude Code** (`claude` on screen), a real session on your own login, confined in a
  container. It runs on your login's model: claude-opus-5-5 in this run.
- **Codex** (`codex` on screen) on qwen3.8-27b, a real session through the Foundation's
  LLM gateway, llm.apache.org. Codex does:
  - a curation task in act 2
  - the delegated work in act 4
  - both conditions in act 6
  - the task in act 7
  - the re-runs in act 8, one of them on gemma4-26b.
- **qwen3.8-27b on its own**. The system calls it directly: to answer a question in 1.5,
  as the model that 5.3 probes, and as the model that 5.5 replays against.
- **gemma4-26b**, the checker. It reads summaries in act 2 and held writes in act 3. In
  5.3, it takes the role of a different model, swapped in behind the name qwen3.8-27b.
  In act 8, it is the other model that a session is re-enacted on.
- **Scripted agents**: `agent:rogue`, `agent:release`, `agent:reader`, `agent:a`,
  `agent:librarian` and `agent:architect`. These are the demo script. The script uses
  the same tools that an agent would use. No model is behind them. Say so.
- **GitHub**, in act 9: its public API. A tap with no credential reads it.
- **The system**: the memory server, the ledger, the policy and the egress (below), and
  the launcher, which runs and measures each harness.

## Words you'll hear

- **Record**: the memory itself, a few hundred Markdown entries.
- **T0**: the short summary that every session loads first.
- **Handoff**: the entry that a session leaves for the next session.
- **Ledger**: the shared tasks. "The board" means its open tasks.
- **Trust tiers**: raw (anyone wrote it), curated (an agent tidied it under a curation
  task), verified (a person approved it).
- **Quarantine**: the place where the policy holds a write, out of search, until you
  decide. "Held" always has this meaning: a write that looks suspect, or that needs the
  approval of a person.
- **Policy**: the rules that check every write, read and outside request. The rules are
  entries too.
- **Egress**: the only way out to a model. Every model call goes through it, from a
  harness or from the system itself, on to the model's provider or the gateway. The
  egress checks, logs and meters each call. Web fetches do not go through it. They leave
  through the memory server's own fetch tool (3.9).
- **Harness**: the agent program that a model works in, such as Claude Code or Codex.
- **Gateway**: llm.apache.org, the Foundation's LLM gateway. It serves the open models
  in this demo.
- **The paper**: "Durable Memory for Disposable Agents", the design that this demo
  builds, in `docs/durable-memory-for-disposable-agents.md`.
- **Platform domain**: the domain that holds the fleet's own configuration as entries:
  agent definitions, eval suites and default models. Each is an *artifact*. An artifact
  loads only after a person verifies it (act 7).
- **Tape**: a session's prompt, harness and model, and what each of its memory tool calls
  returned. Sessions leave a tape. A session that started with a prompt can run again
  (act 8).
- **Tap**: a component that connects to an outside system. It runs as a WebAssembly
  component, and it can reach only what its grant names. The fetch tool in
  3.9 is a tap. The GitHub tap in act 9 is another.
- **Event log**: every write and erasure in order, by id and digest, never a body. The
  index can be rebuilt from the event log and the record (act 10).
- **Bundle**: entries exported at or below a label, as Markdown and a manifest, for
  another record to import (act 10).

## Act 1 · The read path

On stage: you, Claude Code, the memory server and the egress; qwen3.8-27b in 1.5.

The claim: an agent can start from T0 and the last handoff, not from everything that
came before.

**1.1 Before.** The record has 282 entries from this project's real decisions and docs.
T0 is 2.3% of their text, counted in tokens. The demo searches for each of 200 entries by
its own title. Of the 200, 193 come back in the top three, typically in less than a
millisecond.

**1.2 Recall, no model.** Here the memory server has no model. Thus it returns eight
cited sections, and it leaves the writing to the caller. This costs little, and it is
the default.

**1.3 Claude Code works a task.** The demo files a question. Claude Code takes it, in
confinement, with memory as its only source. Point to the prefetch line: the system gave
Claude Code five likely summaries before it asked. The next line shows that the record
keeps its transcript. Claude Code answers that DR-19, one of the project's decision
records, has not decided yet. This is correct: DR-19 is still a proposal. The last line
shows that the harness's own copy of the transcript is removed.

**1.4 Erasure.** You erase a note that was written by mistake. The receipt keeps a
fingerprint, not the text. Thus the system recognises a copy that comes back. Search no
longer finds the note.

**1.5 Recall through the gateway.** qwen3.8-27b now answers the same question. It reads a
smaller pack, with summaries instead of sections. It writes a short brief that cites the
entries that it used. (If there is no gateway key, the step uses an Anthropic API key if
one is set. If neither key is set, the step says that it is skipped.)

**1.6 After.** Starting from T0 and the handoff costs a little more than half as much as
replaying this session: about 2,600 tokens against 4,700. The session was short. A
longer session gives a bigger saving. The funnel shows what memory served beside what the
model read.

**1.7 The handoff.** This is what the next session starts from. Every claim points at an
entry.

## Act 2 · Curation and trust tiers

On stage: gemma4-26b checking, Codex doing a curation task, `agent:librarian` (scripted)
nominating, and you approving.

Memory that nobody maintains becomes out of date. This act shows who maintains it, and
where the authority of an agent stops.

**2.1 One cycle.** A survey of the whole record finds the entries that need work.
gemma4-26b checked six summaries against their entries and rejected four. It used about
2,700 tokens in seven calls.

**2.2 What it filed.** Curation does not edit anything itself. It files tasks on the
ledger, the same as any other work: eight from the survey, and four from the checker's
rejections.

**2.3 A curation task.** Codex on qwen3.8-27b takes the top task and rewrites the
summary. What it writes goes into the record as curated, and not higher.

**2.4 Verified.** `agent:librarian` nominates that entry for verified. `agent:librarian`
is the script, not Codex. Only a person who did not nominate the entry can approve it, so
you approve it. Then `agent:librarian` tries to rewrite the verified entry. The policy
holds that write, and does not write it. You will see it again in 3.3.

**2.5 Telemetry.** This shows what each curator did. The count comes from the record,
not from the curators' own reports.

## Act 3 · The perimeter

On stage: scripted agents writing, reading and fetching; gemma4-26b reading what is held;
the policy deciding; the egress counting tokens; and you.

What gets in, what gets out, and who decides.

**3.1 A rogue write.** `agent:rogue` writes an entry that tells other agents what to do.
The policy holds it in quarantine. Search never sees it.

**3.2 A false alarm.** `agent:release` writes a harmless release signature. The policy
holds it too, on purpose: a false hold costs little.

**3.3 The checker's verdicts.** gemma4-26b reads each held write and gives a verdict: one
malicious, two benign. One of the benign writes is the rewrite that the policy held in
2.4. The checker only advises. You decide.

**3.4 Taint.** This step answers a question: if an entry turned out to be bad, who read
it, and what did they write after? Here `agent:reader` read one and wrote a note. Thus
the list shows the note.

**3.5 Policy is memory.** A rule written as an entry: a person, not an agent, supersedes
decisions. While the rule is raw, it does nothing, and `agent:a` supersedes a decision.
After you verify the rule, the policy holds the same kind of write.

**3.6 An exception.** You approve an exception for one task, until a set date. Every
session sees it in T0.

**3.7 The lint.** This shows how often each rule blocked or allowed something.

**3.8 Spend limits.** For this step, the egress gives the checker a spend limit of 400
tokens at the gateway. The call that goes over the limit still goes out. The egress
refuses the next call. By then, more than 550 tokens are spent.

**3.9 Fetch.** `agent:reader` fetches web pages through memory. No grant names
example.com. After `agent:reader` has read internal entries, it can reach only trusted
hosts, because a URL could leak what it read. The repeated fetch comes from the stored
copy. Nothing leaves.

**3.10 Reads.** A policy limits agents to public entries. When `agent:a` searches, it
finds one hit. Your own search reaches the cap of 50 results.

## Act 4 · The task fabric

On stage: you filing tasks, the ledger ranking them, Claude Code delegating and later
taking the results, and Codex doing each delegated task.

The work is on the ledger. The ledger continues after any one session ends.

**4.1 A strategy.** A policy entry says what is important in this period. After you
verify it, it becomes the scoring rule that puts the ledger in order.

**4.2 A small queue.** The blocking task moved above the task that it blocks. The ledger
did that, not a worker.

**4.3 The rescore.** Each move shows its reason. A task with a higher priority runs
first. One curation task moved from 0 to 10. The reason: curation had done 17% of the
completed work, less than its guaranteed 20%. The curation tasks filed at -1 went down
to -2, because this period's strategy gives curation a lower weight.

**4.4 Delegation.** The demo asks Claude Code to compare two decisions with subagents.
Claude Code calls its Agent tool twice, once for each subagent. The system stops each
call before it runs. It files the subagent's prompt on the ledger as a task. With that
task, it files a follow-up task that continues the comparison after the subagent's task
is done. Claude Code gets the task ids instead. The ledger merged the first follow-up
task into the second, which waits for both. That is the "invalidated" line. When Claude
Code's session ends, the system releases Claude Code's original task: it puts the task
back on the board. The system files what Claude Code wrote as a handoff. The task waits
there. Nobody may claim it while its follow-up task is open. Thus nobody does the work
twice.

**4.5 The delegated work.** Codex on qwen3.8-27b takes each task and files a report.

**4.6 Results come back.** A new Claude Code session claims the follow-up task, works on
it with both reports, and completes it. When the follow-up task completes, the original
task "Compare two decisions" also closes, with the same handoff. The reason: the
follow-up task was the rest of its work. The last line shows it completed.

**4.7 Telemetry.** Tasks by kind and status. "gate" means the held writes that wait for
you. "promote" means the approvals to verified.

## Act 5 · Watching it

On stage: the system watching itself.

**5.1 Traces.** The system logs every read, decision, task change and model call as a
span. A small local sink counts the spans. In production, the sink is any OpenTelemetry
collector.

**5.2 Read metrics.** The demo measures the metrics that our design paper proposes.
Fetch-through, a search hit that a caller opens later, is 0 of 71 in this run. Most of
those hits come from the demo's own scripted searches, which open nothing. Of the entries
that sessions opened, 1 in 33 was never cited in that session's handoff.

**5.3 Vendor drift.** The demo probes qwen3.8-27b twice, with no alarms. Then it checks
gemma4-26b against qwen's baseline, as if a vendor swapped the model behind the name.
Four of six detectors fire, though latency is only reported.

**5.4 Transcripts.** The record keeps transcripts for 30 days. Search never finds them.
You can reach a transcript only through an entry that cites a turn. When a session ends
without leaving a handoff of its own, the launcher files a note of its last exchanges
that cites their turns. Act 4's delegating session is the one here that did. Thus it is
the one that the step shows. The turn shown is the first turn, the prompt.

**5.5 Replay.** The system sends the first four model calls of that session to
qwen3.8-27b again, exactly as the egress recorded them. It checks whether qwen makes the
same choice at each call. In this run, qwen agreed at two of the four: both delegated,
and both wrote. Where Claude Code checked the task tree, qwen wrote. Where Claude Code
ended with a reply, qwen released the task itself. Replay measures whether another model
would agree, not which model is right. The agreement changes from run to run.

## Act 6 · Memory against no memory

On stage: Codex on qwen3.8-27b, twice, and a grader that is a regular expression, not a
model.

**6.1 With and without.** Both runs use the same four questions, the same harness, the
same model and the same sandbox. With this memory: 4 of 4. With an empty memory: 0 of 4.
Codex searches, finds nothing, and says so or gives a wrong answer. With memory, it
finished in 71 seconds against 126. It wrote less than half as much: about 1,600 tokens
against 3,600. It read more fresh input, about 19,000 tokens against 11,000. It read
less cached input, the `c` figures: about 147,000 against 256,000. Cached tokens are
input that the provider has seen before in the session, and the provider charges less
for them. These are amounts, not cost. The provider's prices decide whether the memory
run was cheaper. The demo also measures the host's side: 7.4 seconds of CPU against
10.3. In both runs, the same harness had a peak of about 166 MiB.

## Act 7 · The platform

On stage: `agent:architect` (scripted) proposing, you verifying, Codex working on a task
under what you verified, and the launcher and egress measuring it.

The claim: the fleet's own configuration is memory too, under the same rule. Only what a
person verified runs.

**7.1 A platform domain.** The demo's grants get a second domain, `dom-platform`, named
as the platform's domain. Its entries hold artifacts: fenced blocks that the launcher
reads by kind and name.

**7.2 An agent proposes a definition.** `agent:architect` writes three artifacts:

- an agent definition for Codex (read the decision your task names; end the handoff with
  a fixed line)
- a recall suite of three questions
- the model that Codex should run on.

An agent wrote them, so they are raw, and raw never loads. The listing says why.

**7.3 Verified, it loads.** The agent proposes them for verified, and you approve them,
as in 2.4. Now all three load. Then the agent tries to rewrite the verified definition.
The policy holds that write: an agent cannot change the instructions that the next agent
runs under.

**7.4 A task under the definition.** A question goes to Codex, with no model named.
Codex runs on qwen3.8-27b because the platform says so (7.6 names the model). Its prompt
includes the definition. Its handoff ends with the definition's line. That is how you
can tell that Codex followed the definition. If a model does not follow it, the step
says so.

**7.5 The suite.** The recall suite comes from the platform, not from a file. All three
questions find their entry in the top eight, however they are asked. When the question
is keywords, the entry comes first every time (mean reciprocal rank 1.00). When the
question is as written, the entry ranks lower (0.67).

**7.6 What it cost.** Tokens are one resource. The task took 61 seconds of wall clock and
4.0 seconds of CPU. The CPU time was 2.4 in the sandbox and 1.6 in the launcher, which
includes the egress. Its largest process had a peak of 158 MiB. It made 6 model calls,
about 223,000 tokens including cached input. It spent 53 of its 61 seconds waiting on
them. Nothing in this record has a price, so these are quantities, not money.
`prices.toml` can set prices for CPU-hours and GiB-hours, as well as for tokens. The last
line gives the total of every run in this record so far. It does not include act 6's
empty-memory runs, because they used records of their own. The total is about 12 minutes
of wall clock, 11 of them waiting on models, and less than a minute of CPU.

## Act 8 · Run it again

On stage: Codex re-running a session from act 6, on qwen3.8-27b and then on gemma4-26b,
with the tape answering its memory tool calls.

The claim: you can run a session again later, on another model, and score it on whether
it would have done the same.

**8.1 The tapes.** Sessions leave a tape: its prompt, harness and model, and what each
memory tool call returned. A session that started with a prompt can run again. Act 6's
four questions with memory used a session each, and left four. This one asked who the
promotion gate is, and made 2 memory tool calls.

**8.2 The same model again.** The control, on qwen3.8-27b: the same tools in the same
order as the tape. Its second call asked differently, for example with a different
search. So from there, the record answered it, not the tape. It gave the same answer as
in act 6, where it passed. A model is not deterministic, so even the control can be
different.

**8.3 Another model.** On gemma4-26b, its first call already asked differently. Its
tools align with the tape's less well: alignment 67%. Alignment counts the tools that
both runs used in the same order, against all the calls that either run made. It gave
the same answer, shorter.

**8.4 Side by side.** This step shows every run of the session against the original.
This is the comparison that a platform eval would keep. Replay (5.5) sends single model
calls again. Re-enactment runs the whole session.

## Act 9 · From outside

On stage: the GitHub issues tap, GitHub's public API, and you.

**9.1 The grant.** The tap may reach api.github.com and nothing else. It files into
`dom-demo`, and it holds no credential. It runs as a WebAssembly component with only that
access.

**9.2 Issues become tasks.** The tap reads the open issues of
apache/tooling-trusted-releases, the Foundation's release tooling. It read 59 and filed
59 tasks, and each task cites its issue. The tap would skip pull requests.

**9.3 Again.** When the same run occurs again, it files nothing: all 59 are already
there. The ledger knows each issue by its origin and last update.

**9.4 A person admits.** What a tap brings is proposed. No worker claims it until a
person admits it. You admit one.

## Act 10 · The record moves

On stage: the event log and its materializer, you, and a second record, another owner's.

The claim: the record lasts longer than every component. You can rebuild it from its log
and move it to another owner. When you do, it keeps its erasures.

**10.1 The event log.** The log has 319 events: 318 writes and the erasure from 1.4, by
id and digest, never a body. An erasure of a body adds an erasure to the log. It changes
nothing already in the log.

**10.2 The index, rebuilt.** The materializer rebuilds the index from the log and the
record. 304 entries are live: all except the four superseded entries and the one erased
entry. It replays 172 ledger events. Nothing is behind the log.

**10.3 Exported at a label.** You mark one note public. A public export carries that one
entry. An internal export carries all 309 entries of `dom-demo`, with the superseded
ones, but not the platform's domain. The entry erased in 1.4 is in neither export.

**10.4 Imported elsewhere.** A second record, under another owner and domain, imports the
internal bundle: 309 entries. A search of it for DR-19 finds 10 hits.

**10.5 An erasure holds.** The new owner erases one of the entries. Then the new owner
imports the same bundle again. The import skips 308 entries as already there, and it
refuses 1. It refuses that entry because its erasure receipt holds a fingerprint of the
removed text, and the bundle's copy matches it.

## Act 11 · What this is not, yet

On stage: you.

**11.1 and 11.2 The two lists.** Read out the two lists in the repo's README.md. One list
is what the paper asks for that is not built. The other list is what is built but only
works at laptop scale. Everything that the demo shows is real. The README says what
replaces each part.
