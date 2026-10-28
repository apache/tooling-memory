# AGENTS.md

This file gives the operating rules for AI coding agents in this repository. Claude Code
loads this file through `CLAUDE.md`. Other agents read it directly.

This file is the short contract, and you must follow it. Other files tell you more:

- [PLAN.md](PLAN.md) says what to build, and in which order.
- [CONTRIBUTING.md](CONTRIBUTING.md) says how to set up and run things.
- [docs/](docs/) holds the design that this code implements.

An instruction in this file can conflict with an explicit request from the user. Then ask
before you override the instruction.

## Non-negotiables

- **Ask before you build an ambiguous feature.** A request can have more than one reading:
  which crate it touches, what the on-disk shape is, what "done" looks like. Then ask
  which reading is correct before you write code. A wrong guess costs an implementation
  and its tests. When the reading is settled, build all of the feature. Do not stop
  again to ask.
- **The entry format is the contract.** `crates/core` defines the frontmatter, the bundle
  manifest, the order of the labels and the order of the trust tiers. A change in
  `crates/core` changes every other crate, the on-disk record, and the meaning of an
  export. Read [crates/core/AGENTS.md](crates/core/AGENTS.md) before you change
  `crates/core`.
- **`make check` must pass before you commit.** It runs:

  - `cargo fmt --check`;
  - `cargo clippy`, with warnings as errors;
  - `cargo test`;
  - the tests of the guard;
  - other checks that the `Makefile` names.

  Fix every error and every warning. Do not commit with `--no-verify`. `make init`
  installs `make check` as the pre-commit hook. Some changes touch only Markdown files
  outside `crates/` and `taps/`. For this rule, an `AGENTS.md` in `crates/` or `taps/`
  counts as Markdown. The hook and CI do not run `make check` for such a change, because
  nothing builds from those files or reads them. `scripts/docs-only.sh` decides which
  changes are of this type. Its test holds the premise that nothing builds from those
  files or reads them.
- **Show that a new assertion fails before you trust it.** Run it against the code as it
  was before your change. Put what it reported in the pull request. If a test only ever
  passed, nobody has shown that it tests anything. Assert the rule, not a bound on the
  rule.
- **Stage only the files that you changed** (`git add <path>`). Never use `git add -A`,
  `git add .` or `git commit -a`.
- **Work in your own worktree, and expect other work in the repository.** Several sessions
  and several people work in this repository at the same time, one for each topic:

  ```sh
  git worktree add ../durable-memory-<topic> -b feat/<topic> origin/main
  ```

  Never switch the branch of another worktree. Never use a bare `git stash`, because all
  the worktrees share one stash stack. To set work aside, make a temporary commit.
- **Nothing engine-specific goes outside the backend that needs it.** The record is
  versioned Markdown. The paper names no storage engine. This code also names no storage
  engine outside the backends in `crates/store`, `crates/index` and `crates/ledger`. If a
  doc or a core type says "SQLite" or "S3", that is a bug.
- **The command line and the memory server call the same library functions.** Nothing is
  only in the binary.
- **Before you change the web pages, read the design book and load the `frontend-design`
  skill.** Do both, every time. [docs/design-book.html](docs/design-book.html) says what
  each colour means and how to compose a page. `crates/web/static/tokens.css` holds the
  same values. If you add one of these, change the book in the same commit as the code:

  - a token;
  - a hue;
  - a chip;
  - a glyph;
  - a type role.

  In the Decisions chapter of the book, say why.
- **Only two crates may use `unsafe`.** `crates/sandbox` may use it, because the
  namespaces and the mounts must occur between fork and exec. `crates/wslc-api` may use
  it, because it binds a C API by hand. Every `unsafe` block has a SAFETY comment. In all
  other crates, the workspace forbids `unsafe`. Do not relax that rule. If a crate needs
  foreign calls, give it its own binding crate. Do not make a third exception.
  `make check` holds both halves of this rule: `scripts/unsafe-crates.sh`, and clippy for
  the Windows target of the binding.
- **No image leaves this machine.** Nothing in this repository pushes an image to a
  registry, logs in to a registry or makes a GitHub release. There are two reasons. The
  harness image holds harnesses that the project may not redistribute. The image is also
  built from unreleased source. `make check` runs `scripts/no-image-push.sh`, which
  applies this rule to the files of the repository. The guard applies it to the commands
  that a session types.
  images/harness/README.md gives the reasons.
