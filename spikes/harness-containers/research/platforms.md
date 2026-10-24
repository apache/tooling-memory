# Layer: whole agent-sandbox platforms (replacements for `amem launch`)

Surveyed 19 October 2026. Licences come from `gh api repos/OWNER/REPO --jq '...license.spdx_id'`, with the LICENSE/COPYRIGHT files read where the API said NOASSERTION/null or the repo is mixed. Maintenance comes from `gh api repos/OWNER/REPO --jq '{pushed_at,archived,stargazers_count}'` and `gh api repos/OWNER/REPO/releases/latest`. Capability claims cite the file or doc page that was read. "Not verified" means I did not read it.

Three things shape this layer:

- **Contract point 1 on Kubernetes.** All containers in a pod share one network namespace. A NetworkPolicy, an OpenSandbox egress sidecar, or an agent-sandbox `networkPolicy` therefore applies to the harness and an egress sidecar alike. A pod cannot by itself give "harness: no network, egress sidecar: network". The sidecar shape holds point 1 only if (a) the harness container runs our own Linux backend inside the pod (`unshare` of user+net+mount namespaces, which the default RuntimeDefault seccomp profile is expected to refuse, so it needs a seccomp profile that allows it, `hostUsers: false`, or Kata, where the pod is a VM), or (b) the egress runs in a different pod and the harness pod gets a deny-all NetworkPolicy except to it. Under (b) the harness has an interface and reaches the egress over TCP, not a unix socket.
- **MicroVM platforms cannot bind-mount a host unix socket.** Any host IPC has to cross over vsock. Only microsandbox offers that as a feature (host unix socket to guest vsock CID 2). Our `amem relay` would have to dial `AF_VSOCK` instead of a unix path.
- **Most of the "run Claude Code in a container" tools leave the agent's credentials inside the box.** Docker Sandboxes and OpenSandbox's Credential Vault do not: they keep the credential in a host-side or sidecar proxy, which is our egress model. Both use a transparent MITM in the box's network path rather than "no network plus one socket".

---

## 1. kubernetes-sigs/agent-sandbox

- **Repo / licence (verified)**: https://github.com/kubernetes-sigs/agent-sandbox ; Apache-2.0 ; `gh api repos/kubernetes-sigs/agent-sandbox --jq .license.spdx_id`.
- **Form**: Kubernetes controller plus CRDs (`Sandbox`; extensions `SandboxTemplate`, `SandboxClaim`, `SandboxWarmPool`), an optional `sandbox-router` HTTP proxy, an optional in-pod `sandboxd` (gRPC :9090, HTTP :8080), and Go/Python clients. It needs cluster-admin to install the CRDs. Workloads run with whatever pod security you give them.
- **Platforms**: wherever Kubernetes runs. On a laptop that means kind/minikube: Linux yes, macOS and Windows only through a Linux VM.
- **Mechanism**: a singleton, stateful pod per Sandbox with a stable hostname, PVCs from `volumeClaimTemplates`, suspend/resume, expiry (`shutdownTime`) and warm pools. Isolation is delegated to a `RuntimeClass` (gVisor or Kata) ("Scope" note in README.md). `SandboxTemplate.networkPolicy` creates one shared NetworkPolicy per template. The default is "Egress: Allow Public Internet only. Blocks internal IPs", and an empty `egress` list means default deny (`extensions/api/v1beta1/sandboxtemplate_types.go:76-115`). `NetworkPolicyManagement: Unmanaged` hands networking to Cilium or similar.
- **Against our contract**:
  1. *Partial.* `podTemplate` is a full `corev1.PodSpec` (`api/v1beta1/sandbox_types.go:208-272`), so an egress sidecar plus an `emptyDir` carrying `/run/memory/egress.sock` is expressible. Because of the shared pod netns, the harness still has the pod's interface unless (a) or (b) from the opening section is applied. With an empty egress list the template can deny all egress, but that also denies the egress sidecar. The workable layouts are therefore: harness pod deny-all and egress pod separate (TCP), or harness container running our namespaces backend inside.
  2. *Holds.* Arbitrary volumes at fixed `mountPath`s with `readOnly`, from the PodSpec. The record projection would be a volume (PVC, CSI, or an init container that writes an `emptyDir`).
  3. *Replaced, with a daemon:* the kubelet and container runtime, not unprivileged namespaces.
  4. and 5. *Silent.* Not a laptop tool on macOS or Windows.
  6. *Untouched:* the egress stays ours.
  `examples/recall-memory/README.md` is close to our institutional shape: a memory server outside the sandbox, reached with a projected service-account token bound to the `SandboxClaim`.
- **Maintenance**: pushed 2026-10-19, release v1.0.7 2026-10-15, not archived, 4316 stars. KEPs and a release cadence are under kubernetes-sigs.
- **If we adopted it**: it replaces nothing on the laptop. In the institution it replaces whatever launches a harness pod per workload: lifecycle, warm pools, expiry, and identity through the claim UID. It adds Kubernetes, a controller, and a RuntimeClass. OpenHands' `openhands-workspace/agent_sandbox/` and OpenSandbox's `[agent_sandbox]` config both already target it.
- **Verdict**: **adopt for the institutional sidecar shape** (sidecar-fit: yes, k8s-native). It is the standard surface to emit a `SandboxTemplate` for, not a laptop launcher. Point 1 needs one of the two layouts above, and the spike settles which.

## 2. OpenSandbox (opensandbox-group/OpenSandbox, formerly alibaba/OpenSandbox)

