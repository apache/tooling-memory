# Harness image and egress beside the box

Research layer: (A) what could replace the hand-maintained `images/harness/Dockerfile` and
`scripts/bump-harness-versions.sh`; (B) network-control pieces that ship with or beside
sandboxes, measured against `crates/egress`. All facts checked on 2026-10-19 with `gh api`
(licence, `pushed_at`, `archived`, stars, latest release) and the cited files; anything not
read is marked as such.

## What we have today (read from the tree, for reference)

- `images/harness/Dockerfile`: `FROM ubuntu:24.04` (a tag, **not** a digest), Node from the
  nodejs.org tarball (`ARG NODE_VERSION=24.21.0`), seven harnesses by `npm install -g pkg@ARG`
  (top-level pinned only; transitive deps float at build time, no lockfile, no integrity),
  pi with `--ignore-scripts`, Muse Code from `curl https://dev.meta.ai/install.sh | bash`
  (pinned by nothing but the channel at build time). Fixed paths `/home/user`, `/work`,
  `/opt/memory`, `/run/memory`, `/memory` are `mkdir`ed; the image holds no policy.
- `scripts/bump-harness-versions.sh`: asks npm / nodejs.org for latest, `sed`s the ARGs; the
  daily `image.yml` job opens a PR. The digest that is pinned is the *built* image id, at
  launch (`crates/memory/src/wslc_api.rs`), not the base.
- Contract points 1 and 2 are held by the runtime (namespaces on Linux, `wslc --network none`
  plus mounts on Windows), never by the image. Any image replacement only has to keep the
  fixed paths present and uid 1000; it cannot break or hold points 1-2 itself.
- `crates/egress/src/lib.rs:717-790` (`connect_or_not_found`): the CONNECT tunnel matches the
  host against `tunnel_hosts`, checks label vs ceiling, then `TcpStream::connect(&target)` on
  **any port** of that host, with **no resolved-address check** (an allowlisted name that
  resolves to loopback, link-local or a metadata address is dialled). Several candidates
  below close exactly this gap; noted as a borrow.
- Side finding: the repo has GitHub Actions workflows but no `.github/dependabot.yml`.
  ASF Infra (`infra.apache.org/dependabot.html`, read today): "All repositories using GitHub
  Actions must have Dependabot (or Renovate) enabled for the github-actions ecosystem."

---

# Part A: harness image

## A1. Dev Container spec, Features, reference CLI

- **Repo / licence (verified)**: github.com/devcontainers/spec: `CC-BY-4.0` (spec text);
  github.com/devcontainers/features: `MIT`; github.com/devcontainers/cli: `MIT`;
  github.com/devcontainers/images: `MIT`. Via `gh api repos/OWNER/REPO --jq .license.spdx_id`.
- **Form**: spec + Node CLI (`@devcontainers/cli`) that drives Docker or Podman; Features are
  OCI artifacts holding an `install.sh` run as root at build time.
- **Platforms**: Linux yes, macOS yes, Windows yes (wherever Docker/Podman and Node run). No
  wslc driver.
- **Mechanism**: `devcontainer.json` names a base image plus Features; the CLI builds a derived
  image by running each Feature's install script, and writes `devcontainer-lock.json`.
- **Harness Features that exist** (containers.dev index, fetched today): official
  `ghcr.io/devcontainers/features/copilot-cli` (has `version`); Anthropic's
  `anthropics/devcontainer-features` `claude-code` 1.0.5 (MIT, **no options, so no version
  pin**, last push 2025-12-16); `devcontainers-extra/features` claude-code and opencode (with
  `version`); the rest (codex, gemini-cli, grok, pi, qwen) only from single-maintainer repos
  (sliekens, stu-bell, NolanJX, siri404, ...). No Feature found for Qwen Code or Muse Code.
- **Pinning / bumps**: `devcontainer-lock.json` pins each **Feature artifact** by
  `sha256` digest and integrity, but per the lockfile spec not the tool a Feature installs
  ("installing "latest" of a tool will still have different outcomes"). Tool versions are a
  Feature option string. Dependabot has a `devcontainers` ecosystem (version updates only);
  Renovate's `devcontainer` manager updates the `image` and Feature refs. Neither bumps a
  Feature's `version` option to a new npm release.
- **Against our contract**: silent on 1-2 (the spec has `runArgs`/`mounts`, but the runtime
  holds them, as now). Breaks point 7's spirit only in that it adds Node + a Docker-compatible
  daemon/CLI to the build path. No wslc support: on Windows we would still `wslc build`.
- **Maintenance**: cli tag v0.89.0, last commit 2026-10-02, pushed 2026-10-19; features pushed
  2026-10-09; not archived.
- **If we adopted it**: replaces the Dockerfile with `devcontainer.json` + 8 third-party
  install scripts of uneven quality, run as root; adds Node and the CLI to every build;
  versions still need a bot that understands Feature options (none does).
- **Verdict**: **reject** as the build path (fragmented, unpinned tool versions, no wslc);
  **learn-from** the lockfile idea.

## A2. Anthropic's Claude Code devcontainer reference (with `init-firewall.sh`)

- **Repo / licence (verified)**: github.com/anthropics/claude-code `.devcontainer/`;
  `gh api .../license` returns no SPDX id; `LICENSE.md` reads "© Anthropic PBC. All rights
  reserved. Use is subject to Anthropic's Commercial Terms of Service." **Not permissive.**
- **Form**: Dockerfile + devcontainer.json + bash firewall script; container needs
  `--cap-add=NET_ADMIN --cap-add=NET_RAW` and a sudoers entry for the script.
- **Platforms**: Linux yes; macOS/Windows via Docker Desktop VM.
- **Mechanism**: `FROM node:20`, `npm install -g @anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}`
  with default `latest`; `postStartCommand` runs `init-firewall.sh`, which flushes iptables,
  resolves an allowlist (npm, api.anthropic.com, sentry, statsig, VS Code, GitHub meta ranges)
  into an ipset once, and sets OUTPUT DROP.