- **Nothing has access outside its root.** The wrapper, the per-session memory server and
  its egress, is a confined process. It has access only to the record, its own state and
  its grants file, each through a rooted operator. It has access to nothing else on the
  host. Code validates a name before the name becomes a path. Code is a bug if it does one
  of these things:

  - It opens a file by an absolute path.
  - It reads an environment variable for a location.
  - It follows a path that it was given.

  The exception is the binary when it resolves its own mounts at startup.

## Some of these are held for you

The repository enforces the rules above that read as absolutes. It does not rely on
memory for them. Thus, to break one of these rules takes a decision, not a lapse. The part
of the Claude Code harness that belongs to the repository is committed under `.claude/`:

| Path | What it is |
| ---- | ---------- |
| `.claude/settings.json` | the permission allowlist (cargo, make, and read-only `git` and `gh`) and the hook registration |
| `.claude/hooks/guard-bash.sh` | It refuses a command that does one of these: stages with a wildcard; runs `git stash` bare or with `save`, `pop`, `clear` or `drop`, or a `git stash push` without a message; passes `--no-verify`; pipes `cargo` or `make` into `head` or `tail`; pushes an image, logs in to a registry or makes a GitHub release; posts an issue or pull request body that does not say who wrote it |
| `.claude/hooks/guard-bash.test.sh` | what the guard refuses, and what it must continue to allow. `make check` runs it |
| `.claude/commands/adversarial.md` | the prompt for the pre-PR pass, and the classes that the pass attacks |
| `.claude/commands/reader-test.md` | the prompts for a docs-only branch, instead of that pass: a reader that has only the changed files and their diff, and a fact check against the code |

If the guard refuses something that this repository asks for, the guard has a bug. Add
the case to the test and fix the guard. Do not work around the bug. All other files in
`.claude/` stay ignored. An agent that is not Claude Code gets none of this. The same
rules apply to that agent, and it learns them when it reads them.

## Project shape

| Crate | Holds | Compiles to |
| ----- | ----- | ----------- |
| `crates/core` | entries, frontmatter, bundles, digests, labels. No I/O | native, wasm, WASI |
| `crates/store` | the record over OpenDAL: versions, tombstones | native |
| `crates/index` | search index, summaries first; backends behind one trait | native |
| `crates/ledger` | tasks, leases, origin dedup; backends behind one trait | native |
| `crates/policy` | the grants file, labels, ceilings | native, wasm |
| `crates/server` | the service that the command line and the MCP handler call: T0, bundles, and the memory server over MCP | native |
| `crates/egress` | the egress, the one way out: it forwards with ceilings and usage, and carries MCP over HTTP | native |
| `crates/sandbox` | the sandbox: the plan, a namespaces sandbox backend for Linux, and Seatbelt for macOS (dry-coded) | native |
| `crates/memory` | the binary, `amem`: serve, launch, relay, doctor, and the command line | native |

The code uses Rust stable, in one workspace, with `cargo` on the host. Put a rule that
applies everywhere in this file. Put a rule that applies to one crate in an `AGENTS.md` in
that crate. There, whoever is about to break the rule reads it.

## Quality gates, and where each lives

This repository follows the conventions of Rust as they are. The thresholds are in files,
not in memory.

| Concern | Convention here | Where |
| ------- | --------------- | ----- |
| Tests | Unit tests in a `#[cfg(test)] mod tests` beside the code; integration tests in each crate's `tests/`; doc tests on public examples | `cargo test --workspace` |
| Formatting | rustfmt, Unix line endings on every platform | `rustfmt.toml` |
| Lints and complexity | clippy with warnings as errors; `cognitive_complexity` and `too_many_lines` enabled, thresholds in `clippy.toml`; `unwrap_used` outside tests; `missing_docs` on public items | `Cargo.toml` `[workspace.lints]`, `clippy.toml` |
| Coverage | `cargo llvm-cov`, line coverage over the workspace, `COVERAGE_MIN` in the Makefile | `make coverage` |
| Dependencies | `cargo deny` for advisories, licences, duplicates, sources; `cargo machete` for unused | `deny.toml`, `make deny`, `make machete` |
| Portability | the tests of the workspace run on Linux and Windows; `dm-core` builds for `wasm32-unknown-unknown` | CI, `make wasm-check` |

Two of these values can move in one direction only. **You may raise `COVERAGE_MIN`, and
you may not lower it.** Raise it in the commit whose tests get the coverage to the new
value. **You may not raise the thresholds in `clippy.toml`** to make a reading pass. Split
the function. An `#[allow]` on a lint is a decision. Say why in a comment on the same
line.

