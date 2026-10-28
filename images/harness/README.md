# The harness image

The harness image holds seven of the eight harnesses that `amem launch` knows, Node, and
the `memory` binary. `make image` builds it from the Dockerfile in this directory. It
copies the release binary into the image. Then it builds with `wslc build` (Windows, WSL
containers), or with `podman build` where that is present. The tag is
`durable-memory/harness:dev`. At launch, `MEMORY_IMAGE` or `--image` overrides it.

The harness image gives the same environment on each machine: the same harness versions
and tools. It does not depend on the contents of the WSL or the laptop of a developer.
`package.json` pins the seven npm harnesses. `package-lock.json` pins each package under
them, transitive packages included, by version and sha512. `npm ci` installs exactly
these into `/opt/harness`. The build links their bins into `/usr/local/bin`. Node and pi
are the `ARG`s at the top of the Dockerfile. Muse Code follows its release channel.

To change each pin to its latest release, run the bump script. The daily check does the
same. The script needs npm. If the host does not have Node, run the script in a container,
from the top directory of the repository:

```sh
podman run --rm -v "$PWD":/r:Z -w /r docker.io/library/node:24 sh scripts/bump-harness-versions.sh
```

To set one npm harness to a given version, change `package.json` and the lock together:

```sh
cd images/harness
podman run --rm -v "$PWD":/w:Z -w /w docker.io/library/node:24 \
  npm install --package-lock-only --save-exact --no-fund --no-audit '@openai/codex@0.164.0'
```

A pull request that changes this directory gets a check of its lock against
`package.json`.

`package.json` also has `overrides`. An override holds a package under one harness at a
version that is different from the version that the harness pins. It does this for a fix
that the harness does not have yet. Each entry applies to its harness alone. Today there
is one entry. `@qwen-code/qwen-code` pins `sharp` at exactly 0.35.4. This version has
CVE-2026-96889 in its bundled librsvg (GHSA-wq5f-xc86-pv6w). The entry holds `sharp` at
0.35.5. Remove the entry when qwen-code itself pins 0.35.5 or later. While the entry is
there, it also holds `sharp` back to 0.35.5 for a qwen-code that pins a newer `sharp`.
npm does not tell you when an override applies. Thus the bump script tells you. After a
bump, it prints a line for each override that its harness no longer needs. These lines
go into the daily pull request. A global install (`npm install -g`) ignores `overrides`.
This is one reason that the image installs from the lock.

## When it is rebuilt

No launch builds the harness image again. `amem launch` uses the image that it gets. If
the image is missing, `amem launch` fails immediately. The inputs of the image are the
pins in the lockfile and the Dockerfile, and the base image. Thus the image gets a new
build when these inputs change. It also gets a new build on a schedule, for the one input
that changes without notice:

| When | What | Why |
|---|---|---|
| A pin changes | `.github/workflows/image.yml` builds the image with the release binary, to show that the image still builds. The workflow publishes nothing. Thus each machine builds its own image with `make image` | A new harness version is a deliberate change that a person reviews |
| Daily, 05:41 UTC | `scripts/bump-harness-versions.sh` checks npm and nodejs.org for newer versions of the seven npm harnesses, pi and the Node line. When an npm harness changed, it makes the lock again. It opens or updates a pull request that changes the pins | Security releases of a harness arrive within one day. A person still merges the pull request |
| A machine wants the updates of the base image and of apt | `make image` again, after a pull of `ubuntu:24.04`. Muse Code also changes at this time, because its launcher gets the version from the release channel of Meta at build time | `ubuntu:24.04` changes under its tag. Nobody publishes an image that a machine can pull instead |
| Urgent | Change the pin manually, and merge | The daily check gives the minimum update frequency, not the maximum |

On a local machine, `make image` is for the development of the launcher itself. The bot
opens its pull request with the token of the workflow. Thus that pull request runs CI, as
any other pull request does, when a maintainer approves the workflow run.

## Never published

Nobody publishes this image, and nothing in this repository can push it to a registry.
Each machine builds its own with `make image`. There are two reasons:

- The image holds harnesses under their vendors' own licences. Claude Code and Copilot
  CLI are two of them, and the project may not redistribute them. The Apache release
  policy says that each artifact that the project distributes contains only code with a
  suitable licence.
- The image is built from unreleased source. The Apache release policy says that the
  project does not publish unreleased material outside its development community.

Three parts hold this. They stop an accident. They are not a security boundary: a person
who means to push can write a push that they do not read.

- `scripts/no-image-push.sh`, run by `make check`, reads each file as a shell reads a
  command. It fails when a file would push an image, log in to a registry, make or fill a
  GitHub release, or give a workflow `packages: write`. This includes Rust, Python and
  JavaScript that pass `"push"` as an argument.
- `.claude/hooks/guard-bash.sh` refuses the same commands when a session types them.
- `.github/workflows/image.yml` names `packages: none`. GitHub already gives no
  `packages` permission to a workflow that names other permissions, so this line only
  makes the rule visible.

What they do not stop:

- A push written to get past them, for example an engine found with `$(command -v podman)`,
  or a command run through `cmd.exe /c` or `powershell.exe`.
- An image saved to a file and then uploaded some other way, for example as a workflow
  artifact.
- A workflow that a branch changes. It runs before `make check` can stop it. Only a
  setting of the repository or of the organisation can stop that first run. Ask ASF Infra
  to turn off GitHub Packages for this repository if that matters.

`scripts/no-image-push.sh` also refuses a Rust, Python or JavaScript string that is
exactly `"push"`, whatever it is for. Write such a word another way.
