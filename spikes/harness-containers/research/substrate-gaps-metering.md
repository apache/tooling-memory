# Substrate, gaps in the four reports, and who meters tokens

Research date 20 October 2026. Licences verified with `gh api repos/O/R/license --jq .license.spdx_id`, plus the LICENSE file where that returned NOASSERTION or the repo is open-core. Maintenance comes from `gh api repos/O/R --jq '{pushed_at,archived,stargazers_count}'` and the latest release. Nothing was written in the repository. Parts 2 and 3 drew on three research passes whose working files are in `part2/`. The surprising claims were spot-checked: licence, push date and stars for OpenShell, sandlock, agentsh, coop, agent-safehouse, nvx, matchlock, Busbar, Agent Vault, Bifrost, AISIX, OpenAFW, Claw Patrol, TensorZero (archived) and Portkey (last push 2026-05-25); agentsh's `ExtractSSEUsage`; and agentgateway's Substrate PEP README.

Contents: Part 1, "substrate"; Part 2a, new sandboxes and launchers; Part 2b, new egress, LLM and MCP gateways; Part 3, the capability matrix; Appendix A, matrix evidence for the candidates the four reports already covered.

# Part 1: "Is substrate a candidate?"

Searched 20 October 2026: `gh search repos` for "substrate agent", "substrate sandbox", "substrate llm",
"substrate mcp", and `--match name substrate` (updated after 2026-03-01, blockchain/Polkadot and
chip-design hits filtered), plus web search for "Substrate agent sandbox", "Substrate MCP" and
"substrate.run". Every project named Substrate in or near the agent sandbox / harness /
infrastructure / observability space:

| Project | What it is | Licence (verified) | Stars, last push | In this space? |
|---|---|---|---|---|
| **agent-substrate/substrate** ("Agent Substrate") | Kubernetes runtime for sandboxed agents (gVisor or Cloud Hypervisor), suspend/resume, egress gateway with credential injection | Apache-2.0 | 5593, 2026-10-19 | Yes: the match |
| agent-substrate/env | Environment API on top of Agent Substrate: exec, files, built-in MCP tools per actor | Apache-2.0 | 69, 2026-10-19 | Add-on to the above |
| ai-on-gke/substrate-gke | GKE installer for Agent Substrate | Apache-2.0 | 49, 2026-10-16 | Packaging |
| agent-substrate/always-on-agent | OpenClaw on Agent Substrate | Apache-2.0 | 25, 2026-09-25 | Demo |
| wolfiesch/pi-agent-substrate | Pi / Oh My Pi extension that runs tools in a remote Agent Substrate workspace | Apache-2.0 | 10, 2026-09-21 | Harness adapter |
| taeold/pi-durable-substrate | Pi durable agent deployed on Agent Substrate | none (404 on licence) | 13, 2026-10-06 | Demo; no licence |
| Roasbeef/substrate ("Subtrate") | Mail/messaging and hooks between several Claude Code agents | none (no licence in API) | 14, 2026-10-02 | Coordination, not confinement; no licence: out |
| nexus-substrate/nexus-agents | "Autonomic control plane" over Claude Code/Codex/Gemini/OpenCode: review gates, hash-chained audit | MIT | 23, 2026-10-19 | Orchestration, not confinement |
| damiansire/cognitive-substrate | File-first Gemini task runner | MIT | 28, 2026-10-06 | An agent, not infrastructure |
| Garmo Labs SUBSTRATE (`substrate-mcp` on npm) | Hosted "persistent AI entity" MCP service | proprietary (per its listing) | n/a | Out |
| substrate.run | Former hosted AI-API company; the domain now 301-redirects to zo.computer | n/a | n/a | Out, hosted |
| substratusai (Substratus) | LLM fine-tuning on K8s; `substratusai/runbooks` archived 2024-08-28 | NOASSERTION | 169 | Dead |
| paritytech Substrate and MCP servers for it (ThomasMarches/substrate-mcp-rs MIT, Moonsong-Labs/substrate-mcp Apache-2.0) | Blockchain framework | n/a | n/a | Name collision only |

**Which one the user means.** Almost certainly **Agent Substrate** (agent-substrate/substrate). It is the
only project of that name in agent infrastructure with any weight: 5593 stars five months after
creation (repo created 2026-05-13), released weekly (v0.3.0 2026-09-30, v0.4.0 2026-10-08, v0.5.0
2026-10-14), Google-originated, CNCF Sandbox vote passed (cncf/sandbox#523: labels `gitvote/passed`,
`contribution-agreement/unsigned`, issue still open, so onboarding is not finished), and covered in
the trade press (The New Stack, Solo.io) as the runtime under kagent and Google's Agent Executor
(google/ax, Apache-2.0, 13985 stars). Its README names Claude Code and Codex as workloads and the
repo ships `demos/claude-code-multiplex`. None of the four layer reports covers it.

## Agent Substrate (agent-substrate/substrate)

- **Repo / licence (verified)**: https://github.com/agent-substrate/substrate; `Apache-2.0`;
  `gh api repos/agent-substrate/substrate/license --jq '.license.spdx_id'` -> `Apache-2.0`. Source
  headers read "Copyright 2026 Google LLC ... Apache License, Version 2.0"
  (`internal/egresspolicy/egresspolicy.go`). Third-party licences vendored under `_LICENSES/`.
- **Form**: a Go control plane and data plane for Kubernetes: `ateapi` (gRPC API, PostgreSQL state),
  `atecontroller`, `atelet` (per node), `ateworker-gvisor` / `ateworker-microvm` (worker pods),
  `atenet` (Envoy router + ext_proc for ingress, `atenet-egress` gateway), `credential-provider`,
  `kubectl-ate` CLI. Needs a Kubernetes cluster, an object store for snapshots (GCS, or `rustfs` on
  kind), PostgreSQL. Dev quickstart is kind with Docker (`hack/create-kind-cluster.sh` calls
  `docker` directly; no podman path). microVM workers need `/dev/kvm` in the node.
- **Platforms**: Linux nodes only (gVisor or Cloud Hypervisor). macOS and Windows only as a
  kubectl client or through a Docker VM running kind. Not a laptop-native launcher.
- **Mechanism**: an "actor" (an OCI workload) runs in a gVisor or Cloud Hypervisor sandbox inside a
  pre-warmed worker pod; idle actors are snapshotted (RAM + filesystem) to object storage and
  restored on any worker in under ~400 ms (README). All actor TCP egress is nftables-REDIRECTed to
  `atunnel`, which opens an mTLS CONNECT to an egress policy enforcement point carrying a
  per-actor X.509 identity; the PEP applies the actor's `EgressPolicy` (host/CIDR rules, DNS filtered
  to policy hosts, optional TLS termination with `replace_headers` credential injection from a
  credential provider). Other UDP and protocols are dropped (`docs/egress-traffic.md`,
  `docs/network-egress.md`).
- **Against our contract**:
  1. *Partial, differently.* The actor has a private network stack whose only exit is the PEP
     (fail-closed; `docs/security.md` "Fail-Closed Policy-Enforced Egress Gateway"). That is our
     shape at cluster scale, but the route out is a transparent TCP redirect into an mTLS CONNECT
     tunnel to a network PEP, not one unix socket bind-mounted in. Plain CONNECT and WebSocket from
     the actor are refused (`docs/egress-traffic.md`). The PEP is "fully replaceable"
     (README; contract in `docs/network-egress.md`), so our egress could in principle be the PEP,
     but it would have to speak mTLS CONNECT with the `ActorIdentity` certificate extension and
     check the actor against the ate API, and the harness would need a base URL that resolves to
     a policy-allowed host.
  2. *Breaks for a laptop.* Volumes are `durableDir`, `external` CSI (preview flag), or `systemInfo`
     (`docs/api-guide.md` line ~104). No hostPath: the host working tree, our memory binary and the
     record projection cannot be bind-mounted from a laptop at fixed paths; they would be in the
     image or on a CSI volume (e.g. NFS).
  3. Replaces namespaces with gVisor or a microVM; needs Kubernetes, a daemon set, PostgreSQL and
     an object store. Breaks "no daemon, no container runtime".
  4. macOS: no backend (Linux nodes only). 5. Windows: none. 6. Egress: overlaps allowlist,
     credential injection, policy hook; no token metering (`gh search code --repo
     agent-substrate/substrate input_tokens` and `"token usage"`: no hits), no label concept, no MCP
     in the gateway.
- **Maintenance**: pushed 2026-10-19; v0.5.0 2026-10-14; not archived; pre-1.0, "no guarantees about
  backward compatibility" (README). Weekly community meeting; CNCF Slack channels.
