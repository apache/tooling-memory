# The harness image

One image with seven of the eight harnesses `amem launch` knows, Node, and the `memory` binary,
built from the Dockerfile here. `make image` copies the release binary in and builds with
`wslc build` (Windows, WSL containers) or `podman build` where that is what is present. The
tag is `durable-memory/harness:dev`; `MEMORY_IMAGE` or `--image` overrides it at launch.

The image is the homogeneous environment: the same harness versions and tools on every
machine, independent of what a developer's WSL or laptop contains. The seven npm harnesses
are pinned in `package.json`, and every package under them, transitive ones included, in
`package-lock.json`, by version and sha512; `npm ci` installs exactly that into
`/opt/harness`, and their bins are linked into `/usr/local/bin`. Node and pi are the `ARG`s
at the top of the Dockerfile. Muse Code follows its release channel.

To bump every pin to its latest release, as the daily check does, run the bump script. It
needs npm, so without Node on the host run it in a container, from the repository's root:

```sh
podman run --rm -v "$PWD":/r:Z -w /r docker.io/library/node:24 sh scripts/bump-harness-versions.sh
```

To set one npm harness to a given version, change `package.json` and the lock together:

```sh
cd images/harness
podman run --rm -v "$PWD":/w:Z -w /w docker.io/library/node:24 \
  npm install --package-lock-only --save-exact --no-fund --no-audit '@openai/codex@0.164.0'
```

A pull request that touches this directory has its lock checked against `package.json`.

`package.json` also carries `overrides`, which hold a package under one harness at a version
other than the one the harness pins, for a fix the harness has not taken yet. Each entry is
scoped to its harness. Today there is one: `@qwen-code/qwen-code` pins `sharp` at exactly
0.35.4, which has CVE-2026-96889 in its bundled librsvg (GHSA-wq5f-xc86-pv6w), and the entry
holds it at 0.35.5. Remove the entry once qwen-code pins 0.35.5 or later itself; while it is
there, a qwen-code that pins a newer `sharp` is held back to 0.35.5. npm says nothing when an
override applies, so the bump script does: after a bump it prints a line for each override
that its harness no longer needs, which lands in the daily pull request. A global install
(`npm install -g`) ignores `overrides`, which is one reason the image installs from the lock.

## When it is rebuilt

Never on a run. `amem launch` uses the image it is given and fails fast if it is missing.
The image's inputs are the pins in the lockfile and the Dockerfile, and the base image, so it
is rebuilt when those move, and on a clock for the one that moves silently:

| When | What | Why |
|---|---|---|
| A pin changes | `.github/workflows/image.yml` builds with the release binary to show the image still builds; nothing is published, so each machine builds its own with `make image` | A new harness version is a deliberate, reviewed change |
| Daily, 05:41 UTC | `scripts/bump-harness-versions.sh` checks npm and nodejs.org for newer versions of the seven npm harnesses, pi and the Node line, regenerates the lock when an npm harness moved, and opens or updates a pull request bumping the pins | Security releases of a harness land within a day; a person still merges |
| A machine wants the base image's and apt's updates | `make image` again, after pulling `ubuntu:24.04`; Muse Code moves here too, since its launcher takes the version from Meta's release channel at build time | `ubuntu:24.04` moves under its tag, and nothing is published for a machine to pull instead |
| Urgent | Bump the pin by hand and merge | The daily check is the floor, not the ceiling |

Locally, `make image` is for developing the launcher itself. The pull request the bot opens
runs CI like any other once a maintainer approves the workflow run, since it is opened with
the workflow's own token.

## Never published

Nobody publishes this image, and nothing in this repository can push it to a registry.
Each machine builds its own with `make image`. There are two reasons:

- The image holds harnesses under their vendors' own licences. Claude Code and Copilot
  CLI are two of them, and the project may not redistribute them. The Apache release
  policy says that each artifact that the project distributes contains only code with a
  suitable licence.
- The image is built from unreleased source. The Apache release policy says that the
  project does not publish unreleased material outside its development community.

Three parts hold this:

- `scripts/no-image-push.sh`, run by `make check`, fails when a script, a workflow, the
  `Makefile` or a Dockerfile would push an image or log in to a registry.
- `.claude/hooks/guard-bash.sh` refuses such a command when a session types it.
- `.github/workflows/image.yml` gives its token no `packages` permission.
