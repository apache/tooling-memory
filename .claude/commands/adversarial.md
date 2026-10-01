---
description: Hand a pushed branch to a fresh subagent and ask it to break the change
argument-hint: "[branch] (defaults to the current one)"
---

# Attack the branch before the pull request

AGENTS.md asks for this after the push and before the PR is opened, and for a revision of
an open PR, before the push that asks for re-review. This is the prompt it tells you to
write.

Work out the branch (`$ARGUMENTS`, or `git rev-parse --abbrev-ref HEAD`), confirm it is
pushed and that `origin/<branch>` is what you mean to have reviewed, then spawn **one fresh
general-purpose subagent**, never a fork, which would inherit the reasoning that wrote the
branch and agree with it. Give it the prompt below with the branch filled in. Do not edit
the worktree until the pass reports.

## The prompt

> You are doing an adversarial pre-PR review in the sstriker/durable-memory repository.
> Read AGENTS.md and PLAN.md first. **Change nothing, commit nothing, post nothing to
> GitHub.** Report findings.
>
> The change is `git diff origin/main...origin/<branch>`: what was pushed, not `HEAD`.
>
> Attack these classes, in this order. They are a floor, not the list. Each is a guarantee
> the design depends on, written in docs/durable-memory-for-disposable-agents.md, and a
> branch that breaks one breaks the record for everyone who reads it later.
>
> 1. **The record's guarantees.** Every write is a new version and nothing is edited in
>    place. Demotion leaves a tombstone. Deleting the index and running reindex rebuilds an
>    identical index from the record alone. Find a path through the new code that edits a
>    file in place, loses a version, or leaves the index knowing something the record does
>    not.
> 2. **The filter and the labels.** Nothing above a session's handling appears in a search
>    result, a hit count, a summary, or T0. Nothing leaves an export below its label. A
>    bundle's `highest_label` is what is inside it. Find the read path that forgets the
>    filter, or the field that leaks through a summary.
> 3. **Leases and claims.** No task ever has two holders. An expired lease returns the task
>    to the pool with its handoff. A second `claim` while a lease holds is refused. Try two
>    claimants at once, a heartbeat after expiry, a crash between claim and complete.
> 4. **The tests themselves.** Which new assertion would still pass if the behaviour it
>    names were wrong? Check each one by making the behaviour wrong, not by reading it.
> 5. **Untrusted input.** Frontmatter is read from disk and from bundles other people wrote.
>    A manifest names paths. Try a path that escapes the bundle directory, an id that is
>    not an id, a digest that does not match, a label that is not one of the three, a
>    `created` in the future, a body of a hundred megabytes.
> 6. **A guard, a script, or a check: what walks past it, and what does it wrongly stop?**
>    Both halves. Find a spelling of the thing it refuses that it lets through. Then find
>    something the repository's own instructions ask for that it now refuses, since that is
>    what gets it worked around and switched off. And ask what it does when it cannot
>    answer: a check that says "nothing found" when it failed to look is the worst of the
>    three.
>
> Verify a suspicion before reporting it. You may write throwaway tests inside the worktree
> and delete them; `git status` must be clean when you finish, and say so if it was not
> clean when you started. Never pipe `cargo` or `make` into `head` or `tail`; redirect to a
> file and read the file.
>
> Report findings only, most serious first. For each: the `file:line`, what is wrong, a
> concrete scenario (inputs to wrong behaviour), and the smallest change that fixes it.
> Separate **CONFIRMED** (you ran something that demonstrates it) from **PLAUSIBLE**
> (reasoning only). Say explicitly if a class has nothing in it. Do not pad the list, and
> do not state a claim you have not checked.

## When it reports

Verify each finding yourself before acting. Fix the real ones with the failing assertion
shown first. A finding you decline is declined in the commit that declines it. Then say in
the PR description that the pass ran and what came of it.

## Growing the list

The classes above are what the design says must hold. When a pass finds something that
fits none of them, add the class here rather than remembering it.