- **Repo / licence (verified)**: https://github.com/opensandbox-group/OpenSandbox ; Apache-2.0 ; `gh api repos/alibaba/OpenSandbox` (redirects) `.license.spdx_id`. README "License" section says Apache 2.0.
- **Form**: a lifecycle server (Python, `uvx opensandbox-server`) with SQLite state, plus Go components: `execd` (in-sandbox agent), the egress sidecar, and ingress. SDKs in Python, TS, Java/Kotlin, C#, Go; a `osb` CLI; an MCP server. It drives a Docker-API engine (Podman is recognised in `server/opensandbox_server/services/docker/networking.py:89-90,114-117`) or Kubernetes (its own operator or kubernetes-sigs/agent-sandbox). Optional gVisor, Kata, or Firecracker via `[secure_runtime]`.
- **Platforms**: Linux yes (Docker or Podman). macOS and Windows: through a Docker engine, not verified.
- **Mechanism**: per-sandbox containers or pods. The egress sidecar "shares the sandbox's network namespace, so every outbound packet passes it": DNS proxy plus nftables default-deny, fail-closed if rules cannot be installed (`docs/architecture/network/egress.md`). Its Credential Vault keeps real credentials in the sidecar and MITM-injects headers so the sandbox holds only placeholders (`docs/guides/credential-vault.md`).
- **Against our contract**:
  1. *Not as designed, possible as configured.* `[docker].network_mode` accepts `none`, but "Egress sidecar + networkPolicy require bridge" (`server/configuration.md:110`; `networking.py:191-204` rejects networkPolicy with `none`). With `none` and a host-dir volume carrying our socket, the harness would hold point 1. Whether the server can still reach `execd` with `none` is not documented: it uses mapped ports, `port_allocator.py`. On Kubernetes the same shared-netns limit applies; they chose a sidecar over NetworkPolicy and say why (`docs/guides/network-isolation.md`).
  2. *Holds.* Host volume mounts at a chosen path, read-only or read-write, with subPath, gated by `[storage].allowed_host_paths` (`docs/examples/host-volume-mount.md`).
  3. *Replaced, with a server plus a container engine.*
  4. and 5. *Silent.*
  6. *Overlaps:* their egress plus Credential Vault is a competing implementation of our egress, as a transparent MITM rather than an explicit forwarding proxy, and without our labels/ceilings or usage metering.
- **Maintenance**: pushed 2026-10-19, release-1.1.1 2026-10-09, 15978 stars, in the CNCF Landscape (per README).
- **If we adopted it**: it would replace `amem launch` and could replace the WSLC backend's two-container layout on hosts with Docker or Podman. It adds a Python server, an `execd` binary injected into every sandbox, and a DB. Its egress would have to be switched off in favour of ours.
- **Verdict**: **learn-from / borrow.** It is the closest open design to ours (per-workload egress sidecar holding credentials, Docker to Kubernetes with one API, agent-sandbox as a provider). Borrow its fail-closed redirect rule and the `deny.always` overlay idea. Adopting it means running their control plane and disabling the half that overlaps ours. Sidecar-fit: yes.

## 3. microsandbox (superradcompany/microsandbox, formerly zerocore-ai/microsandbox)

- **Repo / licence (verified)**: https://github.com/superradcompany/microsandbox ; Apache-2.0 ; `gh api repos/microsandbox/microsandbox` (redirects) `.license.spdx_id`.
- **Form**: a Rust library plus the `msb` CLI. Each sandbox is a libkrun microVM spawned as a child process: "No setup server. No long-running daemon" (README). SDKs in Rust, TS, Python, Go, Ruby. An optional hosted cloud exists. Needs `/dev/kvm` read-write. No root otherwise.
- **Platforms**: Linux yes (KVM), macOS yes (Apple Silicon), Windows yes (WHP) (README).
- **Mechanism**: an OCI image becomes the guest rootfs on a libkrunfw kernel. Network policy is enforced by the host-side network stack, with optional TLS inspection and destination-bound secret placeholders (docs `networking/overview.md`, `sandboxes/secrets`).
- **Against our contract**:
  1. *Holds, with a vsock hop.* The Rust/TS `.disableNetwork()` removes the network device. CLI `--no-net` keeps the device and denies all traffic (`networking/overview.md:59`). `--vsock HOST_PATH:PORT` / `.vsock("/run/host-api.sock", 5000)` connects a host unix socket to guest vsock CID 2 (`networking/host-sockets.md`), local backend only. The guest sees `AF_VSOCK`, not a unix path, so `amem relay` would listen on loopback TCP and dial vsock 2:PORT.
  2. *Holds.* `.volume("/guest", |v| v.bind("./host").readonly().noexec())`, CLI `-v SRC:DEST:ro`, `--mount-dir`, `--mount-file`, `--tmpfs` (`sandboxes/volumes.md`, `cli/sandbox-commands.md:76-104`). Fixed guest paths (/home/user, /work, /opt/memory/memory, /memory) are expressible. A read-only rootfs is not verified.
  3. *Replaced* by KVM plus libkrun, still daemonless and unprivileged if the user can open `/dev/kvm`.
  4. *Could replace Seatbelt with a real VM* on Apple Silicon.
  5. *Could replace WSLC* with WHP and a named pipe in place of the socket (`--vsock '\\.\pipe\name:PORT'`).
  6. *Overlaps optionally:* its secrets and TLS inspection duplicate our egress, and we would leave them off.
- **Maintenance**: pushed 2026-10-19, v0.7.8 2026-10-09, 8743 stars. Pre-1.0.
- **If we adopted it**: one backend (Rust crate) for all three OSes, replacing `linux.rs`, `seatbelt.rs` and the wslc binding at the price of KVM/HVF/WHP. It adds libkrun/libkrunfw (C, bundled), an in-guest agent (`agentd`), and an OCI pull path. It loses no-KVM hosts and WSL2 without nested virtualisation. The `unsafe` lives in their crate, not ours.
- **Verdict**: **borrow, spike for adopt as an alternative backend.** It is the only surveyed platform that does "no network plus one host socket plus fixed bind mounts" natively on all three OSes. The vsock relay change is small. Sidecar-fit: no (laptop or VM-host shape).

