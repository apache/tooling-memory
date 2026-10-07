# AGENTS.md

Operating rules for AI coding agents working in this repository. Claude Code loads this
file via `CLAUDE.md`; other agents read it directly.

This is the short, must-follow contract. [PLAN.md](PLAN.md) says what is being built and
in what order; [CONTRIBUTING.md](CONTRIBUTING.md) says how to set up and run things;
[docs/](docs/) holds the design this code implements. If an instruction here conflicts
with an explicit request from the user, ask before overriding it.

## Non-negotiables

- **Ask before building an ambiguous feature.** When a request admits more than one
  reading, which crate it touches, what the on-disk shape is, what "done" looks like, ask
  which one is meant before writing code. A wrong guess costs an implementation and its
  tests. Once the reading is settled, build the whole thing without further check-ins.
- **The entry format is the contract.** `crates/core` defines the frontmatter, the bundle
  manifest, and the label and trust orders. A change there is a change to every other
  crate, to the on-disk record, and to what an export means. Read
  [crates/core/AGENTS.md](crates/core/AGENTS.md) before touching it.
- **`make check` must pass before you commit.** It runs `cargo fmt --check`, `cargo clippy`
  with warnings as errors, `cargo test`, and the guard's own tests. Fix every error and
  warning. Do not commit with `--no-verify`. `make init` installs it as the pre-commit hook.
  A change that touches only Markdown outside `crates/` and `taps/` (an `AGENTS.md` there
  counts as Markdown) skips it, in the hook and in CI, because nothing builds from or reads
  those files; `scripts/docs-only.sh` decides, and its test holds the premise.
- **Show a new assertion failing before you trust it.** Run it against the code as it was
  before your change, and put what it reported in the pull request. A test that has only
  ever passed has not been shown to test anything. Assert the rule, not a bound on it.
- **Stage only the files you changed** (`git add <path>`), never `git add -A`, `git add .`,
  or `git commit -a`.
- **Work in a worktree of your own, and expect company.** Several sessions and several
  people run in this repository at once, one per topic:

  ```sh
  git worktree add ../durable-memory-<topic> -b feat/<topic> origin/main
  ```

  Never switch another worktree's branch, and never bare `git stash`: the stash stack is
  shared with every worktree. Set work aside with a temporary commit instead.
- **Nothing engine-specific outside the backend that needs it.** The record is versioned
  Markdown; the paper names no storage engine and neither does this code outside
  `crates/store`, `crates/index`, and `crates/ledger` backends. A doc or a core type that
  says "SQLite" or "S3" is a bug.
- **The CLI and the MCP server call the same library functions.** Nothing lives only in
  the binary.
- **Before any change to the web pages, read the design book and load the
  `frontend-design` skill.** Both, every time. [docs/design-book.html](docs/design-book.html)
  says what each colour means and how a page is composed; `crates/web/static/tokens.css`
  holds the same values. A new token, hue, chip, glyph or type role changes the book in
  the same commit as the code, and its Decisions chapter says why.
