# Contributing

## Prerequisites

- Rust stable, via [rustup](https://rustup.rs), with two more targets:
  `rustup target add wasm32-unknown-unknown wasm32-wasip2`. The first is the browser
  reader's; the second builds the taps, which `make check` lints and the tap host's tests
  build, so the gate needs it.
- A C toolchain for the bundled SQLite (`build-essential` on Debian and Ubuntu, the Visual
  Studio Build Tools on Windows).
- `make`. On Windows, run the targets from Git Bash or WSL.
- For the browser reader's tests, a Chrome or Chromium, or Podman or Docker: with no
  Chrome installed they run the one in the Playwright image through
  `scripts/chrome-container.sh`, which pulls the image on first use
  (`podman pull mcr.microsoft.com/playwright:v1.61.1-noble` ahead of time keeps that out
  of the test's two minutes). `CHROME=<path>` picks one outright.

## Setup

```sh
git clone git@github.com:sstriker/durable-memory.git
cd durable-memory
make init        # installs the pre-commit hook
make check       # format, clippy, tests, guard tests
```

## Day to day

| Command | What it runs |
| ------- | ------------ |
| `make check` | `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, the guard tests. The pre-commit gate |
| `make test` | `cargo test` alone |
| `make fmt` | `cargo fmt`, rewriting in place |
| `make run ARGS='...'` | the `memory` binary, for example `ARGS='init /tmp/m --domain dom-me'` |
| `make verify` | `check` plus `cargo deny`, `cargo machete`, coverage against `COVERAGE_MIN`, and the browser builds of `dm-core`, `dm-policy` and `dm-reader`. What CI runs |
| `make reader` | rebuilds the browser reader's glue and wasm into `crates/web/static/reader/`; needs `wasm-bindgen-cli` 0.2.129 (`cargo install wasm-bindgen-cli --version 0.2.129`). Commit the result |
| `make coverage` | `cargo llvm-cov` over the workspace; an lcov report lands in `target/lcov.info` |

`verify` needs three cargo extensions. Install them once:

```sh
cargo install cargo-binstall
cargo binstall cargo-llvm-cov cargo-deny cargo-machete
```

`rust-toolchain.toml` pins the channel and pulls in rustfmt, clippy, the llvm tools, and
the wasm targets, so `rustup` sets the rest up on first use.

One topic per worktree, so several people and sessions can work at once:

```sh
git worktree add ../durable-memory-<topic> -b feat/<topic> origin/main
```

## Pull requests

Branch from `main`. `make check` passes before every commit, which the hook enforces. Write
the commit subject with a conventional-commit prefix. Open the PR against `main` with a
description that says what changed and why, and for a new test, what it reported against
the code before the change.

[AGENTS.md](AGENTS.md) is the contract for AI agents working here, and most of it is good
advice for people too.