## 4. E2B (SDK e2b-dev/E2B, runtime e2b-dev/runtime formerly e2b-dev/infra)

- **Repo / licence (verified)**: SDK https://github.com/e2b-dev/E2B, Apache-2.0. Runtime https://github.com/e2b-dev/runtime (`gh api repos/e2b-dev/infra` redirects), Apache-2.0, README badge and LICENSE. The self-host part is Apache too: "E2B Embed ... Open source, Apache-2.0 ... No E2B account and no license key needed" (`embed/README.md`).
- **Form**: a full control plane (Go: API, orchestrator, `envd` in-VM agent, edge router, template builder) plus Postgres, Redis, ClickHouse, and an OTel collector. Embed runs it on one node: Docker Compose with `privileged: true`, `pid: host`, `network_mode: host` services (`embed/compose/compose.yaml:198-283`), plus Terraform for GCP/AWS/Azure and Kubernetes.
- **Platforms**: Linux x86-64/arm64 with `/dev/kvm` and `/dev/net/tun`, Docker Engine 27+, 12 GiB RAM recommended (`embed/compose/README.md`). macOS only inside a nested-virt Linux VM. Windows: not listed.
- **Mechanism**: Firecracker microVMs restored from snapshots, each in its own netns with an nftables egress firewall and SNI/Host allow/deny lists (runtime README). The SDK drives `envd` over Connect RPC.
- **Against our contract**:
  1. *Breaks the unix-socket half.* `allowInternetAccess: false` gives no outbound traffic. `network.allowOut` + `denyOut` gives an allowlist by IP/CIDR/domain, and "Bring your own proxy" is SOCKS5 (docs.e2b.dev/sandbox/internet-access). Nothing exposes a host unix socket in the VM. The best fit is: deny all, allow one IP (our egress on TCP).
  2. *Breaks.* There are no host bind mounts. Files arrive by upload, by E2B-managed volumes, or by template build.
  3. *Replaced* by a heavy privileged stack.
  4. and 5. *No.*
  6. *Silent:* their secrets feature is metadata-only on the control plane.
- **Maintenance**: SDK pushed 2026-10-19, latest release `@e2b/code-interpreter-python@2.11.0` 2026-10-13. Runtime pushed 2026-10-19, release 2026.30 2026-09-10.
- **If we adopted it**: it replaces launch and lifecycle and adds a whole cloud on one node. It is not laptop-first: Docker plus privileged services plus 12 GiB.
- **Verdict**: **reject for the laptop, learn-from for the fleet** (snapshot/resume, fork). Sidecar-fit: no. A sandbox is a VM the platform owns, and E2B Embed's Kubernetes mode runs E2B on a node, not as a pod per workload.

## 5. Dagger and dagger/container-use

- **Repo / licence (verified)**: https://github.com/dagger/dagger, Apache-2.0. https://github.com/dagger/container-use, Apache-2.0. Both via `gh api ... .license.spdx_id`.
- **Form**: the Dagger engine (BuildKit-based) runs as a privileged container in Docker or Podman. The SDKs and the `dagger` CLI talk GraphQL to it. container-use is a Go CLI that also serves as an MCP server (`container-use stdio`).
- **Platforms**: wherever a Docker or Podman engine runs (Linux, macOS, and Windows via a VM). Not verified per OS here.
- **Mechanism**: containers are lazy pipeline objects. container-use gives the agent MCP tools that run commands in a per-branch container. The agent itself stays on the host ("add `container-use stdio` as an MCP server", README).
- **Against our contract**:
  1. *Breaks.* The schema has `withUnixSocket`/`withoutUnixSocket` (`core/schema/container.go:481-494`), but no option to disable a container's network. The only `network` hits in `docs/docs-graphql/schema.graphqls` are ports, service bindings and tunnels.
  2. *Partial.* `withMountedDirectory/File(readOnly)` mounts at any path, but a host directory is loaded into the engine rather than live-bound, which is why container-use moves work back via git branches.
  3. *Replaced* by a privileged daemon container.
  6. *Silent.*
  For container-use specifically, the harness is not confined at all; only its tool calls are.
- **Maintenance**: dagger pushed 2026-10-19, v0.21.10 2026-09-30. container-use pushed 2026-09-21, latest release v0.4.2 2025-08-19, labelled "experimental".
- **If we adopted it**: it adds a privileged engine and loses point 1.
- **Verdict**: **reject.** There is no network-off, and container-use leaves the agent on the host. Sidecar-fit: no.

## 6. OpenHands (Agent Canvas, OpenHands/OpenHands; runtime in OpenHands/software-agent-sdk)

- **Repo / licence (verified)**: https://github.com/OpenHands/OpenHands, MIT (LICENSE read). https://github.com/OpenHands/software-agent-sdk, MIT. The repo listing shows no `enterprise/` directory.
- **Form**: OpenHands/OpenHands is now "Agent Canvas", a self-hosted UI and control centre (npm `@openhands/agent-canvas`, Electron) that runs OpenHands or ACP agents (Claude Code, Codex, Gemini, Pi, OpenCode) on "agent backends". The sandbox lives in the SDK: `openhands-agent-server` runs inside a workspace, and `openhands-workspace` provides `docker`, `apptainer`, `agent_sandbox` (k8s), `remote_api` and `cloud`.
- **Platforms**: Linux, macOS and Windows (README.windows.md) for the UI. Workspaces depend on the backend.
- **Mechanism**: `DockerWorkspace` starts a container running the agent-server and talks to it over a published TCP port (`openhands-workspace/openhands/workspace/docker/workspace.py`: `find_available_tcp_port`, `host_port`, "authenticate network-bound requests when it binds 0.0.0.0"), with extra `volumes`.
- **Against our contract**:
  1. *Breaks.* The control channel is TCP into the box, so the box has a network.
  2. *Partial:* arbitrary `volumes`.
  3. *Replaced* by Docker, Apptainer, or Kubernetes.
  6. *Silent.*
