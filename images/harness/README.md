# The harness image

One image with seven of the eight harnesses `memory launch` knows, Node, and the `memory` binary,
built from the Dockerfile here. `make image` copies the release binary in and builds with
`wslc build` (Windows, WSL containers) or `podman build` where that is what is present. The
tag is `durable-memory/harness:dev`; `MEMORY_IMAGE` or `--image` overrides it at launch.

The image is the homogeneous environment: the same harness versions and tools on every
machine, independent of what a developer's WSL or laptop contains. The versions are the
`ARG`s at the top of the Dockerfile and nothing else; bump them there.

## When it is rebuilt

Never on a run. `memory launch` uses the image it is given and fails fast if it is missing.
The image's inputs are the pins in the Dockerfile and the base image, so it is rebuilt when
those move, and on a clock for the one that moves silently:

| When | What | Why |
|---|---|---|
| A pin changes | `.github/workflows/image.yml` builds with the release binary and publishes to GHCR, tagged by commit and as `latest`; the digest it prints is what machines should run | A new harness version is a deliberate, reviewed change |
| Daily, 05:41 UTC | `scripts/bump-harness-versions.sh` checks npm and nodejs.org for newer versions of the seven harnesses and the Node line, and opens or updates a pull request bumping the pins | Security releases of a harness land within a day; a person still merges |
| Weekly, Monday 04:17 UTC | A rebuild with `--pull`, nothing else changed; Muse Code moves here too, since its launcher takes the version from Meta's release channel at build time and the daily pin check has no version to compare | `ubuntu:24.04` moves under its tag and apt has security updates; a weekly rebuild takes them without anyone bumping anything |
| Urgent | Bump the pin by hand and merge | The daily check is the floor, not the ceiling |

Locally, `make image` is for developing the launcher itself. The pull request the bot opens
runs CI like any other once a maintainer approves the workflow run, since it is opened with
the workflow's own token.
