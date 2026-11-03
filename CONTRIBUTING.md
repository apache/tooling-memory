# Contributing

## Prerequisites

- Rust stable, from [rustup](https://rustup.rs), with two more targets:
  `rustup target add wasm32-unknown-unknown wasm32-wasip2`. The build script of `dm-web`
  builds the browser reader for the first target. Thus each build of `amem` needs it. The
  second target builds the taps. `make check` lints the taps, and the tests of the tap
  host build them. Thus the gate needs the second target.
- The build script of `dm-web` runs a second cargo for the reader. That cargo reads the
  cargo config files and the environment. It does not get the command line of the first
  cargo: `--config`, `--offline` and `--frozen` do not reach it. Put such a setting, for
  example a vendored source replacement, in a cargo config file. For a build without the
  network, set `CARGO_NET_OFFLINE=true`, or `net.offline = true` in a config file.
- A C toolchain for the bundled SQLite: `build-essential` on Debian and Ubuntu, or the
  Visual Studio Build Tools on Windows.
- `make`. On Windows, run the targets from Git Bash or WSL.
- `python3`, for `scripts/conformance.py` and its test. `make check` and the tests of the
  binary run them. The script uses only the standard library.
- For the tests of the browser reader: Chrome or Chromium, or Podman or Docker. If no
  Chrome is installed, the tests use the browser in the Playwright image, through
  `scripts/chrome-container.sh`. This script pulls the image on its first use. The test
  has two minutes. To keep the pull out of these two minutes, run
  `podman pull mcr.microsoft.com/playwright:v1.61.1-noble` before the test. To select a
  browser directly, set `CHROME=<path>`.

## Setup

```sh
git clone git@github.com:apache/tooling-memory.git
cd tooling-memory
make init        # installs the pre-commit hook
make check       # format, clippy, tests, guard, docs-only and conformance tests
```

## Day to day

| Command | What it runs |
| ------- | ------------ |
| `make check` | `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, the guard, docs-only and conformance tests. It is the pre-commit gate. A commit that has only Markdown, outside `crates/` and `taps/`, runs the guard tests instead |
| `make conformance` | `scripts/conformance.py`: the seven M0 steps over MCP, against a new record that this build serves. `URL=<mcp endpoint>` points it at a different server. `OPERATOR=<command>` gives it the export, import and reindex of that server, for the last two steps |
| `make test` | only `cargo test` |
| `make fmt` | `cargo fmt`, which changes the files in place |
| `make run ARGS='...'` | the `amem` binary, for example `ARGS='init --root /tmp/m --domain dom-me'` |
| `make verify` | `check`, and also `cargo deny`, `cargo machete`, coverage against `COVERAGE_MIN`, and the browser builds of `dm-core`, `dm-policy` and `dm-reader`. CI runs this target |
| `make msrv-check` | `cargo check` of the workspace and of the browser builds, with the Rust release that `rust-version` in `Cargo.toml` names. It needs that toolchain: `rustup toolchain install <rust-version> --profile minimal --target wasm32-unknown-unknown`. It is not part of `check` or `verify`. CI runs it as a separate job |
| `make e2e` | the browser tests in `e2e/`. Chromium in the Playwright image drives the pages. The tests put screenshots in `e2e/screenshots/`. It needs Podman or Docker, not Node. It is not part of `check` or `verify`. Run it for a change to the pages. See [e2e/README.md](e2e/README.md) |
| `make coverage` | `cargo llvm-cov` over the workspace. It writes an lcov report to `target/lcov.info` |

`verify` needs three cargo extensions. Install them one time:

```sh
cargo install cargo-binstall
cargo binstall cargo-llvm-cov cargo-deny cargo-machete
```

`rust-toolchain.toml` pins the channel. It also adds rustfmt, clippy, the llvm tools and
the wasm targets. Thus `rustup` sets up the rest on its first use.

Use one worktree for each topic. Then several people and sessions can work at the same
time:

```sh
git worktree add ../durable-memory-<topic> -b feat/<topic> origin/main
```

## Pull requests

Make your branch from `main`. `make check` must pass before each commit. The hook
enforces this. Write the commit subject with a conventional-commit prefix. Open the PR
against `main`. In its description, say what changed and why. For a new test, also say
what the test reported against the code before the change.

[AGENTS.md](AGENTS.md) is the contract for AI agents that work in this repository. Most
of it is also good advice for people.
