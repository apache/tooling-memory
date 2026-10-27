# The browser tests

These tests drive the web pages in a real browser. They test:

- the launch form, its profiles and the command line that it previews;
- what this host has for each harness;
- the terminal of a session, which streams output and takes input, and its bar and status
  strip;
- the reading pane, which opens what a session writes;
- the board;
- the reader, which loads a domain;
- the key that controls access to the web launcher, and how a browser pairs;
- a hosted copy of the pages on a different origin;
- each page at 360, 768 and 1280 pixels, in light and dark, and in more contrast, with an
  accessibility scan.

This suite is based on the suite of the Board Agenda Tool (apache/tooling-agenda, `e2e/`).

```sh
make e2e                                              # the whole suite
make e2e FILTER="tests/launch.spec.ts -g preview"     # one spec, or one test by name
```

The suite runs in `mcr.microsoft.com/playwright:v1.61.1-noble`. This image contains Node,
Chromium and the libraries that Chromium needs. Thus the host does not need them.
`scripts/e2e.sh` first builds `amem` on the host, and mounts the checkout. The Playwright
of the image and the Playwright in `package.json` change together.

## What the server under test is

`serve.sh` makes a new record under `e2e/.state` with three entries. Then it runs
`amem web` on this record with `--session-command`. This hidden flag runs `session.sh` in
the terminal of each session, in place of the launcher. `session.sh` shows a banner that
names an entry. Then it echoes the text that it receives. Thus the pages, the API, the
key and the websocket are the real ones. The tests need no harness, no sandbox and no
credential. `session.sh` also writes the same status file that the launcher writes. Thus
the strip has data to show. `tests/support/static.mjs` serves `crates/web/static` as
plain files on a second origin, `http://localhost:4311`. With `--hosted`, the web
launcher allows this origin. Tests in Rust test the confinement of the launcher
(`crates/memory/tests/launch.rs`, `crates/sandbox`).

## Writing a spec

- Import `test` and `expect` from `tests/support/fixtures.ts`, not from
  `@playwright/test`. The page fixture fails a test if the page has an uncaught error.
  `signedIn` is a page with the same pairing that `amem` gives a page.
- Select an element by role or label where the page gives it an accessible name. Where
  the page does not, select it by id.
- A change to the pages shows how it looks. The tests put the screenshots in
  `e2e/screenshots/`. Git ignores this directory. CI attaches the screenshots to the run.
- Write a known failure as `test.fail(...)`, with the reason. Then the suite tells the
  change that fixes the failure to remove it.