`make check` is the pre-commit gate, and it is fast. CI runs `make verify`. It adds
coverage, dependencies and the wasm build. Run `verify` before you push.

Builds are faster when you install `sccache` and `cargo-nextest`
(`cargo binstall sccache cargo-nextest`). The Makefile uses them when they are present,
and it needs nothing from you. The dev profile keeps only line tables. Thus you can still
read backtraces, and the target directory stays small. If the target directory grows,
`cargo sweep --time 7` removes stale artifacts. The plan for CI is remote execution
(PLAN.md, Build speed). Until then, the cache is local.

## Pull requests

- Branch from `main`. Make sure that `make check` passes. Then open the PR against `main`.
- **A fresh subagent attacks the branch after the push and before the pull request.** It
  attacks again after each push that revises an open pull request, before you request
  review again. The prompt is `/adversarial`, in `.claude/commands/adversarial.md`. Use a
  fresh agent, never a fork. A fork inherits the reasoning that wrote the branch, and it
  agrees with that reasoning. Answer what the subagent finds as you answer a reviewer. In
  the PR description, say that the pass ran and what it found. Thus the PR also shows a
  pass that found nothing.
- **A docs-only branch gets a reader test instead**, at the same points. A docs-only
  branch is a branch whose changed paths `scripts/docs-only.sh` accepts. The prompt is
  `/reader-test`, in `.claude/commands/reader-test.md`. First, write the questions that
  the change should answer. Then one fresh agent answers them. It uses only the changed
  files, their diff, and what every session loads. Another agent checks each claim against
  the code. A wrong or missing answer is a finding against the text. The command says when
  a revision needs both agents again, only the fact check, or neither. In the PR
  description, give:

  - that the pass ran;
  - the questions;
  - how the reader did;
  - what the fact check found;
  - what is still open.
- **A review applies to one commit, so request it again after every push.** Do not read a
  clean round as a verdict.

## Writing in ASD-STE100

Write in [ASD-STE100](https://www.asd-ste100.org/) (Simplified Technical English):

- Keep each sentence short. Use not more than 20 words in an instruction and not more
  than 25 words in a description.
- Write about one topic in each sentence.
- Use the active voice and the simple tenses.
- Use one word for one thing. Do not use a different word for variety. The words are in
  [docs/terms.md](docs/terms.md); add a term there in the commit that first needs it.
- Do not use idioms or figurative language.

This applies to:

- pull request descriptions, comments on issues and pull requests, and commit messages;
- `README.md`, `CONTRIBUTING.md`, `local/README.md`, the READMEs beside the code, and the
  Markdown documents in `docs/`;
- the AGENTS files, and the commands in `.claude/commands/`.

These keep their own style: the paper (`docs/durable-memory-for-disposable-agents.md`),
the case study (`docs/seeding-a-foundation.md`), `PLAN.md`, the records in `spikes/`, the
example records in `examples/`, code comments, and the text of the web pages and the
design book.

## Writing on an issue or a pull request

A session posts a comment under the account of a person, and colleagues read it.

- **Answer only what the request asks, and then stop.**
- **Write in ASD-STE100**, as the section above says.
- **The account holder approves the text first.** You cannot unsend a comment.
- **Say who wrote it**, on a line of its own at the end. The guard refuses a `gh` command
  that posts a body without such a line:

  ```text
  Posted by Claude on behalf of @sstriker
  ```

## Reading your own results

- **A pipe does not keep the exit status.** `cargo test | tail -20` reports the exit
  status of `tail`. To get the output and the exit status, send the output to a file:
  `cargo test > /tmp/out.log 2>&1; echo "exit=$?"`.
- **When the guard refuses a command, no part of the command runs.** The guard stops the
  whole call.
- **Do not state a number, or a claim about what the code does, that you did not just
  produce.** A measurement comes from a command that you ran in this session. A statement
  about what code does comes from code that you just read or ran. When it costs little,
  name the evidence: the command, or the `file:line`.

## Conventions

- Start each commit subject with a conventional-commit prefix: `feat:`, `fix:`, `chore:`,
  `docs:`, `test:`, `refactor:`, or `ci:`. When an agent writes a commit, end the commit
  with a `Co-Authored-By:` trailer that names the agent.
- Do not use emojis in commits, pull requests, issues or code. Keep the text short and
  direct.
- **A comment says what the code does, not how the code got that way.** Put history in a
  comment only when it names a shape that the code avoids, and an edit could bring that
  shape back.
- Do not remove or downgrade code to stop a warning from an outdated dependency. Upgrade
  the dependency.
- Ask before you remove functionality or code that seems intentional.
