---
description: Hand a docs-only branch to two fresh subagents, one that reads only the changed files and one that fact-checks them against the code
argument-hint: "[branch] (defaults to the current one)"
---

# Read the branch before the pull request

AGENTS.md asks for this in place of `/adversarial` on a docs-only branch, one where
`git diff --name-only --no-renames origin/main...origin/<branch> | scripts/docs-only.sh`
exits 0: after the push and before the PR is opened, and for a revision of an open PR,
after its push and before review is re-requested. A doc fails in two ways that a reviewer
who knows the code does not see: a reader comes away believing something other than what
was meant, or the doc says something the code does not do. One subagent tests each.

Work out the branch (`$ARGUMENTS`, or `git rev-parse --abbrev-ref HEAD`), confirm it is
pushed and that `origin/<branch>` is what you mean to have read. Then, before spawning
anything:

1. **Write the questions.** Three to five questions the change should let a reader
   answer, each with the answer you mean the doc to give. Ask what a reader would act on
   ("which part stops X", "what do you lose if you Y"), not what a sentence says. Keep the
   answers to yourself; the reader never sees them. The reader is not quite cold: every
   session here loads CLAUDE.md, and AGENTS.md through it, before it reads anything. Ask
   what that text cannot answer, and when the change is to AGENTS.md itself, tell the
   reader that the copy in its directory is the one to go by.
2. **Copy the changed files** as they stand on `origin/<branch>` into a fresh directory in
   your scratchpad, with nothing else in it, and save `git diff origin/main...origin/<branch>`
   beside them as `change.diff`.

Then spawn **two fresh subagents of the `general-purpose` type** in one message, never
forks, which would inherit the reasoning that wrote the branch and agree with it. Do not
edit the worktree until both report.

## The reader

> You are a reader new to this project. Read only the files in `<dir>`: the changed
> documents, and `change.diff`, which shows what changed in them. **Read nothing else**: no
> other file in the repository, no code, no web. What a reader cannot find in these files,
> you do not know. Change nothing.
>
> 1. Before the questions: in five sentences or fewer, what does the change tell you that
>    the old text did not?
> 2. Answer each question below from the documents alone. Quote the sentence each answer
>    rests on. Where the documents do not answer, say so; do not guess from what such
>    projects usually do.
> 3. List each place where you stopped or read twice: a term used before it is defined,
>    two passages that disagree, a sentence you could read two ways (give both readings),
>    a claim about a part the documents never introduce.
>
> <the questions, without the answers>

## The fact check

> You are checking a documentation change in this repository against the code. Read
> AGENTS.md as it stands on `origin/<branch>` first. **Change nothing, commit nothing,
> post nothing to GitHub.** The change is `git diff origin/main...origin/<branch>`.
>
> List every claim the added or changed lines make about what the code does, what a
> command does, what a file holds, or which part does what. For each, read the code on
> `origin/<branch>` and give one verdict: **HOLDS** with the `file:line` that shows it,
> **WRONG** with the `file:line` that shows otherwise and what the text should say, or
> **UNCHECKED** with why you could not settle it. A claim the text implies but does not
> state counts: a component given a job, a list that reads as complete. Where the doc
> describes a plan as well as what is built, say whether a claim about the plan fits the
> code that exists, rather than marking it WRONG for not being built yet. Where the change
> is to the repository's own rules, also check that it agrees with the rest of AGENTS.md,
> the other commands, the hooks, the scripts and CI. Report WRONG first. Do not report
> style.
>
> <on a re-run, add: Check only the lines that `git diff <commit>..origin/<branch>`
> changes, but report anything WRONG you find anywhere in the full diff. `<commit>` is the
> one the last fact check read.>

## When they report

Grade the reader's answers against yours. A wrong or missing answer is a finding against
the text, not against the reader: change the text until a fresh reader would answer it.
Verify each WRONG from the fact check yourself before acting on it, and settle each
UNCHECKED or name it in the PR description as open.

A revision of one word or one punctuation mark needs no new pass. A larger revision is new
text: push it and run two fresh agents again, with the same questions, since a fix for one
reader's confusion can make a claim the code does not hold. Once the latest reader has
answered every question correctly, a revision for that reader's other notes needs only the
fact check, on the lines the revision changed. These rules hold for a revision of an open
PR too.

Then say in the PR description that the pass ran, the questions, how the reader did, what
the fact check found, and what is still open, so a pass that found nothing leaves a trace.