- **Maintenance**: OpenHands v1.28.1 2026-10-19. SDK v1.56.0 2026-10-19, pushed the same day.
- **If we adopted it**: we would inherit its agent-server inside the box and its TCP control channel.
- **Verdict**: **learn-from.** Its ACP-driven multi-harness UI and the `agent_sandbox` workspace adapter are reference code. Sidecar-fit: via its agent-sandbox workspace, but the agent-server is the in-pod process, not ours.

## 7. SWE-ReX (SWE-agent/SWE-ReX)

- **Repo / licence (verified)**: https://github.com/SWE-agent/SWE-ReX ; MIT.
- **Form**: a Python library. Deployments are `local`, `docker`, `modal`, `fargate`, `daytona` and `remote` (`src/swerex/deployment/`). `swerex-remote` runs in the box.
- **Platforms**: Linux and macOS via Docker or Podman (`container_runtime: Literal["docker","podman"]`, `deployment/config.py:44`). Windows not verified.
- **Mechanism**: `docker run -p {port}:8000 ... image` and an HTTP runtime that drives shell sessions (`deployment/docker.py:251-261`). The agent loop runs outside.
- **Against our contract**:
  1. *Breaks:* a port is always published.
  2. *Partial* via `docker_args`.
  3. *Replaced* by a container engine.
  The harness is not what runs in the box; SWE-ReX is a remote shell for an agent outside.
- **Maintenance**: pushed 2026-10-12, v1.4.0 2025-08-14.
- **If we adopted it**: wrong shape. Our harness is a CLI that must run inside.
- **Verdict**: **reject.** It is a remote-execution layer for evaluation, not a harness launcher. Sidecar-fit: no.

## 8. Coder, coder/envbuilder (and coder/boundary, coder/httpjail)

- **Repo / licence (verified)**:
  - coder/coder: AGPL-3.0, so **out**.
  - coder/envbuilder: Apache-2.0.
  - coder/boundary: MIT.
  - coder/httpjail: CC0-1.0.
  - coder/agentapi: MIT, archived.
  - coder/envbox: AGPL-3.0.
  All via `gh api ... .license.spdx_id`.
- **Form**: envbuilder is an image that builds a devcontainer/Dockerfile environment from inside a container (Docker, Kubernetes, OpenShift). boundary and httpjail are process-level HTTP egress filters, which belong to the egress/OS layer.
- **Platforms**: envbuilder runs wherever a Linux container runs.
- **Mechanism**: envbuilder builds the image in-container (Kaniko-style) and then execs the init. It neither confines nor manages lifecycle.
- **Against our contract**: 1, 2 and 6 *silent*. It is a build step. Coder itself is out on licence and was not examined further.
- **Maintenance**: envbuilder pushed 2026-10-19, v1.3.0 2026-03-06. coder v2.37.5 2026-10-19. boundary pushed 2026-09-29. httpjail pushed 2026-09-24.
- **If we adopted it**: envbuilder could build `images/harness` in-cluster from a Dockerfile. It replaces nothing in launch.
- **Verdict**: Coder **reject** (AGPL). envbuilder **learn-from** for in-cluster image builds. boundary and httpjail go to the egress-layer survey. Sidecar-fit: n/a.

## 9. DevPod (loft-sh/devpod; active fork skevetter/devpod)

- **Repo / licence (verified)**: https://github.com/loft-sh/devpod, MPL-2.0. The fork https://github.com/skevetter/devpod is also MPL-2.0. Both via `gh api ... .license.spdx_id`. MPL is ASF Category B, so it is **ranked out**.
- **Form**: a Go CLI and desktop app. It creates devcontainer.json workspaces on "providers" (Docker, Podman, Kubernetes, SSH, clouds).
- **Platforms**: Linux, macOS, Windows (desktop app). Not verified per provider.
- **Mechanism**: builds the devcontainer, injects its agent, and connects the IDE or a shell.
- **Against our contract**: 1 and 2 not verified. They depend on the provider and on `runArgs`/`mounts` pass-through. 6 silent.
- **Maintenance**: upstream pushed 2025-11-14, last release v0.6.15 2025-03-10, effectively stalled. The fork pushed 2026-10-02, v0.26.3 2026-10-02, 210 stars.
- **If we adopted it**: it adds a workspace manager aimed at IDEs, not at a confined CLI.
- **Verdict**: **reject** (Category B licence, and upstream stalled). Sidecar-fit: no.

## 10. Docker Sandboxes (`sbx`, formerly `docker sandbox`)

- **Repo / licence (verified)**: https://github.com/docker/sbx-releases. The API returns NOASSERTION. LICENSE reads "Copyright © 2026 Docker Inc. All rights reserved." The repo holds binaries only, so it is **proprietary**. The docs say the CLI and local compute are free to use.
- **Form**: a single CLI (`sbx`) plus a microVM per sandbox and a host-side proxy. Packages exist for brew, winget, and apt (`docker-sbx`, plus `usermod -aG kvm`).
- **Platforms**: macOS, Windows, and Linux (Ubuntu apt) per the repo README.
- **Mechanism**: "All outbound TCP traffic from the sandbox routes through a proxy on your host". The forward proxy enforces policy (Open / Balanced / Locked Down) and swaps a sentinel for the real credential. The workspace appears "at the same absolute path as on your host". Local MCP servers run on the host (docs.docker.com/ai/sandboxes/architecture).
- **Against our contract**:
  1. *Different shape.* The VM has networking routed through Docker's proxy, not ours. No way to substitute our egress is documented.
  2. *Breaks the fixed paths:* the workspace sits at the host path.
  6. *Duplicates:* their proxy is our egress.