- **If we adopted it**: it would replace `amem launch` only for a fleet deployment (PLAN.md's "sidecar
  the platform launches per workload"), not on a developer laptop. It adds Kubernetes, Envoy,
  PostgreSQL, object storage, a CA hierarchy, and Google's snapshot machinery. Our egress could run
  as (or behind) its PEP, keeping metering, label check and `/mcp`; its `EgressPolicy` and
  credential provider would then duplicate our grants.
- **Verdict**: **learn-from** now; a **target platform** later. Not a replacement for
  `crates/sandbox` (Kubernetes-only, no host binds), but the most credible place for a hosted
  deployment of `amem`: write our egress so it can sit behind or act as a Substrate PEP. Borrow:
  the fail-closed PEP contract, DNS answered only for policy hosts, and the "PEP must not trust the
  actor's Host/SNI; route to the authorized name" rule (`docs/network-egress.md`, Trust Boundary).

**Answer to "is substrate a candidate?"**: Agent Substrate is a credible, permissively licensed,
fast-moving candidate for the *platform* layer when amem runs on Kubernetes, and a natural home
for our egress as its pluggable PEP. It is not a candidate for the laptop launcher: it needs a
cluster, cannot bind host paths, and has no macOS or Windows backend. The other Substrates are
either unlicensed, hosted, an orchestration layer above the harness, or a name collision.

---

# Part 2a: agent sandboxes and sandboxed harness launchers not in the four reports


Researched 20 Oct 2026. Every licence below was read with `gh api repos/O/R/license --jq .license.spdx_id`
(and the LICENSE file where that said NOASSERTION or the repo is open-core). Maintenance figures come from
`gh api repos/O/R --jq '{pushed_at,archived,stargazers_count}'` and `gh api repos/O/R/releases --jq '.[0]'`,
all run on 20 Oct 2026. Docs quoted are the files in each repo's default branch as fetched that day.

Sources searched: GitHub topics agent-sandbox, ai-sandbox, llm-sandbox, code-sandbox, microvm,
claude-code-sandbox, sandbox-ai; `gh search repos` for "sandbox claude code", "sandbox coding agent",
"agent jail", "claude code devcontainer", "awesome agent sandbox"; awesome-lists arjan/awesome-agent-sandboxes,
dloss/awesome-agent-sandboxes, fishman/awesome-agent-sandbox, msyvr/awesome-agent-sandboxes; HN Algolia
(2026 stories: "agent sandbox", "sandbox coding agents", "microvm agents", "sandbox codex").

Host facts relevant to the spikes (read on this machine): kernel 6.18.40.1-microsoft-standard-WSL2;
`/proc/config.gz` has `CONFIG_SECURITY_LANDLOCK=y` and `CONFIG_LSM="landlock,..."`; `podman version 4.9.3`;
`podman.socket` user unit active; `/dev/kvm` is `crw-rw---- root kvm` and user `striker` is NOT in group
`kvm` (`id -nG`), so every KVM-backed candidate needs a one-time root step (`usermod -aG kvm`) first.

---

### 1. NVIDIA OpenShell

- **Repo / licence (verified)**: https://github.com/NVIDIA/OpenShell ; Apache-2.0 ; `gh api repos/NVIDIA/OpenShell/license` -> `Apache-2.0`; files carry `SPDX-License-Identifier: Apache-2.0`.
- **Form**: Rust workspace (~40 crates). `openshell` CLI (static musl), `openshell-gateway` (control plane daemon, SQLite or Postgres), `openshell-supervisor` (trusted, per sandbox), `openshell-sandbox` (static musl, inside the workload), optional `openshell-prover`. Needs Docker 28+, Podman 5.x, Kubernetes, or KVM/Hypervisor.framework for the VM driver (docs/about/support-matrix.mdx). No root for the Podman (rootless) path; installer script uses sudo for packages but tarballs are on the release.
- **Platforms**: Linux yes (Debian/Ubuntu amd64/arm64 supported); macOS yes (Apple Silicon, via Docker Desktop or VM driver); Windows partial (WSL 2 + Docker Desktop "Experimental"; native `openshell-driver-mxc` crate on Microsoft MXC AppContainer, runtimes doc says "Coming soon").
- **Mechanism**: The compute driver creates a workload with no network (Docker/Podman `network=none`; VM guest has no NIC) and a separate supervisor; they talk over one mTLS HTTP/2 connection on a unix socket on a driver-owned volume (vsock for VM). Inside, `openshell-sandbox` applies Landlock (ABI >= 3) and seccomp user-notification, intercepts every TCP open and DNS query and hands it to the supervisor, which checks policy (host/port/path/method/calling binary), injects credentials, and opens the real connection (docs/about/architecture.mdx).
- **Against our contract**:
  - 1 (no network, one socket): holds in shape, almost exactly ours: workload `network=none`, "the mediated channel to the supervisor is the workload's only allowed egress path" (architecture.mdx). But the socket carries OpenShell's own Sandbox Protocol to its supervisor, not our egress; our egress would sit behind their supervisor as an allowed upstream, or replace it. Whether a bind-mounted unix socket of ours is reachable from inside past their Landlock/seccomp layer is unverified (spike).
  - 2 (fixed-path bind mounts): partial. Podman/Docker drivers accept `bind` mounts with `source`, `target`, `read_only` via `--driver-config-json`, but only with `allow_driver_config = true`, `enable_bind_mounts = true` and resource admission disabled; targets cannot replace the workspace root or `/opt/openshell`, `/etc/openshell`, `/run/openshell` (runtimes.mdx "Docker Mounts", "Podman Mounts"). The VM driver allows at most three mounts on an x86_64 host and no single-file sources (runtimes.mdx "MicroVM Mounts"), which is short of our five (home, work, memory binary, record, socket).
  - 3: breaks (container runtime or VM, a gateway daemon, a supervisor container).
  - 4: breaks (macOS goes through Docker Desktop's Linux VM or libkrun, not Seatbelt).
  - 5: partial: WSL2 experimental; MXC driver is a different Windows primitive from wslc.
  - 6: overlaps heavily: credential injection, L7 allowlist, MCP-aware L7, middleware hook; no token metering.
- **Maintenance**: pushed 2026-10-19, not archived, 16,715 stars; latest stable v0.1.4 published 2026-10-13 (weekly cadence per support-matrix.mdx). Anonymous telemetry on by default, opt-out `OPENSHELL_TELEMETRY_ENABLED=false` (README).
- **If we adopted it**: would replace crates/sandbox Linux backend and much of crates/egress (allowlist, injection, CONNECT). Adds a gateway daemon with a database, a supervisor container per session, Podman 5 or KVM, gRPC/JWT/mTLS machinery, and a much larger attack surface; our memory server and token metering would become a middleware or an upstream.
- **Verdict**: learn-from (strong). Same architecture as ours at a much larger scale; borrow the supervisor/middleware split and the seccomp-notify TCP interception idea; adopting means giving up the no-daemon contract.
- **Matrix**: token no (only an unmapped `input_tokens` in an OCSF test fixture, crates/openshell-ocsf/src/builders/api_activity.rs:166); stream no; attrib partial (decisions and OCSF events are per sandbox and per calling binary, docs/how-it-works/policies/network-rules.mdx "Binary Matching"; no usage); cost no; budget no (only gateway-wide gRPC rate limit, crates/openshell-core/src/config.rs:319-331); inject yes (docs/how-it-works/inference.mdx: placeholder in env, real credential substituted only at authorized endpoints); allow yes, default deny (architecture.mdx "Outer network fence"; network-rules.mdx); hook yes (supervisor middleware, gRPC allow/deny/change before injection, fail-closed, docs/extensibility/supervisor-middleware/index.mdx, proto/supervisor_middleware.proto); otel yes (OTLP traces and logs, crates/openshell-otel/src/lib.rs; Prometheus on gateway, docs/observability/gateway-metrics.mdx); mcp yes (MCP Streamable HTTP revision allowlist and request validation, crates/openshell-supervisor-network/src/l7/mcp.rs). No paid tier found.

### 2. multikernel/sandlock

- **Repo / licence (verified)**: https://github.com/multikernel/sandlock ; Apache-2.0 ; `gh api repos/multikernel/sandlock/license` -> `Apache-2.0`.
- **Form**: Rust library (`sandlock-core`, on crates.io as 0.9.0, 2026-10-13) plus CLI, C FFI, Python/Go SDKs, OCI shim. No root, no cgroups, no containers (README). Linux 6.12+ for full feature set.
- **Platforms**: Linux yes; macOS no; Windows no.
- **Mechanism**: Landlock (filesystem, TCP ports, IPC scoping) plus seccomp-bpf plus a seccomp user-notification supervisor that enforces IP/host rules, resource limits, `/proc` virtualisation, a COW filesystem, and a transparent HTTP proxy for method/host/path ACLs with MITM and credential injection (README; docs/network.md). No namespaces.
- **Against our contract**:
  - 1: partial. Default with no `--net-allow` and no HTTP flags: "Landlock denies every TCP connect(), UDP/ICMP/raw socket creation are denied at the seccomp layer" (docs/network.md "Defaults"). AF_UNIX is governed only by `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET` (docs/network.md), so a pathname socket we expose should be connectable; untested. There is no separate network namespace, so the harness shares the host's interfaces (blocked by policy, not absent), and loopback TCP connect to our in-box relay port would need `--net-allow 127.0.0.1:PORT` which is also reachable host loopback.
  - 2: partial. `--chroot ROOTFS --fs-mount /work:/host/path` maps virtual paths inside a chroot, read-only variant via `:ro` in the TOML `mount` list (docs/sandbox-reference.md, `fs_mount`/`fs_mount_ro`; docs/cli.md:138). This is seccomp path virtualisation, not a mount namespace.
  - 3: holds the "unprivileged, no daemon, no bwrap, no runtime" half but not the namespace half; different (weaker for network, LSM-based) isolation model.
  - 4, 5: silent (Linux only).
  - 6: overlaps (HTTP ACL, credential injection).
- **Maintenance**: pushed 2026-10-19, not archived, 574 stars; latest release v0.9.0 2026-10-13.
- **If we adopted it**: could replace or layer under crates/sandbox/src/linux.rs as a dependency crate (its `unsafe` lives in the dependency). Adds the seccomp-notify supervisor thread in our process, an optional in-process MITM proxy we would leave off. Weaker than our netns for "no network": policy-denied rather than no interface.
- **Verdict**: borrow / spike. Best candidate to harden our Linux backend in depth (Landlock + seccomp inside our namespaces); not a replacement for the netns guarantee.
- **Matrix**: token no; stream no; attrib n/a (per process tree, no usage); cost no; budget no (memory/process/time limits only, README); inject yes (`--credential NAME=SOURCE`, bearer/basic/header, requires HTTP ACL plus MITM CA, docs/sandbox-reference.md "Credential injection"); allow yes, default deny (docs/network.md "Defaults"); hook yes (`policy_fn` callback sees connect/sendto/bind destinations and execve argv and returns allow/deny/audit, docs/policy-fn.md); otel no (no hit for opentelemetry/prometheus in code search); mcp no.

### 3. jingkaihe/matchlock

- **Repo / licence (verified)**: https://github.com/jingkaihe/matchlock ; MIT ; `gh api repos/jingkaihe/matchlock/license` -> `MIT`.
- **Form**: Go CLI plus Go/Python/TypeScript SDKs. One-time `sudo matchlock setup linux` (nftables, sysctl file, file capabilities, kvm/netdev group, /dev/net/tun) per docs/linux-packaging.md; afterwards "NEVER EVER run matchlock with sudo" (AGENTS.md). "Experimental" (README).
- **Platforms**: Linux yes (KVM, Firecracker); macOS yes (Apple Silicon, Virtualization.framework); Windows no.
- **Mechanism**: Each run boots a Firecracker microVM from an OCI image; host-guest control over vsock; when `--allow-host` or `--secret` is passed the network is sealed and traffic goes through a host nftables transparent proxy with HTTP/TLS MITM that swaps placeholder secrets for real ones (README; AGENTS.md). `--no-network` gives an offline VM.
- **Against our contract**:
  - 1: partial. Offline VM (`--no-network`) or allowlisted MITM path; no documented way to hand the guest a single unix socket to our egress (vsock is its own control channel).
  - 2: partial. `/workspace` via FUSE and "volume overlay mounts are isolated snapshots" (README); arbitrary host binds at fixed paths not documented in what was read.
  - 3: breaks (Firecracker, root setup, nftables).
  - 4: breaks (VM rather than Seatbelt).
  - 5: no Windows.
  - 6: overlaps: allowlist, secret injection, request/response hooks.
- **Maintenance**: pushed 2026-10-04, not archived, 625 stars; latest release v0.2.17 2026-07-26.
- **If we adopted it**: replaces sandbox and some egress; adds Firecracker, root-installed nftables setup, Go toolchain.
- **Verdict**: learn-from. Clean statement of the placeholder-secret model and SSE-aware body hooks; root setup and VM-only rule it out as a drop-in.
- **Matrix**: token no; stream no for metering (hooks transform SSE `data:` lines only, docs/network-interception.md "SSE Behavior"); attrib n/a; cost no; budget no; inject yes (`--secret NAME@host`, README); allow yes (`--allow-host`, runtime `allow-list add/delete`, README); hook yes (before/after rules allow/block/mutate and SDK callback hooks, docs/network-interception.md); otel no (code search empty); mcp no.

### 4. gominimal/minimal

- **Repo / licence (verified)**: https://github.com/gominimal/minimal ; Apache-2.0 ; `gh api repos/gominimal/minimal/license` -> `Apache-2.0` (repo also has CLA files under legal/).
- **Form**: Rust workspace: `min` CLI, `minimald` session daemon (SSH over a unix socket), `minvmd` libkrun microVM daemon, plus a package manager/registry. No root on Linux (unprivileged user namespaces; Ubuntu 24.04 needs an AppArmor profile, docs/reference/linux-host-setup.md).
- **Platforms**: Linux yes (Ubuntu/Debian amd64/arm64, kernel >= 5.10); macOS yes (always libkrun VM); Windows no.
- **Mechanism**: Sessions run in a sandbox composed of user and other Linux namespaces built on hakoniwa (crates/sandbox/src/network.rs references `hakoniwa::Child::rustslirp_tapfd`); network modes NoNet / HostNet / OwnIp (gvproxy switch per host) with allow/deny rules (docs/specs/03-spec-networking/architecture.md). Packages agents claude, codex, opencode, pi (README).
- **Against our contract**:
  - 1: partial. `NoNet` = empty netns, our shape; whether a host unix socket can be passed in is not documented in what was read; their own model uses gvproxy and SSH over UDS.
  - 2: partial. Rootfs assembled from the package closure, repo files, `/state`, "pinhole filesystem mappings" (docs/concepts/sandboxing.md); arbitrary host binds at our fixed paths not documented.
  - 3: holds the userns approach but adds two daemons (minimald, minvmd).
  - 4: breaks (macOS uses a libkrun VM, not Seatbelt).
  - 5: no Windows.
  - 6: silent on model-provider metering.
- **Maintenance**: pushed 2026-10-19, not archived, 217 stars; latest release `release-d67d78ee` 2026-10-19 (rolling).
- **If we adopted it**: would replace crates/sandbox Linux backend with a daemon plus their package registry; large.
- **Verdict**: learn-from. Closest sibling to our Linux design in Rust; worth reading its hakoniwa-based sandbox crate (crates/sandbox/src/lib.rs, ~340 KB) and NetworkMode split.
- **Matrix**: token no; stream no; attrib n/a; cost no; budget no; inject no evidence (README describes keeping credentials out, not injecting); allow partial (per-PTask egress/ingress policy and DNS in OwnIp mode, docs/specs/03-spec-networking/architecture.md); hook no; otel no (code search empty); mcp no.

### 5. trailofbits/coop

- **Repo / licence (verified)**: https://github.com/trailofbits/coop ; Apache-2.0 ; `gh api repos/trailofbits/coop/license` -> `Apache-2.0`.
- **Form**: Rust CLI `coop` plus separate `coop-proxy` binary. Linux needs `sudo` for `ip`, `bridge`, `iptables`, `sysctl` and runs Firecracker; macOS uses Lima with no sudo (docs/backends.md, docs/trust-model.md).
- **Platforms**: Linux yes (x86_64 tested, Firecracker/KVM); macOS yes (Lima, Virtualization.framework); Windows no.
- **Mechanism**: One disposable VM per project with Claude Code, Codex or Grok inside; guest gets passwordless sudo and outbound network via TAP/NAT; credentials go through `coop-proxy`, a reverse proxy that checks a per-instance capability token, strips the guest's `authorization`/`x-api-key`, injects the real credential, pins the upstream host, forwards only a provider-specific method/path allowlist, and confines itself with Landlock (coop-proxy/src/proxy.rs header, coop-proxy/src/jail.rs header).
- **Against our contract**:
  - 1: breaks (guest has outbound network; "the guest has outbound network access", docs/claude-integration.md).
  - 2: partial (Lima virtiofs live mounts at chosen paths; Firecracker syncs with rsync/tar, docs/backends.md, docs/ARCHITECTURE.md).
  - 3: breaks (sudo, Firecracker, iptables).
  - 4: breaks (Lima VM).
  - 5: no Windows.
  - 6: coop-proxy overlaps our credential injection and per-provider path allowlist.
- **Maintenance**: pushed 2026-10-19, not archived, 792 stars; latest release v0.8.1 2026-10-13.
- **If we adopted it**: not as a sandbox. coop-proxy is ~800 lines (coop-proxy/src/proxy.rs) of reviewed Rust.
- **Verdict**: borrow (coop-proxy ideas with attribution: capability-token auth of the guest, header strip/inject, default-deny provider operation list, proxy self-confinement with Landlock).
- **Matrix**: token no (no usage parsing in coop-proxy/src/proxy.rs); stream partial (streams bodies through, no metering); attrib partial (per instance capability token); cost no; budget no; inject yes (coop-proxy/src/proxy.rs:1-20); allow partial (fixed upstream plus method/path allowlist for the model API only; general guest egress is open); hook no; otel no; mcp no.

### 6. microsoft/nvx

- **Repo / licence (verified)**: https://github.com/microsoft/nvx ; MIT ; `gh api repos/microsoft/nvx/license` -> `MIT`.
- **Form**: microVM sandbox on an OpenVMM fork; Python driver `scripts/nvx.py` plus Rust crates `nvx-rs` and `aci_edge_sandboxes`. Needs read/write on `/dev/kvm` (Linux), `/dev/mshv`, or Windows Hypervisor Platform. No daemon in what was read.
- **Platforms**: Linux yes (KVM, MSHV); macOS no; Windows yes (WHP).
- **Mechanism**: Boots a Linux guest with its own kernel per sandbox; networking only with `--net ... --network-profile portable`, egress default allow but CIDR allow/deny rules and policy files; ingress default deny; host directories or single files exposed at absolute guest targets ro/rw with deny/allow/write carve-outs (doc/run.md, doc/usage.md, nvx-rs/README.md).
- **Against our contract**:
  - 1: partial. Network is off unless requested; `--host-loopback-forward PROTOCOL:HOST_PORT:GUEST_PORT` publishes one host localhost TCP port to the guest (doc/usage.md), which could carry our egress over TCP rather than a unix socket; whether that works without `--net` is not stated.
  - 2: holds: `--mount GUEST_TARGET,HOST_PATH[,ro|rw]`, including single files (doc/usage.md).
  - 3: breaks (VM, KVM access).
  - 4: no macOS.
  - 5: interesting: same tool on Windows WHP and Linux KVM, an alternative to wslc for M8.
  - 6: L3/L4 only; no L7.
- **Maintenance**: created 2026-08-27; pushed 2026-10-19, not archived, 462 stars; only dev prereleases (latest `v0.1.0-dev.f7e8e3e46859` 2026-10-19; no non-prerelease release).
- **If we adopted it**: a Windows/Linux VM backend replacing wslc and possibly the Linux namespaces; adds OpenVMM, a guest kernel and rootfs, Python driver.
- **Verdict**: learn-from, revisit for Windows when it has a stable release.
- **Matrix**: token n/a; stream n/a; attrib n/a; cost n/a; budget n/a; inject no; allow yes at L3/L4 (`--network-egress deny` plus `--network-egress-allow CIDR:tcp:port`, doc/run.md); hook no; otel no; mcp no.

### 7. eugene1g/agent-safehouse

- **Repo / licence (verified)**: https://github.com/eugene1g/agent-safehouse ; Apache-2.0 ; `gh api repos/eugene1g/agent-safehouse/license` -> `Apache-2.0`.
- **Form**: single shell script `safehouse.sh` plus composable `.sb` profile modules; no privileges.
- **Platforms**: Linux no; macOS yes; Windows no.
- **Mechanism**: Assembles a deny-first Seatbelt profile from modules (profiles/00-base.sb ... profiles/60-agents/*.sb) and runs the agent under `sandbox-exec`.
- **Against our contract**:
  - 1: breaks by design: profiles/20-network.sb allows `network-outbound (remote ip)` and states "blocking exfiltration/C2 is explicitly NOT a goal". Unix sockets excluded by default.
  - 2: n/a on macOS (Seatbelt grants paths, cannot relocate them).
  - 4: directly relevant: per-harness file rules for claude-code, codex, gemini, copilot-cli, opencode, pi, amp, goose and others in profiles/60-agents/, plus toolchain modules (node.sb etc.).
- **Maintenance**: pushed 2026-10-19, not archived, 2,113 stars; latest release v0.12.0 2026-09-07.
- **If we adopted it**: nothing replaced wholesale; its harness and Node toolchain rules would seed crates/sandbox/src/seatbelt.rs with tested paths, with our own network section (deny all, allow only our socket).
- **Verdict**: borrow (per-harness and toolchain Seatbelt rules, Apache-2.0, attribute).
- **Matrix**: all columns n/a or no (no proxy); allow no (outbound allowed, profiles/20-network.sb).

### 8. fencesandbox/fence (formerly Use-Tusk/fence)

- **Repo / licence (verified)**: https://github.com/fencesandbox/fence (Use-Tusk/fence redirects here) ; Apache-2.0 ; `gh api repos/fencesandbox/fence/license` -> `Apache-2.0`.
- **Form**: Go single binary and Go library (`pkg/fence`, docs/library.md). Linux needs bubblewrap and socat; bpftrace optional. No root.
- **Platforms**: Linux yes; macOS yes; Windows partial (Nix on WSL only, README).
- **Mechanism**: "Inspired by Anthropic's sandbox-runtime" (README:214). Linux: bubblewrap with netns plus Landlock and seccomp; macOS: generated `sandbox-exec` profile; network via in-process HTTP and SOCKS5 filtering proxies bridged with socat; optional `upstreamProxy` for grey-zone traffic (ARCHITECTURE.md, docs/configuration.md).
- **Against our contract**:
  - 1: partial: default deny with proxy-only egress; their proxy, not ours, though `upstreamProxy` can chain to an HTTP proxy.
  - 2: partial (allowWrite/denyRead path rules on the host layout; no relocation to fixed paths).
  - 3: breaks (bubblewrap).
  - 4: relevant: Seatbelt profile generator (internal sandbox macos.go) with proxy-only outbound and `allowUnixSockets`.
- **Maintenance**: pushed 2026-10-14, not archived, 995 stars; latest release v0.1.67 2026-09-01.
- **If we adopted it**: adds Go binary, bwrap, socat. Essentially srt (covered) in Go.
- **Verdict**: learn-from (Seatbelt proxy-only and unix-socket allowlist rules for point 4).
- **Matrix**: token no; stream no; attrib no; cost no; budget no; inject no (no injection in docs/configuration.md); allow yes (`network.allowedDomains`, default deny, README); hook partial (`upstreamProxy` chaining to mitmproxy for grey-zone traffic, docs/configuration.md); otel no; mcp no.

### 9. GreyhavenHQ/greywall (+ greyproxy)

- **Repo / licence (verified)**: https://github.com/GreyhavenHQ/greywall ; Apache-2.0 (`gh api .../greywall/license` -> `Apache-2.0`); "a fork of Fence" (README:300). https://github.com/GreyhavenHQ/greyproxy ; MIT (`gh api .../greyproxy/license` -> `MIT`); a fork of GOST v3 (greyproxy README:166).
- **Form**: Go binary `greywall` plus `greyproxy` daemon (HTTP/SOCKS5/DNS proxy with web dashboard and REST API). Linux deps bubblewrap, socat. No root.
- **Platforms**: Linux yes; macOS yes (no transparent capture or DNS capture on macOS, README table); Windows no.
- **Mechanism**: Fence-derived; on Linux adds tun2socks TUN capture inside `--unshare-net`, eBPF violation monitoring, learning mode that generates profiles; all traffic to greyproxy at localhost:43052 with DNS at :43053 (README).
- **Against our contract**: 1 partial (forced proxy via TUN, but to their proxy); 2 partial (host paths); 3 breaks (bubblewrap); 4 Seatbelt-based; 6 overlaps only on allow/deny.
- **Maintenance**: greywall pushed 2026-10-17, 310 stars, release v0.3.7 2026-06-01; greyproxy pushed 2026-06-02, 47 stars, release v0.4.5 2026-06-02 (older than 3 months). Neither archived.
- **If we adopted it**: adds bwrap, socat, a proxy daemon with a dashboard.
- **Verdict**: learn-from (learning mode for profile generation; TUN capture as a fallback for proxy-unaware harnesses).
- **Matrix**: token no; stream no; attrib partial (greyproxy rules match "container, destination, and port", greyproxy README); cost no; budget no; inject partial (keyring injection for gh/glab only, README Linux deps); allow yes; hook partial (interactive pending-request approval in dashboard, greyproxy README); otel no; mcp no.

### 10. canyonroad/agentsh

- **Repo / licence (verified)**: https://github.com/canyonroad/agentsh ; Apache-2.0 ; `gh api repos/canyonroad/agentsh/license` -> `Apache-2.0` (a `build-macos-enterprise` make target exists; no separate licence file seen at root).
- **Form**: Go binary plus shell shim; an auto-started local `agentsh server` holds a FUSE layer and policy engine per session (README "Autostart"). Linux packages via `sudo dpkg -i`.
- **Platforms**: Linux yes (full enforcement); macOS partial (ESF + Network Extension, "Alpha"); Windows partial (WSL2 yes; native minifilter drivers unsigned).
- **Mechanism**: Execution-layer policy under the agent: intercepts file, network, process, signal events (seccomp, Landlock, FUSE, eBPF, ptrace, cgroups as available) and decides allow/deny/approve/redirect; embedded LLM proxy set via `ANTHROPIC_BASE_URL`/`OPENAI_BASE_URL` with DLP, usage extraction, rate limits; MCP interception (README; docs/llm-proxy.md).
- **Against our contract**: 1 breaks (not a network-less box; policy on connect plus a base-URL proxy); 2 breaks (no relocation); 3 breaks (FUSE server, ptrace); 6 overlaps strongly with our egress feature list.
- **Maintenance**: pushed 2026-09-15, not archived, 395 stars; latest release v0.20.5 2026-06-23.
- **If we adopted it**: not as a sandbox. Its LLM proxy is the most complete open metering proxy in this set.
- **Verdict**: learn-from (egress layer: SSE usage extraction, TPM/RPM limiter with in-flight cap, fake/real credential substitution in both directions).
- **Matrix**: token yes (internal/proxy/usage.go; docs/llm-proxy.md "Token Usage Tracking", Anthropic and OpenAI); stream yes (`ExtractSSEUsage`, message_start/message_delta, internal/proxy/usage.go:143-186); attrib yes, unit = agentsh session (`~/.agentsh/sessions/<session-id>/llm-requests.jsonl`, docs/llm-proxy.md); cost no (tokens only, "for cost attribution"); budget yes (RPM and TPM token buckets, internal/proxy/llm_ratelimit.go; MCP/domain rate limits, README:343); inject yes (CredsSubHook replaces fake credentials with real ones in body, headers, query, path and scrubs responses, internal/proxy/credshook.go); allow yes (http_services with fail-closed host enforcement, README); hook yes (pre/post hooks, internal/proxy/hooks.go; approve verdicts); otel yes (internal/store/otel/otel.go, pkg/observability/tracing.go, pkg/observability/prometheus.go); mcp yes (tool allow/deny lists, cross-server exfiltration detection, internal/proxy/mcp_intercept.go, README "MCP Security").

### 11. mattolson/agent-sandbox

- **Repo / licence (verified)**: https://github.com/mattolson/agent-sandbox ; MIT ; `gh api repos/mattolson/agent-sandbox/license` -> `MIT`.
- **Form**: Go CLI generating docker compose plus devcontainer; agent container plus a mitmproxy sidecar; needs a Docker-compatible runtime (Colima, Docker Desktop, Podman).
- **Platforms**: Linux yes (any Docker-compatible runtime); macOS yes (target: Colima); Windows not stated.
- **Mechanism**: iptables in the agent container blocks all outbound except to the proxy container; the proxy enforces a hostname/scheme/method/path/query policy and injects secrets (README; docs/plan/decisions/001-proxy-as-enforcer.md). Per-harness policy files for claude, codex, gemini, opencode (.agent-sandbox/policy/).
- **Against our contract**: 1 partial (network present but firewalled to a proxy container; NET_ADMIN-style iptables inside); 2 holds via compose volumes; 3 breaks (container runtime); 5 partial at best; 6 overlaps on allow and inject.
- **Maintenance**: pushed 2026-10-11, not archived, 211 stars; latest release v0.17.1 2026-09-07.
- **Verdict**: learn-from (per-harness egress policy files; "proxy as enforcer, iptables as gatekeeper" ADR).
- **Matrix**: token no (code search empty); stream no; attrib partial (per project sandbox); cost no; budget no; inject yes (README); allow yes (README); hook partial (policy rules on method/path/query, README); otel no; mcp no.

### 12. VirtusLab/sandcat

- **Repo / licence (verified)**: https://github.com/VirtusLab/sandcat ; Apache-2.0 ; `gh api repos/VirtusLab/sandcat/license` -> `Apache-2.0`.
- **Form**: shell CLI plus docker compose/devcontainer templates and published mitmproxy images; needs docker with compose and yq.
- **Platforms**: Linux yes; macOS yes (via Docker); Windows not stated.
- **Mechanism**: All container traffic routed through a transparent mitmproxy over WireGuard, no per-tool proxy config; allow/deny list for HTTP/S and DNS; raw TCP/UDP dropped; secret substitution at the proxy (README). Agents: Claude Code, Cursor CLI, Codex CLI, Copilot CLI.
- **Against our contract**: 1 partial (no direct egress, but via WireGuard to a proxy container, not a unix socket); 2 holds via compose; 3 breaks; 6 overlaps on allow and inject.
- **Maintenance**: pushed 2026-10-19, not archived, 199 stars; no GitHub releases.
- **Verdict**: learn-from (WireGuard-to-proxy as a transparent capture alternative to base-URL overrides, relevant to Copilot's CONNECT path).
- **Matrix**: token no; stream no; attrib no; cost no; budget no; inject yes (README); allow yes (README); hook partial (mitmproxy addons, README `images/`); otel no; mcp no.

### 13. coipond/coi (Code on Incus)

- **Repo / licence (verified)**: https://github.com/coipond/coi ; MIT ; `gh api repos/coipond/coi/license` -> `MIT`.
- **Form**: CLI over Incus system containers; requires an Incus install on Linux; macOS via Colima/Lima (README).
- **Platforms**: Linux yes; macOS partial (through a Linux VM); Windows no.
- **Mechanism**: One full-OS Incus container per agent (Claude Code, Codex, opencode, pi, omp) with project at `/workspace`; nftables egress control in open, restricted, allowlist modes with DNS pinning (README).
- **Against our contract**: 1 partial (nftables allowlist, not network-less); 2 holds (`/workspace` fixed, other mounts "unless you mount or forward them"); 3 breaks (Incus); 5 no.
- **Maintenance**: pushed 2026-10-19, not archived, 755 stars; latest release v0.15.0 2026-10-12.
- **Verdict**: reject for our layer (system-container runtime dependency); note as a launcher pattern.
- **Matrix**: token no; stream no; attrib no; cost no; budget no; inject no (credentials kept out unless forwarded, README); allow yes (nftables allowlist mode, README); hook no; otel no; mcp no.

### 14. onecli/onecli

- **Repo / licence (verified)**: https://github.com/onecli/onecli ; `gh api` -> `Apache-2.0`, but open-core: LICENSE-ENTERPRISE covers `apps/web/src/ee/`, `packages/api/src/ee/`, `apps/gateway/crates/ee/` (production use needs a subscription). Budgets live in the enterprise crate (`apps/gateway/crates/ee/ee/src/budget.rs`).
- **Form**: team platform: Next.js dashboard, API server with PostgreSQL, Rust MITM gateway, runner (Docker, Kubernetes, cloud backends: apps/runner/src/backend/), in-sandbox supervisor.
- **Platforms**: Linux yes (self-host, Helm); macOS/Windows as dev hosts only.
- **Mechanism**: agent per employee in a sandbox whose only way out is the gateway; gateway MITMs HTTPS, authenticates agents by `Proxy-Authorization` token, injects credentials matched by host/path (README "Architecture").
- **Against our contract**: 1 partial (gateway-only egress, over TCP proxy auth); 2-5 not applicable (server platform); 6 overlaps.
- **Maintenance**: pushed 2026-10-14, not archived, 3,580 stars; latest release v2.14.0 2026-10-14.
- **Verdict**: reject (server platform, open-core); the Apache-2.0 `inject` and `policy-engine` crates are learn-from.
- **Matrix**: token no evidence outside ee; attrib yes (per agent access token, README); budget enterprise-only (apps/gateway/crates/ee/ee/src/budget.rs under LICENSE-ENTERPRISE); inject yes (apps/gateway/crates/inject/src/secret_inject.rs); allow yes (README "only way out is the gateway"); hook yes (approval crate, human-in-the-loop, apps/gateway/crates/approval); otel not checked; mcp not checked.

### 15. TencentCloud/CubeSandbox

- **Repo / licence (verified)**: https://github.com/TencentCloud/CubeSandbox ; `gh api` -> `NOASSERTION`; LICENSE reads "Cube Sandbox is licensed under the Apache-2.0 except for the third-party components listed below" (kata-containers and cloud-hypervisor under Apache-2.0, cilium/bpf under BSD-2-Clause).
- **Form**: multi-component Go/Rust service (CubeAPI, CubeMaster, Cubelet, CubeNet, CubeEgress, CubeProxy, hypervisor); x86_64 Linux with KVM; cluster or single node.
- **Platforms**: Linux yes (server); macOS no; Windows no.
- **Mechanism**: E2B-compatible microVM sandboxes (RustVMM/KVM) with eBPF inter-sandbox isolation and egress filtering plus an L7 proxy with per-domain/path/method policy and credential injection (README "Product Highlights").
- **Against our contract**: a server-side E2B replacement; 1-5 not applicable to a local launcher.
- **Maintenance**: pushed 2026-10-19, not archived, 12,985 stars; latest release v0.7.3 2026-10-12.
- **Verdict**: reject (cloud/server sandbox service, same class as E2B already covered).
- **Matrix**: inject yes, allow yes, hook partial (L7 policy) per README; others not checked.

---

### Seen and skipped (one line each)

- trailofbits/claude-code-devcontainer: Apache-2.0, 956 stars, pushed 2026-08-28, no releases; Docker devcontainer, full outbound by default, optional in-container iptables; learn-from its Dockerfile only.
- mattpocock/sandcastle: MIT, 8,531, v0.12.0 2026-06-29; TS orchestrator over Docker/Podman/Vercel providers, no egress policy (README has only a `network` attach option); learn-from per-harness session capture.
- arcboxlabs/arcbox: MIT/Apache-2.0 dual (LICENSE-MIT, LICENSE-APACHE), 4,233, v0.10.0 2026-10-19; macOS-only Docker/VM runtime with `abctl claude`; not Linux.
- superhq-ai/shuru: Apache-2.0, 873, v0.7.0 2026-08-05; macOS VZ microVM, Linux ARM64 experimental, host-allowlist and secret placeholder proxy; macOS-centric.
- clawkwork/clawk: Apache-2.0, 1,031, v0.4.0 2026-08-13; macOS VM, Linux experimental (Firecracker), allowlist; overlaps coop/matchlock.
- finbarr/yolobox: MIT, 653, v0.19.9 2026-10-18; Docker/Podman wrapper, no egress policy beyond advice.
- nikvdp/cco: MIT, 427, pushed 2026-09-29, no releases; bubblewrap or sandbox-exec or Docker wrapper for Claude Code; native mode exposes host read-only; no egress policy.
- dredozubov/hazmat: MIT, 178, v0.10.2 2026-08-25; macOS agent-user plus Seatbelt plus pf; Linux not supported; below threshold.
- AkihiroSuda/alcless: Apache-2.0, 206, v0.2.1 2026-09-17; macOS separate-user sandbox; macOS only.
- archie-judd/agent-sandbox.nix: MIT, 181; Nix-based bwrap/Seatbelt wrapper; needs Nix.
- carderne/pi-sandbox: MIT, 285; pi-only OS sandbox extension.
- cjermain/pi-less-yolo: Apache-2.0, 166; pi in Docker.
- dtormoen/tsk-tsk: MIT, 171, pushed 2026-07-29; Docker-based agent task runner.
- madarco/agentbox: MIT, 538; parallel agents in VMs/cloud; launcher, no egress model read.
- lynaghk/vibe: MIT, 957, pushed 2026-08-31; macOS-only Linux VM.
- rderaison/bromure: MIT, 302; macOS-focused agent and browser sandbox.
- jayminwest/warren: MIT, 504; agent lifecycle/spend orchestration over other sandboxes.
- schmitthub/clawker: MIT, 58; below threshold.
- obra/packnplay: no LICENSE file in repo (root listing has none; `gh api .../license` 404), 174, pushed 2026-03-21; out (no licence, stale).
- akitaonrails/ai-jail: GPL-3.0, 1,378; out (licence).
- superhq-ai/superhq: AGPL-3.0, 294; out (licence).
- micahflee/claudito: GPL-3.0; out.
- Infisical/agent-vault: MIT outside `ee/` (LICENSE text), 2,381; HTTP credential proxy, egress layer not sandbox.
- stacklok/toolhive: Apache-2.0, 2,299, v0.55.1 2026-10-19; runs MCP servers in containers; MCP-gateway territory, not a harness sandbox.
- google/ax: Apache-2.0, 13,985; cluster orchestrator that "runs on top of Agent Substrate" (README); excluded with Substrate.
- kagent-dev/kagent: Apache-2.0, 4,055; Kubernetes agent framework; README has no sandboxing claim.
- trycua/cua: MIT, 30,221; computer-use GUI sandboxes and benchmarks; different problem.
- vndee/llm-sandbox: MIT, 1,133, 0.3.46 2026-10-16; Python code-interpreter library over Docker/Podman/K8s; not a harness launcher.
- engineer-man/piston: MIT, 2,832, pushed 2026-07-31; code-execution API; not a harness launcher.
- microsoft/wassette: MIT, 967, v0.8.1 2026-10-19; Wasm components via MCP; not a harness sandbox.
- microsoft/litebox: MIT, 3,200, v0.1.0 2026-10-07; library OS; research-grade, no harness story.
- instavm/coderunner: Apache-2.0, 895, pushed 2026-10-10, no releases; "a local sandbox for your AI agents" (description); code-execution focus, README not read in depth.
- BitMiracle-AI/Dormice, zerobootdev/zeroboot, deeplethe/forkd: Apache-2.0, 1,600 / 2,465 / 2,921; server-side microVM pools (E2B-style); not local launchers.
- strukto-ai/mirage, sandbaseai/sandbase-harness, CelestoAI/celesto, nuwax-ai/nuwax, agentconnect-md/agentconnect, openma-ai/open-managed-agents, FootprintAI/Containarium: Apache-2.0; platforms/virtual terminals/hosted computers, not local confinement of a CLI harness.
- okou-ai/okou: NOASSERTION; agent product, not a sandbox.
- liquidmetal-dev/flintlock: MPL-2.0; microVM manager; ranked out (Category B) and server-side.
- cocoonstack/sandbox-operator: AGPL-3.0; out.
- bytedance/SandboxFusion: Apache-2.0, 1,077, pushed 2026-07-14; code-execution evaluation service. bytedance/vArmor: Apache-2.0, 510; Kubernetes AppArmor/BPF hardening operator. Neither is a harness launcher.

---

### Ranked shortlist for this layer

1. **NVIDIA/OpenShell**: same architecture as ours (network-less workload, one socket to a trusted supervisor, injection, L7, MCP, middleware hook, OTel), Apache-2.0, very active. Cost: daemon, container runtime or KVM, large surface.
2. **multikernel/sandlock**: Rust library, unprivileged, Landlock plus seccomp-notify; best defence-in-depth layer to add inside our namespaces, possibly a namespace-less fallback.
3. **eugene1g/agent-safehouse**: borrow per-harness Seatbelt rules for the dry-coded macOS backend.
4. **trailofbits/coop (coop-proxy)**: borrow the credential proxy pattern (capability token, provider operation allowlist, self-Landlock).
5. **canyonroad/agentsh**: reference for egress metering (SSE usage, TPM limiter, credential substitution, MCP policy).

#### Spike 1: OpenShell (Linux)

Blocker first: the Podman driver requires Podman 5.x (docs/about/support-matrix.mdx) and the host has 4.9.3; the VM driver needs `/dev/kvm`, which user `striker` cannot open (not in group `kvm`). Either install Podman 5 or run once as root `usermod -aG kvm striker`. No Node needed. No Docker needed.

```sh
D=$HOME/opt/openshell && mkdir -p $D && cd $D
V=v0.1.4
for a in openshell-x86_64-unknown-linux-musl openshell-gateway-x86_64-unknown-linux-gnu openshell-driver-vm-x86_64-unknown-linux-gnu; do
  gh release download $V -R NVIDIA/OpenShell -p "$a.tar.gz" -D $D && tar -xzf $a.tar.gz
done
gh release download $V -R NVIDIA/OpenShell -p 'openshell-checksums-sha256.txt' -p 'openshell-gateway-checksums-sha256.txt' -D $D
sha256sum -c --ignore-missing openshell-checksums-sha256.txt openshell-gateway-checksums-sha256.txt
# gateway config (VM driver; or compute_driver = "podman" once Podman 5 is present)
cat > $D/gateway.toml <<'EOF'
[openshell]
version = 2
[openshell.gateway]
compute_driver = "vm"
[openshell.drivers.vm]
allow_driver_config = true
enable_bind_mounts = true
[openshell.drivers.vm.resource_admission]
enabled = false
EOF
OPENSHELL_TELEMETRY_ENABLED=false ./openshell-gateway --help   # confirm config flag name, then start with gateway.toml
```

Test point 1 (no network but the socket):
```sh
./openshell sandbox create --name p1 -- sh -c 'cat /proc/net/dev; getent hosts example.com; curl -sS -m5 https://example.com; echo rc=$?'
# expect: only lo, DNS and HTTPS denied by default policy
# then bind our socket dir and try it:
mkdir -p /tmp/os-sock && python3 -I -c 'import socket,os;p="/tmp/os-sock/egress.sock";os.path.exists(p) and os.unlink(p);s=socket.socket(socket.AF_UNIX);s.bind(p);s.listen(1);c,_=s.accept();c.sendall(b"ok\n")' &
./openshell sandbox create --name p1b --driver-config-json '{"vm":{"mounts":[{"type":"bind","source":"/tmp/os-sock","target":"/run/memory","read_only":false}]}}' \
  -- python3 -c 'import socket;s=socket.socket(socket.AF_UNIX);s.connect("/run/memory/egress.sock");print(s.recv(9))'
```
Over virtiofs a host unix socket is not expected to connect (a socket inode does not cross the VM boundary), so on the VM driver point 1 likely needs our egress as a policy-allowed upstream instead; repeat with the Podman driver (`"podman":{"mounts":[{"type":"bind",...}]}`) where it may work, and record the Landlock/seccomp outcome.

Test point 2 (fixed-path mounts): add five `bind` entries (`/home/user` rw, `/work` rw, `/opt/memory/memory` ro, `/memory` ro, `/run/memory` rw) and run `mount; ls -la /home/user /work /opt/memory /memory /run/memory; touch /memory/x` (expect EROFS). On x86_64 the VM driver caps mounts at three (runtimes.mdx), so expect the VM driver to refuse five; the Podman driver is the real test.

#### Spike 2: sandlock (Linux)

No root, no KVM, no Podman, no Node. Needs cargo (host has cargo 1.99.0) and kernel Landlock (present per `/proc/config.gz`).

```sh
D=$HOME/opt/sandlock && mkdir -p $D
cargo install --locked --git https://github.com/multikernel/sandlock --tag v0.9.0 sandlock-cli --root $D
$D/bin/sandlock run --help    # confirm --chroot, --fs-mount and the read-only mount form
```

Test point 1:
```sh
S=$D/bin/sandlock
$S run -r /usr -r /lib -r /lib64 -r /etc -r /bin -- sh -c 'curl -sS -m5 https://example.com; echo rc=$?'   # expect connect denied
mkdir -p /tmp/sl-sock && python3 -I -c 'import socket,os;p="/tmp/sl-sock/egress.sock";os.path.exists(p) and os.unlink(p);s=socket.socket(socket.AF_UNIX);s.bind(p);s.listen(1);c,_=s.accept();c.sendall(b"ok\n")' &
$S run -r /usr -r /lib -r /lib64 -r /etc -r /bin -r /tmp/sl-sock -- python3 -c 'import socket;s=socket.socket(socket.AF_UNIX);s.connect("/tmp/sl-sock/egress.sock");print(s.recv(9))'
$S run -r /usr -r /lib -r /lib64 -r /etc -r /bin -- python3 -c 'import socket;s=socket.socket();s.connect(("127.0.0.1",22))'  # expect denied: loopback is host loopback here
$S run -r /usr -r /lib -r /lib64 -r /etc -r /bin -- cat /proc/net/dev   # expect host interfaces visible (no netns)
```

Test point 2:
```sh
mkdir -p /tmp/sl/{rootfs,home,work,rec} && podman export $(podman create docker.io/library/ubuntu:24.04) | tar -C /tmp/sl/rootfs -x
$S run --chroot /tmp/sl/rootfs --fs-mount /home/user:/tmp/sl/home --fs-mount /work:/tmp/sl/work \
   --fs-mount /memory:/tmp/sl/rec:ro -- sh -c 'ls -la /home/user /work /memory; touch /work/ok /memory/bad; echo rc=$?'
# expect /work writable, /memory EACCES; also check that nothing outside the chroot is reachable: ls /tmp /home
```
Record whether the `:ro` suffix is accepted on the CLI (the TOML form documents it, docs/sandbox-reference.md).

---

# Part 2b: agent egress proxies, LLM gateways and MCP gateways not in the four reports


Researched 20 October 2026. Every licence, date and capability below was read in this session with `gh api`, a tarball of the repo at its default branch (downloaded with `gh api repos/O/R/tarball` into `part2/gw/src/`), or the doc page named. "Pushed" is `gh api repos/O/R --jq '{pushed_at,archived,stargazers_count}'`; "release" is `gh api repos/O/R/releases?per_page=1`.

Excluded as covered (covered.txt): agentgateway, Envoy AI Gateway / Agent Router, LiteLLM, iron-proxy, cloak, Smokescreen, mitmproxy, Pomerium, Envoy, sandbox-runtime, Codex network proxy, coder/httpjail, coder/boundary.

Sources swept: `gh search repos --topic` for llm-gateway, ai-gateway, mcp-gateway, llm-proxy, mcp-proxy (top 40 each by stars); HN Algolia stories since 1 Jan 2026 for "llm gateway", "mcp gateway", "ai gateway", "egress proxy agent", "llm proxy", "mcp proxy", "token usage claude code"; READMEs of e2b-dev/awesome-mcp-gateways and cuihuan/awesome-ai-gateway.

Finding that applies to every candidate, point 1: none of the gateways below accepts traffic on a unix socket. Grep of each source tree for `UnixListener`, `net.Listen("unix"`, `unix://` found only control sockets (clawpatrol daemon, toolhive API, mcpproxy tray) or example code (busbar `examples/smart-router/rust-hook`). All bind TCP (`listen: host:port`). So none can replace the socket end of our egress. Each could only sit *behind* it: our egress keeps the socket, the label-vs-ceiling check and MCP, and forwards model traffic to the gateway on host loopback; the gateway then holds the provider keys and the network. That moves "the only process with the network" from our egress to the gateway, adds a second process per host, and duplicates metering we already do.

---

### 1. Busbar

- **Repo / licence (verified)**: https://github.com/GetBusbar/busbar; Apache-2.0; `gh api repos/GetBusbar/busbar/license --jq .license.spdx_id` -> `Apache-2.0`; single root `LICENSE`, `NOTICE`; README footer "Single Rust binary, MSRV 1.97, Apache-2.0". No paid tier named in the repo; README "Self-hosted, always. No hosted service".
- **Form**: single static Rust binary (12.4 MiB `busbar` in `busbar-x86_64-unknown-linux-gnu.tar.gz`), one `config.yaml` plus a shipped `providers.yaml`; no database; no privileges.
- **Platforms**: Linux yes, macOS yes, Windows yes (v1.5.5 assets: x86_64/aarch64 linux-gnu, x86_64/aarch64 apple-darwin, x86_64-pc-windows-msvc).
- **Mechanism**: reverse proxy speaking six wire protocols both ways (OpenAI Chat, OpenAI Responses, Anthropic, Gemini, Cohere, Bedrock Converse); same-protocol routes forward the original bytes. Virtual keys bind to a group tree whose limits (`requests`, `tokens`, `budget` per minute/hour/day/month/total, `concurrent`) are charged atomically; streamed usage is accrued at stream end (`crates/busbar/src/proxy/response_body.rs` `UsageSink`). Hooks are `tap` (observe) or `gate` (reject/restrict/rewrite) stages, in-process signed cdylib plugins or an out-of-process HTTPS sidecar (`docs/hooks.md`).
- **Against our contract**: 1 breaks: TCP only (`docs/configuration.md` `listen` `host:port`, default `0.0.0.0:8080`); can sit behind our egress on loopback. 2-5 silent. 6 partial: replaces per-destination forwarding, credential injection, token metering incl. SSE, per-key budgets; does not give MCP on the same listener (MCP tool governance is described in README "What Busbar governs" but `crates/` has no MCP proxy; `config/mod.rs:1754` refers to an MCP stage as future), no CONNECT tunnel, no label check (would be a `gate` hook).
- **Maintenance**: pushed 2026-10-19T22:22Z, not archived, 204 stars; latest release v1.5.5 2026-08-20.
- **If we adopted it**: replaces crates/egress forwarding, metering, budgets; adds a second binary per host, a TCP port with its own auth chain, a signed-plugin loader if we use in-process gates. We keep the socket, `/mcp`, CONNECT, label check.
- **Verdict**: learn-from (closest shape to our egress in Rust: group-tree limits charged at stream end, tap/gate hook split, SSRF-safe OTLP exporter notes in `crates/busbar/Cargo.toml`). Not adopt: no socket, no MCP, and it duplicates what we have.
- **Matrix**: token yes (`crates/busbar/src/billing.rs`, `cost.rs`); stream yes (`proxy/response_body.rs` "usage extraction, billing ... for streaming responses", `is_sse`); attrib partial, unit = virtual key -> group (`docs/configuration.md` `groups`; `docs/admin-api.md:74` auto-provisions a leaf group per minted key, so one key per session works); cost yes (`rate_card`, `docs/configuration.md:537`); budget yes (`docs/configuration.md:527`); inject yes (README "keeps provider credentials at the boundary", `api_key: { env: ... }`); allow no (routes to configured providers only; no host allowlist or CONNECT); hook yes (`docs/hooks.md` tap/gate, `busbar-webrequest-hook` sidecar); otel yes (OTLP traces via `module: otlp`, Prometheus `/metrics`, `docs/observability.md:65,127`); mcp no (not implemented in repo). Paid-only: none stated.

### 2. Agent Vault (Infisical)

- **Repo / licence (verified)**: https://github.com/Infisical/agent-vault; MIT outside `ee/`; `gh api .../license` -> `NOASSERTION`; read `LICENSE`: "All content that resides under any "ee/" directory ... licensed under ee/LICENSE ... Content outside ... MIT Expat". `find . -type d -name ee` on the tarball: none, so the whole tree is MIT. README names a commercial "Infisical Agent Vault" built into the Infisical platform as a separate product.
- **Form**: single Go binary (38 MB `agent-vault`), server + CLI; HTTP API on 14321, MITM forward proxy on 14322 (`cmd/server.go:773-778`, default host `127.0.0.1` in `cmd/defaults.go:10`); local encrypted store, master password. No privileges for host mode; container mode needs Docker and grants the container NET_ADMIN for iptables.
- **Platforms**: Linux yes, macOS yes (v0.40.0 assets darwin/linux amd64/arm64), Windows no binary.
- **Mechanism**: the agent sets `HTTPS_PROXY` to the vault; the proxy authenticates each CONNECT/absolute-form request by `Proxy-Authorization` (`internal/mitm/connect.go:74`), terminates TLS with its own CA, substitutes placeholder credentials (`ANTHROPIC_API_KEY=__anthropic_api_key__`) with the real ones for matched services, and forwards; unmatched hosts pass through or, with `unmatched_host_policy=deny`, get 403.
- **Against our contract**: 1 breaks: TCP listeners only; could sit behind our egress for CONNECT traffic. 2-5: its own `--isolation=container` mode runs Claude Code in Docker with iptables `OUTPUT DROP` except the vault ports (`docs/guides/container-isolation.mdx`): needs Docker and root-in-container, breaks 3. 6 partial: covers the CONNECT path with credential injection (which our CONNECT tunnel cannot do without MITM) and a host allowlist; no token metering, no budgets, no label check, no MCP serving.
- **Maintenance**: pushed 2026-10-18T01:52Z, not archived, 2381 stars; latest release v0.40.0 2026-10-01.
- **If we adopted it**: would replace our allowlisted CONNECT tunnel for Copilot with a MITM that injects the key; adds a CA we must install in the harness image, a Go daemon with its own user store and web UI, and TLS interception.
- **Verdict**: learn-from (placeholder-substitution MITM is the design to copy if Copilot ever needs key injection on the CONNECT path; session-scoped tokens per `vault run` match our per-session unit).
- **Matrix**: token no (no `input_tokens`/`prompt_tokens` in `internal/`); stream n/a; attrib partial, unit = agent token or `vault run` session (`docs/agents/protocol.mdx` "Session types"); cost no; budget partial (`internal/ratelimit/` request rate limits, not token/spend); inject yes (README "Credential Brokering"); allow yes (`unmatched_host_policy=deny`, README); hook partial (service rules per host/endpoint, no external decider found); otel no (OTel modules only `// indirect` in `go.mod:96-101`); mcp partial (README "MCP, CLI, SDK, API" traffic routed through it; it does not parse MCP). Paid-only: grouped access policies are in the commercial Infisical product (README "Agent Vault (open source) and Infisical Agent Vault").

### 3. Bifrost (Maxim)

- **Repo / licence (verified)**: https://github.com/maximhq/bifrost; Apache-2.0; `gh api .../license` -> `Apache-2.0`; only one `LICENSE` within depth 3 of the tarball. Enterprise is a separate distribution: `docs/enterprise/overview.mdx` "Bifrost Enterprise is a strict superset of the open-source gateway".
- **Form**: Go binary (`transports/bifrost-http`), fetched by `npx -y @maximhq/bifrost` from `downloads.getmaxim.ai` (`npx/bifrost/bin.js:9`) or Docker; embedded web UI; SQLite/Postgres config and log stores optional. No privileges.
- **Platforms**: Linux yes, macOS yes, Windows yes (`npx/bifrost/bin.js:165-176` darwin/linux/win32).
- **Mechanism**: OpenAI-compatible gateway with drop-in routes for many SDKs and CLI agents (`docs/cli-agents/claude-code.mdx`, `codex-cli.mdx`, `gemini-cli.mdx`, `qwen-code.mdx`, `github-copilot.mdx`); governance plugin enforces virtual-key budgets and rate limits in `PreLLMHook`/`PostLLMHook`, with streamed usage settled on the final chunk (`plugins/governance/tracker.go:144`); also an MCP gateway with per-VK tool allowlists.
- **Against our contract**: 1 breaks: `net.Listen("tcp", serverAddr)` (`transports/bifrost-http/server/server.go:3715`), default host localhost; can sit behind ours. 2-5 silent. 6 partial: metering incl. SSE, budgets, injection, MCP gateway, OTel; no CONNECT tunnel, no label-vs-ceiling (could be a Go or WASM plugin, `docs/plugins/writing-go-plugin.mdx`, `writing-wasm-plugin.mdx`).
- **Maintenance**: pushed 2026-10-19T20:10Z, not archived, 8944 stars; latest release transports/v2.3.1 2026-10-19.
- **If we adopted it**: replaces forwarding, metering, budgets, MCP aggregation; adds a large Go service (repo 691 MB with UI and docs), a config store, an npm or Docker fetch path; we keep the socket and label check.
- **Verdict**: learn-from (the most complete open feature set here; too large to run per session and not Rust).
- **Matrix**: token yes (`plugins/governance/tracker.go`); stream yes (`tracker.go:41-44,144` IsStreaming/IsFinalChunk); attrib partial, unit = virtual key -> team -> customer (`docs/features/governance/budget-and-limits.mdx:28-48`); per-key metadata and required headers (`docs/features/governance/required-headers.mdx`); cost yes (`framework/modelcatalog`, budget doc "Cost Calculation"); budget yes (same doc: budgets and request/token rate limits per VK, team, customer, provider config); inject yes (`docs/features/governance/virtual-keys.mdx`, provider keys held by gateway); allow partial (model/provider and MCP tool allowlists per VK, `docs/features/governance/mcp-tools.mdx`; no host allowlist); hook yes (`PreLLMHook`, `PreMCPHook`, `HTTPTransportPreHook` in `plugins/governance/main.go:442,1177,1557`); otel yes (`docs/features/otel.mdx` OTLP spans and metrics); mcp yes (`docs/mcp/*`, PreMCPHook). Paid-only (`docs/enterprise/overview.mdx`): Virtual MCPs, access profiles, guardrails, RBAC, teams/business units UI, clustering, adaptive routing, audit logs, log exports.

### 4. AISIX OSS (API7)

- **Repo / licence (verified)**: https://github.com/api7/aisix; Apache-2.0; `gh api .../license` -> `Apache-2.0`; workspace `Cargo.toml` `license = "Apache-2.0"`, single `LICENSE`.
- **Form**: single static Rust binary `aisix` (`crates/aisix-server`); Docker image `ghcr.io/api7/aisix`; declarative `resources.yaml` reloaded on SIGHUP, or etcd. No GitHub release assets in the last 30 releases; build from source or pull the image. No privileges.
- **Platforms**: Linux yes (image, cargo); macOS and Windows not stated in README.
- **Mechanism**: OpenAI- and Anthropic-compatible proxy in front of OpenAI, Anthropic, Vertex, Bedrock, Azure, DeepSeek with translation both ways incl. streaming; caller API keys (hashed), RPS..RPD and TPM/TPD limits scoped by api_key/model/team/member; usage events with cache token counts and USD cost; MCP gateway at `/mcp` with gateway-held credentials.
- **Against our contract**: 1 breaks (TCP `proxy.addr`); can sit behind ours. 2-5 silent. 6 partial: forwarding, injection, token limits, MCP, OTel; budgets/spend caps are not in OSS (below).
- **Maintenance**: pushed 2026-10-19T11:02Z, not archived, 200 stars; latest release v1.6.0 2026-10-12.
- **If we adopted it**: as for Busbar, plus etcd or a file reload loop; no money budgets without the commercial control plane.
- **Verdict**: learn-from (Rust, clean usage-event schema `crates/aisix-obs/src/usage.rs` with cost-attribution tags); budgets paywalled.
- **Matrix**: token yes (`crates/aisix-obs/src/usage.rs` `UsageEvent`, `cached_prompt_tokens`); stream yes (README "Native SSE streaming", Anthropic translation "including streaming"); attrib partial, unit = API key / team / member plus tags resolved from the key (`usage.rs:197`); cost yes ("costed in USD per the DP's pricing snapshot", `usage.rs:109`); budget partial: rate and token limits yes (README "Rate limiting & concurrency"), money budgets no in OSS; inject yes (`provider_keys` in `resources.yaml`); allow no (model allowlist per key only); hook partial (guardrail pre/post hooks `crates/aisix-guardrails`, no general external decider found); otel yes (README "Prometheus /metrics ... OTLP/GenAI span export"); mcp yes (README "MCP gateway"). Paid-only: README "Budgets and spend caps are AISIX Enterprise and AISIX Cloud features".

### 5. OpenAFW (OpenGuardrails)

- **Repo / licence (verified)**: https://github.com/openguardrails/openafw; Apache-2.0; `gh api .../license` -> `Apache-2.0`; single `LICENSE`; README "Free and open source. Connects to OpenGuardrails for the paid features (served rulesets, tool-call judgement, dashboards)".
- **Form**: Rust workspace: `afw-engine` library (deps serde, regex, fancy-regex only), `openafw` CLI/daemon on `127.0.0.1:4141`, optional Tauri desktop shell. No release assets; `cargo build --release`. No privileges.
- **Platforms**: Linux yes, macOS yes, Windows yes (README `service install` for launchd/systemd --user/Task Scheduler; desktop builds .dmg/.msi/.deb).
- **Mechanism**: local proxy for Claude Code, Codex, Gemini CLI, OpenCode: replaces secrets in provider-bound requests with placeholders (`OGRKF0000001`) and restores them in replies, including across SSE chunks (`crates/afw-engine/src/sse.rs`, `reassemble.rs`); keys held in an AES-GCM vault and agents get local tokens.
- **Against our contract**: 1 breaks (TCP loopback). 2-5 silent. 6 partial: credential injection per agent and an outbound content rewrite; no metering, budgets, CONNECT, MCP.
- **Maintenance**: pushed 2026-09-17T11:09Z, not archived, 105 stars; latest release v1.0.0 2026-09-17.
- **If we adopted it**: only the `afw-engine` crate would fit, as a library inside crates/egress; it adds two regex crates and an OGR ruleset format.
- **Verdict**: borrow (with attribution): `afw-engine` mask/restore across SSE frame boundaries is the piece our label-vs-ceiling path lacks if we ever redact rather than refuse.
- **Matrix**: token no (no usage parsing in `crates/openafw/src`); stream yes for rewriting, n/a for metering (`afw-engine/src/sse.rs`); attrib partial, unit = agent local token (README "per-agent local tokens"); cost no; budget no; inject yes (vault + local tokens); allow no; hook partial (remote OGR verdicts `step/request`/`step/response`, `--ogr-enforce`, paid service); otel no; mcp no. Paid-only: served rulesets and tool-call judgement (README line 10-11).

### 6. Claw Patrol (Deno)

- **Repo / licence (verified)**: https://github.com/denoland/clawpatrol; MIT; `gh api .../license` -> `MIT`; `LICENSE.md` "MIT License, Copyright 2026 Deno Land Inc"; vendored `third_party/tailscale` keeps its own licence (BSD-3-Clause per Tailscale upstream; file `third_party/tailscale/LICENSE`).
- **Form**: single Go binary, three modes: `clawpatrol gateway config.hcl` (the proxy), `clawpatrol join` (WireGuard tunnel for a host), `clawpatrol run claude` (wraps one process tree). Building needs Go and Node.
- **Platforms**: Linux yes, macOS yes (NetworkExtension for `run`, `Clawpatrol.app`), Windows no (v0.5.14 assets darwin/linux only).
- **Mechanism**: parses traffic at the wire per protocol (HTTP, Postgres, ClickHouse, Kubernetes) and evaluates CEL rules written in HCL; verdicts allow/deny/human approval; credentials are typed handles bound to endpoints and injected by the gateway. `run` on Linux re-execs itself with `CLONE_NEWUSER|CLONE_NEWNET|CLONE_NEWNS` and a TUN to the gateway (`cmd/clawpatrol/run_linux.go:31-36`).
- **Against our contract**: 1 breaks differently: the confined child gets a netns whose only route is a TUN/WireGuard tunnel to the gateway, not a unix socket; the daemon's unix socket (`daemon_linux.go:408`) is control only. 3: `run` is unprivileged userns+netns, same family as ours. 6 partial: policy hook and injection yes; no token metering, no budgets, no MCP serving.
- **Maintenance**: pushed 2026-10-03T06:58Z, not archived, 1087 stars; latest release v0.5.14 2026-10-03.
- **If we adopted it**: would replace our label-vs-ceiling check and CONNECT allowlist with CEL over wire facts; adds WireGuard/TUN, an HCL policy language, and a phone-home update checker (`doc/telemetry.md`).
- **Verdict**: learn-from (CEL over protocol facts is a good model for the ceiling check; `unknown_host = passthrough | deny | inspect` default in `examples/gateway.example.hcl:121`).
- **Matrix**: token no (no usage extraction in `internal/`); stream n/a; attrib partial, unit = wrapped process / WireGuard peer (`doc/wg-ephemeral-sessions-design.md`); cost no; budget no; inject yes (`credential` blocks, `examples/gateway.example.hcl:25-31`); allow yes (`unknown_host = "deny"` option, line 121); hook yes (CEL rules, human approval, `examples/gateway.example.hcl:322-326`); otel partial (`go.mod` otlpmetrichttp; documented use is the update checker, `doc/telemetry.md`); mcp partial (MCP OAuth credential plugin `internal/config/plugins/credentials/notion_mcp_oauth.go`; no MCP tool policy found). Paid-only: none stated.

### 7. Plano (Katanemo, formerly archgw)

- **Repo / licence (verified)**: https://github.com/katanemo/plano (`katanemo/archgw` redirects here); Apache-2.0; `gh api .../license` -> `Apache-2.0`; single `LICENSE`.
- **Form**: Envoy plus two Rust WASM filters (`llm_gateway.wasm`, `prompt_gateway.wasm`) plus a Rust sidecar `brightstaff`, driven by a Python CLI `planoai` that runs either a Docker image `katanemo/plano:0.4.37` or native binaries it downloads (`cli/planoai/native_binaries.py`). No privileges.
- **Platforms**: Linux yes (amd64, arm64), macOS partial (arm64 only, `native_binaries.py:34-42`), Windows no.
- **Mechanism**: Envoy data plane routes to providers with `access_key` per provider; token rate limits per provider keyed by a header value (`x-arch-ratelimit-selector`, `crates/common/src/consts.rs:1`, `crates/common/src/ratelimit.rs:22-28`); brightstaff extracts usage incl. cache tokens and tracks per-session cost for routing decisions.
- **Against our contract**: 1 breaks (Envoy TCP listeners); a Python CLI and Envoy per host. 2-5 silent. 6 partial.
- **Maintenance**: pushed 2026-10-07T16:45Z, not archived, 7095 stars; latest release 0.4.37 2026-09-28.
- **If we adopted it**: adds Envoy, WASM filters, a Python CLI; replaces forwarding and some metering.
- **Verdict**: reject (three runtimes for what our one axum service does); learn-from its header-keyed token limiter.
- **Matrix**: token yes (`crates/llm_gateway/src/stream_context.rs:791-802` `extract_usage_counts`; `crates/brightstaff/src/streaming.rs:35-42` incl. cached and cache-creation tokens); stream partial (Bedrock stream path estimates `content.len() / 4`, `stream_context.rs:677-680`); attrib partial, unit = any header value via selector, session binding in `brightstaff/src/session_cache`; cost partial (`session_cost_usd` from a pricing feed, used for routing, `streaming.rs:227-232`); budget partial (token rate limits; routing budget); inject yes (`access_key`, `stream_context.rs:208`); allow no; hook yes (filter chains, README "Moderation & Memory Hooks"); otel yes (README "OTEL traces/metrics", `crates/common/src/tracing.rs`); mcp partial (MCP JSON-RPC `initialize` handled for agents/filters, `brightstaff/src/handlers/agents/jsonrpc.rs:6-7`). Paid-only: none in repo; hosted Plano models are a free hosted service (README).

### 8. GoModel (Enterpilot)

- **Repo / licence (verified)**: https://github.com/ENTERPILOT/GoModel; MIT; `gh api .../license` -> `MIT`; `docs/about/license.mdx`: MIT today, "GoModel Pro ... extensions are licensed separately, not MIT", and "We may make the license of the gateway itself less permissive". Licence risk.
- **Form**: single Go binary plus dashboard tarball; Prometheus optional. No privileges.
- **Platforms**: Linux, macOS, Windows yes (v0.1.103 assets).
- **Mechanism**: OpenAI- and Anthropic-compatible gateway; budgets per user path/team/key; labels from HTTP headers for attribution; plugins for guardrails, stream filters, routing; MCP gateway.
- **Against our contract**: 1 breaks (TCP); 2-5 silent; 6 partial (no CONNECT, no label ceiling).
- **Maintenance**: pushed 2026-10-19T22:24Z, not archived, 1255 stars; latest release v0.1.103 2026-10-16.
- **If we adopted it**: as Bifrost, smaller; licence may change.
- **Verdict**: learn-from (header-based labelling is the cheapest per-session attribution design: `docs/features/labelling.mdx` "Tagging headers").
- **Matrix**: token yes (README "Cost tracking ... usage analytics"); stream not verified; attrib yes, unit = label from a configured header or key (`docs/features/labelling.mdx:14,142`); cost yes (`docs/features/cost-tracking.mdx`); budget yes (`docs/features/budgets.mdx`, `rate-limits.mdx`); inject yes (provider keys at gateway, `docs/providers/key-rotation`); allow no; hook yes (README "Plugins ... guardrails, response and stream filters"); otel yes (README "Prometheus metrics, OpenTelemetry"); mcp yes (`docs/features/mcp-gateway.mdx`). Paid-only: OIDC SSO and "a couple of other extensions" (GoModel Pro, `docs/about/license.mdx`).

### 9. MCPProxy (smart-mcp-proxy/mcpproxy-go)

- **Repo / licence (verified)**: https://github.com/smart-mcp-proxy/mcpproxy-go; MIT; `gh api .../license` -> `MIT`; the multi-user "server edition" is in the same repo behind `//go:build server` (`internal/serveredition/doc.go`), same licence.
- **Form**: single Go binary with embedded web UI; optional macOS tray app. No privileges.
- **Platforms**: Linux yes, macOS yes, Windows yes (README; v0.71.1 assets).
- **Mechanism**: aggregates many MCP servers behind one endpoint that exposes discovery tools (`retrieve_tools`, `call_tool_*`); new servers are quarantined until approved; stdio servers run under `docker`, `sandbox` (Landlock + rlimits on Linux, `internal/sandbox/sandbox_linux.go`) or `none` (`internal/config/config.go:831-850`).
- **Against our contract**: 1 breaks (TCP `internal/server/port.go:63`); MCP only, so it would sit behind our `/mcp` as an upstream, not replace it. 6: n/a for model traffic.
- **Maintenance**: pushed 2026-10-19T05:33Z, not archived, 396 stars; latest release v0.71.1 2026-10-17.
- **If we adopted it**: adds tool quarantine and third-party MCP aggregation; a Go daemon with UI and storage.
- **Verdict**: learn-from (quarantine-then-approve for tool definitions, and Landlock for stdio MCP servers).
- **Matrix**: token n/a (no model traffic); stream n/a; attrib partial, unit = client identity / user in server edition (`internal/clientidentity`, `internal/serveredition/users`); cost n/a; budget no; inject yes (secrets for upstream servers, `internal/secret`); allow partial (per-server isolation; Docker network settings `internal/config/config.go:443`); hook yes (quarantine, tool review `internal/toolreview`); otel yes (`go.mod` otel, otlptracegrpc, prometheus client); mcp yes. Paid-only: none stated.

### 10. ToolHive (Stacklok)

- **Repo / licence (verified)**: https://github.com/stacklok/toolhive; Apache-2.0; `gh api .../license` -> `Apache-2.0`; single `LICENSE` plus header check workflow.
- **Form**: Go CLI `thv`, a local API server, a Kubernetes operator; runs each MCP server in a container through Docker or Podman.
- **Platforms**: Linux, macOS, Windows yes (v0.55.0 assets).
- **Mechanism**: with `--isolate-network`, each MCP server container gets an internal network whose egress goes through a per-workload Squid (default) or experimental Envoy forward proxy enforcing an `:authority` allowlist, plus dnsmasq (`docs/arch/15-envoy-network-proxy.md`); authz middleware with pluggable authorizers (`docs/authz.md`); vMCP aggregates servers.
- **Against our contract**: 1: the MCP server, not the harness, is confined; needs Docker/Podman (breaks 3); its API socket is control only (`pkg/api/socket_unix.go:37`). 6: MCP side only.
- **Maintenance**: pushed 2026-10-19T22:24Z, not archived, 2299 stars; latest release v0.55.0 2026-10-19.
- **If we adopted it**: for third-party MCP servers only; adds a container runtime and two to three auxiliary containers per server.
- **Verdict**: learn-from (Envoy `:authority` RBAC note that `destination_ip` is inert in a forward proxy is worth keeping in mind for our CONNECT allowlist).
- **Matrix**: token n/a; stream n/a; attrib partial, unit = workload (one container per MCP server); cost n/a; budget no; inject yes (secrets and remote MCP auth, `docs/remote-mcp-authentication.md`); allow yes (Squid/Envoy allowlist, `docs/arch/15-envoy-network-proxy.md`); hook yes (`docs/authz.md` authorizer interface); otel yes (`docs/observability.md` Prometheus and OTLP); mcp yes. Paid-only: none stated.

### 11. Docker MCP Gateway

- **Repo / licence (verified)**: https://github.com/docker/mcp-gateway; MIT; `gh api .../license` -> `MIT`; single `LICENSE`.
- **Form**: Go CLI plugin `docker mcp`; runs MCP servers as containers; needs a Docker engine (`DOCKER_MCP_IN_CONTAINER=1` for Docker CE/WSL2 without Desktop, README:70).
- **Platforms**: Linux, macOS, Windows yes (v0.44.1 assets).
- **Mechanism**: one gateway (stdio, SSE or streaming transport) in front of containerised MCP servers; `--block-network` restricts tools to `allowHosts`; `--block-secrets` scans tool arguments and results; interceptors; signature verification of images (`docs/security.md:75-109`).
- **Against our contract**: needs Docker (no Docker on the target machine; breaks 3); TCP or stdio. MCP side only.
- **Maintenance**: pushed 2026-09-23T16:47Z, not archived, 1607 stars; latest release v0.44.1 2026-09-23.
- **If we adopted it**: requires Docker.
- **Verdict**: reject (Docker dependency); learn-from `--block-secrets` on tool I/O.
- **Matrix**: token n/a; stream n/a; attrib no; cost n/a; budget no; inject yes (`docker mcp secret`, README:251-265); allow yes (`--block-network` + `allowHosts`, `pkg/gateway/clientpool.go:535`); hook yes (interceptors, `cmd/docker-mcp/commands/gateway.go`); otel yes (`docs/telemetry/README.md`); mcp yes. Paid-only: secrets store integration is Docker Desktop (README:22,30).

### 12. ContextForge (IBM/mcp-context-forge)

- **Repo / licence (verified)**: https://github.com/IBM/mcp-context-forge; Apache-2.0; `gh api .../license` -> `Apache-2.0`.
- **Form**: Python service (`pip install mcp-contextforge-gateway`) with Rust components (languages API: Python 37.2 MB, Rust 0.85 MB); database; admin UI.
- **Platforms**: Linux, macOS, Windows yes (README gives bash and PowerShell quickstarts).
- **Mechanism**: registry and proxy federating MCP, A2A and REST/gRPC with auth, retries, rate limiting, 40+ plugins, OTel.
- **Against our contract**: TCP (`:4444`), Python runtime; MCP side only.
- **Maintenance**: pushed 2026-10-19T22:15Z, not archived, 4655 stars; latest release v1.0.11 2026-09-28.
- **Verdict**: reject (Python platform, far beyond a per-session MCP front).
- **Matrix**: token partial (README "LLM-specific metrics: Token usage, costs", tracing); stream not verified; attrib partial, unit = user-scoped tokens (README:78); cost partial (same README line); budget partial (rate limiting, README:24,78); inject yes (README:78 "X-Upstream-Authorization"); allow not verified; hook yes (plugins, README:25); otel yes (README:26,139-145); mcp yes. Paid-only: none stated.

### 13. Microsoft MCP Gateway

- **Repo / licence (verified)**: https://github.com/microsoft/mcp-gateway; MIT; `gh api .../license` -> `MIT`.
- **Form**: C# services deployed on Kubernetes (README architecture "Kubernetes Cluster", 1-click Azure deploy). No GitHub releases.
- **Platforms**: wherever Kubernetes runs; not a local binary.
- **Mechanism**: data gateway routes each MCP request to a server instance; control plane deploys MCP servers; tool router; requires MCP `2026-07-28` clients (README breaking-change note).
- **Against our contract**: Kubernetes; breaks 3; MCP side only.
- **Maintenance**: pushed 2026-10-16T23:49Z, not archived, 889 stars; no releases.
- **Verdict**: reject (cluster-scale).
- **Matrix**: token n/a; stream n/a; attrib partial (per-request authorization, README:30); cost n/a; budget not verified; inject not verified; allow n/a; hook partial (authorization layer, README:46); otel partial (README:22 "telemetry"); mcp yes. Paid-only: none stated.

### 14. ccusage (passive usage from local logs)

- **Repo / licence (verified)**: https://github.com/ccusage/ccusage (moved from ryoppippi/ccusage); `gh api .../license` -> `NOASSERTION`; read `LICENSE`: "MIT License, Copyright (c) 2025 ryoppippi".
- **Form**: CLI (`npx ccusage@latest`; repo now has a Rust workspace under `rust/`). No privileges.
- **Mechanism**: reads the session logs coding CLIs write locally (Claude Code, Codex, OpenCode, Gemini CLI, Copilot CLI, Qwen, Grok, pi and others, README "Supported Sources") and reports daily/weekly/monthly/session tokens and cost. Not a proxy.
- **Against our contract**: no network role; it would read the harness's private home after the session. It sees only what the harness logs, so it cannot enforce anything and trusts the confined process's own record.
- **Maintenance**: pushed 2026-10-19T20:04Z, not archived, 19071 stars; latest release v20.0.37 2026-10-19.
- **Verdict**: learn-from (cross-check for our metered numbers; also getagentseal/codeburn, MIT, 11521 stars, same idea).
- **Matrix**: token yes (from logs); stream n/a; attrib yes, unit = harness session as logged; cost yes; budget no; inject no; allow no; hook no; otel no; mcp no.

---

### Observability, one line each (not egress)

- Langfuse (langfuse/langfuse): MIT outside `ee/`, `web/src/ee/`, `worker/src/ee/` (`LICENSE` read; API says NOASSERTION); trace store and UI, receives OTel; not in the request path.
- OpenLLMetry (traceloop/openllmetry): Apache-2.0; OTel instrumentation SDKs for LLM clients; in-process, not a proxy. Traceloop Hub (traceloop/hub, Apache-2.0, Rust, 232 stars, pushed 2026-07-16) is its gateway: OTel and Prometheus, no budgets in README.

### Seen and not kept (name, licence as read, stars, reason)

- Portkey-AI/gateway: MIT, 13221; last push 2026-05-25, latest release v1.15.2 2026-01-12: outside the 3-month window.
- Helicone/ai-gateway: GPL-3.0, 637; out by licence; last push 2025-11-21.
- tensorzero/tensorzero: Apache-2.0, 11709; archived (API `archived: true`), last release 2026.6.0.
- higress-group/higress (alibaba/higress redirects): Apache-2.0, 9590, v2.2.5 2026-10-04; Envoy + WASM plugins incl. `ai-token-ratelimit`, `ai-quota`, `ai-statistics`, `mcp-server`, `mcp-router` (`plugins/wasm-go/extensions`); Envoy plus control plane, too heavy per host.
- Kong/kong: Apache-2.0, 44282, 3.9.4 2026-10-12; OSS repo has only `ai-proxy`, prompt decorator/guard/template, request/response transformer (`kong/plugins`); AI Rate Limiting Advanced, AI Proxy Advanced, AI MCP Proxy are "only available as part of our AI Gateway Enterprise offering" (developer.konghq.com/plugins/ai-rate-limiting-advanced/, /ai-proxy-advanced/, /ai-mcp-proxy/).
- apache/apisix: Apache-2.0, 17240, 3.19.0; OpenResty gateway with ai plugins; general API gateway, too heavy.
- kgateway-dev/kgateway: Apache-2.0, 5715, v2.4.7; Kubernetes gateway; cluster-scale.
- mlflow/mlflow AI gateway: Apache-2.0, 28452; Python ML platform component.
- vllora/vllora (langdb/ai-gateway redirects): Elastic License 2.0 (`LICENSE.md` read): out.
- ThinkWatchProject/ThinkWatch: BUSL-1.1 (`LICENSE` read): out.
- voidmind-io/voidllm: BUSL-1.1 (`LICENSE` read): out.
- archestra-ai/archestra: AGPL-3.0 plus enterprise (`LICENSE.md` read): out.
- MikkoParkkola/mcp-gateway (Rust): PolyForm Noncommercial 1.0.0 (`LICENSE` read): out.
- theopenco/llmgateway: API says NOASSERTION; not read further (TS, 1693).
- Nasiko-Labs/nasiko: `LICENSE` is Apache-2.0 text (API NOASSERTION), Rust, 9467, last release v1.0.0 2026-02-12; agent runtime platform, not a gateway.
- obot-platform/obot: MIT, 1110; MCP platform with keygen licence entitlements in code (`pkg/license/entitlements.go`, `OBOT_ENTERPRISE_` prefix; `DefaultUserLimit = 100` in `pkg/gateway/client/client.go`): open-core gate.
- mcpjungle/MCPJungle: MPL-2.0, 1303: Category B, ranked out.
- lasso-security/mcp-gateway: MIT, 396; last push 2026-01-22: stale.
- invariantlabs-ai/invariant-gateway: Apache-2.0, 82; last push 2025-11-06: stale.
- sparfenyuk/mcp-proxy: MIT, 2796, v0.13.0 2026-10-10; stdio/SSE transport bridge only, no policy.
- nyroway/nyro: Apache-2.0, 197, v1.8.3 2026-07-13; Rust desktop/server gateway for Claude Code/Codex/Gemini CLI with per-key RPM/TPM quotas; local-user tool, no session attribution or hooks beyond that.
- mozilla-ai/otari: Apache-2.0, 696, v0.29.0; Python gateway with virtual keys and budgets (README); Python service.
- diegosouzapw/OmniRoute: MIT, 78525; TS multi-provider router for free tiers; no governance focus.
- musistudio/claude-code-router: MIT, 37758; Node router for model choice, not policy.
- looplj/axonhub: Apache-2.0, 5370; Go gateway with failover and cost; general purpose.
- labring/aiproxy: MIT, 564; Go gateway; general purpose.
- traceloop/hub: see above.
- openziti/llm-gateway and openziti/mcp-gateway: Apache-2.0, 100 and 53; zero-trust overlay networking (OpenZiti) required.
- thushan/olla: Apache-2.0, 321; local-model load balancer.
- ferro-labs/ai-gateway: Apache-2.0, 272; Go gateway; general purpose.
- bitrouter/bitrouter, tiylabs/tiygate, higress-group/HiRoute, Northwood-Systems/millwright: Apache-2.0 Rust routers (235/146/274/46 stars); model routing, no per-session policy in README descriptions.
- LiteLLM-Labs/litellm-agent-control-plane: MIT, Rust, 1315; calls agents (OpenCode, Claude Managed Agents), not an egress.
- coproduct-opensource/nucleus: MIT, Rust, 22; permission envelopes with Firecracker: sandbox layer, not this one.
- kerlenton/mcpsnoop: MIT, 362; MCP traffic viewer only.
- TheLunarCompany/lunar, samanhappy/mcphub, 1mcp-app/agent, metatool-ai/metamcp, AmoyLab/Unla, agentic-community/mcp-gateway-registry: MIT/Apache MCP aggregators (TS/Python); no egress or token role.
- calebfaruki/tightbeam: AGPL-3.0: out.
- Nexus-Router/nexus: MPL-2.0, last push 2026-03-16: out.
- pydantic/pydantic-ai-gateway: AGPL-3.0, archived: out.
- Tailscale Aperture, Permit MCP Gateway, Edgee, Consus: hosted or commercial products (HN), no open repo checked.

---

### Ranked shortlist for this layer

Nothing here replaces crates/egress: no candidate listens on a unix socket, none combines model metering with serving our MCP on the same listener, and none has a label-vs-ceiling check. The value is in ideas and one crate.

1. **Busbar** (learn-from): Rust, Apache-2.0, single binary; group-tree budgets charged at stream end, tap/gate hook split, SSRF-aware OTLP. The design reference for our budgets and the place a label check would go.
2. **Agent Vault** (learn-from): MIT; placeholder-substitution MITM with per-session tokens and deny-by-default hosts. The design for credential injection on the CONNECT path (Copilot), which our tunnel cannot do.
3. **OpenAFW `afw-engine`** (borrow): Apache-2.0 crate; mask/restore of secrets across SSE frames, small dependency set.
4. **Bifrost** (learn-from): the widest open feature set (VK budgets, MCP gateway with per-key tool allowlists, Go/WASM plugins); Go and large.
5. **Claw Patrol** (learn-from): CEL over wire-level facts with deny/inspect defaults; its `run` mode confines with userns+netns+TUN like ours.

#### Spike 1: Busbar behind our egress (Linux, WSL2)

Needs: no /dev/kvm, no podman, no Node. Download only.

```sh
mkdir -p ~/spike/busbar && cd ~/spike/busbar
gh release download v1.5.5 -R GetBusbar/busbar -p 'busbar-x86_64-unknown-linux-gnu.tar.gz'
tar xzf busbar-x86_64-unknown-linux-gnu.tar.gz           # yields ./busbar (13 MB), verified listing
gh api repos/GetBusbar/busbar/contents/providers.yaml --jq .content | base64 -d > providers.yaml
cat > config.yaml <<'EOF'
listen: "127.0.0.1:18080"
providers:
  anthropic: { api_key: { env: ANTHROPIC_KEY } }
models:
  claude: { provider: anthropic, upstream_model: claude-sonnet-4-5 }
pools:
  fast: { members: [ { model: claude, weight: 1 } ] }
EOF
./busbar --validate && ANTHROPIC_KEY=... BUSBAR_CONFIG=./config.yaml ./busbar &
```

Point 1: Busbar cannot hold it alone (TCP only). Test the stack it would form: point one destination in our egress config at `http://127.0.0.1:18080/fast`, launch a harness with `amem launch`, and from inside the box confirm (a) `curl https://api.anthropic.com` fails (no network), (b) a Claude Code turn succeeds through `/run/memory/egress.sock` -> our egress -> Busbar, (c) Busbar `/metrics` (after adding a `module: prometheus` export) and our egress record the same input/output/cache token counts for a streamed turn. Then add a `groups:` limit (`{ tokens: 1000, per: day }`) on a minted key and check that the second turn is refused with 429 by Busbar while our own per-session limit is above it. Point 2: n/a (Busbar does no filesystem work); nothing to bind into the box.

#### Spike 2: Agent Vault on the CONNECT path (Linux, WSL2)

Needs: no /dev/kvm, no podman, no Node (host isolation mode; container mode needs Docker, which this machine lacks).

```sh
mkdir -p ~/spike/agent-vault && cd ~/spike/agent-vault
gh release download v0.40.0 -R Infisical/agent-vault -p 'agent-vault_0.40.0_linux_amd64.tar.gz'
tar xzf agent-vault_0.40.0_linux_amd64.tar.gz            # yields ./agent-vault (38 MB), verified listing
export AGENT_VAULT_MASTER_PASSWORD=spike-only
./agent-vault server --host 127.0.0.1 -p 14321 --mitm-port 14322 &
# then in the UI at 127.0.0.1:14321: create owner, vault, a credential for api.githubcopilot.com,
# set unmatched_host_policy=deny, and `./agent-vault agent create spike --vault default:proxy`
```

Point 1: Agent Vault listens on TCP only. Test it behind our relay: make our egress's CONNECT handler forward to `127.0.0.1:14322` with `Proxy-Authorization` carrying the spike token instead of dialling out; inside the box set `HTTPS_PROXY` to the relay port and install the vault CA into the image's trust store; confirm (a) a host outside the vault's services gets 403, (b) a Copilot CLI request reaches the API with the real token substituted while the box's environment holds only a placeholder, (c) direct egress from the box still fails. Point 2: the only new file in the box would be the vault CA certificate, read-only; check it can be bind-mounted at a fixed path (for example `/etc/ssl/certs/agent-vault.pem`) by our plan without other host paths.

---

# Part 3: who already tracks token usage, and the rest of what our egress does

## Our egress, checked against the code (main @ 0eacdb8)

Read: `crates/egress/src/usage.rs` (1-90), `crates/egress/src/telemetry.rs` (1-130, 570-600), `crates/egress/src/lib.rs` (1-30, 650-720), `crates/server/src/lib.rs:508`, `crates/memory/src/cost.rs` (head), `crates/otel/Cargo.toml`.

| Claim in the brief | Holds? | Receipt |
|---|---|---|
| Token usage from every model response, JSON and SSE: input, output, cache read, cache write | Yes, for Anthropic Messages, OpenAI Responses, OpenAI Chat Completions, **and Gemini** `usageMetadata` (the brief omits Gemini) | `usage.rs:7-14`, `usage.rs:50-82` |
| Per session | Yes; rows keyed by session, destination, model, billing (`metered` or `plan`) and source (`egress`, or `harness` for what a harness reports about a tunnelled login) | `telemetry.rs:14-33`, `:100-130`, `:174-177` |
| Call timing | Yes, `call_ms`, summed latency of the calls the egress saw | `telemetry.rs:32`, `:64` |
| A cost estimate | **Not in the egress.** The egress stores raw quantities only ("a price vector turns them into cost elsewhere"). Cost is `amem cost`: metering priced by `prices.toml` under the root, per session and per completed task | `telemetry.rs:1-3`; `crates/memory/src/cost.rs:1-10`; `crates/memory/src/main.rs:714` |
| Budgets | **Missing from the brief's list, but present**: per-session and per-day spend limits checked before a model call leaves (`over_budget`), refused as a permission error; the limit is in tokens (input+output), decided by policy (`check_spend`) | `lib.rs:654-673`; `telemetry.rs:451-465`; `crates/server/src/lib.rs:508` |
| Per-destination credential injection | Yes (`Auth::Inject { header, value }`), or pass-through for a subscription login | `lib.rs:17-19`, `:51`, `:800-805` |
| Label-vs-ceiling check before anything leaves | Yes, for forwarding (`admit`), the CONNECT tunnel, and hosted tools | `lib.rs:675-705`, `:327-336` |
| Allowlisted CONNECT tunnel | Yes; exact host or `.domain` suffix; ceiling checked when the tunnel opens; a tunnel is not metered | `lib.rs:66-70`, `:708-719` |
| OpenTelemetry export | Yes, **traces only**: one `model.call` span per call with `gen_ai.usage.input_tokens`/`output_tokens`, cache counts, session, destination, model; OTLP/HTTP protobuf via `dm-otel`. No metrics export | `telemetry.rs:576-597`; `crates/otel/Cargo.toml:11-13` (`features = ["trace"]`) |
| MCP on the same socket | Yes, `/mcp` (streamable HTTP) on the same router | `lib.rs:7`, `:492-504` |

Also present, not in the brief: a per-run host record (wall, user and sys CPU of the confined tree) beside the tokens (`telemetry.rs:69-100`).

## The matrix

Columns: **tok** token usage metering; **sse** streamed-response metering; **attr** per-session/sandbox attribution (unit named); **cost** money; **bud** budgets or rate limits; **inj** credential injection; **allow** egress allowlist; **hook** policy hook before a request leaves (where our label check could live); **otel** OpenTelemetry or metrics export; **mcp** MCP awareness.
Y yes, P partial, N no, n/a not applicable. **$** marks a capability (or the part of it named) that only a paid or enterprise edition has. Evidence: a repo path at the default branch read 20 Oct 2026 (abbreviated) or a doc page. A bare N means a code search of the repo (`input_tokens`, `opentelemetry`, `mcp`, as relevant) or its docs found nothing; the per-candidate sections above and Appendix A carry the full receipts and refs. Licence status is in the second column; out-of-bounds rows are for reference only.

| Candidate | Licence | tok | sse | attr | cost | bud | inj | allow | hook | otel | mcp |
|---|---|---|---|---|---|---|---|---|---|---|---|
| **Ours** (crates/egress) | ours | Y `usage.rs` | Y `usage.rs:9-12` | Y session `telemetry.rs:14` | P outside egress, `memory/src/cost.rs` | Y tokens per session/day `lib.rs:659` | Y `lib.rs:805` | Y CONNECT `lib.rs:710` | Y label vs ceiling `lib.rs:675` | P traces only `telemetry.rs:595` | Y `/mcp` `lib.rs:501` |
| sandbox-runtime (srt, `srt proxy`) | Apache-2.0 | N (`onResponse` gets status+headers, `src/sandbox/request-filter.ts:72`) | N | P `commandId`, violations only (README:302) | N | N | Y sentinel `mode:"mask"` (README:601); decider `cred` (docs/srt-proxy.md:158) | Y default deny (README:122) | Y external decider (docs/srt-proxy.md:114-193) | N | N |
| Codex network-proxy (+ Codex client) | Apache-2.0 | N proxy; client Y `codex-rs/otel/src/events/session_telemetry.rs:657` | N proxy; client consumes its own stream | P execution token (`network-proxy/src/attribution.rs`); client by `conversation.id` | N | N | Y `credential_broker.rs`, `mitm_hook.rs:101` | Y (README:57-66) | Y `NetworkPolicyDecider` (allowlist misses only), MITM hooks `mitm.rs:440` | Y policy events; client metric `codex.turn.token_usage` | N |
| iron-proxy | Apache-2.0 | N (gRPC `TransformResponse` could read buffered bodies) | N (buffered bodies, README:688) | P `remote_addr`; managed `principal_id` | N | N | Y `secrets` token swap (README:446-471); Vault/KMS engines $ | Y default deny + `upstream_deny_cidrs` | Y gRPC transform CONTINUE/REJECT (`proto/transform/v1/transform.proto`) | P OTLP logs only (README:1010) | Y tool allowlist (`internal/mcp/policy.go`) |
| Smokescreen | MIT | N | N | Y role from mTLS cert (`config.go:92`) | N | P global rate (README:114) | P static `add_headers` on MITM domains (`acl/v1/yaml_loader.go:35`) | Y per-role ACL | Y `PostDecisionRequestHandler` (`config.go:153`) | P Prometheus/statsd | N |
| agentgateway | Apache-2.0 | Y `crates/llm/src/lib.rs:377-432` | Y `conversion/messages.rs:894-950` | P API key; CEL key; Substrate actor (`http/substrate/README.md`) | Y built-in catalog `llm/catalog/mod.rs`, metric `gen_ai_client_cost` | Y USD/token budgets per key `http/budget/mod.rs`; token rate limits `localratelimit.rs` | Y backend auth `llm/mod.rs:1623` | Y Substrate CONNECT egress only; else declared backends | Y ext_authz, ext_proc, CEL (`http/ext_authz.rs`) | Y OTLP + Prometheus gen_ai metrics `telemetry/metrics.rs:282-406` | Y MCP gateway, per-tool CEL `mcp/rbac.rs` |
| Agent Router (ex Envoy AI Gateway) | Apache-2.0 | Y `internal/translator/anthropic_usage.go` | Y `anthropic_anthropic.go:43-101` | P header descriptor e.g. `x-tenant-id` | P CEL cost expr `internal/llmcostcel/cel.go` | Y token rate limits via Envoy Gateway; `QuotaPolicy` TODO (`api/v1alpha1/quota_policy.go:48`) | Y `internal/backendauth/` | N reverse gateway | Y Envoy filters; MCP CEL `internal/mcpproxy/authorization.go` | Y `internal/metrics/genai.go` | Y `MCPRoute` |
| LiteLLM (MIT part) | MIT outside `enterprise/` | Y `proxy/spend_tracking/` | Y `streaming_handler.py:308` | Y key/user/team; `session_id` cap `hooks/max_budget_per_session_limiter.py` | Y `model_prices_and_context_window.json` | Y key/user/team budgets, rpm/tpm; tag and per-model-per-key budgets $ | Y virtual keys; secret-manager backends $ | N | Y `async_pre_call_hook`; per-key guardrails $ | Y `integrations/opentelemetry.py`, `prometheus.py` | Y MCP gateway (OSS tier) |
| mitmproxy | MIT | P addon | P addon with `stream` | P client addr / proxyauth | N | N | P `modify_headers` | P `block_list` filter | Y addon API | N | N |
| OpenSandbox egress sidecar | Apache-2.0 | N | N | P one sidecar per sandbox, `sandbox_id` on metrics | N | N | Y Credential Vault (`docs/guides/credential-vault.md`) | Y DNS + nftables (`docs/architecture/network/egress.md`) | P mitmproxy addon script, transparent MITM only | Y OTLP metrics `components/egress/pkg/telemetry/metrics.go` | N |
| E2B (open runtime) | Apache-2.0 | N | N | P sandbox id, connections only (ClickHouse migration; writer not in open repo) | N | P per-sandbox connection cap (`tcpfirewall/proxy.go:291`) | P hosted egress proxy only; open `tcpfirewall` has no CA; BYOC $ | Y `allowOut`/`denyOut` (docs.e2b.dev/sandbox/internet-access) | N | Y `tcpfirewall/metrics.go` | P in-sandbox `mcp-gateway` template |
| kubernetes-sigs/agent-sandbox | Apache-2.0 | N | N | N | N | N | N (example uses nono) | P NetworkPolicy L3/L4 (`sandboxtemplate_types.go:84-106`) | N (CNI's) | P controller Prometheus | N |
| microsandbox | Apache-2.0 | N | N | P per-sandbox bytes (`docs/observability/msb-metrics.mdx`) | N | P bandwidth/conn limits, local only | Y placeholder secrets (`docs/sandboxes/secrets.mdx`) | Y rules, `defaultDeny()` | N | Y OTLP resource metrics | N |
| Docker Sandboxes (reference) | proprietary | N | N | P sandbox name; audit principal $ | N | N | Y sentinel swap (docs.docker.com/ai/sandboxes/architecture/) | Y presets + `sbx policy allow network`; org policy $ | N (interactive approval) | N; audit logs $ | Y per-sandbox MCP gateway; Cedar MCP policy $ |
| coder/httpjail | CC0-1.0 | N | N | P per wrapped command / requester IP | N | N | N | Y default deny | Y JS / shell / line-processor rule engines | N | N |
| coder/boundary | MIT (Coder side $) | N (Coder AI Gateway records it, Premium $) | N | Y session UUID + sequence (`config/session_correlation.go`) | N | N | N | Y default deny | N | N | N (AI Gateway $) |
| strongdm/leash | Apache-2.0 | N | N | P per run | N | N | Y Cedar `HttpRewrite` header set (prototype) | P Cedar `NetworkConnect`, permissive default | Y Cedar before connect and MCP call | P OTel SDK, remote export not implemented | Y Cedar `McpCall` (forbid enforced) |
| gondolin | Apache-2.0 | N (`onResponse` could) | N (streaming `Response` available) | P per VM | N | N | Y placeholder secrets (`docs/secrets.md`) | Y `allowedHosts` (omitted = allow all) | Y `onRequest`/`isRequestAllowed` (`host/src/http/hooks.ts`) | N | N |
| **Agent Substrate** (Part 1) | Apache-2.0 | N | N | Y actor identity (SPIFFE cert) on traffic (`docs/network-egress.md`) | N | N | Y `replace_headers` (`docs/egress-credential-injection.md`) | Y `EgressPolicy`, DNS filtered (`docs/egress-traffic.md`) | Y replaceable PEP (`docs/network-egress.md`) | P system metrics/traces; actor correlation on roadmap (`docs/observability.md`) | N (MCP servers can run as actors) |
| NVIDIA OpenShell | Apache-2.0 | N | N | P per sandbox and calling binary, decisions only | N | N | Y placeholder (`docs/how-it-works/inference.mdx`) | Y default deny L7 | Y supervisor middleware gRPC (`proto/supervisor_middleware.proto`) | Y `crates/openshell-otel`, gateway Prometheus | Y `l7/mcp.rs` |
| sandlock | Apache-2.0 | N | N | n/a | N | N | Y `--credential` (needs MITM CA) | Y default deny (`docs/network.md`) | Y `policy_fn` (`docs/policy-fn.md`) | N | N |
| matchlock | MIT | N | N (SSE transform only) | n/a | N | N | Y `--secret NAME@host` | Y `--allow-host` | Y before/after rules (`docs/network-interception.md`) | N | N |
| trailofbits/coop (coop-proxy) | Apache-2.0 | N | P passes streams through | P per-instance capability token | N | N | Y strip + inject (`coop-proxy/src/proxy.rs`) | P model API only; guest egress open | N | N | N |
| canyonroad/agentsh | Apache-2.0 | Y `internal/proxy/usage.go` | Y `ExtractSSEUsage` (`usage.go:143-186`) | Y agentsh session (`llm-requests.jsonl`) | N | Y RPM/TPM buckets `llm_ratelimit.go` | Y `credshook.go` | Y `http_services` fail-closed | Y pre/post hooks `hooks.go` | Y `pkg/observability/` | Y `mcp_intercept.go` |
| Busbar | Apache-2.0 | Y `crates/busbar/src/billing.rs` | Y `proxy/response_body.rs` | P virtual key -> group (one key per session works) | Y `rate_card` | Y group-tree limits incl. tokens and budget | Y gateway-held keys | N | Y tap/gate hooks (`docs/hooks.md`) | Y OTLP + Prometheus | N (not implemented) |
| Infisical Agent Vault | MIT (no `ee/` present) | N | n/a | P agent token / `vault run` session | N | P request rate (`internal/ratelimit/`) | Y placeholder MITM | Y `unmatched_host_policy=deny` | P per-host service rules | N | P routes MCP traffic, no parsing |
| Bifrost | Apache-2.0 | Y `plugins/governance/tracker.go` | Y `tracker.go:144` | P virtual key -> team -> customer | Y `framework/modelcatalog` | Y per VK/team/customer | Y | P model and MCP-tool allowlists, no hosts | Y `PreLLMHook`/`PreMCPHook` | Y OTLP (`docs/features/otel.mdx`) | Y; Virtual MCPs, guardrails, RBAC $ |
| AISIX OSS (API7) | Apache-2.0 | Y `crates/aisix-obs/src/usage.rs` | Y (README) | P key/team/member + tags | Y USD per pricing snapshot | P rate/token limits Y; money budgets $ (README) | Y `provider_keys` | N | P guardrail hooks | Y Prometheus + OTLP | Y MCP gateway |
| OpenAFW (`afw-engine`) | Apache-2.0 | N | n/a (rewrites across SSE, `afw-engine/src/sse.rs`) | P per-agent local token | N | N | Y vault + local tokens | N | P remote verdicts $ | N | N |
| Claw Patrol (Deno) | MIT | N | n/a | P wrapped process / WireGuard peer | N | N | Y `credential` blocks | Y `unknown_host = "deny"` | Y CEL + human approval | P | P |
| Plano (ex archgw) | Apache-2.0 | Y `stream_context.rs:791` | P (Bedrock stream estimated) | P header selector | P `session_cost_usd` for routing | P token rate limits | Y `access_key` | N | Y filter chains | Y | P |
| GoModel | MIT (may change, `docs/about/license.mdx`) | Y | not verified | Y label from header (`docs/features/labelling.mdx`) | Y | Y | Y | N | Y plugins | Y | Y |
| MCPProxy | MIT | n/a | n/a | P client identity | n/a | N | Y upstream secrets | P per-server isolation | Y quarantine / tool review | Y | Y |
| ToolHive | Apache-2.0 | n/a | n/a | P per MCP workload | n/a | N | Y | Y Squid/Envoy `:authority` allowlist | Y authorizers (`docs/authz.md`) | Y | Y |
| Docker MCP Gateway | MIT | n/a | n/a | N | n/a | N | Y `docker mcp secret`; Desktop store $ | Y `--block-network` + `allowHosts` | Y interceptors | Y | Y |
| IBM ContextForge | Apache-2.0 | P (README "token usage, costs") | not verified | P user-scoped tokens | P | P rate limiting | Y | not verified | Y plugins | Y | Y |
| ccusage (passive, from logs) | MIT | Y from harness logs | n/a | Y harness session as logged | Y | N | N | N | N | N | N |

**Which comes closest, and what slice it could take.** On metering, **agentgateway** comes closest. It is Apache-2.0, a single Rust binary for Linux, macOS and Windows, and it reads input, output, cache-read and cache-write tokens from JSON and streamed Anthropic and OpenAI responses. It prices them from a built-in catalog, enforces USD or token budgets and token rate limits, injects backend credentials, exposes ext_authz/ext_proc/CEL hooks, exports gen_ai metrics over OTLP and Prometheus, and gates MCP per tool. It also ships a Substrate PEP, so it is the PEP that Agent Substrate's own demos pair with. What it lacks for us: a unix-socket data listener, a session unit (its unit is an API key, or a CEL expression we would feed a session header), our label-vs-ceiling rule (an ext_authz call back to us), and serving *our* memory as `/mcp` (it would front it, not replace it). The slice it could take is the **model-forwarding leg**: provider translation, cost pricing and the money budgets that we compute today outside the egress (`amem cost`), with our egress kept as the socket end, label check, MCP server and CONNECT tunnel, forwarding to agentgateway on host loopback with one virtual key or session header per session. Running it this way moves "the only process with the network" from our egress to a second daemon and meters every call twice, so it is worth it only if we want its catalog, budgets in money and Prometheus metrics more than one fewer process. Two others are close in shape. **canyonroad/agentsh** is the only open sandbox launcher whose built-in proxy meters SSE usage per *session* and rate-limits by tokens; it is Go and coupled to its own FUSE/ptrace launcher, so it is a reference rather than a component. **Busbar** is the closest Rust single binary to our egress's forwarding and budget half (tokens charged at stream end, tap/gate hooks), but it has no MCP and no allowlist. No candidate in the four reports or here combines a unix-socket listener, per-session metering, MCP on the same listener, and a pre-egress label check. The sandbox-side proxies (srt, iron-proxy, OpenShell, gondolin, microsandbox, Docker Sandboxes) cover injection, allowlist and the hook but **none counts tokens**. Their pre-request hooks are where a ceiling check fits: srt's decider, iron-proxy's gRPC transform, OpenShell's supervisor middleware.

Correction to image-and-egress.md B3: Smokescreen does terminate TLS for `mitm_domains` and can add static headers there (`acl/v1/yaml_loader.go:35-43`), and its `PostDecisionRequestHandler` can deny (`pkg/smokescreen/config.go:153-157`). The earlier note said it had "no TLS termination, no injection".

---

# Appendix A: matrix evidence for candidates already in the four reports

### 1. anthropic-experimental/sandbox-runtime (ref 642424c6, Apache-2.0)

Component: `srt` HTTP + SOCKS5 proxies (src/sandbox/http-proxy.ts, socks-proxy.ts) and standalone `srt proxy` with an external decider (docs/srt-proxy.md).

- token: no — no response-body reading; the library's `onResponse` observer gets status and headers only (`src/sandbox/request-filter.ts:72` `onResponse?: (status, headers) => void`); `gh search code` for `input_tokens` returns nothing.
- stream: no — as above, no body observer on responses.
- attrib: partial — violations (proxy denies, seatbelt/seccomp events) are keyed by a per-invocation `commandId` (README.md:302), and per-command allow lists are registered by `commandId` (README.md:361-379); no traffic or usage accounting per command.
- cost: no — nothing in README or src.
- budget: no — only resource caps: `tlsTerminate.maxTunnels` (README.md:478) and decider body/frame limits (docs/srt-proxy.md:303-305); no request-rate or spend limits.
- inject: yes — `credentials` `mode: "mask"` puts a sentinel in env/files and swaps the real value back on egress to `injectHosts`, only over terminated TLS unless `allowPlaintextInject` (README.md:601); `srt proxy` decider verdict can carry `cred`/`credential`, which replaces every client credential header (docs/srt-proxy.md:158-180).
- allow: yes — default deny, `network.allowedDomains` / `deniedDomains` with `:port`, plus resolved-address deny set dialled as checked (README.md:122, 458-470).
- hook: yes — `srt proxy` asks an external decider per request (`req` with method/host/port/sni/path/query/headers; `allow`/`deny`/`need_body`, allow may set/remove headers) before dialling (docs/srt-proxy.md:114-193); library form: `network.filterRequest` callback, throwing denies (request-filter.ts:76-84, README.md:496); `SandboxAskCallback` for unlisted hosts (README.md:321).
- otel: no — `gh search code` for `opentelemetry` and `prometheus` returns nothing; telemetry is the in-process `SandboxViolationStore` and debug log (README.md:48, 391).
- mcp: no — not MCP-aware; README shows wrapping an MCP server process in the sandbox (README.md:50-96), the proxy does not parse MCP.
- enterprise-only: none; single Apache-2.0 package (package.json `"license": "Apache-2.0"`, prior notes).
- attribution unit: `commandId` (caller-chosen per invocation, e.g. a tool-use id) for violations and per-command allow lists; the decider sees each request but the frame carries no client or session identity (docs/srt-proxy.md:119-131), so one `srt proxy` per session is the unit.

### 2. openai/codex — codex-rs/network-proxy, codex-rs/responses-api-proxy (ref e05028e7, Apache-2.0)

- token: no (proxy) — network-proxy has no usage extraction (file list at e05028e7: no usage module; README has none); responses-api-proxy forwards and can `--dump-dir` request/response pairs (responses-api-proxy/README.md:6-11), no counting. The Codex client itself records usage: `gen_ai.usage.input_tokens`, `gen_ai.usage.cache_read.input_tokens`, `gen_ai.usage.cache_write.input_tokens`, `gen_ai.usage.output_tokens`, `codex.usage.reasoning_output_tokens` on the responses span (codex-rs/otel/src/events/session_telemetry.rs:657-670) and metric `codex.turn.token_usage` (otel/src/metrics/names.rs:41), tagged `conversation.id` (session_telemetry.rs:425-428).
- stream: no (proxy) — see above; the client-side figures come from the streamed Responses events Codex consumes itself.
- attrib: partial — connections carry an attribution token (`CODEX_NETWORK_PROXY_ATTRIBUTION`, framed `\0CDXPXY1`) resolved to an execution scope (network-proxy/src/attribution.rs:12-60); `NetworkProxy::for_execution(environment_id, execution_id, attribution_token, ...)` (src/proxy/execution_scope.rs:19-52); OTel policy events carry `conversation.id`, `user.account_id` (README.md:203-210). Policy decisions are attributed, traffic volume and usage are not.
- cost: no — nothing in network-proxy or responses-api-proxy.
- budget: no — no rate or token limits in network-proxy README or file list.
- inject: yes — `credential_broker` with providers `github.rs`, `openai.rs` and dummy env keys (src/credential_broker.rs, `brokered_credential_dummy_env_keys`); MITM hooks `inject_request_headers` from env var or file (`SecretSource::EnvVar | File`, src/mitm_hook.rs:101-112); responses-api-proxy holds the API key read from stdin, or GitHub OIDC-minted short-lived credentials (responses-api-proxy/README.md:37-51, 63-112).
- allow: yes — allowlist-first domains, deny wins, private/local ranges blocked including after DNS, limited mode GET/HEAD/OPTIONS only (README.md:57-66, 242-256).
- hook: yes — embedder `NetworkPolicyDecider` (async), but it can only override allowlist misses, not denies (README.md:181-189); declarative MITM hooks match host/method/path/query/headers/body and a hooked host with no matching hook is blocked (`HookEvaluation::HookedHostNoMatch` -> `REASON_MITM_HOOK_DENIED`, src/mitm.rs:440-446).
- otel: yes — `codex.network_proxy.policy_decision` events, target `codex_otel.network_proxy`, when embedded in managed Codex runtime (README.md:191-228); Codex client exports OTLP traces/metrics/logs (codex-rs/otel/src/otlp.rs).
- mcp: no — `gh search code --repo openai/codex mcp` returns no network-proxy path.
- enterprise-only: none in the repo; the OTel audit section is labelled "embedded/managed" (README.md:191), meaning when run inside the Codex runtime, not a paid tier.
- attribution unit: execution attribution token -> (environment_id, execution_id); OTel events by `conversation.id`. Client-side usage by `conversation.id`.

### 3. ironsh/iron-proxy (ref 7c6a6e92, Apache-2.0)

- token: no — no usage extraction for proxied traffic; the only token counts are the LLM judge transform's own calls (`judge.input_tokens`, `judge.output_tokens`, README.md:600). A gRPC `TransformResponse` could read a buffered response body (proto/transform/v1/transform.proto:13, 83-91).
- stream: no — body transforms and gRPC transforms operate on buffered bodies (README.md:688); SSE is handled per event only for MCP `tools/list` filtering (README.md:639).
- attrib: partial — audit log fields `host, method, path, remote_addr, sni, mode, action, status_code` per request (internal/transform/audit.go:31-38); no session field. Response-retry handler is told `IRON_RESPONSE_RETRY_HANDLER_SANDBOX_ID` (README.md:294-299). Control-plane sync returns a `PrincipalID` per proxy (internal/controlplane/client.go:28-29).
- cost: no — nothing in README or file list.
- budget: no — `gh` tree at 7c6a6e92 has no limiter; README limits are body-size caps (`max_request_body_bytes`, `max_response_body_bytes`).
- inject: yes — `secrets` transform swaps proxy tokens for real secrets in headers (optionally query, path, body), `require: true` rejects requests without the proxy token (README.md:222, 271-274, 446-471); resolvers env, Vault KV, 1Password, 1Password Connect, control plane (internal/transform/secrets/*); OAuth, GCP, AWS SigV4, HMAC signing transforms.
- allow: yes — default-deny allowlist with host/method/path rules, `upstream_deny_cidrs` after DNS (README.md:32, 378; internal/transform/allowlist/allowlist.go).
- hook: yes — gRPC transform service `TransformRequest`/`TransformResponse` with `TRANSFORM_ACTION_CONTINUE | REJECT` (proto/transform/v1/transform.proto:12-20, 70-91); also an LLM `judge` transform (internal/transform/judge/).
- otel: partial — audit events as OTLP log records when `OTEL_EXPORTER_OTLP_ENDPOINT` is set (README.md:1010-1040); no metrics: the metrics server serves only `/healthz` (internal/metrics/server.go:27).
- mcp: yes — MCP Streamable HTTP parsing, default-deny tool allowlist, `tools/list` filtering incl. SSE, MCP gateway package (README.md:607-681; internal/mcp/policy.go, internal/mcpgateway/gateway.go).
- enterprise-only: iron.sh "dynamic Vault secret engines, KMS backends, a Kubernetes operator, or centralized policy management" (README.md "## iron.sh"); control-plane managed mode talks to the iron.sh REST API (internal/controlplane/client.go:34-35). OTLP export and the management API are "Standalone mode only" (README.md:1061).
- attribution unit: client `remote_addr` in the audit record, or one proxy per sandbox; control-plane `principal_id` per proxy in managed mode.

### 4. stripe/smokescreen (ref bc3bf31d, MIT)

- token: no — no body parsing beyond MITM header add; nothing in README/flags.
- stream: no.
- attrib: yes (per role) — role from the client's mTLS certificate via `RoleFromRequest` (pkg/smokescreen/config.go:92); `role` on every log line and metric tag (pkg/smokescreen/smokescreen.go:56, 954, 1435); conntrack stats per connection carry `role` (conntrack/stats.go:6). Traffic, not usage.
- cost: no.
- budget: partial — global `--max-request-rate` and `--max-concurrent-requests` (README.md:114-115; rate_limiter.go token bucket), concurrent CONNECT cap (tunnel_limiter.go); not per role, not tokens or spend.
- inject: partial — ACL `mitm_domains[].add_headers` adds static headers on MITM'd domains per role (acl/v1/yaml_loader.go:35-43); values come from the ACL file; client credentials are not stripped by that rule.
- allow: yes — per-role hostname ACL (`allowed_domains`, `global_deny_list`, `global_allow_list`) and private ranges refused after resolution unless allowed (README.md:7-12, 85-88, 111; yaml_loader.go:21-36).
- hook: yes (library) — `PostDecisionRequestHandler func(*http.Request) error` runs after the ACL allows; an error denies (config.go:153-157); also `AcceptResponseHandler` (config.go:142), `UpstreamProxySelector`.
- otel: partial — Prometheus endpoint (`--expose-prometheus-metrics`, README.md:90-98) and statsd (README.md:101); no OTLP found in the file list.
- mcp: no.
- enterprise-only: none (MIT, single repo).
- attribution unit: role (mTLS cert CN/OU by default, or whatever `RoleFromRequest` returns).
## Matrix, group B (gateways and intercepting proxies), verified 2026-10-20

Refs are the default-branch HEAD read today via `gh api repos/O/R/commits/main`.

### 5. agentgateway/agentgateway (ref af2e4e1, Apache-2.0)

- token: yes — `LLMResponse` carries input, output, cache-read, cache-creation (incl. 1h), reasoning tokens (`crates/llm/src/lib.rs:377-432`@af2e4e1); filled per provider in `crates/llm/src/conversion/*.rs`.
- stream: yes — Anthropic `MessageDelta` usage read in `translate_stream` and `passthrough_stream` (`crates/llm/src/conversion/messages.rs:699,894-950,1113-1199`@af2e4e1); SSE parser `crates/llm/src/parse/sse.rs`; TTFT and inter-chunk latency recorded (`lib.rs:420-440`).
- attrib: partial — per API key (budgets keyed `api-key:<id>:<budget>`, `crates/agentgateway/src/http/budget/mod.rs:298-302`); rate-limit bucket key is a CEL expression over the request, so a session header can be the unit (`crates/agentgateway/src/http/localratelimit.rs:46-56`); on the CONNECT egress frontend, per Substrate actor (`ate.actor.name`, `ate.atespace` in access log; `crates/agentgateway/src/http/substrate/README.md`). No built-in session concept.
- cost: yes — built-in price catalog `catalog/model-catalog.json`, cost projection with cache-read/write rates (`crates/agentgateway/src/llm/catalog/mod.rs:27-28,476-506`); metric `gen_ai_client_cost` "Cumulative USD cost" (`crates/agentgateway/src/telemetry/metrics.rs:401-406`); docs https://agentgateway.dev/docs/standalone/latest/documentation/llm/cost-controls/ ("Price LLM requests with a model cost catalog").
- budget: yes — per-API-key budgets in USD or tokens over rolling windows, persisted to SQLite/Postgres (`http/budget/mod.rs:123-298,491-531`, `budget/sqlite_schema.sql`); local token or request rate limits (`localratelimit.rs:62-69` `RateLimitType::{Requests,Tokens}`); remote rate limit (`http/remoteratelimit.rs`).
- inject: yes — backend auth (provider key held by the gateway; key moved to `x-api-key` for Anthropic, `crates/agentgateway/src/llm/mod.rs:1623-1690`); `BackendAuth` in `controller/api/v1alpha1/agentgateway/agentgateway_policy_types.go`; Substrate egress `replace_headers` swaps placeholder values (`http/substrate/README.md`).
- allow: yes — CONNECT frontend with per-actor egress policy, unmatched name/port returns 403 before routing, DNS resolved by the gateway, original port enforced (`http/substrate/README.md`, `http/substrate/egress.rs`); otherwise routes only to declared backends. Policy source is the Substrate control API, not a local list.
- hook: yes — `ext_authz` and `ext_proc` (`http/ext_authz.rs`, `http/ext_proc.rs`), CEL authorization (`http/authorization.rs`), CEL transformation (`http/transformation_cel.rs`), guardrail webhooks incl. streaming (`llm/policy/webhook.rs`, `llm/policy/streaming_guardrails.rs`).
- otel: yes — OTLP span exporter (`crates/agentgateway/src/telemetry/trc.rs:10,248`); Prometheus `gen_ai_client_token_usage`, `gen_ai_client_cost`, TTFT, time per output token (`telemetry/metrics.rs:282-406`).
- mcp: yes — MCP gateway over stdio/SSE/streamable HTTP (`crates/agentgateway/src/mcp/upstream/{stdio,sse,streamablehttp}.rs`), CEL authorization per tool/prompt/resource (`mcp/rbac.rs:15`), MCP guardrails (`mcp/guardrails/`), MCP auth (`mcp/auth.rs`).
- enterprise-only: none found; README badge Apache-2.0 only, and the cost-controls and LLM doc pages read today name no paid tier. A Solo.io enterprise distribution was not checked.
- attribution unit: API key (budgets, cost dashboard "by model, provider, and user"); any CEL-derived key for rate limits (header, JWT claim, source); Substrate actor on CONNECT egress.

### 6. theagentrouter/agent-router, formerly envoyproxy/ai-gateway (ref 9d32e9c, Apache-2.0)

- token: yes — ext_proc translators extract input, output, cached-input, cache-creation, reasoning tokens (`internal/translator/anthropic_anthropic.go:104-119`@9d32e9c, `internal/translator/anthropic_usage.go`); exposed as `llmRequestCosts` types `InputToken`, `OutputToken`, `TotalToken`, `CachedInputToken`, `CacheCreationInputToken` (`api/v1beta1/ai_gateway_route.go:100-122`).
- stream: yes — streaming branch accumulates `streamingTokenUsage` from buffered SSE events (`internal/translator/anthropic_anthropic.go:43-101`); stream folding `internal/apischema/anthropic/stream_fold.go`.
- attrib: partial — token counts written to Envoy dynamic metadata (`io.envoy.ai_gateway`) and consumed by Envoy Gateway rate-limit descriptors keyed on a request header, the doc example being `x-tenant-id` (`api/v1beta1/ai_gateway_route.go:100-140`); request headers can be mapped to metric/span attributes (`internal/requestheaderattrs/`). Unit is whatever header the client sends; no session concept.
- cost: partial — no price table; a CEL expression over model, backend, route_name and token counts yields a cost number (`internal/llmcostcel/cel.go`, `LLMRequestCost` type CEL).
- budget: yes — token rate limits through Envoy Gateway `BackendTrafficPolicy` reduced by captured costs (`api/v1beta1/ai_gateway_route.go:124-140`); `QuotaPolicy` per AIServiceBackend/model is partial: "the descriptor set is not being set in the Envoy Configuration ... TODO" (`api/v1alpha1/quota_policy.go:48-52`).
- inject: yes — `BackendSecurityPolicy` with API key, Anthropic key, AWS, Azure, GCP credentials and credential override (`internal/backendauth/{api_key,anthropicapikey,aws,azure,gcp,credential_override}.go`).
- allow: no — reverse gateway to declared `AIServiceBackend`s; no client-facing CONNECT allowlist. It can send its own upstream traffic through an external CONNECT forward proxy (`internal/extensionserver/forward_proxy.go`).
- hook: yes — Envoy filter chain (ext_authz via Envoy Gateway `SecurityPolicy`, not verified in this repo today); MCP CEL authorization (`internal/mcpproxy/authorization.go`); body/header mutators (`internal/bodymutator/`, `internal/headermutator/`).
- otel: yes — OTel GenAI semconv metrics `gen_ai.client.token.usage`, `gen_ai.server.request.duration`, TTFT, time per output token (`internal/metrics/genai.go:14-26,75-86`); tracing incl. OpenInference (`internal/tracing/`).
- mcp: yes — `MCPRoute` CRD (`api/v1beta1/mcp_route.go`), MCP proxy with sessions, SSE, fan-out, CEL authorization requiring verified JWT for claims (`internal/mcpproxy/{mcpproxy,session,sse,authorization}.go`); `mcp_metrics.go`.
- enterprise-only: none; whole repo Apache-2.0 (gh api licence), Agentic AI Foundation project per README (re-verified in Part B notes).
- attribution unit: request header chosen as rate-limit descriptor (e.g. `x-tenant-id`), plus route, backend, model; requires Envoy Gateway on Kubernetes for limits.

### 7. BerriAI/litellm, MIT part (ref 09e9193, NOASSERTION: MIT outside `enterprise/`)

- token: yes — usage per call into spend logs (`litellm/proxy/spend_tracking/spend_tracking_utils.py`@09e9193, `litellm/proxy/hooks/proxy_track_cost_callback.py`).
- stream: yes — `stream_options.include_usage` handling and usage-only chunks (`litellm/litellm_core_utils/streaming_handler.py:308-309,471-472,512`); chunk rebuild `streaming_chunk_builder_utils.py`; pass-through streams logged (`litellm/proxy/pass_through_endpoints/streaming_handler.py`).
- attrib: yes — virtual key, user, team, end_user, tag; per-session dollar cap keyed by `session_id` / `x-litellm-trace-id` (`litellm/proxy/hooks/max_budget_per_session_limiter.py:1-12,179-188`); Prometheus labels `hashed_api_key`, `user`, `team`, `end_user` (opt-in) (https://docs.litellm.ai/docs/proxy/prometheus).
- cost: yes — price map `model_prices_and_context_window.json`, `litellm/cost_calculator.py`; MCP tool cost `litellm/proxy/_experimental/mcp_server/cost_calculator.py`.
- budget: yes — per-key/user/team budgets and rpm/tpm limits are OSS ("Budgets and rate limits", https://docs.litellm.ai/docs/enterprise); hooks `parallel_request_limiter_v3.py`, `dynamic_rate_limiter_v3.py`, `max_budget_per_session_limiter.py`, `max_iterations_limiter.py` under `litellm/proxy/hooks/`. Tag budgets and per-model budgets per key are enterprise.
- inject: yes — clients hold virtual keys; provider keys live in proxy config/DB. Secret-manager backends for those keys are enterprise.
- allow: no — model gateway, not an egress proxy; per-key model lists only.
- hook: yes — `CustomLogger.async_pre_call_hook` and `async_moderation_hook` can reject (`litellm/integrations/custom_logger.py:417,511`); OSS custom guardrails. Guardrails per key/team and built-in guardrail callbacks are enterprise (`enterprise/enterprise_hooks/`, `enterprise/litellm_enterprise/enterprise_callbacks/`).
- otel: yes — `litellm/integrations/opentelemetry.py` with GenAI semconv (`opentelemetry_utils/gen_ai_semconv.py`); Prometheus `litellm/integrations/prometheus.py`, not marked enterprise on the Prometheus page except managed-batch metrics; no `premium_user` check in `prometheus.py` or `opentelemetry.py` (grep today).
- mcp: yes — MCP gateway with per-key access and client allowlist (`litellm/proxy/_experimental/mcp_server/client_allowlist.py`, `auth/managed_agent_access.py`); listed in the OSS tier on https://docs.litellm.ai/docs/enterprise.
- enterprise-only (https://docs.litellm.ai/docs/enterprise, read today): tag-based budgets, model-specific budgets per key, programmatic spend reports, team-based logging and opt-out, GCS/Azure Blob log export, guardrails per key/team and built-in guardrail callbacks, audit logs, secret managers, RBAC, SSO; managed-batch Prometheus metrics. Code under `enterprise/` (e.g. `enterprise/litellm_enterprise/proxy/audit_logging_endpoints.py`, `.../auth/custom_sso_handler.py`).
- attribution unit: virtual API key (plus user, team, end_user, tag); `session_id` / trace id for the per-session budget hook.

### 8. mitmproxy/mitmproxy (ref 696d4e2, MIT)

- token: partial — nothing built in; an addon `response` hook can parse usage from bodies (addon list at `mitmproxy/addons/`@696d4e2 has no LLM addon).
- stream: partial — SSE is buffered and "swallowed if there's no streaming" (`mitmproxy/addons/server_side_events.py`); `stream_large_bodies` (`mitmproxy/addons/proxyserver.py:165`) or `flow.response.stream` callable lets an addon see chunks.
- attrib: partial — client address per flow, or `proxyauth` username (`mitmproxy/addons/proxyauth.py`); an addon would map it to a session.
- cost: no — none built in.
- budget: no — none built in; addon only.
- inject: partial — `modify_headers` option sets headers per filter (`mitmproxy/addons/modifyheaders.py:61-64`), `upstream_auth` for upstream proxy; no placeholder-swap logic.
- allow: partial — `block_list` with a flow filter (e.g. negated `~d`) returns a status for non-matching hosts (`mitmproxy/addons/blocklist.py`); `block_private`/`block_global` concern client addresses (`mitmproxy/addons/block.py:10-19`); `allow_hosts` only selects what is intercepted (`options.py:93`). No resolved-address guard.
- hook: yes — Python addon API on `request`/`response`, can kill or rewrite a flow (`mitmproxy/addons/script.py`).
- otel: no — no OpenTelemetry or Prometheus in the repo (`gh search code --repo mitmproxy/mitmproxy opentelemetry`: no results).
- mcp: no — no MCP handling.
- enterprise-only: none (MIT, no paid edition).
- attribution unit: client IP/port or proxy-auth username, via an addon.
## Matrix, group C: sandbox platforms with an egress piece (verified 2026-10-20)

Paths are at the named ref (default-branch HEAD on 2026-10-20). "S@" = the section's ref.

### 9. opensandbox-group/OpenSandbox (ref 0548e18, Apache-2.0)

component: `components/egress` (Go sidecar sharing the sandbox netns: DNS proxy + nftables default-deny, optional transparent mitmproxy with Credential Vault); design in `docs/architecture/network/egress.md`.

- token: no — no token/usage parsing in the egress; `gh search code` for `input_tokens` and `prompt_tokens` in the repo returns nothing; mitmscripts are policy/TLS/credential only (`components/egress/mitmscripts/*`@0548e18).
- stream: no — nothing metered; only a note that SSE bodies over ~1 MB can truncate on origin RST (`docs/architecture/network/egress.md`@0548e18, "Scope notes").
- attrib: partial — one sidecar per sandbox; OTel metrics carry `sandbox_id` from `OPENSANDBOX_EGRESS_SANDBOX_ID` (`docs/guides/telemetry.md`@0548e18 l.299), but the counters are policy denials / DNS / nft events, not usage; no destination label ("no queried domain or destination ever becomes a label", egress.md l.120).
- cost: no — nothing in egress.
- budget: no — no request/token limits in the egress; `rate limit` hits in the repo are K8s client / node-agent (`server/opensandbox_server/services/k8s/rate_limiter.py`), none under `components/egress`.
- inject: yes — Credential Vault: "Real credentials are written to the egress sidecar ... the sandbox process only receives fake or empty credential values ... injects the required authentication headers" (`docs/guides/credential-vault.md`@0548e18 l.8); requires `dns+nft` mode and transparent MITM; `components/egress/pkg/credentialvault/`, `mitmscripts/credential_bound.py`.
- allow: yes — FQDN/wildcard/IP/CIDR rules, DNS NXDOMAIN for denied names, nftables "drops everything not explicitly allowed" in `dns+nft`; platform `deny.always`/`allow.always` overlays and `reachable.ports.cap`; fail-closed startup (`docs/architecture/network/egress.md`@0548e18 "Capabilities", "How interception works"). Default is per policy `defaultAction`, not forced deny.
- hook: partial — operator mitmproxy addons via `OPENSANDBOX_EGRESS_MITMPROXY_SCRIPT` get "the full L7 hook surface (hostname, method, URI path, headers, bodies)" after the built-in addon (egress.md l.76); only with transparent MITM (experimental config surface). Deny events also go to a webhook (`components/egress/pkg/events/webhook.go`), which is notification, not a decider.
- otel: yes — OTLP metrics, meter `opensandbox/egress`: `egress.policy.denied_total`, `egress.dns.*`, `egress.nftables.*`, `egress.mitm.tls.connections_total`, process CPU/mem (`docs/guides/telemetry.md`@0548e18 l.210-227; `components/egress/pkg/telemetry/metrics.go`). No per-request spans.
- mcp: no — MCP in the repo is a control-plane server for managing sandboxes (`sdks/mcp/sandbox/python/src/opensandbox_mcp/server.py`); the egress has no MCP awareness.
- enterprise-only: none found; single Apache-2.0 repo, README mentions no paid edition (README.md@0548e18 grep for enterprise/commercial/paid: only a registry line).
- attribution unit: sandbox (one sidecar per sandbox, `sandbox_id` OTel attribute when the platform sets it); credential bindings are per sandbox vault.

### 10. e2b-dev/E2B (ref dc625eb, Apache-2.0) + e2b-dev/runtime (ref 81df066, Apache-2.0)

component: open edition `packages/orchestrator/pkg/tcpfirewall` (host-side TCP proxy per node, iptables-redirected, SNI/Host matching) wired by `defaultEgressFactory` in `packages/orchestrator/main.go`@81df066; interface `packages/orchestrator/pkg/sandbox/network/egressproxy.go`. `factories/run.go` l.279: "EgressFactory builds an edition-specific egress proxy". The open tcpfirewall returns `CABundle() == ""` and `SupportsBYOP() == false` (`tcpfirewall/proxy.go` l.198-204): no TLS termination in the open build.

- token: no — tcpfirewall sees connections only (SNI/Host), no bodies (`tcpfirewall/handlers.go`@81df066).
- stream: no — same.
- attrib: partial — per-sandbox connection verdicts: OTel `connectionsPerSandbox` histogram (`tcpfirewall/metrics.go`) and a ClickHouse `sandbox_egress_local` table keyed by `team_id`, `sandbox_id`, `sandbox_execution_id`, destination, `server_name`, decision, `connections` (`packages/clickhouse/migrations/20260818120000_add_sandbox_egress.sql`@81df066); code search for `sandbox_egress` finds only migrations, so the writer is not in the open repo. Counts connections, not bytes or tokens.
- cost: no.
- budget: partial — per-sandbox concurrent TCP connection cap (feature flag `TCPFirewallMaxConnectionsPerSandbox`, keyed by `LifecycleID`, `tcpfirewall/proxy.go`@81df066 l.291-305); no token/spend/request-rate budget.
- inject: partial (hosted) — docs: "The egress proxy replaces each reference with the secret's current value outside the sandbox", `transform.headers` injected on intercepted TLS, public beta (https://docs.e2b.dev/sandbox/internet-access, https://docs.e2b.dev/secrets). Placeholder parser `${e2b.secrets.*}` / `${e2b.identity.tokens.*}` is open (`packages/shared/pkg/networktransform/placeholders.go`@81df066), but the open egress factory (tcpfirewall) has no CA and no injection.
- allow: yes — `allowInternetAccess: false`, `network.allowOut`/`denyOut` (domains in allow only; "allow rules always take precedence"; `'*'` with `0.0.0.0/0` deny; domain filtering via HTTP Host/TLS SNI) (https://docs.e2b.dev/sandbox/internet-access); enforcement `isEgressAllowed` with always-denied private CIDRs and dial-by-hostname to stop /etc/hosts spoofing (`tcpfirewall/handlers.go` l.52-90, 176-250). Default is allow (internet on).
- hook: no — rules are static per sandbox (updatable via API `sandbox_network_update.go`); no external decider in the open path.
- otel: yes — orchestrator OTel meters: tcpfirewall decisions (`decision`, `protocol`, `match_type`), errors, connection duration, connections per sandbox (`tcpfirewall/metrics.go`@81df066); per-node datapath gauge (`sandbox/network/metrics.go`).
- mcp: partial — SDK option `mcp` starts an in-sandbox `mcp-gateway` template running catalog MCP servers (`packages/js-sdk/src/sandbox/sandboxApi.ts`@dc625eb l.659, l.697; `spec/mcp-server.json`); not an egress-side MCP gate.
- enterprise-only: BYOC and custom deployment are Enterprise ("Contact E2B for ... BYOC"; https://e2b.dev/pricing). Header transforms/secrets injection are documented for the hosted egress proxy only; the open orchestrator ships tcpfirewall ("edition-specific", `factories/run.go` l.279). Pricing page lists no tier gate for network controls or secrets.
- attribution unit: sandbox id / lifecycle (execution) id, team id (API key's team); per connection, not per request.

### 11. kubernetes-sigs/agent-sandbox (ref 2f47477, Apache-2.0)

component: no egress proxy of its own; `SandboxTemplate.networkPolicy` renders one shared Kubernetes NetworkPolicy per template (`extensions/api/v1beta1/sandboxtemplate_types.go`@2f47477 l.27-122). `sandbox-router` is an ingress reverse proxy into sandboxes ("central entry point for all sandbox traffic", `clients/python/agentic-sandbox-client/sandbox-router/README.md`), not egress.

- token: no — nothing in core.
- stream: no.
- attrib: no — controller metrics are lifecycle counts labelled by namespace/template/warm pool, no traffic (`docs/metrics.md`@2f47477).
- cost: no.
- budget: no.
- inject: no in core — example only: `examples/nono-sandbox/README.md` runs nono in the pod, "Agent credential: receives a session-scoped phantom, never the provider key" (third-party nono, not agent-sandbox).
- allow: partial — L3/L4 NetworkPolicy: empty `egress` list = default deny; omitted = "Allow Public Internet only. Blocks internal IPs (RFC1918), Metadata Server" (`sandboxtemplate_types.go` l.84-106); no FQDN rules in core. FQDN allowlisting by identity shown with Cilium (`examples/demo-cilium-egress/README.md`, `NetworkPolicyManagement: Unmanaged` l.47-49).
- hook: no — NetworkPolicy only; any decider is the CNI's (Cilium example).
- otel: partial — controller Prometheus metrics (`docs/metrics.md`), Python client Prometheus histograms (`k8s_agent_sandbox/metrics.py`) and an OTel collector example (`clients/python/agentic-sandbox-client/otel-collector-config.yaml.example`); nothing about egress traffic.
- mcp: no for egress — `clients/integrations/mcp-server/` is an MCP server that creates/execs sandboxes (control plane).
- enterprise-only: n/a (kubernetes-sigs project, Apache-2.0).
- attribution unit: none for traffic; the unit available to a CNI or sidecar is the pod / ServiceAccount identity per Sandbox (Cilium example binds policy to SA identity) and the SandboxClaim UID.

### 12. superradcompany/microsandbox (ref 59d14ba, Apache-2.0)

component: host-side network engine in `crates/network/lib/engine/` (policy `policy/destination.rs`, DNS `dns/`, TLS MITM `tls/`, secrets `secrets/`), per sandbox.

- token: no — secrets handler substitutes placeholders; no response parsing for usage (no such module under `crates/network/lib/engine/`@59d14ba).
- stream: no.
- attrib: partial — `msb-metrics` exports per-sandbox `microsandbox.network.bytes_sent/received` with `sandbox.id`, `sandbox.name` and sandbox labels (`docs/observability/msb-metrics.mdx`@59d14ba l.119-150); bytes only.
- cost: no.
- budget: partial — per-sandbox egress/ingress bandwidth and packet-rate limits (local only, "Cloud does not support this setting") and `maxTcpConnections`/`maxUdpConnections` (`docs/networking/overview.mdx`@59d14ba l.121-125, l.461-490); no token/spend budgets.
- inject: yes — "Secrets give sandboxed code a placeholder ... microsandbox verifies the destination and substitutes the real value outside the guest", bound per host (`.allow("api.github.com")`), violation actions block / block-and-log / block-and-terminate (`docs/sandboxes/secrets.mdx`@59d14ba l.1-30, l.321-325); `crates/network/lib/engine/secrets/` incl. HPACK for HTTP/2.
- allow: yes — rules by group/IP/CIDR/domain/port, `defaultDeny()`, first match wins; strict hostname mode fails closed unless TLS interception can read the authority (`docs/networking/overview.mdx` l.186-200, l.256, l.313-403). Default posture: public internet allowed, private/host/metadata blocked (l.12).
- hook: no — no per-request external decider or plugin found (tree: no hook/plugin under `crates/network`); policy is declarative; `multi-tenant` profile and `managed.json` overrides are static.
- otel: yes — `msb-metrics otel --endpoint=...` OTLP export of per-sandbox CPU/mem/disk/network (`docs/observability/msb-metrics.mdx` l.10-76); resource metrics, not per-request spans.
- mcp: no for egress — `mcp/` is a submodule to superradcompany/microsandbox-mcp, "MCP server for microsandbox - Give your AI agents sandboxes" (`.gitmodules`@59d14ba; gh api description).
- enterprise-only: none of the ten cells is paid-gated in the docs read; enterprise pages cover managed.json rollout and "Private cloud deployment ... Discuss requirements" (`docs/enterprise.mdx`@59d14ba). Inverse gap: TLS inspection, outbound proxies, rate limits are "Local-only", not on microsandbox cloud (`docs/networking/tls.mdx` l.8, `outbound-proxy.mdx` l.7, `overview.mdx` l.463).
- attribution unit: sandbox (`sandbox.id`, `sandbox.name`, user labels); secrets bound per sandbox and destination host.
## Matrix, group D (13-16), verified 2026-10-20

Convention: "no" means not built in. Where a general request/response hook could be used to
build the capability, the cell says so, but it is still "no". Refs are default-branch HEAD
SHAs from `gh api repos/O/R/commits/main`.

### 13. Docker Sandboxes, docker/sbx-releases (ref 2329d12, proprietary: NOASSERTION, LICENSE "Copyright (c) 2026 Docker Inc. All rights reserved.")

- token: no — architecture, credentials, security and FAQ pages say nothing about token or model usage; FAQ telemetry is CLI-invocation analytics only (https://docs.docker.com/ai/sandboxes/faq/)
- stream: no — nothing metered (same pages)
- attrib: partial — rules can be scoped `--sandbox <name>`, and an `sbx policy log` network log exists, but its fields are not documented; audit records carry "principal, action, target, decision, and time", metadata only, no usage (https://docs.docker.com/ai/sandboxes/governance/access-controls/local/, https://docs.docker.com/ai/sandboxes/governance/audit/)
- cost: no — "Model-provider charges are separate"; no spend tracking (FAQ)
- budget: no — no token, spend or rate limits documented (FAQ, governance pages)
- inject: yes — "The proxy replaces the sentinel with the real credential after the request has left the microVM"; built-in services anthropic, openai, github, google, groq, mistral, xai, and others, plus `sbx secret set-custom --host` (experimental) (https://docs.docker.com/ai/sandboxes/architecture/, https://docs.docker.com/ai/sandboxes/configuration/credentials/)
- allow: yes — presets Open / Balanced (default deny plus baseline allowlist) / Locked Down; `sbx policy allow network "host:port"`; deny beats allow; per-sandbox `--deny-network` (local access-controls page)
- hook: no — no external decider; unmatched requests under Balanced or Locked Down "prompt for approval" (interactive, not a programmable hook); Cedar exists only for org MCP policy (local access-controls page; https://docs.docker.com/ai/sandboxes/mcp-gateway/)
- otel: no — no OTel or metrics export on any page read; audit goes to local `.jsonl` or Docker Cloud for SIEM/CSV (audit page)
- mcp: yes — one MCP gateway endpoint per sandbox on the host; Cedar policies over server registration, tool calls (`MCP::Tool`, `invokeTool`), resources, prompts (mcp-gateway page)
- enterprise-only: organization governance (org-level network/filesystem policy, MCP Cedar policies) "is available on a separate paid subscription" (https://docs.docker.com/ai/sandboxes/governance/); audit logs "require a separate paid subscription ... Other Docker subscriptions are not sufficient" (audit page). The `sbx` CLI and local compute are free (https://docs.docker.com/ai/sandboxes/). Local network policy and credential injection are in the free CLI.
- attribution unit: sandbox name (policy scope, network log); audit principal (Docker user) in paid governance. No usage unit.

### 14a. coder/httpjail (ref 6d476da, CC0-1.0)

- token: no — no model-response parsing in `src/` (tree at 6d476da: proxy.rs, rules/*, tls.rs); Cargo.toml has no usage-related crates
- stream: no — no metering
- attrib: partial — one jail per wrapped command, so `--request-log` is per run ("<timestamp> <+/-> <METHOD> <URL>", docs/guide/request-logging.md@6d476da); rule engines receive `requester_ip` (src/rules/common.rs@6d476da `RequestInfo`), the only per-client key in server mode
- cost: no
- budget: no — only `max_tx_bytes`, a per-request upload byte cap (docs/includes/response-format-table.md@6d476da), not a rate or budget
- inject: no — proxy only strips `proxy-connection`/`proxy-authorization` and adds a loop-detection header (src/proxy.rs@6d476da:34-41,189-192); no secret substitution
- allow: yes — "By default, httpjail denies all network requests" (README.md@6d476da); rules decide per request
- hook: yes — three engines: `--js` (V8, sees `r.url/method/host/scheme/path`), `--sh` (process per request, env vars, exit code), `--proc` (persistent line processor, JSON per line on stdin, allow/deny on stdout) (README.md@6d476da; docs/guide/rule-engines/*.md); sees method, URL, requester IP, not headers or body (src/rules/common.rs@6d476da:6-13)
- otel: no — no opentelemetry or prometheus in Cargo.toml@6d476da; logging is the request-log file
- mcp: no — no MCP code in `src/`
- enterprise-only: none (CC0, standalone)
- attribution unit: the wrapped command (one jail per run); in `--server` mode, requester IP

### 14b. coder/boundary (ref e28c8bb, MIT)

- token: no — boundary has none; token usage is recorded by Coder AI Gateway ("record user prompts, token usage, and tool invocations", https://coder.com/docs/ai-coder/ai-gateway), which lives in coder/coder (AGPL) and needs Premium
- stream: no — not in boundary
- attrib: yes — per-run session UUID sent with every audit batch (audit/socket_auditor.go@e28c8bb:38,117 `SessionId`); per-session monotonically increasing sequence number per request (audit/request.go@e28c8bb); boundary injects `X-Coder-Agent-Firewall-Session-Id` and `X-Coder-Agent-Firewall-Sequence-Number` on requests to the AI Gateway so the gateway can join usage to the firewall session (config/session_correlation.go@e28c8bb)
- cost: no — boundary has none; AI Gateway page lists "Manage token spend in a central dashboard" without detail
- budget: no
- inject: no — injects only the session-correlation headers, never credentials (config/session_correlation.go@e28c8bb)
- allow: yes — "Default deny-all"; `--allow "method=... domain=... path=..."` with wildcards; YAML `allowlist` (README.md@e28c8bb; config/config.go@e28c8bb:62)
- hook: no — fixed rule engine (rulesengine/engine.go@e28c8bb); no external decider
- otel: no — stderr structured logs, pprof, and audit batches over a unix socket to the Coder workspace agent (protobuf `BoundaryLog`), which coderd emits as structured logs (README.md@e28c8bb "Audit Logs"); no OTel/Prometheus
- mcp: no — not in boundary; MCP administration is in Coder AI Gateway (ai-gateway page)
- enterprise-only: the Coder side. Agent Firewall (boundary in Coder) "is included with a Premium license" (https://coder.com/docs/ai-coder/agent-firewall); AI Gateway likewise "included with a Premium license" (ai-gateway page). The boundary binary itself is MIT and runs standalone with stderr/log-dir logs.
- attribution unit: boundary session UUID (per run) plus sequence number; in Coder, audit entries also carry workspace ID/name, owner, template ID/version (agent-firewall page)

### 15. strongdm/leash (ref fd8e0c1, Apache-2.0)

- token: no — `gh search code --repo strongdm/leash input_tokens|prompt_tokens` returns nothing
- stream: no — the only SSE handling is the MCP observer sniffing up to `defaultSSEEventLimit = 10` events for MCP outcome (internal/proxy/mcp_observer.go@fd8e0c1:20,387-396), not usage
- attrib: partial — one Leash container per agent container, so logs are per run; the proxy logs host/path/auth-header/status per request (internal/proxy/proxy.go@fd8e0c1:294); the `session_id` in docs/TELEMETRY.md is Statsig product telemetry, not traffic attribution
- cost: no
- budget: no
- inject: yes — Cedar `Action::"HttpRewrite"` on `Host::"..."` with `context.header`/`context.value` makes the MITM proxy set a header, e.g. `Authorization` (docs/design/CEDAR.md@fd8e0c1:111,225-241; internal/proxy/rewriter.go@fd8e0c1 `HeaderRewriteRule`); docs/design/PROXY.md@fd8e0c1 still lists "TODO: Header injection" and docs/todo/SECRETS-INJECTION.md calls the mapping a prototype. It sets a header value per host, not a placeholder swap; README says common API keys are forwarded into the agent container by default
- allow: partial — Cedar `NetworkConnect` on `Host::` enforced by the proxy's `checkConnectPolicy` and eBPF `socket_connect` (internal/proxy/proxy.go@fd8e0c1:272-296); default policy is permissive, `permit ... NetworkConnect ... Host::"*"` (internal/policy/default_cedar_policy.go@fd8e0c1)
- hook: yes — Cedar policy evaluated before connect and for MCP tool calls, editable live via Control UI / `http://localhost:18080/api/policies` (docs/design/CEDAR.md@fd8e0c1:17-38); in-process, not an external decider
- otel: partial — OTel SDK with MCP instruments `mcp.requests_total`, `mcp.errors_total`, duration histogram (internal/telemetry/otel/mcp_instruments.go@fd8e0c1:85-91), but metrics use a ManualReader and traces a stdout exporter; "LEASH_OTEL_ENDPOINT ... ignored: remote OTLP metric export not implemented" (internal/telemetry/otel/provider.go@fd8e0c1:91-105)
- mcp: yes — MCP observer in the MITM proxy parses `tools/call` (mcp_observer.go@fd8e0c1:173); Cedar `Action::"McpCall"` on `MCP::Server`/`MCP::Tool`; "V1: forbid enforced; permit informational only" (docs/design/CEDAR.md@fd8e0c1:112,243-294)
- enterprise-only: none found (Apache-2.0; no paid tier referenced in repo docs read)
- attribution unit: the leash run (one agent container plus its Leash container); no per-request identity beyond that

### 16. earendil-works/gondolin (ref 0705083, Apache-2.0)

- token: no — not built in; `onResponse(response, request)` gets the WHATWG `Response` and could parse usage (docs/sdk-network.md@0705083)
- stream: no — not built in; the hook receives a streaming `Response` body, so an SSE parser could be written there (docs/sdk-network.md@0705083)
- attrib: partial — the hooks are per VM (`VM.create({ httpHooks, env })`, docs/secrets.md@0705083), so whatever the embedder logs is per VM; nothing is recorded by default
- cost: no
- budget: no — `gh search code --repo earendil-works/gondolin "rate limit"` returns nothing
- inject: yes — host generates placeholders, guest env gets only placeholders, host substitutes the real value in headers (opt-in in query) only for the secret's `hosts`; "If a placeholder is used for a disallowed host, the request is blocked" (docs/secrets.md@0705083)
- allow: yes — `allowedHosts` with wildcards, `blockInternalRanges` default true, DNS-rebinding protection by host-side resolution; caveat: "omitted = allow all, explicit empty = deny all" (host/src/http/hooks.ts@0705083:51-52); non-HTTP/TLS TCP dropped unless mapped (docs/sdk-network.md@0705083)
- hook: yes — `isRequestAllowed`, `isIpAllowed`, `onRequest` (may rewrite or short-circuit with a `Response`, can read body via `clone()`), `onResponse` (docs/sdk-network.md@0705083; host/src/http/hooks.ts@0705083:63-71); in-process TypeScript callbacks on the host. Caveat: secrets may be expanded before `onRequest`
- otel: no — `gh search code --repo earendil-works/gondolin opentelemetry` returns nothing
- mcp: no — no MCP handling (search hits are in unrelated Zig guest files)
- enterprise-only: none (Apache-2.0)
- attribution unit: the VM (one `httpHooks` instance per `VM.create`); the embedder adds any key it wants in the hook
