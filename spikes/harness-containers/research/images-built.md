# Harness images, built and measured (20 October 2026)

Host: Linux on WSL2 (kernel 6.18.40), x86_64, 16 CPUs, rootless podman 4.9.3 (runc). Each
image was built alone, measured, run through the checks, then removed (`podman rmi`) before
the next build started. Disk free (`df -B1G --output=avail /`) was 645 GB at the start and
645 GB at the end. The lowest it went was 637 GB.

Spike files are in the worktree:
`$REPO/spikes/harness-containers/image-lockfile/`
(Dockerfile, package.json, package-lock.json, dependabot.yml, README.md) and `.../image-nix/`
(flake.nix, flake.lock, README.md).

## The three images

| | Today (`images/harness/Dockerfile`) | A-1 lockfile | A-2 Nix (llm-agents.nix + nix2container) |
|---|---|---|---|
| Base | `ubuntu:24.04` tag | `ubuntu:24.04@sha256:68ee2200...aff33ac` (index digest, resolved from the registry today) | none (Nix closure only) |
| Build wall time | **319.64 s** (`podman build --no-cache`, base already local) | **313.45 s** (same) | cold, empty store: **360.07 s** build + **115.71 s** `podman load` of the 1.39 GB archive. Warm store, no change: **60.76 s** (nothing rebuilt) + load |
| Image size (`podman image inspect .Size`) | 3,150,165,935 B (3.15 GB), 9 layers | 3,150,165,874 B (3.15 GB), 11 layers | 4,079,415,672 B (4.08 GB), 60 layers. The OCI archive is 1,389,606,400 B |
| claude | 2.1.296 | 2.1.296 | **2.1.304** |
| codex | 0.162.1 | 0.162.1 | **0.163.0** |
| gemini | 0.63.0 | 0.63.0 | **0.64.0** |
| copilot | 1.0.95 | 1.0.95 | **1.0.100** |
| opencode | 1.19.0 | 1.19.0 | **1.19.3** |
| qwen | 0.25.0 | 0.25.0 | **0.25.3** |
| grok | 1.0.50 | 1.0.50 | **1.0.62** |
| pi | 1.1.0 | 1.1.0 | **1.3.0** |
| muse | 1.4.11-R6667.1 (whatever the channel served at build time) | 1.4.11-R6667.1 (same) | 1.4.11-R6667.1 (pinned by version and hash) |
| node / python3 | 24.21.0 / 3.12.3 | 24.21.0 / 3.12.3 | 24.21.0 / 3.14.7 |
| What is pinned | 7 top-level npm versions plus pi's. No integrity hashes. `npm ls -g --all` shows 31 packages under the seven harnesses and 119 under pi. Everything below the top level resolves at build time, within whatever ranges the parent packages declare. The base image and Muse are not pinned | Base pinned by digest. The lock has **107 packages, all 107 with `resolved` and a sha512 `integrity`** (98 are optional per-platform builds; `npm ci` installed 30). pi (119 packages) and Muse are still unpinned, as the spike was told to leave them | `flake.lock` pins 7 inputs by commit and `narHash` (llm-agents.nix `06667f1e`, nixpkgs `f20cd90e`, nix2container `535166ec`, and so on). That yields **169 store paths, each pinned by hash**, with all nine harnesses inside |
| Contract check (`IMAGE=<tag> ./run-podman.sh`) | 11 PASS, 1 FAIL: `no vsock socket (created)`. This is the same result as the podman baseline in the spike README, and the runtime decides it, not the image | 11 PASS, same 1 FAIL | 11 PASS, same 1 FAIL. python3 is present. `/usr` holds only `bin/env`, so I also checked by hand that writing to `/nix/store` and `/bin` gives `Read-only file system` |
| Smoke (`./run-podman.sh sh /work/harness-smoke.sh`) | claude, codex and gemini start. Claude exited 124 (the timeout); **7 calls reached the egress** | same: exit 124, **7 calls** | same: exit 124, **7 calls** |

All nine harnesses also printed a version inside every image when run with `--network none`
as uid 1000 (`versions.sh`, `<b> --version`). Every binary was found: under
`/usr/local/bin` (today), under `/opt/harness/node_modules/.bin` for the seven npm ones
(lockfile), and under `/bin` (Nix).

## Where each number comes from

