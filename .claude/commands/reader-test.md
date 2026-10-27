---
description: Give a docs-only branch to two fresh subagents, one that reads only the changed files and their diff and one that checks their facts against the code
argument-hint: "[branch] (defaults to the current one)"
---

# Read the branch before the pull request

AGENTS.md asks for this pass in place of `/adversarial` on a docs-only branch. A docs-only
branch is one where
`git diff --name-only --no-renames origin/main...origin/<branch> | scripts/docs-only.sh`
exits 0. AGENTS.md asks for the pass after the push and before you open the PR. It also
asks for the pass on a revision of an open PR, after its push and before you request
review again.

A doc can fail in two ways that a reviewer who knows the code does not see:

- A reader comes away with a belief that is different from what the writer meant.
- The doc says something that the code does not do.

One subagent tests for each failure.

Find the branch (`$ARGUMENTS`, or `git rev-parse --abbrev-ref HEAD`). Make sure that it is
pushed, and that `origin/<branch>` is the text that you want the subagents to read. Then,
before you start a subagent, do these steps:

1. **Write the questions.** Write three to five questions that the change should let a
   reader answer. For each question, write the answer that you intend the doc to give. Ask
   about what a reader would act on, not about what a sentence says. For example: "which
   part stops X", "what do you lose if you Y". Do not give the answers to the reader. The
   reader never sees them. The reader does not start with no context. Before it reads
   anything, every session here loads CLAUDE.md, AGENTS.md through CLAUDE.md, and the
   user's own memory and context. Ask what that text cannot answer. When the change is to
   AGENTS.md itself, tell the reader to use the copy in its directory.
2. **Copy the changed files** as they are on `origin/<branch>` into a new directory in
   your scratchpad. Save `git diff origin/main...origin/<branch>` next to them as
   `change.diff`. Put nothing else in the directory.

Then start **two fresh subagents of the `general-purpose` type** in one message. Never use
forks. A fork inherits the reasoning that wrote the branch, and it agrees with that
reasoning. Do not edit the worktree until both subagents report.

## The reader

> You are a reader who is new to this repository. Read only the files in `<dir>`. They
> are the changed documents, and `change.diff`, which shows the changes to them. **Read
> nothing else**: no other file in the repository, no code, no web. If a reader cannot
> find something in these files, you do not know it. Do not change anything.
>
> 1. Before you answer the questions, say what the change tells you that the old text did
>    not. Use five sentences or fewer.
> 2. Answer each question below only from the documents. Quote the sentence that each
>    answer comes from. If the documents do not give an answer, say so. Do not guess from
>    what such projects usually do.
> 3. List each place where you stopped, or where you read a passage two times:
>
>    - a term that the documents use before they define it;
>    - two passages that do not agree;
>    - a sentence that you can read in two ways (give both readings);
>    - a claim about a part that the documents never introduce.
>
> <the questions, without the answers>

## The fact check

> You check a documentation change in this repository against the code. First, read
> AGENTS.md as it is on `origin/<branch>`. **Do not change anything. Do not commit
> anything. Do not post anything to GitHub.** The change is
> `git diff origin/main...origin/<branch>`.
>
> List every claim that the added or changed lines make about these:
>
> - what the code does;
> - what a command does;
> - what a file holds;
> - which part does what.
>
> For each claim, read the code on `origin/<branch>`. Then give one verdict:
>
> - **HOLDS**, with the `file:line` that shows it;
> - **WRONG**, with the `file:line` that shows otherwise, and what the text should say;
> - **UNCHECKED**, with the reason that you could not decide it.
>
> A claim that the text implies but does not state also counts. This includes a component
> that the text gives a job, and a list that reads as complete. A doc can describe a plan
> as well as what exists. For a claim about the plan, say whether it fits the code that
> exists. Do not mark it WRONG only because the code for it does not exist yet. If the
> change is to the rules of the repository, also check that the change agrees with these:
>
> - the rest of AGENTS.md;
> - the other commands;
> - the hooks;
> - the scripts;
> - CI.
>
> Report WRONG first. Do not report on style.
>
> <on a re-run, add: Check only the lines that `git diff <commit>..origin/<branch>`
> changes. But report anything WRONG that you find anywhere in the full diff. `<commit>`
> is the commit that the last fact check read.>

## When they report

Grade the answers of the reader against your answers. A wrong or missing answer is a
finding against the text, not against the reader. Change the text until a fresh reader
can answer the question. Before you act on a WRONG from the fact check, verify it
yourself. Find whether each UNCHECKED claim holds, or name it as open in the PR description.

A revision of one word or of one punctuation mark needs no new pass. A larger revision is
new text. Push it, and run two fresh agents again with the same questions. The reason is
that a fix for the confusion of one reader can make a claim that the code does not hold.
After the latest reader answers every question correctly, a revision for the other notes
of that reader needs only the fact check. That fact check reads only the lines that the
revision changed. These rules also apply to a revision of an open PR.

Then, in the PR description, give:

- that the pass ran;
- the questions;
- how the reader did;
- what the fact check found;
- what is still open.

Thus the PR also shows a pass that found nothing.