- **`unsafe` lives in two crates.** `crates/sandbox` may use it, because namespaces and
  mounts have to happen between fork and exec, and `crates/wslc-api` may, because it binds
  a C API by hand. Every block has a SAFETY comment. Anywhere else the workspace forbids
  it; do not relax that, and give a crate that needs foreign calls its own binding crate
  rather than a third exception. `make check` holds both halves (`scripts/unsafe-crates.sh`,
  and clippy for the binding's Windows target).
- **Nothing reaches outside its root.** The wrapper is a confined process: it touches the
  record, its own state, and its grants file, each behind a rooted operator, and nothing
  else on the host. A name is validated before it becomes a path. Code that opens a file
  by an absolute path, reads an environment variable for a location, or follows a path
  it was handed is a bug unless it is the binary resolving its own mounts at startup.

## Some of these are held for you

The rules above that read as absolutes are enforced rather than remembered, so breaking
one takes a decision rather than a lapse. The part of the Claude Code harness that belongs
to the repository is committed under `.claude/`:

| Path | What it is |
| ---- | ---------- |
| `.claude/settings.json` | the permission allowlist (cargo, make, and read-only `git` and `gh`) and the hook registration |
| `.claude/hooks/guard-bash.sh` | refuses a command that stages with a wildcard, reaches into the shared stash, passes `--no-verify`, pipes `cargo` or `make` into `head` or `tail`, or posts an issue or pull request body that does not say who wrote it |
| `.claude/hooks/guard-bash.test.sh` | what the guard refuses, and what it must go on allowing. Run by `make check` |
| `.claude/commands/adversarial.md` | the prompt the pre-PR pass is given, and the classes it attacks |

If the guard refuses something this repository actually asks for, that is a bug in the
guard: add the case to the test and fix it rather than working around it. Whatever else
lands in `.claude/` stays ignored. An agent that is not Claude Code gets none of this and
is held to the same rules by reading them.

## Project shape

| Crate | Holds | Compiles to |
| ----- | ----- | ----------- |
| `crates/core` | entries, frontmatter, bundles, digests, labels. No I/O | native, wasm, WASI |
| `crates/store` | the record over OpenDAL: versions, tombstones | native |
| `crates/index` | search index, summaries first; backends behind one trait | native |
| `crates/ledger` | tasks, leases, origin dedup; backends behind one trait | native |
| `crates/policy` | the grants file, labels, ceilings | native, wasm |
| `crates/server` | the memory surface: the service, T0, bundles, MCP | native |
| `crates/egress` | the one way out: forwarding proxy with ceilings and usage, MCP over HTTP | native |
| `crates/sandbox` | confinement: the plan, and a namespaces backend for Linux | native |
| `crates/memory` | the binary, `amem`: serve, launch, relay, doctor, and the command line | native |

Rust stable, one workspace, `cargo` on the host. A rule that applies everywhere belongs
here; one that applies to a crate belongs in an `AGENTS.md` in that crate, where it is read
by whoever is about to break it.

## Quality gates, and where each lives

Rust's conventions are followed as they stand; the thresholds are files, not memory.

| Concern | Convention here | Where |
| ------- | --------------- | ----- |
| Tests | Unit tests in a `#[cfg(test)] mod tests` beside the code; integration tests in each crate's `tests/`; doc tests on public examples | `cargo test --workspace` |
| Formatting | rustfmt, Unix line endings on every platform | `rustfmt.toml` |
| Lints and complexity | clippy with warnings as errors; `cognitive_complexity` and `too_many_lines` enabled, thresholds in `clippy.toml`; `unwrap_used` outside tests; `missing_docs` on public items | `Cargo.toml` `[workspace.lints]`, `clippy.toml` |
| Coverage | `cargo llvm-cov`, line coverage over the workspace, `COVERAGE_MIN` in the Makefile | `make coverage` |
| Dependencies | `cargo deny` for advisories, licences, duplicates, sources; `cargo machete` for unused | `deny.toml`, `make deny`, `make machete` |
| Portability | the workspace tests on Linux and Windows; `dm-core` builds for `wasm32-unknown-unknown` | CI, `make wasm-check` |

Two of these are ratchets. **`COVERAGE_MIN` may be raised and may not be lowered**; raise
it in the commit whose tests earn it. **The thresholds in `clippy.toml` may not be raised**
to make a reading pass; split the function. An `#[allow]` on a lint is a decision and says
why in a comment on the same line.

`make check` is the pre-commit gate and is fast. `make verify` is what CI runs and adds
coverage, dependencies, and the wasm build. Run `verify` before a push.

Builds are faster with `sccache` and `cargo-nextest` installed (`cargo binstall sccache
cargo-nextest`); the Makefile uses them when present and needs nothing from you. The dev
profile keeps line tables only, so backtraces still read while the target directory stays
small; `cargo sweep --time 7` clears stale artifacts if it grows. Remote execution for CI
is the direction (PLAN.md, Build speed); until then the cache is local.

## Pull requests

- Branch from `main`, open the PR against `main`, `make check` passing first.
- **A fresh subagent attacks the branch after the push and before the pull request.** The
  prompt is `/adversarial`, in `.claude/commands/adversarial.md`. A fresh agent, never a
  fork: a fork inherits the reasoning that wrote the branch and agrees with it. Answer what
  it finds as you would a reviewer, and say in the PR description that the pass ran and
  what came of it, so a pass that found nothing leaves a trace. Skip it for a branch that
  touches only docs.
- **A review is tied to a commit, so re-request it after every push**, and do not read a
  clean round as a verdict.

## Writing on an issue or a pull request

A comment from a session goes up under somebody's account and is read by colleagues.

- **Answer what was asked, and stop.**
- **The account holder approves the text first.** A comment cannot be unsent.
- **Say who wrote it**, on a line of its own at the end. The guard refuses a `gh` command
  that posts a body without it:

  ```text
  Posted by Claude on behalf of @sstriker
  ```

## Reading your own results

- **An exit status does not survive a pipe.** `cargo test | tail -20` reports tail's. Where
  you need both the output and the answer, send it to a file:
  `cargo test > /tmp/out.log 2>&1; echo "exit=$?"`.
- **A refused command runs none of its parts.** The guard aborts the whole call.
- **Do not state a number, or a claim about what the code does, that you have not just
  produced.** A measurement comes from a command run in this session; a statement about
  what code does comes from having just read or run it. Name the receipt where it is cheap:
  the command, or the `file:line`.

## Conventions

- Commit subjects use a conventional-commit prefix: `feat:`, `fix:`, `chore:`, `docs:`,
  `test:`, `refactor:`, or `ci:`. Commits end with a `Co-Authored-By:` trailer naming the
  agent when one wrote them.
- No emojis in commits, pull requests, issues, or code. Keep prose concise and direct.
- **A comment says what the code does, not how it got that way.** Keep history in a comment
  only when it names a shape the code is avoiding and an edit could bring back.
- Do not remove or downgrade code to silence a warning from an outdated dependency;
  upgrade the dependency.
- Ask before removing functionality or code that appears intentional.