- **Against our contract**: breaks 1 (the box keeps a network namespace with a route out,
  filtered by IP resolved once at start; DNS and SSH out are allowed; the host /24 is
  allowed). Point 2: workspace bind at `/workspace`, volumes for history and `~/.claude`;
  fixed paths possible. Not pinned (`latest`, `node:20` tag).
- **Maintenance**: `.devcontainer` last commit 2026-06-30.
- **If we adopted it**: nothing of ours is replaced that our egress does not already do
  better; it adds root-in-container and a cap set we avoid.
- **Verdict**: **learn-from** only (licence rules out copying; the design is weaker than ours).

## A3. Docker Sandboxes (`sbx`), kits and the kit spec

- **Repo / licence (verified)**:
  - Tool `sbx`: github.com/docker/sbx-releases, SPDX `NOASSERTION`; its LICENSE reads
    "Copyright © 2026 Docker Inc. All rights reserved." **Proprietary.** Docs: CLI and local
    sandboxes free incl. commercial; org policy management paid.
  - Kit spec: github.com/docker/sandbox-kit-spec, `Apache-2.0` (experimental; "final version
    targeted for Q4 2026").
  - Kits: github.com/docker/sbx-kits-contrib, `Apache-2.0`; has kits for claude, codex,
    copilot, gemini, grok, opencode, pi (no qwen, no muse).
  - Base image `docker/sandbox-templates:shell-docker`: Docker Hub description empty; no
    public source or licence found (`gh search code`, Hub API). **Licence of the base image
    unknown.** Published kit images (`docker.io/sbx/claude-image`) redistribute Claude Code.
- **Form**: proprietary host CLI managing microVMs locally ("microVM isolation" per docs) or
  Docker cloud; kits are OCI images with a descriptor annotation.
- **Platforms**: docs do not list OSes on the overview page (not verified further).
- **Mechanism**: a kit `spec.yaml` declares image, entrypoint, credentials (with per-domain
  header injection and OAuth sentinel tokens such as `oai-oat01-proxy-managed`), and a network
  allowlist; a host-side proxy enforces deny-by-default and "injects authentication headers
  into outbound HTTP requests. The raw credential values never enter the VM" (security page).
- **Pinning / bumps**: kit Dockerfiles take `ARG BASE_IMAGE=docker/sandbox-templates:shell-docker`
  (a floating tag; comment: "which is why CI also rebuilds on a schedule") and install harnesses
  by vendor `curl | bash` installers; the claude kit's `CLAUDE_CODE_VERSION` defaults to empty
  (installer default), spec `image: ...:latest`. Dependabot in that repo covers only
  github-actions and gomod. **Harness versions float.**
- **Against our contract**: breaks 3 (needs its own VM runtime) and 5 (not wslc); point 1 is
  held by their proxy, not by a socket we control; point 2 is their mount model.
- **Maintenance**: sandbox-kit-spec pushed 2026-10-19 (153 stars); sbx-kits-contrib pushed
  2026-10-19 (177 stars).
- **If we adopted it**: replaces the image and the egress with a proprietary runtime and proxy;
  loses label/ceiling, metering, `/mcp`.
- **Verdict**: **reject** the tool; **borrow** (Apache-2.0, with attribution) the kit spec's
  credential declaration shape (per-domain `inject` header + format, OAuth sentinels,
  `skipIfEnv`) and the per-harness network allowlists in `sbx-kits-contrib/*/spec.yaml` as a
  checked reference for our tunnel hosts. Note their reasoning (claude kit README) for the
  native installer over npm: the npm package's `bin/claude.exe` is a stub replaced by a
  `postinstall` from optional deps, so an install that skips lifecycle scripts ships the stub
  and still exits 0. We run scripts for everything but pi, so this does not bite us now.

## A4. Maintained multi-harness images

| Repo | SPDX (gh api) | Pushed | Stars | Notes |
|---|---|---|---|---|
| chmouel/cli-agents-container | Apache-2.0 | 2026-10-18 | 1 | `FROM ubuntu:rolling`, Node 20.19.1, nightly rebuild to latest; `version.json` records what was built (claude 2.1.302, codex 0.163.0, gemini 0.64.0, opencode 1.19.3, copilot 1.0.98) |
| cloudyhomelab/agents-docker | GPL-3.0 | 2026-10-19 | 0 | out (GPL) |
| emarc/claude-contained | none | 2026-09-22 | 23 | no licence: out |
| centminmod/claude-code-devcontainers | none | 2026-01-01 | 32 | no licence: out |

- **Form**: Dockerfiles + nightly CI publishing images; no pins (floating base, latest tools).
- **Platforms**: Linux images (amd64/arm64 for chmouel).
- **Against our contract**: silent on 1-2 (images only). None carries our fixed paths, uid,
  or all nine harnesses (none has Qwen, Grok, pi, Muse).
- **If we adopted it**: an unpinned third-party image with a single maintainer as our base.
- **Verdict**: **reject**. Useful only as a cross-check that our pins lag (we pin claude
  2.1.296, codex 0.162.1; latest built there 2.1.302 / 0.163.0).

## A5. Chainguard apko + melange (Wolfi)

- **Repo / licence (verified)**: chainguard-dev/apko `Apache-2.0`; chainguard-dev/melange
  `Apache-2.0`; wolfi-dev/os `NOASSERTION`, LICENSE reads "The Wolfi build recipes themselves
  are provided under the Apache License 2.0 ... underlying packages ... under the license
  terms provided by each FOSS project".
- **Form**: two Go single binaries. apko assembles an OCI image from apk packages
  declaratively (no Dockerfile, no RUN); melange builds apks from YAML pipelines in a
  bubblewrap, QEMU or Docker runner. Rootless.
- **Platforms**: apko releases for Linux and darwin (amd64/arm64); building images targets Linux.
- **Mechanism**: `apko lock` resolves every package to an exact version + checksum, giving a
  bit-reproducible image. melange has a built-in `npm/install` pipeline (`package`, `version`
  inputs).
- **Harness coverage in Wolfi** (checked `wolfi-dev/os/<pkg>.yaml`): `codex` 0.163.0 (built
  from source, Apache-2.0), `nodejs-24` 24.22.0, `git`, `ripgrep`, `python-3.13`. **Absent**:
  claude-code, gemini-cli, copilot, opencode, qwen-code. We would write and host melange
  recipes for seven harnesses plus a signing key and apk repo.
- **Pinning / bumps**: lockfile pins every package; Renovate has no apko manager (found a
  third-party `apko-renovator`, v0.3.0, not evaluated); Wolfi itself bumps via its own bots.
  Chainguard's free `cgr.dev/chainguard/*` images are `:latest` / `:latest-dev` only;
  versioned tags are a paid tier (Chainguard docs, via search).
- **Against our contract**: silent on 1-2; can lay out fixed paths and uid in `apko.yaml`.
  wslc can run the resulting OCI image (not tested).
- **Maintenance**: apko v1.4.12 (2026-10-19), melange v0.63.0 (2026-10-19), both pushed today.
- **If we adopted it**: replaces Dockerfile + bump script with `apko.yaml` + seven melange
  recipes + key management + apk hosting; ubuntu base becomes glibc Wolfi.
- **Verdict**: **learn-from**. Strongest reproducibility story, but most of the packaging
  burden moves onto us.

## A6. buildah (and ko)

- **Repo / licence (verified)**: containers/buildah `Apache-2.0`, v1.45.2 (2026-10-12);
  ko-build/ko `Apache-2.0`, v0.19.1 (2026-06-29).
- **Form**: buildah is a rootless daemonless image builder (already on this machine,
  1.33.7); ko builds images from Go `main` packages only.
- **Mechanism**: buildah builds our existing Dockerfile; upstream docs list
  `--source-date-epoch`, `--rewrite-timestamp`, `--timestamp` (local 1.33.7 has only
  `--timestamp`). Layer reproducibility is capped by `npm install` and `apt-get` resolving
  at build time.
- **Pinning / bumps**: whatever the Dockerfile says; no update mechanism of its own.
- **Against our contract**: silent on 1-2.
- **If we adopted it**: `make image` already uses `podman build` (buildah underneath); no change.
- **Verdict**: buildah: **already in use, no change**; ko: **reject** (Go-only).

## A7. Nix: `dockerTools` / nix2container + numtide/llm-agents.nix

- **Repo / licence (verified)**: NixOS/nixpkgs `MIT` (expressions; package payloads keep their
  own licences); nlewo/nix2container `Apache-2.0`, v1.1.0 (2026-09-24), pushed 2026-10-17;
  numtide/llm-agents.nix (was nix-ai-tools) `MIT`, pushed 2026-10-19, 2160 stars. The Nix
  tool itself (NixOS/nix) is `LGPL-2.1`: build-time tool only, nothing of it ships in the
  image unless asked; flag for the licence review. DavHau/nix-portable `MIT`, but latest
  release v012 is from 2024-04-15.
- **Form**: Nix flake evaluated by the Nix CLI; produces an OCI image without a daemon
  (nix2container streams layers to podman/skopeo; `dockerTools.streamLayeredImage` produces a
  tar stream). No root needed if Nix is run inside a container or in single-user mode.
- **Platforms**: build on Linux (or macOS with a Linux builder); image is Linux.
- **Mechanism**: `flake.lock` pins nixpkgs and llm-agents.nix by git revision + narHash; every
  package is a fixed-output derivation with a hash, so transitive npm/bun deps are pinned
  (claude-code fetched from `downloads.claude.ai` with a manifest checksum; pi via
  `buildNpmPackage` with a lock).
- **Harness coverage**: llm-agents.nix has **all nine**: claude-code 2.1.303, codex 0.163.0,
  gemini-cli 0.64.0, copilot-cli 1.0.98, opencode 1.19.3, qwen-code 0.25.3, grok 1.0.62,
  pi 1.3.0, muse-code 1.4.10-R6427.1 (versions read from `packages/*/` today). README:
  "Automatically updated daily"; binary cache at `cache.numtide.com` (key in its README).
  nixpkgs proper also has claude-code 2.1.303 (`license = unfree`), codex, gemini-cli (0.47.0,
  stale), github-copilot-cli (unfree), opencode, qwen-code, pi-coding-agent. Differences from
  ours: grok there is the GCS-hosted binary, not the `@xai-official/grok` npm package; Muse is
  pinned by version **and hash** from Meta's channel manifest, which our Dockerfile cannot do.
- **Pinning / bumps**: bump = `nix flake update llm-agents` (one PR, all harnesses, all hashes).
  Dependabot has a `nix` ecosystem (version updates); Renovate has a nix manager with
  lock-file maintenance. The image digest is a pure function of `flake.lock`.
- **Against our contract**: silent on 1-2 (runtime holds them); fixed paths/uid via
  `copyToRoot` + `perms` + `config.User`. wslc runs OCI images; whether `wslc build`/load
  accepts an OCI archive was not checked.
- **Maintenance**: as above; all pushed within the last week.
- **If we adopted it**: replaces Dockerfile and bump script with a `flake.nix` (~40 lines) and
  `flake.lock`; adds Nix (LGPL tool) to the image build, and trust in numtide's update bot and
  binary cache (or build from source). The image no longer has apt/Ubuntu; a harness that
  shells out to tools must find them in the closure.
- **Verdict**: **adopt candidate, spike first**. Only option found that covers all nine
  harnesses, pins transitive deps, and turns bumps into one lock update. Even if not adopted,
  **borrow** its Muse pinning (manifest + hash).

## A8. Keep the Dockerfile; replace the script with lockfiles and a stock bot

- **Repo / licence (verified)**: dependabot/dependabot-core `MIT` (GitHub-native service);
  renovatebot/renovate `AGPL-3.0` (run as a hosted app or CI tool, not linked or shipped; ASF
  Infra page allows "Dependabot (or Renovate)"; whether the Renovate app is enabled for the
  apache org was not confirmed, a Sling wiki page says it is not org-wide).
- **Form**: config files in the repo; bot opens PRs.
- **Platforms**: n/a (GitHub-side).
- **Mechanism**:
  1. Put the npm harnesses in `images/harness/package.json` + `package-lock.json` and
     `npm ci` them into a prefix (bins linked into `/usr/local/bin`). The lockfile carries
     `integrity` (sha512) for every transitive package, which today floats.
  2. `FROM ubuntu:24.04@sha256:<digest>`.
  3. Dependabot `npm` ecosystem on `/images/harness` (grouped, with `cooldown`) bumps the
     harnesses and the lock; Dependabot `docker` ecosystem bumps the base digest (it refreshes
     an existing digest but will not add one to a tag-only ref, per Chainguard's Dependabot
     guide); add `github-actions` to meet the ASF rule above.
  4. Node: either a multi-stage `FROM node:24.x-<distro>@sha256 AS node` + `COPY --from`
     (Dependabot docker updates it; untested on ubuntu glibc), or Renovate's
     `customManagers:dockerfileVersions` preset, which matches `# renovate: datasource=...
     depName=...` above an `ARG <NAME>_VERSION=` (our ARGs other than `NODE_VERSION` lack the
     `_VERSION` suffix and would need renaming or a custom regex).
  5. Muse Code stays a script step (not on npm); borrow A7's manifest + hash pin.
- **Pinning / bumps**: digest for base; lockfile with integrity for npm; bot PRs.
- **Against our contract**: unchanged (silent on 1-2).
- **Maintenance**: dependabot-core pushed 2026-10-19; renovate pushed 2026-10-19.
- **If we adopted it**: deletes `bump-harness-versions.sh` and the bump job in `image.yml`
  (keep the build-to-check job); adds `package.json`, `package-lock.json`,
  `.github/dependabot.yml`. No new runtime deps.
- **Verdict**: **adopt**. Smallest change, stock tooling, closes the transitive-pin and
  base-digest gaps.

---

# Part B: egress beside the box

What is ours and stays ours whatever is adopted: the label-vs-ceiling check per session
(needs `dm_core::Label` and the session's read history), per-session token metering read from
response bodies including SSE (`usage.rs`), `/mcp` on the same socket served by `dm_server`,
system-message folding, and the socket-plus-relay topology. Slices a third party could take:
(a) CONNECT allowlist with resolved-address guard, (b) credential injection/sentinel swap,
(c) TLS termination so a tunnel (Copilot) can be metered and credential-injected, (d) audit
logs/OTel.

## B1. sandbox-runtime (`srt`) HTTP and SOCKS proxies, and `srt proxy`

- **Repo / licence (verified)**: github.com/anthropic-experimental/sandbox-runtime,
  `Apache-2.0` (gh api; `package.json` `"license": "Apache-2.0"`, name
  `@anthropic-ai/sandbox-runtime`, 0.0.80).
- **Form**: TypeScript library + CLI `srt` (Node >= 22.12 or Bun >= 1.4); deps
  `@pondwader/socks5-server`, `commander`, `node-forge`, `zod`. `srt proxy` / `srt-proxy`:
  standalone HTTP proxy, no privileges.
- **Platforms**: Linux yes (bubblewrap + proxies over unix sockets), macOS yes (Seatbelt,
  localhost port), Windows yes (WFP filter for an `srt-sandbox` account; README).
- **Mechanism**: per README, on Linux "the network namespace of the sandboxed process is
  removed entirely ... proxies running on the host (listening on Unix sockets that are
  bind-mounted into the sandbox)". HTTP proxy + SOCKS5 enforce domain allow/deny lists, a
  resolved-address check (loopback, link-local, metadata endpoints, this host's addresses,
  configurable CIDRs) on the address actually dialled, optional in-process TLS termination,
  and credential masking (`mode: "mask"` puts a sentinel in the env/file and swaps the real
  value back on egress to `injectHosts`, only over terminated TLS). `srt proxy` takes a
  listening fd from the host and asks an external **decider** process (length-prefixed JSON
  over an fd: `req` -> `verdict` allow/deny/`need_body`, allow may set headers) before
  dialling; CONNECT is refused unless a CA is passed with `--ca-fd`.
- **Against our contract**: point 1 is the same design as ours (no netns, unix socket bound
  in); `srt proxy --listen-fd` can accept on a unix socket we create. Point 2: the
  bubblewrap side lays out binds, but we would use only the proxy. Point 6: the decider sees
  requests (and bodies on `need_body`), so the label check fits there; it does **not** see
  responses, so token metering stays ours; no `/mcp`. Adds Node/Bun to the egress side.
- **Maintenance**: pushed 2026-10-19, v0.0.80 (2026-10-16), 5596 stars, not archived; 0.0.x
  with behaviour changes called out between releases.
- **If we adopted it**: could take slices (a)(b)(c) for the Copilot tunnel only, with our
  egress as decider; adds a Node runtime, a CA to manage, a second process and protocol.
- **Verdict**: **borrow** (Apache-2.0): the resolved-address deny set and "dial the address
  that passed" rule, the decider split, the TRACE/TRACK refusal, and the "inject only over
  verified TLS" rule. Running it beside our Rust egress is not worth a Node dependency.

## B2. Codex network proxy (`codex-rs/network-proxy`) and `responses-api-proxy`

- **Repo / licence (verified)**: github.com/openai/codex, `Apache-2.0`; crate `Cargo.toml`
  has `license.workspace = true`, workspace `license = "Apache-2.0"`. Not on crates.io
  (`crates.io/api/v1/crates/codex-network-proxy`: "does not exist"). Built on `rama`
  `=0.3.0-alpha.4` (plabayo/rama, Apache-2.0 per gh api).
- **Form**: Rust library (`NetworkProxy::builder().policy_decider(...)`) + binary; no
  privileges. Path deps on `codex-utils-*` and `codex-windows-sandbox`.
- **Platforms**: Linux, macOS, Windows (README covers Windows port ranges).
- **Mechanism**: HTTP proxy (default `127.0.0.1:3128`) and SOCKS5 (`127.0.0.1:8081`) with an
  allow/deny domain policy (scoped wildcards, global `*` rejected), local/private address
  blocking, a "limited" (read-only) mode, HTTPS MITM with hooks (match host/method/path,
  e.g. `strip_request_headers`), a credential broker module (`credential_broker.rs`), OTel
  audit events, and an embedder-supplied async decider. `responses-api-proxy` is a separate
  credential-holding reverse proxy for the Responses API (key on stdin).
- **Against our contract**: point 1: listens on loopback TCP; unix sockets appear only as
  a macOS-only upstream (`x-unix-socket`), no unix-socket listener documented, so we would
  keep our socket + relay in front. Point 6: covers (a)(b)(c); no ceiling concept, no
  metering, no MCP.
- **Maintenance**: pushed 2026-10-19, rust-v0.163.0 (2026-10-15), 129931 stars.
- **If we adopted it**: vendoring the crate drags the codex workspace and an alpha `rama`
  pin; a different HTTP stack from our axum/hyper/reqwest.
- **Verdict**: **borrow** (Apache-2.0, Rust, so code can be lifted with attribution): domain
  matcher semantics, the private-address checks, the MITM-hook shape for metering Copilot
  through a terminated tunnel. Do not depend on the crate.

## B3. Stripe Smokescreen

- **Repo / licence (verified)**: github.com/stripe/smokescreen, `MIT`.
- **Form**: Go single binary or Go library (`smokescreen.Config`, pluggable
  `RoleFromRequest`); no privileges.
- **Platforms**: release binaries for Linux and darwin (amd64/arm64); Windows not released.
- **Mechanism**: HTTP CONNECT proxy; per-client role (from mTLS cert CN/OU by default)
  checked against a hostname ACL; refuses private IP ranges after resolution unless allowed;
  Prometheus metrics; `--stats-socket-dir` exposes a unix socket for connection tracking.
- **Against our contract**: point 1: listens on IP/port (`--listen-ip`, `--listen-port`); no
  unix listener in its flags. Point 6: covers (a) only; no TLS termination, no injection.
- **Maintenance**: pushed 2026-10-19, v0.1.0 (2026-09-21), 1368 stars.
- **If we adopted it**: a Go daemon for one slice we already have in ~70 lines.
- **Verdict**: **learn-from** (resolved-IP deny, per-role ACL).

## B4. Envoy

- **Repo / licence (verified)**: github.com/envoyproxy/envoy, `Apache-2.0`.
- **Form**: C++ daemon, xDS/YAML config; no privileges for high ports.
- **Platforms**: release binaries Linux x86_64/aarch64 only on the v1.40.0 release; macOS and
  Windows not checked.
- **Mechanism**: dynamic forward proxy filter (docs warn it must be behind default-deny RBAC;
  `resolved_address_filter` in its DNS cache), `credential_injector` HTTP filter (generic or
  OAuth2 client-credentials into `Authorization`, `overwrite` option), TLS origination, unix
  pipe listeners (not re-verified today).
- **Against our contract**: point 1 holdable (listener on a unix path); point 6 covers
  (a)(b)(c); metering would need a Lua/Wasm/ext_proc filter; no ceiling, no MCP.
- **Maintenance**: v1.40.0 (2026-10-13), pushed 2026-10-19, 29092 stars.
- **If we adopted it**: a large C++ daemon and a config language beside a Rust binary whose
  whole job is this.
- **Verdict**: **reject** for us.

## B5. Agent Router (formerly Envoy AI Gateway)

- **Repo / licence (verified)**: github.com/envoyproxy/ai-gateway redirects to
  github.com/theagentrouter/agent-router, `Apache-2.0` (gh api on both); README: "Envoy AI
  Gateway is now Agent Router, an Agentic AI Foundation project ... same ... Apache 2.0
  license".
- **Form**: Go control plane + Envoy data plane + ext_proc; `aigw run` standalone on a laptop
  (listens `localhost:1975`), otherwise Kubernetes with Envoy Gateway.
- **Platforms**: Linux; others not verified.
- **Mechanism**: OpenAI-compatible front for many providers, upstream credentials held by the
  gateway, token-based quotas and usage attribution, MCP routing.
- **Against our contract**: point 1 not designed for it (TCP listener); point 6 overlaps
  (b) and metering, but per gateway key/route, not per sandbox session label; translation to
  an OpenAI shape is not what our harnesses speak natively.
- **Maintenance**: v1.2.0 (2026-10-07), pushed 2026-10-19, 2245 stars.
- **If we adopted it**: Envoy + ext_proc + controller; heavy.
- **Verdict**: **reject**; it is a destination we could point a session at (as with any
  gateway in `grants.toml`), not a replacement.

## B6. agentgateway

- **Repo / licence (verified)**: github.com/agentgateway/agentgateway, `Apache-2.0`
  (workspace `Cargo.toml` `license = "Apache-2.0"`). Not on crates.io.
- **Form**: Rust single binary (`agentgateway-{linux,darwin,windows}-*` in v1.6.0 assets);
  standalone or Kubernetes.
- **Platforms**: Linux yes, macOS arm64 yes, Windows amd64 yes (release assets).
- **Mechanism**: LLM gateway (unified API, budgets, spend controls), MCP gateway (federation,
  stdio/HTTP/SSE/streamable HTTP, OAuth), A2A, CEL RBAC, guardrails, OTel.
- **Against our contract**: point 1: `UnixListener` appears only in
  `management/hyper_helpers.rs` and `control/spiffe.rs` (gh code search); no unix-socket data
  listener found. Point 6: overlaps (b), metering, and MCP federation, but none keyed to a
  per-session label/ceiling; our `/mcp` is the memory server itself, which it would front, not
  replace.
- **Maintenance**: v1.6.0 (2026-10-02), pushed 2026-10-19, 5416 stars.
- **If we adopted it**: a second Rust HTTP stack and policy language in front of ours.
- **Verdict**: **learn-from** (its usage extraction per provider is worth reading against
  our `usage.rs`).

## B7. mitmproxy / mitmproxy_rs

- **Repo / licence (verified)**: mitmproxy/mitmproxy `MIT`; mitmproxy/mitmproxy_rs `MIT`.
- **Form**: Python application with addon API; Rust extension for capture modes.
- **Platforms**: Linux, macOS, Windows.
- **Mechanism**: intercepting HTTP(S) proxy with on-the-fly certs; addons can rewrite
  requests and read responses.
- **Against our contract**: covers (c) and could meter a tunnel via an addon; Python runtime.
- **Maintenance**: v12.2.3 (2026-05-12), pushed 2026-10-19, 45754 stars.
- **If we adopted it**: Python beside the egress for the Copilot slice.
- **Verdict**: **learn-from** (debugging tool for checking what a harness sends; not a
  component).

## B8. LiteLLM proxy

- **Repo / licence (verified)**: github.com/BerriAI/litellm, SPDX `NOASSERTION`; LICENSE:
  "All content that resides under the "enterprise/" directory ... is licensed under ...
  enterprise/LICENSE ... Content outside ... is available under the MIT license".
  `enterprise/LICENSE.md`: may "only be used in production" with a BerriAI subscription.
- **Form**: Python proxy server (plus DB for keys/spend).
- **Platforms**: Linux, macOS, Windows (Python).
- **Mechanism**: OpenAI-compatible gateway over many providers; virtual keys, spend logs.
  Per docs.litellm.ai/docs/enterprise, the licence key gates: SSO, JWT auth, audit logs, RBAC,
  IP ACLs, key rotation, secret managers, tag-based and model-specific-per-key budgets,
  team-based logging, guardrails per key/team, request/response size limits, among others.
  Basic virtual keys and spend tracking are outside that list.
- **Against our contract**: TCP listener; overlaps (b) and metering per virtual key; no label.
  Supply chain: PyPI releases 1.82.7 and 1.82.8 were malicious (2026-03-24, removed; per
  PureAI / FutureSearch reporting).
- **Maintenance**: v1.106.0 (2026-10-18), pushed 2026-10-19, 61580 stars.
- **If we adopted it**: Python + DB; the useful governance parts are the licensed ones.
- **Verdict**: **reject** as a component (open-core where the governance we would want is
  proprietary); usable as an upstream destination, as `scripts/claude-via-llmao.sh` already
  suggests for gateways.

## B9. iron-proxy (and hoophq/cloak)

- **Repo / licence (verified)**: github.com/ironsh/iron-proxy `Apache-2.0`, v0.53.0
  (2026-10-06), pushed 2026-10-16, 727 stars. github.com/hoophq/cloak `MIT`, v0.4.0
  (2026-07-09), 8 stars.
- **Form**: Go single binary, one YAML file; all code under `internal/` (not importable as a
  library); no privileges for high ports.
- **Platforms**: Linux and darwin (amd64/arm64) release assets; no Windows.
- **Mechanism**: MITM egress proxy with its own DNS server (transparent mode) or an explicit
  `tunnel_listen` accepting HTTP CONNECT and SOCKS5; default-deny allowlist with
  host/method/path rules; `upstream_deny_cidrs` enforced after DNS (IMDS + loopback by
  default); `secrets` transform swaps proxy tokens for real ones in headers/query; OAuth and
  GCP/AWS token minting; `tls.mode: mitm | sni-only`; structured audit log; an MCP gateway
  package; a **gRPC transform hook** (`proto/transform/v1/transform.proto`) with
  `TransformRequest` and `TransformResponse`, each able to CONTINUE or REJECT and modify.
  Bodies are buffered only when a transform needs them (`max_response_body_bytes`); how a
  response transform behaves on a streamed SSE body is not documented.
- **Against our contract**: point 1: listeners are TCP addresses (`http_listen`,
  `https_listen`, `tunnel_listen`); no unix-socket listener in the config reference, so it
  would sit behind our socket. Point 6: covers (a)(b)(c)(d); our label check and metering
  could be a gRPC transform, but streaming metering is unproven; no `/mcp` for our memory.
- **If we adopted it**: a Go daemon plus a gRPC service in our egress, plus a CA; only for
  the Copilot/tunnel path.
- **Verdict**: **borrow** the ideas (`upstream_deny_cidrs` defaults, proxy-token swap,
  request/response transform contract); keep as a candidate if we ever need to MITM tunnels.

## B10. Pomerium

- **Repo / licence (verified)**: github.com/pomerium/pomerium `Apache-2.0`, v0.33.4
  (2026-10-02), pushed 2026-10-19.
- **Form**: Go identity-aware **reverse** proxy (ingress to internal apps).
- **Mechanism / contract**: authenticates users into services; not an egress proxy for a
  sandbox. Silent on 1-6.
- **Verdict**: **reject** (wrong direction).

## B11. Squid, tinyproxy (licences only)

- squid-cache/squid: `GPL-2.0` (gh api). **Out.**
- tinyproxy/tinyproxy: `GPL-2.0` (gh api). **Out.**

---

# Ranked shortlist

## Part A (image)
1. **A8, lockfile + Dependabot (npm, docker, github-actions)**: adopt. Least change, closes
   transitive and base-digest pinning, meets the ASF github-actions rule.
2. **A7, llm-agents.nix + nix2container**: spike. All nine harnesses, hash-pinned, one-lock
   bumps; cost is Nix in the build and trust in numtide's daily bot.
3. **A3, Docker kit spec**: borrow credential/allowlist declarations per harness.
4. **A5, apko/melange**: learn-from; too much packaging falls on us.

## Part B (egress)
1. **B1, sandbox-runtime proxy**: borrow the resolved-address guard, decider split,
   inject-only-over-verified-TLS.
2. **B9, iron-proxy**: borrow `upstream_deny_cidrs` and the transform contract; a candidate
   for a MITM'd Copilot tunnel.
3. **B2, Codex network-proxy**: Rust code to lift (domain matcher, MITM hooks) with attribution.
4. **B6, agentgateway**: learn-from for per-provider usage extraction.

Immediate, independent of any adoption: give `connect_or_not_found` a port restriction (443
unless the entry names one) and a resolved-address deny set, dialling the checked address.

# Spikes

All under `~/spikes`, no root. The machine has podman and /dev/kvm, no Docker, no Node.

## Spike A-1: lockfile + Dependabot (needs podman; no Node on host, Node runs in a container)

```sh
mkdir -p ~/spikes/lock && cd ~/spikes/lock
cp -r /home/striker/clones/durable-memory-harness-containers/images/harness/. .
cat > package.json <<'EOF'
{ "private": true, "dependencies": {
  "@anthropic-ai/claude-code": "2.1.296", "@openai/codex": "0.162.1",
  "@google/gemini-cli": "0.63.0", "@github/copilot": "1.0.95", "opencode-ai": "1.19.0",
  "@qwen-code/qwen-code": "0.25.0", "@xai-official/grok": "1.0.50" } }
EOF
podman run --rm -v "$PWD":/w:Z -w /w docker.io/library/node:24-bookworm-slim \
  npm install --package-lock-only --no-fund --no-audit
grep -c '"integrity"' package-lock.json      # transitive pins now present
podman image inspect --format '{{.Digest}}' docker.io/library/ubuntu:24.04 \
  || podman pull docker.io/library/ubuntu:24.04
# edit Dockerfile: FROM ubuntu:24.04@sha256:<digest>; replace the npm -g RUN with
#   COPY package.json package-lock.json /opt/harness/
#   RUN cd /opt/harness && npm ci --no-fund --no-audit && ln -s /opt/harness/node_modules/.bin/* /usr/local/bin/
cp /home/striker/clones/durable-memory-harness-containers/target/release/memory . 2>/dev/null || true
podman build -t harness-lock:spike .
```

Contract points 1 and 2 (held by the runtime; the image must only not get in the way):

```sh
mkdir -p ~/spikes/lock/run/{home,work,memory}; python3 -I -c 'import socket,os;s=socket.socket(socket.AF_UNIX);s.bind(os.path.expanduser("~/spikes/lock/run/egress.sock"));s.listen();input()' &
podman run --rm --network none --userns keep-id --user 1000:1000 --read-only \
  -v ~/spikes/lock/run/home:/home/user:Z -v ~/spikes/lock/run/work:/work:Z \
  -v ~/spikes/lock/run/memory:/memory:ro,Z \
  -v ~/spikes/lock/run/egress.sock:/run/memory/egress.sock \
  harness-lock:spike sh -c 'ip -o link 2>/dev/null; getent hosts api.anthropic.com || echo no-dns;
    curl -sS -m 3 https://api.anthropic.com || echo no-net; test -S /run/memory/egress.sock && echo sock-ok;
    for b in claude codex gemini copilot opencode qwen grok pi muse; do $b --version >/dev/null 2>&1 && echo ok $b || echo FAIL $b; done;
    touch /work/x /home/user/x && ! touch /usr/x 2>/dev/null && echo fs-ok'
```

Then add `.github/dependabot.yml` with `npm` (`directory: /images/harness`, grouped,
`cooldown`), `docker` (`/images/harness`) and `github-actions` (`/`), and delete the bump job.

## Spike A-2: llm-agents.nix + nix2container (needs podman; Nix runs inside `nixos/nix`)

```sh
mkdir -p ~/spikes/nix && cd ~/spikes/nix
cat > flake.nix <<'EOF'
{
  nixConfig = {
    extra-substituters = [ "https://cache.numtide.com" ];
    extra-trusted-public-keys = [ "niks3.numtide.com-1:DTx8wZduET09hRmMtKdQDxNNthLQETkc/yaX7M4qK0g=" ];
  };
  inputs.llm-agents.url = "github:numtide/llm-agents.nix";
  inputs.nixpkgs.follows = "llm-agents/nixpkgs";
  inputs.nix2container.url = "github:nlewo/nix2container";
  outputs = { nixpkgs, llm-agents, nix2container, ... }:
    let s = "x86_64-linux"; pkgs = nixpkgs.legacyPackages.${s};
        a = llm-agents.packages.${s}; n2c = nix2container.packages.${s}.nix2container;
    in {
      packages.${s}.image = n2c.buildImage {
        name = "harness-nix"; maxLayers = 60;
        copyToRoot = pkgs.buildEnv { name = "root"; pathsToLink = [ "/bin" "/etc" "/share" ];
          paths = with a; [ claude-code codex gemini-cli copilot-cli opencode qwen-code grok pi muse-code ]
            ++ (with pkgs; [ bashInteractive coreutils git ripgrep procps less curl cacert python3 nodejs_24 ]); };
        config = { User = "1000:1000"; WorkingDir = "/work";
          Env = [ "HOME=/home/user" "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt" "PATH=/bin" ]; };
      };
    };
}
EOF
podman run --rm -v "$PWD":/src:Z -w /src docker.io/nixos/nix sh -c '
  echo "experimental-features = nix-command flakes" >> /etc/nix/nix.conf
  echo "accept-flake-config = true" >> /etc/nix/nix.conf
  nix run .#image.copyTo -- oci-archive:/src/harness-nix.tar'
podman load -i harness-nix.tar
```

`copyTo` with an `oci-archive:` destination is the nix2container skopeo wrapper; if that
attribute name differs in v1.1.0, use `.#image.copyToPodman` from a Nix with podman access, or
`dockerTools.streamLayeredImage ... | podman load`. The fixed mount points (`/work`,
`/home/user`, `/run/memory`, `/opt/memory`, `/memory`) do not exist in a Nix image; add them
with a `pkgs.runCommand` that `mkdir`s them into `copyToRoot`. Then run the same contract
check as A-1 against `localhost/harness-nix`. Record: build time cold/warm, image size,
`podman image inspect --format '{{.Digest}}'` on two builds from the same `flake.lock`
(should match), and whether each harness starts without `/usr/bin/env` and friends.

## Spike B-1: `srt proxy` with our socket (needs Node 22.12+, installed into the spike dir)

```sh
mkdir -p ~/spikes/srt && cd ~/spikes/srt
curl -fsSL https://nodejs.org/dist/v24.21.0/node-v24.21.0-linux-x64.tar.xz | tar -xJ --strip-components=1 -C .
./bin/npm install --prefix . @anthropic-ai/sandbox-runtime@0.0.80
openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.crt -days 1 -subj /CN=spike-ca \
  -addext basicConstraints=critical,CA:TRUE -addext keyUsage=critical,keyCertSign
cat ca.key ca.crt > ca.pem
```

A Python driver (written outside the download dir, run with `-I`) binds
`~/spikes/srt/egress.sock`, makes a socketpair for the decider and a pipe for the lifeline,
and execs `./bin/node node_modules/@anthropic-ai/sandbox-runtime/dist/cli.js proxy
--listen-fd 3 --decider-fd 4 --lifeline-fd 5 --ca-fd 6` with those fds passed; the decider
answers the hello with `allowedDomains: ["api.github.com"]` and allows every `req` while
adding `authorization: Bearer test` (to see injection). Point 1 test:

```sh
podman run --rm --network none -v ~/spikes/srt/egress.sock:/s.sock -v ~/spikes/srt/ca.crt:/ca.crt:ro \
  docker.io/curlimages/curl sh -c '
  curl -sS -m 5 https://example.com || echo direct-blocked
  curl -sS -m 10 --unix-socket /s.sock -p -x http://proxy https://api.github.com/zen --cacert /ca.crt && echo via-socket-ok
  curl -sS -m 10 --unix-socket /s.sock -p -x http://proxy https://example.com --cacert /ca.crt || echo denied-ok'
```

(If curl's `--unix-socket` does not apply to the proxy hop, put `socat
TCP-LISTEN:3128,fork UNIX-CONNECT:/s.sock &` in the container first, mirroring `amem relay`.)
Point 2 is not this tool's job. Check also: a CONNECT to an allowed name resolving to
127.0.0.1 (via `/etc/hosts` in the proxy's environment) is refused.

## Spike B-2: iron-proxy tunnel listener (no Node; single Go binary)

```sh
mkdir -p ~/spikes/iron && cd ~/spikes/iron
curl -fsSLO https://github.com/ironsh/iron-proxy/releases/download/v0.53.0/iron-proxy_0.53.0_linux_amd64.tar.gz
curl -fsSLO https://github.com/ironsh/iron-proxy/releases/download/v0.53.0/checksums.txt
sha256sum --ignore-missing -c checksums.txt && mkdir -p bin && tar -xzf iron-proxy_0.53.0_linux_amd64.tar.gz -C bin
openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.crt -days 1 -subj /CN=spike-ca \
  -addext basicConstraints=critical,CA:TRUE -addext keyUsage=critical,keyCertSign
cat > proxy.yaml <<'EOF'
proxy: { http_listen: "127.0.0.1:18080", https_listen: "127.0.0.1:18443", tunnel_listen: "127.0.0.1:11080" }
tls: { mode: mitm, ca_cert: ./ca.crt, ca_key: ./ca.key }
transforms:
  - name: allowlist
    config: { domains: [ "api.github.com" ] }
EOF
./bin/iron-proxy -config proxy.yaml &        # the dns block is omitted; check it starts without one
socat UNIX-LISTEN:$HOME/spikes/iron/egress.sock,fork TCP:127.0.0.1:11080 &
podman run --rm --network none -v ~/spikes/iron/egress.sock:/s.sock -v ~/spikes/iron/ca.crt:/ca.crt:ro \
  docker.io/alpine sh -c 'apk add -q curl socat 2>/dev/null || true; ...'
```

The container has no network, so `apk add` fails: use an image that already carries curl and
socat (or bind-mount a static socat), start `socat TCP-LISTEN:3128,fork UNIX-CONNECT:/s.sock`
inside, then run the same three curl lines as B-1 with `-x http://127.0.0.1:3128`. Also test
the IMDS/loopback refusal: add a `127.0.0.1 api.github.com` hosts entry for the proxy process
and confirm the CONNECT is refused by `upstream_deny_cidrs`. Point 2 is not this tool's job.
