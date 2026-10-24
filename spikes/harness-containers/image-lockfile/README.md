# Spike A-1: the harness image from a lockfile

Today's `images/harness/Dockerfile` changed in two places only, as
`docs/harness-containers.md` (A8) proposes:

- `FROM ubuntu:24.04@sha256:68ee2200...` instead of the tag. The digest is the multi-arch
  index digest the registry served for `ubuntu:24.04` on 20 October 2026.
- The seven npm harnesses come from `package.json` and `package-lock.json` by `npm ci` into
  `/opt/harness`, with `/opt/harness/node_modules/.bin` on `PATH`, instead of
  `npm install -g pkg@ARG`. The lock carries a version and a sha512 `integrity` for every
  package, transitive ones included; today only the seven top-level versions are pinned.

pi and Muse Code are installed exactly as today. Nothing here is built by the workspace.

| File | What it is |
|---|---|
| `Dockerfile` | the image, as above |
| `package.json` | the seven harnesses at the Dockerfile's ARG versions |
| `package-lock.json` | generated from it, see below |
| `dependabot.yml` | a proposed `.github/dependabot.yml`: npm on `/images/harness` grouped with a cooldown, docker for the base digest, github-actions. Inert here |

## Building

No Node on the host; npm runs in a container.

```sh
cd spikes/harness-containers/image-lockfile
# regenerate the lock (only when package.json changes)
podman run --rm -v "$PWD":/w:Z -w /w docker.io/library/node:24 \
  npm install --package-lock-only --no-fund --no-audit
# the build wants a `memory` binary beside the Dockerfile, as `make image` copies in;
# any executable that prints a version will do for the spike
printf '#!/bin/sh\necho "memory 0.0.0-standin"\n' > memory && chmod +x memory
podman build -t localhost/harness-lockfile:spike .
rm memory
```

Then, from `spikes/harness-containers`:

```sh
IMAGE=localhost/harness-lockfile:spike ./run-podman.sh
IMAGE=localhost/harness-lockfile:spike ./run-podman.sh sh /work/harness-smoke.sh
```

A bump is `npm install <pkg>@<version> --package-lock-only` in the same container, or the
Dependabot pull request that does it.

`overrides` in `package.json` raises a transitive package that a harness pins exactly and
that has a fixed release the harness does not take yet. Dependabot cannot raise those
itself. Today it holds `sharp` at 0.35.5 (CVE-2026-96889, in the `librsvg` it bundles),
which `@qwen-code/qwen-code` 0.25.0 and 0.26.0 pin at 0.35.4. Remove an entry when every
harness that pulls the package in takes the fixed version itself.