- **Maintenance**: v0.48.0 2026-10-13, pushed 2026-10-19.
- **If we adopted it**: it is closed source, so we cannot embed or extend it.
- **Verdict**: **reject (licence), learn-from:** its sentinel-credential host proxy and "clone mode" (repo read-only at `/run/sandbox/source`) confirm our design. Sidecar-fit: no.

## 11. Daytona (daytonaio/daytona)

- **Repo / licence (verified)**: https://github.com/daytonaio/daytona. The API reports `license: null`, so I read the files at tag v0.190.0:
  - `COPYRIGHT` says: "Unless otherwise noted ... all files in this repository are licensed under the AGPL 3.0 license". `LICENSE` is the AGPL-3.0 text.
  - The clients moved to https://github.com/daytona/clients, whose LICENSE says Apache-2.0 except `cli/`, which is AGPL-3.0.
- **Form**: a control plane, runner, and toolbox daemon (Go/TS). The repository is **archived**: "As of June 2026, Daytona's core development has moved to a private codebase" (README).
- **Platforms**: n/a.
- **Mechanism**: OCI-based sandboxes on runners.
- **Against our contract**: not evaluated further. It is out on licence, and the open source is frozen.
- **Maintenance**: archived, last push 2026-07-24, last release v0.190.0 2026-06-23.
- **If we adopted it**: n/a.
- **Verdict**: **reject.** The core is AGPL-3.0 and now closed. Daytona Cloud is hosted-only and out of scope. Its SDKs are Apache-2.0. Sidecar-fit: no.

## 12. Arrakis (abshkbh/arrakis)

- **Repo / licence (verified)**: https://github.com/abshkbh/arrakis ; AGPL-3.0.
- **Form**: a microVM sandbox server (Cloud Hypervisor).
- **Platforms**: Linux.
- **Mechanism**: not examined.
- **Against our contract**: not examined (out on licence).
- **Maintenance**: last push 2025-06-02, release-36 2025-06-02. Stale.
- **If we adopted it**: n/a.
- **Verdict**: **reject** (AGPL, stale).

## 13. "Run Claude Code in a container" wrappers

