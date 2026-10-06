# The browser tests

The web pages driven by a real browser: the launch form and the command line it previews,
a session's terminal streaming and taking input, the reading pane opening what a session
writes, the reader loading a domain, the key in front of the server and how a browser
pairs, and every page at 360, 768 and 1280 pixels in light and dark with an accessibility
scan. After the Board Agenda Tool's suite (apache/tooling-agenda, `e2e/`).

```sh
make e2e                                              # the whole suite
make e2e FILTER="tests/launch.spec.ts -g preview"     # one spec, or one test by name
```

It runs in `mcr.microsoft.com/playwright:v1.61.1-noble`, which carries Node, Chromium and
the libraries Chromium needs, so the host needs neither; `scripts/e2e.sh` builds `amem`
on the host first and mounts the checkout. The image's Playwright and the one in
`package.json` move together.

## What the server under test is

`serve.sh` makes a fresh record under `e2e/.state` with three entries, then runs
`amem web` over it with `--session-command`, a hidden flag that runs `session.sh` in each
session's terminal in place of the launcher: a banner naming an entry, then whatever is
typed, echoed. So the pages, the API, the key and the websocket are the real ones, and no
harness, sandbox or credential is needed; the launcher's own confinement is tested in
Rust (`crates/memory/tests/launch.rs`, `crates/sandbox`).

## Writing a spec

- Import `test` and `expect` from `tests/support/fixtures.ts`, not from
  `@playwright/test`: the page fixture fails a test on any uncaught error in the page,
  and `signedIn` is a page paired the way `amem` pairs one.
- Select by role or label where the page gives one an accessible name; an id where it
  does not.
- A change to the pages says what it looks like: the screenshots land in
  `e2e/screenshots/`, ignored by git, and CI attaches them to the run.
- A known failure is `test.fail(...)` with the reason, so the change that fixes it is
  told to remove it.
