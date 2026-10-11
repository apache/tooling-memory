# The demo, narrated

Cues for the presenter, and a walkthrough to hand out afterwards. Step numbers match the
headings the demo prints. Figures are from a run on 11 October 2026. Counts and token
figures vary a little between runs. Anything a model decides can vary more: the
checker's verdicts, what the harness sessions do, the replay in 5.5 and the answers in
act 6.

```sh
make demo-ready HARNESS=claude RESET=1
make demo-all HARNESS=claude TRANSCRIPT=demo.txt
```

`HARNESS` picks the harness that works on your login, in acts 1 and 4. These notes
assume Claude Code, `HARNESS=claude`. Codex on qwen3.8-27b always does the work in acts
2 and 6, and the delegated tasks in act 4.

The run takes about seventeen minutes: about a minute each for acts 1 to 3, and about
five each for acts 4, 5 and 6.

## Who's on stage

- **You**, the owner of the memory. You file tasks, approve what only a person may, and
  erase what shouldn't be there.
- **Claude Code** (`claude` on screen), a real session on your own login, locked inside
  a container. It runs on your login's model, claude-opus-5-5 in this run.
- **Codex** (`codex` on screen) on qwen3.8-27b, a real session through the Foundation's
  LLM gateway, llm.apache.org. It does a curation task in act 2, the delegated work in
  act 4, and both runs in act 6.
- **qwen3.8-27b on its own**, called directly by the system: to answer a question in
  1.5, and as the model probed in 5.3 and replayed against in 5.5.
- **gemma4-26b**, the checker. It reads summaries in act 2 and held writes in act 3. In
  5.3 it plays a different model swapped in behind qwen3.8-27b's name.
- **Scripted agents**: `agent:rogue`, `agent:release`, `agent:reader`, `agent:a` and
  `agent:librarian`. These are the demo script using the same tools an agent would. No
  model is behind them; say so.
- **The system**: the memory server, the ledger, the policy and the egress (below).

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
this session: about 2,500 tokens against 4,600. The session was short; the longer the
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
handoff.

**4.5 The delegated work.** Codex on qwen3.8-27b takes each task and files a report.

**4.6 Results come back.** A new Claude Code session claims the follow-up, works it with
both reports, and completes it. The follow-up is a task of its own. The original
"Compare two decisions" was released in 4.4 and is still on the board, open to any
worker, so a worker could claim it and do the comparison again. Nothing closes it when
its follow-up completes. That's today's design, and an open question.

**4.7 Telemetry.** Tasks by kind and status. "gate" is the held writes waiting for you;
"promote" is the approvals to verified.

## Act 5 · Watching it

On stage: the system watching itself.

**5.1 Traces.** Every read, decision, task change and model call is logged as a span. A
small local sink counts them; in production it's any OpenTelemetry collector.

**5.2 Read metrics.** The metrics our design paper proposes, measured. Fetch-through, a
search hit later opened, is 0 of 84 in this run. Most of those hits are the demo's own
scripted searches, which open nothing. Of the entries sessions opened, 1 in 26 was never
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
nothing, and says so or answers wrongly. With memory it finished in 93 seconds against
207 and wrote about a third as much, about 2,300 tokens against 6,600. It read more
fresh input, about 28,000 tokens against 26,000, and less cached input, the `c` figures:
about 251,000 against 481,000. Cached tokens are input the provider has seen before in
the session and charges less for. These are amounts, not cost; whether the memory run
was cheaper depends on the provider's prices.

## Act 7 · What this is not, yet

On stage: you.

**7.1 and 7.2 The two lists.** Read out the two lists in the repo's README.md: what the
paper asks for that isn't built, and what's built but only works at laptop scale.
Everything shown is real; the README says what replaces each part.