### textcortex/claude-code-sandbox, and its successor textcortex/spritz
- **Repo / licence (verified)**: claude-code-sandbox is **archived**. The API licence is null; `package.json` says `"license": "MIT"`, and there is no LICENSE file in the root listing. Spritz (https://github.com/textcortex/spritz) is MIT via the API.
- **Form**: claude-code-sandbox is a Node CLI over Docker. Spritz is a Kubernetes control plane (Helm, CRDs, ACP gateway, UI) for "disposable agent instances".
- **Platforms**: Docker hosts / Kubernetes.
- **Mechanism**: claude-code-sandbox runs Claude Code with `--dangerously-skip-permissions` in a container (README). Spritz runs one instance workload per user and agent over ACP.
- **Against our contract**: 1 *silent or breaks* (neither README describes a network-none mode). 2 not verified.
- **Maintenance**: claude-code-sandbox last push 2026-02-20, v0.1.0-alpha.2 2025-06-12, archived. Spritz pushed 2026-04-29 with no releases, labelled "alpha".
- **If we adopted it**: n/a.
- **Verdict**: **reject.** claude-code-sandbox is an archived PoC. Spritz is alpha and quiet since April; learn-from it for the ACP gateway shape. Spritz sidecar-fit: k8s-native, but not examined for sidecars.

### RchGrav/claudebox
- **Repo / licence (verified)**: https://github.com/RchGrav/claudebox ; MIT.
- **Form**: a Bash wrapper over Docker. It installs Docker if missing.
- **Platforms**: Linux and macOS with Docker.
- **Mechanism**: per-project images and profiles, with "Network isolation with project-specific firewall allowlists" (README). The README does not say how the firewall is set up.
- **Against our contract**: 1 breaks (an allowlist firewall, not no-network). 2 partial. Credentials live in the container.
- **Maintenance**: pushed 2026-09-17, v2.0.1 2026-09-15.
- **If we adopted it**: n/a.
- **Verdict**: **reject.**

### strongdm/leash
- **Repo / licence (verified)**: https://github.com/strongdm/leash ; Apache-2.0.
- **Form**: a Go CLI (also on npm) that starts two containers: the agent container and a "Leash container" that applies Cedar policies through eBPF LSM hooks (`file_open`, `socket_connect`) and a MITM proxy (`docs/design/ARCHITECTURE.md`). Runs on Docker, Podman, or OrbStack.
- **Platforms**: Linux, macOS (including an experimental native mode), and WSL (README).
- **Mechanism**: the current directory is bind-mounted into the agent container. The Leash container observes and enforces policy. It also includes an MCP observer.
- **Against our contract**: 1 breaks. The agent has network, governed by policy. API keys are forwarded into the agent container ("Environment forwarding maps common API keys automatically"), the opposite of our credential hold. 2: the cwd is bound at the host path. 3: loading eBPF LSM programs needs a privileged helper container and a kernel with BPF LSM.
- **Maintenance**: pushed 2026-04-06, v1.1.7 2026-03-11.
- **If we adopted it**: n/a.
- **Verdict**: **learn-from** (Cedar over syscall and MCP telemetry), not a host for our wrapper.

### imbue-ai/sculptor
- **Repo / licence (verified)**: https://github.com/imbue-ai/sculptor ; MIT (LICENSE.md read, "Copyright (c) 2026 Imbue, Inc.").
- **Form**: a desktop app (Electron plus a Python backend).
- **Platforms**: macOS Apple Silicon and Linux downloads.
- **Mechanism**: workspaces are git worktrees by default, with clone and in-place modes as alternatives (`docs/help/workspaces.md`). The current docs describe no container isolation.
- **Against our contract**: no confinement.
- **Maintenance**: pushed 2026-10-19, sculptor-v0.48.0 2026-09-21.
- **If we adopted it**: n/a.
- **Verdict**: **reject** as a platform (it is a parallel-agent UI).

### superagent-ai/vibekit
- **Repo / licence (verified)**: https://github.com/superagent-ai/vibekit ; MIT.
- **Form**: a TypeScript SDK and CLI with provider packages: `beam`, `blaxel`, `cloudflare`, `dagger`, `daytona`, `e2b`, `modal`, `northflank`. Local mode uses Dagger.
- **Platforms**: wherever Dagger or the hosted provider runs.
- **Mechanism**: an adapter over sandbox providers, with secret redaction in completions.
- **Against our contract**: it inherits each provider's answer. Locally that is Dagger's, so 1 breaks.
- **Maintenance**: pushed 2026-01-13, last release `@vibe-kit/sdk@0.0.57` 2025-08-04. Stale.
- **If we adopted it**: n/a.
- **Verdict**: **reject** (stale, and it adds nothing over its providers).

## 14. Goose (aaif-goose/goose, formerly block/goose)

- **Repo / licence (verified)**: https://github.com/aaif-goose/goose (`gh api repos/block/goose` redirects) ; Apache-2.0.
- **Form**: an agent harness (Rust CLI and desktop app), not a sandbox platform.
- **Platforms**: Linux, macOS, Windows.
- **Mechanism**: an opt-in macOS seatbelt sandbox shipped in v1.25.0 and "has been removed". The goose server "is not sandboxed at the OS level" (`documentation/blog/2026-02-23-goose-v1-25-0/index.md`).
- **Against our contract**: n/a. It is something we would launch, not something that launches.
- **Maintenance**: v1.55.0 2026-10-16.
- **If we adopted it**: n/a.
- **Verdict**: **reject as a platform.** It is a candidate tenth harness for the adapter table.

## 15. BoxLite (boxlite-ai/boxlite)

- **Repo / licence (verified)**: https://github.com/boxlite-ai/boxlite ; Apache-2.0.
- **Form**: an embeddable micro-VM library ("Daemonless ... no root, no background service", with an optional server mode). SDKs in Python, Node, Go, Rust, and C, plus a CLI and a REST API.
- **Platforms**: Linux with KVM, macOS on Apple Silicon (12+), Windows through WSL2 with KVM (README).
- **Mechanism**: a VM per box (KVM or Hypervisor.framework), with seccomp or sandbox-exec around the VMM, an `allow_net` egress allowlist, and secret placeholders.
- **Against our contract**: 1 *partial.* A `network_disabled` option exists (`src/boxlite/src/runtime/options.rs`, `sdks/go/options.go`). The README lists networking as "outbound internet · local TCP port forwarding". I found no documented host-unix-socket-into-guest feature, only SSH streamlocal code in `src/guest`, unverified as an API. 2: volume mounts read-only or read-write (README).
- **Maintenance**: pushed 2026-10-19, v0.10.7 2026-10-18.
- **If we adopted it**: same trade as microsandbox, but without the socket route.
- **Verdict**: **learn-from.** It is second to microsandbox in this niche until a socket route is documented. Belongs to the microVM-runtime layer.

## 16. Rivet Sandbox Agent (rivet-dev/agents, formerly rivet-dev/sandbox-agent)

- **Repo / licence (verified)**: https://github.com/rivet-dev/agents ; Apache-2.0.
- **Form**: a Rust binary that runs inside an existing sandbox (E2B, Daytona, Docker, ...) and exposes HTTP to control coding agents.
- **Platforms**: whatever the host sandbox is.
- **Mechanism**: "run coding agents in sandboxes and control them over HTTP" (README table).
- **Against our contract**: 1 breaks (HTTP into the box). 2 silent. It does no confinement of its own.
- **Maintenance**: v0.5.3 2026-10-14.
- **If we adopted it**: n/a.
- **Verdict**: **learn-from** (a normalised control API across harnesses).

## 17. agent-infra/sandbox (AIO Sandbox)

- **Repo / licence (verified)**: https://github.com/agent-infra/sandbox ; Apache-2.0.
- **Form**: a single Docker image bundling a browser, shell, files, MCP and VS Code server, run with `--security-opt seccomp=unconfined` (README examples).
- **Platforms**: Docker hosts.
- **Mechanism**: it is a tool container that an agent outside calls over HTTP on port 8080.
- **Against our contract**: 1 breaks. 2 silent.
- **Maintenance**: pushed 2026-09-14, v1.11.0 2026-06-23.
- **If we adopted it**: n/a.
- **Verdict**: **reject** (wrong shape: tool server, not harness launcher).

---

## Hosted-only services (out of scope for a laptop-first, self-hosted reference)

| Service | Open component and licence (verified with `gh api`) | Note |
|---|---|---|
| Cloudflare Sandbox SDK | cloudflare/sandbox-sdk. The API says NOASSERTION; the LICENSE file is Apache-2.0. `@cloudflare/sandbox@1.0.1` 2026-10-13 | "A sandbox is a Durable Object and the Container it starts" (README). Workers runtime only |
| Vercel Sandbox | vercel/sandbox, Apache-2.0, `sandbox@4.9.0` 2026-10-19 | Firecracker on Vercel infrastructure (README) |
| Modal Sandboxes | modal-labs/modal-client, Apache-2.0, pushed 2026-10-19 | SDK for the Modal platform |
| E2B Cloud | e2b-dev/E2B, Apache-2.0 | The runtime is self-hostable; see section 4 |
| Daytona Cloud | daytona/clients, Apache-2.0 except `cli/` (AGPL-3.0) | Core is closed since June 2026 |
| Docker Sandboxes cloud | none (proprietary) | Same `sbx` CLI |
| Runloop | runloopai/api-client-python, MIT | SDK only |
| Blaxel | blaxel-ai/sdk-python, MIT | SDK only |
| Morph Cloud | morph-labs/morph-python-sdk, Apache-2.0 | SDK only |
| Fly.io Sprites | superfly/sprites-py, MIT | SDK only |
| Together Code Sandbox | togethercomputer/together-py, Apache-2.0 | SDK only |
| Beam | beam-cloud/beta9, AGPL-3.0 | The self-hostable engine is AGPL, so it is out either way |
| microsandbox cloud | same Apache-2.0 SDK | Host sockets are "rejected by microsandbox cloud" |

Seen, but belonging to the OS-level layer rather than this one: anthropics/sandbox-runtime (Apache-2.0, v0.0.80 2026-10-16) and nolabs-ai/nono (formerly always-further/nono, Apache-2.0, v0.80.0 2026-10-13).

## Sidecar fit (PLAN.md: "the same binary runs as a sidecar the platform launches per workload")

| Platform | Launches a per-workload sidecar we supply? | Holds point 1 with it? |
|---|---|---|
| kubernetes-sigs/agent-sandbox | Yes. `podTemplate` is a full PodSpec (native sidecar, `emptyDir` for the socket), and `SandboxTemplate`/`SandboxWarmPool` stamp it per workload | Not by the pod alone (shared netns). Needs our namespaces backend inside the harness container, or the egress in a separate pod with deny-all on the harness pod |
| OpenSandbox (k8s, or Docker with Podman) | It already launches its own egress sidecar per sandbox. Substituting ours means a custom image in `[egress].image` honouring its sidecar API, or disabling it | Same shared-netns limit. On Docker, `network_mode=none` plus a host-dir socket holds point 1 but forgoes their sidecar |
| Spritz | k8s-native, per-instance workloads | Not examined |
| OpenHands `agent_sandbox` workspace | Goes through agent-sandbox | Its agent-server needs TCP into the pod |
| E2B, Docker Sandboxes, microsandbox, BoxLite | No. The unit is a VM, and the "sidecar" would be a host process or a socket | microsandbox: yes, through vsock |

---

## Ranked shortlist for this layer

1. **microsandbox.** It is the only platform that natively gives no network, one host unix socket, and fixed read-only bind mounts, on Linux, macOS and Windows, daemonless, with a Rust SDK. The costs are KVM/HVF/WHP and a vsock dial in `amem relay`. Candidate alternative backend; laptop shape.
2. **kubernetes-sigs/agent-sandbox.** Apache-2.0, v1.x, k8s-native lifecycle and warm pools. This is where the institutional sidecar shape lives. Adopt as the target to emit for; point 1 needs a decided pod layout.
3. **OpenSandbox.** The closest open design to ours (per-sandbox egress sidecar holding credentials, Docker or Podman to Kubernetes, agent-sandbox provider). Borrow ideas; adopting it means running its server and switching off its overlapping egress.
4. **E2B runtime (Embed).** Apache-2.0 and self-hostable, but privileged, Docker-only, 12 GiB, and with no host socket or bind mounts. Fleet reference only.
5. **Dagger / container-use.** Apache-2.0, but there is no network-off, and container-use leaves the agent on the host.

### Spike A: microsandbox (needs /dev/kvm; no podman, no Node)

On this machine `/dev/kvm` is `root:kvm 0660` and user `striker` is not in `kvm` (checked with `ls -l /dev/kvm; id -nG`). One root step is needed first, `sudo usermod -aG kvm striker` and a new login, or a `setfacl -m u:striker:rw /dev/kvm` that does not survive a reboot. Everything else installs without root into a directory of your choosing. The release script would also symlink into `~/.local/bin`, so install by hand instead:

```sh
S=$HOME/spikes/msb; mkdir -p $S/dl && cd $S/dl
V=v0.7.8
curl -fsSLO https://github.com/superradcompany/microsandbox/releases/download/$V/microsandbox-linux-x86_64.tar.gz
curl -fsSLO https://github.com/superradcompany/microsandbox/releases/download/$V/checksums.sha256
grep -F microsandbox-linux-x86_64.tar.gz checksums.sha256 | sha256sum -c -
mkdir -p $S/pkg && tar -xzf microsandbox-linux-x86_64.tar.gz -C $S/pkg && ls -R $S/pkg | head
export MSB_HOME=$S/home PATH=$S/pkg:$PATH   # adjust if msb sits in a subdirectory; libkrunfw must be beside it or on LD_LIBRARY_PATH (see install.sh:377-394)
msb --version
```

Contract test, with a host unix socket standing in for our egress and fixed paths standing in for our layout:

```sh
T=$S/t; mkdir -p $T/home $T/work $T/opt $T/memory $T/run
echo hi > $T/memory/entry.md; cp /bin/true $T/opt/memory
# stand-in egress: answers one line on a unix socket
python3 -I -c "import socket,os;p='$T/run/egress.sock';os.path.exists(p) and os.unlink(p);s=socket.socket(socket.AF_UNIX);s.bind(p);s.listen();
while True: c,_=s.accept();c.sendall(b'EGRESS-OK\n');c.close()" &
msb run --no-net --vsock $T/run/egress.sock:5000 \
  -v $T/home:/home/user -v $T/work:/work \
  -v $T/opt/memory:/opt/memory/memory:ro -v $T/memory:/memory:ro \
  python:3.12-alpine -- python3 -c "
import socket,os
for a in [('1.1.1.1',443),('10.0.2.2',80)]:
    try: socket.create_connection(a,3); print('NET OPEN',a)
    except OSError as e: print('net blocked',a,e)
print(open('/proc/net/dev').read())                 # expect lo only with SDK disableNetwork; CLI --no-net keeps a device
v=socket.socket(socket.AF_VSOCK,socket.SOCK_STREAM); v.connect((2,5000)); print('vsock:',v.recv(64))
for p in ['/memory/entry.md','/opt/memory/memory']:
    try: open(p,'a'); print('WRITABLE',p)
    except OSError as e: print('ro ok',p,e.strerror)
open('/work/x','w').write('w'); print('work rw ok', os.listdir('/'))"
```

Pass means: both connects blocked, `vsock: b'EGRESS-OK\n'`, `/memory` and `/opt/memory/memory` read-only, `/work` writable, and `/` showing nothing from the host beyond the mounts. Repeat with the Rust SDK's `.disableNetwork()` (`cargo add microsandbox` in a scratch crate) to confirm that no interface exists, which is the strict form of point 1. Check whether the rootfs can be made read-only; that is not verified.

### Spike B: kubernetes-sigs/agent-sandbox (needs rootless podman via kind; no /dev/kvm, no Node)

This host has podman 4.9.3, rootless, cgroup v2, network backend CNI, and delegated controllers `cpu memory pids` (from `podman info` and `user@1000.service/cgroup.controllers`). kind rootless podman may also want `cpuset` and `io` delegated. If `kind create cluster` fails on that, delegation needs a root drop-in (`/etc/systemd/system/user@.service.d/delegate.conf` with `Delegate=yes`). Install, without root, into `$HOME/spikes/k8s/bin`:

```sh
B=$HOME/spikes/k8s/bin; mkdir -p $B; export PATH=$B:$PATH
curl -fsSLo $B/kind https://github.com/kubernetes-sigs/kind/releases/download/v0.33.0/kind-linux-amd64 && chmod +x $B/kind
curl -fsSLo $B/kubectl https://dl.k8s.io/release/v1.37.1/bin/linux/amd64/kubectl && chmod +x $B/kubectl
KIND_EXPERIMENTAL_PROVIDER=podman kind create cluster --name as-spike
kubectl apply --server-side -f https://github.com/kubernetes-sigs/agent-sandbox/releases/download/v1.0.7/sandbox-with-extensions.yaml
kubectl -n agent-sandbox-system rollout status deploy/agent-sandbox-controller
```

Contract test. Build one Sandbox with an egress sidecar and a harness container sharing an `emptyDir` socket and fixed-path mounts, then probe point 1 two ways:

```yaml
# sb.yaml
apiVersion: agents.x-k8s.io/v1beta1
kind: Sandbox
metadata: {name: c1}
spec:
  podTemplate:
    spec:
      automountServiceAccountToken: false
      initContainers:
      - name: egress            # native sidecar: stands in for `amem` egress
        image: docker.io/library/python:3.12-alpine
        restartPolicy: Always
        command: ["python3","-c","import socket,os\np='/run/memory/egress.sock'\ns=socket.socket(socket.AF_UNIX);s.bind(p);os.chmod(p,0o777);s.listen()\nwhile True:\n c,_=s.accept();c.sendall(b'EGRESS-OK\\n');c.close()"]
        volumeMounts: [{name: sock, mountPath: /run/memory}]
      containers:
      - name: harness
        image: docker.io/library/python:3.12-alpine
        command: ["sleep","infinity"]
        securityContext: {seccompProfile: {type: RuntimeDefault}}   # second run: Unconfined
        volumeMounts:
        - {name: sock, mountPath: /run/memory}
        - {name: work, mountPath: /work}
        - {name: home, mountPath: /home/user}
        - {name: mem,  mountPath: /memory, readOnly: true}
      volumes:
      - {name: sock, emptyDir: {}}
      - {name: work, emptyDir: {}}
      - {name: home, emptyDir: {}}
      - {name: mem,  emptyDir: {}}
```

```sh
kubectl apply -f sb.yaml && kubectl wait --for=condition=Ready sandbox/c1 --timeout=180s
P=$(kubectl get pod -o name | grep c1 | head -1)
# (1) socket path works, fixed mounts present, /memory read-only
kubectl exec $P -c harness -- python3 -c "import socket;s=socket.socket(socket.AF_UNIX);s.connect('/run/memory/egress.sock');print(s.recv(64));open('/memory/x','w')"
# (2) shared pod netns: expect eth0 and an open route out -> point 1 NOT held by the pod alone
kubectl exec $P -c harness -- sh -c "cat /proc/net/dev; python3 -c \"import socket;socket.create_connection(('1.1.1.1',443),3);print('NET OPEN')\""
# (3) can our Linux backend run inside? expect EPERM under RuntimeDefault, success under Unconfined
kubectl exec $P -c harness -- unshare -Urmn sh -c 'cat /proc/net/dev; echo inside-own-netns'
# (4) layout (b): deny-all egress on the harness pod (kind's default CNI may not enforce NetworkPolicy; install Calico or note it)
```

Record for each of (2), (3) and (4) whether point 1 holds. Step (3) decides whether the institutional shape is "our backend inside an agent-sandbox pod" (needs a seccomp profile that admits `unshare`, or `hostUsers: false`) or "egress as a separate pod over TCP".