- Build times: `/usr/bin/time -f %e podman build --no-cache -t ... .` on a scratch copy of the
  build context, with a two-line stand-in `memory` script. For Nix:
  `/usr/bin/time podman run --rm -v nix-spike-store:/nix ... docker.io/nixos/nix
  sh -c 'nix run -L .#image.copyTo -- oci-archive:/out/harness-nix.tar'`
  (`nixos/nix` `sha256:ff480dbb...`, Nix 2.35.3, `sandbox = false`), then
  `/usr/bin/time podman load -i`.
- Sizes: `podman image inspect --format '{{.Size}} {{len .RootFS.Layers}}'`. Archive size:
  `ls -la`.
- Versions: `<harness> --version` inside each image.
- Pins: a Python count over `package-lock.json` `packages`. The installed count is
  `npm ls -g --all --parseable` (today) or `npm ls --all --parseable` in `/opt/harness`
  (lockfile), with npm, corepack and pi excluded. npm's own `added 31 packages` (today) and
  `added 30 packages` (lockfile) agree. For Nix, `ls /nix/store | wc -l` inside the image
  and the `locked` entries of `flake.lock`.
- Nix sources: `grep 'copying path' | sort | uniq -c` on the cold-build log. **281 paths came
  from cache.numtide.com and 7 from cache.nixos.org.** Built locally: nix2container (Go),
  nix2container's patched skopeo 1.24.1, and the image JSON.
- Reproducibility: the cold build (empty store volume) loaded as **the same image id**,
  `02db8b33c407...`, as the earlier warm-store build from the same `flake.lock`.
- Nix closure (`du -sm /nix/store/*` in the image): qwen-code 1004 MB, gemini-cli 614,
  codex 561, muse-code 367, claude-code 276, copilot-cli 188, grok 183, opencode 178,
  python3 144, pi 98, nodejs-slim 90. The whole store is 4121 MB.
- Disk, free before/after: today 645/642 GB, lockfile 645/642 GB, Nix first attempt
  644/638 GB, Nix second 638/637 GB, Nix cold 644/637 GB. After cleanup: 645 GB.

## Where the Nix build failed first, and the fix

The first Nix run failed after 181.01 s in `layers.json`, with the message
`the file '/etc' already exists in the tar with perms []types.Perm(nil) but is overridden
with perms [...]`. A `perms` entry (uid 1000 on `/home/user` and `/work`, 1777 on `/tmp`)
was attached to a derivation that also held `/etc/passwd`, and `dockerTools.caCertificates`
also provides `/etc`. nix2container refuses a directory that two `copyToRoot` entries give
different perms. The fix, now in `flake.nix`: the derivation that carries `perms` holds only
`/home/user`, `/work`, `/run/memory`, `/memory`, `/host-home` and `/tmp`. `/etc` and
`/opt/memory` come from separate derivations. A second problem showed after the fix: an
`oci-archive:` written by `copyTo` carries no name, so `podman load` gives an untagged image
and it needs a `podman tag`.

## What surprised me

The lockfile costs nothing. The image is 61 bytes smaller and built in 313 s against 320 s.
It is also smaller in scope than A8 implied. The seven npm harnesses install only 30 to 31
packages, because nearly all of them ship as self-contained bundles or native per-platform
binaries; most of the lock's 107 entries are platform variants that never install. So the
transitive float we close is narrow. The large float is pi: 119 packages installed with
`npm -g`, unpinned, which the spike left alone as told. pi needs its own lock, because it
installs with `--ignore-scripts` while claude-code's postinstall has to run.

Both npm builds print `5 packages have install scripts not yet covered by allowScripts`
(claude-code, grok, opencode, @github/keytar, @qwen-code/audio-capture). The scripts ran
under npm 11.19.0. I did not test npm 12.2.0, which the container announces, and whether it
blocks these scripts should be checked before a Node bump.

On the Nix side:

- The image is reproducible: same image id from a cold store and a warm one.
- It is 30% larger. Most of the difference is qwen-code at 1004 MB, built from source with
  737 top-level `node_modules` entries rather than npm's bundle.
- It runs ahead of our pins on all eight versioned harnesses. A Nix image means taking
  numtide's versions, or overriding them one by one.
- 281 of its 288 store paths came from cache.numtide.com, so in practice we trust numtide's
  cache more than nixpkgs'.
- From an empty store to an image in podman took about 476 s, against 320 s for the
  Dockerfile. A quarter of that was `podman load` of the 1.39 GB archive, a cost the
  Dockerfile path does not have.
- The contract check passes, but its `/usr` read-only test is vacuous for this image, since
  the system lives in `/nix/store`. `CHECK_SYSTEM=/nix/store` would cover it, but
  run-podman.sh does not pass `CHECK_*` through.
