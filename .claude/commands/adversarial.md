---
description: Give a pushed branch to a fresh subagent and ask it to break the change
argument-hint: "[branch] (defaults to the current one)"
---

# Attack the branch before the pull request

AGENTS.md asks for this pass after the push and before you open the PR. It also asks for
the pass on a revision of an open PR, after its push and before you request review again.
This file gives the prompt that AGENTS.md tells you to write. Do these steps:

1. Find the branch: `$ARGUMENTS`, or `git rev-parse --abbrev-ref HEAD`.
2. Make sure that the branch is pushed, and that `origin/<branch>` is the code that you
   want the pass to review.
3. Start **one fresh general-purpose subagent**. Never use a fork. A fork inherits the
   reasoning that wrote the branch, and it agrees with that reasoning.
4. Give the subagent the prompt below, with the branch filled in.

Do not edit the worktree until the pass reports.

## The prompt

> You do an adversarial pre-PR review in this repository.
> Read AGENTS.md and PLAN.md first. **Do not change anything. Do not commit anything. Do
> not post anything to GitHub.** Report your findings.
>
> The change is `git diff origin/main...origin/<branch>`. This is the pushed code, not
> `HEAD`.
>
> Attack these classes, in this order. They are the minimum, not the full list. Each class
> is a guarantee that the design needs. docs/durable-memory-for-disposable-agents.md
> states each guarantee. A branch that breaks one guarantee breaks the record for everyone
> who reads the record later.
>
> 1. **The record's guarantees.** Every write is a new version, and no code edits anything
>    in place. Demotion leaves a tombstone. If you delete the index and run reindex,
>    reindex builds an identical index from the record alone. Find a path through the new
>    code that does one of these:
>
>    - It edits a file in place.
>    - It loses a version.
>    - It leaves data in the index that the record does not have.
>
> 2. **The filter and the labels.** Nothing above the handling of a session appears in
>    these:
>
>    - a search result;
>    - a hit count;
>    - a summary;
>    - T0.
>
>    Nothing leaves an export below its label. The `highest_label` of a bundle is the
>    highest label inside the bundle. Find the read path that does not apply the filter,
>    or the field that leaks through a summary.
> 3. **Leases and claims.** A task never has two holders. When a lease expires, the task
>    goes back to the pool with its handoff. While a lease holds, a second `claim` gets a
>    refusal. Try two claimants at the same time, a heartbeat after expiry, and a crash
>    between claim and complete.
> 4. **The tests themselves.** Find each new assertion that still passes when the
>    behaviour that it names is wrong. To check an assertion, make the behaviour wrong. Do
>    not check it only by reading it.
> 5. **Untrusted input.** The code reads frontmatter from disk and from bundles that other
>    people wrote. A manifest names paths. Try these:
>
>    - a path that escapes the bundle directory;
>    - an id that is not an id;
>    - a digest that does not match;
>    - a label that is not one of the three;
>    - a `created` in the future;
>    - a body of a hundred megabytes.
>
> 6. **A guard, a script, or a check: what gets past it, and what does it stop in
>    error?** Examine both halves. Find a spelling of a refused thing that it lets
>    through. Then find something that the repository's own instructions ask for, and
>    that it now refuses. Such a refusal makes people work around it and turn it off. Also
>    ask what it does when it cannot answer. A check that says "nothing found" when it
>    failed to look is the worst of the three.
>
> Verify a suspicion before you report it. You may write temporary tests in the worktree,
> and then delete them. When you finish, `git status` must be clean. If it was not clean
> when you started, say so. Never pipe `cargo` or `make` into `head` or `tail`. Redirect
> the output to a file, and read the file.
>
> Report only findings, the most serious first. For each finding, give:
>
> - the `file:line`;
> - what is wrong;
> - a concrete scenario (inputs to wrong behaviour);
> - the smallest change that fixes it.
>
> Keep **CONFIRMED** findings (you ran something that shows the finding) apart from
> **PLAUSIBLE** findings (reasoning only). If a class has nothing in it, say so
> explicitly. Do not add items only to make the list longer. Do not state a claim that
> you did not check.

## When it reports

Verify each finding yourself before you act on it. Fix the real findings, and first show
the failing assertion for each fix. When you decline a finding, say so in the commit that
declines it. Then, in the PR description, say that the pass ran and what it found.

## Growing the list

The classes above are what the design says must hold. A pass can find something that fits
none of them. Then add its class here. Do not only remember it.
